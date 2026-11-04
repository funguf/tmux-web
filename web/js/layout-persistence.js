/**
 * Layout persistence and user preferences.
 *
 * The layout document (opaque to the backend) is
 *   { schema: 1, dockview: api.toJSON(), maximizedPanelId: "p-…"|null, prefs: {...} }
 * and is stored with PUT /api/layout. Saves are debounced (300 ms), identical
 * serialisations are skipped, failures are retried, and pagehide flushes with
 * fetch(..., {keepalive: true}). Layout events from other clients are never applied
 * here; the app only offers a [Load] toast.
 *
 * Events: "prefs" (prefs, changedKey), "saved" (version).
 */
import { EventHub } from './emitter.js';
import { toast } from './toast.js';
import { clamp, errorMessage, uid } from './dom.js';

export const SCHEMA_VERSION = 1;
export const DEFAULT_PREFS = Object.freeze({ fontSize: 13, copyOnSelect: true, webgl: true });
export const FONT_SIZE_MIN = 8;
export const FONT_SIZE_MAX = 32;
const RETRY_DELAY_MS = 5000;

/** Merge untrusted prefs over `base`, keeping only valid values. */
export function sanitizePrefs(raw, base = DEFAULT_PREFS) {
  const prefs = { ...base };
  if (raw && typeof raw === 'object') {
    if (typeof raw.fontSize === 'number' && Number.isFinite(raw.fontSize)) {
      prefs.fontSize = clamp(Math.round(raw.fontSize), FONT_SIZE_MIN, FONT_SIZE_MAX);
    }
    if (typeof raw.copyOnSelect === 'boolean') prefs.copyOnSelect = raw.copyOnSelect;
    if (typeof raw.webgl === 'boolean') prefs.webgl = raw.webgl;
  }
  return prefs;
}

export class LayoutPersistence extends EventHub {
  /**
   * @param {import('./api.js').Api} api
   * @param {{debounceMs?: number}} [options]
   */
  constructor(api, { debounceMs = 300 } = {}) {
    super();
    this.api = api;
    this.debounceMs = debounceMs;
    /** identifies this page load in PUT /api/layout and the "layout" event */
    this.clientId = uid('c-', 6);
    this.prefs = { ...DEFAULT_PREFS };
    this.version = 0;
    /** @type {string|null} serialisation of the last document known to be on the server */
    this.lastSaved = null;
    /** @type {(() => {dockview: any, maximizedPanelId: string|null}) | null} */
    this.snapshotProvider = null;
    this.timer = null;
    this.retryTimer = null;
    this.suspended = 0;
    this.inflightText = null;
    this.failing = false;
  }

  /** Provide the function that captures the current workspace state. */
  setSnapshotProvider(fn) {
    this.snapshotProvider = fn;
  }

  /** GET /api/layout. Resolves to {version, updated_at, layout}. */
  async load() {
    const result = await this.api.getLayout();
    this.version = result?.version ?? 0;
    return result;
  }

  /** Adopt the prefs stored in a layout document. */
  applyDocumentPrefs(doc) {
    this.#replacePrefs(sanitizePrefs(doc?.prefs, DEFAULT_PREFS), null);
  }

  /** Change one preference (persisted with the layout). */
  setPref(key, value) {
    const next = sanitizePrefs({ ...this.prefs, [key]: value }, this.prefs);
    if (next[key] === this.prefs[key]) return;
    this.#replacePrefs(next, key);
    this.schedule();
  }

  #replacePrefs(next, changedKey) {
    const changed = Object.keys(next).filter((key) => next[key] !== this.prefs[key]);
    this.prefs = next;
    if (changedKey) this.emit('prefs', this.prefs, changedKey);
    else for (const key of changed) this.emit('prefs', this.prefs, key);
  }

  /** Stop saving (e.g. while a layout is being restored). Calls nest. */
  suspend() {
    this.suspended += 1;
    this.#clearTimer();
  }

  resume() {
    this.suspended = Math.max(0, this.suspended - 1);
  }

  /** Request a save in `debounceMs` (coalesces bursts of changes). */
  schedule() {
    if (this.suspended > 0) return;
    this.#clearTimer();
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.save();
    }, this.debounceMs);
  }

  #clearTimer() {
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
  }

  /** The document that would be saved right now. */
  buildDocument() {
    if (!this.snapshotProvider) throw new Error('layout persistence has no snapshot provider');
    const { dockview, maximizedPanelId } = this.snapshotProvider();
    return { schema: SCHEMA_VERSION, dockview, maximizedPanelId: maximizedPanelId ?? null, prefs: { ...this.prefs } };
  }

  /** Treat the current workspace state as already saved (after load / restore). */
  markSaved() {
    try {
      this.lastSaved = JSON.stringify(this.buildDocument());
    } catch (err) {
      console.error('tmux-web: cannot serialise layout', err);
    }
  }

  /**
   * Save now unless the serialised layout equals the last saved one.
   * @returns {Promise<boolean>} true when a PUT succeeded
   */
  async save({ keepalive = false } = {}) {
    this.#clearTimer();
    if (this.suspended > 0 || !this.snapshotProvider) return false;
    let doc;
    let text;
    try {
      doc = this.buildDocument();
      text = JSON.stringify(doc);
    } catch (err) {
      console.error('tmux-web: cannot serialise layout', err);
      return false;
    }
    if (text === this.lastSaved || text === this.inflightText) return false;
    this.inflightText = text;
    try {
      const result = await this.api.putLayout(doc, this.clientId, { keepalive });
      this.lastSaved = text;
      this.version = result?.version ?? this.version;
      this.failing = false;
      this.emit('saved', this.version);
      return true;
    } catch (err) {
      this.#reportFailure(err);
      // A client error (too large, bad request, auth) will not fix itself: wait for the next change.
      const clientError = err?.status >= 400 && err.status < 500;
      if (!clientError) this.#scheduleRetry();
      return false;
    } finally {
      if (this.inflightText === text) this.inflightText = null;
    }
  }

  /** Fire-and-forget save for pagehide / visibilitychange (uses keepalive). */
  flush() {
    void this.save({ keepalive: true });
  }

  #reportFailure(err) {
    if (this.failing) return;
    this.failing = true;
    toast.error(`Could not save the layout: ${errorMessage(err)}`);
  }

  #scheduleRetry() {
    if (this.retryTimer !== null) return;
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      void this.save();
    }, RETRY_DELAY_MS);
  }
}
