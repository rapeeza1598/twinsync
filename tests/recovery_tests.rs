mod common;

use common::*;
use std::fs;
use twinsync::domain::*;
use twinsync::errors::Error;
use twinsync::storage::*;
use twinsync::transfer::recovery::RecoveryOutcome;
use twinsync::transfer::*;

fn blake(data: &[u8]) -> Hash {
    Hash(*blake3::hash(data).as_bytes())
}

/// Managed file "a" holding `old`, with the source now holding `new`.
fn pending_update(fx: &Fixture) {
    fx.write_src("a", b"old");
    fx.sync_ok(0);
    fx.write_src("a", b"new content");
}

fn crash(fx: &Fixture, point: FaultPoint) {
    fx.crash_at(point);
    let out = fx.sync(0);
    assert!(
        matches!(out[0].1, Err(Error::Fault(_))),
        "expected a simulated crash at {point:?}, got {:?}",
        out[0].1
    );
    fx.clear_fault();
}

fn outcome(fx: &Fixture) -> Vec<RecoveryOutcome> {
    fx.recover(0).ops.into_iter().map(|r| r.outcome).collect()
}

/// After recovery and one more normal sync, everything must agree.
fn assert_converges(fx: &Fixture, expected_versions: usize) {
    assert_eq!(fx.unfinished(0), 0, "no operation may stay unresolved");
    assert!(
        fx.tmp_files(0).is_empty(),
        "no temp file may be left behind"
    );
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"new content");
    assert!(fx.plan(0).is_empty(), "plan must be empty once converged");
    let e = &fx.entries(0)[&rel("a")];
    assert_eq!(e.synced_hash, Some(blake(b"new content")));
    assert_eq!(fx.version_files(0).len(), expected_versions);
    assert_eq!(
        fx.db.versions(&fx.dests[0].id).unwrap().len(),
        expected_versions
    );
}

#[test]
fn update_crash_before_anything_destructive_aborts_and_keeps_the_old_copy() {
    for point in [
        FaultPoint::WriteChunk(0),
        FaultPoint::AfterTempSynced,
        FaultPoint::AfterVerifyRead,
        FaultPoint::BeforePreserve,
    ] {
        let fx = Fixture::new();
        pending_update(&fx);
        crash(&fx, point);
        assert_eq!(outcome(&fx), [RecoveryOutcome::Aborted], "{point:?}");
        assert_eq!(fx.read_dst(0, "a").unwrap(), b"old", "{point:?}");
        assert_eq!(
            fx.entries(0)[&rel("a")].synced_hash,
            Some(blake(b"old")),
            "{point:?}"
        );
        assert_converges(&fx, 1);
    }
}

#[test]
fn update_crash_after_preserving_the_old_version_restores_it() {
    for point in [FaultPoint::AfterPreserve, FaultPoint::BeforeCommitRename] {
        let fx = Fixture::new();
        pending_update(&fx);
        crash(&fx, point);
        // the visible symptom: target gone, old content only in version history
        assert!(fx.read_dst(0, "a").is_none(), "{point:?}");
        assert_eq!(fx.version_files(0).len(), 1, "{point:?}");

        assert_eq!(outcome(&fx), [RecoveryOutcome::RolledBack], "{point:?}");
        assert_eq!(fx.read_dst(0, "a").unwrap(), b"old", "{point:?}");
        assert!(fx.version_files(0).is_empty(), "restored, not duplicated");
        assert_converges(&fx, 1);
    }
}

#[test]
fn update_crash_after_the_rename_commit_is_finished_from_the_filesystem() {
    for point in [FaultPoint::AfterCommitRename, FaultPoint::BeforeDbCommit] {
        let fx = Fixture::new();
        pending_update(&fx);
        crash(&fx, point);
        // database still describes the old file, filesystem has the new one
        assert_eq!(fx.read_dst(0, "a").unwrap(), b"new content");
        assert_eq!(fx.entries(0)[&rel("a")].synced_hash, Some(blake(b"old")));

        assert_eq!(outcome(&fx), [RecoveryOutcome::Finalized], "{point:?}");
        let e = &fx.entries(0)[&rel("a")];
        assert_eq!(e.synced_hash, Some(blake(b"new content")));
        let rows = fx.db.versions(&fx.dests[0].id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size, 3);
        assert_eq!(fx.version_files(0)[0].1, b"old");
        assert!(fx.plan(0).is_empty());
        assert_converges(&fx, 1);
    }
}

#[test]
fn crash_before_the_journal_leaves_no_trace() {
    let fx = Fixture::new();
    pending_update(&fx);
    crash(&fx, FaultPoint::BeforeJournal);
    assert!(fx.recover(0).ops.is_empty());
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"old");
    assert_converges(&fx, 1);
}

