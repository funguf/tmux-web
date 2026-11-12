/**
 * The dockview workspace: creates the dockview instance and owns every operation on
 * panels and groups (add / focus / move / split / maximise / rename / serialise).
 *
 * Emits "change" whenever something that is part of the persisted layout changed
 * (structure, maximise, panel title or params).
 *
 * The session -> panel index is derived from the live panels (panel params carry the
 * session name), so it is always consistent, also in the middle of a drag and drop move.
 */
import { createDockview, themeDark } from '../vendor/dockview-core/dockview-core.esm.min.mjs';
import { uid } from './dom.js';
import { EventHub } from './emitter.js';
import { HeaderActions } from './header-actions.js';
import { wrapAllTabs } from './tab-rows.js';
import { TerminalPanel } from './terminal-panel.js';
import { TerminalTab } from './terminal-tab.js';

const TERMINAL_COMPONENT = 'terminal';
const TAB_COMPONENT = 'terminal-tab';

/** Placeholder for a panel component this build does not know (e.g. from a newer layout). */
class UnknownPanel {
  constructor(name) {
    this.element = document.createElement('div');
    this.element.className = 'unknown-panel';
    this.element.textContent = `Unsupported panel type “${name}”`;
  }

  init() {}

  dispose() {
    this.element.remove();
  }
}

export class Workspace extends EventHub {
  /**
   * @param {HTMLElement} container element dockview fills
   * @param {object} services app services (store, persistence, registry, picker, ...)
   */
  constructor(container, services) {
    super();
    this.container = container;
    this.services = services;
    this.registry = services.registry;
    /** @type {Map<string, {dispose: () => void}[]>} */
    this.watchers = new Map();

    this.api = createDockview(container, {
      theme: themeDark,
      defaultRenderer: 'always',
      disableFloatingGroups: true,
      announcements: false,
      defaultTabComponent: TAB_COMPONENT,
      createComponent: ({ id, name }) =>
        name === TERMINAL_COMPONENT ? new TerminalPanel(services, id) : new UnknownPanel(name),
      createTabComponent: ({ id }) => new TerminalTab(services, id),
      createRightHeaderActionComponent: (group) => new HeaderActions(services, group),
    });

    this.disposables = [
      wrapAllTabs(this.api),
      this.api.onDidLayoutChange(() => this.emit('change')),
      this.api.onDidMaximizedGroupChange(() => this.emit('change')),
      this.api.onDidAddPanel((panel) => {
        this.#watch(panel);
        this.emit('change');
      }),
      this.api.onDidRemovePanel((panel) => {
        this.#unwatch(panel.id);
        this.emit('change');
      }),
    ];
  }

  // ---- queries ---------------------------------------------------------------------

  get panelCount() {
    return this.api.totalPanels;
  }

  static sessionOf(panel) {
    return panel.params?.session ?? panel.title ?? '';
  }

  /** Names of the sessions that have a panel. */
  openSessionNames() {
    return new Set(this.api.panels.map((panel) => Workspace.sessionOf(panel)));
  }

  /** The panel showing `name`, if any. */
  panelForSession(name) {
    return this.api.panels.find((panel) => Workspace.sessionOf(panel) === name);
  }

  /** Close every panel that shows `name` (the tmux session is gone). */
  closeSession(name) {
    for (const panel of this.api.panels.filter((candidate) => Workspace.sessionOf(candidate) === name)) {
      panel.api.close();
    }
  }

  isMaximized(panelId) {
    return this.api.getPanel(panelId)?.group.api.isMaximized() ?? false;
  }

  // ---- adding and moving panels ------------------------------------------------------

