//! Loupe view: full-screen preview with fit/zoom/pan (SPEC §6).
//!
//! The photo renders aspect-fit inside the viewport; the wheel zooms
//! toward the cursor between fit (×1) and pixel parity (100%), dragging
//! pans while clamped so the image can never be flung off-screen, and
//! `Space` toggles the two extremes. Photoshop-style zoom aids layer on
//! top: `Ctrl+0` / `Ctrl+1` jump to fit / 100%, `Ctrl+=` / `Ctrl+-`
//! step multiplicatively, double-click toggles the extremes under the
//! cursor, and Shift+drag draws a marquee that zooms so the selected
//! region fills the viewport. A bottom-left pill shows the current
//! percentage of native resolution, with click steps and scrubby drag.
//! Arrows walk the folder order, `Esc`/`Enter` return to the sheet.
//! While the screen-size texture decodes, a shimmer placeholder keeps
//! the frame alive; neighbours ±3 positions prefetch their previews in
//! the background (SPEC §5.3).
//!
//! The lightbox (`L`, also straight from the sheet) hides every piece
//! of chrome — EXIF bar, pills, palette — and floats the photo alone on
//! black for an unobstructed look; `W` flips that backdrop to pure
//! white for judging high-key frames. Zooming, panning, navigation and
//! labeling keep working inside it; the same keys restore the chrome
//! before a second press leaves the loupe entirely.
//!
//! Overlays: a top-right pill with the color label and position counter
//! (`142 / 3 210`), a bottom EXIF summary bar and the zoom indicator.

use eframe::egui;

use cullr_core::Db;
use cullr_core::Label;
use cullr_core::PhotoDetail;
use cullr_core::PhotoEntry;
use cullr_core::PhotoStatus;

use crate::tex::{TexKey, TextureState, Textures};
use crate::theme;
use crate::views::grid::row_rotation;
use crate::views::widgets;

/// Zoom multiplier at fit-to-window; all zooming lives in `[1, max]`.
const FIT_ZOOM: f32 = 1.0;
/// Exponential wheel gain: scroll delta × gain, exponentiated.
const WHEEL_GAIN: f32 = 0.0016;
/// Per-frame wheel step clamp so a runaway trackpad cannot teleport zoom.
const WHEEL_STEP_MAX: f32 = 1.35;
/// Multiplicative factor per `Ctrl+=` / `Ctrl+-` press and per zoom-pill
/// button click — the classic discrete zoom cadence.
const ZOOM_KEY_STEP: f32 = 1.25;
/// Horizontal pill drag distance that multiplies zoom by e (scrubby).
const SCRUB_GAIN: f32 = 0.01;
/// Smallest marquee side accepted as a deliberate region selection;
/// shorter drags are noise around a click and change nothing.
const MARQUEE_MIN: f32 = 8.0;
/// Height of the bottom EXIF bar.
const BAR_HEIGHT: f32 = 30.0;
/// Height of the zoom indicator pill.
const PILL_HEIGHT: f32 = 24.0;
/// How many positions on each side of the current photo prefetch their
/// screen texture (SPEC §5.3: loupe id±3).
const NEIGHBOR_REACH: usize = 3;

/// What the loupe wants the app to do after this frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Keep showing the loupe.
    Stay,
    /// Return to the contact sheet.
    Close,
}

/// State of the loupe for one browsing session over the grid's entries.
pub struct LoupeView {
    /// Position in the folder order currently on display.
    index: usize,
    /// Zoom multiplier over the fit rectangle; [`FIT_ZOOM`] means fit.
    zoom: f32,
    /// Displayed-image-center offset from the viewport center.
    pan: egui::Vec2,
    /// Chromeless mode: photo alone on black, no bars or pills.
    lightbox: bool,
    /// Lightbox backdrop choice; white suits high-key frames that
    /// vanish against the default near-black surround.
    white_bg: bool,
    /// Screen point where the Shift-drag marquee zoom started, while it
    /// is being drawn. `Some` also suppresses panning for that drag.
    marquee_anchor: Option<egui::Pos2>,
    /// Live marquee rectangle for painting; resolved into a zoom on release.
    marquee_rect: Option<egui::Rect>,
}

impl LoupeView {
    /// Opens the loupe on `index`, fitted to the window.
    pub fn at(index: usize) -> Self {
        Self {
            index,
            zoom: FIT_ZOOM,
            pan: egui::Vec2::ZERO,
            lightbox: false,
            white_bg: false,
            marquee_anchor: None,
            marquee_rect: None,
        }
    }

    /// Opens the loupe straight into the lightbox (`L` from the sheet).
    pub fn at_in_lightbox(index: usize) -> Self {
        let mut loupe = Self::at(index);
        loupe.lightbox = true;
        loupe
    }

