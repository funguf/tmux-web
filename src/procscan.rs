//! /proc scanning: process table, parent/child relations and per-process readers (owned by the core unit).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: i32,
    pub tpgid: i32,
    pub start_ticks: u64,
    pub comm: String,
}

#[derive(Clone, Debug, Default)]
pub struct ProcTable {
    infos: HashMap<u32, ProcInfo>,
    /// parent pid -> child pids, ascending. Built once so that walking the descendants of
    /// every pane does not rescan the whole table.
    children: HashMap<u32, Vec<u32>>,
}

impl ProcTable {
    /// Reads `<proc_root>/<pid>/stat` for every numeric directory. Processes that vanish while
    /// being read, or whose stat cannot be parsed, are skipped; only an unreadable
    /// `proc_root` is an error.
    pub fn scan(proc_root: &Path) -> io::Result<ProcTable> {
        let mut infos = Vec::new();
        for entry in fs::read_dir(proc_root)? {
            let Ok(entry) = entry else { continue };
            let Some(pid) = entry.file_name().to_str().and_then(parse_pid) else {
                continue;
            };
            let Ok(bytes) = fs::read(entry.path().join("stat")) else {
                continue;
            };
            if let Some(mut info) = parse_stat(&String::from_utf8_lossy(&bytes)) {
                // The directory name is what other /proc files are looked up by.
                info.pid = pid;
                infos.push(info);
            }
        }
        Ok(ProcTable::from_infos(infos))
    }

    pub fn from_infos(infos: Vec<ProcInfo>) -> ProcTable {
        let infos: HashMap<u32, ProcInfo> =
            infos.into_iter().map(|info| (info.pid, info)).collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for info in infos.values() {
            // A process that lists itself as its parent must not become its own descendant.
            if info.ppid != info.pid {
                children.entry(info.ppid).or_default().push(info.pid);
            }
        }
        for pids in children.values_mut() {
            pids.sort_unstable();
        }
        ProcTable { infos, children }
    }

    pub fn get(&self, pid: u32) -> Option<&ProcInfo> {
        self.infos.get(&pid)
    }

    /// Ascending pid.
    pub fn children(&self, pid: u32) -> Vec<u32> {
        self.children.get(&pid).cloned().unwrap_or_default()
    }

    /// Breadth-first, excludes `pid`.
    pub fn descendants(&self, pid: u32) -> Vec<u32> {
        let mut found = Vec::new();
        let mut seen = HashSet::from([pid]);
        let mut queue = VecDeque::from([pid]);
        while let Some(parent) = queue.pop_front() {
            for child in self.children.get(&parent).into_iter().flatten().copied() {
                // `seen` also guards against a corrupt table whose parent links form a cycle.
                if seen.insert(child) {
                    found.push(child);
                    queue.push_back(child);
                }
            }
        }
        found
    }

    /// Foreground process of the terminal whose session leader is `shell_pid`: the process whose
    /// pid == tpgid (else the lowest pid with pgrp == tpgid). None when tpgid <= 0 or equals the shell's pgrp.
    pub fn foreground_pid(&self, shell_pid: u32) -> Option<u32> {
        let shell = self.infos.get(&shell_pid)?;
        if shell.tpgid <= 0 || shell.tpgid == shell.pgrp {
            return None;
        }
        if let Some(leader) = u32::try_from(shell.tpgid)
            .ok()
            .filter(|pid| self.infos.contains_key(pid))
        {
            return Some(leader);
        }
        // The group leader has exited but other members of the group are still running.
        self.infos
            .values()
            .filter(|info| info.pgrp == shell.tpgid)
            .map(|info| info.pid)
            .min()
    }
}

