/**
 * Minimal modal dialog: backdrop, focus trap, Escape / backdrop click to close.
 * Subclasses fill `body` and `footer`.
 *
 * Events: "close".
 */
import { el, overlayRoot } from './dom.js';
import { EventHub } from './emitter.js';

const FOCUSABLE = 'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [href], [tabindex]:not([tabindex="-1"])';

export class Modal extends EventHub {
  /**
   * @param {{title: string, testid: string, className?: string}} options
   */
  constructor({ title, testid, className = '' }) {
    super();
    this.titleEl = el('h2', { class: 'modal-title', text: title });
    this.body = el('div', { class: 'modal-body' });
    this.footer = el('div', { class: 'modal-footer' });
    this.dialog = el(
      'div',
      { class: `modal ${className}`.trim(), role: 'dialog', 'aria-modal': 'true', 'aria-label': title, testid },
      this.titleEl,
      this.body,
      this.footer,
    );
    this.backdrop = el('div', { class: 'modal-backdrop', testid: `${testid}-backdrop` }, this.dialog);
    this.isOpen = false;
    this.restoreFocusTo = null;

    this.backdrop.addEventListener('pointerdown', (ev) => {
      if (ev.target === this.backdrop) this.close();
    });
    this.backdrop.addEventListener('keydown', (ev) => this.#onKeyDown(ev));
  }

  /** @param {HTMLElement|null} [initialFocus] element to focus first (default: first focusable) */
  open(initialFocus = null) {
    if (this.isOpen) return;
    this.restoreFocusTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    overlayRoot().append(this.backdrop);
    this.isOpen = true;
    (initialFocus ?? this.dialog.querySelector(FOCUSABLE) ?? this.dialog).focus?.();
  }

  close() {
    if (!this.isOpen) return;
    this.isOpen = false;
    this.backdrop.remove();
    const target = this.restoreFocusTo;
    this.restoreFocusTo = null;
    this.emit('close');
    if (target?.isConnected) target.focus({ preventScroll: true });
  }

  #onKeyDown(ev) {
    if (ev.key === 'Escape') {
      ev.preventDefault();
      ev.stopPropagation();
      this.close();
      return;
    }
    if (ev.key !== 'Tab') return;
    const focusable = Array.from(this.dialog.querySelectorAll(FOCUSABLE));
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (ev.shiftKey && document.activeElement === first) {
      ev.preventDefault();
      last.focus();
    } else if (!ev.shiftKey && document.activeElement === last) {
      ev.preventDefault();
      first.focus();
    }
  }
}
