# tmux-web — build contract (FROZEN)

This document is the single source of truth for every unit that builds tmux-web.
Sections 4–8 are frozen: if something here is wrong or impossible, stop and report it
to the coordinator instead of improvising a different interface.

## 1. What we are building

One Rust binary, `tmux-web`, that serves a browser UI for the local tmux server.

- **Panels are tmux sessions.** Each panel is an xterm.js terminal attached through a PTY
  running `tmux attach-session -t =<name>`.
- **Docking layout** (dockview-core): drag panels around, stack panels as tabs in one group,
  split groups horizontally or vertically, maximise a group to fill the work area and restore it.
- **Panel title = tmux session name**, exactly, and it follows renames done in tmux.
- **Layout is persisted on the server** (survives closing Chrome and restarting tmux-web).
- **File paste**: paste / drop / pick any file (images included) → it is uploaded to the server →
  its absolute path is pasted into the terminal (Claude Code turns a pasted image path into an
  attachment).
- **Select and copy text** (copy-on-select, Ctrl+Shift+C, Ctrl+C while text is selected,
  context menu, and tmux copy-mode via OSC 52).
- **Ctrl+click (Cmd+click on macOS) opens URLs** — plain-text URLs and OSC 8 hyperlinks.
- **Claude badges**: panes running Claude Code are found by an in-memory scan of the tmux
  server (never written to disk) and badged per session.
- **No crash recovery of its own**: tmux server recovery is left to tmux itself (for example
  tmux-resurrect + tmux-continuum).

## 2. Repository layout and ownership

| Path | Owner |
|---|---|
| `Cargo.toml`, `Cargo.lock`, `.cargo/`, `docs/`, `README.md`, `contrib/`, `web/vendor/`, `vendor-types/` | coordinator — do not edit |
| `src/lib.rs`, `src/util.rs`, `src/tmux.rs` | scaffold (P0); afterwards core |
| `src/procscan.rs`, `src/claude.rs`, `src/snapshot.rs`, `tests/core_*.rs` | core |
| `src/main.rs`, `src/config.rs`, `src/monitor.rs`, `src/server.rs`, `src/pty.rs`, `src/layout.rs`, `src/upload.rs`, `src/security.rs`, `src/assets.rs`, `tests/server_*.rs` | server |
| `web/**` except `web/vendor/` | frontend |
| `e2e/**` | e2e (P2) |

Edit only files you own. If you need a change in a file you do not own, or a new dependency,
stop and report it. `Cargo.lock` may change only as a side effect of building with the frozen
`Cargo.toml`.

## 3. Global rules (every unit)

1. **Never touch the operator's real tmux server.** Every tmux command you run — in tests or by
   hand — uses an isolated socket: `tmux -L tmw-<unit>-<random> …` or `-S <tempdir>/sock`.
   Never run `tmux` without `-L`/`-S`. Every test that starts a tmux server kills it
   (`tmux -L <sock> kill-server`) in a Drop guard / `finally`, including on failure.
2. **Never read or write the real `~/.claude/`** and never start the real `claude` binary.
   Tests point `--claude-config-dir` / `CLAUDE_CONFIG_DIR` at a temp dir and use fake
   `claude` scripts.
3. Bind tests to `127.0.0.1` with port 0. Never bind `0.0.0.0`.
4. Wrap long commands in `timeout` (e.g. `timeout 900 cargo test`). Leave no background
   processes, tmux servers, or temp servers running when you finish.
   Never print the environment (`env`, `printenv`, `set`, `export -p`, `/proc/*/environ` of
   processes you did not start) — it holds the operator's secrets.
5. Cargo uses at most 4 jobs (`.cargo/config.toml`); do not override it.
6. No new dependencies (crates or npm packages) beyond `Cargo.toml` / §8.6 without reporting.
7. Commit on your own branch with explicit paths (`git add <paths>`, never `git add -A` or
   `git add .`). Conventional commit messages with the unit as scope, e.g.
   `feat(core): snapshot store`. Do not push. Do not merge other branches unless told to.
8. Rust: rustfmt defaults; `cargo clippy --all-targets -- -D warnings` clean for your files; no
   `unwrap()`/`expect()` on I/O or user input outside tests; no `todo!()`/`unimplemented!()`
   left in your files when you finish. JS: modern ES modules, no build step, no
   `innerHTML` with untrusted strings (session names, paths, server messages) — use
   `textContent`.

## 4. CLI and configuration (server implements; core consumes the values)

```
tmux-web [GLOBAL] [serve] [SERVE]        # default subcommand is serve
```

