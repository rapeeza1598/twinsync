//! SQLite persistence. Every write is one short transaction; nothing here is
//! ever held open across a copy or while waiting for a device.

pub mod legacy;
mod migrations;

use crate::config::Settings;
use crate::domain::{
    DriveMediaType, DrivePerformanceProfile, EntryKind, Fingerprint, Hash, Id, RelPath, Slot,
};
use crate::errors::{Error, IoContext, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use migrations::latest_version;

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn path_blob(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

fn blob_path(b: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(b))
}

fn fp_cols(fp: &Option<Fingerprint>) -> (Option<i64>, Option<i64>, Option<i64>) {
    match fp {
        Some(f) => (Some(f.size as i64), Some(f.mtime_ns), Some(f.inode as i64)),
        None => (None, None, None),
    }
}

fn fp_from(r: &Row<'_>, i: usize) -> rusqlite::Result<Option<Fingerprint>> {
    let size: Option<i64> = r.get(i)?;
    let mtime: Option<i64> = r.get(i + 1)?;
    let inode: Option<i64> = r.get(i + 2)?;
    Ok(match (size, mtime, inode) {
        (Some(s), Some(m), Some(n)) => Some(Fingerprint {
            size: s as u64,
            mtime_ns: m,
            inode: n as u64,
        }),
        _ => None,
    })
}

fn hash_from(r: &Row<'_>, i: usize) -> rusqlite::Result<Option<Hash>> {
    Ok(r.get::<_, Option<Vec<u8>>>(i)?
        .and_then(|b| Hash::from_slice(&b)))
}

fn rel_from(r: &Row<'_>, i: usize) -> rusqlite::Result<RelPath> {
    let b: Vec<u8> = r.get(i)?;
    RelPath::from_bytes(b).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Blob, Box::new(e))
    })
}

fn hash_blob(h: &Option<Hash>) -> Option<Vec<u8>> {
    h.map(|h| h.0.to_vec())
}

// ---------------------------------------------------------------- row types

