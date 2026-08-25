//! Folder scanning: discover RAW files under a root and report them as
//! ordered [`PhotoMeta`] entries.
//!
//! The scan is intentionally cheap (directory walk + `stat` only) so the grid
//! can show placeholders immediately while ingest catches up (SPEC §5.1).
//!
//! Cameras that write RAW+JPEG pairs (same stem, sibling file) produce one
//! entry each, not two: the companion JPEG rides on the RAW's
//! [`PhotoMeta::jpeg_rel_path`] so the sheet shows a single photo and export
//! can copy both originals.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

use crate::model::PhotoMeta;

/// RAW file extensions recognized by culling, lowercase without dot.
///
/// This is the hardcoded half of the SPEC §5.1 filter; [`crate::extract`]
/// intersects it with rawler's supported set at runtime, so this list alone
/// does not make an extension ingestable.
pub(crate) const EXTENSIONS: &[&[u8]] = &[
    b"3fr", b"ari", b"arw", b"bay", b"cr2", b"cr3", b"crw", b"dcr", b"dng", b"erf", b"fff", b"gpr",
    b"iiq", b"kdc", b"mef", b"mrw", b"nef", b"nrw", b"orf", b"pef", b"raf", b"raw", b"rw2", b"rwl",
    b"sr2", b"srw", b"x3f",
];

/// Companion-JPEG extensions eligible for RAW+JPEG pairing, lowercase
/// without dot. JPEGs are never photos of their own here — they only ever
/// attach to a same-stem RAW in the same directory.
pub(crate) const JPEG_EXTENSIONS: &[&[u8]] = &[b"jpg", b"jpeg"];

/// Knobs for a folder scan.
#[derive(Copy, Clone, Debug, Default)]
pub struct ScanOptions {
    /// When `false` (the default), only direct children of the root are
    /// scanned; when `true`, subfolders are walked recursively.
    pub recursive: bool,
}

