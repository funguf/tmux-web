//! The tmux scan (`take_snapshot`) against real tmux servers, each on its own
//! `-L tmw-core-<hex>` socket (see `core_support`).

mod core_support;

use core_support::{normalize_layout, wait_until, TestTmux, PATIENCE};
use std::path::Path;
use std::time::Duration;
use tmux_web::snapshot::take_snapshot;
use tmux_web::tmux::{self, TmuxError};
use tmux_web::util;

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---- the layout normaliser the round-trip tests rely on ---------------------------------------

#[test]
fn normalised_layouts_ignore_checksums_and_pane_ids_but_keep_the_geometry() {
    assert_eq!(normalize_layout("b25f,159x40,0,0,3"), "159x40,0,0");
    assert_eq!(
        normalize_layout("b25f,159x40,0,0[159x20,0,0,3,159x19,0,21,5]"),
        "159x40,0,0[159x20,0,0,159x19,0,21]"
    );
    let nested = "1af7,200x50,0,0{100x50,0,0,4,99x50,101,0[99x25,101,0,5,99x24,101,26,6]}";
    assert_eq!(
        normalize_layout(nested),
        "200x50,0,0{100x50,0,0,99x50,101,0[99x25,101,0,99x24,101,26]}"
    );
    // The same geometry with other pane numbers and another checksum.
    let renumbered = "ffff,200x50,0,0{100x50,0,0,40,99x50,101,0[99x25,101,0,51,99x24,101,26,62]}";
    assert_eq!(normalize_layout(nested), normalize_layout(renumbered));
    // A different geometry stays different.
    let moved = "1af7,200x50,0,0{101x50,0,0,4,98x50,102,0[98x25,102,0,5,98x24,102,26,6]}";
    assert_ne!(normalize_layout(nested), normalize_layout(moved));
}

// ---- a snapshot of a live server ----------------------------------------------------------------

#[test]
fn a_snapshot_agrees_with_what_tmux_reports() {
    let t = TestTmux::new();
    let detector = t.detector(&t.dir("claude-config"));
    let (a, b) = (t.dir("a"), t.dir("b"));
    t.new_session("one", &a);
    t.tmux(&[
        "split-window",
        "-d",
        "-t",
        "=one:0.0",
        "-c",
        &path_string(&b),
    ]);
    t.tmux(&[
        "new-window",
        "-d",
        "-t",
        "=one:4",
        "-n",
        "second",
        "-c",
        &path_string(&b),
    ]);
    t.new_session("two", &b);
    let before = util::now_ms();

    let snap = t.snapshot(&detector);

    let after = util::now_ms();
    assert!(
        (before..=after).contains(&snap.taken_at),
        "{}",
        snap.taken_at
    );
    assert_eq!(snap.version, tmux_web::snapshot::SNAPSHOT_VERSION);
    let (identity, sessions) = tmux::list_sessions(&t.runner).unwrap();
    assert_eq!(Some(snap.server.clone()), identity);
    assert_eq!(
        snap.sessions
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "two"]
    );
    for info in &sessions {
        let found = snap.sessions.iter().find(|s| s.id == info.id).unwrap();
        assert_eq!(found.name, info.name);
        assert_eq!(found.created, info.created);
        assert_eq!(found.group, info.group);
        assert_eq!(found.windows.len(), usize::try_from(info.windows).unwrap());
    }
    let one = &snap.sessions[0];
    assert_eq!(
        one.windows
            .iter()
            .map(|w| (w.index, w.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "sh"), (4, "second")]
    );
    assert_eq!(one.windows[0].panes.len(), 2);
    assert!(one.windows[0].automatic_rename && !one.windows[1].automatic_rename);
    for session in &snap.sessions {
        assert!(session.id.starts_with('$'));
        for window in &session.windows {
            assert!(window.id.starts_with('@'));
            assert_eq!((window.width, window.height), (200, 50));
            assert!(!window.zoomed);
            assert!(!normalize_layout(&window.layout).is_empty());
            assert_eq!(window.panes.iter().filter(|p| p.active).count(), 1);
            for pane in &window.panes {
                assert!(pane.id.starts_with('%'));
                assert!(
                    Path::new(&format!("/proc/{}", pane.pid)).exists(),
                    "{}",
                    pane.pid
                );
                let reported = t.tmux(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane.id,
                    "#{pane_current_path}",
                ]);
                assert_eq!(pane.cwd, reported.trim_end());
                assert_eq!(pane.current_command, "sh");
                assert_eq!(pane.foreground_cmdline, None, "an idle shell");
                assert_eq!(pane.claude, None);
            }
        }
    }
    let cwds: Vec<&str> = one.windows[0]
        .panes
        .iter()
        .map(|p| p.cwd.as_str())
        .collect();
    assert_eq!(cwds, vec![path_string(&a), path_string(&b)]);
}

