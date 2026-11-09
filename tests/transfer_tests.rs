mod common;

use common::*;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};
use twinsync::domain::*;
use twinsync::errors::Error;
use twinsync::planner::*;
use twinsync::storage::*;
use twinsync::transfer::fsops::DirFd;
use twinsync::transfer::*;

fn bump_mtime(path: &std::path::Path, secs: u64) {
    let f = OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(secs))
        .unwrap();
}

fn blake(data: &[u8]) -> Hash {
    Hash(*blake3::hash(data).as_bytes())
}

#[test]
fn copies_new_files_to_both_destinations_independently() {
    let fx = Fixture::with_destinations(2);
    fx.write_src("a.txt", b"alpha");
    fx.write_src("dir/b.txt", b"bravo");
    fx.sync_ok(0);
    fx.sync_ok(1);
    for d in 0..2 {
        assert_eq!(fx.read_dst(d, "a.txt").unwrap(), b"alpha");
        assert_eq!(fx.read_dst(d, "dir/b.txt").unwrap(), b"bravo");
        let e = &fx.entries(d)[&rel("a.txt")];
        assert_eq!(e.status, EntryStatus::Synced);
        assert_eq!(e.synced_hash, Some(blake(b"alpha")));
    }
    // modification time is carried over
    let s = fs::metadata(fx.src_dir.join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let t = fs::metadata(fx.dst_path(0, "a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(s, t);
    // nothing left to do, nothing left behind
    assert!(fx.plan(0).is_empty() && fx.plan(1).is_empty());
    assert!(fx.tmp_files(0).is_empty());
}

#[test]
fn one_drive_offline_does_not_stop_the_other() {
    let fx = Fixture::with_destinations(2);
    fx.write_src("a.txt", b"alpha");
    fs::remove_dir_all(&fx.dests[0].dir).unwrap(); // drive A unplugged
    assert!(fx.try_with(0, |_| ()).is_err());
    assert!(
        !fx.dests[0].dir.exists(),
        "an absent drive must not be re-created"
    );
    fx.sync_ok(1);
    assert_eq!(fx.read_dst(1, "a.txt").unwrap(), b"alpha");
}

#[test]
fn update_keeps_the_previous_version_in_history() {
    let fx = Fixture::new();
    fx.write_src("doc.txt", b"v1");
    fx.sync_ok(0);
    fx.write_src("doc.txt", b"version two");
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "doc.txt").unwrap(), b"version two");
    let versions = fx.version_files(0);
    assert_eq!(versions.len(), 1);
    assert!(versions[0].0.ends_with("/doc.txt"));
    assert_eq!(versions[0].1, b"v1");
    let rows = fx.db.versions(&fx.dests[0].id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].size, 2);
    assert_eq!(rows[0].hash, Some(blake(b"v1")));
    assert_eq!(
        fx.entries(0)[&rel("doc.txt")].synced_hash,
        Some(blake(b"version two"))
    );
    assert!(fx.tmp_files(0).is_empty());
    assert_eq!(fx.unfinished(0), 0);
}

#[test]
fn version_history_is_never_synced_or_planned() {
    let fx = Fixture::new();
    fx.write_src("doc.txt", b"v1");
    fx.sync_ok(0);
    fx.write_src("doc.txt", b"v2 longer");
    fx.sync_ok(0);
    assert!(fx.plan(0).is_empty());
    assert!(!fx.entries(0).keys().any(|k| k.is_reserved()));
}

#[test]
fn touched_source_with_same_content_does_not_recopy() {
    let fx = Fixture::new();
    fx.write_src("a.txt", b"hello");
    fx.sync_ok(0);
    bump_mtime(&fx.src_dir.join("a.txt"), 30);
    let plan = fx.plan(0);
    assert_eq!(plan.items[0].action, Action::Update);
    let out = fx.run(0, &plan);
    assert_eq!(*out[0].1.as_ref().unwrap(), ItemOutcome::Unchanged);
    assert!(
        fx.version_files(0).is_empty(),
        "no version for identical content"
    );
    assert!(fx.plan(0).is_empty(), "bookkeeping refreshed");
}

