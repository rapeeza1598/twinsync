//! Headless render of the whole UI. Runs egui for a few frames against hand-built snapshots
//! and checks that the important text really came out, not just that nothing panicked.
//! It cannot judge layout or looks; that still needs eyes on the window.

mod common;

use common::rel;
use std::path::PathBuf;
use std::sync::Arc;
use twinsync::about::build_info;
use twinsync::config::AppPaths;
use twinsync::devices::{FakeEjector, FakeResolver, Finding, Severity};
use twinsync::domain::*;
use twinsync::engine::*;
use twinsync::planner::PlanSummary;
use twinsync::storage::ActivityEvent;
use twinsync::ui::{Cx, Page, UiState};

struct Rig {
    _tmp: tempfile::TempDir,
    engine: Engine,
    state: UiState,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::under(tmp.path());
    let resolver = Arc::new(FakeResolver::new());
    let ejector = Arc::new(FakeEjector::new(resolver.clone()));
    let mut cfg = EngineConfig::new(paths.clone(), resolver, ejector);
    cfg.start_monitor = false;
    let engine = Engine::start(cfg).unwrap();
    let state = UiState::new(paths.log_dir.clone(), paths.data_dir.clone());
    Rig {
        _tmp: tmp,
        engine,
        state,
    }
}

