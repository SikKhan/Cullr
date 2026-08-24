//! Disk cache for extracted assets: stable paths, content-addressed file
//! names and crash-safe writes.
//!
//! Assets live under a single root (by default `~/.cache/cullr`) as plain
//! JPEGs: `previews/<hash>.jpg` and `thumbs/<hash>.jpg`. The `<hash>` is
//! derived from the photo's identity `(root, rel_path, mtime, size)` so
//! re-opening a known folder skips re-extraction whenever the identity is
//! unchanged (SPEC §4).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use md5::{Digest, Md5};
use thiserror::Error;

/// Longest edge of generated thumbnails in pixels.
pub(crate) const THUMB_LONG_EDGE: u32 = 512;

/// Failure modes of cache operations.
#[derive(Error, Debug)]
pub enum CacheError {
    /// The platform provides no user cache directory (no `$XDG_CACHE_HOME`
    /// and no fallback).
    #[error("no user cache directory available")]
    NoCacheDir,
    /// A cache asset could not be written.
    #[error("cannot write cache file `{}`", .path.display())]
    Write {
        /// Target path of the failed write.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The cache directory tree could not be listed during GC.
    #[error("cannot scan cache directory `{}`", .path.display())]
    Walk {
        /// The directory that could not be read.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Root handle over the on-disk asset cache.
///
/// Cheap to clone; all methods take `&self` and are safe to call from
/// multiple ingest workers concurrently.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Builds a cache rooted at the given directory (created lazily).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Cache rooted at the platform default (`$XDG_CACHE_HOME/cullr`).
    pub fn system_default() -> Result<Self, CacheError> {
        let base = dirs::cache_dir().ok_or(CacheError::NoCacheDir)?;
        Ok(Self::new(base.join("cullr")))
    }

    /// Directory holding full-size preview JPEGs.
    pub fn previews_dir(&self) -> PathBuf {
        self.root.join("previews")
    }

    /// Directory holding downscaled thumbnail JPEGs.
    pub fn thumbs_dir(&self) -> PathBuf {
        self.root.join("thumbs")
    }

    /// Content key of a photo: md5 over its identity tuple
    /// `(root, rel_path, mtime, size)` per SPEC §4.
    ///
    /// Any change to the file (edit, touch) yields a new hash, which is what
    /// makes stale assets unreachable and GC-able.
    pub fn asset_hash(root: &Path, rel_path: &Path, mtime_nanos: u128, size: u64) -> String {
        let mut hasher = Md5::new();
        hasher.update(root.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(rel_path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(mtime_nanos.to_le_bytes());
        hasher.update(size.to_le_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            // Infallible formatting into a pre-sized String.
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// Path under which the preview JPEG for `hash` must live.
    pub fn preview_path(&self, hash: &str) -> PathBuf {
        self.previews_dir().join(format!("{hash}.jpg"))
    }

    /// Path under which the thumbnail JPEG for `hash` must live.
    pub fn thumb_path(&self, hash: &str) -> PathBuf {
        self.thumbs_dir().join(format!("{hash}.jpg"))
    }

    /// Writes `data` to `path` atomically: bytes land in a unique temp file
    /// first and are then renamed into place, so readers never observe a
    /// half-written JPEG even across concurrent workers or crashes.
    pub fn write_atomically(&self, path: &Path, data: &[u8]) -> Result<(), CacheError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CacheError::Write {
                path: parent.to_owned(),
                source,
            })?;
        }
        let tmp = temp_sibling(path);
        {
            let mut file = fs::File::create(&tmp).map_err(|source| CacheError::Write {
                path: tmp.clone(),
                source,
            })?;
            file.write_all(data).map_err(|source| CacheError::Write {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, path).map_err(|source| CacheError::Write {
            path: path.to_owned(),
            source,
        })?;
        Ok(())
    }

    /// Deletes every cached JPEG whose hash is not in `keep`, returning how
    /// many files were removed.
    ///
    /// Orphans arise when source photos are deleted/edited; GC is best-effort
    /// — unreadable directories are reported but never abort the sweep.
    pub fn gc(&self, keep: &HashSet<String>) -> Result<usize, CacheError> {
        let mut removed = 0;
        for dir in [self.previews_dir(), self.thumbs_dir()] {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(CacheError::Walk { path: dir, source }),
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        tracing::warn!(%error, "skipping unreadable cache entry during gc");
                        continue;
                    }
                };
                let path = entry.path();
                if !path.is_file() || !has_jpeg_suffix(path.file_name()) {
                    continue;
                }
                let stale = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy())
                    .is_none_or(|stem| !keep.contains(stem.as_ref()));
                if stale {
                    match fs::remove_file(&path) {
                        Ok(()) => removed += 1,
                        Err(error) => {
                            tracing::warn!(%error, "could not remove stale cache file");
                        }
                    }
                }
            }
        }
        Ok(removed)
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique sibling path for the temp file of `target`.
fn temp_sibling(target: &Path) -> PathBuf {
    let serial = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    target.with_extension(format!("tmp.{}.{}", std::process::id(), serial))
}

fn has_jpeg_suffix(file_name: Option<&std::ffi::OsStr>) -> bool {
    file_name
        .map(|name| name.as_encoded_bytes())
        .is_some_and(|bytes| bytes.ends_with(b".jpg") || bytes.ends_with(b".JPG"))
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used)]

    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn asset_hash_should_be_deterministic_for_identical_inputs() {
        let root = Path::new("/photos");
        let rel = Path::new("IMG_0001.CR3");
        let a = Cache::asset_hash(root, rel, 1_000, 42);
        let b = Cache::asset_hash(root, rel, 1_000, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn asset_hash_should_change_when_identity_changes() {
        let root = Path::new("/photos");
        let rel = Path::new("IMG_0001.CR3");
        let base = Cache::asset_hash(root, rel, 1_000, 42);
        assert_ne!(base, Cache::asset_hash(root, rel, 2_000, 42));
        assert_ne!(base, Cache::asset_hash(root, rel, 1_000, 43));
        assert_ne!(
            base,
            Cache::asset_hash(root, Path::new("IMG_0002.CR3"), 1_000, 42)
        );
        assert_ne!(base, Cache::asset_hash(Path::new("/other"), rel, 1_000, 42));
    }

    #[test]
    fn asset_hash_should_be_a_32_char_hex_digest() {
        let hash = Cache::asset_hash(Path::new("/p"), Path::new("a.nef"), 0, 0);
        assert_eq!(hash.len(), 32);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn paths_should_follow_the_spec_layout() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().to_owned());
        assert_eq!(
            cache.preview_path("abc"),
            dir.path().join("previews").join("abc.jpg")
        );
        assert_eq!(
            cache.thumb_path("abc"),
            dir.path().join("thumbs").join("abc.jpg")
        );
    }

    #[test]
    fn write_atomically_should_create_parents_and_content() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().to_owned());
        let target = cache.preview_path("deadbeef");

        cache
            .write_atomically(&target, b"jpeg-bytes")
            .expect("write");

        assert_eq!(fs::read(&target).expect("read"), b"jpeg-bytes");
    }

