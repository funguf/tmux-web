//! Snoozing Claude Code: stop the Claude processes of a tmux session, remember their
//! conversations, and reopen them later with `--resume`.
//!
//! The store is `<state-dir>/snoozed.json` (`{"version":1,"entries":[…]}`). The planning
//! functions are pure (they work on a [`Snapshot`]); only [`stop_process`] signals anything, and
//! it signals exactly the one pid it is given after re-checking that the pid still is the same
//! Claude process.

use crate::claude::is_claude_process;
use crate::procscan::{self, ProcTable};
use crate::snapshot::Snapshot;
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// File name inside the state directory.
pub const SNOOZE_FILE: &str = "snoozed.json";

const STORE_VERSION: u32 = 1;

/// One snoozed Claude conversation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnoozeEntry {
    /// tmux session NAME
    pub session: String,
    /// "%N"
    pub pane_id: String,
    pub window_index: i64,
    pub pane_index: i64,
    pub claude_session_id: String,
    pub cwd: String,
    pub args: Vec<String>,
    pub config_dir: Option<String>,
    /// unix ms
    pub snoozed_at: i64,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    entries: Vec<SnoozeEntry>,
}

#[derive(Default)]
struct StoreState {
    entries: Vec<SnoozeEntry>,
    /// (session, conversation) pairs whose process is being stopped right now: a scan that still
    /// sees them running must not take that for "resumed by hand".
    stopping: HashSet<(String, String)>,
}

/// The snoozed conversations, persisted after every change.
pub struct SnoozeStore {
    path: PathBuf,
    state: Mutex<StoreState>,
}

