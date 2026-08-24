//! Grid view: virtualized contact sheet (SPEC §6).
//!
//! Layout is computed once per frame: fixed-width cells in as many columns
//! as the viewport fits, drawn through [`egui::ScrollArea::show_rows`] so
//! only the visible rows (+ egui's built-in margin row) allocate anything.
//! Cell images are aspect-fit thumbnails streamed in by the texture cache;
//! pending rows spin, failed rows degrade to error tiles (SPEC §7).
//!
//! The sheet also feeds the ingest engine's visible-window priority: every
//! frame's visible-but-pending ids are reported via [`Self::take_priority_ping`]
//! whenever the set changes (SPEC §5.2).

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;

use cullr_core::PhotoEntry;
use cullr_core::PhotoId;
use cullr_core::PhotoStatus;

use super::Action;
use super::loupe;
use crate::tex::{TexKey, TextureState, Textures};
use crate::theme;

/// Cell width; height derives from the default photo aspect.
const CELL_WIDTH: f32 = 232.0;
/// Inner padding between cell border and image area.
const CELL_PADDING: f32 = 8.0;
/// Filename strip height inside a tile.
const STRIP_HEIGHT: f32 = 22.0;
/// Gap between cells, matching egui's default item spacing we override.
const GAP: f32 = 8.0;
/// Fixed image-area height: uniform rows are what keeps `show_rows`
/// virtualization exact; other aspects letterbox inside this box.
const IMAGE_HEIGHT: f32 = CELL_WIDTH / cullr_core::DEFAULT_ASPECT;
/// Total cell height including the filename strip.
const CELL_HEIGHT: f32 = IMAGE_HEIGHT + STRIP_HEIGHT;
/// Maximum characters of a filename shown in a tile before truncation.
const NAME_MAX_CHARS: usize = 26;
/// Spinner diameter inside cells.
const SPINNER_SIZE: f32 = 22.0;
/// Rows decoded ahead of the viewport above and below it (SPEC §5.3
/// neighbor prefetch) so steady scrolling finds tiles already resident.
const PREFETCH_ROWS: usize = 2;

/// State of the Grid screen for the folder being browsed.
pub struct GridView {
    root: PathBuf,
    entries: Vec<PhotoEntry>,
    /// Row index by photo id so per-photo ingest events are O(1) updates.
    index: HashMap<PhotoId, usize>,
    loading: bool,
    error: Option<String>,
    ingest_total: usize,
    ingest_done: usize,
    /// Visible pending ids already sent as a priority ping; dedups repeats.
    last_ping: Vec<PhotoId>,
    /// Ping waiting to be collected by the app, if any.
    ping: Option<Vec<PhotoId>>,
    /// Full-screen preview state while the loupe is open; `None` shows
    /// the contact sheet (SPEC §6: Grid ⇄ Loupe).
    loupe: Option<loupe::LoupeView>,
}

