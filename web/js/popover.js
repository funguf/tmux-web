/**
 * Anchored popover that closes on Escape or an outside click. Used by the session
 * picker and the settings popover.
 *
 * Events: "open", "close".
 */
import { dismissOnOutside, el, overlayRoot, placeNear } from './dom.js';
import { EventHub } from './emitter.js';

export class Popover extends EventHub {
  /**
   * @param {{className?: string, testid?: string, label: string}} options
   */
  constructor({ className = '', testid, label }) {
    super();
    this.element = el('div', {
      class: `popover ${className}`.trim(),
      role: 'dialog',
      'aria-label': label,
      testid,
      hidden: true,
    });
    this.isOpen = false;
    this.anchorElement = null;
    this.anchorRect = null;
    this.align = 'start';
    this.disposeDismiss = null;
    this.onResize = () => this.reposition();
  }

  /**
   * @param {{anchorElement?: HTMLElement|null, anchorRect?: {left: number, top: number, right?: number, bottom?: number}|null,
   *          align?: 'start'|'end', ignore?: HTMLElement[]}} [options]
   *   Without an anchor the popover is centred near the top of the viewport.
   */
  open({ anchorElement = null, anchorRect = null, align = 'end', ignore = [] } = {}) {
    if (this.isOpen) return;
    this.anchorElement = anchorElement;
    this.anchorRect = anchorRect;
    this.align = align;
    overlayRoot().append(this.element);
    this.element.hidden = false;
    this.isOpen = true;
    this.reposition();
    const inside = [this.element, ...ignore];
    if (anchorElement) inside.push(anchorElement);
    this.disposeDismiss = dismissOnOutside(inside, () => this.close());
    window.addEventListener('resize', this.onResize);
    this.emit('open');
  }

  reposition() {
    if (!this.isOpen) return;
    const rect = this.anchorElement?.getBoundingClientRect() ?? this.anchorRect;
    placeNear(this.element, rect, { align: this.align });
  }

  close() {
    if (!this.isOpen) return;
    this.isOpen = false;
    this.disposeDismiss?.();
    this.disposeDismiss = null;
    window.removeEventListener('resize', this.onResize);
    this.element.hidden = true;
    this.element.remove();
    this.emit('close');
  }
}
