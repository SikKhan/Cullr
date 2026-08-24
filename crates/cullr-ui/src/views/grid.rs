//! Grid view: virtualized contact sheet (SPEC §6).
//!
//! Layout is computed once per frame: fixed-width cells in as many columns
//! as the viewport fits, drawn through [`egui::ScrollArea::show_rows`] so
//! only the visible rows (+ egui's built-in margin row) allocate anything.
//! Cell images are aspect-fit thumbnails streamed in by the texture cache;
//! pending rows spin, failed rows degrade to error tiles (SPEC §7).
//!
//! A [`widgets::LabelFilter`] narrows the sheet through `view`, a list of
//! positions into the full row set; cursor, loupe navigation and priority
//! pings all address that filtered order, so culling keys keep working
//! inside a selection like "red only". Refilters rebuild `view` in one
//! pass over the rows — microseconds at 10k — and re-anchor the cursor
//! onto its photo when it survives (SPEC §10 T11).
//!
//! The sheet also feeds the ingest engine's visible-window priority: every
//! frame's visible-but-pending ids are reported via [`Self::take_priority_ping`]
//! whenever the set changes (SPEC §5.2).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui;

use cullr_core::PhotoEntry;
use cullr_core::PhotoId;
use cullr_core::PhotoStatus;

use super::Action;
use super::loupe;
use super::widgets;
use super::widgets::FilterChip;
use super::widgets::LabelFilter;
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
/// Arrow keys that move the sheet cursor; column count turns ↑/↓ into
/// whole-row steps.
const NAV_KEYS: [egui::Key; 4] = [
    egui::Key::ArrowLeft,
    egui::Key::ArrowRight,
    egui::Key::ArrowUp,
    egui::Key::ArrowDown,
];

/// State of the Grid screen for the folder being browsed.
pub struct GridView {
    root: PathBuf,
    /// Every live row of the folder, in scanner order.
    entries: Vec<PhotoEntry>,
    /// Row index by photo id so per-photo ingest events are O(1) updates.
    index: HashMap<PhotoId, usize>,
    /// Positions into `entries` that survive [`Self::filter`], in display
    /// order. Everything user-facing — cursor, loupe, sheet rows —
    /// addresses this view, never the raw row set.
    view: Vec<usize>,
    filter: LabelFilter,
    loading: bool,
    error: Option<String>,
    ingest_total: usize,
    ingest_done: usize,
    /// When the running ingest batch started, for the files/s stat.
    ingest_started: Option<Instant>,
    /// Visible pending ids already sent as a priority ping; dedups repeats.
    last_ping: Vec<PhotoId>,
    /// Ping waiting to be collected by the app, if any.
    ping: Option<Vec<PhotoId>>,
    /// Keyboard cursor in the filtered sheet (SPEC §6): arrows move it,
    /// digits label its photo, Enter/Space open the loupe on it. Starts
    /// on the first tile so keys work the moment a folder is open.
    cursor: Option<usize>,
    /// Cursor row queued for a one-shot scroll-into-view.
    scroll_target: Option<usize>,
    /// Auto-advance-after-label, persisted across sessions (SPEC §6).
    auto_advance: bool,
    /// Loupe position queued by a keyboard open. Creation waits for the
    /// next frame because the opening keypress must not leak into the
    /// loupe's own handlers (Space would start it zoomed, Enter would
    /// close it instantly).
    open_requested: Option<usize>,
    /// Full-screen preview state while the loupe is open; `None` shows
    /// the contact sheet (SPEC §6: Grid ⇄ Loupe).
    loupe: Option<loupe::LoupeView>,
}

