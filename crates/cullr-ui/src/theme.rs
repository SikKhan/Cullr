//! Cullr dark palette (SPEC §6 theme); shared by every view.

use eframe::egui::Color32;

use cullr_core::Label;

/// App background.
pub const BG: Color32 = Color32::from_rgb(0x16, 0x17, 0x1A);
/// Lightbox backdrop: photo floats alone on near-pure black.
pub const VOID: Color32 = Color32::from_rgb(0x05, 0x05, 0x05);
/// Lightbox white backdrop: pure white for judging high-key frames and
/// highlight placement against a bright surround.
pub const PAPER: Color32 = Color32::WHITE;
/// Panel / bar fill.
pub const PANEL: Color32 = Color32::from_rgb(0x1E, 0x20, 0x23);
/// Primary text color.
pub const TEXT: Color32 = Color32::from_rgb(0xD7, 0xD9, 0xDC);
/// Secondary text (hints, subtitles, tile strips).
pub const MUTED: Color32 = Color32::from_rgb(0x8A, 0x8D, 0x92);
/// Accent for cursor, selection and primary actions.
pub const ACCENT: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
/// Label red (SPEC §6 culling palette).
pub const RED: Color32 = Color32::from_rgb(0xE5, 0x48, 0x4D);
/// Label yellow.
pub const YELLOW: Color32 = Color32::from_rgb(0xF5, 0xC5, 0x18);
/// Label green.
pub const GREEN: Color32 = Color32::from_rgb(0x46, 0xA7, 0x58);
/// Label blue.
pub const BLUE: Color32 = Color32::from_rgb(0x3E, 0x63, 0xDD);
/// Label purple.
pub const PURPLE: Color32 = Color32::from_rgb(0x9D, 0x5C, 0xE8);

/// Swatch color for a photo's persisted color label.
pub fn label_color(label: Label) -> Color32 {
    match label {
        Label::None => MUTED,
        Label::Red => RED,
        Label::Yellow => YELLOW,
        Label::Green => GREEN,
        Label::Blue => BLUE,
        Label::Purple => PURPLE,
    }
}
