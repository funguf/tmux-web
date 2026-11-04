//! Claude Code process detection and session-id lookup (owned by the core unit).

use crate::procscan;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClaudeIdSource {
    SessionsFile,
    Cmdline,
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaudeSnap {
    pub pid: u32,
    pub session_id: Option<String>,
    pub cwd: String,
    /// argv after the claude program token
    pub args: Vec<String>,
    /// "interactive", … from the sessions file
    pub kind: Option<String>,
    pub name: Option<String>,
    pub source: ClaudeIdSource,
}

#[derive(Clone, Debug)]
pub struct ClaudeDetector {
    pub proc_root: PathBuf,
    pub config_dir: PathBuf,
}

/// A sessions file is a few hundred bytes; anything much larger is not one.
const MAX_SESSIONS_FILE_BYTES: u64 = 1 << 20;

/// Program names that run a script: `node …/cli.js`, `sh …/claude`.
const INTERPRETERS: [&str; 5] = ["node", "bun", "deno", "sh", "bash"];

impl ClaudeDetector {
    /// `proc_root` is "/proc".
    pub fn new(config_dir: PathBuf) -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            config_dir,
        }
    }

    /// Breadth-first over [pane_pid] + descendants; the first claude process found wins.
    pub fn detect(&self, table: &procscan::ProcTable, pane_pid: u32) -> Option<ClaudeSnap> {
        std::iter::once(pane_pid)
            .chain(table.descendants(pane_pid))
            .find_map(|pid| self.inspect(table, pid))
    }

    /// The Claude Code snapshot of one process, or None when it is not Claude Code.
    fn inspect(&self, table: &procscan::ProcTable, pid: u32) -> Option<ClaudeSnap> {
        let info = table.get(pid)?;
        let argv = procscan::read_cmdline(&self.proc_root, pid).unwrap_or_default();
        if !is_claude_process(&info.comm, &argv) {
            return None;
        }
        let args = claude_args(&argv);
        let file = self.read_sessions_file(pid, info.start_ticks);
        let (session_id, source) = match file.as_ref().and_then(|f| f.session_id.clone()) {
            Some(id) => (Some(id), ClaudeIdSource::SessionsFile),
            None => match session_id_from_args(&args) {
                Some(id) => (Some(id), ClaudeIdSource::Cmdline),
                None => (None, ClaudeIdSource::None),
            },
        };
        let cwd = file
            .as_ref()
            .and_then(|f| f.cwd.clone())
            .or_else(|| {
                procscan::read_cwd(&self.proc_root, pid)
                    .map(|dir| dir.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        Some(ClaudeSnap {
            pid,
            session_id,
            cwd,
            args,
            kind: file.as_ref().and_then(|f| f.kind.clone()),
            name: file.and_then(|f| f.name),
            source,
        })
    }

    /// `<dir>/sessions/<pid>.json`, where `<dir>` is the `CLAUDE_CONFIG_DIR` the process was
    /// started with, else the configured directory. Claude rewrites this file whenever the
    /// session id changes (/clear, /resume), so it is read afresh for every snapshot.
    fn read_sessions_file(&self, pid: u32, start_ticks: u64) -> Option<SessionFile> {
        let path = self
            .config_dir_of(pid)
            .join("sessions")
            .join(format!("{pid}.json"));
        parse_sessions_file(&read_small_text(&path)?, start_ticks)
    }

    fn config_dir_of(&self, pid: u32) -> PathBuf {
        let from_environ = procscan::read_environ_var(&self.proc_root, pid, "CLAUDE_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        match from_environ {
            Some(dir) if dir.is_absolute() => dir,
            // A relative directory is relative to where the process runs.
            Some(dir) => procscan::read_cwd(&self.proc_root, pid)
                .map_or_else(|| self.config_dir.clone(), |cwd| cwd.join(dir)),
            None => self.config_dir.clone(),
        }
    }
}

/// The fields of `sessions/<pid>.json` that are used.
#[derive(Debug, Default, PartialEq, Eq)]
struct SessionFile {
    session_id: Option<String>,
    cwd: Option<String>,
    kind: Option<String>,
    name: Option<String>,
}

fn read_small_text(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_SESSIONS_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (u64::try_from(text.len()).ok()? <= MAX_SESSIONS_FILE_BYTES).then_some(text)
}

/// None when the text is not a JSON object, or when its `procStart` (a string or a number)
/// differs from the process start ticks: the file then belongs to an earlier process that had
/// the same pid. A file without `procStart` cannot be checked and is accepted.
fn parse_sessions_file(text: &str, start_ticks: u64) -> Option<SessionFile> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    if let Some(proc_start) = object.get("procStart").filter(|value| !value.is_null()) {
        let recorded = match proc_start {
            serde_json::Value::String(text) => text.trim().parse::<u64>().ok(),
            serde_json::Value::Number(number) => number.as_u64(),
            _ => None,
        };
        if recorded != Some(start_ticks) {
            return None;
        }
    }
    let text_of = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Some(SessionFile {
        session_id: text_of("sessionId").filter(|id| usable_session_id(id)),
        cwd: text_of("cwd"),
        kind: text_of("kind"),
        name: text_of("name"),
    })
}

/// Final path component of `path`, or `path` itself when it has none.
fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

/// `interpreter script …` where the interpreter is node, bun, deno, sh or bash and the script
/// is Claude Code: a file called `claude`, or the npm package's `cli.js` / `cli.mjs`.
fn is_interpreted_claude(argv: &[String]) -> bool {
    let (Some(program), Some(script)) = (argv.first(), argv.get(1)) else {
        return false;
    };
    INTERPRETERS.contains(&basename(program))
        && (basename(script) == "claude"
            || (script.contains("claude-code")
                && (script.ends_with("cli.js") || script.ends_with("cli.mjs"))))
}

/// A process is Claude Code when `comm == "claude"`, or basename(argv[0]) == "claude", or
/// argv[0] is an interpreter (node, bun, deno, sh, bash) running the claude script.
pub fn is_claude_process(comm: &str, argv: &[String]) -> bool {
    comm == "claude"
        || argv
            .first()
            .is_some_and(|program| basename(program) == "claude")
        || is_interpreted_claude(argv)
}

/// argv after the claude program token: after the script for `node cli.js …` / `sh claude …`,
/// else after argv[0].
pub fn claude_args(argv: &[String]) -> Vec<String> {
    let program_tokens = if is_interpreted_claude(argv) { 2 } else { 1 };
    argv.iter().skip(program_tokens).cloned().collect()
}

/// The session id Claude was started with: `--resume <id>`, `--resume=<id>`, `-r <id>`,
/// `--session-id <id>` or `--session-id=<id>`. When several are given the last valid one wins.
pub fn session_id_from_args(args: &[String]) -> Option<String> {
    let mut found = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            break;
        }
        let candidate = match arg {
            "--resume" | "-r" | "--session-id" => match args.get(index + 1) {
                // `--resume` alone opens a picker; the next token is then another option.
                Some(value) if !value.starts_with('-') => {
                    index += 1;
                    Some(value.as_str())
                }
                _ => None,
            },
            _ => arg
                .strip_prefix("--resume=")
                .or_else(|| arg.strip_prefix("--session-id=")),
        };
        if let Some(id) = candidate.filter(|id| usable_session_id(id)) {
            found = Some(id.to_string());
        }
        index += 1;
    }
    found
}