/// A /proc directory name that is a pid: ASCII digits only (so no sign, no whitespace).
fn parse_pid(name: &str) -> Option<u32> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// Parse /proc/<pid>/stat (comm may contain spaces and ')' — split at the LAST ')').
///
/// Layout: `pid (comm) state ppid pgrp session tty_nr tpgid flags … starttime …`; numbering the
/// fields as in proc(5), `state` is field 3 and `starttime` field 22, so after the closing
/// parenthesis `ppid` is the 2nd token, `pgrp` the 3rd, `tpgid` the 6th and `starttime` the 20th.
pub fn parse_stat(content: &str) -> Option<ProcInfo> {
    let open = content.find('(')?;
    let close = content.rfind(')')?;
    if close < open {
        return None;
    }
    let pid = content[..open].trim().parse().ok()?;
    let comm = content[open + 1..close].to_string();
    let rest: Vec<&str> = content[close + 1..].split_whitespace().collect();
    Some(ProcInfo {
        pid,
        ppid: rest.get(1)?.parse().ok()?,
        pgrp: rest.get(2)?.parse().ok()?,
        tpgid: rest.get(5)?.parse().ok()?,
        start_ticks: rest.get(19)?.parse().ok()?,
        comm,
    })
}

/// `<proc_root>/<pid>/cmdline` split at NUL. None when it cannot be read or is empty (kernel
/// threads and zombies have no command line). Invalid UTF-8 is replaced.
pub fn read_cmdline(proc_root: &Path, pid: u32) -> Option<Vec<String>> {
    let bytes = fs::read(proc_path(proc_root, pid, "cmdline")).ok()?;
    split_nul_terminated(&bytes)
}

/// Where the process currently is: the target of `<proc_root>/<pid>/cwd`.
pub fn read_cwd(proc_root: &Path, pid: u32) -> Option<PathBuf> {
    fs::read_link(proc_path(proc_root, pid, "cwd")).ok()
}

/// The value of one variable from `<proc_root>/<pid>/environ` (the environment the process was
/// started with). Only the requested value is ever returned; the rest of the environment is
/// not retained. The first entry wins, like `getenv`.
pub fn read_environ_var(proc_root: &Path, pid: u32, key: &str) -> Option<String> {
    let bytes = fs::read(proc_path(proc_root, pid, "environ")).ok()?;
    lookup_environ(&bytes, key)
}

fn proc_path(proc_root: &Path, pid: u32, file: &str) -> PathBuf {
    proc_root.join(pid.to_string()).join(file)
}

