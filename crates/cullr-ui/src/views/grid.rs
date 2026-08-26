//! Grid view: virtualized contact sheet (SPEC §6).
//!
//! Layout is computed once per frame: fixed-width cells in as many columns
//! as the viewport fits, drawn through [`egui::ScrollArea::show_rows`] so
//! only the visible rows (+ egui's built-in margin row) allocate anything.
//! Cell images are aspect-fit thumbnails streamed in by the texture cache;
//! pending rows spin, failed rows degrade to error tiles (SPEC §7). The
//! cell size lives in one [`CellGeom`] value per frame so the zoom slider
//! and Ctrl+wheel resize the sheet without touching any other code.
//!
//! A [`widgets::LabelFilter`] narrows the sheet through `view`, a list of
//! positions into the full row set; cursor, loupe navigation and priority
//! pings all address that filtered order, so culling keys keep working
//! inside a selection like "red only". Refilters rebuild `view` in one
//! pass over the rows — microseconds at 10k — and re-anchor the cursor
//! onto its photo when it survives (SPEC §10 T11).
//!
//! Sorting (SPEC §6: filename / capture time) is applied at that same
//! choke point: [`Self::rebuild_view`] orders the surviving rows before
//! they become `view`, so arrows, Shift-click ranges and the loupe all
//! walk the sorted order by construction. Re-sorts re-anchor cursor and
//! loupe onto their photos by id, exactly like a refilter does.
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
use super::widgets::CHIP_HEIGHT;
use super::widgets::FilterChip;
use super::widgets::LabelFilter;
use super::widgets::SortKey;
use crate::tex::{Rotation, TexKey, TextureState, Textures};
use crate::theme;

/// Display transform for one row: EXIF orientation plus user quarter-turns.
pub(crate) fn row_rotation(entry: &PhotoEntry) -> Rotation {
    Rotation::new(entry.orientation, entry.rot_cw)
}

/// Default cell width; height derives from the default photo aspect.
const CELL_DEFAULT_WIDTH: f32 = 232.0;
/// Narrowest cell the zoom slider / Ctrl+wheel allow (SPEC §6: 128–1024).
pub const CELL_MIN_WIDTH: f32 = 128.0;
/// Widest cell the zoom controls allow.
pub const CELL_MAX_WIDTH: f32 = 1024.0;
/// Points of cell growth per point of Ctrl+wheel scroll; a typical notch
/// (~50 pts) then steps ~25 px, sweeping the range in a few turns.
const ZOOM_WHEEL_GAIN: f32 = 0.5;
/// Inner padding between cell border and image area.
const CELL_PADDING: f32 = 8.0;
/// Filename strip height inside a tile: the name line plus the RAW+JPEG
/// tag line underneath it (empty for unpaired photos, Photo Mechanic-style).
const STRIP_HEIGHT: f32 = 34.0;
/// Vertical center of the filename line within the strip.
const NAME_LINE_CENTER: f32 = 12.0;
/// Distance from the strip's bottom edge to the tag line's center.
const TAG_LINE_BOTTOM: f32 = 7.5;
/// Gap between cells, matching egui's default item spacing we override.
const GAP: f32 = 8.0;
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

/// Cell metrics for one frame: everything about tile layout derives from
/// the zoomable width, so the slider and wheel change one number here and
/// the whole sheet follows (SPEC §6 zoom, 128–1024 px cells).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellGeom {
    /// Long-edge cell width in logical points.
    pub width: f32,
    /// Total cell height including the filename strip.
    height: f32,
}

impl CellGeom {
    /// Derives the full geometry from the zoomable long edge.
    pub fn new(width: f32) -> Self {
        let width = width.clamp(CELL_MIN_WIDTH, CELL_MAX_WIDTH);
        // Uniform rows are what keeps `show_rows` virtualization exact;
        // other aspects letterbox inside this box.
        let height = width / cullr_core::DEFAULT_ASPECT + STRIP_HEIGHT;
        Self { width, height }
    }

    /// Horizontal distance between two neighboring cell origins.
    fn stride_x(self) -> f32 {
        self.width + GAP
    }

    /// Vertical distance between two neighboring cell rows.
    fn stride_y(self) -> f32 {
        self.height + GAP
    }
}

impl Default for CellGeom {
    fn default() -> Self {
        Self::new(CELL_DEFAULT_WIDTH)
    }
}

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
    /// Display order of the sheet (SPEC §6); applied inside
    /// [`Self::rebuild_view`] so every consumer shares one order.
    sort: SortKey,
    /// Capture-time stamps by photo id for [`SortKey::TakenAt`], filled
    /// lazily from point queries; photos absent from the map have not
    /// been looked up yet. `None` values sort last.
    taken_at: HashMap<PhotoId, Option<String>>,
    /// Sort order queued by a pill click this frame, applied after the
    /// bars are drawn (the stamp cache needs the db handle from `ui`).
    pending_sort: Option<SortKey>,
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
    /// Zoomable cell long edge in points (SPEC §6); geometry per frame
    /// derives from it via [`CellGeom::new`].
    cell_width: f32,
    /// Loupe open queued by a keyboard open or double-click. Creation
    /// waits for the next frame because the opening keypress must not
    /// leak into the loupe's own handlers (Space would start it zoomed,
    /// Enter would close it instantly).
    open_requested: Option<OpenRequest>,
    /// Full-screen preview state while the loupe is open; `None` shows
    /// the contact sheet (SPEC §6: Grid ⇄ Loupe).
    loupe: Option<loupe::LoupeView>,
    /// Vertical scroll offset the sheet settled on after its last drawn
    /// frame; diagnostics and headless scroll-behavior tests read this.
    settled_scroll_y: f32,
    /// Screen-space top of the sheet viewport on the last drawn frame;
    /// lets headless tests detect any upward creep of the bars above it.
    settled_viewport_top: f32,
    /// Top-of-sheet scroll queued for the next drawn frame. egui persists
    /// ScrollArea offsets across sessions and folders, so every fresh
    /// mount forces one frame back to the top instead of inheriting
    /// wherever an earlier session left off.
    reset_scroll: bool,
    /// Files processed by the running export job, out of [`Self::export_total`].
    export_done: usize,
    /// Total files announced for the running export job.
    export_total: usize,
    /// Summary of the last finished export, shown beside the button until
    /// the next run or folder change retires it.
    export_note: Option<ExportNote>,
}

/// Finished-export summary pill content beside the export button.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ExportNote {
    text: String,
    color: egui::Color32,
}

/// A queued loupe open: position in the filtered order plus whether it
/// mounts straight into the chromeless lightbox (`L`) or the normal
/// loupe (`Enter`, `Space`, double-click).
#[derive(Clone, Copy, Debug)]
struct OpenRequest {
    index: usize,
    lightbox: bool,
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
            sort: SortKey::default(),
            taken_at: HashMap::new(),
            pending_sort: None,
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
            cell_width: CELL_DEFAULT_WIDTH,
            open_requested: None,
            loupe: None,
            settled_scroll_y: 0.0,
            settled_viewport_top: 0.0,
            reset_scroll: true,
            export_done: 0,
            export_total: 0,
            export_note: None,
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
                // fresh folder's batches, counts or stamp cache.
                self.selection.clear();
                self.anchor = None;
                self.marquee = None;
                self.taken_at.clear();
                self.rebuild_view();
                // Keys must work before any click, so the cursor arms on
                // the first tile as soon as the folder resolves.
                self.cursor = (!self.view.is_empty()).then_some(0);
                // A fresh scan re-mounts the sheet: never resume from a
                // scroll position left over by egui persistence.
                self.reset_scroll = true;
                self.export_done = 0;
                self.export_total = 0;
                self.export_note = None;
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

