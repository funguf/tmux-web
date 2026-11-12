//! HTTP and WebSocket server and routes (owned by the server unit).
//!
//! [`run`] is the in-process entry point shared by `main` and the tests: it serves on a
//! listener the caller bound, until the shutdown future completes. [`router`] builds the axum
//! router for an [`AppState`].

use crate::assets::{self, Asset};
use crate::claude::ClaudeDetector;
use crate::config::{AuthSetup, Config, TokenSource};
use crate::layout::{LayoutError, LayoutStore};
use crate::monitor::{
    blocking, shutdown_requested, Monitor, MonitorSettings, Subscription, REFRESH_MAX_AGE,
};
use crate::pty::{self, AttachContext, AttachRegistry};
use crate::security::{self, Security};
use crate::tmux::{self, RealTmux, TmuxError, TmuxRunner};
use crate::upload::{UploadError, UploadStore, SWEEP_INTERVAL};
use crate::util;
use anyhow::Context;
use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::future::{Future, IntoFuture};
use std::io::IsTerminal;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch, Mutex as AsyncMutex, Notify};
use tokio::time::{interval, interval_at, timeout, Instant, MissedTickBehavior};

/// Body limit of every route except the upload.
pub const MAX_JSON_BODY: usize = 2 * 1024 * 1024;

/// `/ws/events` pings this often.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A single send to an events subscriber may take this long.
const EVENTS_SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// Total time a shutdown may take before connections are abandoned.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Client frames on the events socket are ignored, so none needs to be large.
const EVENTS_MAX_MESSAGE: usize = 64 * 1024;
/// Largest single client frame on the attach socket (a big paste is one frame).
const ATTACH_MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// Longest accepted `session` query value.
const MAX_SESSION_PARAM: usize = 1024;

/// `{"error":<code>,"message":<text>}` with `status`.
pub fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": code, "message": message}))).into_response()
}

/// An error answered as the JSON error body of §7.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    fn too_large(limit: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("the request body is larger than {limit} bytes"),
        )
    }

    /// `no_server` for a missing server, `tmux_error` for anything else.
    fn tmux(err: &TmuxError) -> Self {
        match err {
            TmuxError::NoServer => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_server",
                "the tmux server is not running",
            ),
            other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "tmux_error",
                other.to_string(),
            ),
        }
    }
}

/// Lets `?` turn the failure of a blocking task into an internal error.
impl From<crate::monitor::BlockingError> for ApiError {
    fn from(err: crate::monitor::BlockingError) -> Self {
        Self::internal(err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_error(self.status, self.code, &self.message)
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---- state --------------------------------------------------------------------------------

/// Counts open WebSockets so shutdown can wait for them (an upgraded connection no longer
/// counts as a connection for `axum::serve`).
#[derive(Default)]
pub struct SocketTracker {
    open: AtomicUsize,
    idle: Notify,
}

/// Held by a WebSocket task for as long as it runs.
pub struct SocketGuard(Arc<SocketTracker>);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if self.0.open.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl SocketTracker {
    pub fn enter(self: &Arc<Self>) -> SocketGuard {
        self.open.fetch_add(1, Ordering::AcqRel);
        SocketGuard(Arc::clone(self))
    }

    pub fn open(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    /// True once no socket is open, false when `limit` ran out first.
    pub async fn wait_idle(&self, limit: Duration) -> bool {
        let idle = async {
            loop {
                let notified = self.idle.notified();
                tokio::pin!(notified);
                // Register before checking, so a close between check and wait is not missed.
                notified.as_mut().enable();
                if self.open() == 0 {
                    return;
                }
                notified.await;
            }
        };
        timeout(limit, idle).await.is_ok()
    }
}

pub struct Shared {
    pub config: Config,
    pub hostname: String,
    pub runner: Arc<dyn TmuxRunner>,
    pub monitor: Monitor,
    pub layout: Arc<LayoutStore>,
    pub uploads: Arc<UploadStore>,
    pub security: Arc<Security>,
    pub attach: Arc<AttachRegistry>,
    pub sockets: Arc<SocketTracker>,
    /// Held while tmux is being started, so that two requests cannot both start it.
    pub server_start: AsyncMutex<()>,
    /// Turns `true` when the server is asked to stop.
    pub shutdown: watch::Receiver<bool>,
}

/// Everything the handlers share; cheap to clone.
#[derive(Clone)]
pub struct AppState(Arc<Shared>);

impl Deref for AppState {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.0
    }
}

impl AppState {
    /// State backed by the real tmux described by `config`.
    pub fn new(config: Config, shutdown: watch::Receiver<bool>) -> anyhow::Result<AppState> {
        let runner = Arc::new(RealTmux::new(config.tmux.clone()));
        AppState::with_runner(config, runner, shutdown)
    }

    /// State backed by any tmux runner (the tests use a scripted one).
    pub fn with_runner(
        config: Config,
        runner: Arc<dyn TmuxRunner>,
        shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<AppState> {
        util::ensure_private_dir(&config.state_dir)
            .with_context(|| format!("creating state directory {}", config.state_dir.display()))?;
        let hostname = machine_hostname();
        let layout = LayoutStore::open(&config.state_dir)
            .with_context(|| format!("opening the layout in {}", config.state_dir.display()))?;
        let uploads = UploadStore::open(
            &config.uploads_dir(),
            config.max_upload_bytes,
            config.upload_retention,
        )
        .with_context(|| format!("opening {}", config.uploads_dir().display()))?;
        let monitor = Monitor::new(
            Arc::clone(&runner),
            ClaudeDetector::new(config.claude_config_dir.clone()),
            MonitorSettings {
                poll_interval: config.poll_interval,
            },
        );
        let security = admission_policy(&config, &hostname);
        Ok(AppState(Arc::new(Shared {
            config,
            hostname,
            runner,
            monitor,
            layout: Arc::new(layout),
            uploads: Arc::new(uploads),
            security: Arc::new(security),
            attach: Arc::new(AttachRegistry::default()),
            sockets: Arc::new(SocketTracker::default()),
            server_start: AsyncMutex::new(()),
            shutdown,
        })))
    }
}

/// The request-admission policy `config` asks for: its token and allowed hosts.
fn admission_policy(config: &Config, hostname: &str) -> Security {
    Security::new(
        config.auth.token().map(str::to_string),
        &config.allow_hosts,
        hostname,
    )
}

/// The machine's host name; empty when it cannot be read.
pub fn machine_hostname() -> String {
    nix::unistd::gethostname()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---- router -------------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    let security = Arc::clone(&state.security);
    Router::new()
        .route("/api/info", get(api_info))
        .route("/api/status", get(api_status))
        .route("/api/sessions", post(create_session))
        .route("/api/sessions/rename", post(rename_session))
        .route("/api/sessions/scroll", post(scroll_session))
        .route("/api/layout", get(get_layout).put(put_layout))
        .route("/api/upload", post(upload))
        .route("/api/server/start", post(start_server))
        .route("/ws/events", get(ws_events))
        .route("/ws/attach", get(ws_attach))
        .fallback(fallback)
        .method_not_allowed_fallback(method_not_allowed)
        // Added after the routes, so the fallbacks are covered as well.
        .layer(middleware::from_fn(api_responses_are_not_cached))
        .layer(middleware::from_fn_with_state(security, security::guard))
        .with_state(state)
}

/// API answers describe live state: never let a browser or proxy reuse them.
async fn api_responses_are_not_cached(request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    if is_api {
        response
            .headers_mut()
            .entry(header::CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("no-store"));
    }
    response
}

fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

fn is_ws_path(path: &str) -> bool {
    path == "/ws" || path.starts_with("/ws/")
}

async fn method_not_allowed() -> Response {
    json_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "this method is not allowed here",
    )
}

/// Static files for GET/HEAD; JSON 404 for everything else.
async fn fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if is_api_path(path) || is_ws_path(path) {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no such endpoint");
    }
    if method != Method::GET && method != Method::HEAD {
        return method_not_allowed().await;
    }
    match assets::lookup(path) {
        Some(asset) => asset_response(asset, &headers),
        None => json_error(StatusCode::NOT_FOUND, "not_found", "no such file"),
    }
}

/// 200 with the file, or 304 when the client already has this version.
fn asset_response(asset: Asset, request_headers: &HeaderMap) -> Response {
    let not_modified = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*" || candidate.trim_start_matches("W/") == asset.etag
            })
        });
    let body = if not_modified {
        Body::empty()
    } else {
        match asset.data {
            Cow::Borrowed(bytes) => Body::from(bytes),
            Cow::Owned(bytes) => Body::from(bytes),
        }
    };
    let mut response = Response::new(body);
    *response.status_mut() = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&asset.content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&asset.etag) {
        headers.insert(header::ETAG, value);
    }
    // Embedded files change with the binary: always revalidate (cheap, thanks to the ETag).
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

