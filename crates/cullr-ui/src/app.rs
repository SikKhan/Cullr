//! Cullr application shell: state machine root, event pump, theming, fonts.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use cullr_core::Cache;
use cullr_core::Db;
use cullr_core::IngestEvent;
use cullr_core::IngestPipeline;
use cullr_core::PhotoEntry;
use cullr_core::PhotoId;
use cullr_core::PhotoStatus;
use cullr_core::ScanOptions;

use crate::tex::Textures;
use crate::views::Action;
use crate::views::grid::GridView;
use crate::views::home::HomeView;
use crate::views::modals::Modals;
use crate::views::widgets;

/// Repaint cadence while background work (scan, ingest, decode) is
/// outstanding; short enough that spinners and progressive fills look live.
const FRAME_PULSE: Duration = Duration::from_millis(33);

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

/// Which screen is currently mounted. The grid state is boxed: it is by
/// far the largest variant and swapping screens must stay cheap.
enum Screen {
    Home(HomeView),
    Grid(Box<GridView>),
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
    /// Ingest engine; `None` only when the platform has no cache directory,
    /// in which case tiles stay placeholders (graceful degradation).
    pipeline: Option<IngestPipeline>,
    ingest_rx: Option<crossbeam_channel::Receiver<IngestEvent>>,
    /// Generation of the batch currently mounted on the grid; events from
    /// any other generation belong to a superseded folder and are dropped.
    ingest_generation: u64,
    /// Thumbnail decode + GPU upload service feeding grid cells.
    textures: Textures,
    /// Help overlay + About dialog (SPEC §10 T14); drawn above whatever
    /// screen is mounted, which stays suspended while either is open.
    modals: Modals,
}

impl App {
    /// Configures fonts and theme before the first frame is painted.
    pub fn new(cc: &eframe::CreationContext<'_>, db: Arc<Db>) -> Self {
        install_fonts(&cc.egui_ctx);
        install_theme(&cc.egui_ctx);
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let pipeline = match Cache::system_default() {
            Ok(cache) => {
                let (pipeline, ingest_rx) = IngestPipeline::new(cache, Arc::clone(&db));
                Some((pipeline, ingest_rx))
            }
            Err(error) => {
                tracing::warn!(%error, "no cache directory; preview ingestion disabled");
                None
            }
        };
        let (pipeline, ingest_rx) = pipeline
            .map(|(p, rx)| (Some(p), Some(rx)))
            .unwrap_or_else(|| (None, None));
        Self {
            db,
            screen: Screen::Home(HomeView),
            events_tx,
            events_rx,
            scanning: None,
            pipeline,
            ingest_rx,
            ingest_generation: 0,
            textures: Textures::new(),
            modals: Modals::default(),
        }
    }

    /// Mounts the grid immediately and starts a background scan+sync so
    /// placeholders appear without waiting (SPEC §5.1); any ingest still
    /// running for the previous folder is cancelled up front.
    pub(crate) fn open_folder(&mut self, root: PathBuf) {
        tracing::info!(?root, "opening folder");
        if let Some(pipeline) = &self.pipeline {
            pipeline.cancel();
        }
        self.scanning = Some(root.clone());
        let auto_advance = widgets::load_auto_advance(&self.db);
        self.screen = Screen::Grid(Box::new(GridView::new(root.clone(), auto_advance)));
        let db = Arc::clone(&self.db);
        let sender = self.events_tx.clone();
        // Scan is a cheap walk + tiny upserts (< 300 ms @ 10k per SPEC §8);
        // a plain thread suffices — extraction itself runs on the rayon pool.
        std::thread::spawn(move || {
            let result = scan_and_sync(&db, &root);
            let _sent = sender.send(Event::ScanFinished { root, result });
        });
    }

