//! SQLite index database: schema migrations and typed repositories.
//!
//! One global database (SPEC §4) holds photo rows, recently opened roots and
//! small key/value settings. All access goes through [`Db`], which serializes
//! statements behind a mutex; writes are tiny upserts, so contention is a
//! non-issue at contact-sheet scale.
//!
//! Paths are stored as `TEXT` via lossy UTF-8 conversion. The same conversion
//! is applied on read, so `(root, rel_path)` identity stays stable even for
//! non-UTF-8 file names.
//!
//! ```
//! use cullr_core::{scan_folder, Db, ScanOptions};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = tempfile::tempdir()?;
//! std::fs::write(dir.path().join("IMG_0001.CR3"), b"x")?;
//! let db = Db::open(&dir.path().join("index.db"))?;
//!
//! let scanned = scan_folder(dir.path(), ScanOptions::default())?;
//! let entries = db.sync_scan(dir.path(), &scanned, ScanOptions::default())?;
//! assert_eq!(entries.len(), 1);
//!
//! // Re-syncing an unchanged folder reuses the same rows: no duplicates.
//! let again = db.sync_scan(dir.path(), &scanned, ScanOptions::default())?;
//! assert_eq!(again.len(), 1);
//! assert_eq!(again[0].id, entries[0].id);
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, params};
use thiserror::Error;

use crate::model::{
    IngestInfo, Label, PendingPhoto, PhotoDetail, PhotoEntry, PhotoId, PhotoMeta, PhotoStatus,
};
use crate::scanner::ScanOptions;

/// Schema revision this build understands; bumped with every migration.
const LATEST_VERSION: i64 = 2;

/// Migration scripts indexed by target version minus one.
///
/// Each script runs inside one transaction together with its `user_version`
/// bump, so an interrupted upgrade rolls back cleanly on next open.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE photos (
  id          INTEGER PRIMARY KEY,
  root        TEXT NOT NULL,
  rel_path    TEXT NOT NULL,
  mtime       INTEGER NOT NULL,
  size        INTEGER NOT NULL,
  width INTEGER, height INTEGER,
  orientation INTEGER NOT NULL DEFAULT 1,
  camera TEXT, lens TEXT,
  taken_at TEXT,
  shutter TEXT, aperture REAL, iso INTEGER, focal_mm REAL,
  label       INTEGER NOT NULL DEFAULT 0,
  status      INTEGER NOT NULL DEFAULT 0,
  err_msg TEXT,
  preview_path TEXT, thumb_path TEXT,
  ingested_at INTEGER,
  UNIQUE(root, rel_path));
CREATE INDEX idx_photos_root  ON photos(root);
CREATE INDEX idx_photos_label ON photos(root, label);
CREATE TABLE roots (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, last_opened INTEGER NOT NULL);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
"#,
    r#"
-- v2: user-applied display rotation. Quarter-turns clockwise stacked on
-- top of the EXIF orientation flag; survives re-ingest like `label` does.
ALTER TABLE photos ADD COLUMN rot_cw INTEGER NOT NULL DEFAULT 0;
"#,
];