#[test]
fn source_deletion_keeps_the_copy_and_is_surfaced() {
    let fx = Fixture::new();
    fx.write_src("a.txt", b"keep me");
    fx.sync_ok(0);
    fs::remove_file(fx.src_dir.join("a.txt")).unwrap();
    assert_eq!(fx.action(0, "a.txt"), Some(Action::SourceDeleted));
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "a.txt").unwrap(), b"keep me");
}

#[test]
fn rename_in_source_copies_new_name_and_keeps_old_copy() {
    let fx = Fixture::new();
    fx.write_src("old.txt", b"data");
    fx.sync_ok(0);
    fs::rename(fx.src_dir.join("old.txt"), fx.src_dir.join("new.txt")).unwrap();
    assert_eq!(fx.action(0, "new.txt"), Some(Action::Copy));
    assert_eq!(fx.action(0, "old.txt"), Some(Action::SourceDeleted));
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "old.txt").unwrap(), b"data");
    assert_eq!(fx.read_dst(0, "new.txt").unwrap(), b"data");
}

#[test]
fn files_the_app_does_not_manage_are_never_modified() {
    let fx = Fixture::new();
    fs::write(fx.dst_path(0, "stranger.txt"), b"not yours").unwrap();
    fs::create_dir(fx.dst_path(0, "their-folder")).unwrap();
    fs::write(fx.dst_path(0, "their-folder/x"), b"x").unwrap();
    let before = fs::metadata(fx.dst_path(0, "stranger.txt"))
        .unwrap()
        .modified()
        .unwrap();
    fx.write_src("a.txt", b"mine");
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "stranger.txt").unwrap(), b"not yours");
    assert_eq!(fx.read_dst(0, "their-folder/x").unwrap(), b"x");
    assert_eq!(
        fs::metadata(fx.dst_path(0, "stranger.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    assert!(!fx.entries(0).contains_key(&rel("stranger.txt")));
}

#[test]
fn existing_file_at_target_is_a_collision_and_survives() {
    let fx = Fixture::new();
    fs::write(fx.dst_path(0, "a.txt"), b"theirs").unwrap();
    fx.write_src("a.txt", b"mine");
    assert!(matches!(fx.action(0, "a.txt"), Some(Action::Collision(_))));
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "a.txt").unwrap(), b"theirs");
    assert!(fx.entries(0).is_empty());
    assert!(fx.version_files(0).is_empty());
}

#[test]
fn approved_replacement_overwrites_but_keeps_the_original_as_a_version() {
    let fx = Fixture::new();
    fs::write(fx.dst_path(0, "a.txt"), b"theirs").unwrap();
    fx.write_src("a.txt", b"mine");
    fx.db
        .upsert_dest_entry(
            &fx.dests[0].id,
            &DestEntry {
                rel: rel("a.txt"),
                kind: EntryKind::File,
                status: EntryStatus::ReplaceApproved,
                synced_hash: None,
                dest_fp: None,
                src_fp: None,
                synced_at: None,
                verified_at: None,
            },
        )
        .unwrap();
    assert_eq!(fx.action(0, "a.txt"), Some(Action::Update));
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "a.txt").unwrap(), b"mine");
    assert_eq!(fx.version_files(0)[0].1, b"theirs");
    assert_eq!(fx.entries(0)[&rel("a.txt")].status, EntryStatus::Synced);
}

#[test]
fn identical_existing_file_can_be_adopted_but_a_different_one_cannot() {
    let fx = Fixture::new();
    fx.write_src("same.txt", b"identical");
    fx.write_src("diff.txt", b"source side");
    fs::write(fx.dst_path(0, "same.txt"), b"identical").unwrap();
    fs::write(fx.dst_path(0, "diff.txt"), b"other side").unwrap();
    fx.with(0, |ctx| {
        assert!(adopt_if_identical(ctx, &rel("same.txt")).unwrap());
        assert!(!adopt_if_identical(ctx, &rel("diff.txt")).unwrap());
    });
    assert!(fx.entries(0).contains_key(&rel("same.txt")));
    assert!(!fx.entries(0).contains_key(&rel("diff.txt")));
    assert_eq!(fx.read_dst(0, "diff.txt").unwrap(), b"other side");
    assert_eq!(fx.action(0, "same.txt"), None);
}

