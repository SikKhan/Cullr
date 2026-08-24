//! Small shared widgets and input mappings for the culling workflow.
//!
//! Everything here is view-agnostic: the grid and the loupe both label
//! photos with digits `1..5` / `0` (SPEC §6), both show the palette as
//! clickable swatches, and both honor the persisted auto-advance toggle.

use eframe::egui;

use cullr_core::Db;
use cullr_core::Label;

use crate::theme;

/// kv row backing the auto-advance toggle so it survives restarts.
const AUTO_ADVANCE_KEY: &str = "auto_advance";

/// Digit keys in label order: `0` clears, `1..5` assign (SPEC §6).
const LABEL_KEYS: [(egui::Key, Label); 6] = [
    (egui::Key::Num0, Label::None),
    (egui::Key::Num1, Label::Red),
    (egui::Key::Num2, Label::Yellow),
    (egui::Key::Num3, Label::Green),
    (egui::Key::Num4, Label::Blue),
    (egui::Key::Num5, Label::Purple),
];

/// Swatch dot diameter.
pub const SWATCH_DIAMETER: f32 = 15.0;
/// Gap between dots, also used as the strip's side padding.
const SWATCH_GAP: f32 = 7.0;
/// Total width of the six-swatch strip, for callers reserving space.
pub const SWATCH_STRIP_WIDTH: f32 = 6.0 * SWATCH_DIAMETER + 7.0 * SWATCH_GAP;

/// The label assigned by a digit key press this frame, if any. When
/// several land in one frame only the first applies; mashing two digits
/// is not a workflow, it is a slip.
pub fn pressed_label_key(ctx: &egui::Context) -> Option<Label> {
    ctx.input(|input| {
        LABEL_KEYS
            .into_iter()
            .find(|(key, _)| input.key_pressed(*key))
            .map(|(_, label)| label)
    })
}

/// `true` on the frame Tab goes down; whichever view is on screen toggles
/// auto-advance with it (SPEC §6 keyboard map).
pub fn tab_pressed(ctx: &egui::Context) -> bool {
    ctx.input(|input| input.key_pressed(egui::Key::Tab))
}

/// Loads the persisted auto-advance setting, defaulting to on: the tool's
/// whole purpose is rapid sequential culling, and one Tab turns it off.
/// Read failures degrade to the default rather than blocking startup.
pub fn load_auto_advance(db: &Db) -> bool {
    match db.kv_get(AUTO_ADVANCE_KEY) {
        Ok(Some(value)) => value != "off",
        Ok(None) => true,
        Err(error) => {
            tracing::warn!(%error, "cannot read auto-advance setting");
            true
        }
    }
}

/// Persists the auto-advance toggle; failures are logged and otherwise
/// ignored because the in-memory state already drives the UI.
pub fn store_auto_advance(db: &Db, enabled: bool) {
    let value = if enabled { "on" } else { "off" };
    if let Err(error) = db.kv_set(AUTO_ADVANCE_KEY, value) {
        tracing::warn!(%error, "cannot persist auto-advance setting");
    }
}

/// Draws the label palette as digit-tagged dots and reports a clicked
/// label. The active label gets a bright fill plus an outer ring; others
/// stay dimmed but legible so the whole mapping remains readable.
pub fn label_swatches(ui: &mut egui::Ui, current: Label) -> Option<Label> {
    let mut picked = None;
    ui.spacing_mut().item_spacing.x = SWATCH_GAP;
    ui.horizontal(|ui| {
        for (digit, (_, label)) in LABEL_KEYS.iter().enumerate() {
            let active = *label == current;
            let fill = if active || *label == Label::None {
                theme::label_color(*label)
            } else {
                theme::label_color(*label).gamma_multiply(0.38)
            };
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(SWATCH_DIAMETER, SWATCH_DIAMETER),
                egui::Sense::click(),
            );
            let painter = ui.painter();
            let center = rect.center();
            let radius = SWATCH_DIAMETER / 2.0;
            if *label == Label::None {
                painter.circle_stroke(center, radius - 0.75, egui::Stroke::new(1.25, theme::MUTED));
            } else {
                painter.circle_filled(center, radius - 0.75, fill);
            }
            if active {
                painter.circle_stroke(
                    center,
                    radius,
                    egui::Stroke::new(1.5, theme::TEXT.gamma_multiply(0.85)),
                );
            }
            // Digit hint over the dot: dark on bright fills, light text
            // elsewhere, so every shortcut stays legible at a glance.
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                digit.to_string(),
                egui::FontId::proportional(9.0),
                if active && *label != Label::None {
                    theme::BG
                } else {
                    theme::TEXT
                },
            );
            if response.clicked() {
                picked = Some(*label);
            }
            response.on_hover_text(format!("{digit} · {}", label_name(*label)));
        }
    });
    picked
}

/// Human name for a label, used in tooltips.
fn label_name(label: Label) -> &'static str {
    match label {
        Label::None => "clear",
        Label::Red => "red",
        Label::Yellow => "yellow",
        Label::Green => "green",
        Label::Blue => "blue",
        Label::Purple => "purple",
    }
}

#[cfg(test)]
mod tests {
    // Test setup asserts hard failures; a broken fixture aborts the test.
    #![expect(clippy::expect_used)]

    use super::*;

    #[test]
    fn swatch_strip_width_should_cover_dots_gaps_and_padding() {
        assert_eq!(SWATCH_STRIP_WIDTH, 6.0 * 15.0 + 7.0 * 7.0);
    }

    #[test]
    fn load_auto_advance_should_default_on_and_honor_off() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Db::open(&dir.path().join("index.db")).expect("open db");

        assert!(load_auto_advance(&db), "unset key defaults to on");

        store_auto_advance(&db, false);

        assert!(!load_auto_advance(&db), "persisted off must round-trip");

        store_auto_advance(&db, true);

        assert!(load_auto_advance(&db));
    }
}
