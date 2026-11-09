use std::collections::HashMap;
use twinsync::domain::*;
use twinsync::planner::*;
use twinsync::scanner::ScanResult;
use twinsync::storage::{DestEntry, EntryStatus, SourceEntry};

fn rel(s: &str) -> RelPath {
    RelPath::from_bytes(s.as_bytes()).unwrap()
}

fn fp(size: u64, mtime: i64) -> Fingerprint {
    Fingerprint {
        size,
        mtime_ns: mtime,
        inode: 1,
    }
}

fn src(path: &str, f: Fingerprint) -> SourceEntry {
    SourceEntry {
        rel: rel(path),
        kind: EntryKind::File,
        fp: f,
        hash: None,
    }
}

fn dir(path: &str) -> SourceEntry {
    SourceEntry {
        rel: rel(path),
        kind: EntryKind::Dir,
        fp: fp(0, 0),
        hash: None,
    }
}

fn scan(entries: Vec<SourceEntry>) -> ScanResult {
    ScanResult {
        entries,
        complete: true,
        ..Default::default()
    }
}

/// A file TwinSync copied: source was `src_fp`, destination copy is `dest_fp`.
fn managed(path: &str, src_fp: Fingerprint, dest_fp: Fingerprint) -> (RelPath, DestEntry) {
    (
        rel(path),
        DestEntry {
            rel: rel(path),
            kind: EntryKind::File,
            status: EntryStatus::Synced,
            synced_hash: Some(Hash([9; 32])),
            dest_fp: Some(dest_fp),
            src_fp: Some(src_fp),
            synced_at: Some(1),
            verified_at: None,
        },
    )
}

fn apfs() -> FsCapabilities {
    FsCapabilities::for_fs_type("apfs")
}

struct Case {
    source: ScanResult,
    manifest: HashMap<RelPath, DestEntry>,
    observed: HashMap<RelPath, Observed>,
    caps: FsCapabilities,
    free: Option<u64>,
}

impl Case {
    fn new(entries: Vec<SourceEntry>) -> Self {
        Case {
            source: scan(entries),
            manifest: HashMap::new(),
            observed: HashMap::new(),
            caps: apfs(),
            free: None,
        }
    }
    fn managed(mut self, m: (RelPath, DestEntry)) -> Self {
        self.manifest.insert(m.0, m.1);
        self
    }
    fn observe(mut self, path: &str, o: Observed) -> Self {
        self.observed.insert(rel(path), o);
        self
    }
    fn plan(&self) -> Plan {
        plan(
            &PlanInput {
                source: &self.source,
                manifest: &self.manifest,
                observed: &self.observed,
                caps: &self.caps,
                free_bytes: self.free,
                reserve_bytes: 0,
            },
            7,
        )
    }
    fn action(&self, path: &str) -> Option<Action> {
        self.plan()
            .items
            .into_iter()
            .find(|i| i.rel == rel(path))
            .map(|i| i.action)
    }
}

#[test]
fn new_file_with_free_target_is_copied() {
    let c = Case::new(vec![src("a.txt", fp(10, 1))]).observe("a.txt", Observed::Missing);
    assert_eq!(c.action("a.txt"), Some(Action::Copy));
    let p = c.plan();
    assert_eq!(p.source_generation, 7);
    assert_eq!(p.items[0].expected_src, Some(fp(10, 1)));
    assert_eq!(p.items[0].expected_dest, Observed::Missing);
    assert_eq!(p.summary().bytes_to_write, 10);
}

#[test]
fn existing_unmanaged_file_is_a_collision_never_overwritten() {
    let c = Case::new(vec![src("a.txt", fp(10, 1))]).observe("a.txt", Observed::File(fp(3, 3)));
    assert_eq!(
        c.action("a.txt"),
        Some(Action::Collision(CollisionKind::ExistingFile))
    );
    assert!(!c.plan().items[0].action.is_transfer());
}

#[test]
fn folder_collides_with_existing_file() {
    let c = Case::new(vec![dir("d")]).observe("d", Observed::File(fp(1, 1)));
    assert_eq!(
        c.action("d"),
        Some(Action::Collision(CollisionKind::ExistingDirOrOther))
    );
}

#[test]
fn unchanged_managed_file_needs_nothing() {
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(managed("a", fp(10, 1), fp(10, 5)))
        .observe("a", Observed::File(fp(10, 5)));
    assert!(c.plan().is_empty());
}