    /// Feeds the freshly synced folder's pending photos to the ingest
    /// pipeline and tells the grid a new batch started.
    fn start_ingest(&mut self, root: &Path) {
        let Some(pipeline) = &self.pipeline else {
            return;
        };
        let pending = match self.db.pending_photos(root) {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "cannot list pending photos");
                return;
            }
        };
        if pending.is_empty() {
            return;
        }
        let total = pending.len();
        self.ingest_generation = pipeline.enqueue(pending);
        if let Screen::Grid(grid) = &mut self.screen {
            grid.begin_ingest(total);
        }
    }

    /// Applies finished background jobs to the mounted screen.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            let Event::ScanFinished { root, result } = event;
            if self.scanning.as_ref() != Some(&root) {
                continue;
            }
            self.scanning = None;
            match (&mut self.screen, result) {
                (Screen::Grid(grid), outcome) => {
                    // Purge texture state from any other folder before the
                    // sheet mounts: cancelled batches stop occupying decode
                    // and GPU budget, while this folder's warm cache stays.
                    if let Ok(entries) = &outcome {
                        let keep: HashSet<PhotoId> = entries.iter().map(|e| e.id).collect();
                        self.textures.retain(&keep);
                    }
                    grid.apply_scan(outcome);
                    self.start_ingest(&root);
                }
                (Screen::Home(_), _) => {}
            }
        }
        if let Some(rx) = &self.ingest_rx {
            // Two-phase drain: the receiver borrows `self`, so events are
            // collected before they are applied mutably.
            let mut arrived = Vec::new();
            while let Ok(event) = rx.try_recv() {
                arrived.push(event);
            }
            for event in arrived {
                self.apply_ingest_event(event);
            }
        }
    }

    /// Routes one pipeline event into the grid; events from a superseded
    /// batch (folder switched mid-run) are dropped by generation. Each
    /// result re-reads its index row so cells gain thumb path, pixel size
    /// and error messages exactly when ingest produced them.
    fn apply_ingest_event(&mut self, event: IngestEvent) {
        let (id, status) = match event {
            IngestEvent::Ingested(id) => (id, PhotoStatus::Ok),
            IngestEvent::Failed(id) => (id, PhotoStatus::Error),
            IngestEvent::Finished { generation } => {
                if generation == self.ingest_generation
                    && let Screen::Grid(grid) = &mut self.screen
                {
                    grid.finish_ingest();
                }
                return;
            }
        };
        let fresh = self.lookup_entry(id);
        if let Screen::Grid(grid) = &mut self.screen {
            grid.apply_ingest_result(id, status, fresh);
        }
    }

    /// Best-effort row refresh for one ingest event; failures degrade to a
    /// status-only cell update rather than dropping the event.
    fn lookup_entry(&self, id: cullr_core::PhotoId) -> Option<PhotoEntry> {
        match self.db.photo_entry(id) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, ?id, "cannot refresh ingested photo");
                None
            }
        }
    }

    /// Forwards changed visible-pending sets to the ingest queue so tiles
    /// near the viewport extract first (SPEC §5.2 priority ping).
    fn pump_priority(&mut self) {
        let Screen::Grid(grid) = &mut self.screen else {
            return;
        };
        if let Some(ids) = grid.take_priority_ping()
            && let Some(pipeline) = &self.pipeline
        {
            pipeline.prioritize(&ids);
        }
    }

    /// Keeps the frame loop alive only while background work is outstanding;
    /// idle screens cost zero wakeups.
    fn schedule_repaint(&self, ctx: &egui::Context) {
        let busy = self.scanning.is_some()
            || matches!(&self.screen, Screen::Grid(grid) if grid.is_ingesting())
            || self.textures.busy();
        if busy {
            ctx.request_repaint_after(FRAME_PULSE);
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
                if let Some(pipeline) = &self.pipeline {
                    pipeline.cancel();
                }
                self.screen = Screen::Home(HomeView);
            }
            Some(Action::ShowAbout) => self.modals.open_about(),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Upload freshly decoded thumbnails before drawing so cells can
        // paint them this same frame.
        self.textures.sync(&ctx);
        self.drain_events();
        // Modal open/close intents are read before the views so a closing
        // keypress never falls through into the screen underneath; while
        // a modal is up the views render but stay input-suspended.
        self.modals.pump_input(&ctx);
        let suspended = self.modals.any();

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
                    action = grid.ui(ui, &self.db, &mut self.textures, suspended);
                });
                if !suspended {
                    self.run_action(action);
                }
            }
        }

        // Dialogs paint last so they sit above every layer the screens
        // created this frame (SPEC §6 `?` overlay, About).
        self.modals.draw(&ctx);

        self.pump_priority();
        self.schedule_repaint(&ctx);
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
