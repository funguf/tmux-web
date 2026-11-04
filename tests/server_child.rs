//! Tests that run the real `tmux-web` binary as a child process: what it passes on to the tmux
//! it starts, and what it writes to its log.
//!
//! The tmux here lives on an isolated `-L tmw-srv-<random>` socket (see `server_common`), and the
//! child listens on a port that was free a moment ago.

mod server_common;

use server_common::*;
use std::io::Read;
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const TEST_TOKEN: &str = "child-test-token";

/// The real binary, with every `TMUX_WEB_*` variable of this process removed (the operator's own
/// settings must not configure the child), pointed at the isolated tmux and state directory.
fn child_command(tmux: &Tmux, sandbox: &Sandbox, listen: &str, extra: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tmux-web"));
    for (name, _) in std::env::vars_os() {
        if name.as_bytes().starts_with(b"TMUX_WEB_") {
            command.env_remove(name);
        }
    }
    command
        .args([
            "--tmux-bin",
            &tmux.bin().display().to_string(),
            "-L",
            tmux.socket(),
            "--state-dir",
            &sandbox.state_dir().display().to_string(),
            "--claude-config-dir",
            &sandbox.claude_dir().display().to_string(),
            "serve",
            "--listen",
            listen,
        ])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

/// A port that is free now.
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Kills the child when dropped, also when a test fails.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn wait_until_listening(port: u16) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    eventually(
        "the child to listen",
        Duration::from_secs(15),
        || async move { TcpStream::connect(addr).ok().map(|_| ()) },
    )
    .await;
}

/// The variable names (never the values) of a `NAME=value` listing that start with `TMUX_WEB_`.
fn own_names(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter(|line| line.starts_with("TMUX_WEB_"))
        .map(|line| line.split('=').next().unwrap_or("").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tmux_web_settings_do_not_reach_the_tmux_server_or_its_panes() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let mut command = child_command(&tmux, &sandbox, &listen, &[]);
    command
        .env("TMUX_WEB_TOKEN", TEST_TOKEN)
        .env("TMUX_WEB_CANARY", "1")
        .stderr(Stdio::null());
    let _child = ChildGuard(command.spawn().expect("start tmux-web"));
    wait_until_listening(port).await;

    // The page load starts tmux through tmux-web.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let started = http(
        addr,
        "POST",
        "/api/server/start",
        &[
            ("X-Tmux-Web", "1"),
            ("Authorization", &format!("Bearer {TEST_TOKEN}")),
        ],
        b"",
    )
    .await;
    assert_eq!(started.status, 200, "{}", started.text());
    assert_eq!(started.json()["started"], true);

    // The server's global environment.
    let global = tmux.run(&["show-environment", "-g"]);
    assert_eq!(
        own_names(&global),
        Vec::<String>::new(),
        "tmux global environment"
    );

    // The tmux server process itself.
    let pid = tmux.run(&["display-message", "-p", "#{pid}"]);
    let environ = std::fs::read(format!("/proc/{pid}/environ")).expect("read the server environ");
    let names: Vec<String> = environ
        .split(|byte| *byte == 0)
        .filter(|entry| entry.starts_with(b"TMUX_WEB_"))
        .map(|entry| {
            let end = entry.iter().position(|b| *b == b'=').unwrap_or(entry.len());
            String::from_utf8_lossy(&entry[..end]).into_owned()
        })
        .collect();
    assert_eq!(
        names,
        Vec::<String>::new(),
        "tmux server process environment"
    );

    // A program in a pane.
    let out = sandbox.state_dir().join("pane-env");
    std::fs::create_dir_all(sandbox.state_dir()).expect("state dir");
    tmux.run(&["new-window", "-d", &format!("env > {}", out.display())]);
    let listing = eventually(
        "the pane to write its environment",
        Duration::from_secs(10),
        || {
            let out = out.clone();
            async move {
                std::fs::read_to_string(out)
                    .ok()
                    .filter(|text| !text.is_empty())
            }
        },
    )
    .await;
    assert!(listing.contains("PATH="), "the pane listing looks wrong");
    assert_eq!(
        own_names(&listing),
        Vec::<String>::new(),
        "pane environment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generated_token_is_not_logged_but_its_file_is_named() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let port = free_port();
    // A non-loopback address makes tmux-web generate a token in the state directory. The
    // child serves nothing else and is killed below.
    let listen = format!("0.0.0.0:{port}");
    let mut command = child_command(&tmux, &sandbox, &listen, &[]);
    command.stderr(Stdio::piped());
    let mut child = ChildGuard(command.spawn().expect("start tmux-web"));
    wait_until_listening(port).await;

    let token_file = sandbox.state_dir().join("token");
    let token = std::fs::read_to_string(&token_file).expect("the generated token file");
    assert_eq!(token.trim().len(), 64);

    let _ = child.0.kill();
    let _ = child.0.wait();
    let mut log = String::new();
    child
        .0
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut log)
        .expect("read the log");
    assert!(
        log.contains("a new one was generated"),
        "{}",
        redacted(&log, &token)
    );
    assert!(
        log.contains(&token_file.display().to_string()),
        "the log does not name the token file"
    );
    assert!(!log.contains(token.trim()), "the token is in the log");
}

/// The log with the token blanked out, for a failure message.
fn redacted(log: &str, token: &str) -> String {
    log.replace(token.trim(), "<token>")
}
