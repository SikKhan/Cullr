//! Cullr application shell: state machine root, theming, font setup.

use eframe::egui;

mod theme {
    use eframe::egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x16, 0x17, 0x1A);
    pub const PANEL: Color32 = Color32::from_rgb(0x1E, 0x20, 0x23);
    pub const TEXT: Color32 = Color32::from_rgb(0xD7, 0xD9, 0xDC);
    pub const ACCENT: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
}

/// Root of the UI state machine; owns views and pumps core events per frame.
pub struct App;

impl App {
    /// Configures fonts and theme before the first frame is painted.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        install_theme(&cc.egui_ctx);
        Self
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Themed empty shell until the Home view lands in T4.
        egui::CentralPanel::default().show(ui, |_| {});
    }
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
    // Global style so both light/dark theme variants share our palette;
    // the app is dark-only by design.
    ctx.global_style_mut(|style| {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(theme::TEXT);
        v.panel_fill = theme::PANEL;
        v.window_fill = theme::PANEL;
        v.extreme_bg_color = theme::BG;
        v.faint_bg_color = theme::BG;
        v.selection.bg_fill = theme::ACCENT;
        v.selection.stroke.color = theme::BG;
        v.widgets.inactive.fg_stroke.color = theme::TEXT;
        v.widgets.hovered.fg_stroke.color = theme::ACCENT;
        v.widgets.active.fg_stroke.color = theme::ACCENT;
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
    });
}
