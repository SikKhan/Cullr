//! Cullr core engine.
//!
//! GUI-free library behind the Cullr culling tool: folder scanning, RAW
//! preview extraction, thumbnailing, the SQLite index and color labels.
//!
//! Everything in this crate is usable without a display server; the
//! [`cullr-ui`](https://crates.io/crates/cullr-ui) binary drives it from an
//! egui front end.
//!
//! ```
//! assert!(!cullr_core::VERSION.is_empty());
//! ```
//!
//! ```
//! use cullr_core::{scan_folder, ScanOptions};
//!
//! // The engine is GUI-free; scanning only needs a filesystem path.
//! let dir = tempfile::tempdir().unwrap();
//! std::fs::write(dir.path().join("IMG_0001.CR3"), b"x").unwrap();
//! let photos = scan_folder(dir.path(), ScanOptions::default()).unwrap();
//! assert_eq!(photos.len(), 1);
//! ```

pub mod cache;
pub mod db;
pub mod exif;
pub mod export;
pub mod extract;
pub mod ingest;
pub mod model;
pub mod scanner;

pub use cache::{Cache, CacheError};
pub use db::{Db, DbError, now_millis};
pub use export::{ExportError, ExportFailure, ExportReport, existing_names, export_files};
pub use extract::{ExtractError, extract_file, is_supported_extension};
pub use ingest::{IngestEvent, IngestPipeline};
pub use model::{
    DEFAULT_ASPECT, IngestInfo, Label, PendingPhoto, PhotoDetail, PhotoEntry, PhotoId, PhotoMeta,
    PhotoStatus,
};
pub use scanner::{ScanError, ScanOptions, scan_folder};

/// Semantic version of the engine, mirroring the crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
