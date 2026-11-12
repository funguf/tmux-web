//! tmux access: server selection, the runner abstraction, the real runner and session helpers.

use crate::util;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// Which tmux server to talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketSel {
    /// The default server for this user (no `-L` / `-S`).
    Default,
    /// `-L <name>`: a named socket inside tmux's socket directory.
    Name(String),
    /// `-S <path>`: an explicit socket path.
    Path(PathBuf),
}

/// How to invoke tmux: which binary, and which server it should address.
#[derive(Clone, Debug)]
pub struct TmuxConfig {
    pub bin: PathBuf,
    pub socket: SocketSel,
}

impl TmuxConfig {
    /// [] | ["-L", name] | ["-S", path]
    ///
    /// A socket path that is not valid UTF-8 is converted lossily.
    pub fn global_args(&self) -> Vec<String> {
        match &self.socket {
            SocketSel::Default => Vec::new(),
            SocketSel::Name(name) => vec!["-L".to_string(), name.clone()],
            SocketSel::Path(path) => {
                vec!["-S".to_string(), path.to_string_lossy().into_owned()]
            }
        }
    }

    /// `bin <global args> <args>`, env TMUX and TMUX_PANE removed, SHELL set from the passwd
    /// entry when the environment has none, stdin = null.
    pub fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(self.global_args()).args(args);
        // A TMUX variable inherited from an enclosing tmux would redirect invocations
        // without -L/-S to that enclosing server.
        cmd.env_remove("TMUX").env_remove("TMUX_PANE");
        // tmux-web's own settings, the token above all, would reach every pane of a tmux
        // server started from here.
        for name in own_setting_names() {
            cmd.env_remove(name);
        }
        cmd.stdin(Stdio::null());
        if let Some(shell) = shell_override(std::env::var_os("SHELL"), || {
            util::passwd_entry().map(|user| user.shell)
        }) {
            cmd.env("SHELL", shell);
        }
        cmd
    }

    /// Human description for /api/info: "default" | "-L <name>" | "-S <path>".
    pub fn describe(&self) -> String {
        match &self.socket {
            SocketSel::Default => "default".to_string(),
            SocketSel::Name(name) => format!("-L {name}"),
            SocketSel::Path(path) => format!("-S {}", path.display()),
        }
    }
}

/// Names of the variables in this process's environment that configure tmux-web itself
/// (`TMUX_WEB_*`). Names that are not valid UTF-8 are matched on their bytes.
pub fn own_setting_names() -> Vec<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.as_bytes().starts_with(b"TMUX_WEB_"))
        .collect()
}

/// The `SHELL` to give tmux when our own environment has none (a systemd service, say):
/// the login shell from the passwd entry. `None` means the inherited environment is fine.
/// `passwd_shell` is only consulted when `env_shell` is unset or empty.
fn shell_override(
    env_shell: Option<OsString>,
    passwd_shell: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    match env_shell {
        Some(shell) if !shell.is_empty() => None,
        _ => passwd_shell().filter(|shell| !shell.as_os_str().is_empty()),
    }
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

/// Limits applied to a single tmux invocation.
#[derive(Clone, Copy, Debug)]
struct RunLimits {
    /// The invocation is killed once it has run this long.
    timeout: Duration,
    /// Once tmux has exited, how long to wait for its output pipes to reach EOF. They only
    /// stay open past that point when some other process inherited them.
    drain_grace: Duration,
}

const RUN_LIMITS: RunLimits = RunLimits {
    timeout: Duration::from_secs(10),
    drain_grace: Duration::from_secs(2),
};

/// How often a running invocation is polled for exit.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Runs the real binary. Each invocation is killed after 10 s → Err(Command("timed out …")).
#[derive(Clone, Debug)]
pub struct RealTmux {
    pub cfg: TmuxConfig,
}

impl RealTmux {
    pub fn new(cfg: TmuxConfig) -> Self {
        Self { cfg }
    }
}

impl TmuxRunner for RealTmux {
    fn run(&self, args: &[String]) -> Result<String, TmuxError> {
        run_limited(&self.cfg, args, RUN_LIMITS)
    }
}

fn run_limited(cfg: &TmuxConfig, args: &[String], limits: RunLimits) -> Result<String, TmuxError> {
    let mut child = cfg
        .command(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Both pipes are drained on helper threads while we wait: a chatty tmux must never
    // block on a full pipe buffer while we poll for its exit.
    let captures = PipeCapture::start(child.stdout.take())
        .and_then(|out| PipeCapture::start(child.stderr.take()).map(|err| (out, err)));
    let (stdout, stderr) = match captures {
        Ok(captures) => captures,
        Err(err) => {
            kill_and_reap(&mut child);
            return Err(err.into());
        }
    };

    let deadline = Instant::now() + limits.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                kill_and_reap(&mut child);
                return Err(err.into());
            }
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            return Err(TmuxError::Command(format!(
                "timed out after {}s: {}",
                limits.timeout.as_secs(),
                args.join(" ")
            )));
        }
        thread::sleep(POLL_INTERVAL);
    };

    // One shared deadline, so a pipe held open by a stray process costs one grace period.
    let drain_deadline = Instant::now() + limits.drain_grace;
    let stdout = stdout.finish(drain_deadline);
    let stderr = stderr.finish(drain_deadline);
    if status.success() {
        Ok(stdout)
    } else {
        Err(classify_failure(&stderr, status))
    }
}

/// Kills a child and reaps it, so a timed-out or failed invocation leaves no zombie.
fn kill_and_reap(child: &mut Child) {
    // Errors mean the process is already gone; there is nothing further to do about them.
    let _ = child.kill();
    let _ = child.wait();
}

/// What tmux says when no server answers: nothing listens on the socket, the socket cannot be
/// reached, or the server went away while the client was connected (a client that races
/// `kill-server` sees the last one for a few tens of milliseconds before the socket is gone).
const NO_SERVER_MESSAGES: [&str; 3] = [
    "no server running",
    "error connecting to",
    "server exited unexpectedly",
];

fn classify_failure(stderr: &str, status: ExitStatus) -> TmuxError {
    if NO_SERVER_MESSAGES
        .iter()
        .any(|message| stderr.contains(message))
    {
        return TmuxError::NoServer;
    }
    let message = stderr.trim();
    if message.is_empty() {
        TmuxError::Command(format!("tmux failed ({status})"))
    } else {
        TmuxError::Command(message.to_string())
    }
}

/// Everything a child wrote to one of its pipes, collected on a helper thread.
struct PipeCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Signalled when the helper thread reaches EOF (or gives up on a read error).
    done: mpsc::Receiver<()>,
}

