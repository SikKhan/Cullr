//! Display-string formatting for EXIF exposure values.
//!
//! Ingest pulls primitive numbers out of the RAW header (in
//! [`crate::extract`], the only module that talks to rawler); this module
//! turns them into the strings the UI shows, e.g. `1/250 s` or
//! `2024-05-01 12:33:44`. Pure functions so they are testable without any
//! RAW fixture.

/// Formats a shutter speed from its EXIF rational for display.
///
/// Sub-second speeds render as a reciprocal (`1/250 s`), speeds of one
/// second or more as decimal seconds (`2.5 s`). Returns `None` for zero or
/// non-finite input.
pub fn format_shutter(numerator: u32, denominator: u32) -> Option<String> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    format_shutter_seconds(f64::from(numerator) / f64::from(denominator))
}

/// Formats a shutter speed given in seconds; see [`format_shutter`].
pub fn format_shutter_seconds(seconds: f64) -> Option<String> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    if seconds >= 1.0 {
        let rounded = (seconds * 10.0).round() / 10.0;
        let text = if (rounded - rounded.round()).abs() < f64::EPSILON {
            format!("{} s", rounded.round() as u64)
        } else {
            format!("{rounded} s")
        };
        return Some(text);
    }
    let reciprocal = (1.0 / seconds).round().max(1.0) as u64;
    Some(format!("1/{reciprocal} s"))
}

/// Formats an EXIF datetime (`2024:05:01 12:33:44`) into the display form
/// (`2024-05-01 12:33:44`).
///
/// Anything that does not look like an EXIF datetime is passed through
/// trimmed rather than dropped — showing something beats showing nothing.
pub fn format_capture_time(raw: &str) -> String {
    let raw = raw.trim();
    let bytes = raw.as_bytes();
    let is_exif_shape = bytes.len() >= 10
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b':'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b':'
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if is_exif_shape {
        // EXIF separates the date with colons; only those two become dashes.
        let mut out = raw.to_owned();
        out.replace_range(4..5, "-");
        out.replace_range(7..8, "-");
        out
    } else {
        raw.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutter_should_render_sub_second_speeds_as_reciprocals() {
        assert_eq!(format_shutter(1, 250), Some("1/250 s".to_owned()));
    }

    #[test]
    fn shutter_should_report_the_true_reciprocal_not_a_snapped_value() {
        assert_eq!(format_shutter(1, 253), Some("1/253 s".to_owned()));
    }

    #[test]
    fn shutter_should_render_one_second_plainly() {
        assert_eq!(format_shutter(1, 1), Some("1 s".to_owned()));
    }

    #[test]
    fn shutter_should_render_long_exposures_with_fraction() {
        assert_eq!(format_shutter(5, 2), Some("2.5 s".to_owned()));
        assert_eq!(format_shutter(30, 1), Some("30 s".to_owned()));
    }

    #[test]
    fn shutter_should_reject_zero_and_degenerate_rationals() {
        assert_eq!(format_shutter(0, 250), None);
        assert_eq!(format_shutter(1, 0), None);
        assert_eq!(format_shutter_seconds(0.0), None);
        assert_eq!(format_shutter_seconds(f64::NAN), None);
    }

    #[test]
    fn capture_time_should_swap_exif_colons_for_dashes() {
        assert_eq!(
            format_capture_time("2024:05:01 12:33:44"),
            "2024-05-01 12:33:44"
        );
    }

    #[test]
    fn capture_time_should_format_date_only_values() {
        assert_eq!(format_capture_time("2024:05:01"), "2024-05-01");
    }

    #[test]
    fn capture_time_should_pass_through_unknown_shapes_trimmed() {
        assert_eq!(format_capture_time("  unknown stamp "), "unknown stamp");
        assert_eq!(format_capture_time(""), "");
    }
}
