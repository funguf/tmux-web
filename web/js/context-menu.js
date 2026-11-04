/**
 * Right-click context menu (data-testid="context-menu", items "context-menu-item"
 * with data-action). One menu is open at a time; it closes on selection, Escape,
 * outside click, scroll, resize and window blur.
 */
import { dismissOnOutside, el, overlayRoot, placeNear } from './dom.js';
import { svgIcon } from './icons.js';

/**
 * @typedef {{
 *   label?: string,
 *   action?: string,
 *   icon?: string,
 *   shortcut?: string,
 *   disabled?: boolean,
 *   separator?: boolean,
 *   onSelect?: () => void,
 * }} MenuItem
 */

export class ContextMenu {
  constructor() {
    /** @type {HTMLElement|null} */
    this.element = null;
    this.disposers = [];
    this.onClose = null;
  }

  /**
   * @param {{x: number, y: number}} point viewport coordinates of the pointer
   * @param {MenuItem[]} items
   * @param {{onClose?: () => void}} [options] onClose runs after the menu closed (e.g. to restore focus)
   */
  open(point, items, { onClose } = {}) {
    this.close();
    this.onClose = onClose ?? null;

    const menu = el('div', { class: 'context-menu', role: 'menu', tabindex: '-1', testid: 'context-menu' });
    /** @type {HTMLButtonElement[]} */
    const buttons = [];
    for (const item of items) {
      if (item.separator) {
        menu.append(el('div', { class: 'context-menu-separator', role: 'separator' }));
        continue;
      }
      const button = /** @type {HTMLButtonElement} */ (
        el('button', {
          type: 'button',
          class: 'context-menu-item',
          role: 'menuitem',
          testid: 'context-menu-item',
          data: { action: item.action ?? item.label },
          disabled: item.disabled === true,
        })
      );
      button.append(
        item.icon ? svgIcon(item.icon, { size: 14 }) : el('span', { class: 'context-menu-icon-gap' }),
        el('span', { class: 'context-menu-label', text: item.label ?? '' }),
      );
      if (item.shortcut) button.append(el('span', { class: 'context-menu-shortcut', text: item.shortcut }));
      button.addEventListener('click', () => {
        this.close();
        item.onSelect?.();
      });
      menu.append(button);
      buttons.push(button);
    }

    menu.addEventListener('keydown', (ev) => this.#onKeyDown(ev, buttons));
    menu.addEventListener('contextmenu', (ev) => ev.preventDefault());
    overlayRoot().append(menu);
    this.element = menu;
    placeNear(menu, { left: point.x, top: point.y, right: point.x, bottom: point.y }, { gap: 0 });

    const close = () => this.close();
    this.disposers.push(dismissOnOutside([menu], close));
    window.addEventListener('blur', close);
    window.addEventListener('resize', close);
    window.addEventListener('wheel', close, { passive: true, capture: true });
    this.disposers.push(() => {
      window.removeEventListener('blur', close);
      window.removeEventListener('resize', close);
      window.removeEventListener('wheel', close, { capture: true });
    });
    menu.focus({ preventScroll: true });
  }

  #onKeyDown(ev, buttons) {
    const enabled = buttons.filter((b) => !b.disabled);
    if (enabled.length === 0) return;
    const index = enabled.indexOf(/** @type {HTMLButtonElement} */ (document.activeElement));
    let next = null;
    if (ev.key === 'ArrowDown') next = enabled[(index + 1) % enabled.length];
    else if (ev.key === 'ArrowUp') next = enabled[(index - 1 + enabled.length) % enabled.length];
    else if (ev.key === 'Home') next = enabled[0];
    else if (ev.key === 'End') next = enabled[enabled.length - 1];
    if (next) {
      ev.preventDefault();
      next.focus();
    }
  }

  close() {
    if (!this.element) return;
    this.element.remove();
    this.element = null;
    for (const dispose of this.disposers.splice(0)) dispose();
    const onClose = this.onClose;
    this.onClose = null;
    onClose?.();
  }
}
