//! Authentication, Host/Origin checks, CSRF and security headers
//! (owned by the server unit).
//!
//! The policy lives in [`Security::decide`], a pure function of the request head, so it can be
//! tested without any HTTP machinery. [`guard`] applies it as axum middleware and adds the
//! response headers.

use crate::server::json_error;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::IpAddr;
use std::sync::Arc;

/// Name of the cookie that carries the token after the first `?token=` visit.
pub const COOKIE_NAME: &str = "tmux_web_token";

/// Header every mutating request must carry (value `1`).
pub const CSRF_HEADER: &str = "x-tmux-web";

/// Header in which a TLS-terminating proxy reports the scheme the client used.
const FORWARDED_PROTO_HEADER: &str = "x-forwarded-proto";

/// Lifetime of the token cookie: one year.
const COOKIE_MAX_AGE_SECS: u32 = 31_536_000;

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data: blob:; \
style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; object-src 'none'; \
base-uri 'none'; frame-ancestors 'none'";

/// Shown instead of the app when a page is requested without a valid token.
const LOGIN_FORM: &str = "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>tmux-web</title>\
<form method=\"get\" action=\"\" style=\"font:16px system-ui,sans-serif;margin:20vh auto;max-width:20rem\">\
<label>Access token<br><input name=\"token\" type=\"password\" autocomplete=\"current-password\" \
autofocus required style=\"width:100%;box-sizing:border-box\"></label>\
<p><button type=\"submit\">Open</button></p></form></html>";

/// How the response to a rejected request is shaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectBody {
    /// `{"error":code,"message":message}`
    Json,
    /// Nothing (WebSocket clients only look at the status).
    Empty,
    /// A tiny HTML form that GETs `?token=`.
    LoginForm,
    /// The message as plain text.
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: &'static str,
    pub body: RejectBody,
}

impl Rejection {
    fn json(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            body: RejectBody::Json,
        }
    }

    fn into_response(self) -> Response {
        match self.body {
            RejectBody::Json => {
                let mut response = json_error(self.status, self.code, self.message);
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
            }
            RejectBody::Empty => {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = self.status;
                response
            }
            RejectBody::LoginForm => (
                self.status,
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                LOGIN_FORM,
            )
                .into_response(),
            RejectBody::Text => (
                self.status,
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                self.message,
            )
                .into_response(),
        }
    }
}

/// What to do with a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// A page was opened with a valid `?token=`: store it in a cookie and drop it from the URL.
    Redirect {
        location: String,
        set_cookie: String,
    },
    Reject(Rejection),
}

/// The request-admission policy of §8.5.
#[derive(Clone, Debug)]
pub struct Security {
    /// `Some` = authentication is on.
    token: Option<String>,
    /// Lower-case `--allow-host` entries.
    allow_hosts: Vec<String>,
    /// Lower-case machine host name plus its first label.
    hostnames: Vec<String>,
}

impl Security {
    pub fn new(token: Option<String>, allow_hosts: &[String], machine_hostname: &str) -> Self {
        let machine = normalize_host(machine_hostname);
        let mut hostnames = Vec::new();
        if !machine.is_empty() {
            if let Some((short, _)) = machine.split_once('.') {
                hostnames.push(short.to_string());
            }
            hostnames.push(machine);
        }
        Self {
            token,
            allow_hosts: allow_hosts
                .iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            hostnames,
        }
    }

    pub fn auth_enabled(&self) -> bool {
        self.token.is_some()
    }

