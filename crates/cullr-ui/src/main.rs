//! Cullr — Photo Mechanic-style culling tool for RAW photographs.

mod app;
mod theme;
mod views;

use std::sync::Arc;

use eframe::egui;

fn main() -> anyhow::Result<()> {
    init_tracing();

    let db = open_index_db()?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Cullr")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Cullr",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, Arc::new(db))))),
    )?;
    Ok(())
}

/// Opens the global index database at `<cache>/cullr/index.db` (SPEC §4).
///
/// The database is essential — photo ids and recents live there — so a
/// failure aborts startup instead of degrading into a broken session.
fn open_index_db() -> anyhow::Result<cullr_core::Db> {
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine the system cache directory"))?;
    let path = cache_dir.join("cullr").join("index.db");
    Ok(cullr_core::Db::open(&path)?)
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