#[test]
fn changed_source_with_untouched_destination_is_an_update() {
    let c = Case::new(vec![src("a", fp(11, 2))])
        .managed(managed("a", fp(10, 1), fp(10, 5)))
        .observe("a", Observed::File(fp(10, 5)));
    assert_eq!(c.action("a"), Some(Action::Update));
}

#[test]
fn touched_source_with_identical_hash_is_not_an_update() {
    let mut s = src("a", fp(10, 99));
    s.hash = Some(Hash([9; 32])); // same as synced_hash
    let c = Case::new(vec![s])
        .managed(managed("a", fp(10, 1), fp(10, 5)))
        .observe("a", Observed::File(fp(10, 5)));
    assert!(c.plan().is_empty());
}

#[test]
fn changed_destination_fingerprint_is_checked_not_assumed_edited() {
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(managed("a", fp(10, 1), fp(10, 5)))
        .observe("a", Observed::File(fp(10, 6)));
    assert_eq!(c.action("a"), Some(Action::CheckDestination));
}

#[test]
fn known_conflict_is_reported_without_rehashing() {
    let mut m = managed("a", fp(10, 1), fp(10, 6));
    m.1.status = EntryStatus::Conflict;
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(m)
        .observe("a", Observed::File(fp(10, 6)));
    assert_eq!(c.action("a"), Some(Action::Conflict));
}

#[test]
fn destination_changed_again_after_conflict_is_rechecked() {
    let mut m = managed("a", fp(10, 1), fp(10, 6));
    m.1.status = EntryStatus::Conflict;
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(m)
        .observe("a", Observed::File(fp(10, 7)));
    assert_eq!(c.action("a"), Some(Action::CheckDestination));
}

#[test]
fn copy_deleted_from_drive_waits_for_the_user() {
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(managed("a", fp(10, 1), fp(10, 5)))
        .observe("a", Observed::Missing);
    assert_eq!(c.action("a"), Some(Action::ExternallyDeleted));
}

#[test]
fn file_replaced_by_folder_on_drive_is_a_conflict() {
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(managed("a", fp(10, 1), fp(10, 5)))
        .observe("a", Observed::Dir);
    assert_eq!(c.action("a"), Some(Action::Conflict));
}

#[test]
fn source_deleted_keeps_the_copy_and_is_surfaced() {
    let c = Case::new(vec![]).managed(managed("gone", fp(10, 1), fp(10, 5)));
    assert_eq!(c.action("gone"), Some(Action::SourceDeleted));
    // there is no delete action in the vocabulary at all
    assert!(!c.plan().items[0].action.is_transfer());
}

#[test]
fn acknowledged_source_deletion_stays_quiet() {
    let mut m = managed("gone", fp(10, 1), fp(10, 5));
    m.1.status = EntryStatus::Orphaned;
    assert!(Case::new(vec![]).managed(m).plan().is_empty());
}

#[test]
fn rename_is_new_file_plus_removed_original_with_nothing_deleted() {
    let c = Case::new(vec![src("new-name", fp(10, 1))])
        .managed(managed("old-name", fp(10, 1), fp(10, 5)))
        .observe("new-name", Observed::Missing);
    assert_eq!(c.action("new-name"), Some(Action::Copy));
    assert_eq!(c.action("old-name"), Some(Action::SourceDeleted));
}

#[test]
fn incomplete_scan_never_turns_unreadable_paths_into_deletions() {
    let mut c = Case::new(vec![])
        .managed(managed("locked/inside", fp(1, 1), fp(1, 1)))
        .managed(managed("readable/gone", fp(1, 1), fp(1, 1)));
    c.source.complete = false;
    c.source.unreadable = vec![rel("locked")];
    assert_eq!(c.action("locked/inside"), None, "unknown, not deleted");
    assert_eq!(
        c.action("readable/gone"),
        Some(Action::SourceDeleted),
        "paths outside the unreadable prefix were fully read"
    );
    assert!(!c.plan().source_scan_complete);
}

#[test]
fn fat32_file_size_limit_blocks_before_copying() {
    let mut c = Case::new(vec![
        src("big.bin", fp(5 * 1024 * 1024 * 1024, 1)),
        src("ok.bin", fp(10, 1)),
    ])
    .observe("big.bin", Observed::Missing)
    .observe("ok.bin", Observed::Missing);
    c.caps = FsCapabilities::for_fs_type("msdos");
    assert!(matches!(
        c.action("big.bin"),
        Some(Action::Blocked(BlockReason::TooLarge { .. }))
    ));
    assert_eq!(c.action("ok.bin"), Some(Action::Copy));
}