impl GridView {
    /// Creates the view in its scanning state; contents arrive later via
    /// [`Self::apply_scan`] (SPEC §5.1 placeholders-first).
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            entries: Vec::new(),
            index: HashMap::new(),
            loading: true,
            error: None,
            ingest_total: 0,
            ingest_done: 0,
            last_ping: Vec::new(),
            ping: None,
            loupe: None,
        }
    }

    /// Fills or fails the grid when the background scan reports back.
    pub fn apply_scan(&mut self, result: Result<Vec<PhotoEntry>, String>) {
        self.loading = false;
        match result {
            Ok(entries) => {
                self.index = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| (entry.id, index))
                    .collect();
                self.entries = entries;
            }
            Err(message) => self.error = Some(message),
        }
    }

    /// Announces a running ingest batch of `total` photos.
    pub fn begin_ingest(&mut self, total: usize) {
        self.ingest_total = total;
        self.ingest_done = 0;
    }

    /// Applies one finished extraction; events for photos that are not on
    /// this grid (stale generation) fall through as no-ops. `fresh` carries
    /// the re-read index row (thumb path, pixel size, error message) so the
    /// cell can graduate from placeholder to image; when unavailable the
    /// bare status is patched onto the existing row instead.
    pub fn apply_ingest_result(
        &mut self,
        id: PhotoId,
        status: PhotoStatus,
        fresh: Option<PhotoEntry>,
    ) {
        if self.ingest_done < self.ingest_total {
            self.ingest_done += 1;
        }
        match fresh {
            Some(entry) => {
                if let Some(&row) = self.index.get(&entry.id) {
                    self.entries[row] = entry;
                }
            }
            None => {
                if let Some(&row) = self.index.get(&id) {
                    self.entries[row].status = status;
                }
            }
        }
    }

    /// Marks the active batch complete so progress UI retires.
    pub fn finish_ingest(&mut self) {
        self.ingest_done = self.ingest_total;
    }

    /// `true` while a batch is still producing tiles.
    pub fn is_ingesting(&self) -> bool {
        self.ingest_done < self.ingest_total
    }

    /// Hands the app the latest changed set of visible pending photos for
    /// queue reordering, or `None` when nothing changed since last frame.
    pub fn take_priority_ping(&mut self) -> Option<Vec<PhotoId>> {
        self.ping.take()
    }

    /// Draws the screen and reports the user's action, if any.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        db: &cullr_core::Db,
        textures: &mut Textures,
    ) -> Option<Action> {
        let mut action = None;
        // Keyboard-first navigation: while the loupe is up it owns Esc
        // (back to the sheet); otherwise Esc leaves the grid entirely.
        if self.loupe.is_none() && ui.ctx().input(|input| input.key_pressed(egui::Key::Escape)) {
            action = Some(Action::BackToHome);
        }

        egui::Panel::top(egui::Id::new("cullr_grid_top_bar")).show(ui, |ui| {
            self.top_bar(ui, &mut action);
        });
        egui::CentralPanel::default().show(ui, |ui| self.body(ui, db, textures));

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
            } else if self.is_ingesting() {
                format!(
                    "{} photo{} · extracting {} / {}",
                    self.entries.len(),
                    if self.entries.len() == 1 { "" } else { "s" },
                    self.ingest_done,
                    self.ingest_total
                )
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

    /// Body: loupe when open, otherwise scanning notice, failure notice,
    /// empty notice or tile sheet.
    fn body(&mut self, ui: &mut egui::Ui, db: &cullr_core::Db, textures: &mut Textures) {
        if let Some(active) = self.loupe.as_mut() {
            // The loupe borrows the sheet's entries in place so ingest
            // events keep flowing into the same source of truth; its
            // index stays valid because rows never leave mid-session.
            let outcome = active.ui(ui, db, &self.entries, textures);
            if outcome == loupe::Outcome::Close {
                self.loupe = None;
            }
            return;
        }

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

        self.draw_sheet(ui, textures);
    }

    /// Virtualized tile sheet: only rows intersecting the viewport (plus a
    /// margin row, added by `show_rows`) allocate widgets or textures.
    ///
    /// The visible id set feeds the texture manager's focus (scroll-driven
    /// decode cancellation) and a band of neighbouring rows is prefetched,
    /// both after drawing so they see the settled viewport.
    fn draw_sheet(&mut self, ui: &mut egui::Ui, textures: &mut Textures) {
        ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
        let columns = columns_for_width(ui.available_width()).max(1);
        let total_rows = self.entries.len().div_ceil(columns);
        let entries = &self.entries;
        let mut visible_keys: Vec<TexKey> = Vec::new();
        let mut visible_pending: Vec<PhotoId> = Vec::new();
        let mut clicked: Option<usize> = None;
        let mut rows_shown = 0..0;

        egui::ScrollArea::vertical().auto_shrink(false).show_rows(
            ui,
            CELL_HEIGHT,
            total_rows,
            |ui, rows| {
                rows_shown = rows.clone();
                for row in rows {
                    ui.horizontal(|ui| {
                        for column in 0..columns {
                            let position = row * columns + column;
                            let Some(entry) = entries.get(position) else {
                                break;
                            };
                            visible_keys.push(TexKey::thumb(entry.id));
                            if draw_cell(ui, textures, entry, &mut visible_pending) {
                                clicked = Some(position);
                            }
                        }
                    });
                }
            },
        );

        // Row-major traversal is already sorted; `focus` relies on that.
        visible_keys.sort_unstable();
        textures.focus(&visible_keys);
        self.prefetch_band(textures, rows_shown, columns, total_rows);

        // Clicking a tile opens the loupe on it (SPEC §6 Grid ⇄ Loupe).
        if let Some(position) = clicked {
            self.loupe = Some(loupe::LoupeView::at(position));
        }

        visible_pending.sort_unstable();
        if visible_pending != self.last_ping && !visible_pending.is_empty() {
            self.last_ping = visible_pending.clone();
            self.ping = Some(visible_pending);
        }
    }

    /// Offers the rows just outside the viewport (± [`PREFETCH_ROWS`]) to
    /// the texture cache as low-priority prefetches; only ingested tiles
    /// with a cached thumb are offered, and the cache may decline them all
    /// when its queues are saturated.
    fn prefetch_band(
        &self,
        textures: &mut Textures,
        rows: std::ops::Range<usize>,
        columns: usize,
        total_rows: usize,
    ) {
        if rows.is_empty() {
            return;
        }
        let first = rows.start.saturating_sub(PREFETCH_ROWS);
        let last = (rows.end + PREFETCH_ROWS).min(total_rows);
        let entries = &self.entries;
        let band = (first..last)
            .filter(|row| !rows.contains(row))
            .flat_map(|row| (0..columns).map(move |column| row * columns + column))
            .filter_map(|cell| entries.get(cell))
            .filter(|entry| entry.status == PhotoStatus::Ok)
            .map(|entry| (TexKey::thumb(entry.id), entry.thumb_path.as_deref()));
        textures.prefetch(band);
    }
}

