//! Pure sync planning: source scan + manifest + destination observations in,
//! list of actions out. No I/O, no clock, so every rule is unit-testable.
//!
//! Update Copy never deletes anything. A path that vanished from the source or
//! from the destination is surfaced for a human, not acted on.

use crate::domain::{EntryKind, Fingerprint, FsCapabilities, RelPath};
use crate::scanner::ScanResult;
use crate::storage::{DestEntry, EntryStatus, SourceEntry};
use std::collections::{BTreeMap, HashMap};
use unicode_normalization::UnicodeNormalization;

/// What the worker found at a destination path just before planning.
#[derive(Clone, Debug, PartialEq)]
pub enum Observed {
    Missing,
    File(Fingerprint),
    Dir,
    /// Symlink, fifo, device... never touched.
    Other,
    Unreadable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockReason {
    TooLarge {
        limit: u64,
    },
    BadName(String),
    /// Two source paths map to the same destination name (case or Unicode normalization).
    NameCollision(String),
    ReservedName,
    InsufficientSpace,
    DestinationUnreadable(String),
}

impl BlockReason {
    pub fn describe(&self) -> String {
        match self {
            Self::TooLarge { limit } => {
                format!("file is larger than the {limit} byte limit of this filesystem")
            }
            Self::BadName(why) => format!("file name not supported: {why}"),
            Self::NameCollision(other) => format!("name collides with {other} on this destination"),
            Self::ReservedName => "the .twinsync name is reserved".into(),
            Self::InsufficientSpace => "not enough free space on the destination".into(),
            Self::DestinationUnreadable(e) => format!("destination could not be read: {e}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CollisionKind {
    /// A file TwinSync does not manage already exists at the target path.
    ExistingFile,
    ExistingDirOrOther,
    /// Case-only rename of a managed file on a case-insensitive destination.
    CaseOnlyRename,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    MakeDir,
    Copy,
    /// Replace a managed copy; the previous version goes to version history.
    Update,
    /// Destination fingerprint changed: hash it to tell a harmless touch from an edit.
    CheckDestination,
    Collision(CollisionKind),
    /// Managed copy was edited outside TwinSync; waits for the user.
    Conflict,
    /// Managed copy disappeared from the destination; waits for the user.
    ExternallyDeleted,
    /// File vanished from the source; the destination copy is kept.
    SourceDeleted,
    Blocked(BlockReason),
}

impl Action {
    pub fn is_transfer(&self) -> bool {
        matches!(self, Action::MakeDir | Action::Copy | Action::Update)
    }

    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Action::Collision(_)
                | Action::Conflict
                | Action::ExternallyDeleted
                | Action::SourceDeleted
                | Action::Blocked(_)
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Action::MakeDir => "Create folder",
            Action::Copy => "Copy",
            Action::Update => "Update",
            Action::CheckDestination => "Check destination",
            Action::Collision(_) => "Collision",
            Action::Conflict => "Conflict",
            Action::ExternallyDeleted => "Deleted on drive",
            Action::SourceDeleted => "Removed from source",
            Action::Blocked(_) => "Blocked",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlannedItem {
    pub rel: RelPath,
    pub kind: EntryKind,
    pub action: Action,
    /// Bytes this item will write (0 for anything that is not a transfer).
    pub size: u64,
    pub reason: String,
    /// Preconditions the executor re-checks before touching anything.
    pub expected_src: Option<Fingerprint>,
    pub expected_dest: Observed,
}

#[derive(Clone, Debug, Default)]
pub struct PlanSummary {
    pub copies: usize,
    pub updates: usize,
    pub make_dirs: usize,
    pub checks: usize,
    pub attention: usize,
    pub bytes_to_write: u64,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub source_generation: i64,
    /// False when the source scan was incomplete: the plan may be missing items.
    pub source_scan_complete: bool,
    pub items: Vec<PlannedItem>,
}

impl Plan {
    pub fn summary(&self) -> PlanSummary {
        let mut s = PlanSummary::default();
        for i in &self.items {
            match i.action {
                Action::Copy => s.copies += 1,
                Action::Update => s.updates += 1,
                Action::MakeDir => s.make_dirs += 1,
                Action::CheckDestination => s.checks += 1,
                _ => {}
            }
            if i.action.needs_attention() {
                s.attention += 1;
            }
            if i.action.is_transfer() {
                s.bytes_to_write += i.size;
            }
        }
        s
    }

    /// Nothing left to do or to decide for this destination.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub struct PlanInput<'a> {
    pub source: &'a ScanResult,
    pub manifest: &'a HashMap<RelPath, DestEntry>,
    pub observed: &'a HashMap<RelPath, Observed>,
    pub caps: &'a FsCapabilities,
    pub free_bytes: Option<u64>,
    pub reserve_bytes: u64,
}

/// Key two names must NOT share on the destination: Unicode-normalization
/// insensitive on every macOS filesystem, plus case-insensitive where the
/// destination is.
pub fn collision_key(rel: &RelPath, case_insensitive: bool) -> String {
    let s: String = String::from_utf8_lossy(rel.as_bytes()).nfc().collect();
    if case_insensitive {
        s.to_lowercase()
    } else {
        s
    }
}

fn source_changed(s: &SourceEntry, m: &DestEntry) -> bool {
    match (s.hash, m.synced_hash) {
        (Some(a), Some(b)) => a != b,
        _ => match &m.src_fp {
            Some(f) => !s.fp.same_as(f, true),
            None => true,
        },
    }
}

fn item(
    s: &SourceEntry,
    action: Action,
    observed: &Observed,
    reason: impl Into<String>,
) -> PlannedItem {
    PlannedItem {
        rel: s.rel.clone(),
        kind: s.kind,
        size: if action.is_transfer() { s.fp.size } else { 0 },
        action,
        reason: reason.into(),
        expected_src: (s.kind == EntryKind::File).then_some(s.fp),
        expected_dest: observed.clone(),
    }
}

pub fn plan(input: &PlanInput, source_generation: i64) -> Plan {
    let caps = input.caps;
    let mut items: Vec<PlannedItem> = Vec::new();

    // Names that would land on the same destination name; the first (path order) wins.
    let mut owner: HashMap<String, &RelPath> = HashMap::new();
    let mut colliding: BTreeMap<&RelPath, String> = BTreeMap::new();
    for e in &input.source.entries {
        let key = collision_key(&e.rel, caps.case_insensitive);
        match owner.get(&key) {
            Some(first) => {
                colliding.insert(&e.rel, first.display());
            }
            None => {
                owner.insert(key, &e.rel);
            }
        }
    }

    let source_by_rel: HashMap<&RelPath, &SourceEntry> =
        input.source.entries.iter().map(|e| (&e.rel, e)).collect();

    let not_observed = Observed::Unreadable("not observed".into());
    for s in &input.source.entries {
        let observed = input.observed.get(&s.rel).unwrap_or(&not_observed);
        if let Some(block) = block_reason(s, input, &colliding) {
            let reason = block.describe();
            items.push(item(s, Action::Blocked(block), observed, reason));
            continue;
        }
        if let Observed::Unreadable(e) = observed {
            let b = BlockReason::DestinationUnreadable(e.clone());
            let reason = b.describe();
            items.push(item(s, Action::Blocked(b), observed, reason));
            continue;
        }
        if let Some(it) = decide(s, input.manifest.get(&s.rel), observed, input) {
            items.push(it);
        }
    }

    // Managed files that left the source. Only meaningful where the scan could read.
    for (rel, m) in input.manifest {
        if source_by_rel.contains_key(rel) || m.kind != EntryKind::File {
            continue;
        }
        if !input.source.covers(rel)
            || matches!(m.status, EntryStatus::Orphaned | EntryStatus::Ignored)
        {
            continue;
        }
        items.push(PlannedItem {
            rel: rel.clone(),
            kind: EntryKind::File,
            action: Action::SourceDeleted,
            size: 0,
            reason: "no longer in the source; the copy on this drive is kept".into(),
            expected_src: None,
            expected_dest: Observed::Missing,
        });
    }

    items.sort_by(|a, b| a.rel.components().cmp(b.rel.components()));
    apply_space_limit(&mut items, input);
    Plan {
        source_generation,
        source_scan_complete: input.source.complete,
        items,
    }
}

fn block_reason(
    s: &SourceEntry,
    input: &PlanInput,
    colliding: &BTreeMap<&RelPath, String>,
) -> Option<BlockReason> {
    let caps = input.caps;
    if s.rel.is_reserved() {
        return Some(BlockReason::ReservedName);
    }
    // A child of a blocked name would silently merge into the other folder.
    for (blocked, other) in colliding {
        if s.rel.starts_with(blocked) {
            return Some(BlockReason::NameCollision(other.clone()));
        }
    }
    for comp in s.rel.components() {
        if let Some(why) = caps.name_problem(comp) {
            return Some(BlockReason::BadName(why));
        }
    }
    if s.kind == EntryKind::File
        && let Some(limit) = caps.max_file_size
        && s.fp.size > limit
    {
        return Some(BlockReason::TooLarge { limit });
    }
    None
}

fn decide(
    s: &SourceEntry,
    m: Option<&DestEntry>,
    o: &Observed,
    input: &PlanInput,
) -> Option<PlannedItem> {
    let caps = input.caps;
    // Ignored: stay quiet until the source content moves on from what the user saw.
    let m = match m {
        Some(m) if m.status == EntryStatus::Ignored => {
            let unchanged = !source_changed(s, m);
            if unchanged {
                return None;
            }
            None
        }
        other => other,
    };

    match (s.kind, m) {
        // ---------------------------------------------------------- untracked
        (EntryKind::Dir, None) => Some(match o {
            Observed::Missing | Observed::Dir => item(s, Action::MakeDir, o, "new folder"),
            _ => item(
                s,
                Action::Collision(CollisionKind::ExistingDirOrOther),
                o,
                "a file already exists where this folder should go",
            ),
        }),
        (EntryKind::File, None) => Some(match o {
            Observed::Missing => item(s, Action::Copy, o, "new file"),
            Observed::File(_) => {
                let kind = if is_case_only_rename(s, input, caps) {
                    CollisionKind::CaseOnlyRename
                } else {
                    CollisionKind::ExistingFile
                };
                let why = match kind {
                    CollisionKind::CaseOnlyRename => {
                        "renamed in the source by letter case only; this drive cannot hold both names"
                    }
                    _ => {
                        "a file TwinSync did not copy already exists here; it will not be overwritten"
                    }
                };
                item(s, Action::Collision(kind), o, why)
            }
            _ => item(
                s,
                Action::Collision(CollisionKind::ExistingDirOrOther),
                o,
                "a folder or special file already exists where this file should go",
            ),
        }),
        // ------------------------------------------------------------ managed
        (EntryKind::Dir, Some(_)) => match o {
            Observed::Dir => None,
            Observed::Missing => Some(item(
                s,
                Action::MakeDir,
                o,
                "folder was removed from the drive; recreating it",
            )),
            _ => Some(item(
                s,
                Action::Collision(CollisionKind::ExistingDirOrOther),
                o,
                "something other than a folder is at this path",
            )),
        },
        (EntryKind::File, Some(m)) => decide_managed_file(s, m, o, caps),
    }
}

fn decide_managed_file(
    s: &SourceEntry,
    m: &DestEntry,
    o: &Observed,
    caps: &FsCapabilities,
) -> Option<PlannedItem> {
    // The user approved overwriting whatever is there (previous version is kept).
    if m.status == EntryStatus::ReplaceApproved {
        return Some(match o {
            Observed::Missing => item(s, Action::Copy, o, "approved; nothing exists at the target"),
            Observed::File(_) => item(
                s,
                Action::Update,
                o,
                "replacement approved; existing file goes to version history",
            ),
            _ => item(
                s,
                Action::Collision(CollisionKind::ExistingDirOrOther),
                o,
                "a folder or special file is at this path",
            ),
        });
    }
    match o {
        Observed::Missing => Some(item(
            s,
            Action::ExternallyDeleted,
            o,
            "the copy was deleted from the drive outside TwinSync",
        )),
        Observed::Dir | Observed::Other => Some(item(
            s,
            Action::Conflict,
            o,
            "something other than the copied file is at this path",
        )),
        Observed::File(now) => {
            let dest_unchanged = m
                .dest_fp
                .as_ref()
                .is_some_and(|f| now.same_as(f, caps.inode_stable));
            if !dest_unchanged {
                return Some(item(
                    s,
                    Action::CheckDestination,
                    o,
                    "the copy's size or date changed on the drive; checking whether its content changed",
                ));
            }
            if m.status == EntryStatus::Conflict {
                return Some(item(
                    s,
                    Action::Conflict,
                    o,
                    "the copy on the drive was edited outside TwinSync",
                ));
            }
            source_changed(s, m).then(|| item(s, Action::Update, o, "source changed"))
        }
        Observed::Unreadable(_) => None, // handled before decide()
    }
}

fn is_case_only_rename(s: &SourceEntry, input: &PlanInput, caps: &FsCapabilities) -> bool {
    if !caps.case_insensitive {
        return false;
    }
    let key = collision_key(&s.rel, true);
    input.manifest.iter().any(|(rel, m)| {
        m.kind == EntryKind::File
            && *rel != s.rel
            && collision_key(rel, true) == key
            && !input.source.entries.iter().any(|e| &e.rel == rel)
    })
}

/// Blocks transfers that would not fit, in path order, so the first items
/// still run. `free_bytes = None` (never measured) blocks nothing: the
/// executor re-checks space before every batch regardless.
fn apply_space_limit(items: &mut [PlannedItem], input: &PlanInput) {
    let Some(free) = input.free_bytes else { return };
    let budget = free.saturating_sub(input.reserve_bytes);
    let mut used = 0u64;
    for it in items.iter_mut().filter(|i| i.action.is_transfer()) {
        if used.saturating_add(it.size) > budget {
            it.action = Action::Blocked(BlockReason::InsufficientSpace);
            it.reason = BlockReason::InsufficientSpace.describe();
            it.size = 0;
        } else {
            used += it.size;
        }
    }
}