/// Failure modes of index database operations.
#[derive(Error, Debug)]
pub enum DbError {
    /// The database file could not be opened or created.
    #[error("cannot open index database `{}`", .path.display())]
    Open {
        /// Path that failed to open.
        path: PathBuf,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// A parent directory of the database file could not be created.
    #[error("cannot create directory `{}`", .path.display())]
    CreateDir {
        /// Directory that failed to create.
        path: PathBuf,
        /// Underlying OS error.
        source: std::io::Error,
    },
    /// A migration or statement failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// The database file was written by a newer Cullr build; refusing to
    /// touch it avoids corrupting a schema we do not understand.
    #[error("index database schema v{} is newer than supported v{}", .found, .supported)]
    FutureVersion {
        /// Schema revision found in the database.
        found: i64,
        /// Highest revision this build can migrate to.
        supported: i64,
    },
}

/// Handle to the global index database.
///
/// Cheap to share between threads (`Send + Sync`); every method takes `&self`
/// and internally locks the single connection per SPEC §3 threading rules.
pub struct Db {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl Db {
    /// Opens (or creates) the database at `path`, creating parent directories
    /// and applying pending migrations.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                path: parent.to_owned(),
                source,
            })?;
        }
        let mut conn = Connection::open(path).map_err(|source| DbError::Open {
            path: path.to_owned(),
            source,
        })?;
        // Wait briefly on a locked file instead of failing instantly when a
        // second process (or a slow write) touches the same index.
        conn.busy_timeout(Duration::from_millis(5_000))?;
        // WAL lets grid reads proceed while tiny label/status writes land;
        // pragma returns the resulting mode, hence the checked variant.
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_row| Ok(()))?;
        migrate(&mut conn)?;
        Ok(Self {
            path: path.to_owned(),
            conn: Mutex::new(conn),
        })
    }

    /// Location of the underlying database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs `body` with exclusive access to the connection.
    ///
    /// Poisoned locks are adopted rather than propagated: transactions keep
    /// SQLite consistent even if a worker panicked mid-write, and refusing
    /// all future access would permanently brick the app over one panic.
    /// Statement ownership stays with the repository methods; this is
    /// `pub(crate)` so sibling-module tests can inspect raw state.
    pub(crate) fn with_conn<T>(
        &self,
        body: impl FnOnce(&mut Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        body(&mut guard)
    }

    /// Applies a scan result to the index (SPEC §5.1 scan-diff upsert).
    ///
    /// New files are inserted as [`PhotoStatus::Pending`]; rows whose
    /// `mtime`/`size` changed — or that previously vanished — are reset to
    /// pending and their ingest artifacts cleared, while untouched rows keep
    /// their id, label and extracted state. Rows absent from the scan become
    /// [`PhotoStatus::Missing`], but only within the scan's depth scope so a
    /// shallow rescan never marks subfolder content missing.
    ///
    /// Returns every live entry under `root`, ordered by relative path.
    pub fn sync_scan(
        &self,
        root: &Path,
        scanned: &[PhotoMeta],
        options: ScanOptions,
    ) -> Result<Vec<PhotoEntry>, DbError> {
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            let root_text = root.to_string_lossy().into_owned();
            let existing = load_existing(&tx, &root_text)?;

            {
                let mut insert = tx.prepare_cached(
                    "INSERT INTO photos (root, rel_path, mtime, size, status)
                     VALUES (?1, ?2, ?3, ?4, 0)",
                )?;
                let mut reset = tx.prepare_cached(
                    "UPDATE photos SET mtime = ?2, size = ?3, status = 0,
                        width = NULL, height = NULL, orientation = 1,
                        camera = NULL, lens = NULL, taken_at = NULL,
                        shutter = NULL, aperture = NULL, iso = NULL, focal_mm = NULL,
                        err_msg = NULL, preview_path = NULL, thumb_path = NULL,
                        ingested_at = NULL
                     WHERE id = ?1",
                )?;

                for meta in scanned {
                    let mtime = system_time_to_i64(meta.mtime);
                    match existing.get(meta.rel_path.as_path()) {
                        None => {
                            insert.execute(params![
                                root_text,
                                meta.rel_path.to_string_lossy(),
                                mtime,
                                narrow_u64(meta.size),
                            ])?;
                        }
                        Some(row) if row.needs_refresh(mtime, meta.size) => {
                            reset.execute(params![row.id, mtime, narrow_u64(meta.size)])?;
                        }
                        // Unchanged: keep row id, label and any ingest state.
                        Some(_) => {}
                    }
                }

                let seen: HashSet<&Path> = scanned.iter().map(|m| m.rel_path.as_path()).collect();
                let mut mark_missing =
                    tx.prepare_cached("UPDATE photos SET status = 3 WHERE id = ?1")?;
                for row in existing.values() {
                    let in_scope = options.recursive || !is_nested(&row.rel);
                    if row.status != PhotoStatus::Missing
                        && in_scope
                        && !seen.contains(row.rel.as_path())
                    {
                        mark_missing.execute(params![row.id])?;
                    }
                }
            }

            let entries = load_entries(&tx, &root_text)?;
            tx.commit()?;
            Ok(entries)
        })
    }

    /// Every row under `root` still awaiting ingest, ordered by path.
    ///
    /// This is the queue the ingest pipeline drains after a scan: new files
    /// and rows reset by [`Db::sync_scan`] come back as pending, while
    /// already-ingested and previously-errored rows are left alone so a
    /// poison file is never retried on every folder open.
    pub fn pending_photos(&self, root: &Path) -> Result<Vec<PendingPhoto>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, rel_path, mtime, size FROM photos
                 WHERE root = ?1 AND status = 0 ORDER BY rel_path",
            )?;
            let root_text = root.to_string_lossy().into_owned();
            let rows = stmt.query_map([root_text], |row| {
                Ok(PendingPhoto {
                    id: PhotoId(widen_to_u64(row.get::<_, i64>(0)?)),
                    meta: PhotoMeta {
                        root: root.to_owned(),
                        rel_path: PathBuf::from(row.get::<_, String>(1)?),
                        mtime: i64_to_system_time(row.get::<_, i64>(2)?),
                        size: widen_to_u64(row.get::<_, i64>(3)?),
                    },
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Persists a color label as a single UPDATE (SPEC §6 selection rules).
    ///
    /// Unknown ids are logged and ignored so a stale UI event can never fail
    /// the pipeline.
    pub fn set_label(&self, id: PhotoId, label: Label) -> Result<(), DbError> {
        self.with_conn(|conn| {
            let updated = conn.execute(
                "UPDATE photos SET label = ?2 WHERE id = ?1",
                params![narrow_u64(id.0), label.to_u8()],
            )?;
            if updated == 0 {
                tracing::warn!(?id, "set_label ignored unknown photo id");
            }
            Ok(())
        })
    }

    /// Persists one color label onto many photos in a single transaction
    /// (SPEC §10 T12 batch labeling): one commit however large the batch,
    /// so a hundred-photo relabel costs one fsync, not a hundred.
    /// Unknown ids are skipped like in [`Self::set_label`].
    pub fn set_labels(&self, ids: &[PhotoId], label: Label) -> Result<(), DbError> {
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            {
                let mut update = tx.prepare_cached("UPDATE photos SET label = ?2 WHERE id = ?1")?;
                for &id in ids {
                    update.execute(params![narrow_u64(id.0), label.to_u8()])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Persists a user display rotation as a single UPDATE: `turns` are
    /// quarter-turns clockwise (0–3) stacked on the EXIF orientation.
    /// Values outside `0..4` wrap; unknown ids are logged and ignored like
    /// in [`Self::set_label`].
    pub fn set_rotation(&self, id: PhotoId, turns: u8) -> Result<(), DbError> {
        self.with_conn(|conn| {
            let updated = conn.execute(
                "UPDATE photos SET rot_cw = ?2 WHERE id = ?1",
                params![narrow_u64(id.0), turns % 4],
            )?;
            if updated == 0 {
                tracing::warn!(?id, "set_rotation ignored unknown photo id");
            }
            Ok(())
        })
    }

    /// Persists many rotations in one transaction, mirroring
    /// [`Self::set_labels`]: one commit however large the batch, so a
    /// hundred-photo rotate costs one fsync.
    pub fn set_rotations(&self, updates: &[(PhotoId, u8)]) -> Result<(), DbError> {
        if updates.is_empty() {
            return Ok(());
        }
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            {
                let mut update =
                    tx.prepare_cached("UPDATE photos SET rot_cw = ?2 WHERE id = ?1")?;
                for &(id, turns) in updates {
                    update.execute(params![narrow_u64(id.0), turns % 4])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Records a successful ingest, attaching metadata and cache paths.
    pub fn record_ingest_ok(&self, id: PhotoId, info: &IngestInfo) -> Result<(), DbError> {
        self.with_conn(|conn| {
            let updated = conn.execute(
                "UPDATE photos SET status = 1, width = ?2, height = ?3, orientation = ?4,
                    camera = ?5, lens = ?6, taken_at = ?7, shutter = ?8, aperture = ?9,
                    iso = ?10, focal_mm = ?11, preview_path = ?12, thumb_path = ?13,
                    ingested_at = ?14
                 WHERE id = ?1",
                params![
                    narrow_u64(id.0),
                    info.width,
                    info.height,
                    info.orientation,
                    info.camera,
                    info.lens,
                    info.taken_at,
                    info.shutter,
                    info.aperture,
                    info.iso,
                    info.focal_mm,
                    info.preview_path.to_string_lossy(),
                    info.thumb_path.to_string_lossy(),
                    now_millis(),
                ],
            )?;
            if updated == 0 {
                tracing::warn!(?id, "record_ingest_ok ignored unknown photo id");
            }
            Ok(())
        })
    }

    /// Marks an ingest failure; the pipeline keeps going (SPEC §5.2).
    pub fn record_ingest_error(&self, id: PhotoId, message: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            // Bound the message so thousands of corrupt files cannot bloat
            // the index; a tooltip never needs more than a line anyway.
            let bounded: String = message.chars().take(400).collect();
            conn.execute(
                "UPDATE photos SET status = 2, err_msg = ?2 WHERE id = ?1",
                params![narrow_u64(id.0), bounded],
            )?;
            Ok(())
        })
    }

    /// Fetches one photo row by id, refreshed with its latest ingest state.
    ///
    /// This is how the UI turns an [`crate::IngestEvent`] into up-to-date
    /// cell content (thumbnail path, pixel size, error message); `None`
    /// means the id does not exist (or was never part of this index).
    pub fn photo_entry(&self, id: PhotoId) -> Result<Option<PhotoEntry>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT rel_path, label, status, width, height, orientation, thumb_path, err_msg, rot_cw
                 FROM photos WHERE id = ?1",
            )?;
            let mut rows = stmt.query([narrow_u64(id.0)])?;
            match rows.next()? {
                Some(row) => Ok(Some(entry_from_row(
                    id,
                    PathBuf::from(row.get::<_, String>(0)?),
                    row,
                    1,
                )?)),
                None => Ok(None),
            }
        })
    }

    /// Fetches the full display record for one photo (loupe view).
    ///
    /// Superset of [`Db::photo_entry`]: adds the large preview asset path
    /// and the EXIF summary columns. `None` means the id does not exist.
    pub fn photo_detail(&self, id: PhotoId) -> Result<Option<PhotoDetail>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT rel_path, label, status, width, height, orientation,
                        preview_path, thumb_path, camera, lens, taken_at,
                        shutter, aperture, iso, focal_mm, err_msg, rot_cw
                 FROM photos WHERE id = ?1",
            )?;
            let mut rows = stmt.query([narrow_u64(id.0)])?;
            match rows.next()? {
                None => Ok(None),
                Some(row) => {
                    let label: i64 = row.get(1)?;
                    let status: i64 = row.get(2)?;
                    let width: Option<i64> = row.get(3)?;
                    let height: Option<i64> = row.get(4)?;
                    let pixels = width
                        .and_then(|w| u32::try_from(widen_to_u64(w)).ok())
                        .zip(height.and_then(|h| u32::try_from(widen_to_u64(h)).ok()));
                    Ok(Some(PhotoDetail {
                        id,
                        rel_path: PathBuf::from(row.get::<_, String>(0)?),
                        label: Label::from_u8(u8::try_from(label).unwrap_or(0)).unwrap_or_default(),
                        status: PhotoStatus::from_u8(u8::try_from(status).unwrap_or(0))
                            .unwrap_or_default(),
                        pixels,
                        orientation: u16::try_from(row.get::<_, i64>(5)?).unwrap_or(1),
                        preview_path: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
                        thumb_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
                        camera: row.get(8)?,
                        lens: row.get(9)?,
                        taken_at: row.get(10)?,
                        shutter: row.get(11)?,
                        aperture: row.get(12)?,
                        iso: row.get::<_, Option<i64>>(13)?.and_then(|iso| {
                            u32::try_from(widen_to_u64(iso)).ok().filter(|_| iso >= 0)
                        }),
                        focal_mm: row.get(14)?,
                        err_msg: row.get(15)?,
                        rot_cw: rot_cw_of(row.get::<_, i64>(16)?),
                    }))
                }
            }
        })
    }

    /// Inserts or refreshes the `last_opened` stamp for a folder.
    ///
    /// The timestamp is caller-supplied (`now_millis()`) so ordering is
    /// deterministic and testable.
    pub fn touch_root(&self, root: &Path, opened_at: i64) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO roots (path, last_opened) VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET last_opened = excluded.last_opened",
                params![root.to_string_lossy(), opened_at],
            )?;
            Ok(())
        })
    }

    /// Recently opened folders, most recent first, at most `limit` entries.
    pub fn recent_roots(&self, limit: usize) -> Result<Vec<PathBuf>, DbError> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare_cached("SELECT path FROM roots ORDER BY last_opened DESC LIMIT ?1")?;
            let rows = stmt.query_map([narrow_usize(limit)], |row| {
                let text: String = row.get(0)?;
                Ok(PathBuf::from(text))
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Stores a settings value, replacing any previous one.
    pub fn kv_set(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    /// Reads a settings value, or `None` when unset.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare_cached("SELECT value FROM kv WHERE key = ?1")?;
            let mut rows = stmt.query([key])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get(0)?)),
                None => Ok(None),
            }
        })
    }
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("path", &self.path).finish()
    }
}