impl SnoozeStore {
    /// Loads `<state_dir>/snoozed.json`. A missing file is an empty store; a corrupt one is
    /// logged and treated as empty (and left alone until the next successful write).
    pub fn open(state_dir: &Path) -> SnoozeStore {
        let path = state_dir.join(SNOOZE_FILE);
        let entries = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoreFile>(&bytes) {
                Ok(file) => file.entries,
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "ignoring an unreadable snooze store");
                    Vec::new()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "cannot read the snooze store");
                Vec::new()
            }
        };
        SnoozeStore {
            path,
            state: Mutex::new(StoreState {
                entries,
                stopping: HashSet::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, StoreState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies `change` to the entries and saves; on a failed save the change is undone.
    fn mutate(&self, change: impl FnOnce(&mut Vec<SnoozeEntry>)) -> io::Result<()> {
        let mut state = self.lock();
        let before = state.entries.clone();
        change(&mut state.entries);
        if state.entries == before {
            return Ok(());
        }
        let saved = self.save(&state.entries);
        if saved.is_err() {
            state.entries = before;
        }
        saved
    }

    fn save(&self, entries: &[SnoozeEntry]) -> io::Result<()> {
        let file = StoreFile {
            version: STORE_VERSION,
            entries: entries.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        util::atomic_write(&self.path, &bytes, 0o600)
    }

    pub fn entries_for(&self, session: &str) -> Vec<SnoozeEntry> {
        self.lock()
            .entries
            .iter()
            .filter(|entry| entry.session == session)
            .cloned()
            .collect()
    }

    /// Adds entries; an entry for the same session and conversation is replaced.
    pub fn add(&self, new: Vec<SnoozeEntry>) -> io::Result<()> {
        self.mutate(|entries| {
            for entry in new {
                entries.retain(|old| {
                    !(old.session == entry.session
                        && old.claude_session_id == entry.claude_session_id)
                });
                entries.push(entry);
            }
        })
    }

    pub fn remove_for(&self, session: &str, claude_session_ids: &[String]) -> io::Result<()> {
        self.mutate(|entries| {
            entries.retain(|entry| {
                !(entry.session == session && claude_session_ids.contains(&entry.claude_session_id))
            });
        })
    }

    pub fn remove_session(&self, session: &str) -> io::Result<()> {
        self.mutate(|entries| entries.retain(|entry| entry.session != session))
    }

    pub fn rename_session(&self, old: &str, new: &str) -> io::Result<()> {
        self.mutate(|entries| {
            for entry in entries.iter_mut().filter(|entry| entry.session == old) {
                entry.session = new.to_string();
            }
        })
    }

    /// Marks conversations as being stopped (see [`StoreState::stopping`]).
    pub fn mark_stopping(&self, entries: &[SnoozeEntry]) {
        let mut state = self.lock();
        for entry in entries {
            state
                .stopping
                .insert((entry.session.clone(), entry.claude_session_id.clone()));
        }
    }

    pub fn clear_stopping(&self, entries: &[SnoozeEntry]) {
        let mut state = self.lock();
        for entry in entries {
            state
                .stopping
                .remove(&(entry.session.clone(), entry.claude_session_id.clone()));
        }
    }

    /// Drops the entries of `session` whose conversation is running again (the operator resumed
    /// it by hand), except those being stopped right now.
    pub fn drop_running(&self, session: &str, running: &HashSet<String>) -> io::Result<()> {
        let stopping = self.lock().stopping.clone();
        self.mutate(|entries| {
            entries.retain(|entry| {
                entry.session != session
                    || !running.contains(&entry.claude_session_id)
                    || stopping.contains(&(entry.session.clone(), entry.claude_session_id.clone()))
            });
        })
    }

    /// Entries of `session` whose conversation is not in `running`.
    pub fn snoozed_count(&self, session: &str, running: &HashSet<String>) -> u32 {
        let count = self
            .lock()
            .entries
            .iter()
            .filter(|entry| entry.session == session)
            .filter(|entry| !running.contains(&entry.claude_session_id))
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}

// ---- wake command -----------------------------------------------------------------------------

/// Single-quotes `token` unless it is made only of `[A-Za-z0-9_/.:@%+=,-]` (and not empty).
pub fn shell_quote(token: &str) -> String {
    let plain = !token.is_empty()
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_/.:@%+=,-".contains(&byte));
    if plain {
        token.to_string()
    } else {
        format!("'{}'", token.replace('\'', r"'\''"))
    }
}

/// Flags that never take a value: a word after one of these is a prompt, not its value.
const BOOLEAN_FLAGS: [&str; 10] = [
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--verbose",
    "--debug",
    "--mcp-debug",
    "--ide",
    "--strict-mcp-config",
    "--fork-session",
    "--continue",
    "-c",
];

/// `args` without anything that picks a conversation, without a positional prompt (it must not
/// be sent again), and without repeated switches.
///
/// A word is kept only as the value of the `--long` flag right before it, unless that flag is a
/// known boolean one (an unknown flag followed by a word is assumed to take it). `--` ends the
/// arguments: it and everything after it are dropped.
fn cleaned_args(args: &[String]) -> Vec<&str> {
    let is_switch = |index: usize| {
        let arg = args[index].as_str();
        BOOLEAN_FLAGS.contains(&arg)
            || (arg.starts_with('-')
                && args
                    .get(index + 1)
                    .map_or(true, |next| next.starts_with('-')))
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut seen_switches: HashSet<&str> = HashSet::new();
    // The last kept token, while nothing was dropped after it.
    let mut previous: Option<&str> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            break;
        }
        match arg {
            "--resume" | "-r" | "--session-id" => {
                let takes_value = args
                    .get(index + 1)
                    .is_some_and(|value| !value.starts_with('-'));
                index += if takes_value { 2 } else { 1 };
                previous = None;
                continue;
            }
            "--continue" | "-c" => {
                index += 1;
                previous = None;
                continue;
            }
            _ if arg.starts_with("--resume=") || arg.starts_with("--session-id=") => {
                index += 1;
                previous = None;
                continue;
            }
            _ => {}
        }
        if arg.starts_with('-') {
            if is_switch(index) && !seen_switches.insert(arg) {
                index += 1;
                continue;
            }
        } else if !previous
            .is_some_and(|flag| flag.starts_with("--") && !BOOLEAN_FLAGS.contains(&flag))
        {
            // A positional prompt.
            index += 1;
            previous = None;
            continue;
        }
        kept.push(arg);
        previous = Some(arg);
        index += 1;
    }
    kept
}

/// The shell command typed into the pane to reopen the conversation: `cd <cwd> && ` (when the
/// entry has a cwd, because `--resume` only finds a conversation from the folder it started in),
/// an optional `CLAUDE_CONFIG_DIR=<dir> `, then `claude`, the original arguments without any
/// resume/continue/session-id form, positional prompt, `--` or repeated switch, and finally
/// `--resume <id>`. Every token is shell-quoted unless it is plainly safe.
pub fn wake_command(entry: &SnoozeEntry) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !entry.cwd.is_empty() {
        parts.push(format!("cd {} &&", shell_quote(&entry.cwd)));
    }
    if let Some(dir) = &entry.config_dir {
        parts.push(format!("CLAUDE_CONFIG_DIR={}", shell_quote(dir)));
    }
    parts.push("claude".to_string());
    parts.extend(cleaned_args(&entry.args).into_iter().map(shell_quote));
    parts.push("--resume".to_string());
    parts.push(shell_quote(&entry.claude_session_id));
    parts.join(" ")
}

// ---- planning ---------------------------------------------------------------------------------

/// Why a snooze or wake request is refused. Nothing has been touched when one is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotFound(String),
    NoClaude(String),
    NoSessionId(String),
    Busy(String),
    NotInShell(String),
    PaneMissing(String),
    PaneBusy(String),
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NotFound(_) => "not_found",
            Refusal::NoClaude(_) => "no_claude",
            Refusal::NoSessionId(_) => "no_session_id",
            Refusal::Busy(_) => "busy",
            Refusal::NotInShell(_) => "not_in_shell",
            Refusal::PaneMissing(_) => "pane_missing",
            Refusal::PaneBusy(_) => "pane_busy",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Refusal::NotFound(message)
            | Refusal::NoClaude(message)
            | Refusal::NoSessionId(message)
            | Refusal::Busy(message)
            | Refusal::NotInShell(message)
            | Refusal::PaneMissing(message)
            | Refusal::PaneBusy(message) => message,
        }
    }

    /// 404 for an unknown session, 409 for everything else.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Refusal::NotFound(_))
    }
}