    /// Value of `auth` in `GET /api/info`.
    pub fn auth_mode(&self) -> &'static str {
        if self.auth_enabled() {
            "token"
        } else {
            "none"
        }
    }

    /// Applies the policy in this order: Host (auth off) or credentials (auth on), then the
    /// cookie hand-off of a `?token=` visit, then Origin, then the CSRF header.
    pub fn decide(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> Verdict {
        let path = uri.path();
        let is_api = path == "/api" || path.starts_with("/api/");
        let is_ws = path == "/ws" || path.starts_with("/ws/");
        let host = request_host(headers, uri);

        // The token to store in a cookie when a page was opened with a valid `?token=`.
        let token_to_store = match &self.token {
            None => {
                if !host.as_deref().is_some_and(|host| self.host_allowed(host)) {
                    return Verdict::Reject(Rejection::json(
                        StatusCode::FORBIDDEN,
                        "host_not_allowed",
                        "this Host is not allowed; use --allow-host to add it",
                    ));
                }
                None
            }
            Some(secret) => {
                // A query token is for opening a page only: on API and WebSocket requests it
                // would end up in proxy and access logs, so those need the cookie or a bearer.
                let query_allowed = *method == Method::GET && !is_api && !is_ws;
                match authenticate(secret, uri, headers, query_allowed) {
                    Credentials::Invalid => return Verdict::Reject(unauthorized(is_api, is_ws)),
                    Credentials::Valid { via_query } => via_query.then_some(secret),
                }
            }
        };

        if let Some(secret) = token_to_store {
            return Verdict::Redirect {
                location: location_without_token(uri),
                set_cookie: token_cookie(secret, headers),
            };
        }

        let mutating = is_mutating(method);
        if is_ws || mutating {
            if let Some(origin) = headers.get(header::ORIGIN) {
                let allowed = origin
                    .to_str()
                    .is_ok_and(|origin| self.origin_allowed(origin, host.as_deref()));
                if !allowed {
                    return Verdict::Reject(Rejection::json(
                        StatusCode::FORBIDDEN,
                        "bad_origin",
                        "the Origin of this request does not match the Host",
                    ));
                }
            }
        }
        if mutating && !has_csrf_header(headers) {
            return Verdict::Reject(Rejection::json(
                StatusCode::FORBIDDEN,
                "csrf",
                "mutating requests must carry the header X-Tmux-Web: 1",
            ));
        }
        Verdict::Allow
    }

    /// `localhost`, `*.localhost`, an IP literal, the machine host name or an `--allow-host`
    /// entry; the port is ignored (an entry that includes a port must match it too).
    fn host_allowed(&self, authority: &str) -> bool {
        let full = authority.trim().to_ascii_lowercase();
        let host = normalize_host(host_of(&full));
        if host.is_empty() {
            return false;
        }
        host == "localhost"
            || host.ends_with(".localhost")
            || host.parse::<IpAddr>().is_ok()
            || self.hostnames.contains(&host)
            || self
                .allow_hosts
                .iter()
                .any(|allowed| *allowed == host || *allowed == full)
    }

    /// The Origin's host[:port] must equal the Host header, or its host must be an
    /// `--allow-host` entry.
    fn origin_allowed(&self, origin: &str, host_header: Option<&str>) -> bool {
        let origin = origin.trim().to_ascii_lowercase();
        let Some((scheme, rest)) = origin.split_once("://") else {
            return false; // includes the opaque origin "null"
        };
        if scheme != "http" && scheme != "https" {
            return false;
        }
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() || authority.contains('@') {
            return false;
        }
        let host = normalize_host(host_of(authority));
        if self
            .allow_hosts
            .iter()
            .any(|allowed| *allowed == host || allowed == authority)
        {
            return true;
        }
        host_header.is_some_and(|header| header.trim().to_ascii_lowercase() == authority)
    }
}

/// The `Host` header, or the URI authority (HTTP/2 style requests).
fn request_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            uri.authority()
                .map(|authority| authority.as_str().to_string())
        })
}

