//! PTY attach bridge between a WebSocket and `tmux attach-session` (owned by the server unit).
//!
//! Per attached terminal there is one `tmux attach-session` client running in a PTY and three
//! threads around it:
//!
//! * the **reader** moves PTY output (chunks of at most 64 KiB) into a bounded channel;
//! * the **writer** owns the PTY writer and master and applies input and resizes;
//! * the **reaper** waits for the client to die, reaps it, and only then tells the writer to
//!   stop.
//!
//! The last point is deliberate. `portable-pty`'s writer writes a newline and the terminal's
//! EOF character into the PTY when it is dropped. While the attach client is still alive tmux
//! would forward those two keystrokes to the pane (Enter, then Ctrl-D, which logs a shell out),
//! so the writer must never be dropped before the client is dead and reaped.

use crate::monitor::{blocking, shutdown_requested};
use crate::tmux::{self, TmuxConfig, TmuxError, TmuxRunner};
use crate::util;
use anyhow::{anyhow, Context};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitid, Id, WaitPidFlag};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};

pub const MIN_COLS: u16 = 2;
pub const MAX_COLS: u16 = 1000;
pub const DEFAULT_COLS: u16 = 80;
pub const MIN_ROWS: u16 = 2;
pub const MAX_ROWS: u16 = 500;
pub const DEFAULT_ROWS: u16 = 24;

/// Terminal features announced to tmux for the attach client (`tmux -T`).
pub const TERMINAL_FEATURES: &str =
    "RGB,clipboard,hyperlinks,focus,title,cstyle,usstyle,strikethrough,overline,bpaste,mouse,256";

/// Largest chunk read from the PTY at once.
const READ_CHUNK: usize = 64 * 1024;
/// PTY output chunks buffered between the reader thread and the WebSocket.
const OUTPUT_QUEUE: usize = 64;
/// Input/resize commands buffered between the WebSocket and the writer thread.
const INPUT_QUEUE: usize = 256;
/// Attach clients served at the same time; more are refused with `error`.
pub const MAX_ATTACHES: usize = 256;

const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A single WebSocket send that takes longer than this means the peer is gone.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the client's close reply after our close frame.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// How long after SIGHUP before the attach client gets SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(1);
/// How long to wait for the reaper after SIGKILL, or after PTY EOF.
const REAP_WAIT: Duration = Duration::from_secs(5);
/// Pause before the single retry of a tmux query that failed transiently.
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// Parses a `cols`/`rows` query value: unparsable or missing → `default`, otherwise clamped.
pub fn parse_dimension(raw: Option<&str>, min: u16, max: u16, default: u16) -> u16 {
    raw.and_then(|text| text.trim().parse::<f64>().ok())
        .map_or(default, |value| clamp_dimension(value, min, max))
}

/// Clamps a client supplied number into `min..=max`.
pub fn clamp_dimension(value: f64, min: u16, max: u16) -> u16 {
    if value.is_nan() {
        return min;
    }
    // The cast saturates, and the bounds are applied afterwards.
    (value as i64).clamp(i64::from(min), i64::from(max)) as u16
}

/// Arguments for the tmux binary: `<global args> -u -T <features> attach-session -t =<name>`.
pub fn attach_args(cfg: &TmuxConfig, session: &str) -> Vec<String> {
    let mut args = cfg.global_args();
    args.extend(
        ["-u", "-T", TERMINAL_FEATURES, "attach-session", "-t"]
            .into_iter()
            .map(String::from),
    );
    args.push(tmux::exact_target(session));
    args
}

// ---- process control ---------------------------------------------------------------------

/// How the attach client ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildExit {
    pub code: Option<u32>,
    pub signal: Option<String>,
}

/// Signals one attach client, guaranteeing a recycled pid is never hit: the child is only
/// reaped while the lock is held, and signals are only sent while it is not yet reaped.
pub struct ChildControl {
    pid: Pid,
    reaped: Mutex<bool>,
}

impl ChildControl {
    fn new(pid: Pid) -> Self {
        Self {
            pid,
            reaped: Mutex::new(false),
        }
    }

    /// Sends `signal` unless the child has been reaped. True when a signal was delivered.
    pub fn signal(&self, signal: Signal) -> bool {
        let reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        !*reaped && kill(self.pid, signal).is_ok()
    }

    /// Runs `wait` (which must reap the child) and marks the child reaped afterwards.
    fn reap_with<T>(&self, wait: impl FnOnce() -> T) -> T {
        let mut reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        let result = wait();
        *reaped = true;
        result
    }
}

/// Kills and reaps a freshly spawned child unless it was handed over to the reaper thread.
struct SpawnedChild(Option<Box<dyn Child + Send + Sync>>);