#[test]
fn there_is_no_snapshot_without_a_server_or_without_sessions() {
    let t = TestTmux::new();
    let detector = t.detector(&t.dir("claude-config"));
    let result = take_snapshot(&t.runner, &detector);
    assert!(matches!(result, Err(TmuxError::NoServer)), "{result:?}");

    // A server can outlive its last session when told to.
    t.new_session("only", &t.dir("a"));
    t.tmux(&["set-option", "-g", "exit-empty", "off"]);
    t.tmux(&["kill-session", "-t", "=only"]);
    let (identity, sessions) = tmux::list_sessions(&t.runner).unwrap();
    assert!(identity.is_none() && sessions.is_empty());
    match take_snapshot(&t.runner, &detector) {
        Err(TmuxError::Command(message)) => assert_eq!(message, "no sessions"),
        other => panic!("expected no sessions, got {other:?}"),
    }
}

#[test]
fn a_dying_server_is_no_server_and_never_a_command_error() {
    for round in 0..4 {
        let t = TestTmux::new();
        let detector = t.detector(&t.dir("claude-config"));
        t.new_session("doomed", &t.dir("a"));
        t.kill_server_now();

        // Right after kill-server tmux says "server exited unexpectedly", a moment later "no
        // server running"; both are NoServer. A late answer from the dying server is fine too.
        let outcome = wait_until("the server to be gone", PATIENCE, || {
            let listed = tmux::list_sessions(&t.runner);
            let snapped = take_snapshot(&t.runner, &detector);
            for (what, ok, no_server) in [
                (
                    "list_sessions",
                    listed.is_ok(),
                    matches!(listed, Err(TmuxError::NoServer)),
                ),
                (
                    "take_snapshot",
                    snapped.is_ok(),
                    matches!(snapped, Err(TmuxError::NoServer)),
                ),
            ] {
                assert!(
                    ok || no_server,
                    "round {round}: {what} failed with something else"
                );
            }
            matches!(listed, Err(TmuxError::NoServer)).then_some(())
        });
        assert!(
            outcome.is_some(),
            "round {round}: the server never went away"
        );
        assert!(!t.is_running());
    }
}

// ---- names and directories ------------------------------------------------------------------------

#[test]
fn new_session_and_rename_session_take_hashes_literally() {
    let t = TestTmux::new();
    let detector = t.detector(&t.dir("claude-config"));
    // The server runs from its home directory: a command it ran would create this file there.
    let marker = t.home().join("session-name-ran");
    let dir = t.dir("start#(touch session-name-ran)#S");
    let hostile = "build#S #{session_id} #(touch session-name-ran)";

    let id = tmux::new_session(&t.runner, hostile, Some(dir.as_path())).unwrap();

    assert!(id.starts_with('$'));
    assert_eq!(t.session_names(), vec![hostile.to_string()]);
    assert!(tmux::has_session(&t.runner, hostile).unwrap());
    assert!(!tmux::has_session(&t.runner, "build").unwrap());
    let snap = t.snapshot(&detector);
    assert_eq!(snap.sessions[0].windows[0].panes[0].cwd, path_string(&dir));

    let renamed = "after#W #(touch session-name-ran)";
    tmux::rename_session(&t.runner, hostile, renamed).unwrap();
    assert_eq!(t.session_names(), vec![renamed.to_string()]);
    assert!(!marker.exists(), "a session name was run as a command");
    // The new session carries no format residue, and a clash is still a clash.
    assert!(matches!(
        tmux::new_session(&t.runner, renamed, None),
        Err(TmuxError::Command(message)) if message.contains("duplicate session")
    ));
}

