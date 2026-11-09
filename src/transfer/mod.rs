//! Safe transfer pipeline: stream to a unique temp file on the destination
//! volume, verify by reading it back, re-check source and destination, keep the
//! previous version, then commit with a rename. The journal plus the recovery
//! pass make every crash window resolvable from the filesystem's real state.

pub mod fsops;
pub mod recovery;

use crate::domain::{DriveMediaType, EntryKind, FsCapabilities, Hash, Id, RelPath, new_id};
use crate::errors::{Error, IoContext, Result};
use crate::planner::{Action, Observed, PlannedItem};
use crate::storage::{
    Db, DestEntry, EntryStatus, OpKind, OpRow, OpState, OpStep, VersionRow, now_secs,
};
use fsops::{DirFd, Stat, fstat, is_not_found, is_path_escape, no_cache, sync_fd, utc_stamp};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

pub const MARKER_FILE: &str = "marker.json";

// ------------------------------------------------------------ fault injection

/// Points where tests can interrupt the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPoint {
    BeforeJournal,
    WriteChunk(u64),
    AfterTempSynced,
    AfterVerifyRead,
    BeforePreserve,
    AfterPreserve,
    BeforeCommitRename,
    AfterCommitRename,
    BeforeDbCommit,
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Simulated power loss / process kill: stop dead, no cleanup, journal untouched.
    Crash,
    /// A normal I/O failure with this errno; ordinary error handling applies.
    Io(i32),
}

pub type FaultHook = Arc<dyn Fn(FaultPoint) -> Option<Fault> + Send + Sync>;

// ------------------------------------------------------------------- layout

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub app: String,
    pub project_id: Id,
    pub destination_id: Id,
    pub volume_uuid: Option<String>,
    pub created_at: i64,
}

pub struct ProjectDirs {
    pub project: DirFd,
    pub tmp: DirFd,
    pub versions: DirFd,
}

pub(crate) fn walk_err(e: io::Error, what: &str) -> Error {
    if is_path_escape(&e) {
        Error::PathSafety(format!("{what}: a symlink or non-folder is in the way"))
    } else {
        Error::io(what, e)
    }
}

/// Create `.twinsync/<project>/{tmp,versions,marker.json}` under the destination root.
/// The root itself is never created: a vanished drive must not leave a folder behind.
pub fn ensure_layout(root: &DirFd, marker: &Marker) -> Result<ProjectDirs> {
    let comps: [&[u8]; 2] = [
        crate::domain::RESERVED_DIR.as_bytes(),
        marker.project_id.as_bytes(),
    ];
    let project = root
        .walk(&comps, true)
        .map_err(|e| walk_err(e, "prepare .twinsync"))?;
    let tmp = project
        .walk(&[b"tmp"], true)
        .map_err(|e| walk_err(e, "prepare tmp"))?;
    let versions = project
        .walk(&[b"versions"], true)
        .map_err(|e| walk_err(e, "prepare versions"))?;
    match project.create_new(MARKER_FILE.as_bytes()) {
        Ok(mut f) => {
            let json =
                serde_json::to_vec_pretty(marker).map_err(|e| Error::Config(e.to_string()))?;
            f.write_all(&json).ctx("write marker")?;
            sync_fd(f.as_raw_fd()).ctx("sync marker")?;
        }
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
        Err(e) => return Err(Error::io("create marker", e)),
    }
    Ok(ProjectDirs {
        project,
        tmp,
        versions,
    })
}

/// `Ok(None)` when no marker exists. A marker that cannot be parsed is an
/// identity problem, not "absent".
pub fn read_marker(root: &DirFd, project_id: &str) -> Result<Option<Marker>> {
    let comps: [&[u8]; 2] = [
        crate::domain::RESERVED_DIR.as_bytes(),
        project_id.as_bytes(),
    ];
    let dir = match root.walk(&comps, false) {
        Ok(d) => d,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(walk_err(e, "read marker")),
    };
    let mut f = match dir.open_read(MARKER_FILE.as_bytes()) {
        Ok(f) => f,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(Error::io("open marker", e)),
    };
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ctx("read marker")?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| Error::Identity(format!("marker file is unreadable: {e}")))
}

// -------------------------------------------------------------------- context

