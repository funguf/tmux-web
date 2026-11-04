//! Helpers shared by the server integration tests: an isolated tmux server, an in-process
//! tmux-web server on an ephemeral port, a minimal HTTP client and WebSocket readers.
//!
//! Safety rules of the project (§3): every tmux command here carries `-L <socket>` with a
//! random `tmw-srv-*` name, the socket's server is killed when its guard drops (also when a
//! test fails), everything binds to 127.0.0.1, and the Claude config dir is a temp dir.

#![allow(dead_code)]

use futures_util::StreamExt;
use serde_json::Value;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tmux_web::config::{Cli, Config};
use tmux_web::util;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

// ---- an isolated tmux server --------------------------------------------------------------

/// A tmux server on its own socket (`-L tmw-srv-<random>`), killed when dropped.
pub struct Tmux {
    socket: String,
    wrapper_dir: tempfile::TempDir,
}

impl Tmux {
    pub fn new() -> Tmux {
        let socket = format!("tmw-srv-{}", util::random_hex(4));
        let wrapper_dir = tempfile::tempdir().expect("temp dir");
        // tmux-web is pointed at this script instead of `tmux`: whatever it starts reads no
        // configuration file and runs `/bin/sh` in its panes.
        let wrapper = wrapper_dir.path().join("tmux");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\nexport SHELL=/bin/sh\nexec tmux -f /dev/null \"$@\"\n",
        )
        .expect("write wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        eprintln!("isolated tmux socket: -L {socket}");
        Tmux {
            socket,
            wrapper_dir,
        }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// The `--tmux-bin` for tmux-web (see [`Tmux::new`]).
    pub fn bin(&self) -> PathBuf {
        self.wrapper_dir.path().join("tmux")
    }

    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .args(["-f", "/dev/null", "-L", &self.socket])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null());
        command
    }

    /// Runs `tmux -L <socket> <args>`.
    pub fn tmux(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run tmux")
    }

    /// Runs a tmux command that must succeed; returns its trimmed stdout.
    pub fn run(&self, args: &[&str]) -> String {
        let output = self.tmux(args);
        assert!(
            output.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Starts the server (if needed) with a detached session running `/bin/sh`.
    pub fn new_session(&self, name: &str) {
        self.run(&["new-session", "-d", "-s", name]);
    }

    /// A detached session whose only process is `command` (not a shell).
    pub fn new_session_running(&self, name: &str, command: &str) {
        self.run(&["new-session", "-d", "-s", name, command]);
    }

    /// Expands a tmux format in the context of a session's current window and pane.
    pub fn format(&self, session: &str, format: &str) -> String {
        // The trailing colon makes `=session` a session part of a window/pane target.
        self.run(&[
            "display-message",
            "-p",
            "-t",
            &format!("={session}:"),
            format,
        ])
    }

    pub fn has_session(&self, name: &str) -> bool {
        self.tmux(&["has-session", "-t", &format!("={name}")])
            .status
            .success()
    }

    pub fn session_names(&self) -> Vec<String> {
        let output = self.tmux(&["list-sessions", "-F", "#{session_name}"]);
        let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        names.sort();
        names
    }

    pub fn server_is_running(&self) -> bool {
        self.tmux(&["list-sessions"]).status.success()
    }

    pub fn kill_server(&self) {
        let _ = self
            .command()
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// True once clients can no longer reach a server. Right after `kill-server` tmux briefly
    /// answers "server exited unexpectedly", which is not yet "gone".
    fn server_is_gone(&self) -> bool {
        match self.command().arg("list-sessions").output() {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
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
            std::thread::sleep(Duration::from_millis(25));
        }
        self.server_is_gone()
    }

    fn socket_file(&self) -> PathBuf {
        let base = std::env::var_os("TMUX_TMPDIR")
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        base.join(format!("tmux-{}", nix::unistd::getuid()))
            .join(&self.socket)
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        self.kill_server();
        if !self.wait_until_gone(Duration::from_secs(5)) {
            eprintln!("warning: tmux server on -L {} did not exit", self.socket);
            return;
        }
        // kill-server leaves its dead socket file behind; remove only that.
        let file = self.socket_file();
        if std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_socket()) {
            let _ = std::fs::remove_file(&file);
        }
    }
}

// ---- processes ---------------------------------------------------------------------------------

pub fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Pids of `tmux attach-session -t =<session>` clients on `socket` (found via /proc).
pub fn attach_client_pids(socket: &str, session: &str) -> Vec<u32> {
    let target = format!("={session}");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let args: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
            let has = |wanted: &str| args.contains(&wanted.as_bytes());
            has("attach-session") && has(&target) && has(socket)
        })
        .collect()
}

