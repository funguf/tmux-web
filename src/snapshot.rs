//! In-memory scan of the tmux server (sessions, windows, panes, foreground programs, Claude panes).

use crate::procscan::{self, ProcTable};
use crate::{claude, tmux, util};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub taken_at: i64,
    pub server: tmux::ServerIdentity,
    pub sessions: Vec<SessionSnap>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionSnap {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub group: Option<String>,
    pub windows: Vec<WindowSnap>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowSnap {
    pub id: String,
    pub index: i64,
    pub name: String,
    pub automatic_rename: bool,
    pub active: bool,
    pub zoomed: bool,
    pub width: u32,
    pub height: u32,
    pub layout: String,
    pub panes: Vec<PaneSnap>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PaneSnap {
    pub id: String,
    pub index: i64,
    pub active: bool,
    pub pid: u32,
    pub cwd: String,
    pub current_command: String,
    /// argv of the foreground process when it is not the shell
    pub foreground_cmdline: Option<Vec<String>>,
    pub claude: Option<claude::ClaudeSnap>,
}

/// `list-windows -a -F`: tab separated, the window name LAST because it is free text.
const WINDOW_FORMAT: &str = "#{session_id}\t#{window_id}\t#{window_index}\t#{window_active}\t#{window_zoomed_flag}\t#{window_width}\t#{window_height}\t#{automatic-rename}\t#{window_layout}\t#{window_name}";

/// Number of tab-separated fields in [`WINDOW_FORMAT`].
const WINDOW_FIELDS: usize = 10;

/// `list-panes -a -F`: tab separated, the path LAST because it is free text.
const PANE_FORMAT: &str = "#{session_id}\t#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}";

/// Number of tab-separated fields in [`PANE_FORMAT`].
const PANE_FIELDS: usize = 8;

/// One line of `list-windows -a`; the panes are joined in afterwards.
#[derive(Debug, PartialEq)]
struct WindowRow {
    session_id: String,
    window: WindowSnap,
}

/// One line of `list-panes -a`; the process facts are joined in afterwards.
#[derive(Debug, PartialEq)]
struct PaneRow {
    session_id: String,
    window_id: String,
    pane: PaneSnap,
}

/// tmux prints flags as 1/0 and, for window options such as automatic-rename, may print on/off.
fn parse_flag(text: &str) -> bool {
    matches!(text, "1" | "on")
}

fn parse_field<T: std::str::FromStr>(text: &str, what: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("bad {what} {text:?}"))
}

fn require_id(text: &str, what: &str) -> Result<String, String> {
    if text.is_empty() {
        Err(format!("empty {what}"))
    } else {
        Ok(text.to_string())
    }
}

fn parse_window_line(line: &str) -> Result<WindowRow, String> {
    // The name comes last and may contain tabs, hence the splitn limit.
    let fields: Vec<&str> = line.splitn(WINDOW_FIELDS, '\t').collect();
    let Ok(
        [session_id, window_id, index, active, zoomed, width, height, automatic_rename, layout, name],
    ) = <[&str; WINDOW_FIELDS]>::try_from(fields.as_slice())
    else {
        return Err(format!(
            "expected {WINDOW_FIELDS} tab-separated fields, found {}",
            fields.len()
        ));
    };
    Ok(WindowRow {
        session_id: require_id(session_id, "session id")?,
        window: WindowSnap {
            id: require_id(window_id, "window id")?,
            index: parse_field(index, "window index")?,
            name: name.to_string(),
            automatic_rename: parse_flag(automatic_rename),
            active: parse_flag(active),
            zoomed: parse_flag(zoomed),
            width: parse_field(width, "window width")?,
            height: parse_field(height, "window height")?,
            layout: layout.to_string(),
            panes: Vec::new(),
        },
    })
}

fn parse_pane_line(line: &str) -> Result<PaneRow, String> {
    // The path comes last and may contain tabs, hence the splitn limit.
    let fields: Vec<&str> = line.splitn(PANE_FIELDS, '\t').collect();
    let Ok([session_id, window_id, pane_id, index, active, pid, command, path]) =
        <[&str; PANE_FIELDS]>::try_from(fields.as_slice())
    else {
        return Err(format!(
            "expected {PANE_FIELDS} tab-separated fields, found {}",
            fields.len()
        ));
    };
    Ok(PaneRow {
        session_id: require_id(session_id, "session id")?,
        window_id: require_id(window_id, "window id")?,
        pane: PaneSnap {
            id: require_id(pane_id, "pane id")?,
            index: parse_field(index, "pane index")?,
            active: parse_flag(active),
            pid: parse_field(pid, "pane pid")?,
            cwd: path.to_string(),
            current_command: command.to_string(),
            foreground_cmdline: None,
            claude: None,
        },
    })
}

/// Parses every line with `parse`. A line that does not parse is reported, not fatal: one odd
/// pane (say, a directory whose name contains a newline) must not stop the snapshots of every
/// other session.
fn parse_lines<T>(out: &str, parse: fn(&str) -> Result<T, String>) -> (Vec<T>, Vec<String>) {
    let mut rows = Vec::new();
    let mut problems = Vec::new();
    for (number, line) in out.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        match parse(line) {
            Ok(row) => rows.push(row),
            Err(reason) => problems.push(format!("line {} ({reason}): {line:?}", number + 1)),
        }
    }
    (rows, problems)
}