impl GridView {
    /// Creates the view in its scanning state; contents arrive later via
    /// [`Self::apply_scan`] (SPEC §5.1 placeholders-first). `auto_advance`
    /// arrives pre-loaded from the persisted kv setting.
    pub fn new(root: PathBuf, auto_advance: bool) -> Self {
        Self {
            root,
            entries: Vec::new(),
            index: HashMap::new(),
            view: Vec::new(),
            filter: LabelFilter::all(),
            loading: true,
            error: None,
            ingest_total: 0,
            ingest_done: 0,
            ingest_started: None,
            last_ping: Vec::new(),
            ping: None,
            cursor: None,
            scroll_target: None,
            auto_advance,
            open_requested: None,
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
                self.rebuild_view();
                // Keys must work before any click, so the cursor arms on
                // the first tile as soon as the folder resolves.
                self.cursor = (!self.view.is_empty()).then_some(0);
            }
            Err(message) => self.error = Some(message),
        }
    }

    /// Announces a running ingest batch of `total` photos.
    pub fn begin_ingest(&mut self, total: usize) {
        self.ingest_total = total;
        self.ingest_done = 0;
        self.ingest_started = Some(Instant::now());
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
        let columns = columns_for_width(ui.available_width()).max(1);
        // Mount a loupe queued last frame before any input handling, so
        // its first drawn frame starts with fresh key state.
        if self.loupe.is_none() {
            self.loupe = self.open_requested.take().map(loupe::LoupeView::at);
        }
        // Keyboard-first navigation. While the loupe is up it owns every
        // key except `F`; on the sheet, Esc leaves the folder and the
        // rest drives cursor + labeling (SPEC §6).
        //
        // Esc works on every mounted sheet state — including scan-error,
        // empty-folder and filtered-to-nothing, whose notices all promise
        // it — while the movement keys need tiles to move across.
        let escape =
            self.loupe.is_none() && ui.ctx().input(|input| input.key_pressed(egui::Key::Escape));
        let sheet_keys = self.loupe.is_none() && !self.view.is_empty();
        let (enter, space, nav) = if sheet_keys {
            ui.ctx().input(|input| {
                (
                    input.key_pressed(egui::Key::Enter),
                    input.key_pressed(egui::Key::Space),
                    NAV_KEYS.into_iter().find(|key| input.key_pressed(*key)),
                )
            })
        } else {
            (false, false, None)
        };
        // The filter preset key works everywhere in the folder, loupe
        // included, so "show me only what I marked" is one press away.
        if !self.loading && ui.ctx().input(|input| input.key_pressed(egui::Key::F)) {
            self.filter.cycle_preset();
            self.refilter();
        }
        if escape {
            action = Some(Action::BackToHome);
        }
        // Tab and digits belong to whichever view is on screen: the
        // loupe handles its own copies, so the sheet must stay quiet
        // while it is open or every press would land twice.
        if sheet_keys {
            if widgets::tab_pressed(ui.ctx()) {
                self.auto_advance = !self.auto_advance;
                widgets::store_auto_advance(db, self.auto_advance);
            }
            if let Some(label) = widgets::pressed_label_key(ui.ctx())
                && let Some(cursor) = self.cursor
            {
                self.label_cursor(db, cursor, label);
            }
            if let Some(cursor) = self.cursor
                && let Some(next) =
                    nav.and_then(|key| stepped_cursor(cursor, key, columns, self.view.len()))
            {
                self.cursor = Some(next);
                self.scroll_target = Some(next);
            }
            // Full keyboard cull pass: Enter/Space jump into the loupe at
            // the cursor so digits keep flowing without touching the mouse.
            if enter || space {
                self.open_requested = Some(self.cursor.unwrap_or(0));
            }
        }

        let tally = self.tally();
        egui::Panel::top(egui::Id::new("cullr_grid_top_bar")).show(ui, |ui| {
            self.top_bar(ui, &tally, &mut action);
        });
        if self.show_filter_bar() {
            egui::Panel::top(egui::Id::new("cullr_filter_bar")).show(ui, |ui| {
                self.filter_bar(ui, &tally);
            });
        }
        egui::CentralPanel::default().show(ui, |ui| self.body(ui, db, textures, columns));

        action
    }

    /// Persistent top bar: back affordance, folder name, status stats —
    /// shown/total when filtered, ingest progress + rate while extracting,
    /// and a clickable error counter that jumps between failed tiles.
    fn top_bar(&mut self, ui: &mut egui::Ui, tally: &Tally, action: &mut Option<Action>) {
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

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Live state of the cull-pass mode; the tooltip carries
                // the shortcut since the bar has no room for hints.
                let (text, color) = if self.auto_advance {
                    ("auto-advance on", theme::ACCENT)
                } else {
                    ("auto-advance off", theme::MUTED)
                };
                ui.label(egui::RichText::new(text).color(color))
                    .on_hover_text("Tab — after labeling, jump to the next photo");
                if tally.errors > 0 && draw_error_chip(ui, tally.errors) {
                    self.jump_next_error();
                }
                ui.separator();
                ui.label(egui::RichText::new(self.stats_line()).color(theme::MUTED));
            });
        });
    }

    /// Right-hand status text (SPEC §6 status bar): shown/total whenever a
    /// filter is active, ingest progress plus files/s while a batch runs.
    fn stats_line(&self) -> String {
        if self.loading {
            return "Scanning…".to_owned();
        }
        let mut line = if self.filter.is_all() {
            format!(
                "{} photo{}",
                widgets::grouped(self.entries.len()),
                if self.entries.len() == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "{} / {} shown",
                widgets::grouped(self.view.len()),
                widgets::grouped(self.entries.len())
            )
        };
        if self.is_ingesting() && self.ingest_total > 0 {
            // Average rate over the batch so far; floored at half a
            // second so the first events cannot divide by ~zero.
            let secs = self
                .ingest_started
                .map_or(0.5, |started| started.elapsed().as_secs_f64())
                .max(0.5);
            let rate = self.ingest_done as f64 / secs;
            line.push_str(&format!(
                " · extracting {} / {} · {}/s",
                self.ingest_done, self.ingest_total, rate as u64
            ));
        }
        line
    }

    /// The filter bar only makes sense once there is something to filter;
    /// it must survive an empty result though, or a too-narrow selection
    /// could never be cleared.
    fn show_filter_bar(&self) -> bool {
        !self.loading && self.error.is_none() && !self.entries.is_empty()
    }

    /// Filter bar row: chips with live per-label counts; clicks refilter
    /// immediately (SPEC §6).
    fn filter_bar(&mut self, ui: &mut egui::Ui, tally: &Tally) {
        ui.add_space(3.0);
        let picked = widgets::filter_chips(ui, self.filter, &tally.counts);
        match picked {
            Some(FilterChip::All) => {
                self.filter.clear();
                self.refilter();
            }
            Some(FilterChip::Label(label)) => {
                self.filter.toggle(label);
                self.refilter();
            }
            None => {}
        }
        ui.add_space(3.0);
    }

    /// One pass over the rows for every count the bars render: per-label
    /// chips and the error tile counter. A single linear scan keeps this
    /// far inside a frame even at 10k rows (SPEC §10 T11).
    fn tally(&self) -> Tally {
        let mut counts = [0_usize; 6];
        let mut errors = 0_usize;
        for entry in &self.entries {
            counts[entry.label.to_u8() as usize] += 1;
            if entry.status == PhotoStatus::Error {
                errors += 1;
            }
        }
        Tally { counts, errors }
    }

    /// Rebuilds the filtered view from the current filter state and
    /// re-anchors cursor and loupe onto their photos. Photos that survive
    /// stay under the user's hands at their new position; a photo that
    /// fell out of the loupe's order closes it back to the sheet.
    ///
    /// Membership deliberately ignores later label edits: relabeling a
    /// tile never yanks it out from under the cursor mid-cull-pass — the
    /// next explicit refilter (chip click / `F`) refreshes the set.
    fn refilter(&mut self) {
        let anchor_row = self
            .cursor
            .and_then(|cursor| self.view.get(cursor))
            .copied();
        let anchor_pos = self.cursor;
        let loupe_row = self
            .loupe
            .as_ref()
            .and_then(|loupe| self.view.get(loupe.index()))
            .copied();

        self.rebuild_view();
        let found = anchor_row.and_then(|row| self.view.iter().position(|&r| r == row));
        self.cursor = match found {
            Some(position) => Some(position),
            // The anchored photo was filtered away: park the cursor on
            // the nearest surviving slot rather than dumping it.
            None => anchor_pos
                .filter(|_| !self.view.is_empty())
                .map(|position| position.min(self.view.len() - 1)),
        };
        if self.cursor.is_none() && !self.view.is_empty() {
            // A refilter that brings tiles back arms the cursor so keys
            // work without a click, mirroring apply_scan.
            self.cursor = Some(0);
        }
        if let Some(position) = self.cursor {
            self.scroll_target = Some(position);
        }
        match loupe_row.and_then(|row| self.view.iter().position(|&r| r == row)) {
            Some(position) => {
                if let Some(loupe) = &mut self.loupe {
                    loupe.jump_to(position);
                }
            }
            None => self.loupe = None,
        }
    }

    /// Recomputes the filtered view: one branchy pass over the rows.
    fn rebuild_view(&mut self) {
        let filter = self.filter;
        self.view = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| filter.matches(entry.label))
            .map(|(row, _)| row)
            .collect();
    }

    /// Moves the cursor to the next extraction-error tile in view order,
    /// wrapping once past the end (SPEC §6 status bar: clickable error
    /// count). No-op when nothing fails.
    fn jump_next_error(&mut self) {
        let len = self.view.len();
        if len == 0 {
            return;
        }
        let start = self.cursor.map_or(0, |cursor| (cursor + 1) % len);
        for step in 0..len {
            let position = (start + step) % len;
            let row = self.view[position];
            if self.entries[row].status == PhotoStatus::Error {
                self.cursor = Some(position);
                self.scroll_target = Some(position);
                return;
            }
        }
    }

    /// Body: loupe when open, otherwise scanning notice, failure notice,
    /// empty notices or tile sheet.
    fn body(
        &mut self,
        ui: &mut egui::Ui,
        db: &cullr_core::Db,
        textures: &mut Textures,
        columns: usize,
    ) {
        if let Some(active) = self.loupe.as_mut() {
            // The loupe mutates the sheet's rows in place (labels land in
            // the same source of truth) and shares the advance toggle;
            // its index addresses the filtered order, so arrows skip
            // tiles the filter hides.
            let outcome = active.ui(
                ui,
                db,
                &mut self.entries,
                &self.view,
                textures,
                &mut self.auto_advance,
            );
            if outcome == loupe::Outcome::Close {
                // Resume sheet navigation where the preview left off.
                self.cursor = Some(active.index());
                self.scroll_target = Some(active.index());
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

        if self.view.is_empty() {
            // Every photo is filtered out; the chips stay visible above
            // so one click on All brings the sheet back.
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 2.0 - 50.0);
                ui.label(egui::RichText::new("No photos match this filter").color(theme::TEXT));
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Click All above — or press F — to see everything again")
                        .small()
                        .color(theme::MUTED),
                );
            });
            return;
        }

        self.draw_sheet(ui, textures, columns);
    }

    /// Applies a digit label to the photo under the cursor: instant
    /// persist (single UPDATE) mirrored into the row so the strip dot
    /// refreshes immediately. Auto-advance then steps forward through the
    /// filtered order, exactly like the loupe's flow (SPEC §6).
    fn label_cursor(&mut self, db: &cullr_core::Db, cursor: usize, label: cullr_core::Label) {
        let Some(&row) = self.view.get(cursor) else {
            return;
        };
        let Some(entry) = self.entries.get_mut(row) else {
            return;
        };
        if entry.label != label {
            entry.label = label;
            if let Err(error) = db.set_label(entry.id, label) {
                tracing::warn!(%error, id = entry.id.0, "cannot persist label");
            }
        }
        if self.auto_advance {
            let last = self.view.len().saturating_sub(1);
            let next = (cursor + 1).min(last);
            if next != cursor {
                self.cursor = Some(next);
                self.scroll_target = Some(next);
            }
        }
    }

    /// Virtualized tile sheet: only rows intersecting the viewport (plus a
    /// margin row, added by `show_rows`) allocate widgets or textures.
    /// Cells walk the filtered `view`, so hidden photos cost nothing.
    ///
    /// The visible id set feeds the texture manager's focus (scroll-driven
    /// decode cancellation) and a band of neighbouring rows is prefetched,
    /// both after drawing so they see the settled viewport.
    fn draw_sheet(&mut self, ui: &mut egui::Ui, textures: &mut Textures, columns: usize) {
        ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
        let total_rows = self.view.len().div_ceil(columns);
        let view = &self.view;
        let entries = &self.entries;
        let cursor = self.cursor;
        let mut visible_keys: Vec<TexKey> = Vec::new();
        let mut visible_pending: Vec<PhotoId> = Vec::new();
        let mut clicked: Option<usize> = None;
        let mut rows_shown = 0..0;

        // One-shot scroll-into-view after arrow/auto-advance/refilter
        // moves: egui 0.36 has no request-a-row API, so the pixel offset
        // of the target row is computed and applied exactly once.
        let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
        if let Some(row) = self.scroll_target.take() {
            let content_height = total_rows.max(1) as f32 * CELL_HEIGHT + GAP;
            let centered =
                row as f32 * (CELL_HEIGHT + GAP) + CELL_HEIGHT / 2.0 - ui.available_height() / 2.0;
            scroll = scroll.vertical_scroll_offset(centered.clamp(0.0, content_height));
        }
        scroll.show_rows(ui, CELL_HEIGHT, total_rows, |ui, rows| {
            rows_shown = rows.clone();
            for row in rows {
                ui.horizontal(|ui| {
                    for column in 0..columns {
                        let position = row * columns + column;
                        let Some(&entry_row) = view.get(position) else {
                            break;
                        };
                        let Some(entry) = entries.get(entry_row) else {
                            break;
                        };
                        visible_keys.push(TexKey::thumb(entry.id));
                        if draw_cell(
                            ui,
                            textures,
                            entry,
                            cursor == Some(position),
                            &mut visible_pending,
                        ) {
                            clicked = Some(position);
                        }
                    }
                });
            }
        });

        // Row-major traversal is already sorted; `focus` relies on that.
        visible_keys.sort_unstable();
        textures.focus(&visible_keys);
        self.prefetch_band(textures, rows_shown, columns, total_rows);

        // Clicking a tile focuses it and opens the loupe on it
        // (SPEC §6 Grid ⇄ Loupe).
        if let Some(position) = clicked {
            self.cursor = Some(position);
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
        let band = (first..last)
            .filter(|row| !rows.contains(row))
            .flat_map(|row| (0..columns).map(move |column| row * columns + column))
            .filter_map(|cell| self.view.get(cell))
            .filter_map(|&entry_row| self.entries.get(entry_row))
            .filter(|entry| entry.status == PhotoStatus::Ok)
            .map(|entry| (TexKey::thumb(entry.id), entry.thumb_path.as_deref()));
        textures.prefetch(band);
    }
}

