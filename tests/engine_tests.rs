mod common;

use common::{rel, vol};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use twinsync::config::AppPaths;
use twinsync::devices::{DeviceResolver, FakeEjector, FakeResolver};
use twinsync::domain::*;
use twinsync::engine::*;
use twinsync::transfer::FaultPoint;

const T: Duration = Duration::from_secs(20);

struct H {
    _tmp: TempDir,
    base: PathBuf,
    resolver: Arc<FakeResolver>,
    ejector: Arc<FakeEjector>,
    engine: Engine,
    src: PathBuf,
    cfg: EngineConfig,
}

fn base_cfg(paths: &AppPaths, r: &Arc<FakeResolver>, e: &Arc<FakeEjector>) -> EngineConfig {
    let mut cfg = EngineConfig::new(paths.clone(), r.clone(), e.clone());
    cfg.start_monitor = false;
    cfg.tick = Duration::from_millis(20);
    cfg.retry_base = Duration::from_millis(1);
    cfg
}

impl H {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(tweak: impl FnOnce(&mut EngineConfig)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let src = base.join("home/source");
        for d in ["home/source", "A/Backup", "B/Backup"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        let resolver = Arc::new(FakeResolver::new());
        resolver.add(vol(
            &base.join("home"),
            "HOME",
            DriveMediaType::Ssd,
            Some("disk0"),
        ));
        resolver.add(vol(
            &base.join("A"),
            "UA",
            DriveMediaType::Ssd,
            Some("disk4"),
        ));
        resolver.add(vol(
            &base.join("B"),
            "UB",
            DriveMediaType::Hdd,
            Some("disk8"),
        ));
        let ejector = Arc::new(FakeEjector::new(resolver.clone()));
        let paths = AppPaths::under(&base.join("app"));
        let mut cfg = base_cfg(&paths, &resolver, &ejector);
        tweak(&mut cfg);
        let engine = Engine::start(cfg.clone()).unwrap();
        H {
            _tmp: tmp,
            base,
            resolver,
            ejector,
            engine,
            src,
            cfg,
        }
    }

    fn restart(&mut self) {
        self.engine.shutdown();
        self.engine = Engine::start(self.cfg.clone()).unwrap();
    }

    fn dir(&self, d: usize) -> PathBuf {
        self.base.join(if d == 0 { "A/Backup" } else { "B/Backup" })
    }

    fn put(&self, path: &str, data: &[u8]) {
        let p = self.src.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    fn wait(&self, what: &str, pred: impl Fn(&UiSnapshot) -> bool) -> Arc<UiSnapshot> {
        self.engine.wait_for(T, pred).unwrap_or_else(|| {
            panic!(
                "timed out waiting for {what}; snapshot: {:#?}",
                self.engine.snapshot()
            )
        })
    }

    fn setup(&self) {
        self.engine.send(Command::SetSource(self.src.clone()));
        for (slot, d) in [(Slot::A, 0), (Slot::B, 1)] {
            self.engine.send(Command::ConfigureDestination {
                slot,
                path: self.dir(d),
                label: format!("Drive {}", slot.as_str()),
            });
            self.wait("destination online", |s| online(s, d));
        }
    }

    fn sync_and_wait(&self) {
        self.engine.send(Command::SyncNow {
            preview_signature: None,
        });
        self.wait("all up to date", |s| s.all_up_to_date);
    }
}

fn online(s: &UiSnapshot, d: usize) -> bool {
    matches!(s.destinations[d].connectivity, Connectivity::Online { .. })
}

fn status(s: &UiSnapshot, d: usize) -> DestStatus {
    s.destinations[d].status
}

fn read(p: PathBuf) -> Option<Vec<u8>> {
    fs::read(p).ok()
}

// ------------------------------------------------------------- vertical slice

#[test]
fn setup_preview_sync_verify_and_state_survives_restart() {
    let mut h = H::new();
    h.put("a.txt", b"alpha");
    h.put("d/b.txt", b"bravo");
    h.setup();
    assert!(
        !h.engine.snapshot().all_up_to_date,
        "nothing has been checked yet"
    );
    assert!(matches!(
        status(&h.engine.snapshot(), 0),
        DestStatus::Idle | DestStatus::Scanning
    ));

    h.engine.send(Command::Preview);
    let s = h.wait("preview", |s| s.preview.is_some());
    let p = s.preview.as_ref().unwrap();
    assert!(
        p.rows
            .iter()
            .any(|r| r.rel == rel("a.txt") && r.action == "Copy" && r.slot == Slot::A)
    );
    assert!(
        p.rows
            .iter()
            .any(|r| r.rel == rel("d/b.txt") && r.slot == Slot::B)
    );
    assert_eq!(p.per_dest.len(), 2);
    assert!(
        read(h.dir(0).join("a.txt")).is_none(),
        "preview must not copy anything"
    );

    h.engine.send(Command::SyncNow {
        preview_signature: Some(p.signature),
    });
    let s = h.wait("synced", |s| s.all_up_to_date);
    for d in 0..2 {
        assert_eq!(read(h.dir(d).join("a.txt")).unwrap(), b"alpha");
        assert_eq!(read(h.dir(d).join("d/b.txt")).unwrap(), b"bravo");
        assert_eq!(status(&s, d), DestStatus::UpToDate);
        assert!(s.destinations[d].last_sync_at.is_some());
        assert!(
            s.destinations[d].last_verify_at.is_none(),
            "a metadata check is not a verification"
        );
        assert_eq!(
            s.destinations[d].pending.as_ref().unwrap().summary.copies,
            0
        );
    }
    assert!(
        s.activity
            .iter()
            .any(|e| e.kind == "sync" && e.result == "ok")
    );

    // full verify by content hash
    h.engine.send(Command::FullVerify { slot: Slot::A });
    let s = h.wait("verified", |s| {
        s.destinations[0].last_verify_summary.is_some()
    });
    assert!(
        s.destinations[0]
            .last_verify_summary
            .as_ref()
            .unwrap()
            .contains("by content hash")
    );
    assert!(
        h.engine
            .wait_for(T, |s| s.destinations[0].last_verify_at.is_some())
            .is_some()
    );

    // restart: configuration and history persist, drives reconnect, nothing to redo
    h.restart();
    h.engine.send(Command::RefreshMounts);
    let s = h.wait("reconnected", |s| online(s, 0) && online(s, 1));
    assert_eq!(s.source.path.as_deref(), Some(h.src.as_path()));
    assert!(s.destinations[0].last_sync_at.is_some());
    h.engine.send(Command::Preview);
    let s = h.wait("preview after restart", |s| {
        s.preview.as_ref().is_some_and(|p| p.rows.is_empty()) && s.all_up_to_date
    });
    assert!(s.preview.as_ref().unwrap().rows.is_empty());
}

#[test]
fn source_change_makes_up_to_date_false_until_checked_again() {
    let h = H::new();
    h.put("a.txt", b"v1");
    h.setup();
    h.sync_and_wait();
    h.put("a.txt", b"v2 changed");
    h.engine.send(Command::Preview);
    h.wait("preview shows update", |s| {
        s.preview
            .as_ref()
            .is_some_and(|p| p.rows.iter().any(|r| r.action == "Update"))
    });
    assert!(!h.engine.snapshot().all_up_to_date);
    h.sync_and_wait();
    assert_eq!(read(h.dir(1).join("a.txt")).unwrap(), b"v2 changed");
}

#[test]
fn sync_now_refuses_a_plan_the_user_did_not_review() {
    let h = H::new();
    h.put("a.txt", b"one");
    h.setup();
    h.engine.send(Command::Preview);
    let sig = h
        .wait("preview", |s| s.preview.is_some())
        .preview
        .as_ref()
        .unwrap()
        .signature;
    h.put("late.txt", b"added after the preview");
    h.engine.send(Command::SyncNow {
        preview_signature: Some(sig),
    });
    let s = h.wait("new preview", |s| {
        s.preview
            .as_ref()
            .is_some_and(|p| p.signature != sig && p.out_of_date)
    });
    assert!(s.notices.iter().any(|n| n.text.contains("plan changed")));
    assert!(read(h.dir(0).join("a.txt")).is_none(), "nothing ran");
    assert!(read(h.dir(0).join("late.txt")).is_none());
}

// --------------------------------------------------------------------- devices

#[test]
fn one_drive_offline_does_not_stop_the_other_and_it_catches_up_on_return() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    let b_vol = h
        .resolver
        .mounted_volumes()
        .unwrap()
        .into_iter()
        .find(|v| v.volume_uuid.as_deref() == Some("UB"))
        .unwrap();
    h.resolver.remove_mount(&h.base.join("B"));
    h.engine.send(Command::RefreshMounts);
    h.wait("B offline", |s| {
        s.destinations[1].connectivity == Connectivity::Offline
    });

    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    let s = h.wait("A synced", |s| status(s, 0) == DestStatus::UpToDate);
    assert_eq!(status(&s, 1), DestStatus::Offline);
    assert!(!s.all_up_to_date, "an offline drive is not up to date");
    assert!(read(h.dir(1).join("a.txt")).is_none());

    h.resolver.add(b_vol);
    h.engine.send(Command::RefreshMounts);
    h.wait("B back", |s| online(s, 1));
    h.sync_and_wait();
    assert_eq!(read(h.dir(1).join("a.txt")).unwrap(), b"data");
}

#[test]
fn a_returning_drive_at_a_new_mount_path_is_found_again() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    let new_mount = h.base.join("elsewhere");
    fs::rename(h.base.join("B"), &new_mount).unwrap();
    h.resolver.move_volume("UB", &new_mount);
    h.engine.send(Command::RefreshMounts);
    h.wait("B reconnected at the new path", |s| {
        matches!(&s.destinations[1].connectivity, Connectivity::Online { mount } if mount == &new_mount)
    });
    h.sync_and_wait();
    assert_eq!(read(new_mount.join("Backup/a.txt")).unwrap(), b"data");
}

