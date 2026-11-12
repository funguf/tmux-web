/**
 * Actions on sessions shared by the picker and the empty state.
 */
import { errorMessage, validateSessionName } from './dom.js';
import { toast } from './toast.js';

const BYTES_PER_MB = 1024 * 1024;

/**
 * Stop Claude in a session and remember its conversations (POST api/sessions/snooze).
 * Shows a toast either way.
 *
 * @param {{api: any}} services
 * @param {string} name
 */
export async function snoozeSession({ api }, name) {
  try {
    const result = await api.snoozeSession(name);
    const megabytes = Math.round((result?.freed_bytes ?? 0) / BYTES_PER_MB);
    toast.success(`Snoozed \u201c${name}\u201d \u00b7 freed ${megabytes} MB`);
  } catch (err) {
    toast.error(errorMessage(err));
  }
}

/**
 * Reopen the snoozed conversations of a session (POST api/sessions/wake).
 *
 * @param {{api: any}} services
 * @param {string} name
 */
export async function wakeSession({ api }, name) {
  try {
    await api.wakeSession(name);
    toast.success(`Woke \u201c${name}\u201d`);
  } catch (err) {
    toast.error(errorMessage(err));
  }
}

/**
 * Create a tmux session (POST /api/sessions) and open it in a panel.
 * An already existing session is simply opened.
 *
 * @param {{api: any, store: any, workspace: any}} services
 * @param {string} name
 * @param {object} [placement] options for Workspace.openSession
 * @returns {Promise<{ok: true} | {ok: false, message: string}>}
 */
export async function createAndOpenSession({ api, store, workspace }, name, placement = {}) {
  const problem = validateSessionName(name);
  if (problem) return { ok: false, message: problem };
  const known = store.session(name);
  if (known && store.isRunning()) {
    toast.info(`Session \u201c${name}\u201d already exists; opened it`);
    workspace.openSession({ name, id: known.id }, placement);
    return { ok: true };
  }
  try {
    const created = await api.createSession(name);
    workspace.openSession({ name: created?.name ?? name, id: created?.id ?? null }, placement);
    return { ok: true };
  } catch (err) {
    if (err?.code === 'exists') {
      toast.info(`Session “${name}” already exists; opened it`);
      workspace.openSession({ name }, placement);
      return { ok: true };
    }
    return { ok: false, message: errorMessage(err) };
  }
}