    /// Draws the screen, consumes input, and reports the next action.
    ///
    /// `entries` is mutable so digit labels update the sheet's source of
    /// truth in place; `order` is the grid's filtered view (positions into
    /// `entries`, SPEC §6 "arrows navigate within filtered order") and
    /// `index` addresses it, so navigation skips filtered-out photos.
    /// `auto_advance` is shared with the grid because Tab works in
    /// whichever view is on screen (SPEC §6).
    ///
    /// `suspended` freezes input while a modal dialog covers the screen
    /// (help overlay, About): the photo keeps painting under the dimmed
    /// backdrop, but keys, wheel zooming and panning stay quiet.
    // The parameter list mirrors what a full-screen preview needs: state,
    // shared sheet data, services and flags. Splitting it into structs
    // would only move the count elsewhere.
    #[expect(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        db: &Db,
        entries: &mut [PhotoEntry],
        order: &[usize],
        textures: &mut Textures,
        auto_advance: &mut bool,
        suspended: bool,
    ) -> Outcome {
        if order.is_empty() || self.index >= order.len() {
            return Outcome::Close;
        }
        let (left, right, escape, enter, lightbox_key) = if suspended {
            (false, false, false, false, false)
        } else {
            ui.ctx().input(|input| {
                (
                    input.key_pressed(egui::Key::ArrowLeft),
                    input.key_pressed(egui::Key::ArrowRight),
                    input.key_pressed(egui::Key::Escape),
                    input.key_pressed(egui::Key::Enter),
                    input.key_pressed(egui::Key::L),
                )
            })
        };
        // The lightbox unwraps first: the same keys that leave the loupe
        // restore its chrome while the photo is shown without any.
        if self.lightbox && (escape || enter || lightbox_key) {
            self.lightbox = false;
        } else if escape || enter {
            return Outcome::Close;
        } else if lightbox_key {
            self.lightbox = true;
        }
        // Backdrop color only means something in the lightbox; the
        // choice sticks for the rest of the session.
        if self.lightbox && !suspended && ui.ctx().input(|input| input.key_pressed(egui::Key::W)) {
            self.white_bg = !self.white_bg;
        }
        // Navigation resets to fit: each photo starts centered.
        if left && self.index > 0 {
            self.move_to(self.index - 1);
        }
        if right && self.index + 1 < order.len() {
            self.move_to(self.index + 1);
        }
        // Cull-pass keys: Tab flips the persisted advance mode, digits
        // label the photo on display and (when armed) walk forward.
        if !suspended && widgets::tab_pressed(ui.ctx()) {
            *auto_advance = !*auto_advance;
            widgets::store_auto_advance(db, *auto_advance);
        }
        if !suspended && let Some(label) = widgets::pressed_label_key(ui.ctx()) {
            self.apply_label(entries, order, db, label, *auto_advance);
        }
        // `[` / `]` turn the photo on display a quarter step; the change
        // persists and re-decodes its texture next frame (SPEC §6).
        if !suspended && let Some(direction) = widgets::pressed_rotate_key(ui.ctx()) {
            self.rotate_current(entries, order, db, direction);
        }

        let row = order[self.index];
        let entry = &entries[row];
        let current_label = entry.label;
        // Copied out before any mutable borrow of `entries` (labeling)
        // so the zoom readout can use it afterwards.
        let native_width = entry.display_pixels().map(|(width, _)| width);
        let detail = fetch_detail(db, entry.id);

        // Focus cancels stale neighbour decodes when flying through the
        // set; the band around the current photo refills the queues.
        // Each neighbour needs its own row for the preview asset path;
        // the per-photo point queries are microsecond-scale.
        textures.focus(&[TexKey::screen(entry.id)]);
        for position in neighbor_indices(self.index, order.len(), NEIGHBOR_REACH) {
            let neighbor_row = order[position];
            let Some(neighbor) = entries.get(neighbor_row) else {
                continue;
            };
            if neighbor.status != PhotoStatus::Ok {
                continue;
            }
            let path = fetch_detail(db, neighbor.id).and_then(|detail| detail.preview_path);
            let key = TexKey::screen(neighbor.id);
            let rotation = row_rotation(neighbor);
            textures.prefetch([(key, path.as_deref().map(|path| (path, rotation)))].into_iter());
        }

        let (area, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        // The lightbox gives the photo the whole screen; otherwise the
        // bottom strip stays reserved for the EXIF bar.
        let bar_height = if self.lightbox { 0.0 } else { BAR_HEIGHT };
        let image_area = egui::Rect::from_min_max(area.min, area.max - egui::vec2(0.0, bar_height));
        let aspect = image_aspect(entry);
        let fitted_size = super::grid::fit_rect(image_area, aspect).size();
        let max_zoom = max_zoom(entry, fitted_size);

        if !suspended && ui.ctx().input(|input| input.key_pressed(egui::Key::Space)) {
            let zoomed_in = self.zoom > FIT_ZOOM + f32::EPSILON * 8.0;
            self.zoom = if zoomed_in { FIT_ZOOM } else { max_zoom };
            self.pan = egui::Vec2::ZERO;
        }

        // Photoshop-standard jumps and steps: fit, pixel parity, and
        // multiplicative in/out anchored at the pointer when it hovers
        // the photo, at the center otherwise.
        let (fit_key, parity_key, step_in, step_out) = if suspended {
            (false, false, false, false)
        } else {
            ui.ctx().input(|input| {
                (
                    input.key_pressed(egui::Key::Num0) && input.modifiers.command_only(),
                    input.key_pressed(egui::Key::Num1) && input.modifiers.command_only(),
                    (input.key_pressed(egui::Key::Equals) || input.key_pressed(egui::Key::Plus))
                        && input.modifiers.command_only(),
                    input.key_pressed(egui::Key::Minus) && input.modifiers.command_only(),
                )
            })
        };
        if fit_key {
            self.zoom = FIT_ZOOM;
            self.pan = egui::Vec2::ZERO;
        }
        if parity_key {
            self.zoom = max_zoom;
            self.pan = egui::Vec2::ZERO;
        }
        if step_in || step_out {
            let anchor = response
                .hover_pos()
                .map_or(egui::Vec2::ZERO, |cursor| cursor - area.center());
            let factor = if step_out {
                1.0 / ZOOM_KEY_STEP
            } else {
                ZOOM_KEY_STEP
            };
            let (zoom, pan) = zoom_step(
                self.zoom,
                self.pan,
                factor,
                max_zoom,
                anchor,
                fitted_size,
                image_area.size(),
            );
            self.zoom = zoom;
            self.pan = pan;
        }

        // Wheel zooming has two channels in egui: plain scroll lands in
        // smooth_scroll_delta (SPEC §6), while Ctrl/Cmd + scroll and
        // trackpad pinch are routed into zoom_delta instead — one event
        // never feeds both, so nothing applies twice.
        let (scroll, zoom_gesture) =
            ui.input(|input| (input.smooth_scroll_delta.y, input.zoom_delta()));
        if !suspended
            && response.hovered()
            && let Some(cursor) = response.hover_pos()
        {
            let factor = if zoom_gesture != 1.0 {
                zoom_gesture.clamp(1.0 / WHEEL_STEP_MAX, WHEEL_STEP_MAX)
            } else if scroll != 0.0 {
                (scroll * WHEEL_GAIN)
                    .exp()
                    .clamp(1.0 / WHEEL_STEP_MAX, WHEEL_STEP_MAX)
            } else {
                1.0
            };
            if factor != 1.0 {
                let (zoom, pan) = zoom_step(
                    self.zoom,
                    self.pan,
                    factor,
                    max_zoom,
                    cursor - area.center(),
                    fitted_size,
                    image_area.size(),
                );
                self.zoom = zoom;
                self.pan = pan;
            }
        }

        // Shift starts a marquee-zoom drag instead of a pan; the region
        // drawn while it lasts is resolved into a zoom on release.
        let shift_drag = !suspended
            && response.drag_started()
            && ui.ctx().input(|input| input.modifiers.shift_only());
        if shift_drag
            && let Some(origin) = response.interact_pointer_pos()
            && image_area.contains(origin)
        {
            self.marquee_anchor = Some(origin);
        }
        if let Some(anchor) = self.marquee_anchor {
            if response.dragged() {
                if let Some(pointer) = response.interact_pointer_pos() {
                    self.marquee_rect = Some(egui::Rect::from_two_pos(anchor, pointer));
                }
            }
            if response.drag_stopped() {
                if let Some(rect) = self.marquee_rect.take()
                    && rect.width().min(rect.height()) >= MARQUEE_MIN
                {
                    let (zoom, pan) = marquee_zoom_step(
                        rect,
                        image_area.center(),
                        self.zoom,
                        self.pan,
                        fitted_size,
                        image_area.size(),
                        max_zoom,
                    );
                    self.zoom = zoom;
                    self.pan = pan;
                }
                self.marquee_anchor = None;
            }
        }
        if !suspended && response.dragged() && self.marquee_anchor.is_none() {
            self.pan = clamp_pan(
                self.pan + response.drag_delta(),
                fitted_size * self.zoom,
                image_area.size(),
            );
        }
        // Double-click flips between the two extremes under the cursor,
        // like clicking through Photoshop's zoom presets.
        if !suspended
            && response.double_clicked()
            && let Some(cursor) = response.interact_pointer_pos()
        {
            let zoomed_in = self.zoom > FIT_ZOOM + f32::EPSILON * 8.0;
            let factor = if zoomed_in {
                FIT_ZOOM / self.zoom
            } else {
                max_zoom / self.zoom
            };
            let (zoom, pan) = zoom_step(
                self.zoom,
                self.pan,
                factor,
                max_zoom,
                cursor - area.center(),
                fitted_size,
                image_area.size(),
            );
            self.zoom = zoom;
            self.pan = pan;
        }

        let background = if self.lightbox {
            if self.white_bg {
                theme::PAPER
            } else {
                theme::VOID
            }
        } else {
            theme::BG
        };
        paint_photo(
            ui,
            textures,
            entry,
            preview_path(&detail),
            image_area,
            aspect,
            self.zoom,
            self.pan,
            background,
        );
        if let Some(rect) = self.marquee_rect {
            draw_marquee(ui.painter(), image_area, rect);
        }
        // Chromeless lightbox: the photo alone, nothing else on screen.
        if !self.lightbox {
            draw_position_overlay(
                ui.painter(),
                image_area,
                detail.as_ref(),
                self.index + 1,
                order.len(),
            );
            let swatch_pick = draw_exif_bar(
                ui,
                area,
                detail.as_ref(),
                entry,
                current_label,
                *auto_advance,
            );
            if let Some(label) = swatch_pick {
                self.apply_label(entries, order, db, label, *auto_advance);
            }
            if let Some(percent) = zoom_percent(native_width, self.zoom, fitted_size)
                && let Some(factor) = match draw_zoom_pill(ui, image_area, percent) {
                    ZoomPill::None => None,
                    ZoomPill::StepIn => Some(ZOOM_KEY_STEP),
                    ZoomPill::StepOut => Some(1.0 / ZOOM_KEY_STEP),
                    ZoomPill::Scrub(factor) => Some(factor),
                }
            {
                // Pill steps and scrubs anchor at the viewport center:
                // the pointer is on the pill, not on a photo point.
                let (zoom, pan) = zoom_step(
                    self.zoom,
                    self.pan,
                    factor,
                    max_zoom,
                    egui::Vec2::ZERO,
                    fitted_size,
                    image_area.size(),
                );
                self.zoom = zoom;
                self.pan = pan;
            }
        }

        Outcome::Stay
    }

    /// Position in the filtered folder order currently on display; the
    /// grid picks it up as its cursor when the loupe closes.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Re-anchors the loupe after the surrounding filter changed: lands
    /// on `index` in the new order, reset to fit like any navigation.
    pub fn jump_to(&mut self, index: usize) {
        self.move_to(index);
    }

    /// Persists a label instantly (single UPDATE, SPEC §6) and mirrors it
    /// into the sheet's row so tiles refresh without a re-read. With
    /// auto-advance armed every labeling keystroke also walks to the next
    /// photo; re-labeling with the same color still advances, because the
    /// keystroke means "decided", not "changed".
    fn apply_label(
        &mut self,
        entries: &mut [PhotoEntry],
        order: &[usize],
        db: &Db,
        label: Label,
        auto_advance: bool,
    ) {
        let Some(&row) = order.get(self.index) else {
            return;
        };
        let Some(entry) = entries.get_mut(row) else {
            return;
        };
        if entry.label != label {
            entry.label = label;
            if let Err(error) = db.set_label(entry.id, label) {
                tracing::warn!(%error, id = entry.id.0, "cannot persist label");
            }
        }
        if auto_advance && self.index + 1 < order.len() {
            self.move_to(self.index + 1);
        }
    }

    /// Persists a quarter-turn for the photo on display and mirrors it
    /// into the sheet's row. Zoom resets to fit because the silhouette
    /// may swap between landscape and portrait; the texture re-decodes
    /// automatically since the demanded rotation no longer matches the
    /// resident slot.
    fn rotate_current(
        &mut self,
        entries: &mut [PhotoEntry],
        order: &[usize],
        db: &Db,
        direction: widgets::RotateDir,
    ) {
        let Some(&row) = order.get(self.index) else {
            return;
        };
        let Some(entry) = entries.get_mut(row) else {
            return;
        };
        entry.rot_cw = widgets::turned(entry.rot_cw, direction.delta());
        if let Err(error) = db.set_rotation(entry.id, entry.rot_cw) {
            tracing::warn!(%error, id = entry.id.0, "cannot persist rotation");
        }
        self.zoom = FIT_ZOOM;
        self.pan = egui::Vec2::ZERO;
    }

    /// Jumps to `index` and recenters: every photo starts at fit.
    fn move_to(&mut self, index: usize) {
        self.index = index;
        self.zoom = FIT_ZOOM;
        self.pan = egui::Vec2::ZERO;
    }
}