/// One grid cell: panel rectangle, aspect-fit thumbnail (or spinner /
/// error fallback) and the filename strip with its color-label dot.
/// Reports whether the cell was clicked (open in loupe).
fn draw_cell(
    ui: &mut egui::Ui,
    textures: &mut Textures,
    entry: &PhotoEntry,
    visible_pending: &mut Vec<PhotoId>,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(CELL_WIDTH, CELL_HEIGHT), egui::Sense::click());
    ui.painter().rect_filled(rect, 6.0, theme::PANEL);

    let image_area = egui::Rect::from_min_max(
        egui::pos2(rect.left() + CELL_PADDING, rect.top() + CELL_PADDING),
        egui::pos2(rect.right() - CELL_PADDING, rect.bottom() - STRIP_HEIGHT),
    );
    draw_image_area(ui, textures, entry, image_area, visible_pending);
    draw_strip(ui.painter(), rect, entry);

    let tooltip = match &entry.err_msg {
        Some(message) => format!("{}\n⚠ {message}", entry.rel_path.display()),
        None => entry.rel_path.display().to_string(),
    };
    // `on_hover_text` consumes the response, so read the click first.
    let clicked = response.clicked();
    response.on_hover_text(tooltip);
    clicked
}

/// Image region of a cell: thumbnail when ready, spinner while pending or
/// decoding, warning glyph when the asset cannot be shown (SPEC §7).
fn draw_image_area(
    ui: &mut egui::Ui,
    textures: &mut Textures,
    entry: &PhotoEntry,
    area: egui::Rect,
    visible_pending: &mut Vec<PhotoId>,
) {
    let painter = ui.painter();
    match entry.status {
        PhotoStatus::Ok => {
            match textures.handle(TexKey::thumb(entry.id), entry.thumb_path.as_deref()) {
                TextureState::Ready(handle) => {
                    let fitted = fit_rect(area, entry.display_aspect());
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    painter.image(handle.id(), fitted, uv, egui::Color32::WHITE);
                }
                TextureState::Broken => {
                    painter.text(
                        area.center(),
                        egui::Align2::CENTER_CENTER,
                        "⚠",
                        egui::FontId::proportional(30.0),
                        theme::MUTED,
                    );
                }
                TextureState::Loading => {
                    let spinner = egui::Spinner::new().size(SPINNER_SIZE);
                    ui.put(
                        egui::Rect::from_center_size(area.center(), egui::vec2(24.0, 24.0)),
                        spinner,
                    );
                }
            }
        }
        PhotoStatus::Pending => {
            visible_pending.push(entry.id);
            let spinner = egui::Spinner::new().size(SPINNER_SIZE);
            ui.put(
                egui::Rect::from_center_size(area.center(), egui::vec2(24.0, 24.0)),
                spinner,
            );
        }
        PhotoStatus::Error => {
            painter.text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                "⚠",
                egui::FontId::proportional(30.0),
                theme::MUTED,
            );
        }
        // Missing rows are filtered out of navigation by the index (SPEC §7).
        PhotoStatus::Missing => {}
    }
}