/// A Claude process to stop, with the entry that remembers its conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnoozeTarget {
    pub pane_pid: u32,
    pub claude_pid: u32,
    pub entry: SnoozeEntry,
}

/// Whether `argv` is an interactive shell: argv[0] (a leading `-` stripped, then its basename)
/// is a shell, and every later argument is an option other than `-c` (also inside a cluster
/// such as `-lc`). `-bash`, `/bin/bash`, `bash -l`, `zsh -i` qualify; `bash -c …`, `bash script`,
/// `sh script.sh` and `node` do not.
pub fn is_interactive_shell(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    let program = program.strip_prefix('-').unwrap_or(program);
    let name = program.rsplit('/').next().unwrap_or(program);
    if !SHELLS.contains(&name) {
        return false;
    }
    argv[1..].iter().all(|arg| {
        let single_dash_cluster = arg.starts_with('-') && !arg.starts_with("--");
        arg.starts_with('-') && !(single_dash_cluster && arg.contains('c'))
    })
}

/// Every Claude pane of session `name`, or the reason none may be snoozed.
///
/// `root_argv(pid)` is the command line of a pane's root process. Claude must have been started
/// from an interactive shell there: a pane whose root is a wrapper (`bash -c "claude …"`, a
/// script) ends when Claude does, and the session may go with it. An unreadable command line
/// counts as not interactive.
pub fn plan_snooze(
    snapshot: &Snapshot,
    name: &str,
    now_ms: i64,
    root_argv: &dyn Fn(u32) -> Option<Vec<String>>,
) -> Result<Vec<SnoozeTarget>, Refusal> {
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.name == name)
        .ok_or_else(|| Refusal::NotFound(format!("no session named {name:?}")))?;
    let mut targets = Vec::new();
    for window in &session.windows {
        for pane in &window.panes {
            let Some(claude) = &pane.claude else { continue };
            let Some(session_id) = claude.session_id.clone() else {
                return Err(Refusal::NoSessionId(format!(
                    "Claude in {name} has no known conversation id, so it cannot be resumed"
                )));
            };
            if claude.status.as_deref() == Some("busy") {
                return Err(Refusal::Busy(format!(
                    "Claude is replying in {name}; try again when it is idle"
                )));
            }
            let interactive = claude.pid != pane.pid
                && root_argv(pane.pid).is_some_and(|argv| is_interactive_shell(&argv));
            if !interactive {
                return Err(Refusal::NotInShell(format!(
                    "Claude in {name} was not started from an interactive shell; stopping it would close the pane"
                )));
            }
            targets.push(SnoozeTarget {
                pane_pid: pane.pid,
                claude_pid: claude.pid,
                entry: SnoozeEntry {
                    session: name.to_string(),
                    pane_id: pane.id.clone(),
                    window_index: window.index,
                    pane_index: pane.index,
                    claude_session_id: session_id,
                    cwd: claude.cwd.clone(),
                    args: claude.args.clone(),
                    config_dir: claude.config_dir.clone(),
                    snoozed_at: now_ms,
                },
            });
        }
    }
    if targets.is_empty() {
        return Err(Refusal::NoClaude(format!(
            "Claude is not running in {name}"
        )));
    }
    Ok(targets)
}

const SHELLS: [&str; 6] = ["bash", "zsh", "sh", "dash", "fish", "ksh"];

fn is_shell(command: &str) -> bool {
    SHELLS.contains(&command.trim_start_matches('-'))
}

