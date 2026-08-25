//! Exporting culled originals: copying photo files to a user-chosen folder.
//!
//! Culling ends with keeping the keepers; this module copies the selected
//! original files (RAW or otherwise — it is format-agnostic byte copying)
//! into one destination directory. It is GUI-free and blocking by design:
//! the UI calls it from a worker thread and receives progress through the
//! [`on_progress`] callback.
//!
//! Semantics are deliberately plain: existing destination files are
//! overwritten (`cp` behavior), so re-exporting an extended selection to
//! the same folder never duplicates anything. A source that already lives
//! in the destination (the user picked the source folder itself) is
//! skipped rather than copied onto itself.

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use thiserror::Error;

/// One copy that did not complete, with the reason it failed.
///
/// Failures never abort the run: a vanished source or a locked file must
/// not stop the remaining keepers from landing (mirrors the scan policy of
/// partial success over total failure).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportFailure {
    /// Source file whose copy failed.
    pub source: PathBuf,
    /// Human-readable reason for the failure.
    pub reason: String,
}

/// Outcome of an [`export_files`] run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Files successfully written (or verified already in place).
    pub copied: usize,
    /// Per-file failures, in input order.
    pub failures: Vec<ExportFailure>,
    /// `true` when cancellation stopped the run before every source was
    /// attempted; `copied` then covers only the files handled so far.
    pub cancelled: bool,
}

impl ExportReport {
    /// Total sources processed so far: successes plus failures.
    pub fn processed(&self) -> usize {
        self.copied + self.failures.len()
    }
}

/// Failure modes of the export run as a whole. Per-file problems are
/// reported inside [`ExportReport`] instead.
#[derive(Error, Debug)]
pub enum ExportError {
    /// The chosen destination could not be used as a folder.
    #[error("cannot export into `{}`", .0.display())]
    Destination(PathBuf),
}

/// Copies every file in `sources` into `dest_dir`, reporting progress after
/// each attempt.
///
/// * `cancel` is polled before every file; once set the run stops early and
///   [`ExportReport::cancelled`] is returned `true`.
/// * `on_progress` receives the number of sources processed so far (successes
///   and failures alike), which the UI renders as `n / m`.
///
/// Sources whose canonical path equals their destination (exporting a
/// folder into itself) are skipped silently — they count neither as copied
/// nor failed.
pub fn export_files(
    sources: &[PathBuf],
    dest_dir: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(usize),
) -> Result<ExportReport, ExportError> {
    let metadata = dest_dir
        .metadata()
        .map_err(|_| ExportError::Destination(dest_dir.to_owned()))?;
    if !metadata.is_dir() {
        return Err(ExportError::Destination(dest_dir.to_owned()));
    }

    let mut report = ExportReport::default();
    for source in sources {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        copy_one(source, dest_dir, &mut report);
        on_progress(report.processed());
    }
    tracing::info!(
        copied = report.copied,
        failed = report.failures.len(),
        cancelled = report.cancelled,
        ?dest_dir,
        "export finished"
    );
    Ok(report)
}

/// Names from `sources` that already exist as entries in `dest_dir`, in
/// input order without duplicates. Purely advisory — lets the UI warn
/// before [`export_files`] overwrites them (`cp` semantics).
pub fn existing_names(sources: &[PathBuf], dest_dir: &Path) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut existing = Vec::new();
    for name in sources.iter().filter_map(|source| source.file_name()) {
        let name = name.to_string_lossy().into_owned();
        if seen.insert(name.clone()) && dest_dir.join(&name).exists() {
            existing.push(name);
        }
    }
    existing
}

/// Copies one file, appending either a success or a failure to `report`.
fn copy_one(source: &Path, dest_dir: &Path, report: &mut ExportReport) {
    let Some(name) = source.file_name() else {
        report.failures.push(ExportFailure {
            source: source.to_owned(),
            reason: "path has no file name".to_owned(),
        });
        return;
    };
    let dest = dest_dir.join(name);

    // Picking the source folder as the destination would copy every file
    // onto itself; treat that as "already exported" instead of touching it.
    if same_file(source, &dest) {
        report.copied += 1;
        return;
    }
    match std::fs::copy(source, &dest) {
        Ok(_) => report.copied += 1,
        Err(error) => report.failures.push(ExportFailure {
            source: source.to_owned(),
            reason: error.to_string(),
        }),
    }
}