fn fetch_detail(db: &Db, id: cullr_core::PhotoId) -> Option<PhotoDetail> {
    match db.photo_detail(id) {
        Ok(detail) => detail,
        Err(error) => {
            tracing::warn!(%error, id = id.0, "cannot load photo details");
            None
        }
    }
}

/// Preview asset path of the photo on display.
fn preview_path(detail: &Option<PhotoDetail>) -> Option<&std::path::Path> {
    detail
        .as_ref()
        .and_then(|detail| detail.preview_path.as_deref())
}

/// Positions adjacent to `index`, nearest first, both directions,
/// clamped to the set. Pure so the prefetch band is testable.
fn neighbor_indices(index: usize, len: usize, reach: usize) -> impl Iterator<Item = usize> {
    (1..=reach)
        .flat_map(move |distance| [index.checked_sub(distance), Some(index + distance)])
        .flatten()
        .filter(move |&position| position < len)
}

/// Aspect of the photo as displayed: preview pixels transposed by the
/// EXIF orientation and user turns, exactly what the texture pipeline
/// rotates the decoded JPEG into.
fn image_aspect(entry: &PhotoEntry) -> f32 {
    entry.display_aspect()
}

/// Zoom multiplier that makes the displayed image pixel-parity (100%) in
/// the viewport; at least [`FIT_ZOOM`] so small previews stay put. Uses
/// display pixels — a rotated portrait's parity width is its stored
/// height, matching what the texture actually shows.
fn max_zoom(entry: &PhotoEntry, fitted_size: egui::Vec2) -> f32 {
    let Some((width, _)) = entry.display_pixels() else {
        return FIT_ZOOM;
    };
    if fitted_size.x <= 0.0 {
        return FIT_ZOOM;
    }
    (width as f32 / fitted_size.x).max(FIT_ZOOM)
}

