/**
 * Dockview tab renderer: connection dot, title (exactly the session name), a Claude
 * badge and a close button; the tab flashes while its panel has an unseen bell. The tab
 * contains no text besides the title, so its text content always equals the tmux session name.
 *
 * Double-click toggles maximise of the tab's group; middle-click closes the panel.
 * Closing a panel never kills the tmux session.
 */
import { el } from './dom.js';
import { svgIcon } from './icons.js';

const DOT_TITLES = {
  idle: 'Not connected yet',
  connecting: 'Connecting…',
  connected: 'Connected',
  waiting: 'Connection lost, retrying',
  closed: 'Disconnected',
};

const CLOSED_TITLES = {
  detached: 'Detached from the session',
  'session-closed': 'Session ended',
  'no-session': 'Session does not exist',
  'server-down': 'tmux server stopped',
  'no-server': 'tmux server is not running',
};

export class TerminalTab {
  /**
   * @param {object} services
   * @param {string} id dockview panel id
   */
  constructor(services, id) {
    this.services = services;
    this.id = id;
    this.api = null;
    this.params = {};
    this.disposables = [];

    this.dot = el('span', { class: 'tab-dot', testid: 'tab-dot', data: { state: 'idle' } });
    this.titleEl = el('span', { class: 'tab-title', testid: 'tab-title' });
    this.badge = el('span', {
      class: 'tab-claude',
      testid: 'tab-claude',
      title: 'Claude Code is running in this session',
      hidden: true,
    });
    this.badge.append(svgIcon('claude', { size: 12 }));
    this.snoozedMark = el('span', { class: 'tab-snoozed', testid: 'tab-snoozed', title: 'Claude is snoozed', hidden: true });
    this.snoozedMark.append(svgIcon('moon', { size: 12 }));
    this.closeButton = el('button', {
      type: 'button',
      class: 'tab-close',
      testid: 'tab-close',
      title: 'Close panel (the tmux session keeps running)',
      'aria-label': 'Close panel',
    });
    this.closeButton.append(svgIcon('close', { size: 12 }));

    this.element = el(
      'div',
      { class: 'tmw-tab', testid: 'tab', data: { panelId: id, attention: 'false' } },
      this.dot,
      this.titleEl,
      this.badge,
      this.snoozedMark,
      this.closeButton,
    );
  }

  /** @param {{params: object, title: string, api: any, containerApi: any}} parameters */
  init(parameters) {
    this.api = parameters.api;
    this.params = { ...(parameters.params ?? {}) };
    this.#setTitle(parameters.title ?? this.params.session ?? '');
    this.#syncBadge();
    this.#syncDot();
    this.#syncAttention();

    const { store, registry } = this.services;
    this.disposables.push(
      this.api.onDidTitleChange((event) => this.#setTitle(event.title)),
      { dispose: store.on('status', () => this.#syncBadge()) },
    );
    const panel = registry.get(this.id);
    if (panel) {
      this.disposables.push(
        { dispose: panel.events.on('state', () => this.#syncDot()) },
        { dispose: panel.events.on('attention', () => this.#syncAttention()) },
      );
    }

    this.closeButton.addEventListener('pointerdown', (ev) => ev.stopPropagation());
    this.closeButton.addEventListener('click', (ev) => {
      ev.stopPropagation();
      this.api.close();
    });
    // The tab itself takes the focus on mousedown. Hand it to the terminal again, also when
    // the tab was already active (then no activation event does it).
    this.element.addEventListener('click', (ev) => {
      if (this.closeButton.contains(/** @type {Node} */ (ev.target))) return;
      this.services.registry.get(this.id)?.focus();
    });
    this.element.addEventListener('dblclick', (ev) => {
      if (this.closeButton.contains(/** @type {Node} */ (ev.target))) return;
      ev.preventDefault();
      this.services.workspace.toggleMaximize(this.id);
    });
    this.element.addEventListener('mousedown', (ev) => {
      if (ev.button === 1) ev.preventDefault(); // no autoscroll on middle-click
    });
    this.element.addEventListener('auxclick', (ev) => {
      if (ev.button !== 1) return;
      ev.preventDefault();
      ev.stopPropagation();
      this.api.close();
    });
  }

  update(event) {
    this.params = { ...(event?.params ?? this.params) };
    this.#syncBadge();
  }

  dispose() {
    for (const disposable of this.disposables.splice(0)) disposable.dispose();
    this.element.remove();
  }

  get #session() {
    return this.params.session ?? this.titleEl.textContent ?? '';
  }

  #setTitle(title) {
    this.titleEl.textContent = title;
    this.element.dataset.session = title;
    this.element.title = title;
  }

  #syncBadge() {
    const info = this.services.store.session(this.#session);
    const count = info?.claude_panes ?? 0;
    this.badge.hidden = count <= 0;
    this.element.dataset.claude = count > 0 ? String(count) : '0';
    const snoozed = info?.snoozed ?? 0;
    this.snoozedMark.hidden = snoozed <= 0;
    this.element.dataset.snoozed = snoozed > 0 ? String(snoozed) : '0';
    this.badge.title = count > 1 ? `Claude Code is running in ${count} panes of this session` : 'Claude Code is running in this session';
  }

  #syncDot() {
    const panel = this.services.registry.get(this.id);
    const state = panel?.dotState ?? 'idle';
    this.dot.dataset.state = state;
    const reason = state === 'closed' ? panel?.conn.reason : null;
    this.dot.title = (reason && CLOSED_TITLES[reason]) || DOT_TITLES[state] || state;
  }

  #syncAttention() {
    this.element.dataset.attention = this.services.registry.get(this.id)?.attention ? 'true' : 'false';
  }
}