pub struct TransferCtx<'a> {
    pub db: &'a Db,
    pub project_id: &'a str,
    pub dest_id: &'a str,
    pub source: &'a DirFd,
    pub root: &'a DirFd,
    pub dirs: &'a ProjectDirs,
    pub caps: &'a FsCapabilities,
    pub media: DriveMediaType,
    pub chunk: usize,
    pub reserve_bytes: u64,
    pub cancel: &'a AtomicBool,
    pub fault: Option<&'a FaultHook>,
    /// Called after every chunk; the engine uses it for progress and stall detection.
    pub on_bytes: &'a dyn Fn(u64),
}

impl TransferCtx<'_> {
    fn hit(&self, p: FaultPoint) -> Result<()> {
        if let Some(hook) = self.fault
            && let Some(f) = hook(p)
        {
            return Err(match f {
                Fault::Crash => Error::Fault(format!("{p:?}")),
                Fault::Io(errno) => Error::io(
                    format!("injected fault at {p:?}"),
                    io::Error::from_raw_os_error(errno),
                ),
            });
        }
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ItemOutcome {
    Done {
        bytes: u64,
    },
    /// Content already matched; only bookkeeping was refreshed.
    Unchanged,
    /// A precondition from the preview no longer holds; nothing was written.
    Stale(String),
}

// ------------------------------------------------------------------- helpers

pub fn hash_reader(r: &mut impl Read, chunk: usize, cancel: &AtomicBool) -> Result<(Hash, u64)> {
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; chunk.max(4096)];
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                h.update(&buf[..n]);
                total += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::io("read while hashing", e)),
        }
    }
    Ok((Hash(*h.finalize().as_bytes()), total))
}

pub(crate) fn split_rel(rel: &RelPath) -> (&[u8], Vec<&[u8]>) {
    let mut comps: Vec<&[u8]> = rel.components().collect();
    let name = comps.pop().unwrap_or(&[]);
    (name, comps)
}

fn same_stat_strict(a: &Stat, b: &Stat) -> bool {
    a.size() == b.size()
        && a.mtime_ns() == b.mtime_ns()
        && a.ctime_ns() == b.ctime_ns()
        && a.0.st_ino == b.0.st_ino
}

fn dest_matches(expected: &Observed, now: &Option<Stat>, caps: &FsCapabilities) -> bool {
    match (expected, now) {
        (Observed::Missing, None) => true,
        (Observed::File(fp), Some(st)) => {
            st.is_file() && st.fingerprint().same_as(fp, caps.inode_stable)
        }
        (Observed::Dir, Some(st)) => st.is_dir(),
        _ => false,
    }
}

/// What is at each destination path right now. Opens a directory once per
/// parent (items arrive grouped by directory). Missing parents mean Missing.
pub fn observe_paths(root: &DirFd, rels: &[&RelPath]) -> HashMap<RelPath, Observed> {
    struct Parent {
        comps: Vec<Vec<u8>>,
        dir: Option<DirFd>,
        err: Option<String>,
    }
    let mut out = HashMap::new();
    let mut cache: Option<Parent> = None;
    for rel in rels {
        let (name, parent) = split_rel(rel);
        let comps: Vec<Vec<u8>> = parent.iter().map(|c| c.to_vec()).collect();
        if cache.as_ref().is_none_or(|p| p.comps != comps) {
            let (dir, err) = match root.walk(&parent, false) {
                Ok(d) => (Some(d), None),
                Err(e) if is_not_found(&e) => (None, None),
                Err(e) => (None, Some(e.to_string())),
            };
            cache = Some(Parent { comps, dir, err });
        }
        let p = cache.as_ref().expect("cache filled above");
        let observed = match (&p.dir, &p.err) {
            (_, Some(e)) => Observed::Unreadable(e.clone()),
            (None, None) => Observed::Missing,
            (Some(d), None) => match d.stat(name) {
                Ok(None) => Observed::Missing,
                Ok(Some(st)) if st.is_file() => Observed::File(st.fingerprint()),
                Ok(Some(st)) if st.is_dir() => Observed::Dir,
                Ok(Some(_)) => Observed::Other,
                Err(e) => Observed::Unreadable(e.to_string()),
            },
        };
        out.insert((*rel).clone(), observed);
    }
    out
}

fn mtime_of(st: &Stat) -> Option<std::time::SystemTime> {
    let ns = st.mtime_ns();
    (ns >= 0).then(|| UNIX_EPOCH + Duration::from_nanos(ns as u64))
}

// ------------------------------------------------------------------ execution