#[test]
fn external_edit_becomes_a_conflict_and_blocks_overwrite() {
    let fx = Fixture::new();
    fx.write_src("a.txt", b"original");
    fx.sync_ok(0);
    fs::write(fx.dst_path(0, "a.txt"), b"edited on the drive").unwrap();
    assert_eq!(fx.action(0, "a.txt"), Some(Action::CheckDestination));
    let r = fx.with(0, |ctx| check_destination(ctx, &rel("a.txt")).unwrap());
    assert_eq!(r, CheckOutcome::Modified);
    assert_eq!(fx.entries(0)[&rel("a.txt")].status, EntryStatus::Conflict);
    // even if the source changes too, nothing is overwritten
    fx.write_src("a.txt", b"source moved on");
    assert_eq!(fx.action(0, "a.txt"), Some(Action::Conflict));
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "a.txt").unwrap(), b"edited on the drive");
    assert!(fx.version_files(0).is_empty());
}

#[test]
fn touching_a_copy_on_the_drive_is_not_a_conflict() {
    let fx = Fixture::new();
    fx.write_src("a.txt", b"same bytes");
    fx.sync_ok(0);
    bump_mtime(&fx.dst_path(0, "a.txt"), 60);
    assert_eq!(fx.action(0, "a.txt"), Some(Action::CheckDestination));
    let r = fx.with(0, |ctx| check_destination(ctx, &rel("a.txt")).unwrap());
    assert_eq!(r, CheckOutcome::Unchanged);
    assert!(fx.plan(0).is_empty());
}

#[test]
fn copy_deleted_from_the_drive_is_not_silently_recreated() {
    let fx = Fixture::new();
    fx.write_src("a.txt", b"data");
    fx.sync_ok(0);
    fs::remove_file(fx.dst_path(0, "a.txt")).unwrap();
    assert_eq!(fx.action(0, "a.txt"), Some(Action::ExternallyDeleted));
    fx.sync_ok(0);
    assert!(fx.read_dst(0, "a.txt").is_none());
}

#[test]
fn folders_including_empty_ones_are_created() {
    let fx = Fixture::new();
    fs::create_dir(fx.src_dir.join("empty")).unwrap();
    fx.write_src("d/x.txt", b"x");
    fx.sync_ok(0);
    assert!(fx.dst_path(0, "empty").is_dir());
    assert_eq!(fx.read_dst(0, "d/x.txt").unwrap(), b"x");
    assert_eq!(fx.entries(0)[&rel("empty")].kind, EntryKind::Dir);
    assert!(fx.plan(0).is_empty());
}

#[test]
fn large_file_streams_in_chunks_and_verifies() {
    let mut fx = Fixture::new();
    fx.chunk = 4096;
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    fx.write_src("big.bin", &data);
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "big.bin").unwrap(), data);
}

// ------------------------------------------------------------------ races

#[test]
fn source_changing_during_copy_defers_and_leaves_nothing_behind() {
    let mut fx = Fixture::new();
    fx.chunk = 4096;
    fx.write_src("big", &vec![b'a'; 20_000]);
    let p = fx.src_dir.join("big");
    fx.set_fault(move |pt| {
        if pt == FaultPoint::WriteChunk(1) {
            let mut f = OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(b"x").unwrap();
        }
        None
    });
    let out = fx.sync(0);
    assert!(matches!(out[0].1, Err(Error::SourceChanged)));
    assert!(fx.read_dst(0, "big").is_none());
    assert!(fx.tmp_files(0).is_empty());
    assert_eq!(fx.op_states(0), ["aborted"]);
    // once the source settles, the next attempt succeeds
    fx.clear_fault();
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "big").unwrap().len(), 20_001);
}

#[test]
fn destination_edited_during_update_becomes_a_conflict_not_an_overwrite() {
    let fx = Fixture::new();
    fx.write_src("a.txt", b"v1");
    fx.sync_ok(0);
    fx.write_src("a.txt", b"version 2");
    let target = fx.dst_path(0, "a.txt");
    fx.set_fault(move |pt| {
        if pt == FaultPoint::AfterVerifyRead {
            fs::write(&target, b"someone else").unwrap();
        }
        None
    });
    let out = fx.sync(0);
    assert!(matches!(out[0].1, Err(Error::DestChanged(_))));
    assert_eq!(fx.read_dst(0, "a.txt").unwrap(), b"someone else");
    assert!(fx.version_files(0).is_empty());
    assert!(fx.tmp_files(0).is_empty());
}