impl SpawnedChild {
    /// Hands the child over; it is then the new owner's job to reap it.
    fn release(mut self) -> Option<Box<dyn Child + Send + Sync>> {
        self.0.take()
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Errors mean the child is already gone.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// What the writer thread is asked to do.
pub enum WriterCmd {
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Sent by the reaper once the attach client is dead and reaped.
    Stop,
}

/// A running `tmux attach-session`.
pub struct AttachProcess {
    /// PTY output; closes (`None`) at PTY EOF.
    pub output: mpsc::Receiver<Vec<u8>>,
    input: mpsc::Sender<WriterCmd>,
    control: Arc<ChildControl>,
    exit: watch::Receiver<Option<ChildExit>>,
}

impl Drop for AttachProcess {
    /// Safety net for a bridge that is dropped without [`AttachProcess::terminate`] (a task
    /// cancelled during shutdown): at least ask the client to hang up. A no-op once reaped.
    fn drop(&mut self) {
        self.control.signal(Signal::SIGHUP);
    }
}

impl AttachProcess {
    pub fn input(&self) -> mpsc::Sender<WriterCmd> {
        self.input.clone()
    }

    pub fn control(&self) -> Arc<ChildControl> {
        Arc::clone(&self.control)
    }

    /// Waits until the reaper has reaped the child; `None` when it did not within `limit`.
    pub async fn wait_exit(&mut self, limit: Duration) -> Option<ChildExit> {
        let reaped = timeout(limit, self.exit.wait_for(|exit| exit.is_some())).await;
        match reaped {
            Ok(Ok(exit)) => exit.clone(),
            // The reaper thread vanished without reporting; nothing is left to wait for.
            Ok(Err(_)) => Some(ChildExit {
                code: None,
                signal: None,
            }),
            Err(_) => None,
        }
    }

    /// SIGHUP, then SIGKILL after a grace period; returns once the child is reaped.
    pub async fn terminate(&mut self) {
        self.control.signal(Signal::SIGHUP);
        if self.wait_exit(KILL_GRACE).await.is_some() {
            return;
        }
        self.control.signal(Signal::SIGKILL);
        if self.wait_exit(REAP_WAIT).await.is_none() {
            tracing::error!("an attach client did not exit after SIGKILL");
        }
    }
}

/// Starts `tmux attach-session` for `session` in a PTY of the given size. Blocking.
pub fn spawn_attach(
    cfg: &TmuxConfig,
    session: &str,
    cols: u16,
    rows: u16,
) -> anyhow::Result<AttachProcess> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| anyhow!("opening a pseudo-terminal: {err:#}"))?;
    let mut command = CommandBuilder::new(&cfg.bin);
    command.args(attach_args(cfg, session));
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env_remove("TMUX");
    command.env_remove("TMUX_PANE");
    // A tmux server started by this client would hand tmux-web's settings (the token) to
    // every pane.
    for name in tmux::own_setting_names() {
        command.env_remove(name);
    }
    command.cwd(attach_cwd());
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|err| anyhow!("starting {}: {err:#}", cfg.bin.display()))?;
    let child = SpawnedChild(Some(child));
    // The slave end must not stay open here, or the master would never see EOF.
    drop(pair.slave);
    let master = pair.master;

    let pid = child
        .0
        .as_ref()
        .and_then(|child| child.process_id())
        .ok_or_else(|| anyhow!("the attach client has no process id"))?;
    let pid = Pid::from_raw(
        i32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| anyhow!("unexpected process id {pid}"))?,
    );
    let control = Arc::new(ChildControl::new(pid));
    let reader = master
        .try_clone_reader()
        .map_err(|err| anyhow!("cloning the PTY reader: {err:#}"))?;
    let writer = master
        .take_writer()
        .map_err(|err| anyhow!("taking the PTY writer: {err:#}"))?;

    let (output_tx, output) = mpsc::channel(OUTPUT_QUEUE);
    let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
    let (exit_tx, exit) = watch::channel(None);

    // The reaper goes first: from here on the child is always reaped, whatever fails below.
    let reaper_control = Arc::clone(&control);
    let reaper_stop = input_tx.clone();
    let child = child
        .release()
        .ok_or_else(|| anyhow!("the attach client vanished before it could be watched"))?;
    if let Err(err) = spawn_thread("tmw-pty-reap", move || {
        reap_loop(child, &reaper_control, &exit_tx, &reaper_stop);
    }) {
        // Nobody will reap it now (the process is out of threads), but it must not linger.
        control.signal(Signal::SIGKILL);
        return Err(err);
    }
    let started = spawn_thread("tmw-pty-write", move || {
        write_loop(writer, master, input_rx)
    })
    .and_then(|()| spawn_thread("tmw-pty-read", move || read_loop(reader, &output_tx)));
    if let Err(err) = started {
        control.signal(Signal::SIGKILL);
        return Err(err);
    }
    Ok(AttachProcess {
        output,
        input: input_tx,
        control,
        exit,
    })
}

fn spawn_thread(name: &str, body: impl FnOnce() + Send + 'static) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .map(drop)
        .with_context(|| format!("starting thread {name}"))
}

/// The attach client runs in `$HOME` (or `/` when that is not a directory).
fn attach_cwd() -> std::path::PathBuf {
    let home = util::home_dir();
    if home.is_dir() {
        home
    } else {
        std::path::PathBuf::from("/")
    }
}

fn read_loop(mut reader: Box<dyn Read + Send>, output: &mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                // A closed channel means nobody listens any more.
                if output.blocking_send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn write_loop(
    mut writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    mut commands: mpsc::Receiver<WriterCmd>,
) {
    let mut broken = false;
    while let Some(command) = commands.blocking_recv() {
        match command {
            WriterCmd::Input(bytes) => {
                if !broken && (writer.write_all(&bytes).is_err() || writer.flush().is_err()) {
                    // The terminal is gone; keep draining so senders never block on us.
                    broken = true;
                }
            }
            WriterCmd::Resize { cols, rows } => {
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            WriterCmd::Stop => break,
        }
    }
    // `Stop` only comes from the reaper, after the attach client was reaped, so dropping the
    // writer (which types a newline and EOF into the PTY) can no longer reach a live client.
    drop(writer);
    drop(master);
}

fn reap_loop(
    mut child: Box<dyn Child + Send + Sync>,
    control: &ChildControl,
    exit: &watch::Sender<Option<ChildExit>>,
    writer: &mpsc::Sender<WriterCmd>,
) {
    // Wait for the child to die without reaping it: while it is a zombie its pid cannot be
    // recycled, so signals sent meanwhile cannot hit a stranger. Only an interrupted wait is
    // retried; any other outcome (exited, or ECHILD when there is nothing to wait for) ends it.
    while let Err(Errno::EINTR) = waitid(
        Id::Pid(control.pid),
        WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT,
    ) {}
    let status = control.reap_with(|| child.wait());
    let result = match status {
        Ok(status) => ChildExit {
            code: Some(status.exit_code()),
            signal: status.signal().map(str::to_string),
        },
        Err(_) => ChildExit {
            code: None,
            signal: None,
        },
    };
    // The writer may be gone already; then there is nothing to stop.
    let _ = writer.blocking_send(WriterCmd::Stop);
    let _ = exit.send(Some(result));
}

// ---- exit classification -----------------------------------------------------------------

/// Why an attach session ended (§7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Detached,
    SessionClosed,
    ServerDown,
    NoSession,
    NoServer,
    Error,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Detached => "detached",
            ExitReason::SessionClosed => "session-closed",
            ExitReason::ServerDown => "server-down",
            ExitReason::NoSession => "no-session",
            ExitReason::NoServer => "no-server",
            ExitReason::Error => "error",
        }
    }

    /// WebSocket close code sent after the exit frame.
    pub fn close_code(self) -> u16 {
        match self {
            ExitReason::Detached | ExitReason::SessionClosed | ExitReason::ServerDown => 1000,
            ExitReason::NoSession => 4404,
            ExitReason::NoServer => 4503,
            ExitReason::Error => 1011,
        }
    }

    fn message(self) -> &'static str {
        match self {
            ExitReason::Detached => "detached from the tmux session",
            ExitReason::SessionClosed => "the tmux session was closed",
            ExitReason::ServerDown => "the tmux server stopped",
            ExitReason::NoSession => "no such tmux session",
            ExitReason::NoServer => "the tmux server is not running",
            ExitReason::Error => "internal error",
        }
    }
}

