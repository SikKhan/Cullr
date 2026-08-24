//! Performance acceptance gates (SPEC §8), run on demand.
//!
//! Budgets are release-mode numbers, so run these with:
//!
//! ```text
//! CULLR_PERF=<dir> cargo test --release -p cullr-core --test perf
//! ```
//!
//! With `CULLR_PERF` unset every test here returns silently; the directory
//! is only used as scratch space and may not exist.

#![expect(clippy::expect_used)]

use std::path::Path;
use std::time::Instant;

use tempfile::TempDir;

/// Scratch directory for one soak test, created inside `CULLR_PERF`.
/// Returns `None` when the gate variable is unset so callers skip silently.
fn soak_scratch() -> Option<TempDir> {
    let base = std::env::var_os("CULLR_PERF").map(std::path::PathBuf::from)?;
    Some(TempDir::new_in(base).expect("scratch dir"))
}

/// Writes `count` placeholder RAW files; content is irrelevant because the
/// scan gates measure walk + stat + index upsert, not extraction.
fn seed_placeholder_raws(dir: &Path, count: u32) {
    for index in 0..count {
        std::fs::write(dir.join(format!("IMG_{index:05}.NEF")), b"x").expect("test setup write");
    }
}

/// SPEC §5.1/§8: scanning 10k files (walk + filter + stat + index upsert)
/// must land in under 300 ms so the grid shows placeholders immediately.
#[test]
fn scan_of_10k_files_should_meet_the_300ms_budget() {
    let Some(scratch_base) = std::env::var_os("CULLR_PERF").map(std::path::PathBuf::from) else {
        return;
    };
    let dir = TempDir::new_in(&scratch_base).expect("scratch dir");
    for index in 0..10_000u32 {
        std::fs::write(dir.path().join(format!("IMG_{index:05}.NEF")), b"x")
            .expect("test setup write");
    }

    let db = cullr_core::Db::open(&dir.path().join("index.db")).expect("db");
    let started = Instant::now();
    let scanned =
        cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default()).expect("scan");
    let entries = db
        .sync_scan(dir.path(), &scanned, cullr_core::ScanOptions::default())
        .expect("sync");
    let elapsed = started.elapsed();

    println!("scan+sync of 10k files took {elapsed:?}");
    assert_eq!(entries.len(), 10_000);
    assert!(
        elapsed.as_millis() < 300,
        "scan budget blown: {elapsed:?} ≥ 300 ms"
    );
}

/// Warm reopen (SPEC §8): a second pass over an unchanged folder skips all
/// upserts and must be even cheaper than the cold gate.
#[test]
fn rescan_of_10k_files_should_stay_within_the_300ms_budget() {
    let Some(scratch_base) = std::env::var_os("CULLR_PERF").map(std::path::PathBuf::from) else {
        return;
    };
    let dir = TempDir::new_in(&scratch_base).expect("scratch dir");
    for index in 0..10_000u32 {
        std::fs::write(dir.path().join(format!("IMG_{index:05}.NEF")), b"x")
            .expect("test setup write");
    }
    let db = cullr_core::Db::open(&dir.path().join("index.db")).expect("db");
    let scanned =
        cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default()).expect("scan");
    db.sync_scan(dir.path(), &scanned, cullr_core::ScanOptions::default())
        .expect("warm sync");

    let started = Instant::now();
    let rescanned =
        cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default()).expect("rescan");
    db.sync_scan(dir.path(), &rescanned, cullr_core::ScanOptions::default())
        .expect("warm sync");
    let elapsed = started.elapsed();

    println!("warm rescan of 10k files took {elapsed:?}");
    assert_eq!(rescanned.len(), 10_000);
    assert!(elapsed.as_millis() < 300, "warm reopen blown: {elapsed:?}");
}

