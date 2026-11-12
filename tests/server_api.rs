//! Integration tests for the HTTP and WebSocket API, against a real listener on
//! `127.0.0.1:0`, an isolated tmux server (`-L tmw-srv-<random>`) and a temp state directory.

mod server_common;

use futures_util::SinkExt;
use serde_json::{json, Value};
use server_common::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

const WITHIN: Duration = Duration::from_secs(10);

const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
];

fn names(status: &Value) -> Vec<String> {
    status["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|session| session["name"].as_str().expect("name").to_string())
        .collect()
}

fn text(value: &str) -> Message {
    Message::text(value.to_string())
}

/// Sends a command line to the shell of an attached session and waits for its output.
async fn type_and_expect(ws: &mut Ws, line: &str, expect: &str) {
    ws.send(Message::binary(format!("{line}\r").into_bytes()))
        .await
        .expect("send input");
    read_output_until(ws, expect, WITHIN).await;
}

// ---- info and status -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_describes_this_server() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &["--max-upload-mb", "3"]).await;
    let info = server.get_json("/api/info").await;
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert!(info["hostname"].is_string());
    assert_eq!(info["tmux_socket"], format!("-L {}", tmux.socket()));
    assert_eq!(info["upload_max_bytes"], 3 * 1024 * 1024);
    assert!(info.get("auto_restore").is_none() && info.get("auto_restore_on_start").is_none());
    assert_eq!(info["auth"], "none");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_is_not_running_before_any_server_and_lists_sessions_after() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;

    let status = server.get_json("/api/status").await;
    assert_eq!(status["running"], false);
    assert_eq!(status["server"], Value::Null);
    assert_eq!(status["sessions"], json!([]));
    assert!(status["updated_at"].as_i64().expect("updated_at") > 0);

    tmux.new_session("beta");
    tmux.new_session("alpha");
    let status = server
        .wait_status("both sessions", |status| names(status).len() == 2)
        .await;
    assert_eq!(status["running"], true);
    assert_eq!(names(&status), ["alpha", "beta"], "sorted by name");

    let identity = &status["server"];
    assert!(identity["pid"].as_u64().expect("pid") > 0);
    assert!(identity["start_time"].as_i64().expect("start_time") > 0);
    assert!(identity["socket_path"]
        .as_str()
        .expect("socket_path")
        .ends_with(tmux.socket()));
    assert!(!identity["version"].as_str().expect("version").is_empty());

    let alpha = &status["sessions"][0];
    assert!(alpha["id"].as_str().expect("id").starts_with('$'));
    assert_eq!(alpha["windows"], 1);
    assert_eq!(alpha["attached"], 0);
    assert!(alpha["created"].as_i64().expect("created") > 0);
    assert!(alpha["activity"].as_i64().expect("activity") >= alpha["created"].as_i64().unwrap());
    assert_eq!(alpha["group"], Value::Null);
    assert_eq!(alpha["claude_panes"], 0);
}