/// The session an attach client was started for, identified so that a rename is survivable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub server_id: String,
    pub session_id: String,
}

/// Runs `operation`, and once more after `delay` when it failed with a command error. Right
/// after a `kill-server`, tmux briefly answers with errors that are not "no server running".
fn retry_once<T>(
    delay: Duration,
    mut operation: impl FnMut() -> Result<T, TmuxError>,
) -> Result<T, TmuxError> {
    match operation() {
        Err(TmuxError::Command(_)) => {
            std::thread::sleep(delay);
            operation()
        }
        other => other,
    }
}

/// Step 1 of the attach bridge: does the session exist? (`list_sessions` answers exactly what
/// `has_session` would and also yields the ids used to survive a rename.) Blocking.
pub fn resolve_target(
    runner: &dyn TmuxRunner,
    name: &str,
    retry_delay: Duration,
) -> Result<Target, (ExitReason, String)> {
    if name.is_empty() || name.contains(['\0', ':']) {
        // tmux cannot hold such a name, and ':' would be read as a window separator.
        return Err((
            ExitReason::NoSession,
            ExitReason::NoSession.message().into(),
        ));
    }
    match retry_once(retry_delay, || tmux::list_sessions(runner)) {
        Err(TmuxError::NoServer) => {
            Err((ExitReason::NoServer, ExitReason::NoServer.message().into()))
        }
        Err(err) => Err((ExitReason::Error, err.to_string())),
        Ok((identity, sessions)) => sessions
            .iter()
            .find(|session| session.name == name)
            .map(|session| Target {
                server_id: identity.map(|identity| identity.id()).unwrap_or_default(),
                session_id: session.id.clone(),
            })
            .ok_or_else(|| {
                (
                    ExitReason::NoSession,
                    ExitReason::NoSession.message().into(),
                )
            }),
    }
}

/// Step 5 of the attach bridge, after PTY EOF and reaping: session still there → `detached`;
/// server up but session gone → `session-closed`; no server → `server-down`. A transient
/// command error is retried once before it becomes `error`, for both questions: when the
/// session is reported missing, only a successful answer showing the server running without
/// it yields `session-closed`; a follow-up that still fails with anything but "no server" is
/// `error`, because nothing confirmed that the session is gone. A session that was renamed
/// while attached still counts as there. Blocking.
pub fn classify_exit(
    runner: &dyn TmuxRunner,
    name: &str,
    target: &Target,
    retry_delay: Duration,
) -> (ExitReason, String) {
    let reason = match retry_once(retry_delay, || tmux::has_session(runner, name)) {
        Ok(true) => ExitReason::Detached,
        Ok(false) => match retry_once(retry_delay, || tmux::list_sessions(runner)) {
            Ok((identity, sessions)) => {
                let same_server =
                    identity.is_some_and(|identity| identity.id() == target.server_id);
                if same_server
                    && sessions
                        .iter()
                        .any(|session| session.id == target.session_id)
                {
                    ExitReason::Detached
                } else {
                    ExitReason::SessionClosed
                }
            }
            Err(TmuxError::NoServer) => ExitReason::ServerDown,
            Err(err) => return (ExitReason::Error, err.to_string()),
        },
        Err(TmuxError::NoServer) => ExitReason::ServerDown,
        Err(err) => return (ExitReason::Error, err.to_string()),
    };
    (reason, reason.message().to_string())
}

// ---- registry ----------------------------------------------------------------------------

/// The attach clients that are alive, so shutdown can reach every one of them.
#[derive(Default)]
pub struct AttachRegistry {
    next: AtomicU64,
    live: Mutex<HashMap<u64, Arc<ChildControl>>>,
}

/// Keeps one client registered; unregisters on drop.
pub struct AttachSlot {
    registry: Arc<AttachRegistry>,
    id: u64,
}

impl Drop for AttachSlot {
    fn drop(&mut self) {
        self.registry.live().remove(&self.id);
    }
}