/// T13 50k soak, cold pass: scan + sync at five times the §8 scale must stay
/// well inside a linear extrapolation of the 300 ms gate. The bound is a
/// generous 2 s so slow machines and CI variance never flake; the printed
/// number is what actually tracks regressions.
#[test]
fn scan_of_50k_files_should_meet_the_2s_soak_budget() {
    let Some(dir) = soak_scratch() else {
        return;
    };
    seed_placeholder_raws(dir.path(), 50_000);

    let db = cullr_core::Db::open(&dir.path().join("index.db")).expect("db");
    let started = Instant::now();
    let scanned =
        cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default()).expect("scan");
    let entries = db
        .sync_scan(dir.path(), &scanned, cullr_core::ScanOptions::default())
        .expect("sync");
    let elapsed = started.elapsed();

    println!("cold scan+sync of 50k files took {elapsed:?}");
    // Row-count integrity: every file landed exactly once.
    assert_eq!(entries.len(), 50_000);
    assert!(
        elapsed.as_millis() < 2000,
        "50k cold scan blown: {elapsed:?} ≥ 2 s"
    );
}

/// T13 50k soak, warm pass: reopening an unchanged 50k folder must reuse all
/// rows (no duplicates, same ids) and stay inside the same generous bound as
/// the cold soak.
#[test]
fn rescan_of_50k_files_should_meet_the_2s_soak_budget() {
    let Some(dir) = soak_scratch() else {
        return;
    };
    seed_placeholder_raws(dir.path(), 50_000);
    let db = cullr_core::Db::open(&dir.path().join("index.db")).expect("db");
    let first = db
        .sync_scan(
            dir.path(),
            &cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default()).expect("scan"),
            cullr_core::ScanOptions::default(),
        )
        .expect("cold sync");

    let started = Instant::now();
    let rescanned =
        cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default()).expect("rescan");
    let entries = db
        .sync_scan(dir.path(), &rescanned, cullr_core::ScanOptions::default())
        .expect("warm sync");
    let elapsed = started.elapsed();

    println!("warm rescan of 50k files took {elapsed:?}");
    // Integrity: identical row set — no re-inserts, no lost rows.
    let first_ids: Vec<_> = first.iter().map(|entry| entry.id).collect();
    let warm_ids: Vec<_> = entries.iter().map(|entry| entry.id).collect();
    assert_eq!(warm_ids.len(), 50_000);
    assert_eq!(warm_ids, first_ids);
    assert!(
        elapsed.as_millis() < 2000,
        "50k warm rescan blown: {elapsed:?} ≥ 2 s"
    );
}

/// T13 50k soak, bulk relabel: a whole-folder recolor (the T12 batch path,
/// one transaction over 50k UPDATEs) must stay snappy — under 500 ms — so a
/// select-all keystroke never feels like a migration.
#[test]
fn bulk_relabel_across_50k_rows_should_stay_under_500ms() {
    let Some(dir) = soak_scratch() else {
        return;
    };
    seed_placeholder_raws(dir.path(), 50_000);
    let db = cullr_core::Db::open(&dir.path().join("index.db")).expect("db");
    let entries = db
        .sync_scan(
            dir.path(),
            &cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default()).expect("scan"),
            cullr_core::ScanOptions::default(),
        )
        .expect("sync");
    let ids: Vec<_> = entries.iter().map(|entry| entry.id).collect();

    let started = Instant::now();
    db.set_labels(&ids, cullr_core::Label::Green)
        .expect("bulk relabel");
    let elapsed = started.elapsed();

    println!("bulk relabel of {} rows took {elapsed:?}", ids.len());
    assert!(
        elapsed.as_millis() < 500,
        "50k bulk relabel blown: {elapsed:?} ≥ 500 ms"
    );

    // Integrity: the relabel is durable and complete across every row.
    let reread = db
        .sync_scan(
            dir.path(),
            &cullr_core::scan_folder(dir.path(), cullr_core::ScanOptions::default())
                .expect("rescan"),
            cullr_core::ScanOptions::default(),
        )
        .expect("resync");
    assert_eq!(reread.len(), 50_000);
    assert!(
        reread
            .iter()
            .all(|entry| entry.label == cullr_core::Label::Green),
        "some rows missed the bulk label"
    );
}
