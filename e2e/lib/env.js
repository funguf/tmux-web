/**
 * One isolated environment per suite (a test file, or a describe block that needs its own
 * tmux-web or tmux server): a temp dir (state dir, cwds, fake claude, its log and
 * CLAUDE_CONFIG_DIR), a private tmux socket, a tmux-web process and a headless Chromium.
 *
 * Teardown (`dispose`) closes the browser, stops tmux-web, kills the tmux server and removes
 * the temp dir; the same cleanup runs synchronously when the test process exits or is
 * signalled, so a failing or killed run leaves nothing behind.
 */
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { launchBrowser, newContext } from './browser.js';
import { TmuxWebProcess } from './server.js';
import { cleanEnv, IsolatedTmux } from './tmux.js';
import { sleepSync } from './wait.js';

const here = path.dirname(fileURLToPath(import.meta.url));
/** Failure screenshots go here (git-ignored). */
export const RESULTS_DIR = path.resolve(here, '../test-results');

const live = new Set();

function killTagged(tag) {
  const found = spawnSync('pgrep', ['-f', '--', `--tmw-e2e-run=${tag}`], { encoding: 'utf8' });
  for (const line of (found.stdout ?? '').split('\n')) {
    const pid = Number(line);
    if (!Number.isInteger(pid) || pid <= 0 || pid === process.pid) continue;
    for (const target of [-pid, pid]) {
      try {
        process.kill(target, 'SIGKILL');
      } catch {
        // already gone, or not a group leader
      }
    }
  }
}

process.on('exit', () => {
  for (const env of [...live]) env.forceCleanup();
});
for (const [signal, code] of [
  ['SIGINT', 130],
  ['SIGTERM', 143],
  ['SIGHUP', 129],
]) {
  process.on(signal, () => process.exit(code));
}