#[test]
fn a_different_drive_in_the_same_place_is_never_written_to() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    // the real B is replaced by a lookalike with a different UUID at the same mount path
    h.resolver.remove_mount(&h.base.join("B"));
    h.resolver.add(vol(
        &h.base.join("B"),
        "IMPOSTOR",
        DriveMediaType::Hdd,
        Some("disk9"),
    ));
    h.engine.send(Command::RefreshMounts);
    h.wait("B offline", |s| {
        s.destinations[1].connectivity == Connectivity::Offline
    });
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("A synced", |s| status(s, 0) == DestStatus::UpToDate);
    assert!(read(h.dir(1).join("a.txt")).is_none());
}

#[test]
fn a_marker_from_another_destination_or_a_missing_marker_stops_all_writes() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    let marker_dir = h
        .dir(1)
        .join(".twinsync")
        .join(h.engine.snapshot().destinations[1].label.clone());
    let _ = marker_dir; // layout is keyed by project id; find the marker instead
    let marker = fs::read_dir(h.dir(1).join(".twinsync"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("marker.json");
    let original = fs::read_to_string(&marker).unwrap();
    let forged = original.replace(
        "\"destination_id\": \"",
        "\"destination_id\": \"not-this-one-",
    );
    fs::write(&marker, forged).unwrap();
    // force a reconnect
    let b_vol = h
        .resolver
        .mounted_volumes()
        .unwrap()
        .into_iter()
        .find(|v| v.volume_uuid.as_deref() == Some("UB"))
        .unwrap();
    h.resolver.remove_mount(&h.base.join("B"));
    h.engine.send(Command::RefreshMounts);
    h.wait("B offline", |s| {
        s.destinations[1].connectivity == Connectivity::Offline
    });
    h.resolver.add(b_vol.clone());
    h.engine.send(Command::RefreshMounts);
    let s = h.wait("mismatch", |s| {
        matches!(
            s.destinations[1].connectivity,
            Connectivity::IdentityMismatch(_)
        )
    });
    assert_eq!(status(&s, 1), DestStatus::NeedsAttention);
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("A synced", |s| status(s, 0) == DestStatus::UpToDate);
    assert!(
        read(h.dir(1).join("a.txt")).is_none(),
        "wrong identity must not be written"
    );

    // marker missing entirely: needs confirmation, still no writes until confirmed
    fs::remove_file(&marker).unwrap();
    h.resolver.remove_mount(&h.base.join("B"));
    h.engine.send(Command::RefreshMounts);
    h.wait("offline again", |s| {
        s.destinations[1].connectivity == Connectivity::Offline
    });
    h.resolver.add(b_vol);
    h.engine.send(Command::RefreshMounts);
    h.wait("needs confirmation", |s| {
        matches!(
            s.destinations[1].connectivity,
            Connectivity::NeedsConfirmation(_)
        )
    });
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("A still fine", |s| status(s, 0) == DestStatus::UpToDate);
    assert!(read(h.dir(1).join("a.txt")).is_none());
    h.engine.send(Command::ConfirmIdentity { slot: Slot::B });
    h.wait("confirmed", |s| online(s, 1));
    h.sync_and_wait();
    assert_eq!(read(h.dir(1).join("a.txt")).unwrap(), b"data");
}

#[test]
fn a_missing_destination_folder_is_reported_not_recreated() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    let b_vol = h
        .resolver
        .mounted_volumes()
        .unwrap()
        .into_iter()
        .find(|v| v.volume_uuid.as_deref() == Some("UB"))
        .unwrap();
    h.resolver.remove_mount(&h.base.join("B"));
    h.engine.send(Command::RefreshMounts);
    h.wait("offline", |s| {
        s.destinations[1].connectivity == Connectivity::Offline
    });
    fs::remove_dir_all(h.dir(1)).unwrap();
    h.resolver.add(b_vol);
    h.engine.send(Command::RefreshMounts);
    h.wait("mismatch", |s| {
        matches!(
            s.destinations[1].connectivity,
            Connectivity::IdentityMismatch(_)
        )
    });
    assert!(
        !h.dir(1).exists(),
        "TwinSync must not recreate the destination folder"
    );
}

