//! Integration tests for `POST /api/server/start`: opening tmux-web is a request for tmux, so
//! the endpoint starts it by creating the default session, a plain login shell, and nothing
//! else ever starts tmux.
//!
//! Every tmux here lives on an isolated `-L tmw-srv-<random>` socket (see `server_common`).

mod server_common;

use serde_json::{json, Value};
use server_common::*;
use std::net::SocketAddr;
use std::time::Duration;

const START: &str = "/api/server/start";

/// Long enough for many polls (50 ms apart) to have happened.
const MANY_POLLS: Duration = Duration::from_millis(600);

/// `POST /api/server/start` the way the page sends it: the CSRF header and no body.
async fn start(server: &TestServer) -> Response {
    start_at(server.addr).await
}

async fn start_at(addr: SocketAddr) -> Response {
    http(addr, "POST", START, &[("X-Tmux-Web", "1")], b"").await
}

/// Sends `count` start requests at the same time; their answers, in no particular order.
async fn start_together(server: &TestServer, count: usize) -> Vec<Response> {
    let addr = server.addr;
    let requests: Vec<_> = (0..count).map(|_| tokio::spawn(start_at(addr))).collect();
    let mut answers = Vec::new();
    for request in requests {
        answers.push(request.await.expect("request task"));
    }
    answers
}

fn names(status: &Value) -> Vec<String> {
    sorted_strings(&Value::Array(
        status["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .map(|session| session["name"].clone())
            .collect(),
    ))
}

fn sorted_strings(array: &Value) -> Vec<String> {
    let mut strings: Vec<String> = array
        .as_array()
        .expect("an array")
        .iter()
        .map(|item| item.as_str().expect("a string").to_string())
        .collect();
    strings.sort();
    strings
}

fn started(response: &Response) -> bool {
    assert_eq!(response.status, 200, "{}", response.text());
    response.json()["started"] == true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_server_the_default_session_is_created() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;

    // Only the endpoint starts tmux: the server, polling away, leaves it down.
    tokio::time::sleep(MANY_POLLS).await;
    assert!(!tmux.server_is_running(), "tmux-web started tmux by itself");
    assert_eq!(server.get_json("/api/status").await["running"], false);

    let response = start(&server).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"started": true, "session": "main"}));
    assert_eq!(tmux.session_names(), ["main"]);

    // The status the answer came with is the current one.
    let status = server.get_json("/api/status").await;
    assert_eq!(status["running"], true);
    assert_eq!(names(&status), ["main"]);

    // A second start finds tmux running and leaves it alone.
    let again = start(&server).await;
    assert_eq!(again.status, 200, "{}", again.text());
    assert_eq!(again.json(), json!({"started": false}));
    assert_eq!(tmux.session_names(), ["main"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_leaves_a_server_that_runs_alone() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    tmux.new_session("mine");
    let server = TestServer::start(&tmux, &sandbox, &[]).await;

    assert!(!started(&start(&server).await));
    assert_eq!(tmux.session_names(), ["mine"], "no default session on top");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_session_option_names_the_session_that_is_created() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &["--default-session", "work"]).await;

    let response = start(&server).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"started": true, "session": "work"}));
    assert_eq!(tmux.session_names(), ["work"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_session_name_is_taken_literally() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let name = "mein #tmux ünï";
    let server = TestServer::start(&tmux, &sandbox, &["--default-session", name]).await;

    let response = start(&server).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"started": true, "session": name}));
    assert_eq!(tmux.session_names(), [name]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_starts_at_once_create_the_default_session_once() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;

    let answers = start_together(&server, 2).await;
    let created = answers.iter().filter(|answer| started(answer)).count();
    assert_eq!(created, 1, "exactly one request starts tmux");
    assert_eq!(tmux.session_names(), ["main"]);
}

// ---- who may ask ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_tmux_needs_the_csrf_header_and_a_post() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;

    for headers in [&[][..], &[("X-Tmux-Web", "0")][..]] {
        let response = server.http("POST", START, headers, b"").await;
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (403, "csrf"),
            "{headers:?}"
        );
    }
    let foreign = server
        .http(
            "POST",
            START,
            &[("X-Tmux-Web", "1"), ("Origin", "http://evil.example")],
            b"",
        )
        .await;
    assert_eq!(
        (foreign.status, foreign.error_code().as_str()),
        (403, "bad_origin")
    );
    // A read never starts anything.
    let read = server.get(START).await;
    assert_eq!(
        (read.status, read.error_code().as_str()),
        (405, "method_not_allowed")
    );

    tokio::time::sleep(MANY_POLLS).await;
    assert!(!tmux.server_is_running(), "a refused request started tmux");

    // The request the page sends is the one that works.
    assert!(started(&start(&server).await));
    assert_eq!(tmux.session_names(), ["main"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_tmux_is_admitted_like_every_other_mutation() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &["--token", "tok"]).await;
    let csrf = ("X-Tmux-Web", "1");
    let bearer = ("Authorization", "Bearer tok");

    for (headers, status, code) in [
        (vec![csrf], 401, "unauthorized"),
        (vec![bearer], 403, "csrf"),
    ] {
        let response = server.http("POST", START, &headers, b"").await;
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (status, code),
            "{headers:?}"
        );
    }
    tokio::time::sleep(MANY_POLLS).await;
    assert!(!tmux.server_is_running(), "a refused request started tmux");

    let response = server.http("POST", START, &[csrf, bearer], b"").await;
    assert!(started(&response));
    assert_eq!(tmux.session_names(), ["main"]);
}