/// Current zoom as a percentage of native resolution (100% = pixel
/// parity), the number Photoshop shows in its status bar. Takes the
/// photo's displayed pixel width; `None` when unknown or the viewport
/// collapsed.
fn zoom_percent(native_width: Option<u32>, zoom: f32, fitted_size: egui::Vec2) -> Option<f32> {
    let width = native_width?;
    if fitted_size.x <= 0.0 {
        return None;
    }
    Some(zoom * fitted_size.x / width as f32 * 100.0)
}

/// Zoom that makes the dragged `marquee` region fill the viewport,
/// centered on the region's middle — Photoshop's marquee-zoom tool.
///
/// The factor scales whichever marquee side is relatively larger (the
/// contain fit); the region's image point then lands exactly on the
/// viewport center before the usual edge clamping applies. Returns fit
/// centered when the clamp pulls the zoom back down.
fn marquee_zoom_step(
    marquee: egui::Rect,
    viewport_center: egui::Pos2,
    zoom: f32,
    pan: egui::Vec2,
    fitted_size: egui::Vec2,
    viewport: egui::Vec2,
    max_zoom: f32,
) -> (f32, egui::Vec2) {
    if fitted_size.x <= 0.0 || fitted_size.y <= 0.0 || zoom <= 0.0 {
        return (zoom, pan);
    }
    let factor =
        (viewport.x / marquee.width().max(1.0)).min(viewport.y / marquee.height().max(1.0));
    let new_zoom = (zoom * factor).clamp(FIT_ZOOM, max_zoom.max(FIT_ZOOM));
    if new_zoom <= FIT_ZOOM + f32::EPSILON * 8.0 {
        return (new_zoom, egui::Vec2::ZERO);
    }
    // Fraction of the displayed image (from its center) the region covers.
    let region_from_center = marquee.center() - viewport_center;
    let anchor = (region_from_center - pan) / (fitted_size * zoom);
    let new_pan = -anchor * (fitted_size * new_zoom);
    (
        new_zoom,
        clamp_pan(new_pan, fitted_size * new_zoom, viewport),
    )
}

/// Keeps the displayed image pinned to the viewport: at fit there is no
/// slack (always centered); zoomed in, edges may reach but never pass
/// the viewport edge.
fn clamp_pan(pan: egui::Vec2, displayed: egui::Vec2, viewport: egui::Vec2) -> egui::Vec2 {
    let slack_x = ((displayed.x - viewport.x) / 2.0).max(0.0);
    let slack_y = ((displayed.y - viewport.y) / 2.0).max(0.0);
    egui::vec2(
        pan.x.clamp(-slack_x, slack_x),
        pan.y.clamp(-slack_y, slack_y),
    )
}

/// One multiplicative zoom step keeping the image point under the cursor
/// fixed on screen.
///
/// `cursor_from_center` is the pointer relative to the viewport center;
/// internally converted to a fraction of the displayed image so anchoring
/// stays exact at any pan. Snaps back to perfectly centered when the
/// clamp returns zoom to fit.
fn zoom_step(
    zoom: f32,
    pan: egui::Vec2,
    factor: f32,
    max_zoom: f32,
    cursor_from_center: egui::Vec2,
    fitted_size: egui::Vec2,
    viewport: egui::Vec2,
) -> (f32, egui::Vec2) {
    let new_zoom = (zoom * factor).clamp(FIT_ZOOM, max_zoom.max(FIT_ZOOM));
    if (new_zoom - zoom).abs() <= f32::EPSILON * 8.0 {
        return (zoom, pan);
    }
    // Fraction of the image (relative to its center) under the cursor.
    let anchor = (cursor_from_center - pan) / (fitted_size * zoom);
    let new_pan = cursor_from_center - anchor * (fitted_size * new_zoom);
    if new_zoom <= FIT_ZOOM + f32::EPSILON * 8.0 {
        return (new_zoom, egui::Vec2::ZERO);
    }
    (
        new_zoom,
        clamp_pan(new_pan, fitted_size * new_zoom, viewport),
    )
}

