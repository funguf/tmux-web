/**
 * Custom key handling for xterm (CONTRACT §8.6 items 8, 9 and 13).
 *
 * xterm calls the handler for every key event; returning false stops xterm from
 * processing the key. For paste shortcuts the handler returns false WITHOUT calling
 * preventDefault, so the browser still fires a native `paste` event that xterm handles
 * (bracketed paste) and the panel can intercept for files.
 */
import { isMac } from './dom.js';

/**
 * @param {{
 *   hasSelection: () => boolean,
 *   copySelection: (options?: {clear?: boolean}) => void,
 *   toggleMaximize: () => void,
 * }} actions
 * @returns {(event: KeyboardEvent) => boolean}
 */
export function createKeyHandler(actions) {
  const mac = isMac();
  return (event) => {
    if (event.type !== 'keydown') return true;
    const { ctrlKey, shiftKey, altKey, metaKey } = event;
    const letter = (code, key) => event.code === code || event.key?.toLowerCase() === key;

    // Copy: Ctrl+Shift+C, Ctrl+Insert, Cmd+C (macOS).
    const copyShortcut =
      (ctrlKey && shiftKey && !altKey && !metaKey && letter('KeyC', 'c')) ||
      (ctrlKey && !shiftKey && !altKey && !metaKey && event.key === 'Insert') ||
      (mac && metaKey && !ctrlKey && !altKey && !shiftKey && letter('KeyC', 'c'));
    if (copyShortcut) {
      event.preventDefault();
      actions.copySelection();
      return false;
    }

    // Ctrl+C copies (and clears the selection) instead of sending ^C while text is selected.
    if (ctrlKey && !shiftKey && !altKey && !metaKey && letter('KeyC', 'c') && actions.hasSelection()) {
      event.preventDefault();
      actions.copySelection({ clear: true });
      return false;
    }

    // Paste: Ctrl+V, Ctrl+Shift+V, Cmd+V, Shift+Insert. No preventDefault on purpose.
    const pasteShortcut =
      (ctrlKey && !altKey && !metaKey && letter('KeyV', 'v')) ||
      (metaKey && !ctrlKey && !altKey && letter('KeyV', 'v')) ||
      (shiftKey && !ctrlKey && !altKey && !metaKey && event.key === 'Insert');
    if (pasteShortcut) return false;

    // Ctrl+Shift+Enter toggles maximise of this panel's group.
    if (ctrlKey && shiftKey && !altKey && !metaKey && event.key === 'Enter') {
      event.preventDefault();
      actions.toggleMaximize();
      return false;
    }

    return true;
  };
}