/// The Claude badges: a pane whose process is called `claude` counts in `/api/status`, and the
/// in-memory scan that finds it writes nothing under the state directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pane_running_claude_is_counted_in_the_status_without_writing_snapshots() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    // A stand-in for Claude Code: an idle process whose comm is `claude`.
    let bin = tempfile::tempdir().expect("temp dir");
    let fake = bin.path().join("claude");
    std::fs::copy("/bin/sleep", &fake).expect("copy sleep");
    tmux.new_session_running("busy", &format!("exec {} 600", fake.display()));
    tmux.new_session("idle");
    let pane_pid = tmux.format("busy", "#{pane_pid}");
    eventually("the pane process to be `claude`", WITHIN, || async {
        let comm = std::fs::read_to_string(format!("/proc/{pane_pid}/comm")).ok()?;
        (comm.trim() == "claude").then_some(())
    })
    .await;

    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    let status = server
        .wait_status("one Claude pane", |status| {
            status["sessions"]
                .as_array()
                .is_some_and(|sessions| sessions.iter().any(|s| s["claude_panes"] == 1))
        })
        .await;
    let counts: Vec<(&str, u64)> = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap(),
                s["claude_panes"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(counts, [("busy", 1), ("idle", 0)]);

    assert!(
        !sandbox.state_dir().join("snapshots").exists(),
        "the scan must not write a snapshots directory"
    );
    let _ = tmux.run(&["kill-session", "-t", "=busy"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_follows_the_server_going_away_and_coming_back() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    server
        .wait_status("running", |s| s["running"] == true)
        .await;
    let first_pid = server.get_json("/api/status").await["server"]["pid"].clone();

    tmux.kill_server();
    assert!(tmux.wait_until_gone(Duration::from_secs(5)));
    let status = server
        .wait_status("stopped", |s| s["running"] == false)
        .await;
    assert_eq!(status["server"], Value::Null);
    assert_eq!(status["sessions"], json!([]));

    tmux.new_session("gamma");
    let status = server
        .wait_status("running again", |s| s["running"] == true)
        .await;
    assert_eq!(names(&status), ["gamma"]);
    assert_ne!(status["server"]["pid"], first_pid, "a new server instance");
}

// ---- sessions --------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_can_be_created_with_201_400_and_409() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    // Start the isolated server first; the code under test then only talks to it.
    tmux.new_session("seed");

    let response = server
        .post_json("/api/sessions", &json!({"name": "made by api"}))
        .await;
    assert_eq!(response.status, 201, "{}", response.text());
    let body = response.json();
    assert_eq!(body["name"], "made by api");
    let id = body["id"].as_str().expect("id");
    assert!(id.starts_with('$'));
    assert_eq!(tmux.format("made by api", "#{session_id}"), id);

    // It is visible in the status straight away.
    let status = server.get_json("/api/status").await;
    assert!(names(&status).contains(&"made by api".to_string()));

    let duplicate = server
        .post_json("/api/sessions", &json!({"name": "made by api"}))
        .await;
    assert_eq!(duplicate.status, 409);
    assert_eq!(duplicate.error_code(), "exists");

    for bad in [
        "",
        "a:b",
        "a.b",
        "-lead",
        " lead",
        "trail ",
        "ctl\u{7}",
        &"x".repeat(101),
    ] {
        let response = server
            .post_json("/api/sessions", &json!({"name": bad}))
            .await;
        assert_eq!(response.status, 400, "{bad:?}");
        assert_eq!(response.error_code(), "invalid_name", "{bad:?}");
    }
    assert!(!tmux.has_session("a:b") && !tmux.has_session("a_b"));

    let directory = tempfile::tempdir().expect("temp dir");
    let cwd = directory.path().to_str().expect("utf-8 path");
    let response = server
        .post_json("/api/sessions", &json!({"name": "in-dir", "cwd": cwd}))
        .await;
    assert_eq!(response.status, 201, "{}", response.text());
    let path = tmux.format("in-dir", "#{session_path}");
    assert_eq!(
        std::fs::canonicalize(path).expect("session path"),
        std::fs::canonicalize(directory.path()).expect("dir")
    );
    let response = server
        .post_json(
            "/api/sessions",
            &json!({"name": "bad-dir", "cwd": "/no/such/directory"}),
        )
        .await;
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (400, "invalid_cwd")
    );
    assert!(!tmux.has_session("bad-dir"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_can_be_renamed_with_200_404_and_409() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    tmux.new_session("beta");

    let response = server
        .post_json(
            "/api/sessions/rename",
            &json!({"name": "alpha", "new_name": "omega"}),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"name": "omega"}));
    assert_eq!(tmux.session_names(), ["beta", "omega"]);
    assert_eq!(
        names(&server.get_json("/api/status").await),
        ["beta", "omega"]
    );

    let missing = server
        .post_json(
            "/api/sessions/rename",
            &json!({"name": "ghost", "new_name": "x"}),
        )
        .await;
    assert_eq!(
        (missing.status, missing.error_code().as_str()),
        (404, "not_found")
    );

    let taken = server
        .post_json(
            "/api/sessions/rename",
            &json!({"name": "omega", "new_name": "beta"}),
        )
        .await;
    assert_eq!((taken.status, taken.error_code().as_str()), (409, "exists"));

    for bad in ["", "a:b", "a.b", "-x"] {
        let response = server
            .post_json(
                "/api/sessions/rename",
                &json!({"name": "omega", "new_name": bad}),
            )
            .await;
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (400, "invalid_name"),
            "{bad:?}"
        );
    }
    assert_eq!(
        tmux.session_names(),
        ["beta", "omega"],
        "nothing changed by the failures"
    );

    // Renaming to the same name is not an error.
    let same = server
        .post_json(
            "/api/sessions/rename",
            &json!({"name": "omega", "new_name": "omega"}),
        )
        .await;
    assert_eq!(same.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_without_a_server_is_503_no_server() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    let response = server
        .post_json(
            "/api/sessions/rename",
            &json!({"name": "a", "new_name": "b"}),
        )
        .await;
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (503, "no_server")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_can_be_killed_with_200_404_and_400() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    tmux.new_session("beta");

    let response = server
        .post_json("/api/sessions/kill", &json!({"name": "alpha"}))
        .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"killed": "alpha"}));
    assert!(!tmux.has_session("alpha"), "tmux no longer has the session");
    assert!(tmux.has_session("beta"));
    assert_eq!(names(&server.get_json("/api/status").await), ["beta"]);

    let missing = server
        .post_json("/api/sessions/kill", &json!({"name": "alpha"}))
        .await;
    assert_eq!(
        (missing.status, missing.error_code().as_str()),
        (404, "not_found")
    );
    let empty = server
        .post_json("/api/sessions/kill", &json!({"name": ""}))
        .await;
    assert_eq!(
        (empty.status, empty.error_code().as_str()),
        (400, "invalid_name")
    );
    assert_eq!(
        tmux.session_names(),
        ["beta"],
        "the failures killed nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_scroll_through_tmux_history_without_typing_into_the_pane() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session_running("hist", "sh -c 'seq 1 300; echo END-OF-OUTPUT; sleep 600'");
    eventually("the output to reach the pane", WITHIN, || async {
        tmux.run(&["capture-pane", "-p", "-t", "=hist:"])
            .contains("END-OF-OUTPUT")
            .then_some(())
    })
    .await;
    let scroll = |lines: i64| {
        let server = &server;
        async move {
            server
                .post_json(
                    "/api/sessions/scroll",
                    &json!({"name": "hist", "lines": lines}),
                )
                .await
        }
    };

    // Not in a mode: scrolling down does nothing, and nothing reaches the program.
    let visible = tmux.run(&["capture-pane", "-p", "-t", "=hist:"]);
    let idle = scroll(3).await;
    assert_eq!(idle.status, 200, "{}", idle.text());
    assert_eq!(idle.json(), json!({"in_mode": false, "scroll_position": 0}));
    assert_eq!(tmux.format("hist", "#{pane_in_mode}"), "0");
    assert_eq!(tmux.run(&["capture-pane", "-p", "-t", "=hist:"]), visible);

    // Up enters copy mode, calling it again scrolls further.
    let up = scroll(-5).await;
    assert_eq!(up.status, 200, "{}", up.text());
    assert_eq!(up.json(), json!({"in_mode": true, "scroll_position": 5}));
    assert_eq!(
        tmux.format("hist", "#{pane_in_mode} #{scroll_position}"),
        "1 5"
    );
    assert_eq!(scroll(-2).await.json()["scroll_position"], 7);

    // Down goes back, and reaching the bottom leaves copy mode again.
    assert_eq!(scroll(3).await.json()["scroll_position"], 4);
    let bottom = scroll(50).await;
    assert_eq!(
        bottom.json(),
        json!({"in_mode": false, "scroll_position": 0})
    );
    assert_eq!(tmux.format("hist", "#{pane_in_mode}"), "0");
    assert_eq!(tmux.run(&["capture-pane", "-p", "-t", "=hist:"]), visible);

    // Errors.
    let ghost = server
        .post_json(
            "/api/sessions/scroll",
            &json!({"name": "ghost", "lines": -1}),
        )
        .await;
    assert_eq!(
        (ghost.status, ghost.error_code().as_str()),
        (404, "not_found")
    );
    let ghost_down = server
        .post_json(
            "/api/sessions/scroll",
            &json!({"name": "ghost", "lines": 1}),
        )
        .await;
    assert_eq!(ghost_down.status, 404);
    for lines in [0, 1001, -1001, i64::from(i32::MIN), i64::from(i32::MAX)] {
        let response = scroll(lines).await;
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (400, "invalid_lines"),
            "{lines}"
        );
    }
    let nameless = server
        .post_json("/api/sessions/scroll", &json!({"name": "", "lines": -1}))
        .await;
    assert_eq!(
        (nameless.status, nameless.error_code().as_str()),
        (400, "invalid_name")
    );
    let fractional = server
        .post_json(
            "/api/sessions/scroll",
            &json!({"name": "hist", "lines": 1.5}),
        )
        .await;
    assert_eq!(fractional.status, 400);
    assert_eq!(tmux.format("hist", "#{pane_in_mode}"), "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scrolling_without_a_server_is_503_no_server() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    let response = server
        .post_json("/api/sessions/scroll", &json!({"name": "a", "lines": -1}))
        .await;
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (503, "no_server")
    );
}