#[test]
fn setup_rejects_overlap_and_asks_before_sharing_a_physical_disk() {
    let h = H::new();
    h.engine.send(Command::SetSource(h.src.clone()));
    h.wait("source", |s| s.source.path.is_some());
    let inside = h.src.join("backup-here");
    fs::create_dir(&inside).unwrap();
    h.engine.send(Command::ConfigureDestination {
        slot: Slot::A,
        path: inside,
        label: "A".into(),
    });
    let s = h.wait("report", |s| s.setup.is_some());
    assert!(!s.setup.as_ref().unwrap().awaiting_confirmation);
    assert!(!s.destinations[0].configured);

    // two volumes on one physical disk
    fs::create_dir_all(h.base.join("C/Backup")).unwrap();
    h.resolver.add(vol(
        &h.base.join("C"),
        "UC",
        DriveMediaType::Hdd,
        Some("disk4"),
    ));
    h.engine.send(Command::ConfigureDestination {
        slot: Slot::A,
        path: h.dir(0),
        label: "A".into(),
    });
    h.wait("A online", |s| online(s, 0));
    h.engine.send(Command::ConfigureDestination {
        slot: Slot::B,
        path: h.base.join("C/Backup"),
        label: "B".into(),
    });
    let s = h.wait("needs confirmation", |s| {
        s.setup.as_ref().is_some_and(|r| r.awaiting_confirmation)
    });
    assert!(
        s.setup
            .as_ref()
            .unwrap()
            .findings
            .iter()
            .any(|f| f.message.contains("same physical disk"))
    );
    assert!(
        !s.destinations[1].configured,
        "not configured until confirmed"
    );
    h.engine.send(Command::ConfirmSetup { slot: Slot::B });
    let s = h.wait("B configured", |s| {
        s.destinations[1].configured && online(s, 1)
    });
    assert!(s.destinations[1].shares_disk_note.is_some());
    h.put("a.txt", b"data");
    h.sync_and_wait();
}