| Global option | Env | Default |
|---|---|---|
| `--tmux-bin <PATH>` | `TMUX_WEB_TMUX_BIN` | `tmux` |
| `-L, --tmux-socket-name <NAME>` | `TMUX_WEB_SOCKET_NAME` | none (tmux default server) |
| `-S, --tmux-socket-path <PATH>` | `TMUX_WEB_SOCKET_PATH` | none; conflicts with `-L` |
| `--state-dir <DIR>` | `TMUX_WEB_STATE_DIR` | `$XDG_STATE_HOME/tmux-web`, else `~/.local/state/tmux-web` |
| `--claude-config-dir <DIR>` | `CLAUDE_CONFIG_DIR` | `~/.claude` |
| `--log-format <auto\|text\|json>` | `TMUX_WEB_LOG_FORMAT` | `auto` (json when stderr is not a TTY) |

| Serve option | Env | Default |
|---|---|---|
| `--listen <ADDR:PORT>` | `TMUX_WEB_LISTEN` | `127.0.0.1:7690` |
| `--token <TOKEN>` | `TMUX_WEB_TOKEN` | none |
| `--no-auth` | — | off; required to listen on a non-loopback address without a token |
| `--allow-host <HOST>` (repeatable) | `TMUX_WEB_ALLOW_HOSTS` (comma-separated) | none |
| `--poll-interval-ms <N>` | — | 1500 |
| `--default-session <NAME>` | `TMUX_WEB_DEFAULT_SESSION` | `main` |
| `--max-upload-mb <N>` | — | 25 |
| `--upload-retention-days <N>` | — | 7 |

Log level comes from `RUST_LOG` (default `info`).

## 5. State directory

Directory mode 0700, files 0600. Every write is atomic (`util::atomic_write`).

```
<state-dir>/
  layout.json                               {"version":u64,"updated_at":ms|null,"layout":<any JSON>|null}
  token                                     generated auth token (only when one is generated)
  uploads/img-<unixms>-<8 hex>.<png|jpg|gif|webp>
  uploads/file-<unixms>-<8 hex>-<name>      (any other file; <name> sanitised, <= 100 bytes)
```

## 6. Rust library API (FROZEN)

`src/lib.rs`:

```rust
pub mod assets;
pub mod claude;
pub mod config;
pub mod layout;
pub mod monitor;
pub mod procscan;
pub mod pty;
pub mod security;
pub mod server;
pub mod snapshot;
pub mod tmux;
pub mod upload;
pub mod util;
```

### 6.1 `util` (scaffold implements fully)

```rust
pub fn now_ms() -> i64;                       // unix epoch milliseconds
pub fn now_secs() -> i64;
/// Write to a temp file in the same directory, fsync, set `mode`, rename over `path`.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8], mode: u32) -> std::io::Result<()>;
/// create_dir_all + chmod 0700.
pub fn ensure_private_dir(path: &std::path::Path) -> std::io::Result<()>;
/// Lower-case hex of `n` random bytes (getrandom).
pub fn random_hex(n: usize) -> String;
/// $HOME, falling back to the passwd entry of the current uid, then "/".
pub fn home_dir() -> std::path::PathBuf;
```

### 6.2 `tmux` (scaffold implements fully; core owns afterwards)

