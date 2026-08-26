//! Small shared widgets and input mappings for the culling workflow.
//!
//! Everything here is view-agnostic: the grid and the loupe both label
//! photos with digits `1..5` / `0` (SPEC §6), both show the palette as
//! clickable swatches, and both honor the persisted auto-advance toggle.
//! The filter bar (chips + counts, SPEC §6) is also defined here so its
//! [`LabelFilter`] state type stays decoupled from any one view.

use eframe::egui;

use cullr_core::Db;
use cullr_core::Label;

use crate::theme;

/// kv row backing the auto-advance toggle so it survives restarts.
const AUTO_ADVANCE_KEY: &str = "auto_advance";

/// kv row backing the export mode so it survives restarts.
const EXPORT_MODE_KEY: &str = "export_mode";

/// Bit for [`Label::None`] inside [`LabelFilter::mask`].
const UNLABELED_BIT: u8 = 1;
/// Bitmask selecting every colored label (`Red..=Purple`).
const LABELED_MASK: u8 = 0b0011_1110;

/// Active label-filter selection over a folder's photos (SPEC §6).
///
/// A bitmask of selected labels with OR semantics: an empty selection
/// shows everything, `{unlabeled}` shows only unmarked photos and any mix
/// of color bits shows photos carrying at least one of them. The `F` key
/// cycles the three presets All → Labeled → Unlabeled; chips toggle
/// individual labels on top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabelFilter {
    /// Bit per [`Label::to_u8`] value; zero means "no filtering".
    mask: u8,
}

impl LabelFilter {
    /// The unfiltered preset.
    pub const fn all() -> Self {
        Self { mask: 0 }
    }

    /// The Labeled preset: every photo carrying any color mark.
    pub const fn labeled() -> Self {
        Self { mask: LABELED_MASK }
    }

    /// The Unlabeled preset: only photos without a mark.
    pub const fn unlabeled() -> Self {
        Self {
            mask: UNLABELED_BIT,
        }
    }

    /// `true` when nothing is filtered out.
    pub fn is_all(self) -> bool {
        self.mask == 0
    }

    /// Whether photos carrying `label` survive this filter.
    ///
    /// With no selection everything matches; otherwise membership is a
    /// single bit test, which is what keeps a refilter over 10k rows
    /// inside one frame (SPEC §10 T11 acceptance).
    pub fn matches(self, label: Label) -> bool {
        self.mask == 0 || self.mask & (1 << label.to_u8()) != 0
    }

    /// Whether `label`'s chip is currently toggled on.
    pub fn is_selected(self, label: Label) -> bool {
        !self.is_all() && self.mask & (1 << label.to_u8()) != 0
    }

    /// Flips one label's chip; toggling the last bit off returns to All.
    pub fn toggle(&mut self, label: Label) {
        self.mask ^= 1 << label.to_u8();
    }

    /// Drops back to the unfiltered preset.
    pub fn clear(&mut self) {
        self.mask = 0;
    }

    /// Advances All → Labeled → Unlabeled → All (SPEC §6 keyboard map).
    /// A custom chip mix counts as "before Labeled", so pressing `F`
    /// always lands on a named preset in at most two presses.
    pub fn cycle_preset(&mut self) {
        *self = if *self == Self::unlabeled() {
            Self::all()
        } else if *self == Self::labeled() {
            Self::unlabeled()
        } else {
            Self::labeled()
        };
    }
}

/// Which filter chip the user activated in [`filter_chips`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterChip {
    /// The `[All]` reset chip.
    All,
    /// One of the six label chips (`○` or a color dot).
    Label(Label),
}

/// Sort order of the contact sheet (SPEC §6: filename / taken_at).
///
/// Pure data so the grid can sort its filtered view in-memory and keep
/// loupe navigation, shift-click ranges and the cursor on one order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    /// File name, case-insensitively.
    #[default]
    FileName,
    /// EXIF capture time; photos without a stamp sort last.
    TakenAt,
}

