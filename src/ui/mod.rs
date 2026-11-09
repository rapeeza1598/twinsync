//! egui front end. Talks to the engine only through `Command` and `UiSnapshot`;
//! it never touches SQLite, the file system or macOS APIs, and never blocks on I/O.

mod activity;
mod overview;
mod settings;
pub mod strings;

use crate::config::Settings;
use crate::domain::Slot;
use crate::engine::{Command, Engine, UiSnapshot};
use crate::storage::ActivityFilter;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Overview,
    Activity,
    Settings,
}

pub struct UiState {
    pub page: Page,
    pub show_preview: bool,
    pub draft: Option<Settings>,
    pub exclusions_text: String,
    pub only_failures: bool,
    pub activity_slot: Option<Slot>,
    pub log_dir: PathBuf,
}

impl UiState {
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            page: Page::Overview,
            show_preview: false,
            draft: None,
            exclusions_text: String::new(),
            only_failures: false,
            activity_slot: None,
            log_dir,
        }
    }
}

pub struct Cx<'a> {
    pub snap: &'a UiSnapshot,
    pub engine: &'a Engine,
    pub state: &'a mut UiState,
    pub now: i64,
}

impl Cx<'_> {
    pub fn send(&self, c: Command) {
        self.engine.send(c);
    }
}

pub fn show(ui: &mut egui::Ui, cx: &mut Cx) {
    egui::Panel::left("nav")
        .resizable(false)
        .exact_size(150.0)
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.heading(crate::config::APP_NAME);
            ui.label(egui::RichText::new(strings::TAGLINE).small().weak());
            ui.add_space(12.0);
            for (page, label) in [
                (Page::Overview, strings::NAV_OVERVIEW),
                (Page::Activity, strings::NAV_ACTIVITY),
                (Page::Settings, strings::NAV_SETTINGS),
            ] {
                if ui.selectable_label(cx.state.page == page, label).clicked()
                    && cx.state.page != page
                {
                    cx.state.page = page;
                    if page == Page::Activity {
                        cx.send(Command::LoadActivity(ActivityFilter::default()));
                    }
                    if page == Page::Settings {
                        cx.state.draft = None;
                    }
                }
            }
        });
    egui::CentralPanel::default_margins().show(ui, |ui| match cx.state.page {
        Page::Overview => overview::show(ui, cx),
        Page::Activity => activity::show(ui, cx),
        Page::Settings => settings::show(ui, cx),
    });
    overview::dialogs(ui.ctx(), cx);
}
