//! Fixture-gated extraction tests (SPEC §9).
//!
//! Real RAW files are large and vendor-encumbered, so they never live in the
//! repository. Point `CULLR_FIXTURES` at a directory containing at least one
//! `.CR3`, one `.NEF` and one `.ARW` file to run these; with the variable
//! unset every test here returns silently.

#![expect(clippy::expect_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cullr_core::{Cache, IngestEvent, IngestPipeline, PhotoMeta, extract_file};
use tempfile::TempDir;

/// Returns the first file in the fixture directory matching `extension`
/// (case-insensitive), or `None` when the gate is closed.
fn find_fixture(extension: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("CULLR_FIXTURES")?);
    let entries = std::fs::read_dir(&dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file()
                && path.extension().is_some_and(|ext| {
                    ext.as_encoded_bytes()
                        .eq_ignore_ascii_case(extension.as_bytes())
                })
        })
}

/// Runs a full extract round-trip against one real RAW file and asserts the
/// SPEC T5 done-criteria: preview + thumb on disk and EXIF parsed.
fn should_extract(path: &Path) {
    let scratch = TempDir::new().expect("scratch dir");
    let cache = Cache::new(scratch.path().join("cache"));
    let metadata = std::fs::metadata(path).expect("fixture stat");
    let photo = PhotoMeta {
        root: path.parent().expect("fixture parent").to_owned(),
        rel_path: path.file_name().expect("fixture name").into(),
        mtime: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: metadata.len(),
    };

    let info = extract_file(&photo, &cache)
        .unwrap_or_else(|error| panic!("{} should extract cleanly: {error}", path.display()));

    // Preview + thumbnail written, non-empty, valid JPEGs.
    for asset in [&info.preview_path, &info.thumb_path] {
        let bytes = std::fs::read(asset).unwrap_or_else(|error| panic!("{asset:?}: {error}"));
        assert!(bytes.len() > 2, "{asset:?} is suspiciously empty");
        assert_eq!(&bytes[..2], b"\xff\xd8", "{asset:?} is not a JPEG");
    }

    // Dimensions describe the actual preview pixels.
    assert!(info.width > 0 && info.height > 0);
    let decoded = image::load_from_memory(&std::fs::read(&info.thumb_path).expect("thumb bytes"))
        .expect("cached thumb decodes");
    assert!(
        decoded.width() <= 512 && decoded.height() <= 512,
        "thumb exceeds long-edge budget: {}x{}",
        decoded.width(),
        decoded.height()
    );

    // EXIF summary present (T5 done-criteria: preview+thumb+EXIF).
    assert!(
        info.camera.is_some(),
        "{} should identify its camera",
        path.display()
    );
    assert!((1..=8).contains(&info.orientation));
}

#[test]
fn fixture_cr3_should_produce_preview_thumb_and_exif() {
    let Some(path) = find_fixture("cr3") else {
        return;
    };
    should_extract(&path);
}

#[test]
fn fixture_nef_should_produce_preview_thumb_and_exif() {
    let Some(path) = find_fixture("nef") else {
        return;
    };
    should_extract(&path);
}

#[test]
fn fixture_arw_should_produce_preview_thumb_and_exif() {
    let Some(path) = find_fixture("arw") else {
        return;
    };
    should_extract(&path);
}

/// Fuji RAF is the vendor SPEC §11 flags for missing large previews; it
/// exercises the full fallback chain on real cards.
#[test]
fn fixture_raf_should_produce_preview_thumb_and_exif() {
    let Some(path) = find_fixture("raf") else {
        return;
    };
    should_extract(&path);
}

#[test]
fn fixtures_should_produce_stable_hashes_across_re_extraction() {
    let Some(path) = find_fixture("raf")
        .or_else(|| find_fixture("arw"))
        .or_else(|| find_fixture("nef"))
        .or_else(|| find_fixture("cr3"))
    else {
        return;
    };
    let scratch = TempDir::new().expect("scratch dir");
    let cache = Cache::new(scratch.path().to_owned());
    let metadata = std::fs::metadata(&path).expect("fixture stat");
    let make_photo = || PhotoMeta {
        root: path.parent().expect("fixture parent").to_owned(),
        rel_path: path.file_name().expect("fixture name").into(),
        mtime: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: metadata.len(),
    };

    let first = extract_file(&make_photo(), &cache).expect("first pass");
    let second = extract_file(&make_photo(), &cache).expect("second pass");

    assert_eq!(first.preview_path, second.preview_path);
    assert_eq!(first.thumb_path, second.thumb_path);

    // GC must treat freshly extracted assets as live.
    let mut keep = HashSet::new();
    keep.insert(
        first
            .preview_path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .into_owned(),
    );
    keep.insert(
        first
            .thumb_path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .into_owned(),
    );
    assert_eq!(cache.gc(&keep).expect("gc"), 0);
}

