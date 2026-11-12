//! Background tmux status polling (owned by the server unit).
//!
//! The [`Monitor`] polls `tmux list-sessions`, keeps the latest [`TmuxStatus`] and the per-session
//! Claude pane counts (from a periodic in-memory scan, never written to disk), and publishes every
//! change as a JSON text frame on a broadcast channel (the `/ws/events` stream). All mutation goes
//! through one polling routine, so the timer and API-triggered refreshes cannot disagree.

use crate::claude::ClaudeDetector;
use crate::snapshot::{self, Snapshot};
use crate::snooze::SnoozeStore;
use crate::tmux::{self, ServerIdentity, SessionInfo, TmuxError, TmuxRunner};
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tokio::time::{interval, MissedTickBehavior};

/// Events a lagging subscriber can fall behind by before it is resynchronised.
const EVENT_CAPACITY: usize = 64;

/// How often the panes are rescanned for Claude (also whenever the session/window structure changes).
const SCAN_INTERVAL: Duration = Duration::from_secs(15);

/// Repeated identical warnings are logged at most this often.
const WARN_EVERY: Duration = Duration::from_secs(300);

/// A poll that finished less than this long ago satisfies a plain refresh request.
pub const REFRESH_MAX_AGE: Duration = Duration::from_millis(150);

/// Failure of a `spawn_blocking` task (it panicked or the runtime is shutting down).
#[derive(Debug, thiserror::Error)]
#[error("background task failed: {0}")]
pub struct BlockingError(String);

/// Runs `work` on tokio's blocking pool. Everything that talks to tmux, scans `/proc`
/// goes through here, never on an async worker thread.
pub async fn blocking<T, F>(work: F) -> Result<T, BlockingError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| BlockingError(err.to_string()))
}

/// Resolves once shutdown was requested (or the sender is gone, which means the same).
///
/// Returns `()` rather than the watch guard so it can sit in a `select!` arm of a future that
/// must be `Send`.
pub async fn shutdown_requested(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// `TmuxStatus` of §7: what `GET /api/status` returns and what `status` events carry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TmuxStatus {
    pub running: bool,
    pub server: Option<ServerIdentity>,
    /// Sorted by name.
    pub sessions: Vec<SessionInfo>,
    pub updated_at: i64,
}

impl TmuxStatus {
    fn stopped() -> Self {
        Self {
            running: false,
            server: None,
            sessions: Vec::new(),
            updated_at: util::now_ms(),
        }
    }

    /// Equality for change detection: `activity` ticks constantly and `updated_at` is only a
    /// timestamp, so neither counts.
    fn same_as(&self, other: &TmuxStatus) -> bool {
        self.running == other.running
            && self.server == other.server
            && self.sessions.len() == other.sessions.len()
            && self.sessions.iter().zip(&other.sessions).all(|(a, b)| {
                a.id == b.id
                    && a.name == b.name
                    && a.windows == b.windows
                    && a.attached == b.attached
                    && a.created == b.created
                    && a.group == b.group
                    && a.claude_panes == b.claude_panes
                    && a.snoozed == b.snoozed
            })
    }
}

