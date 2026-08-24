//! Cullr dark palette (SPEC §6 theme); shared by every view.

use eframe::egui::Color32;

/// App background.
pub const BG: Color32 = Color32::from_rgb(0x16, 0x17, 0x1A);
/// Panel / bar fill.
pub const PANEL: Color32 = Color32::from_rgb(0x1E, 0x20, 0x23);
/// Primary text color.
pub const TEXT: Color32 = Color32::from_rgb(0xD7, 0xD9, 0xDC);
/// Secondary text (hints, subtitles, tile strips).
pub const MUTED: Color32 = Color32::from_rgb(0x8A, 0x8D, 0x92);
/// Accent for cursor, selection and primary actions.
pub const ACCENT: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
