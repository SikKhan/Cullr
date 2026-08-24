//! RAW preview extraction — the only module in the workspace that touches
//! rawler.
//!
//! Per SPEC §5.2, extraction is header-parse + embedded-JPEG work only: read
//! metadata, walk the preview fallback chain (`preview_image` →
//! `full_image` → `thumbnail_image`), re-encode to the cache. Sensor data is
//! never decoded. All rawler calls run under `catch_unwind` so a panic on a
//! corrupt file degrades to an error row instead of killing a worker.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::OnceLock;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder};
use rawler::decoders::{Decoder, RawDecodeParams, RawLoader, RawMetadata};
use rawler::rawsource::RawSource;
use thiserror::Error;

use crate::cache::{Cache, CacheError, THUMB_LONG_EDGE};
use crate::exif;
use crate::model::{IngestInfo, PhotoMeta};

/// JPEG quality for cached full-size previews (SPEC §4).
const PREVIEW_QUALITY: u8 = 88;
/// JPEG quality for cached thumbnails (SPEC §4).
const THUMB_QUALITY: u8 = 85;

/// Shared rawler loader; building it parses the embedded camera databases,
/// so every worker reuses one instance.
fn loader() -> &'static RawLoader {
    static LOADER: OnceLock<RawLoader> = OnceLock::new();
    LOADER.get_or_init(RawLoader::new)
}

/// Failure modes of [`extract_file`].
#[derive(Error, Debug)]
pub enum ExtractError {
    /// The RAW file could not be opened from disk.
    #[error("cannot open RAW file `{}`", .path.display())]
    Open {
        /// The offending path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// Rawler has no decoder for this file (unknown vendor or corrupt
    /// header); the row becomes an error tile per SPEC §7.
    #[error("unsupported or corrupt RAW file `{}`", .0.display())]
    Unsupported(PathBuf),
    /// Decoding or metadata parsing failed.
    #[error("failed to extract `{}`: {}", .path.display(), .message)]
    Decode {
        /// The offending path.
        path: PathBuf,
        /// Human-readable failure description surfaced in error tiles.
        message: String,
    },
    /// A rawler call panicked; caught by design so one bad file never takes
    /// down the ingest pool (SPEC §5.2 step 6).
    #[error("extracting `{}` panicked: {}", .path.display(), .message)]
    Panicked {
        /// The offending path.
        path: PathBuf,
        /// Panic payload if it carried a string message.
        message: String,
    },
    /// Writing preview/thumb assets into the cache failed.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

/// Returns `true` when the scanner should hand this lowercase-or-not
/// extension (bytes without leading dot) to the extractor: the hardcoded
/// culling list intersected with rawler's actually supported set (SPEC §5.1).
pub fn is_supported_extension(extension: &[u8]) -> bool {
    crate::scanner::EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
        && rawler::decoders::supported_extensions()
            .iter()
            .any(|known| known.as_bytes().eq_ignore_ascii_case(extension))
}

/// Extracts preview, thumbnail and EXIF summary for one photo.
///
/// Writes `previews/<hash>.jpg` and `thumbs/<hash>.jpg` into `cache` and
/// returns everything the index row needs. Never panics on hostile input;
/// every failure mode is a typed [`ExtractError`] so the ingest pipeline can
/// mark the row as errored and move on.
///
/// ```
/// use std::time::SystemTime;
///
/// use cullr_core::{extract_file, Cache, PhotoMeta};
///
/// let dir = tempfile::tempdir().unwrap();
/// let cache = Cache::new(dir.path().join("cache"));
/// let photo = PhotoMeta {
///     root: dir.path().to_owned(),
///     rel_path: "garbage.CR3".into(),
///     mtime: SystemTime::UNIX_EPOCH,
///     size: 3,
/// };
/// std::fs::write(dir.path().join("garbage.CR3"), b"junk").unwrap();
///
/// // Not every .CR3 is a CR3: garbage must yield a typed error, not a crash.
/// assert!(extract_file(&photo, &cache).is_err());
/// ```
pub fn extract_file(photo: &PhotoMeta, cache: &Cache) -> Result<IngestInfo, ExtractError> {
    let path = photo.root.join(&photo.rel_path);
    match catch_unwind(AssertUnwindSafe(|| extract_inner(photo, cache))) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload_message(&payload);
            tracing::warn!(path = %path.display(), %message, "rawler panicked during extraction");
            Err(ExtractError::Panicked { path, message })
        }
    }
}

