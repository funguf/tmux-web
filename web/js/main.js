/**
 * Bootstrap: wires the services together, restores the saved layout, then starts the
 * live event stream.
 */
import { Api, ApiError } from './api.js';
import { ContextMenu } from './context-menu.js';
import { EmptyState } from './empty-state.js';
import { errorMessage } from './dom.js';
import { EventsClient, BACKOFF_MS } from './events-client.js';
import { dragHasFiles } from './file-upload.js';
import { LayoutPersistence } from './layout-persistence.js';
import { LinkTooltip } from './link-tooltip.js';
import { KillDialog } from './kill-dialog.js';
import { RenameDialog } from './rename-dialog.js';
import { startTmuxOnFirstStatus } from './server-actions.js';
import { SessionPicker } from './session-picker.js';
import { SettingsPopover } from './settings-popover.js';
import { StatusStore } from './status-store.js';
import { installTestHook } from './test-hook.js';
import { toast } from './toast.js';
import { TopBar } from './top-bar.js';
import { Workspace } from './workspace.js';

const SCHEMA_VERSION = 1;

/**
 * Resolve after `count` animation frames (lets dockview finish its layout work), or after
 * 300 ms at the latest: a page opened in a background tab never gets animation frames.
 */
function nextFrames(count = 2) {
  const frames = new Promise((resolve) => {
    const step = (remaining) => requestAnimationFrame(() => (remaining > 1 ? step(remaining - 1) : resolve()));
    step(count);
  });
  return Promise.race([frames, new Promise((resolve) => setTimeout(resolve, 300))]);
}

const api = new Api();
const store = new StatusStore();
const persistence = new LayoutPersistence(api);
const registry = new Map();

/** Everything components may need; filled in below, read lazily by the components. */
const services = {
  api,
  store,
  persistence,
  registry,
  contextMenu: new ContextMenu(),
  linkTooltip: new LinkTooltip(),
};

let markReady;
const ready = new Promise((resolve) => {
  markReady = resolve;
});

const workspaceWrap = document.getElementById('workspace-wrap');
const workspace = new Workspace(document.getElementById('workspace'), services);
services.workspace = workspace;
services.killDialog = new KillDialog(services);
services.picker = new SessionPicker(services);
services.settings = new SettingsPopover(services);
services.renameDialog = new RenameDialog(services);

new TopBar(document.getElementById('top-bar'), services);
const emptyState = new EmptyState(workspaceWrap, services);

installTestHook({ ready, workspace, registry });

// ---- state flowing in from the server -------------------------------------------------------

const events = new EventsClient(api);
events.on('status', (frame) => store.applyStatus(frame));
events.on('layout', (frame) => onRemoteLayout(frame));
events.on('connection', ({ connected }) => {
  store.setEventsConnected(connected);
  if (connected && !store.info) void loadInfo();
});
store.on('status', (status) => workspace.syncWithStatus(status));
// Opening the page asks for tmux: if the first status says it is down, start it (this once).
startTmuxOnFirstStatus(services);

async function loadInfo() {
  try {
    store.setInfo(await api.info());
  } catch (err) {
    console.warn('tmux-web: /api/info failed', err);
  }
}

async function loadInitialState() {
  const results = await Promise.allSettled([api.info(), api.status()]);
  const [info, status] = results;
  if (info.status === 'fulfilled') store.setInfo(info.value);
  if (status.status === 'fulfilled') store.applyStatus(status.value);
  const failed = results.find((result) => result.status === 'rejected');
  if (failed) toast.error(`Could not reach the tmux-web server: ${errorMessage(failed.reason)}`);
}

// ---- layout ------------------------------------------------------------------------------------

persistence.setSnapshotProvider(() => workspace.serialize());
workspace.on('change', () => persistence.schedule());

/** Apply a stored layout document; returns false when it could not be restored. */
function applyLayoutDocument(doc, options) {
  if (!doc || typeof doc !== 'object') return true;
  persistence.applyDocumentPrefs(doc);
  if (doc.schema !== SCHEMA_VERSION || !doc.dockview) {
    if (doc.dockview !== undefined || doc.schema !== undefined) toast.warn('The saved layout has an unknown format; starting empty');
    return true;
  }
  try {
    workspace.restore(doc, options);
    return true;
  } catch (err) {
    console.error('tmux-web: cannot restore the saved layout', err);
    toast.error(`The saved layout could not be restored (${errorMessage(err)}); starting empty`);
    return false;
  }
}

/** Run a layout replacement without triggering saves, then treat the result as saved. */
async function withoutSaving(work) {
  persistence.suspend();
  try {
    await work();
  } finally {
    await nextFrames(3);
    persistence.markSaved();
    persistence.resume();
  }
}

async function restoreLayout() {
  let result;
  try {
    result = await persistence.load();
  } catch (err) {
    // Saving stays suspended until a load succeeds, so the stored layout is never overwritten.
    persistence.suspend();
    toast.error(`Could not load the saved layout: ${errorMessage(err)}`);
    retryLayoutLoad(0);
    return;
  }
  await withoutSaving(() => applyLayoutDocument(result?.layout, undefined));
}

/** The initial layout request failed: keep trying, and only then allow saving. */
function retryLayoutLoad(attempt) {
  const delay = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
  setTimeout(async () => {
    try {
      const result = await persistence.load();
      if (workspace.panelCount === 0) applyLayoutDocument(result?.layout, undefined);
      else toast.info('The saved layout was not applied because panels are already open');
      await nextFrames(3);
      persistence.markSaved();
      persistence.resume();
    } catch {
      retryLayoutLoad(attempt + 1);
    }
  }, delay);
}

function onRemoteLayout(frame) {
  if (frame.client_id === persistence.clientId) return;
  toast.info('The layout was changed in another window', {
    actions: [{ label: 'Load', testid: 'toast-load-layout', onClick: () => void loadRemoteLayout() }],
  });
}

async function loadRemoteLayout() {
  let result;
  try {
    result = await persistence.load();
  } catch (err) {
    toast.error(`Could not load the layout: ${errorMessage(err)}`);
    return;
  }
  if (!result?.layout) {
    toast.warn('There is no saved layout to load');
    return;
  }
  await withoutSaving(() => applyLayoutDocument(result.layout, { reuse: true }));
}

window.addEventListener('pagehide', () => persistence.flush());
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden') persistence.flush();
});

// A file dropped outside a terminal would make the browser navigate away from the app.
for (const type of ['dragover', 'drop']) {
  window.addEventListener(type, (event) => {
    if (!dragHasFiles(event.dataTransfer)) return;
    event.preventDefault();
    if (type === 'drop') toast.info('Drop files onto a terminal to upload them');
  });
}

// ---- go -----------------------------------------------------------------------------------------------

async function bootstrap() {
  await loadInitialState();
  await restoreLayout();
  emptyState.enable();
  events.start();
}

bootstrap()
  .catch((err) => {
    console.error('tmux-web: startup failed', err);
    toast.error(`Startup failed: ${err instanceof ApiError ? err.message : errorMessage(err)}`);
  })
  .finally(() => markReady());