    /// Announces a running export of `total` original files.
    pub fn begin_export(&mut self, total: usize) {
        self.export_total = total;
        self.export_done = 0;
        self.export_note = None;
    }

    /// Applies one progress tick from the export worker; monotonic so a
    /// late-arriving tick can never rewind the counter.
    pub fn apply_export_progress(&mut self, done: usize) {
        self.export_done = done.max(self.export_done);
    }

    /// Retires a finished export job with its user-facing summary line.
    /// Partial failures and cancellation are reported honestly — the note
    /// stays visible until the next run or folder change replaces it.
    pub fn finish_export(&mut self, outcome: &Result<cullr_core::ExportReport, String>) {
        let note = match outcome {
            Ok(report) => {
                let text = if report.cancelled {
                    "Export cancelled".to_owned()
                } else if report.failures.is_empty() && report.copied == 0 {
                    "Nothing to copy".to_owned()
                } else if report.failures.is_empty() {
                    format!("✓ Exported {}", widgets::grouped(report.copied))
                } else {
                    format!(
                        "Exported {} · {} failed",
                        widgets::grouped(report.copied),
                        widgets::grouped(report.failures.len())
                    )
                };
                ExportNote {
                    color: match report.cancelled || !report.failures.is_empty() {
                        true => theme::RED,
                        false => theme::ACCENT,
                    },
                    text,
                }
            }
            Err(error) => ExportNote {
                text: format!("Export failed: {error}"),
                color: theme::RED,
            },
        };
        self.export_note = Some(note);
        self.export_done = self.export_total;
    }

    /// `true` while an export job is still copying files.
    pub fn is_exporting(&self) -> bool {
        self.export_done < self.export_total
    }

    /// Whether this grid still browses the folder a background event or
    /// job belongs to; stale results for other roots are dropped.
    pub fn is_browsing(&self, root: &std::path::Path) -> bool {
        self.root == root
    }

    /// Absolute paths export should copy: the selection when one exists,
    /// otherwise every photo surviving the filter — both in display order,
    /// so the destination fills in sheet order. RAW+JPEG pairs contribute
    /// both originals back to back (the JPEG right after its RAW).
    pub fn export_set(&self) -> Vec<PathBuf> {
        self.view
            .iter()
            .filter_map(|&row| self.entries.get(row))
            .filter(|entry| self.selection.is_empty() || self.selection.contains(&entry.id))
            .flat_map(|entry| {
                let raw = self.root.join(&entry.rel_path);
                let jpeg = entry.jpeg_rel_path.as_ref().map(|rel| self.root.join(rel));
                [raw].into_iter().chain(jpeg)
            })
            .collect()
    }

    /// Hands the app the latest changed set of visible pending photos for
    /// queue reordering, or `None` when nothing changed since last frame.
    pub fn take_priority_ping(&mut self) -> Option<Vec<PhotoId>> {
        self.ping.take()
    }