pub fn execute_item(ctx: &TransferCtx, item: &PlannedItem) -> Result<ItemOutcome> {
    match item.action {
        Action::MakeDir => make_dir(ctx, item),
        Action::Copy | Action::Update => transfer_file(ctx, item),
        _ => Err(Error::Unsupported(format!(
            "{} is not a transfer action",
            item.action.label()
        ))),
    }
}

fn make_dir(ctx: &TransferCtx, item: &PlannedItem) -> Result<ItemOutcome> {
    let (name, parent) = split_rel(&item.rel);
    let dst_parent = ctx
        .root
        .walk(&parent, true)
        .map_err(|e| walk_err(e, "create parent folders"))?;
    match dst_parent.mkdir(name) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            let st = dst_parent.stat(name).ctx("check folder")?;
            if !st.is_some_and(|s| s.is_dir()) {
                return Ok(ItemOutcome::Stale(
                    "a file appeared where a folder should go".into(),
                ));
            }
        }
        Err(e) => return Err(Error::io("create folder", e)),
    }
    dst_parent.sync().ctx("sync parent folder")?;
    let now = now_secs();
    ctx.db.upsert_dest_entry(
        ctx.dest_id,
        &DestEntry {
            rel: item.rel.clone(),
            kind: EntryKind::Dir,
            status: EntryStatus::Synced,
            synced_hash: None,
            dest_fp: None,
            src_fp: None,
            synced_at: Some(now),
            verified_at: None,
        },
    )?;
    Ok(ItemOutcome::Done { bytes: 0 })
}

struct Phase {
    preserved: bool,
    committed: bool,
}