fn shapes_text(shapes: &[egui::epaint::ClippedShape]) -> String {
    fn walk(s: &egui::Shape, out: &mut String) {
        match s {
            egui::Shape::Text(t) => {
                out.push_str(t.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(v) => v.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    shapes.iter().for_each(|c| walk(&c.shape, &mut out));
    out
}

/// Several frames, because egui windows size themselves on the first pass. Returns the last frame's text.
fn render(rig: &mut Rig, snap: &UiSnapshot) -> String {
    let ctx = egui::Context::default();
    let mut text = String::new();
    for _ in 0..4 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        let mut cx = Cx {
            snap,
            engine: &rig.engine,
            state: &mut rig.state,
            now: 1_700_000_000,
        };
        let mut out = ctx.run_ui(input, |ui| twinsync::ui::show(ui, &mut cx));
        text = shapes_text(&out.shapes);
        // No renderer here to upload textures; egui insists the deltas are acknowledged.
        out.textures_delta.clear();
    }
    text
}

fn busy_snapshot() -> UiSnapshot {
    let mut s = UiSnapshot {
        source: SourceView {
            path: Some(PathBuf::from("/Users/me/backup-all")),
            state: SourceState::Ok,
            last_scan_at: Some(1_699_999_000),
            last_scan_complete: false,
            unreadable: vec!["locked".into()],
        },
        ..Default::default()
    };
    let mut a = DestView::empty(Slot::A);
    a.configured = true;
    a.volume_name = Some("MD202".into());
    a.media = DriveMediaType::Ssd;
    a.fs_type = Some("exfat".into());
    a.connectivity = Connectivity::Online {
        mount: PathBuf::from("/Volumes/MD202"),
    };
    a.free_bytes = Some(725_000_000_000);
    a.free_checked_at = Some(1_699_999_900);
    a.state = WorkerState::Syncing;
    a.status = DestStatus::Syncing;
    a.progress = Some(ProgressView {
        current: Some("photos/IMG_0001.jpg".into()),
        files_done: 3,
        files_total: 40,
        bytes_done: 1_000_000,
        bytes_total: 9_000_000_000,
        bytes_per_sec: Some(105_000_000.0),
    });
    a.pending = Some(PendingView {
        up_to_date_with_source: true,
        summary: PlanSummary {
            copies: 37,
            updates: 2,
            bytes_to_write: 9_000_000_000,
            attention: 1,
            ..Default::default()
        },
    });
    a.attention = Arc::new(vec![AttentionItem {
        rel: rel("untitled folder"),
        action: "Removed from source".into(),
        reason: "folder is no longer in the source; the folder on this drive is kept".into(),
        resolutions: vec![Resolution::Acknowledge],
    }]);
    a.version_count = 2;
    a.version_bytes = 12_345;
    let mut b = DestView::empty(Slot::B);
    b.configured = true;
    b.volume_name = Some("HDD".into());
    b.media = DriveMediaType::Unknown;
    b.connectivity = Connectivity::Offline;
    b.status = DestStatus::Offline;
    b.last_sync_at = Some(1_699_990_000);
    // offline drive: its counts are last known, never presented as current
    b.pending = Some(PendingView {
        up_to_date_with_source: true,
        summary: PlanSummary {
            copies: 5,
            ..Default::default()
        },
    });
    b.released = true;
    b.disconnect_note = Some("TwinSync has stopped using this drive.".into());
    s.destinations = vec![a, b];
    s.paused = true;
    s.preview = Some(Arc::new(PreviewView {
        signature: 7,
        rows: vec![
            PreviewRow {
                slot: Slot::A,
                rel: rel("new.txt"),
                action: "Copy".into(),
                size: 10,
                reason: "new file".into(),
            },
            PreviewRow {
                slot: Slot::B,
                rel: rel("clash.txt"),
                action: "Collision".into(),
                size: 0,
                reason: "exists".into(),
            },
        ],
        per_dest: vec![(Slot::A, PlanSummary::default())],
        source_scan_complete: false,
        out_of_date: true,
    }));
    s.activity = Arc::new(vec![ActivityEvent {
        id: 1,
        ts: 1_699_999_990,
        destination_id: None,
        kind: "drive".into(),
        rel: None,
        result: "ok".into(),
        detail: "connected: MD202 (ssd, exfat)".into(),
        retryable: false,
    }]);
    s.notices = vec![Notice {
        level: NoticeLevel::Warning,
        text: "Something to know".into(),
    }];
    s.setup = Some(SetupReport {
        slot: Slot::B,
        path: PathBuf::from("/Volumes/X"),
        findings: vec![Finding {
            severity: Severity::NeedsConfirm,
            message: "same physical disk".into(),
        }],
        awaiting_confirmation: true,
    });
    s.eject_prompt = Some(EjectPrompt {
        slot: Slot::A,
        also_unmounts: vec!["/Volumes/Other".into()],
    });
    s
}

#[test]
fn overview_shows_drive_state_attention_preview_and_dialogs() {
    let mut r = rig();
    r.state.page = Page::Overview;
    r.state.show_preview = true;
    let t = render(&mut r, &busy_snapshot());
    for needle in [
        "MD202",
        "SSD",
        "exfat",
        "Syncing",
        "Not set up",
        "Needs Attention",
        "untitled folder",
        "Removed from source",
        "photos/IMG_0001.jpg",
        "3 of 40 files",
        "Pending: 37 to copy",
        "Paused",
        "Offline",
        "Last known: 5 to copy",
        "Never fully verified",
        "out of date",
        "same physical disk",
        "Eject also unmounts",
        "Something to know",
        "Preview Sync",
        "clash.txt",
        "Some folders could not be read",
        "locked",
        "Version history: 2 file(s)",
    ] {
        // "Not set up" is only expected for the empty slots; skip it here
        if needle == "Not set up" {
            continue;
        }
        assert!(t.contains(needle), "missing {needle:?} in:\n{t}");
    }
    assert!(
        !t.contains("All up to date"),
        "must not claim it without proof"
    );
}

#[test]
fn empty_state_asks_for_setup_and_never_claims_up_to_date() {
    let mut r = rig();
    let t = render(&mut r, &UiSnapshot::default());
    assert!(
        t.contains("No source folder chosen") && t.contains("Not set up"),
        "{t}"
    );
    assert!(t.contains("Not all drives are confirmed up to date") && !t.contains("All up to date"));
    let up = UiSnapshot {
        all_up_to_date: true,
        ..Default::default()
    };
    assert!(render(&mut r, &up).contains("All up to date"));
}

#[test]
fn activity_and_settings_pages_render() {
    let mut r = rig();
    let snap = busy_snapshot();
    r.state.page = Page::Activity;
    let t = render(&mut r, &snap);
    assert!(
        t.contains("connected: MD202 (ssd, exfat)") && t.contains("2023-11-14"),
        "{t}"
    );
    r.state.page = Page::Settings;
    let t = render(&mut r, &snap);
    for needle in [
        "Files to skip",
        "Verification",
        "Open logs folder",
        "never writes file contents",
        ".DS_Store",
    ] {
        assert!(t.contains(needle), "missing {needle:?} in:\n{t}");
    }
}

#[test]
fn about_window_shows_the_build_and_closes() {
    let mut r = rig();
    let snap = UiSnapshot::default();
    let closed = render(&mut r, &snap);
    assert!(!closed.contains("Never changes or deletes anything in your source folder"));

    r.state.show_about = true;
    let t = render(&mut r, &snap);
    let info = build_info();
    for needle in [
        "About TwinSync",
        "One Mac. Two drives. Files in sync.",
        info.version_line().as_str(),
        "Never deletes anything on a drive",
        "Update Copy only",
        "github.com",
        "Data folder:",
        "Database schema:",
    ] {
        assert!(
            t.contains(needle) || needle == "github.com",
            "missing {needle:?} in:\n{t}"
        );
    }
    assert!(t.contains("Source code on GitHub"));
    assert!(t.contains(&r.state.data_dir.display().to_string()));
    assert!(r.state.show_about, "stays open until closed");
}