/// The pane to type each entry's wake command into (pane id, command); all checked before any
/// is typed.
pub fn plan_wake(
    snapshot: &Snapshot,
    name: &str,
    entries: &[SnoozeEntry],
) -> Result<Vec<(String, String)>, Refusal> {
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.name == name)
        .ok_or_else(|| Refusal::NotFound(format!("no session named {name:?}")))?;
    let mut plan = Vec::new();
    let mut used: HashSet<&str> = HashSet::new();
    for entry in entries {
        let panes = || {
            session
                .windows
                .iter()
                .flat_map(|window| window.panes.iter().map(move |pane| (window.index, pane)))
        };
        let found = panes()
            .find(|(_, pane)| pane.id == entry.pane_id)
            .or_else(|| {
                panes().find(|(window, pane)| {
                    *window == entry.window_index && pane.index == entry.pane_index
                })
            })
            .map(|(_, pane)| pane)
            .ok_or_else(|| {
                Refusal::PaneMissing(format!(
                    "the pane Claude ran in ({}) no longer exists in {name}",
                    entry.pane_id
                ))
            })?;
        if !is_shell(&found.current_command) {
            return Err(Refusal::PaneBusy(format!(
                "{} is running in the pane",
                found.current_command
            )));
        }
        if !used.insert(found.id.as_str()) {
            return Err(Refusal::PaneBusy(format!(
                "two conversations were snoozed from pane {}; wake them one at a time",
                found.id
            )));
        }
        plan.push((found.id.clone(), wake_command(entry)));
    }
    Ok(plan)
}

// ---- stopping a process -----------------------------------------------------------------------

/// What [`stop_process`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The process is gone; the sum of the resident bytes of it and its descendants just before.
    Stopped { freed_bytes: u64 },
    /// The pid was not (or no longer) the expected Claude process: nothing was signalled.
    Skipped,
    /// Still alive after SIGKILL.
    Stuck,
}

/// How long SIGTERM is given, how long SIGKILL, and how often the pid is looked at.
#[derive(Debug, Clone, Copy)]
pub struct StopTimings {
    pub term_grace: Duration,
    pub kill_grace: Duration,
    pub poll: Duration,
}

impl Default for StopTimings {
    fn default() -> Self {
        Self {
            term_grace: Duration::from_secs(10),
            kill_grace: Duration::from_secs(2),
            poll: Duration::from_millis(100),
        }
    }
}

/// `(stat fields, state letter)` of a process, from `<proc_root>/<pid>/stat`.
fn stat_of(proc_root: &Path, pid: u32) -> Option<(procscan::ProcInfo, char)> {
    let text = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let info = procscan::parse_stat(&text)?;
    let state = text.rsplit_once(')')?.1.trim_start().chars().next()?;
    Some((info, state))
}

/// The start time of `pid` when it is a Claude process, else None.
pub fn claude_start_ticks(proc_root: &Path, pid: u32) -> Option<u64> {
    let (info, state) = stat_of(proc_root, pid)?;
    if matches!(state, 'Z' | 'X') {
        return None;
    }
    let argv = procscan::read_cmdline(proc_root, pid).unwrap_or_default();
    is_claude_process(&info.comm, &argv).then_some(info.start_ticks)
}

/// Whether the process that had `start_ticks` is gone (exited, reaped, or its pid reused).
fn is_gone(proc_root: &Path, pid: u32, start_ticks: u64) -> bool {
    match stat_of(proc_root, pid) {
        Some((info, state)) => info.start_ticks != start_ticks || matches!(state, 'Z' | 'X'),
        None => true,
    }
}

/// Resident set size of one process, in bytes (0 when unreadable).
pub fn rss_bytes(proc_root: &Path, pid: u32) -> u64 {
    let Ok(text) = fs::read_to_string(proc_root.join(pid.to_string()).join("statm")) else {
        return 0;
    };
    let pages: u64 = text
        .split_whitespace()
        .nth(1)
        .and_then(|field| field.parse().ok())
        .unwrap_or(0);
    pages.saturating_mul(page_size())
}

fn page_size() -> u64 {
    nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .and_then(|size| u64::try_from(size).ok())
        .filter(|size| *size > 0)
        .unwrap_or(4096)
}

fn signal(pid: u32, which: nix::sys::signal::Signal) {
    // A pid that does not fit a positive i32 would address a process group: never.
    let Ok(raw) = i32::try_from(pid) else { return };
    if raw <= 1 {
        return;
    }
    // ESRCH (already gone) is the outcome we want anyway.
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), which);
}

