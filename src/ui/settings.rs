use super::{Cx, strings as t};
use crate::config::{Settings, Theme, default_exclusions};
use crate::engine::Command;
use egui::RichText;

pub(super) fn show(ui: &mut egui::Ui, cx: &mut Cx) {
    ui.heading(t::NAV_SETTINGS);
    if cx.state.draft.is_none() {
        cx.state.exclusions_text = cx.snap.settings.exclusions.join("\n");
        cx.state.draft = Some(cx.snap.settings.clone());
    }
    let mut draft: Settings = cx.state.draft.clone().unwrap_or_default();
    let mut text = std::mem::take(&mut cx.state.exclusions_text);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.checkbox(&mut draft.auto_sync, t::AUTO_SYNC);
        ui.horizontal(|ui| {
            ui.label("Wait for changes to settle (ms):");
            ui.add(egui::DragValue::new(&mut draft.debounce_ms).range(200..=60_000));
        });
        ui.horizontal(|ui| {
            ui.label("Check everything again every (minutes):");
            ui.add(egui::DragValue::new(&mut draft.reconcile_minutes).range(1..=1_440));
        });
        ui.label(RichText::new(t::AUTO_SYNC_HDD_NOTE).small().weak());
        ui.separator();

        ui.label(RichText::new("Files to skip").strong());
        ui.label(
            RichText::new("One pattern per line. Folders such as node_modules or build are NOT skipped unless you add them.")
                .small()
                .weak(),
        );
        ui.add(egui::TextEdit::multiline(&mut text).desired_rows(8).desired_width(f32::INFINITY));
        if ui.small_button("Restore defaults").clicked() {
            text = default_exclusions().join("\n");
        }
        ui.separator();

        ui.label(RichText::new("Verification").strong());
        ui.checkbox(
            &mut draft.hash_all_source_files,
            "Re-hash every source file on each scan (slower; catches changes that keep size and date)",
        );
        ui.label(
            RichText::new("Copies are always read back and compared before they replace anything. Full Verify on a drive card re-reads the whole drive.")
                .small()
                .weak(),
        );
        ui.horizontal(|ui| {
            ui.label("Keep at least this much free (MB):");
            ui.add(egui::DragValue::new(&mut draft.free_space_reserve_mb).range(0..=1_000_000));
        });
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Theme:");
            egui::ComboBox::from_id_salt("theme")
                .selected_text(format!("{:?}", draft.theme))
                .show_ui(ui, |ui| {
                    for opt in [Theme::System, Theme::Light, Theme::Dark] {
                        ui.selectable_value(&mut draft.theme, opt, format!("{opt:?}"));
                    }
                });
        });
        ui.label(
            RichText::new("Performance profiles are set per drive on the Overview page. They change how gently TwinSync reads and writes; they never reduce safety checks.")
                .small()
                .weak(),
        );
        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                draft.exclusions = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                cx.send(Command::SaveSettings(draft.clone()));
            }
            if ui.button("Open logs folder").clicked() {
                // Direct argument, no shell.
                let _ = std::process::Command::new("/usr/bin/open").arg(&cx.state.log_dir).spawn();
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new(t::PRIVACY_NOTE).small().weak());
    });
    cx.state.draft = Some(draft);
    cx.state.exclusions_text = text;
}
