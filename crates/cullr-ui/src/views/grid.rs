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
//! Selection is a set of photo ids layered over that cursor (SPEC §6):
//! click focuses + selects, Ctrl-click toggles, Shift-click ranges from
//! an anchor, dragging a rubber band selects what it covers, and digits
//! label the whole set in one keystroke. Because membership is keyed by
//! id, refilters and reorders never silently drop marked photos; the
//! marquee maps screen points back to tiles through pure cell geometry.
//!
//! The sheet also feeds the ingest engine's visible-window priority: every
//! frame's visible-but-pending ids are reported via [`Self::take_priority_ping`]
//! whenever the set changes (SPEC §5.2).

use std::collections::HashMap;
use std::collections::HashSet;
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
/// Pointer travel before a press becomes a rubber-band drag; below it a
/// press-and-release stays a click (SPEC §6 selection). Matches egui's
/// own click slop so the two never disagree.
const MARQUEE_MIN_DRAG: f32 = 6.0;

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
    /// Selected photos (SPEC §6), keyed by id so refilters keep member-
    /// ship while tiles hide or reorder. Digits label the whole set.
    selection: HashSet<PhotoId>,
    /// View position where the last plain or Ctrl click landed; Shift-
    /// click ranges from here through the clicked tile.
    anchor: Option<usize>,
    /// Rubber-band drag over the sheet, while one is in progress.
    marquee: Option<Marquee>,
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
            selection: HashSet::new(),
            anchor: None,
            marquee: None,
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
                // Ids from the replaced row set must not leak into the
                // fresh folder's batches or counts.
                self.selection.clear();
                self.anchor = None;
                self.marquee = None;
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
        // Select all / none sweep only what survives the filter: photos
        // hidden by a chip never join a batch (SPEC §6 keyboard map).
        let (select_all, select_none) = if sheet_keys {
            ui.ctx().input(|input| {
                (
                    input.key_pressed(egui::Key::A) && input.modifiers.command_only(),
                    input.key_pressed(egui::Key::A) && input.modifiers.shift_only(),
                )
            })
        } else {
            (false, false)
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
            if select_all {
                self.select_all();
            }
            if select_none {
                // The cursor stays put so arrows and digits keep working
                // from the same tile (SPEC §6: cursor + selection set).
                self.selection.clear();
            }
            if let Some(label) = widgets::pressed_label_key(ui.ctx()) {
                self.apply_label(db, label);
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
        if !self.selection.is_empty() {
            // Batch feedback next to the counts it acts on (SPEC §10 T12).
            line.push_str(&format!(
                " · {} selected",
                widgets::grouped(self.selection.len())
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

    /// Selects every photo surviving the filter (SPEC §6 Ctrl+A): a
    /// batch must never sweep tiles the user culled out of sight.
    fn select_all(&mut self) {
        self.selection = self
            .view
            .iter()
            .filter_map(|&row| self.entries.get(row))
            .map(|entry| entry.id)
            .collect();
    }

    /// Photo id at a filtered-order position, if any.
    fn id_at(&self, position: usize) -> Option<PhotoId> {
        let &row = self.view.get(position)?;
        self.entries.get(row).map(|entry| entry.id)
    }

    /// Applies a digit label to every selected photo — or to the
    /// cursor's photo when nothing is selected (SPEC §6: digits apply to
    /// the selection). Rows mirror instantly so tiles recolor this
    /// frame; persistence lands as one transaction however big the
    /// batch. Auto-advance stays out of multi-photo relabels: a hundred-
    /// photo keystroke must not fling the cursor down the sheet.
    fn apply_label(&mut self, db: &cullr_core::Db, label: cullr_core::Label) {
        if self.selection.is_empty() {
            if let Some(cursor) = self.cursor {
                self.label_cursor(db, cursor, label);
            }
            return;
        }
        let mut changed: Vec<PhotoId> = Vec::new();
        for entry in &mut self.entries {
            if self.selection.contains(&entry.id) && entry.label != label {
                entry.label = label;
                changed.push(entry.id);
            }
        }
        if !changed.is_empty()
            && let Err(error) = db.set_labels(&changed, label)
        {
            tracing::warn!(%error, photos = changed.len(), "cannot persist labels");
        }
    }

    /// Advances the rubber-band state machine one frame (SPEC §6 drag
    /// marquee). A press inside the sheet viewport arms a candidate;
    /// travel past [`MARQUEE_MIN_DRAG`] makes it live and every frame it
    /// selects the tiles its band covers — replacing the set, or union-
    /// ing onto the press-time set under Ctrl. Release returns `true`
    /// when a real drag just finished so tile click handlers stay quiet;
    /// press-and-release without travel on bare sheet clears the set,
    /// like any file manager's empty-space click.
    ///
    /// `origin` is the screen point of view slot 0 (from [`tile_origin`]);
    /// without drawn tiles there is no geometry and nothing to do.
    fn step_marquee(
        &mut self,
        ui: &egui::Ui,
        origin: Option<egui::Pos2>,
        columns: usize,
        viewport: egui::Rect,
    ) -> bool {
        let Some(origin) = origin else {
            return false;
        };
        let (pressed, released, pointer, modifiers) = ui.ctx().input(|input| {
            (
                input.pointer.primary_pressed(),
                input.pointer.primary_released(),
                input.pointer.latest_pos(),
                input.modifiers,
            )
        });
        if pressed {
            if let Some(start) = pointer.filter(|start| viewport.contains(*start)) {
                // The live set parks in `base`: ctrl-drag unions onto it,
                // an aborted drag hands it back untouched.
                self.marquee = Some(Marquee {
                    start,
                    base: std::mem::take(&mut self.selection),
                    additive: modifiers.command,
                    dragging: false,
                });
            }
            return false;
        }
        // Owned for the rest of the frame so tile lookups can run while
        // the state is out of `self`; stored back before returning.
        let Some(mut marquee) = self.marquee.take() else {
            return false;
        };
        if released {
            let dragging = marquee.dragging;
            let on_tile = point_on_tile(origin, marquee.start, columns, self.view.len());
            self.selection = std::mem::take(&mut marquee.base);
            if dragging {
                // The band owns the final word on the set; swallow the
                // release so it cannot double as a tile click.
                return true;
            }
            if !on_tile {
                // A press-and-release that never became a drag is a
                // click: on bare sheet it means "select nothing".
                self.selection.clear();
            }
            return false;
        }
        let Some(latest) = pointer else {
            // Pointer left the window mid-press; stay armed until it
            // reports again or the release arrives.
            self.marquee = Some(marquee);
            return false;
        };
        if !marquee.dragging && marquee.start.distance(latest) > MARQUEE_MIN_DRAG {
            marquee.dragging = true;
        }
        if marquee.dragging {
            let band = egui::Rect::from_two_pos(marquee.start, latest);
            let covered: HashSet<PhotoId> =
                covered_positions(origin, band, columns, self.view.len())
                    .filter_map(|position| self.id_at(position))
                    .collect();
            self.selection = if marquee.additive {
                marquee.base.union(&covered).copied().collect()
            } else {
                covered
            };
        }
        self.marquee = Some(marquee);
        false
    }

    /// Draws the active band over the tiles: translucent accent wash
    /// with a hairline stroke, matching the selection language.
    fn paint_marquee(&self, ui: &egui::Ui) {
        let Some(marquee) = self.marquee.as_ref().filter(|marquee| marquee.dragging) else {
            return;
        };
        let Some(latest) = ui.ctx().input(|input| input.pointer.latest_pos()) else {
            return;
        };
        let band = egui::Rect::from_two_pos(marquee.start, latest);
        let painter = ui.painter();
        painter.rect_filled(band, 2.0, theme::ACCENT.gamma_multiply(0.12));
        painter.rect_stroke(
            band,
            2.0,
            egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.8)),
            egui::StrokeKind::Inside,
        );
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
        let selection = &self.selection;
        let cursor = self.cursor;
        let mut visible_keys: Vec<TexKey> = Vec::new();
        let mut visible_pending: Vec<PhotoId> = Vec::new();
        let mut first_tile: Option<(usize, egui::Rect)> = None;
        let mut clicked: Option<usize> = None;
        let mut double_clicked: Option<usize> = None;
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
        let output = scroll.show_rows(ui, CELL_HEIGHT, total_rows, |ui, rows| {
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
                        let hit = draw_cell(
                            ui,
                            textures,
                            entry,
                            cursor == Some(position),
                            selection.contains(&entry.id),
                            &mut visible_pending,
                        );
                        if first_tile.is_none() {
                            // Anchors the marquee's screen-to-tile math:
                            // allocation is exact even when the tile is
                            // half scrolled out of view.
                            first_tile = Some((position, hit.rect));
                        }
                        if hit.clicked {
                            clicked = Some(position);
                        }
                        if hit.double_clicked {
                            double_clicked = Some(position);
                        }
                    }
                });
            }
        });
        // The on-screen viewport, excluding scroll bars: presses outside
        // it (bars, chips) must not start a marquee.
        let viewport = output.inner_rect;

        // Row-major traversal is already sorted; `focus` relies on that.
        visible_keys.sort_unstable();
        textures.focus(&visible_keys);
        self.prefetch_band(textures, rows_shown, columns, total_rows);

        // Rubber band first so a finished drag can swallow the release
        // that would otherwise land on a tile as a click (SPEC §6).
        let origin = first_tile.map(|(position, rect)| tile_origin(position, rect, columns));
        let release_consumed = self.step_marquee(ui, origin, columns, viewport);
        self.paint_marquee(ui);

        // Mouse path to the loupe is a double-click now that plain click
        // selects; it mounts next frame like keyboard opens so the
        // second click never leaks into the preview (SPEC §6).
        if !release_consumed {
            if let Some(position) = double_clicked {
                self.cursor = Some(position);
                self.anchor = Some(position);
                self.selection = self.id_at(position).into_iter().collect();
                self.open_requested = Some(position);
            } else if let Some(position) = clicked {
                let modifiers = ui.ctx().input(|input| input.modifiers);
                self.cursor = Some(position);
                if modifiers.shift {
                    // Extend from the anchor through this tile, replacing
                    // whatever was selected (SPEC §6 shift-click).
                    let anchor = self.anchor.unwrap_or(position);
                    self.selection = click_range(anchor, position)
                        .filter_map(|slot| self.id_at(slot))
                        .collect();
                } else if modifiers.command_only() {
                    if let Some(id) = self.id_at(position)
                        && !self.selection.remove(&id)
                    {
                        self.selection.insert(id);
                    }
                } else {
                    self.selection = self.id_at(position).into_iter().collect();
                }
                // Plain and Ctrl clicks both re-arm the range anchor;
                // Shift keeps walking from wherever it last landed.
                self.anchor = Some(position);
            }
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

/// Rubber-band drag over the sheet (SPEC §6 selection).
struct Marquee {
    /// Screen point where the press began; the band stretches to the
    /// live pointer position.
    start: egui::Pos2,
    /// The selection as it stood at press time: ctrl-drag unions onto
    /// it, an aborted drag hands it back untouched.
    base: HashSet<PhotoId>,
    /// Whether the press turned into a live drag (travel passed
    /// [`MARQUEE_MIN_DRAG`]) or is still just a click candidate.
    dragging: bool,
    /// Ctrl held at press time: the band unions onto [`Self::base`]
    /// instead of replacing it.
    additive: bool,
}

/// What one tile reported this frame: its allocated rect anchors the
/// marquee's screen-to-tile math, the flags drive click semantics
/// (SPEC §6 selection).
struct CellHit {
    rect: egui::Rect,
    clicked: bool,
    double_clicked: bool,
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
/// Selected tiles get an accent wash plus hairline; the keyboard cursor
/// a full accent ring on top (SPEC §6 cursor + selection). Reports the
/// allocated rect and click events for the sheet's selection handling.
fn draw_cell(
    ui: &mut egui::Ui,
    textures: &mut Textures,
    entry: &PhotoEntry,
    is_cursor: bool,
    is_selected: bool,
    visible_pending: &mut Vec<PhotoId>,
) -> CellHit {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(CELL_WIDTH, CELL_HEIGHT), egui::Sense::click());
    ui.painter().rect_filled(rect, 6.0, theme::PANEL);
    if is_selected {
        // Quieter than the cursor ring so a large batch reads as one
        // mass without drowning the tile under it.
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, theme::ACCENT.gamma_multiply(0.10));
        painter.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.55)),
            egui::StrokeKind::Inside,
        );
    }
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
    // `on_hover_text` consumes the response, so read the clicks first.
    let hit = CellHit {
        rect,
        clicked: response.clicked(),
        double_clicked: response.double_clicked(),
    };
    response.on_hover_text(tooltip);
    hit
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