fn transfer_file(ctx: &TransferCtx, item: &PlannedItem) -> Result<ItemOutcome> {
    let rel = &item.rel;
    let (name, parent) = split_rel(rel);
    let is_update = item.action == Action::Update;

    // --- source: opened through a no-follow directory walk, never by path.
    let src_parent = ctx
        .source
        .walk(&parent, false)
        .map_err(|e| walk_err(e, "open source folder"))?;
    let mut src = src_parent.open_read(name).ctx("open source file")?;
    let st0 = fstat(&src).ctx("stat source file")?;
    if !st0.is_file() {
        return Ok(ItemOutcome::Stale(
            "source is no longer a regular file".into(),
        ));
    }
    if let Some(expected) = &item.expected_src
        && !st0.fingerprint().same_as(expected, true)
    {
        return Ok(ItemOutcome::Stale(
            "source changed since the preview".into(),
        ));
    }

    // --- destination: parent folders (created for new files), current state.
    let dst_parent = match ctx.root.walk(&parent, !is_update) {
        Ok(d) => d,
        Err(e) if is_not_found(&e) => {
            return Ok(ItemOutcome::Stale("destination folder disappeared".into()));
        }
        Err(e) => return Err(walk_err(e, "open destination folder")),
    };
    let dest_now = dst_parent.stat(name).ctx("check destination")?;
    if !dest_matches(&item.expected_dest, &dest_now, ctx.caps) {
        return Ok(ItemOutcome::Stale(
            "destination changed since the preview; nothing was overwritten".into(),
        ));
    }

    let entry = ctx.db.dest_entry(ctx.dest_id, rel)?;

    // --- cheap content check: metadata moved but bytes may not have.
    if is_update
        && let Some(e) = &entry
        && e.status == EntryStatus::Synced
        && let Some(synced) = e.synced_hash
    {
        let (h, _) = hash_reader(&mut src, ctx.chunk, ctx.cancel)?;
        if h == synced {
            let mut refreshed = e.clone();
            refreshed.src_fp = Some(st0.fingerprint());
            ctx.db.upsert_dest_entry(ctx.dest_id, &refreshed)?;
            return Ok(ItemOutcome::Unchanged);
        }
        src.seek(SeekFrom::Start(0)).ctx("rewind source")?;
    }

    // --- space, measured now rather than trusting an earlier reading.
    let need = st0.size();
    let free = dst_parent.free_bytes().ctx("read free space")?;
    if free < need.saturating_add(ctx.reserve_bytes) {
        return Err(Error::NoSpace {
            need: need.saturating_add(ctx.reserve_bytes),
            have: free,
        });
    }

    // --- journal
    ctx.hit(FaultPoint::BeforeJournal)?;
    let op_id = new_id();
    let temp_name = format!("{op_id}.part");
    let stamp_dir = format!("{}_{}", utc_stamp(now_secs()), &op_id[..8]);
    let version_rel = if is_update {
        let mut v = stamp_dir.clone().into_bytes();
        v.push(b'/');
        v.extend_from_slice(rel.as_bytes());
        Some(RelPath::from_bytes(v)?)
    } else {
        None
    };
    ctx.db.insert_op(&OpRow {
        id: op_id.clone(),
        destination_id: ctx.dest_id.to_string(),
        kind: if is_update {
            OpKind::Update
        } else {
            OpKind::Create
        },
        rel: rel.clone(),
        state: OpState::Running,
        step: OpStep::Started,
        temp_name: temp_name.clone(),
        version_rel: version_rel.clone(),
        expected_old_hash: entry.as_ref().and_then(|e| e.synced_hash),
        new_hash: None,
        src_fp: Some(st0.fingerprint()),
        bytes_total: need,
        bytes_done: 0,
        attempts: 0,
        error: None,
    })?;

    let mut state = Phase {
        preserved: false,
        committed: false,
    };
    let result = run_pipeline(
        ctx,
        item,
        &mut src,
        &src_parent,
        &st0,
        &dst_parent,
        &dest_now,
        entry.as_ref(),
        &op_id,
        &temp_name,
        &stamp_dir,
        version_rel.as_ref(),
        &mut state,
    );
    match result {
        Ok(bytes) => Ok(ItemOutcome::Done { bytes }),
        // Simulated crash: leave everything exactly as it is.
        Err(e @ Error::Fault(_)) => Err(e),
        Err(e) => {
            settle_failure(
                ctx,
                item,
                &dst_parent,
                &op_id,
                &temp_name,
                version_rel.as_ref(),
                &state,
                &e,
            );
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    ctx: &TransferCtx,
    item: &PlannedItem,
    src: &mut File,
    src_parent: &DirFd,
    st0: &Stat,
    dst_parent: &DirFd,
    dest_before: &Option<Stat>,
    entry: Option<&DestEntry>,
    op_id: &str,
    temp_name: &str,
    stamp_dir: &str,
    version_rel: Option<&RelPath>,
    state: &mut Phase,
) -> Result<u64> {
    let rel = &item.rel;
    let (name, parent) = split_rel(rel);

    // 5-6. stream copy with hashing during the source read
    let mut out = ctx
        .dirs
        .tmp
        .create_new(temp_name.as_bytes())
        .ctx("create temp file")?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; ctx.chunk];
    let (mut done, mut idx) = (0u64, 0u64);
    loop {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::io("read source", e)),
        };
        ctx.hit(FaultPoint::WriteChunk(idx))?;
        idx += 1;
        out.write_all(&buf[..n]).ctx("write temp file")?;
        hasher.update(&buf[..n]);
        done += n as u64;
        (ctx.on_bytes)(n as u64);
    }
    if done != st0.size() {
        return Err(Error::SourceChanged);
    }
    let new_hash = Hash(*hasher.finalize().as_bytes());
    // Best effort: keep the source's modification time and permission bits.
    if let Some(t) = mtime_of(st0) {
        let _ = out.set_modified(t);
    }
    unsafe {
        libc::fchmod(out.as_raw_fd(), st0.perm() as libc::mode_t);
    }
    // 10. flush file contents (and the drive's own cache where supported)
    sync_fd(out.as_raw_fd()).ctx("sync temp file")?;
    drop(out);
    ctx.hit(FaultPoint::AfterTempSynced)?;

    // 7. read the temp file back and compare
    {
        let mut rb = ctx
            .dirs
            .tmp
            .open_read(temp_name.as_bytes())
            .ctx("reopen temp file")?;
        no_cache(&rb);
        let (h, len) = hash_reader(&mut rb, ctx.chunk, ctx.cancel)?;
        if h != new_hash || len != done {
            return Err(Error::VerifyFailed(format!(
                "{} did not read back identical to the source",
                rel.display()
            )));
        }
    }
    ctx.hit(FaultPoint::AfterVerifyRead)?;

    // 8. source must not have changed while we copied
    let st1 = fstat(src).ctx("re-check source")?;
    let st1_path = src_parent.stat(name).ctx("re-check source path")?;
    if !same_stat_strict(st0, &st1) || !st1_path.is_some_and(|p| same_stat_strict(st0, &p)) {
        return Err(Error::SourceChanged);
    }
    // 9. destination must not have been touched by anyone else
    let dest_now = dst_parent.stat(name).ctx("re-check destination")?;
    let untouched = match (dest_before, &dest_now) {
        (None, None) => true,
        (Some(a), Some(b)) => same_stat_strict(a, b),
        _ => false,
    };
    if !untouched {
        return Err(Error::DestChanged(rel.display()));
    }

    // The hash is journaled before anything destructive so recovery can recognize it.
    ctx.db
        .set_op_step(op_id, OpStep::TempReady, Some(new_hash))?;

    // 11. keep the previous version; if that is impossible, do not replace
    let mut version_size = None;
    if version_rel.is_some() {
        let mut vparent: Vec<&[u8]> = vec![stamp_dir.as_bytes()];
        vparent.extend(parent.iter().copied());
        let vdir = ctx
            .dirs
            .versions
            .walk(&vparent, true)
            .map_err(|e| walk_err(e, "prepare version folder"))?;
        version_size = dest_now.as_ref().map(Stat::size);
        ctx.hit(FaultPoint::BeforePreserve)?;
        dst_parent
            .rename_to(name, &vdir, name)
            .ctx("keep previous version")?;
        state.preserved = true;
        ctx.hit(FaultPoint::AfterPreserve)?;
        ctx.db.set_op_step(op_id, OpStep::Preserved, None)?;
    }

    // 12. commit by rename; never replaces a file that appeared meanwhile
    ctx.hit(FaultPoint::BeforeCommitRename)?;
    match ctx
        .dirs
        .tmp
        .rename_excl(temp_name.as_bytes(), dst_parent, name)
    {
        Ok(()) => {}
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) => {
            // Filesystem without RENAME_EXCL (FAT family): small check-then-rename window.
            if dst_parent
                .stat(name)
                .ctx("check target before rename")?
                .is_some()
            {
                return Err(Error::DestChanged(rel.display()));
            }
            ctx.dirs
                .tmp
                .rename_to(temp_name.as_bytes(), dst_parent, name)
                .ctx("commit rename")?;
        }
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            return Err(Error::DestChanged(rel.display()));
        }
        Err(e) => return Err(Error::io("commit rename", e)),
    }
    state.committed = true;
    ctx.hit(FaultPoint::AfterCommitRename)?;
    dst_parent.sync().ctx("sync destination folder")?;
    ctx.db.set_op_step(op_id, OpStep::Committed, None)?;

    // 13. database after the filesystem commit
    let dest_final = dst_parent
        .stat(name)
        .ctx("stat committed file")?
        .ok_or_else(|| Error::DestChanged(rel.display()))?;
    let now = now_secs();
    let new_entry = DestEntry {
        rel: rel.clone(),
        kind: EntryKind::File,
        status: EntryStatus::Synced,
        synced_hash: Some(new_hash),
        dest_fp: Some(dest_final.fingerprint()),
        src_fp: Some(st0.fingerprint()),
        synced_at: Some(now),
        verified_at: None,
    };
    let version_row = version_rel.map(|v| VersionRow {
        rel: rel.clone(),
        version_rel: v.clone(),
        size: version_size.unwrap_or(0),
        hash: entry
            .filter(|e| e.status == EntryStatus::Synced)
            .and_then(|e| e.synced_hash),
        op_id: Some(op_id.to_string()),
        created_at: now,
    });
    ctx.hit(FaultPoint::BeforeDbCommit)?;
    ctx.db.commit_op(
        op_id,
        ctx.dest_id,
        &new_entry,
        version_row.as_ref(),
        if version_rel.is_some() {
            "updated"
        } else {
            "copied"
        },
    )?;
    Ok(done)
}