    #[test]
    fn write_atomically_should_replace_existing_content_without_tmp_leftovers() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().to_owned());
        let target = cache.thumb_path("cafe");
        cache
            .write_atomically(&target, b"old")
            .expect("first write");

        cache
            .write_atomically(&target, b"new")
            .expect("second write");

        assert_eq!(fs::read(&target).expect("read"), b"new");
        let leftovers: Vec<_> = fs::read_dir(cache.thumbs_dir())
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["cafe.jpg"]);
    }

    #[test]
    fn system_default_should_agree_with_xdg_cache_dir() {
        if let Some(base) = dirs::cache_dir() {
            let expected = base.join("cullr");
            let cache = Cache::system_default().expect("system default exists when XDG does");
            assert_eq!(cache.previews_dir().parent(), Some(expected.as_path()));
        }
    }

    #[test]
    fn gc_should_remove_files_not_in_keep_set_and_count_them() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().to_owned());
        cache
            .write_atomically(&cache.preview_path("keep1"), b"p")
            .expect("w1");
        cache
            .write_atomically(&cache.preview_path("stale1"), b"p")
            .expect("w2");
        cache
            .write_atomically(&cache.thumb_path("keep1"), b"t")
            .expect("w3");
        cache
            .write_atomically(&cache.thumb_path("stale2"), b"t")
            .expect("w4");

        let mut keep = HashSet::new();
        keep.insert("keep1".to_owned());
        let removed = cache.gc(&keep).expect("gc");

        assert_eq!(removed, 2);
        assert!(cache.preview_path("keep1").is_file());
        assert!(cache.thumb_path("keep1").is_file());
        assert!(!cache.preview_path("stale1").exists());
        assert!(!cache.thumb_path("stale2").exists());
    }

    #[test]
    fn gc_should_tolerate_missing_cache_directories() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().join("never-created"));

        let removed = cache.gc(&HashSet::new()).expect("gc on empty cache");

        assert_eq!(removed, 0);
    }

    #[test]
    fn gc_should_ignore_non_jpeg_files() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().to_owned());
        fs::create_dir_all(cache.previews_dir()).expect("mkdir");
        fs::write(cache.previews_dir().join("notes.txt"), b"hi").expect("write txt");

        let removed = cache.gc(&HashSet::new()).expect("gc");

        assert_eq!(removed, 0);
        assert!(cache.previews_dir().join("notes.txt").is_file());
    }

    #[test]
    fn asset_hash_should_use_mtime_not_wall_clock() {
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let to_nanos = |t: SystemTime| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .expect("fixture epoch times")
                .as_nanos()
        };
        let a = Cache::asset_hash(Path::new("/p"), Path::new("x.arw"), to_nanos(earlier), 1);
        let b = Cache::asset_hash(Path::new("/p"), Path::new("x.arw"), to_nanos(later), 1);
        assert_ne!(a, b);
    }
}
