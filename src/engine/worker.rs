//! One worker thread per destination. It is the only thing that ever touches
//! that drive, one command at a time, so two writers can never meet on one
//! target. A slow or stalled drive stalls only its own worker.

use super::snapshot::{ProgressView, Resolution};
use super::{BatchSummary, Msg, WorkerEvent};
use crate::config::Settings;
use crate::devices::{VolumeInfo, detect_caps, probe_writable};
use crate::domain::{Connectivity, EntryKind, FsCapabilities, RelPath, WorkerState};
use crate::errors::Error;
use crate::planner::{Action, Plan, PlanInput, PlannedItem, plan, plan_signature};
use crate::scanner::ScanResult;
use crate::storage::{Db, DestinationRow, EntryStatus, now_secs};
use crate::transfer::fsops::DirFd;
use crate::transfer::recovery::{RecoveryOutcome, recover};
use crate::transfer::{
    FaultHook, ItemOutcome, Marker, TransferCtx, check_destination, ensure_layout, execute_item,
    hash_reader, observe_paths, read_marker,
};
use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) struct PlanJob {
    pub scan: Arc<ScanResult>,
    pub scan_gen: i64,
    pub source_root: PathBuf,
    pub settings: Settings,
    pub execute: bool,
    pub expect_signature: Option<u64>,
}

pub(crate) enum WorkerCmd {
    Connect {
        volume: Box<VolumeInfo>,
        row: Box<DestinationRow>,
        initialize: bool,
    },
    Disconnect,
    Plan(Box<PlanJob>),
    Resolve {
        rel: RelPath,
        resolution: Resolution,
        source_root: PathBuf,
    },
    FullVerify,
    /// Drop everything and confirm nothing of ours is open on the drive.
    Release,
    Shutdown,
}

pub(crate) struct WorkerEnv {
    pub idx: usize,
    pub db_path: PathBuf,
    pub tx: Sender<Msg>,
    pub cancel: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub last_progress: Arc<AtomicU64>,
    pub fault: Option<FaultHook>,
    /// Both destinations share one physical disk: take turns instead of competing.
    pub gate: Arc<Mutex<()>>,
    pub shared_gate: Arc<AtomicBool>,
    pub retry_base: Duration,
}

#[derive(Clone)]
struct Conn {
    row: DestinationRow,
    volume: VolumeInfo,
    root_path: PathBuf,
    caps: FsCapabilities,
}

const MAX_IO_RETRIES: u32 = 5;
const MAX_SOURCE_RETRIES: u32 = 3;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn spawn(env: WorkerEnv, rx: Receiver<WorkerCmd>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("twinsync-drive-{}", env.idx))
        .spawn(move || {
            let Ok(db) = Db::open(&env.db_path) else {
                let _ = env.tx.send(Msg::Worker(
                    env.idx,
                    WorkerEvent::Failed("could not open the database".into()),
                ));
                return;
            };
            let mut w = Worker {
                env,
                db,
                conn: None,
            };
            while let Ok(cmd) = rx.recv() {
                if matches!(cmd, WorkerCmd::Shutdown) {
                    break;
                }
                w.handle(cmd);
            }
        })
        .expect("spawn worker thread")
}

struct Worker {
    env: WorkerEnv,
    db: Db,
    conn: Option<Conn>,
}

#[derive(PartialEq)]
enum Stop {
    Cancel,
    Pause,
}

impl Worker {
    fn ev(&self, e: WorkerEvent) {
        let _ = self.env.tx.send(Msg::Worker(self.env.idx, e));
    }

    fn state(&self, s: WorkerState) {
        self.touch();
        self.ev(WorkerEvent::State(s));
    }

    fn touch(&self) {
        self.env.last_progress.store(now_ms(), Ordering::Relaxed);
    }

    fn stop_requested(&self) -> Option<Stop> {
        if self.env.cancel.load(Ordering::Relaxed) {
            Some(Stop::Cancel)
        } else if self.env.paused.load(Ordering::Relaxed) {
            Some(Stop::Pause)
        } else {
            None
        }
    }

    /// Sleep that ends early on cancel or pause.
    fn sleep(&self, d: Duration) {
        let until = Instant::now() + d;
        while Instant::now() < until && self.stop_requested().is_none() {
            std::thread::sleep(Duration::from_millis(5).min(d));
        }
    }

