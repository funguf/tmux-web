/**
 * Clipboard helpers.
 *
 * copyText() uses navigator.clipboard.writeText in secure contexts and falls back to a
 * hidden textarea + document.execCommand('copy') everywhere else (plain http on a LAN
 * address is not a secure context). A failed copy shows a toast.
 */
import { toast } from './toast.js';

let lastFailureToastAt = 0;
const FAILURE_TOAST_INTERVAL_MS = 5000;

function legacyCopy(text) {
  const previouslyFocused = document.activeElement;
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.setAttribute('aria-hidden', 'true');
  area.style.cssText = 'position:fixed;top:0;left:-9999px;width:1px;height:1px;opacity:0;pointer-events:none;';
  document.body.append(area);
  let ok = false;
  try {
    area.select();
    area.setSelectionRange(0, text.length);
    ok = document.execCommand('copy');
  } catch {
    ok = false;
  } finally {
    area.remove();
    if (previouslyFocused instanceof HTMLElement) previouslyFocused.focus({ preventScroll: true });
  }
  return ok;
}

function reportFailure(quiet) {
  if (quiet) return;
  const now = Date.now();
  if (now - lastFailureToastAt < FAILURE_TOAST_INTERVAL_MS) return;
  lastFailureToastAt = now;
  toast.error('Copy failed: the browser blocked clipboard access (try Ctrl+Shift+C)');
}

/**
 * Copy text to the system clipboard.
 * @param {string} text
 * @param {{quiet?: boolean}} [options] quiet: do not toast on failure
 * @returns {Promise<boolean>} whether the text was copied
 */
export async function copyText(text, { quiet = false } = {}) {
  if (typeof text !== 'string' || text.length === 0) return false;
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // fall through to the legacy path (focus / permission problems)
    }
  }
  const ok = legacyCopy(text);
  if (!ok) reportFailure(quiet);
  return ok;
}

/**
 * Read text from the clipboard, if the browser allows it.
 * @returns {Promise<string|null>} null when unavailable or denied
 */
export async function readClipboardText() {
  if (!window.isSecureContext || !navigator.clipboard?.readText) return null;
  try {
    return await navigator.clipboard.readText();
  } catch {
    return null;
  }
}

/** Whether the asynchronous clipboard read API is usable here. */
export function canReadClipboard() {
  return Boolean(window.isSecureContext && navigator.clipboard?.readText);
}

/**
 * Provider for the xterm ClipboardAddon (OSC 52). Remote programs may write to the
 * local clipboard but can never read it: readText always resolves to "".
 */
export const osc52Provider = {
  readText: async () => '',
  writeText: async (_selection, text) => {
    await copyText(text);
  },
};
