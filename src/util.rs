//! Small shared helpers: clocks, atomic file writes, private directories, randomness and `$HOME`.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix epoch milliseconds.
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).unwrap_or(i64::MAX),
        // A clock set before 1970 yields a negative timestamp instead of a panic.
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}

/// Unix epoch seconds.
pub fn now_secs() -> i64 {
    now_ms().div_euclid(1000)
}

/// Write to a temp file in the same directory, fsync, set `mode`, rename over `path`.
///
/// The temp file is named `<name>.tmp-<pid>-<random hex>` and is created exclusively with
/// `mode` (so it is never readable by more users than the final file will be). It is removed
/// again when any step after its creation fails. If `path` is a symlink, the link itself is
/// replaced, not its target.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let tmp = temp_path(path)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let result = fill_and_publish(file, &tmp, path, bytes, mode);
    if result.is_err() {
        // Best effort: the error already in hand is the one worth reporting.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The temp file used by [`atomic_write`]: a sibling of `path`, so the final rename never
/// crosses a filesystem boundary.
fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no file name: {}", path.display()),
        )
    })?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp-{}-{}", std::process::id(), random_hex(8)));
    Ok(path.with_file_name(tmp_name))
}

fn fill_and_publish(
    mut file: fs::File,
    tmp: &Path,
    path: &Path,
    bytes: &[u8],
    mode: u32,
) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    // Creation applied the umask; chmod makes the final mode exactly `mode`.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    drop(file);
    fs::rename(tmp, path)
}

/// create_dir_all + chmod 0700.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Lower-case hex of `n` random bytes (getrandom).
///
/// # Panics
///
/// Panics if the operating system's random number generator fails. That is deliberate: the
/// result is used for auth tokens and temp-file names, and a weaker fallback would silently
/// weaken authentication.
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes)
        .unwrap_or_else(|err| panic!("operating system random number generator failed: {err}"));
    hex::encode(bytes)
}

/// $HOME, falling back to the passwd entry of the current uid, then "/".
pub fn home_dir() -> PathBuf {
    pick_home(std::env::var_os("HOME"), || {
        passwd_entry().map(|user| user.dir)
    })
}

/// The decision behind [`home_dir`], with both sources injected so it is testable without
/// touching the process environment. `passwd_home` is only consulted when `env_home` is
/// unset or empty.
fn pick_home(env_home: Option<OsString>, passwd_home: impl FnOnce() -> Option<PathBuf>) -> PathBuf {
    match env_home {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => passwd_home()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("/")),
    }
}

