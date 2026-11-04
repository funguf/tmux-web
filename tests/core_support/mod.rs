//! Helpers shared by the core integration tests: an isolated tmux server and polling.
//!
//! Every tmux command goes through a wrapper script that refuses to run unless the socket is a
//! `tmw-core-*` one, reads only its own tiny configuration file, starts `sh` in every pane and runs from the
//! test's private home directory, so that nothing here can reach the operator's tmux and the
//! panes behave the same on every machine.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tmux_web::claude::ClaudeDetector;
use tmux_web::snapshot::{self, Snapshot};
use tmux_web::tmux::{RealTmux, SocketSel, TmuxConfig, TmuxError, TmuxRunner};
use tmux_web::util;

pub const PATIENCE: Duration = Duration::from_secs(10);

const WRAPPER: &str = r#"#!/bin/sh
# tmux for the core tests: only ever the test socket, no user configuration, sh in the panes.
case "$1:$2" in
  -L:tmw-core-*) ;;
  *) echo "test tmux wrapper: refusing to run without -L tmw-core-*: $*" >&2; exit 97 ;;
esac
unset TMUX TMUX_PANE
SHELL=/bin/sh
HOME='@HOME@'
export SHELL HOME
# The server starts here, so anything it runs or creates relative to its directory stays in
# the test's own tree.
cd "$HOME" || exit 98
exec tmux -f '@CONF@' "$@"
"#;

/// The only tmux configuration the test servers read. Panes run a plain, non-login `sh`:
/// a login shell would run this machine's /etc/profile scripts, whose short-lived programs
/// (`locale`, say) show up as the pane's command and rename automatically-named windows.
const TMUX_CONF: &str =
    "set-option -g default-shell /bin/sh\nset-option -g default-command \"exec /bin/sh\"\n";

/// An isolated tmux server on its own socket, killed when this is dropped.
pub struct TestTmux {
    pub socket: String,
    root: tempfile::TempDir,
    pub runner: RealTmux,
}

impl TestTmux {
    pub fn new() -> Self {
        let socket = format!("tmw-core-{}", util::random_hex(6));
        let root = tempfile::tempdir().unwrap();
        // Everything below is canonical, so paths compare equal to what tmux reports.
        let base = fs::canonicalize(root.path()).unwrap();
        for sub in ["bin", "home"] {
            fs::create_dir(base.join(sub)).unwrap();
        }
        let wrapper = base.join("bin").join("tmux");
        let conf = base.join("tmux.conf");
        fs::write(&conf, TMUX_CONF).unwrap();
        let script = WRAPPER
            .replace("@HOME@", &base.join("home").to_string_lossy())
            .replace("@CONF@", &conf.to_string_lossy());
        install_executable(&wrapper, &script, &["-L", &socket, "-V"], &[]);
        let runner = RealTmux::new(TmuxConfig {
            bin: wrapper,
            socket: SocketSel::Name(socket.clone()),
        });
        Self {
            socket,
            root,
            runner,
        }
    }

    /// The private home directory of the tmux server and its panes (and its working directory).
    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    /// The canonical root directory of this test.
    pub fn root(&self) -> PathBuf {
        fs::canonicalize(self.root.path()).unwrap()
    }

    /// A new (or existing) directory below the root; the name may contain anything but `/`.
    pub fn dir(&self, name: &str) -> PathBuf {
        let path = self.root().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    pub fn detector(&self, config_dir: &Path) -> ClaudeDetector {
        ClaudeDetector::new(config_dir.to_path_buf())
    }

    /// Runs a tmux command exactly the way the code under test does; panics on failure.
    pub fn tmux(&self, args: &[&str]) -> String {
        self.try_tmux(args)
            .unwrap_or_else(|err| panic!("tmux {args:?} failed: {err}"))
    }

    pub fn try_tmux(&self, args: &[&str]) -> Result<String, TmuxError> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        self.runner.run(&args)
    }

    /// `new-session -d -s <name> -x 200 -y 50 -c <cwd>`.
    pub fn new_session(&self, name: &str, cwd: &Path) {
        self.tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "200",
            "-y",
            "50",
            "-c",
            &cwd.to_string_lossy(),
        ]);
    }

    pub fn session_names(&self) -> Vec<String> {
        let out = self.tmux(&["list-sessions", "-F", "#{session_name}"]);
        let mut names: Vec<String> = out.lines().map(str::to_string).collect();
        names.sort();
        names
    }

    pub fn is_running(&self) -> bool {
        self.try_tmux(&["list-sessions"]).is_ok()
    }

    /// Kills the server and returns at once; for a moment the old server still answers
    /// "server exited unexpectedly".
    pub fn kill_server_now(&self) {
        let _ = self.own_tmux(&["kill-server"]).output();
    }

    /// `tmux -L <socket> …` without any of the code under test, for cleanup.
    fn own_tmux(&self, args: &[&str]) -> Command {
        assert!(self.socket.starts_with("tmw-core-"), "not a test socket");
        let mut cmd = Command::new("tmux");
        cmd.arg("-L")
            .arg(&self.socket)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null());
        cmd
    }

    fn server_is_gone(&self) -> bool {
        match self.own_tmux(&["list-sessions"]).output() {
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                stderr.contains("no server running") || stderr.contains("error connecting to")
            }
            Ok(_) => false,
            Err(_) => true,
        }
    }

    pub fn wait_until_gone(&self, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        while Instant::now() < deadline {
            if self.server_is_gone() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.server_is_gone()
    }

    /// Takes snapshots until `ready` accepts one (processes need a moment to start and tmux a
    /// moment to report their directories).
    pub fn snapshot_when(
        &self,
        detector: &ClaudeDetector,
        what: &str,
        ready: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        let mut last = String::from("no snapshot yet");
        let found = wait_until(what, PATIENCE, || {
            match snapshot::take_snapshot(&self.runner, detector) {
                Ok(snap) if ready(&snap) => Some(snap),
                Ok(snap) => {
                    last = format!("{:#?}", snap.sessions);
                    None
                }
                Err(err) => {
                    last = err.to_string();
                    None
                }
            }
        });
        match found {
            Some(snap) => snap,
            None => panic!("timed out waiting for {what}; last attempt: {last}"),
        }
    }

    /// A snapshot once every pane reports a directory.
    pub fn snapshot(&self, detector: &ClaudeDetector) -> Snapshot {
        self.snapshot_when(detector, "a snapshot with all pane directories", |snap| {
            snap.sessions
                .iter()
                .flat_map(|s| &s.windows)
                .flat_map(|w| &w.panes)
                .all(|p| !p.cwd.is_empty())
        })
    }
}