    fn handle(&mut self, cmd: WorkerCmd) {
        match cmd {
            WorkerCmd::Connect {
                volume,
                row,
                initialize,
            } => self.connect(*volume, *row, initialize),
            WorkerCmd::Disconnect => {
                self.conn = None;
                self.state(WorkerState::Idle);
            }
            WorkerCmd::Plan(job) => self.run_job(*job),
            WorkerCmd::Resolve {
                rel,
                resolution,
                source_root,
            } => self.resolve(rel, resolution, source_root),
            WorkerCmd::FullVerify => self.full_verify(),
            WorkerCmd::Release => {
                // Nothing is held between commands; every descriptor is scoped to one command.
                self.env.cancel.store(false, Ordering::Relaxed);
                self.ev(WorkerEvent::Stopped);
            }
            WorkerCmd::Shutdown => {}
        }
    }

    // ------------------------------------------------------------ connect

    fn connected(&self, c: Connectivity) {
        self.ev(WorkerEvent::Connected {
            connectivity: c,
            free: None,
            notes: Vec::new(),
            permission_denied: false,
        });
        self.state(WorkerState::Idle);
    }

    fn connect(&mut self, volume: VolumeInfo, row: DestinationRow, initialize: bool) {
        self.env.cancel.store(false, Ordering::Relaxed);
        self.conn = None;
        self.state(WorkerState::Scanning);
        let root_path = volume.mount_point.join(&row.root_rel_path);
        // Never create the root: a missing folder or a vanished drive must stay missing.
        let root = match DirFd::open_path(&root_path) {
            Ok(r) => r,
            Err(e) => {
                let msg = if e.raw_os_error() == Some(libc::ENOENT) {
                    "the destination folder is missing on this drive; TwinSync will not create it"
                        .to_string()
                } else {
                    format!("cannot open the destination folder: {e}")
                };
                return self.connected(Connectivity::IdentityMismatch(msg));
            }
        };
        match root.stat_self() {
            Ok(st) if st.dev() == volume.dev => {}
            _ => {
                return self.connected(Connectivity::IdentityMismatch(
                    "the volume at this path is not the one that was identified".into(),
                ));
            }
        }
        match read_marker(&root, &row.project_id) {
            Ok(Some(m)) if m.destination_id == row.id => {}
            Ok(Some(_)) => {
                return self.connected(Connectivity::IdentityMismatch(
                    "this drive's TwinSync marker belongs to a different destination".into(),
                ));
            }
            Ok(None) if initialize => {}
            Ok(None) => {
                return self.connected(Connectivity::NeedsConfirmation(
                    "the volume matches but carries no TwinSync marker; confirm this is the right drive"
                        .into(),
                ));
            }
            Err(e) => return self.connected(Connectivity::IdentityMismatch(e.to_string())),
        }

        let mut permission_denied = false;
        if let Err(e) = probe_writable(&root) {
            permission_denied = true;
            let _ = e;
        }
        let caps = detect_caps(&root, &volume);
        let free = root.free_bytes().ok();
        let mut notes = Vec::new();
        if !permission_denied {
            let marker = Marker {
                app: "TwinSync".into(),
                project_id: row.project_id.clone(),
                destination_id: row.id.clone(),
                volume_uuid: volume.volume_uuid.clone(),
                created_at: now_secs(),
            };
            match ensure_layout(&root, &marker) {
                Ok(dirs) => {
                    // The journal is reconciled with the drive before anything new is written.
                    let source_placeholder = root.try_clone();
                    if let Ok(src) = source_placeholder {
                        let ctx = TransferCtx {
                            db: &self.db,
                            project_id: &row.project_id,
                            dest_id: &row.id,
                            source: &src, // recovery never reads the source
                            root: &root,
                            dirs: &dirs,
                            caps: &caps,
                            media: volume.media,
                            chunk: 1 << 20,
                            reserve_bytes: 0,
                            cancel: &self.env.cancel,
                            fault: self.env.fault.as_ref(),
                            on_bytes: &|_| {},
                        };
                        match recover(&ctx) {
                            Ok(res) => {
                                for r in res.ops {
                                    if let RecoveryOutcome::NeedsAttention(m) = r.outcome {
                                        notes.push(format!("{}: {m}", r.rel.display()));
                                    }
                                }
                            }
                            Err(e) => notes.push(format!("recovery could not finish: {e}")),
                        }
                    }
                }
                Err(e) => notes.push(format!("could not prepare the drive: {e}")),
            }
        }
        let mount = volume.mount_point.clone();
        self.conn = Some(Conn {
            row,
            volume,
            root_path,
            caps: caps.clone(),
        });
        self.ev(WorkerEvent::Connected {
            connectivity: Connectivity::Online { mount },
            free,
            notes,
            permission_denied,
        });
        self.state(WorkerState::Idle);
    }