#[test]
fn directories_with_tabs_and_spaces_survive_the_tab_separated_listing() {
    let t = TestTmux::new();
    let detector = t.detector(&t.dir("claude-config"));
    let tabbed = t.dir("tab\there and  two  spaces");
    t.new_session("odd", &tabbed);
    t.tmux(&[
        "split-window",
        "-d",
        "-t",
        "=odd:0.0",
        "-c",
        &path_string(&t.dir("ünï cödé 日本語")),
    ]);

    let snap = t.snapshot(&detector);

    let cwds: Vec<&str> = snap.sessions[0].windows[0]
        .panes
        .iter()
        .map(|p| p.cwd.as_str())
        .collect();
    assert_eq!(
        cwds,
        vec![path_string(&tabbed), path_string(&t.dir("ünï cödé 日本語"))]
    );
}

#[test]
fn a_directory_with_a_newline_in_its_name_does_not_spoil_the_snapshot() {
    let t = TestTmux::new();
    let detector = t.detector(&t.dir("claude-config"));
    let normal = t.dir("normal");
    let newline = t.dir("new\nline");
    t.new_session("fine", &normal);
    t.new_session("weird", &newline);
    t.tmux(&[
        "split-window",
        "-d",
        "-t",
        "=weird:0.0",
        "-c",
        &path_string(&normal),
    ]);

    // tmux prints that directory over two lines; the snapshot keeps every session and pane.
    let snap = t.snapshot_when(&detector, "a snapshot of both sessions", |s| {
        s.sessions.len() == 2
    });

    assert_eq!(
        snap.sessions
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["fine", "weird"]
    );
    let weird = &snap.sessions[1].windows[0].panes;
    assert_eq!(weird.len(), 2);
    assert!(!weird[0].cwd.contains('\n'));
    assert!(
        weird[0].cwd.ends_with("new"),
        "cut at the newline: {:?}",
        weird[0].cwd
    );
    assert_eq!(weird[1].cwd, path_string(&normal));
    assert_eq!(
        snap.sessions[0].windows[0].panes[0].cwd,
        path_string(&normal)
    );
}

#[test]
fn window_names_are_whatever_tmux_reports() {
    let t = TestTmux::new();
    let detector = t.detector(&t.dir("claude-config"));
    t.new_session("names", &t.dir("a"));
    // The first window of a session is created by `new-session -n`, the others by `new-window -n`.
    t.tmux(&["rename-window", "-t", "=names:0", "first\\window\there"]);
    let names = [
        "with spaces  ",
        "日本語 ✓",
        "tab\there",
        "quote'\"s",
        "##{literal}",
        "a|b;c",
    ];
    for (index, name) in names.iter().enumerate() {
        let target = format!("=names:{}", index + 1);
        t.tmux(&[
            "new-window",
            "-d",
            "-t",
            &target,
            "-c",
            &path_string(&t.dir("a")),
        ]);
        t.tmux(&["rename-window", "-t", &target, name]);
    }
    let snap = t.snapshot(&detector);
    for (index, window) in snap.sessions[0].windows.iter().enumerate() {
        let reported = t.tmux(&["display-message", "-p", "-t", &window.id, "#{window_name}"]);
        assert_eq!(
            window.name,
            reported.trim_end_matches('\n'),
            "window {index}"
        );
        assert!(!window.automatic_rename);
    }
}

#[test]
fn waiting_for_a_condition_that_holds_returns_at_once() {
    let started = std::time::Instant::now();
    assert_eq!(
        wait_until("true", Duration::from_secs(5), || Some(7)),
        Some(7)
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}