/// The photo itself: ready texture (zoomable), shimmer while decoding,
/// spinner while extraction is pending, warning fallback otherwise.
/// The caller picks the backdrop — app background for the loupe,
/// lightbox black or white otherwise.
#[expect(clippy::too_many_arguments)]
fn paint_photo(
    ui: &mut egui::Ui,
    textures: &mut Textures,
    entry: &PhotoEntry,
    preview_path: Option<&std::path::Path>,
    area: egui::Rect,
    aspect: f32,
    zoom: f32,
    pan: egui::Vec2,
    background: egui::Color32,
) {
    let painter = ui.painter().clone();
    painter.rect_filled(area, 0.0, background);
    let center = area.center() + pan;
    match entry.status {
        PhotoStatus::Ok => {
            match textures.handle(TexKey::screen(entry.id), preview_path, row_rotation(entry)) {
                TextureState::Ready(handle) => {
                    let displayed = super::grid::fit_rect(area, aspect).size() * zoom;
                    let destination = egui::Rect::from_center_size(center, displayed);
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    painter.image(handle.id(), destination, uv, egui::Color32::WHITE);
                }
                TextureState::Loading => draw_shimmer(&painter, area, ui.input(|input| input.time)),
                TextureState::Broken => {
                    draw_warning(&painter, area, "Preview could not be decoded")
                }
            }
        }
        PhotoStatus::Pending => {
            let spinner = egui::Spinner::new().size(28.0);
            ui.put(
                egui::Rect::from_center_size(center, egui::vec2(32.0, 32.0)),
                spinner,
            );
            painter.text(
                center + egui::vec2(0.0, 36.0),
                egui::Align2::CENTER_CENTER,
                "Extracting preview…",
                egui::FontId::proportional(13.0),
                theme::MUTED,
            );
        }
        other => {
            let message = if other == PhotoStatus::Error {
                entry
                    .err_msg
                    .as_deref()
                    .unwrap_or("Extraction failed")
                    .to_owned()
            } else {
                "File is missing".to_owned()
            };
            draw_warning(&painter, area, &message);
        }
    }
}

/// Animated placeholder while the preview decodes: a soft light band
/// sweeps across the dark frame so waiting reads as progress, not freeze.
fn draw_shimmer(painter: &egui::Painter, area: egui::Rect, time: f64) {
    let painter = painter.with_clip_rect(area);
    painter.rect_filled(area, 0.0, theme::PANEL);
    let band_width = (area.width() * 0.3).max(60.0);
    let travel = area.width() + band_width;
    let lead = area.left() - band_width + travel * ((time * 0.45) % 1.0) as f32;
    let slice = band_width / 7.0;
    for step in -3..=3_i32 {
        let alpha = 16 - step.abs() * 5;
        let x = lead + step as f32 * slice;
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, area.top()), egui::vec2(slice, area.height())),
            0.0,
            egui::Color32::from_white_alpha(alpha.max(0) as u8),
        );
    }
}

/// Centered warning glyph plus explanation for unviewable photos.
fn draw_warning(painter: &egui::Painter, area: egui::Rect, message: &str) {
    painter.text(
        area.center() - egui::vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        "⚠",
        egui::FontId::proportional(34.0),
        theme::MUTED,
    );
    painter.text(
        area.center() + egui::vec2(0.0, 18.0),
        egui::Align2::CENTER_CENTER,
        message,
        egui::FontId::proportional(13.0),
        theme::MUTED,
    );
}

/// Accent wash over the Shift-drag region while the marquee zoom is
/// being drawn; clipped so it cannot spill onto bars or outside.
fn draw_marquee(painter: &egui::Painter, clip: egui::Rect, rect: egui::Rect) {
    let painter = painter.with_clip_rect(clip);
    painter.rect_filled(rect, 0.0, theme::ACCENT.gamma_multiply(0.16));
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.25, theme::ACCENT),
        egui::StrokeKind::Outside,
    );
}

/// What the zoom indicator pill wants applied this frame.
#[derive(Debug, PartialEq)]
enum ZoomPill {
    /// Nothing happened.
    None,
    /// The `−` side stepped out one notch.
    StepOut,
    /// The `+` side stepped in one notch.
    StepIn,
    /// The percentage was dragged; apply this multiplicative factor.
    Scrub(f32),
}