/// What /proc says about the processes of one pane.
#[derive(Clone)]
struct PaneProcess {
    foreground_cmdline: Option<Vec<String>>,
    claude: Option<claude::ClaudeSnap>,
}

fn inspect_pane(
    table: &ProcTable,
    detector: &claude::ClaudeDetector,
    pane_pid: u32,
) -> PaneProcess {
    PaneProcess {
        foreground_cmdline: table
            .foreground_pid(pane_pid)
            .and_then(|pid| procscan::read_cmdline(&detector.proc_root, pid)),
        claude: detector.detect(table, pane_pid),
    }
}

/// Joins sessions, windows and panes by their ids. Rows that point at a session or window that
/// is not in the other lists belong to something created or destroyed between the tmux calls
/// and are dropped, as are windows without panes and sessions without windows.
fn assemble_sessions(
    sessions: Vec<tmux::SessionInfo>,
    window_rows: Vec<WindowRow>,
    pane_rows: Vec<PaneRow>,
    table: &ProcTable,
    detector: &claude::ClaudeDetector,
) -> Vec<SessionSnap> {
    // Panes shared by grouped sessions appear once per session; look each process up once.
    let mut processes: HashMap<u32, PaneProcess> = HashMap::new();
    let mut panes: HashMap<(String, String), Vec<PaneSnap>> = HashMap::new();
    for row in pane_rows {
        let mut pane = row.pane;
        let process = processes
            .entry(pane.pid)
            .or_insert_with(|| inspect_pane(table, detector, pane.pid))
            .clone();
        pane.foreground_cmdline = process.foreground_cmdline;
        pane.claude = process.claude;
        panes
            .entry((row.session_id, row.window_id))
            .or_default()
            .push(pane);
    }

    let known: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let mut windows: HashMap<String, Vec<WindowSnap>> = HashMap::new();
    for row in window_rows {
        if !known.contains(row.session_id.as_str()) {
            continue;
        }
        let mut window = row.window;
        let Some(mut window_panes) = panes.remove(&(row.session_id.clone(), window.id.clone()))
        else {
            continue;
        };
        window_panes.sort_by_key(|pane| pane.index);
        window.panes = window_panes;
        windows.entry(row.session_id).or_default().push(window);
    }

    sessions
        .into_iter()
        .filter_map(|session| {
            let mut session_windows = windows.remove(&session.id)?;
            session_windows.sort_by_key(|window| window.index);
            Some(SessionSnap {
                id: session.id,
                name: session.name,
                created: session.created,
                group: session.group,
                windows: session_windows,
            })
        })
        .collect()
}

fn tmux_args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