fn split_nul_terminated(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.is_empty() {
        return None;
    }
    // Every argument ends with a NUL, so the last one does too. Empty arguments in the middle
    // are real arguments and are kept.
    let body = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    Some(
        body.split(|byte| *byte == 0)
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

fn lookup_environ(bytes: &[u8], key: &str) -> Option<String> {
    if key.is_empty() || key.contains('=') {
        return None;
    }
    bytes.split(|byte| *byte == 0).find_map(|entry| {
        let value = entry.strip_prefix(key.as_bytes())?.strip_prefix(b"=")?;
        Some(String::from_utf8_lossy(value).into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // ---- helpers -------------------------------------------------------------------

    /// A stat line with every field filled in; `rest` holds the 20 tokens after `)`:
    /// state ppid pgrp session tty_nr tpgid flags minflt … starttime.
    fn stat_line(pid: u32, comm: &str, ppid: u32, pgrp: i32, tpgid: i32, start: u64) -> String {
        format!(
            "{pid} ({comm}) S {ppid} {pgrp} {pgrp} 34816 {tpgid} 4194560 100 0 0 0 1 2 0 0 20 0 1 0 {start} 1000000 200 18446744073709551615 1 1 0"
        )
    }

    fn info(pid: u32, ppid: u32, pgrp: i32, tpgid: i32, comm: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            pgrp,
            tpgid,
            start_ticks: u64::from(pid) * 10,
            comm: comm.to_string(),
        }
    }

    /// Adds `<root>/<pid>/stat` and optionally cmdline, environ and a cwd symlink.
    fn write_proc(root: &Path, pid: u32, stat: &str) -> PathBuf {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stat"), stat).unwrap();
        dir
    }

    // ---- parse_stat -----------------------------------------------------------------

    #[test]
    fn parse_stat_reads_the_documented_fields() {
        let parsed = parse_stat(&stat_line(4242, "bash", 1, 4242, 4300, 123_456)).unwrap();
        assert_eq!(
            parsed,
            ProcInfo {
                pid: 4242,
                ppid: 1,
                pgrp: 4242,
                tpgid: 4300,
                start_ticks: 123_456,
                comm: "bash".to_string(),
            }
        );
    }

    #[test]
    fn parse_stat_of_a_real_looking_line() {
        // Field 22 (starttime) is 18195475, as in the Claude sessions file's procStart.
        let line = "683573 (claude) S 683000 683573 683000 34816 683573 4194304 5000 0 0 0 100 50 0 0 20 0 12 0 18195475 9999 888 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        let parsed = parse_stat(line).unwrap();
        assert_eq!(parsed.pid, 683_573);
        assert_eq!(parsed.ppid, 683_000);
        assert_eq!(parsed.pgrp, 683_573);
        assert_eq!(parsed.tpgid, 683_573);
        assert_eq!(parsed.start_ticks, 18_195_475);
        assert_eq!(parsed.comm, "claude");
    }

    #[test]
    fn parse_stat_keeps_spaces_and_parentheses_in_comm() {
        for comm in [
            "my prog",
            "a) b",
            "(sd-pam)",
            "evil) S 1 2 3 4 5",
            "x (y) z",
            ")",
            "(",
            "",
        ] {
            let parsed = parse_stat(&stat_line(77, comm, 5, 77, -1, 999))
                .unwrap_or_else(|| panic!("comm {comm:?} should parse"));
            assert_eq!(parsed.comm, comm);
            assert_eq!(parsed.pid, 77);
            assert_eq!(parsed.ppid, 5, "comm {comm:?}");
            assert_eq!(parsed.pgrp, 77, "comm {comm:?}");
            assert_eq!(parsed.tpgid, -1, "comm {comm:?}");
            assert_eq!(parsed.start_ticks, 999, "comm {comm:?}");
        }
    }

    #[test]
    fn parse_stat_accepts_a_trailing_newline_as_read_from_proc() {
        let line = format!("{}\n", stat_line(9, "sleep", 1, 9, 9, 5));
        assert_eq!(parse_stat(&line).unwrap().start_ticks, 5);
    }

    #[test]
    fn parse_stat_handles_negative_group_ids() {
        let parsed = parse_stat(&stat_line(9, "daemon", 1, 9, -1, 5)).unwrap();
        assert_eq!(parsed.tpgid, -1);
    }

    #[test]
    fn parse_stat_rejects_malformed_content() {
        let good = stat_line(1, "init", 0, 1, 1, 1);
        let too_short = "1 (init) S 0 1 1 0 1 4194560 100 0 0 0 1 2 0 0 20 0 1 0";
        for (why, bad) in [
            ("empty", String::new()),
            ("no parentheses", "1 init S 0 1 1".to_string()),
            ("only an open paren", "1 (init S 0 1 1".to_string()),
            ("only a close paren", "1 init) S 0 1 1".to_string()),
            ("parens reversed", ") (".to_string()),
            ("non numeric pid", good.replacen("1 (", "x (", 1)),
            ("missing pid", good.replacen("1 (", "(", 1)),
            ("truncated before starttime", too_short.to_string()),
            ("non numeric ppid", good.replace(") S 0 ", ") S zero ")),
            (
                "non numeric starttime",
                good.replace(" 1 1000000 ", " later 1000000 "),
            ),
        ] {
            assert_eq!(parse_stat(&bad), None, "{why}: {bad:?}");
        }
    }

    // ---- ProcTable relations --------------------------------------------------------

    fn sample_table() -> ProcTable {
        // 1
        // ├── 10 (shell A) ── 11 ── 14
        // │                └─ 12 ── 13
        // └── 20 (shell B)
        ProcTable::from_infos(vec![
            info(1, 0, 1, -1, "init"),
            info(20, 1, 20, 20, "bash"),
            info(14, 11, 10, 11, "leaf"),
            info(10, 1, 10, 10, "bash"),
            info(13, 12, 12, 12, "leaf"),
            info(11, 10, 11, 11, "mid"),
            info(12, 10, 12, 12, "mid"),
        ])
    }

    #[test]
    fn get_returns_the_process_or_none() {
        let table = sample_table();
        assert_eq!(table.get(11).unwrap().comm, "mid");
        assert_eq!(table.get(999), None);
        assert_eq!(ProcTable::default().get(1), None);
    }

    #[test]
    fn children_are_in_ascending_pid_order_whatever_the_input_order() {
        let table = sample_table();
        assert_eq!(table.children(1), vec![10, 20]);
        assert_eq!(table.children(10), vec![11, 12]);
        assert_eq!(table.children(11), vec![14]);
        assert_eq!(table.children(14), Vec::<u32>::new());
        assert_eq!(table.children(999), Vec::<u32>::new());
    }

    #[test]
    fn descendants_are_breadth_first_and_exclude_the_start() {
        let table = sample_table();
        assert_eq!(table.descendants(10), vec![11, 12, 14, 13]);
        assert_eq!(table.descendants(1), vec![10, 20, 11, 12, 14, 13]);
        assert_eq!(table.descendants(20), Vec::<u32>::new());
        assert_eq!(table.descendants(999), Vec::<u32>::new());
    }

    #[test]
    fn descendants_of_an_unknown_parent_still_follow_the_links() {
        // The parent itself is not in the table (it vanished), its children are.
        let table = ProcTable::from_infos(vec![info(5, 4, 5, 5, "a"), info(6, 5, 6, 6, "b")]);
        assert_eq!(table.descendants(4), vec![5, 6]);
    }

    #[test]
    fn descendants_survive_cycles_and_self_parents() {
        let table = ProcTable::from_infos(vec![
            info(2, 2, 2, 2, "self"),
            info(3, 4, 3, 3, "a"),
            info(4, 3, 4, 4, "b"),
        ]);
        assert_eq!(table.children(2), Vec::<u32>::new());
        assert_eq!(table.descendants(2), Vec::<u32>::new());
        assert_eq!(table.descendants(3), vec![4]);
        assert_eq!(table.descendants(4), vec![3]);
    }

    #[test]
    fn from_infos_keeps_the_last_entry_for_a_duplicate_pid() {
        let table = ProcTable::from_infos(vec![info(7, 1, 7, 7, "old"), info(7, 1, 7, 7, "new")]);
        assert_eq!(table.get(7).unwrap().comm, "new");
        assert_eq!(table.children(1), vec![7]);
    }

    // ---- foreground_pid -------------------------------------------------------------

    #[test]
    fn foreground_is_the_process_whose_pid_is_the_terminal_group() {
        // Shell 100 (pgrp 100) runs job 150 in the foreground.
        let table = ProcTable::from_infos(vec![
            info(100, 1, 100, 150, "bash"),
            info(150, 100, 150, 150, "tail"),
            info(151, 100, 150, 150, "grep"),
        ]);
        assert_eq!(table.foreground_pid(100), Some(150));
    }

    #[test]
    fn foreground_is_none_when_the_shell_owns_the_terminal() {
        let table = ProcTable::from_infos(vec![
            info(100, 1, 100, 100, "bash"),
            info(150, 100, 150, 100, "sleep"),
        ]);
        assert_eq!(table.foreground_pid(100), None);
    }

    #[test]
    fn foreground_is_none_without_a_foreground_group() {
        for tpgid in [0, -1, -5] {
            let table = ProcTable::from_infos(vec![info(100, 1, 100, tpgid, "bash")]);
            assert_eq!(table.foreground_pid(100), None, "tpgid {tpgid}");
        }
    }

    #[test]
    fn foreground_is_none_for_an_unknown_shell() {
        let table = ProcTable::from_infos(vec![info(100, 1, 100, 150, "bash")]);
        assert_eq!(table.foreground_pid(555), None);
        assert_eq!(ProcTable::default().foreground_pid(1), None);
    }

    #[test]
    fn foreground_falls_back_to_the_lowest_member_when_the_leader_is_gone() {
        // Group 150's leader exited; 153 and 152 remain.
        let table = ProcTable::from_infos(vec![
            info(100, 1, 100, 150, "bash"),
            info(153, 100, 150, 150, "worker"),
            info(152, 100, 150, 150, "worker"),
            info(160, 100, 160, 150, "other"),
        ]);
        assert_eq!(table.foreground_pid(100), Some(152));
    }

    #[test]
    fn foreground_is_none_when_the_group_has_no_live_member() {
        let table = ProcTable::from_infos(vec![info(100, 1, 100, 150, "bash")]);
        assert_eq!(table.foreground_pid(100), None);
    }

    // ---- scan -------------------------------------------------------------------------

    #[test]
    fn scan_reads_numeric_directories_only() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 1, &stat_line(1, "init", 0, 1, -1, 5));
        write_proc(root.path(), 42, &stat_line(42, "my shell", 1, 42, 99, 77));
        // Not processes: non-numeric names, signed numbers, plain files.
        fs::create_dir(root.path().join("self")).unwrap();
        fs::write(
            root.path().join("self").join("stat"),
            stat_line(8, "x", 1, 8, 8, 8),
        )
        .unwrap();
        fs::create_dir(root.path().join("+7")).unwrap();
        fs::write(
            root.path().join("+7").join("stat"),
            stat_line(7, "x", 1, 7, 7, 7),
        )
        .unwrap();
        fs::write(root.path().join("uptime"), "1.0 2.0").unwrap();

        let table = ProcTable::scan(root.path()).unwrap();

        assert_eq!(table.get(1).unwrap().comm, "init");
        let shell = table.get(42).unwrap();
        assert_eq!(shell.comm, "my shell");
        assert_eq!((shell.ppid, shell.pgrp, shell.tpgid), (1, 42, 99));
        assert_eq!(shell.start_ticks, 77);
        assert_eq!(table.get(8), None);
        assert_eq!(table.get(7), None);
        assert_eq!(table.children(1), vec![42]);
    }

    #[test]
    fn scan_skips_processes_whose_stat_is_missing_or_garbage() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 5, &stat_line(5, "ok", 1, 5, 5, 5));
        write_proc(root.path(), 6, "garbage without parentheses");
        fs::create_dir(root.path().join("7")).unwrap(); // no stat file at all
        let table = ProcTable::scan(root.path()).unwrap();
        assert!(table.get(5).is_some());
        assert_eq!(table.get(6), None);
        assert_eq!(table.get(7), None);
    }

    #[test]
    fn scan_keys_a_process_by_its_directory_name() {
        let root = tempfile::tempdir().unwrap();
        write_proc(root.path(), 12, &stat_line(999, "renamed", 1, 12, 12, 1));
        let table = ProcTable::scan(root.path()).unwrap();
        assert_eq!(table.get(12).unwrap().pid, 12);
        assert_eq!(table.get(999), None);
    }

    #[test]
    fn scan_of_a_missing_proc_root_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let err = ProcTable::scan(&root.path().join("nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn scan_of_the_real_proc_sees_this_process_and_its_parent() {
        let table = ProcTable::scan(Path::new("/proc")).unwrap();
        let me = table.get(std::process::id()).expect("this process");
        assert!(!me.comm.is_empty());
        assert!(me.start_ticks > 0);
        let parent = me.ppid;
        assert!(table.children(parent).contains(&me.pid));
        assert!(table.descendants(parent).contains(&me.pid));
    }

    // ---- read_cmdline -----------------------------------------------------------------

    #[test]
    fn read_cmdline_splits_at_nul_and_drops_only_the_final_terminator() {
        let root = tempfile::tempdir().unwrap();
        let dir = write_proc(root.path(), 3, &stat_line(3, "x", 1, 3, 3, 3));
        fs::write(dir.join("cmdline"), b"claude\0--resume\0abc\0").unwrap();
        assert_eq!(
            read_cmdline(root.path(), 3).unwrap(),
            vec!["claude", "--resume", "abc"]
        );
    }

    #[test]
    fn read_cmdline_keeps_empty_arguments() {
        let root = tempfile::tempdir().unwrap();
        let dir = write_proc(root.path(), 3, &stat_line(3, "x", 1, 3, 3, 3));
        fs::write(dir.join("cmdline"), b"prog\0\0last\0\0").unwrap();
        assert_eq!(
            read_cmdline(root.path(), 3).unwrap(),
            vec!["prog", "", "last", ""]
        );
    }

    #[test]
    fn read_cmdline_accepts_a_missing_final_terminator() {
        let root = tempfile::tempdir().unwrap();
        let dir = write_proc(root.path(), 3, &stat_line(3, "x", 1, 3, 3, 3));
        fs::write(dir.join("cmdline"), b"a\0b").unwrap();
        assert_eq!(read_cmdline(root.path(), 3).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn read_cmdline_keeps_spaces_and_replaces_invalid_utf8() {
        let root = tempfile::tempdir().unwrap();
        let dir = write_proc(root.path(), 3, &stat_line(3, "x", 1, 3, 3, 3));
        fs::write(dir.join("cmdline"), b"echo\0two words\0caf\xff\0").unwrap();
        assert_eq!(
            read_cmdline(root.path(), 3).unwrap(),
            vec![
                "echo".to_string(),
                "two words".to_string(),
                "caf\u{fffd}".to_string()
            ]
        );
    }

    #[test]
    fn read_cmdline_is_none_for_empty_or_missing_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = write_proc(root.path(), 3, &stat_line(3, "kthread", 2, 0, -1, 3));
        fs::write(dir.join("cmdline"), b"").unwrap();
        assert_eq!(read_cmdline(root.path(), 3), None);
        assert_eq!(read_cmdline(root.path(), 4), None);
    }

    // ---- read_cwd / read_environ_var ----------------------------------------------------

    #[test]
    fn read_cwd_follows_the_symlink() {
        let root = tempfile::tempdir().unwrap();
        let dir = write_proc(root.path(), 3, &stat_line(3, "x", 1, 3, 3, 3));
        symlink("/srv/project dir", dir.join("cwd")).unwrap();
        assert_eq!(
            read_cwd(root.path(), 3),
            Some(PathBuf::from("/srv/project dir"))
        );
        assert_eq!(read_cwd(root.path(), 4), None);
    }

    #[test]
    fn read_cwd_of_a_real_process_matches_current_dir() {
        let cwd = read_cwd(Path::new("/proc"), std::process::id()).unwrap();
        assert_eq!(cwd, std::env::current_dir().unwrap());
    }

    fn write_environ(root: &Path, bytes: &[u8]) {
        let dir = write_proc(root, 3, &stat_line(3, "x", 1, 3, 3, 3));
        fs::write(dir.join("environ"), bytes).unwrap();
    }

    #[test]
    fn read_environ_var_finds_exact_keys_only() {
        let root = tempfile::tempdir().unwrap();
        write_environ(
            root.path(),
            b"CLAUDE_CONFIG_DIR_OLD=/old\0CLAUDE_CONFIG_DIR=/cfg dir\0PATH=/usr/bin\0",
        );
        assert_eq!(
            read_environ_var(root.path(), 3, "CLAUDE_CONFIG_DIR").as_deref(),
            Some("/cfg dir")
        );
        assert_eq!(
            read_environ_var(root.path(), 3, "PATH").as_deref(),
            Some("/usr/bin")
        );
        assert_eq!(read_environ_var(root.path(), 3, "CLAUDE_CONFIG"), None);
        assert_eq!(read_environ_var(root.path(), 3, "HOME"), None);
    }

    #[test]
    fn read_environ_var_first_entry_wins_and_values_may_contain_equals() {
        let root = tempfile::tempdir().unwrap();
        write_environ(root.path(), b"A=1\0A=2\0B=x=y=z\0C=\0");
        assert_eq!(read_environ_var(root.path(), 3, "A").as_deref(), Some("1"));
        assert_eq!(
            read_environ_var(root.path(), 3, "B").as_deref(),
            Some("x=y=z")
        );
        assert_eq!(read_environ_var(root.path(), 3, "C").as_deref(), Some(""));
    }

    #[test]
    fn read_environ_var_rejects_degenerate_keys_and_missing_files() {
        let root = tempfile::tempdir().unwrap();
        write_environ(root.path(), b"=oops\0A=1\0");
        assert_eq!(read_environ_var(root.path(), 3, ""), None);
        assert_eq!(read_environ_var(root.path(), 3, "A=1"), None);
        assert_eq!(read_environ_var(root.path(), 3, "="), None);
        assert_eq!(read_environ_var(root.path(), 9, "A"), None);
    }

    #[test]
    fn read_environ_var_of_a_real_process_sees_a_variable_it_was_started_with() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("TMW_CORE_PROBE", "probe value")
            .spawn()
            .unwrap();
        // The kernel publishes a new program's environment a moment after exec returns to
        // the parent, so an immediate read can come back empty.
        let mut value = None;
        for _ in 0..200 {
            value = read_environ_var(Path::new("/proc"), child.id(), "TMW_CORE_PROBE");
            if value.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(value.as_deref(), Some("probe value"));
    }
}