/// The host part of `host[:port]`, understanding bracketed IPv6 literals. The input must be
/// lower-case already.
fn host_of(authority: &str) -> &str {
    let authority = authority.trim();
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.find(']').map_or(rest, |end| &rest[..end]);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// Lower-case, without a trailing dot.
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn has_csrf_header(headers: &HeaderMap) -> bool {
    headers
        .get(HeaderName::from_static(CSRF_HEADER))
        .is_some_and(|value| value.as_bytes() == b"1")
}

fn unauthorized(is_api: bool, is_ws: bool) -> Rejection {
    let (body, message) = if is_api {
        (RejectBody::Json, "authentication required")
    } else if is_ws {
        (RejectBody::Empty, "authentication required")
    } else {
        (RejectBody::LoginForm, "authentication required")
    };
    Rejection {
        status: StatusCode::UNAUTHORIZED,
        code: "unauthorized",
        message,
        body,
    }
}

enum Credentials {
    Invalid,
    /// `via_query` = the token was (also) supplied as `?token=`.
    Valid {
        via_query: bool,
    },
}

/// Checks bearer header, cookie and (when `query_allowed`) query. Bearer and cookie are always
/// evaluated so the time taken does not depend on which one matched; whether the query counts
/// depends on the request path and method, which are not secret.
fn authenticate(secret: &str, uri: &Uri, headers: &HeaderMap, query_allowed: bool) -> Credentials {
    let mut bearer = false;
    for value in headers.get_all(header::AUTHORIZATION) {
        if let Some(token) = value.to_str().ok().and_then(bearer_token) {
            bearer |= constant_time_eq(token.as_bytes(), secret.as_bytes());
        }
    }
    let mut cookie = false;
    for value in headers.get_all(header::COOKIE) {
        for pair in value.to_str().unwrap_or("").split(';') {
            if let Some((name, token)) = pair.trim().split_once('=') {
                if name.trim() == COOKIE_NAME {
                    cookie |= constant_time_eq(token.trim().as_bytes(), secret.as_bytes());
                }
            }
        }
    }
    let mut query = false;
    if query_allowed {
        for pair in uri.query().unwrap_or("").split('&') {
            if let Some(("token", value)) = pair.split_once('=') {
                query |= constant_time_eq(percent_decode(value).as_bytes(), secret.as_bytes());
            }
        }
    }
    if bearer || cookie || query {
        Credentials::Valid { via_query: query }
    } else {
        Credentials::Invalid
    }
}

/// `Bearer <token>` (scheme compared case-insensitively).
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

/// Compares in time that depends only on the longer length, never on where a difference is.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    std::hint::black_box(difference) == 0
}

/// Decodes `%XX` sequences; anything malformed is kept as it is.
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

/// The `Set-Cookie` value that stores `secret` after a `?token=` visit.
fn token_cookie(secret: &str, headers: &HeaderMap) -> String {
    // SameSite=Lax, not Strict: the SSO login ends in a navigation started on the identity
    // provider's site, and browsers withhold Strict cookies there, so the first page after each
    // sign-in would show the token form. Lax cookies still cross sites only on top-level
    // navigations, and state-changing requests stay guarded by the Origin and X-Tmux-Web checks.
    let mut cookie = format!(
        "{COOKIE_NAME}={secret}; Path=/; HttpOnly; SameSite=Lax; Max-Age={COOKIE_MAX_AGE_SECS}"
    );
    // A forged header can only make the cookie stricter: it then travels over HTTPS only.
    if forwarded_over_https(headers) {
        cookie.push_str("; Secure");
    }
    cookie
}

/// True when the proxy reports an HTTPS client connection: the first comma-separated value of
/// `X-Forwarded-Proto` (the client-facing hop of a chain) is `https`, ignoring case.
fn forwarded_over_https(headers: &HeaderMap) -> bool {
    headers
        .get(HeaderName::from_static(FORWARDED_PROTO_HEADER))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|scheme| scheme.trim().eq_ignore_ascii_case("https"))
}

/// The request path and query with every `token` parameter removed. The path is forced to
/// start with exactly one `/` so the result can never be read as a protocol-relative URL.
fn location_without_token(uri: &Uri) -> String {
    let path = format!("/{}", uri.path().trim_start_matches('/'));
    let kept: Vec<&str> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some("token"))
        .collect();
    if kept.is_empty() {
        path
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

/// Adds the headers every response gets, and the HTML-only ones.
fn add_security_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/html")
        });
    if is_html {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        );
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
    }
}

/// Middleware: admission policy first, then security headers on whatever comes back.
pub async fn guard(
    State(security): State<Arc<Security>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = match security.decide(request.method(), request.uri(), request.headers()) {
        Verdict::Allow => next.run(request).await,
        Verdict::Redirect {
            location,
            set_cookie,
        } => redirect_response(&location, &set_cookie),
        Verdict::Reject(rejection) => rejection.into_response(),
    };
    add_security_headers(&mut response);
    response
}