/// A photo row already present in the index, keyed by relative path during
/// scan diffing.
struct ExistingRow {
    id: i64,
    rel: PathBuf,
    mtime: i64,
    size: u64,
    status: PhotoStatus,
}

impl ExistingRow {
    /// A row must be re-ingested when the file changed on disk, or when it
    /// had been reported missing earlier (its cache files may be gone).
    fn needs_refresh(&self, mtime: i64, size: u64) -> bool {
        self.status == PhotoStatus::Missing || self.mtime != mtime || self.size != size
    }
}

fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > LATEST_VERSION {
        return Err(DbError::FutureVersion {
            found: current,
            supported: LATEST_VERSION,
        });
    }
    let tx = conn.transaction()?;
    for (index, script) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if current >= version {
            continue;
        }
        tx.execute_batch(script)?;
        tx.pragma_update(None, "user_version", version)?;
    }
    tx.commit()?;
    Ok(())
}

fn load_existing(
    tx: &rusqlite::Transaction<'_>,
    root: &str,
) -> Result<HashMap<PathBuf, ExistingRow>, DbError> {
    let mut stmt =
        tx.prepare("SELECT id, rel_path, mtime, size, status FROM photos WHERE root = ?1")?;
    let rows = stmt.query_map([root], |row| {
        Ok(ExistingRow {
            id: row.get(0)?,
            rel: PathBuf::from(row.get::<_, String>(1)?),
            mtime: row.get(2)?,
            size: widen_to_u64(row.get::<_, i64>(3)?),
            status: PhotoStatus::from_u8(u8::try_from(row.get::<_, i64>(4)?).unwrap_or(0))
                .unwrap_or_default(),
        })
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let row = row?;
        map.insert(row.rel.clone(), row);
    }
    Ok(map)
}

fn load_entries(tx: &rusqlite::Transaction<'_>, root: &str) -> Result<Vec<PhotoEntry>, DbError> {
    // Missing rows stay out of navigation entirely (SPEC §7).
    let mut stmt = tx.prepare(
        "SELECT id, rel_path, label, status, width, height, orientation, thumb_path, err_msg, rot_cw
         FROM photos WHERE root = ?1 AND status <> 3",
    )?;
    let rows = stmt.query_map([root], |row| {
        entry_from_row(
            PhotoId(widen_to_u64(row.get::<_, i64>(0)?)),
            PathBuf::from(row.get::<_, String>(1)?),
            row,
            2,
        )
    })?;
    let mut entries: Vec<PhotoEntry> = rows.collect::<Result<Vec<_>, _>>()?;
    // Match scanner ordering (PathBuf Ord), which SQL BINARY collation does
    // not guarantee across platforms.
    entries.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(entries)
}

/// Assembles a [`PhotoEntry`] from the cell-facing columns of a `photos`
/// row. `rel_path` comes from the caller because the two query shapes
/// select it at different positions; `base` is the column index of `label`
/// followed by `status, width, height, orientation, thumb_path, err_msg,
/// rot_cw`.
fn entry_from_row(
    id: PhotoId,
    rel_path: PathBuf,
    row: &rusqlite::Row<'_>,
    base: usize,
) -> Result<PhotoEntry, rusqlite::Error> {
    let label: i64 = row.get(base)?;
    let status: i64 = row.get(base + 1)?;
    let width: Option<i64> = row.get(base + 2)?;
    let height: Option<i64> = row.get(base + 3)?;
    let pixels = width
        .and_then(|w| u32::try_from(widen_to_u64(w)).ok())
        .zip(height.and_then(|h| u32::try_from(widen_to_u64(h)).ok()));
    Ok(PhotoEntry {
        id,
        rel_path,
        label: Label::from_u8(u8::try_from(label).unwrap_or(0)).unwrap_or_default(),
        status: PhotoStatus::from_u8(u8::try_from(status).unwrap_or(0)).unwrap_or_default(),
        pixels,
        orientation: u16::try_from(row.get::<_, i64>(base + 4)?).unwrap_or(1),
        rot_cw: rot_cw_of(row.get::<_, i64>(base + 7)?),
        thumb_path: row.get::<_, Option<String>>(base + 5)?.map(PathBuf::from),
        err_msg: row.get(base + 6)?,
    })
}

/// Clamps a stored `rot_cw` into the canonical `0..4` range; out-of-range
/// or negative values (hand-edited DBs) wrap rather than panic.
fn rot_cw_of(value: i64) -> u8 {
    value.rem_euclid(4) as u8
}

fn is_nested(rel_path: &Path) -> bool {
    rel_path.components().count() > 1
}

/// Wall-clock milliseconds since the Unix epoch, for `last_opened` /
/// `ingested_at` stamps; saturates at 0 if the clock is before 1970.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|delta| delta.as_millis() as i64)
        .unwrap_or(0)
}

