//! Image upload handling (owned by the server unit).
//!
//! The type of an upload is decided by its magic bytes alone; the declared Content-Type is
//! only a hint and is never consulted. Files land in `<state-dir>/uploads/` (mode 0700) as
//! `img-<unixms>-<8 hex>.<ext>` with mode 0600 and are swept after the retention period.

use crate::util;
use serde::Serialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often the retention sweep runs after the one at startup.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Image types accepted for upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    pub fn extension(self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpg",
            ImageKind::Gif => "gif",
            ImageKind::Webp => "webp",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            ImageKind::Png => "image/png",
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
        }
    }
}

/// PNG `89 50 4E 47 0D 0A 1A 0A`, JPEG `FF D8 FF`, GIF `GIF87a` / `GIF89a`, WebP `RIFF????WEBP`.
pub fn detect(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageKind::Gif)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("the upload is empty")]
    Empty,
    #[error("the upload is larger than {max} bytes")]
    TooLarge { max: u64 },
    #[error("only PNG, JPEG, GIF and WebP images are accepted")]
    UnsupportedType,
    #[error("could not store the upload: {0}")]
    Io(#[from] io::Error),
}

/// `201` body of `POST /api/upload`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UploadResult {
    pub path: String,
    pub bytes: usize,
    pub mime: String,
}

pub struct UploadStore {
    dir: PathBuf,
    max_bytes: u64,
    retention: Duration,
}