// --------------------------------------------------------------------- source

#[test]
fn unavailable_source_is_never_treated_as_empty() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    h.sync_and_wait();
    let moved = h.base.join("home/source-moved");
    fs::rename(&h.src, &moved).unwrap();
    h.engine.send(Command::Preview);
    let s = h.wait("unavailable", |s| {
        matches!(s.source.state, SourceState::Unavailable(_))
    });
    assert_eq!(status(&s, 0), DestStatus::SourceUnavailable);
    assert!(!s.all_up_to_date);
    assert_eq!(
        read(h.dir(0).join("a.txt")).unwrap(),
        b"data",
        "copies untouched"
    );
    assert!(
        s.destinations[0].attention.is_empty(),
        "no SourceDeleted storm"
    );
    fs::rename(&moved, &h.src).unwrap();
    h.engine.send(Command::Preview);
    h.wait("recovered", |s| s.source.state == SourceState::Ok);
}

#[test]
fn incomplete_scan_is_reported_and_unreadable_files_are_not_called_deleted() {
    let h = H::new();
    h.put("locked/inside.txt", b"x");
    h.put("ok.txt", b"y");
    h.setup();
    h.sync_and_wait();
    let locked = h.src.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    h.engine.send(Command::Preview);
    let s = h.wait("incomplete", |s| {
        s.source.state == SourceState::Ok && !s.source.last_scan_complete
    });
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(s.source.unreadable, ["locked"]);
    assert!(
        !s.all_up_to_date,
        "an incomplete scan cannot prove anything is up to date"
    );
    for d in 0..2 {
        assert!(
            !s.destinations[d]
                .attention
                .iter()
                .any(|a| a.rel == rel("locked/inside.txt")),
            "unreadable is unknown, not deleted"
        );
    }
}

// ---------------------------------------------------------- stop, pause, eject

fn slow_hook(delay_ms: u64, threads: Arc<Mutex<Vec<String>>>) -> twinsync::transfer::FaultHook {
    Arc::new(move |p| {
        if matches!(p, FaultPoint::WriteChunk(_)) {
            threads
                .lock()
                .unwrap()
                .push(std::thread::current().name().unwrap_or("?").to_string());
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        None
    })
}

#[test]
fn slow_drive_never_blocks_the_coordinator_and_never_gets_two_writers() {
    let threads = Arc::new(Mutex::new(Vec::new()));
    let hook = slow_hook(400, threads.clone());
    let h = H::with(|c| {
        c.fault = Some(hook);
        c.stall_after = Duration::from_millis(150);
    });
    h.put("big.bin", &vec![7u8; 3 * (1 << 20)]);
    h.setup();
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    // the drive is stuck inside a 400ms write: the app says so, and still answers
    h.wait("waiting for drive", |s| {
        s.destinations[0].state == WorkerState::WaitingForDrive
    });
    let t0 = std::time::Instant::now();
    h.engine.send(Command::Pause);
    h.wait("paused flag", |s| s.paused);
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "coordinator answered while a drive was stuck"
    );
    // a second SyncNow while the first is still running must queue, not start another writer
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("completes", |s| s.all_up_to_date);
    let seen: std::collections::HashSet<_> = threads.lock().unwrap().iter().cloned().collect();
    assert!(
        seen.iter()
            .all(|n| n == "twinsync-drive-0" || n == "twinsync-drive-1"),
        "writes only ever happen on the two destination workers: {seen:?}"
    );
    assert_eq!(read(h.dir(0).join("big.bin")).unwrap().len(), 3 * (1 << 20));
}

#[test]
fn pause_stops_between_items_and_resume_finishes() {
    let threads = Arc::new(Mutex::new(Vec::new()));
    let hook = slow_hook(30, threads);
    let h = H::with(|c| c.fault = Some(hook));
    for i in 0..12 {
        h.put(&format!("f{i:02}.bin"), &vec![1u8; 2 * (1 << 20)]);
    }
    h.setup();
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("progress", |s| {
        s.destinations[0]
            .progress
            .as_ref()
            .is_some_and(|p| p.files_done >= 1)
    });
    h.engine.send(Command::Pause);
    let s = h.wait("paused", |s| {
        s.destinations
            .iter()
            .all(|d| d.state == WorkerState::Paused)
    });
    assert!(!s.all_up_to_date);
    let done = fs::read_dir(h.dir(0))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('f')
        })
        .count();
    assert!(done < 12, "paused before finishing ({done}/12)");
    h.engine.send(Command::Resume);
    h.wait("finished", |s| s.all_up_to_date);
    assert_eq!(
        fs::read_dir(h.dir(1))
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('f'))
            .count(),
        12
    );
}

