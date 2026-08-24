//! Core data types shared across the engine: photo identity, scan metadata
//! and color labels.
//!
//! These types are plain data with no I/O or GUI dependencies so they can be
//! freely passed between threads (scanner, ingest pool, UI).

use std::path::PathBuf;
use std::time::SystemTime;

/// Stable identifier for a photo row in the index database.
///
/// Assigned by [`crate::db`] once a scanned file is persisted; the scanner
/// emits entries before they have an id, so treat this as the handle used
/// everywhere after ingest registration.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhotoId(pub u64);

/// A single photo file as discovered by the scanner.
///
/// Holds only what `stat` can tell us (identity fields per the index schema);
/// EXIF-derived attributes are attached later during ingest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotoMeta {
    /// Folder the scan was rooted at, as given by the caller.
    pub root: PathBuf,
    /// Path of the file relative to [`PhotoMeta::root`].
    pub rel_path: PathBuf,
    /// Last modification time reported by the filesystem; part of the cache
    /// identity together with `root`, `rel_path` and `size`.
    pub mtime: SystemTime,
    /// File size in bytes.
    pub size: u64,
}

/// Color label applied to a photo during culling.
///
/// Numeric mapping matches the `label` column in the index database:
/// `0 = None`, `1..5 = Red, Yellow, Green, Blue, Purple`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Label {
    /// No label yet.
    #[default]
    None,
    /// Reject-style red mark (`1`).
    Red,
    /// Yellow mark (`2`).
    Yellow,
    /// Green mark (`3`).
    Green,
    /// Blue mark (`4`).
    Blue,
    /// Purple / select mark (`5`).
    Purple,
}

impl Label {
    /// Every representable label, in database numeric order.
    pub const ALL: [Label; 6] = [
        Label::None,
        Label::Red,
        Label::Yellow,
        Label::Green,
        Label::Blue,
        Label::Purple,
    ];

    /// Database representation of this label.
    pub fn to_u8(self) -> u8 {
        match self {
            Label::None => 0,
            Label::Red => 1,
            Label::Yellow => 2,
            Label::Green => 3,
            Label::Blue => 4,
            Label::Purple => 5,
        }
    }

    /// Parses the database representation, rejecting out-of-range values.
    pub fn from_u8(value: u8) -> Option<Label> {
        match value {
            0 => Some(Label::None),
            1 => Some(Label::Red),
            2 => Some(Label::Yellow),
            3 => Some(Label::Green),
            4 => Some(Label::Blue),
            5 => Some(Label::Purple),
            _ => None,
        }
    }
}

/// Lifecycle of a photo row in the index, mirroring the `status` column.
///
/// Numeric mapping matches the database: `0 = Pending`, `1 = Ok`,
/// `2 = Error`, `3 = Missing`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PhotoStatus {
    /// Discovered by a scan, awaiting preview extraction (`0`).
    #[default]
    Pending,
    /// Preview and thumbnail extracted successfully (`1`).
    Ok,
    /// Extraction failed; the row carries an error message (`2`).
    Error,
    /// File vanished from disk since the last scan (`3`).
    Missing,
}

impl PhotoStatus {
    /// Every representable status, in database numeric order.
    pub const ALL: [PhotoStatus; 4] = [
        PhotoStatus::Pending,
        PhotoStatus::Ok,
        PhotoStatus::Error,
        PhotoStatus::Missing,
    ];

    /// Database representation of this status.
    pub fn to_u8(self) -> u8 {
        match self {
            PhotoStatus::Pending => 0,
            PhotoStatus::Ok => 1,
            PhotoStatus::Error => 2,
            PhotoStatus::Missing => 3,
        }
    }

    /// Parses the database representation, rejecting out-of-range values.
    pub fn from_u8(value: u8) -> Option<PhotoStatus> {
        match value {
            0 => Some(PhotoStatus::Pending),
            1 => Some(PhotoStatus::Ok),
            2 => Some(PhotoStatus::Error),
            3 => Some(PhotoStatus::Missing),
            _ => None,
        }
    }
}

/// A photo row as served by the index after a scan sync.
///
/// This is everything a grid cell renders: identity, pipeline state, and —
/// once ingest has touched the row — the thumbnail cache path plus preview
/// pixel size for aspect-fit layout. Fields beyond [`PhotoEntry::status`]
/// stay `None`/default until extraction fills them via ingest events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotoEntry {
    /// Stable row handle used by all subsequent commands.
    pub id: PhotoId,
    /// Path of the file relative to its scan root.
    pub rel_path: PathBuf,
    /// Color label persisted from previous culling sessions.
    pub label: Label,
    /// Current pipeline state of the row.
    pub status: PhotoStatus,
    /// Pixel size `(width, height)` of the extracted preview, before any
    /// orientation rotation is applied; `None` until ingest succeeds.
    pub pixels: Option<(u32, u32)>,
    /// EXIF orientation flag (`1..8`) that must be applied on top of
    /// [`PhotoEntry::pixels`] for display; defaults to upright (`1`).
    pub orientation: u16,
    /// Cache path of the downscaled thumbnail JPEG; `None` until ingest
    /// succeeds or for rows whose extraction failed.
    pub thumb_path: Option<PathBuf>,
    /// Extraction failure description shown in error tiles (SPEC §7);
    /// `None` unless status is [`PhotoStatus::Error`].
    pub err_msg: Option<String>,
}

