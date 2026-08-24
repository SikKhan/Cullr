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

use std::time::Instant;

use tempfile::TempDir;

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