/// Signed nanoseconds since the Unix epoch; pre-epoch times go negative so
/// mtimes round-trip losslessly regardless of filesystem epoch offsets.
fn system_time_to_i64(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(delta) => nanos_to_i64(delta.as_nanos()),
        Err(err) => -nanos_to_i64(err.duration().as_nanos()),
    }
}

/// Inverse of [`system_time_to_i64`]; reconstructs the exact stat instant
/// stored during a scan.
fn i64_to_system_time(value: i64) -> SystemTime {
    if value >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(u64::try_from(value).unwrap_or(0))
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_nanos(value.unsigned_abs())
    }
}

fn nanos_to_i64(nanos: u128) -> i64 {
    nanos.min(i64::MAX as u128) as i64
}

fn narrow_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn narrow_usize(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn widen_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    // Test setup asserts hard failures; a broken fixture aborts the test.
    #![expect(clippy::expect_used)]

    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        // Kept alive so the database directory is not deleted mid-test.
        _dir: TempDir,
        db: Db,
    }

    fn open_db() -> Fixture {
        let dir = TempDir::new().expect("temp dir");
        let db = Db::open(&dir.path().join("index.db")).expect("open db");
        Fixture { _dir: dir, db }
    }

    fn meta(root: &Path, name: &str, mtime_secs: u64, size: u64) -> PhotoMeta {
        PhotoMeta {
            root: root.to_owned(),
            rel_path: PathBuf::from(name),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
            size,
        }
    }

    fn scalar(db: &Db, sql: &str) -> i64 {
        db.with_conn(|conn| Ok(conn.query_row(sql, [], |row| row.get(0))?))
            .expect("scalar query")
    }

    fn text(db: &Db, sql: &str, id: PhotoId) -> Option<String> {
        db.with_conn(|conn| {
            let mut stmt = conn.prepare(sql)?;
            let mut rows = stmt.query([narrow_u64(id.0)])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get(0)?)),
                None => Ok(None),
            }
        })
        .expect("text query")
    }

    /// Freshly scanned rows carry only identity + pending state; every
    /// ingest-filled column is empty.
    fn pending_entry() -> PhotoEntry {
        PhotoEntry {
            id: PhotoId(0),
            rel_path: PathBuf::new(),
            label: Label::None,
            status: PhotoStatus::Pending,
            pixels: None,
            orientation: 1,
            rot_cw: 0,
            thumb_path: None,
            err_msg: None,
        }
    }

    #[test]
    fn open_should_apply_latest_schema_version_on_fresh_database() {
        let fx = open_db();
        assert_eq!(scalar(&fx.db, "PRAGMA user_version"), LATEST_VERSION);
    }

    #[test]
    fn open_should_refuse_databases_from_newer_schema_versions() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("index.db");
        Db::open(&path).expect("seed open");
        let raw = Connection::open(&path).expect("raw open");
        raw.pragma_update(None, "user_version", LATEST_VERSION + 1)
            .expect("bump version");
        drop(raw);

        let result = Db::open(&path);

        assert!(matches!(result, Err(DbError::FutureVersion { .. })));
    }

    #[test]
    fn open_should_enable_wal_journal_mode() {
        let fx = open_db();
        let mode = fx
            .db
            .with_conn(|conn| {
                Ok(conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?)
            })
            .expect("journal mode");
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn open_should_create_missing_parent_directories() {
        let dir = TempDir::new().expect("temp dir");
        let nested = dir.path().join("a/b/index.db");

        let db = Db::open(&nested).expect("open nested");

        assert!(db.path().is_file());
    }

    #[test]
    fn reopen_should_preserve_previously_written_data() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("index.db");
        Db::open(&path)
            .expect("first open")
            .kv_set("zoom", "256")
            .expect("kv_set");

        let reopened = Db::open(&path).expect("reopen");

        assert_eq!(
            reopened.kv_get("zoom").expect("kv_get"),
            Some("256".to_owned())
        );
    }

    #[test]
    fn sync_scan_should_insert_new_photos_as_pending_sorted_by_rel_path() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "b.arw", 1, 10), meta(root, "a.nef", 2, 20)];

        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");

        assert_eq!(
            entries,
            vec![
                PhotoEntry {
                    id: entries[0].id,
                    rel_path: "a.nef".into(),
                    ..pending_entry()
                },
                PhotoEntry {
                    id: entries[1].id,
                    rel_path: "b.arw".into(),
                    ..pending_entry()
                },
            ]
        );
    }

    #[test]
    fn resync_should_reuse_ids_and_add_zero_redundant_rows() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10), meta(root, "b.arw", 2, 20)];
        let first = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("first sync");

        let second = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("second sync");

        assert_eq!(second, first);
        assert_eq!(scalar(&fx.db, "SELECT count(*) FROM photos"), 2);
    }

    #[test]
    fn rescan_should_return_the_same_rows_as_a_real_double_open() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join("IMG_0001.CR3"), b"abc").expect("write");
        std::fs::write(dir.path().join("IMG_0002.CR3"), b"abcd").expect("write");
        let db = Db::open(&dir.path().join("index.db")).expect("open");

        let first = db
            .sync_scan(
                dir.path(),
                &crate::scan_folder(dir.path(), ScanOptions::default()).expect("scan"),
                ScanOptions::default(),
            )
            .expect("first sync");
        let second = db
            .sync_scan(
                dir.path(),
                &crate::scan_folder(dir.path(), ScanOptions::default()).expect("rescan"),
                ScanOptions::default(),
            )
            .expect("second sync");

        assert_eq!(second, first);
    }

    #[test]
    fn sync_scan_should_reset_changed_files_to_pending_keeping_their_id() {
        let fx = open_db();
        let root = fx._dir.path();
        let original = vec![meta(root, "a.nef", 1, 10)];
        let first = fx
            .db
            .sync_scan(root, &original, ScanOptions::default())
            .expect("first sync");
        let changed = vec![meta(root, "a.nef", 1, 99)];

        let second = fx
            .db
            .sync_scan(root, &changed, ScanOptions::default())
            .expect("second sync");

        assert_eq!(second[0].id, first[0].id);
        assert_eq!(second[0].status, PhotoStatus::Pending);
    }

    #[test]
    fn sync_scan_should_reset_files_with_new_mtime_to_pending() {
        let fx = open_db();
        let root = fx._dir.path();
        let original = vec![meta(root, "a.nef", 1, 10)];
        fx.db
            .sync_scan(root, &original, ScanOptions::default())
            .expect("first sync");
        let touched = vec![meta(root, "a.nef", 2, 10)];

        let second = fx
            .db
            .sync_scan(root, &touched, ScanOptions::default())
            .expect("second sync");

        assert_eq!(second[0].status, PhotoStatus::Pending);
    }

    #[test]
    fn sync_scan_should_keep_ingested_state_for_untouched_files() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let first = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("first sync");
        fx.db
            .record_ingest_ok(
                first[0].id,
                &IngestInfo {
                    width: 100,
                    height: 50,
                    orientation: 1,
                    camera: None,
                    lens: None,
                    taken_at: None,
                    shutter: None,
                    aperture: None,
                    iso: None,
                    focal_mm: None,
                    preview_path: "p.jpg".into(),
                    thumb_path: "t.jpg".into(),
                },
            )
            .expect("ingest ok");

        let second = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("resync");

        assert_eq!(second[0].status, PhotoStatus::Ok);
    }

    #[test]
    fn sync_scan_should_mark_vanished_top_level_files_missing() {
        let fx = open_db();
        let root = fx._dir.path();
        let both = vec![meta(root, "a.nef", 1, 10), meta(root, "b.arw", 2, 20)];
        let first = fx
            .db
            .sync_scan(root, &both, ScanOptions::default())
            .expect("first sync");
        let survivor_only = vec![meta(root, "a.nef", 1, 10)];

        fx.db
            .sync_scan(root, &survivor_only, ScanOptions::default())
            .expect("resync");

        assert_eq!(
            scalar(
                &fx.db,
                &format!("SELECT status = 3 FROM photos WHERE id = {}", first[1].id.0)
            ),
            1
        );
    }

    #[test]
    fn shallow_resync_should_leave_subfolder_rows_alone() {
        let fx = open_db();
        let root = fx._dir.path();
        let recursive = vec![
            meta(root, "top.rw2", 1, 10),
            meta(root, "sub/nested.cr3", 2, 20),
        ];
        fx.db
            .sync_scan(root, &recursive, ScanOptions { recursive: true })
            .expect("recursive sync");
        let shallow = vec![meta(root, "top.rw2", 1, 10)];

        fx.db
            .sync_scan(root, &shallow, ScanOptions::default())
            .expect("shallow resync");

        let nested_status = scalar(
            &fx.db,
            "SELECT status FROM photos WHERE rel_path = 'sub/nested.cr3'",
        );
        assert_eq!(nested_status, PhotoStatus::Pending.to_u8() as i64);
    }

    #[test]
    fn sync_scan_should_reingest_files_that_come_back_after_being_missing() {
        let fx = open_db();
        let root = fx._dir.path();
        let present = vec![meta(root, "a.nef", 1, 10)];
        fx.db
            .sync_scan(root, &present, ScanOptions::default())
            .expect("first sync");
        fx.db
            .sync_scan(root, &[], ScanOptions::default())
            .expect("mark missing");

        let back = fx
            .db
            .sync_scan(root, &present, ScanOptions::default())
            .expect("resync");

        assert_eq!(back[0].status, PhotoStatus::Pending);
    }

    #[test]
    fn sync_scan_should_preserve_labels_across_reingest() {
        let fx = open_db();
        let root = fx._dir.path();
        let original = vec![meta(root, "a.nef", 1, 10)];
        let first = fx
            .db
            .sync_scan(root, &original, ScanOptions::default())
            .expect("first sync");
        fx.db.set_label(first[0].id, Label::Green).expect("label");
        let changed = vec![meta(root, "a.nef", 9, 999)];

        let second = fx
            .db
            .sync_scan(root, &changed, ScanOptions::default())
            .expect("resync");

        assert_eq!(second[0].label, Label::Green);
    }

    #[test]
    fn pending_photos_should_return_only_unprocessed_rows_with_file_identity() {
        let fx = open_db();
        let root = fx._dir.path();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let scanned = vec![
            PhotoMeta {
                root: root.to_owned(),
                rel_path: "a.nef".into(),
                mtime,
                size: 123,
            },
            PhotoMeta {
                root: root.to_owned(),
                rel_path: "b.arw".into(),
                mtime,
                size: 456,
            },
        ];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        fx.db
            .record_ingest_ok(
                entries[0].id,
                &IngestInfo {
                    width: 1,
                    height: 1,
                    orientation: 1,
                    camera: None,
                    lens: None,
                    taken_at: None,
                    shutter: None,
                    aperture: None,
                    iso: None,
                    focal_mm: None,
                    preview_path: "p.jpg".into(),
                    thumb_path: "t.jpg".into(),
                },
            )
            .expect("ingest ok");

        let pending = fx.db.pending_photos(root).expect("pending");

        assert_eq!(
            pending,
            vec![PendingPhoto {
                id: entries[1].id,
                meta: PhotoMeta {
                    root: root.to_owned(),
                    rel_path: "b.arw".into(),
                    mtime,
                    size: 456,
                },
            }]
        );
    }

    #[test]
    fn pending_photos_should_round_trip_pre_epoch_mtimes_losslessly() {
        let fx = open_db();
        let root = fx._dir.path();
        let pre_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(5);
        let scanned = vec![PhotoMeta {
            root: root.to_owned(),
            rel_path: "old.orf".into(),
            mtime: pre_epoch,
            size: 7,
        }];
        fx.db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");

        let pending = fx.db.pending_photos(root).expect("pending");

        assert_eq!(pending[0].meta.mtime, pre_epoch);
    }

    #[test]
    fn record_ingest_ok_should_store_metadata_and_mark_row_ok() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        let info = IngestInfo {
            width: 6000,
            height: 4000,
            orientation: 6,
            camera: Some("Canon EOS R6".to_owned()),
            lens: Some("RF 24-70mm".to_owned()),
            taken_at: Some("2026-08-24 10:00:00".to_owned()),
            shutter: Some("1/250 s".to_owned()),
            aperture: Some(2.8),
            iso: Some(400),
            focal_mm: Some(35.0),
            preview_path: "/cache/previews/x.jpg".into(),
            thumb_path: "/cache/thumbs/x.jpg".into(),
        };

        fx.db
            .record_ingest_ok(entries[0].id, &info)
            .expect("ingest ok");

        assert_eq!(
            scalar(
                &fx.db,
                &format!(
                    "SELECT status = 1 AND width = 6000 AND orientation = 6 AND aperture = 2.8 FROM photos WHERE id = {}",
                    entries[0].id.0
                )
            ),
            1
        );
    }

    #[test]
    fn record_ingest_error_should_store_message_and_mark_row_error() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");

        fx.db
            .record_ingest_error(entries[0].id, "unsupported RAW")
            .expect("ingest error");

        assert_eq!(
            text(
                &fx.db,
                "SELECT err_msg FROM photos WHERE id = ?",
                entries[0].id
            )
            .as_deref(),
            Some("unsupported RAW")
        );
        assert_eq!(
            scalar(
                &fx.db,
                &format!(
                    "SELECT status = 2 FROM photos WHERE id = {}",
                    entries[0].id.0
                )
            ),
            1
        );
    }

    #[test]
    fn photo_entry_should_surface_ingested_cell_state() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        fx.db
            .record_ingest_ok(
                entries[0].id,
                &IngestInfo {
                    width: 6000,
                    height: 4000,
                    orientation: 6,
                    camera: None,
                    lens: None,
                    taken_at: None,
                    shutter: None,
                    aperture: None,
                    iso: None,
                    focal_mm: None,
                    preview_path: "/cache/previews/x.jpg".into(),
                    thumb_path: "/cache/thumbs/x.jpg".into(),
                },
            )
            .expect("ingest ok");

        let entry = fx.db.photo_entry(entries[0].id).expect("photo_entry");

        assert_eq!(entry.as_ref().map(|e| e.id), Some(entries[0].id));
        let entry = entry.expect("row exists");
        assert_eq!(entry.status, PhotoStatus::Ok);
        assert_eq!(entry.pixels, Some((6000, 4000)));
        assert_eq!(entry.orientation, 6);
        assert_eq!(entry.thumb_path, Some("/cache/thumbs/x.jpg".into()));
    }

    #[test]
    fn photo_entry_should_carry_error_messages_for_failed_rows() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        fx.db
            .record_ingest_error(entries[0].id, "corrupt header")
            .expect("ingest error");

        let entry = fx.db.photo_entry(entries[0].id).expect("photo_entry");

        let entry = entry.expect("row exists");
        assert_eq!(entry.status, PhotoStatus::Error);
        assert_eq!(entry.err_msg.as_deref(), Some("corrupt header"));
        assert_eq!(entry.thumb_path, None);
    }

    #[test]
    fn photo_detail_should_return_the_full_loupe_record() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        fx.db
            .record_ingest_ok(
                entries[0].id,
                &IngestInfo {
                    width: 6000,
                    height: 4000,
                    orientation: 6,
                    camera: Some("Canon EOS R6".to_owned()),
                    lens: Some("RF 24-70mm".to_owned()),
                    taken_at: Some("2026-08-24 10:00:00".to_owned()),
                    shutter: Some("1/250 s".to_owned()),
                    aperture: Some(2.8),
                    iso: Some(400),
                    focal_mm: Some(35.0),
                    preview_path: "/cache/previews/x.jpg".into(),
                    thumb_path: "/cache/thumbs/x.jpg".into(),
                },
            )
            .expect("ingest ok");

        let detail = fx.db.photo_detail(entries[0].id).expect("photo_detail");

        let detail = detail.expect("row exists");
        assert_eq!(detail.id, entries[0].id);
        assert_eq!(detail.status, PhotoStatus::Ok);
        assert_eq!(detail.pixels, Some((6000, 4000)));
        assert_eq!(detail.orientation, 6);
        assert_eq!(detail.preview_path, Some("/cache/previews/x.jpg".into()));
        assert_eq!(detail.camera.as_deref(), Some("Canon EOS R6"));
        assert_eq!(detail.aperture, Some(2.8));
        assert_eq!(detail.iso, Some(400));
        assert_eq!(detail.focal_mm, Some(35.0));
        // Pending rows keep every ingest-filled column empty.
        assert_eq!(detail.label, Label::None);
        assert_eq!(detail.err_msg, None);
    }

    #[test]
    fn photo_detail_should_return_none_for_unknown_ids() {
        let fx = open_db();

        let detail = fx.db.photo_detail(PhotoId(9_999)).expect("photo_detail");

        assert_eq!(detail, None);
    }

    #[test]
    fn photo_entry_should_return_none_for_unknown_ids() {
        let fx = open_db();

        let entry = fx.db.photo_entry(PhotoId(9_999)).expect("photo_entry");

        assert_eq!(entry, None);
    }

    #[test]
    fn pending_entries_should_default_orientation_and_leave_assets_empty() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");

        assert_eq!(
            entries,
            vec![PhotoEntry {
                id: entries[0].id,
                rel_path: "a.nef".into(),
                ..pending_entry()
            }]
        );
    }

    #[test]
    fn entries_for_root_should_exclude_missing_photos() {
        let fx = open_db();
        let root = fx._dir.path();
        let both = vec![meta(root, "a.nef", 1, 10), meta(root, "b.arw", 2, 20)];
        fx.db
            .sync_scan(root, &both, ScanOptions::default())
            .expect("first sync");
        let survivor_only = vec![meta(root, "a.nef", 1, 10)];

        let entries = fx
            .db
            .sync_scan(root, &survivor_only, ScanOptions::default())
            .expect("resync");

        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn set_label_should_persist_across_close_and_reopen() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("index.db");
        let root = dir.path();
        let db = Db::open(&path).expect("open");
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        let id = entries[0].id;
        db.set_label(id, Label::Purple).expect("label");
        drop(db);

        let reopened = Db::open(&path).expect("reopen");

        assert_eq!(
            scalar(
                &reopened,
                &format!(
                    "SELECT label = {} FROM photos WHERE id = {}",
                    Label::Purple.to_u8(),
                    id.0
                )
            ),
            1
        );
    }

    #[test]
    fn set_labels_should_relabel_every_listed_row_in_one_commit() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10), meta(root, "b.arw", 2, 20)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        let ids: Vec<PhotoId> = entries.iter().map(|entry| entry.id).collect();

        fx.db.set_labels(&ids, Label::Green).expect("labels");

        for id in ids {
            assert_eq!(
                scalar(
                    &fx.db,
                    &format!("SELECT label FROM photos WHERE id = {}", id.0)
                ),
                Label::Green.to_u8() as i64
            );
        }
    }

    #[test]
    fn set_labels_should_skip_unknown_ids_without_failing() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");

        // One live row plus a stale one from a vanished photo; the batch
        // must land on the former and shrug off the latter.
        fx.db
            .set_labels(&[entries[0].id, PhotoId(999_999)], Label::Blue)
            .expect("labels");

        assert_eq!(
            scalar(
                &fx.db,
                &format!("SELECT label FROM photos WHERE id = {}", entries[0].id.0)
            ),
            Label::Blue.to_u8() as i64
        );
    }

    #[test]
    fn set_rotation_should_wrap_and_persist_across_reopen() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("index.db");
        let root = dir.path();
        let db = Db::open(&path).expect("open");
        let scanned = vec![meta(root, "a.nef", 1, 10)];
        let entries = db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");
        let id = entries[0].id;
        // Three CW turns wrap to one CCW turn; the stored value must be
        // canonical 0..4 either way.
        db.set_rotation(id, 7).expect("rotate");
        drop(db);

        let reopened = Db::open(&path).expect("reopen");
        let entry = reopened.photo_entry(id).expect("read").expect("row exists");
        assert_eq!(entry.rot_cw, 3);
    }

    #[test]
    fn set_rotations_should_land_every_update_in_one_commit() {
        let fx = open_db();
        let root = fx._dir.path();
        let scanned = vec![meta(root, "a.nef", 1, 10), meta(root, "b.arw", 2, 20)];
        let entries = fx
            .db
            .sync_scan(root, &scanned, ScanOptions::default())
            .expect("sync");

        fx.db
            .set_rotations(&[(entries[0].id, 1), (entries[1].id, 3)])
            .expect("batch rotate");
        // Empty batches are a cheap no-op.
        fx.db.set_rotations(&[]).expect("empty batch");

        assert_eq!(
            scalar(
                &fx.db,
                &format!("SELECT rot_cw FROM photos WHERE id = {}", entries[0].id.0)
            ),
            1
        );
        assert_eq!(
            scalar(
                &fx.db,
                &format!("SELECT rot_cw FROM photos WHERE id = {}", entries[1].id.0)
            ),
            3
        );
    }

    #[test]
    fn rotation_should_survive_a_file_change_and_reingest() {
        let fx = open_db();
        let root = fx._dir.path();
        let original = vec![meta(root, "a.nef", 1, 10)];
        let first = fx
            .db
            .sync_scan(root, &original, ScanOptions::default())
            .expect("first sync");
        fx.db.set_rotation(first[0].id, 2).expect("rotate");

        // The file was edited on disk: the row resets for re-extraction,
        // but the user's display rotation is a preference, like the label.
        let changed = vec![meta(root, "a.nef", 9, 999)];
        let second = fx
            .db
            .sync_scan(root, &changed, ScanOptions::default())
            .expect("resync");

        assert_eq!(second[0].status, PhotoStatus::Pending);
        assert_eq!(second[0].rot_cw, 2);
    }

    #[test]
    fn open_should_migrate_a_v1_database_to_v2_with_rotation_defaults() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("index.db");
        {
            // Build a genuine v1 database with one ingested photo.
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(MIGRATIONS[0]).expect("v1 schema");
            conn.execute_batch(
                "INSERT INTO photos (root, rel_path, mtime, size, status) VALUES
                 ('/p', 'a.nef', 1, 10, 1);",
            )
            .expect("seed row");
            conn.pragma_update(None, "user_version", 1)
                .expect("stamp v1");
        }

        let db = Db::open(&path).expect("migrating open");
        let entry = db
            .photo_entry(PhotoId(1))
            .expect("read")
            .expect("row survives migration");
        assert_eq!(entry.rot_cw, 0);
        assert_eq!(scalar(&db, "PRAGMA user_version"), LATEST_VERSION);
    }

    #[test]
    fn touch_root_should_upsert_without_duplicating_roots() {
        let fx = open_db();

        fx.db
            .touch_root(Path::new("/photos/a"), 100)
            .expect("touch");
        fx.db
            .touch_root(Path::new("/photos/a"), 200)
            .expect("re-touch");

        assert_eq!(scalar(&fx.db, "SELECT count(*) FROM roots"), 1);
    }

    #[test]
    fn recent_roots_should_order_most_recently_touched_first() {
        let fx = open_db();
        fx.db
            .touch_root(Path::new("/photos/old"), 100)
            .expect("touch old");
        fx.db
            .touch_root(Path::new("/photos/new"), 300)
            .expect("touch new");

        let recent = fx.db.recent_roots(10).expect("recent");

        assert_eq!(
            recent,
            vec![PathBuf::from("/photos/new"), PathBuf::from("/photos/old")]
        );
    }

    #[test]
    fn recent_roots_should_respect_limit() {
        let fx = open_db();
        fx.db.touch_root(Path::new("/a"), 1).expect("touch a");
        fx.db.touch_root(Path::new("/b"), 2).expect("touch b");
        fx.db.touch_root(Path::new("/c"), 3).expect("touch c");

        let recent = fx.db.recent_roots(2).expect("recent");

        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn kv_should_round_trip_a_value() {
        let fx = open_db();

        fx.db.kv_set("auto_advance", "on").expect("kv_set");

        assert_eq!(
            fx.db.kv_get("auto_advance").expect("kv_get"),
            Some("on".to_owned())
        );
    }

    #[test]
    fn kv_set_should_overwrite_an_existing_key() {
        let fx = open_db();
        fx.db.kv_set("cell", "128").expect("first set");

        fx.db.kv_set("cell", "512").expect("second set");

        assert_eq!(
            fx.db.kv_get("cell").expect("kv_get"),
            Some("512".to_owned())
        );
    }

    #[test]
    fn kv_get_should_return_none_for_unknown_keys() {
        let fx = open_db();
        assert_eq!(fx.db.kv_get("nope").expect("kv_get"), None);
    }

    #[test]
    fn db_should_be_shareable_across_threads_per_spec_threading_rules() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Db>();
    }

    #[test]
    fn mtime_conversion_should_survive_epoch_extremes() {
        assert_eq!(system_time_to_i64(SystemTime::UNIX_EPOCH), 0);
    }
}