fn extract_inner(photo: &PhotoMeta, cache: &Cache) -> Result<IngestInfo, ExtractError> {
    let path = photo.root.join(&photo.rel_path);
    let mtime_nanos = mtime_unix_nanos(photo.mtime);
    let hash = Cache::asset_hash(&photo.root, &photo.rel_path, mtime_nanos, photo.size);

    let rawfile = RawSource::new(&path).map_err(|source| ExtractError::Open {
        path: path.clone(),
        source,
    })?;
    let decoder = loader()
        .get_decoder(&rawfile)
        .map_err(|_| ExtractError::Unsupported(path.clone()))?;

    let metadata = decoder
        .raw_metadata(&rawfile, &RawDecodeParams::default())
        .map_err(|error| ExtractError::Decode {
            path: path.clone(),
            message: error.to_string(),
        })?;

    let preview = acquire_preview(&*decoder, &rawfile, &path)?;
    let thumb = preview.thumbnail(THUMB_LONG_EDGE, THUMB_LONG_EDGE);

    let preview_bytes = encode_jpeg(&preview, PREVIEW_QUALITY, &path)?;
    let thumb_bytes = encode_jpeg(&thumb, THUMB_QUALITY, &path)?;

    let preview_path = cache.preview_path(&hash);
    let thumb_path = cache.thumb_path(&hash);
    cache.write_atomically(&preview_path, &preview_bytes)?;
    cache.write_atomically(&thumb_path, &thumb_bytes)?;

    tracing::debug!(path = %path.display(), width = preview.width(), height = preview.height(), "extracted");

    Ok(IngestInfo {
        width: preview.width(),
        height: preview.height(),
        orientation: orientation_of(&metadata),
        camera: camera_of(&metadata),
        lens: lens_of(&metadata),
        taken_at: taken_at_of(&metadata),
        shutter: shutter_of(&metadata),
        aperture: aperture_of(&metadata),
        iso: iso_of(&metadata),
        focal_mm: focal_mm_of(&metadata),
        preview_path,
        thumb_path,
    })
}

/// Preview acquisition fallback chain (SPEC §5.2 step 3): first link that
/// yields an embedded JPEG wins. Each link is individually panic-isolated so
/// one broken stage cannot poison the remaining fallbacks.
fn acquire_preview(
    decoder: &dyn Decoder,
    rawfile: &RawSource,
    path: &std::path::Path,
) -> Result<DynamicImage, ExtractError> {
    let params = RawDecodeParams::default();
    for stage in ["preview", "full", "thumbnail"] {
        let outcome = catch_unwind(AssertUnwindSafe(|| match stage {
            "preview" => decoder.preview_image(rawfile, &params),
            "full" => decoder.full_image(rawfile, &params),
            _ => decoder.thumbnail_image(rawfile, &params),
        }));
        match outcome {
            Ok(Ok(Some(image))) => {
                tracing::debug!(stage, "preview acquired");
                return Ok(image);
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                tracing::debug!(stage, %error, "fallback link failed");
            }
            Err(payload) => {
                // Swallowed here so later links still get a chance; only a
                // total failure surfaces to the caller below.
                let message = payload_message(&payload);
                tracing::debug!(stage, message, "fallback link panicked");
            }
        }
    }
    Err(ExtractError::Decode {
        path: path.to_owned(),
        message: "no embedded preview found in any fallback stage".to_owned(),
    })
}

fn encode_jpeg(
    image: &DynamicImage,
    quality: u8,
    path: &std::path::Path,
) -> Result<Vec<u8>, ExtractError> {
    let rgb = image.to_rgb8();
    let mut buffer = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut buffer, quality);
    encoder
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| ExtractError::Decode {
            path: path.to_owned(),
            message: format!("jpeg re-encode failed: {error}"),
        })?;
    Ok(buffer)
}

fn mtime_unix_nanos(mtime: std::time::SystemTime) -> u128 {
    mtime
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |age| age.as_nanos())
}

fn orientation_of(metadata: &RawMetadata) -> u16 {
    metadata
        .exif
        .orientation
        .filter(|value| (1..=8).contains(value))
        .unwrap_or(1)
}