```rust
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketSel { Default, Name(String), Path(PathBuf) }

#[derive(Clone, Debug)]
pub struct TmuxConfig { pub bin: PathBuf, pub socket: SocketSel }

impl TmuxConfig {
    /// [] | ["-L", name] | ["-S", path]
    pub fn global_args(&self) -> Vec<String>;
    /// `bin <global args> <args>`, env TMUX, TMUX_PANE and every TMUX_WEB_* removed, SHELL set from the passwd
    /// entry when the environment has none, stdin = null.
    pub fn command(&self, args: &[String]) -> std::process::Command;
    /// Human description for /api/info: "default" | "-L <name>" | "-S <path>".
    pub fn describe(&self) -> String;
}

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("tmux server is not running")]
    NoServer,
    #[error("tmux command failed: {0}")]
    Command(String),
    #[error("could not parse tmux output: {0}")]
    Parse(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub trait TmuxRunner: Send + Sync {
    /// Run tmux with `args`. Ok(stdout) on exit 0. Err(NoServer) when stderr contains
    /// "no server running", "error connecting to" or "server exited unexpectedly" (seen right
    /// after kill-server). Otherwise Err(Command(stderr trimmed)).
    fn run(&self, args: &[String]) -> Result<String, TmuxError>;
}

/// Runs the real binary. Each invocation is killed after 10 s → Err(Command("timed out")).
#[derive(Clone, Debug)]
pub struct RealTmux { pub cfg: TmuxConfig }
impl RealTmux { pub fn new(cfg: TmuxConfig) -> Self; }
impl TmuxRunner for RealTmux { /* … */ }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ServerIdentity { pub pid: u32, pub start_time: i64, pub socket_path: String, pub version: String }
impl ServerIdentity {
    /// "<start_time>-<pid>".
    pub fn id(&self) -> String;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,              // "$3" (only unique within one server instance)
    pub name: String,
    pub windows: u32,
    pub attached: u32,
    pub created: i64,            // unix seconds
    pub activity: i64,           // unix seconds
    pub group: Option<String>,   // session group name when grouped
    #[serde(default)]
    pub claude_panes: u32,       // 0 from tmux; the monitor fills it in from its latest in-memory pane scan
}

/// `list-sessions -F SESSION_FORMAT`, tab separated, session name LAST.
pub const SESSION_FORMAT: &str =
    "#{pid}\t#{start_time}\t#{socket_path}\t#{version}\t#{session_id}\t#{session_windows}\t#{session_attached}\t#{session_created}\t#{session_activity}\t#{session_grouped}\t#{session_group}\t#{session_name}";

pub fn parse_sessions(out: &str) -> Result<(Option<ServerIdentity>, Vec<SessionInfo>), TmuxError>;
/// Err(NoServer) when no server. Ok((None, [])) when a server runs with no sessions.
pub fn list_sessions(r: &dyn TmuxRunner) -> Result<(Option<ServerIdentity>, Vec<SessionInfo>), TmuxError>;
/// `has-session -t =<name>`; Ok(false) when the session is missing; Err(NoServer) when no server.
pub fn has_session(r: &dyn TmuxRunner, name: &str) -> Result<bool, TmuxError>;
/// `new-session -d -s <name> [-c <cwd>] -P -F '#{session_id}'` → the new session id. Starts a server if needed.
pub fn new_session(r: &dyn TmuxRunner, name: &str, cwd: Option<&Path>) -> Result<String, TmuxError>;
/// `rename-session -t =<old> <new>`.
pub fn rename_session(r: &dyn TmuxRunner, old: &str, new: &str) -> Result<(), TmuxError>;
/// Scrolls the active pane (`=<name>:`) of a session: `lines < 0` runs `copy-mode -e` and
/// `send-keys -X -N <n> scroll-up`; `lines > 0` runs `scroll-down` only while `#{pane_in_mode}` is 1
/// (never sends keys to the program in the pane). Returns the state read back afterwards.
pub fn scroll_pane(r: &dyn TmuxRunner, session: &str, lines: i32) -> Result<PaneScroll, TmuxError>;
pub struct PaneScroll { pub in_mode: bool, pub scroll_position: u32 }
/// Non-empty, ≤ 100 chars, no ':' '.' or control characters, no leading '-', no leading/trailing whitespace.
pub fn validate_session_name(name: &str) -> Result<(), String>;
/// "=" + name (exact-match target).
pub fn exact_target(name: &str) -> String;
```

### 6.3 `procscan` (core)

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo { pub pid: u32, pub ppid: u32, pub pgrp: i32, pub tpgid: i32, pub start_ticks: u64, pub comm: String }

#[derive(Clone, Debug, Default)]
pub struct ProcTable { /* private */ }
impl ProcTable {
    pub fn scan(proc_root: &std::path::Path) -> std::io::Result<ProcTable>;
    pub fn from_infos(infos: Vec<ProcInfo>) -> ProcTable;
    pub fn get(&self, pid: u32) -> Option<&ProcInfo>;
    pub fn children(&self, pid: u32) -> Vec<u32>;             // ascending pid
    pub fn descendants(&self, pid: u32) -> Vec<u32>;          // breadth-first, excludes `pid`
    /// Foreground process of the terminal whose session leader is `shell_pid`: the process whose
    /// pid == tpgid (else the lowest pid with pgrp == tpgid). None when tpgid <= 0 or equals the shell's pgrp.
    pub fn foreground_pid(&self, shell_pid: u32) -> Option<u32>;
}
/// Parse /proc/<pid>/stat (comm may contain spaces and ')' — split at the LAST ')').
pub fn parse_stat(content: &str) -> Option<ProcInfo>;
pub fn read_cmdline(proc_root: &std::path::Path, pid: u32) -> Option<Vec<String>>;
pub fn read_cwd(proc_root: &std::path::Path, pid: u32) -> Option<std::path::PathBuf>;
pub fn read_environ_var(proc_root: &std::path::Path, pid: u32, key: &str) -> Option<String>;
```

### 6.4 `claude` (core)