// ---- request bodies -------------------------------------------------------------------------

/// Reads a request body of at most `limit` bytes.
async fn read_body(headers: &HeaderMap, body: Body, limit: usize) -> ApiResult<Vec<u8>> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if declared.is_some_and(|declared| declared > limit as u64) {
        return Err(ApiError::too_large(limit));
    }
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::bad_request("could not read the request body"))?;
        if bytes.len() + chunk.len() > limit {
            return Err(ApiError::too_large(limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn json_body<T: DeserializeOwned>(request: Request) -> ApiResult<T> {
    let (parts, body) = request.into_parts();
    let bytes = read_body(&parts.headers, body, MAX_JSON_BODY).await?;
    serde_json::from_slice(&bytes)
        .map_err(|err| ApiError::bad_request(format!("invalid JSON body: {err}")))
}

// ---- plain API --------------------------------------------------------------------------------

async fn api_info(State(app): State<AppState>) -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "hostname": app.hostname,
        "tmux_socket": app.config.tmux.describe(),
        "upload_max_bytes": app.config.max_upload_bytes,
        "auth": app.security.auth_mode(),
    }))
}

async fn api_status(State(app): State<AppState>) -> Json<crate::monitor::TmuxStatus> {
    Json(app.monitor.refresh(REFRESH_MAX_AGE).await)
}

#[derive(Deserialize)]
struct CreateSession {
    name: String,
    cwd: Option<String>,
}

/// What tmux says when a session of that name exists already.
fn is_duplicate_session(message: &str) -> bool {
    message.contains("duplicate session")
}

async fn create_session(State(app): State<AppState>, request: Request) -> ApiResult<Response> {
    let body: CreateSession = json_body(request).await?;
    tmux::validate_session_name(&body.name)
        .map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, "invalid_name", message))?;
    let cwd = body.cwd.filter(|cwd| !cwd.is_empty());
    let runner = Arc::clone(&app.runner);
    let name = body.name.clone();
    let id = blocking(move || -> ApiResult<String> {
        let cwd = cwd.as_deref().map(existing_directory).transpose()?;
        tmux::new_session(&*runner, &name, cwd.as_deref()).map_err(|err| match err {
            TmuxError::Command(message) if is_duplicate_session(&message) => ApiError::new(
                StatusCode::CONFLICT,
                "exists",
                format!("a session named {name:?} already exists"),
            ),
            other => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "tmux_error",
                other.to_string(),
            ),
        })
    })
    .await??;
    app.monitor.refresh(Duration::ZERO).await;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": id, "name": body.name})),
    )
        .into_response())
}

/// `~` and `~/…` expand to the home directory; the result must be an existing directory.
fn existing_directory(cwd: &str) -> ApiResult<PathBuf> {
    let path = match cwd.strip_prefix('~') {
        Some("") => util::home_dir(),
        Some(rest) if rest.starts_with('/') => util::home_dir().join(rest.trim_start_matches('/')),
        _ => PathBuf::from(cwd),
    };
    if !path.is_absolute() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cwd",
            "cwd must be an absolute path",
        ));
    }
    if !path.is_dir() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cwd",
            format!("{} is not a directory", path.display()),
        ));
    }
    Ok(path)
}

#[derive(Deserialize)]
struct RenameSession {
    name: String,
    new_name: String,
}

async fn rename_session(State(app): State<AppState>, request: Request) -> ApiResult<Json<Value>> {
    let body: RenameSession = json_body(request).await?;
    if body.name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_name",
            "the session name must not be empty",
        ));
    }
    tmux::validate_session_name(&body.new_name)
        .map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, "invalid_name", message))?;
    let runner = Arc::clone(&app.runner);
    let (old, new) = (body.name.clone(), body.new_name.clone());
    blocking(move || tmux::rename_session(&*runner, &old, &new))
        .await?
        .map_err(|err| match err {
            TmuxError::NoServer => ApiError::tmux(&TmuxError::NoServer),
            TmuxError::Command(message) if message.contains("can't find session") => ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no session named {:?}", body.name),
            ),
            TmuxError::Command(message) if is_duplicate_session(&message) => ApiError::new(
                StatusCode::CONFLICT,
                "exists",
                format!("a session named {:?} already exists", body.new_name),
            ),
            other => ApiError::tmux(&other),
        })?;
    app.monitor.refresh(Duration::ZERO).await;
    Ok(Json(json!({"name": body.new_name})))
}