fn redirect_response(location: &str, set_cookie: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    let headers = response.headers_mut();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    if let Ok(cookie) = HeaderValue::from_str(set_cookie) {
        headers.insert(header::SET_COOKIE, cookie);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::middleware::from_fn_with_state;
    use axum::response::Html;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    const TOKEN: &str = "s3cret-token";

    fn open() -> Security {
        Security::new(None, &[], "My-host.lan")
    }

    fn with_token() -> Security {
        Security::new(Some(TOKEN.into()), &[], "my-host")
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn decide(security: &Security, method: Method, uri: &str, pairs: &[(&str, &str)]) -> Verdict {
        security.decide(&method, &uri.parse().unwrap(), &headers(pairs))
    }

    fn rejected(verdict: Verdict) -> Rejection {
        match verdict {
            Verdict::Reject(rejection) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    // ---- Host check ------------------------------------------------------------------

    #[test]
    fn host_check_accepts_loopback_names_ip_literals_and_the_machine_name() {
        let security = Security::new(None, &["Box.Example".to_string()], "My-host.lan");
        for host in [
            "localhost",
            "localhost:7690",
            "LOCALHOST:1",
            "localhost.",
            "app.localhost",
            "a.b.localhost:8080",
            "127.0.0.1",
            "127.0.0.1:7690",
            "10.1.2.3:80",
            "[::1]",
            "[::1]:7690",
            "[2001:db8::1]:443",
            "my-host.lan",
            "MY-HOST.LAN:7690",
            "my-host",
            "box.example",
            "Box.Example:9000",
        ] {
            assert_eq!(
                decide(&security, Method::GET, "/api/status", &[("host", host)]),
                Verdict::Allow,
                "{host} should be allowed"
            );
        }
    }

    #[test]
    fn host_check_rejects_everything_else_with_403_host_not_allowed() {
        let security = Security::new(None, &["box.example".to_string()], "my-host");
        for host in [
            "evil.example",
            "evil.example:7690",
            "localhost.evil.example",
            "notlocalhost",
            "127.0.0.1.evil.example",
            "box.example.evil.example",
            "my-host-2",
            "",
        ] {
            let rejection = rejected(decide(
                &security,
                Method::GET,
                "/api/status",
                &[("host", host)],
            ));
            assert_eq!(rejection.status, StatusCode::FORBIDDEN, "{host:?}");
            assert_eq!(rejection.code, "host_not_allowed", "{host:?}");
        }
    }

    #[test]
    fn a_request_without_any_host_is_rejected_when_auth_is_off() {
        let rejection = rejected(decide(&open(), Method::GET, "/api/status", &[]));
        assert_eq!(rejection.code, "host_not_allowed");
    }

    #[test]
    fn an_allow_host_entry_with_a_port_only_matches_that_port() {
        let security = Security::new(None, &["box.example:9000".to_string()], "my-host");
        assert_eq!(
            decide(&security, Method::GET, "/", &[("host", "box.example:9000")]),
            Verdict::Allow
        );
        assert_eq!(
            rejected(decide(
                &security,
                Method::GET,
                "/",
                &[("host", "box.example:1")]
            ))
            .code,
            "host_not_allowed"
        );
    }

    #[test]
    fn the_host_check_is_skipped_when_auth_is_on() {
        assert_eq!(
            decide(
                &with_token(),
                Method::GET,
                "/api/status",
                &[
                    ("host", "anything.example"),
                    ("authorization", "Bearer s3cret-token")
                ]
            ),
            Verdict::Allow
        );
    }

    // ---- authentication ----------------------------------------------------------------

    #[test]
    fn missing_credentials_are_rejected_in_the_shape_the_path_needs() {
        let security = with_token();
        let api = rejected(decide(&security, Method::GET, "/api/status", &[]));
        assert_eq!(
            (api.status, api.code, api.body),
            (StatusCode::UNAUTHORIZED, "unauthorized", RejectBody::Json)
        );
        let ws = rejected(decide(&security, Method::GET, "/ws/events", &[]));
        assert_eq!(
            (ws.status, ws.body),
            (StatusCode::UNAUTHORIZED, RejectBody::Empty)
        );
        let page = rejected(decide(&security, Method::GET, "/", &[]));
        assert_eq!(
            (page.status, page.body),
            (StatusCode::UNAUTHORIZED, RejectBody::LoginForm)
        );
        let asset = rejected(decide(
            &security,
            Method::GET,
            "/vendor/xterm/xterm.mjs",
            &[],
        ));
        assert_eq!(asset.body, RejectBody::LoginForm);
    }

    #[test]
    fn a_bearer_token_authorises_any_path_without_redirect() {
        let security = with_token();
        for path in ["/api/status", "/ws/events", "/", "/app.js"] {
            assert_eq!(
                decide(
                    &security,
                    Method::GET,
                    path,
                    &[("authorization", "Bearer s3cret-token")]
                ),
                Verdict::Allow,
                "{path}"
            );
        }
        // The scheme is case-insensitive.
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/api/status",
                &[("authorization", "bearer s3cret-token")]
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn wrong_or_malformed_credentials_are_rejected() {
        let security = with_token();
        for pairs in [
            vec![("authorization", "Bearer wrong")],
            vec![("authorization", "Bearer")],
            vec![("authorization", "Basic s3cret-token")],
            vec![("authorization", "s3cret-token")],
            vec![("authorization", "Bearer s3cret-token-and-more")],
            vec![("cookie", "tmux_web_token=wrong")],
            vec![("cookie", "other=s3cret-token")],
            vec![("cookie", "tmux_web_token=")],
        ] {
            assert_eq!(
                rejected(decide(&security, Method::GET, "/api/status", &pairs)).status,
                StatusCode::UNAUTHORIZED,
                "{pairs:?}"
            );
        }
        assert_eq!(
            rejected(decide(
                &security,
                Method::GET,
                "/api/status?token=wrong",
                &[]
            ))
            .status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn the_cookie_authorises_requests_among_other_cookies() {
        let security = with_token();
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/api/status",
                &[("cookie", "a=b; tmux_web_token=s3cret-token; c=d")]
            ),
            Verdict::Allow
        );
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/",
                &[("cookie", "a=b"), ("cookie", "tmux_web_token=s3cret-token")]
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn a_query_token_on_api_and_ws_paths_is_refused() {
        let security = with_token();
        let api = rejected(decide(
            &security,
            Method::GET,
            "/api/status?token=s3cret-token",
            &[],
        ));
        assert_eq!(
            (api.status, api.body),
            (StatusCode::UNAUTHORIZED, RejectBody::Json)
        );
        for uri in [
            "/ws/events?token=s3cret-token",
            "/ws/attach?session=a&token=s3cret-token&cols=80",
        ] {
            let ws = rejected(decide(&security, Method::GET, uri, &[]));
            assert_eq!(
                (ws.status, ws.body),
                (StatusCode::UNAUTHORIZED, RejectBody::Empty)
            );
        }
        // A page request with a query token and a non-GET method is refused too.
        let post = rejected(decide(&security, Method::POST, "/?token=s3cret-token", &[]));
        assert_eq!(post.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn a_bearer_or_cookie_still_authorises_api_and_ws_paths_that_carry_a_query_token() {
        let security = with_token();
        for uri in ["/api/status?token=wrong", "/ws/events?token=wrong"] {
            assert_eq!(
                decide(
                    &security,
                    Method::GET,
                    uri,
                    &[("authorization", "Bearer s3cret-token")]
                ),
                Verdict::Allow
            );
        }
    }

    #[test]
    fn a_query_token_on_a_page_redirects_and_sets_the_cookie() {
        let security = with_token();
        let Verdict::Redirect {
            location,
            set_cookie,
        } = decide(&security, Method::GET, "/?token=s3cret-token", &[])
        else {
            panic!("expected a redirect");
        };
        assert_eq!(location, "/");
        assert_eq!(
            set_cookie,
            "tmux_web_token=s3cret-token; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000"
        );
    }

    #[test]
    fn the_redirect_keeps_the_path_and_the_other_query_parameters() {
        let security = with_token();
        let Verdict::Redirect { location, .. } = decide(
            &security,
            Method::GET,
            "/vendor/a.mjs?x=1&token=s3cret-token&y=2",
            &[],
        ) else {
            panic!("expected a redirect");
        };
        assert_eq!(location, "/vendor/a.mjs?x=1&y=2");
    }

    #[test]
    fn the_redirect_target_can_never_leave_the_site() {
        let security = with_token();
        let Verdict::Redirect { location, .. } = decide(
            &security,
            Method::GET,
            "//evil.example/x?token=s3cret-token",
            &[],
        ) else {
            panic!("expected a redirect");
        };
        assert!(
            location.starts_with('/') && !location.starts_with("//"),
            "{location}"
        );
    }

    #[test]
    fn a_percent_encoded_query_token_is_decoded() {
        let security = with_token();
        assert!(matches!(
            decide(&security, Method::GET, "/?token=s3cret%2Dtoken", &[]),
            Verdict::Redirect { .. }
        ));
    }

    #[test]
    fn only_get_redirects() {
        let security = with_token();
        // A POST with a good query token is not authenticated at all: the query token is for
        // opening a page.
        assert_eq!(
            rejected(decide(
                &security,
                Method::POST,
                "/?token=s3cret-token",
                &[("x-tmux-web", "1")]
            ))
            .status,
            StatusCode::UNAUTHORIZED
        );
        // With a bearer it is an ordinary POST (then CSRF applies).
        assert_eq!(
            decide(
                &security,
                Method::POST,
                "/?token=s3cret-token",
                &[
                    ("x-tmux-web", "1"),
                    ("authorization", "Bearer s3cret-token")
                ]
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn a_valid_cookie_plus_a_wrong_query_token_is_allowed_without_redirect() {
        let security = with_token();
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/?token=wrong",
                &[("cookie", "tmux_web_token=s3cret-token")]
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn percent_decode_handles_edges() {
        assert_eq!(percent_decode("abc"), "abc");
        assert_eq!(percent_decode("a%41b"), "aAb");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%41"), "A");
    }

    // ---- Origin ------------------------------------------------------------------------

    #[test]
    fn origin_matching_the_host_is_accepted_on_ws_and_mutating_requests() {
        let security = open();
        let host = "127.0.0.1:7690";
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/ws/events",
                &[("host", host), ("origin", "http://127.0.0.1:7690")]
            ),
            Verdict::Allow
        );
        assert_eq!(
            decide(
                &security,
                Method::POST,
                "/api/sessions",
                &[
                    ("host", host),
                    ("origin", "http://127.0.0.1:7690"),
                    ("x-tmux-web", "1")
                ]
            ),
            Verdict::Allow
        );
        // Case-insensitive, https allowed too.
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/ws/events",
                &[
                    ("host", "Localhost:7690"),
                    ("origin", "https://LOCALHOST:7690")
                ]
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn a_foreign_origin_is_rejected_with_403_bad_origin() {
        let security = open();
        for origin in [
            "http://evil.example",
            "http://127.0.0.1:7691",
            "http://127.0.0.1",
            "https://evil.example:7690",
            "null",
            "ftp://127.0.0.1:7690",
            "http://user@127.0.0.1:7690",
            "garbage",
            "",
        ] {
            let ws = rejected(decide(
                &security,
                Method::GET,
                "/ws/attach?session=a",
                &[("host", "127.0.0.1:7690"), ("origin", origin)],
            ));
            assert_eq!(
                (ws.status, ws.code),
                (StatusCode::FORBIDDEN, "bad_origin"),
                "ws {origin:?}"
            );
            let post = rejected(decide(
                &security,
                Method::POST,
                "/api/sessions",
                &[
                    ("host", "127.0.0.1:7690"),
                    ("origin", origin),
                    ("x-tmux-web", "1"),
                ],
            ));
            assert_eq!(post.code, "bad_origin", "post {origin:?}");
        }
    }

    #[test]
    fn an_origin_on_an_allow_host_is_accepted_even_when_the_host_header_differs() {
        let security = Security::new(
            Some(TOKEN.into()),
            &["proxy.example".to_string()],
            "my-host",
        );
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/ws/events",
                &[
                    ("host", "10.0.0.5:7690"),
                    ("origin", "https://proxy.example"),
                    ("authorization", "Bearer s3cret-token")
                ]
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn the_origin_header_is_not_checked_on_ordinary_reads_or_when_absent() {
        let security = open();
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/api/status",
                &[("host", "localhost"), ("origin", "http://evil.example")]
            ),
            Verdict::Allow
        );
        assert_eq!(
            decide(
                &security,
                Method::GET,
                "/ws/events",
                &[("host", "localhost")]
            ),
            Verdict::Allow
        );
        assert_eq!(
            decide(
                &security,
                Method::POST,
                "/api/sessions",
                &[("host", "localhost"), ("x-tmux-web", "1")]
            ),
            Verdict::Allow
        );
    }

    // ---- CSRF ---------------------------------------------------------------------------

    #[test]
    fn mutating_methods_need_the_csrf_header() {
        let security = open();
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            let rejection = rejected(decide(
                &security,
                method.clone(),
                "/api/layout",
                &[("host", "localhost")],
            ));
            assert_eq!(
                (rejection.status, rejection.code),
                (StatusCode::FORBIDDEN, "csrf"),
                "{method}"
            );
            for bad in ["0", "true", "", "11"] {
                assert_eq!(
                    rejected(decide(
                        &security,
                        method.clone(),
                        "/api/layout",
                        &[("host", "localhost"), ("x-tmux-web", bad)]
                    ))
                    .code,
                    "csrf",
                    "{method} with {bad:?}"
                );
            }
            assert_eq!(
                decide(
                    &security,
                    method.clone(),
                    "/api/layout",
                    &[("host", "localhost"), ("x-tmux-web", "1")]
                ),
                Verdict::Allow,
                "{method}"
            );
        }
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert_eq!(
                decide(
                    &security,
                    method.clone(),
                    "/api/status",
                    &[("host", "localhost")]
                ),
                Verdict::Allow,
                "{method}"
            );
        }
    }

    #[test]
    fn origin_is_reported_before_csrf_and_auth_before_both() {
        let security = open();
        let rejection = rejected(decide(
            &security,
            Method::POST,
            "/api/sessions",
            &[("host", "localhost"), ("origin", "http://evil.example")],
        ));
        assert_eq!(rejection.code, "bad_origin");

        let secured = with_token();
        let rejection = rejected(decide(
            &secured,
            Method::POST,
            "/api/sessions",
            &[("origin", "http://evil.example")],
        ));
        assert_eq!(rejection.status, StatusCode::UNAUTHORIZED);
    }

    /// The `Set-Cookie` of a `?token=` visit whose request also carries `extra`.
    fn set_cookie_after_token_visit(extra: &[(&str, &str)]) -> String {
        match decide(&with_token(), Method::GET, "/?token=s3cret-token", extra) {
            Verdict::Redirect { set_cookie, .. } => set_cookie,
            other => panic!("expected a redirect, got {other:?}"),
        }
    }

    #[test]
    fn the_cookie_is_secure_when_the_proxy_reports_https() {
        for proto in [
            "https",
            "HTTPS",
            "Https",
            " https ",
            "https, http",
            "https,http",
        ] {
            assert_eq!(
                set_cookie_after_token_visit(&[("x-forwarded-proto", proto)]),
                "tmux_web_token=s3cret-token; Path=/; HttpOnly; SameSite=Lax; \
Max-Age=31536000; Secure",
                "{proto:?}"
            );
        }
    }

    #[test]
    fn the_cookie_is_not_secure_unless_the_first_forwarded_proto_is_https() {
        let plain = "tmux_web_token=s3cret-token; Path=/; HttpOnly; SameSite=Lax; \
Max-Age=31536000";
        assert_eq!(set_cookie_after_token_visit(&[]), plain);
        for proto in ["http", "HTTP", "http, https", "", "https2", "xhttps", "wss"] {
            assert_eq!(
                set_cookie_after_token_visit(&[("x-forwarded-proto", proto)]),
                plain,
                "{proto:?}"
            );
        }
    }

    // ---- middleware ----------------------------------------------------------------------

    fn app(security: Security) -> Router {
        Router::new()
            .route("/", get(|| async { Html("<p>hello</p>") }))
            .route("/api/x", get(|| async { "ok" }).post(|| async { "posted" }))
            .route("/ws/x", get(|| async { "ws" }))
            .layer(from_fn_with_state(Arc::new(security), guard))
    }

    async fn call(app: &Router, request: Request) -> Response {
        app.clone().oneshot(request).await.unwrap()
    }

    fn request(method: Method, uri: &str, pairs: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in pairs {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn html_responses_get_the_full_header_set_and_everything_gets_nosniff() {
        let app = app(open());
        let html = call(&app, request(Method::GET, "/", &[("host", "localhost")])).await;
        assert_eq!(html.status(), StatusCode::OK);
        let headers = html.headers();
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            CONTENT_SECURITY_POLICY
        );

        let text = call(
            &app,
            request(Method::GET, "/api/x", &[("host", "localhost")]),
        )
        .await;
        assert_eq!(text.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(text
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .is_none());
        assert!(text.headers().get(header::X_FRAME_OPTIONS).is_none());
    }

    #[tokio::test]
    async fn the_csp_matches_the_contract_text() {
        assert_eq!(
            CONTENT_SECURITY_POLICY,
            "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; \
script-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"
        );
    }

    #[tokio::test]
    async fn the_cookie_flow_works_end_to_end_through_the_middleware() {
        let app = app(with_token());

        // 1. A page without credentials: 401 with the login form (and its headers).
        let denied = call(&app, request(Method::GET, "/", &[])).await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(denied.headers()[header::X_FRAME_OPTIONS], "DENY");
        let form = body_text(denied).await;
        assert!(
            form.contains("name=\"token\"") && form.contains("method=\"get\""),
            "{form}"
        );

        // 2. The same page with ?token=: 302 to the clean URL plus the cookie.
        let redirect = call(&app, request(Method::GET, "/?token=s3cret-token", &[])).await;
        assert_eq!(redirect.status(), StatusCode::FOUND);
        assert_eq!(redirect.headers()[header::LOCATION], "/");
        let cookie = redirect.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            cookie.starts_with("tmux_web_token=s3cret-token;"),
            "{cookie}"
        );
        for attribute in ["Path=/", "HttpOnly", "SameSite=Lax", "Max-Age=31536000"] {
            assert!(
                cookie.contains(attribute),
                "{attribute} missing in {cookie}"
            );
        }
        // No proxy said the connection was HTTPS, so the cookie must also work over plain HTTP.
        assert!(!cookie.contains("Secure"), "{cookie}");

        // 3. The cookie alone opens the page and the API.
        let pair = cookie.split(';').next().unwrap();
        let page = call(&app, request(Method::GET, "/", &[("cookie", pair)])).await;
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(body_text(page).await, "<p>hello</p>");
        let api = call(&app, request(Method::GET, "/api/x", &[("cookie", pair)])).await;
        assert_eq!(api.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_and_ws_rejections_through_the_middleware() {
        let app = app(with_token());
        let api = call(&app, request(Method::GET, "/api/x", &[])).await;
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(api.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(api.headers()[header::CACHE_CONTROL], "no-store");
        let body: serde_json::Value = serde_json::from_str(&body_text(api).await).unwrap();
        assert_eq!(body["error"], "unauthorized");
        assert!(body["message"].is_string());

        let ws = call(&app, request(Method::GET, "/ws/x", &[])).await;
        assert_eq!(ws.status(), StatusCode::UNAUTHORIZED);
        assert!(body_text(ws).await.is_empty());

        let bearer = call(
            &app,
            request(
                Method::GET,
                "/api/x",
                &[("authorization", "Bearer s3cret-token")],
            ),
        )
        .await;
        assert_eq!(bearer.status(), StatusCode::OK);

        let query = call(&app, request(Method::GET, "/api/x?token=s3cret-token", &[])).await;
        assert_eq!(query.status(), StatusCode::UNAUTHORIZED);
        assert!(query.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn csrf_and_origin_rejections_carry_json_bodies() {
        let app = app(open());
        let csrf = call(
            &app,
            request(Method::POST, "/api/x", &[("host", "localhost")]),
        )
        .await;
        assert_eq!(csrf.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value = serde_json::from_str(&body_text(csrf).await).unwrap();
        assert_eq!(body["error"], "csrf");

        let ok = call(
            &app,
            request(
                Method::POST,
                "/api/x",
                &[("host", "localhost"), ("x-tmux-web", "1")],
            ),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);

        let origin = call(
            &app,
            request(
                Method::GET,
                "/ws/x",
                &[("host", "localhost"), ("origin", "http://evil.example")],
            ),
        )
        .await;
        assert_eq!(origin.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value = serde_json::from_str(&body_text(origin).await).unwrap();
        assert_eq!(body["error"], "bad_origin");

        let host = call(
            &app,
            request(Method::GET, "/api/x", &[("host", "evil.example")]),
        )
        .await;
        assert_eq!(host.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value = serde_json::from_str(&body_text(host).await).unwrap();
        assert_eq!(body["error"], "host_not_allowed");
    }

    #[test]
    fn auth_mode_names() {
        assert_eq!(open().auth_mode(), "none");
        assert_eq!(with_token().auth_mode(), "token");
    }
}