  #newPanelId() {
    let id;
    do id = uid('p-');
    while (this.api.getPanel(id));
    return id;
  }

  /**
   * Add a panel for a session.
   * @param {{name: string, id?: string|null}} ref
   * @param {{position?: object, inactive?: boolean}} [options]
   */
  #addPanel(ref, { position, inactive = false } = {}) {
    const { store } = this.services;
    const sessionId = ref.id ?? store.session(ref.name)?.id ?? null;
    return this.api.addPanel({
      id: this.#newPanelId(),
      component: TERMINAL_COMPONENT,
      title: ref.name,
      params: { session: ref.name, sessionId, serverId: store.serverId() },
      position,
      inactive,
    });
  }

  /**
   * Open a session in a panel.
   *  - mode "add": focus the existing panel, or add a tab to the active group.
   *  - mode "split": put the session in a new group right of / below `referenceGroup`;
   *    an already open session is moved there instead.
   * @param {{name: string, id?: string|null}} ref
   * @param {{mode?: 'add'|'split', referenceGroup?: any, direction?: 'right'|'below'}} [options]
   */
  openSession(ref, { mode = 'add', referenceGroup, direction = 'right' } = {}) {
    const existing = this.panelForSession(ref.name);
    if (mode === 'split' && referenceGroup) {
      if (this.api.hasMaximizedGroup()) this.api.exitMaximizedGroup();
      if (existing) {
        const alone = existing.group === referenceGroup && referenceGroup.panels.length === 1;
        if (!alone) existing.api.moveTo({ group: referenceGroup, position: direction === 'right' ? 'right' : 'bottom' });
        this.focusPanel(existing);
        return existing;
      }
      const panel = this.#addPanel(ref, { position: { referenceGroup, direction } });
      this.focusPanel(panel);
      return panel;
    }
    if (existing) {
      this.focusPanel(existing);
      return existing;
    }
    const target = this.api.activeGroup ?? this.api.groups[0];
    const panel = this.#addPanel(ref, { position: target ? { referenceGroup: target } : undefined });
    this.focusPanel(panel);
    return panel;
  }

  /**
   * Add every session that has no panel yet as tabs of the active group.
   * @param {{name: string, id?: string|null}[]} sessions
   * @returns {any[]} the new panels
   */
  openAll(sessions) {
    const open = this.openSessionNames();
    const missing = sessions.filter((ref) => !open.has(ref.name));
    const hadPanels = this.api.totalPanels > 0;
    let target = this.api.activeGroup ?? this.api.groups[0];
    const created = [];
    missing.forEach((ref, index) => {
      const panel = this.#addPanel(ref, {
        position: target ? { referenceGroup: target } : undefined,
        inactive: hadPanels || index > 0,
      });
      target ??= panel.group;
      created.push(panel);
    });
    return created;
  }

  focusPanel(panel) {
    if (this.api.hasMaximizedGroup() && !panel.group.api.isMaximized()) this.api.exitMaximizedGroup();
    panel.api.setActive();
    this.registry.get(panel.id)?.focus();
  }

  /** Give keyboard focus back to the active terminal. */
  focusActive() {
    const panel = this.api.activePanel;
    if (panel) this.registry.get(panel.id)?.focus();
  }

  // ---- maximise ------------------------------------------------------------------------

  toggleMaximize(panelId) {
    const panel = this.api.getPanel(panelId);
    if (panel) this.toggleMaximizeGroup(panel.group);
  }

  toggleMaximizeGroup(group) {
    if (group.api.isMaximized()) {
      this.api.exitMaximizedGroup();
      return;
    }
    const panel = group.activePanel ?? group.panels[0];
    if (panel) this.api.maximizeGroup(panel);
  }

  /** Active panel of the maximised group, or null. */
  maximizedPanelId() {
    const group = this.api.groups.find((candidate) => candidate.api.isMaximized());
    return group?.activePanel?.id ?? null;
  }

  // ---- renames and identity --------------------------------------------------------------

  /** Make a panel follow a (tmux) rename: new title and params. */
  applyRename(panelId, newName) {
    const panel = this.api.getPanel(panelId);
    if (!panel || Workspace.sessionOf(panel) === newName) return;
    panel.api.updateParameters({ session: newName });
    panel.api.setTitle(newName);
  }

  /**
   * React to a tmux status change (CONTRACT §8.6 item 7):
   *  - a panel from the current server whose session id now has another name follows it;
   *  - connected panels learn their session id / server id by name.
   * @param {{running: boolean, sessions: any[]}} status
   */
  syncWithStatus(status) {
    if (!status.running) return;
    const serverId = this.services.store.serverId();
    for (const panel of this.api.panels) this.#syncPanel(panel, status, serverId);
  }

  /** Learn the ids of one panel from the current status (a panel calls this when it connects). */
  syncPanel(panelId) {
    const { store } = this.services;
    const panel = this.api.getPanel(panelId);
    if (panel && store.status?.running) this.#syncPanel(panel, store.status, store.serverId());
  }

  #syncPanel(panel, status, serverId) {
    const params = panel.params ?? {};
    const name = Workspace.sessionOf(panel);
    const bySessionId =
      serverId && params.serverId === serverId && params.sessionId
        ? status.sessions.find((session) => session.id === params.sessionId)
        : undefined;
    if (bySessionId) {
      if (bySessionId.name !== name) this.applyRename(panel.id, bySessionId.name);
      return;
    }
    if (!this.registry.get(panel.id)?.connected) return;
    const byName = status.sessions.find((session) => session.name === name);
    if (byName && (params.sessionId !== byName.id || params.serverId !== serverId)) {
      panel.api.updateParameters({ sessionId: byName.id, serverId });
    }
  }

  // ---- persistence -----------------------------------------------------------------------

  /** Current state for the layout document. */
  serialize() {
    return { dockview: this.api.toJSON(), maximizedPanelId: this.maximizedPanelId() };
  }

  /**
   * Replace the workspace with a saved layout document, then re-maximise.
   * Throws when dockview rejects the layout (the workspace is then cleared).
   * @param {{dockview: any, maximizedPanelId?: string|null}} doc
   * @param {{reuse?: boolean}} [options] reuse: keep panels with the same id (no reconnect)
   */
  restore(doc, { reuse = false } = {}) {
    try {
      this.api.fromJSON(doc.dockview, reuse ? { reuseExistingPanels: true } : undefined);
      const target = doc.maximizedPanelId ? this.api.getPanel(doc.maximizedPanelId) : undefined;
      if (target && !target.group.api.isMaximized()) {
        if (this.api.hasMaximizedGroup()) this.api.exitMaximizedGroup();
        this.api.maximizeGroup(target);
      }
    } catch (err) {
      try {
        this.api.clear();
      } catch {
        // nothing more to do
      }
      throw err;
    }
  }

  // ---- internals -------------------------------------------------------------------------

  #watch(panel) {
    if (this.watchers.has(panel.id)) return;
    this.watchers.set(panel.id, [
      panel.api.onDidTitleChange(() => this.emit('change')),
      panel.api.onDidParametersChange(() => this.emit('change')),
    ]);
  }

  #unwatch(panelId) {
    for (const disposable of this.watchers.get(panelId) ?? []) disposable.dispose();
    this.watchers.delete(panelId);
  }

  dispose() {
    for (const disposable of this.disposables.splice(0)) disposable.dispose();
    for (const id of [...this.watchers.keys()]) this.#unwatch(id);
    this.api.dispose();
  }
}