/// list-sessions + list-windows -a + list-panes -a (tab-separated formats, free text LAST) + /proc scan.
/// Tolerates races (a session vanishing between calls is skipped).
///
/// `Err(NoServer)` when there is no server. A server that has no sessions (or whose sessions all
/// vanished while the snapshot was being taken) is `Err(Command("no sessions"))`: an empty
/// snapshot would replace the useful one of the same server instance.
pub fn take_snapshot(
    r: &dyn tmux::TmuxRunner,
    detector: &claude::ClaudeDetector,
) -> Result<Snapshot, tmux::TmuxError> {
    let no_sessions = || tmux::TmuxError::Command("no sessions".to_string());
    let (identity, sessions) = tmux::list_sessions(r)?;
    let server = identity.ok_or_else(no_sessions)?;
    let windows_out = r.run(&tmux_args(&["list-windows", "-a", "-F", WINDOW_FORMAT]))?;
    let panes_out = r.run(&tmux_args(&["list-panes", "-a", "-F", PANE_FORMAT]))?;
    let (window_rows, window_problems) = parse_lines(&windows_out, parse_window_line);
    let (pane_rows, pane_problems) = parse_lines(&panes_out, parse_pane_line);
    for problem in &window_problems {
        tracing::warn!(%problem, "skipping unreadable list-windows line");
    }
    for problem in &pane_problems {
        tracing::warn!(%problem, "skipping unreadable list-panes line");
    }

    // One scan per snapshot. Without /proc the structure is still worth keeping.
    let table = ProcTable::scan(&detector.proc_root).unwrap_or_else(|err| {
        tracing::warn!(
            proc_root = %detector.proc_root.display(),
            error = %err,
            "cannot scan processes; snapshot has no program or Claude information"
        );
        ProcTable::default()
    });
    let sessions = assemble_sessions(sessions, window_rows, pane_rows, &table, detector);
    if sessions.is_empty() {
        return Err(no_sessions());
    }
    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
        taken_at: util::now_ms(),
        server,
        sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::{ClaudeDetector, ClaudeIdSource, ClaudeSnap};
    use crate::tmux::{ServerIdentity, SessionInfo, TmuxError, TmuxRunner};
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;

    const SESSION_ID: &str = "0b6f6a59-3c1e-4c55-9f0e-1d2a3b4c5d6e";

    // ---- builders ---------------------------------------------------------------------

    /// (pane index, pane id, cwd)
    type PaneView<'a> = (i64, &'a str, &'a str);
    /// (window index, name, panes)
    type WindowView<'a> = (i64, &'a str, Vec<PaneView<'a>>);
    /// (session name, group, windows)
    type SessionView<'a> = (&'a str, Option<&'a str>, Vec<WindowView<'a>>);

    fn identity(start_time: i64, pid: u32) -> ServerIdentity {
        ServerIdentity {
            pid,
            start_time,
            socket_path: "/tmp/tmux-1000/default".to_string(),
            version: "3.4".to_string(),
        }
    }

    fn pane(id: &str, index: i64, active: bool, cwd: &str) -> PaneSnap {
        PaneSnap {
            id: id.to_string(),
            index,
            active,
            pid: 1000 + u32::try_from(index).unwrap(),
            cwd: cwd.to_string(),
            current_command: "sh".to_string(),
            foreground_cmdline: None,
            claude: None,
        }
    }

    fn window(id: &str, index: i64, name: &str, panes: Vec<PaneSnap>) -> WindowSnap {
        WindowSnap {
            id: id.to_string(),
            index,
            name: name.to_string(),
            automatic_rename: false,
            active: index == 0,
            zoomed: false,
            width: 200,
            height: 50,
            layout: "layout".to_string(),
            panes,
        }
    }

    fn session(id: &str, name: &str, windows: Vec<WindowSnap>) -> SessionSnap {
        SessionSnap {
            id: id.to_string(),
            name: name.to_string(),
            created: 1_700_000_000,
            group: None,
            windows,
        }
    }

    fn claude_snap() -> ClaudeSnap {
        ClaudeSnap {
            pid: 4242,
            session_id: Some(SESSION_ID.to_string()),
            cwd: "/work".to_string(),
            args: vec!["--verbose".to_string()],
            kind: Some("interactive".to_string()),
            name: None,
            source: ClaudeIdSource::SessionsFile,
        }
    }

    fn snapshot(start_time: i64, pid: u32, taken_at: i64, sessions: Vec<SessionSnap>) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            taken_at,
            server: identity(start_time, pid),
            sessions,
        }
    }

    // ---- Snapshot helpers -----------------------------------------------------------

    #[test]
    fn snapshot_json_round_trips() {
        let mut p = pane("%2", 1, false, "/w");
        p.foreground_cmdline = Some(vec!["tail".to_string(), "-f".to_string(), "x".to_string()]);
        p.claude = Some(claude_snap());
        let snap = snapshot(
            1_700_000_000,
            42,
            99,
            vec![session("$0", "alpha", vec![window("@1", 0, "a", vec![p])])],
        );
        let json = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&json).unwrap(), snap);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["taken_at"], 99);
        assert_eq!(value["server"]["pid"], 42);
        assert_eq!(
            value["sessions"][0]["windows"][0]["panes"][0]["claude"]["source"],
            "sessions-file"
        );
    }

    // ---- parsers ----------------------------------------------------------------------

    #[test]
    fn formats_match_the_parsers_field_order() {
        let windows: Vec<&str> = WINDOW_FORMAT.split('\t').collect();
        assert_eq!(windows.len(), WINDOW_FIELDS);
        assert_eq!(
            windows,
            [
                "#{session_id}",
                "#{window_id}",
                "#{window_index}",
                "#{window_active}",
                "#{window_zoomed_flag}",
                "#{window_width}",
                "#{window_height}",
                "#{automatic-rename}",
                "#{window_layout}",
                "#{window_name}",
            ]
        );
        let panes: Vec<&str> = PANE_FORMAT.split('\t').collect();
        assert_eq!(panes.len(), PANE_FIELDS);
        assert_eq!(
            panes,
            [
                "#{session_id}",
                "#{window_id}",
                "#{pane_id}",
                "#{pane_index}",
                "#{pane_active}",
                "#{pane_pid}",
                "#{pane_current_command}",
                "#{pane_current_path}",
            ]
        );
    }

    fn window_line(fields: [&str; WINDOW_FIELDS]) -> String {
        fields.join("\t")
    }

    fn good_window() -> [&'static str; WINDOW_FIELDS] {
        [
            "$3",
            "@7",
            "5",
            "1",
            "0",
            "200",
            "50",
            "1",
            "b25f,200x50,0,0,7",
            "editor",
        ]
    }

    #[test]
    fn parse_window_line_reads_every_field() {
        let row = parse_window_line(&window_line(good_window())).unwrap();
        assert_eq!(row.session_id, "$3");
        assert_eq!(
            row.window,
            WindowSnap {
                id: "@7".to_string(),
                index: 5,
                name: "editor".to_string(),
                automatic_rename: true,
                active: true,
                zoomed: false,
                width: 200,
                height: 50,
                layout: "b25f,200x50,0,0,7".to_string(),
                panes: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_window_line_flags() {
        let mut fields = good_window();
        fields[3] = "0";
        fields[4] = "1";
        fields[7] = "0";
        let row = parse_window_line(&window_line(fields)).unwrap();
        assert!(!row.window.active && row.window.zoomed && !row.window.automatic_rename);
        for (text, expected) in [
            ("1", true),
            ("on", true),
            ("0", false),
            ("off", false),
            ("", false),
        ] {
            let mut fields = good_window();
            fields[7] = text;
            let row = parse_window_line(&window_line(fields)).unwrap();
            assert_eq!(
                row.window.automatic_rename, expected,
                "automatic-rename {text:?}"
            );
        }
    }

    #[test]
    fn parse_window_line_keeps_tabs_spaces_and_unicode_in_the_name() {
        for name in [
            "with\ttab",
            "two  spaces ",
            "日本語 ✓",
            "#{literal}",
            "",
            "a\tb\tc\td\te",
        ] {
            let mut fields = good_window();
            fields[9] = name;
            let row = parse_window_line(&window_line(fields)).unwrap();
            assert_eq!(row.window.name, name);
        }
    }

    #[test]
    fn parse_window_line_accepts_negative_and_large_indexes() {
        let mut fields = good_window();
        fields[2] = "-1";
        assert_eq!(
            parse_window_line(&window_line(fields))
                .unwrap()
                .window
                .index,
            -1
        );
        fields[2] = "4294967296";
        assert_eq!(
            parse_window_line(&window_line(fields))
                .unwrap()
                .window
                .index,
            4_294_967_296
        );
    }

    #[test]
    fn parse_window_line_rejects_malformed_lines() {
        assert!(parse_window_line("").is_err());
        assert!(parse_window_line("only\tthree\tfields").is_err());
        assert!(parse_window_line(&good_window()[..9].join("\t")).is_err());
        for (index, why) in [(2, "index"), (5, "width"), (6, "height")] {
            let mut fields = good_window();
            fields[index] = "abc";
            assert!(parse_window_line(&window_line(fields)).is_err(), "{why}");
        }
        let mut fields = good_window();
        fields[5] = "-3";
        assert!(
            parse_window_line(&window_line(fields)).is_err(),
            "negative width"
        );
        for index in [0, 1] {
            let mut fields = good_window();
            fields[index] = "";
            assert!(
                parse_window_line(&window_line(fields)).is_err(),
                "empty id {index}"
            );
        }
    }

    fn good_pane() -> [&'static str; PANE_FIELDS] {
        [
            "$3",
            "@7",
            "%12",
            "2",
            "1",
            "4321",
            "bash",
            "/home/u/project",
        ]
    }

    #[test]
    fn parse_pane_line_reads_every_field() {
        let row = parse_pane_line(&good_pane().join("\t")).unwrap();
        assert_eq!(
            (row.session_id.as_str(), row.window_id.as_str()),
            ("$3", "@7")
        );
        assert_eq!(
            row.pane,
            PaneSnap {
                id: "%12".to_string(),
                index: 2,
                active: true,
                pid: 4321,
                cwd: "/home/u/project".to_string(),
                current_command: "bash".to_string(),
                foreground_cmdline: None,
                claude: None,
            }
        );
        let mut fields = good_pane();
        fields[4] = "0";
        assert!(!parse_pane_line(&fields.join("\t")).unwrap().pane.active);
    }

    #[test]
    fn parse_pane_line_keeps_tabs_spaces_and_unicode_in_the_path() {
        for path in [
            "/tmp/a b",
            "/tmp/tab\there",
            "/tmp/日本語",
            "/tmp/#{x}",
            "",
            "/a\tb\tc",
        ] {
            let mut fields = good_pane();
            fields[7] = path;
            let row = parse_pane_line(&fields.join("\t")).unwrap();
            assert_eq!(row.pane.cwd, path);
        }
    }

    #[test]
    fn parse_pane_line_rejects_malformed_lines() {
        assert!(parse_pane_line("").is_err());
        assert!(parse_pane_line("$0\t@1\t%1").is_err());
        for (index, why) in [(3, "index"), (5, "pid")] {
            let mut fields = good_pane();
            fields[index] = "x";
            assert!(parse_pane_line(&fields.join("\t")).is_err(), "{why}");
        }
        let mut fields = good_pane();
        fields[5] = "-1";
        assert!(parse_pane_line(&fields.join("\t")).is_err(), "negative pid");
        for index in [0, 1, 2] {
            let mut fields = good_pane();
            fields[index] = "";
            assert!(
                parse_pane_line(&fields.join("\t")).is_err(),
                "empty id {index}"
            );
        }
    }

    #[test]
    fn parse_lines_reports_bad_lines_with_their_number_and_keeps_the_rest() {
        let good = good_pane().join("\t");
        let out = format!("{good}\n\nthis is broken\n{good}\n");
        let (rows, problems) = parse_lines(&out, parse_pane_line);
        assert_eq!(rows.len(), 2);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("line 3 ("), "{problems:?}");
        assert!(problems[0].contains("this is broken"), "{problems:?}");
        let (rows, problems) = parse_lines("", parse_pane_line);
        assert!(rows.is_empty() && problems.is_empty());
    }

    // ---- take_snapshot with a scripted tmux and a fake /proc --------------------------

    enum Reply {
        Out(String),
        NoServer,
        Fail(&'static str),
    }

    impl Reply {
        fn out(text: impl Into<String>) -> Self {
            Reply::Out(text.into())
        }

        fn result(&self) -> Result<String, TmuxError> {
            match self {
                Reply::Out(text) => Ok(text.clone()),
                Reply::NoServer => Err(TmuxError::NoServer),
                Reply::Fail(message) => Err(TmuxError::Command((*message).to_string())),
            }
        }
    }

    struct FakeTmux {
        sessions: Reply,
        windows: Reply,
        panes: Reply,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeTmux {
        fn new(sessions: &str, windows: &str, panes: &str) -> Self {
            Self {
                sessions: Reply::out(sessions),
                windows: Reply::out(windows),
                panes: Reply::out(panes),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TmuxRunner for FakeTmux {
        fn run(&self, args: &[String]) -> Result<String, TmuxError> {
            self.calls.lock().unwrap().push(args.to_vec());
            match args.first().map(String::as_str) {
                Some("list-sessions") => self.sessions.result(),
                Some("list-windows") => self.windows.result(),
                Some("list-panes") => self.panes.result(),
                other => panic!("take_snapshot must not run {other:?}"),
            }
        }
    }

    /// One `list-sessions -F SESSION_FORMAT` line.
    fn session_line(id: &str, name: &str, created: i64, group: Option<&str>) -> String {
        [
            "4242",
            "1700000000",
            "/tmp/tmux-1000/default",
            "3.4",
            id,
            "1",
            "0",
            &created.to_string(),
            &created.to_string(),
            if group.is_some() { "1" } else { "0" },
            group.unwrap_or(""),
            name,
        ]
        .join("\t")
    }

    fn window_row(session: &str, window: &str, index: i64, active: bool, name: &str) -> String {
        [
            session,
            window,
            &index.to_string(),
            if active { "1" } else { "0" },
            "0",
            "200",
            "50",
            "0",
            &format!("lay-{window}"),
            name,
        ]
        .join("\t")
    }

    fn pane_row(
        session: &str,
        window: &str,
        pane: &str,
        index: i64,
        pid: u32,
        path: &str,
    ) -> String {
        [
            session,
            window,
            pane,
            &index.to_string(),
            if index == 0 { "1" } else { "0" },
            &pid.to_string(),
            "sh",
            path,
        ]
        .join("\t")
    }

    fn lines(rows: &[String]) -> String {
        rows.iter().map(|row| format!("{row}\n")).collect()
    }

    /// A detector that sees no processes at all.
    fn blind_detector() -> (tempfile::TempDir, ClaudeDetector) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("proc")).unwrap();
        let detector = ClaudeDetector {
            proc_root: root.path().join("proc"),
            config_dir: root.path().join("cfg"),
        };
        (root, detector)
    }

    #[test]
    fn take_snapshot_asks_tmux_for_exactly_the_documented_listings() {
        let (_root, detector) = blind_detector();
        let tmux = FakeTmux::new(
            &lines(&[session_line("$0", "main", 100, None)]),
            &lines(&[window_row("$0", "@0", 0, true, "w")]),
            &lines(&[pane_row("$0", "@0", "%0", 0, 5, "/tmp")]),
        );
        take_snapshot(&tmux, &detector).unwrap();
        let args = |parts: &[&str]| {
            parts
                .iter()
                .map(|p| (*p).to_string())
                .collect::<Vec<String>>()
        };
        assert_eq!(
            tmux.calls(),
            vec![
                args(&["list-sessions", "-F", tmux::SESSION_FORMAT]),
                args(&["list-windows", "-a", "-F", WINDOW_FORMAT]),
                args(&["list-panes", "-a", "-F", PANE_FORMAT]),
            ]
        );
    }

    #[test]
    fn take_snapshot_joins_and_orders_everything() {
        let (_root, detector) = blind_detector();
        let tmux = FakeTmux::new(
            &lines(&[
                session_line("$0", "alpha", 100, Some("alpha")),
                session_line("$1", "beta session", 200, None),
            ]),
            &lines(&[
                window_row("$1", "@5", 0, true, "日本語 name"),
                window_row("$0", "@3", 3, false, "logs"),
                window_row("$0", "@1", 0, true, "ed\tit"),
            ]),
            &lines(&[
                pane_row("$0", "@3", "%9", 0, 30, "/var/log"),
                pane_row("$0", "@1", "%4", 1, 12, "/home/u/b c"),
                pane_row("$1", "@5", "%7", 0, 50, "/srv"),
                pane_row("$0", "@1", "%2", 0, 11, "/home/u/a"),
            ]),
        );

        let snap = take_snapshot(&tmux, &detector).unwrap();

        assert_eq!(snap.version, SNAPSHOT_VERSION);
        assert_eq!(snap.server, identity(1_700_000_000, 4242));
        assert_eq!(snap.server.id(), "1700000000-4242");
        let layout: Vec<SessionView> = snap
            .sessions
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.group.as_deref(),
                    s.windows
                        .iter()
                        .map(|w| {
                            (
                                w.index,
                                w.name.as_str(),
                                w.panes
                                    .iter()
                                    .map(|p| (p.index, p.id.as_str(), p.cwd.as_str()))
                                    .collect(),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            layout,
            vec![
                (
                    "alpha",
                    Some("alpha"),
                    vec![
                        (
                            0,
                            "ed\tit",
                            vec![(0, "%2", "/home/u/a"), (1, "%4", "/home/u/b c")]
                        ),
                        (3, "logs", vec![(0, "%9", "/var/log")]),
                    ]
                ),
                (
                    "beta session",
                    None,
                    vec![(0, "日本語 name", vec![(0, "%7", "/srv")])]
                ),
            ]
        );
        assert_eq!(snap.sessions[0].id, "$0");
        assert_eq!(snap.sessions[0].created, 100);
        assert_eq!(snap.sessions[0].windows[1].layout, "lay-@3");
        assert_eq!(
            (
                snap.sessions[0].windows[1].width,
                snap.sessions[0].windows[1].height
            ),
            (200, 50)
        );
        assert!(snap.sessions[0].windows[0].active && !snap.sessions[0].windows[1].active);
        assert!(snap.sessions[0].windows[0].panes[0].active);
        assert_eq!(snap.sessions[0].windows[0].panes[1].pid, 12);
        assert_eq!(snap.sessions[0].windows[0].panes[0].current_command, "sh");
    }

    #[test]
    fn take_snapshot_stamps_the_current_time() {
        let (_root, detector) = blind_detector();
        let tmux = FakeTmux::new(
            &lines(&[session_line("$0", "a", 1, None)]),
            &lines(&[window_row("$0", "@0", 0, true, "w")]),
            &lines(&[pane_row("$0", "@0", "%0", 0, 5, "/")]),
        );
        let before = util::now_ms();
        let snap = take_snapshot(&tmux, &detector).unwrap();
        let after = util::now_ms();
        assert!(
            (before..=after).contains(&snap.taken_at),
            "{}",
            snap.taken_at
        );
    }

    #[test]
    fn take_snapshot_lists_the_windows_of_grouped_sessions_under_each_member() {
        let (_root, detector) = blind_detector();
        let tmux = FakeTmux::new(
            &lines(&[
                session_line("$0", "one", 100, Some("one")),
                session_line("$1", "two", 101, Some("one")),
            ]),
            &lines(&[
                window_row("$0", "@0", 0, true, "w0"),
                window_row("$0", "@1", 4, false, "w4"),
                window_row("$1", "@0", 0, false, "w0"),
                window_row("$1", "@1", 4, true, "w4"),
            ]),
            &lines(&[
                pane_row("$0", "@0", "%0", 0, 5, "/a"),
                pane_row("$0", "@1", "%1", 0, 6, "/b"),
                pane_row("$1", "@0", "%0", 0, 5, "/a"),
                pane_row("$1", "@1", "%1", 0, 6, "/b"),
            ]),
        );
        let snap = take_snapshot(&tmux, &detector).unwrap();
        assert_eq!(snap.sessions.len(), 2);
        for s in &snap.sessions {
            assert_eq!(s.group.as_deref(), Some("one"));
            assert_eq!(s.windows.len(), 2);
            assert_eq!(s.windows[0].panes[0].id, "%0");
            assert_eq!(s.windows[1].panes[0].id, "%1");
        }
        // Each session has its own current window.
        assert!(snap.sessions[0].windows[0].active);
        assert!(snap.sessions[1].windows[1].active);
    }

    #[test]
    fn take_snapshot_skips_rows_for_things_that_appeared_or_vanished_between_calls() {
        let (_root, detector) = blind_detector();
        let tmux = FakeTmux::new(
            &lines(&[
                session_line("$0", "stays", 100, None),
                session_line("$1", "vanished", 101, None),
                session_line("$2", "windowless", 102, None),
            ]),
            &lines(&[
                window_row("$0", "@0", 0, true, "ok"),
                window_row("$0", "@1", 1, false, "no panes any more"),
                window_row("$1", "@2", 0, true, "gone"),
                window_row("$9", "@9", 0, true, "belongs to a session created later"),
            ]),
            &lines(&[
                pane_row("$0", "@0", "%0", 0, 5, "/a"),
                pane_row("$0", "@8", "%8", 0, 6, "/window created after list-windows"),
                pane_row("$9", "@9", "%9", 0, 7, "/new session"),
                pane_row("$1", "@2", "%2", 0, 8, "/vanished session still listed"),
            ]),
        );
        let snap = take_snapshot(&tmux, &detector).unwrap();
        // "vanished" has a window and a pane here, so it is kept; "windowless" and the rows
        // for unknown sessions or windows are dropped.
        let names: Vec<&str> = snap.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["stays", "vanished"]);
        assert_eq!(snap.sessions[0].windows.len(), 1);
        assert_eq!(snap.sessions[0].windows[0].panes.len(), 1);
    }

    #[test]
    fn take_snapshot_with_everything_vanished_is_no_sessions() {
        let (_root, detector) = blind_detector();
        let tmux = FakeTmux::new(&lines(&[session_line("$0", "gone", 100, None)]), "", "");
        match take_snapshot(&tmux, &detector) {
            Err(TmuxError::Command(message)) => assert_eq!(message, "no sessions"),
            other => panic!("expected no sessions, got {other:?}"),
        }
    }

    #[test]
    fn take_snapshot_of_a_server_without_sessions_is_no_sessions_and_asks_nothing_more() {
        let (_root, detector) = blind_detector();
        let tmux = FakeTmux::new("", "", "");
        match take_snapshot(&tmux, &detector) {
            Err(TmuxError::Command(message)) => assert_eq!(message, "no sessions"),
            other => panic!("expected no sessions, got {other:?}"),
        }
        assert_eq!(tmux.calls().len(), 1);
    }

    #[test]
    fn take_snapshot_passes_no_server_through_from_every_call() {
        let (_root, detector) = blind_detector();
        let sessions = lines(&[session_line("$0", "a", 1, None)]);
        for failing in 0..3 {
            let mut tmux = FakeTmux::new(
                &sessions,
                &lines(&[window_row("$0", "@0", 0, true, "w")]),
                &lines(&[pane_row("$0", "@0", "%0", 0, 5, "/")]),
            );
            match failing {
                0 => tmux.sessions = Reply::NoServer,
                1 => tmux.windows = Reply::NoServer,
                _ => tmux.panes = Reply::NoServer,
            }
            assert!(
                matches!(take_snapshot(&tmux, &detector), Err(TmuxError::NoServer)),
                "call {failing}"
            );
        }
    }

    #[test]
    fn take_snapshot_passes_other_errors_through() {
        let (_root, detector) = blind_detector();
        let mut tmux = FakeTmux::new(&lines(&[session_line("$0", "a", 1, None)]), "", "");
        tmux.windows = Reply::Fail("boom");
        assert!(matches!(
            take_snapshot(&tmux, &detector),
            Err(TmuxError::Command(message)) if message == "boom"
        ));
        let tmux = FakeTmux::new("this is not a session line\n", "", "");
        assert!(matches!(
            take_snapshot(&tmux, &detector),
            Err(TmuxError::Parse(_))
        ));
    }

    #[test]
    fn take_snapshot_survives_unreadable_lines() {
        let (_root, detector) = blind_detector();
        // A directory name with a newline splits one pane line into two.
        let tmux = FakeTmux::new(
            &lines(&[session_line("$0", "a", 1, None)]),
            &lines(&[window_row("$0", "@0", 0, true, "w")]),
            &format!(
                "{}\n{}\nsecond-half-of-the-path\n",
                pane_row("$0", "@0", "%0", 0, 5, "/tmp/first"),
                pane_row("$0", "@0", "%1", 1, 6, "/tmp/odd\nname").replace("\nname", "")
            ),
        );
        let snap = take_snapshot(&tmux, &detector).unwrap();
        assert_eq!(snap.sessions[0].windows[0].panes.len(), 2);
    }

    #[test]
    fn take_snapshot_still_works_when_proc_cannot_be_scanned() {
        let root = tempfile::tempdir().unwrap();
        let detector = ClaudeDetector {
            proc_root: root.path().join("no-such-proc"),
            config_dir: root.path().join("cfg"),
        };
        let tmux = FakeTmux::new(
            &lines(&[session_line("$0", "a", 1, None)]),
            &lines(&[window_row("$0", "@0", 0, true, "w")]),
            &lines(&[pane_row("$0", "@0", "%0", 0, 5, "/tmp")]),
        );
        let snap = take_snapshot(&tmux, &detector).unwrap();
        let only = &snap.sessions[0].windows[0].panes[0];
        assert_eq!(
            (only.foreground_cmdline.clone(), only.claude.clone()),
            (None, None)
        );
    }

    /// Writes `<proc>/<pid>/{stat,cmdline}` for a fake machine.
    fn add_process(
        proc_root: &Path,
        pid: u32,
        ppid: u32,
        pgrp: u32,
        tpgid: i32,
        comm: &str,
        argv: &[&str],
    ) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("stat"),
            format!(
                "{pid} ({comm}) S {ppid} {pgrp} {pgrp} 34816 {tpgid} 4194560 100 0 0 0 1 2 0 0 20 0 1 0 {} 1000000 200 0",
                5_000 + pid
            ),
        )
        .unwrap();
        let mut cmdline = Vec::new();
        for arg in argv {
            cmdline.extend_from_slice(arg.as_bytes());
            cmdline.push(0);
        }
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn take_snapshot_records_the_foreground_program_and_claude() {
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let config_dir = root.path().join("cfg");
        // Pane 1: a shell running `tail -f /dev/null` in the foreground.
        add_process(&proc_root, 100, 1, 100, 150, "sh", &["-sh"]);
        add_process(
            &proc_root,
            150,
            100,
            150,
            150,
            "tail",
            &["tail", "-f", "/dev/null"],
        );
        // Pane 2: a shell whose child is Claude.
        add_process(&proc_root, 200, 1, 200, 210, "sh", &["-sh"]);
        add_process(
            &proc_root,
            210,
            200,
            210,
            210,
            "claude",
            &["claude", "--dangerously-skip-permissions"],
        );
        fs::create_dir_all(config_dir.join("sessions")).unwrap();
        fs::write(
            config_dir.join("sessions").join("210.json"),
            format!(
                r#"{{"pid":210,"sessionId":"{SESSION_ID}","cwd":"/work/project","procStart":"{}","kind":"interactive","name":"n"}}"#,
                5_000 + 210
            ),
        )
        .unwrap();
        // Pane 3: an idle shell (the shell owns the terminal).
        add_process(&proc_root, 300, 1, 300, 300, "sh", &["-sh"]);
        let detector = ClaudeDetector {
            proc_root,
            config_dir,
        };

        let tmux = FakeTmux::new(
            &lines(&[session_line("$0", "main", 1, None)]),
            &lines(&[window_row("$0", "@0", 0, true, "w")]),
            &lines(&[
                pane_row("$0", "@0", "%0", 0, 100, "/a"),
                pane_row("$0", "@0", "%1", 1, 200, "/b"),
                pane_row("$0", "@0", "%2", 2, 300, "/c"),
                pane_row("$0", "@0", "%3", 3, 999, "/d"),
            ]),
        );
        let snap = take_snapshot(&tmux, &detector).unwrap();
        let panes = &snap.sessions[0].windows[0].panes;

        assert_eq!(
            panes[0].foreground_cmdline,
            Some(vec![
                "tail".to_string(),
                "-f".to_string(),
                "/dev/null".to_string()
            ])
        );
        assert_eq!(panes[0].claude, None);

        assert_eq!(
            panes[1].foreground_cmdline,
            Some(vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string()
            ])
        );
        assert_eq!(
            panes[1].claude,
            Some(ClaudeSnap {
                pid: 210,
                session_id: Some(SESSION_ID.to_string()),
                cwd: "/work/project".to_string(),
                args: vec!["--dangerously-skip-permissions".to_string()],
                kind: Some("interactive".to_string()),
                name: Some("n".to_string()),
                source: ClaudeIdSource::SessionsFile,
            })
        );

        assert_eq!(
            (panes[2].foreground_cmdline.clone(), panes[2].claude.clone()),
            (None, None)
        );
        // A pane whose process is not in /proc at all.
        assert_eq!(
            (panes[3].foreground_cmdline.clone(), panes[3].claude.clone()),
            (None, None)
        );
    }

    #[test]
    fn session_info_is_all_that_take_snapshot_needs_from_list_sessions() {
        // Guards the field names that assemble_sessions relies on.
        let info = SessionInfo {
            id: "$1".to_string(),
            name: "n".to_string(),
            windows: 1,
            attached: 0,
            created: 5,
            activity: 6,
            group: Some("g".to_string()),
            claude_panes: 0,
        };
        let (_root, detector) = blind_detector();
        let rows = vec![WindowRow {
            session_id: "$1".to_string(),
            window: window("@1", 0, "w", vec![]),
        }];
        let panes = vec![PaneRow {
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            pane: pane("%1", 0, true, "/x"),
        }];
        let built = assemble_sessions(vec![info], rows, panes, &ProcTable::default(), &detector);
        assert_eq!(built.len(), 1);
        assert_eq!(
            (built[0].created, built[0].group.as_deref()),
            (5, Some("g"))
        );
    }
}
