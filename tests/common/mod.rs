#![allow(dead_code)]
//! Shared harness: a real source folder and one or more real "drive" folders in a
//! temp dir, a real SQLite database, and fault injection. No real drives are touched.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;
use twinsync::config::default_exclusions;
use twinsync::domain::*;
use twinsync::errors::Result;
use twinsync::planner::*;
use twinsync::scanner::*;
use twinsync::storage::*;
use twinsync::transfer::fsops::DirFd;
use twinsync::transfer::*;

pub struct Dest {
    pub dir: PathBuf,
    pub id: Id,
}

pub struct Fixture {
    pub tmp: TempDir,
    pub src_dir: PathBuf,
    pub dests: Vec<Dest>,
    pub db: Db,
    pub project_id: Id,
    pub caps: FsCapabilities,
    pub cancel: Arc<AtomicBool>,
    pub fault: RefCell<Option<FaultHook>>,
    pub chunk: usize,
    pub reserve: u64,
}

pub fn rel(s: &str) -> RelPath {
    RelPath::from_bytes(s.as_bytes()).unwrap()
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_destinations(1)
    }

    pub fn with_destinations(n: usize) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("source");
        fs::create_dir(&src_dir).unwrap();
        let db = Db::open(&tmp.path().join("state/twinsync.sqlite3")).unwrap();
        let project = db.create_project("TwinSync", Some(&src_dir)).unwrap();
        let mut dests = Vec::new();
        for (i, slot) in [Slot::A, Slot::B].into_iter().take(n).enumerate() {
            let dir = tmp.path().join(format!("drive{i}"));
            fs::create_dir(&dir).unwrap();
            let row = DestinationRow::new(&project.id, slot);
            db.upsert_destination(&row).unwrap();
            dests.push(Dest { dir, id: row.id });
        }
        Fixture {
            tmp,
            src_dir,
            dests,
            db,
            project_id: project.id,
            caps: FsCapabilities::for_fs_type("apfs"),
            cancel: Arc::new(AtomicBool::new(false)),
            fault: RefCell::new(None),
            chunk: 64 * 1024,
            reserve: 0,
        }
    }

    /// A fixture whose single destination (and database) live at the given,
    /// persistent locations. Used by the real-filesystem compatibility tests.
    pub fn on_volume(dest_dir: PathBuf, state_dir: PathBuf, src_dir: PathBuf) -> Self {
        for d in [&dest_dir, &state_dir, &src_dir] {
            fs::create_dir_all(d).unwrap();
        }
        let db = Db::open(&state_dir.join("twinsync.sqlite3")).unwrap();
        let project = match db.first_project().unwrap() {
            Some(p) => p,
            None => db.create_project("TwinSync", Some(&src_dir)).unwrap(),
        };
        let row = match db.destinations(&project.id).unwrap().into_iter().next() {
            Some(r) => r,
            None => {
                let r = DestinationRow::new(&project.id, Slot::A);
                db.upsert_destination(&r).unwrap();
                r
            }
        };
        Fixture {
            tmp: tempfile::tempdir().unwrap(),
            src_dir,
            dests: vec![Dest {
                dir: dest_dir,
                id: row.id,
            }],
            db,
            project_id: project.id,
            caps: FsCapabilities::for_fs_type("apfs"),
            cancel: Arc::new(AtomicBool::new(false)),
            fault: RefCell::new(None),
            chunk: 64 * 1024,
            reserve: 0,
        }
    }

    // ------------------------------------------------------------- files

    pub fn write_src(&self, path: &str, content: &[u8]) {
        let p = self.src_dir.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    pub fn read_dst(&self, d: usize, path: &str) -> Option<Vec<u8>> {
        fs::read(self.dests[d].dir.join(path)).ok()
    }

    pub fn dst_path(&self, d: usize, path: &str) -> PathBuf {
        self.dests[d].dir.join(path)
    }

    pub fn tmp_dir(&self, d: usize) -> PathBuf {
        self.dests[d]
            .dir
            .join(".twinsync")
            .join(&self.project_id)
            .join("tmp")
    }

    pub fn versions_dir(&self, d: usize) -> PathBuf {
        self.dests[d]
            .dir
            .join(".twinsync")
            .join(&self.project_id)
            .join("versions")
    }

    pub fn tmp_files(&self, d: usize) -> Vec<String> {
        match fs::read_dir(self.tmp_dir(d)) {
            Ok(rd) => rd
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => vec![],
        }
    }

    /// All files below the versions dir as (relative path, content).
    pub fn version_files(&self, d: usize) -> Vec<(String, Vec<u8>)> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    out.push((
                        p.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                        fs::read(&p).unwrap(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        let base = self.versions_dir(d);
        walk(&base, &base, &mut out);
        out
    }

    // ------------------------------------------------------------ faults

    pub fn set_fault(&self, hook: impl Fn(FaultPoint) -> Option<Fault> + Send + Sync + 'static) {
        *self.fault.borrow_mut() = Some(Arc::new(hook));
    }

    pub fn crash_at(&self, point: FaultPoint) {
        self.set_fault(move |p| (p == point).then_some(Fault::Crash));
    }

    pub fn clear_fault(&self) {
        *self.fault.borrow_mut() = None;
    }

    // ------------------------------------------------------------ engine-ish

    pub fn try_with<R>(&self, d: usize, f: impl FnOnce(&TransferCtx) -> R) -> Result<R> {
        let src = DirFd::open_path(&self.src_dir)
            .map_err(|e| twinsync::errors::Error::io("open source", e))?;
        let root = DirFd::open_path(&self.dests[d].dir)
            .map_err(|e| twinsync::errors::Error::io("open destination", e))?;
        let marker = Marker {
            app: "TwinSync".into(),
            project_id: self.project_id.clone(),
            destination_id: self.dests[d].id.clone(),
            volume_uuid: None,
            created_at: 0,
        };
        let dirs = ensure_layout(&root, &marker)?;
        let fault = self.fault.borrow().clone();
        let ctx = TransferCtx {
            db: &self.db,
            project_id: &self.project_id,
            dest_id: &self.dests[d].id,
            source: &src,
            root: &root,
            dirs: &dirs,
            caps: &self.caps,
            media: DriveMediaType::Unknown,
            chunk: self.chunk,
            reserve_bytes: self.reserve,
            cancel: &self.cancel,
            fault: fault.as_ref(),
            on_bytes: &|_| {},
        };
        Ok(f(&ctx))
    }

    pub fn with<R>(&self, d: usize, f: impl FnOnce(&TransferCtx) -> R) -> R {
        self.try_with(d, f).expect("destination layout")
    }

    pub fn scan(&self) -> ScanResult {
        let ex = Exclusions::new(&default_exclusions()).unwrap();
        match scan_source(&self.src_dir, &ex, false, &AtomicBool::new(false)).unwrap() {
            ScanOutcome::Done(r) => r,
            ScanOutcome::SourceUnavailable(e) => panic!("source unavailable: {e}"),
        }
    }

    pub fn plan(&self, d: usize) -> Plan {
        let scan = self.scan();
        let manifest = self.db.dest_entries(&self.dests[d].id).unwrap();
        let rels: Vec<&RelPath> = scan
            .entries
            .iter()
            .map(|e| &e.rel)
            .chain(manifest.keys())
            .collect();
        let root = DirFd::open_path(&self.dests[d].dir).unwrap();
        let observed = observe_paths(&root, &rels);
        plan(
            &PlanInput {
                source: &scan,
                manifest: &manifest,
                observed: &observed,
                caps: &self.caps,
                free_bytes: None,
                reserve_bytes: 0,
            },
            1,
        )
    }

    pub fn action(&self, d: usize, path: &str) -> Option<Action> {
        self.plan(d)
            .items
            .into_iter()
            .find(|i| i.rel == rel(path))
            .map(|i| i.action)
    }

    /// Plan then execute every transfer item once.
    pub fn sync(&self, d: usize) -> Vec<(RelPath, Result<ItemOutcome>)> {
        let plan = self.plan(d);
        self.run(d, &plan)
    }

    pub fn run(&self, d: usize, plan: &Plan) -> Vec<(RelPath, Result<ItemOutcome>)> {
        self.with(d, |ctx| {
            plan.items
                .iter()
                .filter(|i| i.action.is_transfer())
                .map(|i| (i.rel.clone(), execute_item(ctx, i)))
                .collect()
        })
    }

    pub fn sync_ok(&self, d: usize) {
        for (r, res) in self.sync(d) {
            res.unwrap_or_else(|e| panic!("{r}: {e}"));
        }
    }

    pub fn entries(&self, d: usize) -> std::collections::HashMap<RelPath, DestEntry> {
        self.db.dest_entries(&self.dests[d].id).unwrap()
    }

    pub fn recover(&self, d: usize) -> twinsync::transfer::recovery::RecoveryResult {
        self.with(d, |ctx| twinsync::transfer::recovery::recover(ctx).unwrap())
    }

    /// States of every journaled operation for destination `d`, oldest first.
    pub fn op_states(&self, d: usize) -> Vec<String> {
        let conn =
            rusqlite::Connection::open(self.tmp.path().join("state/twinsync.sqlite3")).unwrap();
        let mut st = conn
            .prepare("SELECT state FROM sync_operations WHERE destination_id = ?1 ORDER BY created_at, rowid")
            .unwrap();
        st.query_map([&self.dests[d].id], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    pub fn op_ids(&self, d: usize) -> Vec<String> {
        let conn =
            rusqlite::Connection::open(self.tmp.path().join("state/twinsync.sqlite3")).unwrap();
        let mut st = conn
            .prepare("SELECT id FROM sync_operations WHERE destination_id = ?1 ORDER BY created_at, rowid")
            .unwrap();
        st.query_map([&self.dests[d].id], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    pub fn unfinished(&self, d: usize) -> usize {
        self.db.unfinished_ops(&self.dests[d].id).unwrap().len()
    }
}

// -------------------------------------------------------------- devices

pub fn vol(
    mount: &Path,
    uuid: &str,
    media: DriveMediaType,
    physical: Option<&str>,
) -> twinsync::devices::VolumeInfo {
    use std::os::unix::fs::MetadataExt;
    twinsync::devices::VolumeInfo {
        mount_point: mount.to_path_buf(),
        bsd_name: "disk9s1".into(),
        volume_uuid: Some(uuid.to_string()),
        volume_name: mount.file_name().unwrap().to_string_lossy().into_owned(),
        fs_type: "exfat".into(),
        media,
        bus_protocol: Some("USB".into()),
        internal: false,
        writable: true,
        ejectable: true,
        local_disk: true,
        physical_disk: physical.map(str::to_string),
        media_name: None,
        total_bytes: 1 << 40,
        dev: fs::metadata(mount).map_or(0, |m| m.dev()),
    }
}