/// Screen-space top-left corner of view slot 0 in unbounded sheet
/// geometry, derived from any one allocated tile — a tile's rect is exact
/// even when half scrolled out of view, so the marquee never depends on
/// scroll-area internals. Pure so the mapping is unit-testable.
fn tile_origin(position: usize, rect: egui::Rect, columns: usize) -> egui::Pos2 {
    let row = (position / columns.max(1)) as f32;
    let column = (position % columns.max(1)) as f32;
    egui::pos2(
        rect.left() - column * (CELL_WIDTH + GAP),
        rect.top() - row * (CELL_HEIGHT + GAP),
    )
}

/// Rect of the tile at `position` in unbounded sheet geometry.
fn tile_rect(origin: egui::Pos2, position: usize, columns: usize) -> egui::Rect {
    let row = (position / columns.max(1)) as f32;
    let column = (position % columns.max(1)) as f32;
    egui::Rect::from_min_size(
        egui::pos2(
            origin.x + column * (CELL_WIDTH + GAP),
            origin.y + row * (CELL_HEIGHT + GAP),
        ),
        egui::vec2(CELL_WIDTH, CELL_HEIGHT),
    )
}

/// Filtered-order position under a screen point, or `None` off the
/// sheet: left or above of the first tile, or past the last one. Pure so
/// marquee behavior is unit-testable.
fn position_at_point(
    origin: egui::Pos2,
    point: egui::Pos2,
    columns: usize,
    len: usize,
) -> Option<usize> {
    let column = ((point.x - origin.x) / (CELL_WIDTH + GAP)).floor();
    let row = ((point.y - origin.y) / (CELL_HEIGHT + GAP)).floor();
    if len == 0 || column < 0.0 || row < 0.0 {
        return None;
    }
    let position = row as usize * columns.max(1) + column as usize;
    (position < len).then_some(position)
}