    // --------------------------------------------------------------- jobs

    fn open_root(&mut self, conn: &Conn) -> Option<DirFd> {
        let root = match DirFd::open_path(&conn.root_path) {
            Ok(r) => r,
            Err(e) => {
                self.ev(WorkerEvent::Failed(format!("cannot open the drive: {e}")));
                self.state(WorkerState::Idle);
                return None;
            }
        };
        // Identity is re-checked before every batch, not only at connect time.
        match root.stat_self() {
            Ok(st) if st.dev() == conn.volume.dev => Some(root),
            _ => {
                self.conn = None;
                self.connected(Connectivity::IdentityMismatch(
                    "the volume at this path changed; nothing was written".into(),
                ));
                None
            }
        }
    }

    fn make_plan(&self, conn: &Conn, root: &DirFd, job: &PlanJob) -> Result<Plan, Error> {
        let manifest = self.db.dest_entries(&conn.row.id)?;
        let rels: Vec<&RelPath> = job.scan.entries.iter().map(|e| &e.rel).collect();
        let observed = observe_paths(root, &rels);
        Ok(plan(
            &PlanInput {
                source: &job.scan,
                manifest: &manifest,
                observed: &observed,
                caps: &conn.caps,
                free_bytes: root.free_bytes().ok(),
                reserve_bytes: job.settings.free_space_reserve_mb << 20,
            },
            job.scan_gen,
        ))
    }

    fn run_job(&mut self, job: PlanJob) {
        let Some(conn) = self.conn.clone() else {
            self.ev(WorkerEvent::Failed("the drive is not connected".into()));
            return;
        };
        self.state(WorkerState::Scanning);
        let Some(root) = self.open_root(&conn) else {
            return;
        };
        let plan0 = match self.make_plan(&conn, &root, &job) {
            Ok(p) => p,
            Err(e) => {
                self.ev(WorkerEvent::Failed(e.to_string()));
                self.state(WorkerState::Idle);
                return;
            }
        };
        if let Ok(free) = root.free_bytes() {
            let _ = self.db.set_free_space(&conn.row.id, free);
            self.ev(WorkerEvent::Free(free));
        }
        let stale_preview = job
            .expect_signature
            .is_some_and(|s| s != plan_signature(&plan0));
        if !job.execute || stale_preview {
            self.ev(WorkerEvent::PlanReady {
                plan: plan0,
                scan_gen: job.scan_gen,
                executed: false,
                stale_preview,
                summary: None,
            });
            self.state(WorkerState::Idle);
            return;
        }
        self.execute(conn, root, job, plan0);
    }

