/**
 * On-screen extra keys for phones: Esc, Tab, Ctrl, Alt, arrows and a few symbols that the soft
 * keyboard lacks. Keys are sent through xterm as if typed (`term.input`), so they take the same
 * `term.onData` path as real typing. Ctrl and Alt are sticky for the next key, which may also
 * come from the soft keyboard: terminal-panel.js passes its typed data through `transform()`.
 */
import { el } from './dom.js';

const STORAGE_KEY = 'tmux-web.extraKeys';
const MODES = ['auto', 'on', 'off'];
const DOUBLE_TAP_MS = 350;

/** id, label (also the aria-label) and what it does. */
const KEYS = [
  { id: 'esc', label: 'Esc', data: '\x1b' },
  { id: 'tab', label: 'Tab', data: '\t' },
  { id: 'ctrl', label: 'Ctrl', modifier: 'ctrl' },
  { id: 'alt', label: 'Alt', modifier: 'alt' },
  { id: 'left', label: '←', arrow: 'D' },
  { id: 'down', label: '↓', arrow: 'B' },
  { id: 'up', label: '↑', arrow: 'A' },
  { id: 'right', label: '→', arrow: 'C' },
  { id: 'pipe', label: '|', data: '|' },
  { id: 'slash', label: '/', data: '/' },
  { id: 'dash', label: '-', data: '-' },
  { id: 'tilde', label: '~', data: '~' },
  { id: 'ctrlc', label: '^C', data: '\x03' },
];

/** Control character for `char` (Ctrl+char), or the char itself when it has none. */
function controlOf(char) {
  const code = char.charCodeAt(0);
  if (/^[a-z]$/i.test(char)) return String.fromCharCode(code & 0x1f);
  switch (char) {
    case '@':
    case ' ':
      return '\x00';
    case '[':
      return '\x1b';
    case '\\':
      return '\x1c';
    case ']':
      return '\x1d';
    case '^':
      return '\x1e';
    case '_':
      return '\x1f';
    case '?':
      return '\x7f';
    default:
      return char;
  }
}

function readMode() {
  try {
    const value = window.localStorage.getItem(STORAGE_KEY);
    return MODES.includes(value) ? value : 'auto';
  } catch {
    return 'auto';
  }
}

export class ExtraKeys {
  #mode = readMode();
  /** @type {{ctrl: 'off'|'armed'|'locked', alt: 'off'|'armed'|'locked'}} */
  #state = { ctrl: 'off', alt: 'off' };
  #lastTap = { ctrl: 0, alt: 0 };
  #bypass = false;
  #buttons = new Map();
  #coarse = window.matchMedia ? window.matchMedia('(pointer: coarse)') : null;

  /** @param {{workspace: any, registry: Map<string, any>}} services */
  constructor(services, parent = document.getElementById('app')) {
    this.services = services;
    this.element = el('div', {
      class: 'extra-keys',
      testid: 'extra-keys',
      role: 'toolbar',
      'aria-label': 'Extra keys',
    });
    for (const key of KEYS) {
      const button = el('button', {
        type: 'button',
        class: 'extra-key',
        testid: `extra-key-${key.id}`,
        'aria-label': key.label,
        text: key.label,
        data: { key: key.id },
      });
      if (key.modifier) button.setAttribute('aria-pressed', 'false');
      // Never take the focus from the terminal: that would close the on-screen keyboard.
      button.addEventListener('pointerdown', (event) => event.preventDefault());
      button.addEventListener('mousedown', (event) => event.preventDefault());
      button.addEventListener('click', () => this.#press(key));
      this.#buttons.set(key.id, button);
      this.element.append(button);
    }
    parent.append(this.element);
    this.#coarse?.addEventListener?.('change', () => this.#updateVisibility());
    this.#updateVisibility();
  }

  get mode() {
    return this.#mode;
  }

  get visible() {
    return !this.element.hidden;
  }

  setMode(mode) {
    this.#mode = MODES.includes(mode) ? mode : 'auto';
    try {
      window.localStorage.setItem(STORAGE_KEY, this.#mode);
    } catch {
      // storage unavailable: the choice lasts until the page is reloaded
    }
    this.#updateVisibility();
  }

  #updateVisibility() {
    const show = this.#mode === 'on' || (this.#mode === 'auto' && Boolean(this.#coarse?.matches));
    this.element.hidden = !show;
  }

  /**
   * Apply the armed modifiers to text typed on the soft keyboard. Called from the terminal's
   * data path; anything but a single character passes through and keeps the modifiers armed.
   */
  transform(data) {
    if (this.#bypass) return data;
    const { ctrl, alt } = this.#state;
    if (ctrl === 'off' && alt === 'off') return data;
    if (typeof data !== 'string' || [...data].length !== 1) return data;
    let out = data;
    if (ctrl !== 'off') out = controlOf(out);
    if (alt !== 'off') out = `\x1b${out}`;
    this.#consume();
    return out;
  }

  #consume() {
    for (const name of ['ctrl', 'alt']) {
      if (this.#state[name] === 'armed') this.#setState(name, 'off');
    }
  }

  #setState(name, value) {
    this.#state[name] = value;
    const button = this.#buttons.get(name);
    button.setAttribute('aria-pressed', value === 'off' ? 'false' : 'true');
    button.classList.toggle('is-armed', value === 'armed');
    button.classList.toggle('is-locked', value === 'locked');
  }

  #tapModifier(name) {
    const now = performance.now();
    const current = this.#state[name];
    if (current === 'off') {
      this.#setState(name, 'armed');
    } else if (current === 'armed' && now - this.#lastTap[name] < DOUBLE_TAP_MS) {
      this.#setState(name, 'locked');
    } else {
      this.#setState(name, 'off');
    }
    this.#lastTap[name] = now;
  }

  #target() {
    const panel = this.services.workspace?.api?.activePanel;
    const target = panel ? this.services.registry.get(panel.id) : null;
    return target?.term ? target : null;
  }

  #press(key) {
    if (key.modifier) {
      this.#tapModifier(key.modifier);
      return;
    }
    const target = this.#target();
    if (!target) return;
    const term = target.term;
    const textarea = term.textarea;
    const active = document.activeElement;
    const refocus = active === textarea || !active || active === document.body;

    const { ctrl, alt } = this.#state;
    const code = (ctrl !== 'off' ? 4 : 0) + (alt !== 'off' ? 2 : 0);
    let data = key.data;
    if (key.arrow) {
      if (code) data = `\x1b[1;${1 + code}${key.arrow}`;
      else data = term.modes?.applicationCursorKeysMode ? `\x1bO${key.arrow}` : `\x1b[${key.arrow}`;
    } else if (key.id === 'tab' && alt !== 'off') {
      data = '\x1b\t';
    }
    this.#consume();
    this.#bypass = true;
    try {
      term.input(data, true);
    } finally {
      this.#bypass = false;
    }
    if (refocus) target.focus();
  }
}
