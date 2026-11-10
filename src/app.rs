//! The eframe application: owns the engine handle, forwards snapshots to the UI.

use crate::config::{AppPaths, Theme};
use crate::engine::{Engine, UiSnapshot};
use crate::ui::{Cx, UiState};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct TwinSyncApp {
    engine: Engine,
    snap: Arc<UiSnapshot>,
    state: UiState,
    applied_theme: Option<Theme>,
}

impl TwinSyncApp {
    pub fn new(engine: Engine, paths: &AppPaths) -> Self {
        let snap = engine.snapshot();
        Self {
            engine,
            snap,
            state: UiState::new(paths.log_dir.clone(), paths.data_dir.clone()),
            applied_theme: None,
        }
    }
}

impl eframe::App for TwinSyncApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snap = self.engine.snapshot();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let theme = self.snap.settings.theme;
        if self.applied_theme != Some(theme) {
            ui.ctx().set_theme(match theme {
                Theme::System => egui::ThemePreference::System,
                Theme::Light => egui::ThemePreference::Light,
                Theme::Dark => egui::ThemePreference::Dark,
            });
            self.applied_theme = Some(theme);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let mut cx = Cx {
            snap: &self.snap,
            engine: &self.engine,
            state: &mut self.state,
            now,
        };
        crate::ui::show(ui, &mut cx);
        // Relative times ("5 min ago") need an occasional refresh even when idle.
        ui.ctx().request_repaint_after(Duration::from_secs(30));
    }
}