#[test]
fn prepare_to_disconnect_stops_writes_and_does_not_claim_it_is_safe_to_unplug() {
    let threads = Arc::new(Mutex::new(Vec::new()));
    let hook = slow_hook(25, threads);
    let h = H::with(|c| c.fault = Some(hook));
    for i in 0..6 {
        h.put(&format!("f{i}.bin"), &vec![2u8; 2 * (1 << 20)]);
    }
    h.setup();
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("copying to A", |s| {
        s.destinations[0]
            .progress
            .as_ref()
            .is_some_and(|p| p.bytes_done > 0)
    });
    h.engine.send(Command::PrepareDisconnect { slot: Slot::A });
    let s = h.wait("released", |s| s.destinations[0].released);
    let note = s.destinations[0]
        .disconnect_note
        .clone()
        .unwrap()
        .to_lowercase();
    assert!(
        note.contains("eject") && !note.contains("safe to unplug"),
        "{note}"
    );
    assert_ne!(status(&s, 0), DestStatus::Syncing);
    // nothing of ours is left half-written and nothing changes any more
    let tmp_leftovers = |d: usize| {
        fs::read_dir(h.dir(d).join(".twinsync"))
            .ok()
            .map(|rd| {
                rd.flat_map(|p| {
                    fs::read_dir(p.unwrap().path().join("tmp"))
                        .into_iter()
                        .flatten()
                })
                .count()
            })
            .unwrap_or(0)
    };
    assert_eq!(tmp_leftovers(0), 0);
    let listing = |d: usize| {
        let mut v: Vec<_> = fs::read_dir(h.dir(d))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let before = listing(0);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        listing(0),
        before,
        "a released drive receives no new writes"
    );
    // B was not affected
    h.wait("B finishes", |s| status(s, 1) == DestStatus::UpToDate);
    // use the drive again and finish
    h.engine.send(Command::UseDriveAgain { slot: Slot::A });
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("all synced", |s| s.all_up_to_date);
}

#[test]
fn eject_reports_success_only_when_the_volume_really_left() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    h.sync_and_wait();

    // refused before the app has stopped using the drive
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: false,
    });
    h.wait("warning", |s| {
        s.notices
            .iter()
            .any(|n| n.text.contains("Prepare to Disconnect"))
    });
    assert!(h.ejector.calls.lock().unwrap().is_empty());

    h.engine.send(Command::PrepareDisconnect { slot: Slot::A });
    h.wait("released", |s| s.destinations[0].released);

    // failure is reported as failure
    *h.ejector.fail_with.lock().unwrap() = Some("Volume is busy".into());
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: false,
    });
    let s = h.wait("error", |s| {
        s.notices
            .iter()
            .any(|n| n.level == NoticeLevel::Error && n.text.contains("busy"))
    });
    assert!(
        !s.destinations[0]
            .disconnect_note
            .clone()
            .unwrap()
            .to_lowercase()
            .contains("safe to unplug")
    );

    // a lying eject (exit 0, volume still mounted) is not success either
    *h.ejector.fail_with.lock().unwrap() = None;
    *h.ejector.leave_mounted.lock().unwrap() = true;
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: false,
    });
    h.wait("still mounted", |s| {
        s.notices.iter().any(|n| n.text.contains("still mounted"))
    });

    // real success
    *h.ejector.leave_mounted.lock().unwrap() = false;
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: false,
    });
    let s = h.wait("ejected", |s| {
        s.destinations[0]
            .disconnect_note
            .as_deref()
            .is_some_and(|n| n.contains("safe to unplug"))
    });
    assert_eq!(s.destinations[0].connectivity, Connectivity::Offline);
}

#[test]
fn eject_asks_before_unmounting_other_volumes_on_the_same_disk() {
    let h = H::new();
    h.put("a.txt", b"data");
    h.setup();
    fs::create_dir_all(h.base.join("A2")).unwrap();
    h.resolver.add(vol(
        &h.base.join("A2"),
        "UA2",
        DriveMediaType::Ssd,
        Some("disk4"),
    ));
    h.engine.send(Command::PrepareDisconnect { slot: Slot::A });
    h.wait("released", |s| s.destinations[0].released);
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: false,
    });
    let s = h.wait("prompt", |s| s.eject_prompt.is_some());
    assert_eq!(s.eject_prompt.as_ref().unwrap().also_unmounts.len(), 1);
    assert!(h.ejector.calls.lock().unwrap().is_empty());
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: true,
    });
    h.wait("ejected", |s| {
        s.destinations[0]
            .disconnect_note
            .as_deref()
            .is_some_and(|n| n.contains("safe to unplug"))
    });
    assert_eq!(h.ejector.calls.lock().unwrap().len(), 1);
}

// ----------------------------------------------------------------- resolutions

