/**
 * Single source of truth for server-provided state: /api/info and tmux status.
 * Components read the getters and subscribe to changes.
 *
 * Events (via on()): "info", "status", "connection".
 * subscribe(fn) calls fn(kind) for any of them.
 */
import { EventHub } from './emitter.js';

const KINDS = ['info', 'status', 'connection'];

export class StatusStore extends EventHub {
  constructor() {
    super();
    /** @type {null | {version: string, hostname: string, tmux_socket: string, upload_max_bytes: number, auth: string}} */
    this.info = null;
    /** @type {null | {running: boolean, server: null | {pid: number, start_time: number, socket_path: string, version: string}, sessions: any[], updated_at: number}} */
    this.status = null;
    /** true while the /ws/events socket is connected */
    this.eventsConnected = false;
    this.sessionsByName = new Map();
  }

  /** Call `fn(kind)` on every change. Returns an unsubscribe function. */
  subscribe(fn) {
    const offs = KINDS.map((kind) => this.on(kind, () => fn(kind)));
    return () => offs.forEach((off) => off());
  }

  setInfo(info) {
    this.info = info;
    this.emit('info', info);
  }

  /** @param {any} frame a TmuxStatus (a "status" event frame is accepted as-is) */
  applyStatus(frame) {
    if (!frame || typeof frame !== 'object') return;
    const sessions = Array.isArray(frame.sessions) ? frame.sessions : [];
    this.status = {
      running: Boolean(frame.running),
      server: frame.server ?? null,
      sessions,
      updated_at: typeof frame.updated_at === 'number' ? frame.updated_at : Date.now(),
    };
    this.sessionsByName = new Map(sessions.map((s) => [s.name, s]));
    this.emit('status', this.status);
  }

  setEventsConnected(connected) {
    if (this.eventsConnected === connected) return;
    this.eventsConnected = connected;
    this.emit('connection', connected);
  }

  /** Server identity string "<start_time>-<pid>", or null when no server. */
  serverId() {
    const server = this.status?.server;
    return server ? `${server.start_time}-${server.pid}` : null;
  }

  isRunning() {
    return Boolean(this.status?.running);
  }

  session(name) {
    return this.sessionsByName.get(name);
  }

  /** True when `name` exists on a running tmux server. */
  hasSession(name) {
    return this.isRunning() && this.sessionsByName.has(name);
  }
}