/// `true` when the point sits inside some tile's rectangle; gaps between
/// cells and the space below the last row count as bare sheet, which is
/// what makes click-on-empty clear the selection (SPEC §6).
fn point_on_tile(origin: egui::Pos2, point: egui::Pos2, columns: usize, len: usize) -> bool {
    position_at_point(origin, point, columns, len)
        .is_some_and(|position| tile_rect(origin, position, columns).contains(point))
}

/// Every tile position whose rectangle intersects `band`, clamped to the
/// live set; drives rubber-band selection (SPEC §6 marquee). Row-major
/// and ascending, so callers may rely on order.
fn covered_positions(
    origin: egui::Pos2,
    band: egui::Rect,
    columns: usize,
    len: usize,
) -> impl Iterator<Item = usize> {
    let columns = columns.max(1);
    let step_x = CELL_WIDTH + GAP;
    let step_y = CELL_HEIGHT + GAP;
    // A tile counts when its rect merely touches the band, hence the
    // size offsets on the leading edges.
    let first_column = (((band.left() - origin.x - CELL_WIDTH) / step_x)
        .ceil()
        .max(0.0)) as usize;
    // Inclusive bounds clamped to the sheet's own shape: a wider band
    // would only wrap columns into duplicate positions.
    let last_column = ((((band.right() - origin.x) / step_x).floor().max(0.0)) as usize)
        .min(first_column + columns - 1);
    let first_row = (((band.top() - origin.y - CELL_HEIGHT) / step_y)
        .ceil()
        .max(0.0)) as usize;
    let last_row = ((((band.bottom() - origin.y) / step_y).floor().max(0.0)) as usize)
        .min(first_row + len.div_ceil(columns).saturating_sub(1));
    (first_row..=last_row)
        .flat_map(move |row| (first_column..=last_column).map(move |column| row * columns + column))
        .take_while(move |&position| position < len.max(1))
}