impl SortKey {
    /// Flips between the two orders; the control is a single cycling
    /// pill because two orders need no menu.
    pub fn cycle(self) -> Self {
        match self {
            Self::FileName => Self::TakenAt,
            Self::TakenAt => Self::FileName,
        }
    }

    /// Human name shown inside the sort pill.
    pub fn label(self) -> &'static str {
        match self {
            Self::FileName => "filename",
            Self::TakenAt => "capture time",
        }
    }
}

/// Which side of RAW+JPEG pairs an export copies (SPEC §6 export).
///
/// Pure data so the grid can scope [`crate::views::grid::GridView`]'s
/// file set and the export pill can render the choice; the core's
/// `export_files` stays agnostic and copies whatever list it is given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportMode {
    /// The RAW plus its companion JPEG, back to back.
    #[default]
    All,
    /// Only the RAW originals; companion JPEGs stay behind.
    RawOnly,
    /// Only companion JPEGs; photos without one contribute nothing.
    JpegOnly,
}

impl ExportMode {
    /// Human name shown inside the export scope menu.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all files",
            Self::RawOnly => "RAW only",
            Self::JpegOnly => "JPEG only",
        }
    }

    /// Parses the persisted kv representation, rejecting unknown values.
    pub fn from_kv(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "raw" => Some(Self::RawOnly),
            "jpeg" => Some(Self::JpegOnly),
            _ => None,
        }
    }

    /// The persisted kv representation of this mode.
    pub fn to_kv(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::RawOnly => "raw",
            Self::JpegOnly => "jpeg",
        }
    }
}