export class Env {
  /**
   * @param {string} name
   * @param {{serveArgs?: string[]}} [options] serveArgs: extra `serve` options, e.g. `--token`
   */
  constructor(name, { serveArgs = [] } = {}) {
    this.name = name;
    this.root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'tmw-e2e-')));
    this.homeDir = path.join(this.root, 'home');
    this.stateDir = path.join(this.root, 'state');
    this.claudeDir = path.join(this.root, 'claude');
    this.cwdRoot = path.join(this.root, 'cwd');
    for (const dir of [this.homeDir, this.cwdRoot]) fs.mkdirSync(dir, { recursive: true });

    this.tmux = new IsolatedTmux({ root: this.root, home: this.homeDir });
    this.server = new TmuxWebProcess({
      args: [
        '-L',
        this.tmux.socket,
        '--tmux-bin',
        this.tmux.bin,
        '--state-dir',
        this.stateDir,
        '--claude-config-dir',
        this.claudeDir,
        '--log-format',
        'text',
      ],
      serveArgs,
      env: cleanEnv(this.homeDir),
      cwd: this.homeDir,
    });
    this.browser = null;
    this.context = null;
    this.page = null;
    this.disposed = false;
    live.add(this);
  }

  get socket() {
    return this.tmux.socket;
  }

  get url() {
    return this.server.url;
  }

  get uploadsDir() {
    return path.join(this.stateDir, 'uploads');
  }

  /** Finished uploads in `<state-dir>/uploads`, sorted (not the temp file of a write in progress). */
  uploads() {
    try {
      return fs
        .readdirSync(this.uploadsDir)
        .filter((name) => /^img-\d+-[0-9a-f]{8}\.(png|jpg|gif|webp)$/.test(name))
        .sort();
    } catch {
      return [];
    }
  }

  /** A directory for a session's cwd (created; the path is canonical and shell safe). */
  dir(name) {
    const dir = path.join(this.cwdRoot, name);
    fs.mkdirSync(dir, { recursive: true });
    return dir;
  }

  async start() {
    await this.server.start();
    this.browser = await launchBrowser({ tag: this.tmux.socket });
    return this;
  }

  // ---- HTTP ----------------------------------------------------------------------------

  /** A request to tmux-web from Node (with the CSRF header on mutating methods). */
  async api(method, pathname, body) {
    const init = { method, headers: {} };
    if (method !== 'GET') init.headers['X-Tmux-Web'] = '1';
    if (body !== undefined) {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }
    const response = await fetch(new URL(pathname, this.server.url), init);
    const text = await response.text();
    let json = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      json = null;
    }
    return { status: response.status, json, text };
  }

  /** Forget the saved layout (call with no page open). */
  async resetLayout() {
    const reply = await this.api('PUT', '/api/layout', { layout: null, client_id: 'e2e-reset' });
    if (reply.status !== 200) throw new Error(`could not reset the layout: ${reply.status} ${reply.text}`);
  }

  /** The stored layout document. */
  async savedLayout() {
    return (await this.api('GET', '/api/layout')).json;
  }

  // ---- pages -----------------------------------------------------------------------------

  async closePage() {
    const context = this.context;
    this.context = null;
    this.page = null;
    if (context) await context.close().catch(() => {});
  }

  /**
   * Close the previous page, optionally forget the saved layout, open a new context + page on
   * the app and wait until the saved layout has been restored.
   * @param {{resetLayout?: boolean, viewport?: {width: number, height: number}, url?: string, beforeOpen?: (page: any) => unknown}} [options]
   *   beforeOpen: called with the new page before it navigates, to watch or route its requests
   */
  async freshPage({ resetLayout = true, viewport, url, beforeOpen } = {}) {
    await this.closePage();
    if (resetLayout) await this.resetLayout();
    const { context, page } = await newContext(this.browser, viewport ? { viewport } : undefined);
    this.context = context;
    this.page = page;
    await beforeOpen?.(page);
    await this.openApp(page, url);
    return page;
  }

  /** Navigate to the app and wait for window.__tmuxWeb.ready. */
  async openApp(page, url = this.server.url) {
    await page.goto(url, { waitUntil: 'load' });
    await page.waitForFunction(() => window.__tmuxWeb !== undefined, undefined, { timeout: 15_000 });
    await page.evaluate(() => window.__tmuxWeb.ready);
  }

  // ---- reports -----------------------------------------------------------------------------

  /** Screenshot + logs of a failed test (stderr; the screenshot goes to e2e/test-results). */
  async report(label) {
    const lines = [`\n--- e2e diagnostics: ${this.name} / ${label} ---`];
    if (this.page && !this.page.isClosed()) {
      try {
        fs.mkdirSync(RESULTS_DIR, { recursive: true });
        const file = path.join(RESULTS_DIR, `${this.name}-${label.replace(/[^A-Za-z0-9._-]+/g, '_')}.png`);
        await this.page.screenshot({ path: file, timeout: 5000 });
        lines.push(`screenshot: ${file}`);
      } catch (err) {
        lines.push(`screenshot failed: ${err.message}`);
      }
      lines.push(`page errors: ${JSON.stringify(this.page.__errors ?? [])}`);
      lines.push(`console: ${JSON.stringify((this.page.__console ?? []).slice(-15))}`);
    }
    lines.push(`tmux sessions: ${JSON.stringify(this.tmux.sessions())}`);
    lines.push(`tmux-web log (tail):\n${this.server.tail(25)}`);
    console.error(lines.join('\n'));
  }

  /** For node:test: `afterEach((t) => env.reportIfFailed(t))`. */
  async reportIfFailed(t) {
    if (t.passed === false) await this.report(t.name);
  }

  // ---- teardown --------------------------------------------------------------------------

  async dispose() {
    if (this.disposed) return;
    this.disposed = true;
    await this.closePage();
    if (this.browser) await this.browser.close().catch(() => {});
    this.browser = null;
    await this.server.stop();
    this.tmux.cleanup();
    killTagged(this.tmux.socket);
    fs.rmSync(this.root, { recursive: true, force: true });
    live.delete(this);
  }

  /** Synchronous, best-effort version of dispose() for process exit. */
  forceCleanup() {
    this.disposed = true;
    killTagged(this.tmux.socket);
    this.server.kill();
    try {
      this.tmux.cleanup();
    } catch {
      // nothing more to do
    }
    sleepSync(50);
    fs.rmSync(this.root, { recursive: true, force: true });
    live.delete(this);
  }
}

/** Create and start an environment. */
export async function createEnv(name, options) {
  const env = new Env(name, options);
  try {
    return await env.start();
  } catch (err) {
    await env.dispose();
    throw err;
  }
}