impl PhotoEntry {
    /// Aspect ratio (width / height) to lay out this photo with: the stored
    /// preview size rotated by the EXIF orientation flag. Falls back to the
    /// common 3:2 landscape default while the row is still pending.
    ///
    /// Orientations 5..=8 are the 90° swaps, so they transpose the ratio;
    /// mirrored variants keep the same silhouette as their base value.
    pub fn display_aspect(&self) -> f32 {
        let Some((width, height)) = self.pixels else {
            return DEFAULT_ASPECT;
        };
        if width == 0 || height == 0 {
            return DEFAULT_ASPECT;
        }
        let raw = width as f32 / height as f32;
        if (5..=8).contains(&self.orientation) {
            1.0 / raw
        } else {
            raw
        }
    }
}

/// Aspect ratio assumed for photos whose preview has not been measured yet.
pub const DEFAULT_ASPECT: f32 = 3.0 / 2.0;

/// A photo row awaiting ingest: its stable id plus the on-disk identity
/// needed to extract it.
///
/// Produced by [`crate::db`] when a folder opens; consumed by the ingest
/// pipeline, which turns each one into an `Ok`/`Error` row plus an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPhoto {
    /// Index row handle.
    pub id: PhotoId,
    /// File identity used for extraction and cache keying.
    pub meta: PhotoMeta,
}

/// Metadata and cache paths captured during a successful ingest.
///
/// Produced by the extraction module, written to the index by [`crate::db`];
/// every field maps to a `photos` column.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestInfo {
    /// Pixel width of the extracted preview.
    pub width: u32,
    /// Pixel height of the extracted preview.
    pub height: u32,
    /// EXIF orientation flag (`1..8`, per the TIFF spec).
    pub orientation: u16,
    /// Camera make/model string, when EXIF provides one.
    pub camera: Option<String>,
    /// Lens designation string, when EXIF provides one.
    pub lens: Option<String>,
    /// Capture timestamp formatted for display, when EXIF provides one.
    pub taken_at: Option<String>,
    /// Shutter speed formatted for display (e.g. `1/250 s`).
    pub shutter: Option<String>,
    /// Aperture as f-number (e.g. `2.8`).
    pub aperture: Option<f64>,
    /// ISO sensitivity rating.
    pub iso: Option<u32>,
    /// Focal length in millimeters.
    pub focal_mm: Option<f64>,
    /// Cache path of the full-size re-encoded preview JPEG.
    pub preview_path: PathBuf,
    /// Cache path of the downscaled thumbnail JPEG.
    pub thumb_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_aspect_should_fall_back_to_three_halves_while_pending() {
        let entry = PhotoEntry {
            id: PhotoId(1),
            rel_path: "a.nef".into(),
            label: Label::None,
            status: PhotoStatus::Pending,
            pixels: None,
            orientation: 1,
            thumb_path: None,
            err_msg: None,
        };
        assert!((entry.display_aspect() - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn display_aspect_should_rotate_for_quarter_turn_orientations() {
        let make = |orientation: u16| PhotoEntry {
            id: PhotoId(1),
            rel_path: "a.nef".into(),
            label: Label::None,
            status: PhotoStatus::Ok,
            pixels: Some((6000, 4000)),
            orientation,
            thumb_path: None,
            err_msg: None,
        };
        assert!((make(1).display_aspect() - 1.5).abs() < 0.01);
        assert!((make(3).display_aspect() - 1.5).abs() < 0.01);
        assert!((make(6).display_aspect() - 2.0 / 3.0).abs() < 0.01);
        assert!((make(8).display_aspect() - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn label_to_u8_should_match_database_mapping() {
        assert_eq!(Label::None.to_u8(), 0);
    }

    #[test]
    fn label_to_u8_should_map_purple_to_five() {
        assert_eq!(Label::Purple.to_u8(), 5);
    }

    #[test]
    fn label_from_u8_should_round_trip_every_variant() {
        for expected in Label::ALL {
            assert_eq!(Label::from_u8(expected.to_u8()), Some(expected));
        }
    }

    #[test]
    fn label_from_u8_should_reject_out_of_range_value() {
        assert_eq!(Label::from_u8(6), None);
    }

    #[test]
    fn photo_status_round_trip_should_cover_every_variant() {
        for expected in PhotoStatus::ALL {
            assert_eq!(PhotoStatus::from_u8(expected.to_u8()), Some(expected));
        }
    }

    #[test]
    fn photo_status_from_u8_should_reject_out_of_range_value() {
        assert_eq!(PhotoStatus::from_u8(4), None);
    }
}