impl UploadStore {
    /// `dir` is made absolute so the reported paths can be used from anywhere.
    pub fn open(dir: &Path, max_bytes: u64, retention: Duration) -> io::Result<UploadStore> {
        let dir = std::path::absolute(dir)?;
        util::ensure_private_dir(&dir)?;
        Ok(UploadStore {
            dir,
            max_bytes,
            retention,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Validates and stores `bytes`.
    pub fn save(&self, bytes: &[u8]) -> Result<UploadResult, UploadError> {
        if bytes.is_empty() {
            return Err(UploadError::Empty);
        }
        if u64::try_from(bytes.len()).map_or(true, |len| len > self.max_bytes) {
            return Err(UploadError::TooLarge {
                max: self.max_bytes,
            });
        }
        let kind = detect(bytes).ok_or(UploadError::UnsupportedType)?;
        // The directory could have been removed since startup.
        util::ensure_private_dir(&self.dir)?;
        let name = format!(
            "img-{}-{}.{}",
            util::now_ms(),
            util::random_hex(4),
            kind.extension()
        );
        let path = self.dir.join(name);
        util::atomic_write(&path, bytes, 0o600)?;
        Ok(UploadResult {
            path: path.to_string_lossy().into_owned(),
            bytes: bytes.len(),
            mime: kind.mime().to_string(),
        })
    }

    /// Deletes the uploads older than the retention period; returns how many.
    pub fn sweep(&self) -> io::Result<usize> {
        sweep_dir(&self.dir, self.retention, SystemTime::now())
    }
}

/// True for names this module could have generated: `img-<digits>-<8 hex>.<ext>`.
fn is_upload_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("img-") else {
        return false;
    };
    let Some((stem, extension)) = rest.rsplit_once('.') else {
        return false;
    };
    if !matches!(extension, "png" | "jpg" | "gif" | "webp") {
        return false;
    }
    let Some((millis, random)) = stem.split_once('-') else {
        return false;
    };
    !millis.is_empty()
        && millis.bytes().all(|b| b.is_ascii_digit())
        && random.len() == 8
        && random.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Removes regular files named like uploads whose modification time is more than `retention`
/// before `now`. Anything else in the directory is left alone, and symlinks are never followed.
pub fn sweep_dir(dir: &Path, retention: Duration, now: SystemTime) -> io::Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_name().to_str().is_some_and(is_upload_name) {
            continue;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let expired = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > retention);
        if expired && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F'];
    const GIF87: &[u8] = b"GIF87a\x01\x00\x01\x00";
    const GIF89: &[u8] = b"GIF89a\x01\x00\x01\x00";
    const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

    fn store(max: u64) -> (tempfile::TempDir, UploadStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = UploadStore::open(
            &dir.path().join("uploads"),
            max,
            Duration::from_secs(86_400),
        )
        .unwrap();
        (dir, store)
    }

    #[test]
    fn every_magic_type_is_detected_and_stored_with_the_right_extension_and_mime() {
        let (_dir, store) = store(1 << 20);
        for (bytes, extension, mime) in [
            (PNG, "png", "image/png"),
            (JPEG, "jpg", "image/jpeg"),
            (GIF87, "gif", "image/gif"),
            (GIF89, "gif", "image/gif"),
            (WEBP, "webp", "image/webp"),
        ] {
            let result = store.save(bytes).unwrap();
            assert_eq!(result.mime, mime);
            assert_eq!(result.bytes, bytes.len());
            assert!(
                result.path.ends_with(&format!(".{extension}")),
                "{}",
                result.path
            );
            assert_eq!(fs::read(&result.path).unwrap(), bytes);
        }
    }

    #[test]
    fn near_misses_are_not_images() {
        for bytes in [
            &b"\x89PNG\r\n\x1a"[..],     // PNG signature cut short
            b"\x89PNG\r\n\x1a\x0b",      // wrong last byte
            b"\xFF\xD8",                 // JPEG needs three bytes
            b"GIF88a",                   // unknown GIF version
            b"GIF89",                    // truncated
            b"RIFF\x00\x00\x00\x00WAVE", // RIFF but not WebP
            b"RIFF\x00\x00\x00\x00WEB",  // truncated
            b"<svg xmlns='http://www.w3.org/2000/svg'/>",
            b"%PDF-1.7",
            b"just text",
        ] {
            assert_eq!(detect(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn the_declared_type_is_never_consulted() {
        // `save` has no content-type parameter by construction; this documents that a text
        // payload is refused no matter what a client claims.
        let (_dir, store) = store(1 << 20);
        assert!(matches!(
            store.save(b"not an image at all"),
            Err(UploadError::UnsupportedType)
        ));
    }

    #[test]
    fn empty_and_oversize_uploads_are_refused() {
        let (_dir, store) = store(16);
        assert!(matches!(store.save(b""), Err(UploadError::Empty)));
        let mut big = PNG.to_vec();
        big.resize(17, 0);
        assert!(matches!(
            store.save(&big),
            Err(UploadError::TooLarge { max: 16 })
        ));
        // Exactly at the limit is accepted.
        let mut exact = PNG.to_vec();
        exact.resize(16, 0);
        assert_eq!(store.save(&exact).unwrap().bytes, 16);
        // Nothing was written for the refused ones.
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 1);
    }

    #[test]
    fn size_is_checked_before_the_type() {
        let (_dir, store) = store(4);
        assert!(matches!(
            store.save(b"this is not an image and too long"),
            Err(UploadError::TooLarge { .. })
        ));
    }

    #[test]
    fn files_are_private_and_named_img_ms_hex_ext() {
        let (_dir, store) = store(1 << 20);
        let before = util::now_ms();
        let result = store.save(PNG).unwrap();
        let after = util::now_ms();
        let path = Path::new(&result.path);
        assert!(path.is_absolute(), "{}", result.path);
        assert_eq!(path.parent().unwrap(), store.dir());
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(is_upload_name(name), "unexpected name {name}");
        let millis: i64 = name.split('-').nth(1).unwrap().parse().unwrap();
        assert!((before..=after).contains(&millis));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn names_are_unique_even_within_one_millisecond() {
        let (_dir, store) = store(1 << 20);
        let paths: std::collections::HashSet<_> =
            (0..50).map(|_| store.save(PNG).unwrap().path).collect();
        assert_eq!(paths.len(), 50);
    }

    #[test]
    fn upload_names_are_recognised_strictly() {
        assert!(is_upload_name("img-1793122138000-0a1b2c3d.png"));
        assert!(is_upload_name("img-1-ABCDEF01.webp"));
        for bad in [
            "img-1793122138000-0a1b2c3.png",   // 7 hex
            "img-1793122138000-0a1b2c3d4.png", // 9 hex
            "img-x-0a1b2c3d.png",
            "img--0a1b2c3d.png",
            "img-1-0a1b2c3d.txt",
            "img-1-0a1b2c3d",
            "other-1-0a1b2c3d.png",
            "img-1-0a1b2c3g.png",
            "notes.txt",
        ] {
            assert!(!is_upload_name(bad), "{bad}");
        }
    }

    #[test]
    fn sweep_removes_only_old_upload_files() {
        let dir = tempfile::tempdir().unwrap();
        let old_png = dir.path().join("img-1-00000000.png");
        let fresh_png = dir.path().join("img-2-11111111.png");
        let old_other = dir.path().join("notes.txt");
        let old_dir = dir.path().join("img-3-22222222.png.d");
        fs::write(&old_png, PNG).unwrap();
        fs::write(&fresh_png, PNG).unwrap();
        fs::write(&old_other, b"keep").unwrap();
        fs::create_dir(&old_dir).unwrap();
        let now = SystemTime::now();
        let long_ago = now - Duration::from_secs(10 * 86_400);
        for path in [&old_png, &old_other] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        let removed = sweep_dir(dir.path(), Duration::from_secs(7 * 86_400), now).unwrap();
        assert_eq!(removed, 1);
        assert!(!old_png.exists());
        assert!(fresh_png.exists());
        assert!(
            old_other.exists(),
            "files that are not uploads are never deleted"
        );
        assert!(old_dir.exists());
    }

    #[test]
    fn sweep_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("precious.png");
        fs::write(&target, PNG).unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(30 * 86_400);
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let link = dir.path().join("img-9-33333333.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let removed =
            sweep_dir(dir.path(), Duration::from_secs(86_400), SystemTime::now()).unwrap();
        assert_eq!(removed, 0);
        assert!(target.exists());
        assert!(fs::symlink_metadata(&link).is_ok());
    }

    #[test]
    fn the_store_sweeps_with_its_retention() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            UploadStore::open(&dir.path().join("u"), 1 << 20, Duration::from_secs(60)).unwrap();
        let kept = store.save(PNG).unwrap().path;
        let old = store.save(PNG).unwrap().path;
        let long_ago = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert_eq!(store.sweep().unwrap(), 1);
        assert!(Path::new(&kept).exists());
        assert!(!Path::new(&old).exists());
    }

    #[test]
    fn save_recreates_a_removed_directory() {
        let (_dir, store) = store(1 << 20);
        fs::remove_dir_all(store.dir()).unwrap();
        let result = store.save(PNG).unwrap();
        assert!(Path::new(&result.path).exists());
    }
}