impl AttachRegistry {
    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<ChildControl>>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a client; `None` when [`MAX_ATTACHES`] are already running.
    pub fn register(self: &Arc<Self>, control: Arc<ChildControl>) -> Option<AttachSlot> {
        let mut live = self.live();
        if live.len() >= MAX_ATTACHES {
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        live.insert(id, control);
        Some(AttachSlot {
            registry: Arc::clone(self),
            id,
        })
    }

    pub fn len(&self) -> usize {
        self.live().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Signals every registered client.
    pub fn signal_all(&self, signal: Signal) {
        for control in self.live().values() {
            control.signal(signal);
        }
    }

    /// SIGHUP everything, then SIGKILL whatever is still registered after `grace`.
    pub async fn terminate_all(&self, grace: Duration) {
        self.signal_all(Signal::SIGHUP);
        let deadline = Instant::now() + grace;
        while !self.is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.signal_all(Signal::SIGKILL);
    }
}

// ---- the WebSocket bridge ------------------------------------------------------------------

/// Everything one attach connection needs.
pub struct AttachContext {
    pub runner: Arc<dyn TmuxRunner>,
    pub tmux: TmuxConfig,
    pub registry: Arc<AttachRegistry>,
    pub shutdown: watch::Receiver<bool>,
    pub session: String,
    pub cols: u16,
    pub rows: u16,
}

/// Text frames a client may send.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientText {
    Resize { cols: f64, rows: f64 },
    Input { data: String },
}

/// Turns a client text frame into a writer command; unknown or malformed frames are ignored.
fn parse_client_text(text: &str) -> Option<WriterCmd> {
    match serde_json::from_str::<ClientText>(text).ok()? {
        ClientText::Resize { cols, rows } => Some(WriterCmd::Resize {
            cols: clamp_dimension(cols, MIN_COLS, MAX_COLS),
            rows: clamp_dimension(rows, MIN_ROWS, MAX_ROWS),
        }),
        ClientText::Input { data } => Some(WriterCmd::Input(data.into_bytes())),
    }
}

/// Why the output pump stopped.
enum PumpEnd {
    /// PTY EOF: the attach client is exiting.
    OutputClosed,
    /// The browser went away (close frame, error or end of stream).
    ClientGone,
    /// Sending to the browser failed or timed out.
    SendFailed,
    /// The server is shutting down.
    Shutdown,
}

type Sink = futures_util::stream::SplitSink<WebSocket, Message>;

/// Serves one `/ws/attach` connection until it ends.
pub async fn run_attach(socket: WebSocket, mut ctx: AttachContext) {
    let (mut sink, mut stream) = socket.split();

    let runner = Arc::clone(&ctx.runner);
    let name = ctx.session.clone();
    let target = blocking(move || resolve_target(&*runner, &name, RETRY_DELAY)).await;
    let target = match target {
        Ok(Ok(target)) => target,
        Ok(Err((reason, message))) => {
            finish(&mut sink, reason, &message).await;
            linger(&mut stream).await;
            return;
        }
        Err(err) => {
            tracing::error!(%err, "resolving the session failed");
            finish(&mut sink, ExitReason::Error, ExitReason::Error.message()).await;
            linger(&mut stream).await;
            return;
        }
    };

    let cfg = ctx.tmux.clone();
    let name = ctx.session.clone();
    let (cols, rows) = (ctx.cols, ctx.rows);
    let spawned = blocking(move || spawn_attach(&cfg, &name, cols, rows)).await;
    let mut process = match spawned {
        Ok(Ok(process)) => process,
        Ok(Err(err)) => {
            tracing::warn!(session = %ctx.session, error = %format!("{err:#}"), "could not start tmux attach");
            finish(&mut sink, ExitReason::Error, &format!("{err:#}")).await;
            linger(&mut stream).await;
            return;
        }
        Err(err) => {
            tracing::error!(%err, "starting tmux attach failed");
            finish(&mut sink, ExitReason::Error, ExitReason::Error.message()).await;
            linger(&mut stream).await;
            return;
        }
    };
    let Some(_slot) = ctx.registry.register(process.control()) else {
        process.terminate().await;
        finish(
            &mut sink,
            ExitReason::Error,
            "too many terminals are attached",
        )
        .await;
        linger(&mut stream).await;
        return;
    };
    if *ctx.shutdown.borrow() {
        process.terminate().await;
        close(&mut sink, 1001, "server shutting down").await;
        linger(&mut stream).await;
        return;
    }

    let (gone_tx, mut gone) = oneshot::channel::<()>();
    let mut client = tokio::spawn(read_client(stream, process.input(), gone_tx));

    let end = pump(&mut sink, &mut process.output, &mut gone, &mut ctx.shutdown).await;
    let mut closed_politely = false;
    match end {
        PumpEnd::OutputClosed => {
            if process.wait_exit(REAP_WAIT).await.is_none() {
                process.terminate().await;
            }
            let runner = Arc::clone(&ctx.runner);
            let name = ctx.session.clone();
            let (reason, message) = match blocking(move || {
                classify_exit(&*runner, &name, &target, RETRY_DELAY)
            })
            .await
            {
                Ok(classified) => classified,
                Err(err) => {
                    tracing::error!(%err, "classifying the end of an attach session failed");
                    (ExitReason::Error, ExitReason::Error.message().to_string())
                }
            };
            finish(&mut sink, reason, &message).await;
            closed_politely = true;
        }
        PumpEnd::ClientGone | PumpEnd::SendFailed => process.terminate().await,
        PumpEnd::Shutdown => {
            process.terminate().await;
            close(&mut sink, 1001, "server shutting down").await;
            closed_politely = true;
        }
    }

    if closed_politely {
        // Let the client answer our close frame before the connection is dropped.
        let _ = timeout(CLOSE_GRACE, &mut client).await;
    }
    client.abort();
}

/// Waits (briefly) for the client to answer our close frame, so the connection is not torn
/// down under a reply that is still on its way.
async fn linger(stream: &mut SplitStream<WebSocket>) {
    let _ = timeout(CLOSE_GRACE, async {
        while let Some(Ok(message)) = stream.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}

/// Reads client frames until the client leaves, feeding the writer thread.
///
/// When the writer thread is the one that is gone (the attach client died) the end of the
/// session is announced by PTY EOF, with a proper exit frame. Input is then discarded, but
/// frames keep being read so the close handshake still completes.
async fn read_client(
    mut stream: SplitStream<WebSocket>,
    input: mpsc::Sender<WriterCmd>,
    gone: oneshot::Sender<()>,
) {
    let mut writer_alive = true;
    while let Some(message) = stream.next().await {
        let command = match message {
            Ok(Message::Binary(data)) => Some(WriterCmd::Input(data.to_vec())),
            Ok(Message::Text(text)) => parse_client_text(text.as_str()),
            Ok(Message::Ping(_) | Message::Pong(_)) => None,
            Ok(Message::Close(_)) | Err(_) => break,
        };
        if let (true, Some(command)) = (writer_alive, command) {
            writer_alive = input.send(command).await.is_ok();
        }
    }
    // The pump may have finished already.
    let _ = gone.send(());
}

/// Forwards PTY output to the browser until something ends the session.
async fn pump(
    sink: &mut Sink,
    output: &mut mpsc::Receiver<Vec<u8>>,
    gone: &mut oneshot::Receiver<()>,
    shutdown: &mut watch::Receiver<bool>,
) -> PumpEnd {
    let mut ping = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            chunk = output.recv() => match chunk {
                Some(bytes) => {
                    if !send(sink, Message::Binary(Bytes::from(bytes))).await {
                        return PumpEnd::SendFailed;
                    }
                }
                None => return PumpEnd::OutputClosed,
            },
            _ = &mut *gone => return PumpEnd::ClientGone,
            () = shutdown_requested(shutdown) => return PumpEnd::Shutdown,
            _ = ping.tick() => {
                if !send(sink, Message::Ping(Bytes::new())).await {
                    return PumpEnd::SendFailed;
                }
            }
        }
    }
}

/// One WebSocket send with a deadline. False when it failed or timed out.
async fn send(sink: &mut Sink, message: Message) -> bool {
    matches!(timeout(SEND_TIMEOUT, sink.send(message)).await, Ok(Ok(())))
}

/// The exit frame (§7.2) followed by the matching close frame.
async fn finish(sink: &mut Sink, reason: ExitReason, message: &str) {
    let frame = serde_json::json!({"type": "exit", "reason": reason.as_str(), "message": message});
    if send(sink, Message::Text(frame.to_string().into())).await {
        close(sink, reason.close_code(), reason.as_str()).await;
    }
}

async fn close(sink: &mut Sink, code: u16, reason: &str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = send(sink, Message::Close(Some(frame))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::SocketSel;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    // ---- pure helpers ----------------------------------------------------------------

    #[test]
    fn dimensions_are_clamped_and_default_when_unusable() {
        let cols = |raw| parse_dimension(raw, MIN_COLS, MAX_COLS, DEFAULT_COLS);
        let rows = |raw| parse_dimension(raw, MIN_ROWS, MAX_ROWS, DEFAULT_ROWS);
        assert_eq!(cols(None), 80);
        assert_eq!(cols(Some("120")), 120);
        assert_eq!(cols(Some(" 120 ")), 120);
        assert_eq!(cols(Some("0")), 2);
        assert_eq!(cols(Some("1")), 2);
        assert_eq!(cols(Some("-7")), 2);
        assert_eq!(cols(Some("1000")), 1000);
        assert_eq!(cols(Some("1001")), 1000);
        assert_eq!(cols(Some("99999999999999999999")), 1000);
        assert_eq!(cols(Some("abc")), 80);
        assert_eq!(cols(Some("")), 80);
        assert_eq!(cols(Some("80.9")), 80);
        assert_eq!(rows(None), 24);
        assert_eq!(rows(Some("500")), 500);
        assert_eq!(rows(Some("501")), 500);
        assert_eq!(rows(Some("1")), 2);
        assert_eq!(rows(Some("nope")), 24);
        assert_eq!(clamp_dimension(f64::NAN, 2, 10), 2);
        assert_eq!(clamp_dimension(f64::INFINITY, 2, 10), 10);
        assert_eq!(clamp_dimension(f64::NEG_INFINITY, 2, 10), 2);
    }

    #[test]
    fn attach_args_follow_the_contract_for_every_socket_selection() {
        let features = "RGB,clipboard,hyperlinks,focus,title,cstyle,usstyle,strikethrough,overline,bpaste,mouse,256";
        assert_eq!(TERMINAL_FEATURES, features);
        let cfg = |socket| TmuxConfig {
            bin: PathBuf::from("tmux"),
            socket,
        };
        let tail = ["-u", "-T", features, "attach-session", "-t", "=my work"];
        assert_eq!(attach_args(&cfg(SocketSel::Default), "my work"), tail);
        let mut named = vec!["-L".to_string(), "sock".to_string()];
        named.extend(tail.map(String::from));
        assert_eq!(
            attach_args(&cfg(SocketSel::Name("sock".into())), "my work"),
            named
        );
        let mut path = vec!["-S".to_string(), "/run/t.sock".to_string()];
        path.extend(tail.map(String::from));
        assert_eq!(
            attach_args(&cfg(SocketSel::Path("/run/t.sock".into())), "my work"),
            path
        );
    }

    #[test]
    fn exit_reasons_map_to_the_documented_strings_and_close_codes() {
        for (reason, name, code) in [
            (ExitReason::Detached, "detached", 1000),
            (ExitReason::SessionClosed, "session-closed", 1000),
            (ExitReason::ServerDown, "server-down", 1000),
            (ExitReason::NoSession, "no-session", 4404),
            (ExitReason::NoServer, "no-server", 4503),
            (ExitReason::Error, "error", 1011),
        ] {
            assert_eq!(reason.as_str(), name);
            assert_eq!(reason.close_code(), code, "{name}");
            assert!(!reason.message().is_empty());
        }
    }

    #[test]
    fn client_text_frames_are_parsed_leniently() {
        let resize = |text| match parse_client_text(text) {
            Some(WriterCmd::Resize { cols, rows }) => Some((cols, rows)),
            _ => None,
        };
        assert_eq!(
            resize(r#"{"type":"resize","cols":100,"rows":30}"#),
            Some((100, 30))
        );
        assert_eq!(
            resize(r#"{"type":"resize","cols":100.0,"rows":30.5}"#),
            Some((100, 30))
        );
        assert_eq!(
            resize(r#"{"type":"resize","cols":0,"rows":100000}"#),
            Some((2, 500))
        );
        assert_eq!(
            resize(r#"{"type":"resize","cols":5000,"rows":1}"#),
            Some((1000, 2))
        );
        assert_eq!(resize(r#"{"type":"resize","cols":"80","rows":24}"#), None);
        assert_eq!(resize(r#"{"type":"resize","cols":80}"#), None);
        match parse_client_text(r#"{"type":"input","data":"ls\r"}"#) {
            Some(WriterCmd::Input(bytes)) => assert_eq!(bytes, b"ls\r"),
            _ => panic!("expected input"),
        }
        for ignored in [
            "",
            "not json",
            "{}",
            r#"{"type":"bogus"}"#,
            r#"{"type":"input"}"#,
            "[1]",
        ] {
            assert!(parse_client_text(ignored).is_none(), "{ignored:?}");
        }
    }

    // ---- exit classification ----------------------------------------------------------

    /// Replays canned answers; anything not scripted fails the test.
    struct Scripted {
        replies: Mutex<VecDeque<Result<String, TmuxError>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Scripted {
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

    impl TmuxRunner for Scripted {
        fn run(&self, args: &[String]) -> Result<String, TmuxError> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unscripted tmux call")
        }
    }

    fn session_line(id: &str, name: &str) -> String {
        format!("4242\t1700000000\t/tmp/tmux-1000/default\t3.4\t{id}\t1\t0\t1700000100\t1700000200\t0\t\t{name}\n")
    }

    fn listing(lines: &[(&str, &str)]) -> Result<String, TmuxError> {
        Ok(lines
            .iter()
            .map(|(id, name)| session_line(id, name))
            .collect())
    }

    fn command_error() -> TmuxError {
        TmuxError::Command("server exited unexpectedly".into())
    }

    const NO_DELAY: Duration = Duration::ZERO;
    const SERVER: &str = "1700000000-4242";

    fn target(session_id: &str) -> Target {
        Target {
            server_id: SERVER.into(),
            session_id: session_id.into(),
        }
    }

    #[test]
    fn resolve_target_finds_the_session_and_its_ids() {
        let runner = Scripted::new(vec![listing(&[("$0", "alpha"), ("$1", "beta")])]);
        assert_eq!(resolve_target(&runner, "beta", NO_DELAY), Ok(target("$1")));
    }

    #[test]
    fn resolve_target_reports_missing_sessions_and_servers() {
        let runner = Scripted::new(vec![listing(&[("$0", "alpha")])]);
        assert_eq!(
            resolve_target(&runner, "beta", NO_DELAY).unwrap_err().0,
            ExitReason::NoSession
        );
        // The prefix of an existing name is not that session.
        let runner = Scripted::new(vec![listing(&[("$0", "alpha")])]);
        assert_eq!(
            resolve_target(&runner, "alp", NO_DELAY).unwrap_err().0,
            ExitReason::NoSession
        );
        let runner = Scripted::new(vec![Err(TmuxError::NoServer)]);
        assert_eq!(
            resolve_target(&runner, "alpha", NO_DELAY).unwrap_err().0,
            ExitReason::NoServer
        );
        // A server without sessions has no such session either.
        let runner = Scripted::new(vec![Ok(String::new())]);
        assert_eq!(
            resolve_target(&runner, "alpha", NO_DELAY).unwrap_err().0,
            ExitReason::NoSession
        );
    }

    #[test]
    fn resolve_target_never_asks_tmux_about_names_it_cannot_hold() {
        let runner = Scripted::new(vec![]);
        for name in ["", "a:b", "a\0b"] {
            assert_eq!(
                resolve_target(&runner, name, NO_DELAY).unwrap_err().0,
                ExitReason::NoSession,
                "{name:?}"
            );
        }
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn resolve_target_retries_one_transient_command_error() {
        let runner = Scripted::new(vec![Err(command_error()), listing(&[("$0", "alpha")])]);
        assert_eq!(resolve_target(&runner, "alpha", NO_DELAY), Ok(target("$0")));
        let runner = Scripted::new(vec![Err(command_error()), Err(command_error())]);
        assert_eq!(
            resolve_target(&runner, "alpha", NO_DELAY).unwrap_err().0,
            ExitReason::Error
        );
        assert_eq!(runner.calls().len(), 2, "exactly one retry");
    }

    #[test]
    fn a_session_that_is_still_there_means_detached() {
        let runner = Scripted::new(vec![Ok(String::new())]);
        let (reason, message) = classify_exit(&runner, "alpha", &target("$0"), NO_DELAY);
        assert_eq!(reason, ExitReason::Detached);
        assert!(!message.is_empty());
        // The question is asked about the exact name.
        assert_eq!(runner.calls(), vec![vec!["has-session", "-t", "=alpha"]]);
    }

    #[test]
    fn a_server_without_the_session_means_session_closed() {
        let runner = Scripted::new(vec![
            Err(TmuxError::Command("can't find session: alpha".into())),
            listing(&[("$1", "beta")]),
        ]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::SessionClosed
        );
    }

    /// The tmux subcommand of every call the runner has seen so far.
    fn verbs(runner: &Scripted) -> Vec<String> {
        runner
            .calls()
            .into_iter()
            .map(|argv| argv[0].clone())
            .collect()
    }

    #[test]
    fn a_failing_confirmation_query_means_error_not_session_closed() {
        let gone = || Err(TmuxError::Command("can't find session: alpha".into()));
        let boom = || Err(TmuxError::Command("boom".into()));
        // `has-session` says the session is missing, but the query that should confirm
        // "server up, session gone" fails, on its one retry as well: nothing was confirmed.
        let runner = Scripted::new(vec![gone(), boom(), boom()]);
        let (reason, message) = classify_exit(&runner, "alpha", &target("$0"), NO_DELAY);
        assert_eq!(reason, ExitReason::Error);
        assert!(message.contains("boom"), "{message}");
        assert_eq!(
            verbs(&runner),
            ["has-session", "list-sessions", "list-sessions"]
        );
        // Errors that are not retried still end as `error`, after a single query.
        for failure in [
            TmuxError::Parse("garbled".into()),
            TmuxError::Io(std::io::Error::other("pipe broke")),
        ] {
            let detail = failure.to_string();
            let runner = Scripted::new(vec![gone(), Err(failure)]);
            let (reason, message) = classify_exit(&runner, "alpha", &target("$0"), NO_DELAY);
            assert_eq!(reason, ExitReason::Error, "{detail}");
            assert!(message.contains(&detail), "{message}");
            assert_eq!(verbs(&runner), ["has-session", "list-sessions"]);
        }
    }

    #[test]
    fn the_confirmation_query_still_gets_its_one_retry() {
        let gone = || Err(TmuxError::Command("can't find session: alpha".into()));
        let boom = || Err(TmuxError::Command("boom".into()));
        // A retry that succeeds is a real answer: server up, session gone.
        let runner = Scripted::new(vec![gone(), boom(), listing(&[("$1", "beta")])]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::SessionClosed
        );
        // A server running without any session is also a successful answer.
        let runner = Scripted::new(vec![gone(), Ok(String::new())]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::SessionClosed
        );
        // A retry that finds the server gone is `server-down`, not `error`.
        let runner = Scripted::new(vec![gone(), boom(), Err(TmuxError::NoServer)]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::ServerDown
        );
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn no_server_means_server_down() {
        let runner = Scripted::new(vec![Err(TmuxError::NoServer)]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::ServerDown
        );
        // The server may also disappear between the two questions.
        let runner = Scripted::new(vec![
            Err(TmuxError::Command("can't find session: alpha".into())),
            Err(TmuxError::NoServer),
        ]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::ServerDown
        );
    }

    #[test]
    fn a_transient_command_error_right_after_eof_is_retried_once() {
        let runner = Scripted::new(vec![Err(command_error()), Err(TmuxError::NoServer)]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::ServerDown
        );
        let runner = Scripted::new(vec![Err(command_error()), Ok(String::new())]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::Detached
        );
        // Still failing on the retry: only now it becomes `error`, carrying tmux's message.
        let runner = Scripted::new(vec![Err(command_error()), Err(command_error())]);
        let (reason, message) = classify_exit(&runner, "alpha", &target("$0"), NO_DELAY);
        assert_eq!(reason, ExitReason::Error);
        assert!(message.contains("server exited unexpectedly"), "{message}");
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn the_retry_waits_for_the_delay() {
        let runner = Scripted::new(vec![Err(command_error()), Err(TmuxError::NoServer)]);
        let started = std::time::Instant::now();
        classify_exit(&runner, "alpha", &target("$0"), Duration::from_millis(150));
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn a_session_renamed_while_attached_still_counts_as_detached() {
        let runner = Scripted::new(vec![
            Err(TmuxError::Command("can't find session: alpha".into())),
            listing(&[("$0", "alpha-renamed")]),
        ]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::Detached
        );
    }

    #[test]
    fn a_new_server_with_a_reused_session_id_is_not_the_old_session() {
        // Session ids restart at $0 on every server; the server id tells them apart.
        let mut other = session_line("$0", "other");
        other = other.replace("1700000000\t/tmp", "1800000000\t/tmp");
        let runner = Scripted::new(vec![
            Err(TmuxError::Command("can't find session: alpha".into())),
            Ok(other),
        ]);
        assert_eq!(
            classify_exit(&runner, "alpha", &target("$0"), NO_DELAY).0,
            ExitReason::SessionClosed
        );
    }

    // ---- registry ---------------------------------------------------------------------

    fn dummy_control() -> Arc<ChildControl> {
        // Pid 1 is never signalled by these tests; the control is only stored.
        Arc::new(ChildControl::new(Pid::from_raw(1)))
    }

    #[test]
    fn the_registry_counts_slots_and_enforces_its_limit() {
        let registry = Arc::new(AttachRegistry::default());
        assert!(registry.is_empty());
        let slots: Vec<_> = (0..MAX_ATTACHES)
            .map(|_| registry.register(dummy_control()).expect("below the limit"))
            .collect();
        assert_eq!(registry.len(), MAX_ATTACHES);
        assert!(registry.register(dummy_control()).is_none());
        drop(slots);
        assert!(registry.is_empty());
        assert!(registry.register(dummy_control()).is_some());
    }

    #[test]
    fn a_reaped_child_is_never_signalled() {
        let control = ChildControl::new(Pid::from_raw(1));
        control.reap_with(|| ());
        // Were this to signal pid 1, SIGKILL would be refused anyway; the point is that
        // nothing is attempted once the child counts as reaped.
        assert!(!control.signal(Signal::SIGKILL));
    }

    // ---- real PTY with a stand-in for tmux -----------------------------------------------

    /// A script playing the part of tmux: it ignores its arguments and runs `body`.
    fn fake_tmux(dir: &std::path::Path, body: &str) -> TmuxConfig {
        let path = dir.join("fake-tmux");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        TmuxConfig {
            bin: path,
            socket: SocketSel::Default,
        }
    }

    async fn read_until(process: &mut AttachProcess, needle: &str) -> String {
        let mut seen = String::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen.contains(needle) {
            let chunk = timeout(deadline - Instant::now(), process.output.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; saw {seen:?}"))
                .unwrap_or_else(|| panic!("PTY closed before {needle:?}; saw {seen:?}"));
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        seen
    }

    fn process_exists(pid: Pid) -> bool {
        std::path::Path::new(&format!("/proc/{}", pid.as_raw())).exists()
    }

    #[tokio::test]
    async fn the_pty_has_the_requested_size_echoes_input_and_follows_resizes() {
        let dir = tempfile::tempdir().unwrap();
        // Prints the terminal size whenever it changes. Input is echoed by the tty itself.
        let cfg = fake_tmux(
            dir.path(),
            "last=\nwhile true; do cur=$(stty size); if [ \"$cur\" != \"$last\" ]; then echo size=$cur; last=$cur; fi; sleep 0.05; done",
        );
        let mut process =
            tokio::task::spawn_blocking(move || spawn_attach(&cfg, "ignored", 100, 30))
                .await
                .unwrap()
                .unwrap();
        read_until(&mut process, "size=30 100").await;

        process
            .input()
            .send(WriterCmd::Input(b"hello\n".to_vec()))
            .await
            .unwrap();
        read_until(&mut process, "hello").await;

        process
            .input()
            .send(WriterCmd::Resize {
                cols: 120,
                rows: 40,
            })
            .await
            .unwrap();
        read_until(&mut process, "size=40 120").await;

        process.terminate().await;
        assert!(process.wait_exit(Duration::from_secs(1)).await.is_some());
    }

    #[tokio::test]
    async fn the_client_runs_with_the_documented_environment_and_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = fake_tmux(
            dir.path(),
            "echo \"term=$TERM colorterm=$COLORTERM tmux=${TMUX-unset} pane=${TMUX_PANE-unset}\"\necho \"args=$*\"\necho \"cwd=$(pwd)\"\nsleep 1",
        );
        let cfg = TmuxConfig {
            socket: SocketSel::Name("sock".into()),
            ..cfg
        };
        let mut process =
            tokio::task::spawn_blocking(move || spawn_attach(&cfg, "my work", 80, 24))
                .await
                .unwrap()
                .unwrap();
        let seen = read_until(&mut process, "cwd=/").await;
        // When the tests run inside tmux the parent has TMUX set, so this checks the removal.
        assert!(
            seen.contains("term=xterm-256color colorterm=truecolor tmux=unset pane=unset"),
            "{seen}"
        );
        assert!(
            seen.contains(&format!(
                "args=-L sock -u -T {TERMINAL_FEATURES} attach-session -t =my work"
            )),
            "{seen}"
        );
        let home = std::fs::canonicalize(util::home_dir()).unwrap();
        assert!(seen.contains(&format!("cwd={}", home.display())), "{seen}");
        process.terminate().await;
    }

    #[tokio::test]
    async fn terminate_reaps_the_child_so_no_zombie_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = fake_tmux(dir.path(), "echo ready\nexec sleep 30");
        let mut process = tokio::task::spawn_blocking(move || spawn_attach(&cfg, "x", 80, 24))
            .await
            .unwrap()
            .unwrap();
        read_until(&mut process, "ready").await;
        let pid = process.control().pid;
        assert!(process_exists(pid));
        process.terminate().await;
        // Reaped means gone from /proc altogether, not merely a zombie.
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!process_exists(pid), "pid {pid} is still listed in /proc");
        let exit = process.wait_exit(Duration::from_secs(1)).await.unwrap();
        assert!(exit.signal.is_some() || exit.code.is_some());
    }

    #[tokio::test]
    async fn a_client_that_ignores_sighup_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = fake_tmux(
            dir.path(),
            "trap '' HUP\necho ready\nwhile true; do sleep 1; done",
        );
        let mut process = tokio::task::spawn_blocking(move || spawn_attach(&cfg, "x", 80, 24))
            .await
            .unwrap()
            .unwrap();
        read_until(&mut process, "ready").await;
        let pid = process.control().pid;
        let started = std::time::Instant::now();
        process.terminate().await;
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(!process_exists(pid));
    }

    #[tokio::test]
    async fn pty_eof_is_reported_when_the_client_exits_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = fake_tmux(dir.path(), "echo bye\nexit 3");
        let mut process = tokio::task::spawn_blocking(move || spawn_attach(&cfg, "x", 80, 24))
            .await
            .unwrap()
            .unwrap();
        read_until(&mut process, "bye").await;
        let closed = timeout(Duration::from_secs(5), process.output.recv())
            .await
            .unwrap();
        assert!(closed.is_none(), "output closes at EOF");
        let exit = process
            .wait_exit(Duration::from_secs(5))
            .await
            .expect("reaped");
        assert_eq!(exit.code, Some(3));
    }

    #[tokio::test]
    async fn a_missing_binary_is_an_error_not_a_panic() {
        let cfg = TmuxConfig {
            bin: PathBuf::from("/nonexistent/tmux"),
            socket: SocketSel::Default,
        };
        let result = tokio::task::spawn_blocking(move || spawn_attach(&cfg, "x", 80, 24))
            .await
            .unwrap();
        assert!(result.is_err());
    }

    /// Characterises the library behaviour the writer/reaper ordering exists for: dropping a
    /// portable-pty writer types a newline and the EOF character into the terminal. If this
    /// ever stops being true the ordering can be simplified; while it holds it must be kept.
    #[tokio::test]
    async fn dropping_the_pty_writer_types_a_newline_and_eof_into_a_live_client() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "cat >/dev/null; echo cat-saw-eof"]);
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        drop(writer);
        let collected = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let mut buffer = [0u8; 256];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                out.extend_from_slice(&buffer[..read]);
            }
            String::from_utf8_lossy(&out).into_owned()
        })
        .await
        .unwrap();
        child.wait().unwrap();
        assert!(collected.contains("cat-saw-eof"), "{collected:?}");
    }
}