#[derive(Clone, Debug)]
pub struct Project {
    pub id: Id,
    pub name: String,
    pub source_path: Option<PathBuf>,
    pub settings: Settings,
    pub source_generation: i64,
    pub last_scan_complete: bool,
    pub last_scan_at: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct DestinationRow {
    pub id: Id,
    pub project_id: Id,
    pub slot: Slot,
    pub label: String,
    pub volume_uuid: Option<String>,
    pub volume_name: Option<String>,
    pub fs_type: Option<String>,
    pub media_type: DriveMediaType,
    pub bus_protocol: Option<String>,
    pub physical_id: Option<String>,
    pub identity_strong: bool,
    pub profile: DrivePerformanceProfile,
    /// Destination folder relative to the volume mount point; empty means the volume root.
    pub root_rel_path: PathBuf,
    pub last_mount_path: Option<PathBuf>,
    pub free_bytes: Option<u64>,
    pub free_checked_at: Option<i64>,
    pub last_sync_at: Option<i64>,
    pub last_verify_at: Option<i64>,
    /// Source generation this destination was last fully planned against; -1 = never.
    pub planned_generation: i64,
}

impl DestinationRow {
    pub fn new(project_id: &str, slot: Slot) -> Self {
        Self {
            id: crate::domain::new_id(),
            project_id: project_id.to_string(),
            slot,
            label: slot.default_label(),
            volume_uuid: None,
            volume_name: None,
            fs_type: None,
            media_type: DriveMediaType::Unknown,
            bus_protocol: None,
            physical_id: None,
            identity_strong: false,
            profile: DrivePerformanceProfile::Auto,
            root_rel_path: PathBuf::new(),
            last_mount_path: None,
            free_bytes: None,
            free_checked_at: None,
            last_sync_at: None,
            last_verify_at: None,
            planned_generation: -1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceEntry {
    pub rel: RelPath,
    pub kind: EntryKind,
    pub fp: Fingerprint,
    pub hash: Option<Hash>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryStatus {
    Synced,
    /// User chose to leave this path alone (until the source content changes again).
    Ignored,
    /// Source file is gone; the destination copy is retained and acknowledged.
    Orphaned,
    /// User approved replacing the existing destination file (previous version is kept).
    ReplaceApproved,
}

impl EntryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Ignored => "ignored",
            Self::Orphaned => "orphaned",
            Self::ReplaceApproved => "replace_approved",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "ignored" => Self::Ignored,
            "orphaned" => Self::Orphaned,
            "replace_approved" => Self::ReplaceApproved,
            _ => Self::Synced,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DestEntry {
    pub rel: RelPath,
    pub kind: EntryKind,
    pub status: EntryStatus,
    pub synced_hash: Option<Hash>,
    pub dest_fp: Option<Fingerprint>,
    pub src_fp: Option<Fingerprint>,
    pub synced_at: Option<i64>,
    pub verified_at: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpKind {
    MakeDir,
    Create,
    Update,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MakeDir => "mkdir",
            Self::Create => "create",
            Self::Update => "update",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "mkdir" => Self::MakeDir,
            "update" => Self::Update,
            _ => Self::Create,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpState {
    Running,
    Done,
    Failed,
    Aborted,
    Attention,
}

impl OpState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Attention => "attention",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "failed" => Self::Failed,
            "aborted" => Self::Aborted,
            "attention" => Self::Attention,
            _ => Self::Running,
        }
    }
}

/// Journal checkpoints, in the order they are reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum OpStep {
    Started,
    /// Temp file fully written, synced and read back; `new_hash` is recorded.
    TempReady,
    /// Previous version moved into version history; target path is currently empty.
    Preserved,
    /// Temp renamed onto the target. Database not yet updated.
    Committed,
}

impl OpStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::TempReady => "temp_ready",
            Self::Preserved => "preserved",
            Self::Committed => "committed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "temp_ready" => Self::TempReady,
            "preserved" => Self::Preserved,
            "committed" => Self::Committed,
            _ => Self::Started,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpRow {
    pub id: Id,
    pub destination_id: Id,
    pub kind: OpKind,
    pub rel: RelPath,
    pub state: OpState,
    pub step: OpStep,
    pub temp_name: String,
    /// Path of the preserved previous version, relative to the project's `versions/` dir.
    pub version_rel: Option<RelPath>,
    pub expected_old_hash: Option<Hash>,
    pub new_hash: Option<Hash>,
    pub src_fp: Option<Fingerprint>,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub attempts: u32,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VersionRow {
    pub rel: RelPath,
    pub version_rel: RelPath,
    pub size: u64,
    pub hash: Option<Hash>,
    pub op_id: Option<Id>,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct ActivityEvent {
    pub id: i64,
    pub ts: i64,
    pub destination_id: Option<Id>,
    pub kind: String,
    pub rel: Option<RelPath>,
    pub result: String,
    pub detail: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ActivityFilter {
    pub destination_id: Option<Id>,
    pub only_failures: bool,
    pub limit: i64,
}

// ---------------------------------------------------------------------- Db

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Each thread opens its own `Db`; SQLite handles cross-connection locking.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ctx("create data directory")?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )?;
        migrations::run(&conn)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )?)
    }

    // ------------------------------------------------------------ projects

    pub fn first_project(&self) -> Result<Option<Project>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name, source_path, settings_json, source_generation,
                        last_scan_complete, last_scan_at
                 FROM projects ORDER BY rowid LIMIT 1",
                [],
                |r| {
                    let json: String = r.get(3)?;
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        source_path: r.get::<_, Option<Vec<u8>>>(2)?.map(blob_path),
                        settings: serde_json::from_str(&json).unwrap_or_default(),
                        source_generation: r.get(4)?,
                        last_scan_complete: r.get::<_, i64>(5)? != 0,
                        last_scan_at: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn create_project(&self, name: &str, source: Option<&Path>) -> Result<Project> {
        let id = crate::domain::new_id();
        let settings = Settings::default();
        self.conn.execute(
            "INSERT INTO projects (id, name, source_path, settings_json) VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                name,
                source.map(path_blob),
                serde_json::to_string(&settings).map_err(|e| Error::Config(e.to_string()))?
            ],
        )?;
        Ok(self.first_project()?.expect("project just inserted"))
    }

