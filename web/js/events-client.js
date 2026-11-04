/**
 * Client for GET /ws/events (CONTRACT §7.1) with automatic reconnect.
 *
 * Emits one event per server frame type ("status", "layout", each with the parsed
 * frame) plus "connection" ({connected}). The server re-sends the full status frame
 * on every (re)connect, so the client never needs to resynchronise by hand.
 */
import { EventHub } from './emitter.js';

/** Reconnect delays in ms; the last value repeats. */
export const BACKOFF_MS = [1000, 2000, 4000, 8000, 10000];

export class EventsClient extends EventHub {
  /** @param {import('./api.js').Api} api */
  constructor(api) {
    super();
    this.api = api;
    this.socket = null;
    this.connected = false;
    this.attempt = 0;
    this.timer = null;
    this.stopped = true;
  }

  start() {
    if (!this.stopped) return;
    this.stopped = false;
    this.#open();
  }

  stop() {
    this.stopped = true;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
      try {
        socket.close();
      } catch {
        // already closed
      }
    }
    this.#setConnected(false);
  }

  #setConnected(connected) {
    if (this.connected === connected) return;
    this.connected = connected;
    this.emit('connection', { connected });
  }

  #open() {
    if (this.stopped) return;
    let socket;
    try {
      socket = new WebSocket(this.api.wsUrl('ws/events'));
    } catch (err) {
      console.error('tmux-web: cannot open events socket', err);
      this.#scheduleReconnect();
      return;
    }
    this.socket = socket;
    // Every callback starts with this check, so a socket that was replaced or stopped earlier
    // cannot change state, emit frames or schedule a reconnect, whatever it still delivers.
    const guarded =
      (callback) =>
      (...args) => {
        if (this.socket !== socket || this.stopped) return;
        callback(...args);
      };
    socket.onopen = guarded(() => {
      this.attempt = 0;
      this.#setConnected(true);
    });
    socket.onmessage = guarded((ev) => {
      if (typeof ev.data !== 'string') return;
      let frame;
      try {
        frame = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (frame && typeof frame.type === 'string') this.emit(frame.type, frame);
    });
    socket.onclose = guarded(() => {
      this.socket = null;
      this.#setConnected(false);
      this.#scheduleReconnect();
    });
    // "error" is always followed by "close"; nothing to do but avoid unhandled noise.
    socket.onerror = guarded(() => {});
  }

  #scheduleReconnect() {
    if (this.stopped || this.timer !== null) return;
    const delay = BACKOFF_MS[Math.min(this.attempt, BACKOFF_MS.length - 1)];
    this.attempt += 1;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.#open();
    }, delay);
  }
}
