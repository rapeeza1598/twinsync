//! Coordinator. Owns the state machine and the database, never touches a
//! drive: scanning runs in a short-lived thread, every destination in its own
//! worker. So a slow or sleeping drive can never freeze it or the UI.

pub mod snapshot;
mod worker;

pub use snapshot::*;

use crate::config::{AppPaths, Settings};
use crate::devices::{
    DestCheck, DeviceResolver, Ejector, Presence, VolumeInfo, check_destination_choice, find_volume,
};
use crate::domain::{
    Connectivity, DestStatus, EntryKind, RelPath, Slot, StatusInputs, WorkerState, derive_status,
};
use crate::errors::{Error, Result};
use crate::monitor::{Callbacks, FsSignal, Monitor};
use crate::planner::{Action, Plan, plan_signature};
use crate::scanner::{Exclusions, ScanOutcome, ScanResult, scan_source};
use crate::storage::{ActivityFilter, Db, DestEntry, DestinationRow, EntryStatus, Project, legacy};
use crate::transfer::FaultHook;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use worker::{PlanJob, WorkerCmd, WorkerEnv, now_ms};

// --------------------------------------------------------------- messages

pub(crate) enum Msg {
    Ui(Command),
    Scan(ScanMsg),
    Worker(usize, WorkerEvent),
    Mounts(Vec<VolumeInfo>),
    Fs(FsSignal),
    Wake,
    SetupChecked {
        slot: Slot,
        label: String,
        result: Result<DestCheck>,
    },
    IdentityResolved {
        slot: Slot,
        /// The folder the user means, used to derive the path relative to the volume.
        target: PathBuf,
        result: Result<VolumeInfo>,
    },
    EjectNeedsConfirm {
        slot: Slot,
        also: Vec<String>,
    },
    EjectDone {
        slot: Slot,
        result: std::result::Result<String, String>,
    },
}

pub(crate) enum ScanMsg {
    Done {
        result: Arc<ScanResult>,
        generation: i64,
    },
    Unavailable(String),
    Cancelled,
    Failed(String),
}

pub(crate) enum WorkerEvent {
    Connected {
        connectivity: Connectivity,
        free: Option<u64>,
        notes: Vec<String>,
        permission_denied: bool,
    },
    State(WorkerState),
    Progress(ProgressView),
    PlanReady {
        plan: Plan,
        scan_gen: i64,
        executed: bool,
        stale_preview: bool,
        summary: Option<BatchSummary>,
    },
    Free(u64),
    Resolved(std::result::Result<String, String>),
    VerifyDone(String, bool),
    Failed(String),
    Stopped,
}

#[derive(Clone, Debug, Default)]
pub struct BatchSummary {
    pub done: u64,
    pub unchanged: u64,
    pub stale: u64,
    pub failed: u64,
    pub bytes: u64,
    pub stopped_early: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Job {
    /// Source scan only; drives are not touched.
    ScanOnly,
    /// Plan against drives to refresh "pending", without showing a preview table.
    Refresh,
    Preview,
    Sync(Option<u64>),
}

/// Why a job was requested. It decides whether the drives may be left alone after an
/// unchanged scan, and it is written to the log so every scan can be explained later.
/// Ordered by how much the requester cares about fresh drive state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    FsEvent,
    Startup,
    Timer,
    Wake,
    Mount,
    User,
}

impl Origin {
    fn label(self) -> &'static str {
        match self {
            Origin::FsEvent => "fs-event",
            Origin::Startup => "startup",
            Origin::Timer => "timer",
            Origin::Wake => "wake",
            Origin::Mount => "mount",
            Origin::User => "user",
        }
    }
}

// ----------------------------------------------------------------- config

#[derive(Clone)]
pub struct EngineConfig {
    pub paths: AppPaths,
    pub resolver: Arc<dyn DeviceResolver>,
    pub ejector: Arc<dyn Ejector>,
    pub fault: Option<FaultHook>,
    /// Called after every published change (the GUI requests a repaint here).
    pub on_change: Arc<dyn Fn() + Send + Sync>,
    pub poll_interval: Duration,
    pub start_monitor: bool,
    pub retry_base: Duration,
    /// Granularity of timers (debounce, reconciliation, stall detection).
    pub tick: Duration,
    /// No progress for this long while busy is shown as "Waiting for drive".
    pub stall_after: Duration,
}

impl EngineConfig {
    pub fn new(
        paths: AppPaths,
        resolver: Arc<dyn DeviceResolver>,
        ejector: Arc<dyn Ejector>,
    ) -> Self {
        Self {
            paths,
            resolver,
            ejector,
            fault: None,
            on_change: Arc::new(|| {}),
            poll_interval: Duration::from_secs(3),
            start_monitor: true,
            retry_base: Duration::from_secs(1),
            tick: Duration::from_secs(1),
            stall_after: Duration::from_secs(20),
        }
    }
}

pub struct Engine {
    tx: Sender<Msg>,
    snapshot: Arc<Mutex<Arc<UiSnapshot>>>,
    join: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(cfg: EngineConfig) -> Result<Engine> {
        std::fs::create_dir_all(&cfg.paths.data_dir)
            .map_err(|e| Error::io("create data directory", e))?;
        let db = Db::open(&cfg.paths.db_path())?;
        let mut notices = Vec::new();
        match legacy::import_if_present(&db, &cfg.paths.legacy_config_path()) {
            Ok(true) => notices.push(Notice {
                level: NoticeLevel::Info,
                text: "Imported your SSD A / SSD B setup. Confirm each drive before TwinSync writes to it.".into(),
            }),
            Ok(false) => {}
            Err(e) => notices.push(Notice {
                level: NoticeLevel::Warning,
                text: format!("Old configuration was not imported: {e}"),
            }),
        }
        let project = match db.first_project()? {
            Some(p) => p,
            None => db.create_project("TwinSync", None)?,
        };
        let (tx, rx) = channel();
        let snapshot = Arc::new(Mutex::new(Arc::new(UiSnapshot::default())));
        let mut c = Coordinator::new(cfg, db, project, tx.clone(), snapshot.clone(), notices)?;
        let join = std::thread::Builder::new()
            .name("twinsync-coordinator".into())
            .spawn(move || c.run(rx))
            .map_err(|e| Error::io("start coordinator", e))?;
        Ok(Engine {
            tx,
            snapshot,
            join: Some(join),
        })
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(Msg::Ui(c));
    }