    /// Draws the screen and reports the user's action, if any.
    ///
    /// `suspended` is set while a modal dialog (help overlay, About) is
    /// up: the sheet keeps painting — so the dimmed backdrop shows real
    /// content — but every key, wheel and rubber-band path stays quiet,
    /// and clicks are already claimed by the topmost modal layer.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        db: &cullr_core::Db,
        textures: &mut Textures,
        suspended: bool,
    ) -> Option<Action> {
        let mut action = None;
        let geom = CellGeom::new(self.cell_width);
        let columns = columns_for_width(ui.available_width(), geom).max(1);
        // Mount a loupe queued last frame before any input handling, so
        // its first drawn frame starts with fresh key state.
        if self.loupe.is_none() && !suspended {
            self.loupe = self.open_requested.take().map(|request| {
                if request.lightbox {
                    loupe::LoupeView::at_in_lightbox(request.index)
                } else {
                    loupe::LoupeView::at(request.index)
                }
            });
        }
        // Keyboard-first navigation. While the loupe is up it owns every
        // key except `F`; on the sheet, Esc leaves the folder and the
        // rest drives cursor + labeling (SPEC §6).
        //
        // Esc works on every mounted sheet state — including scan-error,
        // empty-folder and filtered-to-nothing, whose notices all promise
        // it — while the movement keys need tiles to move across.
        let escape = self.loupe.is_none()
            && !suspended
            && ui.ctx().input(|input| input.key_pressed(egui::Key::Escape));
        let sheet_keys = self.loupe.is_none() && !self.view.is_empty() && !suspended;
        let (enter, space, lightbox_open, nav) = if sheet_keys {
            ui.ctx().input(|input| {
                (
                    input.key_pressed(egui::Key::Enter),
                    input.key_pressed(egui::Key::Space),
                    input.key_pressed(egui::Key::L),
                    NAV_KEYS.into_iter().find(|key| input.key_pressed(*key)),
                )
            })
        } else {
            (false, false, false, None)
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
        // Export shortcut mirrors the bottom-right button; the scope is
        // computed at press time exactly like a click would compute it.
        let export_key = sheet_keys
            && ui
                .ctx()
                .input(|input| input.key_pressed(egui::Key::E) && input.modifiers.command_only());
        // The filter preset key works everywhere in the folder, loupe
        // included, so "show me only what I marked" is one press away.
        if !self.loading && !suspended && ui.ctx().input(|input| input.key_pressed(egui::Key::F)) {
            self.filter.cycle_preset();
            self.apply_order_change();
        }
        if escape {
            action = Some(Action::BackToHome);
        }
        if export_key && !self.is_exporting() {
            let files = self.export_set();
            if !files.is_empty() {
                action = Some(Action::Export {
                    root: self.root.clone(),
                    files,
                });
            }
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
            if let Some(direction) = widgets::pressed_rotate_key(ui.ctx()) {
                self.apply_rotation(db, direction);
            }
            if let Some(cursor) = self.cursor
                && let Some(next) =
                    nav.and_then(|key| stepped_cursor(cursor, key, columns, self.view.len()))
            {
                self.cursor = Some(next);
                self.scroll_target = Some(next);
            }
            // Full keyboard cull pass: Enter/Space jump into the loupe at
            // the cursor so digits keep flowing without touching the mouse;
            // L goes one step further and opens straight into the lightbox.
            if enter || space || lightbox_open {
                self.open_requested = Some(OpenRequest {
                    index: self.cursor.unwrap_or(0),
                    lightbox: lightbox_open,
                });
            }
        }

        let tally = self.tally();
        egui::Panel::top(egui::Id::new("cullr_grid_top_bar")).show(ui, |ui| {
            self.top_bar(ui, db, &tally, &mut action);
        });
        if self.show_filter_bar() {
            egui::Panel::top(egui::Id::new("cullr_filter_bar")).show(ui, |ui| {
                self.filter_bar(ui, &tally);
            });
        }
        egui::CentralPanel::default()
            .show(ui, |ui| self.body(ui, db, textures, columns, suspended));

        // Floating export control pinned to the sheet's bottom-right
        // corner; hidden in the states that have nothing to export
        // (scanning, failure, empty folder, loupe up).
        if !self.loading
            && self.error.is_none()
            && self.loupe.is_none()
            && !self.view.is_empty()
            && let Some(picked) = self.draw_export_control(ui.ctx())
        {
            action = Some(picked);
        }

        // A sort picked in the bar applies here, where the db handle is
        // at hand for the stamp cache; re-anchoring keeps the cursor on
        // its photo through the reorder.
        if let Some(sort) = self.pending_sort.take() {
            self.sort = sort;
            if sort == SortKey::TakenAt {
                self.refresh_taken_at(db);
            }
            self.apply_order_change();
        }

        action
    }

    /// Persistent top bar: back affordance, folder name, status stats —
    /// shown/total when filtered, ingest progress + rate while extracting,
    /// and a clickable error counter that jumps between failed tiles.
    fn top_bar(
        &mut self,
        ui: &mut egui::Ui,
        db: &cullr_core::Db,
        tally: &Tally,
        action: &mut Option<Action>,
    ) {
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
                // Live state of the cull-pass mode, clickable for mouse
                // users; the tooltip carries the shortcut since the bar
                // has no room for hints.
                let (text, color) = if self.auto_advance {
                    ("auto-advance on", theme::ACCENT)
                } else {
                    ("auto-advance off", theme::MUTED)
                };
                if ui
                    .button(egui::RichText::new(text).color(color))
                    .on_hover_text("Tab — after labeling, jump to the next photo")
                    .clicked()
                {
                    self.auto_advance = !self.auto_advance;
                    widgets::store_auto_advance(db, self.auto_advance);
                }
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

    /// Filter bar row: chips with live per-label counts on the left; the
    /// zoom slider (SPEC §6) and sort pill on the right. Clicks and drags
    /// refilter / resize / reorder immediately.
    ///
    /// Both halves share one horizontal row, exactly like the top bar:
    /// the right-aligned section must nest *inside* it. Placed directly
    /// in the panel's vertical flow instead, its `ui.separator()` would
    /// span the full available height — which for a content-sized panel
    /// is the panel's own last-frame height — ratcheting the bar a few
    /// pixels taller every frame and sliding the whole sheet down.
    fn filter_bar(&mut self, ui: &mut egui::Ui, tally: &Tally) {
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            let picked = widgets::filter_chips(ui, self.filter, &tally.counts);
            match picked {
                Some(FilterChip::All) => {
                    self.filter.clear();
                    self.apply_order_change();
                }
                Some(FilterChip::Label(label)) => {
                    self.filter.toggle(label);
                    self.apply_order_change();
                }
                None => {}
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // The order swap is queued, not applied in place: applying
                // it needs the db for the stamp cache, which only `ui`
                // holds.
                if let Some(sort) = widgets::sort_pill(ui, self.sort) {
                    self.pending_sort = Some(sort);
                }
                ui.separator();
                ui.spacing_mut().interact_size.y = CHIP_HEIGHT;
                let slider =
                    egui::Slider::new(&mut self.cell_width, CELL_MIN_WIDTH..=CELL_MAX_WIDTH)
                        .show_value(false);
                let cell = ui.add(slider);
                let resized = cell.changed();
                // `on_hover_text` consumes the response; read it first.
                cell.on_hover_text("Cell size — Ctrl+wheel over the sheet also zooms");
                if resized {
                    self.cell_width = CellGeom::new(self.cell_width).width;
                }
            });
        });
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

    /// Rebuilds the view after filter or sort state changed and re-anchors
    /// cursor and loupe onto their photos. Photos that survive stay under
    /// the user's hands at their new position — whether they moved because
    /// a chip hid their neighbors or because the order flipped (SPEC §6);
    /// a photo that fell out of the loupe's order closes it back to the
    /// sheet.
    ///
    /// Membership deliberately ignores later label edits: relabeling a
    /// tile never yanks it out from under the cursor mid-cull-pass — the
    /// next explicit refilter (chip click / `F`) refreshes the set.
    fn apply_order_change(&mut self) {
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
            // A change that brings tiles back arms the cursor so keys
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

    /// Recomputes the filtered, sorted view: one branchy pass over the
    /// rows collecting survivors, then an in-memory sort of those rows by
    /// the active [`SortKey`]. This is the single choke point for display
    /// order — arrows, shift-click ranges and loupe navigation all read
    /// `view`, so they follow the sort for free (SPEC §6).
    ///
    /// Sorts decorate-style, computing each key once: comparators must
    /// stay allocation-free to hold the refilter budget at 10k rows.
    fn rebuild_view(&mut self) {
        let filter = self.filter;
        let survivors: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| filter.matches(entry.label))
            .map(|(row, _)| row)
            .collect();
        match self.sort {
            SortKey::FileName => {
                // Case-insensitive so mixed-case card dumps interleave
                // naturally; the full rel_path breaks ties deterministically.
                let mut keyed: Vec<(String, usize)> = survivors
                    .into_iter()
                    .map(|row| (file_name(&self.entries[row]).to_lowercase(), row))
                    .collect();
                keyed.sort_by(|a, b| {
                    a.0.cmp(&b.0)
                        .then_with(|| self.entries[a.1].rel_path.cmp(&self.entries[b.1].rel_path))
                });
                self.view = keyed.into_iter().map(|(_, row)| row).collect();
            }
            SortKey::TakenAt => {
                // Stamps are ISO-shaped display strings (`2024-05-01
                // 12:33:44`), so lexicographic order is chronological.
                // Missing stamps rank last; filename breaks ties.
                let mut keyed: Vec<(bool, &str, usize)> = survivors
                    .into_iter()
                    .map(|row| {
                        let stamp = self
                            .taken_at
                            .get(&self.entries[row].id)
                            .and_then(|stamp| stamp.as_deref());
                        (stamp.is_none(), stamp.unwrap_or(""), row)
                    })
                    .collect();
                keyed.sort_by(|a, b| {
                    a.0.cmp(&b.0)
                        .then(a.1.cmp(b.1))
                        .then_with(|| self.entries[a.2].rel_path.cmp(&self.entries[b.2].rel_path))
                });
                self.view = keyed.into_iter().map(|(_, _, row)| row).collect();
            }
        }
    }

    /// Fetches capture-time stamps for every row missing from the cache,
    /// degrading to "unknown" on read errors so sorting still works. Only
    /// called when [`SortKey::TakenAt`] is active; point queries are
    /// microsecond-scale (same path as the loupe's EXIF bar).
    fn refresh_taken_at(&mut self, db: &cullr_core::Db) {
        for entry in &self.entries {
            if self.taken_at.contains_key(&entry.id) {
                continue;
            }
            let stamp = db
                .photo_detail(entry.id)
                .ok()
                .flatten()
                .and_then(|detail| detail.taken_at);
            self.taken_at.insert(entry.id, stamp);
        }
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

    /// Floating export control: a pill anchored to the viewport's bottom-
    /// right corner carrying the run progress / last-run note and the
    /// button itself. Returns the export action when clicked. The scope
    /// (selection first, filtered view otherwise) is resolved only on
    /// click so relabeling never costs anything per frame.
    fn draw_export_control(&self, ctx: &egui::Context) -> Option<Action> {
        let mut picked = None;
        egui::Area::new(egui::Id::new("cullr_export_control"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .corner_radius(10.0)
                    .stroke(egui::Stroke::new(1.0, theme::MUTED.gamma_multiply(0.35)))
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if let Some(note) = &self.export_note {
                                ui.label(egui::RichText::new(&note.text).color(note.color));
                            }
                            let exporting = self.is_exporting();
                            let label = if exporting {
                                format!(
                                    "Exporting {} / {}…",
                                    widgets::grouped(self.export_done.min(self.export_total)),
                                    widgets::grouped(self.export_total)
                                )
                            } else if self.selection.is_empty() {
                                format!("Export {}", widgets::grouped(self.view.len()))
                            } else {
                                format!("Export {}", widgets::grouped(self.selection.len()))
                            };
                            // Primary action of the sheet, styled like the
                            // Home screen's picker button.
                            let response = ui.add_enabled(
                                !exporting,
                                egui::Button::new(
                                    egui::RichText::new(label).size(15.0).color(theme::BG),
                                )
                                .min_size(egui::vec2(132.0, 32.0))
                                .fill(theme::ACCENT),
                            );
                            let hint = if exporting {
                                "Copying originals…"
                            } else if self.selection.is_empty() {
                                "Copy these photos' original files to another folder — Ctrl+E"
                            } else {
                                "Copy the selected photos' original files to another folder — Ctrl+E"
                            };
                            if response.on_hover_text(hint).clicked() {
                                let files = self.export_set();
                                if !files.is_empty() {
                                    picked = Some(Action::Export {
                                        root: self.root.clone(),
                                        files,
                                    });
                                }
                            }
                        });
                    });
            });
        picked
    }

    /// Body: loupe when open, otherwise scanning notice, failure notice,
    /// empty notices or tile sheet.
    fn body(
        &mut self,
        ui: &mut egui::Ui,
        db: &cullr_core::Db,
        textures: &mut Textures,
        columns: usize,
        suspended: bool,
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
                suspended,
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

        self.draw_sheet(ui, textures, columns, suspended);
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

    /// Rotates every selected photo a quarter turn (SPEC §6 keyboard map),
    /// or the cursor's photo when nothing is selected — mirroring how
    /// digits address the selection. Rows mirror instantly so tiles
    /// re-render this frame; persistence lands as one transaction. The
    /// texture cache re-decodes automatically because the requested
    /// rotation no longer matches each tile's resident slot.
    fn apply_rotation(&mut self, db: &cullr_core::Db, direction: widgets::RotateDir) {
        let targets: Vec<usize> = if self.selection.is_empty() {
            match self.cursor {
                Some(cursor) if self.view.get(cursor).is_some() => vec![self.view[cursor]],
                _ => return,
            }
        } else {
            (0..self.entries.len())
                .filter(|&row| self.selection.contains(&self.entries[row].id))
                .collect()
        };
        let updates: Vec<(cullr_core::PhotoId, u8)> = targets
            .iter()
            .filter_map(|&row| {
                let entry = self.entries.get_mut(row)?;
                entry.rot_cw = widgets::turned(entry.rot_cw, direction.delta());
                Some((entry.id, entry.rot_cw))
            })
            .collect();
        if !updates.is_empty()
            && let Err(error) = db.set_rotations(&updates)
        {
            tracing::warn!(%error, photos = updates.len(), "cannot persist rotation");
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
        geom: CellGeom,
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
            let on_tile = point_on_tile(origin, marquee.start, columns, self.view.len(), geom);
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
                covered_positions(origin, band, columns, self.view.len(), geom)
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
    /// Ctrl+wheel over the sheet resizes the cells (SPEC §6 zoom) instead
    /// of scrolling; plain wheel keeps its scroll behavior.
    ///
    /// The visible id set feeds the texture manager's focus (scroll-driven
    /// decode cancellation) and a band of neighbouring rows is prefetched,
    /// both after drawing so they see the settled viewport.
    fn draw_sheet(
        &mut self,
        ui: &mut egui::Ui,
        textures: &mut Textures,
        columns: usize,
        suspended: bool,
    ) {
        let geom = CellGeom::new(self.cell_width);
        ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
        let total_rows = self.view.len().div_ceil(columns);
        // Zoom takes the wheel before the scroll area sees it: with Ctrl
        // held and the pointer over the sheet, the delta drives the cell
        // size and is zeroed out so the sheet does not also scroll.
        if !suspended {
            let (wheel_zooming, scroll_y, pointer) = ui.input(|input| {
                (
                    input.modifiers.command,
                    input.smooth_scroll_delta.y,
                    input.pointer.hover_pos(),
                )
            });
            if wheel_zooming
                && scroll_y != 0.0
                && pointer.is_some_and(|pointer| ui.max_rect().contains(pointer))
            {
                self.cell_width = zoomed_cell_width(self.cell_width, scroll_y);
                ui.input_mut(|input| input.smooth_scroll_delta = egui::Vec2::ZERO);
            }
        }
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
        // of the target row is computed and applied exactly once. Only
        // rows outside the last settled viewport move the sheet, and
        // then just enough to come fully into view — re-centering on
        // every step would yank the sheet around and make tiles flicker
        // whenever an arrow walks along already-visible cells. The salt
        // keeps this sheet's persisted state separate from every other
        // ScrollArea in the app (they share egui's default id).
        let mut scroll = egui::ScrollArea::vertical()
            .auto_shrink(false)
            .id_salt("cullr_contact_sheet");
        if let Some(position) = self.scroll_target.take() {
            let content_height = total_rows.max(1) as f32 * geom.stride_y() + GAP;
            let viewport = ui.available_height();
            // The queue holds a filtered-order position; scrolling works
            // in whole display rows.
            let row = position / columns.max(1);
            let row_top = row as f32 * geom.stride_y();
            let row_bottom = row_top + geom.height;
            let view_top = self.settled_scroll_y;
            let revealed = if row_top < view_top {
                // Above the viewport: pull up to its top edge.
                row_top - GAP
            } else if row_bottom > view_top + viewport {
                // Below the viewport: push down until its bottom edge
                // clears the fold; a row taller than the viewport itself
                // can only ever show its top.
                if geom.height > viewport {
                    row_top - GAP
                } else {
                    row_bottom + GAP - viewport
                }
            } else {
                // Fully visible already: hands off the sheet.
                view_top
            };
            scroll = scroll.vertical_scroll_offset(revealed.clamp(0.0, content_height));
        } else if self.reset_scroll {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        self.reset_scroll = false;
        let output = scroll.show_rows(ui, geom.height, total_rows, |ui, rows| {
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
                            geom,
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
        self.settled_scroll_y = output.state.offset.y;
        self.settled_viewport_top = viewport.top();

        // Row-major traversal is already sorted; `focus` relies on that.
        visible_keys.sort_unstable();
        textures.focus(&visible_keys);
        self.prefetch_band(textures, rows_shown, columns, total_rows);

        // Rubber band first so a finished drag can swallow the release
        // that would otherwise land on a tile as a click (SPEC §6).
        let origin = first_tile.map(|(position, rect)| tile_origin(position, rect, columns, geom));
        let release_consumed = if suspended {
            false
        } else {
            self.step_marquee(ui, origin, columns, viewport, geom)
        };
        self.paint_marquee(ui);

        // Mouse path to the loupe is a double-click now that plain click
        // selects; it mounts next frame like keyboard opens so the
        // second click never leaks into the preview (SPEC §6). Under a
        // modal the clicks belong to its layer anyway; the guard makes
        // that independent of egui's hit-testing.
        if !release_consumed && !suspended {
            if let Some(position) = double_clicked {
                self.cursor = Some(position);
                self.anchor = Some(position);
                self.selection = self.id_at(position).into_iter().collect();
                self.open_requested = Some(OpenRequest {
                    index: position,
                    lightbox: false,
                });
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
            .map(|entry| {
                (
                    TexKey::thumb(entry.id),
                    entry
                        .thumb_path
                        .as_deref()
                        .map(|path| (path, row_rotation(entry))),
                )
            });
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
    geom: CellGeom,
    is_cursor: bool,
    is_selected: bool,
    visible_pending: &mut Vec<PhotoId>,
) -> CellHit {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(geom.width, geom.height), egui::Sense::click());
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

    let mut tooltip = entry.rel_path.display().to_string();
    if let Some(jpeg) = &entry.jpeg_rel_path {
        tooltip.push_str(&format!("\n+ {}", jpeg.display()));
    }
    if let Some(message) = &entry.err_msg {
        tooltip.push_str(&format!("\n⚠ {message}"));
    }
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
            match textures.handle(
                TexKey::thumb(entry.id),
                entry.thumb_path.as_deref(),
                row_rotation(entry),
            ) {
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

/// Bottom strip: color-label dot plus truncated filename on the top line,
/// and — for RAW+JPEG pairs (SPEC §5.1) — a muted tag line underneath.
/// The name dims while the photo is still in flight and flags extraction
/// failures.
fn draw_strip(painter: &egui::Painter, cell: egui::Rect, entry: &PhotoEntry) {
    let strip = egui::Rect::from_min_max(
        egui::pos2(cell.left(), cell.bottom() - STRIP_HEIGHT),
        cell.right_bottom(),
    );
    painter.rect_filled(strip, 6.0, theme::BG);

    let name_y = strip.top() + NAME_LINE_CENTER;
    if entry.label != cullr_core::Label::None {
        painter.circle_filled(
            egui::pos2(strip.left() + 10.0, name_y),
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
        egui::pos2(name_x, name_y),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(11.0),
        color,
    );

    if entry.jpeg_rel_path.is_some() {
        painter.text(
            egui::pos2(strip.left() + 8.0, strip.bottom() - TAG_LINE_BOTTOM),
            egui::Align2::LEFT_CENTER,
            "RAW+JPEG",
            egui::FontId::proportional(9.0),
            theme::MUTED,
        );
    }
}

/// Number of whole cells that fit in `width`, accounting for gaps.
fn columns_for_width(width: f32, geom: CellGeom) -> usize {
    ((width + GAP) / geom.stride_x()).floor().max(0.0) as usize
}

/// Cell long edge after one Ctrl+wheel step (SPEC §6 zoom): the scroll
/// delta scales the width linearly, clamped to the 128–1024 range. Pure
/// so the wheel gain is unit-testable.
pub(crate) fn zoomed_cell_width(current: f32, scroll_y: f32) -> f32 {
    (current + scroll_y * ZOOM_WHEEL_GAIN).clamp(CELL_MIN_WIDTH, CELL_MAX_WIDTH)
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
fn tile_origin(position: usize, rect: egui::Rect, columns: usize, geom: CellGeom) -> egui::Pos2 {
    let row = (position / columns.max(1)) as f32;
    let column = (position % columns.max(1)) as f32;
    egui::pos2(
        rect.left() - column * geom.stride_x(),
        rect.top() - row * geom.stride_y(),
    )
}

/// Rect of the tile at `position` in unbounded sheet geometry.
fn tile_rect(origin: egui::Pos2, position: usize, columns: usize, geom: CellGeom) -> egui::Rect {
    let row = (position / columns.max(1)) as f32;
    let column = (position % columns.max(1)) as f32;
    egui::Rect::from_min_size(
        egui::pos2(
            origin.x + column * geom.stride_x(),
            origin.y + row * geom.stride_y(),
        ),
        egui::vec2(geom.width, geom.height),
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
    geom: CellGeom,
) -> Option<usize> {
    let column = ((point.x - origin.x) / geom.stride_x()).floor();
    let row = ((point.y - origin.y) / geom.stride_y()).floor();
    if len == 0 || column < 0.0 || row < 0.0 {
        return None;
    }
    let position = row as usize * columns.max(1) + column as usize;
    (position < len).then_some(position)
}

/// `true` when the point sits inside some tile's rectangle; gaps between
/// cells and the space below the last row count as bare sheet, which is
/// what makes click-on-empty clear the selection (SPEC §6).
fn point_on_tile(
    origin: egui::Pos2,
    point: egui::Pos2,
    columns: usize,
    len: usize,
    geom: CellGeom,
) -> bool {
    position_at_point(origin, point, columns, len, geom)
        .is_some_and(|position| tile_rect(origin, position, columns, geom).contains(point))
}

/// Every tile position whose rectangle intersects `band`, clamped to the
/// live set; drives rubber-band selection (SPEC §6 marquee). Row-major
/// and ascending, so callers may rely on order.
fn covered_positions(
    origin: egui::Pos2,
    band: egui::Rect,
    columns: usize,
    len: usize,
    geom: CellGeom,
) -> impl Iterator<Item = usize> {
    let columns = columns.max(1);
    let step_x = geom.stride_x();
    let step_y = geom.stride_y();
    // A tile counts when its rect merely touches the band, hence the
    // size offsets on the leading edges.
    let first_column = (((band.left() - origin.x - geom.width) / step_x)
        .ceil()
        .max(0.0)) as usize;
    // Inclusive bounds clamped to the sheet's own shape: a wider band
    // would only wrap columns into duplicate positions.
    let last_column = ((((band.right() - origin.x) / step_x).floor().max(0.0)) as usize)
        .min(first_column + columns - 1);
    let first_row = (((band.top() - origin.y - geom.height) / step_y)
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
        let geom = CellGeom::default();
        // Two cells plus one gap need exactly 232 * 2 + 8 = 472 px.
        assert_eq!(columns_for_width(472.0, geom), 2);
        // One pixel short must not promise a second column.
        assert_eq!(columns_for_width(471.0, geom), 1);
        // A third cell would need 712 px.
        assert_eq!(columns_for_width(711.0, geom), 2);
    }

    #[test]
    fn columns_for_width_should_report_zero_for_zero_width() {
        // Callers clamp to one column; the pure function stays honest.
        assert_eq!(columns_for_width(0.0, CellGeom::default()), 0);
    }

    #[test]
    fn columns_for_width_should_follow_the_zoomed_cell_size() {
        // Half-size tiles must fit strictly more columns into the same
        // viewport; exact doubling does not survive column flooring.
        let geom = CellGeom::new(CELL_DEFAULT_WIDTH / 2.0);

        assert!(
            columns_for_width(472.0, geom) > columns_for_width(472.0, CellGeom::default()),
            "smaller cells mean a denser sheet"
        );
    }

    #[test]
    fn cell_geom_should_clamp_width_to_the_zoom_range() {
        assert_eq!(CellGeom::new(10.0).width, CELL_MIN_WIDTH);
        assert_eq!(CellGeom::new(f32::MAX).width, CELL_MAX_WIDTH);
        assert_eq!(CellGeom::default().width, CELL_DEFAULT_WIDTH);
    }

    #[test]
    fn zoomed_cell_width_should_scale_with_wheel_delta_and_clamp() {
        assert_eq!(
            zoomed_cell_width(CELL_DEFAULT_WIDTH, -100.0),
            CELL_DEFAULT_WIDTH - 50.0
        );
        assert_eq!(
            zoomed_cell_width(CELL_DEFAULT_WIDTH, 40.0),
            CELL_DEFAULT_WIDTH + 20.0
        );
        assert_eq!(zoomed_cell_width(CELL_MIN_WIDTH, -10_000.0), CELL_MIN_WIDTH);
        assert_eq!(zoomed_cell_width(CELL_MAX_WIDTH, 10_000.0), CELL_MAX_WIDTH);
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
            rot_cw: 0,
            thumb_path: None,
            err_msg: None,
            jpeg_rel_path: None,
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
        grid.apply_order_change();

        assert!(grid.view.is_empty());
        assert_eq!(grid.cursor, None);

        grid.filter = LabelFilter::all();
        grid.apply_order_change();

        assert_eq!(grid.view.len(), 1);
        assert_eq!(grid.cursor, Some(0));
    }

    #[test]
    fn refilter_should_narrow_the_view_to_matching_labels_in_order() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red, TestLabel::None]);

        grid.filter.toggle(TestLabel::Red);
        grid.apply_order_change();

        assert_eq!(grid.view, vec![1]);
        grid.filter.clear();
        grid.apply_order_change();

        assert_eq!(grid.view, vec![0, 1, 2]);
    }

    #[test]
    fn refilter_should_reanchor_cursor_onto_its_photo() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red]);
        grid.cursor = Some(1);
        grid.filter.toggle(TestLabel::Red);
        grid.apply_order_change();

        // The red photo moved from view slot 1 to slot 0 but keeps the
        // cursor; digits keep landing on the same tile.
        assert_eq!(grid.cursor, Some(0));
    }

    #[test]
    fn refilter_should_park_cursor_nearby_when_its_photo_is_filtered_out() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None, TestLabel::None]);
        grid.cursor = Some(2);
        grid.filter.toggle(TestLabel::Red);
        grid.apply_order_change();

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
        grid.apply_order_change();

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
                jpeg_rel_path: None,
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
        let geom = CellGeom::default();

        assert_eq!(
            position_at_point(origin, egui::pos2(100.0, 50.0), columns, 7, geom),
            Some(0),
            "the sheet origin is tile zero's corner"
        );
        assert_eq!(
            position_at_point(
                origin,
                egui::pos2(100.0 + geom.width + GAP + 1.0, 60.0),
                columns,
                7,
                geom
            ),
            Some(1)
        );
        assert_eq!(
            position_at_point(
                origin,
                egui::pos2(110.0, 50.0 + geom.height + GAP + 1.0),
                columns,
                7,
                geom
            ),
            Some(3),
            "one row down steps by the column count"
        );
        assert_eq!(
            position_at_point(
                origin,
                egui::pos2(100.0 + 3.0 * (geom.width + GAP) + 5.0, 60.0),
                columns,
                2,
                geom
            ),
            None,
            "past the last tile there is nothing to address"
        );
        assert_eq!(
            position_at_point(origin, egui::pos2(50.0, 60.0), columns, 7, geom),
            None
        );
        assert_eq!(
            position_at_point(origin, egui::pos2(120.0, 10.0), columns, 7, geom),
            None
        );
    }

    #[test]
    fn point_on_tile_should_exclude_gaps_and_trailing_space() {
        let columns = 2;
        let origin = egui::Pos2::ZERO;
        let geom = CellGeom::default();

        assert!(point_on_tile(
            origin,
            egui::pos2(geom.width - 1.0, geom.height - 1.0),
            columns,
            4,
            geom
        ));
        assert!(
            !point_on_tile(
                origin,
                egui::pos2(geom.width + GAP / 2.0, 5.0),
                columns,
                4,
                geom
            ),
            "the gap between cells is bare sheet"
        );
        // One row down with only one row of tiles: bare space.
        assert!(!point_on_tile(
            origin,
            egui::pos2(5.0, geom.height + GAP + 1.0),
            columns,
            2,
            geom
        ));
    }

    #[test]
    fn covered_positions_should_take_every_intersecting_tile() {
        let columns = 3;
        let origin = egui::Pos2::ZERO;
        let geom = CellGeom::default();
        // A small band straddling the corner shared by tiles 0, 1, 3, 4.
        let band = egui::Rect::from_min_max(
            egui::pos2(geom.width - 2.0, geom.height - 2.0),
            egui::pos2(geom.width + GAP + 2.0, geom.height + GAP + 2.0),
        );

        let covered: Vec<usize> = covered_positions(origin, band, columns, 9, geom).collect();

        assert_eq!(covered, vec![0, 1, 3, 4]);
    }

    #[test]
    fn covered_positions_should_clamp_to_the_live_set() {
        let columns = 3;
        let origin = egui::Pos2::ZERO;
        let band = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(10_000.0, 10_000.0));

        assert_eq!(
            covered_positions(origin, band, columns, 7, CellGeom::default()).count(),
            7
        );
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
        grid.apply_order_change();

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

        grid.apply_order_change();

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

    // --- export (bottom-right control, Ctrl+E) ---

    use cullr_core::ExportReport;

    #[test]
    fn export_set_should_prefer_the_selection_over_the_filtered_view() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None, TestLabel::None]);
        // Selection of the first and third photo.
        grid.selection.insert(PhotoId(1));
        grid.selection.insert(PhotoId(3));

        let files = grid.export_set();

        assert_eq!(
            files,
            vec![
                PathBuf::from("/photos/IMG_0001.CR3"),
                PathBuf::from("/photos/IMG_0003.CR3"),
            ]
        );
    }

    #[test]
    fn export_set_should_fall_back_to_the_whole_filtered_view() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::Red]);
        grid.filter.toggle(TestLabel::Red);
        grid.apply_order_change();
        assert!(grid.selection.is_empty());

        let files = grid.export_set();

        assert_eq!(files, vec![PathBuf::from("/photos/IMG_0002.CR3")]);
    }

    #[test]
    fn export_set_should_export_only_tiles_still_visible() {
        // A refilter keeps selection membership of hidden photos for
        // labeling continuity, but a batch export must never copy tiles
        // the sheet no longer shows.
        let mut grid = grid_with(&[TestLabel::Red, TestLabel::None]);
        grid.select_all();
        grid.filter.toggle(TestLabel::None);
        grid.apply_order_change();

        let files = grid.export_set();

        assert_eq!(files, vec![PathBuf::from("/photos/IMG_0002.CR3")]);
    }

    #[test]
    fn export_set_should_copy_the_companion_jpeg_right_after_its_raw() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None]);
        grid.entries[0].jpeg_rel_path = Some("IMG_0001.JPG".into());

        let files = grid.export_set();

        assert_eq!(
            files,
            vec![
                PathBuf::from("/photos/IMG_0001.CR3"),
                PathBuf::from("/photos/IMG_0001.JPG"),
                PathBuf::from("/photos/IMG_0002.CR3"),
            ]
        );
    }

    #[test]
    fn export_run_state_should_track_progress_and_summaries() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None]);

        grid.begin_export(2);
        assert!(grid.is_exporting());

        grid.apply_export_progress(1);
        grid.apply_export_progress(0);
        assert_eq!(grid.export_done, 1, "progress must never rewind");

        grid.finish_export(&Ok(ExportReport {
            copied: 2,
            failures: Vec::new(),
            cancelled: false,
        }));
        assert!(!grid.is_exporting());
        let note = grid.export_note.as_ref().expect("note set");
        assert_eq!(note.text, "✓ Exported 2");

        grid.begin_export(3);
        grid.finish_export(&Ok(ExportReport {
            copied: 1,
            failures: vec![cullr_core::ExportFailure {
                source: "/photos/a.nef".into(),
                reason: "vanished".to_owned(),
            }],
            cancelled: false,
        }));
        let note = grid.export_note.as_ref().expect("note set");
        assert!(note.text.contains("failed"), "{}", note.text);

        grid.finish_export(&Ok(ExportReport {
            copied: 0,
            failures: Vec::new(),
            cancelled: true,
        }));
        assert_eq!(
            grid.export_note.as_ref().map(|note| note.text.as_str()),
            Some("Export cancelled")
        );
    }

    #[test]
    fn finish_export_should_report_a_failed_job_and_nothing_to_copy() {
        let mut grid = grid_with(&[TestLabel::None]);

        grid.finish_export(&Err("destination rejected".to_owned()));
        let note = grid.export_note.as_ref().expect("note set");
        assert!(note.text.contains("destination rejected"), "{}", note.text);

        // Destination equal to the source folder copies nothing: say so
        // instead of claiming a successful export of zero files.
        grid.finish_export(&Ok(ExportReport::default()));
        assert_eq!(
            grid.export_note.as_ref().map(|note| note.text.as_str()),
            Some("Nothing to copy")
        );
    }

    #[test]
    fn apply_scan_should_retire_finished_export_state() {
        let mut grid = grid_with(&[TestLabel::None]);
        grid.begin_export(4);
        grid.apply_export_progress(2);

        let fresh = (0..2)
            .map(|index| entry_at(index as u64 + 10, TestLabel::None))
            .collect();
        grid.apply_scan(Ok(fresh));

        assert!(!grid.is_exporting());
        assert_eq!(grid.export_note, None);
        assert!(grid.is_browsing(std::path::Path::new("/photos")));
        assert!(!grid.is_browsing(std::path::Path::new("/other")));
    }

    // --- manual rotation (SPEC §6 keyboard map) ---

    #[test]
    fn rotation_should_fall_back_to_the_cursor_photo_and_persist() {
        let (_dir, db) = db_for_tests();
        let mut grid = grid_with_db(&[TestLabel::None, TestLabel::None], &db);
        grid.cursor = Some(0);

        grid.apply_rotation(&db, widgets::RotateDir::Clockwise);
        grid.apply_rotation(&db, widgets::RotateDir::CounterClockwise);

        assert_eq!(grid.entries[0].rot_cw, 0, "CW then CCW returns upright");
        assert_eq!(grid.entries[1].rot_cw, 0);
        let stored = db
            .photo_entry(grid.entries[0].id)
            .expect("read")
            .expect("row");
        assert_eq!(stored.rot_cw, 0);
    }

    #[test]
    fn rotation_should_batch_turn_every_selected_photo() {
        let (_dir, db) = db_for_tests();
        let mut grid = grid_with_db(&[TestLabel::None, TestLabel::None], &db);
        grid.select_all();
        grid.entries[1].rot_cw = 3;

        grid.apply_rotation(&db, widgets::RotateDir::Clockwise);

        // Each photo advances from its own current turn count.
        assert_eq!(grid.entries[0].rot_cw, 1);
        assert_eq!(grid.entries[1].rot_cw, 0);
        let stored = db
            .photo_entry(grid.entries[1].id)
            .expect("read")
            .expect("row");
        assert_eq!(stored.rot_cw, 0);
    }

    #[test]
    fn stats_line_should_count_the_selection_while_present() {
        let mut grid = grid_with(&[TestLabel::None, TestLabel::None]);

        assert_eq!(grid.stats_line(), "2 photos");
        grid.select_all();

        assert_eq!(grid.stats_line(), "2 photos · 2 selected");
    }

    // --- sorting (SPEC §10 T14) ---

    /// Grid whose rows sit in deliberately shuffled scan order so both
    /// sort keys have something to reorder; names follow ids, so the
    /// display order under filename sort is exactly id order.
    fn grid_shuffled() -> GridView {
        let mut grid = grid_with(&[
            TestLabel::None, // IMG_0001
            TestLabel::Red,  // IMG_0002
            TestLabel::None, // IMG_0003
            TestLabel::None, // IMG_0004
            TestLabel::None, // IMG_0005
        ]);
        let shuffled: Vec<PhotoEntry> = [4_usize, 0, 3, 1, 2]
            .iter()
            .map(|&row| grid.entries[row].clone())
            .collect();
        grid.entries = shuffled;
        for (index, entry) in grid.entries.iter().enumerate() {
            grid.index.insert(entry.id, index);
        }
        grid.apply_order_change();
        grid
    }

    /// Capture-time stamps by photo id, as `refresh_taken_at` would cache.
    fn stamp(grid: &mut GridView, id: u64, taken_at: &str) {
        grid.taken_at
            .insert(cullr_core::PhotoId(id), Some(taken_at.to_owned()));
    }

    #[test]
    fn filename_sort_should_order_rows_by_name_ignoring_scan_order() {
        let mut grid = grid_shuffled();
        grid.sort = SortKey::FileName;

        grid.apply_order_change();

        // Shuffled scan order maps names to rows as
        //   IMG_0001→1 · IMG_0002→3 · IMG_0003→4 · IMG_0004→2 · IMG_0005→0,
        // so name order visits exactly those rows.
        assert_eq!(grid.view, vec![1, 3, 4, 2, 0]);
    }

    #[test]
    fn taken_at_sort_should_order_chronologically_and_put_unknowns_last() {
        let mut grid = grid_shuffled();
        grid.sort = SortKey::TakenAt;
        stamp(&mut grid, 1, "2024-05-01 10:00:00");
        stamp(&mut grid, 2, "2024-05-01 08:00:00");
        stamp(&mut grid, 4, "2024-05-01 09:00:00");
        // Ids 3 and 5 carry no stamp and must land after all stamped ones.

        grid.apply_order_change();

        // Chronological: 08:00 (row 3) → 09:00 (row 2) → 10:00 (row 1);
        // unknowns trail in filename order (rows 4, 0).
        assert_eq!(grid.view, vec![3, 2, 1, 4, 0]);
    }

    #[test]
    fn resort_should_reanchor_the_cursor_onto_its_photo() {
        let mut grid = grid_shuffled();
        grid.apply_order_change(); // FileName order: cursor on first tile
        let cursor_photo = grid.id_at(grid.cursor.expect("armed")).expect("row");
        grid.sort = SortKey::TakenAt;
        stamp(&mut grid, 5, "2020-01-01 00:00:00"); // earliest overall

        grid.apply_order_change();

        let new_position = grid.cursor.expect("cursor survives a re-sort");
        assert_eq!(
            grid.id_at(new_position),
            Some(cursor_photo),
            "digits must keep hitting the same photo"
        );
    }

    #[test]
    fn resort_should_move_the_loupe_with_its_photo() {
        let mut grid = grid_shuffled();
        grid.sort = SortKey::TakenAt;
        stamp(&mut grid, 2, "2019-01-01 00:00:00"); // id 2 sorts first
        grid.apply_order_change();
        grid.loupe = Some(loupe::LoupeView::at(0));

        // Switching back to names moves id 2 away from slot 0…
        grid.sort = SortKey::FileName;

        grid.apply_order_change();

        let loupe_position = grid.loupe.as_ref().expect("kept open").index();
        assert_ne!(
            loupe_position, 0,
            "the loupe follows the photo it was showing"
        );
        assert_eq!(
            grid.view[loupe_position], 3,
            "…and lands on id 2's new slot (row 3)"
        );
    }

    // --- headless scroll harness (launch + wheel behavior, SPEC §6) ---

    const SHEET_FRAMES: f64 = 1.0 / 60.0;

    /// Drives [`GridView`] through real egui passes with no renderer so
    /// scroll behavior can be asserted end to end: wheel events in,
    /// settled sheet offset and viewport position out.
    struct Bench {
        ctx: egui::Context,
        db: Db,
        _dir: TempDir,
        textures: Textures,
        time: f64,
    }

    struct Frame {
        /// Sheet scroll offset after the frame settled.
        offset_y: f32,
        /// Screen-space top of the sheet viewport after the frame.
        viewport_top: f32,
    }

    impl Bench {
        fn new() -> Self {
            let (_dir, db) = db_for_tests();
            Self {
                ctx: egui::Context::default(),
                db,
                _dir,
                textures: Textures::new(),
                time: 0.0,
            }
        }

        fn step(&mut self, grid: &mut GridView, events: &[egui::Event]) -> Frame {
            self.time += SHEET_FRAMES;
            let ctx = &self.ctx;
            let db = &self.db;
            let textures = &mut self.textures;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                time: Some(self.time),
                events: events.to_vec(),
                ..Default::default()
            };
            let mut full = ctx.run_ui(raw, |ui| {
                textures.sync(ui.ctx());
                grid.ui(ui, db, textures, false);
            });
            // Headless run: there is no renderer to upload deltas to.
            full.textures_delta.clear();
            Frame {
                offset_y: grid.settled_scroll_y,
                viewport_top: grid.settled_viewport_top,
            }
        }

        /// `count` frames with no input at all.
        fn idle(&mut self, grid: &mut GridView, count: usize) -> Vec<Frame> {
            (0..count).map(|_| self.step(grid, &[])).collect()
        }
    }

    /// One mouse-wheel notch upward (toward the sheet's top), as winit
    /// reports a LineDelta notch on Linux.
    fn wheel_up() -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// One mouse-wheel notch downward.
    fn wheel_down() -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn pointer_over_sheet() -> egui::Event {
        egui::Event::PointerMoved(egui::pos2(640.0, 400.0))
    }

    fn scanned_grid(count: usize, first_id: u64) -> GridView {
        let entries: Vec<PhotoEntry> = (0..count)
            .map(|index| entry_at(first_id + index as u64, TestLabel::None))
            .collect();
        let mut grid = GridView::new("/photos".into(), false);
        grid.apply_scan(Ok(entries));
        grid
    }

    /// The reported launch bug: the sheet slid down a little on open and
    /// could never be scrolled back — the filter bar's right-aligned
    /// separator claimed the panel's own height each frame, growing the
    /// bar ~360 px/s until the sheet slid out from under the pointer and
    /// blank space filled the view.
    #[test]
    fn sheet_should_hold_its_position_across_idle_frames_and_wheel_up() {
        let mut bench = Bench::new();
        let mut grid = scanned_grid(2000, 1);

        bench.step(&mut grid, &[pointer_over_sheet()]);
        let frames = bench.idle(&mut grid, 120);

        for frame in &frames {
            assert_eq!(frame.offset_y, 0.0, "an idle sheet must not move");
            assert!(
                frame.viewport_top < 200.0,
                "the bars above the sheet must not grow: top {}",
                frame.viewport_top
            );
        }
        assert_eq!(
            frames.last().expect("idle frames").viewport_top,
            frames.first().expect("idle frames").viewport_top,
            "the viewport top must be stable, not creeping"
        );

        // Wheel up hard at the top: nothing to see above row zero, but it
        // must never push content down either.
        for _ in 0..30 {
            let frame = bench.step(&mut grid, &[wheel_up()]);
            assert_eq!(frame.offset_y, 0.0);
        }
    }

    #[test]
    fn wheel_scrolling_should_move_the_sheet_both_ways() {
        let mut bench = Bench::new();
        let mut grid = scanned_grid(2000, 1);
        bench.step(&mut grid, &[pointer_over_sheet()]);
        bench.idle(&mut grid, 5);

        for _ in 0..20 {
            bench.step(&mut grid, &[wheel_down()]);
        }
        let descended = bench.step(&mut grid, &[]);
        assert!(
            descended.offset_y > 300.0,
            "wheel-down must descend, got {}",
            descended.offset_y
        );

        // Wheel-up must climb back toward the top, not stall or invert.
        // The wheel notch is smoothed over later frames, so let the
        // downward residue drain before judging the upward strokes.
        bench.idle(&mut grid, 45);
        let mut last = bench.step(&mut grid, &[]).offset_y;
        for _ in 0..60 {
            let frame = bench.step(&mut grid, &[wheel_up()]);
            assert!(
                frame.offset_y < last + f32::EPSILON,
                "wheel-up went down: {} -> {}",
                last,
                frame.offset_y
            );
            last = frame.offset_y;
        }
        assert_eq!(
            bench.step(&mut grid, &[]).offset_y,
            0.0,
            "enough wheel-up must reach the top"
        );
    }

    /// egui persists ScrollArea offsets across folders and sessions; a
    /// freshly opened folder must mount at its top regardless of where
    /// the previous one was left.
    #[test]
    fn reopening_a_folder_should_mount_the_sheet_at_the_top() {
        let mut bench = Bench::new();
        let mut big = scanned_grid(2000, 1);
        bench.step(&mut big, &[pointer_over_sheet()]);
        for _ in 0..25 {
            bench.step(&mut big, &[wheel_down()]);
        }
        let deep = bench.step(&mut big, &[]);
        assert!(deep.offset_y > 100.0, "precondition: scrolled down");

        // Real folder openings take a moment; that drains the wheel
        // smoothing tail before the next sheet mounts.
        bench.idle(&mut big, 45);

        // A different folder opens (fresh GridView, same persistent egui
        // state, exactly like a restart or a folder switch).
        let mut small = scanned_grid(24, 10_000);
        bench.step(&mut small, &[pointer_over_sheet()]);
        let mounted = bench.step(&mut small, &[]);
        assert_eq!(
            mounted.offset_y, 0.0,
            "a fresh folder must not inherit the old scroll"
        );
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// The reported bug: arrowing between tiles re-centered the sheet on
    /// every step, so the whole view visibly jumped and freshly exposed
    /// rows streamed in — while clicking tiles left it alone. A cursor
    /// move within the visible rows must not move the sheet at all.
    #[test]
    fn arrows_across_visible_tiles_should_not_scroll_the_sheet() {
        let mut bench = Bench::new();
        let mut grid = scanned_grid(2000, 1);
        bench.step(&mut grid, &[pointer_over_sheet()]);
        bench.idle(&mut grid, 5);

        // Five columns at this window size: right walks one tile, down
        // one row. All land inside the initially visible rows.
        for _ in 0..3 {
            bench.step(&mut grid, &[key(egui::Key::ArrowRight)]);
        }
        for _ in 0..2 {
            bench.step(&mut grid, &[key(egui::Key::ArrowDown)]);
        }
        let frames = bench.idle(&mut grid, 10);

        for frame in frames {
            assert_eq!(
                frame.offset_y, 0.0,
                "an arrow move onto an already-visible row moved the sheet"
            );
        }
    }

    /// Arrowing past the bottom edge must reveal the new row with the
    /// smallest possible offset change (not a re-center), and stepping
    /// back up into fully visible rows must not scroll again.
    #[test]
    fn arrows_below_the_fold_should_reveal_the_new_row_minimally() {
        let mut bench = Bench::new();
        let mut grid = scanned_grid(2000, 1);
        let geom = CellGeom::default();
        bench.step(&mut grid, &[pointer_over_sheet()]);
        bench.idle(&mut grid, 5);

        // Walk down until the first press scrolls; five columns make
        // each ArrowDown one display row.
        let mut downs = 0_usize;
        let mut offset = 0.0_f32;
        while offset <= 0.0 {
            downs += 1;
            assert!(downs < 20, "arrowing down never scrolled the sheet");
            offset = bench.step(&mut grid, &[key(egui::Key::ArrowDown)]).offset_y;
        }

        // The minimal reveal pins the new row's bottom edge just inside
        // the viewport, so the settled viewport height is derivable from
        // that first adjustment alone.
        let row = downs;
        let viewport = row as f32 * geom.stride_y() + geom.height + GAP - offset;
        assert!(
            viewport > 0.0 && viewport < 800.0,
            "nonsensical viewport {viewport}"
        );

        // One further row below the fold: exactly the same viewport
        // worth of deficit again — not a jump to screen center.
        let expected = (row + 1) as f32 * geom.stride_y() + geom.height + GAP - viewport;
        let next = bench.step(&mut grid, &[key(egui::Key::ArrowDown)]).offset_y;
        assert!(
            (next - expected).abs() < 1.0,
            "expected minimal reveal {expected}, got {next}"
        );

        // Stepping back up lands on a fully visible row: no movement.
        let up = bench.step(&mut grid, &[key(egui::Key::ArrowUp)]).offset_y;
        assert_eq!(
            up, next,
            "an upward step onto a visible row moved the sheet"
        );
    }
}