/// Bottom-left zoom indicator (`− 42% +`) styled like the position
/// pill. The buttons step like `Ctrl+=` / `Ctrl+-`; dragging the number
/// scrubs zoom horizontally, Photoshop's scrubby slider. Anchoring is
/// the caller's business — this only reports intent.
fn draw_zoom_pill(ui: &mut egui::Ui, image_area: egui::Rect, percent: f32) -> ZoomPill {
    let text_font = egui::FontId::proportional(12.0);
    let glyph_font = egui::FontId::proportional(14.0);
    let painter = ui.painter();
    let label = painter.layout_no_wrap(format!("{:.0}%", percent), text_font, theme::TEXT);
    let minus = painter.layout_no_wrap("−".to_owned(), glyph_font.clone(), theme::MUTED);
    let plus = painter.layout_no_wrap("+".to_owned(), glyph_font, theme::MUTED);
    let gap = 8.0;
    let padding_x = 10.0;
    let width = padding_x * 2.0 + minus.size().x + gap + label.size().x + gap + plus.size().x;
    let rect = egui::Rect::from_min_size(
        egui::pos2(
            image_area.left() + 12.0,
            image_area.bottom() - 12.0 - PILL_HEIGHT,
        ),
        egui::vec2(width, PILL_HEIGHT),
    );
    painter.rect_filled(
        rect,
        PILL_HEIGHT / 2.0,
        egui::Color32::from_black_alpha(160),
    );

    let mut action = ZoomPill::None;
    let mut strip = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    strip.spacing_mut().item_spacing.x = gap;
    strip.horizontal(|ui| {
        // `layout_no_wrap` hands back shared galleys; zones paint a clone
        // centered inside their full-height hit target.
        let glyph_zone =
            |ui: &mut egui::Ui, galley: std::sync::Arc<egui::Galley>, hint: &'static str| {
                let size = egui::vec2(galley.size().x, PILL_HEIGHT);
                let (zone, response) = ui.allocate_exact_size(size, egui::Sense::click());
                ui.painter().galley(
                    egui::pos2(
                        zone.center().x - galley.size().x / 2.0,
                        zone.center().y - galley.size().y / 2.0,
                    ),
                    galley,
                    theme::MUTED,
                );
                let response = response.on_hover_text(hint);
                response.clicked()
            };
        if glyph_zone(ui, minus, "Zoom out (Ctrl+-)") {
            action = ZoomPill::StepOut;
        }
        let (zone, response) = ui.allocate_exact_size(
            egui::vec2(label.size().x, PILL_HEIGHT),
            egui::Sense::click_and_drag(),
        );
        ui.painter().galley(
            egui::pos2(
                zone.center().x - label.size().x / 2.0,
                zone.center().y - label.size().y / 2.0,
            ),
            label,
            theme::TEXT,
        );
        let response = response
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
            .on_hover_text("Drag sideways to change zoom");
        if response.dragged() {
            let delta = response.drag_delta().x;
            if delta != 0.0 {
                action = ZoomPill::Scrub(
                    (delta * SCRUB_GAIN)
                        .exp()
                        .clamp(1.0 / WHEEL_STEP_MAX, WHEEL_STEP_MAX),
                );
            }
        }
        if glyph_zone(ui, plus, "Zoom in (Ctrl+=)") {
            action = ZoomPill::StepIn;
        }
    });
    action
}

