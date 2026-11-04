/**
 * Session picker (data-testid="session-picker"): choose a tmux session to open as a
 * panel, create a new session, or open every session at once.
 *
 *  - mode "add":   the chosen session becomes a tab in the active group (or its
 *                  existing panel is focused).
 *  - mode "split": the chosen session goes into a new group right of / below
 *                  `referenceGroup` (an already open session is moved there).
 *
 * Keyboard: Up / Down move the highlight, Enter opens it, Escape closes.
 */
import { clear, el, plural } from './dom.js';
import { svgIcon } from './icons.js';
import { Popover } from './popover.js';
import { createAndOpenSession } from './session-actions.js';

export class SessionPicker {
  /**
   * @param {{store: any, api: any, workspace: any}} services
   */
  constructor(services) {
    this.services = services;
    this.mode = 'add';
    this.referenceGroup = null;
    this.direction = 'right';
    this.filter = '';
    this.highlight = 0;
    this.creating = false;
    this.unsubscribe = null;
    this.visible = [];

    this.popover = new Popover({ className: 'session-picker', testid: 'session-picker', label: 'Open a session' });
    this.popover.on('close', () => this.#onClosed());

    this.title = el('div', { class: 'picker-title' });
    this.filterInput = el('input', {
      type: 'text',
      class: 'picker-filter',
      testid: 'session-picker-filter',
      placeholder: 'Filter sessions…',
      'aria-label': 'Filter sessions',
      autocomplete: 'off',
      spellcheck: 'false',
    });
    this.list = el('div', { class: 'picker-list', role: 'listbox', testid: 'session-picker-list' });
    this.newInput = el('input', {
      type: 'text',
      class: 'picker-new-input',
      testid: 'new-session-input',
      placeholder: 'New session…',
      'aria-label': 'New session name',
      autocomplete: 'off',
      spellcheck: 'false',
    });
    this.createButton = el('button', {
      type: 'button',
      class: 'picker-create-btn',
      testid: 'new-session-btn',
      title: 'Create the session and open it',
      text: 'Create',
    });
    this.openAllButton = el('button', {
      type: 'button',
      class: 'picker-open-all',
      testid: 'open-all-btn',
      title: 'Add every session that is not open yet as a tab',
    });
    this.error = el('div', { class: 'picker-error', testid: 'picker-error', role: 'alert', hidden: true });

    const newRow = el(
      'div',
      { class: 'picker-new' },
      el('span', { class: 'picker-new-icon' }, svgIcon('plus', { size: 14 })),
      this.newInput,
      this.createButton,
    );
    this.popover.element.append(
      this.title,
      this.filterInput,
      this.list,
      el('div', { class: 'picker-footer' }, newRow, this.openAllButton),
      this.error,
    );

    this.filterInput.addEventListener('input', () => {
      this.filter = this.filterInput.value;
      this.highlight = 0;
      this.#render();
    });
    this.filterInput.addEventListener('keydown', (ev) => this.#onFilterKey(ev));
    this.newInput.addEventListener('keydown', (ev) => this.#onNewKey(ev));
    this.newInput.addEventListener('input', () => this.#showError(''));
    this.createButton.addEventListener('click', () => void this.#create());
    this.openAllButton.addEventListener('click', () => this.#openAll());
  }

  get isOpen() {
    return this.popover.isOpen;
  }

  /**
   * @param {{mode?: 'add'|'split', referenceGroup?: any, direction?: 'right'|'below',
   *          anchorElement?: HTMLElement|null, anchor?: DOMRect|null}} [options]
   */
  open({ mode = 'add', referenceGroup = null, direction = 'right', anchorElement = null, anchor = null } = {}) {
    if (this.popover.isOpen) this.popover.close();
    this.mode = mode;
    this.referenceGroup = referenceGroup;
    this.direction = direction;
    this.filter = '';
    this.highlight = 0;
    this.filterInput.value = '';
    this.newInput.value = '';
    this.#showError('');
    this.popover.element.dataset.mode = mode;
    this.#render();
    this.popover.open({ anchorElement, anchorRect: anchor, align: 'end' });
    this.unsubscribe = this.services.store.on('status', () => this.#render());
    this.filterInput.focus();
  }

  toggle(options) {
    if (this.popover.isOpen) this.close();
    else this.open(options);
  }

  close() {
    this.popover.close();
  }

  #onClosed() {
    this.unsubscribe?.();
    this.unsubscribe = null;
    // Escape / outside click: hand the keyboard back to the terminal. When a session was
    // chosen, openSession() focuses the new panel right after this.
    this.services.workspace.focusActive();
  }

  #sessions() {
    return this.services.store.status?.sessions ?? [];
  }

  #render() {
    const { store, workspace } = this.services;
    const open = workspace.openSessionNames();
    const needle = this.filter.trim().toLowerCase();
    const all = this.#sessions();
    this.visible = all.filter((session) => !needle || session.name.toLowerCase().includes(needle));
    this.highlight = Math.min(this.highlight, Math.max(0, this.visible.length - 1));

    this.title.textContent =
      this.mode === 'split'
        ? `Open a session ${this.direction === 'right' ? 'to the right' : 'below'}`
        : 'Open a session';

    clear(this.list);
    if (this.visible.length === 0) {
      const running = store.isRunning();
      this.list.append(
        el('div', {
          class: 'picker-empty',
          testid: 'session-picker-empty',
          text: all.length > 0 ? 'No session matches the filter' : running ? 'No sessions' : 'tmux server is not running: create a session to start it',
        }),
      );
    }
    this.visible.forEach((session, index) => {
      const isOpen = open.has(session.name);
      const item = el('button', {
        type: 'button',
        class: `picker-item${index === this.highlight ? ' is-highlighted' : ''}`,
        role: 'option',
        'aria-selected': index === this.highlight ? 'true' : 'false',
        testid: 'session-picker-item',
        data: { session: session.name, open: isOpen ? 'true' : 'false' },
        on: {
          click: () => this.#choose(session),
          pointermove: () => this.#setHighlight(index),
        },
      });
      item.append(el('span', { class: 'picker-name', text: session.name }));
      const meta = [plural(session.windows ?? 0, 'window')];
      meta.push(`${session.attached ?? 0} attached`);
      item.append(el('span', { class: 'picker-meta', testid: 'session-picker-meta', text: meta.join(' · ') }));
      if ((session.claude_panes ?? 0) > 0) {
        const badge = el('span', { class: 'badge badge-claude', testid: 'session-picker-claude', title: 'Claude Code is running here' });
        badge.append(svgIcon('claude', { size: 11 }), 'Claude');
        item.append(badge);
      }
      if (isOpen) item.append(el('span', { class: 'badge badge-open', testid: 'session-picker-open', text: 'open' }));
      this.list.append(item);
    });

    const notOpen = all.filter((session) => !open.has(session.name));
    this.openAllButton.textContent = notOpen.length > 0 ? `Open all (${notOpen.length})` : 'Open all';
    this.openAllButton.disabled = notOpen.length === 0;
    this.openAllButton.hidden = this.mode === 'split';
    this.popover.reposition();
  }

  #setHighlight(index) {
    if (index === this.highlight) return;
    const items = this.list.querySelectorAll('.picker-item');
    items[this.highlight]?.classList.remove('is-highlighted');
    items[this.highlight]?.setAttribute('aria-selected', 'false');
    this.highlight = index;
    items[index]?.classList.add('is-highlighted');
    items[index]?.setAttribute('aria-selected', 'true');
  }

  #moveHighlight(delta) {
    if (this.visible.length === 0) return;
    const next = (this.highlight + delta + this.visible.length) % this.visible.length;
    this.#setHighlight(next);
    this.list.querySelectorAll('.picker-item')[next]?.scrollIntoView({ block: 'nearest' });
  }

  #onFilterKey(ev) {
    if (ev.key === 'ArrowDown') {
      ev.preventDefault();
      this.#moveHighlight(1);
    } else if (ev.key === 'ArrowUp') {
      ev.preventDefault();
      this.#moveHighlight(-1);
    } else if (ev.key === 'Enter') {
      ev.preventDefault();
      const session = this.visible[this.highlight];
      if (session) this.#choose(session);
      else if (this.filter.trim()) {
        this.newInput.value = this.filter.trim();
        this.newInput.focus();
      }
    }
  }

  #onNewKey(ev) {
    if (ev.key === 'Enter') {
      ev.preventDefault();
      void this.#create();
    } else if (ev.key === 'ArrowUp') {
      ev.preventDefault();
      this.filterInput.focus();
    }
  }

  #showError(message) {
    this.error.textContent = message;
    this.error.hidden = message === '';
  }

  #placement() {
    return { mode: this.mode, referenceGroup: this.referenceGroup, direction: this.direction };
  }

  #choose(session) {
    const placement = this.#placement();
    this.close();
    this.services.workspace.openSession({ name: session.name, id: session.id }, placement);
  }

  #openAll() {
    const sessions = this.#sessions().map((session) => ({ name: session.name, id: session.id }));
    this.close();
    this.services.workspace.openAll(sessions);
  }

  async #create() {
    if (this.creating) return;
    const name = this.newInput.value;
    this.creating = true;
    this.createButton.disabled = true;
    const placement = this.#placement();
    const result = await createAndOpenSession(this.services, name, placement);
    this.creating = false;
    this.createButton.disabled = false;
    if (result.ok) this.close();
    else {
      this.#showError(result.message);
      this.newInput.focus();
    }
  }
}
