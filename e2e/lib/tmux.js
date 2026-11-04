/**
 * An isolated tmux: its own socket (`tmw-e2e-<random hex>`), no user configuration, plain `sh`
 * in every pane. Every tmux command goes through a wrapper script that refuses to run unless
 * the socket is a `tmw-e2e-*` one, so nothing here can reach the operator's tmux server.
 * tmux-web itself is pointed at the same wrapper (`--tmux-bin`), so a server that tmux-web
 * starts (when the page asks for one after `kill-server`) is configured exactly like the seeded one.
 */
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { sleepSync } from './wait.js';

/**
 * Panes run a plain, non-login `sh`: a login shell would run this machine's profile scripts.
 * The status line shows the whole session name (the default cuts it at 10 characters), which
 * is how the tests see which session a terminal is attached to.
 */
const TMUX_CONF = [
  'set-option -g default-shell /bin/sh',
  'set-option -g default-command "exec /bin/sh"',
  'set-option -g status-left-length 80',
  '',
].join('\n');

const WRAPPER = `#!/bin/sh
# tmux for the e2e tests: only ever a tmw-e2e-* socket, no user configuration, plain sh in the panes.
case "$1:$2" in
  -L:tmw-e2e-*) ;;
  *) echo "e2e tmux wrapper: refusing to run without -L tmw-e2e-*: $*" >&2; exit 97 ;;
esac
unset TMUX TMUX_PANE
exec tmux -f '@CONF@' "$@"
`;

/** Environment of every process the harness starts: a short allow-list, not the operator's. */
export function cleanEnv(home, extra = {}) {
  return {
    PATH: process.env.PATH ?? '/usr/local/bin:/usr/bin:/bin',
    HOME: home,
    SHELL: '/bin/sh',
    LANG: 'C.UTF-8',
    LC_ALL: 'C.UTF-8',
    ...extra,
  };
}

/** Single-quote `text` for a POSIX shell. */
export function shellQuote(text) {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(text) ? text : `'${text.replaceAll("'", `'\\''`)}'`;
}

export class IsolatedTmux {
  /**
   * @param {{root: string, home: string}} options root: the test's temp dir; home: HOME of tmux
   */
  constructor({ root, home }) {
    this.socket = `tmw-e2e-${randomBytes(4).toString('hex')}`;
    this.home = home;
    this.env = cleanEnv(home);
    const binDir = path.join(root, 'bin');
    fs.mkdirSync(binDir, { recursive: true });
    const conf = path.join(root, 'tmux.conf');
    fs.writeFileSync(conf, TMUX_CONF);
    /** The wrapper script: what tmux-web gets as `--tmux-bin`. */
    this.bin = path.join(binDir, 'tmux');
    fs.writeFileSync(this.bin, WRAPPER.replace('@CONF@', conf), { mode: 0o755 });
    fs.chmodSync(this.bin, 0o755);
  }

  /** `tmux -L <socket> <args>`: {status, stdout, stderr}. */
  try(...args) {
    const result = spawnSync(this.bin, ['-L', this.socket, ...args.map(String)], {
      cwd: this.home,
      env: this.env,
      encoding: 'utf8',
      timeout: 15_000,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return {
      status: result.status,
      stdout: result.stdout ?? '',
      stderr: result.stderr ?? '',
      error: result.error ?? null,
    };
  }

  /** Like try(), but throws unless tmux exits 0. Resolves to stdout. */
  run(...args) {
    const result = this.try(...args);
    if (result.error || result.status !== 0) {
      throw new Error(
        `tmux ${args.join(' ')} failed (${result.error ? result.error.message : `exit ${result.status}`}): ${result.stderr.trim()}`,
      );
    }
    return result.stdout;
  }

  /** `new-session -d` (starts the server when needed). Resolves to the session id. */
  newSession(name, { cwd, width = 120, height = 30, command = [] } = {}) {
    const args = ['new-session', '-d', '-s', name, '-x', width, '-y', height, '-P', '-F', '#{session_id}'];
    if (cwd) args.push('-c', cwd);
    args.push(...command);
    return this.run(...args).trim();
  }

  /** Names of all sessions, sorted; [] when no server runs. */
  sessions() {
    const result = this.try('list-sessions', '-F', '#{session_name}');
    if (result.status !== 0) return [];
    return result.stdout.split('\n').filter(Boolean).sort();
  }

  /** `display-message -p` of a format in the context of a target ("=name:" is session name's window). */
  format(target, format) {
    return this.run('display-message', '-p', '-t', target, format).trim();
  }

  /** {width, height} of the session's current window. */
  windowSize(session) {
    const [width, height] = this.format(`=${session}:`, '#{window_width} #{window_height}').split(' ').map(Number);
    return { width, height };
  }

  /** {width, height} of the (single) attached client of the session, or null. */
  clientSize(session) {
    const out = this.run('list-clients', '-t', `=${session}`, '-F', '#{client_width} #{client_height}').trim();
    const first = out.split('\n').filter(Boolean)[0];
    if (!first) return null;
    const [width, height] = first.split(' ').map(Number);
    return { width, height };
  }

  /** How many tmux clients are attached to the session. */
  clientCount(session) {
    const result = this.try('list-clients', '-t', `=${session}`, '-F', '#{client_pid}');
    return result.status === 0 ? result.stdout.split('\n').filter(Boolean).length : 0;
  }

  /** Types `text` literally into a pane (not followed by Enter). */
  typeText(target, text) {
    this.run('send-keys', '-t', target, '-l', text);
  }

  /** Types `text` into a pane and presses Enter. */
  typeLine(target, text) {
    this.typeText(target, text);
    this.run('send-keys', '-t', target, 'Enter');
  }

  /** The pane's visible text. */
  capture(target) {
    return this.run('capture-pane', '-p', '-t', target);
  }

  hasServer() {
    return this.try('list-sessions').status === 0;
  }

  /** True once clients can no longer reach a server (right after kill-server tmux still answers). */
  serverGone() {
    const result = this.try('list-sessions');
    if (result.error) return true;
    if (result.status === 0) return false;
    return /no server running|error connecting to/.test(result.stderr);
  }

  waitGone(timeout = 5000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      if (this.serverGone()) return true;
      sleepSync(25);
    }
    return this.serverGone();
  }

  killServer() {
    this.try('kill-server');
  }

  socketFile() {
    return path.join('/tmp', `tmux-${process.getuid()}`, this.socket);
  }

  /** Kills the server and removes its dead socket file. Safe to call repeatedly. */
  cleanup() {
    this.killServer();
    this.waitGone();
    try {
      const file = this.socketFile();
      if (fs.lstatSync(file).isSocket()) fs.unlinkSync(file);
    } catch {
      // no socket file: nothing to remove
    }
  }
}
