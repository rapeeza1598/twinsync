use super::{Cx, strings as t};
use crate::devices::Severity;
use crate::domain::{
    Connectivity, DestStatus, DriveMediaType, DrivePerformanceProfile, Slot, WorkerState,
};
use crate::engine::{AttentionItem, Command, DestView, NoticeLevel, PreviewView, SourceState};
use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};

fn ok_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(110, 200, 130)
    } else {
        Color32::from_rgb(20, 120, 50)
    }
}

pub(super) fn show(ui: &mut egui::Ui, cx: &mut Cx) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            header(ui, cx);
            notices(ui, cx);
            source_card(ui, cx);
            ui.add_space(8.0);
            controls(ui, cx);
            ui.add_space(8.0);
            ui.columns(2, |cols| {
                dest_card(&mut cols[0], cx, 0);
                dest_card(&mut cols[1], cx, 1);
            });
            preview(ui, cx);
            attention(ui, cx);
        });
}

fn header(ui: &mut egui::Ui, cx: &mut Cx) {
    ui.horizontal(|ui| {
        ui.heading(crate::config::APP_NAME);
        // Only ever claimed when every drive was checked against the latest source scan.
        if cx.snap.all_up_to_date {
            ui.label(
                RichText::new(format!("✔ {}", t::ALL_UP_TO_DATE))
                    .color(ok_color(ui))
                    .strong(),
            );
        } else {
            ui.label(RichText::new("Not all drives are confirmed up to date").weak());
        }
    });
    ui.add_space(4.0);
}

fn notices(ui: &mut egui::Ui, cx: &mut Cx) {
    for (i, n) in cx.snap.notices.iter().enumerate() {
        ui.horizontal(|ui| {
            let (icon, color) = match n.level {
                NoticeLevel::Info => ("ℹ", ui.visuals().text_color()),
                NoticeLevel::Warning => ("⚠", ui.visuals().warn_fg_color),
                NoticeLevel::Error => ("⛔", ui.visuals().error_fg_color),
            };
            ui.label(RichText::new(format!("{icon} {}", n.text)).color(color));
            if ui.small_button("Dismiss").clicked() {
                cx.send(Command::DismissNotice(i));
            }
        });
    }
}

fn source_card(ui: &mut egui::Ui, cx: &mut Cx) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(t::SOURCE_TITLE).strong());
        let s = &cx.snap.source;
        match &s.path {
            Some(p) => ui.label(p.display().to_string()),
            None => ui.label(RichText::new("No source folder chosen").weak()),
        };
        match &s.state {
            SourceState::Unavailable(why) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Source unavailable: {why}. Nothing is treated as deleted."),
                );
            }
            SourceState::Ok => {
                let when = t::ago(cx.now, s.last_scan_at);
                if s.last_scan_complete {
                    ui.label(format!("Scanned {when}."));
                } else {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("{} ({when})", t::INCOMPLETE_SCAN),
                    );
                    for u in &s.unreadable {
                        ui.label(format!("  • {u}"));
                    }
                }
            }
            SourceState::Unknown if cx.snap.scanning => {
                ui.label("Scanning…");
            }
            _ => {}
        }
        if ui.button(t::CHOOSE_FOLDER).clicked()
            && let Some(p) = rfd::FileDialog::new()
                .set_title("Choose the source folder")
                .pick_folder()
        {
            cx.send(Command::SetSource(p));
        }
    });
}

fn controls(ui: &mut egui::Ui, cx: &mut Cx) {
    let any_online = cx
        .snap
        .destinations
        .iter()
        .any(|d| matches!(d.connectivity, Connectivity::Online { .. }));
    let ready = cx.snap.source.path.is_some() && any_online;
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new(t::PREVIEW_SYNC))
            .clicked()
        {
            cx.state.show_preview = true;
            cx.send(Command::Preview);
        }
        if ui
            .add_enabled(ready, egui::Button::new(t::SYNC_NOW))
            .clicked()
        {
            cx.state.show_preview = false;
            cx.send(Command::SyncNow {
                preview_signature: None,
            });
        }
        if cx.snap.paused {
            if ui.button(t::RESUME).clicked() {
                cx.send(Command::Resume);
            }
        } else if ui.button(t::PAUSE).clicked() {
            cx.send(Command::Pause);
        }
        let failed = cx.snap.destinations.iter().any(|d| d.last_error.is_some());
        if ui
            .add_enabled(failed, egui::Button::new(t::RETRY_FAILED))
            .clicked()
        {
            cx.send(Command::RetryFailed);
        }
        let mut auto = cx.snap.auto_sync;
        if ui
            .checkbox(&mut auto, t::AUTO_SYNC)
            .on_hover_text(t::AUTO_SYNC_HDD_NOTE)
            .changed()
        {
            cx.send(Command::SetAutoSync(auto));
        }
        if cx.snap.paused {
            ui.label(RichText::new("Paused").color(ui.visuals().warn_fg_color));
        }
    });
}