fn camera_of(metadata: &RawMetadata) -> Option<String> {
    let make = metadata.make.trim();
    let model = metadata.model.trim();
    let joined = match (make.is_empty(), model.is_empty()) {
        (true, true) => return None,
        (true, false) => model.to_owned(),
        (false, true) => make.to_owned(),
        (false, false) => format!("{make} {model}"),
    };
    Some(joined)
}

fn lens_of(metadata: &RawMetadata) -> Option<String> {
    let exif_lens = metadata
        .exif
        .lens_model
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if let Some(text) = exif_lens {
        return Some(text.to_owned());
    }
    let detected = metadata.lens.as_ref().map(|lens| lens.lens_name.trim());
    detected.filter(|text| !text.is_empty()).map(str::to_owned)
}

fn taken_at_of(metadata: &RawMetadata) -> Option<String> {
    metadata
        .exif
        .date_time_original
        .as_deref()
        .or(metadata.exif.create_date.as_deref())
        .map(exif::format_capture_time)
}

fn shutter_of(metadata: &RawMetadata) -> Option<String> {
    if let Some(ratio) = metadata.exif.exposure_time {
        return exif::format_shutter(ratio.n, ratio.d);
    }
    // Some vendors only populate the APEX shutter-speed value; seconds = 2^-x.
    metadata
        .exif
        .shutter_speed_value
        .and_then(apex_ratio)
        .and_then(exif::format_shutter_seconds)
}

fn aperture_of(metadata: &RawMetadata) -> Option<f64> {
    if let Some(ratio) = metadata.exif.fnumber {
        let value = f64::from(ratio.n) / f64::from(ratio.d);
        if value > 0.0 && value.is_finite() {
            return Some((value * 10.0).round() / 10.0);
        }
    }
    // APEX aperture value: f-number = 2^(x/2).
    metadata
        .exif
        .aperture_value
        .and_then(|apex| positive_ratio(apex.n, apex.d))
        .map(|apex| (apex / 2.0).exp2())
        .filter(|value| *value > 0.0 && value.is_finite())
        .map(|value| (value * 10.0).round() / 10.0)
}

/// Converts an APEX shutter-speed value into seconds as `2^-x`.
fn apex_ratio(apex: rawler::formats::tiff::SRational) -> Option<f64> {
    if apex.d == 0 {
        return None;
    }
    let value = f64::from(apex.n) / f64::from(apex.d);
    value.is_finite().then_some(value)
}

fn positive_ratio(numerator: u32, denominator: u32) -> Option<f64> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    let value = f64::from(numerator) / f64::from(denominator);
    (value > 0.0 && value.is_finite()).then_some(value)
}

fn iso_of(metadata: &RawMetadata) -> Option<u32> {
    metadata
        .exif
        .iso_speed_ratings
        .map(u32::from)
        .or(metadata.exif.iso_speed)
        .or(metadata.exif.recommended_exposure_index)
        .filter(|iso| *iso > 0)
}

