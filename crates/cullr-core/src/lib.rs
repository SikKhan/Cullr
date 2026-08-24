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

/// Semantic version of the engine, mirroring the crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