    fn execute(&mut self, conn: Conn, root: DirFd, job: PlanJob, mut plan0: Plan) {
        // Two destinations on one physical disk take turns.
        let gate = self.env.gate.clone();
        let _turn = self
            .env
            .shared_gate
            .load(Ordering::Relaxed)
            .then(|| gate.lock().unwrap_or_else(|p| p.into_inner()));
        let Ok(source) = DirFd::open_path(&job.source_root) else {
            self.ev(WorkerEvent::Failed(
                "the source folder cannot be opened".into(),
            ));
            self.state(WorkerState::Idle);
            return;
        };
        let marker = Marker {
            app: "TwinSync".into(),
            project_id: conn.row.project_id.clone(),
            destination_id: conn.row.id.clone(),
            volume_uuid: conn.volume.volume_uuid.clone(),
            created_at: now_secs(),
        };
        let dirs = match ensure_layout(&root, &marker) {
            Ok(d) => d,
            Err(e) => {
                self.ev(WorkerEvent::Failed(e.to_string()));
                self.state(WorkerState::Idle);
                return;
            }
        };

        let bytes_cell = Cell::new(0u64);
        let last_emit = Cell::new(Instant::now());
        let started = Instant::now();
        let files_done = Cell::new(0u64);
        let current: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let emit = |force: bool, files_total: u64, bytes_total: u64| {
            if !force && last_emit.get().elapsed() < Duration::from_millis(100) {
                return;
            }
            last_emit.set(Instant::now());
            let secs = started.elapsed().as_secs_f64();
            self.ev(WorkerEvent::Progress(ProgressView {
                current: current.borrow().clone(),
                files_done: files_done.get(),
                files_total,
                bytes_done: bytes_cell.get(),
                bytes_total,
                bytes_per_sec: (secs >= 2.0).then(|| bytes_cell.get() as f64 / secs),
            }));
        };
        let totals = |p: &Plan| -> (u64, u64) {
            (
                p.items.iter().filter(|i| i.action.is_transfer()).count() as u64,
                p.items
                    .iter()
                    .filter(|i| i.action.is_transfer())
                    .map(|i| i.size)
                    .sum(),
            )
        };
        let tick = |n: u64| {
            bytes_cell.set(bytes_cell.get() + n);
            self.touch();
        };
        let chunk = conn.row.profile.chunk_size(conn.volume.media);
        let ctx = TransferCtx {
            db: &self.db,
            project_id: &conn.row.project_id,
            dest_id: &conn.row.id,
            source: &source,
            root: &root,
            dirs: &dirs,
            caps: &conn.caps,
            media: conn.volume.media,
            chunk,
            reserve_bytes: job.settings.free_space_reserve_mb << 20,
            cancel: &self.env.cancel,
            fault: self.env.fault.as_ref(),
            on_bytes: &tick,
        };

        let mut summary = BatchSummary::default();

        // 1. Hash destination files whose fingerprint moved, then plan again.
        let checks: Vec<RelPath> = plan0
            .items
            .iter()
            .filter(|i| i.action == Action::CheckDestination)
            .map(|i| i.rel.clone())
            .collect();
        if !checks.is_empty() {
            self.state(WorkerState::Verifying);
            for rel in &checks {
                if self.stop_requested().is_some() {
                    break;
                }
                *current.borrow_mut() = Some(rel.display());
                self.touch();
                if let Err(e) = check_destination(&ctx, rel) {
                    summary.failed += 1;
                    let _ = self.db.log_event(
                        Some(&conn.row.id),
                        "check",
                        Some(rel),
                        "failed",
                        &e.to_string(),
                        false,
                    );
                }
            }
            match self.make_plan(&conn, &root, &job) {
                Ok(p) => plan0 = p,
                Err(e) => {
                    self.ev(WorkerEvent::Failed(e.to_string()));
                    self.state(WorkerState::Idle);
                    return;
                }
            }
        }

        // 2. Transfers.
        let (files_total, bytes_total) = totals(&plan0);
        self.state(WorkerState::Syncing);
        emit(true, files_total, bytes_total);
        let mut queue: VecDeque<(PlannedItem, u32)> = plan0
            .items
            .iter()
            .filter(|i| i.action.is_transfer())
            .cloned()
            .map(|i| (i, 0))
            .collect();
        let mut stopped: Option<Stop> = None;
        while let Some((item, attempts)) = queue.pop_front() {
            if let Some(s) = self.stop_requested() {
                stopped = Some(s);
                break;
            }
            *current.borrow_mut() = Some(item.rel.display());
            self.touch();
            match execute_item(&ctx, &item) {
                Ok(ItemOutcome::Done { .. }) => {
                    summary.done += 1;
                    files_done.set(files_done.get() + 1);
                }
                Ok(ItemOutcome::Unchanged) => {
                    summary.unchanged += 1;
                    files_done.set(files_done.get() + 1);
                }
                Ok(ItemOutcome::Stale(why)) => {
                    summary.stale += 1;
                    let _ = self.db.log_event(
                        Some(&conn.row.id),
                        "skip",
                        Some(&item.rel),
                        "stale",
                        &why,
                        true,
                    );
                }
                Err(Error::Cancelled) => {
                    stopped = Some(Stop::Cancel);
                    break;
                }
                Err(e) if e.is_no_space() => {
                    summary.failed += 1;
                    summary.stopped_early = Some("not enough free space on the drive".into());
                    let _ = self.db.log_event(
                        Some(&conn.row.id),
                        "transfer",
                        Some(&item.rel),
                        "failed",
                        &e.to_string(),
                        true,
                    );
                    break;
                }
                Err(e) => {
                    let limit = if matches!(e, Error::SourceChanged) {
                        MAX_SOURCE_RETRIES
                    } else {
                        MAX_IO_RETRIES
                    };
                    if e.is_transient() && attempts < limit {
                        // Bounded exponential backoff; the item goes to the back of the line.
                        self.state(WorkerState::WaitingForDrive);
                        self.sleep(self.env.retry_base * 2u32.saturating_pow(attempts));
                        self.state(WorkerState::Syncing);
                        queue.push_back((item, attempts + 1));
                        continue;
                    }
                    summary.failed += 1;
                    let _ = self.db.log_event(
                        Some(&conn.row.id),
                        "transfer",
                        Some(&item.rel),
                        "failed",
                        &e.to_string(),
                        e.is_transient(),
                    );
                }
            }
            emit(false, files_total, bytes_total);
        }
        summary.bytes = bytes_cell.get();
        emit(true, files_total, bytes_total);

        // 3. Plan again so "pending" reflects the drive as it is now.
        let after = self.make_plan(&conn, &root, &job);
        if let Ok(free) = root.free_bytes() {
            let _ = self.db.set_free_space(&conn.row.id, free);
            self.ev(WorkerEvent::Free(free));
        }
        let _ = self.db.log_event(
            Some(&conn.row.id),
            "sync",
            None,
            if summary.failed == 0 && stopped.is_none() {
                "ok"
            } else {
                "partial"
            },
            &format!(
                "{} copied, {} unchanged, {} skipped as stale, {} failed",
                summary.done, summary.unchanged, summary.stale, summary.failed
            ),
            false,
        );
        if let Ok(p) = after {
            self.ev(WorkerEvent::PlanReady {
                plan: p,
                scan_gen: job.scan_gen,
                executed: true,
                stale_preview: false,
                summary: Some(summary),
            });
        }
        match stopped {
            Some(Stop::Cancel) => {
                self.env.cancel.store(false, Ordering::Relaxed);
                self.state(WorkerState::Idle);
                self.ev(WorkerEvent::Stopped);
            }
            Some(Stop::Pause) => self.state(WorkerState::Paused),
            None => self.state(WorkerState::Idle),
        }
    }