    pub fn set_source_path(&self, project_id: &str, source: &Path) -> Result<()> {
        // A new source invalidates everything derived from the old one.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE projects SET source_path = ?2, source_generation = source_generation + 1,
                 last_scan_complete = 0, last_scan_at = NULL WHERE id = ?1",
            params![project_id, path_blob(source)],
        )?;
        tx.execute(
            "DELETE FROM source_entries WHERE project_id = ?1",
            [project_id],
        )?;
        tx.execute(
            "UPDATE destinations SET planned_generation = -1 WHERE project_id = ?1",
            [project_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn save_settings(&self, project_id: &str, settings: &Settings) -> Result<()> {
        self.conn.execute(
            "UPDATE projects SET settings_json = ?2 WHERE id = ?1",
            params![
                project_id,
                serde_json::to_string(settings).map_err(|e| Error::Config(e.to_string()))?
            ],
        )?;
        Ok(())
    }

    /// Record a finished source scan and return the new generation number.
    /// Entries not seen are removed only when the scan was complete; after an
    /// incomplete scan they are unknown, not deleted.
    pub fn commit_scan(
        &self,
        project_id: &str,
        entries: &[SourceEntry],
        complete: bool,
    ) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let generation: i64 = tx.query_row(
            "SELECT source_generation + 1 FROM projects WHERE id = ?1",
            [project_id],
            |r| r.get(0),
        )?;
        {
            let mut up = tx.prepare(
                "INSERT INTO source_entries
                    (project_id, rel_path, kind, size, mtime_ns, inode, hash, seen_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (project_id, rel_path) DO UPDATE SET
                    kind = excluded.kind, size = excluded.size, mtime_ns = excluded.mtime_ns,
                    inode = excluded.inode, hash = excluded.hash,
                    seen_generation = excluded.seen_generation",
            )?;
            for e in entries {
                up.execute(params![
                    project_id,
                    e.rel.as_bytes(),
                    e.kind.as_str(),
                    e.fp.size as i64,
                    e.fp.mtime_ns,
                    e.fp.inode as i64,
                    hash_blob(&e.hash),
                    generation
                ])?;
            }
        }
        if complete {
            tx.execute(
                "DELETE FROM source_entries WHERE project_id = ?1 AND seen_generation < ?2",
                params![project_id, generation],
            )?;
        }
        tx.execute(
            "UPDATE projects SET source_generation = ?2, last_scan_complete = ?3,
                 last_scan_at = ?4 WHERE id = ?1",
            params![project_id, generation, complete as i64, now_secs()],
        )?;
        tx.commit()?;
        Ok(generation)
    }

    pub fn source_entries(&self, project_id: &str) -> Result<HashMap<RelPath, SourceEntry>> {
        let mut st = self.conn.prepare(
            "SELECT rel_path, kind, size, mtime_ns, inode, hash
             FROM source_entries WHERE project_id = ?1",
        )?;
        let rows = st.query_map([project_id], |r| {
            let rel = rel_from(r, 0)?;
            Ok(SourceEntry {
                rel,
                kind: EntryKind::parse(&r.get::<_, String>(1)?).unwrap_or(EntryKind::File),
                fp: Fingerprint {
                    size: r.get::<_, i64>(2)? as u64,
                    mtime_ns: r.get(3)?,
                    inode: r.get::<_, i64>(4)? as u64,
                },
                hash: hash_from(r, 5)?,
            })
        })?;
        let mut out = HashMap::new();
        for e in rows {
            let e = e?;
            out.insert(e.rel.clone(), e);
        }
        Ok(out)
    }

    // -------------------------------------------------------- destinations

    pub fn upsert_destination(&self, d: &DestinationRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO destinations
                (id, project_id, slot, label, volume_uuid, volume_name, fs_type, media_type,
                 bus_protocol, physical_id, identity_strong, performance_profile, root_rel_path,
                 last_mount_path, free_bytes, free_checked_at, last_sync_at, last_verify_at,
                 planned_generation)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
             ON CONFLICT (id) DO UPDATE SET
                label=excluded.label, volume_uuid=excluded.volume_uuid,
                volume_name=excluded.volume_name, fs_type=excluded.fs_type,
                media_type=excluded.media_type, bus_protocol=excluded.bus_protocol,
                physical_id=excluded.physical_id, identity_strong=excluded.identity_strong,
                performance_profile=excluded.performance_profile,
                root_rel_path=excluded.root_rel_path, last_mount_path=excluded.last_mount_path,
                free_bytes=excluded.free_bytes, free_checked_at=excluded.free_checked_at,
                last_sync_at=excluded.last_sync_at, last_verify_at=excluded.last_verify_at,
                planned_generation=excluded.planned_generation",
            params![
                d.id,
                d.project_id,
                d.slot.as_str(),
                d.label,
                d.volume_uuid,
                d.volume_name,
                d.fs_type,
                d.media_type.as_str(),
                d.bus_protocol,
                d.physical_id,
                d.identity_strong as i64,
                d.profile.as_str(),
                path_blob(&d.root_rel_path),
                d.last_mount_path.as_deref().map(path_blob),
                d.free_bytes.map(|v| v as i64),
                d.free_checked_at,
                d.last_sync_at,
                d.last_verify_at,
                d.planned_generation
            ],
        )?;
        Ok(())
    }

    pub fn destinations(&self, project_id: &str) -> Result<Vec<DestinationRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, project_id, slot, label, volume_uuid, volume_name, fs_type, media_type,
                    bus_protocol, physical_id, identity_strong, performance_profile,
                    root_rel_path, last_mount_path, free_bytes, free_checked_at, last_sync_at,
                    last_verify_at, planned_generation
             FROM destinations WHERE project_id = ?1 ORDER BY slot",
        )?;
        let rows = st.query_map([project_id], |r| {
            Ok(DestinationRow {
                id: r.get(0)?,
                project_id: r.get(1)?,
                slot: Slot::parse(&r.get::<_, String>(2)?).unwrap_or(Slot::A),
                label: r.get(3)?,
                volume_uuid: r.get(4)?,
                volume_name: r.get(5)?,
                fs_type: r.get(6)?,
                media_type: DriveMediaType::parse(&r.get::<_, String>(7)?),
                bus_protocol: r.get(8)?,
                physical_id: r.get(9)?,
                identity_strong: r.get::<_, i64>(10)? != 0,
                profile: DrivePerformanceProfile::parse(&r.get::<_, String>(11)?),
                root_rel_path: blob_path(r.get(12)?),
                last_mount_path: r.get::<_, Option<Vec<u8>>>(13)?.map(blob_path),
                free_bytes: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
                free_checked_at: r.get(15)?,
                last_sync_at: r.get(16)?,
                last_verify_at: r.get(17)?,
                planned_generation: r.get(18)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn destination(&self, id: &str) -> Result<Option<DestinationRow>> {
        let project: Option<String> = self
            .conn
            .query_row(
                "SELECT project_id FROM destinations WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match project {
            Some(p) => self.destinations(&p)?.into_iter().find(|d| d.id == id),
            None => None,
        })
    }

    pub fn set_planned_generation(&self, dest_id: &str, generation: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE destinations SET planned_generation = ?2 WHERE id = ?1",
            params![dest_id, generation],
        )?;
        Ok(())
    }

    pub fn set_free_space(&self, dest_id: &str, free: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE destinations SET free_bytes = ?2, free_checked_at = ?3 WHERE id = ?1",
            params![dest_id, free as i64, now_secs()],
        )?;
        Ok(())
    }

    pub fn set_last_mount(&self, dest_id: &str, mount: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE destinations SET last_mount_path = ?2 WHERE id = ?1",
            params![dest_id, path_blob(mount)],
        )?;
        Ok(())
    }