/// Draws the sort pill (`Sort: filename`) and reports the order to apply,
/// if the user clicked. Styled after [`filter_chips`] so both bars read
/// as one control language.
pub fn sort_pill(ui: &mut egui::Ui, current: SortKey) -> Option<SortKey> {
    let text_font = egui::FontId::proportional(12.0);
    let galley = ui.painter().layout_no_wrap(
        format!("Sort: {}", current.label()),
        text_font,
        egui::Color32::WHITE,
    );
    let size = egui::vec2(CHIP_PADDING * 2.0 + galley.size().x, CHIP_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, CHIP_HEIGHT / 2.0, theme::PANEL);
    painter.rect_stroke(
        rect,
        CHIP_HEIGHT / 2.0,
        egui::Stroke::new(1.0, theme::MUTED.gamma_multiply(0.45)),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        egui::pos2(
            rect.left() + CHIP_PADDING,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        theme::TEXT,
    );
    let clicked = response.clicked();
    response.on_hover_text("Toggle sort order — arrows and Shift-click follow it");
    clicked.then(|| current.cycle())
}

/// Pill height shared by every chip.
pub(crate) const CHIP_HEIGHT: f32 = 22.0;
/// Horizontal padding inside a chip.
const CHIP_PADDING: f32 = 9.0;
/// Gap between chips.
const CHIP_GAP: f32 = 6.0;
/// Diameter of the colored dot inside label chips.
const CHIP_DOT: f32 = 7.0;
/// Space between the dot and the count text.
const CHIP_DOT_GAP: f32 = 5.0;

/// Draws the filter bar: `[All] [○ n] [R n][Y n][G n][B n][P n]` pills
/// with per-chip photo counts (SPEC §6). Active chips glow in their own
/// accent; reports the clicked chip so the caller owns the state change.
pub fn filter_chips(
    ui: &mut egui::Ui,
    filter: LabelFilter,
    counts: &[usize; 6],
) -> Option<FilterChip> {
    let mut picked = None;
    ui.spacing_mut().item_spacing.x = CHIP_GAP;
    ui.horizontal(|ui| {
        if draw_chip(
            ui,
            &ChipLook::All(filter.is_all()),
            None,
            "show every photo",
        ) {
            picked = Some(FilterChip::All);
        }
        for label in [
            Label::None,
            Label::Red,
            Label::Yellow,
            Label::Green,
            Label::Blue,
            Label::Purple,
        ] {
            let count = counts[label.to_u8() as usize];
            let look = match label {
                Label::None => ChipLook::Unlabeled(filter.is_selected(label)),
                color => ChipLook::Color(theme::label_color(color), filter.is_selected(color)),
            };
            let name = format!("{}-only", label_name(label));
            if draw_chip(ui, &look, Some(count), &name) {
                picked = Some(FilterChip::Label(label));
            }
        }
    });
    picked
}

/// Visual recipe of one filter chip: its accent source and active state.
#[derive(Clone, Copy)]
enum ChipLook {
    /// Reset chip; lit while no filter is engaged.
    All(bool),
    /// Hollow-circle chip for unmarked photos.
    Unlabeled(bool),
    /// Color-dot chip for one label color.
    Color(egui::Color32, bool),
}

impl ChipLook {
    fn active(self) -> bool {
        match self {
            ChipLook::All(active) | ChipLook::Unlabeled(active) | ChipLook::Color(_, active) => {
                active
            }
        }
    }

    fn accent(self) -> egui::Color32 {
        match self {
            ChipLook::All(..) => theme::ACCENT,
            ChipLook::Unlabeled(..) => theme::TEXT,
            ChipLook::Color(color, ..) => color,
        }
    }

    fn has_dot(self) -> bool {
        !matches!(self, ChipLook::All(_))
    }
}

/// Paints one pill chip and reports whether it was clicked. Width comes
/// from measuring the count text, so `9 999+` counts still fit cleanly.
fn draw_chip(ui: &mut egui::Ui, look: &ChipLook, count: Option<usize>, hint: &str) -> bool {
    let text_font = egui::FontId::proportional(12.0);
    let text = count.map_or_else(String::new, grouped);
    let galley = ui
        .painter()
        .layout_no_wrap(text, text_font, egui::Color32::WHITE);
    let dot_width = if look.has_dot() {
        CHIP_DOT + CHIP_DOT_GAP
    } else {
        0.0
    };
    let size = egui::vec2(
        CHIP_PADDING * 2.0 + dot_width + galley.size().x,
        CHIP_HEIGHT,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    let (active, accent) = (look.active(), look.accent());
    // Active chips get a translucent wash of their own accent plus a
    // crisp ring; idle ones stay quiet so the bar reads as available
    // filters rather than buttons demanding clicks.
    let fill = if active {
        accent.gamma_multiply(0.22)
    } else {
        theme::PANEL
    };
    let stroke = if active {
        egui::Stroke::new(1.25, accent)
    } else {
        egui::Stroke::new(1.0, theme::MUTED.gamma_multiply(0.45))
    };
    painter.rect_filled(rect, CHIP_HEIGHT / 2.0, fill);
    painter.rect_stroke(rect, CHIP_HEIGHT / 2.0, stroke, egui::StrokeKind::Inside);

    let mut cursor_x = rect.left() + CHIP_PADDING;
    if let ChipLook::Color(color, _) = look {
        painter.circle_filled(
            egui::pos2(cursor_x + CHIP_DOT / 2.0, rect.center().y),
            CHIP_DOT / 2.0,
            *color,
        );
        cursor_x += CHIP_DOT + CHIP_DOT_GAP;
    } else if look.has_dot() {
        painter.circle_stroke(
            egui::pos2(cursor_x + CHIP_DOT / 2.0, rect.center().y),
            CHIP_DOT / 2.0,
            egui::Stroke::new(1.25, theme::MUTED),
        );
        cursor_x += CHIP_DOT + CHIP_DOT_GAP;
    }
    painter.galley(
        egui::pos2(cursor_x, rect.center().y - galley.size().y / 2.0),
        galley,
        if active { theme::TEXT } else { theme::MUTED },
    );

    let clicked = response.clicked();
    response.on_hover_text(match count {
        Some(count) => format!(
            "{hint} · {} photo{}",
            grouped(count),
            if count == 1 { "" } else { "s" }
        ),
        None => hint.to_owned(),
    });
    clicked
}

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

/// Direction of a manual display rotation (SPEC §6 keyboard map).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotateDir {
    /// `[`: quarter-turn counter-clockwise.
    CounterClockwise,
    /// `]`: quarter-turn clockwise.
    Clockwise,
}

impl RotateDir {
    /// Signed quarter-turn step to add to a photo's stored `rot_cw`.
    pub fn delta(self) -> i8 {
        match self {
            Self::CounterClockwise => -1,
            Self::Clockwise => 1,
        }
    }
}

/// The manual rotation key pressed this frame, if any. Works in whichever
/// view is on screen: grid tiles (selection or cursor) and the loupe.
pub fn pressed_rotate_key(ctx: &egui::Context) -> Option<RotateDir> {
    ctx.input(|input| {
        if input.key_pressed(egui::Key::OpenBracket) {
            Some(RotateDir::CounterClockwise)
        } else if input.key_pressed(egui::Key::CloseBracket) {
            Some(RotateDir::Clockwise)
        } else {
            None
        }
    })
}

/// Wraps a quarter-turn count into `0..4` after applying `delta`; pure so
/// both views share one wrap rule.
pub fn turned(current: u8, delta: i8) -> u8 {
    (i16::from(current % 4) + i16::from(delta)).rem_euclid(4) as u8
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

/// Loads the persisted export mode, defaulting to all-files: copying
/// both sides of every pair is the loss-free choice, so narrowing is
/// always an explicit act. Read failures degrade to the default rather
/// than blocking startup.
pub fn load_export_mode(db: &Db) -> ExportMode {
    match db.kv_get(EXPORT_MODE_KEY) {
        Ok(Some(value)) => ExportMode::from_kv(&value).unwrap_or_default(),
        Ok(None) => ExportMode::default(),
        Err(error) => {
            tracing::warn!(%error, "cannot read export mode setting");
            ExportMode::default()
        }
    }
}

/// Persists the export mode; failures are logged and otherwise ignored
/// because the in-memory state already drives the UI.
pub fn store_export_mode(db: &Db, mode: ExportMode) {
    if let Err(error) = db.kv_set(EXPORT_MODE_KEY, mode.to_kv()) {
        tracing::warn!(%error, "cannot persist export mode");
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

/// Human name for a label, used in tooltips and chip hints.
fn label_name(label: Label) -> &'static str {
    match label {
        Label::None => "unlabeled",
        Label::Red => "red",
        Label::Yellow => "yellow",
        Label::Green => "green",
        Label::Blue => "blue",
        Label::Purple => "purple",
    }
}

/// Thousands grouping with narrow no-break spaces, as in `3 210`.
///
/// Lives here because the loupe's position pill, the status stats and the
/// filter-chip counts all render user-facing magnitudes.
pub fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * '\u{202f}'.len_utf8());
    for (index, digit) in digits.chars().enumerate() {
        // Separator goes where exactly three digits remain behind it.
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(digit);
    }
    out
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

    #[test]
    fn load_export_mode_should_default_to_all_and_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Db::open(&dir.path().join("index.db")).expect("open db");

        assert_eq!(
            load_export_mode(&db),
            ExportMode::All,
            "unset key defaults to all files"
        );

        store_export_mode(&db, ExportMode::JpegOnly);

        assert_eq!(load_export_mode(&db), ExportMode::JpegOnly);

        store_export_mode(&db, ExportMode::RawOnly);

        assert_eq!(load_export_mode(&db), ExportMode::RawOnly);
    }

    #[test]
    fn load_export_mode_should_degrade_to_all_on_unknown_values() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Db::open(&dir.path().join("index.db")).expect("open db");

        db.kv_set(EXPORT_MODE_KEY, "bogus").expect("write kv");

        assert_eq!(load_export_mode(&db), ExportMode::All);
    }

    #[test]
    fn export_mode_kv_should_round_trip_every_variant() {
        for mode in [ExportMode::All, ExportMode::RawOnly, ExportMode::JpegOnly] {
            assert_eq!(ExportMode::from_kv(mode.to_kv()), Some(mode));
        }
        assert_eq!(ExportMode::from_kv("nope"), None);
    }

    #[test]
    fn empty_filter_should_match_every_label() {
        let filter = LabelFilter::all();
        for label in Label::ALL {
            assert!(filter.matches(label), "{label:?} must survive All");
            assert!(!filter.is_selected(label));
        }
    }

    #[test]
    fn labeled_preset_should_hide_unmarked_photos_only() {
        let filter = LabelFilter::labeled();
        assert!(!filter.matches(Label::None));
        for color in [
            Label::Red,
            Label::Yellow,
            Label::Green,
            Label::Blue,
            Label::Purple,
        ] {
            assert!(filter.matches(color), "{color:?} must survive Labeled");
        }
    }

    #[test]
    fn unlabeled_preset_should_keep_only_unmarked_photos() {
        let filter = LabelFilter::unlabeled();
        assert!(filter.matches(Label::None));
        assert!(!filter.matches(Label::Green));
    }

    #[test]
    fn toggled_chips_should_select_labels_with_or_semantics() {
        let mut filter = LabelFilter::all();
        filter.toggle(Label::Red);
        filter.toggle(Label::Blue);
        assert!(!filter.is_all());
        assert!(filter.is_selected(Label::Red));
        assert!(filter.matches(Label::Blue));
        assert!(!filter.matches(Label::Yellow), "untoggled colors stay out");
        assert!(!filter.matches(Label::None));
        filter.toggle(Label::Red);

        assert!(
            !filter.matches(Label::Red),
            "clicking an active chip deselects it"
        );
        assert!(filter.matches(Label::Blue), "the other chip survives");
    }

    #[test]
    fn cycle_preset_should_walk_all_labeled_unlabeled_and_back() {
        let mut filter = LabelFilter::all();
        filter.cycle_preset();
        assert_eq!(filter, LabelFilter::labeled());
        filter.cycle_preset();
        assert_eq!(filter, LabelFilter::unlabeled());
        filter.cycle_preset();
        assert!(filter.is_all(), "the cycle must close back on All");
    }

    #[test]
    fn cycle_preset_should_land_on_a_named_preset_from_custom_mixes() {
        let mut filter = LabelFilter::all();
        filter.toggle(Label::Purple);

        filter.cycle_preset();

        assert_eq!(filter, LabelFilter::labeled(), "custom mixes act as All");
    }

    #[test]
    fn clear_should_return_to_the_unfiltered_preset() {
        let mut filter = LabelFilter::unlabeled();

        filter.clear();

        assert!(filter.is_all());
        assert!(filter.matches(Label::Red));
    }

    #[test]
    fn grouped_should_insert_narrow_spaces_every_three_digits() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(142), "142");
        assert_eq!(grouped(3210), "3\u{202f}210");
        assert_eq!(grouped(1_234_567), "1\u{202f}234\u{202f}567");
    }

    #[test]
    fn sort_key_should_cycle_between_filename_and_capture_time() {
        assert_eq!(SortKey::default(), SortKey::FileName);
        assert_eq!(SortKey::FileName.cycle(), SortKey::TakenAt);

        assert_eq!(
            SortKey::TakenAt.cycle(),
            SortKey::FileName,
            "the cycle closes"
        );
    }

    #[test]
    fn sort_key_should_name_both_orders_for_the_pill() {
        assert!(!SortKey::FileName.label().is_empty());
        assert!(!SortKey::TakenAt.label().is_empty());
    }

    #[test]
    fn turned_should_wrap_quarter_turns_into_canonical_range() {
        assert_eq!(turned(0, RotateDir::Clockwise.delta()), 1);
        assert_eq!(turned(3, RotateDir::Clockwise.delta()), 0, "CW wraps");
        assert_eq!(
            turned(0, RotateDir::CounterClockwise.delta()),
            3,
            "CCW wraps"
        );
        // Repeated presses keep cycling without ever leaving 0..4.
        let mut turns = 2;
        for delta in [1, 1, -1, -1] {
            turns = turned(turns, delta);
        }
        assert_eq!(turns, 2, "two CW then two CCW returns to start");
    }
}