impl Drop for TestTmux {
    fn drop(&mut self) {
        self.kill_server_now();
        if !self.wait_until_gone(PATIENCE) {
            eprintln!("warning: tmux server on -L {} did not exit", self.socket);
            return;
        }
        // kill-server leaves its dead socket file behind; remove only that.
        let base = std::env::var_os("TMUX_TMPDIR")
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let file = base
            .join(format!("tmux-{}", nix::unistd::getuid()))
            .join(&self.socket);
        if fs::symlink_metadata(&file)
            .is_ok_and(|meta| std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()))
        {
            let _ = fs::remove_file(&file);
        }
    }
}

/// Writes an executable script and makes sure it can be run before anything relies on it.
///
/// The tests run on parallel threads that all spawn processes. A thread that forks while
/// another still has the new script open for writing hands the child a copy of that file
/// descriptor until the child's exec, and running the script in that moment fails with
/// ETXTBSY ("text file busy"). Once one run has succeeded nothing holds the file open for
/// writing any more, and the script stays runnable.
fn install_executable(path: &Path, content: &str, probe_args: &[&str], probe_env: &[(&str, &str)]) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let ran = Command::new(path)
            .args(probe_args)
            .envs(probe_env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match ran {
            Ok(_) => return,
            Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("cannot run {}: {err}", path.display()),
        }
    }
}

/// Calls `check` until it returns Some, for at most `patience`.
pub fn wait_until<T>(
    what: &str,
    patience: Duration,
    mut check: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + patience;
    loop {
        if let Some(found) = check() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            eprintln!("timed out after {patience:?} waiting for {what}");
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

// ---- layout strings -----------------------------------------------------------------------

/// A tmux layout string without its checksum and without the pane ids in its leaves:
/// `b25f,159x40,0,0[159x20,0,0,3,159x19,0,21,5]` becomes `159x40,0,0[159x20,0,0,159x19,0,21]`.
/// Two windows with the same geometry normalise equally whatever their pane numbers are.
pub fn normalize_layout(layout: &str) -> String {
    let body = layout.split_once(',').map_or(layout, |(_, rest)| rest);
    let bytes = body.as_bytes();
    let mut out = String::new();
    let mut at = 0;
    layout_cell(bytes, &mut at, &mut out);
    assert_eq!(at, bytes.len(), "trailing text in layout {layout:?}");
    out
}

fn layout_number(bytes: &[u8], at: &mut usize, out: &mut String) {
    let start = *at;
    while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    assert!(
        *at > start,
        "number expected at {at} in {:?}",
        String::from_utf8_lossy(bytes)
    );
    out.push_str(std::str::from_utf8(&bytes[start..*at]).unwrap());
}

fn layout_separator(bytes: &[u8], at: &mut usize, expected: u8, out: &mut String) {
    assert_eq!(bytes.get(*at), Some(&expected), "layout syntax");
    *at += 1;
    out.push(char::from(expected));
}

fn layout_cell(bytes: &[u8], at: &mut usize, out: &mut String) {
    layout_number(bytes, at, out);
    layout_separator(bytes, at, b'x', out);
    layout_number(bytes, at, out);
    layout_separator(bytes, at, b',', out);
    layout_number(bytes, at, out);
    layout_separator(bytes, at, b',', out);
    layout_number(bytes, at, out);
    match bytes.get(*at) {
        // A leaf: `,<pane id>`.
        Some(b',') => {
            *at += 1;
            let mut ignored = String::new();
            layout_number(bytes, at, &mut ignored);
        }
        Some(&open @ (b'{' | b'[')) => {
            let close = if open == b'{' { b'}' } else { b']' };
            *at += 1;
            out.push(char::from(open));
            loop {
                layout_cell(bytes, at, out);
                match bytes.get(*at) {
                    Some(b',') => {
                        *at += 1;
                        out.push(',');
                    }
                    Some(&found) if found == close => {
                        *at += 1;
                        out.push(char::from(close));
                        break;
                    }
                    other => panic!("layout syntax: unexpected {other:?}"),
                }
            }
        }
        _ => {}
    }
}