    pub fn touch_verified(&self, dest_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE destinations SET last_verify_at = ?2 WHERE id = ?1",
            params![dest_id, now_secs()],
        )?;
        Ok(())
    }

    // ---------------------------------------------------- destination entries

    pub fn dest_entries(&self, dest_id: &str) -> Result<HashMap<RelPath, DestEntry>> {
        let mut st = self.conn.prepare(
            "SELECT rel_path, kind, status, synced_hash, dest_size, dest_mtime_ns, dest_inode,
                    src_size, src_mtime_ns, src_inode, synced_at, verified_at
             FROM destination_entries WHERE destination_id = ?1",
        )?;
        let rows = st.query_map([dest_id], |r| {
            Ok(DestEntry {
                rel: rel_from(r, 0)?,
                kind: EntryKind::parse(&r.get::<_, String>(1)?).unwrap_or(EntryKind::File),
                status: EntryStatus::parse(&r.get::<_, String>(2)?),
                synced_hash: hash_from(r, 3)?,
                dest_fp: fp_from(r, 4)?,
                src_fp: fp_from(r, 7)?,
                synced_at: r.get(10)?,
                verified_at: r.get(11)?,
            })
        })?;
        let mut out = HashMap::new();
        for e in rows {
            let e = e?;
            out.insert(e.rel.clone(), e);
        }
        Ok(out)
    }

