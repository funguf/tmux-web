/**
 * The tmux-web process under test: `target/debug/tmux-web -L <sock> --state-dir … serve
 * --listen 127.0.0.1:0 …`, started with a short allow-listed environment and SHELL=/bin/sh.
 * The port is read from the "listening on <addr>" line it logs on stderr.
 */
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { sleep } from './wait.js';

const here = path.dirname(fileURLToPath(import.meta.url));

/** The binary built by `cargo build`. */
export const BINARY = process.env.TMUX_WEB_BIN ?? path.resolve(here, '../../target/debug/tmux-web');

const LISTENING = /listening on ([0-9.:]+)/;
const LOG_LINES = 400;

export class TmuxWebProcess {
  /**
   * @param {{args: string[], serveArgs?: string[], env: Record<string, string>, cwd: string}} options
   *   args: the global arguments, before `serve`; serveArgs: extra `serve` options (`--listen` and
   *   the poll interval are added by start())
   */
  constructor({ args, serveArgs = [], env, cwd }) {
    this.args = args;
    this.serveArgs = serveArgs;
    this.env = env;
    this.cwd = cwd;
    this.child = null;
    this.address = null;
    /** Recent stderr lines, for failure reports. */
    this.logs = [];
  }

  get pid() {
    return this.child?.pid ?? null;
  }

  get running() {
    return this.child !== null && this.child.exitCode === null && this.child.signalCode === null;
  }

  /** `http://127.0.0.1:<port>` */
  get url() {
    if (!this.address) throw new Error('tmux-web is not running');
    return `http://${this.address}`;
  }

  /**
   * Start the process and wait for its "listening on" line.
   * @param {{listen?: string}} [options] listen: `127.0.0.1:<port>`; default an ephemeral port
   */
  async start({ listen = '127.0.0.1:0' } = {}) {
    if (this.running) throw new Error('tmux-web is already running');
    const args = [
      ...this.args,
      'serve',
      '--listen',
      listen,
      '--poll-interval-ms',
      '300',
      ...this.serveArgs,
    ];
    const child = spawn(BINARY, args, { cwd: this.cwd, env: this.env, stdio: ['ignore', 'ignore', 'pipe'] });
    this.child = child;
    this.address = null;

    let pending = '';
    const address = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`tmux-web did not report its address within 15 s\n${this.tail()}`)), 15_000);
      child.stderr.setEncoding('utf8');
      child.stderr.on('data', (chunk) => {
        pending += chunk;
        let newline;
        while ((newline = pending.indexOf('\n')) >= 0) {
          const line = pending.slice(0, newline);
          pending = pending.slice(newline + 1);
          this.logs.push(line);
          if (this.logs.length > LOG_LINES) this.logs.shift();
          const match = LISTENING.exec(line);
          if (match) {
            clearTimeout(timer);
            resolve(match[1]);
          }
        }
      });
      child.once('error', (err) => {
        clearTimeout(timer);
        reject(err);
      });
      child.once('exit', (code, signal) => {
        clearTimeout(timer);
        reject(new Error(`tmux-web exited early (code ${code}, signal ${signal})\n${this.tail()}`));
      });
    });
    try {
      this.address = await address;
    } catch (err) {
      this.kill();
      throw err;
    }
    return this.url;
  }

  /** SIGTERM, then SIGKILL after `grace` ms. Resolves once the process is gone. */
  async stop({ grace = 6000 } = {}) {
    const child = this.child;
    if (!child) return;
    if (this.running) {
      const exited = new Promise((resolve) => child.once('exit', resolve));
      child.kill('SIGTERM');
      const timedOut = await Promise.race([exited.then(() => false), sleep(grace).then(() => true)]);
      if (timedOut) {
        child.kill('SIGKILL');
        await exited;
      }
    }
    this.child = null;
    this.address = null;
  }

  /** Synchronous SIGKILL (exit handlers). */
  kill() {
    try {
      this.child?.kill('SIGKILL');
    } catch {
      // already gone
    }
  }

  /** The recent log, for error messages. */
  tail(lines = 40) {
    return this.logs.slice(-lines).join('\n');
  }
}
