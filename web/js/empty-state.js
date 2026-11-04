/**
 * Empty state (data-testid="empty-state"): shown over the workspace while there are no
 * panels. Lists the tmux sessions (click to open), lets the user create a session and
 * offers "Open all". While tmux is not running it also offers "Start tmux".
 */
import { clear, el, plural } from './dom.js';
import { svgIcon } from './icons.js';
import { startTmux } from './server-actions.js';
import { createAndOpenSession } from './session-actions.js';

export class EmptyState {
  /**
   * @param {HTMLElement} container positioned element covering the workspace
   * @param {{store: any, api: any, workspace: any}} services
   */
  constructor(container, services) {
    this.services = services;
    this.enabled = false;
    this.pending = false;
    this.creating = false;
    this.starting = false;

    this.heading = el('h2', { class: 'empty-heading', testid: 'empty-heading' });
    this.hint = el('p', { class: 'empty-hint' });
    this.startButton = el('button', {
      type: 'button',
      class: 'btn btn-primary empty-start-btn',
      testid: 'empty-start-tmux',
      title: 'Start the tmux server',
      text: 'Start tmux',
      hidden: true,
    });
    this.list = el('div', { class: 'empty-list', testid: 'empty-session-list' });
    this.newInput = el('input', {
      type: 'text',
      class: 'picker-new-input',
      testid: 'empty-new-session-input',
      placeholder: 'New session…',
      'aria-label': 'New session name',
      autocomplete: 'off',
      spellcheck: 'false',
    });
    this.createButton = el('button', { type: 'button', class: 'picker-create-btn', testid: 'empty-new-session-btn', text: 'Create' });
    this.openAllButton = el('button', { type: 'button', class: 'picker-open-all', testid: 'empty-open-all-btn' });
    this.error = el('div', { class: 'picker-error', testid: 'empty-error', role: 'alert', hidden: true });

    this.element = el(
      'div',
      { class: 'empty-state', testid: 'empty-state', hidden: true },
      el(
        'div',
        { class: 'empty-card' },
        el('div', { class: 'empty-icon' }, svgIcon('terminal', { size: 28 })),
        this.heading,
        this.hint,
        this.startButton,
        this.list,
        el(
          'div',
          { class: 'picker-footer' },
          el('div', { class: 'picker-new' }, el('span', { class: 'picker-new-icon' }, svgIcon('plus', { size: 14 })), this.newInput, this.createButton),
          this.openAllButton,
        ),
        this.error,
      ),
    );
    container.append(this.element);

    this.newInput.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        void this.#create();
      }
    });
    this.newInput.addEventListener('input', () => this.#showError(''));
    this.createButton.addEventListener('click', () => void this.#create());
    this.startButton.addEventListener('click', () => void this.#start());
    this.openAllButton.addEventListener('click', () => services.workspace.openAll(this.#sessions()));

    const refresh = () => this.#scheduleRender();
    services.store.on('status', refresh);
    services.workspace.on('change', refresh);
  }

  /** Allow the empty state to appear (after the saved layout has been restored). */
  enable() {
    this.enabled = true;
    this.#render();
  }

  #sessions() {
    return (this.services.store.status?.sessions ?? []).map((session) => ({ name: session.name, id: session.id }));
  }

  /** Coalesce bursts of changes (e.g. a whole fromJSON) into one update after the current task. */
  #scheduleRender() {
    if (this.pending) return;
    this.pending = true;
    queueMicrotask(() => {
      this.pending = false;
      this.#render();
    });
  }

  #render() {
    const { store, workspace } = this.services;
    const empty = this.enabled && workspace.panelCount === 0;
    this.element.hidden = !empty;
    if (!empty) return;

    const sessions = store.status?.sessions ?? [];
    const running = store.isRunning();
    this.heading.textContent = running ? 'No panels open' : 'tmux server is not running';
    this.hint.textContent = !running
      ? 'Start tmux to begin.'
      : sessions.length > 0
        ? 'Pick a session to open it in a panel.'
        : 'There are no tmux sessions yet. Create one below.';
    this.startButton.hidden = running;

    clear(this.list);
    for (const session of sessions) {
      const item = el('button', {
        type: 'button',
        class: 'picker-item',
        testid: 'empty-session-item',
        data: { session: session.name },
        on: { click: () => this.services.workspace.openSession({ name: session.name, id: session.id }) },
      });
      item.append(
        el('span', { class: 'picker-name', text: session.name }),
        el('span', { class: 'picker-meta', text: `${plural(session.windows ?? 0, 'window')} · ${session.attached ?? 0} attached` }),
      );
      if ((session.claude_panes ?? 0) > 0) {
        const badge = el('span', { class: 'badge badge-claude', title: 'Claude Code is running here' });
        badge.append(svgIcon('claude', { size: 11 }), 'Claude');
        item.append(badge);
      }
      this.list.append(item);
    }
    this.list.hidden = sessions.length === 0;
    this.openAllButton.textContent = sessions.length > 0 ? `Open all (${sessions.length})` : 'Open all';
    this.openAllButton.disabled = sessions.length === 0;
  }

  #showError(message) {
    this.error.textContent = message;
    this.error.hidden = message === '';
  }

  async #start() {
    if (this.starting) return;
    this.starting = true;
    this.startButton.disabled = true;
    const result = await startTmux(this.services);
    this.starting = false;
    this.startButton.disabled = false;
    this.#showError(result.ok ? '' : result.message);
  }

  async #create() {
    if (this.creating) return;
    this.creating = true;
    this.createButton.disabled = true;
    const result = await createAndOpenSession(this.services, this.newInput.value);
    this.creating = false;
    this.createButton.disabled = false;
    if (result.ok) {
      this.newInput.value = '';
      this.#showError('');
    } else {
      this.#showError(result.message);
      this.newInput.focus();
    }
  }
}