/// Whether both paths resolve to the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used)]

    use std::sync::atomic::AtomicBool;

    use tempfile::TempDir;

    use super::*;

    fn touch(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("test setup should write fixture file");
        path
    }

    fn run(sources: &[PathBuf], dest: &Path, cancel: &AtomicBool) -> ExportReport {
        export_files(sources, dest, cancel, |_| {}).expect("destination should be usable")
    }

    #[test]
    fn export_should_copy_files_under_their_own_names() {
        let src = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "IMG_0001.CR3", b"alpha");
        let b = touch(src.path(), "IMG_0002.nef", b"beta");

        let report = run(&[a, b], dest.path(), &AtomicBool::new(false));

        assert_eq!(report.copied, 2);
        assert!(report.failures.is_empty());
        assert!(!report.cancelled);
        assert_eq!(
            std::fs::read(dest.path().join("IMG_0001.CR3")).expect("copy exists"),
            b"alpha"
        );
        assert_eq!(
            std::fs::read(dest.path().join("IMG_0002.nef")).expect("copy exists"),
            b"beta"
        );
    }

    #[test]
    fn export_should_overwrite_existing_destination_files() {
        let src = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "a.nef", b"fresh-and-longer");
        touch(dest.path(), "a.nef", b"stale");

        let report = run(
            std::slice::from_ref(&a),
            dest.path(),
            &AtomicBool::new(false),
        );

        assert_eq!(report.copied, 1);
        assert_eq!(
            std::fs::read(dest.path().join("a.nef")).expect("copy exists"),
            b"fresh-and-longer"
        );
    }

    #[test]
    fn export_should_continue_past_a_missing_source() {
        let src = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("temp dir");
        let missing = src.path().join("gone.nef");
        let present = touch(src.path(), "here.nef", b"data");

        let report = run(&[missing, present], dest.path(), &AtomicBool::new(false));

        assert_eq!(report.copied, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].source, src.path().join("gone.nef"));
        assert_eq!(
            std::fs::read(dest.path().join("here.nef")).expect("later file still copied"),
            b"data"
        );
    }

    #[test]
    fn export_should_stop_early_when_cancelled() {
        let src = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "a.nef", b"a");
        let b = touch(src.path(), "b.nef", b"b");

        let cancel = AtomicBool::new(true);
        let report = run(&[a, b], dest.path(), &cancel);

        assert!(report.cancelled);
        assert_eq!(report.copied, 0);
        assert!(
            !dest.path().join("b.nef").exists(),
            "cancelled run must not reach later files"
        );
    }

    #[test]
    fn export_into_the_source_folder_should_skip_instead_of_failing() {
        let src = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "a.nef", b"a");

        let report = run(
            std::slice::from_ref(&a),
            src.path(),
            &AtomicBool::new(false),
        );

        assert_eq!(report.copied, 1);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn export_progress_should_advance_once_per_attempt() {
        let src = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "a.nef", b"a");
        let missing = src.path().join("gone.nef");

        let mut ticks = Vec::new();
        let report = export_files(
            &[a, missing],
            dest.path(),
            &AtomicBool::new(false),
            |done| {
                ticks.push(done);
            },
        )
        .expect("destination should be usable");

        assert_eq!(ticks, vec![1, 2]);
        assert_eq!(report.processed(), 2);
    }

    #[test]
    fn existing_names_should_list_only_sources_already_in_the_destination() {
        let src = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("temp dir");
        let fresh = touch(src.path(), "fresh.nef", b"f");
        let clash = touch(src.path(), "clash.nef", b"c");

        let names = existing_names(&[fresh.clone(), clash.clone()], dest.path());

        assert!(
            names.is_empty(),
            "nothing in dest yet, nothing to warn about"
        );

        std::fs::write(dest.path().join("clash.nef"), b"old").expect("fixture write");
        let names = existing_names(&[fresh, clash], dest.path());
        assert_eq!(names, vec!["clash.nef".to_owned()]);
    }

    #[test]
    fn existing_names_should_deduplicate_repeated_source_names() {
        let src = TempDir::new().expect("temp dir");
        let other = TempDir::new().expect("temp dir");
        let dest = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "a.nef", b"a");
        let same_name = touch(other.path(), "a.nef", b"different bytes");
        let b = touch(src.path(), "b.nef", b"b");

        let names = existing_names(&[a.clone(), same_name.clone(), b.clone()], dest.path());
        assert!(names.is_empty(), "empty destination, nothing to warn about");

        std::fs::write(dest.path().join("a.nef"), b"old").expect("fixture write");
        std::fs::write(dest.path().join("b.nef"), b"old").expect("fixture write");
        let names = existing_names(&[a, same_name, b], dest.path());
        assert_eq!(
            names,
            vec!["a.nef".to_owned(), "b.nef".to_owned()],
            "two sources sharing a name warn once"
        );
    }

    #[test]
    fn export_should_reject_a_missing_destination() {
        let src = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "a.nef", b"a");
        let nowhere = src.path().join("nowhere");

        let result = export_files(
            std::slice::from_ref(&a),
            &nowhere,
            &AtomicBool::new(false),
            |_| {},
        );

        assert!(matches!(result, Err(ExportError::Destination(_))));
    }

    #[test]
    fn export_should_reject_a_file_destination() {
        let src = TempDir::new().expect("temp dir");
        let a = touch(src.path(), "a.nef", b"a");
        let plain = touch(src.path(), "plain.txt", b"x");

        let result = export_files(
            std::slice::from_ref(&a),
            &plain,
            &AtomicBool::new(false),
            |_| {},
        );

        assert!(matches!(result, Err(ExportError::Destination(_))));
    }
}