fn status_color(ui: &egui::Ui, s: DestStatus) -> Color32 {
    match s {
        DestStatus::UpToDate => ok_color(ui),
        DestStatus::NeedsAttention
        | DestStatus::PermissionDenied
        | DestStatus::Error
        | DestStatus::SourceUnavailable => ui.visuals().error_fg_color,
        DestStatus::WaitingForDrive | DestStatus::Stopping | DestStatus::Paused => {
            ui.visuals().warn_fg_color
        }
        _ => ui.visuals().weak_text_color(),
    }
}

fn dest_card(ui: &mut egui::Ui, cx: &mut Cx, i: usize) {
    let d: &DestView = &cx.snap.destinations[i];
    let slot = d.slot;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(&d.label).heading());
            // Text and colour together: status is never colour alone.
            ui.label(
                RichText::new(t::status_label(d.status))
                    .color(status_color(ui, d.status))
                    .strong(),
            );
        });
        if !d.configured {
            ui.label(RichText::new("Not set up").weak());
            choose_folder(ui, cx, slot, &d.label);
            return;
        }
        ui.label(format!(
            "{} · {} · {}",
            d.volume_name.as_deref().unwrap_or("(unnamed volume)"),
            t::media_label(d.media),
            d.fs_type.as_deref().unwrap_or("unknown filesystem"),
        ));
        if d.media == DriveMediaType::Unknown {
            ui.label(RichText::new(t::UNKNOWN_MEDIA_NOTE).small().weak());
        }
        ui.label(t::connectivity_label(&d.connectivity));
        match d.free_bytes {
            Some(f) => ui.label(format!(
                "Free space: {} (checked {})",
                t::human_bytes(f),
                t::ago(cx.now, d.free_checked_at)
            )),
            None => ui.label("Free space: not measured"),
        };
        ui.horizontal(|ui| {
            ui.label("Profile:");
            let mut p = d.profile;
            egui::ComboBox::from_id_salt(("profile", i))
                .selected_text(t::profile_label(p))
                .show_ui(ui, |ui| {
                    for opt in [
                        DrivePerformanceProfile::Auto,
                        DrivePerformanceProfile::Conservative,
                        DrivePerformanceProfile::Balanced,
                    ] {
                        ui.selectable_value(&mut p, opt, t::profile_label(opt));
                    }
                });
            if p != d.profile {
                cx.send(Command::SetProfile { slot, profile: p });
            }
        });
        ui.label(format!("Last sync: {}", t::ago(cx.now, d.last_sync_at)));
        ui.label(match d.last_verify_at {
            Some(ts) => format!(
                "Last full verification (content hashes): {}",
                t::ago(cx.now, Some(ts))
            ),
            None => "Never fully verified (a metadata check is not a verification)".into(),
        });
        if let Some(note) = &d.shares_disk_note {
            ui.colored_label(ui.visuals().warn_fg_color, note);
        }
        if let Some(e) = &d.last_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }

        // current operation
        if d.state != WorkerState::Idle {
            ui.label(RichText::new(t::worker_label(d.state)).strong());
        }
        if let Some(p) = &d.progress {
            if let Some(cur) = &p.current {
                ui.label(RichText::new(cur).small().weak());
            }
            let frac = if p.bytes_total > 0 {
                p.bytes_done as f32 / p.bytes_total as f32
            } else if p.files_total > 0 {
                p.files_done as f32 / p.files_total as f32
            } else {
                0.0
            };
            let mut text = format!("{} of {} files", p.files_done, p.files_total);
            if p.bytes_total > 0 {
                text += &format!(
                    " · {} of {}",
                    t::human_bytes(p.bytes_done),
                    t::human_bytes(p.bytes_total)
                );
            }
            if let Some(bps) = p.bytes_per_sec {
                text += &format!(" · {}/s", t::human_bytes(bps as u64));
            }
            ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).text(text));
        }

        // pending: exact only when computed against the latest scan
        match &d.pending {
            None => {
                ui.label(RichText::new(format!("Pending: {}", t::NOT_CHECKED)).weak());
            }
            Some(p) => {
                let s = &p.summary;
                let counts = format!(
                    "{} to copy, {} to update ({}){}",
                    s.copies,
                    s.updates,
                    t::human_bytes(s.bytes_to_write),
                    if s.attention > 0 {
                        format!(", {} need attention", s.attention)
                    } else {
                        String::new()
                    }
                );
                let offline = !matches!(d.connectivity, Connectivity::Online { .. });
                if p.up_to_date_with_source && !offline {
                    ui.label(format!("Pending: {counts}"));
                } else {
                    ui.label(RichText::new(format!("Last known: {counts}")).weak());
                }
            }
        }
        if d.version_count > 0 {
            ui.label(format!(
                "Version history: {} file(s), {} (never deleted automatically)",
                d.version_count,
                t::human_bytes(d.version_bytes)
            ));
        }
        if let Some(v) = &d.last_verify_summary {
            ui.label(RichText::new(v).small());
        }

        ui.separator();
        actions(ui, cx, d);
    });
}

