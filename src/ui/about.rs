use super::{Cx, strings as t};
use crate::about::{REPO_URL, build_info};
use egui::RichText;

/// The About window. Opened from the sidebar; closes with its own button or the title bar.
pub(super) fn window(ctx: &egui::Context, cx: &mut Cx) {
    if !cx.state.show_about {
        return;
    }
    let mut open = true;
    let info = build_info();
    egui::Window::new(format!("About {}", crate::config::APP_NAME))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.heading(crate::config::APP_NAME);
            ui.label(RichText::new(t::TAGLINE).weak());
            ui.add_space(4.0);
            ui.label(info.version_line());
            ui.separator();

            ui.label(t::ABOUT_SUMMARY);
            ui.add_space(4.0);
            for promise in t::ABOUT_PROMISES {
                ui.label(format!("• {promise}"));
            }
            ui.add_space(4.0);
            ui.label(RichText::new(t::ABOUT_LIMITS).small().weak());
            ui.separator();

            ui.label(RichText::new(t::PRIVACY_NOTE).small());
            ui.add_space(4.0);
            ui.label(format!("Data folder: {}", cx.state.data_dir.display()));
            ui.label(format!("Log folder: {}", cx.state.log_dir.display()));
            ui.label(format!("Database schema: v{}", info.schema_version));
            ui.hyperlink_to("Source code on GitHub", REPO_URL);
            ui.add_space(6.0);
            if ui.button("Close").clicked() {
                cx.state.show_about = false;
            }
        });
    if !open {
        cx.state.show_about = false;
    }
}
