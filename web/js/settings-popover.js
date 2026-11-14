/**
 * Settings popover: terminal font size, copy-on-select, GPU (WebGL) rendering.
 * Preferences are stored with the layout (see layout-persistence.js).
 */
import { el, iconButton } from './dom.js';
import { FONT_SIZE_MAX, FONT_SIZE_MIN } from './layout-persistence.js';
import { Popover } from './popover.js';

export class SettingsPopover {
  /**
   * @param {{persistence: any, api: any, workspace: any}} services
   */
  constructor(services) {
    this.services = services;
    this.popover = new Popover({ className: 'settings-popover', testid: 'settings-popover', label: 'Settings' });

    this.fontDown = iconButton({ icon: 'minus', title: 'Smaller font', testid: 'font-size-dec', className: 'step-btn', size: 14 });
    this.fontUp = iconButton({ icon: 'plus', title: 'Larger font', testid: 'font-size-inc', className: 'step-btn', size: 14 });
    this.fontValue = el('span', { class: 'font-size-value', testid: 'font-size-value', 'aria-live': 'polite' });
    this.copyToggle = el('input', { type: 'checkbox', testid: 'copy-on-select-toggle' });
    this.webglToggle = el('input', { type: 'checkbox', testid: 'webgl-toggle' });
    this.extraKeysSelect = el(
      'select',
      { testid: 'extra-keys-mode', 'aria-label': 'Extra keys' },
      el('option', { value: 'auto', text: 'Auto (touch screens)' }),
      el('option', { value: 'on', text: 'Always' }),
      el('option', { value: 'off', text: 'Never' }),
    );

    this.popover.element.append(
      el('div', { class: 'setting-title', text: 'Settings' }),
      el(
        'div',
        { class: 'setting-row' },
        el('span', { class: 'setting-label', text: 'Font size' }),
        el('span', { class: 'font-stepper' }, this.fontDown, this.fontValue, this.fontUp),
      ),
      el(
        'label',
        { class: 'setting-row', title: 'Copy the selected text to the clipboard as soon as the selection is made' },
        el('span', { class: 'setting-label', text: 'Copy on select' }),
        this.copyToggle,
      ),
      el(
        'label',
        { class: 'setting-row', title: 'Render terminals with WebGL (falls back to the DOM renderer automatically)' },
        el('span', { class: 'setting-label', text: 'GPU rendering' }),
        this.webglToggle,
      ),
      el(
        'label',
        { class: 'setting-row', title: 'The Esc / Tab / Ctrl / arrows bar for phones (this device only)' },
        el('span', { class: 'setting-label', text: 'Extra keys' }),
        this.extraKeysSelect,
      ),
    );

    const prefs = () => this.services.persistence.prefs;
    this.fontDown.addEventListener('click', () => this.services.persistence.setPref('fontSize', prefs().fontSize - 1));
    this.fontUp.addEventListener('click', () => this.services.persistence.setPref('fontSize', prefs().fontSize + 1));
    this.copyToggle.addEventListener('change', () => this.services.persistence.setPref('copyOnSelect', this.copyToggle.checked));
    this.webglToggle.addEventListener('change', () => this.services.persistence.setPref('webgl', this.webglToggle.checked));
    this.extraKeysSelect.addEventListener('change', () => this.services.extraKeys?.setMode(this.extraKeysSelect.value));
    this.services.persistence.on('prefs', () => this.#sync());
    // Escape / outside click: hand the keyboard back to the terminal.
    this.popover.on('close', () => this.services.workspace.focusActive());
    this.#sync();
  }

  get isOpen() {
    return this.popover.isOpen;
  }

  toggle({ anchorElement } = {}) {
    if (this.popover.isOpen) {
      this.popover.close();
      return;
    }
    this.#sync();
    this.popover.open({ anchorElement, align: 'end' });
  }

  close() {
    this.popover.close();
  }

  #sync() {
    const { fontSize, copyOnSelect, webgl } = this.services.persistence.prefs;
    this.fontValue.textContent = String(fontSize);
    this.fontDown.disabled = fontSize <= FONT_SIZE_MIN;
    this.fontUp.disabled = fontSize >= FONT_SIZE_MAX;
    this.copyToggle.checked = copyOnSelect;
    this.webglToggle.checked = webgl;
    this.extraKeysSelect.value = this.services.extraKeys?.mode ?? 'auto';
  }
}
