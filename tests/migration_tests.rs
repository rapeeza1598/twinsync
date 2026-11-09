use tempfile::tempdir;
use twinsync::domain::*;
use twinsync::storage::legacy::import_if_present;
use twinsync::storage::*;

fn rel(b: &[u8]) -> RelPath {
    RelPath::from_bytes(b).unwrap()
}

fn fp(size: u64) -> Fingerprint {
    Fingerprint {
        size,
        mtime_ns: 1_700_000_000_000_000_000,
        inode: 42,
    }
}

fn entry(path: &[u8]) -> SourceEntry {
    SourceEntry {
        rel: rel(path),
        kind: EntryKind::File,
        fp: fp(10),
        hash: Some(Hash([7; 32])),
    }
}

#[test]
fn fresh_database_is_at_latest_schema() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    assert_eq!(db.schema_version().unwrap(), latest_version());
    // Re-opening must not re-run or fail.
    drop(db);
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    assert_eq!(db.schema_version().unwrap(), latest_version());
}

#[test]
fn newer_schema_is_refused_not_downgraded() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.sqlite3");
    drop(Db::open(&path).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (999, 0)",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(Db::open(&path).is_err());
}

#[test]
fn state_survives_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.sqlite3");
    let (project_id, dest_id) = {
        let db = Db::open(&path).unwrap();
        let p = db.create_project("TwinSync", Some(dir.path())).unwrap();
        let mut d = DestinationRow::new(&p.id, Slot::A);
        d.volume_uuid = Some("UUID-1".into());
        d.media_type = DriveMediaType::Hdd;
        d.profile = DrivePerformanceProfile::Conservative;
        d.free_bytes = Some(123);
        db.upsert_destination(&d).unwrap();
        db.commit_scan(&p.id, &[entry(b"a.txt")], true).unwrap();
        db.upsert_dest_entry(
            &d.id,
            &DestEntry {
                rel: rel(b"a.txt"),
                kind: EntryKind::File,
                status: EntryStatus::Synced,
                synced_hash: Some(Hash([7; 32])),
                dest_fp: Some(fp(10)),
                src_fp: Some(fp(10)),
                synced_at: Some(1),
                verified_at: None,
            },
        )
        .unwrap();
        (p.id, d.id)
    };
    let db = Db::open(&path).unwrap();
    let p = db.first_project().unwrap().unwrap();
    assert_eq!(p.id, project_id);
    assert_eq!(p.source_generation, 1);
    assert!(p.last_scan_complete);
    let dests = db.destinations(&p.id).unwrap();
    assert_eq!(dests.len(), 1);
    assert_eq!(dests[0].id, dest_id);
    assert_eq!(dests[0].media_type, DriveMediaType::Hdd);
    assert_eq!(dests[0].profile, DrivePerformanceProfile::Conservative);
    assert_eq!(dests[0].free_bytes, Some(123));
    assert_eq!(db.source_entries(&p.id).unwrap().len(), 1);
    let e = &db.dest_entries(&dest_id).unwrap()[&rel(b"a.txt")];
    assert_eq!(e.synced_hash, Some(Hash([7; 32])));
    assert_eq!(e.dest_fp, Some(fp(10)));
}

#[test]
fn non_utf8_paths_round_trip_through_every_table() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    let p = db.create_project("t", Some(dir.path())).unwrap();
    let d = DestinationRow::new(&p.id, Slot::A);
    db.upsert_destination(&d).unwrap();
    let weird = b"dir/\xff\xfe-name.txt";
    db.commit_scan(&p.id, &[entry(weird)], true).unwrap();
    assert!(db.source_entries(&p.id).unwrap().contains_key(&rel(weird)));
    db.log_event(Some(&d.id), "x", Some(&rel(weird)), "failed", "boom", true)
        .unwrap();
    let ev = db.activity(&ActivityFilter::default()).unwrap();
    assert_eq!(ev[0].rel.as_ref().unwrap().as_bytes(), weird);
}

#[test]
fn incomplete_scan_keeps_unseen_entries_but_complete_scan_removes_them() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    let p = db.create_project("t", Some(dir.path())).unwrap();
    db.commit_scan(&p.id, &[entry(b"a"), entry(b"b")], true)
        .unwrap();
    db.commit_scan(&p.id, &[entry(b"a")], false).unwrap();
    assert_eq!(
        db.source_entries(&p.id).unwrap().len(),
        2,
        "unseen != deleted"
    );
    assert!(!db.first_project().unwrap().unwrap().last_scan_complete);
    db.commit_scan(&p.id, &[entry(b"a")], true).unwrap();
    assert_eq!(db.source_entries(&p.id).unwrap().len(), 1);
}

