//! Crash and disconnect recovery.
//!
//! The database and the filesystem are not one transaction, so after an
//! interruption the journal step is only a hint. Decisions come from what is
//! actually on the drive: does the target exist, does it hash to the new
//! content, does the preserved previous version exist.
//!
//! Rules (never destructive):
//! - target already has the new content -> the commit happened: finish the DB side
//! - target missing but the previous version is preserved -> put it back
//! - target present with something else -> leave it alone, drop our temp (or ask)
//! - anything ambiguous -> NeedsAttention, nothing deleted

use super::fsops::{DirFd, Stat, is_not_found};
use super::{TransferCtx, hash_reader, split_rel, walk_err};
use crate::domain::{EntryKind, Id, RelPath};
use crate::errors::{Error, IoContext, Result};
use crate::storage::{DestEntry, EntryStatus, OpKind, OpRow, OpState, VersionRow, now_secs};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The commit had already happened; bookkeeping completed.
    Finalized,
    /// The previous version was restored to the target path.
    RolledBack,
    /// Nothing had been committed; temp file removed.
    Aborted,
    NeedsAttention(String),
}

#[derive(Clone, Debug)]
pub struct RecoveryReport {
    pub op_id: Id,
    pub rel: RelPath,
    pub outcome: RecoveryOutcome,
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryResult {
    pub ops: Vec<RecoveryReport>,
    /// Files in `tmp/` that TwinSync cannot prove it owns. Reported, never deleted.
    pub unknown_in_tmp: Vec<String>,
}

/// Run before any new write to this destination, and only after drive identity is confirmed.
pub fn recover(ctx: &TransferCtx) -> Result<RecoveryResult> {
    let mut result = RecoveryResult::default();
    for op in ctx.db.unfinished_ops(ctx.dest_id)? {
        let outcome = recover_op(ctx, &op)?;
        let (state, text) = match &outcome {
            RecoveryOutcome::Finalized => (None, "finished after interruption"),
            RecoveryOutcome::RolledBack => (
                Some(OpState::Aborted),
                "interrupted update rolled back; previous version restored",
            ),
            RecoveryOutcome::Aborted => (
                Some(OpState::Aborted),
                "interrupted copy discarded; nothing was committed",
            ),
            RecoveryOutcome::NeedsAttention(m) => (Some(OpState::Attention), m.as_str()),
        };
        if let Some(s) = state {
            ctx.db.finish_op(&op.id, s, Some(text))?;
        }
        let ok = !matches!(outcome, RecoveryOutcome::NeedsAttention(_));
        ctx.db.log_event(
            Some(ctx.dest_id),
            "recovery",
            Some(&op.rel),
            if ok { "ok" } else { "attention" },
            text,
            false,
        )?;
        result.ops.push(RecoveryReport {
            op_id: op.id.clone(),
            rel: op.rel.clone(),
            outcome,
        });
    }
    sweep_tmp(ctx, &mut result)?;
    Ok(result)
}

fn stat_in(dir: &Option<DirFd>, name: &[u8]) -> Result<Option<Stat>> {
    match dir {
        Some(d) => d.stat(name).ctx("recovery stat"),
        None => Ok(None),
    }
}

fn open_existing(root: &DirFd, comps: &[&[u8]]) -> Result<Option<DirFd>> {
    match root.walk(comps, false) {
        Ok(d) => Ok(Some(d)),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(walk_err(e, "recovery: open folder")),
    }
}

fn recover_op(ctx: &TransferCtx, op: &OpRow) -> Result<RecoveryOutcome> {
    let (name, parent) = split_rel(&op.rel);
    let dst_parent = open_existing(ctx.root, &parent)?;
    let target = stat_in(&dst_parent, name)?;

    // Where the preserved previous version would be.
    let (vdir, vname): (Option<DirFd>, Option<Vec<u8>>) = match &op.version_rel {
        Some(v) => {
            let comps: Vec<&[u8]> = v.components().collect();
            let (n, p) = comps.split_last().expect("RelPath has a component");
            (open_existing(&ctx.dirs.versions, p)?, Some(n.to_vec()))
        }
        None => (None, None),
    };
    let version = match &vname {
        Some(n) => stat_in(&vdir, n)?,
        None => None,
    };
    let temp_exists = ctx
        .dirs
        .tmp
        .stat(op.temp_name.as_bytes())
        .ctx("recovery stat temp")?
        .is_some();
    let drop_temp = |ctx: &TransferCtx| -> Result<()> {
        if temp_exists {
            match ctx.dirs.tmp.unlink(op.temp_name.as_bytes()) {
                Ok(()) => {}
                Err(e) if is_not_found(&e) => {}
                Err(e) => return Err(Error::io("remove interrupted temp file", e)),
            }
        }
        Ok(())
    };

    if let Some(t) = &target
        && !t.is_file()
    {
        return Ok(RecoveryOutcome::NeedsAttention(
            "a folder or special file is at the target path of an interrupted operation".into(),
        ));
    }

    // Committed already? Only possible once the new hash was journaled.
    if let (Some(t), Some(new_hash), Some(dir)) = (&target, op.new_hash, &dst_parent) {
        let mut f = dir.open_read(name).ctx("recovery open target")?;
        let (h, _) = hash_reader(&mut f, ctx.chunk, ctx.cancel)?;
        if h == new_hash {
            finalize(ctx, op, t, version.as_ref())?;
            drop_temp(ctx)?;
            return Ok(RecoveryOutcome::Finalized);
        }
    }

    match (&target, version.is_some(), op.kind) {
        // Something else sits at the target while the old version was already moved away.
        (Some(_), true, OpKind::Update) => Ok(RecoveryOutcome::NeedsAttention(
            "a different file appeared at the target after the previous version was preserved; \
             both were kept"
                .into(),
        )),
        // Nothing destructive happened (or the target is not ours): discard our temp.
        (Some(_), _, _) => {
            drop_temp(ctx)?;
            Ok(RecoveryOutcome::Aborted)
        }
        // Target gone, previous version safe: restore it.
        (None, true, OpKind::Update) => {
            let (Some(dir), Some(vdir), Some(vname)) = (&dst_parent, &vdir, &vname) else {
                return Ok(RecoveryOutcome::NeedsAttention(
                    "previous version exists but its folders could not be opened".into(),
                ));
            };
            match vdir.rename_excl(vname, dir, name) {
                Ok(()) => {
                    dir.sync().ctx("sync restored folder")?;
                    drop_temp(ctx)?;
                    Ok(RecoveryOutcome::RolledBack)
                }
                Err(e) => Ok(RecoveryOutcome::NeedsAttention(format!(
                    "could not restore the previous version: {e}"
                ))),
            }
        }
        // Target and any preserved version are both absent: nothing to restore.
        (None, _, _) => {
            drop_temp(ctx)?;
            Ok(RecoveryOutcome::Aborted)
        }
    }
}

fn finalize(ctx: &TransferCtx, op: &OpRow, target: &Stat, version: Option<&Stat>) -> Result<()> {
    let now = now_secs();
    let entry = DestEntry {
        rel: op.rel.clone(),
        kind: EntryKind::File,
        status: EntryStatus::Synced,
        synced_hash: op.new_hash,
        dest_fp: Some(target.fingerprint()),
        src_fp: op.src_fp,
        synced_at: Some(now),
        verified_at: None,
    };
    let version_row = match (&op.version_rel, version) {
        (Some(v), Some(st)) => Some(VersionRow {
            rel: op.rel.clone(),
            version_rel: v.clone(),
            size: st.size(),
            hash: op.expected_old_hash,
            op_id: Some(op.id.clone()),
            created_at: now,
        }),
        _ => None,
    };
    ctx.db.commit_op(
        &op.id,
        ctx.dest_id,
        &entry,
        version_row.as_ref(),
        "completed by recovery",
    )
}

/// Remove leftovers of operations that are finished; report anything unrecognized.
fn sweep_tmp(ctx: &TransferCtx, result: &mut RecoveryResult) -> Result<()> {
    for raw in ctx.dirs.tmp.list().ctx("list temp folder")? {
        let name = String::from_utf8_lossy(&raw).into_owned();
        let owned_op = name
            .strip_suffix(".part")
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            .map(str::to_string);
        let Some(id) = owned_op else {
            result.unknown_in_tmp.push(name);
            continue;
        };
        match ctx.db.op(&id)? {
            Some(op) if matches!(op.state, OpState::Done | OpState::Failed | OpState::Aborted) => {
                match ctx.dirs.tmp.unlink(&raw) {
                    Ok(()) => {}
                    Err(e) if is_not_found(&e) => {}
                    Err(e) => return Err(Error::io("remove leftover temp file", e)),
                }
            }
            // Attention: keep until the user decides. Unknown id: not provably ours.
            Some(_) => {}
            None => result.unknown_in_tmp.push(name),
        }
    }
    Ok(())
}
