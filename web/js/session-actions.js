/**
 * Actions on sessions shared by the picker and the empty state.
 */
import { errorMessage, validateSessionName } from './dom.js';
import { toast } from './toast.js';

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
