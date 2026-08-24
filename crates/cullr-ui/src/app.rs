//! Cullr application shell: state machine root, event pump, theming, fonts.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use cullr_core::Db;
use cullr_core::PhotoEntry;
use cullr_core::ScanOptions;

use crate::views::Action;
use crate::views::grid::GridView;
use crate::views::home::HomeView;

/// How often the UI wakes up to poll for scan results while one runs.
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Events produced by background jobs and drained by the App every frame.
enum Event {
    /// A folder scan finished (successfully or not).
    ScanFinished {
        /// Root the job was launched for; used to drop stale results.
        root: PathBuf,
        /// Ordered index entries, or a user-facing error message.
        result: Result<Vec<PhotoEntry>, String>,
    },
}

/// Which screen is currently mounted.
enum Screen {
    Home(HomeView),
    Grid(GridView),
}

/// Root of the UI state machine; owns views and pumps core events per frame.
pub struct App {
    db: Arc<Db>,
    screen: Screen,
    events_tx: crossbeam_channel::Sender<Event>,
    events_rx: crossbeam_channel::Receiver<Event>,
    /// Root whose scan is in flight; results for anything else are stale
    /// (e.g. the user backed out mid-scan) and get dropped.
    scanning: Option<PathBuf>,
}

impl App {
    /// Configures fonts and theme before the first frame is painted.
    pub fn new(cc: &eframe::CreationContext<'_>, db: Arc<Db>) -> Self {
        install_fonts(&cc.egui_ctx);
        install_theme(&cc.egui_ctx);
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        Self {
            db,
            screen: Screen::Home(HomeView),
            events_tx,
            events_rx,
            scanning: None,
        }
    }

    /// Mounts the grid immediately and starts a background scan+sync so
    /// placeholders appear without waiting (SPEC §5.1).
    fn open_folder(&mut self, root: PathBuf) {
        tracing::info!(?root, "opening folder");
        self.scanning = Some(root.clone());
        self.screen = Screen::Grid(GridView::new(root.clone()));
        let db = Arc::clone(&self.db);
        let sender = self.events_tx.clone();
        // Scan is a cheap walk + tiny upserts (< 300 ms @ 10k per SPEC §8);
        // a plain thread suffices until the rayon ingest pool lands in T6.
        std::thread::spawn(move || {
            let result = scan_and_sync(&db, &root);
            let _sent = sender.send(Event::ScanFinished { root, result });
        });
    }

    /// Applies finished background jobs to the mounted screen and keeps the
    /// frame loop alive while work is outstanding.
    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events_rx.try_recv() {
            let Event::ScanFinished { root, result } = event;
            if self.scanning.as_ref() != Some(&root) {
                continue;
            }
            self.scanning = None;
            match (&mut self.screen, result) {
                (Screen::Grid(grid), outcome) => grid.apply_scan(outcome),
                (Screen::Home(_), _) => {}
            }
        }
        if self.scanning.is_some() {
            ctx.request_repaint_after(SCAN_POLL_INTERVAL);
        }
    }

    /// Executes one action produced by the active view.
    fn run_action(&mut self, action: Option<Action>) {
        match action {
            None => {}
            Some(Action::PickFolder) => {
                // Blocking native dialog; cancelled picks are a no-op.
                if let Some(picked) = rfd::FileDialog::new().pick_folder() {
                    self.open_folder(picked);
                }
            }
            Some(Action::OpenFolder(path)) => self.open_folder(path),
            Some(Action::BackToHome) => {
                self.scanning = None;
                self.screen = Screen::Home(HomeView);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events(ui.ctx());

        match &mut self.screen {
            Screen::Home(home) => {
                let mut action = None;
                egui::CentralPanel::default().show(ui, |ui| {
                    action = home.ui(ui, &self.db);
                });
                self.run_action(action);
            }
            Screen::Grid(grid) => {
                let mut action = None;
                egui::CentralPanel::default().show(ui, |ui| {
                    action = grid.ui(ui);
                });
                self.run_action(action);
            }
        }
    }
}

/// One scan pass over a folder: filesystem walk, index sync and recents
/// stamp. Runs off the UI thread; failures become user-facing strings.
fn scan_and_sync(db: &Db, root: &Path) -> Result<Vec<PhotoEntry>, String> {
    let scanned =
        cullr_core::scan_folder(root, ScanOptions::default()).map_err(|error| error.to_string())?;
    let entries = db
        .sync_scan(root, &scanned, ScanOptions::default())
        .map_err(|error| error.to_string())?;
    db.touch_root(root, cullr_core::now_millis())
        .map_err(|error| error.to_string())?;
    tracing::info!(count = entries.len(), ?root, "folder synced");
    Ok(entries)
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let inter = egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf"));
    fonts.font_data.insert("inter_regular".into(), inter.into());
    // Inter first, stock egui fonts as fallback for glyphs Inter lacks.
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "inter_regular".into());
    ctx.set_fonts(fonts);
}

fn install_theme(ctx: &egui::Context) {
    use crate::theme::{ACCENT, BG, MUTED, PANEL, TEXT};

    // Global style so both light/dark theme variants share our palette;
    // the app is dark-only by design.
    ctx.global_style_mut(|style| {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(TEXT);
        v.panel_fill = PANEL;
        v.window_fill = PANEL;
        v.extreme_bg_color = BG;
        v.faint_bg_color = BG;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke.color = BG;
        v.widgets.inactive.fg_stroke.color = TEXT;
        v.widgets.hovered.fg_stroke.color = ACCENT;
        v.widgets.active.fg_stroke.color = ACCENT;
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        // Secondary text (hints, tile strips) uses the muted gray.
        v.weak_text_color = Some(MUTED);
    });
}