/// Per-frame counts for the bars: chip tallies by label plus error tiles
/// (SPEC §6 filter bar / status bar).
struct Tally {
    counts: [usize; 6],
    errors: usize,
}

/// Clickable `⚠ N` pill for extraction failures; jumps the cursor between
/// error tiles on click (SPEC §6 status bar).
fn draw_error_chip(ui: &mut egui::Ui, errors: usize) -> bool {
    let text = widgets::grouped(errors);
    let label = format!("⚠ {text}");
    let galley = ui
        .painter()
        .layout_no_wrap(label, egui::FontId::proportional(12.0), theme::MUTED);
    let padding_x = 9.0;
    let size = egui::vec2(padding_x * 2.0 + galley.size().x, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    let hot = response.hovered();
    painter.rect_filled(rect, 11.0, if hot { theme::PANEL } else { theme::BG });
    painter.rect_stroke(
        rect,
        11.0,
        egui::Stroke::new(1.0, theme::RED.gamma_multiply(if hot { 0.8 } else { 0.5 })),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        egui::pos2(
            rect.left() + padding_x,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        if hot { theme::TEXT } else { theme::RED },
    );
    let clicked = response.clicked();
    response.on_hover_text("Jump to the next photo that failed to extract");
    clicked
}

/// One grid cell: panel rectangle, aspect-fit thumbnail (or spinner /
/// error fallback) and the filename strip with its color-label dot.
/// The keyboard cursor cell gets an accent border. Reports whether the
/// cell was clicked (focus + open in loupe).
fn draw_cell(
    ui: &mut egui::Ui,
    textures: &mut Textures,
    entry: &PhotoEntry,
    is_cursor: bool,
    visible_pending: &mut Vec<PhotoId>,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(CELL_WIDTH, CELL_HEIGHT), egui::Sense::click());
    ui.painter().rect_filled(rect, 6.0, theme::PANEL);
    if is_cursor {
        // Accent ring marks where digit keys will land (SPEC §6 cursor).
        ui.painter().rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(2.0, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
    }

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

/// Cursor position after an arrow press: left/right step by one, up/down
/// by a full row. Every move clamps inside the set instead of wrapping,
/// so the cursor is never flung to the far side of the sheet; `None`
/// means the key was not navigational or the move ran into an edge.
fn stepped_cursor(cursor: usize, key: egui::Key, columns: usize, len: usize) -> Option<usize> {
    let target = match key {
        egui::Key::ArrowLeft => cursor.checked_sub(1)?,
        egui::Key::ArrowRight => cursor + 1,
        egui::Key::ArrowUp => cursor.checked_sub(columns.max(1))?,
        egui::Key::ArrowDown => cursor + columns.max(1),
        _ => return None,
    };
    Some(target.min(len - 1))
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

    #[test]
    fn stepped_cursor_should_step_by_one_horizontally() {
        assert_eq!(stepped_cursor(4, egui::Key::ArrowRight, 5, 10), Some(5));
        assert_eq!(stepped_cursor(4, egui::Key::ArrowLeft, 5, 10), Some(3));
    }

    #[test]
    fn stepped_cursor_should_clamp_at_set_edges() {
        assert_eq!(stepped_cursor(9, egui::Key::ArrowRight, 5, 10), Some(9));
        assert_eq!(stepped_cursor(0, egui::Key::ArrowLeft, 5, 10), None);
    }

    #[test]
    fn stepped_cursor_should_step_whole_rows_vertically() {
        // Three columns: down from position 1 lands on 4.
        assert_eq!(stepped_cursor(1, egui::Key::ArrowDown, 3, 10), Some(4));
        assert_eq!(stepped_cursor(4, egui::Key::ArrowUp, 3, 10), Some(1));
    }

    #[test]
    fn stepped_cursor_should_stop_inside_the_last_row() {
        // Ten items in three columns end at position 9; moving down from
        // the middle of the last row settles on the final item.
        assert_eq!(stepped_cursor(7, egui::Key::ArrowDown, 3, 10), Some(9));
    }

    #[test]
    fn stepped_cursor_should_refuse_moves_off_the_top() {
        // First row has no row above to move into.
        assert_eq!(stepped_cursor(1, egui::Key::ArrowUp, 3, 10), None);
    }

    #[test]
    fn stepped_cursor_should_ignore_non_navigational_keys() {
        assert_eq!(stepped_cursor(3, egui::Key::Space, 5, 10), None);
    }

    // --- filter integration (SPEC §10 T11) ---

    use cullr_core::Label as TestLabel;

    fn entry_at(id: u64, label: TestLabel) -> PhotoEntry {
        PhotoEntry {
            id: cullr_core::PhotoId(id),
            rel_path: format!("IMG_{id:04}.CR3").into(),
            label,
            status: PhotoStatus::Ok,
            pixels: Some((6000, 4000)),
            orientation: 1,
            thumb_path: None,
            err_msg: None,
        }
    }

    fn grid_with(labels: &[TestLabel]) -> GridView {
        let mut grid = GridView::new("/photos".into(), false);
        let entries = labels
            .iter()
            .enumerate()
            .map(|(index, label)| entry_at(index as u64 + 1, *label))
            .collect();
        grid.apply_scan(Ok(entries));
        grid
    }

    #[test]
    fn apply_scan_should_arm_cursor_on_first_visible_tile() {
        let mut grid = grid_with(&[TestLabel::None]);
        grid.filter = LabelFilter::labeled();
        grid.refilter();

        assert!(grid.view.is_empty());
        assert_eq!(grid.cursor, None);

        grid.filter = LabelFilter::all();
        grid.refilter();

        assert_eq!(grid.view.len(), 1);
        assert_eq!(grid.cursor, Some(0));
    }

    #[test]
    fn refilter_should_narrow_the_view_to_matching_labels_in_order() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red, TestLabel::None]);

        grid.filter.toggle(TestLabel::Red);
        grid.refilter();

        assert_eq!(grid.view, vec![1]);
        grid.filter.clear();
        grid.refilter();

        assert_eq!(grid.view, vec![0, 1, 2]);
    }

    #[test]
    fn refilter_should_reanchor_cursor_onto_its_photo() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red]);
        grid.cursor = Some(1);
        grid.filter.toggle(TestLabel::Red);
        grid.refilter();

        // The red photo moved from view slot 1 to slot 0 but keeps the
        // cursor; digits keep landing on the same tile.
        assert_eq!(grid.cursor, Some(0));
    }

    #[test]
    fn refilter_should_park_cursor_nearby_when_its_photo_is_filtered_out() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None, TestLabel::None]);
        grid.cursor = Some(2);
        grid.filter.toggle(TestLabel::Red);
        grid.refilter();

        assert_eq!(grid.cursor, None);
    }

    #[test]
    fn jump_next_error_should_find_errors_after_the_cursor_then_wrap() {
        let mut grid = GridView::new("/photos".into(), false);
        let entries = vec![
            entry_at(1, TestLabel::None),
            entry_at(2, TestLabel::None),
            entry_at(3, TestLabel::None),
            entry_at(4, TestLabel::None),
        ];
        grid.apply_scan(Ok(entries));
        grid.entries[1].status = PhotoStatus::Error;
        grid.entries[3].status = PhotoStatus::Error;
        grid.rebuild_view();
        grid.cursor = Some(1);

        grid.jump_next_error();

        assert_eq!(grid.cursor, Some(3), "the next error in order wins");

        grid.jump_next_error();

        assert_eq!(grid.cursor, Some(1), "wrapping must revisit earlier tiles");
    }

    #[test]
    fn jump_next_error_should_be_a_noop_without_failures() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None]);
        grid.cursor = Some(1);

        grid.jump_next_error();

        assert_eq!(grid.cursor, Some(1));
    }

    #[test]
    fn stats_line_should_report_shown_over_total_only_while_filtered() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red]);

        assert_eq!(grid.stats_line(), "2 photos");
        grid.filter.toggle(TestLabel::Red);
        grid.refilter();

        assert_eq!(grid.stats_line(), "1 / 2 shown");
    }
}