#[test]
fn file_appearing_at_a_new_target_during_copy_is_not_overwritten() {
    let fx = Fixture::new();
    fx.write_src("new.txt", b"mine");
    let target = fx.dst_path(0, "new.txt");
    fx.set_fault(move |pt| {
        if pt == FaultPoint::AfterVerifyRead {
            fs::write(&target, b"theirs").unwrap();
        }
        None
    });
    let out = fx.sync(0);
    assert!(matches!(out[0].1, Err(Error::DestChanged(_))));
    assert_eq!(fx.read_dst(0, "new.txt").unwrap(), b"theirs");
}

#[test]
fn stale_preview_is_rejected_without_writing() {
    let fx = Fixture::new();
    fx.write_src("a.txt", b"v1");
    fx.write_src("b.txt", b"b1");
    fx.sync_ok(0);
    fx.write_src("a.txt", b"v2 changed after sync");
    fx.write_src("b.txt", b"b2 changed after sync");
    let plan = fx.plan(0);
    // between preview and execute: source changes again and the drive file is edited
    fx.write_src("a.txt", b"v3, yet another change");
    fs::write(fx.dst_path(0, "b.txt"), b"edited meanwhile").unwrap();
    let out = fx.run(0, &plan);
    for (r, res) in &out {
        assert!(matches!(res, Ok(ItemOutcome::Stale(_))), "{r}: {res:?}");
    }
    assert_eq!(fx.read_dst(0, "a.txt").unwrap(), b"v1");
    assert_eq!(fx.read_dst(0, "b.txt").unwrap(), b"edited meanwhile");
    assert_eq!(
        fx.op_states(0).len(),
        2,
        "only the two original copies were journaled"
    );
}

#[test]
fn cancellation_stops_between_chunks_and_cleans_up() {
    let mut fx = Fixture::new();
    fx.chunk = 4096;
    fx.write_src("a", b"old");
    fx.sync_ok(0);
    fx.write_src("a", &vec![b'n'; 50_000]);
    let c = fx.cancel.clone();
    fx.set_fault(move |pt| {
        if pt == FaultPoint::WriteChunk(2) {
            c.store(true, Ordering::Relaxed);
        }
        None
    });
    let out = fx.sync(0);
    assert!(matches!(out[0].1, Err(Error::Cancelled)));
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"old");
    assert!(fx.tmp_files(0).is_empty());
    assert_eq!(fx.op_states(0), ["done", "aborted"]);
}

// ------------------------------------------------------- failures keep data

#[test]
fn write_failure_midway_keeps_the_existing_copy() {
    let mut fx = Fixture::new();
    fx.chunk = 4096;
    fx.write_src("a", b"old");
    fx.sync_ok(0);
    fx.write_src("a", &vec![b'n'; 30_000]);
    fx.set_fault(|pt| (pt == FaultPoint::WriteChunk(3)).then_some(Fault::Io(libc::ENOSPC)));
    let out = fx.sync(0);
    let err = out[0].1.as_ref().unwrap_err();
    assert!(err.is_no_space(), "{err}");
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"old");
    assert!(fx.tmp_files(0).is_empty());
    assert_eq!(fx.op_states(0), ["done", "failed"]);
    assert_eq!(fx.entries(0)[&rel("a")].synced_hash, Some(blake(b"old")));
}

#[test]
fn verification_failure_discards_the_temp_and_keeps_the_old_copy() {
    let fx = Fixture::new();
    fx.write_src("a", b"old");
    fx.sync_ok(0);
    fx.write_src("a", b"new content");
    let tmp = fx.tmp_dir(0);
    fx.set_fault(move |pt| {
        if pt == FaultPoint::AfterTempSynced {
            for e in fs::read_dir(&tmp).unwrap() {
                let p = e.unwrap().path();
                let mut f = OpenOptions::new().write(true).open(p).unwrap();
                f.write_all(b"X").unwrap(); // silent corruption of the temp copy
            }
        }
        None
    });
    let out = fx.sync(0);
    assert!(
        matches!(out[0].1, Err(Error::VerifyFailed(_))),
        "{:?}",
        out[0].1
    );
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"old");
    assert!(fx.tmp_files(0).is_empty());
    assert!(fx.version_files(0).is_empty());
}