    // ---------------------------------------------------------- resolutions

    fn resolve(&mut self, rel: RelPath, resolution: Resolution, source_root: PathBuf) {
        let Some(conn) = self.conn.clone() else {
            self.ev(WorkerEvent::Resolved(Err(
                "the drive is not connected".into()
            )));
            return self.state(WorkerState::Idle);
        };
        if resolution != Resolution::AdoptIfIdentical {
            return self.state(WorkerState::Idle);
        }
        let (Some(root), Ok(source)) = (self.open_root(&conn), DirFd::open_path(&source_root))
        else {
            return self.ev(WorkerEvent::Resolved(Err(
                "could not open the folders".into()
            )));
        };
        let marker = Marker {
            app: "TwinSync".into(),
            project_id: conn.row.project_id.clone(),
            destination_id: conn.row.id.clone(),
            volume_uuid: conn.volume.volume_uuid.clone(),
            created_at: now_secs(),
        };
        let Ok(dirs) = ensure_layout(&root, &marker) else {
            self.ev(WorkerEvent::Resolved(Err(
                "could not prepare the drive".into()
            )));
            return self.state(WorkerState::Idle);
        };
        let ctx = TransferCtx {
            db: &self.db,
            project_id: &conn.row.project_id,
            dest_id: &conn.row.id,
            source: &source,
            root: &root,
            dirs: &dirs,
            caps: &conn.caps,
            media: conn.volume.media,
            chunk: conn.row.profile.chunk_size(conn.volume.media),
            reserve_bytes: 0,
            cancel: &self.env.cancel,
            fault: None,
            on_bytes: &|_| self.touch(),
        };
        self.state(WorkerState::Verifying);
        let r = match crate::transfer::adopt_if_identical(&ctx, &rel) {
            Ok(true) => Ok(format!("{} is now managed", rel.display())),
            Ok(false) => Err(format!(
                "{} differs from the source; left untouched",
                rel.display()
            )),
            Err(e) => Err(e.to_string()),
        };
        self.state(WorkerState::Idle);
        self.ev(WorkerEvent::Resolved(r));
    }