```rust
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClaudeIdSource { SessionsFile, Cmdline, None }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaudeSnap {
    pub pid: u32,
    pub session_id: Option<String>,
    pub cwd: String,
    pub args: Vec<String>,          // argv after the claude program token
    pub kind: Option<String>,       // "interactive", … from the sessions file
    pub name: Option<String>,
    pub source: ClaudeIdSource,
}

#[derive(Clone, Debug)]
pub struct ClaudeDetector { pub proc_root: std::path::PathBuf, pub config_dir: std::path::PathBuf }
impl ClaudeDetector {
    pub fn new(config_dir: std::path::PathBuf) -> Self;     // proc_root = "/proc"
    /// Breadth-first over [pane_pid] + descendants; the first claude process found wins.
    pub fn detect(&self, table: &procscan::ProcTable, pane_pid: u32) -> Option<ClaudeSnap>;
}
pub fn is_claude_process(comm: &str, argv: &[String]) -> bool;
pub fn claude_args(argv: &[String]) -> Vec<String>;
pub fn session_id_from_args(args: &[String]) -> Option<String>;
pub fn is_valid_session_id(s: &str) -> bool;             // 8..=64 chars of [0-9A-Za-z-]
```

### 6.5 `snapshot` (core)

An in-memory scan of the tmux server; nothing here is stored.

```rust
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot { pub version: u32, pub taken_at: i64, pub server: tmux::ServerIdentity, pub sessions: Vec<SessionSnap> }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionSnap { pub id: String, pub name: String, pub created: i64, pub group: Option<String>, pub windows: Vec<WindowSnap> }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowSnap {
    pub id: String, pub index: i64, pub name: String, pub automatic_rename: bool,
    pub active: bool, pub zoomed: bool, pub width: u32, pub height: u32, pub layout: String,
    pub panes: Vec<PaneSnap>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaneSnap {
    pub id: String, pub index: i64, pub active: bool, pub pid: u32, pub cwd: String,
    pub current_command: String,
    pub foreground_cmdline: Option<Vec<String>>,   // argv of the foreground process when it is not the shell
    pub claude: Option<claude::ClaudeSnap>,
}

/// list-sessions + list-windows -a + list-panes -a (tab-separated formats, free text LAST) + /proc scan.
/// Tolerates races (a session vanishing between calls is skipped).
pub fn take_snapshot(r: &dyn tmux::TmuxRunner, detector: &claude::ClaudeDetector) -> Result<Snapshot, tmux::TmuxError>;
```

Modules owned by the server unit (`config`, `monitor`, `server`, `pty`, `layout`, `upload`,
`security`, `assets`) have no frozen Rust API beyond what `main.rs` needs; their external
behaviour is frozen by §7 and §8.

## 7. HTTP and WebSocket API (FROZEN — backend serves, frontend consumes)

- JSON bodies, `Content-Type: application/json`. Times are unix **milliseconds** unless a field
  above says seconds (`SessionInfo.created`, `.activity`, `ServerIdentity.start_time`).
- Error body: `{"error":"<code>","message":"<human text>"}`.
- Every `POST`/`PUT`/`PATCH`/`DELETE` must carry the header `X-Tmux-Web: 1` (else 403 `csrf`).

| Method & path | Request | Success | Errors |
|---|---|---|---|
| `GET /api/info` | — | `{"version","hostname","tmux_socket","upload_max_bytes","auth":"none"\|"token"}` | — |
| `GET /api/status` | — | `TmuxStatus` (below) | — |
| `POST /api/sessions` | `{"name":s,"cwd"?:s}` | 201 `{"id":"$4","name":s}` | 400 `invalid_name`, 409 `exists`, 500 `tmux_error` |
| `POST /api/sessions/rename` | `{"name":old,"new_name":new}` | 200 `{"name":new}` | 400 `invalid_name`, 404 `not_found`, 409 `exists`, 503 `no_server` |
| `POST /api/sessions/kill` | `{"name":s}` | 200 `{"killed":s}` | 400 `invalid_name` (empty), 404 `not_found`, 503 `no_server`, 500 `tmux_error` |
| `POST /api/sessions/scroll` | `{"name":s,"lines":n}` (`n` < 0 = earlier output, > 0 = later; integer, 1 ≤ \|n\| ≤ 1000) | 200 `{"in_mode":bool,"scroll_position":n}` | 400 `invalid_name`, 400 `invalid_lines`, 404 `not_found`, 503 `no_server` |
| `GET /api/layout` | — | `{"version":u64,"updated_at":ms\|null,"layout":any\|null}` | — |
| `PUT /api/layout` | `{"layout":any,"client_id":s}` | 200 `{"version":u64,"updated_at":ms}` | 413 `too_large` (> 2 MiB) |
| `POST /api/upload` | raw bytes of any file, optional `?name=<file name>` | 201 `{"path":abs,"bytes":n,"mime":s}` | 400 `empty`, 413 `too_large` |
| `POST /api/server/start` | — | 200 `{"started":false}` (tmux runs), `{"started":true,"session":s}` (created `--default-session`, a plain login shell); one start at a time | 500 `tmux_error` |

