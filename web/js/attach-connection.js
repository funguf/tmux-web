/**
 * WebSocket connection of one panel to GET /ws/attach (CONTRACT §7.2).
 *
 * States: idle -> connecting -> open | closed | waiting.
 *   - "closed":  the server ended the attach for a definite reason
 *                (detached, session-closed, server-down, no-session, no-server).
 *                There is no automatic retry; the owner decides when to start() again
 *                (a status event showing the session present, or the user).
 *   - "waiting": a network error or `error` exit. The connection retries by itself
 *                with the backoff 1, 2, 4, 8, 10 s (capped).
 *
 * Events: "state" ({state, reason, message, retryAt, attempt}), "open" ({reconnect}),
 *         "output" (Uint8Array).
 */
import { EventHub } from './emitter.js';
import { BACKOFF_MS } from './events-client.js';

/** Exit reasons after which the connection stays closed. */
export const DEFINITE_REASONS = new Set(['detached', 'session-closed', 'server-down', 'no-session', 'no-server']);
/** Closed reasons that end by themselves once tmux (or the session) is back. */
export const RECOVERABLE_REASONS = new Set(['session-closed', 'server-down', 'no-session', 'no-server']);

const STABLE_OPEN_MS = 5000;

function reasonFromCloseCode(code) {
  if (code === 4404) return 'no-session';
  if (code === 4503) return 'no-server';
  if (code === 1011) return 'error';
  return 'network';
}

export class AttachConnection extends EventHub {
  /**
   * @param {{api: import('./api.js').Api, getSession: () => string, getSize: () => {cols: number, rows: number}}} options
   */
  constructor({ api, getSession, getSize }) {
    super();
    this.api = api;
    this.getSession = getSession;
    this.getSize = getSize;
    /** @type {'idle'|'connecting'|'open'|'closed'|'waiting'} */
    this.state = 'idle';
    this.reason = null;
    this.message = '';
    this.retryAt = 0;
    this.attempt = 0;
    this.socket = null;
    this.exitFrame = null;
    this.openedAt = 0;
    this.timer = null;
    this.hasBeenOpen = false;
  }

  get isOpen() {
    return this.state === 'open';
  }

  /** Open the socket unless one is already open or being opened. */
  start() {
    if (this.state === 'connecting' || this.state === 'open') return;
    this.#clearTimer();
    const { cols, rows } = this.getSize();
    let socket;
    try {
      socket = new WebSocket(this.api.wsUrl('ws/attach', { session: this.getSession(), cols, rows }));
    } catch (err) {
      this.#scheduleRetry('network', String(err));
      return;
    }
    socket.binaryType = 'arraybuffer';
    this.socket = socket;
    this.exitFrame = null;
    this.reason = null;
    this.message = '';
    this.#setState('connecting');

    socket.onopen = () => {
      if (this.socket !== socket) return;
      this.openedAt = Date.now();
      this.#setState('open');
      this.emit('open', { reconnect: this.hasBeenOpen });
      this.hasBeenOpen = true;
    };
    socket.onmessage = (ev) => {
      if (this.socket !== socket) return;
      if (typeof ev.data === 'string') {
        this.#onTextFrame(ev.data);
      } else if (ev.data instanceof ArrayBuffer) {
        this.emit('output', new Uint8Array(ev.data));
      }
    };
    socket.onclose = (ev) => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.#onClosed(ev);
    };
    // "error" is always followed by "close", which carries the decision.
    socket.onerror = () => {};
  }

  /** Close the socket and cancel any retry; the connection returns to "idle". */
  stop() {
    this.#clearTimer();
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
      try {
        socket.close(1000);
      } catch {
        // already closed
      }
    }
    this.openedAt = 0;
    this.attempt = 0;
    if (this.state !== 'idle') this.#setState('idle');
  }

  /** Reconnect right away (skips the backoff timer, or reattaches after `detached`). */
  retryNow() {
    if (this.state !== 'waiting' && this.state !== 'closed') return;
    this.#clearTimer();
    this.attempt = 0;
    this.start();
  }

  /** Send raw input bytes to the PTY. Returns false when not connected. */
  sendInput(bytes) {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) return false;
    this.socket.send(bytes);
    return true;
  }

  /** Tell the server the terminal size changed. Returns false when not connected. */
  sendResize(cols, rows) {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) return false;
    this.socket.send(JSON.stringify({ type: 'resize', cols, rows }));
    return true;
  }

  #onTextFrame(text) {
    let frame;
    try {
      frame = JSON.parse(text);
    } catch {
      return;
    }
    if (frame?.type === 'exit') this.exitFrame = frame;
  }

  #onClosed(ev) {
    const openFor = this.openedAt ? Date.now() - this.openedAt : 0;
    this.openedAt = 0;
    const reason = this.exitFrame?.reason ?? reasonFromCloseCode(ev.code);
    const message = this.exitFrame?.message ?? '';
    if (DEFINITE_REASONS.has(reason)) {
      this.attempt = 0;
      this.reason = reason;
      this.message = message;
      this.#setState('closed');
      return;
    }
    // error / network: retry with backoff; a connection that lived a while resets it.
    if (openFor >= STABLE_OPEN_MS) this.attempt = 0;
    this.#scheduleRetry(reason, message);
  }

  #scheduleRetry(reason, message) {
    const delay = BACKOFF_MS[Math.min(this.attempt, BACKOFF_MS.length - 1)];
    this.attempt += 1;
    this.reason = reason;
    this.message = message;
    this.retryAt = Date.now() + delay;
    this.#setState('waiting');
    this.timer = setTimeout(() => {
      this.timer = null;
      this.start();
    }, delay);
  }

  #clearTimer() {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
  }

  #setState(state) {
    this.state = state;
    this.emit('state', {
      state,
      reason: this.reason,
      message: this.message,
      retryAt: this.retryAt,
      attempt: this.attempt,
    });
  }
}