    pub fn upsert_dest_entry(&self, dest_id: &str, e: &DestEntry) -> Result<()> {
        upsert_dest_entry(&self.conn, dest_id, e)
    }

    pub fn delete_dest_entry(&self, dest_id: &str, rel: &RelPath) -> Result<()> {
        self.conn.execute(
            "DELETE FROM destination_entries WHERE destination_id = ?1 AND rel_path = ?2",
            params![dest_id, rel.as_bytes()],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------- operations

    pub fn insert_op(&self, op: &OpRow) -> Result<()> {
        let now = now_secs();
        let (s, m, i) = fp_cols(&op.src_fp);
        self.conn.execute(
            "INSERT INTO sync_operations
                (id, destination_id, kind, rel_path, state, step, temp_name, version_rel_path,
                 expected_old_hash, new_hash, src_size, src_mtime_ns, src_inode, bytes_total,
                 bytes_done, attempts, error, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?18)",
            params![
                op.id,
                op.destination_id,
                op.kind.as_str(),
                op.rel.as_bytes(),
                op.state.as_str(),
                op.step.as_str(),
                op.temp_name,
                op.version_rel.as_ref().map(|v| v.as_bytes().to_vec()),
                hash_blob(&op.expected_old_hash),
                hash_blob(&op.new_hash),
                s,
                m,
                i,
                op.bytes_total as i64,
                op.bytes_done as i64,
                op.attempts,
                op.error,
                now
            ],
        )?;
        Ok(())
    }

    pub fn set_op_step(&self, id: &str, step: OpStep, new_hash: Option<Hash>) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_operations SET step = ?2, new_hash = COALESCE(?3, new_hash),
                 updated_at = ?4 WHERE id = ?1",
            params![id, step.as_str(), hash_blob(&new_hash), now_secs()],
        )?;
        Ok(())
    }