/// The `/ws/events` frames. `type` is serialised first.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame<'a> {
    Status(&'a TmuxStatus),
    Layout { version: u64, client_id: &'a str },
}

fn frame_json(frame: &Frame<'_>) -> Arc<str> {
    match serde_json::to_string(frame) {
        Ok(json) => Arc::from(json),
        Err(err) => {
            // Everything serialised here is plain data; this cannot happen in practice.
            tracing::error!(%err, "could not serialise an event frame");
            Arc::from(r#"{"type":"error"}"#)
        }
    }
}

/// Number of panes running Claude Code per session, from the latest scan. Session ids
/// are only unique within one tmux server, so the counts remember which server they are from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ClaudeCounts {
    server_id: String,
    by_session: HashMap<String, u32>,
    /// Conversation ids of the Claude processes running now, per session NAME.
    running: HashMap<String, HashSet<String>>,
}

impl ClaudeCounts {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        let by_session = snapshot
            .sessions
            .iter()
            .map(|session| {
                let panes = session
                    .windows
                    .iter()
                    .flat_map(|window| &window.panes)
                    .filter(|pane| pane.claude.is_some())
                    .count();
                (session.id.clone(), u32::try_from(panes).unwrap_or(u32::MAX))
            })
            .collect();
        let running = snapshot
            .sessions
            .iter()
            .map(|session| (session.name.clone(), Self::running_ids(session)))
            .collect();
        Self {
            server_id: snapshot.server.id(),
            by_session,
            running,
        }
    }

    fn running_ids(session: &snapshot::SessionSnap) -> HashSet<String> {
        session
            .windows
            .iter()
            .flat_map(|window| &window.panes)
            .filter_map(|pane| pane.claude.as_ref()?.session_id.clone())
            .collect()
    }

    /// The conversations running in the session called `name` (empty for another server).
    fn running_in(&self, server: Option<&ServerIdentity>, name: &str) -> HashSet<String> {
        match server {
            Some(server) if server.id() == self.server_id => {
                self.running.get(name).cloned().unwrap_or_default()
            }
            _ => HashSet::new(),
        }
    }

    fn for_session(&self, server: Option<&ServerIdentity>, session: &SessionInfo) -> u32 {
        match server {
            Some(server) if server.id() == self.server_id => {
                self.by_session.get(&session.id).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MonitorSettings {
    pub poll_interval: Duration,
}

/// How the panes are scanned; replaceable so tests need no real tmux.
pub type SnapshotFn =
    dyn Fn(&dyn TmuxRunner, &ClaudeDetector) -> Result<Snapshot, TmuxError> + Send + Sync;

/// Mutable state shared with readers; only ever locked briefly and never across an await.
struct State {
    status: TmuxStatus,
    status_frame: Arc<str>,
    claude: ClaudeCounts,
}

/// State owned by whoever is polling (guarded by the poll lock).
#[derive(Default)]
struct PollState {
    /// `(session id, window count)` pairs of the previous poll.
    structure: Option<Vec<(String, u32)>>,
    last_scan_attempt: Option<Instant>,
    last_finished: Option<Instant>,
    last_warning: Option<(String, Instant)>,
}

struct Inner {
    settings: MonitorSettings,
    runner: Arc<dyn TmuxRunner>,
    detector: ClaudeDetector,
    scan: Arc<SnapshotFn>,
    snooze: Arc<SnoozeStore>,
    events: broadcast::Sender<Arc<str>>,
    state: Mutex<State>,
    poll: AsyncMutex<PollState>,
    scan_lock: AsyncMutex<()>,
}

/// Cheap to clone; all clones share one state.
#[derive(Clone)]
pub struct Monitor {
    inner: Arc<Inner>,
}

/// A new `/ws/events` subscriber: the frame describing the present, and the stream of changes
/// after it. Nothing published between the two is missed or duplicated.
pub struct Subscription {
    pub status: Arc<str>,
    pub events: broadcast::Receiver<Arc<str>>,
}

impl Monitor {
    pub fn new(
        runner: Arc<dyn TmuxRunner>,
        detector: ClaudeDetector,
        settings: MonitorSettings,
        snooze: Arc<SnoozeStore>,
    ) -> Monitor {
        Monitor::with_snapshot_fn(
            runner,
            detector,
            settings,
            snooze,
            Arc::new(|runner: &dyn TmuxRunner, detector: &ClaudeDetector| {
                snapshot::take_snapshot(runner, detector)
            }),
        )
    }

    pub fn with_snapshot_fn(
        runner: Arc<dyn TmuxRunner>,
        detector: ClaudeDetector,
        settings: MonitorSettings,
        snooze: Arc<SnoozeStore>,
        scan: Arc<SnapshotFn>,
    ) -> Monitor {
        let status = TmuxStatus::stopped();
        let state = State {
            status_frame: frame_json(&Frame::Status(&status)),
            status,
            claude: ClaudeCounts::default(),
        };
        Monitor {
            inner: Arc::new(Inner {
                settings,
                runner,
                detector,
                scan,
                snooze,
                events: broadcast::channel(EVENT_CAPACITY).0,
                state: Mutex::new(state),
                poll: AsyncMutex::new(PollState::default()),
                scan_lock: AsyncMutex::new(()),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The most recent status (as of the last poll).
    pub fn status(&self) -> TmuxStatus {
        self.state().status.clone()
    }

    /// The frame that describes the present, for a subscriber that fell behind.
    pub fn current_frame(&self) -> Arc<str> {
        self.state().status_frame.clone()
    }

    /// Frames are published while the state lock is held, and so is this subscription, which
    /// is what makes "present + changes after it" gap-free.
    pub fn subscribe(&self) -> Subscription {
        let state = self.state();
        Subscription {
            status: state.status_frame.clone(),
            events: self.inner.events.subscribe(),
        }
    }

    /// Announces a saved layout to every events subscriber.
    pub fn publish_layout(&self, version: u64, client_id: &str) {
        let frame = frame_json(&Frame::Layout { version, client_id });
        let _state = self.state();
        // No subscribers is fine.
        let _ = self.inner.events.send(frame);
    }

    // ---- polling ---------------------------------------------------------------------

    /// Polls now and returns the new status.
    pub async fn poll_now(&self) -> TmuxStatus {
        let mut poll = self.inner.poll.lock().await;
        self.poll_locked(&mut poll).await;
        self.status()
    }

    /// The status, at most `max_age` old: polls only when the last poll is older than that.
    pub async fn refresh(&self, max_age: Duration) -> TmuxStatus {
        let mut poll = self.inner.poll.lock().await;
        let fresh_enough = poll
            .last_finished
            .is_some_and(|finished| finished.elapsed() < max_age);
        if !fresh_enough {
            self.poll_locked(&mut poll).await;
        }
        self.status()
    }

    /// A fresh snapshot of every pane, outside the polling cycle (the same scan the monitor uses).
    pub async fn snapshot(&self) -> Result<Snapshot, TmuxError> {
        let runner = Arc::clone(&self.inner.runner);
        let detector = self.inner.detector.clone();
        let scan = Arc::clone(&self.inner.scan);
        blocking(move || scan(&*runner, &detector))
            .await
            .map_err(|err| TmuxError::Command(err.to_string()))?
    }

    /// Polls and rescans the panes now, so that Claude counts and snoozed counts are current.
    pub async fn refresh_with_scan(&self) -> TmuxStatus {
        let mut poll = self.inner.poll.lock().await;
        self.poll_locked(&mut poll).await;
        if self.status().running {
            poll.last_scan_attempt = Some(Instant::now());
            if let Err(err) = self.scan_panes().await {
                self.warn_throttled(&mut poll, format!("pane scan failed: {err}"));
            }
        }
        self.status()
    }

    /// Polls every `poll_interval` for as long as the future is polled.
    pub async fn run(self) {
        let mut ticker = interval(self.inner.settings.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let mut poll = self.inner.poll.lock().await;
            self.poll_locked(&mut poll).await;
        }
    }

    async fn poll_locked(&self, poll: &mut PollState) {
        self.poll_inner(poll).await;
        poll.last_finished = Some(Instant::now());
    }

    async fn poll_inner(&self, poll: &mut PollState) {
        let runner = Arc::clone(&self.inner.runner);
        let listed = blocking(move || tmux::list_sessions(&*runner)).await;
        let (running, server, sessions) = match listed {
            Ok(Ok((server, sessions))) => (true, server, sessions),
            Ok(Err(TmuxError::NoServer)) => (false, None, Vec::new()),
            // A transient failure says nothing about the server: keep what we know.
            Ok(Err(err)) => {
                self.warn_throttled(poll, format!("listing tmux sessions failed: {err}"));
                return;
            }
            Err(err) => {
                self.warn_throttled(poll, format!("listing tmux sessions failed: {err}"));
                return;
            }
        };

        let mut structure: Vec<(String, u32)> = sessions
            .iter()
            .map(|session| (session.id.clone(), session.windows))
            .collect();
        structure.sort();
        let structure_changed = poll.structure.as_ref() != Some(&structure);
        poll.structure = Some(structure);

        let has_sessions = !sessions.is_empty();
        self.publish_status(running, server, sessions);

        let interval_elapsed = poll
            .last_scan_attempt
            .map_or(true, |attempt| attempt.elapsed() >= SCAN_INTERVAL);
        if running && has_sessions && (structure_changed || interval_elapsed) {
            poll.last_scan_attempt = Some(Instant::now());
            if let Err(err) = self.scan_panes().await {
                self.warn_throttled(poll, format!("pane scan failed: {err}"));
            }
        }
    }

    fn warn_throttled(&self, poll: &mut PollState, message: String) {
        let repeated = poll
            .last_warning
            .as_ref()
            .is_some_and(|(last, at)| *last == message && at.elapsed() < WARN_EVERY);
        if !repeated {
            tracing::warn!("{message}");
            poll.last_warning = Some((message, Instant::now()));
        }
    }

    /// Stores `status` as the current one and publishes it when it changed.
    fn publish_status(
        &self,
        running: bool,
        server: Option<ServerIdentity>,
        mut sessions: Vec<SessionInfo>,
    ) {
        sessions.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        let mut state = self.state();
        for session in &mut sessions {
            session.claude_panes = state.claude.for_session(server.as_ref(), session);
            let running = state.claude.running_in(server.as_ref(), &session.name);
            session.snoozed = self.inner.snooze.snoozed_count(&session.name, &running);
        }
        let status = TmuxStatus {
            running,
            server,
            sessions,
            updated_at: util::now_ms(),
        };
        let changed = !state.status.same_as(&status);
        let frame = frame_json(&Frame::Status(&status));
        state.status = status;
        state.status_frame = frame.clone();
        if changed {
            let _ = self.inner.events.send(frame);
        }
    }

    /// Re-applies the current Claude pane counts to the cached status.
    fn republish_cached(&self) {
        let (running, server, sessions) = {
            let state = self.state();
            (
                state.status.running,
                state.status.server.clone(),
                state.status.sessions.clone(),
            )
        };
        self.publish_status(running, server, sessions);
    }

    // ---- pane scan -------------------------------------------------------------------

    /// Scans the panes (in memory only) and folds the Claude counts into the status.
    async fn scan_panes(&self) -> Result<(), ScanError> {
        let _serial = self.inner.scan_lock.lock().await;
        let runner = Arc::clone(&self.inner.runner);
        let detector = self.inner.detector.clone();
        let scan = Arc::clone(&self.inner.scan);
        let snapshot = blocking(move || scan(&*runner, &detector)).await??;

        let counts = ClaudeCounts::from_snapshot(&snapshot);
        // A conversation seen running again was resumed by hand: it is not snoozed any more.
        for (name, running) in &counts.running {
            if let Err(err) = self.inner.snooze.drop_running(name, running) {
                tracing::warn!(%err, "could not update the snooze store");
            }
        }
        self.state().claude = counts;
        // Always: the store pass above may have changed the snoozed counts too.
        self.republish_cached();
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum ScanError {
    #[error("{0}")]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Internal(#[from] BlockingError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::{ClaudeIdSource, ClaudeSnap};
    use crate::snapshot::{PaneSnap, SessionSnap, WindowSnap};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // ---- a scriptable tmux ---------------------------------------------------------

    #[derive(Clone)]
    struct FakeSession {
        id: &'static str,
        name: &'static str,
        windows: u32,
        activity: i64,
    }

    /// `None` = no server; otherwise (server pid, sessions).
    #[derive(Default)]
    struct FakeTmux {
        server: Mutex<Option<(u32, Vec<FakeSession>)>>,
        failing: AtomicBool,
        list_calls: AtomicUsize,
        /// Every command other than `list-sessions`, in order.
        other: Mutex<Vec<Vec<String>>>,
    }

    impl FakeTmux {
        fn set(&self, pid: u32, sessions: Vec<FakeSession>) {
            *self.server.lock().unwrap() = Some((pid, sessions));
        }

        fn stop(&self) {
            *self.server.lock().unwrap() = None;
        }
    }

    impl TmuxRunner for FakeTmux {
        fn run(&self, args: &[String]) -> Result<String, TmuxError> {
            let command = args.first().map(String::as_str);
            if command != Some("list-sessions") {
                self.other.lock().unwrap().push(args.to_vec());
            } else {
                self.list_calls.fetch_add(1, Ordering::SeqCst);
            }
            if self.failing.load(Ordering::SeqCst) {
                return Err(TmuxError::Command("server exited unexpectedly".into()));
            }
            let guard = self.server.lock().unwrap();
            let Some((pid, sessions)) = guard.as_ref() else {
                return Err(TmuxError::NoServer);
            };
            match command {
                Some("list-sessions") => {}
                // Anything else fails here.
                _ => return Err(TmuxError::Command("the fake cannot do that".into())),
            }
            Ok(sessions
                .iter()
                .map(|s| {
                    format!(
                        "{pid}\t1700000000\t/tmp/tmux-1000/test\t3.4\t{}\t{}\t0\t1700000100\t{}\t0\t\t{}\n",
                        s.id, s.windows, s.activity, s.name
                    )
                })
                .collect())
        }
    }

    fn session(id: &'static str, name: &'static str) -> FakeSession {
        FakeSession {
            id,
            name,
            windows: 1,
            activity: 1_700_000_200,
        }
    }

    fn claude_snap() -> ClaudeSnap {
        ClaudeSnap {
            pid: 1,
            session_id: Some("0123456789abcdef".into()),
            cwd: "/tmp".into(),
            args: Vec::new(),
            kind: Some("interactive".into()),
            name: None,
            source: ClaudeIdSource::SessionsFile,
            status: None,
            config_dir: None,
        }
    }

    fn pane(id: &str, claude: bool) -> PaneSnap {
        PaneSnap {
            id: id.into(),
            index: 0,
            active: true,
            pid: 1,
            cwd: "/tmp".into(),
            current_command: "sh".into(),
            foreground_cmdline: None,
            claude: claude.then(claude_snap),
        }
    }

    /// A snapshot of server pid 4242 where `claude_in` lists `(session id, claude pane count)`.
    fn snapshot_with(pid: u32, claude_in: &[(&str, usize)]) -> Snapshot {
        Snapshot {
            version: 1,
            taken_at: util::now_ms(),
            server: ServerIdentity {
                pid,
                start_time: 1_700_000_000,
                socket_path: "/tmp/tmux-1000/test".into(),
                version: "3.4".into(),
            },
            sessions: claude_in
                .iter()
                .map(|(id, claude_panes)| SessionSnap {
                    id: (*id).into(),
                    name: format!("name-{id}"),
                    created: 1,
                    group: None,
                    windows: vec![WindowSnap {
                        id: "@0".into(),
                        index: 0,
                        name: "w".into(),
                        automatic_rename: true,
                        active: true,
                        zoomed: false,
                        width: 80,
                        height: 24,
                        layout: "layout".into(),
                        panes: (0..*claude_panes.max(&1))
                            .map(|n| pane(&format!("%{n}"), n < *claude_panes))
                            .collect(),
                    }],
                })
                .collect(),
        }
    }

    struct Rig {
        monitor: Monitor,
        tmux: Arc<FakeTmux>,
        scans_taken: Arc<AtomicUsize>,
        next_scan: Arc<Mutex<Option<Result<Snapshot, TmuxError>>>>,
        _dir: tempfile::TempDir,
    }

    fn settings() -> MonitorSettings {
        MonitorSettings {
            poll_interval: Duration::from_millis(20),
        }
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings();
        let tmux = Arc::new(FakeTmux::default());
        let taken = Arc::new(AtomicUsize::new(0));
        let next: Arc<Mutex<Option<Result<Snapshot, TmuxError>>>> = Arc::new(Mutex::new(None));
        let (taken2, next2) = (taken.clone(), next.clone());
        let monitor = Monitor::with_snapshot_fn(
            tmux.clone(),
            ClaudeDetector::new(dir.path().join("claude")),
            settings,
            Arc::new(SnoozeStore::open(dir.path())),
            Arc::new(move |_: &dyn TmuxRunner, _: &ClaudeDetector| {
                taken2.fetch_add(1, Ordering::SeqCst);
                next2
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| Ok(snapshot_with(4242, &[])))
            }),
        );
        Rig {
            monitor,
            tmux,
            scans_taken: taken,
            next_scan: next,
            _dir: dir,
        }
    }

    fn drain(events: &mut broadcast::Receiver<Arc<str>>) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Ok(frame) = events.try_recv() {
            frames.push(serde_json::from_str(&frame).unwrap());
        }
        frames
    }

    fn types(frames: &[Value]) -> Vec<&str> {
        frames.iter().map(|f| f["type"].as_str().unwrap()).collect()
    }

    // ---- frames and statuses -------------------------------------------------------

    #[test]
    fn frames_put_the_type_first_and_carry_the_documented_fields() {
        let status = TmuxStatus {
            running: true,
            server: None,
            sessions: vec![],
            updated_at: 5,
        };
        let json = frame_json(&Frame::Status(&status));
        assert!(
            json.starts_with(r#"{"type":"status","running":true"#),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap(),
            json!({"type": "status", "running": true, "server": null, "sessions": [], "updated_at": 5})
        );
        let json = frame_json(&Frame::Layout {
            version: 3,
            client_id: "c-1",
        });
        assert_eq!(&*json, r#"{"type":"layout","version":3,"client_id":"c-1"}"#);
    }

    #[tokio::test]
    async fn the_initial_state_is_stopped() {
        let rig = rig();
        let status = rig.monitor.status();
        assert!(!status.running && status.server.is_none() && status.sessions.is_empty());
        let subscription = rig.monitor.subscribe();
        assert!(subscription.status.contains(r#""running":false"#));
    }

    #[tokio::test]
    async fn no_server_means_not_running_and_sessions_are_sorted_by_name() {
        let rig = rig();
        let status = rig.monitor.poll_now().await;
        assert!(!status.running && status.server.is_none() && status.sessions.is_empty());

        rig.tmux.set(
            4242,
            vec![
                session("$2", "zeta"),
                session("$0", "alpha"),
                session("$1", "Mid"),
            ],
        );
        let status = rig.monitor.poll_now().await;
        assert!(status.running);
        let names: Vec<_> = status.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Mid", "alpha", "zeta"], "byte order, like `sort`");
        let server = status.server.expect("identity comes from the sessions");
        assert_eq!((server.pid, server.version.as_str()), (4242, "3.4"));
    }

    #[tokio::test]
    async fn a_server_without_sessions_is_running_with_no_identity() {
        let rig = rig();
        rig.tmux.set(4242, vec![]);
        let status = rig.monitor.poll_now().await;
        assert!(status.running);
        assert!(status.server.is_none() && status.sessions.is_empty());
        assert_eq!(rig.scans_taken.load(Ordering::SeqCst), 0, "nothing to scan");
    }

    #[tokio::test]
    async fn status_events_fire_on_real_changes_only_and_ignore_activity() {
        let rig = rig();
        let mut sub = rig.monitor.subscribe();
        rig.monitor.poll_now().await;
        assert!(
            drain(&mut sub.events).is_empty(),
            "still stopped: nothing changed"
        );

        rig.tmux.set(4242, vec![session("$0", "alpha")]);
        rig.monitor.poll_now().await;
        let frames = drain(&mut sub.events);
        assert_eq!(types(&frames), ["status"]);
        assert_eq!(frames[0]["running"], true);
        assert_eq!(frames[0]["sessions"][0]["name"], "alpha");

        // Only `activity` moves.
        let mut busy = session("$0", "alpha");
        busy.activity += 99;
        rig.tmux.set(4242, vec![busy]);
        let status = rig.monitor.poll_now().await;
        assert!(
            drain(&mut sub.events).is_empty(),
            "activity alone must not publish"
        );
        assert_eq!(
            status.sessions[0].activity, 1_700_000_299,
            "but the cached status is fresh"
        );

        // A rename, a window count change, a new session and a stopped server each publish.
        rig.tmux.set(4242, vec![session("$0", "beta")]);
        rig.monitor.poll_now().await;
        assert_eq!(types(&drain(&mut sub.events)), ["status"], "rename");

        let mut two_windows = session("$0", "beta");
        two_windows.windows = 2;
        rig.tmux.set(4242, vec![two_windows]);
        rig.monitor.poll_now().await;
        assert_eq!(types(&drain(&mut sub.events)), ["status"], "window count");

        rig.tmux
            .set(4242, vec![session("$0", "beta"), session("$1", "gamma")]);
        rig.monitor.poll_now().await;
        assert_eq!(types(&drain(&mut sub.events)), ["status"], "new session");

        rig.tmux.stop();
        rig.monitor.poll_now().await;
        let frames = drain(&mut sub.events);
        assert_eq!(types(&frames), ["status"], "server gone");
        assert_eq!(frames[0]["running"], false);
        assert_eq!(frames[0]["sessions"], json!([]));
    }

    #[tokio::test]
    async fn a_new_server_with_the_same_sessions_is_a_change() {
        let rig = rig();
        rig.tmux.set(1111, vec![session("$0", "alpha")]);
        rig.monitor.poll_now().await;
        let mut sub = rig.monitor.subscribe();
        rig.tmux.set(2222, vec![session("$0", "alpha")]);
        rig.monitor.poll_now().await;
        assert_eq!(types(&drain(&mut sub.events)), ["status"]);
    }

    #[tokio::test]
    async fn a_transient_tmux_error_keeps_the_previous_status() {
        let rig = rig();
        rig.tmux.set(4242, vec![session("$0", "alpha")]);
        rig.monitor.poll_now().await;
        let mut sub = rig.monitor.subscribe();
        rig.tmux.failing.store(true, Ordering::SeqCst);
        let status = rig.monitor.poll_now().await;
        assert!(status.running, "an error is not 'no server'");
        assert_eq!(status.sessions.len(), 1);
        assert!(drain(&mut sub.events).is_empty());
    }

    #[tokio::test]
    async fn refresh_reuses_a_recent_poll_and_polls_when_stale() {
        let rig = rig();
        rig.monitor.poll_now().await;
        let calls = || rig.tmux.list_calls.load(Ordering::SeqCst);
        assert_eq!(calls(), 1);
        rig.monitor.refresh(Duration::from_secs(60)).await;
        assert_eq!(calls(), 1, "recent enough");
        rig.monitor.refresh(Duration::ZERO).await;
        assert_eq!(calls(), 2, "stale");
    }

    // ---- pane scan ----------------------------------------------------------------------

    #[tokio::test]
    async fn a_scan_is_taken_at_the_first_poll_with_sessions_and_on_structure_changes() {
        let rig = rig();
        let taken = || rig.scans_taken.load(Ordering::SeqCst);

        rig.monitor.poll_now().await;
        assert_eq!(taken(), 0, "no server");

        rig.tmux.set(4242, vec![session("$0", "alpha")]);
        rig.monitor.poll_now().await;
        assert_eq!(taken(), 1, "first poll with a session");

        rig.monitor.poll_now().await;
        assert_eq!(
            taken(),
            1,
            "nothing changed and the interval has not elapsed"
        );

        let mut busy = session("$0", "alpha");
        busy.activity += 5;
        rig.tmux.set(4242, vec![busy]);
        rig.monitor.poll_now().await;
        assert_eq!(taken(), 1, "activity is not structural");

        rig.tmux.set(4242, vec![session("$0", "renamed")]);
        rig.monitor.poll_now().await;
        assert_eq!(taken(), 1, "a rename is not structural either");

        rig.tmux
            .set(4242, vec![session("$0", "renamed"), session("$1", "b")]);
        rig.monitor.poll_now().await;
        assert_eq!(taken(), 2, "a new session id");

        let mut windows = session("$0", "renamed");
        windows.windows = 3;
        rig.tmux.set(4242, vec![windows, session("$1", "b")]);
        rig.monitor.poll_now().await;
        assert_eq!(taken(), 3, "a window count change");

        rig.tmux.stop();
        rig.monitor.poll_now().await;
        assert_eq!(taken(), 3, "never while stopped");
    }

    #[tokio::test]
    async fn a_failing_scan_is_not_fatal_and_not_retried_every_poll() {
        let rig = rig();
        rig.tmux.set(4242, vec![session("$0", "alpha")]);
        *rig.next_scan.lock().unwrap() = Some(Err(TmuxError::Command("boom".into())));
        let status = rig.monitor.poll_now().await;
        assert!(status.running, "the status is published regardless");
        assert_eq!(rig.scans_taken.load(Ordering::SeqCst), 1);
        rig.monitor.poll_now().await;
        assert_eq!(
            rig.scans_taken.load(Ordering::SeqCst),
            1,
            "waits for the interval"
        );
    }

    #[tokio::test]
    async fn claude_pane_counts_come_from_the_latest_scan_per_session() {
        let rig = rig();
        let mut sub = rig.monitor.subscribe();
        *rig.next_scan.lock().unwrap() = Some(Ok(snapshot_with(4242, &[("$0", 2), ("$1", 0)])));
        rig.tmux
            .set(4242, vec![session("$0", "alpha"), session("$1", "beta")]);
        let status = rig.monitor.poll_now().await;
        let counts: Vec<_> = status
            .sessions
            .iter()
            .map(|s| (s.name.as_str(), s.claude_panes))
            .collect();
        assert_eq!(counts, [("alpha", 2), ("beta", 0)]);

        // The new counts arrive as a status change of their own.
        let frames = drain(&mut sub.events);
        let last = frames.last().unwrap();
        assert_eq!(last["type"], "status");
        assert_eq!(last["sessions"][0]["claude_panes"], 2);
    }

    #[tokio::test]
    async fn claude_counts_follow_a_rename_but_not_a_new_server() {
        let rig = rig();
        *rig.next_scan.lock().unwrap() = Some(Ok(snapshot_with(4242, &[("$0", 1)])));
        rig.tmux.set(4242, vec![session("$0", "alpha")]);
        rig.monitor.poll_now().await;
        // Same server, renamed session: the count is keyed by session id.
        rig.tmux.set(4242, vec![session("$0", "renamed")]);
        let status = rig.monitor.poll_now().await;
        assert_eq!(status.sessions[0].claude_panes, 1);
        // A different server reuses "$0" for another session: stale counts must not apply.
        rig.tmux.set(9999, vec![session("$0", "other")]);
        let status = rig.monitor.poll_now().await;
        assert_eq!(status.sessions[0].claude_panes, 0);
    }

    #[tokio::test]
    async fn blocking_reports_a_panicking_task_instead_of_propagating_it() {
        let result = blocking(|| -> u32 { panic!("boom") }).await;
        assert!(result.is_err());
        assert_eq!(blocking(|| 7).await.unwrap(), 7);
    }

    // ---- subscriptions --------------------------------------------------------------------

    #[tokio::test]
    async fn a_subscription_starts_from_the_present_and_misses_nothing_after_it() {
        let rig = rig();
        rig.tmux.set(4242, vec![session("$0", "alpha")]);
        rig.monitor.poll_now().await;
        let mut sub = rig.monitor.subscribe();
        let present: Value = serde_json::from_str(&sub.status).unwrap();
        assert_eq!(present["sessions"][0]["name"], "alpha");
        assert!(drain(&mut sub.events).is_empty(), "history is not replayed");
        rig.tmux
            .set(4242, vec![session("$0", "alpha"), session("$1", "beta")]);
        rig.monitor.poll_now().await;
        assert_eq!(types(&drain(&mut sub.events)), ["status"]);
    }

    #[tokio::test]
    async fn layout_events_reach_subscribers() {
        let rig = rig();
        let mut sub = rig.monitor.subscribe();
        rig.monitor.publish_layout(7, "client-a");
        let frames = drain(&mut sub.events);
        assert_eq!(
            frames,
            vec![json!({"type": "layout", "version": 7, "client_id": "client-a"})]
        );
    }

    #[tokio::test]
    async fn a_lagging_subscriber_can_resynchronise_from_the_current_frames() {
        let rig = rig();
        let mut sub = rig.monitor.subscribe();
        for n in 0..(EVENT_CAPACITY + 10) {
            rig.monitor.publish_layout(n as u64, "c");
        }
        assert!(matches!(
            sub.events.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        assert!(rig.monitor.current_frame().contains(r#""type":"status""#));
    }

    // ---- the polling loop -------------------------------------------------------------------

    #[tokio::test]
    async fn the_loop_keeps_the_status_current() {
        let rig = rig();
        let mut sub = rig.monitor.subscribe();
        let task = tokio::spawn(rig.monitor.clone().run());
        rig.tmux.set(4242, vec![session("$0", "alpha")]);
        let frame = tokio::time::timeout(Duration::from_secs(5), sub.events.recv())
            .await
            .expect("a status frame within five seconds")
            .unwrap();
        assert!(frame.contains(r#""type":"status""#) && frame.contains("alpha"));
        task.abort();
    }
}