#[test]
fn commit_op_writes_entry_version_and_terminal_state_together() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    let p = db.create_project("t", Some(dir.path())).unwrap();
    let d = DestinationRow::new(&p.id, Slot::B);
    db.upsert_destination(&d).unwrap();
    let op = OpRow {
        id: new_id(),
        destination_id: d.id.clone(),
        kind: OpKind::Update,
        rel: rel(b"f"),
        state: OpState::Running,
        step: OpStep::Committed,
        temp_name: "x.part".into(),
        version_rel: Some(rel(b"2024_ab/f")),
        expected_old_hash: Some(Hash([1; 32])),
        new_hash: Some(Hash([2; 32])),
        src_fp: Some(fp(5)),
        bytes_total: 5,
        bytes_done: 5,
        attempts: 0,
        error: None,
    };
    db.insert_op(&op).unwrap();
    assert_eq!(db.unfinished_ops(&d.id).unwrap().len(), 1);
    let entry = DestEntry {
        rel: rel(b"f"),
        kind: EntryKind::File,
        status: EntryStatus::Synced,
        synced_hash: Some(Hash([2; 32])),
        dest_fp: Some(fp(5)),
        src_fp: Some(fp(5)),
        synced_at: Some(1),
        verified_at: None,
    };
    let ver = VersionRow {
        rel: rel(b"f"),
        version_rel: rel(b"2024_ab/f"),
        size: 9,
        hash: Some(Hash([1; 32])),
        op_id: Some(op.id.clone()),
        created_at: 1,
    };
    db.commit_op(&op.id, &d.id, &entry, Some(&ver), "updated")
        .unwrap();
    assert!(db.unfinished_ops(&d.id).unwrap().is_empty());
    assert_eq!(db.op(&op.id).unwrap().unwrap().state, OpState::Done);
    assert_eq!(db.versions(&d.id).unwrap().len(), 1);
    assert_eq!(db.version_bytes(&d.id).unwrap(), 9);
    assert_eq!(db.dest_entries(&d.id).unwrap().len(), 1);
}

#[test]
fn legacy_ssd_config_is_imported_and_original_kept() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    let legacy = dir.path().join("config.json");
    let body = r#"{"source":"/Users/x/Work","ssd_a":"/Volumes/A/Backup","ssd_b":"/Volumes/B/Backup","extra":1}"#;
    std::fs::write(&legacy, body).unwrap();

    assert!(import_if_present(&db, &legacy).unwrap());
    assert!(!legacy.exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config.json.migrated")).unwrap(),
        body,
        "original must be preserved verbatim"
    );
    let p = db.first_project().unwrap().unwrap();
    assert_eq!(
        p.source_path.as_deref(),
        Some(std::path::Path::new("/Users/x/Work"))
    );
    let dests = db.destinations(&p.id).unwrap();
    assert_eq!(dests.len(), 2);
    assert_eq!(dests[0].label, "SSD A");
    assert_eq!(dests[1].label, "SSD B");
    assert!(
        dests
            .iter()
            .all(|d| d.volume_uuid.is_none() && !d.identity_strong)
    );
    assert_eq!(
        dests[0].last_mount_path.as_deref(),
        Some(std::path::Path::new("/Volumes/A/Backup"))
    );
    // Second run is a no-op.
    assert!(!import_if_present(&db, &legacy).unwrap());
}

#[test]
fn malformed_legacy_config_is_left_untouched() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    let legacy = dir.path().join("config.json");
    std::fs::write(&legacy, "{not json").unwrap();
    assert!(import_if_present(&db, &legacy).is_err());
    assert!(legacy.exists());
    assert!(db.first_project().unwrap().is_none());
}

#[test]
fn changing_source_invalidates_scan_state() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.sqlite3")).unwrap();
    let p = db.create_project("t", Some(dir.path())).unwrap();
    let d = DestinationRow::new(&p.id, Slot::A);
    db.upsert_destination(&d).unwrap();
    db.commit_scan(&p.id, &[entry(b"a")], true).unwrap();
    db.set_planned_generation(&d.id, 1).unwrap();
    db.set_source_path(&p.id, dir.path()).unwrap();
    assert!(db.source_entries(&p.id).unwrap().is_empty());
    assert_eq!(
        db.destination(&d.id).unwrap().unwrap().planned_generation,
        -1
    );
    assert!(!db.first_project().unwrap().unwrap().last_scan_complete);
}