/// The passwd entry of the current uid, when there is one.
pub(crate) fn passwd_entry() -> Option<nix::unistd::User> {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn entry_names(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<OsString> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn clocks_are_plausible_and_consistent() {
        let ms = now_ms();
        let secs = now_secs();
        // After 2024-01-01 and before 2100-01-01.
        assert!(ms > 1_704_067_200_000, "now_ms too small: {ms}");
        assert!(ms < 4_102_444_800_000, "now_ms too large: {ms}");
        assert!(
            (secs * 1000 - ms).abs() < 5_000,
            "secs {secs} vs ms {ms} disagree"
        );
    }

    #[test]
    fn random_hex_has_requested_length_and_lowercase_hex_charset() {
        assert_eq!(random_hex(0), "");
        for n in [1, 4, 8, 32] {
            let s = random_hex(n);
            assert_eq!(s.len(), n * 2);
            assert!(
                s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "not lower-case hex: {s}"
            );
        }
    }

    #[test]
    fn random_hex_differs_between_calls() {
        assert_ne!(random_hex(16), random_hex(16));
    }

    #[test]
    fn atomic_write_creates_file_with_content_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write(&path, br#"{"a":1}"#, 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), br#"{"a":1}"#);
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn atomic_write_replaces_content_and_applies_the_new_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write(&path, b"old", 0o644).unwrap();
        assert_eq!(mode_of(&path), 0o644);
        atomic_write(&path, b"new and longer", 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new and longer");
        assert_eq!(mode_of(&path), 0o600);
        atomic_write(&path, b"", 0o640).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
        assert_eq!(mode_of(&path), 0o640);
    }

    #[test]
    fn atomic_write_mode_is_exact_whatever_the_umask() {
        // A umask of 022 would strip group/other write at creation time; the explicit chmod
        // must restore it, so the final mode is exactly what was asked for.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("open.bin");
        atomic_write(&path, b"x", 0o666).unwrap();
        assert_eq!(mode_of(&path), 0o666);
    }

    #[test]
    fn atomic_write_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        for round in 0..5 {
            atomic_write(&path, format!("round {round}").as_bytes(), 0o600).unwrap();
        }
        assert_eq!(entry_names(dir.path()), vec![OsString::from("state.json")]);
    }

    #[test]
    fn atomic_write_removes_its_temp_file_when_the_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        // Renaming a file over a non-empty directory fails after the temp file was written.
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("child"), b"keep me").unwrap();

        assert!(atomic_write(&target, b"data", 0o600).is_err());

        assert_eq!(entry_names(dir.path()), vec![OsString::from("target")]);
        assert_eq!(fs::read(target.join("child")).unwrap(), b"keep me");
    }

    #[test]
    fn atomic_write_into_a_missing_directory_fails_with_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            atomic_write(&dir.path().join("missing").join("state.json"), b"x", 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(entry_names(dir.path()).is_empty());
    }

    #[test]
    fn atomic_write_rejects_a_path_without_a_file_name() {
        for bad in ["/", ""] {
            let err = atomic_write(Path::new(bad), b"x", 0o600).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "path {bad:?}");
        }
    }

    #[test]
    fn atomic_write_does_not_follow_a_symlink_at_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"untouched").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        atomic_write(&link, b"replacement", 0o600).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
        assert_eq!(fs::read(&link).unwrap(), b"replacement");
        assert!(!fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn temp_path_is_a_sibling_named_with_pid_and_random_hex() {
        let path = Path::new("/some/dir/state.json");
        let tmp = temp_path(path).unwrap();
        assert_eq!(tmp.parent(), path.parent());
        let name = tmp.file_name().unwrap().to_str().unwrap().to_owned();
        let prefix = format!("state.json.tmp-{}-", std::process::id());
        assert!(name.starts_with(&prefix), "unexpected temp name {name}");
        let suffix = &name[prefix.len()..];
        assert_eq!(suffix.len(), 16);
        assert!(suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(tmp, temp_path(path).unwrap());
    }

    #[test]
    fn temp_path_of_a_bare_file_name_stays_relative_to_the_same_directory() {
        let path = Path::new("state.json");
        let tmp = temp_path(path).unwrap();
        assert_eq!(tmp.parent(), path.parent());
        assert!(tmp
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("state.json.tmp-"));
    }

    #[test]
    fn ensure_private_dir_creates_nested_directories_with_mode_0700() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path().join("a").join("b").join("state");
        ensure_private_dir(&leaf).unwrap();
        assert!(leaf.is_dir());
        assert_eq!(mode_of(&leaf), 0o700);
    }

    #[test]
    fn ensure_private_dir_tightens_an_existing_directory_and_keeps_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(state.join("keep"), b"data").unwrap();

        ensure_private_dir(&state).unwrap();
        ensure_private_dir(&state).unwrap(); // idempotent

        assert_eq!(mode_of(&state), 0o700);
        assert_eq!(fs::read(state.join("keep")).unwrap(), b"data");
    }

    #[test]
    fn ensure_private_dir_fails_when_the_path_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(ensure_private_dir(&file).is_err());
    }

    #[test]
    fn pick_home_prefers_a_non_empty_environment_value() {
        let consulted = Cell::new(false);
        let home = pick_home(Some(OsString::from("/env/home")), || {
            consulted.set(true);
            Some(PathBuf::from("/passwd/home"))
        });
        assert_eq!(home, PathBuf::from("/env/home"));
        assert!(
            !consulted.get(),
            "passwd must not be consulted when HOME is set"
        );
    }

    #[test]
    fn pick_home_falls_back_to_passwd_when_home_is_unset_or_empty() {
        for env_home in [None, Some(OsString::new())] {
            let home = pick_home(env_home, || Some(PathBuf::from("/passwd/home")));
            assert_eq!(home, PathBuf::from("/passwd/home"));
        }
    }

    #[test]
    fn pick_home_ends_at_root_without_environment_or_usable_passwd_entry() {
        assert_eq!(pick_home(None, || None), PathBuf::from("/"));
        assert_eq!(pick_home(None, || Some(PathBuf::new())), PathBuf::from("/"));
        assert_eq!(
            pick_home(Some(OsString::new()), || None),
            PathBuf::from("/")
        );
    }

    #[test]
    fn home_dir_is_never_empty() {
        assert!(!home_dir().as_os_str().is_empty());
    }
}
