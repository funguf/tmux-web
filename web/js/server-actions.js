/**
 * Actions on the tmux server itself, shared by the start when the page opens and the empty
 * state's "Start tmux" button.
 */
import { errorMessage } from './dom.js';
import { toast } from './toast.js';

/**
 * Ask the server to start tmux (POST /api/server/start) and say what it did. The server
 * creates its default session (a plain login shell). When tmux runs already it does nothing,
 * and so does this.
 *
 * @param {{api: any}} services
 * @returns {Promise<{ok: true} | {ok: false, message: string}>}
 */
export async function startTmux({ api }) {
  let reply;
  try {
    reply = await api.startServer();
  } catch (err) {
    return { ok: false, message: errorMessage(err) };
  }
  announceStart(reply);
  return { ok: true };
}

/**
 * Toast what a start did. `started: false` needs none: the status stream already shows that
 * tmux runs.
 */
function announceStart(reply) {
  if (!reply?.started) return;
  toast.success(`Started tmux: session ${reply.session}`);
}

/**
 * Start tmux once per page load, when the first status the page receives says that it is not
 * running: opening the page means the user wants tmux. No later status starts it again, so a
 * tmux that is stopped on purpose while the page stays open stays down.
 *
 * @param {{api: any, store: any}} services
 */
export function startTmuxOnFirstStatus(services) {
  const stopListening = services.store.on('status', (status) => {
    stopListening();
    if (status.running) return;
    void startTmux(services).then((result) => {
      if (!result.ok) toast.error(`Could not start tmux: ${result.message}`);
    });
  });
}