// ---- events stream ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_events_stream_sends_status_on_connect_and_then_changes() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    server.wait_status("alpha", |s| names(s) == ["alpha"]).await;

    let mut events = server.ws("/ws/events").await;
    let Frame::Text(status) = next_frame(&mut events, WITHIN).await else {
        panic!("the first frame is not text");
    };
    assert_eq!(status["type"], "status");
    assert_eq!(status["running"], true);
    assert_eq!(names(&status), ["alpha"]);

    // A session created through the API shows up as a new status frame.
    let created = server
        .post_json("/api/sessions", &json!({"name": "beta"}))
        .await;
    assert_eq!(created.status, 201);
    loop {
        let update = next_event(&mut events, "status", WITHIN).await;
        if names(&update) == ["alpha", "beta"] {
            break;
        }
    }

    // So does one created behind the server's back.
    tmux.new_session("gamma");
    loop {
        let update = next_event(&mut events, "status", WITHIN).await;
        if names(&update).contains(&"gamma".to_string()) {
            break;
        }
    }

    // And a saved layout is announced with its version and the client that saved it.
    let saved = server
        .send_json(
            "PUT",
            "/api/layout",
            &json!({"layout": {"panels": []}, "client_id": "tab-7"}),
        )
        .await;
    assert_eq!(saved.status, 200);
    let layout = next_event(&mut events, "layout", WITHIN).await;
    assert_eq!(
        layout,
        json!({"type": "layout", "version": 1, "client_id": "tab-7"})
    );

    // The server going away is a status change too.
    tmux.kill_server();
    loop {
        let update = next_event(&mut events, "status", WITHIN).await;
        if update["running"] == false {
            assert_eq!(update["sessions"], json!([]));
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_frames_on_the_events_stream_are_ignored() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    let mut events = server.ws("/ws/events").await;
    next_event(&mut events, "status", WITHIN).await;
    for noise in ["hello", "{\"type\":\"status\"}", "[]"] {
        events.send(text(noise)).await.expect("send");
    }
    events
        .send(Message::binary(vec![1, 2, 3]))
        .await
        .expect("send");
    // Still alive: a change is still delivered.
    server
        .post_json("/api/sessions", &json!({"name": "after-noise"}))
        .await;
    loop {
        let update = next_event(&mut events, "status", WITHIN).await;
        if names(&update).contains(&"after-noise".to_string()) {
            break;
        }
    }
}

// ---- attach ----------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_runs_commands_in_the_session_and_follows_resizes() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");

    let mut ws = server.ws("/ws/attach?session=alpha&cols=100&rows=30").await;
    type_and_expect(&mut ws, "echo tmw-$((40+2))", "tmw-42").await;
    assert_eq!(attach_client_pids(tmux.socket(), "alpha").len(), 1);
    assert_eq!(tmux.format("alpha", "#{session_attached}"), "1");
    assert_eq!(tmux.format("alpha", "#{window_width}"), "100");

    // A resize frame changes the window the pane lives in.
    ws.send(text(r#"{"type":"resize","cols":80,"rows":24}"#))
        .await
        .expect("resize");
    eventually("a window 80 columns wide", WITHIN, || async {
        (tmux.format("alpha", "#{window_width}") == "80").then_some(())
    })
    .await;
    type_and_expect(&mut ws, "echo tmw-$((50+5))", "tmw-55").await;

    // Text `input` frames are typed like binary ones.
    ws.send(text(r#"{"type":"input","data":"echo tmw-$((60+6))\r"}"#))
        .await
        .expect("input");
    read_output_until(&mut ws, "tmw-66", WITHIN).await;

    // Garbage text frames are ignored and the terminal keeps working.
    ws.send(text("not json")).await.expect("noise");
    ws.send(text(r#"{"type":"nonsense"}"#))
        .await
        .expect("noise");
    type_and_expect(&mut ws, "echo tmw-$((70+7))", "tmw-77").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_dimensions_are_clamped_and_default_when_unusable() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    tmux.new_session("beta");

    let width = |session: &str| tmux.format(session, "#{window_width}");

    let mut huge = server
        .ws("/ws/attach?session=alpha&cols=5000&rows=99999")
        .await;
    type_and_expect(&mut huge, "echo ready-$((1+1))", "ready-2").await;
    assert_eq!(width("alpha"), "1000");
    assert_eq!(
        tmux.format("alpha", "#{window_height}"),
        "499",
        "500 rows minus the status line"
    );

    let mut default = server.ws("/ws/attach?session=beta&cols=abc").await;
    type_and_expect(&mut default, "echo ready-$((2+2))", "ready-4").await;
    assert_eq!(width("beta"), "80");
    assert_eq!(
        tmux.format("beta", "#{window_height}"),
        "23",
        "24 rows minus the status line"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_output_is_delivered_completely() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    let mut ws = server.ws("/ws/attach?session=alpha&cols=120&rows=40").await;
    type_and_expect(&mut ws, "echo ready-$((1+1))", "ready-2").await;
    // Hundreds of kilobytes through tmux, the PTY, the bounded queue and the socket.
    type_and_expect(
        &mut ws,
        "seq 1 60000; echo flood-done-$((3+4))",
        "flood-done-7",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_can_share_a_session_and_leave_independently() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    let mut first = server.ws("/ws/attach?session=alpha").await;
    let mut second = server.ws("/ws/attach?session=alpha").await;
    type_and_expect(&mut first, "echo shared-$((8+1))", "shared-9").await;
    read_output_until(&mut second, "shared-9", WITHIN).await;
    assert_eq!(attach_client_pids(tmux.socket(), "alpha").len(), 2);

    drop(first);
    eventually("one client left", WITHIN, || async {
        (attach_client_pids(tmux.socket(), "alpha").len() == 1).then_some(())
    })
    .await;
    type_and_expect(&mut second, "echo still-$((9+1))", "still-10").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_names_with_spaces_and_unicode_attach_by_exact_name() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("my work \u{e9}\u{1f600}");
    tmux.new_session("my");
    // Percent-encoded exactly as `encodeURIComponent` would.
    let mut ws = server
        .ws("/ws/attach?session=my%20work%20%C3%A9%F0%9F%98%80")
        .await;
    type_and_expect(&mut ws, "echo name-$((1+2))", "name-3").await;
    assert_eq!(
        tmux.format("my work \u{e9}\u{1f600}", "#{session_attached}"),
        "1"
    );
    assert_eq!(
        tmux.format("my", "#{session_attached}"),
        "0",
        "a prefix of the name is another session"
    );
    // `+` is a space in a query string, so a literal plus must arrive as %2B.
    tmux.new_session("a+b");
    let mut plus = server.ws("/ws/attach?session=a%2Bb").await;
    type_and_expect(&mut plus, "echo plus-$((2+2))", "plus-4").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_session_gives_no_session_and_close_code_4404() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    for missing in ["nope", "alp", "alpha2"] {
        let mut ws = server.ws(&format!("/ws/attach?session={missing}")).await;
        let exit = read_until_closed(&mut ws, WITHIN).await;
        assert_eq!(exit.reason.as_deref(), Some("no-session"), "{missing}");
        assert_eq!(exit.close_code, Some(4404), "{missing}");
        assert!(exit.message.is_some_and(|m| !m.is_empty()));
    }
    assert!(attach_client_pids(tmux.socket(), "nope").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_tmux_server_attach_gives_no_server_and_close_code_4503() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    let mut ws = server.ws("/ws/attach?session=alpha").await;
    let exit = read_until_closed(&mut ws, WITHIN).await;
    assert_eq!(exit.reason.as_deref(), Some("no-server"));
    assert_eq!(exit.close_code, Some(4503));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_without_a_session_parameter_is_refused_before_the_upgrade() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    for path in ["/ws/attach", "/ws/attach?session=", "/ws/attach?cols=80"] {
        match ws_connect(server.addr, path, &[]).await {
            Err(WsError::Http(response)) => assert_eq!(response.status(), 400, "{path}"),
            other => panic!("{path}: expected an HTTP 400, got {:?}", other.map(|_| ())),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_the_session_gives_session_closed() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    tmux.new_session("bystander");
    let mut ws = server.ws("/ws/attach?session=alpha").await;
    type_and_expect(&mut ws, "echo up-$((5+5))", "up-10").await;
    tmux.run(&["kill-session", "-t", "=alpha"]);
    let exit = read_until_closed(&mut ws, WITHIN).await;
    assert_eq!(exit.reason.as_deref(), Some("session-closed"), "{exit:?}");
    assert_eq!(exit.close_code, Some(1000));
    assert!(tmux.has_session("bystander"));
    assert!(attach_client_pids(tmux.socket(), "alpha").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_into_a_dying_terminal_still_ends_with_the_exit_frame() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    tmux.new_session("bystander");
    let mut ws = server.ws("/ws/attach?session=alpha").await;
    type_and_expect(&mut ws, "echo up-$((9+9))", "up-18").await;
    tmux.run(&["kill-session", "-t", "=alpha"]);

    // Keep typing while the terminal goes away: input that can no longer be delivered must
    // not turn the end of the session into an unannounced disconnect.
    let mut reason = None;
    let mut close_code = None;
    let deadline = std::time::Instant::now() + WITHIN;
    while close_code.is_none() && std::time::Instant::now() < deadline {
        let _ = ws.send(Message::binary(b"x".to_vec())).await;
        match tokio::time::timeout(
            Duration::from_millis(5),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        {
            Ok(Some(Ok(Message::Text(frame)))) => {
                let frame: Value = serde_json::from_str(frame.as_str()).expect("json");
                assert_eq!(frame["type"], "exit");
                reason = frame["reason"].as_str().map(str::to_string);
            }
            Ok(Some(Ok(Message::Close(frame)))) => {
                close_code = Some(frame.map_or(0, |f| u16::from(f.code)));
            }
            Ok(None | Some(Err(_))) => break,
            Ok(Some(Ok(_))) | Err(_) => {}
        }
    }
    assert_eq!(reason.as_deref(), Some("session-closed"));
    assert_eq!(close_code, Some(1000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_the_server_gives_server_down() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    let mut ws = server.ws("/ws/attach?session=alpha").await;
    type_and_expect(&mut ws, "echo up-$((6+6))", "up-12").await;
    tmux.kill_server();
    let exit = read_until_closed(&mut ws, WITHIN).await;
    assert_eq!(exit.reason.as_deref(), Some("server-down"), "{exit:?}");
    assert_eq!(exit.close_code, Some(1000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detaching_gives_detached_and_leaves_the_session_alone() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    let mut ws = server.ws("/ws/attach?session=alpha").await;
    type_and_expect(&mut ws, "echo up-$((7+7))", "up-14").await;
    tmux.run(&["detach-client", "-s", "=alpha"]);
    let exit = read_until_closed(&mut ws, WITHIN).await;
    assert_eq!(exit.reason.as_deref(), Some("detached"), "{exit:?}");
    assert_eq!(exit.close_code, Some(1000));
    assert!(tmux.has_session("alpha"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_renamed_while_attached_still_detaches_cleanly() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    let mut ws = server.ws("/ws/attach?session=alpha").await;
    type_and_expect(&mut ws, "echo up-$((8+8))", "up-16").await;
    tmux.run(&["rename-session", "-t", "=alpha", "renamed"]);
    // The terminal keeps working under the new name.
    type_and_expect(&mut ws, "echo after-rename-$((1+0))", "after-rename-1").await;
    tmux.run(&["detach-client", "-s", "=renamed"]);
    let exit = read_until_closed(&mut ws, WITHIN).await;
    assert_eq!(exit.reason.as_deref(), Some("detached"), "{exit:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_reaps_the_attach_client_and_types_nothing_into_the_pane() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    // `cat` dies on a Ctrl-D at the start of a line: if closing the socket typed Enter and
    // Ctrl-D into the terminal, the pane (and with it the session) would be gone.
    for (session, graceful) in [("cat-clean", true), ("cat-abrupt", false)] {
        tmux.new_session_running(session, "cat");
        let mut ws = server.ws(&format!("/ws/attach?session={session}")).await;
        // Any output at all means the attach client is up.
        assert!(matches!(
            next_frame(&mut ws, WITHIN).await,
            Frame::Binary(_)
        ));
        let pids = attach_client_pids(tmux.socket(), session);
        assert_eq!(pids.len(), 1, "{session}: {pids:?}");

        if graceful {
            ws.close(None).await.expect("close");
        }
        drop(ws);

        // Gone from /proc altogether: killed *and* reaped, not left as a zombie.
        eventually("the attach client to be reaped", WITHIN, || async {
            (!process_exists(pids[0])).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            tmux.has_session(session),
            "{session}: the pane received a stray Ctrl-D"
        );
        assert_eq!(tmux.format(session, "#{pane_dead}"), "0");
    }
}

// ---- security --------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutating_requests_need_the_csrf_header() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("seed");
    let response = server
        .http(
            "POST",
            "/api/sessions",
            &[("Content-Type", "application/json")],
            br#"{"name":"sneaky"}"#,
        )
        .await;
    assert_eq!(response.status, 403);
    assert_eq!(response.error_code(), "csrf");
    assert!(!tmux.has_session("sneaky"));

    // The header must say 1.
    let response = server
        .http(
            "POST",
            "/api/sessions",
            &[("X-Tmux-Web", "0")],
            br#"{"name":"sneaky"}"#,
        )
        .await;
    assert_eq!(response.status, 403);

    for (method, path) in [
        ("PUT", "/api/layout"),
        ("POST", "/api/upload"),
        ("POST", "/api/sessions/rename"),
        ("POST", "/api/sessions/scroll"),
    ] {
        assert_eq!(
            server.http(method, path, &[], b"{}").await.status,
            403,
            "{method} {path}"
        );
    }
    assert_eq!(
        server.get("/api/status").await.status,
        200,
        "reads need no header"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_origins_are_refused_on_websockets_and_mutations() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");

    for path in ["/ws/events", "/ws/attach?session=alpha"] {
        match ws_connect(server.addr, path, &[("Origin", "http://evil.example")]).await {
            Err(WsError::Http(response)) => assert_eq!(response.status(), 403, "{path}"),
            other => panic!("{path}: expected an HTTP 403, got {:?}", other.map(|_| ())),
        }
    }
    assert!(attach_client_pids(tmux.socket(), "alpha").is_empty());

    // The same origin as the Host header is fine.
    let own_origin = format!("http://{}", server.addr);
    let mut ws = ws_connect(
        server.addr,
        "/ws/events",
        &[("Origin", own_origin.as_str())],
    )
    .await
    .expect("same-origin upgrade");
    next_event(&mut ws, "status", WITHIN).await;

    let response = server
        .http(
            "POST",
            "/api/sessions",
            &[("X-Tmux-Web", "1"), ("Origin", "http://evil.example")],
            br#"{"name":"cross-site"}"#,
        )
        .await;
    assert_eq!(response.status, 403);
    assert_eq!(response.error_code(), "bad_origin");
    assert!(!tmux.has_session("cross-site"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_host_header_is_refused_unless_allowed() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &["--allow-host", "box.example"]).await;
    let refused = server
        .http("GET", "/api/status", &[("Host", "evil.example")], b"")
        .await;
    assert_eq!(refused.status, 403);
    assert_eq!(refused.error_code(), "host_not_allowed");
    for host in [
        "localhost",
        "127.0.0.1:1",
        "app.localhost",
        "[::1]",
        "box.example",
        "BOX.example:8080",
    ] {
        assert_eq!(
            server
                .http("GET", "/api/status", &[("Host", host)], b"")
                .await
                .status,
            200,
            "{host}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_authentication_covers_bearer_cookie_and_query() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &["--token", "s3cret-token"]).await;
    tmux.new_session("alpha");

    // Nothing gets through without the token, in the shape each kind of path needs.
    let api = server.get("/api/status").await;
    assert_eq!(api.status, 401);
    assert_eq!(api.error_code(), "unauthorized");
    let page = server.get("/").await;
    assert_eq!(page.status, 401);
    assert!(page
        .header("content-type")
        .is_some_and(|t| t.starts_with("text/html")));
    assert!(
        page.text().contains("name=\"token\""),
        "a form that GETs ?token="
    );
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    assert!(page
        .header("content-security-policy")
        .is_some_and(|csp| csp.contains("default-src 'self'")));
    assert_eq!(server.get("/vendor/xterm/xterm.mjs").await.status, 401);
    match ws_connect(server.addr, "/ws/events", &[]).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 401),
        other => panic!("expected 401, got {:?}", other.map(|_| ())),
    }
    assert_eq!(server.get("/api/status?token=wrong").await.status, 401);

    // Bearer header.
    let bearer = server
        .http(
            "GET",
            "/api/info",
            &[("Authorization", "Bearer s3cret-token")],
            b"",
        )
        .await;
    assert_eq!(bearer.status, 200);
    assert_eq!(bearer.json()["auth"], "token");

    // A query token is refused on the API (it would end up in access logs)…
    let query = server.get("/api/status?token=s3cret-token").await;
    assert_eq!(query.status, 401);
    assert!(query.header("set-cookie").is_none());

    // …on a page it becomes a cookie and a redirect to the clean URL.
    let redirect = server
        .get("/vendor/xterm/xterm.css?v=2&token=s3cret-token")
        .await;
    assert_eq!(redirect.status, 302);
    assert_eq!(
        redirect.header("location"),
        Some("/vendor/xterm/xterm.css?v=2")
    );
    let cookie = redirect.header("set-cookie").expect("cookie").to_string();
    for attribute in [
        "tmux_web_token=s3cret-token",
        "Path=/",
        "HttpOnly",
        "SameSite=Lax",
        "Max-Age=31536000",
    ] {
        assert!(
            cookie.contains(attribute),
            "{attribute} missing from {cookie}"
        );
    }
    let pair = cookie.split(';').next().expect("cookie pair").to_string();

    // The cookie opens everything, WebSockets included.
    let page = server
        .http(
            "GET",
            "/vendor/xterm/xterm.css",
            &[("Cookie", pair.as_str())],
            b"",
        )
        .await;
    assert_eq!(page.status, 200);
    let mut events = ws_connect(server.addr, "/ws/events", &[("Cookie", pair.as_str())])
        .await
        .expect("cookie-authenticated upgrade");
    next_event(&mut events, "status", WITHIN).await;
    // …and on WebSocket upgrades, which need the cookie or a bearer.
    match ws_connect(
        server.addr,
        "/ws/attach?session=alpha&token=s3cret-token",
        &[],
    )
    .await
    {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 401),
        other => panic!("expected 401, got {:?}", other.map(|_| ())),
    }
    let mut by_cookie = ws_connect(
        server.addr,
        "/ws/attach?session=alpha",
        &[("Cookie", pair.as_str())],
    )
    .await
    .expect("cookie-authenticated upgrade");
    type_and_expect(&mut by_cookie, "echo auth-$((2+2))", "auth-4").await;

    // Mutations need the token and the CSRF header.
    let created = server
        .http(
            "POST",
            "/api/sessions",
            &[
                ("Authorization", "Bearer s3cret-token"),
                ("X-Tmux-Web", "1"),
            ],
            br#"{"name":"via-bearer"}"#,
        )
        .await;
    assert_eq!(created.status, 201);
    let no_token = server
        .post_json("/api/sessions", &json!({"name": "no-token"}))
        .await;
    assert_eq!(no_token.status, 401);
    assert!(!tmux.has_session("no-token"));

    // Scrolling is a mutation like the others: no token, no scroll.
    let scroll = json!({"name": "via-bearer", "lines": -3});
    assert_eq!(
        server
            .post_json("/api/sessions/scroll", &scroll)
            .await
            .status,
        401
    );
    assert_eq!(tmux.format("via-bearer", "#{pane_in_mode}"), "0");
    let scrolled = server
        .http(
            "POST",
            "/api/sessions/scroll",
            &[
                ("Authorization", "Bearer s3cret-token"),
                ("X-Tmux-Web", "1"),
            ],
            scroll.to_string().as_bytes(),
        )
        .await;
    assert_eq!(scrolled.status, 200, "{}", scrolled.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_loopback_listen_address_gets_a_generated_token_in_the_state_dir() {
    // Config only: nothing is bound to the LAN address.
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let mut args = TestServer::base_args(&tmux, &sandbox);
    args.extend(["serve", "--listen", "192.0.2.1:7690"].map(String::from));
    let cli = tmux_web::config::Cli::try_parse_args(args).expect("arguments");
    let config = tmux_web::config::Config::from_cli(&cli).expect("configuration");
    let token = config
        .auth
        .token()
        .expect("auth is required off loopback")
        .to_string();
    let file = sandbox.state_dir().join("token");
    assert_eq!(std::fs::read_to_string(&file).expect("token file"), token);
    assert_eq!(token.len(), 64);
    assert_eq!(
        std::fs::metadata(&file).expect("stat").permissions().mode() & 0o777,
        0o600
    );
}

// ---- upload -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uploaded_image_round_trips_to_a_private_file() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;

    let mut image = PNG.to_vec();
    image.extend((0..=255u8).cycle().take(5000));
    let response = server
        .http(
            "POST",
            "/api/upload",
            &[("X-Tmux-Web", "1"), ("Content-Type", "image/png")],
            &image,
        )
        .await;
    assert_eq!(response.status, 201, "{}", response.text());
    let body = response.json();
    assert_eq!(body["bytes"], image.len());
    assert_eq!(body["mime"], "image/png");

    let path = std::path::PathBuf::from(body["path"].as_str().expect("path"));
    assert!(path.is_absolute());
    assert_eq!(
        path.parent().expect("parent"),
        sandbox.state_dir().join("uploads")
    );
    let name = path
        .file_name()
        .expect("name")
        .to_str()
        .expect("utf-8")
        .to_string();
    let parts: Vec<&str> = name.trim_end_matches(".png").splitn(3, '-').collect();
    assert_eq!(parts[0], "img");
    assert!(parts[1].parse::<u64>().is_ok(), "unix ms in {name}");
    assert_eq!(parts[2].len(), 8, "8 hex digits in {name}");
    assert!(name.ends_with(".png"));
    assert_eq!(
        std::fs::read(&path).expect("uploaded file"),
        image,
        "byte for byte"
    );
    assert_eq!(
        std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().expect("parent"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    // Every accepted type, whatever the client claims it is.
    for (bytes, extension, mime) in [
        (&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10][..], "jpg", "image/jpeg"),
        (b"GIF89a\x01\x00\x01\x00", "gif", "image/gif"),
        (b"RIFF\x24\x00\x00\x00WEBPVP8 ", "webp", "image/webp"),
    ] {
        let response = server
            .http(
                "POST",
                "/api/upload",
                &[
                    ("X-Tmux-Web", "1"),
                    ("Content-Type", "application/octet-stream"),
                ],
                bytes,
            )
            .await;
        assert_eq!(response.status, 201);
        assert_eq!(response.json()["mime"], mime);
        assert!(response.json()["path"]
            .as_str()
            .expect("path")
            .ends_with(&format!(".{extension}")));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_uploads_are_400_and_413_and_any_file_is_accepted() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &["--max-upload-mb", "1"]).await;
    let upload = |body: Vec<u8>, content_type: &'static str| {
        let server = &server;
        async move {
            server
                .http(
                    "POST",
                    "/api/upload",
                    &[("X-Tmux-Web", "1"), ("Content-Type", content_type)],
                    &body,
                )
                .await
        }
    };

    let empty = upload(Vec::new(), "image/png").await;
    assert_eq!((empty.status, empty.error_code().as_str()), (400, "empty"));

    let text = server
        .http(
            "POST",
            "/api/upload?name=notes.txt",
            &[("X-Tmux-Web", "1"), ("Content-Type", "image/png")],
            b"definitely not an image",
        )
        .await;
    assert_eq!(text.status, 201);
    let text_path = text.json()["path"].as_str().unwrap().to_string();
    assert!(text_path.ends_with("-notes.txt"), "{text_path}");
    assert_eq!(
        std::fs::read(&text_path).unwrap(),
        b"definitely not an image"
    );
    let svg = upload(
        b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
        "image/svg+xml",
    )
    .await;
    assert_eq!(svg.status, 201);

    let mut too_big = PNG.to_vec();
    too_big.resize(2 * 1024 * 1024, 7);
    let big = upload(too_big, "image/png").await;
    assert_eq!((big.status, big.error_code().as_str()), (413, "too_large"));

    let mut limit = PNG.to_vec();
    limit.resize(1024 * 1024, 7);
    assert_eq!(
        upload(limit, "image/png").await.status,
        201,
        "exactly the limit is fine"
    );

    let stored = std::fs::read_dir(sandbox.state_dir().join("uploads"))
        .expect("uploads dir")
        .count();
    assert_eq!(
        stored, 3,
        "refused uploads leave nothing behind (text, svg and the exact-limit PNG are kept)"
    );
}

// ---- layout ---------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_layout_survives_a_restart_and_versions_keep_counting() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let mut server = TestServer::start(&tmux, &sandbox, &[]).await;

    assert_eq!(
        server.get_json("/api/layout").await,
        json!({"version": 0, "updated_at": null, "layout": null})
    );
    let layout = json!({"schema": 1, "dockview": {"grid": {"root": {}}, "panels": {}}, "maximizedPanelId": null, "prefs": {"fontSize": 13}});
    let saved = server
        .send_json(
            "PUT",
            "/api/layout",
            &json!({"layout": layout, "client_id": "tab-1"}),
        )
        .await;
    assert_eq!(saved.status, 200, "{}", saved.text());
    assert_eq!(saved.json()["version"], 1);
    let updated_at = saved.json()["updated_at"].as_i64().expect("updated_at");
    assert!(updated_at > 1_700_000_000_000, "milliseconds: {updated_at}");
    assert_eq!(
        server.get_json("/api/layout").await,
        json!({"version": 1, "updated_at": updated_at, "layout": layout})
    );

    let file = sandbox.state_dir().join("layout.json");
    assert_eq!(
        std::fs::metadata(&file)
            .expect("layout.json")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    server.stop().await;
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    assert_eq!(
        server.get_json("/api/layout").await,
        json!({"version": 1, "updated_at": updated_at, "layout": layout})
    );
    let again = server
        .send_json(
            "PUT",
            "/api/layout",
            &json!({"layout": {"second": true}, "client_id": "tab-2"}),
        )
        .await;
    assert_eq!(again.json()["version"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversize_layout_is_413_and_malformed_requests_are_400() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;
    let huge = json!({"layout": {"blob": "x".repeat(3 * 1024 * 1024)}, "client_id": "c"});
    let response = server.send_json("PUT", "/api/layout", &huge).await;
    assert_eq!(
        (response.status, response.error_code().as_str()),
        (413, "too_large")
    );
    assert_eq!(server.get_json("/api/layout").await["version"], 0);

    for bad in [
        json!({"layout": {}}),
        json!({"client_id": "c"}),
        json!({"layout": {}, "client_id": 3}),
    ] {
        let response = server.send_json("PUT", "/api/layout", &bad).await;
        assert_eq!(
            (response.status, response.error_code().as_str()),
            (400, "bad_request"),
            "{bad}"
        );
    }
    let response = server
        .http("PUT", "/api/layout", &[("X-Tmux-Web", "1")], b"{broken")
        .await;
    assert_eq!(response.status, 400);
}

// ---- static files and errors -----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn static_files_have_the_right_types_and_declarations_are_hidden() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let server = TestServer::start(&tmux, &sandbox, &[]).await;

    let module = server.get("/vendor/xterm/xterm.mjs").await;
    assert_eq!(module.status, 200);
    assert_eq!(
        module.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(module.header("x-content-type-options"), Some("nosniff"));
    assert!(module.header("etag").is_some());
    assert!(!module.body.is_empty());
    let cached = server
        .http(
            "GET",
            "/vendor/xterm/xterm.mjs",
            &[("If-None-Match", module.header("etag").expect("etag"))],
            b"",
        )
        .await;
    assert_eq!(cached.status, 304);

    let css = server.get("/vendor/xterm/xterm.css").await;
    assert_eq!(css.header("content-type"), Some("text/css; charset=utf-8"));

    for hidden in [
        "/vendor/addon-fit/addon-fit.d.ts",
        "/vendor/dockview-core/types/api/component.api.d.ts",
    ] {
        assert_eq!(server.get(hidden).await.status, 404, "{hidden}");
    }
    for traversal in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/vendor/../../Cargo.toml",
    ] {
        assert_eq!(server.get(traversal).await.status, 404, "{traversal}");
    }

    let unknown = server.get("/nothing-here.js").await;
    assert_eq!(unknown.status, 404);
    let api = server.get("/api/nothing-here").await;
    assert_eq!((api.status, api.error_code().as_str()), (404, "not_found"));
    assert_eq!(api.header("content-type"), Some("application/json"));
    let wrong_method = server.get("/api/sessions").await;
    assert_eq!(
        (wrong_method.status, wrong_method.error_code().as_str()),
        (405, "method_not_allowed")
    );
}

// ---- shutdown -----------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_closes_every_socket_and_kills_the_attach_clients() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let mut server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");

    let mut attach = server.ws("/ws/attach?session=alpha").await;
    type_and_expect(&mut attach, "echo up-$((3+3))", "up-6").await;
    let mut events = server.ws("/ws/events").await;
    next_event(&mut events, "status", WITHIN).await;
    let pids = attach_client_pids(tmux.socket(), "alpha");
    assert_eq!(pids.len(), 1);

    let took = server.stop().await;
    assert!(took < Duration::from_secs(3), "shutdown took {took:?}");

    let exit = read_until_closed(&mut attach, Duration::from_secs(3)).await;
    assert_eq!(exit.close_code, Some(1001), "going away: {exit:?}");
    assert_eq!(exit.reason, None, "no exit frame for a server shutdown");
    loop {
        match next_frame(&mut events, Duration::from_secs(3)).await {
            Frame::Close(frame) => {
                assert_eq!(frame.map(|(code, _)| code), Some(1001));
                break;
            }
            Frame::Ended => break,
            _ => {}
        }
    }
    for pid in pids {
        assert!(
            !process_exists(pid),
            "attach client {pid} outlived the server"
        );
    }
    assert!(
        tmux.has_session("alpha"),
        "the tmux session itself is untouched"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_does_not_wait_for_a_client_that_never_answers() {
    let tmux = Tmux::new();
    let sandbox = Sandbox::new();
    let mut server = TestServer::start(&tmux, &sandbox, &[]).await;
    tmux.new_session("alpha");
    // Connected but never read from again: the close handshake can never complete.
    let _silent_attach = server.ws("/ws/attach?session=alpha").await;
    let _silent_events = server.ws("/ws/events").await;
    eventually("the attach client", WITHIN, || async {
        (!attach_client_pids(tmux.socket(), "alpha").is_empty()).then_some(())
    })
    .await;
    let took = server.stop().await;
    assert!(took < Duration::from_secs(4), "shutdown took {took:?}");
    eventually("the attach client to be gone", WITHIN, || async {
        attach_client_pids(tmux.socket(), "alpha")
            .is_empty()
            .then_some(())
    })
    .await;
}