/// Failure modes of [`scan_folder`].
#[derive(Error, Debug)]
pub enum ScanError {
    /// The given root could not be stat'ed (missing or permission denied).
    #[error("cannot access folder `{}`", .path.display())]
    Stat {
        /// The offending path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The given root exists but is not a directory.
    #[error("root `{}` is not a directory", .0.display())]
    NotADirectory(PathBuf),
}

/// Scans `root` for RAW photos and returns them ordered by relative path.
///
/// Hidden files and directories (leading `.`) are skipped in both modes.
/// Individual unreadable entries are logged and skipped — a partial listing
/// beats a failed one for culling workflows. Sibling JPEGs sharing a stem
/// with a RAW (case-insensitively, same directory) attach as companions;
/// unpaired JPEGs are ignored entirely.
pub fn scan_folder(root: &Path, options: ScanOptions) -> Result<Vec<PhotoMeta>, ScanError> {
    let metadata = root.metadata().map_err(|source| ScanError::Stat {
        path: root.to_owned(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ScanError::NotADirectory(root.to_owned()));
    }

    let max_depth = if options.recursive { usize::MAX } else { 1 };
    let mut photos: Vec<PhotoMeta> = Vec::new();
    // Companion candidates keyed by (parent directory, lowercased stem);
    // several spellings (`a.jpg` + `A.JPEG`) resolve deterministically later.
    let mut jpegs: HashMap<(PathBuf, String), Vec<PathBuf>> = HashMap::new();
    for entry in WalkDir::new(root)
        .min_depth(1)
        .max_depth(max_depth)
        .into_iter()
        // The root itself is exempt so users may open dot-prefixed folders;
        // anything hidden below it is pruned with its subtree.
        .filter_entry(|entry| entry.depth() == 0 || !is_hidden(entry.file_name()))
        .filter_map(handle_entry_error)
        .filter(|entry| entry.file_type().is_file())
    {
        let file_name = entry.file_name();
        if has_raw_extension(file_name) {
            if let Some(meta) = to_photo_meta(root, entry) {
                photos.push(meta);
            }
        } else if has_jpeg_extension(file_name)
            && let Some(stem) = lowered_stem(file_name)
            && let Ok(rel_path) = entry.path().strip_prefix(root)
        {
            jpegs
                .entry((parent_of(rel_path), stem))
                .or_default()
                .push(rel_path.to_owned());
        }
    }

    for photo in &mut photos {
        let Some(stem) = lowered_stem(path_file_name(&photo.rel_path)) else {
            continue;
        };
        if let Some(mut candidates) = jpegs.remove(&(parent_of(&photo.rel_path), stem)) {
            candidates.sort_unstable();
            photo.jpeg_rel_path = candidates.into_iter().next();
        }
    }

    photos.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(photos)
}

fn handle_entry_error(entry: Result<DirEntry, walkdir::Error>) -> Option<DirEntry> {
    match entry {
        Ok(entry) => Some(entry),
        // Unreadable directory or vanished file: keep scanning the rest.
        Err(error) => {
            tracing::warn!(%error, "skipping unreadable path during scan");
            None
        }
    }
}

fn is_hidden(file_name: &OsStr) -> bool {
    file_name.as_encoded_bytes().first() == Some(&b'.')
}

/// Bytes after the final dot of a non-empty extension, if any.
fn extension_bytes(file_name: &OsStr) -> Option<&[u8]> {
    let bytes = file_name.as_encoded_bytes();
    let dot = bytes.iter().rposition(|&b| b == b'.')?;
    Some(&bytes[dot + 1..])
}

fn has_raw_extension(file_name: &OsStr) -> bool {
    extension_bytes(file_name).is_some_and(crate::extract::is_supported_extension)
}

fn has_jpeg_extension(file_name: &OsStr) -> bool {
    let Some(ext) = extension_bytes(file_name) else {
        return false;
    };
    JPEG_EXTENSIONS
        .iter()
        .any(|candidate| ext.eq_ignore_ascii_case(candidate))
}

/// ASCII-lowercased bytes before the final dot; pairing compares stems
/// case-insensitively so card-written `IMG_0001.JPG` matches `img_0001.CR3`.
fn lowered_stem(file_name: &OsStr) -> Option<String> {
    let bytes = file_name.as_encoded_bytes();
    let dot = bytes.iter().rposition(|&b| b == b'.')?;
    let stem: Vec<u8> = bytes[..dot]
        .iter()
        .map(|b| b.to_ascii_lowercase())
        .collect();
    Some(String::from_utf8_lossy(&stem).into_owned())
}

/// Parent directory as a map key; top-level files share the empty path.
fn parent_of(rel_path: &Path) -> PathBuf {
    rel_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_owned()
}

/// A file name that always exists for scanned entries, as bytes.
fn path_file_name(path: &Path) -> &OsStr {
    path.file_name().unwrap_or_default()
}

fn to_photo_meta(root: &Path, entry: DirEntry) -> Option<PhotoMeta> {
    let metadata = match entry.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::warn!(%error, "skipping file that could not be stat'ed");
            return None;
        }
    };
    let mtime = match metadata.modified() {
        Ok(mtime) => mtime,
        Err(error) => {
            tracing::warn!(%error, "skipping file without modification time");
            return None;
        }
    };
    let rel_path = match entry.path().strip_prefix(root) {
        Ok(rel_path) => rel_path.to_owned(),
        Err(error) => {
            // Impossible by construction of the WalkDir above; skip rather
            // than corrupt the index if it ever changes.
            tracing::warn!(%error, "skipping entry outside scan root");
            return None;
        }
    };
    Some(PhotoMeta {
        root: root.to_owned(),
        rel_path,
        mtime,
        size: metadata.len(),
        jpeg_rel_path: None,
    })
}

#[cfg(test)]
mod tests {
    // Test setup asserts hard failures; a broken fixture aborts the test.
    #![expect(clippy::expect_used)]

    use std::fs;
    use std::time::SystemTime;