/// Stops one Claude process: SIGTERM, then SIGKILL when it does not leave in time.
///
/// Before each signal it re-checks that `pid` still has `start_ticks` and still is a Claude
/// process; otherwise nothing is signalled ([`StopOutcome::Skipped`]). The resident memory of the
/// process and its descendants (MCP servers and the like) is summed before the signal.
pub fn stop_process(
    proc_root: &Path,
    pid: u32,
    start_ticks: u64,
    timings: StopTimings,
) -> StopOutcome {
    use nix::sys::signal::Signal;
    let still_that_claude = || claude_start_ticks(proc_root, pid) == Some(start_ticks);
    if !still_that_claude() {
        return StopOutcome::Skipped;
    }
    let freed_bytes = match ProcTable::scan(proc_root) {
        Ok(table) => std::iter::once(pid)
            .chain(table.descendants(pid))
            .map(|member| rss_bytes(proc_root, member))
            .sum(),
        Err(_) => rss_bytes(proc_root, pid),
    };
    if !still_that_claude() {
        return StopOutcome::Skipped;
    }
    signal(pid, Signal::SIGTERM);
    if wait_gone(
        proc_root,
        pid,
        start_ticks,
        timings.term_grace,
        timings.poll,
    ) {
        return StopOutcome::Stopped { freed_bytes };
    }
    if !still_that_claude() {
        return StopOutcome::Stopped { freed_bytes };
    }
    signal(pid, Signal::SIGKILL);
    if wait_gone(
        proc_root,
        pid,
        start_ticks,
        timings.kill_grace,
        timings.poll,
    ) {
        StopOutcome::Stopped { freed_bytes }
    } else {
        StopOutcome::Stuck
    }
}