// ---- polling -----------------------------------------------------------------------------------

/// Calls `check` every 50 ms until it returns `Some`, or panics after `within`.
pub async fn eventually<T, F, Fut>(what: &str, within: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + within;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {within:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- an in-process tmux-web ----------------------------------------------------------------------

/// State and Claude config directories for one server (and its restarts).
pub struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        Sandbox {
            root: tempfile::tempdir().expect("temp dir"),
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.path().join("state")
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.root.path().join("claude")
    }
}

pub struct TestServer {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl TestServer {
    /// The arguments every test server gets before its own: fast polling.
    pub fn base_args(tmux: &Tmux, sandbox: &Sandbox) -> Vec<String> {
        [
            "tmux-web".to_string(),
            "--tmux-bin".into(),
            tmux.bin().display().to_string(),
            "-L".into(),
            tmux.socket().to_string(),
            "--state-dir".into(),
            sandbox.state_dir().display().to_string(),
            "--claude-config-dir".into(),
            sandbox.claude_dir().display().to_string(),
        ]
        .into()
    }

    /// Serves on a fresh `127.0.0.1:0` listener. `extra` are `serve` options.
    pub async fn start(tmux: &Tmux, sandbox: &Sandbox, extra: &[&str]) -> TestServer {
        Self::start_with_poll(tmux, sandbox, 50, extra).await
    }

    /// Like [`TestServer::start`] with a poll every `poll_interval_ms`.
    pub async fn start_with_poll(
        tmux: &Tmux,
        sandbox: &Sandbox,
        poll_interval_ms: u64,
        extra: &[&str],
    ) -> TestServer {
        let mut args = Self::base_args(tmux, sandbox);
        args.extend(["serve", "--listen", "127.0.0.1:0", "--poll-interval-ms"].map(String::from));
        args.push(poll_interval_ms.to_string());
        args.extend(extra.iter().map(|arg| (*arg).to_string()));
        let cli = Cli::try_parse_args(args).expect("arguments");
        let config = Config::from_cli(&cli).expect("configuration");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(tmux_web::server::run(config, listener, async move {
            let _ = stopped.await;
        }));
        TestServer {
            addr,
            stop: Some(stop),
            task: Some(task),
        }
    }

    /// Asks the server to shut down and waits for `run` to return; the time that took.
    pub async fn stop(&mut self) -> Duration {
        let started = Instant::now();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let result = tokio::time::timeout(Duration::from_secs(15), task)
                .await
                .expect("the server did not stop within 15 s")
                .expect("server task");
            result.expect("server returned an error");
        }
        started.elapsed()
    }

    pub async fn http(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Response {
        http(self.addr, method, path, headers, body).await
    }

    pub async fn get(&self, path: &str) -> Response {
        self.http("GET", path, &[], b"").await
    }

    pub async fn get_json(&self, path: &str) -> Value {
        let response = self.get(path).await;
        assert_eq!(response.status, 200, "GET {path}: {}", response.text());
        response.json()
    }

    /// POST/PUT with a JSON body and the CSRF header.
    pub async fn send_json(&self, method: &str, path: &str, body: &Value) -> Response {
        self.http(
            method,
            path,
            &[("X-Tmux-Web", "1"), ("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        )
        .await
    }

    pub async fn post_json(&self, path: &str, body: &Value) -> Response {
        self.send_json("POST", path, body).await
    }

    pub async fn ws(&self, path: &str) -> Ws {
        ws_connect(self.addr, path, &[]).await.expect("websocket")
    }

    /// Polls `/api/status` until `check` accepts it; returns that status.
    pub async fn wait_status(&self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        eventually(what, Duration::from_secs(10), || async {
            let status = self.get_json("/api/status").await;
            check(&status).then_some(status)
        })
        .await
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ---- HTTP -----------------------------------------------------------------------------------------

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|err| panic!("not JSON ({err}): {}", self.text()))
    }

    pub fn error_code(&self) -> String {
        let body = self.json();
        assert!(body["message"].is_string(), "{body}");
        body["error"].as_str().expect("error code").to_string()
    }
}

/// One HTTP/1.1 request on a fresh connection (`Connection: close`).
///
/// The body is written while the response is read, so a server that answers before it has
/// read everything (an oversize upload, say) cannot lose the answer to a reset.
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Response {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        head.push_str(&format!("Host: {addr}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let (mut read_half, mut write_half) = stream.split();
    let write = async {
        // A server may hang up before reading the whole body; that is fine here.
        let _ = write_half.write_all(head.as_bytes()).await;
        let _ = write_half.write_all(body).await;
    };
    let read = async {
        let mut raw = Vec::new();
        let _ = read_half.read_to_end(&mut raw).await;
        raw
    };
    let (_, raw) = tokio::join!(write, read);
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Response {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "no HTTP response head in {:?}",
                String::from_utf8_lossy(raw)
            )
        });
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    let body = &raw[split + 4..];
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.contains("chunked")
    });
    Response {
        status,
        body: if chunked {
            dechunk(body)
        } else {
            body.to_vec()
        },
        headers,
    }
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(end) = body.windows(2).position(|window| window == b"\r\n") else {
            return out;
        };
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&body[..end]).trim(), 16).unwrap_or(0);
        if size == 0 {
            return out;
        }
        let start = end + 2;
        out.extend_from_slice(&body[start..start + size]);
        body = &body[(start + size + 2).min(body.len())..];
    }
}