#[test]
fn every_resolution_works_and_none_creates_a_reverse_sync() {
    let h = H::new();
    h.put("same.txt", b"identical");
    h.put("diff.txt", b"source side");
    h.put("gone-from-drive.txt", b"keep");
    h.put("gone-from-source.txt", b"old");
    h.setup();
    // pre-existing files on drive A before the first sync
    fs::write(h.dir(0).join("same.txt"), b"identical").unwrap();
    fs::write(h.dir(0).join("diff.txt"), b"drive side").unwrap();
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    let s = h.wait("collisions", |s| {
        s.destinations[0]
            .attention
            .iter()
            .any(|a| a.rel == rel("diff.txt"))
    });
    assert!(
        s.destinations[0]
            .attention
            .iter()
            .any(|a| a.rel == rel("same.txt"))
    );
    assert_eq!(
        read(h.dir(0).join("diff.txt")).unwrap(),
        b"drive side",
        "never overwritten on its own"
    );

    h.engine.send(Command::Resolve {
        slot: Slot::A,
        rel: rel("same.txt"),
        resolution: Resolution::AdoptIfIdentical,
    });
    h.wait("adopted", |s| {
        !s.destinations[0]
            .attention
            .iter()
            .any(|a| a.rel == rel("same.txt"))
    });

    h.engine.send(Command::Resolve {
        slot: Slot::A,
        rel: rel("diff.txt"),
        resolution: Resolution::ReplaceWithSource,
    });
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("replaced", |s| s.all_up_to_date);
    assert_eq!(read(h.dir(0).join("diff.txt")).unwrap(), b"source side");
    assert_eq!(
        h.engine.snapshot().destinations[0].version_count,
        1,
        "the drive's original is kept"
    );

    // deleted from the drive: waits, then Restore brings it back
    fs::remove_file(h.dir(1).join("gone-from-drive.txt")).unwrap();
    fs::remove_file(h.dir(1).join("gone-from-source.txt")).unwrap();
    h.engine.send(Command::Preview);
    h.wait("deleted noticed", |s| {
        s.destinations[1]
            .attention
            .iter()
            .any(|a| a.action == "Deleted on drive")
    });
    h.engine.send(Command::Resolve {
        slot: Slot::B,
        rel: rel("gone-from-drive.txt"),
        resolution: Resolution::RestoreFromSource,
    });
    h.engine.send(Command::Resolve {
        slot: Slot::B,
        rel: rel("gone-from-source.txt"),
        resolution: Resolution::StopManaging,
    });
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("restored", |s| {
        s.destinations[1].attention.is_empty() && status(s, 1) == DestStatus::UpToDate
    });
    assert_eq!(read(h.dir(1).join("gone-from-drive.txt")).unwrap(), b"keep");
    assert!(
        read(h.dir(1).join("gone-from-source.txt")).is_none(),
        "stop managing does not recreate"
    );

    // source file removed: surfaced, copy kept, acknowledged
    fs::remove_file(h.src.join("gone-from-source.txt")).unwrap();
    h.engine.send(Command::Preview);
    h.wait("source deletion surfaced", |s| {
        s.destinations[0]
            .attention
            .iter()
            .any(|a| a.action == "Removed from source")
    });
    assert_eq!(read(h.dir(0).join("gone-from-source.txt")).unwrap(), b"old");
    h.engine.send(Command::Resolve {
        slot: Slot::A,
        rel: rel("gone-from-source.txt"),
        resolution: Resolution::Acknowledge,
    });
    h.wait("acknowledged", |s| {
        !s.destinations[0]
            .attention
            .iter()
            .any(|a| a.action == "Removed from source")
    });
    assert_eq!(read(h.dir(0).join("gone-from-source.txt")).unwrap(), b"old");
    assert_eq!(
        read(h.src.join("same.txt")).unwrap(),
        b"identical",
        "source never modified"
    );
}

#[test]
fn full_verify_finds_corruption_that_metadata_cannot_see() {
    let h = H::new();
    h.put("a.txt", b"important data");
    h.setup();
    h.sync_and_wait();
    // flip one byte, keep size and modification time
    let p = h.dir(0).join("a.txt");
    let mtime = fs::metadata(&p).unwrap().modified().unwrap();
    let mut bytes = fs::read(&p).unwrap();
    bytes[3] ^= 0xFF;
    let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
    std::io::Write::write_all(&mut &f, &bytes).unwrap();
    f.set_modified(mtime).unwrap();
    drop(f);
    h.engine.send(Command::Preview);
    h.wait("metadata says fine", |s| {
        s.preview.as_ref().is_some_and(|p| p.rows.is_empty())
    });
    h.engine.send(Command::FullVerify { slot: Slot::A });
    let s = h.wait("verify result", |s| {
        s.destinations[0].last_verify_summary.is_some()
    });
    assert!(
        s.destinations[0]
            .last_verify_summary
            .as_ref()
            .unwrap()
            .contains("1 mismatch")
    );
    h.engine.send(Command::Preview);
    h.wait("conflict surfaced", |s| {
        s.destinations[0]
            .attention
            .iter()
            .any(|a| a.action == "Conflict")
    });
}

// ------------------------------------------------------------- legacy, auto sync

#[test]
fn old_ssd_a_b_config_is_imported_and_needs_confirmation_before_any_write() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    for d in ["home/source", "A/Backup", "B/Backup"] {
        fs::create_dir_all(base.join(d)).unwrap();
    }
    fs::write(base.join("home/source/a.txt"), b"data").unwrap();
    let paths = AppPaths::under(&base.join("app"));
    fs::create_dir_all(&paths.data_dir).unwrap();
    fs::write(
        paths.legacy_config_path(),
        format!(
            r#"{{"source":"{}","ssd_a":"{}","ssd_b":"{}"}}"#,
            base.join("home/source").display(),
            base.join("A/Backup").display(),
            base.join("B/Backup").display()
        ),
    )
    .unwrap();
    let resolver = Arc::new(FakeResolver::new());
    resolver.add(vol(
        &base.join("A"),
        "UA",
        DriveMediaType::Ssd,
        Some("disk4"),
    ));
    resolver.add(vol(
        &base.join("B"),
        "UB",
        DriveMediaType::Ssd,
        Some("disk8"),
    ));
    let ejector = Arc::new(FakeEjector::new(resolver.clone()));
    let mut engine = Engine::start(base_cfg(&paths, &resolver, &ejector)).unwrap();
    engine.send(Command::RefreshMounts);
    let s = engine
        .wait_for(T, |s| {
            s.destinations
                .iter()
                .all(|d| matches!(d.connectivity, Connectivity::NeedsConfirmation(_)))
        })
        .expect("legacy drives wait for confirmation");
    assert_eq!(s.destinations[0].label, "SSD A");
    assert_eq!(s.destinations[1].label, "SSD B");
    assert!(s.notices.iter().any(|n| n.text.contains("Imported")));
    engine.send(Command::SyncNow {
        preview_signature: None,
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        fs::read(base.join("A/Backup/a.txt")).is_err(),
        "no write before confirmation"
    );

    engine.send(Command::ConfirmIdentity { slot: Slot::A });
    engine.send(Command::ConfirmIdentity { slot: Slot::B });
    engine
        .wait_for(T, |s| {
            s.destinations
                .iter()
                .all(|d| matches!(d.connectivity, Connectivity::Online { .. }))
        })
        .expect("confirmed");
    engine.send(Command::SyncNow {
        preview_signature: None,
    });
    engine
        .wait_for(T, |s| s.all_up_to_date)
        .expect("synced after confirmation");
    assert_eq!(fs::read(base.join("A/Backup/a.txt")).unwrap(), b"data");
    assert!(paths.data_dir.join("config.json.migrated").exists());
    engine.shutdown();
}