`TmuxStatus` = `{"running":bool,"server":ServerIdentity|null,"sessions":[SessionInfo],"updated_at":ms}`,
sessions sorted by name. `ServerIdentity` and `SessionInfo`
serialise exactly as the serde structs in §6 (snake_case field names).

Upload: images are still detected by magic bytes (PNG, JPEG, GIF, WebP) and named `img-…`.
Anything else is stored as `file-<unixms>-<8 hex>-<sanitised name>` (name from `?name=`, reduced
to `[A-Za-z0-9._-]`, at most 100 bytes, `upload` when empty) with mime
`application/octet-stream`. The declared Content-Type is only a hint.

### 7.1 `GET /ws/events`

Server → client JSON text frames, `type` first:

- `{"type":"status", …TmuxStatus fields}` — on connect, then whenever `running`, the server
  identity, the session list (ignoring `activity`) or a `claude_panes` count changes.
- `{"type":"layout","version":u64,"client_id":s}` — after every successful `PUT /api/layout`.

Client → server frames are ignored. The server pings every 30 s. A lagging subscriber is
re-sent the full `status` frame.

### 7.2 `GET /ws/attach?session=<name>&cols=<n>&rows=<n>`

`cols` clamp 2..=1000 (default 80), `rows` clamp 2..=500 (default 24).

- Server → client **binary**: raw terminal output bytes.
- Server → client **text**, sent once right before the server closes:
  `{"type":"exit","reason":R,"message":s}` with R one of `detached`, `session-closed`,
  `server-down`, `no-session`, `no-server`, `error`.
- Client → server **binary**: input bytes, written to the PTY unchanged.
- Client → server **text**: `{"type":"resize","cols":n,"rows":n}` or `{"type":"input","data":s}`.
- Close codes: 1000 normal (after `detached`/`session-closed`/`server-down`), 4404 `no-session`,
  4503 `no-server`, 1011 `error`.

### 7.3 Static files

`GET /` serves `web/index.html`; `GET /<path>` serves the embedded `web/<path>`; unknown
paths → 404 (JSON for `/api/*`). `.js`/`.mjs` → `text/javascript; charset=utf-8`.
Vendored type definitions live outside `web/` in `vendor-types/` and are neither embedded nor served.

## 8. Behaviour specs

### 8.1 Attach bridge (server: `pty.rs`)

1. Before spawning: `has_session` (blocking pool). `Err(NoServer)` → exit `no-server`, close 4503.
   `Ok(false)` → exit `no-session`, close 4404.
2. Spawn in a PTY of the requested size (portable-pty):
   `tmux <global args> -u -T RGB,clipboard,hyperlinks,focus,title,cstyle,usstyle,strikethrough,overline,bpaste,mouse,256 attach-session -t =<name>`
   with env `TERM=xterm-256color`, `COLORTERM=truecolor`, `TMUX`/`TMUX_PANE` removed, cwd `$HOME`.
3. A blocking reader thread reads ≤ 64 KiB chunks into a bounded channel (capacity 64) → binary
   frames. A blocking writer thread owns the PTY writer and master: input bytes and resizes.
4. Client close / error → kill the attach client process and reap it (no zombies).
5. PTY EOF → reap the child, then decide the reason: session still exists → `detached`;
   server up but session gone → `session-closed`; no server → `server-down`. Send the exit frame, close 1000.

### 8.2 Monitor (server: `monitor.rs`)

- Every `poll_interval`: `list_sessions` on the blocking pool. Builds `TmuxStatus`
  (`running=false`, `server=null`, `sessions=[]` on `NoServer`), fills `claude_panes` per session
  from the latest pane scan, broadcasts `status` on change (activity ignored).
- Pane scan (`take_snapshot`, in memory only, nothing is written to disk) every 15 s and at the
  first poll after a structural change (session ids or window counts changed), only while
  running with ≥ 1 session. Scan errors are logged (warn), never fatal. The per-session count of
  panes running Claude becomes `claude_panes`.
- The monitor never starts, restores or configures tmux. `POST /api/server/start` is the only
  thing that starts tmux, by creating `--default-session`.