fn wait_gone(
    proc_root: &Path,
    pid: u32,
    start_ticks: u64,
    limit: Duration,
    poll: Duration,
) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if is_gone(proc_root, pid, start_ticks) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::{ClaudeIdSource, ClaudeSnap};
    use crate::snapshot::{PaneSnap, SessionSnap, WindowSnap, SNAPSHOT_VERSION};
    use crate::tmux::ServerIdentity;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    const ID: &str = "a6967ea6-b23c-4d6a-8dee-914e0050be69";

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    fn entry(args: &[&str], config_dir: Option<&str>) -> SnoozeEntry {
        SnoozeEntry {
            session: "work".into(),
            pane_id: "%1".into(),
            window_index: 0,
            pane_index: 0,
            claude_session_id: ID.into(),
            cwd: "/w".into(),
            args: strings(args),
            config_dir: config_dir.map(str::to_string),
            snoozed_at: 1,
        }
    }

    // ---- wake_command --------------------------------------------------------------------

    #[test]
    fn wake_command_for_the_operators_real_shape() {
        let entry = entry(
            &[
                "--dangerously-skip-permissions",
                "--dangerously-skip-permissions",
                "--resume",
                ID,
            ],
            None,
        );
        assert_eq!(
            wake_command(&entry),
            format!("cd /w && claude --dangerously-skip-permissions --resume {ID}")
        );
    }

    #[test]
    fn wake_command_drops_every_way_of_picking_a_conversation() {
        for args in [
            vec!["-c"],
            vec!["--continue"],
            vec!["-r", "other-id-12345"],
            vec!["--resume=other-id-12345"],
            vec!["--session-id", "other-id-12345"],
            vec!["--session-id=other-id-12345"],
            vec!["--resume"],
        ] {
            assert_eq!(
                wake_command(&entry(&args, None)),
                format!("cd /w && claude --resume {ID}"),
                "{args:?}"
            );
        }
    }

    #[test]
    fn wake_command_quotes_odd_tokens_and_the_config_dir() {
        let quoted = entry(&["--model", "opus 5", "--append", "it's"], Some("/tmp/a b"));
        assert_eq!(
            wake_command(&quoted),
            format!(
                "cd /w && CLAUDE_CONFIG_DIR='/tmp/a b' claude --model 'opus 5' --append 'it'\\''s' --resume {ID}"
            )
        );
        let plain = entry(&[], Some("/home/u/.claude-alt"));
        assert_eq!(
            wake_command(&plain),
            format!("cd /w && CLAUDE_CONFIG_DIR=/home/u/.claude-alt claude --resume {ID}")
        );
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a$b"), "'a$b'");
        assert_eq!(shell_quote("a;b"), "'a;b'");
    }

    #[test]
    fn wake_command_keeps_values_that_repeat_and_options_with_values() {
        let entry = entry(
            &["--add-dir", "x", "--add-dir", "y", "--verbose", "--verbose"],
            None,
        );
        assert_eq!(
            wake_command(&entry),
            format!("cd /w && claude --add-dir x --add-dir y --verbose --resume {ID}")
        );
    }

    #[test]
    fn wake_command_starts_in_the_conversations_folder() {
        let mut spaced = entry(&[], None);
        spaced.cwd = "/home/u/my project".into();
        assert_eq!(
            wake_command(&spaced),
            format!("cd '/home/u/my project' && claude --resume {ID}")
        );
        spaced.cwd = "/home/u/it's".into();
        assert_eq!(
            wake_command(&spaced),
            format!("cd '/home/u/it'\\''s' && claude --resume {ID}")
        );
        spaced.cwd = String::new();
        spaced.config_dir = Some("/c".into());
        assert_eq!(
            wake_command(&spaced),
            format!("CLAUDE_CONFIG_DIR=/c claude --resume {ID}")
        );
        let mut both = entry(&[], Some("/c"));
        both.cwd = "/w".into();
        assert_eq!(
            wake_command(&both),
            format!("cd /w && CLAUDE_CONFIG_DIR=/c claude --resume {ID}")
        );
    }

    #[test]
    fn wake_command_never_sends_a_prompt_again() {
        let plain = format!("cd /w && claude --resume {ID}");
        assert_eq!(wake_command(&entry(&["fix the bug"], None)), plain);
        assert_eq!(
            wake_command(&entry(&["--dangerously-skip-permissions", "hello"], None)),
            format!("cd /w && claude --dangerously-skip-permissions --resume {ID}")
        );
        assert_eq!(
            wake_command(&entry(&["--model", "opus", "hi"], None)),
            format!("cd /w && claude --model opus --resume {ID}")
        );
        assert_eq!(
            wake_command(&entry(&["--resume", "X", "--", "prompt"], None)),
            plain
        );
        assert_eq!(
            wake_command(&entry(&["--verbose", "--", "fix it"], None)),
            format!("cd /w && claude --verbose --resume {ID}")
        );
        // A word right after a dropped resume form is not the value of the flag before it.
        assert_eq!(
            wake_command(&entry(&["--foo", "--resume", "X", "word"], None)),
            format!("cd /w && claude --foo --resume {ID}")
        );
        // A known switch repeated around a prompt still collapses.
        assert_eq!(
            wake_command(&entry(
                &["--verbose", "p", "--verbose", "--model", "m", "q"],
                None
            )),
            format!("cd /w && claude --verbose --model m --resume {ID}")
        );
    }

    // ---- the store -----------------------------------------------------------------------

    #[test]
    fn the_store_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnoozeStore::open(dir.path());
        assert!(store.entries_for("work").is_empty());
        store.add(vec![entry(&[], None)]).unwrap();
        // The same conversation again replaces, not duplicates.
        store.add(vec![entry(&["-v"], None)]).unwrap();
        assert_eq!(store.entries_for("work").len(), 1);

        let reopened = SnoozeStore::open(dir.path());
        assert_eq!(reopened.entries_for("work"), store.entries_for("work"));
        let text = fs::read_to_string(dir.path().join(SNOOZE_FILE)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["entries"][0]["claude_session_id"], ID);
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.path().join(SNOOZE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_corrupt_file_is_empty_and_kept_until_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNOOZE_FILE);
        fs::write(&path, "{not json").unwrap();
        let store = SnoozeStore::open(dir.path());
        assert!(store.entries_for("work").is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
        // A change that changes nothing writes nothing either.
        store.remove_session("work").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
        store.add(vec![entry(&[], None)]).unwrap();
        assert_eq!(SnoozeStore::open(dir.path()).entries_for("work").len(), 1);
    }

    #[test]
    fn remove_rename_and_drop_running() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnoozeStore::open(dir.path());
        let mut other = entry(&[], None);
        other.claude_session_id = "bbbbbbbb-1111".into();
        let mut elsewhere = entry(&[], None);
        elsewhere.session = "other".into();
        store
            .add(vec![entry(&[], None), other.clone(), elsewhere])
            .unwrap();

        store.rename_session("work", "job").unwrap();
        assert_eq!(store.entries_for("job").len(), 2);
        assert!(store.entries_for("work").is_empty());

        let running: HashSet<String> = [ID.to_string()].into();
        assert_eq!(store.snoozed_count("job", &running), 1);
        store.mark_stopping(&[SnoozeEntry {
            session: "job".into(),
            ..entry(&[], None)
        }]);
        store.drop_running("job", &running).unwrap();
        assert_eq!(store.entries_for("job").len(), 2, "being stopped: kept");
        store.clear_stopping(&[SnoozeEntry {
            session: "job".into(),
            ..entry(&[], None)
        }]);
        store.drop_running("job", &running).unwrap();
        assert_eq!(
            store.entries_for("job"),
            vec![SnoozeEntry {
                session: "job".into(),
                ..other.clone()
            }]
        );

        store
            .remove_for("job", &["bbbbbbbb-1111".to_string()])
            .unwrap();
        assert!(store.entries_for("job").is_empty());
        assert_eq!(store.entries_for("other").len(), 1);
        store.remove_session("other").unwrap();
        assert!(SnoozeStore::open(dir.path())
            .entries_for("other")
            .is_empty());
    }

    // ---- planning ------------------------------------------------------------------------

    fn claude(pid: u32, id: Option<&str>, status: Option<&str>) -> ClaudeSnap {
        ClaudeSnap {
            pid,
            session_id: id.map(str::to_string),
            cwd: "/w".into(),
            args: strings(&["--resume", ID]),
            kind: Some("interactive".into()),
            name: None,
            source: ClaudeIdSource::SessionsFile,
            status: status.map(str::to_string),
            config_dir: None,
        }
    }

    fn pane(id: &str, index: i64, pid: u32, command: &str, claude: Option<ClaudeSnap>) -> PaneSnap {
        PaneSnap {
            id: id.into(),
            index,
            active: true,
            pid,
            cwd: "/w".into(),
            current_command: command.into(),
            foreground_cmdline: None,
            claude,
        }
    }

    fn snapshot(panes: Vec<PaneSnap>) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            taken_at: 1,
            server: ServerIdentity {
                pid: 1,
                start_time: 1,
                socket_path: "/s".into(),
                version: "3".into(),
            },
            sessions: vec![SessionSnap {
                id: "$1".into(),
                name: "work".into(),
                created: 1,
                group: None,
                windows: vec![WindowSnap {
                    id: "@1".into(),
                    index: 0,
                    name: "w".into(),
                    automatic_rename: false,
                    active: true,
                    zoomed: false,
                    width: 80,
                    height: 24,
                    layout: String::new(),
                    panes,
                }],
            }],
        }
    }

    fn login_shell(_pid: u32) -> Option<Vec<String>> {
        Some(strings(&["-bash"]))
    }

    #[test]
    fn only_interactive_shells_may_be_a_panes_root() {
        for argv in [
            vec!["-bash"],
            vec!["-/bin/bash"],
            vec!["/bin/bash"],
            vec!["bash", "-l"],
            vec!["bash", "--login"],
            vec!["zsh", "-i"],
            vec!["sh"],
        ] {
            assert!(is_interactive_shell(&strings(&argv)), "{argv:?}");
        }
        for argv in [
            vec!["bash", "-c", "claude --resume x"],
            vec!["bash", "-lc", "claude"],
            vec!["bash", "/x/claude-persist", "uuid:1"],
            vec!["sh", "script.sh"],
            vec!["node"],
            vec!["node", "-i"],
            vec!["claude"],
            vec![],
        ] {
            assert!(!is_interactive_shell(&strings(&argv)), "{argv:?}");
        }
    }

    #[test]
    fn a_wrapper_root_or_an_unreadable_root_refuses_the_snooze() {
        let snap = snapshot(vec![pane(
            "%1",
            0,
            100,
            "claude",
            Some(claude(101, Some(ID), Some("idle"))),
        )]);
        let wrapper = |_: u32| Some(strings(&["bash", "-c", "claude --resume x"]));
        let refusal = plan_snooze(&snap, "work", 1, &wrapper).unwrap_err();
        assert_eq!(refusal.code(), "not_in_shell");
        assert_eq!(
            refusal.message(),
            "Claude in work was not started from an interactive shell; stopping it would close the pane"
        );
        let unreadable = |_: u32| None;
        assert_eq!(
            plan_snooze(&snap, "work", 1, &unreadable)
                .unwrap_err()
                .code(),
            "not_in_shell"
        );
    }

    #[test]
    fn snooze_plans_the_claude_panes() {
        let snap = snapshot(vec![
            pane(
                "%1",
                0,
                100,
                "claude",
                Some(claude(101, Some(ID), Some("idle"))),
            ),
            pane("%2", 1, 200, "bash", None),
        ]);
        let plan = plan_snooze(&snap, "work", 55, &login_shell).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!((plan[0].pane_pid, plan[0].claude_pid), (100, 101));
        assert_eq!(plan[0].entry.pane_id, "%1");
        assert_eq!(plan[0].entry.claude_session_id, ID);
        assert_eq!(plan[0].entry.snoozed_at, 55);
    }

    #[test]
    fn snooze_refusals() {
        let busy = snapshot(vec![
            pane(
                "%1",
                0,
                100,
                "claude",
                Some(claude(101, Some(ID), Some("idle"))),
            ),
            pane(
                "%2",
                1,
                200,
                "claude",
                Some(claude(201, Some(ID), Some("busy"))),
            ),
        ]);
        let refusal = plan_snooze(&busy, "work", 1, &login_shell).unwrap_err();
        assert_eq!(refusal.code(), "busy");
        assert_eq!(
            refusal.message(),
            "Claude is replying in work; try again when it is idle"
        );

        let no_id = snapshot(vec![pane(
            "%1",
            0,
            100,
            "claude",
            Some(claude(101, None, None)),
        )]);
        assert_eq!(
            plan_snooze(&no_id, "work", 1, &login_shell)
                .unwrap_err()
                .code(),
            "no_session_id"
        );

        let own = snapshot(vec![pane(
            "%1",
            0,
            101,
            "claude",
            Some(claude(101, Some(ID), None)),
        )]);
        assert_eq!(
            plan_snooze(&own, "work", 1, &login_shell)
                .unwrap_err()
                .code(),
            "not_in_shell"
        );

        let none = snapshot(vec![pane("%1", 0, 100, "bash", None)]);
        let refusal = plan_snooze(&none, "work", 1, &login_shell).unwrap_err();
        assert_eq!(refusal.code(), "no_claude");
        assert_eq!(refusal.message(), "Claude is not running in work");

        let refusal = plan_snooze(&none, "ghost", 1, &login_shell).unwrap_err();
        assert!(refusal.is_not_found());
        assert_eq!(refusal.code(), "not_found");
    }

    #[test]
    fn wake_finds_the_pane_by_id_then_by_position_and_needs_a_shell() {
        let snap = snapshot(vec![pane("%1", 0, 100, "zsh", None)]);
        let entries = vec![entry(&[], None)];
        let plan = plan_wake(&snap, "work", &entries).unwrap();
        assert_eq!(
            plan,
            vec![("%1".to_string(), format!("cd /w && claude --resume {ID}"))]
        );

        // The pane was recreated with another id at the same position.
        let moved = snapshot(vec![pane("%9", 0, 100, "-bash", None)]);
        assert_eq!(plan_wake(&moved, "work", &entries).unwrap()[0].0, "%9");

        let gone = snapshot(vec![pane("%9", 3, 100, "bash", None)]);
        assert_eq!(
            plan_wake(&gone, "work", &entries).unwrap_err().code(),
            "pane_missing"
        );

        let busy = snapshot(vec![pane("%1", 0, 100, "sleep", None)]);
        let refusal = plan_wake(&busy, "work", &entries).unwrap_err();
        assert_eq!(refusal.code(), "pane_busy");
        assert_eq!(refusal.message(), "sleep is running in the pane");

        assert!(plan_wake(&snap, "ghost", &entries)
            .unwrap_err()
            .is_not_found());
    }

    // ---- stopping ------------------------------------------------------------------------

    /// A process this test started whose argv[0] is `claude`, so it counts as Claude.
    fn spawn_fake_claude() -> std::process::Child {
        Command::new("sleep")
            .arg0("claude")
            .arg("60")
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    }

    struct Reaper(std::process::Child);

    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn stop_process_ends_only_the_matching_process() {
        let mut child = Reaper(spawn_fake_claude());
        let pid = child.0.id();
        let proc = Path::new("/proc");
        // The child may not have finished exec yet when spawn returns on a busy machine.
        let started = Instant::now();
        let ticks = loop {
            if let Some(ticks) = claude_start_ticks(proc, pid) {
                break ticks;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the fake counts as claude"
            );
            sleep(Duration::from_millis(10));
        };

        // A wrong start time (pid reuse) must signal nothing.
        let timings = StopTimings {
            term_grace: Duration::from_secs(5),
            kill_grace: Duration::from_secs(2),
            poll: Duration::from_millis(20),
        };
        assert_eq!(
            stop_process(proc, pid, ticks + 1, timings),
            StopOutcome::Skipped
        );
        assert!(child.0.try_wait().unwrap().is_none(), "still running");

        // The reaper thread below turns the dead process from a zombie into nothing.
        let waiter = std::thread::spawn({
            let proc = proc.to_path_buf();
            move || stop_process(&proc, pid, ticks, timings)
        });
        // The child is ours: reap it once it has been signalled.
        let status = child.0.wait().unwrap();
        assert!(!status.success());
        let outcome = waiter.join().unwrap();
        assert!(
            matches!(outcome, StopOutcome::Stopped { freed_bytes } if freed_bytes > 0),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_process_that_is_not_claude_is_never_signalled() {
        let mut child = Reaper(
            Command::new("sleep")
                .arg("60")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        assert_eq!(claude_start_ticks(Path::new("/proc"), pid), None);
        let ticks = stat_of(Path::new("/proc"), pid).unwrap().0.start_ticks;
        let timings = StopTimings {
            term_grace: Duration::from_millis(200),
            kill_grace: Duration::from_millis(200),
            poll: Duration::from_millis(20),
        };
        assert_eq!(
            stop_process(Path::new("/proc"), pid, ticks, timings),
            StopOutcome::Skipped
        );
        assert!(child.0.try_wait().unwrap().is_none());
    }
}