#[test]
fn new_file_crash_matrix() {
    let aborted = [
        FaultPoint::WriteChunk(0),
        FaultPoint::AfterTempSynced,
        FaultPoint::AfterVerifyRead,
        FaultPoint::BeforeCommitRename,
    ];
    for point in aborted {
        let fx = Fixture::new();
        fx.write_src("a", b"new content");
        crash(&fx, point);
        assert!(
            fx.read_dst(0, "a").is_none(),
            "{point:?}: nothing committed yet"
        );
        assert_eq!(outcome(&fx), [RecoveryOutcome::Aborted], "{point:?}");
        assert_converges(&fx, 0);
    }
    for point in [FaultPoint::AfterCommitRename, FaultPoint::BeforeDbCommit] {
        let fx = Fixture::new();
        fx.write_src("a", b"new content");
        crash(&fx, point);
        assert_eq!(fx.read_dst(0, "a").unwrap(), b"new content");
        assert!(fx.entries(0).is_empty(), "database not yet updated");
        assert_eq!(outcome(&fx), [RecoveryOutcome::Finalized], "{point:?}");
        assert_converges(&fx, 0);
    }
}

#[test]
fn recovery_is_idempotent() {
    let fx = Fixture::new();
    pending_update(&fx);
    crash(&fx, FaultPoint::AfterPreserve);
    assert_eq!(fx.recover(0).ops.len(), 1);
    assert!(fx.recover(0).ops.is_empty());
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"old");
}

#[test]
fn a_foreign_file_that_appeared_after_preserve_is_kept_alongside_the_version() {
    let fx = Fixture::new();
    pending_update(&fx);
    crash(&fx, FaultPoint::AfterPreserve);
    fs::write(fx.dst_path(0, "a"), b"intruder").unwrap();
    let out = outcome(&fx);
    assert!(
        matches!(out[0], RecoveryOutcome::NeedsAttention(_)),
        "{out:?}"
    );
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"intruder", "never clobbered");
    assert_eq!(fx.version_files(0)[0].1, b"old", "old content still safe");
    assert_eq!(fx.op_states(0).last().unwrap(), "attention");
}

#[test]
fn folder_at_the_target_of_an_interrupted_copy_needs_attention() {
    let fx = Fixture::new();
    fx.write_src("a", b"data");
    crash(&fx, FaultPoint::AfterTempSynced);
    fs::create_dir(fx.dst_path(0, "a")).unwrap();
    assert!(matches!(
        outcome(&fx)[0],
        RecoveryOutcome::NeedsAttention(_)
    ));
    assert!(fx.dst_path(0, "a").is_dir());
}

#[test]
fn disconnect_mid_copy_is_cleaned_up_when_the_drive_returns() {
    let mut fx = Fixture::new();
    fx.chunk = 4096;
    fx.write_src("a", b"old");
    fx.sync_ok(0);
    fx.write_src("a", &vec![b'n'; 40_000]);
    // the drive disappears: the write fails and so does the cleanup
    fx.set_fault(|p| match p {
        FaultPoint::WriteChunk(1) | FaultPoint::Cleanup => Some(Fault::Io(libc::EIO)),
        _ => None,
    });
    let out = fx.sync(0);
    assert!(out[0].1.as_ref().unwrap_err().is_transient());
    assert_eq!(
        fx.op_states(0),
        ["done", "running"],
        "unresolved until recovery"
    );
    assert_eq!(
        fx.tmp_files(0).len(),
        1,
        "partial temp is still on the drive"
    );

    // drive is back
    fx.clear_fault();
    assert_eq!(outcome(&fx), [RecoveryOutcome::Aborted]);
    assert!(fx.tmp_files(0).is_empty());
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"old");
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "a").unwrap().len(), 40_000);
}

#[test]
fn temp_cleanup_removes_only_files_it_can_prove_it_owns() {
    let fx = Fixture::new();
    fx.write_src("a", b"data");
    fx.sync_ok(0);
    let tmp = fx.tmp_dir(0);

    // a leftover of a finished operation: provably ours
    let done_op = &fx.op_ids(0)[0];
    fs::write(tmp.join(format!("{done_op}.part")), b"leftover").unwrap();
    // things that merely live in the folder: not provably ours
    fs::write(tmp.join("notes.txt"), b"user file").unwrap();
    let stranger = format!("{}.part", new_id());
    fs::write(tmp.join(&stranger), b"unknown operation").unwrap();

    let res = fx.recover(0);
    assert!(!tmp.join(format!("{done_op}.part")).exists());
    assert_eq!(fs::read(tmp.join("notes.txt")).unwrap(), b"user file");
    assert_eq!(fs::read(tmp.join(&stranger)).unwrap(), b"unknown operation");
    let mut unknown = res.unknown_in_tmp.clone();
    unknown.sort();
    let mut expected = vec!["notes.txt".to_string(), stranger];
    expected.sort();
    assert_eq!(unknown, expected);
    // user data outside tmp is of course untouched too
    assert_eq!(fx.read_dst(0, "a").unwrap(), b"data");
}

#[test]
fn recovery_reports_land_in_the_activity_log() {
    let fx = Fixture::new();
    pending_update(&fx);
    crash(&fx, FaultPoint::AfterPreserve);
    fx.recover(0);
    let ev = fx.db.activity(&ActivityFilter::default()).unwrap();
    assert!(ev.iter().any(|e| e.kind == "recovery" && e.result == "ok"));
}