    use tempfile::TempDir;

    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"x").expect("test setup should write fixture file");
        path
    }

    fn names(photos: &[PhotoMeta]) -> Vec<String> {
        photos
            .iter()
            .map(|p| p.rel_path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn scan_should_return_direct_children_ordered_by_relative_path() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "IMG_0002.CR3");
        touch(dir.path(), "IMG_0001.nef");
        touch(dir.path(), "IMG_0010.ARW");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(
            names(&photos),
            vec!["IMG_0001.nef", "IMG_0002.CR3", "IMG_0010.ARW"]
        );
    }

    #[test]
    fn scan_should_skip_unsupported_extensions() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "photo.jpg");
        touch(dir.path(), "notes.txt");
        touch(dir.path(), "keep.NEF");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(names(&photos), vec!["keep.NEF"]);
    }

    #[test]
    fn scan_should_pair_a_sibling_jpeg_onto_its_raw() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "IMG_0001.CR3");
        touch(dir.path(), "IMG_0001.JPG");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        // One copy on the sheet: the JPEG never becomes its own photo.
        assert_eq!(photos.len(), 1);
        assert_eq!(names(&photos), vec!["IMG_0001.CR3"]);
        assert_eq!(
            photos[0].jpeg_rel_path.as_deref(),
            Some(Path::new("IMG_0001.JPG"))
        );
    }

    #[test]
    fn scan_should_match_companion_stems_and_extensions_case_insensitively() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "img_0002.nef");
        touch(dir.path(), "IMG_0002.JPEG");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(
            photos[0].jpeg_rel_path.as_deref(),
            Some(Path::new("IMG_0002.JPEG"))
        );
    }

    #[test]
    fn scan_should_leave_raws_without_a_jpeg_companion_alone() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "a.cr3");
        touch(dir.path(), "b.arw");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert!(
            photos.iter().all(|photo| photo.jpeg_rel_path.is_none()),
            "no siblings means no pairing"
        );
    }

    #[test]
    fn scan_should_ignore_unpaired_jpegs_entirely() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "solo.jpg");
        touch(dir.path(), "b.arw");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(names(&photos), vec!["b.arw"]);
        assert_eq!(photos[0].jpeg_rel_path, None);
    }

    #[test]
    fn scan_should_not_pair_across_directories_even_recursively() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        touch(dir.path(), "a.cr3");
        touch(&dir.path().join("sub"), "a.jpg");

        let photos = scan_folder(dir.path(), ScanOptions { recursive: true }).expect("scan");

        assert!(
            !photos.iter().any(|photo| photo.jpeg_rel_path.is_some()),
            "companions must be same-directory"
        );
    }

    #[test]
    fn scan_should_pick_a_deterministic_companion_when_several_match() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "A.CR3");
        touch(dir.path(), "a.jpeg");
        touch(dir.path(), "a.jpg");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(photos[0].rel_path, Path::new("A.CR3"));
        // Sorted-path pick, stable across rescans whichever spelling won.
        assert_eq!(
            photos[0].jpeg_rel_path.as_deref(),
            Some(Path::new("a.jpeg"))
        );
    }

    #[test]
    fn scan_should_pair_within_subdirectories_when_recursive() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir(dir.path().join("day1")).expect("mkdir");
        touch(&dir.path().join("day1"), "b.orf");
        touch(&dir.path().join("day1"), "b.JPG");

        let photos = scan_folder(dir.path(), ScanOptions { recursive: true }).expect("scan");

        assert_eq!(
            photos[0].jpeg_rel_path.as_deref(),
            Some(Path::new("day1/b.JPG"))
        );
    }

    #[test]
    fn scan_should_match_extensions_case_insensitively() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "lower.orf");
        touch(dir.path(), "upper.ORF");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(names(&photos).len(), 2);
    }

    #[test]
    fn scan_should_ignore_extensionless_and_dotfiles() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "noext");
        touch(dir.path(), ".hidden.nef");
        touch(dir.path(), "visible.raf");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(names(&photos), vec!["visible.raf"]);
    }

    #[test]
    fn scan_should_skip_hidden_directories() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir(dir.path().join(".trash")).expect("mkdir");
        touch(&dir.path().join(".trash"), "junk.dng");
        touch(dir.path(), "shot.arw");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(names(&photos), vec!["shot.arw"]);
    }

    #[test]
    fn scan_should_stay_at_depth_one_by_default() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        touch(&dir.path().join("sub"), "nested.cr3");
        touch(dir.path(), "top.rw2");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(names(&photos), vec!["top.rw2"]);
    }

    #[test]
    fn scan_should_recurse_when_requested() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        touch(&dir.path().join("sub"), "nested.cr3");
        touch(dir.path(), "top.rw2");

        let photos = scan_folder(dir.path(), ScanOptions { recursive: true }).expect("scan");

        assert_eq!(
            names(&photos),
            vec!["sub/nested.cr3".to_owned(), "top.rw2".to_owned(),]
        );
    }

    #[test]
    fn scan_should_capture_file_size() {
        let dir = TempDir::new().expect("temp dir");
        fs::write(dir.path().join("a.dng"), b"12345").expect("write");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert_eq!(photos[0].size, 5);
    }

    #[test]
    fn scan_should_capture_mtime_not_in_the_future() {
        let dir = TempDir::new().expect("temp dir");
        touch(dir.path(), "a.dng");

        let photos = scan_folder(dir.path(), ScanOptions::default()).expect("scan");

        assert!(photos[0].mtime <= SystemTime::now());
    }

    #[test]
    fn scan_should_report_missing_root_as_error() {
        let missing = std::env::temp_dir().join("cullr-definitely-missing-42");

        let result = scan_folder(&missing, ScanOptions::default());

        assert!(result.is_err());
    }

    #[test]
    fn scan_should_reject_file_root() {
        let dir = TempDir::new().expect("temp dir");
        let file = touch(dir.path(), "plain.txt");

        let result = scan_folder(&file, ScanOptions::default());

        assert!(matches!(result, Err(ScanError::NotADirectory(_))));
    }
}