#[test]
fn names_fat_cannot_store_are_blocked_not_renamed() {
    let mut c = Case::new(vec![src("a:b.txt", fp(1, 1)), src("fine.txt", fp(1, 1))])
        .observe("a:b.txt", Observed::Missing)
        .observe("fine.txt", Observed::Missing);
    c.caps = FsCapabilities::for_fs_type("exfat");
    assert!(matches!(
        c.action("a:b.txt"),
        Some(Action::Blocked(BlockReason::BadName(_)))
    ));
    assert_eq!(c.action("fine.txt"), Some(Action::Copy));
    // the same name is fine on APFS
    let mut c2 = Case::new(vec![src("a:b.txt", fp(1, 1))]).observe("a:b.txt", Observed::Missing);
    c2.caps = apfs();
    assert_eq!(c2.action("a:b.txt"), Some(Action::Copy));
}

#[test]
fn reserved_twinsync_name_is_never_synced() {
    let c = Case::new(vec![dir(".twinsync"), src(".twinsync/x", fp(1, 1))])
        .observe(".twinsync", Observed::Missing)
        .observe(".twinsync/x", Observed::Missing);
    assert_eq!(
        c.action(".twinsync"),
        Some(Action::Blocked(BlockReason::ReservedName))
    );
    assert_eq!(
        c.action(".twinsync/x"),
        Some(Action::Blocked(BlockReason::ReservedName))
    );
}

#[test]
fn case_collision_blocks_the_second_name_and_children() {
    let c = Case::new(vec![
        dir("Photos"),
        dir("photos"),
        src("Photos/a.jpg", fp(1, 1)),
        src("photos/b.jpg", fp(1, 1)),
    ])
    .observe("Photos", Observed::Missing)
    .observe("photos", Observed::Missing)
    .observe("Photos/a.jpg", Observed::Missing)
    .observe("photos/b.jpg", Observed::Missing);
    assert_eq!(c.action("Photos"), Some(Action::MakeDir));
    assert!(matches!(
        c.action("photos"),
        Some(Action::Blocked(BlockReason::NameCollision(_)))
    ));
    assert_eq!(c.action("Photos/a.jpg"), Some(Action::Copy));
    assert!(
        matches!(
            c.action("photos/b.jpg"),
            Some(Action::Blocked(BlockReason::NameCollision(_)))
        ),
        "children of a blocked name must not merge into the other folder"
    );
}

#[test]
fn case_sensitive_destination_keeps_both_names() {
    let mut c = Case::new(vec![src("a.txt", fp(1, 1)), src("A.txt", fp(1, 1))])
        .observe("a.txt", Observed::Missing)
        .observe("A.txt", Observed::Missing);
    c.caps.case_insensitive = false;
    assert_eq!(c.action("a.txt"), Some(Action::Copy));
    assert_eq!(c.action("A.txt"), Some(Action::Copy));
}

#[test]
fn unicode_nfc_and_nfd_names_collide() {
    let nfc = "caf\u{e9}.txt";
    let nfd = "cafe\u{301}.txt";
    let mut c = Case::new(vec![src(nfc, fp(1, 1)), src(nfd, fp(1, 1))])
        .observe(nfc, Observed::Missing)
        .observe(nfd, Observed::Missing);
    c.caps.case_insensitive = false; // normalization insensitivity applies regardless
    let blocked = c
        .plan()
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::Blocked(BlockReason::NameCollision(_))))
        .count();
    assert_eq!(blocked, 1);
}

#[test]
fn case_only_rename_on_case_insensitive_drive_is_called_out() {
    // source: foo -> Foo. The drive still holds our managed "foo".
    let c = Case::new(vec![src("Foo", fp(10, 1))])
        .managed(managed("foo", fp(10, 1), fp(10, 5)))
        .observe("Foo", Observed::File(fp(10, 5)));
    assert_eq!(
        c.action("Foo"),
        Some(Action::Collision(CollisionKind::CaseOnlyRename))
    );
    assert_eq!(c.action("foo"), Some(Action::SourceDeleted));
}