impl PipeCapture {
    fn start<R: Read + Send + 'static>(pipe: Option<R>) -> io::Result<Self> {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let (finished, done) = mpsc::channel();
        // The thread is detached on purpose: if some other process holds the pipe open it
        // simply ends whenever that process lets go.
        thread::Builder::new()
            .name("tmux-pipe".to_string())
            .spawn(move || {
                if let Some(mut pipe) = pipe {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match pipe.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => sink
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .extend_from_slice(&chunk[..n]),
                            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                }
                // The receiver is gone when `finish` already gave up; nobody is waiting.
                let _ = finished.send(());
            })?;
        Ok(Self { bytes, done })
    }

    /// Waits until EOF or `deadline`, then returns what was read so far (lossily decoded).
    fn finish(self, deadline: Instant) -> String {
        // Timeout and disconnect both mean "stop waiting"; the bytes read so far are what
        // the child wrote.
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ServerIdentity {
    pub pid: u32,
    pub start_time: i64,
    pub socket_path: String,
    pub version: String,
}

impl ServerIdentity {
    /// "<start_time>-<pid>" — also the snapshot id.
    pub fn id(&self) -> String {
        format!("{}-{}", self.start_time, self.pid)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String, // "$3" (only unique within one server instance)
    pub name: String,
    pub windows: u32,
    pub attached: u32,
    pub created: i64,          // unix seconds
    pub activity: i64,         // unix seconds
    pub group: Option<String>, // session group name when grouped
    #[serde(default)]
    pub claude_panes: u32, // 0 from tmux; the monitor fills it in from the latest snapshot
}

/// `list-sessions -F SESSION_FORMAT`, tab separated, session name LAST.
pub const SESSION_FORMAT: &str =
    "#{pid}\t#{start_time}\t#{socket_path}\t#{version}\t#{session_id}\t#{session_windows}\t#{session_attached}\t#{session_created}\t#{session_activity}\t#{session_grouped}\t#{session_group}\t#{session_name}";

/// Number of tab-separated fields in [`SESSION_FORMAT`].
const SESSION_FIELDS: usize = 12;

/// Parses `list-sessions -F SESSION_FORMAT` output: one line per session. The server identity
/// is taken from the first line; empty output (a server without sessions) gives no identity.
pub fn parse_sessions(out: &str) -> Result<(Option<ServerIdentity>, Vec<SessionInfo>), TmuxError> {
    let mut identity = None;
    let mut sessions = Vec::new();
    for (index, line) in out.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (line_identity, session) = parse_session_line(line).map_err(|reason| {
            TmuxError::Parse(format!("session line {} ({reason}): {line:?}", index + 1))
        })?;
        identity.get_or_insert(line_identity);
        sessions.push(session);
    }
    Ok((identity, sessions))
}

fn parse_session_line(line: &str) -> Result<(ServerIdentity, SessionInfo), String> {
    // The session name comes last and may itself contain tabs, hence the splitn limit.
    let fields: Vec<&str> = line.splitn(SESSION_FIELDS, '\t').collect();
    let Ok(
        [pid, start_time, socket_path, version, id, windows, attached, created, activity, grouped, group, name],
    ) = <[&str; SESSION_FIELDS]>::try_from(fields.as_slice())
    else {
        return Err(format!(
            "expected {SESSION_FIELDS} tab-separated fields, found {}",
            fields.len()
        ));
    };
    if id.is_empty() {
        return Err("empty session id".to_string());
    }
    let identity = ServerIdentity {
        pid: parse_number(pid, "server pid")?,
        start_time: parse_number(start_time, "server start time")?,
        socket_path: socket_path.to_string(),
        version: version.to_string(),
    };
    let session = SessionInfo {
        id: id.to_string(),
        name: name.to_string(),
        windows: parse_number(windows, "window count")?,
        attached: parse_number(attached, "attached count")?,
        created: parse_number(created, "creation time")?,
        activity: parse_number(activity, "activity time")?,
        group: (grouped == "1" && !group.is_empty()).then(|| group.to_string()),
        claude_panes: 0,
    };
    Ok((identity, session))
}

fn parse_number<T: std::str::FromStr>(field: &str, what: &str) -> Result<T, String> {
    field.parse().map_err(|_| format!("bad {what} {field:?}"))
}

/// Builds an owned argv from string slices.
fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

/// Escapes `text` for a tmux argument that tmux expands as a format: session and window
/// names (`-s`, `-n`, `rename-session`) and start directories (`-c`).
///
/// Without this a name such as `build#S`, or a directory called `x#{pane_id}`, is silently
/// rewritten, and `#(command)` is run by the tmux server. Targets (`-t`) are not formats and
/// must not be escaped.
pub(crate) fn escape_format(text: &str) -> String {
    text.replace('#', "##")
}

/// Err(NoServer) when no server. Ok((None, [])) when a server runs with no sessions.
pub fn list_sessions(
    r: &dyn TmuxRunner,
) -> Result<(Option<ServerIdentity>, Vec<SessionInfo>), TmuxError> {
    let out = r.run(&argv(&["list-sessions", "-F", SESSION_FORMAT]))?;
    parse_sessions(&out)
}

/// `has-session -t =<name>`; Ok(false) when the session is missing; Err(NoServer) when no server.
pub fn has_session(r: &dyn TmuxRunner, name: &str) -> Result<bool, TmuxError> {
    let args = vec![
        "has-session".to_string(),
        "-t".to_string(),
        exact_target(name),
    ];
    match r.run(&args) {
        Ok(_) => Ok(true),
        Err(TmuxError::Command(message)) if is_missing_session(&message) => Ok(false),
        Err(err) => Err(err),
    }
}

/// tmux words "no such session" differently across versions.
fn is_missing_session(message: &str) -> bool {
    message.contains("can't find session") || message.contains("session not found")
}

/// `new-session -d -s <name> [-c <cwd>] -P -F '#{session_id}'` → the new session id. Starts a server if needed.
/// `name` and `cwd` are taken literally (`#` is escaped for tmux's format expansion).
/// The output is accepted only when, trimmed, it is exactly one id of the form `$<digits>`;
/// anything else is a [`TmuxError::Parse`] quoting what tmux printed.
pub fn new_session(
    r: &dyn TmuxRunner,
    name: &str,
    cwd: Option<&Path>,
) -> Result<String, TmuxError> {
    let mut args = argv(&["new-session", "-d", "-s"]);
    args.push(escape_format(name));
    if let Some(cwd) = cwd {
        args.push("-c".to_string());
        args.push(escape_format(&cwd.to_string_lossy()));
    }
    args.extend(argv(&["-P", "-F", "#{session_id}"]));
    let out = r.run(&args)?;
    let id = out.trim();
    if !is_session_id(id) {
        return Err(TmuxError::Parse(format!(
            "new-session printed {out:?} instead of one session id like $3"
        )));
    }
    Ok(id.to_string())
}

/// A tmux session id: `$` followed by one or more ASCII digits, nothing else (so also no
/// second line).
fn is_session_id(text: &str) -> bool {
    text.strip_prefix('$')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// `rename-session -t =<old> <new>`; `new` is taken literally (`#` is escaped for tmux).
pub fn rename_session(r: &dyn TmuxRunner, old: &str, new: &str) -> Result<(), TmuxError> {
    let args = vec![
        "rename-session".to_string(),
        "-t".to_string(),
        exact_target(old),
        escape_format(new),
    ];
    r.run(&args)?;
    Ok(())
}

/// `kill-session -t =<name>`: terminate a session and everything running in it.
pub fn kill_session(r: &dyn TmuxRunner, name: &str) -> Result<(), TmuxError> {
    let args = vec![
        "kill-session".to_string(),
        "-t".to_string(),
        exact_target(name),
    ];
    r.run(&args)?;
    Ok(())
}

/// Whether a pane is in a mode (copy mode) and how far it is scrolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneScroll {
    pub in_mode: bool,
    /// Lines above the live screen; 0 when the pane is not in a mode.
    pub scroll_position: u32,
}

/// Scrolls the active pane of a session through tmux's own history, like a finger swipe would.
/// `lines < 0` goes back to earlier output (entering copy mode first, `-e` so that reaching
/// the bottom leaves it again), `lines > 0` goes forward and only acts while the pane is
/// already in a mode, so keys never reach the program in the pane. Returns the state after.
pub fn scroll_pane(r: &dyn TmuxRunner, session: &str, lines: i32) -> Result<PaneScroll, TmuxError> {
    // The trailing colon makes `=session` the session part of a pane target.
    let target = format!("{}:", exact_target(session));
    let count = lines.unsigned_abs().to_string();
    let scroll = |direction: &str| {
        let mut args = argv(&["send-keys", "-t"]);
        args.push(target.clone());
        args.extend(argv(&["-X", "-N"]));
        args.push(count.clone());
        args.push(direction.to_string());
        args
    };
    if lines < 0 {
        let mut args = argv(&["copy-mode", "-e", "-t"]);
        args.push(target.clone());
        r.run(&args)?;
        r.run(&scroll("scroll-up"))?;
    } else if lines > 0 && pane_scroll_state(r, &target)?.in_mode {
        match r.run(&scroll("scroll-down")) {
            // The pane left its mode between the check and the key: nothing left to scroll.
            Err(TmuxError::Command(message)) if message.contains("not in a mode") => {}
            other => {
                other?;
            }
        }
    }
    pane_scroll_state(r, &target)
}

/// `list-panes -t <target> -f '#{pane_active}' -F '#{pane_in_mode} #{scroll_position}'`, parsed.
/// (`display-message` would print nothing, not fail, for a session that does not exist.)
fn pane_scroll_state(r: &dyn TmuxRunner, target: &str) -> Result<PaneScroll, TmuxError> {
    let mut args = argv(&["list-panes", "-t"]);
    args.push(target.to_string());
    args.extend(argv(&[
        "-f",
        "#{pane_active}",
        "-F",
        "#{pane_in_mode} #{scroll_position}",
    ]));
    let out = r.run(&args)?;
    let mut fields = out.split_whitespace();
    let in_mode = match fields.next() {
        Some("0") => false,
        Some("1") => true,
        _ => {
            return Err(TmuxError::Parse(format!(
                "list-panes printed {out:?} instead of pane_in_mode and scroll_position"
            )))
        }
    };
    // scroll_position is empty outside a mode.
    let scroll_position = fields.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    Ok(PaneScroll {
        in_mode,
        scroll_position,
    })
}

/// Longest accepted session name, in characters.
const MAX_SESSION_NAME_CHARS: usize = 100;

/// Non-empty, ≤ 100 chars, no ':' '.' or control characters, no leading '-', no leading/trailing whitespace.
pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name must not be empty".to_string());
    }
    if name.chars().count() > MAX_SESSION_NAME_CHARS {
        return Err(format!(
            "session name must be at most {MAX_SESSION_NAME_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("session name must not contain control characters".to_string());
    }
    if name.contains([':', '.']) {
        return Err("session name must not contain ':' or '.'".to_string());
    }
    if name.starts_with('-') {
        return Err("session name must not start with '-'".to_string());
    }
    if name.trim() != name {
        return Err("session name must not start or end with whitespace".to_string());
    }
    Ok(())
}

/// "=" + name (exact-match target).
pub fn exact_target(name: &str) -> String {
    format!("={name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::FileTypeExt;

    // ---- helpers -------------------------------------------------------------------

    /// A runner that replays canned replies and records every argv it is given.
    struct ScriptedRunner {
        replies: Mutex<VecDeque<Result<String, TmuxError>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedRunner {
        fn new(replies: Vec<Result<String, TmuxError>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TmuxRunner for ScriptedRunner {
        fn run(&self, args: &[String]) -> Result<String, TmuxError> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("ScriptedRunner ran out of replies")
        }
    }

    fn cfg(socket: SocketSel) -> TmuxConfig {
        TmuxConfig {
            bin: PathBuf::from("tmux"),
            socket,
        }
    }

    /// `sh` stands in for tmux so the runner can be exercised without a tmux server.
    fn sh_cfg() -> TmuxConfig {
        TmuxConfig {
            bin: PathBuf::from("sh"),
            socket: SocketSel::Default,
        }
    }

    fn sh_script(script: &str) -> Vec<String> {
        argv(&["-c", script])
    }

    fn to_args(parts: &[&str]) -> Vec<String> {
        argv(parts)
    }

    const SERVER_SOCKET: &str = "/tmp/tmux-1000/default";

    /// A valid 12-field session line, as an array so tests can alter single fields.
    fn base_fields() -> [&'static str; SESSION_FIELDS] {
        [
            "4242",
            "1700000000",
            SERVER_SOCKET,
            "3.4",
            "$0",
            "2",
            "1",
            "1700000100",
            "1700000200",
            "0",
            "",
            "main",
        ]
    }

    fn line_of(fields: [&str; SESSION_FIELDS]) -> String {
        fields.join("\t")
    }

    // ---- global args / describe / command ---------------------------------------------

    #[test]
    fn global_args_for_every_socket_selection() {
        assert_eq!(cfg(SocketSel::Default).global_args(), Vec::<String>::new());
        assert_eq!(
            cfg(SocketSel::Name("work".into())).global_args(),
            to_args(&["-L", "work"])
        );
        assert_eq!(
            cfg(SocketSel::Path(PathBuf::from("/run/user/1000/tmux.sock"))).global_args(),
            to_args(&["-S", "/run/user/1000/tmux.sock"])
        );
    }

    #[test]
    fn describe_for_every_socket_selection() {
        assert_eq!(cfg(SocketSel::Default).describe(), "default");
        assert_eq!(cfg(SocketSel::Name("work".into())).describe(), "-L work");
        assert_eq!(
            cfg(SocketSel::Path(PathBuf::from("/run/tmux.sock"))).describe(),
            "-S /run/tmux.sock"
        );
    }

    #[test]
    fn command_puts_global_args_before_the_given_args() {
        let config = TmuxConfig {
            bin: PathBuf::from("/opt/tmux/bin/tmux"),
            socket: SocketSel::Name("sock".into()),
        };
        let cmd = config.command(&to_args(&["list-sessions", "-F", "#{session_name}"]));
        assert_eq!(cmd.get_program(), OsStr::new("/opt/tmux/bin/tmux"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            ["-L", "sock", "list-sessions", "-F", "#{session_name}"]
                .map(OsStr::new)
                .to_vec()
        );
    }

    #[test]
    fn command_without_a_socket_selection_passes_only_the_given_args() {
        let cmd = cfg(SocketSel::Default).command(&to_args(&["kill-server"]));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(args, vec![OsStr::new("kill-server")]);
    }

    #[test]
    fn command_removes_tmux_and_tmux_pane_from_the_environment() {
        let cmd = cfg(SocketSel::Default).command(&[]);
        let envs: HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        // `Some(None)` is how `Command` records an explicit removal.
        assert_eq!(envs.get(OsStr::new("TMUX")), Some(&None));
        assert_eq!(envs.get(OsStr::new("TMUX_PANE")), Some(&None));
    }

    #[test]
    fn command_sets_shell_only_when_the_environment_has_none() {
        let expected = shell_override(std::env::var_os("SHELL"), || {
            util::passwd_entry().map(|user| user.shell)
        });
        let cmd = cfg(SocketSel::Default).command(&[]);
        let envs: HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        match &expected {
            Some(shell) => assert_eq!(
                envs.get(OsStr::new("SHELL")),
                Some(&Some(shell.as_os_str()))
            ),
            None => assert!(!envs.contains_key(OsStr::new("SHELL"))),
        }
    }

    #[test]
    fn shell_override_keeps_a_non_empty_environment_shell() {
        let consulted = std::cell::Cell::new(false);
        let shell = shell_override(Some(OsString::from("/bin/zsh")), || {
            consulted.set(true);
            Some(PathBuf::from("/bin/bash"))
        });
        assert_eq!(shell, None);
        assert!(!consulted.get(), "passwd must not be consulted");
    }

    #[test]
    fn shell_override_uses_the_passwd_shell_when_unset_or_empty() {
        for env_shell in [None, Some(OsString::new())] {
            let shell = shell_override(env_shell, || Some(PathBuf::from("/bin/bash")));
            assert_eq!(shell, Some(PathBuf::from("/bin/bash")));
        }
    }

    #[test]
    fn shell_override_gives_nothing_when_passwd_has_no_usable_shell() {
        assert_eq!(shell_override(None, || None), None);
        assert_eq!(shell_override(None, || Some(PathBuf::new())), None);
    }

    // ---- errors and serde ---------------------------------------------------------

    #[test]
    fn error_messages_are_stable() {
        assert_eq!(
            TmuxError::NoServer.to_string(),
            "tmux server is not running"
        );
        assert_eq!(
            TmuxError::Command("boom".into()).to_string(),
            "tmux command failed: boom"
        );
        assert_eq!(
            TmuxError::Parse("bad".into()).to_string(),
            "could not parse tmux output: bad"
        );
        let io_err = io::Error::new(io::ErrorKind::NotFound, "no such tmux");
        assert_eq!(TmuxError::from(io_err).to_string(), "no such tmux");
    }

    #[test]
    fn server_identity_id_is_start_time_dash_pid() {
        let identity = ServerIdentity {
            pid: 964016,
            start_time: 1793122138,
            socket_path: SERVER_SOCKET.to_string(),
            version: "3.4".to_string(),
        };
        assert_eq!(identity.id(), "1793122138-964016");
        // Usable as a map key.
        let set: HashSet<ServerIdentity> = [identity.clone(), identity].into_iter().collect();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn server_identity_serialises_with_snake_case_fields() {
        let identity = ServerIdentity {
            pid: 7,
            start_time: 1700000000,
            socket_path: "/s".to_string(),
            version: "3.4".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&identity).unwrap(),
            json!({"pid": 7, "start_time": 1700000000, "socket_path": "/s", "version": "3.4"})
        );
    }

    #[test]
    fn session_info_serialises_and_defaults_claude_panes_when_absent() {
        let info = SessionInfo {
            id: "$3".to_string(),
            name: "main".to_string(),
            windows: 2,
            attached: 1,
            created: 10,
            activity: 20,
            group: None,
            claude_panes: 4,
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({
                "id": "$3", "name": "main", "windows": 2, "attached": 1,
                "created": 10, "activity": 20, "group": null, "claude_panes": 4
            })
        );
        let without: SessionInfo = serde_json::from_value(json!({
            "id": "$3", "name": "main", "windows": 2, "attached": 1,
            "created": 10, "activity": 20, "group": "g"
        }))
        .unwrap();
        assert_eq!(without.claude_panes, 0);
        assert_eq!(without.group.as_deref(), Some("g"));
    }

    // ---- parse_sessions ------------------------------------------------------------

    #[test]
    fn session_format_matches_the_parser_field_order() {
        let placeholders: Vec<&str> = SESSION_FORMAT.split('\t').collect();
        assert_eq!(
            placeholders,
            [
                "#{pid}",
                "#{start_time}",
                "#{socket_path}",
                "#{version}",
                "#{session_id}",
                "#{session_windows}",
                "#{session_attached}",
                "#{session_created}",
                "#{session_activity}",
                "#{session_grouped}",
                "#{session_group}",
                "#{session_name}",
            ]
        );
        assert_eq!(placeholders.len(), SESSION_FIELDS);
    }

    #[test]
    fn parse_sessions_reads_identity_and_sessions() {
        let mut second = base_fields();
        second[4] = "$1";
        second[5] = "3";
        second[6] = "0";
        second[10] = "";
        second[11] = "work";
        let out = format!("{}\n{}\n", line_of(base_fields()), line_of(second));

        let (identity, sessions) = parse_sessions(&out).unwrap();

        assert_eq!(
            identity,
            Some(ServerIdentity {
                pid: 4242,
                start_time: 1700000000,
                socket_path: SERVER_SOCKET.to_string(),
                version: "3.4".to_string(),
            })
        );
        assert_eq!(
            sessions,
            vec![
                SessionInfo {
                    id: "$0".to_string(),
                    name: "main".to_string(),
                    windows: 2,
                    attached: 1,
                    created: 1700000100,
                    activity: 1700000200,
                    group: None,
                    claude_panes: 0,
                },
                SessionInfo {
                    id: "$1".to_string(),
                    name: "work".to_string(),
                    windows: 3,
                    attached: 0,
                    created: 1700000100,
                    activity: 1700000200,
                    group: None,
                    claude_panes: 0,
                },
            ]
        );
    }

    #[test]
    fn parse_sessions_takes_the_identity_from_the_first_line() {
        let mut other = base_fields();
        other[0] = "9999";
        other[4] = "$1";
        other[11] = "other";
        let out = format!("{}\n{}", line_of(base_fields()), line_of(other));
        let (identity, sessions) = parse_sessions(&out).unwrap();
        assert_eq!(identity.unwrap().pid, 4242);
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn parse_sessions_sets_group_only_for_grouped_sessions_with_a_group_name() {
        let mut grouped = base_fields();
        grouped[9] = "1";
        grouped[10] = "team";
        let mut grouped_without_name = base_fields();
        grouped_without_name[4] = "$1";
        grouped_without_name[9] = "1";
        grouped_without_name[10] = "";
        let mut ungrouped_with_name = base_fields();
        ungrouped_with_name[4] = "$2";
        ungrouped_with_name[9] = "0";
        ungrouped_with_name[10] = "stale";
        let out = [grouped, grouped_without_name, ungrouped_with_name]
            .map(line_of)
            .join("\n");

        let (_, sessions) = parse_sessions(&out).unwrap();

        assert_eq!(sessions[0].group.as_deref(), Some("team"));
        assert_eq!(sessions[1].group, None);
        assert_eq!(sessions[2].group, None);
    }

    #[test]
    fn parse_sessions_keeps_tabs_inside_the_session_name() {
        let mut fields = base_fields();
        fields[11] = "odd\tname\twith tabs";
        let (_, sessions) = parse_sessions(&line_of(fields)).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "odd\tname\twith tabs");
    }

    #[test]
    fn parse_sessions_keeps_unicode_and_spaces_in_names() {
        let mut fields = base_fields();
        fields[11] = "café  日本語 ";
        let (_, sessions) = parse_sessions(&line_of(fields)).unwrap();
        assert_eq!(sessions[0].name, "café  日本語 ");
    }

    #[test]
    fn parse_sessions_of_empty_output_has_no_identity_and_no_sessions() {
        for out in ["", "\n", "\n\n"] {
            let (identity, sessions) = parse_sessions(out).unwrap();
            assert_eq!(identity, None, "output {out:?}");
            assert!(sessions.is_empty(), "output {out:?}");
        }
    }

    #[test]
    fn parse_sessions_accepts_missing_trailing_newline_and_blank_lines() {
        let mut second = base_fields();
        second[4] = "$1";
        second[11] = "b";
        let out = format!("{}\n\n{}", line_of(base_fields()), line_of(second));
        let (_, sessions) = parse_sessions(&out).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn parse_sessions_rejects_malformed_lines_and_names_them() {
        let too_few = "1\t2\t3";
        let garbage = "this is not tmux output";
        for bad in [too_few, garbage] {
            match parse_sessions(bad) {
                Err(TmuxError::Parse(message)) => {
                    // The line is quoted with Debug escaping, so tabs show up as \t.
                    let quoted = format!("{bad:?}");
                    assert!(
                        message.contains(&quoted),
                        "message {message:?} lacks {quoted}"
                    );
                    assert!(message.contains("line 1"), "message {message:?}");
                }
                other => panic!("expected Parse error for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_sessions_rejects_each_non_numeric_numeric_field() {
        // Fields 0, 1, 5, 6, 7, 8 are numbers; 2, 3, 4, 9, 10, 11 are text.
        for index in [0, 1, 5, 6, 7, 8] {
            let mut fields = base_fields();
            fields[index] = "abc";
            assert!(
                matches!(parse_sessions(&line_of(fields)), Err(TmuxError::Parse(_))),
                "field {index} should be numeric"
            );
        }
        // Negative counts are not valid for the unsigned fields.
        let mut fields = base_fields();
        fields[5] = "-1";
        assert!(matches!(
            parse_sessions(&line_of(fields)),
            Err(TmuxError::Parse(_))
        ));
    }

    #[test]
    fn parse_sessions_rejects_an_empty_session_id() {
        let mut fields = base_fields();
        fields[4] = "";
        assert!(matches!(
            parse_sessions(&line_of(fields)),
            Err(TmuxError::Parse(_))
        ));
    }

    #[test]
    fn parse_sessions_reports_the_line_number_of_a_later_bad_line() {
        let out = format!("{}\nbroken line\n", line_of(base_fields()));
        match parse_sessions(&out) {
            Err(TmuxError::Parse(message)) => {
                assert!(message.contains("line 2"), "message {message:?}");
                assert!(message.contains("broken line"), "message {message:?}");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    // ---- list/has/new/rename through a scripted runner --------------------------------

    #[test]
    fn list_sessions_asks_for_the_session_format_and_parses_the_reply() {
        let runner = ScriptedRunner::new(vec![Ok(format!("{}\n", line_of(base_fields())))]);
        let (identity, sessions) = list_sessions(&runner).unwrap();
        assert_eq!(identity.unwrap().pid, 4242);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            runner.calls(),
            vec![to_args(&["list-sessions", "-F", SESSION_FORMAT])]
        );
    }

    #[test]
    fn list_sessions_passes_no_server_through() {
        let runner = ScriptedRunner::new(vec![Err(TmuxError::NoServer)]);
        assert!(matches!(list_sessions(&runner), Err(TmuxError::NoServer)));
    }

    #[test]
    fn list_sessions_of_a_server_without_sessions_is_ok_empty() {
        let runner = ScriptedRunner::new(vec![Ok(String::new())]);
        let (identity, sessions) = list_sessions(&runner).unwrap();
        assert_eq!(identity, None);
        assert!(sessions.is_empty());
    }

    #[test]
    fn has_session_uses_an_exact_match_target() {
        let runner = ScriptedRunner::new(vec![Ok(String::new())]);
        assert!(has_session(&runner, "main").unwrap());
        assert_eq!(
            runner.calls(),
            vec![to_args(&["has-session", "-t", "=main"])]
        );
    }

    #[test]
    fn has_session_maps_missing_session_errors_to_false() {
        let runner = ScriptedRunner::new(vec![
            Err(TmuxError::Command("can't find session: main".into())),
            Err(TmuxError::Command("session not found: main".into())),
        ]);
        assert!(!has_session(&runner, "main").unwrap());
        assert!(!has_session(&runner, "main").unwrap());
    }

    #[test]
    fn has_session_keeps_no_server_and_other_failures_as_errors() {
        let runner = ScriptedRunner::new(vec![
            Err(TmuxError::NoServer),
            Err(TmuxError::Command("something else went wrong".into())),
            Err(TmuxError::Io(io::Error::from(
                io::ErrorKind::PermissionDenied,
            ))),
        ]);
        assert!(matches!(
            has_session(&runner, "main"),
            Err(TmuxError::NoServer)
        ));
        assert!(matches!(
            has_session(&runner, "main"),
            Err(TmuxError::Command(message)) if message == "something else went wrong"
        ));
        assert!(matches!(
            has_session(&runner, "main"),
            Err(TmuxError::Io(_))
        ));
    }

    #[test]
    fn new_session_builds_the_documented_argv_and_returns_the_trimmed_id() {
        let runner = ScriptedRunner::new(vec![Ok("$7\n".into()), Ok("$8\n".into())]);
        assert_eq!(new_session(&runner, "main", None).unwrap(), "$7");
        assert_eq!(
            new_session(&runner, "work", Some(Path::new("/srv/project"))).unwrap(),
            "$8"
        );
        assert_eq!(
            runner.calls(),
            vec![
                to_args(&[
                    "new-session",
                    "-d",
                    "-s",
                    "main",
                    "-P",
                    "-F",
                    "#{session_id}"
                ]),
                to_args(&[
                    "new-session",
                    "-d",
                    "-s",
                    "work",
                    "-c",
                    "/srv/project",
                    "-P",
                    "-F",
                    "#{session_id}"
                ]),
            ]
        );
    }

    #[test]
    fn escape_format_doubles_every_hash_and_nothing_else() {
        assert_eq!(escape_format("plain name"), "plain name");
        assert_eq!(escape_format(""), "");
        assert_eq!(escape_format("#"), "##");
        assert_eq!(escape_format("a#S"), "a##S");
        assert_eq!(escape_format("#(touch x)"), "##(touch x)");
        assert_eq!(escape_format("x#{pane_id}y"), "x##{pane_id}y");
        assert_eq!(escape_format("##"), "####");
        // Everything that is not a hash passes through untouched.
        assert_eq!(
            escape_format("café 日本語 '\"\\ $x"),
            "café 日本語 '\"\\ $x"
        );
    }

    #[test]
    fn new_session_escapes_hashes_in_the_name_and_the_directory() {
        let runner = ScriptedRunner::new(vec![Ok("$1\n".into())]);
        new_session(&runner, "a#S#(x)", Some(Path::new("/srv/p#{pane_id}"))).unwrap();
        assert_eq!(
            runner.calls(),
            vec![to_args(&[
                "new-session",
                "-d",
                "-s",
                "a##S##(x)",
                "-c",
                "/srv/p##{pane_id}",
                "-P",
                "-F",
                "#{session_id}"
            ])]
        );
    }

    #[test]
    fn rename_session_escapes_the_new_name_but_not_the_target() {
        let runner = ScriptedRunner::new(vec![Ok(String::new())]);
        rename_session(&runner, "old#1", "new#(x)").unwrap();
        assert_eq!(
            runner.calls(),
            vec![to_args(&["rename-session", "-t", "=old#1", "new##(x)"])]
        );
    }

    #[test]
    fn has_session_does_not_escape_the_target() {
        let runner = ScriptedRunner::new(vec![Ok(String::new())]);
        assert!(has_session(&runner, "a#S").unwrap());
        assert_eq!(
            runner.calls(),
            vec![to_args(&["has-session", "-t", "=a#S"])]
        );
    }

    #[test]
    fn new_session_reports_empty_output_as_a_parse_error() {
        let runner = ScriptedRunner::new(vec![Ok("\n".into())]);
        assert!(matches!(
            new_session(&runner, "main", None),
            Err(TmuxError::Parse(_))
        ));
    }

    #[test]
    fn new_session_accepts_exactly_one_session_id_around_whitespace() {
        for (output, id) in [
            ("$4\n", "$4"),
            ("$0", "$0"),
            ("$123\r\n", "$123"),
            (" \t$7 \n\n", "$7"),
        ] {
            let runner = ScriptedRunner::new(vec![Ok(output.into())]);
            assert_eq!(
                new_session(&runner, "main", None).unwrap(),
                id,
                "{output:?}"
            );
        }
    }

    #[test]
    fn new_session_rejects_output_that_is_not_exactly_one_session_id() {
        for output in [
            "$1\nunexpected",
            "",
            "4",
            "   \n",
            "$",
            "$\n",
            "$x",
            "$1x",
            "$-1",
            "$1 $2",
            "$1\n$2",
            "id=$1",
            "@1",
            "%1",
            "$\u{663}",
        ] {
            let runner = ScriptedRunner::new(vec![Ok(output.into())]);
            match new_session(&runner, "main", None) {
                Err(TmuxError::Parse(message)) => assert!(
                    message.contains(&format!("{output:?}")),
                    "the message must quote {output:?}: {message}"
                ),
                other => panic!("{output:?} must be a parse error, got {other:?}"),
            }
        }
    }

    #[test]
    fn new_session_passes_runner_errors_through() {
        let runner = ScriptedRunner::new(vec![Err(TmuxError::Command(
            "duplicate session: main".into(),
        ))]);
        assert!(matches!(
            new_session(&runner, "main", None),
            Err(TmuxError::Command(message)) if message == "duplicate session: main"
        ));
    }

    #[test]
    fn rename_session_targets_the_old_name_exactly() {
        let runner = ScriptedRunner::new(vec![Ok(String::new())]);
        rename_session(&runner, "old", "new").unwrap();
        assert_eq!(
            runner.calls(),
            vec![to_args(&["rename-session", "-t", "=old", "new"])]
        );
    }

    #[test]
    fn rename_session_passes_errors_through() {
        let runner = ScriptedRunner::new(vec![
            Err(TmuxError::Command("can't find session: old".into())),
            Err(TmuxError::NoServer),
        ]);
        assert!(matches!(
            rename_session(&runner, "old", "new"),
            Err(TmuxError::Command(_))
        ));
        assert!(matches!(
            rename_session(&runner, "old", "new"),
            Err(TmuxError::NoServer)
        ));
    }

    #[test]
    fn kill_session_targets_the_name_exactly() {
        let runner = ScriptedRunner::new(vec![Ok(String::new())]);
        kill_session(&runner, "old#1").unwrap();
        assert_eq!(
            runner.calls(),
            vec![to_args(&["kill-session", "-t", "=old#1"])]
        );
    }

    #[test]
    fn kill_session_passes_errors_through() {
        let runner = ScriptedRunner::new(vec![
            Err(TmuxError::Command("can't find session: old".into())),
            Err(TmuxError::NoServer),
        ]);
        assert!(matches!(
            kill_session(&runner, "old"),
            Err(TmuxError::Command(_))
        ));
        assert!(matches!(
            kill_session(&runner, "old"),
            Err(TmuxError::NoServer)
        ));
    }

    #[test]
    fn scroll_pane_up_enters_copy_mode_then_scrolls() {
        let runner = ScriptedRunner::new(vec![
            Ok(String::new()),
            Ok(String::new()),
            Ok("1 5\n".into()),
        ]);
        let state = scroll_pane(&runner, "a#S", -5).unwrap();
        assert_eq!(
            state,
            PaneScroll {
                in_mode: true,
                scroll_position: 5
            }
        );
        assert_eq!(
            runner.calls(),
            vec![
                to_args(&["copy-mode", "-e", "-t", "=a#S:"]),
                to_args(&["send-keys", "-t", "=a#S:", "-X", "-N", "5", "scroll-up"]),
                to_args(&[
                    "list-panes",
                    "-t",
                    "=a#S:",
                    "-f",
                    "#{pane_active}",
                    "-F",
                    "#{pane_in_mode} #{scroll_position}"
                ]),
            ]
        );
    }

    #[test]
    fn scroll_pane_down_scrolls_only_while_in_a_mode() {
        let runner = ScriptedRunner::new(vec![
            Ok("1 8\n".into()),
            Ok(String::new()),
            Ok("1 5\n".into()),
        ]);
        assert_eq!(scroll_pane(&runner, "main", 3).unwrap().scroll_position, 5);
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][0], "list-panes");
        assert_eq!(
            calls[1],
            to_args(&["send-keys", "-t", "=main:", "-X", "-N", "3", "scroll-down"])
        );
    }

    #[test]
    fn scroll_pane_down_outside_a_mode_sends_no_keys() {
        let runner = ScriptedRunner::new(vec![Ok("0 \n".into()), Ok("0 \n".into())]);
        let state = scroll_pane(&runner, "main", 4).unwrap();
        assert_eq!(
            state,
            PaneScroll {
                in_mode: false,
                scroll_position: 0
            }
        );
        assert!(
            runner.calls().iter().all(|call| call[0] == "list-panes"),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn scroll_pane_down_tolerates_a_mode_that_just_ended() {
        let runner = ScriptedRunner::new(vec![
            Ok("1 2\n".into()),
            Err(TmuxError::Command("not in a mode".into())),
            Ok("0 \n".into()),
        ]);
        assert!(!scroll_pane(&runner, "main", 9).unwrap().in_mode);
    }

    #[test]
    fn scroll_pane_passes_errors_through_and_rejects_odd_output() {
        let runner = ScriptedRunner::new(vec![Err(TmuxError::Command(
            "can't find session: ghost".into(),
        ))]);
        assert!(matches!(
            scroll_pane(&runner, "ghost", -1),
            Err(TmuxError::Command(_))
        ));
        let runner = ScriptedRunner::new(vec![Err(TmuxError::NoServer)]);
        assert!(matches!(
            scroll_pane(&runner, "ghost", 1),
            Err(TmuxError::NoServer)
        ));
        let runner = ScriptedRunner::new(vec![Ok("maybe\n".into())]);
        assert!(matches!(
            scroll_pane(&runner, "main", 1),
            Err(TmuxError::Parse(_))
        ));
    }

    // ---- validate_session_name / exact_target -----------------------------------------

    #[test]
    fn validate_session_name_accepts_good_names() {
        let long_ascii = "a".repeat(100);
        let long_multibyte = "é".repeat(100); // 200 bytes, 100 characters
        for good in [
            "main",
            "a",
            "my session",
            "work-2",
            "a_b",
            "123",
            "日本語",
            "emoji 🚀 ok",
            "dash-inside-ok",
            "x'y\"z",
            "tab\u{2003}inside is not at the edge", // EM SPACE in the middle
            long_ascii.as_str(),
            long_multibyte.as_str(),
        ] {
            assert_eq!(validate_session_name(good), Ok(()), "name {good:?}");
        }
    }

    #[test]
    fn validate_session_name_rejects_every_documented_case() {
        let too_long_ascii = "a".repeat(101);
        let too_long_multibyte = "é".repeat(101);
        let cases: Vec<(&str, &str)> = vec![
            ("", "empty"),
            (too_long_ascii.as_str(), "101 ascii chars"),
            (too_long_multibyte.as_str(), "101 multibyte chars"),
            ("a:b", "colon"),
            (":", "only colon"),
            ("a.b", "dot"),
            (".", "only dot"),
            ("a\nb", "newline"),
            ("a\tb", "tab"),
            ("a\0b", "NUL"),
            ("a\x1bb", "escape"),
            ("a\x7fb", "DEL"),
            ("a\u{85}b", "C1 control NEL"),
            ("-name", "leading dash"),
            ("-", "only dash"),
            (" name", "leading space"),
            ("name ", "trailing space"),
            ("   ", "only spaces"),
            ("\u{a0}name", "leading no-break space"),
            ("name\u{3000}", "trailing ideographic space"),
        ];
        for (name, why) in cases {
            let result = validate_session_name(name);
            assert!(
                matches!(&result, Err(message) if !message.is_empty()),
                "{why}: {name:?} should be rejected with a message, got {result:?}"
            );
        }
    }

    #[test]
    fn validate_session_name_messages_say_what_is_wrong() {
        let message = |name: &str| validate_session_name(name).unwrap_err();
        assert!(message("").contains("empty"));
        assert!(message(&"a".repeat(101)).contains("100"));
        assert!(message("a:b").contains("':'"));
        assert!(message("a\nb").contains("control"));
        assert!(message("-x").contains("'-'"));
        assert!(message(" x").contains("whitespace"));
    }

    #[test]
    fn exact_target_prefixes_an_equals_sign() {
        assert_eq!(exact_target("main"), "=main");
        assert_eq!(exact_target("with space"), "=with space");
        assert_eq!(exact_target(""), "=");
    }

    // ---- RealTmux::run, exercised with `sh` standing in for tmux -----------------------

    fn run_sh(script: &str, limits: RunLimits) -> Result<String, TmuxError> {
        run_limited(&sh_cfg(), &sh_script(script), limits)
    }

    #[test]
    fn run_returns_stdout_on_success() {
        let runner = RealTmux::new(sh_cfg());
        let out = runner.run(&sh_script("printf 'hello\\nworld\\n'")).unwrap();
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn run_ignores_stderr_output_on_success() {
        let runner = RealTmux::new(sh_cfg());
        let out = runner
            .run(&sh_script("echo noise >&2; printf out"))
            .unwrap();
        assert_eq!(out, "out");
    }

    #[test]
    fn run_connects_the_child_stdin_to_dev_null() {
        let runner = RealTmux::new(sh_cfg());
        let out = runner.run(&sh_script("readlink /proc/self/fd/0")).unwrap();
        assert_eq!(out.trim(), "/dev/null");
    }

    #[test]
    fn run_reads_output_larger_than_the_pipe_buffer_without_deadlocking() {
        let runner = RealTmux::new(sh_cfg());
        let out = runner
            .run(&sh_script("yes abcdefghij | head -c 300000"))
            .unwrap();
        assert_eq!(out.len(), 300_000);
    }

    #[test]
    fn run_reads_large_stdout_and_stderr_together() {
        let runner = RealTmux::new(sh_cfg());
        let result = runner.run(&sh_script(
            "yes out | head -c 200000; yes errtext | head -c 200000 >&2; exit 3",
        ));
        match result {
            Err(TmuxError::Command(message)) => {
                assert!(
                    message.starts_with("errtext"),
                    "message starts {:?}",
                    &message[..20]
                );
                assert_eq!(message.len(), 200_000 - 1, "trimmed trailing newline only");
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn run_maps_no_server_running_to_no_server() {
        let runner = RealTmux::new(sh_cfg());
        let result = runner.run(&sh_script(
            "echo 'no server running on /tmp/tmux-1000/default' >&2; exit 1",
        ));
        assert!(matches!(result, Err(TmuxError::NoServer)), "{result:?}");
    }

    #[test]
    fn run_maps_error_connecting_to_no_server() {
        let runner = RealTmux::new(sh_cfg());
        let result = runner.run(&sh_script(
            "echo 'error connecting to /tmp/tmux-1000/x (No such file or directory)' >&2; exit 1",
        ));
        assert!(matches!(result, Err(TmuxError::NoServer)), "{result:?}");
    }

    #[test]
    fn run_maps_server_exited_unexpectedly_to_no_server() {
        // What a client prints when it races kill-server.
        let runner = RealTmux::new(sh_cfg());
        let result = runner.run(&sh_script("echo 'server exited unexpectedly' >&2; exit 1"));
        assert!(matches!(result, Err(TmuxError::NoServer)), "{result:?}");
    }

    #[test]
    fn run_maps_every_no_server_message_even_amid_other_stderr_text() {
        let runner = RealTmux::new(sh_cfg());
        for message in NO_SERVER_MESSAGES {
            let script = format!("echo 'warning: something odd' >&2; echo '{message}' >&2; exit 1");
            let result = runner.run(&sh_script(&script));
            assert!(
                matches!(result, Err(TmuxError::NoServer)),
                "{message}: {result:?}"
            );
        }
    }

    #[test]
    fn run_does_not_mistake_similar_failures_for_no_server() {
        let runner = RealTmux::new(sh_cfg());
        for message in [
            "server exited",
            "unexpected server error",
            "duplicate session: main",
        ] {
            let script = format!("echo '{message}' >&2; exit 1");
            match runner.run(&sh_script(&script)) {
                Err(TmuxError::Command(text)) => assert_eq!(text, message),
                other => panic!("{message}: expected Command error, got {other:?}"),
            }
        }
    }

    #[test]
    fn run_keeps_no_server_text_on_stdout_from_meaning_no_server() {
        // Only stderr decides; a successful command printing the phrase is just output.
        let runner = RealTmux::new(sh_cfg());
        let out = runner.run(&sh_script("echo 'no server running'")).unwrap();
        assert_eq!(out, "no server running\n");
    }

    #[test]
    fn run_returns_trimmed_stderr_for_other_failures() {
        let runner = RealTmux::new(sh_cfg());
        let result = runner.run(&sh_script(
            "printf '  \\n duplicate session: main \\n\\n' >&2; exit 1",
        ));
        match result {
            Err(TmuxError::Command(message)) => assert_eq!(message, "duplicate session: main"),
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn run_describes_a_failure_without_stderr_by_its_exit_status() {
        let runner = RealTmux::new(sh_cfg());
        match runner.run(&sh_script("exit 7")) {
            Err(TmuxError::Command(message)) => {
                assert!(message.contains('7'), "message {message:?}");
            }
            other => panic!("expected Command error, got {other:?}"),
        }
    }

    #[test]
    fn run_reports_a_missing_binary_as_an_io_error() {
        let runner = RealTmux::new(TmuxConfig {
            bin: PathBuf::from("/nonexistent/definitely-not-tmux"),
            socket: SocketSel::Default,
        });
        match runner.run(&[]) {
            Err(TmuxError::Io(err)) => assert_eq!(err.kind(), io::ErrorKind::NotFound),
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn run_kills_and_reaps_a_command_that_exceeds_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        // `exec` makes the shell itself the sleeper, so the pid we record is the one killed.
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let limits = RunLimits {
            timeout: Duration::from_secs(1),
            drain_grace: Duration::from_secs(1),
        };

        let started = Instant::now();
        let result = run_sh(&script, limits);
        let elapsed = started.elapsed();

        match result {
            Err(TmuxError::Command(message)) => {
                assert!(
                    message.starts_with("timed out after 1s: "),
                    "message {message:?}"
                );
                assert!(
                    message.contains("exec sleep 30"),
                    "message lists the args: {message:?}"
                );
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_secs(1),
            "returned early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "took too long: {elapsed:?}"
        );

        // A killed-but-unreaped child would still answer kill(pid, 0) as a zombie.
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let probe = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
        assert_eq!(
            probe,
            Err(nix::errno::Errno::ESRCH),
            "child {pid} was not reaped"
        );
    }

    #[test]
    fn run_returns_promptly_when_a_stray_process_keeps_the_pipes_open() {
        // The shell exits at once but its background `sleep` still holds our pipes.
        let limits = RunLimits {
            timeout: Duration::from_secs(5),
            drain_grace: Duration::from_millis(200),
        };
        let started = Instant::now();
        let out = run_sh("sleep 1 & echo done", limits).unwrap();
        assert_eq!(out, "done\n");
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "waited for the stray process: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn real_tmux_is_a_shareable_trait_object() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<RealTmux>();
        assert_send_sync::<dyn TmuxRunner>();
        let runner: Box<dyn TmuxRunner> = Box::new(RealTmux::new(sh_cfg()));
        assert_eq!(runner.run(&sh_script("printf ok")).unwrap(), "ok");
    }

    // ---- integration: the real tmux binary on an isolated socket -------------------------

    /// Kills the isolated test server when dropped, also when the test panics.
    ///
    /// Everything here builds its own `tmux -L <socket> …` command line instead of going
    /// through [`TmuxConfig::command`], so cleanup never depends on the code under test and
    /// can never address a server other than the isolated one.
    struct ServerGuard {
        socket: String,
    }

    impl ServerGuard {
        fn new(socket: &str) -> Self {
            assert!(
                socket.starts_with("tmw-scaffold-"),
                "refusing to manage a socket that is not a scaffold test socket: {socket}"
            );
            Self {
                socket: socket.to_string(),
            }
        }

        fn tmux(&self, args: &[&str]) -> Command {
            let mut cmd = Command::new("tmux");
            cmd.arg("-L")
                .arg(&self.socket)
                .args(args)
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .stdin(Stdio::null());
            cmd
        }

        /// `tmux -L <socket> kill-server`.
        fn kill_server(&self) {
            let _ = self
                .tmux(&["kill-server"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        /// True once a client can no longer reach a server on the socket. Right after
        /// `kill-server` a client can briefly see "server exited unexpectedly", which is not
        /// yet "gone".
        fn server_is_gone(&self) -> bool {
            match self.tmux(&["list-sessions"]).output() {
                Ok(out) if !out.status.success() => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    stderr.contains("no server running") || stderr.contains("error connecting to")
                }
                Ok(_) => false,
                // tmux itself cannot run, so there is no server to wait for.
                Err(_) => true,
            }
        }

        fn wait_until_gone(&self, patience: Duration) -> bool {
            let deadline = Instant::now() + patience;
            while Instant::now() < deadline {
                if self.server_is_gone() {
                    return true;
                }
                thread::sleep(Duration::from_millis(25));
            }
            self.server_is_gone()
        }

        /// tmux's socket file for `-L <socket>`: `$TMUX_TMPDIR` (else /tmp) / tmux-<uid> / name.
        fn socket_file(&self) -> PathBuf {
            let base = std::env::var_os("TMUX_TMPDIR")
                .filter(|dir| !dir.is_empty())
                .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
            base.join(format!("tmux-{}", nix::unistd::getuid()))
                .join(&self.socket)
        }
    }

    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.kill_server();
            if !self.wait_until_gone(Duration::from_secs(5)) {
                eprintln!(
                    "warning: tmux server on -L {} did not exit after kill-server",
                    self.socket
                );
                return;
            }
            // kill-server leaves its (now dead) socket file behind; remove only that.
            let file = self.socket_file();
            if fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_socket()) {
                let _ = fs::remove_file(&file);
            }
        }
    }

    fn session_names(sessions: &[SessionInfo]) -> Vec<&str> {
        let mut names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    fn find<'a>(sessions: &'a [SessionInfo], name: &str) -> &'a SessionInfo {
        sessions
            .iter()
            .find(|session| session.name == name)
            .unwrap_or_else(|| panic!("no session named {name:?} in {sessions:?}"))
    }

    #[test]
    fn real_tmux_session_lifecycle_on_an_isolated_socket() {
        let socket = format!("tmw-scaffold-{}", util::random_hex(4));
        // Created before any server exists, so it cleans up on every exit path.
        let guard = ServerGuard::new(&socket);
        // Visible with `cargo test -- --nocapture`; lets a human confirm the cleanup.
        eprintln!("tmux integration test socket: -L {socket}");

        let config = TmuxConfig {
            bin: PathBuf::from("tmux"),
            socket: SocketSel::Name(socket.clone()),
        };
        let r = RealTmux::new(config.clone());

        // Safety pre-flight: every command built from this config must address the
        // isolated socket, or the calls below could reach the operator's real server.
        let preflight = config.command(&to_args(&["list-sessions"]));
        let preflight_args: Vec<&OsStr> = preflight.get_args().collect();
        assert_eq!(
            &preflight_args[..2],
            &[OsStr::new("-L"), OsStr::new(&socket)],
            "tmux would not be isolated on its own socket"
        );
        assert_eq!(config.describe(), format!("-L {socket}"));

        // No server yet: every operation reports NoServer instead of starting one.
        assert!(matches!(list_sessions(&r), Err(TmuxError::NoServer)));
        assert!(matches!(has_session(&r, "main"), Err(TmuxError::NoServer)));
        assert!(matches!(
            rename_session(&r, "main", "other"),
            Err(TmuxError::NoServer)
        ));

        // Creating the first session starts the server.
        let workdir = tempfile::tempdir().unwrap();
        let main_id = new_session(&r, "main", None).unwrap();
        let work_id = new_session(&r, "work", Some(workdir.path())).unwrap();
        assert!(main_id.starts_with('$'), "session id {main_id:?}");
        assert!(work_id.starts_with('$'), "session id {work_id:?}");
        assert_ne!(main_id, work_id);
        assert!(
            matches!(
                new_session(&r, "main", None),
                Err(TmuxError::Command(ref message)) if message.contains("duplicate session")
            ),
            "creating an existing session must fail with tmux's message"
        );

        // Both sessions are listed, with a valid server identity.
        let before = util::now_secs();
        let (identity, sessions) = list_sessions(&r).unwrap();
        let identity = identity.expect("a running server has an identity");
        assert!(identity.pid > 0, "{identity:?}");
        assert!(identity.start_time > 0, "{identity:?}");
        assert!(!identity.version.is_empty(), "{identity:?}");
        assert!(
            identity.socket_path.ends_with(&socket),
            "socket path {:?} should end with {socket}",
            identity.socket_path
        );
        assert_eq!(
            identity.id(),
            format!("{}-{}", identity.start_time, identity.pid)
        );
        assert_eq!(session_names(&sessions), ["main", "work"]);
        let main = find(&sessions, "main");
        let work = find(&sessions, "work");
        assert_eq!(main.id, main_id);
        assert_eq!(work.id, work_id);
        for session in [main, work] {
            assert_eq!(session.windows, 1, "{session:?}");
            assert_eq!(session.attached, 0, "{session:?}");
            assert_eq!(session.group, None, "{session:?}");
            assert_eq!(session.claude_panes, 0, "{session:?}");
            assert!(
                (before - 60..=before + 5).contains(&session.created),
                "created {} not near now {before}",
                session.created
            );
            assert!(session.activity >= session.created, "{session:?}");
        }
        assert_eq!(
            fs::canonicalize(
                r.run(&to_args(&[
                    "display-message",
                    "-p",
                    "-t",
                    "=work:",
                    "#{session_path}"
                ]))
                .unwrap()
                .trim()
            )
            .unwrap(),
            fs::canonicalize(workdir.path()).unwrap(),
            "new_session must honour the cwd argument"
        );

        // has_session: exact names only.
        assert!(has_session(&r, "main").unwrap());
        assert!(has_session(&r, "work").unwrap());
        assert!(!has_session(&r, "nope").unwrap());
        // `exact_target` prevents prefix matching: plain `-t ma` would have matched "main".
        assert!(!has_session(&r, "ma").unwrap());
        assert!(!has_session(&r, "wor").unwrap());

        // rename_session keeps the id and moves the name.
        rename_session(&r, "work", "play").unwrap();
        let (_, sessions) = list_sessions(&r).unwrap();
        assert_eq!(session_names(&sessions), ["main", "play"]);
        assert_eq!(find(&sessions, "play").id, work_id);
        assert!(!has_session(&r, "work").unwrap());
        assert!(has_session(&r, "play").unwrap());
        assert!(matches!(
            rename_session(&r, "play", "main"),
            Err(TmuxError::Command(ref message)) if message.contains("duplicate session")
        ));
        assert!(matches!(
            rename_session(&r, "ghost", "anything"),
            Err(TmuxError::Command(ref message)) if message.contains("can't find session")
        ));

        // Grouped sessions report their group; ungrouped ones do not.
        r.run(&to_args(&[
            "new-session",
            "-d",
            "-s",
            "twin",
            "-t",
            "=main",
        ]))
        .unwrap();
        let (_, sessions) = list_sessions(&r).unwrap();
        assert_eq!(session_names(&sessions), ["main", "play", "twin"]);
        let main_group = find(&sessions, "main").group.clone();
        assert!(main_group.is_some(), "main should be grouped now");
        assert_eq!(find(&sessions, "twin").group, main_group);
        assert_eq!(find(&sessions, "play").group, None);

        // Stop the server through the guard's own command line (never through the code under
        // test) and watch it disappear.
        guard.kill_server();
        assert!(
            guard.wait_until_gone(Duration::from_secs(5)),
            "server did not exit"
        );
        assert!(matches!(list_sessions(&r), Err(TmuxError::NoServer)));
        assert!(matches!(has_session(&r, "main"), Err(TmuxError::NoServer)));
    }
}