fn choose_folder(ui: &mut egui::Ui, cx: &mut Cx, slot: Slot, label: &str) {
    let enabled = cx.snap.source.path.is_some();
    if ui
        .add_enabled(enabled, egui::Button::new(t::CHOOSE_FOLDER))
        .on_disabled_hover_text("Choose the source folder first")
        .clicked()
        && let Some(p) = rfd::FileDialog::new()
            .set_title(format!("Choose the folder on {label}"))
            .pick_folder()
    {
        cx.send(Command::ConfigureDestination {
            slot,
            path: p,
            label: label.to_string(),
        });
    }
}

fn actions(ui: &mut egui::Ui, cx: &mut Cx, d: &DestView) {
    let slot = d.slot;
    let online = matches!(d.connectivity, Connectivity::Online { .. });
    ui.horizontal_wrapped(|ui| {
        choose_folder(ui, cx, slot, &d.label);
        if matches!(d.connectivity, Connectivity::NeedsConfirmation(_))
            && ui.button(t::CONFIRM_DRIVE).clicked()
        {
            cx.send(Command::ConfirmIdentity { slot });
        }
        let idle = matches!(d.state, WorkerState::Idle | WorkerState::Paused);
        if ui
            .add_enabled(
                online && idle && !d.released,
                egui::Button::new(t::FULL_VERIFY),
            )
            .clicked()
        {
            cx.send(Command::FullVerify { slot });
        }
        if d.released {
            if ui.button(t::USE_DRIVE_AGAIN).clicked() {
                cx.send(Command::UseDriveAgain { slot });
            }
            if ui.button(t::EJECT).clicked() {
                cx.send(Command::Eject {
                    slot,
                    confirm_siblings: false,
                });
            }
        } else if ui
            .add_enabled(
                online && d.state != WorkerState::Stopping,
                egui::Button::new(t::PREPARE_DISCONNECT),
            )
            .clicked()
        {
            cx.send(Command::PrepareDisconnect { slot });
        }
    });
    if d.state == WorkerState::Stopping {
        ui.label(RichText::new("Waiting for the current write to finish…").weak());
    }
    if let Some(note) = &d.disconnect_note {
        ui.add_space(4.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(note);
        });
    }
}

fn preview(ui: &mut egui::Ui, cx: &mut Cx) {
    if !cx.state.show_preview {
        return;
    }
    ui.add_space(8.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(t::PREVIEW_SYNC).strong());
            if ui.small_button("Close").clicked() {
                cx.state.show_preview = false;
            }
        });
        let Some(p) = cx.snap.preview.clone() else {
            ui.label("Working out the plan…");
            return;
        };
        if p.out_of_date {
            ui.colored_label(ui.visuals().warn_fg_color, t::PREVIEW_STALE);
        }
        if !p.source_scan_complete {
            ui.colored_label(ui.visuals().warn_fg_color, t::INCOMPLETE_SCAN);
        }
        for (slot, s) in &p.per_dest {
            ui.label(format!(
                "Drive {}: {} to copy, {} to update, {} folder(s), {} to write, {} need attention, {} to check",
                slot.as_str(),
                s.copies,
                s.updates,
                s.make_dirs,
                t::human_bytes(s.bytes_to_write),
                s.attention,
                s.checks
            ));
        }
        if ui.button(t::SYNC_NOW).clicked() {
            cx.state.show_preview = false;
            cx.send(Command::SyncNow { preview_signature: Some(p.signature) });
        }
        preview_table(ui, &p);
    });
}