/// Top-right pill: color-label dot plus position counter `n / total`
/// with thin-space thousands grouping.
fn draw_position_overlay(
    painter: &egui::Painter,
    area: egui::Rect,
    detail: Option<&PhotoDetail>,
    ordinal: usize,
    total: usize,
) {
    let counter = format!(
        "{} / {}",
        widgets::grouped(ordinal),
        widgets::grouped(total)
    );
    let galley = painter.layout_no_wrap(counter, egui::FontId::proportional(12.0), theme::TEXT);
    let dot_diameter = 9.0;
    let gap = 8.0;
    let padding_x = 11.0;
    let height = 24.0;
    let width = padding_x + dot_diameter + gap + galley.size().x + padding_x;
    let rect = egui::Rect::from_min_size(
        egui::pos2(area.right() - width - 12.0, area.top() + 12.0),
        egui::vec2(width, height),
    );
    painter.rect_filled(rect, height / 2.0, egui::Color32::from_black_alpha(150));

    let dot_center = egui::pos2(
        rect.left() + padding_x + dot_diameter / 2.0,
        rect.center().y,
    );
    match detail.map(|detail| detail.label) {
        Some(label) if label != cullr_core::Label::None => {
            painter.circle_filled(dot_center, dot_diameter / 2.0, theme::label_color(label));
        }
        _ => {
            painter.circle_stroke(
                dot_center,
                dot_diameter / 2.0,
                egui::Stroke::new(1.25, theme::MUTED),
            );
        }
    }
    painter.galley(
        egui::pos2(
            rect.left() + padding_x + dot_diameter + gap,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        theme::TEXT,
    );
}

/// Bottom bar: EXIF summary on the left (clipped before the palette),
/// the auto-advance state and clickable label swatches on the right.
/// Reports a swatch click as the label to apply.
fn draw_exif_bar(
    ui: &mut egui::Ui,
    area: egui::Rect,
    detail: Option<&PhotoDetail>,
    entry: &PhotoEntry,
    current_label: Label,
    auto_advance: bool,
) -> Option<Label> {
    let bar = egui::Rect::from_min_max(
        egui::pos2(area.left(), area.bottom() - BAR_HEIGHT),
        area.right_bottom(),
    );
    ui.painter().rect_filled(bar, 0.0, theme::PANEL);

    // Palette block, vertically centered in the bar; `label_swatches`
    // owns its own hit-testing through a child UI pinned to this rect.
    let swatch_left = bar.right() - widgets::SWATCH_STRIP_WIDTH;
    let swatch_rect = egui::Rect::from_min_size(
        egui::pos2(swatch_left, bar.center().y - widgets::SWATCH_DIAMETER / 2.0),
        egui::vec2(widgets::SWATCH_STRIP_WIDTH, widgets::SWATCH_DIAMETER),
    );
    let picked = {
        let mut palette = ui.new_child(egui::UiBuilder::new().max_rect(swatch_rect));
        widgets::label_swatches(&mut palette, current_label)
    };

    // Tiny mode flag so fullscreen culling still shows whether labeling
    // walks forward; sits just left of the palette.
    let flag = if auto_advance { "⏭" } else { "⏸" };
    ui.painter().text(
        egui::pos2(swatch_left - 24.0, bar.center().y),
        egui::Align2::CENTER_CENTER,
        flag,
        egui::FontId::proportional(11.0),
        if auto_advance {
            theme::ACCENT
        } else {
            theme::MUTED
        },
    );

    // Summary line gets whatever width the right-hand controls leave it.
    let line = detail.map_or_else(|| file_name(entry), |detail| exif_line(detail, entry));
    let text_clip = egui::Rect::from_min_max(bar.min, egui::pos2(swatch_left - 40.0, bar.max.y));
    ui.painter().with_clip_rect(text_clip).text(
        egui::pos2(bar.left() + 12.0, bar.center().y),
        egui::Align2::LEFT_CENTER,
        line,
        egui::FontId::proportional(12.0),
        theme::MUTED,
    );
    picked
}

/// Builds the EXIF summary line; absent fields are skipped rather than
/// shown as gaps, and an all-empty record degrades to the file name.
fn exif_line(detail: &PhotoDetail, entry: &PhotoEntry) -> String {
    let mut parts: Vec<String> = Vec::new();
    for text in [
        detail.camera.clone(),
        detail.lens.clone(),
        detail
            .aperture
            .map(|value| format!("f/{}", trim_number(value))),
        detail.shutter.clone(),
        detail.iso.map(|value| format!("ISO {value}")),
        detail
            .focal_mm
            .map(|value| format!("{}mm", trim_number(value))),
        detail.taken_at.clone(),
    ]
    .into_iter()
    .flatten()
    {
        parts.push(text);
    }
    if parts.is_empty() {
        file_name(entry)
    } else {
        parts.join(" · ")
    }
}

/// Renders a float without a dangling `.0` (`2.8` stays, `35.0` → `35`).
fn trim_number(value: f64) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 0.05 {
        format!("{rounded:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn file_name(entry: &PhotoEntry) -> String {
    entry.rel_path.file_name().map_or_else(
        || entry.rel_path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    // Test setup asserts hard failures; a broken fixture aborts the test.
    #![expect(clippy::expect_used)]

    use super::*;

    #[test]
    fn clamp_pan_should_pin_fit_images_to_the_center() {
        let viewport = egui::vec2(800.0, 600.0);
        let displayed = viewport * 0.5;
        assert_eq!(
            clamp_pan(egui::vec2(120.0, -90.0), displayed, viewport),
            egui::Vec2::ZERO
        );
    }

    #[test]
    fn clamp_pan_should_stop_at_image_edges_when_zoomed() {
        let viewport = egui::vec2(800.0, 600.0);
        // Displayed twice the viewport: 400 px of horizontal slack.
        assert_eq!(
            clamp_pan(egui::vec2(-900.0, 50.0), viewport * 2.0, viewport),
            egui::vec2(-400.0, 50.0)
        );
    }

    #[test]
    fn zoom_step_should_keep_the_cursor_point_fixed_without_panning() {
        let viewport = egui::vec2(800.0, 600.0);
        let fitted = egui::vec2(600.0, 400.0);
        // Pointer on the image center: zooming must not drift the image.
        let cursor = egui::Vec2::ZERO;

        let (zoom, pan) = zoom_step(1.0, egui::Vec2::ZERO, 2.0, 4.0, cursor, fitted, viewport);

        assert!((zoom - 2.0).abs() < 1e-4);
        assert!(pan.x.abs() < 1e-4 && pan.y.abs() < 1e-4);
    }

    #[test]
    fn zoom_step_should_anchor_the_zoomed_point_under_the_cursor() {
        let viewport = egui::vec2(800.0, 600.0);
        let fitted = egui::vec2(600.0, 400.0);
        let zoom = 1.0;
        let pan = egui::Vec2::ZERO;
        // A point right of and below the image center…
        let cursor = egui::vec2(100.0, 50.0);
        let before = cursor - pan;

        let (new_zoom, new_pan) = zoom_step(zoom, pan, 2.0, 8.0, cursor, fitted, viewport);

        assert!((new_zoom - 2.0).abs() < 1e-4);
        // …must stay put while the image doubles under it.
        let after = cursor - new_pan;
        let drift = after - before * (new_zoom / zoom);
        assert!(
            drift.x.abs() < 1e-3 && drift.y.abs() < 1e-3,
            "anchored point moved: {before:?} → {after:?}"
        );
    }

    #[test]
    fn zoom_step_should_snap_back_to_center_when_returning_to_fit() {
        let viewport = egui::vec2(800.0, 600.0);
        let fitted = egui::vec2(600.0, 400.0);

        let (zoom, pan) = zoom_step(
            1.05,
            egui::vec2(80.0, 0.0),
            0.5,
            4.0,
            egui::Vec2::ZERO,
            fitted,
            viewport,
        );

        assert!((zoom - 1.0).abs() < 1e-6);
        assert_eq!(pan, egui::Vec2::ZERO);
    }

    #[test]
    fn zoom_step_should_clamp_beyond_pixel_parity() {
        let viewport = egui::vec2(800.0, 600.0);
        let fitted = egui::vec2(600.0, 400.0);

        let (zoom, _) = zoom_step(
            4.0,
            egui::Vec2::ZERO,
            10.0,
            4.0,
            egui::Vec2::ZERO,
            fitted,
            viewport,
        );

        assert!((zoom - 4.0).abs() < 1e-5);
    }

    #[test]
    fn neighbor_indices_should_walk_both_directions_nearest_first() {
        let indices: Vec<usize> = neighbor_indices(5, 10, 3).collect();
        assert_eq!(indices, vec![4, 6, 3, 7, 2, 8]);
    }

    #[test]
    fn neighbor_indices_should_clamp_to_set_bounds() {
        let indices: Vec<usize> = neighbor_indices(0, 3, 3).collect();
        assert_eq!(indices, vec![1, 2]);
    }

    #[test]
    fn max_zoom_should_be_pixel_parity_over_the_fit_rect() {
        let entry = PhotoEntry {
            id: cullr_core::PhotoId(1),
            rel_path: "a.nef".into(),
            label: cullr_core::Label::None,
            status: PhotoStatus::Ok,
            pixels: Some((3000, 2000)),
            orientation: 1,
            rot_cw: 0,
            thumb_path: None,
            err_msg: None,
            jpeg_rel_path: None,
        };
        // Fit rect is 1500 px wide: 100% needs exactly 2×.
        assert!((max_zoom(&entry, egui::vec2(1500.0, 1000.0)) - 2.0).abs() < 1e-5);
    }

    #[test]
    fn max_zoom_should_use_the_rotated_width_for_portrait_previews() {
        let entry = PhotoEntry {
            id: cullr_core::PhotoId(1),
            rel_path: "a.nef".into(),
            label: cullr_core::Label::None,
            status: PhotoStatus::Ok,
            pixels: Some((6000, 4000)),
            orientation: 6,
            rot_cw: 0,
            thumb_path: None,
            err_msg: None,
            jpeg_rel_path: None,
        };
        // Displayed portrait at fit width 1333 px: parity needs the
        // stored height (4000), not the stored width.
        let expected = 4000.0 / 1333.0;
        assert!((max_zoom(&entry, egui::vec2(1333.0, 2000.0)) - expected).abs() < 1e-3);
    }

    #[test]
    fn max_zoom_should_stay_at_one_for_unknown_pixels() {
        let entry = PhotoEntry {
            id: cullr_core::PhotoId(1),
            rel_path: "a.nef".into(),
            label: cullr_core::Label::None,
            status: PhotoStatus::Pending,
            pixels: None,
            orientation: 1,
            rot_cw: 0,
            thumb_path: None,
            err_msg: None,
            jpeg_rel_path: None,
        };
        assert_eq!(max_zoom(&entry, egui::vec2(500.0, 500.0)), 1.0);
    }

    #[test]
    fn zoom_percent_should_report_the_native_resolution_fraction() {
        let fitted = egui::vec2(1500.0, 1000.0);
        let fit_pct = zoom_percent(Some(6000), FIT_ZOOM, fitted).expect("percent");
        assert!((fit_pct - 25.0).abs() < 1e-4);
        let parity_pct = zoom_percent(Some(6000), 4.0, fitted).expect("percent");
        assert!((parity_pct - 100.0).abs() < 1e-4);
    }

    #[test]
    fn zoom_percent_should_never_divide_by_missing_pixels_or_a_zero_viewport() {
        assert_eq!(zoom_percent(None, FIT_ZOOM, egui::vec2(500.0, 500.0)), None);
        assert_eq!(zoom_percent(Some(6000), FIT_ZOOM, egui::Vec2::ZERO), None);
    }

    #[test]
    fn marquee_zoom_step_should_land_the_region_on_the_viewport_center() {
        let viewport = egui::vec2(800.0, 600.0);
        let center = egui::pos2(400.0, 300.0);
        let fitted = egui::vec2(600.0, 400.0);
        // A 320×300 region left of and below the image center.
        let marquee = egui::Rect::from_min_size(egui::pos2(160.0, 190.0), egui::vec2(320.0, 300.0));

        let (zoom, pan) = marquee_zoom_step(
            marquee,
            center,
            FIT_ZOOM,
            egui::Vec2::ZERO,
            fitted,
            viewport,
            8.0,
        );

        // Contain picks the smaller factor; here the height constrains.
        assert!((zoom - 2.0).abs() < 1e-4);
        // The dragged region's image point must sit at the viewport
        // center afterwards; nothing here hits the edge clamps.
        let anchor = (marquee.center() - center) / fitted;
        let landed = center + pan + anchor * (fitted * zoom);
        assert!(landed.distance(center) < 1e-3, "landed at {landed:?}");
    }

    #[test]
    fn marquee_zoom_step_should_clamp_to_fit_for_oversized_regions() {
        let viewport = egui::vec2(800.0, 600.0);
        let fitted = egui::vec2(600.0, 400.0);
        let marquee = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 700.0));

        let (zoom, pan) = marquee_zoom_step(
            marquee,
            egui::pos2(400.0, 300.0),
            FIT_ZOOM,
            egui::vec2(60.0, 0.0),
            fitted,
            viewport,
            8.0,
        );

        assert!((zoom - FIT_ZOOM).abs() < 1e-6);
        assert_eq!(pan, egui::Vec2::ZERO);
    }

    #[test]
    fn marquee_zoom_step_should_clamp_to_pixel_parity_for_tiny_regions() {
        let viewport = egui::vec2(800.0, 600.0);
        let fitted = egui::vec2(600.0, 400.0);
        let marquee = egui::Rect::from_min_size(egui::pos2(300.0, 250.0), egui::vec2(10.0, 10.0));

        let (zoom, _) = marquee_zoom_step(
            marquee,
            egui::pos2(400.0, 300.0),
            FIT_ZOOM,
            egui::Vec2::ZERO,
            fitted,
            viewport,
            4.0,
        );

        assert!((zoom - 4.0).abs() < 1e-5);
    }

    fn detail(fields: impl FnOnce(&mut PhotoDetail)) -> PhotoDetail {
        let mut detail = PhotoDetail {
            id: cullr_core::PhotoId(1),
            rel_path: "IMG_0001.CR3".into(),
            label: cullr_core::Label::None,
            status: PhotoStatus::Ok,
            pixels: Some((6000, 4000)),
            orientation: 1,
            rot_cw: 0,
            preview_path: None,
            thumb_path: None,
            camera: None,
            lens: None,
            taken_at: None,
            shutter: None,
            aperture: None,
            iso: None,
            focal_mm: None,
            err_msg: None,
            jpeg_rel_path: None,
        };
        fields(&mut detail);
        detail
    }

    fn entry() -> PhotoEntry {
        PhotoEntry {
            id: cullr_core::PhotoId(1),
            rel_path: "IMG_0001.CR3".into(),
            label: cullr_core::Label::None,
            status: PhotoStatus::Ok,
            pixels: Some((6000, 4000)),
            orientation: 1,
            rot_cw: 0,
            thumb_path: None,
            err_msg: None,
            jpeg_rel_path: None,
        }
    }

    #[test]
    fn exif_line_should_join_present_fields_with_middle_dots() {
        let detail = detail(|d| {
            d.camera = Some("Canon EOS R6".to_owned());
            d.shutter = Some("1/250 s".to_owned());
            d.iso = Some(400);
        });

        assert_eq!(
            exif_line(&detail, &entry()),
            "Canon EOS R6 · 1/250 s · ISO 400"
        );
    }

    #[test]
    fn exif_line_should_trim_integral_aperture_and_focal_values() {
        let detail = detail(|d| {
            d.aperture = Some(8.0);
            d.focal_mm = Some(35.0);
        });

        assert_eq!(exif_line(&detail, &entry()), "f/8 · 35mm");
    }

    #[test]
    fn exif_line_should_keep_fractional_exposure_values() {
        let detail = detail(|d| {
            d.aperture = Some(2.8);
            d.focal_mm = Some(24.5);
        });

        assert_eq!(exif_line(&detail, &entry()), "f/2.8 · 24.5mm");
    }

    #[test]
    fn exif_line_should_fall_back_to_the_file_name_when_empty() {
        let detail = detail(|_| {});

        assert_eq!(exif_line(&detail, &entry()), "IMG_0001.CR3");
    }
}