#[test]
fn insufficient_space_blocks_only_what_does_not_fit() {
    let mut c = Case::new(vec![
        src("a", fp(60, 1)),
        src("b", fp(60, 1)),
        src("c", fp(30, 1)),
    ])
    .observe("a", Observed::Missing)
    .observe("b", Observed::Missing)
    .observe("c", Observed::Missing);
    c.free = Some(100);
    assert_eq!(c.action("a"), Some(Action::Copy));
    assert_eq!(
        c.action("b"),
        Some(Action::Blocked(BlockReason::InsufficientSpace))
    );
    assert_eq!(
        c.action("c"),
        Some(Action::Copy),
        "smaller later file still fits"
    );
}

#[test]
fn unreadable_or_unobserved_destination_is_blocked_not_guessed() {
    let c = Case::new(vec![src("a", fp(1, 1)), src("b", fp(1, 1))])
        .observe("a", Observed::Unreadable("EIO".into()));
    assert!(matches!(
        c.action("a"),
        Some(Action::Blocked(BlockReason::DestinationUnreadable(_)))
    ));
    assert!(matches!(
        c.action("b"),
        Some(Action::Blocked(BlockReason::DestinationUnreadable(_)))
    ));
}

#[test]
fn ignored_entry_stays_quiet_until_the_source_changes_again() {
    let mut m = managed("a", fp(10, 1), fp(10, 5));
    m.1.status = EntryStatus::Ignored;
    let quiet = Case::new(vec![src("a", fp(10, 1))])
        .managed(m.clone())
        .observe("a", Observed::File(fp(99, 9)));
    assert!(quiet.plan().is_empty());
    let changed = Case::new(vec![src("a", fp(11, 2))])
        .managed(m)
        .observe("a", Observed::File(fp(99, 9)));
    assert_eq!(
        changed.action("a"),
        Some(Action::Collision(CollisionKind::ExistingFile)),
        "source moved on: surfaces again instead of overwriting"
    );
}

#[test]
fn approved_replacement_updates_and_missing_target_just_copies() {
    let mut m = managed("a", fp(10, 1), fp(10, 5));
    m.1.status = EntryStatus::ReplaceApproved;
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(m.clone())
        .observe("a", Observed::File(fp(3, 3)));
    assert_eq!(c.action("a"), Some(Action::Update));
    let c = Case::new(vec![src("a", fp(10, 1))])
        .managed(m)
        .observe("a", Observed::Missing);
    assert_eq!(c.action("a"), Some(Action::Copy));
}

#[test]
fn fat_inode_changes_across_remount_do_not_trigger_checks() {
    let mut c = Case::new(vec![src("a", fp(10, 1))])
        .managed(managed("a", fp(10, 1), fp(10, 5)))
        .observe(
            "a",
            Observed::File(Fingerprint {
                size: 10,
                mtime_ns: 5,
                inode: 999,
            }),
        );
    c.caps = FsCapabilities::for_fs_type("exfat");
    assert!(!c.caps.inode_stable);
    assert!(c.plan().is_empty(), "no mass re-hash after a replug");
    c.caps = apfs();
    assert_eq!(c.action("a"), Some(Action::CheckDestination));
}

#[test]
fn items_are_grouped_by_directory_and_parents_come_first() {
    let c = Case::new(vec![
        src("b/z.txt", fp(1, 1)),
        src("a.txt", fp(1, 1)),
        dir("b"),
        src("b/a.txt", fp(1, 1)),
    ])
    .observe("b/z.txt", Observed::Missing)
    .observe("a.txt", Observed::Missing)
    .observe("b", Observed::Missing)
    .observe("b/a.txt", Observed::Missing);
    let order: Vec<String> = c.plan().items.iter().map(|i| i.rel.display()).collect();
    assert_eq!(order, ["a.txt", "b", "b/a.txt", "b/z.txt"]);
}

#[test]
fn summary_counts_actions_and_bytes() {
    let c = Case::new(vec![
        src("new", fp(10, 1)),
        src("upd", fp(20, 2)),
        src("clash", fp(5, 1)),
    ])
    .managed(managed("upd", fp(20, 1), fp(20, 5)))
    .observe("new", Observed::Missing)
    .observe("upd", Observed::File(fp(20, 5)))
    .observe("clash", Observed::File(fp(1, 1)));
    let s = c.plan().summary();
    assert_eq!((s.copies, s.updates, s.attention), (1, 1, 1));
    assert_eq!(s.bytes_to_write, 30);
}