/// After a failed pipeline: put things back as they were where possible and
/// say honestly what state the operation is in. If cleanup itself fails (drive
/// gone), the operation stays `Running` so recovery finishes the job later.
#[allow(clippy::too_many_arguments)]
fn settle_failure(
    ctx: &TransferCtx,
    item: &PlannedItem,
    dst_parent: &DirFd,
    op_id: &str,
    temp_name: &str,
    version_rel: Option<&RelPath>,
    state: &Phase,
    err: &Error,
) {
    let (name, _) = split_rel(&item.rel);
    if state.committed {
        // Only the database update failed; recovery will finalize it.
        return;
    }
    if state.preserved
        && let Some(v) = version_rel
    {
        // Put the previous version back before anything else.
        let mut vparent: Vec<&[u8]> = Vec::new();
        let vcomps: Vec<&[u8]> = v.components().collect();
        vparent.extend(&vcomps[..vcomps.len() - 1]);
        let restored = ctx
            .dirs
            .versions
            .walk(&vparent, false)
            .and_then(|vdir| vdir.rename_excl(name, dst_parent, name));
        if restored.is_err() {
            return; // stays Running; recovery decides
        }
    }
    let cleaned = match ctx.hit(FaultPoint::Cleanup) {
        Err(_) => false,
        Ok(()) => match ctx.dirs.tmp.unlink(temp_name.as_bytes()) {
            Ok(()) => true,
            Err(e) => is_not_found(&e),
        },
    };
    if cleaned {
        let final_state = match err {
            Error::Cancelled | Error::DestChanged(_) | Error::SourceChanged => OpState::Aborted,
            _ => OpState::Failed,
        };
        let _ = ctx.db.finish_op(op_id, final_state, Some(&err.to_string()));
    }
}

