/**
 * Small DOM helpers shared by every component. Text always goes through textContent,
 * never innerHTML, so session names, paths and server messages cannot inject markup.
 */
import { svgIcon } from './icons.js';

/**
 * Create an element.
 *
 * props: class, text, testid, title, data ({k: v} -> data-k), attrs ({name: value}),
 * style ({prop: value}), on ({event: handler}); any other key becomes an attribute
 * (`true` -> empty attribute, false/null/undefined -> skipped).
 *
 * @param {string} tag
 * @param {Record<string, any>} [props]
 * @param {...(Node|string|null|false|undefined|Array<any>)} children
 * @returns {HTMLElement}
 */
export function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    switch (key) {
      case 'class':
        node.className = value;
        break;
      case 'text':
        node.textContent = String(value);
        break;
      case 'testid':
        node.setAttribute('data-testid', value);
        break;
      case 'data':
        for (const [k, v] of Object.entries(value)) {
          if (v !== undefined && v !== null) node.dataset[k] = String(v);
        }
        break;
      case 'attrs':
        for (const [k, v] of Object.entries(value)) {
          if (v !== undefined && v !== null && v !== false) node.setAttribute(k, v === true ? '' : String(v));
        }
        break;
      case 'style':
        Object.assign(node.style, value);
        break;
      case 'on':
        for (const [event, handler] of Object.entries(value)) node.addEventListener(event, handler);
        break;
      default:
        node.setAttribute(key, value === true ? '' : String(value));
    }
  }
  append(node, children);
  return node;
}

/** Append strings / nodes (arrays are flattened, null-ish values skipped). */
export function append(parent, children) {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (Array.isArray(child)) append(parent, child);
    else parent.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return parent;
}

/** Remove all children. */
export function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
  return node;
}

/**
 * A <button> showing only an icon, with a tooltip and an accessible name.
 * @param {{icon: string, title: string, testid?: string, className?: string, onClick?: (ev: MouseEvent) => void, size?: number}} options
 */
export function iconButton({ icon, title, testid, className = '', onClick, size = 16 }) {
  const button = el('button', {
    type: 'button',
    class: `icon-btn ${className}`.trim(),
    title,
    'aria-label': title,
    testid,
  });
  button.append(svgIcon(icon, { size }));
  if (onClick) button.addEventListener('click', onClick);
  return button;
}

/** Replace the icon inside an iconButton and refresh its tooltip. */
export function setButtonIcon(button, icon, title, size = 16) {
  const old = button.querySelector('svg');
  const next = svgIcon(icon, { size });
  if (old) old.replaceWith(next);
  else button.prepend(next);
  if (title) {
    button.title = title;
    button.setAttribute('aria-label', title);
  }
}

/** Random lowercase hex string of `bytes` bytes (works in insecure contexts too). */
export function randomHex(bytes = 4) {
  const array = new Uint8Array(bytes);
  crypto.getRandomValues(array);
  return Array.from(array, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Id with a prefix, e.g. uid('p-') -> "p-1a2b3c4d". */
export function uid(prefix = '', bytes = 4) {
  return `${prefix}${randomHex(bytes)}`;
}

export function isMac() {
  return /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent || '');
}

export function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

export function plural(count, word, pluralWord = `${word}s`) {
  return `${count} ${count === 1 ? word : pluralWord}`;
}

/**
 * Debounce `fn`. The returned function has cancel() and flush().
 * @template {(...args: any[]) => void} F
 * @param {F} fn
 * @param {number} ms
 */
export function debounce(fn, ms) {
  let timer = null;
  let pendingArgs = null;
  const run = () => {
    timer = null;
    const args = pendingArgs;
    pendingArgs = null;
    if (args) fn(...args);
  };
  const debounced = (...args) => {
    pendingArgs = args;
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(run, ms);
  };
  debounced.cancel = () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    pendingArgs = null;
  };
  debounced.flush = () => {
    if (timer !== null) {
      clearTimeout(timer);
      run();
    }
  };
  return debounced;
}

/** The container that holds modals, popovers and menus (outside the app layout). */
export function overlayRoot() {
  let root = document.getElementById('overlay-root');
  if (!root) {
    root = el('div', { id: 'overlay-root' });
    document.body.append(root);
  }
  return root;
}

/**
 * Close something when the user presses Escape or clicks outside of it.
 * @param {{contains: (target: Node) => boolean}[]} inside elements that count as "inside"
 * @param {() => void} onDismiss
 * @returns {() => void} disposer
 */
export function dismissOnOutside(inside, onDismiss) {
  const onPointerDown = (ev) => {
    if (inside.some((node) => node.contains(ev.target))) return;
    onDismiss();
  };
  const onKeyDown = (ev) => {
    if (ev.key === 'Escape') {
      ev.preventDefault();
      ev.stopPropagation();
      onDismiss();
    }
  };
  document.addEventListener('pointerdown', onPointerDown, true);
  document.addEventListener('keydown', onKeyDown, true);
  return () => {
    document.removeEventListener('pointerdown', onPointerDown, true);
    document.removeEventListener('keydown', onKeyDown, true);
  };
}

/**
 * Place a fixed-position element near an anchor rectangle (below it by default),
 * keeping it inside the viewport.
 * @param {HTMLElement} node
 * @param {{left: number, top: number, right?: number, bottom?: number} | null} anchor viewport rect, or null to centre
 * @param {{align?: 'start'|'end', gap?: number, margin?: number}} [options]
 */
export function placeNear(node, anchor, { align = 'start', gap = 4, margin = 8 } = {}) {
  node.style.position = 'fixed';
  const vw = document.documentElement.clientWidth;
  const vh = document.documentElement.clientHeight;
  const width = node.offsetWidth;
  const height = node.offsetHeight;
  let left;
  let top;
  if (!anchor) {
    left = (vw - width) / 2;
    top = Math.max(margin, vh * 0.14);
  } else {
    const anchorRight = anchor.right ?? anchor.left;
    const anchorBottom = anchor.bottom ?? anchor.top;
    left = align === 'end' ? anchorRight - width : anchor.left;
    top = anchorBottom + gap;
    if (top + height > vh - margin) top = Math.max(margin, anchor.top - gap - height);
  }
  left = clamp(left, margin, Math.max(margin, vw - width - margin));
  top = clamp(top, margin, Math.max(margin, vh - height - margin));
  node.style.left = `${Math.round(left)}px`;
  node.style.top = `${Math.round(top)}px`;
}

/** Validate a tmux session name with the same rules as the server (§6.2). */
export function validateSessionName(raw) {
  const name = raw ?? '';
  if (name.length === 0) return 'Enter a session name';
  if (name.length > 100) return 'At most 100 characters';
  if (name !== name.trim()) return 'No leading or trailing spaces';
  if (name.startsWith('-')) return 'Must not start with "-"';
  if (/[:.]/.test(name)) return 'Must not contain ":" or "."';
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(name)) return 'No control characters';
  return null;
}

/** Message text of any thrown value. */
export function errorMessage(err) {
  if (err && typeof err === 'object' && 'message' in err && err.message) return String(err.message);
  return String(err);
}