#[derive(Deserialize)]
struct ScrollSession {
    name: String,
    lines: i32,
}

/// Most lines one scroll request may move.
const MAX_SCROLL_LINES: u32 = 1000;

async fn scroll_session(State(app): State<AppState>, request: Request) -> ApiResult<Json<Value>> {
    let body: ScrollSession = json_body(request).await?;
    if body.name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_name",
            "the session name must not be empty",
        ));
    }
    if body.lines == 0 || body.lines.unsigned_abs() > MAX_SCROLL_LINES {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_lines",
            format!("lines must be a non-zero integer of at most {MAX_SCROLL_LINES} either way"),
        ));
    }
    let runner = Arc::clone(&app.runner);
    let (name, lines) = (body.name.clone(), body.lines);
    let state = blocking(move || tmux::scroll_pane(&*runner, &name, lines))
        .await?
        .map_err(|err| match err {
            TmuxError::NoServer => ApiError::tmux(&TmuxError::NoServer),
            TmuxError::Command(message) if message.contains("can't find session") => ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no session named {:?}", body.name),
            ),
            other => ApiError::tmux(&other),
        })?;
    Ok(Json(json!({
        "in_mode": state.in_mode,
        "scroll_position": state.scroll_position,
    })))
}

async fn get_layout(State(app): State<AppState>) -> Json<crate::layout::LayoutDoc> {
    Json(app.layout.get())
}

#[derive(Deserialize)]
struct PutLayout {
    layout: Value,
    client_id: String,
}

async fn put_layout(State(app): State<AppState>, request: Request) -> ApiResult<Json<Value>> {
    let body: PutLayout = json_body(request).await?;
    let store = Arc::clone(&app.layout);
    let layout = body.layout;
    let doc = blocking(move || store.put(layout))
        .await?
        .map_err(|err| match err {
            LayoutError::TooLarge => ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "the layout is larger than 2 MiB",
            ),
            LayoutError::Io(err) => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "io_error",
                err.to_string(),
            ),
        })?;
    app.monitor.publish_layout(doc.version, &body.client_id);
    Ok(Json(
        json!({"version": doc.version, "updated_at": doc.updated_at}),
    ))
}

/// Longest accepted `name` query value of an upload, in bytes.
const MAX_UPLOAD_NAME_PARAM: usize = 1024;

async fn upload(State(app): State<AppState>, request: Request) -> ApiResult<Response> {
    let limit = usize::try_from(app.config.max_upload_bytes).unwrap_or(usize::MAX);
    let (parts, body) = request.into_parts();
    let name = Query::<Vec<(String, String)>>::try_from_uri(&parts.uri)
        .ok()
        .and_then(|Query(params)| params.into_iter().find(|(key, _)| key == "name"))
        .map(|(_, mut value)| {
            if value.len() > MAX_UPLOAD_NAME_PARAM {
                let mut end = MAX_UPLOAD_NAME_PARAM;
                while !value.is_char_boundary(end) {
                    end -= 1;
                }
                value.truncate(end);
            }
            value
        });
    let bytes = read_body(&parts.headers, body, limit).await?;
    let store = Arc::clone(&app.uploads);
    let saved = blocking(move || store.save(&bytes, name.as_deref()))
        .await?
        .map_err(|err| match err {
            UploadError::Empty => {
                ApiError::new(StatusCode::BAD_REQUEST, "empty", "the upload is empty")
            }
            UploadError::TooLarge { .. } => ApiError::too_large(limit),
            UploadError::Io(err) => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "io_error",
                err.to_string(),
            ),
        })?;
    Ok((StatusCode::CREATED, Json(saved)).into_response())
}

// ---- starting tmux ------------------------------------------------------------------------------

/// `POST /api/server/start`: makes sure tmux runs. When it does not, the default session is
/// created (a plain login shell) and the answer is `{"started":true,"session":<name>}`.
/// Answers `{"started":false}` when tmux was running already.
///
/// Only this endpoint starts tmux; the monitor never does.
async fn start_server(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    // A task of its own: a client that gives up must not leave the start half done.
    tokio::spawn(async move { start_tmux(&app).await })
        .await
        .map_err(|err| ApiError::internal(format!("start task failed: {err}")))?
        .map(Json)
}

async fn start_tmux(app: &AppState) -> ApiResult<Value> {
    // Held until the answer is ready: a second request waits here, then finds tmux running.
    let _one_start_at_a_time = app.server_start.lock().await;
    if app.monitor.refresh(Duration::ZERO).await.running {
        return Ok(json!({"started": false}));
    }
    let started = create_default_session(app).await?;
    app.monitor.refresh(Duration::ZERO).await;
    Ok(started)
}

/// Creates the default session, which starts the tmux server. A session of that name that
/// exists already means someone else started tmux in the meantime: nothing is left to do.
async fn create_default_session(app: &AppState) -> ApiResult<Value> {
    let runner = Arc::clone(&app.runner);
    let name = app.config.default_session.clone();
    let created = blocking({
        let name = name.clone();
        move || tmux::new_session(&*runner, &name, None)
    })
    .await?;
    match created {
        Ok(_) => {
            tracing::info!("started tmux with the default session");
            Ok(json!({"started": true, "session": name}))
        }
        Err(TmuxError::Command(message)) if is_duplicate_session(&message) => {
            Ok(json!({"started": false}))
        }
        Err(other) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "tmux_error",
            other.to_string(),
        )),
    }
}

// ---- WebSockets ---------------------------------------------------------------------------------

async fn ws_events(State(app): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    let guard = app.sockets.enter();
    upgrade
        .max_message_size(EVENTS_MAX_MESSAGE)
        .max_frame_size(EVENTS_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            events_session(app, socket).await;
            drop(guard);
        })
}

async fn send_text(socket: &mut WebSocket, text: &str) -> bool {
    matches!(
        timeout(EVENTS_SEND_TIMEOUT, socket.send(Message::Text(text.into()))).await,
        Ok(Ok(()))
    )
}

/// Sends a close frame and gives the client a moment to answer it.
async fn close_socket(socket: &mut WebSocket, code: u16, reason: &str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    if timeout(
        EVENTS_SEND_TIMEOUT,
        socket.send(Message::Close(Some(frame))),
    )
    .await
    .is_ok_and(|sent| sent.is_ok())
    {
        let _ = timeout(Duration::from_secs(1), async {
            while let Some(Ok(_)) = socket.recv().await {}
        })
        .await;
    }
}