fn focal_mm_of(metadata: &RawMetadata) -> Option<f64> {
    let ratio = metadata.exif.focal_length?;
    let mm = f64::from(ratio.n) / f64::from(ratio.d);
    (mm > 0.0 && mm.is_finite()).then_some(mm)
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload_message(payload)
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used)]

    use std::fs;
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    use super::*;

    fn photo_with(root: &std::path::Path, name: &str, contents: &[u8]) -> PhotoMeta {
        let path = root.join(name);
        fs::write(&path, contents).expect("test setup write");
        PhotoMeta {
            root: root.to_owned(),
            rel_path: name.into(),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            size: contents.len() as u64,
        }
    }

    #[test]
    fn extract_should_reject_garbage_claiming_to_be_a_raw_file() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().join("cache"));
        let photo = photo_with(dir.path(), "fake.CR3", b"definitely not an ISOBMFF box");

        let result = extract_file(&photo, &cache);

        assert!(matches!(
            result,
            Err(ExtractError::Unsupported(_))
                | Err(ExtractError::Decode { .. })
                | Err(ExtractError::Panicked { .. })
        ));
    }

    #[test]
    fn extract_should_report_unreadable_files_without_panicking() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().join("cache"));
        let photo = PhotoMeta {
            root: dir.path().to_owned(),
            rel_path: "missing.nef".into(),
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
        };

        let result = extract_file(&photo, &cache);

        assert!(matches!(result, Err(ExtractError::Open { .. })));
    }

    #[test]
    fn extract_should_not_write_cache_entries_on_failure() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Cache::new(dir.path().join("cache"));
        let photo = photo_with(dir.path(), "broken.arw", &[0u8; 64]);

        let _ = extract_file(&photo, &cache);

        assert!(!cache.previews_dir().exists());
        assert!(!cache.thumbs_dir().exists());
    }

    #[test]
    fn supported_extension_intersection_should_keep_common_camera_formats() {
        assert!(is_supported_extension(b"cr3"));
        assert!(is_supported_extension(b"nef"));
        assert!(is_supported_extension(b"arw"));
    }

    #[test]
    fn supported_extension_intersection_should_match_case_insensitively() {
        assert!(is_supported_extension(b"CR3"));
        assert!(is_supported_extension(b"Nef"));
    }

    #[test]
    fn supported_extension_intersection_should_drop_formats_rawler_cannot_decode() {
        // In the hardcoded list but absent from rawler's supported set.
        assert!(!is_supported_extension(b"gpr"));
    }

    #[test]
    fn supported_extension_intersection_should_reject_non_raw_extensions() {
        assert!(!is_supported_extension(b"jpg"));
        assert!(!is_supported_extension(b"txt"));
        assert!(!is_supported_extension(b""));
    }

    fn exif_with_orientation(value: Option<u16>) -> RawMetadata {
        RawMetadata {
            exif: rawler::exif::Exif {
                orientation: value,
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        }
    }

    #[test]
    fn helpers_should_default_orientation_to_one_when_absent_or_invalid() {
        assert_eq!(orientation_of(&exif_with_orientation(None)), 1);
        assert_eq!(orientation_of(&exif_with_orientation(Some(9))), 1);
        assert_eq!(orientation_of(&exif_with_orientation(Some(6))), 6);
    }

    #[test]
    fn camera_should_join_make_and_model_trimmed() {
        let metadata = RawMetadata {
            make: " Canon ".to_owned(),
            model: "EOS R5 ".to_owned(),
            ..RawMetadata::default()
        };
        assert_eq!(camera_of(&metadata).as_deref(), Some("Canon EOS R5"));
    }

    #[test]
    fn camera_should_be_none_without_any_identification() {
        assert_eq!(camera_of(&RawMetadata::default()), None);
    }

    #[test]
    fn iso_should_prefer_classic_ratings_over_extended_fields() {
        let metadata = RawMetadata {
            exif: rawler::exif::Exif {
                iso_speed_ratings: Some(400),
                iso_speed: Some(3200),
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        };
        assert_eq!(iso_of(&metadata), Some(400));
        let no_ratings = RawMetadata {
            exif: rawler::exif::Exif {
                iso_speed: Some(3200),
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        };
        assert_eq!(iso_of(&no_ratings), Some(3200));
        let zero = RawMetadata {
            exif: rawler::exif::Exif {
                iso_speed: Some(0),
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        };
        assert_eq!(iso_of(&zero), None);
    }

    #[test]
    fn aperture_should_round_to_tenths() {
        let from_fnumber = RawMetadata {
            exif: rawler::exif::Exif {
                fnumber: Some(rawler::formats::tiff::Rational::new(28, 10)),
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        };
        assert_eq!(aperture_of(&from_fnumber), Some(2.8));
        let from_apex = RawMetadata {
            exif: rawler::exif::Exif {
                aperture_value: Some(rawler::formats::tiff::Rational::new(197, 100)),
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        };
        assert_eq!(aperture_of(&from_apex), Some(2.0));
    }

    #[test]
    fn focal_length_should_convert_rational_millimetres() {
        let at_105mm = RawMetadata {
            exif: rawler::exif::Exif {
                focal_length: Some(rawler::formats::tiff::Rational::new(105, 1)),
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        };
        assert_eq!(focal_mm_of(&at_105mm), Some(105.0));
        let at_zero = RawMetadata {
            exif: rawler::exif::Exif {
                focal_length: Some(rawler::formats::tiff::Rational::new(0, 1)),
                ..rawler::exif::Exif::default()
            },
            ..RawMetadata::default()
        };
        assert_eq!(focal_mm_of(&at_zero), None);
    }

    #[test]
    fn mtime_before_epoch_should_hash_as_zero_instead_of_failing() {
        assert_eq!(
            mtime_unix_nanos(SystemTime::UNIX_EPOCH - Duration::from_secs(5)),
            0
        );
    }
}
