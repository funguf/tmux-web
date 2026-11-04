/**
 * Overlay shown on top of a terminal while it is not connected
 * (data-testid="panel-overlay"). The caller picks a view:
 *
 *   gone        Session “X” no longer exists   [Recreate session] [Close panel]
 *   server-down tmux server is not running     [Close panel]
 *   detached    Detached from session “X”      [Reattach] [Close panel]
 *   waiting     Connection lost, retrying in N s   [Retry now] [Close panel]
 *   reconnecting  Reconnecting…
 */
import { el } from './dom.js';
import { svgIcon } from './icons.js';

export class PanelOverlay {
  /**
   * @param {{onRecreate: () => void, onClose: () => void, onReattach: () => void, onRetry: () => void}} handlers
   */
  constructor(handlers) {
    this.handlers = handlers;
    this.view = null;
    this.ticker = null;
    this.element = el('div', { class: 'panel-overlay', testid: 'panel-overlay', hidden: true });
  }

  get visible() {
    return !this.element.hidden;
  }

  /**
   * @param {{kind: 'gone'|'server-down'|'detached'|'waiting'|'reconnecting', session: string, message?: string, retryAt?: number}} view
   */
  show(view) {
    this.view = view;
    this.#stopTicker();
    this.element.hidden = false;
    this.#render();
    if (view.kind === 'waiting') {
      this.ticker = setInterval(() => this.#tick(), 1000);
    }
  }

  hide() {
    this.view = null;
    this.#stopTicker();
    this.element.hidden = true;
    this.element.replaceChildren();
  }

  dispose() {
    this.#stopTicker();
  }

  /** Update only the countdown text, so buttons are never re-created under the pointer. */
  #tick() {
    const view = this.view;
    const hint = this.element.querySelector('[data-testid="overlay-hint"]');
    if (view?.kind === 'waiting' && hint) hint.textContent = this.#countdown(view);
  }

  #countdown(view) {
    const seconds = Math.max(0, Math.ceil(((view.retryAt ?? 0) - Date.now()) / 1000));
    return `Retrying in ${seconds}s\u2026`;
  }

  #stopTicker() {
    if (this.ticker !== null) clearInterval(this.ticker);
    this.ticker = null;
  }

  #render() {
    const view = this.view;
    if (!view) return;
    const { onRecreate, onClose, onReattach, onRetry } = this.handlers;
    const button = (label, testid, handler, primary = false) =>
      el('button', {
        type: 'button',
        class: `overlay-btn${primary ? ' overlay-btn-primary' : ''}`,
        testid,
        text: label,
        on: { click: handler },
      });

    let icon = 'terminal';
    let title = '';
    let hint = '';
    const buttons = [];
    switch (view.kind) {
      case 'gone':
        title = `Session “${view.session}” no longer exists`;
        buttons.push(button('Recreate session', 'overlay-recreate', onRecreate, true));
        break;
      case 'server-down':
        title = 'tmux server is not running';
        hint = 'It will start again when a session is created.';
        break;
      case 'detached':
        title = `Detached from session “${view.session}”`;
        icon = 'refresh';
        buttons.push(button('Reattach', 'overlay-reattach', onReattach, true));
        break;
      case 'waiting': {
        const seconds = Math.max(0, Math.ceil(((view.retryAt ?? 0) - Date.now()) / 1000));
        title = view.message ? `Connection problem: ${view.message}` : 'Connection lost';
        hint = `Retrying in ${seconds}s…`;
        icon = 'refresh';
        buttons.push(button('Retry now', 'overlay-retry', onRetry, true));
        break;
      }
      case 'reconnecting':
        title = 'Reconnecting…';
        icon = 'refresh';
        break;
      default:
        break;
    }
    buttons.push(button('Close panel', 'overlay-close', onClose));

    this.element.dataset.kind = view.kind;
    this.element.replaceChildren(
      el(
        'div',
        { class: 'overlay-card', role: 'alert' },
        el('div', { class: 'overlay-icon' }, svgIcon(icon, { size: 22 })),
        el('div', { class: 'overlay-message', testid: 'overlay-message', text: title }),
        hint ? el('div', { class: 'overlay-hint', testid: 'overlay-hint', text: hint }) : null,
        el('div', { class: 'overlay-actions' }, buttons),
      ),
    );
  }
}