    pub fn set_op_progress(&self, id: &str, bytes_done: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_operations SET bytes_done = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, bytes_done as i64, now_secs()],
        )?;
        Ok(())
    }

    pub fn finish_op(&self, id: &str, state: OpState, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_operations SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, state.as_str(), error, now_secs()],
        )?;
        Ok(())
    }

    pub fn op(&self, id: &str) -> Result<Option<OpRow>> {
        Ok(self
            .query_ops("WHERE id = ?1", params![id])?
            .into_iter()
            .next())
    }

    /// Operations whose outcome was never reconciled with the filesystem.
    pub fn unfinished_ops(&self, dest_id: &str) -> Result<Vec<OpRow>> {
        self.query_ops(
            "WHERE destination_id = ?1 AND state = 'running' ORDER BY created_at",
            params![dest_id],
        )
    }

    fn query_ops(&self, tail: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<OpRow>> {
        let sql = format!(
            "SELECT id, destination_id, kind, rel_path, state, step, temp_name, version_rel_path,
                    expected_old_hash, new_hash, src_size, src_mtime_ns, src_inode, bytes_total,
                    bytes_done, attempts, error FROM sync_operations {tail}"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(p, |r| {
            Ok(OpRow {
                id: r.get(0)?,
                destination_id: r.get(1)?,
                kind: OpKind::parse(&r.get::<_, String>(2)?),
                rel: rel_from(r, 3)?,
                state: OpState::parse(&r.get::<_, String>(4)?),
                step: OpStep::parse(&r.get::<_, String>(5)?),
                temp_name: r.get(6)?,
                version_rel: match r.get::<_, Option<Vec<u8>>>(7)? {
                    Some(b) => RelPath::from_bytes(b).ok(),
                    None => None,
                },
                expected_old_hash: hash_from(r, 8)?,
                new_hash: hash_from(r, 9)?,
                src_fp: fp_from(r, 10)?,
                bytes_total: r.get::<_, i64>(13)? as u64,
                bytes_done: r.get::<_, i64>(14)? as u64,
                attempts: r.get(15)?,
                error: r.get(16)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Final step of a transfer, after the filesystem commit: manifest entry,
    /// optional preserved-version record and the operation's terminal state go
    /// in one transaction so they cannot disagree.
    pub fn commit_op(
        &self,
        op_id: &str,
        dest_id: &str,
        entry: &DestEntry,
        version: Option<&VersionRow>,
        detail: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        upsert_dest_entry(&tx, dest_id, entry)?;
        if let Some(v) = version {
            tx.execute(
                "INSERT INTO file_versions
                    (destination_id, rel_path, version_rel_path, size, hash, op_id, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    dest_id,
                    v.rel.as_bytes(),
                    v.version_rel.as_bytes(),
                    v.size as i64,
                    hash_blob(&v.hash),
                    v.op_id,
                    v.created_at
                ],
            )?;
        }
        tx.execute(
            "UPDATE sync_operations SET state = 'done', error = NULL, updated_at = ?2 WHERE id = ?1",
            params![op_id, now_secs()],
        )?;
        tx.execute(
            "UPDATE destinations SET last_sync_at = ?2 WHERE id = ?1",
            params![dest_id, now_secs()],
        )?;
        tx.execute(
            "INSERT INTO activity_events (ts, destination_id, kind, rel_path, result, detail)
             VALUES (?1, ?2, ?3, ?4, 'ok', ?5)",
            params![
                now_secs(),
                dest_id,
                "transfer",
                entry.rel.as_bytes(),
                detail
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ versions

    pub fn versions(&self, dest_id: &str) -> Result<Vec<VersionRow>> {
        let mut st = self.conn.prepare(
            "SELECT rel_path, version_rel_path, size, hash, op_id, created_at
             FROM file_versions WHERE destination_id = ?1 ORDER BY id",
        )?;
        let rows = st.query_map([dest_id], |r| {
            Ok(VersionRow {
                rel: rel_from(r, 0)?,
                version_rel: rel_from(r, 1)?,
                size: r.get::<_, i64>(2)? as u64,
                hash: hash_from(r, 3)?,
                op_id: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn version_bytes(&self, dest_id: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM file_versions WHERE destination_id = ?1",
            [dest_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    // ------------------------------------------------------------ activity

    pub fn log_event(
        &self,
        dest_id: Option<&str>,
        kind: &str,
        rel: Option<&RelPath>,
        result: &str,
        detail: &str,
        retryable: bool,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO activity_events
                (ts, destination_id, kind, rel_path, result, detail, retryable)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                now_secs(),
                dest_id,
                kind,
                rel.map(|r| r.as_bytes().to_vec()),
                result,
                detail,
                retryable as i64
            ],
        )?;
        Ok(())
    }

    pub fn activity(&self, f: &ActivityFilter) -> Result<Vec<ActivityEvent>> {
        let limit = if f.limit > 0 { f.limit } else { 500 };
        let mut st = self.conn.prepare(
            "SELECT id, ts, destination_id, kind, rel_path, result, detail, retryable
             FROM activity_events
             WHERE (?1 IS NULL OR destination_id = ?1) AND (?2 = 0 OR result != 'ok')
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(
            params![f.destination_id, f.only_failures as i64, limit],
            |r| {
                Ok(ActivityEvent {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    destination_id: r.get(2)?,
                    kind: r.get(3)?,
                    rel: r
                        .get::<_, Option<Vec<u8>>>(4)?
                        .and_then(|b| RelPath::from_bytes(b).ok()),
                    result: r.get(5)?,
                    detail: r.get(6)?,
                    retryable: r.get::<_, i64>(7)? != 0,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn upsert_dest_entry(conn: &Connection, dest_id: &str, e: &DestEntry) -> Result<()> {
    let (ds, dm, di) = fp_cols(&e.dest_fp);
    let (ss, sm, si) = fp_cols(&e.src_fp);
    conn.execute(
        "INSERT INTO destination_entries
            (destination_id, rel_path, kind, status, synced_hash, dest_size, dest_mtime_ns,
             dest_inode, src_size, src_mtime_ns, src_inode, synced_at, verified_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT (destination_id, rel_path) DO UPDATE SET
            kind=excluded.kind, status=excluded.status, synced_hash=excluded.synced_hash,
            dest_size=excluded.dest_size, dest_mtime_ns=excluded.dest_mtime_ns,
            dest_inode=excluded.dest_inode, src_size=excluded.src_size,
            src_mtime_ns=excluded.src_mtime_ns, src_inode=excluded.src_inode,
            synced_at=excluded.synced_at, verified_at=excluded.verified_at",
        params![
            dest_id,
            e.rel.as_bytes(),
            e.kind.as_str(),
            e.status.as_str(),
            hash_blob(&e.synced_hash),
            ds,
            dm,
            di,
            ss,
            sm,
            si,
            e.synced_at,
            e.verified_at
        ],
    )?;
    Ok(())
}
