//! Modal overlays: the `?` shortcut help and the About dialog.
//!
//! Both share one mechanism: [`Modals`] holds the open flags, reads the
//! raw input for open/close intents *before* any view runs (SPEC §6),
//! and paints last, on top. While any modal is up the app suspends view
//! input handling, so digits never label photos underneath the dimmed
//! backdrop; the backdrop itself swallows clicks (topmost layer wins)
//! and closes the dialog when pressed.
//!
//! The shortcut tables are plain data so the overlay renders straight
//! from SPEC §6 without duplicating logic elsewhere.

use eframe::egui;

use crate::theme;

/// Width of the centered dialog panels; narrow enough for small windows.
const PANEL_WIDTH: f32 = 560.0;
/// Corner radius shared by both dialogs.
const PANEL_RADIUS: f32 = 10.0;
/// Translucent wash dimming the UI beneath a modal.
const BACKDROP_DIM: egui::Color32 = egui::Color32::from_black_alpha(150);

/// Keyboard shortcuts shown in the overlay, mirroring SPEC §6 exactly.
const KEYBOARD_ROWS: [(&str, &str); 11] = [
    ("← → ↑ ↓", "move the cursor"),
    ("1 … 5", "label red / yellow / green / blue / purple"),
    ("0", "clear the label"),
    ("[ · ]", "rotate left / right (persisted)"),
    ("Tab", "toggle auto-advance after labeling"),
    ("Enter · Esc", "loupe ⇄ contact sheet"),
    ("Space", "loupe: fit ↔ 100% · sheet: open the loupe"),
    ("Ctrl+A · Shift+A", "select all / none"),
    ("Ctrl+E", "export originals (selection · filtered view)"),
    ("F", "cycle filter preset: All → Labeled → Unlabeled"),
    ("?", "this overlay"),
];

/// Mouse behaviors shown alongside [`KEYBOARD_ROWS`].
const MOUSE_ROWS: [(&str, &str); 8] = [
    ("Click", "focus + select a tile"),
    ("Ctrl-click", "toggle a tile in the selection"),
    ("Shift-click", "select the range from the anchor"),
    ("Drag", "rubber-band selection"),
    ("Double-click", "open the loupe"),
    ("Wheel", "scroll the sheet"),
    ("Ctrl+Wheel", "cell size"),
    ("Loupe wheel / drag", "zoom toward cursor / pan"),
];

/// Open-state of the app's modal dialogs; owned by the app shell.
///
/// Views receive [`Self::any`] as their `suspended` flag and stay quiet
/// while a dialog covers them.
#[derive(Default)]
pub struct Modals {
    help: bool,
    about: bool,
}

impl Modals {
    /// Whether a modal currently covers the screen and views must not
    /// act on input.
    pub fn any(&self) -> bool {
        self.help || self.about
    }

    /// Opens the About dialog (Home corner button, help-overlay footer).
    pub fn open_about(&mut self) {
        self.about = true;
    }

    /// Reads raw input for open/close intents; runs before the views so
    /// a closing keypress cannot fall through into the freshly un-
    /// suspended screen. `?` toggles the overlay anywhere; Esc closes
    /// whatever is open.
    pub fn pump_input(&mut self, ctx: &egui::Context) {
        let question = ctx.input(question_pressed);
        let escape = ctx.input(|input| input.key_pressed(egui::Key::Escape));
        if question {
            self.help = !self.help;
        }
        if escape && self.any() {
            // Swallow the Escape that closed us so the grid underneath
            // does not read it as "leave the folder" the same frame.
            ctx.input_mut(|input| {
                input.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            });
            self.help = false;
            self.about = false;
        }
    }

    /// Paints every open modal on top of everything drawn this frame.
    pub fn draw(&mut self, ctx: &egui::Context) {
        if self.help {
            let mut about = false;
            let dismissed = draw_dialog(ctx, egui::Id::new("cullr_help"), |ui| {
                draw_help_panel(ui, &mut about);
                false
            });
            if about {
                self.about = true;
            }
            if dismissed {
                self.help = false;
            }
        }
        if self.about {
            let dismissed = draw_dialog(ctx, egui::Id::new("cullr_about"), draw_about_panel);
            if dismissed {
                self.about = false;
            }
        }
    }
}