// --------------------------------------------------- destination-side checks

#[derive(Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Content still matches what TwinSync copied; fingerprint refreshed.
    Unchanged,
    /// Content differs: recorded as a conflict, nothing modified.
    Modified,
    Missing,
}

/// Hash a managed destination file to tell a harmless touch from an edit.
pub fn check_destination(ctx: &TransferCtx, rel: &RelPath) -> Result<CheckOutcome> {
    let (name, parent) = split_rel(rel);
    let dir = match ctx.root.walk(&parent, false) {
        Ok(d) => d,
        Err(e) if is_not_found(&e) => return Ok(CheckOutcome::Missing),
        Err(e) => return Err(walk_err(e, "open destination folder")),
    };
    let Some(st) = dir.stat(name).ctx("stat destination file")? else {
        return Ok(CheckOutcome::Missing);
    };
    let Some(mut entry) = ctx.db.dest_entry(ctx.dest_id, rel)? else {
        return Err(Error::Unsupported("path is not managed".into()));
    };
    if !st.is_file() {
        entry.status = EntryStatus::Conflict;
        entry.dest_fp = None;
        ctx.db.upsert_dest_entry(ctx.dest_id, &entry)?;
        return Ok(CheckOutcome::Modified);
    }
    let mut f = dir.open_read(name).ctx("open destination file")?;
    let (h, _) = hash_reader(&mut f, ctx.chunk, ctx.cancel)?;
    entry.dest_fp = Some(st.fingerprint());
    if Some(h) == entry.synced_hash {
        entry.status = EntryStatus::Synced;
        entry.verified_at = Some(now_secs());
        ctx.db.upsert_dest_entry(ctx.dest_id, &entry)?;
        Ok(CheckOutcome::Unchanged)
    } else {
        entry.status = EntryStatus::Conflict;
        ctx.db.upsert_dest_entry(ctx.dest_id, &entry)?;
        Ok(CheckOutcome::Modified)
    }
}

/// For a collision: if the existing destination file is byte-identical to the
/// source, start managing it. Never modifies either file. Returns whether it adopted.
pub fn adopt_if_identical(ctx: &TransferCtx, rel: &RelPath) -> Result<bool> {
    let (name, parent) = split_rel(rel);
    let src_dir = ctx
        .source
        .walk(&parent, false)
        .map_err(|e| walk_err(e, "open source folder"))?;
    let mut s = src_dir.open_read(name).ctx("open source file")?;
    let s_st = fstat(&s).ctx("stat source")?;
    let dst_dir = ctx
        .root
        .walk(&parent, false)
        .map_err(|e| walk_err(e, "open destination folder"))?;
    let Some(d_st) = dst_dir.stat(name).ctx("stat destination")? else {
        return Ok(false);
    };
    if !d_st.is_file() || !s_st.is_file() || d_st.size() != s_st.size() {
        return Ok(false);
    }
    let mut d = dst_dir.open_read(name).ctx("open destination file")?;
    let (hs, _) = hash_reader(&mut s, ctx.chunk, ctx.cancel)?;
    let (hd, _) = hash_reader(&mut d, ctx.chunk, ctx.cancel)?;
    if hs != hd {
        return Ok(false);
    }
    let now = now_secs();
    ctx.db.upsert_dest_entry(
        ctx.dest_id,
        &DestEntry {
            rel: rel.clone(),
            kind: EntryKind::File,
            status: EntryStatus::Synced,
            synced_hash: Some(hs),
            dest_fp: Some(d_st.fingerprint()),
            src_fp: Some(s_st.fingerprint()),
            synced_at: Some(now),
            verified_at: Some(now),
        },
    )?;
    ctx.db.log_event(
        Some(ctx.dest_id),
        "adopt",
        Some(rel),
        "ok",
        "existing identical file is now managed",
        false,
    )?;
    Ok(true)
}