### 8.4 Claude detection (core: `claude.rs`)

- A process is Claude Code when `comm == "claude"`, or basename(argv[0]) == "claude", or argv[0]
  is an interpreter (`node`, `bun`, `deno`, `sh`, `bash`) and argv[1] has basename `claude` or
  contains `claude-code` and ends with `cli.js` / `cli.mjs`.
- Session file: `<dir>/sessions/<pid>.json` where `<dir>` = `CLAUDE_CONFIG_DIR` from
  `/proc/<pid>/environ` when set, else `ClaudeDetector.config_dir`. Fields used: `sessionId`
  (string), `cwd` (string), `procStart` (string or number; must equal the process start ticks
  when present, else the file is stale and ignored), `kind`, `name`. A valid `sessionId` →
  source `sessions-file`.
- Fallback: `--resume <id>`, `--resume=<id>`, `-r <id>`, `--session-id <id>`, `--session-id=<id>`
  in the args → source `cmdline`. Else `session_id = None`, source `none`.
- `cwd`: from the sessions file, else `/proc/<pid>/cwd`.

### 8.5 Security (server: `security.rs`)

- **Auth.** Loopback `--listen` → no auth unless `--token` is given. Non-loopback: `--token`
  wins; else `--no-auth` disables auth explicitly; else read or generate `<state-dir>/token`
  (32 random bytes hex, 0600) and log where it is stored (never the token itself; a terminal on stderr also gets the sign-in
  URL `http://<listen>/?token=…` printed once). When auth is on, a request is authorised by
  `Authorization: Bearer <t>`, cookie `tmux_web_token=<t>`, or, on a `GET` of a non-`/api`,
  non-`/ws` path only, query `token=<t>`. A matching query token on such a request answers 302 to the same URL without `token`, setting
  `tmux_web_token=<t>; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000`, plus `; Secure` when
  the first value of `X-Forwarded-Proto` is `https`. Unauthorised:
  `/api/*` → 401 JSON, `/ws/*` → 401, other paths → 401 with a tiny HTML form that GETs `?token=`.
  Constant-time comparison.
- **Host check** (only when auth is off; DNS-rebinding guard): the `Host` header's host must be
  `localhost`, `*.localhost`, an IP literal, the machine hostname (case-insensitive), or an
  `--allow-host` entry; else 403 `host_not_allowed`.
- **Origin check** on `/ws/*` upgrades and on every mutating method: when an `Origin` header is
  present, its host[:port] must equal the `Host` header, or its host must be an `--allow-host`
  entry; else 403 `bad_origin`.
- **CSRF**: mutating methods need `X-Tmux-Web: 1` (403 `csrf`).
- **Headers on HTML**: `Content-Security-Policy: default-src 'self'; img-src 'self' data: blob:;
  style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; object-src 'none';
  base-uri 'none'; frame-ancestors 'none'`, `X-Frame-Options: DENY`,
  `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`. All responses: `nosniff`.
- Body limits: upload route = `--max-upload-mb`; everything else 2 MiB.

### 8.6 Frontend (frontend: `web/`)

Libraries (already vendored under `web/vendor/`, ES modules, no bundler, import by relative path):

| File | Exports used |
|---|---|
| `vendor/dockview-core/dockview-core.esm.min.mjs` (8.5.1, injects its own CSS) | `createDockview`, `themeDark` (or another exported theme) |
| `vendor/xterm/xterm.mjs` + `vendor/xterm/xterm.css` (6.0.0) | `Terminal` |
| `vendor/addon-fit/addon-fit.mjs` | `FitAddon` |
| `vendor/addon-web-links/addon-web-links.mjs` | `WebLinksAddon` |
| `vendor/addon-clipboard/addon-clipboard.mjs` | `ClipboardAddon` |
| `vendor/addon-webgl/addon-webgl.mjs` | `WebglAddon` |
| `vendor/addon-unicode11/addon-unicode11.mjs` | `Unicode11Addon` |

Type definitions are in `vendor-types/<package>/` at the repo root (`vendor-types/dockview-core/types/`, `vendor-types/xterm/xterm.d.ts`, `vendor-types/addon-*/`). Read them;
do not guess APIs.

**Layout document** (opaque to the backend, stored via `PUT /api/layout`):

```json
{ "schema": 1,
  "dockview": "<api.toJSON()>",
  "maximizedPanelId": "p-… or null",
  "prefs": { "fontSize": 13, "copyOnSelect": true, "webgl": true } }
```

Panels: id `p-<random>`, component `terminal`, title = session name, params
`{"session": name, "sessionId": "$3"|null, "serverId": "<start_time>-<pid>"|null}`.