/// `true` when a `?` keystroke arrived this frame: either an explicit
/// text event carrying the character or Shift+Slash on layouts that do
/// not emit text for it. Both may fire together; callers treat this as
/// one toggle.
pub fn question_pressed(input: &egui::InputState) -> bool {
    input.events.iter().any(is_question_event)
}

/// Recognizes the `?` keystroke among raw input events. Pure so the
/// recognition rule is unit-testable across keyboard layouts.
fn is_question_event(event: &egui::Event) -> bool {
    match event {
        egui::Event::Text(text) => text == "?",
        // Shift+Slash covers layouts that emit no text event; some map
        // the glyph to its own named key instead.
        egui::Event::Key {
            key: egui::Key::Slash,
            pressed: true,
            modifiers,
            ..
        } => modifiers.shift,
        egui::Event::Key {
            key: egui::Key::Questionmark,
            pressed: true,
            ..
        } => true,
        _ => false,
    }
}

/// Shared modal chrome: full-screen dimmed backdrop plus a centered
/// rounded panel built by `content` (returning `true` for an explicit
/// close request, e.g. a Close button). Returns `true` when the dialog
/// should dismiss — backdrop click or the panel's own close.
fn draw_dialog(
    ctx: &egui::Context,
    id: egui::Id,
    content: impl FnOnce(&mut egui::Ui) -> bool,
) -> bool {
    let screen = ctx.viewport_rect();
    let mut dismissed = false;
    egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.painter().rect_filled(screen, 0.0, BACKDROP_DIM);
            dismissed = ui.allocate_rect(screen, egui::Sense::click()).clicked();
        });
    let mut requested_close = false;
    egui::Area::new(id.with("panel"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::PANEL)
                .corner_radius(PANEL_RADIUS)
                .stroke(egui::Stroke::new(1.0, theme::MUTED.gamma_multiply(0.35)))
                .inner_margin(egui::Margin::same(18))
                .show(ui, |ui| {
                    ui.set_min_width(PANEL_WIDTH - 36.0);
                    requested_close = content(ui);
                });
        });
    dismissed || requested_close
}

/// Help panel body: SPEC §6 keyboard table, mouse behaviors and a footer
/// with the About entry point. `about` flips when the footer button is
/// used, stacking the About dialog on top. Never self-dismisses; the
/// backdrop and Esc handle that.
fn draw_help_panel(ui: &mut egui::Ui, about: &mut bool) -> bool {
    ui.label(
        egui::RichText::new("Keyboard & Mouse")
            .heading()
            .strong()
            .color(theme::TEXT),
    );
    ui.add_space(10.0);
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .max_height(ui.max_rect().height() * 3.0)
        .show(ui, |ui| {
            section_title(ui, "SHORTCUTS");
            for (keys, action) in KEYBOARD_ROWS {
                shortcut_row(ui, keys, action);
            }
            ui.add_space(8.0);
            section_title(ui, "MOUSE");
            for (gesture, action) in MOUSE_ROWS {
                shortcut_row(ui, gesture, action);
            }
        });
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Press ?, Esc — or click outside — to close")
                .small()
                .color(theme::MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("About Cullr").clicked() {
                *about = true;
            }
        });
    });
    false
}