#[test]
fn fixture_mtime_change_should_re_key_the_cache() {
    let Some(path) = find_fixture("nef").or_else(|| find_fixture("cr3")) else {
        return;
    };
    let scratch = TempDir::new().expect("scratch dir");
    let cache = Cache::new(scratch.path().to_owned());
    let metadata = std::fs::metadata(&path).expect("fixture stat");
    let base = PhotoMeta {
        root: path.parent().expect("fixture parent").to_owned(),
        rel_path: path.file_name().expect("fixture name").into(),
        mtime: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: metadata.len(),
    };

    let first = extract_file(&base, &cache).expect("base extraction");
    let touched = PhotoMeta {
        mtime: base.mtime + Duration::from_secs(1),
        ..base
    };
    let second = extract_file(&touched, &cache).expect("re-extraction");

    assert_ne!(first.preview_path, second.preview_path);
}

/// T6 done-criterion on a real RAW: scan → sync → ingest streams an
/// `Ingested` event and lands the row in `Ok` with cache paths recorded.
#[test]
fn fixture_folder_should_ingest_to_an_ok_row_streaming_events() {
    let Some(path) = find_fixture("cr3")
        .or_else(|| find_fixture("nef"))
        .or_else(|| find_fixture("arw"))
        .or_else(|| find_fixture("raf"))
    else {
        return;
    };
    // One file per folder copy keeps the batch small but real.
    let scratch = TempDir::new().expect("scratch dir");
    let root = scratch.path().join("card");
    std::fs::create_dir(&root).expect("mkdir card");
    std::fs::copy(&path, root.join(path.file_name().expect("fixture name"))).expect("copy");

    let db = Arc::new(cullr_core::Db::open(&scratch.path().join("index.db")).expect("db"));
    let scanned = cullr_core::scan_folder(&root, cullr_core::ScanOptions::default()).expect("scan");
    db.sync_scan(&root, &scanned, cullr_core::ScanOptions::default())
        .expect("sync");
    let (pipeline, events) =
        IngestPipeline::new(Cache::new(scratch.path().join("cache")), Arc::clone(&db));
    let pending = db.pending_photos(&root).expect("pending");

    let generation = pipeline.enqueue(pending);

    let mut ingested = Vec::new();
    loop {
        match events
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("ingest events")
        {
            IngestEvent::Ingested(id) => ingested.push(id),
            IngestEvent::Failed(id) => panic!("{id:?} should ingest cleanly"),
            IngestEvent::Finished { generation: done } => {
                assert_eq!(done, generation);
                break;
            }
        }
    }
    assert_eq!(ingested.len(), 1);

    let rows = db.pending_photos(&root).expect("pending after run");
    assert!(
        rows.is_empty(),
        "the single photo must have left the pending queue"
    );
}

/// T6 done-criterion over the whole fixture folder: every supported file
/// streams through the pipeline to an `Ok` row, with the batch completing
/// exactly once. Logs wall-clock throughput for eyeballing SPEC §8's
/// ≥20 files/s ingest budget on real media.
#[test]
fn fixture_folder_should_ingest_every_file_through_the_pipeline() {
    let Some(root) = std::env::var_os("CULLR_FIXTURES").map(PathBuf::from) else {
        return;
    };
    if !root.is_dir() {
        return;
    }
    let scratch = TempDir::new().expect("scratch dir");
    let db = Arc::new(cullr_core::Db::open(&scratch.path().join("index.db")).expect("db"));
    let scanned = cullr_core::scan_folder(&root, cullr_core::ScanOptions::default()).expect("scan");
    assert!(
        !scanned.is_empty(),
        "fixture folder holds no supported files"
    );
    db.sync_scan(&root, &scanned, cullr_core::ScanOptions::default())
        .expect("sync");
    let (pipeline, events) =
        IngestPipeline::new(Cache::new(scratch.path().join("cache")), Arc::clone(&db));
    let pending = db.pending_photos(&root).expect("pending");
    let total = pending.len();
    let started = std::time::Instant::now();

    let generation = pipeline.enqueue(pending);

    let mut ingested = 0usize;
    loop {
        match events
            .recv_timeout(std::time::Duration::from_secs(600))
            .expect("ingest events")
        {
            IngestEvent::Ingested(_) => ingested += 1,
            IngestEvent::Failed(id) => panic!("{id:?} should ingest cleanly"),
            IngestEvent::Finished { generation: done } => {
                assert_eq!(done, generation);
                break;
            }
        }
    }
    let elapsed = started.elapsed();
    let rate = total as f64 / elapsed.as_secs_f64();
    println!(
        "ingested {total} files in {:.2?} ({rate:.1} files/s)",
        elapsed
    );
    assert_eq!(ingested, total);
    assert!(db.pending_photos(&root).expect("pending").is_empty());
}