/// One `/ws/events` subscriber: the present first, then every change.
async fn events_session(app: AppState, mut socket: WebSocket) {
    let mut shutdown = app.shutdown.clone();
    let Subscription { status, mut events } = app.monitor.subscribe();
    if !send_text(&mut socket, &status).await {
        return;
    }
    let mut ping = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(frame) => {
                    if !send_text(&mut socket, &frame).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Frames were dropped: start over from the present.
                    let status = app.monitor.current_frame();
                    if !send_text(&mut socket, &status).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // Whatever the client sends is ignored; this only notices it leaving.
            message = socket.recv() => match message {
                None | Some(Err(_) | Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                let sent = timeout(EVENTS_SEND_TIMEOUT, socket.send(Message::Ping(Bytes::new()))).await;
                if !matches!(sent, Ok(Ok(()))) {
                    return;
                }
            }
            () = shutdown_requested(&mut shutdown) => {
                close_socket(&mut socket, 1001, "server shutting down").await;
                return;
            }
        }
    }
}

/// `GET /ws/attach?session=<name>&cols=<n>&rows=<n>`
async fn ws_attach(State(app): State<AppState>, uri: Uri, upgrade: WebSocketUpgrade) -> Response {
    let Ok(Query(params)) = Query::<Vec<(String, String)>>::try_from_uri(&uri) else {
        return ApiError::bad_request("malformed query string").into_response();
    };
    let value = |key: &str| {
        params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let Some(session) = value("session").filter(|name| !name.is_empty()) else {
        return ApiError::bad_request("the session query parameter is required").into_response();
    };
    if session.len() > MAX_SESSION_PARAM {
        return ApiError::bad_request("the session name is too long").into_response();
    }
    let context = AttachContext {
        runner: Arc::clone(&app.runner),
        tmux: app.config.tmux.clone(),
        registry: Arc::clone(&app.attach),
        shutdown: app.shutdown.clone(),
        session: session.to_string(),
        cols: pty::parse_dimension(
            value("cols"),
            pty::MIN_COLS,
            pty::MAX_COLS,
            pty::DEFAULT_COLS,
        ),
        rows: pty::parse_dimension(
            value("rows"),
            pty::MIN_ROWS,
            pty::MAX_ROWS,
            pty::DEFAULT_ROWS,
        ),
    };
    let guard = app.sockets.enter();
    upgrade
        .max_message_size(ATTACH_MAX_MESSAGE)
        .max_frame_size(ATTACH_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            pty::run_attach(socket, context).await;
            drop(guard);
        })
}

// ---- running ------------------------------------------------------------------------------------

/// Serves on `listener` until `shutdown` completes, then stops accepting, closes the
/// WebSockets, kills the attach clients and returns — within about three seconds even when a
/// connection hangs. Logs `listening on <addr>` once the socket is serving.
pub async fn run(
    config: Config,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let state = AppState::new(config, stop_rx)?;
    serve(state, listener, stop_tx, shutdown).await
}

