/**
 * A stand-in for Claude Code, for the snooze tests: an executable shell script called `claude`
 * (so the process name is `claude`) that registers itself in `$CLAUDE_CONFIG_DIR/sessions/<pid>.json`,
 * deletes that file on SIGTERM and otherwise sleeps. The real `claude` is never started: the
 * session's shell finds this script first on its PATH.
 */
import fs from 'node:fs';
import path from 'node:path';
import { shellQuote } from './tmux.js';

const SCRIPT = `#!/bin/sh
dir="\${CLAUDE_CONFIG_DIR:?}"
mkdir -p "$dir/sessions"
id=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--resume" ]; then id="$2"; fi
  shift
done
status=$(cat "$dir/fake-status" 2>/dev/null || echo idle)
file="$dir/sessions/$$.json"
printf '{"pid":%s,"sessionId":"%s","status":"%s","kind":"interactive","cwd":"%s"}' "$$" "$id" "$status" "$PWD" > "$file"
trap 'rm -f "$file"; exit 0' TERM
while :; do sleep 1; done
`;

export class FakeClaude {
  /** @param {import('./env.js').Env} env */
  constructor(env) {
    this.env = env;
    this.binDir = path.join(env.root, 'fake-claude-bin');
    fs.mkdirSync(this.binDir, { recursive: true });
    this.script = path.join(this.binDir, 'claude');
    fs.writeFileSync(this.script, SCRIPT, { mode: 0o755 });
    fs.chmodSync(this.script, 0o755);
    fs.mkdirSync(env.claudeDir, { recursive: true });
  }

  /** What the fake reports as its status in the sessions file ("idle" or "busy"). */
  setStatus(status) {
    fs.writeFileSync(path.join(this.env.claudeDir, 'fake-status'), status);
  }

  /**
   * A session whose interactive shell (the pane's root process) has the fake first on PATH and
   * CLAUDE_CONFIG_DIR set to the test's.
   */
  newSession(name) {
    this.env.tmux.newSession(name, { cwd: this.env.dir(name) });
    this.env.tmux.typeLine(
      `=${name}:`,
      `export PATH=${shellQuote(this.binDir)}:$PATH CLAUDE_CONFIG_DIR=${shellQuote(this.env.claudeDir)}`,
    );
  }

  /** Types the fake into the session's shell, as the operator starts Claude. */
  start(name, conversation) {
    this.env.tmux.typeLine(`=${name}:`, `${shellQuote(this.script)} --resume ${conversation}`);
  }

  /** Pids of the fakes that registered themselves and are still alive. */
  pids() {
    const dir = path.join(this.env.claudeDir, 'sessions');
    let names = [];
    try {
      names = fs.readdirSync(dir);
    } catch {
      return [];
    }
    return names
      .map((file) => Number(file.replace(/\.json$/, '')))
      .filter((pid) => Number.isInteger(pid) && pid > 0 && fs.existsSync(`/proc/${pid}`))
      .sort((a, b) => a - b);
  }

  /** The conversation ids of the live fakes, read from their command lines. */
  conversations() {
    return this.pids()
      .map((pid) => {
        try {
          const argv = fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0');
          const at = argv.indexOf('--resume');
          return at >= 0 ? argv[at + 1] : null;
        } catch {
          return null;
        }
      })
      .filter(Boolean);
  }
}