    /// Cheap: clones an `Arc`.
    pub fn snapshot(&self) -> Arc<UiSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Poll until `pred` holds or the timeout passes. For tests and headless use.
    pub fn wait_for(
        &self,
        timeout: Duration,
        pred: impl Fn(&UiSnapshot) -> bool,
    ) -> Option<Arc<UiSnapshot>> {
        let until = Instant::now() + timeout;
        loop {
            let s = self.snapshot();
            if pred(&s) {
                return Some(s);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn shutdown(&mut self) {
        self.send(Command::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ------------------------------------------------------------ coordinator

struct DestRt {
    row: Option<DestinationRow>,
    tx: Sender<WorkerCmd>,
    handle: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    last_progress: Arc<AtomicU64>,
    state: WorkerState,
    connectivity: Connectivity,
    volume: Option<VolumeInfo>,
    progress: Option<ProgressView>,
    last_plan: Option<Plan>,
    attention: Arc<Vec<AttentionItem>>,
    last_error: Option<String>,
    permission_denied: bool,
    /// Commands sent to the worker that have not finished yet.
    outstanding: u32,
    held: bool,
    released: bool,
    disconnect_note: Option<String>,
    free: Option<u64>,
    verify_summary: Option<String>,
    version_count: usize,
    version_bytes: u64,
}

impl DestRt {
    fn busy(&self) -> bool {
        self.outstanding > 0
    }
}

struct Coordinator {
    cfg: EngineConfig,
    db: Db,
    project: Project,
    tx: Sender<Msg>,
    snapshot: Arc<Mutex<Arc<UiSnapshot>>>,
    rev: u64,
    changed: bool,
    dests: [DestRt; 2],
    paused: Arc<AtomicBool>,
    shared_gate: Arc<AtomicBool>,
    monitor: Option<Monitor>,
    volumes: Vec<VolumeInfo>,

    scan_running: bool,
    scan_cancel: Arc<AtomicBool>,
    latest_scan: Option<Arc<ScanResult>>,
    latest_gen: i64,
    source_unavailable: Option<String>,
    wanted: Option<Job>,
    wanted_origin: Origin,
    /// True only if every request merged into `wanted` came from file-system events.
    wanted_fs_only: bool,
    active_job: Option<Job>,
    active_origin: Origin,
    active_fs_only: bool,
    /// Origin of the most recent dispatch to the drives (for the batch log line).
    dispatch_origin: Origin,
    scans_completed: u64,
    awaiting: HashSet<usize>,
    build_preview: bool,
    preview_stale: bool,
    preview: Option<Arc<PreviewView>>,
    preview_sigs: [u64; 2],

    dirty_first: Option<Instant>,
    dirty_last: Option<Instant>,
    next_reconcile: Instant,
    shutting_down: bool,

    pending_setup: [Option<(DestCheck, String)>; 2],
    setup_report: Option<SetupReport>,
    eject_prompt: Option<EjectPrompt>,
    notices: Vec<Notice>,
    activity: Arc<Vec<crate::storage::ActivityEvent>>,
    activity_filter: ActivityFilter,
}

fn idx(slot: Slot) -> usize {
    match slot {
        Slot::A => 0,
        Slot::B => 1,
    }
}

fn slot_of(i: usize) -> Slot {
    if i == 0 { Slot::A } else { Slot::B }
}

impl Coordinator {
    fn new(
        cfg: EngineConfig,
        db: Db,
        project: Project,
        tx: Sender<Msg>,
        snapshot: Arc<Mutex<Arc<UiSnapshot>>>,
        notices: Vec<Notice>,
    ) -> Result<Self> {
        let paused = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Mutex::new(()));
        let shared_gate = Arc::new(AtomicBool::new(false));
        let rows = db.destinations(&project.id)?;
        let mk = |i: usize| -> DestRt {
            let (wtx, wrx) = channel();
            let cancel = Arc::new(AtomicBool::new(false));
            let last_progress = Arc::new(AtomicU64::new(now_ms()));
            let handle = worker::spawn(
                WorkerEnv {
                    idx: i,
                    db_path: cfg.paths.db_path(),
                    tx: tx.clone(),
                    cancel: cancel.clone(),
                    paused: paused.clone(),
                    last_progress: last_progress.clone(),
                    fault: cfg.fault.clone(),
                    gate: gate.clone(),
                    shared_gate: shared_gate.clone(),
                    retry_base: cfg.retry_base,
                },
                wrx,
            );
            let row = rows.iter().find(|r| r.slot == slot_of(i)).cloned();
            DestRt {
                row,
                tx: wtx,
                handle: Some(handle),
                cancel,
                last_progress,
                state: WorkerState::Idle,
                connectivity: Connectivity::Offline,
                volume: None,
                progress: None,
                last_plan: None,
                attention: Arc::new(Vec::new()),
                last_error: None,
                permission_denied: false,
                outstanding: 0,
                held: false,
                released: false,
                disconnect_note: None,
                free: None,
                verify_summary: None,
                version_count: 0,
                version_bytes: 0,
            }
        };
        let dests = [mk(0), mk(1)];
        let mut c = Coordinator {
            next_reconcile: Instant::now()
                + Duration::from_secs(project.settings.reconcile_minutes.max(1) * 60),
            cfg,
            db,
            project,
            tx,
            snapshot,
            rev: 0,
            changed: true,
            dests,
            paused,
            shared_gate,
            monitor: None,
            volumes: Vec::new(),
            scan_running: false,
            scan_cancel: Arc::new(AtomicBool::new(false)),
            latest_scan: None,
            latest_gen: 0,
            source_unavailable: None,
            wanted: None,
            wanted_origin: Origin::Startup,
            wanted_fs_only: false,
            active_job: None,
            active_origin: Origin::Startup,
            active_fs_only: false,
            dispatch_origin: Origin::Startup,
            scans_completed: 0,
            awaiting: HashSet::new(),
            build_preview: false,
            preview_stale: false,
            preview: None,
            preview_sigs: [0; 2],
            dirty_first: None,
            dirty_last: None,
            shutting_down: false,
            pending_setup: [None, None],
            setup_report: None,
            eject_prompt: None,
            notices,
            activity: Arc::new(Vec::new()),
            activity_filter: ActivityFilter::default(),
        };
        c.reload_activity();
        for i in 0..2 {
            c.refresh_versions(i);
        }
        c.update_shared_gate();
        if c.cfg.start_monitor {
            c.start_monitor();
        }
        if c.project.source_path.is_some() {
            c.watch_source();
            c.request(Job::ScanOnly, Origin::Startup);
        }
        Ok(c)
    }

    fn start_monitor(&mut self) {
        let (a, b, c) = (self.tx.clone(), self.tx.clone(), self.tx.clone());
        let cb = Callbacks {
            on_fs: Arc::new(move |s| {
                let _ = a.send(Msg::Fs(s));
            }),
            on_mounts: Arc::new(move |v| {
                let _ = b.send(Msg::Mounts(v));
            }),
            on_wake: Arc::new(move || {
                let _ = c.send(Msg::Wake);
            }),
        };
        self.monitor = Some(Monitor::start(
            self.cfg.resolver.clone(),
            self.cfg.poll_interval,
            cb,
        ));
    }

    fn watch_source(&mut self) {
        let p = self.project.source_path.clone();
        // Changes to excluded files (Finder's .DS_Store, ._*, ...) must not cause a scan.
        let ignore: crate::monitor::IgnoreFn =
            match (&p, Exclusions::new(&self.settings().exclusions)) {
                (Some(root), Ok(ex)) => {
                    let root = root.clone();
                    Arc::new(move |abs: &Path| ex.matches_path(&root, abs))
                }
                _ => Arc::new(|_: &Path| false),
            };
        if let Some(m) = &mut self.monitor {
            m.watch_source(p.as_deref(), ignore);
        }
    }

    /// One place for "something happened": an Activity row, a log line (counts, labels
    /// and volume names only, never file names or folder paths) and a repaint.
    fn record(&mut self, dest: Option<usize>, kind: &str, result: &str, detail: &str) {
        let id = dest.and_then(|i| self.dests[i].row.as_ref().map(|r| r.id.clone()));
        let _ = self
            .db
            .log_event(id.as_deref(), kind, None, result, detail, false);
        tracing::info!(
            drive = dest.map_or("-", |i| slot_of(i).as_str()),
            kind,
            result,
            detail,
            "event"
        );
        self.reload_activity();
    }

    fn settings(&self) -> &Settings {
        &self.project.settings
    }

    fn notice_once(&mut self, level: NoticeLevel, text: &str) {
        if !self.notices.iter().any(|n| n.text == text) {
            self.notice(level, text);
        }
    }

    fn notice(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.notices.push(Notice {
            level,
            text: text.into(),
        });
        if self.notices.len() > 20 {
            self.notices.remove(0);
        }
    }

    // ------------------------------------------------------------ main loop

    fn run(&mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(self.cfg.tick) {
                Ok(Msg::Ui(Command::Shutdown)) => break,
                Ok(m) => {
                    self.handle(m);
                    self.changed = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.timers();
            if self.changed {
                self.publish();
            }
        }
        self.shutting_down = true;
        self.scan_cancel.store(true, Ordering::Relaxed);
        for d in &self.dests {
            d.cancel.store(true, Ordering::Relaxed);
            let _ = d.tx.send(WorkerCmd::Shutdown);
        }
        for d in &mut self.dests {
            if let Some(h) = d.handle.take() {
                let _ = h.join();
            }
        }
        if let Some(m) = &mut self.monitor {
            m.stop();
        }
    }

    fn timers(&mut self) {
        let now = Instant::now();
        if let (Some(first), Some(last)) = (self.dirty_first, self.dirty_last) {
            let quiet = Duration::from_millis(self.settings().debounce_ms.max(1));
            if now.duration_since(last) >= quiet
                || now.duration_since(first) >= Duration::from_secs(10)
            {
                self.dirty_first = None;
                self.dirty_last = None;
                self.request(self.automatic_job(), Origin::FsEvent);
            }
        }
        if now >= self.next_reconcile {
            self.next_reconcile =
                now + Duration::from_secs(self.settings().reconcile_minutes.max(1) * 60);
            self.request(self.automatic_job(), Origin::Timer);
        }
        // Stall display only changes with time, so keep republishing while busy.
        if self.dests.iter().any(|d| d.busy()) {
            self.changed = true;
        }
    }

    /// What a signal from the file system or a timer should trigger. With Auto
    /// Sync off only the source is scanned, so sleeping drives stay asleep.
    fn automatic_job(&self) -> Job {
        if self.settings().auto_sync && !self.paused.load(Ordering::Relaxed) {
            Job::Sync(None)
        } else {
            Job::ScanOnly
        }
    }

    // ------------------------------------------------------------- dispatch

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Ui(c) => self.command(c),
            Msg::Scan(s) => self.on_scan(s),
            Msg::Worker(i, e) => self.on_worker(i, e),
            Msg::Mounts(v) => self.on_mounts(v),
            Msg::Fs(sig) => {
                let now = Instant::now();
                self.dirty_first.get_or_insert(now);
                self.dirty_last = Some(now);
                if sig == FsSignal::Overflow {
                    self.dirty_first = Some(now - Duration::from_secs(60)); // do not wait for quiet
                }
            }
            Msg::Wake => {
                self.refresh_mounts();
                self.request(self.automatic_job(), Origin::Wake);
            }
            Msg::SetupChecked {
                slot,
                label,
                result,
            } => self.on_setup_checked(slot, label, result),
            Msg::IdentityResolved {
                slot,
                target,
                result,
            } => self.on_identity_resolved(slot, target, result),
            Msg::EjectNeedsConfirm { slot, also } => {
                self.eject_prompt = Some(EjectPrompt {
                    slot,
                    also_unmounts: also,
                });
            }
            Msg::EjectDone { slot, result } => {
                self.eject_prompt = None;
                match result {
                    Ok(text) => {
                        let d = &mut self.dests[idx(slot)];
                        d.disconnect_note = Some(text.clone());
                        d.connectivity = Connectivity::Offline;
                        d.volume = None;
                        d.held = false;
                        d.released = false;
                        self.record(Some(idx(slot)), "eject", "ok", &text);
                    }
                    Err(e) => {
                        self.record(Some(idx(slot)), "eject", "failed", &e);
                        self.notice(NoticeLevel::Error, e)
                    }
                }
            }
        }
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::SetSource(p) => self.set_source(&p),
            Command::ConfigureDestination { slot, path, label } => {
                self.configure(slot, path, label)
            }
            Command::ConfirmSetup { slot } => {
                self.setup_report = None;
                if let Some((check, label)) = self.pending_setup[idx(slot)].take() {
                    self.apply_setup(slot, check, label);
                }
            }
            Command::CancelSetup { slot } => {
                self.pending_setup[idx(slot)] = None;
                self.setup_report = None;
            }
            Command::ConfirmIdentity { slot } => self.confirm_identity(slot),
            Command::RenameDestination { slot, label } => {
                if let Some(r) = &mut self.dests[idx(slot)].row {
                    r.label = label;
                    let _ = self.db.upsert_destination(r);
                }
            }
            Command::SetProfile { slot, profile } => {
                if let Some(r) = &mut self.dests[idx(slot)].row {
                    r.profile = profile;
                    let _ = self.db.upsert_destination(r);
                }
            }
            Command::Preview => self.request(Job::Preview, Origin::User),
            Command::SyncNow { preview_signature } => {
                self.paused.store(false, Ordering::Relaxed);
                self.request(Job::Sync(preview_signature), Origin::User);
            }
            Command::Pause => {
                self.paused.store(true, Ordering::Relaxed);
                self.record(None, "control", "ok", "paused");
            }
            Command::Resume => {
                self.paused.store(false, Ordering::Relaxed);
                self.record(None, "control", "ok", "resumed");
                self.request(Job::Sync(None), Origin::User);
            }
            Command::RetryFailed => self.request(Job::Sync(None), Origin::User),
            Command::SetAutoSync(on) => {
                self.project.settings.auto_sync = on;
                let _ = self
                    .db
                    .save_settings(&self.project.id, &self.project.settings);
                self.record(
                    None,
                    "control",
                    "ok",
                    if on { "Auto Sync on" } else { "Auto Sync off" },
                );
                if on {
                    self.request(Job::Sync(None), Origin::User);
                }
            }
            Command::SaveSettings(s) => {
                self.project.settings = s;
                let _ = self
                    .db
                    .save_settings(&self.project.id, &self.project.settings);
                self.next_reconcile = Instant::now()
                    + Duration::from_secs(self.settings().reconcile_minutes.max(1) * 60);
                self.watch_source();
                self.record(None, "control", "ok", "settings saved");
            }
            Command::FullVerify { slot } => {
                let d = &mut self.dests[idx(slot)];
                if matches!(d.connectivity, Connectivity::Online { .. }) && !d.busy() && !d.held {
                    d.outstanding += 1;
                    d.verify_summary = None;
                    let _ = d.tx.send(WorkerCmd::FullVerify);
                }
            }
            Command::PrepareDisconnect { slot } => self.prepare_disconnect(slot),
            Command::UseDriveAgain { slot } => {
                let d = &mut self.dests[idx(slot)];
                d.held = false;
                d.released = false;
                d.disconnect_note = None;
                self.request(Job::Refresh, Origin::User);
            }
            Command::Eject {
                slot,
                confirm_siblings,
            } => self.eject(slot, confirm_siblings),
            Command::CancelEject => self.eject_prompt = None,
            Command::Resolve {
                slot,
                rel,
                resolution,
            } => self.resolve(slot, rel, resolution),
            Command::LoadActivity(f) => {
                self.activity_filter = f;
                self.reload_activity();
            }
            Command::RefreshMounts => self.refresh_mounts(),
            Command::DismissNotice(i) => {
                if i < self.notices.len() {
                    self.notices.remove(i);
                }
            }
            Command::Shutdown => {}
        }
    }

    // --------------------------------------------------------------- source

    fn set_source(&mut self, p: &Path) {
        let canon = match std::fs::canonicalize(p) {
            Ok(c) if c.is_dir() => c,
            Ok(_) => return self.notice(NoticeLevel::Error, "The source must be a folder."),
            Err(e) => {
                return self.notice(NoticeLevel::Error, format!("Cannot use that folder: {e}"));
            }
        };
        for d in &self.dests {
            if let Some(r) = &d.row
                && let Some(m) = &r.last_mount_path
            {
                let root = m.join(&r.root_rel_path);
                if canon.starts_with(&root) || root.starts_with(&canon) {
                    return self.notice(
                        NoticeLevel::Error,
                        "The source overlaps a destination folder; choose a different source.",
                    );
                }
            }
        }
        if let Err(e) = self.db.set_source_path(&self.project.id, &canon) {
            return self.notice(NoticeLevel::Error, e.to_string());
        }
        self.reload_project();
        self.latest_scan = None;
        self.source_unavailable = None;
        self.preview = None;
        for d in &mut self.dests {
            d.last_plan = None;
            d.attention = Arc::new(Vec::new());
        }
        self.watch_source();
        self.record(None, "setup", "ok", "source folder set");
        self.request(Job::ScanOnly, Origin::User);
    }

    fn reload_project(&mut self) {
        if let Ok(Some(p)) = self.db.first_project() {
            self.project = p;
        }
    }

    // ---------------------------------------------------------------- jobs

    fn request(&mut self, j: Job, origin: Origin) {
        if self.shutting_down {
            return;
        }
        let j = match j {
            Job::Sync(_) if self.paused.load(Ordering::Relaxed) => Job::Refresh,
            other => other,
        };
        let first = self.wanted.is_none();
        self.wanted = Some(self.wanted.map_or(j, |w| w.max(j)));
        let fs = origin == Origin::FsEvent;
        self.wanted_fs_only = if first { fs } else { self.wanted_fs_only && fs };
        self.wanted_origin = if first {
            origin
        } else {
            self.wanted_origin.max(origin)
        };
        self.try_start();
    }

    fn try_start(&mut self) {
        let Some(job) = self.wanted else { return };
        if self.scan_running || self.dests.iter().any(|d| d.busy()) {
            return; // a pending request is picked up when the current work ends
        }
        let Some(source) = self.project.source_path.clone() else {
            self.wanted = None;
            return;
        };
        self.wanted = None;
        self.active_job = Some(job);
        self.active_origin = self.wanted_origin;
        self.active_fs_only = self.wanted_fs_only;
        self.scan_running = true;
        self.scan_cancel.store(false, Ordering::Relaxed);
        let (tx, cancel) = (self.tx.clone(), self.scan_cancel.clone());
        let (db_path, project_id) = (self.cfg.paths.db_path(), self.project.id.clone());
        let hash_all = self.settings().hash_all_source_files;
        let patterns = self.settings().exclusions.clone();
        std::thread::spawn(move || {
            let msg = (|| -> ScanMsg {
                let ex = match Exclusions::new(&patterns) {
                    Ok(e) => e,
                    Err(e) => return ScanMsg::Failed(e.to_string()),
                };
                match scan_source(&source, &ex, hash_all, &cancel) {
                    Ok(ScanOutcome::Done(r)) => match Db::open(&db_path)
                        .and_then(|db| db.commit_scan(&project_id, &r.entries, r.complete))
                    {
                        Ok(generation) => ScanMsg::Done {
                            result: Arc::new(r),
                            generation,
                        },
                        Err(e) => ScanMsg::Failed(e.to_string()),
                    },
                    Ok(ScanOutcome::SourceUnavailable(e)) => ScanMsg::Unavailable(e),
                    Err(Error::Cancelled) => ScanMsg::Cancelled,
                    Err(e) => ScanMsg::Failed(e.to_string()),
                }
            })();
            let _ = tx.send(Msg::Scan(msg));
        });
    }

    fn on_scan(&mut self, s: ScanMsg) {
        self.scan_running = false;
        let job = self.active_job.take();
        match s {
            ScanMsg::Done { result, generation } => {
                tracing::info!(
                    entries = result.entries.len(),
                    complete = result.complete,
                    generation,
                    trigger = self.active_origin.label(),
                    "source scan finished"
                );
                self.source_unavailable = None;
                self.scans_completed += 1;
                let (prev_scan, prev_gen) = (self.latest_scan.take(), self.latest_gen);
                self.latest_gen = generation;
                self.latest_scan = Some(result.clone());
                self.reload_project();
                self.dispatch_origin = self.active_origin;
                if let Some(job) = job {
                    if self.unchanged_since_last_plan(job, prev_scan.as_deref(), &result, prev_gen)
                    {
                        tracing::info!(
                            trigger = self.active_origin.label(),
                            "source unchanged; drives not touched"
                        );
                    } else {
                        self.dispatch(job);
                    }
                }
            }
            ScanMsg::Unavailable(e) => {
                tracing::warn!("source folder is unavailable");
                // Not an empty folder: nothing is planned and nothing is concluded.
                self.source_unavailable = Some(e);
                self.latest_scan = None;
                self.preview = None;
            }
            ScanMsg::Cancelled => {}
            ScanMsg::Failed(e) => {
                tracing::warn!(error = %e, "source scan failed");
                self.notice(NoticeLevel::Error, format!("Scan failed: {e}"))
            }
        }
        self.try_start();
    }

    /// After a scan caused only by file-system events: if the source is identical to the
    /// previous scan and every connected drive was already fully up to date at that
    /// generation, there is nothing to find out, so the drives are left alone (a sleeping
    /// HDD stays asleep). The plans' generation moves forward so "up to date" stays true.
    ///
    /// Anything the user asks for, the periodic timer, wake and mount changes never take
    /// this shortcut: they are exactly how drive-side changes are noticed.
    fn unchanged_since_last_plan(
        &mut self,
        job: Job,
        prev: Option<&ScanResult>,
        new: &ScanResult,
        prev_gen: i64,
    ) -> bool {
        if !self.active_fs_only || !matches!(job, Job::Sync(None) | Job::ScanOnly) {
            return false;
        }
        if !prev.is_some_and(|p| p.same_content_as(new)) {
            return false;
        }
        let current = |d: &DestRt| {
            d.last_plan.as_ref().is_some_and(|p| {
                p.is_empty() && p.source_scan_complete && p.source_generation == prev_gen
            })
        };
        if !(0..2)
            .filter(|&i| self.eligible(i))
            .all(|i| current(&self.dests[i]))
        {
            return false;
        }
        let generation = self.latest_gen;
        for i in 0..2 {
            if self.eligible(i)
                && let Some(p) = &mut self.dests[i].last_plan
            {
                p.source_generation = generation;
            }
        }
        true
    }

    fn eligible(&self, i: usize) -> bool {
        let d = &self.dests[i];
        d.row.is_some()
            && matches!(d.connectivity, Connectivity::Online { .. })
            && !d.held
            && !d.permission_denied
    }

    fn dispatch(&mut self, job: Job) {
        let Some(scan) = self.latest_scan.clone() else {
            return;
        };
        let Some(source_root) = self.project.source_path.clone() else {
            return;
        };
        if job == Job::ScanOnly {
            return; // drives untouched; pending becomes "not up to date with the source"
        }
        let (execute, sig) = match job {
            Job::Sync(sig) => (true, sig),
            _ => (false, None),
        };
        // Sync with a reviewed preview: refuse to run anything the user did not see.
        let reviewed = sig.is_some_and(|s| self.preview.as_ref().is_some_and(|p| p.signature == s));
        self.build_preview = matches!(job, Job::Preview) || (execute && sig.is_some() && !reviewed);
        self.preview_stale = execute && sig.is_some() && !reviewed;
        self.awaiting.clear();
        for i in 0..2 {
            if !self.eligible(i) {
                continue;
            }
            let per_dest_sig = if execute && reviewed {
                Some(self.preview_sigs[i])
            } else {
                None
            };
            let d = &mut self.dests[i];
            d.outstanding += 1;
            d.progress = None;
            self.awaiting.insert(i);
            let _ = d.tx.send(WorkerCmd::Plan(Box::new(PlanJob {
                scan: scan.clone(),
                scan_gen: self.latest_gen,
                source_root: source_root.clone(),
                settings: self.project.settings.clone(),
                execute: execute && (sig.is_none() || reviewed),
                expect_signature: per_dest_sig,
            })));
        }
        if execute && sig.is_some() && !reviewed {
            self.notice_once(
                NoticeLevel::Warning,
                "The plan changed since you reviewed it. Nothing was copied; review the new preview.",
            );
        }
    }

    // -------------------------------------------------------------- workers

    fn on_worker(&mut self, i: usize, e: WorkerEvent) {
        match e {
            WorkerEvent::State(s) => {
                let d = &mut self.dests[i];
                d.state = s;
                if matches!(s, WorkerState::Idle | WorkerState::Paused) {
                    d.outstanding = d.outstanding.saturating_sub(1);
                    d.progress = None;
                    if d.outstanding == 0 {
                        self.after_idle();
                    }
                }
            }
            WorkerEvent::Progress(p) => self.dests[i].progress = Some(p),
            WorkerEvent::Free(f) => self.dests[i].free = Some(f),
            WorkerEvent::Connected {
                connectivity,
                free,
                notes,
                permission_denied,
            } => {
                tracing::info!(
                    drive = slot_of(i).as_str(),
                    online = matches!(connectivity, Connectivity::Online { .. }),
                    permission_denied,
                    "drive connectivity changed"
                );
                let came_online = matches!(connectivity, Connectivity::Online { .. })
                    && !matches!(self.dests[i].connectivity, Connectivity::Online { .. });
                {
                    let d = &mut self.dests[i];
                    d.connectivity = connectivity;
                    d.permission_denied = permission_denied;
                    if free.is_some() {
                        d.free = free;
                    }
                    d.last_error = permission_denied.then(|| {
                        "This drive is not writable by TwinSync (permission denied).".into()
                    });
                }
                for n in notes {
                    self.notice(NoticeLevel::Warning, n);
                }
                let (result, detail) = match &self.dests[i].connectivity {
                    Connectivity::Online { .. } => {
                        let r = self.dests[i].row.as_ref();
                        (
                            "ok",
                            format!(
                                "connected: {} ({}, {})",
                                r.and_then(|r| r.volume_name.clone()).unwrap_or_default(),
                                r.map_or("unknown", |r| r.media_type.as_str()),
                                r.and_then(|r| r.fs_type.clone()).unwrap_or_default(),
                            ),
                        )
                    }
                    Connectivity::Offline => ("ok", "disconnected".to_string()),
                    Connectivity::IdentityMismatch(m) => {
                        ("attention", format!("not the expected drive: {m}"))
                    }
                    Connectivity::NeedsConfirmation(m) => {
                        ("attention", format!("confirmation needed: {m}"))
                    }
                };
                self.record(Some(i), "drive", result, &detail);
                self.refresh_versions(i);
                if came_online {
                    let job = if self.settings().auto_sync {
                        Job::Sync(None)
                    } else {
                        Job::Refresh
                    };
                    self.request(job, Origin::Mount);
                }
            }
            WorkerEvent::PlanReady {
                plan,
                scan_gen,
                executed,
                stale_preview,
                summary,
            } => {
                if let Some(r) = &self.dests[i].row {
                    let _ = self.db.set_planned_generation(&r.id, scan_gen);
                }
                self.dests[i].attention = Arc::new(attention_of(&plan));
                self.dests[i].last_plan = Some(plan);
                self.dests[i].last_error = None;
                if let Some(s) = &summary {
                    tracing::info!(
                        drive = slot_of(i).as_str(),
                        copied = s.done,
                        unchanged = s.unchanged,
                        stale = s.stale,
                        failed = s.failed,
                        bytes = s.bytes,
                        trigger = self.dispatch_origin.label(),
                        "batch finished"
                    );
                    if let Some(why) = s.stopped_early.clone() {
                        self.dests[i].last_error = Some(why.clone());
                        self.notice(
                            NoticeLevel::Error,
                            format!("{}: {why}", slot_of(i).default_label()),
                        );
                    } else if s.failed > 0 {
                        self.dests[i].last_error =
                            Some(format!("{} item(s) failed; see Activity", s.failed));
                    }
                }
                if stale_preview {
                    self.build_preview = true;
                    self.preview_stale = true;
                    self.notice_once(
                        NoticeLevel::Warning,
                        "The plan changed since you reviewed it. Nothing was copied; review the new preview.",
                    );
                }
                self.awaiting.remove(&i);
                self.refresh_versions(i);
                if executed {
                    self.reload_activity();
                    self.reload_dest_row(i);
                }
                if self.awaiting.is_empty() && self.build_preview {
                    self.build_preview = false;
                    self.assemble_preview(self.preview_stale);
                }
            }
            WorkerEvent::Resolved(r) => {
                match r {
                    Ok(m) => self.notice(NoticeLevel::Info, m),
                    Err(m) => self.notice(NoticeLevel::Warning, m),
                }
                self.request(Job::Refresh, Origin::User);
            }
            WorkerEvent::VerifyDone(msg, ok) => {
                self.dests[i].verify_summary = Some(msg);
                self.reload_dest_row(i);
                self.reload_activity();
                if !ok {
                    self.dests[i].last_error =
                        Some("Verification found problems; see Activity".into());
                }
            }
            WorkerEvent::Failed(m) => {
                tracing::warn!(drive = slot_of(i).as_str(), error = %m, "drive operation failed");
                self.dests[i].last_error = Some(m.clone());
                self.awaiting.remove(&i);
                self.notice(
                    NoticeLevel::Error,
                    format!("{}: {m}", slot_of(i).default_label()),
                );
            }
            WorkerEvent::Stopped => {
                let d = &mut self.dests[i];
                d.cancel.store(false, Ordering::Relaxed);
                let newly_released = d.held && !d.released;
                if d.held {
                    d.released = true;
                    d.disconnect_note = Some(
                        "TwinSync has stopped using this drive and closed its files. This is not the same \
                         as ejecting: eject it in Finder, or use Eject here, before unplugging."
                            .into(),
                    );
                }
                if newly_released {
                    self.record(
                        Some(i),
                        "disconnect",
                        "ok",
                        "TwinSync stopped using this drive and closed its files (this is not an eject)",
                    );
                }
            }
        }
    }

    fn after_idle(&mut self) {
        if self.awaiting.is_empty() && self.build_preview {
            // Every drive answered (or none was eligible): show what we have.
            self.build_preview = false;
            self.assemble_preview(self.preview_stale);
        }
        self.try_start();
    }

    fn assemble_preview(&mut self, out_of_date: bool) {
        let mut rows = Vec::new();
        let mut per_dest = Vec::new();
        let mut sig = blake3::Hasher::new();
        for i in 0..2 {
            let Some(plan) = &self.dests[i].last_plan else {
                continue;
            };
            let s = plan_signature(plan);
            self.preview_sigs[i] = s;
            sig.update(&s.to_le_bytes());
            per_dest.push((slot_of(i), plan.summary()));
            for it in &plan.items {
                rows.push(PreviewRow {
                    slot: slot_of(i),
                    rel: it.rel.clone(),
                    action: it.action.label().to_string(),
                    size: it.size,
                    reason: it.reason.clone(),
                });
            }
        }
        let d = sig.finalize();
        self.preview = Some(Arc::new(PreviewView {
            signature: u64::from_le_bytes(d.as_bytes()[..8].try_into().expect("8 bytes")),
            rows,
            per_dest,
            source_scan_complete: self.latest_scan.as_ref().is_some_and(|s| s.complete),
            out_of_date,
        }));
    }

    // ------------------------------------------------------------- mounts

    fn refresh_mounts(&mut self) {
        let (r, tx) = (self.cfg.resolver.clone(), self.tx.clone());
        std::thread::spawn(move || {
            if let Ok(v) = r.mounted_volumes() {
                let _ = tx.send(Msg::Mounts(v));
            }
        });
    }

    fn on_mounts(&mut self, vols: Vec<VolumeInfo>) {
        self.volumes = vols;
        for i in 0..2 {
            let Some(row) = self.dests[i].row.clone() else {
                continue;
            };
            match find_volume(&self.volumes, &row) {
                Presence::Offline => {
                    let d = &mut self.dests[i];
                    if d.connectivity != Connectivity::Offline || d.volume.is_some() {
                        d.cancel.store(true, Ordering::Relaxed);
                        d.connectivity = Connectivity::Offline;
                        d.volume = None;
                        d.released = false;
                        d.held = false;
                        d.outstanding += 1;
                        let _ = d.tx.send(WorkerCmd::Disconnect);
                        self.record(Some(i), "drive", "ok", "disconnected (no longer mounted)");
                    }
                }
                Presence::Candidate(v) => {
                    let d = &mut self.dests[i];
                    if d.volume.as_ref() == Some(&v) {
                        continue;
                    }
                    d.volume = Some(v.clone());
                    let mut r = row;
                    r.last_mount_path = Some(v.mount_point.clone());
                    let _ = self.db.set_last_mount(&r.id, &v.mount_point);
                    d.row = Some(r.clone());
                    d.outstanding += 1;
                    d.cancel.store(false, Ordering::Relaxed);
                    let _ = d.tx.send(WorkerCmd::Connect {
                        volume: Box::new(v),
                        row: Box::new(r),
                        initialize: false,
                    });
                }
                Presence::Ambiguous(m) => {
                    let now = Connectivity::IdentityMismatch(m.clone());
                    if self.dests[i].connectivity != now {
                        self.record(
                            Some(i),
                            "drive",
                            "attention",
                            &format!("not the expected drive: {m}"),
                        );
                    }
                    self.dests[i].connectivity = now;
                    self.dests[i].volume = None;
                }
                Presence::NoIdentity(m) => {
                    let now = Connectivity::NeedsConfirmation(m.clone());
                    if self.dests[i].connectivity != now {
                        self.record(
                            Some(i),
                            "drive",
                            "attention",
                            &format!("confirmation needed: {m}"),
                        );
                    }
                    self.dests[i].connectivity = now;
                }
            }
        }
        self.update_shared_gate();
    }

    fn update_shared_gate(&mut self) {
        let phys = |d: &DestRt| {
            d.volume
                .as_ref()
                .and_then(|v| v.physical_disk.clone())
                .or_else(|| d.row.as_ref().and_then(|r| r.physical_id.clone()))
        };
        let shared = match (phys(&self.dests[0]), phys(&self.dests[1])) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        self.shared_gate.store(shared, Ordering::Relaxed);
    }

    // --------------------------------------------------------------- setup

    fn configure(&mut self, slot: Slot, path: PathBuf, label: String) {
        let Some(source) = self.project.source_path.clone() else {
            return self.notice(NoticeLevel::Error, "Choose the source folder first.");
        };
        let other = &self.dests[1 - idx(slot)];
        let other_info = match (&other.row, &other.volume) {
            (Some(r), Some(v)) => Some((v.mount_point.join(&r.root_rel_path), v.clone())),
            _ => None,
        };
        let (resolver, tx) = (self.cfg.resolver.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let result = check_destination_choice(
                resolver.as_ref(),
                &source,
                &path,
                other_info.as_ref().map(|(p, v)| (p.as_path(), v)),
            );
            let _ = tx.send(Msg::SetupChecked {
                slot,
                label,
                result,
            });
        });
    }

    fn on_setup_checked(&mut self, slot: Slot, label: String, result: Result<DestCheck>) {
        match result {
            Err(e) => {
                self.record(None, "setup", "rejected", &e.to_string());
                self.notice(NoticeLevel::Error, format!("Cannot use that folder: {e}"))
            }
            Ok(check) => {
                let blocked = check.has_error();
                let confirm = check.needs_confirmation();
                if blocked {
                    let why: Vec<String> = check
                        .findings
                        .iter()
                        .filter(|f| f.severity == crate::devices::Severity::Error)
                        .map(|f| f.message.clone())
                        .collect();
                    self.record(None, "setup", "rejected", &why.join("; "));
                }
                if blocked || confirm {
                    self.setup_report = Some(SetupReport {
                        slot,
                        path: check.canonical.clone(),
                        findings: check.findings.clone(),
                        awaiting_confirmation: !blocked,
                    });
                    if !blocked {
                        self.pending_setup[idx(slot)] = Some((check, label));
                    }
                } else {
                    self.setup_report = None;
                    self.apply_setup(slot, check, label);
                }
            }
        }
    }

    fn apply_setup(&mut self, slot: Slot, check: DestCheck, label: String) {
        let i = idx(slot);
        if let Some(old) = self.dests[i].row.take() {
            let _ = self.db.delete_destination(&old.id);
        }
        let v = &check.volume;
        let mut row = DestinationRow::new(&self.project.id, slot);
        row.label = if label.trim().is_empty() {
            slot.default_label()
        } else {
            label
        };
        row.volume_uuid = v.volume_uuid.clone();
        row.volume_name = Some(v.volume_name.clone());
        row.fs_type = Some(v.fs_type.clone());
        row.media_type = v.media;
        row.bus_protocol = v.bus_protocol.clone();
        row.physical_id = v.physical_disk.clone().or_else(|| v.media_name.clone());
        row.identity_strong = v.volume_uuid.is_some();
        row.root_rel_path = check.root_rel.clone();
        row.last_mount_path = Some(v.mount_point.clone());
        if let Err(e) = self.db.upsert_destination(&row) {
            return self.notice(NoticeLevel::Error, e.to_string());
        }
        let detail = format!(
            "{} set up: {} ({}, {})",
            row.label,
            row.volume_name.clone().unwrap_or_default(),
            row.media_type.as_str(),
            row.fs_type.clone().unwrap_or_default()
        );
        let d = &mut self.dests[i];
        d.row = Some(row.clone());
        d.volume = Some(check.volume.clone());
        d.last_plan = None;
        d.attention = Arc::new(Vec::new());
        d.held = false;
        d.released = false;
        d.last_error = None;
        d.outstanding += 1;
        d.cancel.store(false, Ordering::Relaxed);
        let _ = d.tx.send(WorkerCmd::Connect {
            volume: Box::new(check.volume),
            row: Box::new(row),
            initialize: true,
        });
        self.update_shared_gate();
        if check.shares_disk_with_other {
            self.shared_gate.store(true, Ordering::Relaxed);
        }
        self.preview = None;
        self.record(Some(i), "setup", "ok", &detail);
    }

    fn confirm_identity(&mut self, slot: Slot) {
        let i = idx(slot);
        let Some(row) = self.dests[i].row.clone() else {
            return;
        };
        let (resolver, tx) = (self.cfg.resolver.clone(), self.tx.clone());
        let target = self.dests[i]
            .volume
            .as_ref()
            .map(|v| v.mount_point.join(&row.root_rel_path))
            .or_else(|| {
                row.last_mount_path
                    .clone()
                    .map(|p| p.join(&row.root_rel_path))
            });
        let Some(target) = target else {
            return self.notice(NoticeLevel::Error, "No known location for this drive.");
        };
        std::thread::spawn(move || {
            let result = resolver.volume_for_path(&target);
            let _ = tx.send(Msg::IdentityResolved {
                slot,
                target,
                result,
            });
        });
    }

    fn on_identity_resolved(&mut self, slot: Slot, target: PathBuf, result: Result<VolumeInfo>) {
        let i = idx(slot);
        let Some(mut row) = self.dests[i].row.clone() else {
            return;
        };
        let v = match result {
            Ok(v) => v,
            Err(e) => {
                return self.notice(
                    NoticeLevel::Error,
                    format!("Cannot identify the drive: {e}"),
                );
            }
        };
        if !v.local_disk || !v.writable {
            return self.notice(
                NoticeLevel::Error,
                "That volume cannot be used as a destination.",
            );
        }
        row.volume_uuid = v.volume_uuid.clone();
        row.volume_name = Some(v.volume_name.clone());
        row.fs_type = Some(v.fs_type.clone());
        row.media_type = v.media;
        row.bus_protocol = v.bus_protocol.clone();
        row.physical_id = v.physical_disk.clone().or_else(|| v.media_name.clone());
        row.identity_strong = v.volume_uuid.is_some();
        row.last_mount_path = Some(v.mount_point.clone());
        // A legacy config stored the full folder path; keep that folder, relative to the volume.
        match target.strip_prefix(&v.mount_point) {
            Ok(rel) => row.root_rel_path = rel.to_path_buf(),
            Err(_) => {
                return self.notice(
                    NoticeLevel::Error,
                    "The folder is not inside the volume it resolves to.",
                );
            }
        }
        let _ = self.db.upsert_destination(&row);
        let d = &mut self.dests[i];
        d.row = Some(row.clone());
        d.volume = Some(v.clone());
        d.outstanding += 1;
        let _ = d.tx.send(WorkerCmd::Connect {
            volume: Box::new(v),
            row: Box::new(row),
            initialize: true,
        });
        self.update_shared_gate();
    }

    // ---------------------------------------------------------- disconnect

    fn prepare_disconnect(&mut self, slot: Slot) {
        let d = &mut self.dests[idx(slot)];
        d.held = true;
        d.released = false;
        d.disconnect_note = None;
        d.cancel.store(true, Ordering::Relaxed);
        // The worker answers `Stopped` once it has genuinely stopped: right away
        // if idle, otherwise after the current chunk. Until then the UI says Stopping.
        let _ = d.tx.send(WorkerCmd::Release);
        self.record(
            Some(idx(slot)),
            "disconnect",
            "ok",
            "Prepare to Disconnect requested",
        );
    }

    fn eject(&mut self, slot: Slot, confirm: bool) {
        let d = &self.dests[idx(slot)];
        let Some(vol) = d.volume.clone() else {
            return self.notice(NoticeLevel::Warning, "That drive is not connected.");
        };
        if !d.released {
            return self.notice(
                NoticeLevel::Warning,
                "Use Prepare to Disconnect first so TwinSync stops using the drive.",
            );
        }
        let (ej, resolver, tx) = (
            self.cfg.ejector.clone(),
            self.cfg.resolver.clone(),
            self.tx.clone(),
        );
        std::thread::spawn(move || {
            let others: Vec<String> = ej
                .siblings(&vol)
                .unwrap_or_default()
                .into_iter()
                .filter(|v| v.mount_point != vol.mount_point)
                .map(|v| v.mount_point.display().to_string())
                .collect();
            if !others.is_empty() && !confirm {
                let _ = tx.send(Msg::EjectNeedsConfirm { slot, also: others });
                return;
            }
            let result = match ej.eject(&vol) {
                Err(e) => Err(format!("Eject failed: {e}")),
                Ok(()) => {
                    // Success only if macOS agrees: the volume must have left the mount table.
                    let gone = (0..15).any(|_| {
                        let still = resolver
                            .mounted_volumes()
                            .map(|v| v.iter().any(|x| x.mount_point == vol.mount_point))
                            .unwrap_or(true);
                        if still {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        !still
                    });
                    if gone {
                        Ok("Ejected. It is safe to unplug the drive.".to_string())
                    } else {
                        Err("Eject was requested but the volume is still mounted; do not unplug it.".into())
                    }
                }
            };
            let _ = tx.send(Msg::EjectDone { slot, result });
        });
    }

    // --------------------------------------------------------- resolutions

    fn resolve(&mut self, slot: Slot, rel: RelPath, res: Resolution) {
        let i = idx(slot);
        let Some(row) = self.dests[i].row.clone() else {
            return;
        };
        if res == Resolution::AdoptIfIdentical {
            let (Some(src), d) = (self.project.source_path.clone(), &mut self.dests[i]) else {
                return;
            };
            d.outstanding += 1;
            let _ = d.tx.send(WorkerCmd::Resolve {
                rel,
                resolution: res,
                source_root: src,
            });
            return;
        }
        let current = self
            .latest_scan
            .as_ref()
            .and_then(|s| s.entries.iter().find(|e| e.rel == rel))
            .cloned();
        let existing = self.db.dest_entry(&row.id, &rel).ok().flatten();
        let r = match res {
            Resolution::RestoreFromSource => self.db.delete_dest_entry(&row.id, &rel),
            Resolution::Acknowledge => match existing {
                Some(mut e) => {
                    e.status = EntryStatus::Orphaned;
                    self.db.upsert_dest_entry(&row.id, &e)
                }
                None => Ok(()),
            },
            Resolution::ReplaceWithSource => {
                let mut e = existing.unwrap_or_else(|| blank_entry(&rel));
                e.status = EntryStatus::ReplaceApproved;
                self.db.upsert_dest_entry(&row.id, &e)
            }
            Resolution::Ignore | Resolution::StopManaging => {
                let mut e = existing.unwrap_or_else(|| blank_entry(&rel));
                e.status = EntryStatus::Ignored;
                if let Some(s) = &current {
                    e.src_fp = Some(s.fp);
                    e.synced_hash = s.hash.or(e.synced_hash);
                }
                self.db.upsert_dest_entry(&row.id, &e)
            }
            Resolution::AdoptIfIdentical => Ok(()),
        };
        match r {
            Ok(()) => {
                let _ = self.db.log_event(
                    Some(&row.id),
                    "resolve",
                    Some(&rel),
                    "ok",
                    res.label(),
                    false,
                );
                self.reload_activity();
                self.request(Job::Refresh, Origin::User);
            }
            Err(e) => self.notice(NoticeLevel::Error, e.to_string()),
        }
    }

    // ------------------------------------------------------------ plumbing

    fn reload_activity(&mut self) {
        if let Ok(a) = self.db.activity(&self.activity_filter) {
            self.activity = Arc::new(a);
        }
    }

    fn reload_dest_row(&mut self, i: usize) {
        if let Some(r) = &self.dests[i].row
            && let Ok(Some(fresh)) = self.db.destination(&r.id)
        {
            self.dests[i].row = Some(fresh);
        }
    }

    fn refresh_versions(&mut self, i: usize) {
        let Some(r) = &self.dests[i].row else { return };
        let (id, n) = (
            r.id.clone(),
            self.db.versions(&r.id).map(|v| v.len()).unwrap_or(0),
        );
        self.dests[i].version_count = n;
        self.dests[i].version_bytes = self.db.version_bytes(&id).unwrap_or(0);
    }

    fn is_up_to_date(&self, d: &DestRt) -> bool {
        d.last_plan.as_ref().is_some_and(|p| {
            p.is_empty()
                && p.source_generation == self.latest_gen
                && p.source_scan_complete
                && self.source_unavailable.is_none()
                && self.dirty_last.is_none()
                && matches!(d.connectivity, Connectivity::Online { .. })
        })
    }

    fn publish(&mut self) {
        self.changed = false;
        let paused = self.paused.load(Ordering::Relaxed);
        let now = now_ms();
        let mut views = Vec::new();
        for (i, d) in self.dests.iter().enumerate() {
            let slot = slot_of(i);
            let mut v = DestView::empty(slot);
            if let Some(r) = &d.row {
                v.configured = true;
                v.id = Some(r.id.clone());
                v.label = r.label.clone();
                v.volume_name = r.volume_name.clone();
                v.media = r.media_type;
                v.fs_type = r.fs_type.clone();
                v.profile = r.profile;
                v.last_sync_at = r.last_sync_at;
                v.last_verify_at = r.last_verify_at;
                v.free_checked_at = r.free_checked_at;
                v.free_bytes = d.free.or(r.free_bytes);
            }
            v.connectivity = d.connectivity.clone();
            let stalled = d.busy()
                && matches!(
                    d.state,
                    WorkerState::Syncing | WorkerState::Scanning | WorkerState::Verifying
                )
                && now.saturating_sub(d.last_progress.load(Ordering::Relaxed))
                    > self.cfg.stall_after.as_millis() as u64;
            let stopping = d.held && !d.released && d.busy();
            v.state = if stopping {
                WorkerState::Stopping
            } else if stalled {
                WorkerState::WaitingForDrive
            } else if paused && !d.busy() {
                WorkerState::Paused
            } else {
                d.state
            };
            v.progress = d.progress.clone();
            v.pending = d.last_plan.as_ref().map(|p| PendingView {
                up_to_date_with_source: p.source_generation == self.latest_gen
                    && self.dirty_last.is_none(),
                summary: p.summary(),
            });
            v.attention = d.attention.clone();
            v.version_count = d.version_count;
            v.version_bytes = d.version_bytes;
            v.released = d.released;
            v.disconnect_note = d.disconnect_note.clone();
            v.last_error = d.last_error.clone();
            v.last_verify_summary = d.verify_summary.clone();
            v.status = derive_status(&StatusInputs {
                configured: d.row.is_some(),
                connectivity: d.connectivity.clone(),
                worker: v.state,
                source_unavailable: self.source_unavailable.is_some(),
                permission_denied: d.permission_denied,
                has_error: d.last_error.is_some(),
                attention_items: d.attention.len(),
                verified_up_to_date: self.is_up_to_date(d),
            });
            views.push(v);
        }
        if self.shared_gate.load(Ordering::Relaxed) {
            for v in views.iter_mut().filter(|v| v.configured) {
                v.shares_disk_note = Some(
                    "Both destinations are on the same physical disk: copies take turns and this is not two separate devices."
                        .into(),
                );
            }
        }
        let configured: Vec<&DestView> = views.iter().filter(|v| v.configured).collect();
        let all_up =
            !configured.is_empty() && configured.iter().all(|v| v.status == DestStatus::UpToDate);
        let source = SourceView {
            path: self.project.source_path.clone(),
            state: match (&self.project.source_path, &self.source_unavailable) {
                (None, _) => SourceState::NotConfigured,
                (Some(_), Some(e)) => SourceState::Unavailable(e.clone()),
                (Some(_), None) if self.latest_scan.is_some() => SourceState::Ok,
                _ => SourceState::Unknown,
            },
            last_scan_at: self.project.last_scan_at,
            last_scan_complete: self.latest_scan.as_ref().is_some_and(|s| s.complete),
            unreadable: self
                .latest_scan
                .as_ref()
                .map(|s| s.unreadable.iter().map(|r| r.display()).collect())
                .unwrap_or_default(),
        };
        self.rev += 1;
        let snap = UiSnapshot {
            rev: self.rev,
            source,
            destinations: views,
            settings: self.project.settings.clone(),
            auto_sync: self.project.settings.auto_sync,
            paused,
            scanning: self.scan_running,
            scans_completed: self.scans_completed,
            preview: self.preview.clone(),
            activity: self.activity.clone(),
            notices: self.notices.clone(),
            setup: self.setup_report.clone(),
            eject_prompt: self.eject_prompt.clone(),
            all_up_to_date: all_up,
        };
        *self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(snap);
        (self.cfg.on_change)();
    }
}

fn blank_entry(rel: &RelPath) -> DestEntry {
    DestEntry {
        rel: rel.clone(),
        kind: EntryKind::File,
        status: EntryStatus::Synced,
        synced_hash: None,
        dest_fp: None,
        src_fp: None,
        synced_at: None,
        verified_at: None,
    }
}

fn attention_of(plan: &Plan) -> Vec<AttentionItem> {
    plan.items
        .iter()
        .filter(|i| i.action.needs_attention())
        .map(|i| AttentionItem {
            rel: i.rel.clone(),
            action: i.action.label().to_string(),
            reason: i.reason.clone(),
            resolutions: match &i.action {
                Action::Collision(_) => vec![
                    Resolution::AdoptIfIdentical,
                    Resolution::ReplaceWithSource,
                    Resolution::Ignore,
                ],
                Action::Conflict => vec![Resolution::ReplaceWithSource, Resolution::Ignore],
                Action::ExternallyDeleted => {
                    vec![Resolution::RestoreFromSource, Resolution::StopManaging]
                }
                Action::SourceDeleted => vec![Resolution::Acknowledge],
                _ => vec![],
            },
        })
        .collect()
}
