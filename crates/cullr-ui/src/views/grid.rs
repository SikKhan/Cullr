//! Grid view: contact sheet of placeholder tiles for one folder.
//!
//! Real virtualization, aspect-fit cells and textures arrive in T7/T8; this
//! module already owns the screen chrome (top bar with folder + count) and
//! the Home ⇄ Grid navigation contract.

use std::path::PathBuf;

use eframe::egui;

use cullr_core::PhotoEntry;

use super::Action;
use crate::theme;

/// Placeholder tile size until real aspect-fit cells land in T7.
const CELL_SIZE: egui::Vec2 = egui::Vec2::new(176.0, 148.0);
/// Filename strip height inside a tile.
const STRIP_HEIGHT: f32 = 22.0;
/// Maximum characters of a filename shown in a tile before truncation.
const NAME_MAX_CHARS: usize = 26;

/// State of the Grid screen for the folder being browsed.
pub struct GridView {
    root: PathBuf,
    entries: Vec<PhotoEntry>,
    loading: bool,
    error: Option<String>,
}

impl GridView {
    /// Creates the view in its scanning state; contents arrive later via
    /// [`Self::apply_scan`] (SPEC §5.1 placeholders-first).
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            entries: Vec::new(),
            loading: true,
            error: None,
        }
    }

    /// Fills or fails the grid when the background scan reports back.
    pub fn apply_scan(&mut self, result: Result<Vec<PhotoEntry>, String>) {
        self.loading = false;
        match result {
            Ok(entries) => self.entries = entries,
            Err(message) => self.error = Some(message),
        }
    }

    /// Draws the screen and reports the user's action, if any.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        // Keyboard-first navigation: Esc leaves the grid from anywhere.
        if ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
            action = Some(Action::BackToHome);
        }

        egui::Panel::top(egui::Id::new("cullr_grid_top_bar")).show(ui, |ui| {
            self.top_bar(ui, &mut action);
        });
        egui::CentralPanel::default().show(ui, |ui| self.body(ui));

        action
    }

    /// Persistent top bar: back affordance, folder name, photo count.
    fn top_bar(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            if ui.button("‹ Back").clicked() {
                *action = Some(Action::BackToHome);
            }
            ui.separator();
            ui.label(
                egui::RichText::new(root_label(&self.root))
                    .strong()
                    .color(theme::TEXT),
            );

            let count = if self.loading {
                "Scanning…".to_owned()
            } else {
                format!(
                    "{} photo{}",
                    self.entries.len(),
                    if self.entries.len() == 1 { "" } else { "s" }
                )
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(count).color(theme::MUTED));
            });
        });
    }

    /// Body: scanning notice, failure notice, empty notice or tile sheet.
    fn body(&mut self, ui: &mut egui::Ui) {
        if self.loading {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 2.0 - 40.0);
                ui.add(egui::Spinner::new().size(28.0));
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(format!("Scanning {}…", root_label(&self.root)))
                        .color(theme::MUTED),
                );
            });
            return;
        }

        if let Some(error) = &self.error {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 2.0 - 60.0);
                ui.heading("Could not open folder");
                ui.add_space(8.0);
                ui.label(egui::RichText::new(error).color(theme::MUTED));
                ui.add_space(16.0);
                ui.label(
                    egui::RichText::new("Press Esc to go back")
                        .small()
                        .color(theme::MUTED),
                );
            });
            return;
        }

        if self.entries.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 2.0 - 40.0);
                ui.label(
                    egui::RichText::new("No supported RAW files in this folder")
                        .color(theme::MUTED),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Press Esc to go back")
                        .small()
                        .color(theme::MUTED),
                );
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                for entry in &self.entries {
                    draw_cell(ui, entry);
                }
            });
        });
    }
}

/// One placeholder tile: panel-colored rectangle with a filename strip and
/// the full relative path as tooltip.
fn draw_cell(ui: &mut egui::Ui, entry: &PhotoEntry) {
    let (rect, response) = ui.allocate_exact_size(CELL_SIZE, egui::Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, 6.0, theme::PANEL);

    let strip = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.bottom() - STRIP_HEIGHT),
        rect.right_bottom(),
    );
    painter.rect_filled(strip, 6.0, theme::BG);
    painter.text(
        strip.center(),
        egui::Align2::CENTER_CENTER,
        truncated(file_name(entry)),
        egui::FontId::proportional(11.0),
        theme::MUTED,
    );

    response.on_hover_text(entry.rel_path.display().to_string());
}

fn file_name(entry: &PhotoEntry) -> std::borrow::Cow<'_, str> {
    entry.rel_path.file_name().map_or_else(
        || entry.rel_path.to_string_lossy(),
        |name| name.to_string_lossy(),
    )
}

/// Keeps the tail of the name so extensions stay readable when truncated.
fn truncated(name: std::borrow::Cow<'_, str>) -> String {
    let count = name.chars().count();
    if count <= NAME_MAX_CHARS {
        return name.into_owned();
    }
    let tail: String = name.chars().skip(count - (NAME_MAX_CHARS - 1)).collect();
    format!("…{tail}")
}

/// Folder name shown in bar and notices; falls back to the full path for
/// roots without a final component (e.g. `/`).
fn root_label(root: &std::path::Path) -> String {
    root.file_name().map_or_else(
        || root.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}