Required behaviour:

1. **Top bar**: `tmux-web · <hostname>`; status pill (`data-testid="status-pill"`): green
   "tmux <version> · N sessions" / red "tmux server not running"; "Add panel" button
   (`add-panel-btn`) opening the session picker; settings popover (font size −/+, copy-on-select,
   GPU rendering); browser-fullscreen toggle.
2. **Session picker** (`session-picker`): filter input, one row per session
   (`session-picker-item`, `data-session=<name>`) with window count, attached count, a Claude
   badge when `claude_panes > 0`, an "open" badge when already in a panel; keyboard ↑/↓/Enter/Esc;
   "New session…" row (`new-session-input`) creating via `POST /api/sessions`; "Open all"
   (`open-all-btn`) adding every session not yet open as tabs. Choosing a session that is
   already open focuses its panel (add mode) or moves that panel to the new split (split mode).
   Each row is a `div.picker-row` holding the item button and a red ✕ (`session-picker-kill`,
   `data-session=<name>`, not part of the ↑/↓ list) that closes the picker and opens the confirm
   dialog `kill-dialog` (Cancel `kill-cancel-btn`, Terminate `kill-confirm-btn`, error
   `kill-error`); Terminate calls `POST /api/sessions/kill` and closes every panel of that session.
3. **Dockview**: `defaultRenderer: 'always'`, floating groups disabled, dark theme, custom tab
   renderer: `<div data-testid="tab" data-session=<name>>` with a connection dot, the title
   (exactly the session name), a Claude badge, and a close button. Double-click on a tab toggles
   maximise of its group; middle-click closes the panel. Closing a panel never kills the session.
   When a group's header is too narrow for all its tabs, they wrap onto more rows (every tab stays
   visible; the header grows and the terminal shrinks to fit) instead of hiding in an overflow list.
4. **Group header actions** (right side, `data-testid` = `header-split-right`,
   `header-split-down`, `header-upload`, `header-maximize`): split right / split down open the
   picker in split mode and place the chosen session in a new group right of / below this group;
   upload opens a file chooser (any file, multiple) for the group's active panel; maximise toggles
   `maximizeGroup` / `exitMaximizedGroup` and shows the matching icon. Drag-and-drop of tabs
   (reorder, stack into another group, split to an edge) is dockview's built-in behaviour.
5. **Terminal panel**: xterm `Terminal` (`allowProposedApi: true`, Unicode 11 active, scrollback
   1000, font from prefs, dark theme), FitAddon, WebglAddon when prefs.webgl (dispose on context
   loss and fall back to the DOM renderer), WebLinksAddon, ClipboardAddon. Opens
   `/ws/attach` with the fitted size; refits on dockview dimension/visibility changes and window
   resize (skip while the element has zero size); sends `resize` only when cols/rows change.
   Input via `onData` (UTF-8 binary frames) and `onBinary` (bytes 0–255 as binary frames).
6. **Overlay** (`panel-overlay`) when not connected: `no-session`/`session-closed` → "Session “X”
   no longer exists" + [Recreate session] (POST /api/sessions with that name) + [Close panel];
   `no-server`/`server-down` → "tmux server is not running"; `detached` → [Reattach]; network error → retry with backoff 1, 2, 4, 8, 10 s + [Retry now].
   A disconnected panel (except `detached`) reconnects automatically when a `status` event shows
   its session name present on a running server.
7. **Names follow tmux**: on every `status` event, a panel whose `serverId` equals the current
   server id and whose `sessionId` matches a session with a different name gets the new title
   and params (layout saved). Connected panels learn `sessionId`/`serverId` by name.
8. **Copy**: copy-on-select (prefs, default on, debounced); Ctrl+Shift+C (Cmd+C on macOS) copies;
   Ctrl+C with a non-empty selection copies and clears it instead of sending ^C; context menu
   Copy. Writes use `navigator.clipboard.writeText` in secure contexts, else a hidden textarea +
   `document.execCommand('copy')`; failure → toast. OSC 52 from tmux goes through
   ClipboardAddon with a provider whose `writeText` uses the same copy routine and whose
   `readText` returns "" (never expose the clipboard to the remote side).
9. **Paste**: Ctrl+V, Ctrl+Shift+V, Cmd+V are not sent to the terminal (custom key handler returns
   false) so the browser fires a native paste; text pastes go through xterm (bracketed paste).