fn preview_table(ui: &mut egui::Ui, p: &PreviewView) {
    if p.rows.is_empty() {
        ui.label("Nothing to do.");
        return;
    }
    TableBuilder::new(ui)
        .striped(true)
        .max_scroll_height(320.0)
        .column(Column::remainder().at_least(160.0).clip(true))
        .column(Column::auto())
        .column(Column::auto())
        .column(Column::auto())
        .column(Column::remainder().at_least(160.0).clip(true))
        .header(20.0, |mut h| {
            for title in ["Path", "Drive", "Action", "Size", "Reason"] {
                h.col(|ui| {
                    ui.strong(title);
                });
            }
        })
        .body(|body| {
            body.rows(18.0, p.rows.len(), |mut row| {
                let r = &p.rows[row.index()];
                row.col(|ui| {
                    ui.label(r.rel.display());
                });
                row.col(|ui| {
                    ui.label(r.slot.as_str());
                });
                row.col(|ui| {
                    ui.label(&r.action);
                });
                row.col(|ui| {
                    ui.label(if r.size > 0 {
                        t::human_bytes(r.size)
                    } else {
                        String::new()
                    });
                });
                row.col(|ui| {
                    ui.label(&r.reason);
                });
            });
        });
}

fn attention(ui: &mut egui::Ui, cx: &mut Cx) {
    let any = cx.snap.destinations.iter().any(|d| !d.attention.is_empty());
    if !any {
        return;
    }
    ui.add_space(8.0);
    ui.label(RichText::new(t::NEEDS_ATTENTION).heading());
    ui.label(
        RichText::new("Nothing here is changed until you choose. No choice copies anything back to the source.")
            .small()
            .weak(),
    );
    for d in &cx.snap.destinations {
        if d.attention.is_empty() {
            continue;
        }
        egui::CollapsingHeader::new(format!("{} — {} item(s)", d.label, d.attention.len()))
            .id_salt(("attention", d.slot.as_str()))
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("attention-scroll", d.slot.as_str()))
                    .max_height(280.0)
                    .show(ui, |ui| {
                        // Long lists stay cheap: only the first rows are drawn with buttons.
                        for item in d.attention.iter().take(200) {
                            attention_row(ui, cx, d.slot, item);
                        }
                        if d.attention.len() > 200 {
                            ui.label(format!("… and {} more", d.attention.len() - 200));
                        }
                    });
            });
    }
}

fn attention_row(ui: &mut egui::Ui, cx: &mut Cx, slot: Slot, a: &AttentionItem) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(a.rel.display()).strong());
        ui.label(RichText::new(&a.action).color(ui.visuals().warn_fg_color));
        ui.label(RichText::new(&a.reason).small().weak());
    });
    ui.horizontal_wrapped(|ui| {
        ui.add_space(16.0);
        for r in &a.resolutions {
            if ui.small_button(r.label()).clicked() {
                cx.send(Command::Resolve {
                    slot,
                    rel: a.rel.clone(),
                    resolution: *r,
                });
            }
        }
    });
}

pub(super) fn dialogs(ctx: &egui::Context, cx: &mut Cx) {
    if let Some(setup) = &cx.snap.setup {
        egui::Window::new(format!("Set up Drive {}", setup.slot.as_str()))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(setup.path.display().to_string());
                ui.add_space(6.0);
                for f in &setup.findings {
                    let (icon, color) = match f.severity {
                        Severity::Error => ("⛔ Cannot use", ui.visuals().error_fg_color),
                        Severity::NeedsConfirm => ("⚠ Please confirm", ui.visuals().warn_fg_color),
                        Severity::Warning => ("ℹ Note", ui.visuals().text_color()),
                    };
                    ui.label(RichText::new(format!("{icon}: {}", f.message)).color(color));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if setup.awaiting_confirmation && ui.button("Use this folder anyway").clicked()
                    {
                        cx.send(Command::ConfirmSetup { slot: setup.slot });
                    }
                    if ui
                        .button(if setup.awaiting_confirmation {
                            "Cancel"
                        } else {
                            "Close"
                        })
                        .clicked()
                    {
                        cx.send(Command::CancelSetup { slot: setup.slot });
                    }
                });
            });
    }
    if let Some(p) = &cx.snap.eject_prompt {
        egui::Window::new("Eject also unmounts other volumes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Ejecting this drive will also unmount these volumes on the same disk:");
                for v in &p.also_unmounts {
                    ui.label(format!("  • {v}"));
                }
                ui.horizontal(|ui| {
                    if ui.button("Eject all of them").clicked() {
                        cx.send(Command::Eject {
                            slot: p.slot,
                            confirm_siblings: true,
                        });
                    }
                    if ui.button("Cancel").clicked() {
                        cx.send(Command::CancelEject);
                    }
                });
            });
    }
}