/// Inclusive display-order span between two click endpoints, visiting
/// order regardless of drag direction (SPEC §6 shift-click).
fn click_range(anchor: usize, clicked: usize) -> std::ops::RangeInclusive<usize> {
    anchor.min(clicked)..=anchor.max(clicked)
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
    // Test setup asserts hard failures; a broken fixture aborts the test.
    #![expect(clippy::expect_used)]

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

    // --- selection (SPEC §10 T12) ---

    use cullr_core::Db;

    use tempfile::TempDir;

    fn db_for_tests() -> (TempDir, Db) {
        let dir = TempDir::new().expect("temp dir");
        let db = Db::open(&dir.path().join("index.db")).expect("open db");
        (dir, db)
    }

    /// Grid whose rows exist in `db` too, so label persistence is
    /// observable: ids come from a real scan-diff, not thin air.
    fn grid_with_db(labels: &[TestLabel], db: &Db) -> GridView {
        let root = std::path::Path::new("/photos");
        let scanned: Vec<cullr_core::PhotoMeta> = labels
            .iter()
            .enumerate()
            .map(|(index, _)| cullr_core::PhotoMeta {
                root: root.to_owned(),
                rel_path: format!("IMG_{:04}.CR3", index + 1).into(),
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: index as u64 + 1,
            })
            .collect();
        let entries = db
            .sync_scan(root, &scanned, cullr_core::ScanOptions::default())
            .expect("sync");
        let mut grid = GridView::new(root.to_owned(), false);
        grid.apply_scan(Ok(entries));
        grid
    }

    #[test]
    fn position_at_point_should_map_points_to_tiles_and_reject_off_sheet_points() {
        let columns = 3;
        let origin = egui::pos2(100.0, 50.0);

        assert_eq!(
            position_at_point(origin, egui::pos2(100.0, 50.0), columns, 7),
            Some(0),
            "the sheet origin is tile zero's corner"
        );
        assert_eq!(
            position_at_point(
                origin,
                egui::pos2(100.0 + CELL_WIDTH + GAP + 1.0, 60.0),
                columns,
                7
            ),
            Some(1)
        );
        assert_eq!(
            position_at_point(
                origin,
                egui::pos2(110.0, 50.0 + CELL_HEIGHT + GAP + 1.0),
                columns,
                7
            ),
            Some(3),
            "one row down steps by the column count"
        );
        assert_eq!(
            position_at_point(
                origin,
                egui::pos2(100.0 + 3.0 * (CELL_WIDTH + GAP) + 5.0, 60.0),
                columns,
                2
            ),
            None,
            "past the last tile there is nothing to address"
        );
        assert_eq!(
            position_at_point(origin, egui::pos2(50.0, 60.0), columns, 7),
            None
        );
        assert_eq!(
            position_at_point(origin, egui::pos2(120.0, 10.0), columns, 7),
            None
        );
    }

    #[test]
    fn point_on_tile_should_exclude_gaps_and_trailing_space() {
        let columns = 2;
        let origin = egui::Pos2::ZERO;

        assert!(point_on_tile(
            origin,
            egui::pos2(CELL_WIDTH - 1.0, CELL_HEIGHT - 1.0),
            columns,
            4
        ));
        assert!(
            !point_on_tile(origin, egui::pos2(CELL_WIDTH + GAP / 2.0, 5.0), columns, 4),
            "the gap between cells is bare sheet"
        );
        // One row down with only one row of tiles: bare space.
        assert!(!point_on_tile(
            origin,
            egui::pos2(5.0, CELL_HEIGHT + GAP + 1.0),
            columns,
            2
        ));
    }

    #[test]
    fn covered_positions_should_take_every_intersecting_tile() {
        let columns = 3;
        let origin = egui::Pos2::ZERO;
        // A small band straddling the corner shared by tiles 0, 1, 3, 4.
        let band = egui::Rect::from_min_max(
            egui::pos2(CELL_WIDTH - 2.0, CELL_HEIGHT - 2.0),
            egui::pos2(CELL_WIDTH + GAP + 2.0, CELL_HEIGHT + GAP + 2.0),
        );

        let covered: Vec<usize> = covered_positions(origin, band, columns, 9).collect();

        assert_eq!(covered, vec![0, 1, 3, 4]);
    }

    #[test]
    fn covered_positions_should_clamp_to_the_live_set() {
        let columns = 3;
        let origin = egui::Pos2::ZERO;
        let band = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(10_000.0, 10_000.0));

        assert_eq!(covered_positions(origin, band, columns, 7).count(), 7);
    }

    #[test]
    fn click_range_should_order_its_endpoints() {
        assert_eq!(click_range(4, 1), 1..=4);
        assert_eq!(click_range(2, 2), 2..=2);
    }

    #[test]
    fn select_all_should_cover_only_the_filtered_view() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red, TestLabel::None]);
        grid.filter.toggle(TestLabel::Red);
        grid.refilter();

        grid.select_all();

        // Only the surviving red photo joins; hidden tiles never join a
        // batch (SPEC §6 Ctrl+A).
        assert_eq!(grid.selection.len(), 1);
        assert!(grid.selection.contains(&cullr_core::PhotoId(2)));
    }

    #[test]
    fn refilter_should_keep_selection_membership_of_hidden_photos() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red]);
        grid.select_all();
        grid.filter.toggle(TestLabel::Red);

        grid.refilter();

        assert_eq!(grid.view.len(), 1);
        assert_eq!(grid.selection.len(), 2, "ids outlive their hiding");
    }

    #[test]
    fn apply_label_should_relabel_a_hundred_selected_photos_in_one_keystroke() {
        let (_dir, db) = db_for_tests();
        let mut grid = grid_with_db(&[TestLabel::None; 100], &db);
        grid.select_all();

        grid.apply_label(&db, TestLabel::Red);

        assert!(
            grid.entries
                .iter()
                .all(|entry| entry.label == TestLabel::Red)
        );
        for entry in &grid.entries {
            let stored = db.photo_entry(entry.id).expect("read").expect("row");
            assert_eq!(stored.label, TestLabel::Red);
        }
    }

    #[test]
    fn apply_label_should_fall_back_to_the_cursor_when_nothing_is_selected() {
        let (_dir, db) = db_for_tests();
        let mut grid = grid_with_db(&[TestLabel::None, TestLabel::None], &db);
        grid.cursor = Some(0);

        grid.apply_label(&db, TestLabel::Green);

        assert_eq!(grid.entries[0].label, TestLabel::Green);
        assert_eq!(grid.entries[1].label, TestLabel::None);
        let stored = db
            .photo_entry(grid.entries[0].id)
            .expect("read")
            .expect("row");
        assert_eq!(stored.label, TestLabel::Green);
    }

    #[test]
    fn apply_label_should_skip_already_labeled_rows_in_a_batch() {
        let (_dir, db) = db_for_tests();
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red]);
        grid.select_all();

        grid.apply_label(&db, TestLabel::Red);

        // The already-red row keeps its label untouched either way; both
        // end red with only one real UPDATE behind them.
        assert!(
            grid.entries
                .iter()
                .all(|entry| entry.label == TestLabel::Red)
        );
    }

    #[test]
    fn select_none_should_empty_the_set_but_keep_the_cursor() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None]);
        grid.cursor = Some(1);
        grid.select_all();

        grid.selection.clear();

        assert!(grid.selection.is_empty());
        assert_eq!(grid.cursor, Some(1));
    }

    #[test]
    fn stats_line_should_count_the_selection_while_present() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None]);

        assert_eq!(grid.stats_line(), "2 photos");
        grid.select_all();

        assert_eq!(grid.stats_line(), "2 photos · 2 selected");
    }
}
