//! Cullr — Photo Mechanic-style culling tool for RAW photographs.

mod app;

use eframe::egui;

fn main() -> anyhow::Result<()> {
    init_tracing();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Cullr")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Cullr",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )?;
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