#[test]
fn auto_sync_notices_changes_debounces_and_copies() {
    let h = H::with(|c| c.start_monitor = true);
    h.put("first.txt", b"1");
    h.setup();
    let mut settings = h.engine.snapshot().settings.clone();
    settings.debounce_ms = 200;
    h.engine.send(Command::SaveSettings(settings));
    h.engine.send(Command::SetAutoSync(true));
    h.wait("initial sync", |s| s.all_up_to_date);
    // a burst of changes becomes one batch, not one sync per file
    for i in 0..20 {
        h.put(&format!("burst/{i}.txt"), format!("file {i}").as_bytes());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    loop {
        let n = fs::read_dir(h.dir(1).join("burst"))
            .map(|r| r.count())
            .unwrap_or(0);
        if n == 20 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "auto sync did not copy the burst ({n}/20)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    h.wait("settles", |s| s.all_up_to_date);
    let syncs = h
        .engine
        .snapshot()
        .activity
        .iter()
        .filter(|e| e.kind == "sync" && e.destination_id.is_some())
        .count();
    assert!(
        syncs <= 8,
        "burst was coalesced, got {syncs} sync batches for 2 drives"
    );
}

#[test]
fn a_folder_removed_from_the_source_is_reported_kept_and_can_be_dismissed() {
    let h = H::new();
    h.put("keep.txt", b"x");
    fs::create_dir(h.src.join("untitled folder")).unwrap();
    h.setup();
    h.sync_and_wait();
    for d in 0..2 {
        assert!(h.dir(d).join("untitled folder").is_dir());
    }
    // renamed in the source: the old, empty folder must not linger unannounced
    fs::rename(
        h.src.join("untitled folder"),
        h.src.join("prewedding-photo"),
    )
    .unwrap();
    h.engine.send(Command::Preview);
    let s = h.wait("orphan surfaced", |s| {
        s.destinations.iter().all(|d| {
            d.attention
                .iter()
                .any(|a| a.rel == rel("untitled folder") && a.action == "Removed from source")
        })
    });
    assert!(
        s.destinations[0]
            .attention
            .iter()
            .any(|a| a.resolutions.contains(&Resolution::Acknowledge))
    );
    for d in 0..2 {
        assert!(h.dir(d).join("untitled folder").is_dir(), "never deleted");
    }
    h.engine.send(Command::Resolve {
        slot: Slot::A,
        rel: rel("untitled folder"),
        resolution: Resolution::Acknowledge,
    });
    h.wait("dismissed on A", |s| {
        !s.destinations[0]
            .attention
            .iter()
            .any(|a| a.rel == rel("untitled folder"))
    });
    assert!(
        h.dir(0).join("untitled folder").is_dir(),
        "dismissing keeps the folder"
    );
    assert!(
        h.engine.snapshot().destinations[1]
            .attention
            .iter()
            .any(|a| a.rel == rel("untitled folder")),
        "each drive is dismissed separately"
    );
}

// ------------------------------------------------ quiet drives, and an audit trail

fn sync_events(s: &UiSnapshot) -> usize {
    s.activity
        .iter()
        .filter(|e| e.kind == "sync" && e.destination_id.is_some())
        .count()
}

/// Monitor on, Auto Sync on, everything already synced.
fn watching_harness() -> H {
    let h = H::with(|c| c.start_monitor = true);
    h.put("first.txt", b"1");
    h.setup();
    let mut settings = h.engine.snapshot().settings.clone();
    settings.debounce_ms = 200;
    h.engine.send(Command::SaveSettings(settings));
    h.engine.send(Command::SetAutoSync(true));
    h.wait("initial sync", |s| s.all_up_to_date);
    // let the watcher settle after our own setup writes
    std::thread::sleep(Duration::from_millis(1500));
    h
}

#[test]
fn changes_to_excluded_files_never_cause_a_scan_or_wake_the_drives() {
    let h = watching_harness();
    let (scans0, syncs0) = {
        let s = h.engine.snapshot();
        (s.scans_completed, sync_events(&s))
    };
    // what Finder and macOS do to a folder you merely look at
    for i in 0..5 {
        fs::write(h.src.join(".DS_Store"), format!("junk {i}")).unwrap();
        fs::write(h.src.join("._sidecar"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(3)); // far beyond the 200 ms debounce
    let s = h.engine.snapshot();
    assert_eq!(
        s.scans_completed, scans0,
        "an excluded file must not trigger a scan"
    );
    assert_eq!(sync_events(&s), syncs0, "and must not read the drives");

    // control: the watcher is alive, a real change still flows through
    h.put("real.txt", b"data");
    h.wait("real change synced", |s| {
        s.scans_completed > scans0 && sync_events(s) > syncs0
    });
    h.wait("up to date", |s| s.all_up_to_date);
    assert_eq!(read(h.dir(1).join("real.txt")).unwrap(), b"data");
}

#[test]
fn an_unchanged_source_after_file_events_leaves_the_drives_alone_but_user_actions_do_not() {
    let h = watching_harness();
    let (scans0, syncs0) = {
        let s = h.engine.snapshot();
        (s.scans_completed, sync_events(&s))
    };
    // a real file-system event whose net effect on the source is nothing
    fs::write(h.src.join("flash.txt"), b"here and gone").unwrap();
    fs::remove_file(h.src.join("flash.txt")).unwrap();
    h.wait("scan ran", |s| s.scans_completed > scans0);
    std::thread::sleep(Duration::from_millis(800));
    let s = h.engine.snapshot();
    assert_eq!(
        sync_events(&s),
        syncs0,
        "identical source: no read of either drive"
    );
    assert!(s.all_up_to_date, "and the claim stays truthful");

    // the same situation, but the user asks: the drives are read
    h.engine.send(Command::SyncNow {
        preview_signature: None,
    });
    h.wait("drives read on request", |s| sync_events(s) > syncs0);
}

#[test]
fn drive_lifecycle_and_controls_leave_an_audit_trail_without_paths() {
    let h = H::new();
    h.put("a.txt", b"x");
    h.setup();
    let has = |s: &UiSnapshot, kind: &str, needle: &str| {
        s.activity
            .iter()
            .any(|e| e.kind == kind && e.detail.contains(needle))
    };
    let s = h.engine.snapshot();
    assert!(has(&s, "setup", "source folder set"));
    assert!(has(&s, "setup", "Drive A set up"), "{:#?}", s.activity);
    assert!(has(&s, "drive", "connected: B (hdd, exfat)"));

    // unplugged and back
    let b_vol = h
        .resolver
        .mounted_volumes()
        .unwrap()
        .into_iter()
        .find(|v| v.volume_uuid.as_deref() == Some("UB"))
        .unwrap();
    h.resolver.remove_mount(&h.base.join("B"));
    h.engine.send(Command::RefreshMounts);
    h.wait("disconnect recorded", |s| has(s, "drive", "disconnected"));
    h.resolver.add(b_vol);
    h.engine.send(Command::RefreshMounts);
    h.wait("reconnect recorded", |s| {
        s.activity
            .iter()
            .filter(|e| e.kind == "drive" && e.detail.starts_with("connected: B"))
            .count()
            >= 2
    });

    // controls
    h.engine.send(Command::Pause);
    h.engine.send(Command::Resume);
    h.engine.send(Command::SetAutoSync(true));
    h.wait("controls recorded", |s| {
        has(s, "control", "paused")
            && has(s, "control", "resumed")
            && has(s, "control", "Auto Sync on")
    });

    // prepare + failed eject + real eject
    h.engine.send(Command::PrepareDisconnect { slot: Slot::A });
    h.wait("released recorded", |s| {
        has(s, "disconnect", "closed its files")
    });
    assert!(
        h.engine
            .snapshot()
            .activity
            .iter()
            .any(|e| e.kind == "disconnect" && e.detail.contains("requested"))
    );
    *h.ejector.fail_with.lock().unwrap() = Some("Volume is busy".into());
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: false,
    });
    h.wait("failed eject recorded", |s| {
        s.activity
            .iter()
            .any(|e| e.kind == "eject" && e.result == "failed" && e.detail.contains("busy"))
    });
    *h.ejector.fail_with.lock().unwrap() = None;
    h.engine.send(Command::Eject {
        slot: Slot::A,
        confirm_siblings: false,
    });
    h.wait("eject recorded", |s| {
        s.activity
            .iter()
            .any(|e| e.kind == "eject" && e.result == "ok")
    });

    // privacy: nothing recorded contains a folder path or the temp directory
    let base = h.base.to_string_lossy().to_string();
    let s = h.engine.snapshot();
    assert!(
        s.activity
            .iter()
            .all(|e| !e.detail.contains(&base) && !e.detail.contains("/Volumes")),
        "{:#?}",
        s.activity
    );
}

#[test]
fn rejected_setups_are_recorded_with_the_reason() {
    let h = H::new();
    h.engine.send(Command::SetSource(h.src.clone()));
    h.wait("source", |s| s.source.path.is_some());
    let inside = h.src.join("backup-here");
    fs::create_dir(&inside).unwrap();
    h.engine.send(Command::ConfigureDestination {
        slot: Slot::A,
        path: inside,
        label: "A".into(),
    });
    h.wait("rejection recorded", |s| {
        s.activity
            .iter()
            .any(|e| e.kind == "setup" && e.result == "rejected" && e.detail.contains("overlap"))
    });
}
