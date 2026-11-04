/**
 * Finger scrolling on touch screens.
 *
 * A panel is a `tmux attach-session` client drawing in xterm's alternate screen, so xterm
 * has no scrollback of its own: the history lives in tmux. A vertical one-finger swipe is
 * turned into "scroll N lines" requests (POST api/sessions/scroll), which move tmux's copy
 * mode. The tmux `mouse` option is not involved.
 *
 * When the program in the pane tracks the mouse (a full-screen app such as Claude Code keeps
 * its own scrollback, tmux only has what came before it), the swipe goes to that program the
 * way a desktop wheel would: as synthetic wheel events that xterm encodes itself.
 *
 * Taps are left alone (they focus the terminal and bring up the keyboard), as are the
 * desktop wheel and mouse.
 */

/** Movement in px before a touch counts as a swipe rather than a tap. */
const SWIPE_THRESHOLD_PX = 8;
/** Largest `lines` the server accepts in one request. */
const MAX_LINES_PER_REQUEST = 1000;

/**
 * @param {HTMLElement} root element that receives the touches (the panel)
 * @param {{
 *   rowHeight: () => number,
 *   scroll: (lines: number) => Promise<unknown>,
 *   mouseTracking: () => boolean,
 *   wheel: (deltaY: number, x: number, y: number) => void,
 * }} options `rowHeight` is the height of one terminal row in px; `scroll` sends the lines
 *   (negative = earlier output) and its failures are ignored; while `mouseTracking()` is
 *   true the finger movement goes to `wheel` instead, as pixels (negative deltaY = up).
 * @returns {() => void} removes the listeners and stops any further request
 */
export function installTouchScroll(root, { rowHeight, scroll, mouseTracking, wheel }) {
  let disposed = false;
  /** Last touch position, or null when no single-finger gesture is being tracked. */
  let last = null;
  let start = { x: 0, y: 0 };
  let swiping = false;
  /** Finger movement not yet converted to whole rows. */
  let pendingPx = 0;
  /** Lines waiting for the request in flight to return. */
  let pendingLines = 0;
  let inFlight = false;

  const reset = () => {
    last = null;
    swiping = false;
    pendingPx = 0;
  };

  const flush = async () => {
    if (inFlight || disposed || pendingLines === 0) return;
    const lines = Math.max(-MAX_LINES_PER_REQUEST, Math.min(MAX_LINES_PER_REQUEST, pendingLines));
    pendingLines -= lines;
    inFlight = true;
    try {
      await scroll(lines);
    } catch {
      // A missed scroll step is not worth a toast; the next swipe tries again.
      pendingLines = 0;
    } finally {
      inFlight = false;
    }
    void flush();
  };

  const onStart = (event) => {
    if (event.touches.length !== 1) {
      reset();
      return;
    }
    const touch = event.touches[0];
    start = { x: touch.clientX, y: touch.clientY };
    last = { ...start };
    swiping = false;
    pendingPx = 0;
  };

  const onMove = (event) => {
    if (event.touches.length !== 1 || !last) {
      reset();
      return;
    }
    const touch = event.touches[0];
    if (!swiping) {
      const dx = touch.clientX - start.x;
      const dy = touch.clientY - start.y;
      if (Math.abs(dy) <= SWIPE_THRESHOLD_PX || Math.abs(dy) <= Math.abs(dx)) return;
      swiping = true;
      // Count from the start of the swipe, not from where it was recognised.
      last = { ...start };
    }
    // From here on the page (and dockview) must not move with the finger.
    if (event.cancelable) event.preventDefault();
    const moved = touch.clientY - last.y;
    last = { x: touch.clientX, y: touch.clientY };
    if (mouseTracking()) {
      // Finger down shows earlier output, which is a wheel up; xterm turns pixels into lines.
      pendingPx = 0;
      if (moved !== 0) wheel(-moved, touch.clientX, touch.clientY);
      return;
    }
    pendingPx += moved;
    const row = rowHeight();
    if (!(row > 0)) return;
    const rows = Math.trunc(pendingPx / row);
    if (rows === 0) return;
    pendingPx -= rows * row;
    // Finger down shows earlier output: negative lines.
    pendingLines -= rows;
    void flush();
  };

  const options = { passive: false, capture: true };
  root.addEventListener('touchstart', onStart, options);
  root.addEventListener('touchmove', onMove, options);
  root.addEventListener('touchend', reset, options);
  root.addEventListener('touchcancel', reset, options);
  return () => {
    disposed = true;
    root.removeEventListener('touchstart', onStart, options);
    root.removeEventListener('touchmove', onMove, options);
    root.removeEventListener('touchend', reset, options);
    root.removeEventListener('touchcancel', reset, options);
  };
}
