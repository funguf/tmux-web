/**
 * Toast notifications (data-testid="toast"). Every toast auto-dismisses after 4 s;
 * the timer pauses while the pointer is over it so action buttons stay reachable.
 */
import { el } from './dom.js';

const DEFAULT_DURATION_MS = 4000;
const MAX_VISIBLE = 5;

class ToastHost {
  #container = null;

  #ensureContainer() {
    if (this.#container?.isConnected) return this.#container;
    let container = document.getElementById('toasts');
    if (!container) {
      container = el('div', { id: 'toasts', 'aria-live': 'polite', role: 'status' });
      document.body.append(container);
    }
    this.#container = container;
    return container;
  }

  /**
   * Show a toast.
   * @param {string} message
   * @param {{kind?: 'info'|'success'|'warn'|'error', actions?: {label: string, onClick: () => void, testid?: string}[], duration?: number}} [options]
   * @returns {{dismiss: () => void, element: HTMLElement}}
   */
  show(message, { kind = 'info', actions = [], duration = DEFAULT_DURATION_MS } = {}) {
    const container = this.#ensureContainer();
    while (container.children.length >= MAX_VISIBLE) container.firstElementChild?.remove();

    const element = el('div', { class: `toast toast-${kind}`, testid: 'toast', data: { kind } });
    element.append(el('span', { class: 'toast-message', text: message }));

    let timer = null;
    let dismissed = false;
    const dismiss = () => {
      if (dismissed) return;
      dismissed = true;
      if (timer !== null) clearTimeout(timer);
      element.remove();
    };
    const arm = () => {
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(dismiss, duration);
    };

    for (const action of actions) {
      element.append(
        el('button', {
          type: 'button',
          class: 'toast-action',
          testid: action.testid ?? 'toast-action',
          text: action.label,
          on: {
            click: () => {
              dismiss();
              action.onClick();
            },
          },
        }),
      );
    }

    element.addEventListener('pointerenter', () => {
      if (timer !== null) clearTimeout(timer);
      timer = null;
    });
    element.addEventListener('pointerleave', () => {
      if (!dismissed) arm();
    });

    container.append(element);
    arm();
    return { dismiss, element };
  }

  info(message, options) {
    return this.show(message, { ...options, kind: 'info' });
  }

  success(message, options) {
    return this.show(message, { ...options, kind: 'success' });
  }

  warn(message, options) {
    return this.show(message, { ...options, kind: 'warn' });
  }

  error(message, options) {
    return this.show(message, { ...options, kind: 'error' });
  }
}

export const toast = new ToastHost();