/// 8..=64 chars of [0-9A-Za-z-]
pub fn is_valid_session_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A valid id that cannot be mistaken for an option when it follows `--resume`.
fn usable_session_id(id: &str) -> bool {
    is_valid_session_id(id) && !id.starts_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procscan::{ProcInfo, ProcTable};
    use std::os::unix::fs::symlink;

    const SESSION: &str = "0b6f6a59-3c1e-4c55-9f0e-1d2a3b4c5d6e";
    const OTHER_SESSION: &str = "11111111-2222-3333-4444-555555555555";

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    fn snap(args: &[&str], session_id: Option<&str>) -> ClaudeSnap {
        ClaudeSnap {
            pid: 1,
            session_id: session_id.map(str::to_string),
            cwd: "/work".to_string(),
            args: strings(args),
            kind: Some("interactive".to_string()),
            name: None,
            source: ClaudeIdSource::SessionsFile,
        }
    }

    // ---- is_valid_session_id --------------------------------------------------------

    #[test]
    fn session_id_validity_bounds() {
        assert!(is_valid_session_id(SESSION));
        assert!(is_valid_session_id("abcdefgh")); // 8
        assert!(!is_valid_session_id("abcdefg")); // 7
        assert!(is_valid_session_id(&"a".repeat(64)));
        assert!(!is_valid_session_id(&"a".repeat(65)));
        assert!(is_valid_session_id("ABC-123-xyz"));
        assert!(is_valid_session_id("--------")); // the contract only constrains length and charset
        for bad in [
            "",
            "abcdefg_",
            "abcdefgh ",
            "abc defgh",
            "abcdefgh\n",
            "abcdefgé",
            "abcdefg.",
            "abcdefg/",
            "日本語日本語日本語",
        ] {
            assert!(!is_valid_session_id(bad), "{bad:?}");
        }
    }

    // ---- is_claude_process / claude_args --------------------------------------------

    #[test]
    fn claude_is_recognised_by_comm_or_program_name() {
        assert!(is_claude_process("claude", &[]));
        assert!(is_claude_process(
            "claude",
            &strings(&["something", "else"])
        ));
        assert!(is_claude_process(
            "2.1.311",
            &strings(&["claude", "--resume", SESSION])
        ));
        assert!(is_claude_process(
            "x",
            &strings(&["/home/u/.local/bin/claude", "-c"])
        ));
        assert!(is_claude_process("x", &strings(&["claude/"])));
    }

    #[test]
    fn claude_is_recognised_behind_an_interpreter() {
        for interpreter in [
            "node",
            "bun",
            "deno",
            "sh",
            "bash",
            "/bin/sh",
            "/usr/bin/node",
        ] {
            assert!(
                is_claude_process("x", &strings(&[interpreter, "/path/to/claude", "--flag"])),
                "{interpreter} claude"
            );
            assert!(
                is_claude_process(
                    "x",
                    &strings(&[
                        interpreter,
                        "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
                    ])
                ),
                "{interpreter} cli.js"
            );
        }
        assert!(is_claude_process(
            "node",
            &strings(&["node", "/opt/claude-code/dist/cli.mjs", "--resume", SESSION])
        ));
    }

    #[test]
    fn other_processes_are_not_claude() {
        for (comm, argv) in [
            ("bash", strings(&["bash"])),
            ("node", strings(&["node", "server.js"])),
            ("node", strings(&["node", "/opt/claude-code/server.js"])),
            ("node", strings(&["node", "/opt/other/cli.js"])),
            ("node", strings(&["node"])),
            ("python3", strings(&["python3", "/path/to/claude"])),
            ("claudette", strings(&["claudette"])),
            ("vim", strings(&["vim", "claude"])),
            ("sh", strings(&["sh", "-c", "claude-code cli.js"])),
            ("tail", strings(&["tail", "-f", "/var/log/claude"])),
            ("x", strings(&["claude.sh"])),
            ("x", strings(&["/opt/claude/bin/run"])),
        ] {
            assert!(!is_claude_process(comm, &argv), "{comm} {argv:?}");
        }
        assert!(!is_claude_process("", &[]));
    }

    #[test]
    fn claude_args_skip_the_program_token() {
        assert_eq!(
            claude_args(&strings(&[
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                SESSION
            ])),
            strings(&["--dangerously-skip-permissions", "--resume", SESSION])
        );
        assert_eq!(claude_args(&strings(&["claude"])), Vec::<String>::new());
        assert_eq!(claude_args(&[]), Vec::<String>::new());
    }

    #[test]
    fn claude_args_skip_interpreter_and_script() {
        assert_eq!(
            claude_args(&strings(&[
                "/bin/sh",
                "/path/to/claude",
                "--dangerously-skip-permissions"
            ])),
            strings(&["--dangerously-skip-permissions"])
        );
        assert_eq!(
            claude_args(&strings(&[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                "-c",
                "fix it"
            ])),
            strings(&["-c", "fix it"])
        );
        assert_eq!(
            claude_args(&strings(&["bash", "claude"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn claude_args_of_an_interpreter_not_running_claude_skip_only_argv0() {
        // comm said "claude" but the command line is not an interpreted claude script.
        assert_eq!(
            claude_args(&strings(&["node", "other.js", "x"])),
            strings(&["other.js", "x"])
        );
    }

    // ---- session_id_from_args -------------------------------------------------------

    #[test]
    fn session_id_comes_from_every_documented_spelling() {
        let cases: [(&[&str], &str); 6] = [
            (&["--resume", SESSION], SESSION),
            (
                &["--resume=11111111-2222-3333-4444-555555555555"],
                OTHER_SESSION,
            ),
            (&["-r", SESSION], SESSION),
            (&["--session-id", OTHER_SESSION], OTHER_SESSION),
            (
                &["--session-id=0b6f6a59-3c1e-4c55-9f0e-1d2a3b4c5d6e"],
                SESSION,
            ),
            (
                &[
                    "--dangerously-skip-permissions",
                    "--model",
                    "opus",
                    "--resume",
                    SESSION,
                ],
                SESSION,
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(
                session_id_from_args(&strings(args)).as_deref(),
                Some(expected),
                "{args:?}"
            );
        }
    }

    #[test]
    fn session_id_ignores_invalid_and_missing_values() {
        for args in [
            vec![],
            vec!["--resume"],
            vec!["-r"],
            vec!["--session-id"],
            vec!["--resume", "--verbose"],
            vec!["--resume", "fix the bug"],
            vec!["--resume", "short"],
            vec!["--resume", "not_a_valid_id_chars"],
            vec!["--resume=short"],
            vec!["--resume", "--looks-like-a-flag-123"],
            vec!["-c", SESSION],
            vec!["--continue"],
            vec!["--name", SESSION],
            vec!["--", "--resume", SESSION],
            vec![SESSION],
        ] {
            assert_eq!(session_id_from_args(&strings(&args)), None, "{args:?}");
        }
    }

    #[test]
    fn session_id_last_valid_value_wins_and_flags_are_not_swallowed() {
        assert_eq!(
            session_id_from_args(&strings(&["--resume", SESSION, "--resume", OTHER_SESSION]))
                .as_deref(),
            Some(OTHER_SESSION)
        );
        assert_eq!(
            session_id_from_args(&strings(&["--resume", SESSION, "--resume", "bogus"])).as_deref(),
            Some(SESSION)
        );
        // `--resume` without a value must not eat the next flag's value.
        assert_eq!(
            session_id_from_args(&strings(&["--resume", "--session-id", SESSION])).as_deref(),
            Some(SESSION)
        );
    }

    // ---- detect, with a fake /proc --------------------------------------------------

    /// `<root>/proc` and `<root>/cfg`, the way the detector sees a machine.
    struct FakeMachine {
        root: tempfile::TempDir,
    }

    impl FakeMachine {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join("proc")).unwrap();
            fs::create_dir_all(root.path().join("cfg").join("sessions")).unwrap();
            Self { root }
        }

        fn proc_root(&self) -> PathBuf {
            self.root.path().join("proc")
        }

        fn config_dir(&self) -> PathBuf {
            self.root.path().join("cfg")
        }

        fn detector(&self) -> ClaudeDetector {
            ClaudeDetector {
                proc_root: self.proc_root(),
                config_dir: self.config_dir(),
            }
        }

        /// Writes a process into the fake /proc and returns its info.
        fn process(&self, pid: u32, ppid: u32, comm: &str, argv: &[&str]) -> ProcInfo {
            let info = ProcInfo {
                pid,
                ppid,
                pgrp: i32::try_from(pid).unwrap(),
                tpgid: i32::try_from(pid).unwrap(),
                start_ticks: 1_000 + u64::from(pid),
                comm: comm.to_string(),
            };
            let dir = self.proc_root().join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            let mut cmdline = Vec::new();
            for arg in argv {
                cmdline.extend_from_slice(arg.as_bytes());
                cmdline.push(0);
            }
            fs::write(dir.join("cmdline"), cmdline).unwrap();
            info
        }

        fn set_cwd(&self, pid: u32, cwd: &str) {
            symlink(cwd, self.proc_root().join(pid.to_string()).join("cwd")).unwrap();
        }

        fn set_environ(&self, pid: u32, entries: &[&str]) {
            let mut bytes = Vec::new();
            for entry in entries {
                bytes.extend_from_slice(entry.as_bytes());
                bytes.push(0);
            }
            fs::write(
                self.proc_root().join(pid.to_string()).join("environ"),
                bytes,
            )
            .unwrap();
        }

        fn write_sessions_file(&self, dir: &Path, pid: u32, json: &str) {
            let sessions = dir.join("sessions");
            fs::create_dir_all(&sessions).unwrap();
            fs::write(sessions.join(format!("{pid}.json")), json).unwrap();
        }

        fn sessions_json(&self, info: &ProcInfo, session_id: &str, cwd: &str) -> String {
            format!(
                r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"{cwd}","startedAt":1793119790299,"procStart":"{start}","version":"2.1.311","kind":"interactive","entrypoint":"cli","name":"u-fb","status":"busy","updatedAt":1793120221401}}"#,
                pid = info.pid,
                start = info.start_ticks
            )
        }
    }

    fn table(infos: &[&ProcInfo]) -> ProcTable {
        ProcTable::from_infos(infos.iter().map(|info| (*info).clone()).collect())
    }

    #[test]
    fn detect_reads_the_sessions_file() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let claude = machine.process(
            200,
            100,
            "claude",
            &[
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                OTHER_SESSION,
            ],
        );
        machine.set_cwd(200, "/proc-says/here");
        machine.write_sessions_file(
            &machine.config_dir(),
            200,
            &machine.sessions_json(&claude, SESSION, "/home/u/project"),
        );

        let found = machine
            .detector()
            .detect(&table(&[&shell, &claude]), 100)
            .unwrap();

        assert_eq!(
            found,
            ClaudeSnap {
                pid: 200,
                session_id: Some(SESSION.to_string()),
                cwd: "/home/u/project".to_string(),
                args: strings(&["--dangerously-skip-permissions", "--resume", OTHER_SESSION]),
                kind: Some("interactive".to_string()),
                name: Some("u-fb".to_string()),
                source: ClaudeIdSource::SessionsFile,
            }
        );
    }

    #[test]
    fn detect_accepts_proc_start_as_a_number_or_absent() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let claude = machine.process(200, 100, "claude", &["claude"]);
        for json in [
            format!(
                r#"{{"sessionId":"{SESSION}","cwd":"/w","procStart":{}}}"#,
                claude.start_ticks
            ),
            format!(r#"{{"sessionId":"{SESSION}","cwd":"/w"}}"#),
            format!(r#"{{"sessionId":"{SESSION}","cwd":"/w","procStart":null}}"#),
            format!(
                r#"{{"sessionId":"{SESSION}","cwd":"/w","procStart":" {} "}}"#,
                claude.start_ticks
            ),
        ] {
            machine.write_sessions_file(&machine.config_dir(), 200, &json);
            let found = machine
                .detector()
                .detect(&table(&[&shell, &claude]), 100)
                .unwrap();
            assert_eq!(found.source, ClaudeIdSource::SessionsFile, "{json}");
            assert_eq!(found.session_id.as_deref(), Some(SESSION), "{json}");
            assert_eq!(found.cwd, "/w", "{json}");
        }
    }

    #[test]
    fn detect_ignores_a_stale_sessions_file() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let claude = machine.process(200, 100, "claude", &["claude", "--resume", OTHER_SESSION]);
        machine.set_cwd(200, "/proc-says/here");
        for json in [
            // procStart of an earlier process with the same pid
            format!(
                r#"{{"sessionId":"{SESSION}","cwd":"/stale","procStart":"{}"}}"#,
                claude.start_ticks + 1
            ),
            format!(
                r#"{{"sessionId":"{SESSION}","cwd":"/stale","procStart":{}}}"#,
                claude.start_ticks - 1
            ),
            // unusable procStart values cannot vouch for the file
            format!(r#"{{"sessionId":"{SESSION}","cwd":"/stale","procStart":"soon"}}"#),
            format!(r#"{{"sessionId":"{SESSION}","cwd":"/stale","procStart":true}}"#),
            format!(r#"{{"sessionId":"{SESSION}","cwd":"/stale","procStart":-1}}"#),
        ] {
            machine.write_sessions_file(&machine.config_dir(), 200, &json);
            let found = machine
                .detector()
                .detect(&table(&[&shell, &claude]), 100)
                .unwrap();
            // Falls back to the command line and to /proc for the directory.
            assert_eq!(found.source, ClaudeIdSource::Cmdline, "{json}");
            assert_eq!(found.session_id.as_deref(), Some(OTHER_SESSION), "{json}");
            assert_eq!(found.cwd, "/proc-says/here", "{json}");
            assert_eq!(found.kind, None, "{json}");
            assert_eq!(found.name, None, "{json}");
        }
    }

    #[test]
    fn detect_ignores_unreadable_sessions_files() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let claude = machine.process(200, 100, "claude", &["claude"]);
        machine.set_cwd(200, "/proc-says/here");
        let detector = machine.detector();
        let table = table(&[&shell, &claude]);

        // No file at all.
        let found = detector.detect(&table, 100).unwrap();
        assert_eq!(
            (found.source, found.session_id.clone()),
            (ClaudeIdSource::None, None)
        );

        for garbage in ["", "not json", "[1,2]", "42", "null", "{\"truncated\":"] {
            machine.write_sessions_file(&machine.config_dir(), 200, garbage);
            let found = detector.detect(&table, 100).unwrap();
            assert_eq!(found.source, ClaudeIdSource::None, "{garbage:?}");
            assert_eq!(found.cwd, "/proc-says/here", "{garbage:?}");
        }

        // Bigger than any real sessions file.
        let huge = format!(
            r#"{{"sessionId":"{SESSION}","cwd":"/w","padding":"{}"}}"#,
            "x".repeat(usize::try_from(MAX_SESSIONS_FILE_BYTES).unwrap())
        );
        machine.write_sessions_file(&machine.config_dir(), 200, &huge);
        assert_eq!(
            detector.detect(&table, 100).unwrap().source,
            ClaudeIdSource::None
        );
    }

    #[test]
    fn detect_uses_the_cmdline_when_the_file_has_no_valid_session_id() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let claude = machine.process(
            200,
            100,
            "claude",
            &["claude", "--session-id", OTHER_SESSION],
        );
        for bad_id in [
            r#""short""#,
            "123",
            "null",
            r#""has space in it""#,
            r#""-------------""#,
        ] {
            let json = format!(
                r#"{{"sessionId":{bad_id},"cwd":"/from/file","kind":"interactive","name":"n","procStart":"{}"}}"#,
                claude.start_ticks
            );
            machine.write_sessions_file(&machine.config_dir(), 200, &json);
            let found = machine
                .detector()
                .detect(&table(&[&shell, &claude]), 100)
                .unwrap();
            assert_eq!(found.source, ClaudeIdSource::Cmdline, "{bad_id}");
            assert_eq!(found.session_id.as_deref(), Some(OTHER_SESSION), "{bad_id}");
            // The rest of a fresh file is still used.
            assert_eq!(found.cwd, "/from/file", "{bad_id}");
            assert_eq!(found.kind.as_deref(), Some("interactive"), "{bad_id}");
            assert_eq!(found.name.as_deref(), Some("n"), "{bad_id}");
        }
    }

    #[test]
    fn detect_without_any_id_reports_source_none() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let claude = machine.process(200, 100, "claude", &["claude", "-c", "--verbose"]);
        machine.set_cwd(200, "/where/it/runs");
        let found = machine
            .detector()
            .detect(&table(&[&shell, &claude]), 100)
            .unwrap();
        assert_eq!(found.session_id, None);
        assert_eq!(found.source, ClaudeIdSource::None);
        assert_eq!(found.cwd, "/where/it/runs");
        assert_eq!(found.args, strings(&["-c", "--verbose"]));
    }

    #[test]
    fn detect_without_cwd_anywhere_gives_an_empty_cwd() {
        let machine = FakeMachine::new();
        let claude = machine.process(200, 1, "claude", &["claude"]);
        let found = machine.detector().detect(&table(&[&claude]), 200).unwrap();
        assert_eq!(found.cwd, "");
    }

    #[test]
    fn detect_prefers_the_config_dir_from_the_process_environment() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let claude = machine.process(200, 100, "claude", &["claude"]);
        let other_dir = machine.root.path().join("other-config");
        machine.set_environ(
            200,
            &[
                "HOME=/home/u",
                &format!("CLAUDE_CONFIG_DIR={}", other_dir.display()),
            ],
        );
        // The file in the detector's own directory must not be used.
        machine.write_sessions_file(
            &machine.config_dir(),
            200,
            &machine.sessions_json(&claude, OTHER_SESSION, "/default"),
        );
        machine.write_sessions_file(
            &other_dir,
            200,
            &machine.sessions_json(&claude, SESSION, "/from-env-dir"),
        );

        let found = machine
            .detector()
            .detect(&table(&[&shell, &claude]), 100)
            .unwrap();

        assert_eq!(found.session_id.as_deref(), Some(SESSION));
        assert_eq!(found.cwd, "/from-env-dir");
    }

    #[test]
    fn detect_falls_back_to_the_configured_dir_for_an_unset_or_empty_variable() {
        let machine = FakeMachine::new();
        let claude = machine.process(200, 1, "claude", &["claude"]);
        machine.write_sessions_file(
            &machine.config_dir(),
            200,
            &machine.sessions_json(&claude, SESSION, "/default"),
        );
        for environ in [&["HOME=/home/u"][..], &["CLAUDE_CONFIG_DIR="][..]] {
            machine.set_environ(200, environ);
            let found = machine.detector().detect(&table(&[&claude]), 200).unwrap();
            assert_eq!(found.session_id.as_deref(), Some(SESSION), "{environ:?}");
            assert_eq!(found.cwd, "/default", "{environ:?}");
        }
        // A process whose environ cannot be read at all behaves the same.
        let unreadable = machine.process(201, 1, "claude", &["claude"]);
        machine.write_sessions_file(
            &machine.config_dir(),
            201,
            &machine.sessions_json(&unreadable, OTHER_SESSION, "/default-2"),
        );
        let found = machine
            .detector()
            .detect(&table(&[&unreadable]), 201)
            .unwrap();
        assert_eq!(found.session_id.as_deref(), Some(OTHER_SESSION));
        assert_eq!(found.cwd, "/default-2");
    }

    #[test]
    fn detect_resolves_a_relative_config_dir_against_the_process_cwd() {
        let machine = FakeMachine::new();
        let claude = machine.process(200, 1, "claude", &["claude"]);
        let work = machine.root.path().join("work");
        machine.set_cwd(200, work.to_str().unwrap());
        machine.set_environ(200, &["CLAUDE_CONFIG_DIR=rel-config"]);
        machine.write_sessions_file(
            &work.join("rel-config"),
            200,
            &machine.sessions_json(&claude, SESSION, "/rel"),
        );
        let found = machine.detector().detect(&table(&[&claude]), 200).unwrap();
        assert_eq!(found.session_id.as_deref(), Some(SESSION));
    }

    #[test]
    fn detect_looks_up_the_sessions_file_by_pid() {
        let machine = FakeMachine::new();
        let claude = machine.process(200, 1, "claude", &["claude"]);
        machine.write_sessions_file(
            &machine.config_dir(),
            201,
            &machine.sessions_json(&claude, SESSION, "/other-pid"),
        );
        let found = machine.detector().detect(&table(&[&claude]), 200).unwrap();
        assert_eq!(found.source, ClaudeIdSource::None);
    }

    #[test]
    fn detect_is_breadth_first_and_the_first_claude_wins() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let wrapper = machine.process(110, 100, "node", &["node", "wrapper.js"]);
        // Deeper but with a lower pid than the direct child below.
        let inner = machine.process(111, 110, "claude", &["claude", "--resume", OTHER_SESSION]);
        let direct = machine.process(120, 100, "claude", &["claude", "--resume", SESSION]);
        let found = machine
            .detector()
            .detect(&table(&[&shell, &wrapper, &inner, &direct]), 100)
            .unwrap();
        assert_eq!(found.pid, 120);
        assert_eq!(found.session_id.as_deref(), Some(SESSION));
    }

    #[test]
    fn detect_finds_claude_below_a_wrapper_and_as_the_pane_process_itself() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let wrapper = machine.process(110, 100, "env", &["env", "A=1", "claude"]);
        let claude = machine.process(111, 110, "claude", &["claude", "-r", SESSION]);
        let tbl = table(&[&shell, &wrapper, &claude]);
        assert_eq!(machine.detector().detect(&tbl, 100).unwrap().pid, 111);
        // `tmux new-session claude`: the pane process is Claude.
        assert_eq!(machine.detector().detect(&tbl, 111).unwrap().pid, 111);
        // From the wrapper's point of view Claude is its child.
        assert_eq!(machine.detector().detect(&tbl, 110).unwrap().pid, 111);
    }

    #[test]
    fn detect_recognises_interpreted_scripts_and_strips_the_script_from_the_args() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let fake = machine.process(
            200,
            100,
            "claude",
            &[
                "/bin/sh",
                "/tmp/fake/bin/claude",
                "--dangerously-skip-permissions",
            ],
        );
        let found = machine
            .detector()
            .detect(&table(&[&shell, &fake]), 100)
            .unwrap();
        assert_eq!(found.args, strings(&["--dangerously-skip-permissions"]));

        let node = machine.process(
            300,
            100,
            "node",
            &[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                "--resume",
                SESSION,
            ],
        );
        let found = machine
            .detector()
            .detect(&table(&[&shell, &node]), 100)
            .unwrap();
        assert_eq!(found.pid, 300);
        assert_eq!(found.args, strings(&["--resume", SESSION]));
        assert_eq!(found.session_id.as_deref(), Some(SESSION));
        assert_eq!(found.source, ClaudeIdSource::Cmdline);
    }

    #[test]
    fn detect_recognises_a_claude_comm_with_an_unreadable_command_line() {
        let machine = FakeMachine::new();
        let claude = ProcInfo {
            pid: 200,
            ppid: 1,
            pgrp: 200,
            tpgid: 200,
            start_ticks: 5,
            comm: "claude".to_string(),
        };
        // No /proc/200 directory at all.
        let found = machine.detector().detect(&table(&[&claude]), 200).unwrap();
        assert_eq!(found.args, Vec::<String>::new());
        assert_eq!(found.source, ClaudeIdSource::None);
    }

    #[test]
    fn detect_returns_none_without_claude() {
        let machine = FakeMachine::new();
        let shell = machine.process(100, 1, "bash", &["-bash"]);
        let sleeper = machine.process(110, 100, "sleep", &["sleep", "1000"]);
        let editor = machine.process(120, 100, "vim", &["vim", "claude"]);
        let tbl = table(&[&shell, &sleeper, &editor]);
        assert_eq!(machine.detector().detect(&tbl, 100), None);
        assert_eq!(machine.detector().detect(&tbl, 999), None);
        assert_eq!(machine.detector().detect(&ProcTable::default(), 100), None);
    }

    #[test]
    fn detect_does_not_look_outside_the_pane_tree() {
        let machine = FakeMachine::new();
        let shell_a = machine.process(100, 1, "bash", &["-bash"]);
        let shell_b = machine.process(300, 1, "bash", &["-bash"]);
        let claude_b = machine.process(310, 300, "claude", &["claude"]);
        let tbl = table(&[&shell_a, &shell_b, &claude_b]);
        assert_eq!(machine.detector().detect(&tbl, 100), None);
        assert_eq!(machine.detector().detect(&tbl, 300).unwrap().pid, 310);
    }

    #[test]
    fn detect_works_on_the_real_proc_with_this_process_as_non_claude() {
        let table = ProcTable::scan(Path::new("/proc")).unwrap();
        let detector = ClaudeDetector::new(PathBuf::from("/nonexistent-claude-config"));
        assert_eq!(detector.proc_root, PathBuf::from("/proc"));
        assert_eq!(detector.detect(&table, std::process::id()), None);
    }

    #[test]
    fn claude_snap_serialises_with_kebab_case_sources() {
        let mut c = snap(&["--verbose"], Some(SESSION));
        for (source, text) in [
            (ClaudeIdSource::SessionsFile, "sessions-file"),
            (ClaudeIdSource::Cmdline, "cmdline"),
            (ClaudeIdSource::None, "none"),
        ] {
            c.source = source;
            let value = serde_json::to_value(&c).unwrap();
            assert_eq!(value["source"], text);
            let back: ClaudeSnap = serde_json::from_value(value).unwrap();
            assert_eq!(back, c);
        }
    }
}