// ---- WebSockets -------------------------------------------------------------------------------------

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn ws_connect(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{addr}{path}")
        .into_client_request()
        .expect("request");
    for (name, value) in headers {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    connect_async(request).await.map(|(socket, _)| socket)
}

/// What a WebSocket delivered.
#[derive(Debug)]
pub enum Frame {
    Text(Value),
    Binary(Vec<u8>),
    /// A close frame: code and reason.
    Close(Option<(u16, String)>),
    /// The stream ended without a close frame.
    Ended,
}

/// The next data or close frame (pings and pongs are skipped); panics after `within`.
pub async fn next_frame(ws: &mut Ws, within: Duration) -> Frame {
    loop {
        let message = tokio::time::timeout(within, ws.next())
            .await
            .unwrap_or_else(|_| panic!("no WebSocket frame within {within:?}"));
        match message {
            None => return Frame::Ended,
            Some(Err(err)) => panic!("WebSocket error: {err}"),
            Some(Ok(Message::Text(text))) => {
                return Frame::Text(
                    serde_json::from_str(text.as_str())
                        .unwrap_or_else(|err| panic!("text frame is not JSON ({err}): {text}")),
                )
            }
            Some(Ok(Message::Binary(bytes))) => return Frame::Binary(bytes.to_vec()),
            Some(Ok(Message::Close(frame))) => {
                return Frame::Close(frame.map(|f| (u16::from(f.code), f.reason.to_string())))
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
        }
    }
}

/// The next JSON text frame whose `type` is `kind`; other text frames are skipped.
pub async fn next_event(ws: &mut Ws, kind: &str, within: Duration) -> Value {
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match next_frame(ws, remaining.max(Duration::from_millis(1))).await {
            Frame::Text(value) if value["type"] == kind => return value,
            Frame::Text(_) | Frame::Binary(_) => {}
            other => panic!("expected a {kind:?} event, got {other:?}"),
        }
    }
}

/// Collects binary output until it contains `needle`; panics (with what it saw) after `within`.
pub async fn read_output_until(ws: &mut Ws, needle: &str, within: Duration) -> String {
    let deadline = Instant::now() + within;
    let mut seen = Vec::new();
    loop {
        let text = String::from_utf8_lossy(&seen).into_owned();
        if text.contains(needle) {
            return text;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {needle:?} in terminal output {text:?}"
        );
        match next_frame(ws, remaining).await {
            Frame::Binary(bytes) => seen.extend_from_slice(&bytes),
            Frame::Text(value) => panic!("unexpected text frame {value} while waiting for {needle:?} (output so far {text:?})"),
            other => panic!("{other:?} while waiting for {needle:?} (output so far {text:?})"),
        }
    }
}

/// How an attach session ended.
#[derive(Debug)]
pub struct Exit {
    pub reason: Option<String>,
    pub message: Option<String>,
    pub close_code: Option<u16>,
    pub output: String,
}

/// Reads until the connection is closed: output, the exit frame, the close frame.
pub async fn read_until_closed(ws: &mut Ws, within: Duration) -> Exit {
    let deadline = Instant::now() + within;
    let mut exit = Exit {
        reason: None,
        message: None,
        close_code: None,
        output: String::new(),
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the connection was not closed within {within:?}: {exit:?}"
        );
        match next_frame(ws, remaining).await {
            Frame::Binary(bytes) => exit.output.push_str(&String::from_utf8_lossy(&bytes)),
            Frame::Text(value) => {
                assert_eq!(value["type"], "exit", "unexpected frame {value}");
                assert!(exit.reason.is_none(), "two exit frames");
                exit.reason = value["reason"].as_str().map(str::to_string);
                exit.message = value["message"].as_str().map(str::to_string);
            }
            Frame::Close(frame) => {
                exit.close_code = frame.map(|(code, _)| code);
                return exit;
            }
            Frame::Ended => return exit,
        }
    }
}