#[test]
fn not_enough_space_is_caught_before_anything_is_written() {
    let mut fx = Fixture::new();
    fx.reserve = u64::MAX / 2;
    fx.write_src("a", b"data");
    let out = fx.sync(0);
    assert!(out[0].1.as_ref().unwrap_err().is_no_space());
    assert!(fx.read_dst(0, "a").is_none());
    assert!(fx.op_states(0).is_empty());
    assert!(fx.tmp_files(0).is_empty());
}

#[test]
fn failing_to_preserve_the_old_version_means_the_file_is_not_replaced() {
    let fx = Fixture::new();
    fx.write_src("a", b"old");
    fx.sync_ok(0);
    fx.write_src("a", b"newer content");
    let vdir = fx.versions_dir(0);
    fs::set_permissions(&vdir, fs::Permissions::from_mode(0o555)).unwrap();
    let out = fx.sync(0);
    fs::set_permissions(&vdir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(out[0].1.is_err(), "{:?}", out[0].1);
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"old");
    assert!(fx.tmp_files(0).is_empty());
    assert_eq!(fx.entries(0)[&rel("a")].synced_hash, Some(blake(b"old")));
}

#[test]
fn read_only_destination_is_refused_without_writing_anything() {
    let fx = Fixture::new();
    fx.write_src("a", b"data");
    fs::set_permissions(&fx.dests[0].dir, fs::Permissions::from_mode(0o555)).unwrap();
    let r = fx.try_with(0, |_| ());
    let listing: Vec<_> = fs::read_dir(&fx.dests[0].dir).unwrap().collect();
    fs::set_permissions(&fx.dests[0].dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.is_err());
    assert!(listing.is_empty());
}

// ------------------------------------------------------------ path safety

#[test]
fn symlinked_destination_folder_is_never_followed() {
    let fx = Fixture::new();
    let outside = fx.tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, fx.dst_path(0, "sub")).unwrap();
    fx.write_src("sub/file.txt", b"secret");

    // normal path: planning refuses, nothing is transferred
    let plan = fx.plan(0);
    assert!(
        plan.items.iter().all(|i| !i.action.is_transfer()),
        "{plan:?}"
    );
    fx.sync_ok(0);
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

    // forged item, as if a stale plan or a race got through: execution still refuses
    let forged = PlannedItem {
        rel: rel("sub/file.txt"),
        kind: EntryKind::File,
        action: Action::Copy,
        size: 6,
        reason: String::new(),
        expected_src: None,
        expected_dest: Observed::Missing,
    };
    let res = fx.with(0, |ctx| execute_item(ctx, &forged));
    assert!(matches!(res, Err(Error::PathSafety(_))), "{res:?}");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn symlinked_source_folder_is_never_followed_during_transfer() {
    let fx = Fixture::new();
    let elsewhere = fx.tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("loot.txt"), b"not in the source").unwrap();
    symlink(&elsewhere, fx.src_dir.join("linked")).unwrap();
    let forged = PlannedItem {
        rel: rel("linked/loot.txt"),
        kind: EntryKind::File,
        action: Action::Copy,
        size: 1,
        reason: String::new(),
        expected_src: None,
        expected_dest: Observed::Missing,
    };
    let res = fx.with(0, |ctx| execute_item(ctx, &forged));
    assert!(matches!(res, Err(Error::PathSafety(_))), "{res:?}");
    assert!(fx.read_dst(0, "linked/loot.txt").is_none());
}

#[test]
fn destination_root_that_is_a_symlink_or_missing_is_not_opened_or_created() {
    let d = tempfile::tempdir().unwrap();
    let real = d.path().join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, d.path().join("link")).unwrap();
    assert!(DirFd::open_path(&d.path().join("link")).is_err());
    let missing = d.path().join("Volumes/Gone");
    assert!(DirFd::open_path(&missing).is_err());
    assert!(!missing.exists() && !d.path().join("Volumes").exists());
}

#[test]
fn rel_paths_with_parent_traversal_cannot_even_be_constructed() {
    assert!(RelPath::from_bytes(&b"../escape"[..]).is_err());
    assert!(RelPath::from_bytes(&b"/etc/passwd"[..]).is_err());
}
