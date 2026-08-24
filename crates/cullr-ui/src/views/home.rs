//! Home view: folder picker entry point and recently opened folders.

use std::path::Path;

use eframe::egui;

use cullr_core::Db;

use super::Action;
use crate::theme;

/// How many recent folders Home offers.
const RECENT_LIMIT: usize = 10;

/// State of the Home screen; recents are read from the index per frame, so
/// no cached state is needed yet.
#[derive(Default)]
pub struct HomeView;

impl HomeView {
    /// Draws the screen and reports the user's action, if any.
    pub fn ui(&mut self, ui: &mut egui::Ui, db: &Db) -> Option<Action> {
        let mut action = None;
        draw_about_button(ui, &mut action);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(72.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("Cullr")
                        .size(34.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Instant culling for RAW folders")
                        .size(14.0)
                        .color(theme::MUTED),
                );
                ui.add_space(32.0);

                let open = egui::Button::new(
                    egui::RichText::new("Open Folder…")
                        .heading()
                        .color(theme::BG),
                )
                .min_size(egui::vec2(190.0, 40.0))
                .fill(theme::ACCENT);
                if ui.add(open).clicked() {
                    action = Some(Action::PickFolder);
                }

                draw_resume(ui, db, &mut action);
                draw_recents(ui, db, &mut action);
            });
        });

        action
    }
}

/// Quiet `About` button pinned to the top-right corner of the screen
/// (SPEC §10 T14). An anchored Area keeps it clear of the centered flow.
fn draw_about_button(ui: &mut egui::Ui, action: &mut Option<Action>) {
    egui::Area::new(egui::Id::new("cullr_home_about"))
        .anchor(egui::Align2::RIGHT_TOP, [-10.0, 8.0])
        .show(ui.ctx(), |ui| {
            if ui
                .button(egui::RichText::new("ⓘ  About").small().color(theme::MUTED))
                .clicked()
            {
                *action = Some(Action::ShowAbout);
            }
        });
}

/// Prominent "Resume — <folder>" affordance for the last opened folder
/// (SPEC §10 T14): the top recents entry is by definition the most
/// recently opened root (`roots.last_opened`), so resuming means jumping
/// straight into its grid. Deliberately never auto-opened.
fn draw_resume(ui: &mut egui::Ui, db: &Db, action: &mut Option<Action>) {
    let Some(last) = load_recents(db).into_iter().next() else {
        return;
    };
    ui.add_space(48.0);
    let resume = egui::Button::new(
        egui::RichText::new(format!("Resume — {}", display_name(&last)))
            .heading()
            .color(theme::ACCENT),
    )
    .min_size(egui::vec2(190.0, 36.0))
    .fill(theme::PANEL)
    .stroke(egui::Stroke::new(1.25, theme::ACCENT.gamma_multiply(0.7)));
    let response = ui.add(resume);
    if response.clicked() {
        *action = Some(Action::OpenFolder(last.clone()));
    }
    response.on_hover_text(last.display().to_string());
}

/// Lists recently opened folders below the picker button.
fn draw_recents(ui: &mut egui::Ui, db: &Db, action: &mut Option<Action>) {
    let recents = load_recents(db);
    if recents.is_empty() {
        return;
    }

    ui.add_space(56.0);
    ui.label(
        egui::RichText::new("RECENT FOLDERS")
            .small()
            .strong()
            .color(theme::MUTED),
    );
    ui.add_space(10.0);

    for path in recents {
        let response = ui.button(display_name(&path));
        if response.clicked() {
            *action = Some(Action::OpenFolder(path));
        } else {
            response.on_hover_text(path.display().to_string());
        }
    }
}

fn load_recents(db: &Db) -> Vec<std::path::PathBuf> {
    match db.recent_roots(RECENT_LIMIT) {
        Ok(paths) => paths,
        Err(error) => {
            tracing::warn!(%error, "cannot load recent folders");
            Vec::new()
        }
    }
}

/// Short label for a folder button; falls back to the full path for roots
/// without a final component (e.g. `/`).
fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}