10. **Files**: a capture-phase `paste` listener on the panel element intercepts clipboard items of
    kind `file`, any type (preventDefault + stopPropagation), uploads each via `POST /api/upload`, then
    `term.paste(paths.map(shellQuote).join(' ') + ' ')`: a path with any character outside
    `[A-Za-z0-9_/.:@%+=,-]` is single-quoted (`'` becomes `'\''`). When the clipboard holds any
    file (including a file copied in a file manager, which also carries its name as text) the files
    are uploaded and the text is not pasted. Same for files dropped on the terminal (only when
    `dataTransfer.types` includes `Files`, so dockview tab DnD is untouched) and the header upload
    button; a dropped folder is skipped with a warning toast. Toast on success/failure.
11. **Links**: WebLinksAddon handler and the terminal `linkHandler` (OSC 8, http/https only)
    open `window.open(url, '_blank', 'noopener,noreferrer')` **only when Ctrl or Meta is held**;
    hovering a link shows a small tooltip "Ctrl+click to open".
12. **Context menu** (right-click when the terminal is not tracking the mouse, or Shift+right-click
    always): Copy, Paste (navigator.clipboard.readText when available, else a toast "use Ctrl+V"),
    Upload file…, Split right, Split down, Maximise/Restore, Rename session… (POST
    /api/sessions/rename), Close panel.
13. **Keyboard**: Ctrl+Shift+Enter toggles maximise of the focused panel's group.
14. **Persistence**: save (debounced 300 ms) on dockview layout change, maximise change, title/param
    change and prefs change; flush with `fetch(…, {keepalive: true})` on `pagehide`. On startup
    load `/api/layout` and `fromJSON` it (try/catch → empty layout + toast), then re-maximise
    `maximizedPanelId`. A `layout` event from another `client_id` shows a toast with [Load];
    never auto-apply (no save loops).
15. **Empty state** (`empty-state`) when there are no panels: session list (click to open),
    New session, Open all; while tmux is not running also **Start tmux**
    (`empty-start-tmux`, calls `POST /api/server/start`).
15a. **Start on open**: when the first status a page load receives says tmux is not running,
    the page calls `POST /api/server/start` once and toasts the outcome: a plain default session
    ("Started tmux: session <name>"). No later status starts tmux again, so tmux stopped while the page is open stays
    stopped.
16. **Test hook**: `window.__tmuxWeb = { ready: Promise, api /* DockviewApi */, panels(),
    screenText(panelId), terminal(panelId) }` where `panels()` returns
    `[{id, session, groupId, connected, maximized}]` and `screenText` returns the visible rows of
    the terminal buffer joined by "\n" (trailing spaces trimmed).
16a. **Finger scrolling** (`touch-scroll.js`): a one-finger touch that moves more than 8 px, mostly
    vertically, is a scroll: `touchmove` (non-passive) is `preventDefault`ed from then on, and the
    movement is converted to terminal rows (host height / rows). Finger down = earlier output
    (negative `lines`). While the program tracks the mouse (`term.modes.mouseTrackingMode` is not
    `none`, e.g. a full-screen app with its own scrollback) the movement is instead dispatched as
    pixel `WheelEvent`s (deltaY = -movement) on the xterm screen, so xterm encodes them like a
    desktop wheel and tmux history is left alone. At most one `POST /api/sessions/scroll` per panel is in flight; rows that
    accumulate meanwhile are sent when it returns; failures are ignored without a toast. Taps,
    multi-touch, the desktop wheel and mouse are untouched, and the tmux `mouse` option stays off.
17. **Toasts**: `data-testid="toast"`, auto-dismiss after 4 s.

## 9. Tests

- **core**: unit tests for every parser (sessions/windows/panes output, `/proc/<pid>/stat`
  with spaces and parentheses in comm, cmdline/environ NUL splitting), Claude detection, the in-memory pane scan against a mock runner. Integration tests
  (`tests/core_*.rs`) against real tmux on isolated sockets: the scan agrees with what tmux
  reports, including awkward names and directories.
- **server**: unit tests for security (auth cookie flow, Host allowlist, Origin, CSRF), layout
  store (versions, size limit, atomicity), upload (magic bytes, size, naming, mode 0600),
  asset content types. Integration tests (`tests/server_*.rs`) on an ephemeral port with an
  isolated tmux socket and temp state dir: status, create/rename session, attach (type
  `echo tmw-$((40+2))` → output contains `tmw-42`), resize, exit reasons `session-closed` and
  `server-down`, events stream, a pane running a process called `claude` counted in `/api/status` without anything
  written to disk, plain `POST /api/server/start`, CSRF/Origin rejection.
- **e2e (P2)**: `e2e/` with `playwright-core@1.64.0` and Node's built-in test runner, using the
  local Chromium at `$TMUX_WEB_CHROME` (default
  `~/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome`).