/// About panel body (SPEC §10 T14 / §12): identity, license, the rawler
/// notice and the major-dependency list. Returns `true` on Close.
fn draw_about_panel(ui: &mut egui::Ui) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Cullr")
                .size(24.0)
                .strong()
                .color(theme::TEXT),
        );
        ui.label(egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).color(theme::MUTED));
    });
    ui.add_space(4.0);
    ui.label(egui::RichText::new("Instant culling for RAW folders").color(theme::MUTED));
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(env!("CARGO_PKG_LICENSE"))
            .strong()
            .color(theme::TEXT),
    );
    ui.add_space(12.0);
    section_title(ui, "THIRD-PARTY NOTICES");
    notice_row(
        ui,
        "rawler",
        "© dnglab contributors — LGPL-2.1-only",
        "RAW decoding. Linked dynamically per LGPL §6: Cullr is open \
         source under GPL-3.0-or-later, so the combined work satisfies \
         the source-availability provision.",
    );
    ui.add_space(8.0);
    section_title(ui, "MAJOR DEPENDENCIES");
    for (name, license) in DEPENDENCIES {
        dependency_row(ui, name, license);
    }
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        if ui.button("Close").clicked() {
            close = true;
        }
        ui.label(
            egui::RichText::new("Esc or click outside also closes")
                .small()
                .color(theme::MUTED),
        );
    });
    close
}

/// Verified license expressions of the major dependencies (read from the
/// vendored crate manifests, SPEC §2 stack).
const DEPENDENCIES: [(&str, &str); 7] = [
    ("eframe / egui", "MIT OR Apache-2.0"),
    ("rusqlite", "MIT"),
    ("image", "MIT OR Apache-2.0"),
    ("rayon", "MIT OR Apache-2.0"),
    ("walkdir", "Unlicense OR MIT"),
    ("rfd", "MIT"),
    ("crossbeam-channel", "MIT OR Apache-2.0"),
];

/// Small uppercase section label.
fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .small()
            .strong()
            .color(theme::MUTED),
    );
    ui.add_space(2.0);
}

/// One `keys — action` row: the key badge sits in a fixed-width column so
/// descriptions align into a readable table.
fn shortcut_row(ui: &mut egui::Ui, keys: &str, action: &str) {
    ui.horizontal(|ui| {
        let column = 130.0;
        let (badge_rect, _) =
            ui.allocate_exact_size(egui::vec2(column, 20.0), egui::Sense::hover());
        ui.painter().text(
            badge_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            keys,
            egui::FontId::proportional(12.5),
            theme::ACCENT,
        );
        ui.label(egui::RichText::new(action).color(theme::TEXT));
    });
}

/// One third-party notice block: name + license line, then the prose.
fn notice_row(ui: &mut egui::Ui, name: &str, license: &str, note: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(name).strong().color(theme::TEXT));
        ui.label(egui::RichText::new(license).color(theme::ACCENT));
    });
    ui.label(egui::RichText::new(note).small().color(theme::MUTED));
}

/// Compact one-line dependency attribution.
fn dependency_row(ui: &mut egui::Ui, name: &str, license: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(name).color(theme::TEXT));
        ui.label(egui::RichText::new(format!("— {license}")).color(theme::MUTED));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_events_should_match_text_shifted_slash_and_named_key() {
        assert!(is_question_event(&egui::Event::Text("?".to_owned())));
        assert!(is_question_event(&egui::Event::Key {
            key: egui::Key::Slash,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        }));
        assert!(is_question_event(&egui::Event::Key {
            key: egui::Key::Questionmark,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }));
    }

    #[test]
    fn question_events_should_reject_plain_slash_and_other_text() {
        assert!(!is_question_event(&egui::Event::Text("!".to_owned())));
        assert!(!is_question_event(&egui::Event::Key {
            key: egui::Key::Slash,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }));
        // Key *releases* never toggle anything.
        assert!(!is_question_event(&egui::Event::Key {
            key: egui::Key::Questionmark,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }));
    }

    #[test]
    fn shortcut_tables_should_stay_in_sync_with_the_spec_shape() {
        // SPEC §6 lists eleven keyboard rows; the mouse table mirrors the
        // selection + zoom behaviors documented there.
        assert_eq!(KEYBOARD_ROWS.len(), 11);
        assert_eq!(MOUSE_ROWS.len(), 8);
        assert!(
            KEYBOARD_ROWS
                .iter()
                .chain(MOUSE_ROWS.iter())
                .all(|(k, a)| !k.is_empty() && !a.is_empty()),
            "every row needs both a trigger and an action"
        );
    }
}
