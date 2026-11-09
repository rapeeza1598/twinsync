use super::{Cx, strings as t};
use crate::domain::Slot;
use crate::engine::Command;
use egui::RichText;
use egui_extras::{Column, TableBuilder};

pub(super) fn show(ui: &mut egui::Ui, cx: &mut Cx) {
    ui.heading(t::NAV_ACTIVITY);
    ui.horizontal(|ui| {
        ui.label("Drive:");
        let current = cx.state.activity_slot;
        let name = |s: Option<Slot>| match s {
            None => "All".to_string(),
            Some(sl) => cx
                .snap
                .destinations
                .iter()
                .find(|d| d.slot == sl)
                .map_or_else(|| sl.default_label(), |d| d.label.clone()),
        };
        egui::ComboBox::from_id_salt("activity-drive")
            .selected_text(name(current))
            .show_ui(ui, |ui| {
                for opt in [None, Some(Slot::A), Some(Slot::B)] {
                    if ui.selectable_label(current == opt, name(opt)).clicked() {
                        cx.state.activity_slot = opt;
                    }
                }
            });
        ui.checkbox(&mut cx.state.only_failures, "Only problems");
        if ui.button("Refresh").clicked() {
            cx.send(Command::LoadActivity(Default::default()));
        }
    });
    ui.add_space(6.0);

    let wanted_id = cx.state.activity_slot.and_then(|s| {
        cx.snap
            .destinations
            .iter()
            .find(|d| d.slot == s)
            .and_then(|d| d.id.clone())
    });
    let rows: Vec<_> = cx
        .snap
        .activity
        .iter()
        .filter(|e| {
            wanted_id
                .as_ref()
                .is_none_or(|id| e.destination_id.as_ref() == Some(id))
        })
        .filter(|e| !cx.state.only_failures || e.result != "ok")
        .collect();
    if rows.is_empty() {
        ui.label(RichText::new("No activity yet.").weak());
        return;
    }
    let label_of = |id: &Option<String>| -> String {
        match id {
            None => "—".into(),
            Some(id) => cx
                .snap
                .destinations
                .iter()
                .find(|d| d.id.as_ref() == Some(id))
                .map_or_else(|| "(removed drive)".into(), |d| d.label.clone()),
        }
    };
    let mut retry = false;
    TableBuilder::new(ui)
        .striped(true)
        .column(Column::auto().at_least(150.0))
        .column(Column::auto().at_least(80.0))
        .column(Column::auto().at_least(80.0))
        .column(Column::auto().at_least(70.0))
        .column(Column::remainder().at_least(200.0).clip(true))
        .column(Column::auto())
        .header(20.0, |mut h| {
            for title in ["Time", "Drive", "Operation", "Result", "Details", ""] {
                h.col(|ui| {
                    ui.strong(title);
                });
            }
        })
        .body(|body| {
            body.rows(18.0, rows.len(), |mut row| {
                let e = rows[row.index()];
                row.col(|ui| {
                    ui.label(t::timestamp(e.ts));
                });
                row.col(|ui| {
                    ui.label(label_of(&e.destination_id));
                });
                row.col(|ui| {
                    ui.label(&e.kind);
                });
                row.col(|ui| {
                    let text = if e.result == "ok" {
                        RichText::new("ok")
                    } else {
                        RichText::new(&e.result).color(ui.visuals().error_fg_color)
                    };
                    ui.label(text);
                });
                row.col(|ui| {
                    let path = e
                        .rel
                        .as_ref()
                        .map(|r| format!("{} — ", r.display()))
                        .unwrap_or_default();
                    ui.label(format!("{path}{}", e.detail));
                });
                row.col(|ui| {
                    if e.retryable && ui.small_button("Retry sync").clicked() {
                        retry = true;
                    }
                });
            });
        });
    if retry {
        cx.send(Command::RetryFailed);
    }
}