/// [`run`] with a prepared state; `stop_tx` is the sender behind `state.shutdown`.
pub async fn serve(
    state: AppState,
    listener: TcpListener,
    stop_tx: watch::Sender<bool>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let local = listener
        .local_addr()
        .context("reading the listen address")?;

    // One poll before the first request, so neither /api/status nor the first events frame
    // describes a server nobody has looked at yet.
    if timeout(Duration::from_secs(3), state.monitor.poll_now())
        .await
        .is_err()
    {
        tracing::warn!("the first tmux poll took too long; continuing without it");
    }
    let monitor_task = tokio::spawn(state.monitor.clone().run());
    let sweep_task = tokio::spawn(sweep_uploads(Arc::clone(&state.uploads)));

    if let AuthSetup::Token { token, source } = &state.config.auth {
        if *source != TokenSource::Explicit {
            // The token itself never goes to the log (journald keeps it); a terminal gets the
            // full sign-in link, everyone else is told where the token is stored.
            let token_file = state.config.state_dir.join(crate::config::TOKEN_FILE);
            let token_file = token_file.display();
            if *source == TokenSource::Generated {
                tracing::info!("a token is required; a new one was generated in {token_file}");
            } else {
                tracing::info!("a token is required; it is stored in {token_file}");
            }
            if std::io::stderr().is_terminal() {
                let authority = if local.ip().is_unspecified() {
                    format!("{}:{}", state.hostname, local.port())
                } else {
                    local.to_string()
                };
                eprintln!("sign in at http://{authority}/?token={token}");
            }
        }
    }
    if matches!(state.config.auth, AuthSetup::Disabled) && !crate::config::is_loopback(&local) {
        tracing::warn!(addr = %local, "serving WITHOUT authentication on a non-loopback address");
    }
    tracing::info!(addr = %local, "listening on {local}");

    let app = router(state.clone());
    let listener = listener.tap_io(|stream| {
        // Keystroke echo must not wait for Nagle's algorithm.
        let _ = stream.set_nodelay(true);
    });
    let graceful = {
        let stop_tx = stop_tx.clone();
        async move {
            shutdown.await;
            // Everything with a socket watches this.
            let _ = stop_tx.send(true);
        }
    };
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(graceful)
        .into_future();
    let mut stopping = state.shutdown.clone();
    let outcome = tokio::select! {
        result = server => result.context("serving"),
        () = async {
            shutdown_requested(&mut stopping).await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => {
            tracing::warn!("graceful shutdown timed out; dropping the remaining connections");
            Ok(())
        }
    };

    monitor_task.abort();
    sweep_task.abort();
    // Upgraded connections no longer count for `axum::serve`: reach them directly.
    state.attach.terminate_all(Duration::from_millis(500)).await;
    if !state.sockets.wait_idle(Duration::from_secs(1)).await {
        tracing::warn!(
            open = state.sockets.open(),
            "some WebSockets did not close in time"
        );
    }
    outcome
}

/// Deletes expired uploads at startup and every six hours.
async fn sweep_uploads(store: Arc<UploadStore>) {
    let mut ticker = interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // The first tick is immediate: that is the sweep at startup.
        ticker.tick().await;
        let sweeper = Arc::clone(&store);
        match blocking(move || sweeper.sweep()).await {
            Ok(Ok(0)) => {}
            Ok(Ok(removed)) => tracing::info!(removed, "removed expired uploads"),
            Ok(Err(err)) => tracing::warn!(%err, "sweeping the uploads failed"),
            Err(err) => tracing::warn!(%err, "sweeping the uploads failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Cli;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// An in-memory tmux that understands the few commands the API issues.
    #[derive(Default)]
    struct FakeTmux {
        sessions: Mutex<Option<Vec<(String, String)>>>, // None = no server
        next_id: AtomicUsize,
    }

    impl FakeTmux {
        fn with_sessions(names: &[&str]) -> Arc<Self> {
            let fake = Arc::new(FakeTmux::default());
            let list = names
                .iter()
                .map(|name| {
                    (
                        format!("${}", fake.next_id.fetch_add(1, Ordering::SeqCst)),
                        (*name).to_string(),
                    )
                })
                .collect();
            *fake.sessions.lock().unwrap() = Some(list);
            fake
        }
    }

    impl TmuxRunner for FakeTmux {
        fn run(&self, args: &[String]) -> Result<String, TmuxError> {
            let mut guard = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let arg = |n: usize| args.get(n).map(String::as_str).unwrap_or("");
            match arg(0) {
                "list-sessions" => {
                    let sessions = guard.as_ref().ok_or(TmuxError::NoServer)?;
                    Ok(sessions
                        .iter()
                        .map(|(id, name)| format!("77\t1700000000\t/tmp/fake\t3.4\t{id}\t1\t0\t1700000100\t1700000200\t0\t\t{name}\n"))
                        .collect())
                }
                "new-session" => {
                    let name = arg(3).to_string();
                    let sessions = guard.get_or_insert_with(Vec::new);
                    if sessions.iter().any(|(_, existing)| *existing == name) {
                        return Err(TmuxError::Command(format!("duplicate session: {name}")));
                    }
                    let id = format!("${}", self.next_id.fetch_add(1, Ordering::SeqCst));
                    sessions.push((id.clone(), name));
                    Ok(format!("{id}\n"))
                }
                "rename-session" => {
                    let sessions = guard.as_mut().ok_or(TmuxError::NoServer)?;
                    let old = arg(2).trim_start_matches('=').to_string();
                    let new = arg(3).to_string();
                    if sessions.iter().any(|(_, name)| *name == new && new != old) {
                        return Err(TmuxError::Command(format!("duplicate session: {new}")));
                    }
                    match sessions.iter_mut().find(|(_, name)| *name == old) {
                        Some(entry) => {
                            entry.1 = new;
                            Ok(String::new())
                        }
                        None => Err(TmuxError::Command(format!("can't find session: {old}"))),
                    }
                }
                // Snapshots (once core is real) ask for windows and panes too; a refusal is
                // only logged by the monitor, which is all these API tests need from it.
                other => Err(TmuxError::Command(format!("the fake tmux has no {other}"))),
            }
        }
    }

    struct Harness {
        app: Router,
        state: AppState,
        _stop: watch::Sender<bool>,
        dir: tempfile::TempDir,
    }

    fn harness_with(tmux: Arc<dyn TmuxRunner>, extra_args: &[&str]) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let claude_dir = dir.path().join("claude");
        let mut args = vec![
            "tmux-web".to_string(),
            "--state-dir".into(),
            state_dir.to_str().unwrap().into(),
            "--claude-config-dir".into(),
            claude_dir.to_str().unwrap().into(),
            "-L".into(),
            "tmw-srv-unit-never-used".into(),
            "serve".into(),
            "--listen".into(),
            "127.0.0.1:0".into(),
        ];
        args.extend(extra_args.iter().map(|arg| (*arg).to_string()));
        let cli = Cli::try_parse_args(args).unwrap();
        let config = Config::from_cli(&cli).unwrap();
        let (stop, stop_rx) = watch::channel(false);
        let state = AppState::with_runner(config, tmux, stop_rx).unwrap();
        Harness {
            app: router(state.clone()),
            state,
            _stop: stop,
            dir,
        }
    }

    fn harness() -> Harness {
        harness_with(FakeTmux::with_sessions(&["alpha", "beta"]), &[])
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or_else(|err| {
                panic!(
                    "not JSON ({err}): {:?}",
                    String::from_utf8_lossy(&self.body)
                )
            })
        }
    }

    async fn send(
        h: &Harness,
        method: Method,
        path: &str,
        extra: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Reply {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        let response = h
            .app
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, 64 << 20).await.unwrap().to_vec();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    async fn get_json(h: &Harness, path: &str) -> Reply {
        send(h, Method::GET, path, &[], Vec::new()).await
    }

    async fn post_json(h: &Harness, path: &str, body: Value) -> Reply {
        send(
            h,
            Method::POST,
            path,
            &[("x-tmux-web", "1"), ("content-type", "application/json")],
            body.to_string().into_bytes(),
        )
        .await
    }

    fn error_code(reply: &Reply) -> String {
        let body = reply.json();
        assert!(body["message"].is_string(), "{body}");
        body["error"].as_str().unwrap().to_string()
    }

    // ---- info / status ----------------------------------------------------------------------

    #[tokio::test]
    async fn info_describes_the_server() {
        let h = harness();
        let reply = get_json(&h, "/api/info").await;
        assert_eq!(reply.status, StatusCode::OK);
        let info = reply.json();
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(info["hostname"], machine_hostname());
        assert_eq!(info["tmux_socket"], "-L tmw-srv-unit-never-used");
        assert_eq!(info["upload_max_bytes"], 25 * 1024 * 1024);
        assert!(info.get("auto_restore").is_none() && info.get("auto_restore_on_start").is_none());
        assert_eq!(info["auth"], "none");
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(reply.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }

    #[tokio::test]
    async fn info_reflects_options() {
        let h = harness_with(
            FakeTmux::with_sessions(&[]),
            &["--token", "tok", "--max-upload-mb", "3"],
        );
        let reply = send(
            &h,
            Method::GET,
            "/api/info",
            &[("authorization", "Bearer tok")],
            Vec::new(),
        )
        .await;
        let info = reply.json();
        assert_eq!(info["auth"], "token");
        assert_eq!(info["upload_max_bytes"], 3 * 1024 * 1024);
    }

    #[tokio::test]
    async fn status_lists_sessions_sorted_and_reports_no_server() {
        let tmux = FakeTmux::with_sessions(&["zed", "alpha"]);
        let h = harness_with(tmux.clone(), &[]);
        let status = get_json(&h, "/api/status").await.json();
        assert_eq!(status["running"], true);
        assert_eq!(status["server"]["pid"], 77);
        let names: Vec<_> = status["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha", "zed"]);
        assert!(status["updated_at"].as_i64().unwrap() > 0);

        *tmux.sessions.lock().unwrap() = None;
        tokio::time::sleep(REFRESH_MAX_AGE + Duration::from_millis(50)).await;
        let status = get_json(&h, "/api/status").await.json();
        assert_eq!(status["running"], false);
        assert_eq!(status["server"], Value::Null);
        assert_eq!(status["sessions"], json!([]));
    }

    // ---- sessions -----------------------------------------------------------------------------

    #[tokio::test]
    async fn creating_a_session_returns_201_and_shows_up_in_the_status() {
        let h = harness();
        let reply = post_json(&h, "/api/sessions", json!({"name": "gamma"})).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let body = reply.json();
        assert_eq!(body["name"], "gamma");
        assert!(body["id"].as_str().unwrap().starts_with('$'));
        let status = get_json(&h, "/api/status").await.json();
        assert!(status["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "gamma"));
    }

    #[tokio::test]
    async fn creating_a_session_starts_a_server_when_there_is_none() {
        let h = harness_with(Arc::new(FakeTmux::default()), &[]);
        assert_eq!(get_json(&h, "/api/status").await.json()["running"], false);
        let reply = post_json(&h, "/api/sessions", json!({"name": "first"})).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(get_json(&h, "/api/status").await.json()["running"], true);
    }

    #[tokio::test]
    async fn creating_a_session_validates_the_name_and_the_cwd() {
        let h = harness();
        for bad in [
            "",
            "a:b",
            "a.b",
            "-x",
            " pad",
            "pad ",
            "tab\there",
            &"x".repeat(101),
        ] {
            let reply = post_json(&h, "/api/sessions", json!({"name": bad})).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad:?}");
            assert_eq!(error_code(&reply), "invalid_name", "{bad:?}");
        }
        let reply = post_json(
            &h,
            "/api/sessions",
            json!({"name": "ok", "cwd": "relative/dir"}),
        )
        .await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::BAD_REQUEST, "invalid_cwd")
        );
        let reply = post_json(
            &h,
            "/api/sessions",
            json!({"name": "ok", "cwd": "/definitely/not/here"}),
        )
        .await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::BAD_REQUEST, "invalid_cwd")
        );
        let cwd = h.dir.path().to_str().unwrap();
        let reply = post_json(&h, "/api/sessions", json!({"name": "with-cwd", "cwd": cwd})).await;
        assert_eq!(reply.status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn creating_a_duplicate_is_409_exists_and_other_failures_are_500() {
        let h = harness();
        let reply = post_json(&h, "/api/sessions", json!({"name": "alpha"})).await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::CONFLICT, "exists")
        );
        let reply = post_json(&h, "/api/sessions", json!({})).await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::BAD_REQUEST, "bad_request")
        );
        let reply = send(
            &h,
            Method::POST,
            "/api/sessions",
            &[("x-tmux-web", "1")],
            b"{not json".to_vec(),
        )
        .await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::BAD_REQUEST, "bad_request")
        );
    }

    #[tokio::test]
    async fn renaming_covers_ok_not_found_exists_invalid_and_no_server() {
        let tmux = FakeTmux::with_sessions(&["alpha", "beta"]);
        let h = harness_with(tmux.clone(), &[]);
        let reply = post_json(
            &h,
            "/api/sessions/rename",
            json!({"name": "alpha", "new_name": "omega"}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), json!({"name": "omega"}));
        let names: Vec<String> = get_json(&h, "/api/status").await.json()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            ["beta", "omega"],
            "the status reflects the rename at once"
        );

        let reply = post_json(
            &h,
            "/api/sessions/rename",
            json!({"name": "ghost", "new_name": "x"}),
        )
        .await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::NOT_FOUND, "not_found")
        );
        let reply = post_json(
            &h,
            "/api/sessions/rename",
            json!({"name": "omega", "new_name": "beta"}),
        )
        .await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::CONFLICT, "exists")
        );
        for (old, new) in [("omega", "bad:name"), ("omega", ""), ("", "fine")] {
            let reply = post_json(
                &h,
                "/api/sessions/rename",
                json!({"name": old, "new_name": new}),
            )
            .await;
            assert_eq!(
                (reply.status, error_code(&reply).as_str()),
                (StatusCode::BAD_REQUEST, "invalid_name"),
                "{old:?} -> {new:?}"
            );
        }

        *tmux.sessions.lock().unwrap() = None;
        let reply = post_json(
            &h,
            "/api/sessions/rename",
            json!({"name": "omega", "new_name": "x"}),
        )
        .await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, "no_server")
        );
    }

    // ---- starting tmux ---------------------------------------------------------------------------

    /// `POST /api/server/start` the way the page sends it: the CSRF header and no body.
    async fn start(h: &Harness) -> Reply {
        send(
            h,
            Method::POST,
            "/api/server/start",
            &[("x-tmux-web", "1")],
            Vec::new(),
        )
        .await
    }

    async fn session_names(h: &Harness) -> Vec<String> {
        get_json(h, "/api/status").await.json()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// A tmux that is not running and fails `new-session` with `message`.
    struct FailsToStart(&'static str);

    impl TmuxRunner for FailsToStart {
        fn run(&self, args: &[String]) -> Result<String, TmuxError> {
            match args.first().map(String::as_str) {
                Some("new-session") => Err(TmuxError::Command(self.0.to_string())),
                _ => Err(TmuxError::NoServer),
            }
        }
    }

    #[tokio::test]
    async fn starting_tmux_creates_the_default_session_once() {
        let h = harness_with(Arc::new(FakeTmux::default()), &[]);
        assert_eq!(get_json(&h, "/api/status").await.json()["running"], false);

        let reply = start(&h).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), json!({"started": true, "session": "main"}));
        // No poll loop runs here: the monitor knows because the start refreshed it.
        assert!(
            h.state.monitor.status().running,
            "the status is current when the answer arrives"
        );
        assert_eq!(session_names(&h).await, ["main"]);

        let again = start(&h).await;
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(again.json(), json!({"started": false}));
        assert_eq!(session_names(&h).await, ["main"]);
    }

    #[tokio::test]
    async fn starting_tmux_leaves_a_running_server_alone() {
        let h = harness();
        let reply = start(&h).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), json!({"started": false}));
        assert_eq!(session_names(&h).await, ["alpha", "beta"]);
    }

    #[tokio::test]
    async fn the_default_session_is_the_configured_one() {
        let h = harness_with(
            Arc::new(FakeTmux::default()),
            &["--default-session", "work"],
        );
        let reply = start(&h).await;
        assert_eq!(reply.json(), json!({"started": true, "session": "work"}));
        assert_eq!(session_names(&h).await, ["work"]);
    }

    #[tokio::test]
    async fn a_session_that_exists_already_means_someone_else_started_tmux() {
        let h = harness_with(Arc::new(FailsToStart("duplicate session: main")), &[]);
        let reply = start(&h).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), json!({"started": false}));
    }

    #[tokio::test]
    async fn another_tmux_failure_is_500_tmux_error_with_the_message() {
        let h = harness_with(Arc::new(FailsToStart("no space left on device")), &[]);
        let reply = start(&h).await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, "tmux_error")
        );
        let message = reply.json()["message"].as_str().unwrap().to_string();
        assert!(message.contains("no space left on device"), "{message}");
    }

    // ---- layout --------------------------------------------------------------------------------

    fn put_layout_request(layout: Value, client: &str) -> Value {
        json!({"layout": layout, "client_id": client})
    }

    #[tokio::test]
    async fn layout_round_trips_and_versions_increase() {
        let h = harness();
        let empty = get_json(&h, "/api/layout").await.json();
        assert_eq!(
            empty,
            json!({"version": 0, "updated_at": null, "layout": null})
        );

        let put = |layout, client| {
            let h = &h;
            async move {
                send(
                    h,
                    Method::PUT,
                    "/api/layout",
                    &[("x-tmux-web", "1")],
                    put_layout_request(layout, client).to_string().into_bytes(),
                )
                .await
            }
        };
        let first = put(json!({"schema": 1, "n": 1}), "client-a").await;
        assert_eq!(first.status, StatusCode::OK);
        let first = first.json();
        assert_eq!(first["version"], 1);
        assert!(first["updated_at"].as_i64().unwrap() > 0);
        let second = put(json!({"schema": 1, "n": 2}), "client-b").await.json();
        assert_eq!(second["version"], 2);

        let stored = get_json(&h, "/api/layout").await.json();
        assert_eq!(stored["version"], 2);
        assert_eq!(stored["layout"], json!({"schema": 1, "n": 2}));
        assert_eq!(stored["updated_at"], second["updated_at"]);
    }

    #[tokio::test]
    async fn layout_puts_are_announced_on_the_events_stream() {
        let h = harness();
        let mut sub = h.state.monitor.subscribe();
        let reply = send(
            &h,
            Method::PUT,
            "/api/layout",
            &[("x-tmux-web", "1")],
            put_layout_request(json!({"a": 1}), "me")
                .to_string()
                .into_bytes(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let frame = sub.events.try_recv().expect("a layout frame");
        assert_eq!(
            serde_json::from_str::<Value>(&frame).unwrap(),
            json!({"type": "layout", "version": 1, "client_id": "me"})
        );
    }

    #[tokio::test]
    async fn an_oversize_layout_is_413_and_a_malformed_one_400() {
        let h = harness();
        let huge = put_layout_request(json!({"blob": "x".repeat(3 * 1024 * 1024)}), "c");
        let reply = send(
            &h,
            Method::PUT,
            "/api/layout",
            &[("x-tmux-web", "1")],
            huge.to_string().into_bytes(),
        )
        .await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
        );
        assert_eq!(
            get_json(&h, "/api/layout").await.json()["version"],
            0,
            "nothing was stored"
        );

        // A layout whose request still fits under the 2 MiB body limit is accepted.
        let large = put_layout_request(json!({"blob": "x".repeat(MAX_JSON_BODY - 100)}), "c");
        let large = large.to_string().into_bytes();
        assert!(large.len() < MAX_JSON_BODY);
        let reply = send(
            &h,
            Method::PUT,
            "/api/layout",
            &[("x-tmux-web", "1")],
            large,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        // One byte over the body limit is not.
        let envelope = put_layout_request(json!({"blob": ""}), "c")
            .to_string()
            .len();
        let over = put_layout_request(
            json!({"blob": "x".repeat(MAX_JSON_BODY - envelope + 1)}),
            "c",
        );
        let over = over.to_string().into_bytes();
        assert_eq!(over.len(), MAX_JSON_BODY + 1);
        let reply = send(&h, Method::PUT, "/api/layout", &[("x-tmux-web", "1")], over).await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
        );

        for bad in [
            json!({"layout": {}}),
            json!({"client_id": "c"}),
            json!({"layout": {}, "client_id": 5}),
        ] {
            let reply = send(
                &h,
                Method::PUT,
                "/api/layout",
                &[("x-tmux-web", "1")],
                bad.to_string().into_bytes(),
            )
            .await;
            assert_eq!(
                (reply.status, error_code(&reply).as_str()),
                (StatusCode::BAD_REQUEST, "bad_request"),
                "{bad}"
            );
        }
    }

    // ---- upload --------------------------------------------------------------------------------

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];

    #[tokio::test]
    async fn uploading_an_image_stores_it_and_returns_the_path() {
        let h = harness();
        let reply = send(
            &h,
            Method::POST,
            "/api/upload",
            &[("x-tmux-web", "1"), ("content-type", "image/png")],
            PNG.to_vec(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let body = reply.json();
        assert_eq!(body["bytes"], PNG.len());
        assert_eq!(body["mime"], "image/png");
        let path = body["path"].as_str().unwrap();
        assert!(
            std::path::Path::new(path).is_absolute() && path.ends_with(".png"),
            "{path}"
        );
        assert!(path.starts_with(h.state.config.uploads_dir().to_str().unwrap()));
        assert_eq!(std::fs::read(path).unwrap(), PNG);
    }

    #[tokio::test]
    async fn upload_errors_are_400_and_413_and_any_file_is_accepted() {
        let h = harness_with(FakeTmux::with_sessions(&[]), &["--max-upload-mb", "1"]);
        let upload = |body: Vec<u8>, content_type: &'static str| {
            let h = &h;
            async move {
                send(
                    h,
                    Method::POST,
                    "/api/upload",
                    &[("x-tmux-web", "1"), ("content-type", content_type)],
                    body,
                )
                .await
            }
        };
        let reply = upload(Vec::new(), "image/png").await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::BAD_REQUEST, "empty")
        );
        // The declared type is only a hint: a PNG labelled as text is still an image.
        let reply = upload(PNG.to_vec(), "text/plain").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let mut big = PNG.to_vec();
        big.resize(1024 * 1024 + 1, 0);
        let reply = upload(big, "image/png").await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
        );
        let mut exact = PNG.to_vec();
        exact.resize(1024 * 1024, 0);
        assert_eq!(upload(exact, "image/png").await.status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn a_non_image_is_stored_under_its_name() {
        let h = harness();
        let post = |uri: &'static str, body: &[u8]| {
            let h = &h;
            let body = body.to_vec();
            async move {
                send(
                    h,
                    Method::POST,
                    uri,
                    &[("x-tmux-web", "1"), ("content-type", "text/plain")],
                    body,
                )
                .await
            }
        };
        let reply = post("/api/upload?name=notes.txt", b"hello file").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let body = reply.json();
        assert_eq!(body["mime"], "application/octet-stream");
        let path = body["path"].as_str().unwrap();
        assert!(path.ends_with("-notes.txt"), "{path}");
        assert_eq!(std::fs::read(path).unwrap(), b"hello file");
        // Percent-decoding and sanitising: a traversal attempt keeps only the last part.
        let reply = post("/api/upload?name=..%2F..%2Fmy%20file.txt", b"x").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert!(
            reply.json()["path"]
                .as_str()
                .unwrap()
                .ends_with("-my_file.txt"),
            "{:?}",
            reply.json()
        );
        // A malformed escape is kept literally (then sanitised); valid escapes and '+' are decoded.
        let reply = post("/api/upload?name=%zz", b"x").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert!(reply.json()["path"].as_str().unwrap().ends_with("-_zz"));
        let reply = post("/api/upload?name=a%2Bb%26c%23d.txt", b"x").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert!(reply.json()["path"]
            .as_str()
            .unwrap()
            .ends_with("-a_b_c_d.txt"));
        let reply = post("/api/upload?name=a+b.txt", b"x").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert!(reply.json()["path"].as_str().unwrap().ends_with("-a_b.txt"));
        // An SVG is just a file.
        let reply = post("/api/upload", b"<svg xmlns='http://www.w3.org/2000/svg'/>").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert!(reply.json()["path"].as_str().unwrap().ends_with("-upload"));
    }

    // ---- removed endpoints ---------------------------------------------------------------------

    #[tokio::test]
    async fn the_recovery_and_snapshot_endpoints_are_gone() {
        let h = harness();
        for path in ["/api/recovery", "/api/snapshots"] {
            let reply = get_json(&h, path).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
        }
        for path in [
            "/api/recovery/restore",
            "/api/recovery/dismiss",
            "/api/snapshot",
        ] {
            let reply = post_json(&h, path, json!({})).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    // ---- guards, headers and static files -----------------------------------------------------------

    #[tokio::test]
    async fn mutating_requests_without_the_csrf_header_are_403() {
        let h = harness();
        for (method, path) in [
            (Method::POST, "/api/sessions"),
            (Method::POST, "/api/sessions/rename"),
            (Method::PUT, "/api/layout"),
            (Method::POST, "/api/upload"),
            (Method::DELETE, "/api/anything"),
        ] {
            let reply = send(&h, method.clone(), path, &[], b"{}".to_vec()).await;
            assert_eq!(
                (reply.status, error_code(&reply).as_str()),
                (StatusCode::FORBIDDEN, "csrf"),
                "{method} {path}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_paths_and_wrong_methods_answer_json() {
        let h = harness();
        let reply = get_json(&h, "/api/nope").await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::NOT_FOUND, "not_found")
        );
        let reply = get_json(&h, "/ws/nope").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        let reply = get_json(&h, "/api/sessions").await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed")
        );
        let reply = send(
            &h,
            Method::DELETE,
            "/api/layout",
            &[("x-tmux-web", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
        let reply = get_json(&h, "/definitely-missing.js").await;
        assert_eq!(
            (reply.status, error_code(&reply).as_str()),
            (StatusCode::NOT_FOUND, "not_found")
        );
        let reply = send(
            &h,
            Method::POST,
            "/vendor/xterm/xterm.mjs",
            &[("x-tmux-web", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn static_files_are_served_with_types_etags_and_conditional_requests() {
        let h = harness();
        let reply = get_json(&h, "/vendor/xterm/xterm.mjs").await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.headers[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-cache");
        assert_eq!(reply.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(!reply.body.is_empty());
        let etag = reply.headers[header::ETAG].to_str().unwrap().to_string();

        let again = send(
            &h,
            Method::GET,
            "/vendor/xterm/xterm.mjs",
            &[("if-none-match", &etag)],
            Vec::new(),
        )
        .await;
        assert_eq!(again.status, StatusCode::NOT_MODIFIED);
        assert!(again.body.is_empty());
        let stale = send(
            &h,
            Method::GET,
            "/vendor/xterm/xterm.mjs",
            &[("if-none-match", "\"other\"")],
            Vec::new(),
        )
        .await;
        assert_eq!(stale.status, StatusCode::OK);

        let css = get_json(&h, "/vendor/xterm/xterm.css").await;
        assert_eq!(css.headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    }

    #[tokio::test]
    async fn declaration_files_and_traversal_are_404() {
        let h = harness();
        for path in [
            "/vendor/addon-fit/addon-fit.d.ts",
            "/vendor/dockview-core/types/api/component.api.d.ts",
            "/../Cargo.toml",
            "/vendor/../Cargo.toml",
            "/%2e%2e/Cargo.toml",
        ] {
            assert_eq!(
                get_json(&h, path).await.status,
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn the_host_check_applies_to_every_route() {
        let h = harness();
        let mut request = Request::builder()
            .uri("/api/info")
            .header("host", "evil.example")
            .body(Body::empty())
            .unwrap();
        request
            .headers_mut()
            .insert("x-extra", HeaderValue::from_static("1"));
        let response = h.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ---- shutdown plumbing ------------------------------------------------------------------------------

    #[tokio::test]
    async fn the_socket_tracker_waits_for_every_guard() {
        let tracker = Arc::new(SocketTracker::default());
        assert!(
            tracker.wait_idle(Duration::from_millis(10)).await,
            "nothing open"
        );
        let first = tracker.enter();
        let second = tracker.enter();
        assert_eq!(tracker.open(), 2);
        assert!(!tracker.wait_idle(Duration::from_millis(30)).await);
        let waiter = tokio::spawn({
            let tracker = Arc::clone(&tracker);
            async move { tracker.wait_idle(Duration::from_secs(5)).await }
        });
        drop(first);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "one guard is still held");
        drop(second);
        assert!(waiter.await.unwrap());
    }
}