/// Bottom strip: color-label dot plus truncated filename that dims while
/// the photo is still in flight and flags extraction failures.
fn draw_strip(painter: &egui::Painter, cell: egui::Rect, entry: &PhotoEntry) {
    let strip = egui::Rect::from_min_max(
        egui::pos2(cell.left(), cell.bottom() - STRIP_HEIGHT),
        cell.right_bottom(),
    );
    painter.rect_filled(strip, 6.0, theme::BG);

    if entry.label != cullr_core::Label::None {
        painter.circle_filled(
            egui::pos2(strip.left() + 10.0, strip.center().y),
            3.5,
            theme::label_color(entry.label),
        );
    }

    let mut name = truncated(file_name(entry));
    if entry.status == PhotoStatus::Error {
        name.insert_str(0, "⚠ ");
    }
    let color = if entry.status == PhotoStatus::Ok || entry.status == PhotoStatus::Error {
        theme::TEXT
    } else {
        theme::MUTED
    };
    let name_x = if entry.label == cullr_core::Label::None {
        strip.left() + 8.0
    } else {
        // Clear the label dot.
        strip.left() + 17.0
    };
    painter.text(
        egui::pos2(name_x, strip.center().y),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(11.0),
        color,
    );
}

/// Number of whole cells that fit in `width`, accounting for gaps.
fn columns_for_width(width: f32) -> usize {
    ((width + GAP) / (CELL_WIDTH + GAP)).floor().max(0.0) as usize
}

/// Largest centered rectangle with `aspect` that fits inside `container`.
/// Shared with the loupe, which fits the same previews to the window.
pub(crate) fn fit_rect(container: egui::Rect, aspect: f32) -> egui::Rect {
    let aspect = aspect.max(0.01);
    let width = container.width().min(container.height() * aspect);
    let height = width / aspect;
    egui::Rect::from_center_size(container.center(), egui::vec2(width, height))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_for_width_should_fit_whole_cells_only() {
        // Two cells plus one gap need exactly 232 * 2 + 8 = 472 px.
        assert_eq!(columns_for_width(472.0), 2);
        // One pixel short must not promise a second column.
        assert_eq!(columns_for_width(471.0), 1);
        // A third cell would need 712 px.
        assert_eq!(columns_for_width(711.0), 2);
    }

    #[test]
    fn columns_for_width_should_report_zero_for_zero_width() {
        // Callers clamp to one column; the pure function stays honest.
        assert_eq!(columns_for_width(0.0), 0);
    }

    #[test]
    fn fit_rect_should_letterbox_portrait_images_inside_landscape_cells() {
        let container = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 100.0));

        let portrait = fit_rect(container, 0.5);
        let landscape = fit_rect(container, 2.0);

        assert_eq!(portrait.size(), egui::vec2(50.0, 100.0));
        assert_eq!(landscape.size(), egui::vec2(200.0, 100.0));
    }

    #[test]
    fn fit_rect_should_center_within_the_container() {
        let container =
            egui::Rect::from_min_size(egui::Pos2::new(10.0, 20.0), egui::vec2(300.0, 150.0));

        let fitted = fit_rect(container, 1.0);

        assert_eq!(fitted.center(), container.center());
        assert_eq!(fitted.size(), egui::vec2(150.0, 150.0));
    }

    #[test]
    fn truncated_should_keep_short_names_intact() {
        assert_eq!(truncated("IMG_0001.CR3".into()), "IMG_0001.CR3");
    }

    #[test]
    fn truncated_should_keep_the_extension_when_cutting() {
        let long = "I".repeat(40) + ".CR3";
        let cut = truncated(long.as_str().into());
        assert!(cut.starts_with('…'));
        assert!(cut.ends_with(".CR3"));
    }
}
