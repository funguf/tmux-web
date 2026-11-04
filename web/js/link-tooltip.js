/**
 * Small tooltip shown next to the pointer while hovering a terminal link,
 * telling the user that a modifier-click is required to open it.
 */
import { el, isMac, overlayRoot } from './dom.js';

export class LinkTooltip {
  constructor() {
    this.element = el('div', {
      class: 'link-tooltip',
      testid: 'link-tooltip',
      role: 'tooltip',
      hidden: true,
      text: isMac() ? 'Cmd+click to open' : 'Ctrl+click to open',
    });
    overlayRoot().append(this.element);
  }

  /** Show near viewport coordinates (x, y). */
  show(x, y) {
    this.element.hidden = false;
    const width = this.element.offsetWidth;
    const height = this.element.offsetHeight;
    const vw = document.documentElement.clientWidth;
    const vh = document.documentElement.clientHeight;
    const left = Math.min(Math.max(4, x + 12), Math.max(4, vw - width - 4));
    const top = y + 22 + height > vh ? Math.max(4, y - height - 10) : y + 22;
    this.element.style.left = `${Math.round(left)}px`;
    this.element.style.top = `${Math.round(top)}px`;
  }

  hide() {
    this.element.hidden = true;
  }
}