    // ---------------------------------------------------------- full verify

    fn full_verify(&mut self) {
        let Some(conn) = self.conn.clone() else {
            self.ev(WorkerEvent::VerifyDone(
                "the drive is not connected".into(),
                false,
            ));
            return self.state(WorkerState::Idle);
        };
        let Some(root) = self.open_root(&conn) else {
            return;
        };
        let entries = match self.db.dest_entries(&conn.row.id) {
            Ok(e) => e,
            Err(e) => {
                self.ev(WorkerEvent::VerifyDone(e.to_string(), false));
                return self.state(WorkerState::Idle);
            }
        };
        let mut todo: Vec<_> = entries
            .values()
            .filter(|e| e.kind == EntryKind::File && e.status == EntryStatus::Synced)
            .collect();
        todo.sort_by(|a, b| a.rel.components().cmp(b.rel.components()));
        self.state(WorkerState::Verifying);
        let (mut ok, mut bad, mut missing, mut errors) = (Vec::new(), 0u64, 0u64, 0u64);
        let mut stopped = false;
        let chunk = conn.row.profile.chunk_size(conn.volume.media);
        for (i, e) in todo.iter().enumerate() {
            if self.stop_requested().is_some() {
                stopped = true;
                break;
            }
            self.touch();
            self.ev(WorkerEvent::Progress(ProgressView {
                current: Some(e.rel.display()),
                files_done: i as u64,
                files_total: todo.len() as u64,
                ..Default::default()
            }));
            let comps: Vec<&[u8]> = e.rel.components().collect();
            let (name, parent) = comps.split_last().expect("non-empty path");
            let Ok(dir) = root.walk(parent, false) else {
                missing += 1;
                continue;
            };
            match dir.stat(name) {
                Ok(None) => missing += 1,
                Ok(Some(st)) if st.is_file() => match dir.open_read(name) {
                    Ok(mut f) => match hash_reader(&mut f, chunk, &self.env.cancel) {
                        Ok((h, _)) if Some(h) == e.synced_hash => ok.push(e.rel.clone()),
                        Ok(_) => {
                            bad += 1;
                            let mut c = (*e).clone();
                            c.status = EntryStatus::Conflict;
                            c.dest_fp = Some(st.fingerprint());
                            let _ = self.db.upsert_dest_entry(&conn.row.id, &c);
                            let _ = self.db.log_event(
                                Some(&conn.row.id),
                                "verify",
                                Some(&e.rel),
                                "mismatch",
                                "content on the drive differs from what was copied",
                                false,
                            );
                        }
                        Err(Error::Cancelled) => {
                            stopped = true;
                            break;
                        }
                        Err(_) => errors += 1,
                    },
                    Err(_) => errors += 1,
                },
                Ok(Some(_)) => bad += 1,
                Err(_) => errors += 1,
            }
        }
        let _ = self.db.mark_verified(&conn.row.id, &ok);
        let complete = !stopped && errors == 0;
        if complete {
            let _ = self.db.touch_verified(&conn.row.id);
        }
        let msg = format!(
            "{} file(s) verified by content hash, {} mismatch, {} missing{}{}",
            ok.len(),
            bad,
            missing,
            if errors > 0 {
                format!(", {errors} unreadable")
            } else {
                String::new()
            },
            if stopped {
                " (stopped early; not recorded as a full verification)"
            } else {
                ""
            },
        );
        let _ = self.db.log_event(
            Some(&conn.row.id),
            "verify",
            None,
            if bad == 0 && errors == 0 && !stopped {
                "ok"
            } else {
                "attention"
            },
            &msg,
            false,
        );
        if stopped {
            self.env.cancel.store(false, Ordering::Relaxed);
            self.ev(WorkerEvent::Stopped);
        }
        self.state(WorkerState::Idle);
        self.ev(WorkerEvent::VerifyDone(msg, complete && bad == 0));
    }
}
