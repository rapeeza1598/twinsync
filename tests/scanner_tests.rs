use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::AtomicBool;
use tempfile::tempdir;
use twinsync::config::default_exclusions;
use twinsync::domain::*;
use twinsync::scanner::*;

fn run(root: &std::path::Path, hash_all: bool) -> ScanOutcome {
    let ex = Exclusions::new(&default_exclusions()).unwrap();
    scan_source(root, &ex, hash_all, &AtomicBool::new(false)).unwrap()
}

fn done(o: ScanOutcome) -> ScanResult {
    match o {
        ScanOutcome::Done(r) => r,
        ScanOutcome::SourceUnavailable(e) => panic!("unexpectedly unavailable: {e}"),
    }
}

fn names(r: &ScanResult) -> Vec<String> {
    r.entries.iter().map(|e| e.rel.display()).collect()
}

#[test]
fn walks_files_and_dirs_in_path_order_without_modifying_anything() {
    let d = tempdir().unwrap();
    fs::create_dir_all(d.path().join("sub/deep")).unwrap();
    fs::write(d.path().join("b.txt"), "bb").unwrap();
    fs::write(d.path().join("a.txt"), "a").unwrap();
    fs::write(d.path().join("sub/deep/x"), "xyz").unwrap();
    fs::create_dir(d.path().join("empty")).unwrap();
    let before = fs::metadata(d.path().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();

    let r = done(run(d.path(), false));
    assert!(r.complete);
    assert_eq!(
        names(&r),
        ["a.txt", "b.txt", "empty", "sub", "sub/deep", "sub/deep/x"]
    );
    let x = r.entries.last().unwrap();
    assert_eq!((x.kind, x.fp.size), (EntryKind::File, 3));
    assert_eq!(fs::read_to_string(d.path().join("b.txt")).unwrap(), "bb");
    assert_eq!(
        fs::metadata(d.path().join("a.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}

#[test]
fn symlinks_and_special_files_are_skipped_with_a_reason_and_never_followed() {
    let d = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret"), "s").unwrap();
    fs::write(d.path().join("real"), "r").unwrap();
    symlink(outside.path(), d.path().join("dirlink")).unwrap();
    symlink(outside.path().join("secret"), d.path().join("filelink")).unwrap();
    let fifo = std::ffi::CString::new(d.path().join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let r = done(run(d.path(), false));
    assert_eq!(names(&r), ["real"], "nothing behind a link is scanned");
    let skipped: std::collections::HashMap<_, _> = r
        .skipped
        .iter()
        .map(|(p, why)| (p.display(), *why))
        .collect();
    assert_eq!(skipped["dirlink"], SkipReason::Symlink);
    assert_eq!(skipped["filelink"], SkipReason::Symlink);
    assert_eq!(skipped["pipe"], SkipReason::SpecialFile);
    assert!(r.complete, "skips are reported, they are not a failed scan");
}

#[test]
fn default_exclusions_skip_os_junk_but_not_build_folders() {
    let d = tempdir().unwrap();
    for f in [
        ".DS_Store",
        "._resource",
        "~$lock.docx",
        "notes.swp",
        "keep.txt",
    ] {
        fs::write(d.path().join(f), "x").unwrap();
    }
    fs::create_dir_all(d.path().join("node_modules/pkg")).unwrap();
    fs::write(d.path().join("node_modules/pkg/index.js"), "x").unwrap();
    fs::create_dir(d.path().join("build")).unwrap();

    let r = done(run(d.path(), false));
    assert_eq!(
        names(&r),
        [
            "build",
            "keep.txt",
            "node_modules",
            "node_modules/pkg",
            "node_modules/pkg/index.js"
        ]
    );
    assert_eq!(r.excluded_count, 4);
}

#[test]
fn user_exclusions_apply() {
    let d = tempdir().unwrap();
    fs::create_dir(d.path().join("node_modules")).unwrap();
    fs::write(d.path().join("node_modules/x"), "x").unwrap();
    fs::write(d.path().join("a.txt"), "x").unwrap();
    let ex = Exclusions::new(&["node_modules".to_string()]).unwrap();
    let r = done(scan_source(d.path(), &ex, false, &AtomicBool::new(false)).unwrap());
    assert_eq!(names(&r), ["a.txt"]);
}

#[test]
fn non_utf8_names_are_kept_byte_exact() {
    let d = tempdir().unwrap();
    let raw = b"bad-\xff\xfe-name".to_vec();
    match fs::write(d.path().join(OsString::from_vec(raw.clone())), "x") {
        Ok(()) => {
            let r = done(run(d.path(), false));
            assert_eq!(r.entries[0].rel.as_bytes(), raw.as_slice());
        }
        // APFS/HFS+ reject invalid UTF-8 names (EILSEQ), so on those volumes the
        // scanner never sees one. Byte-exact round-trip of RelPath and the
        // database is covered in domain and migration tests.
        Err(e) => {
            assert_eq!(e.raw_os_error(), Some(libc::EILSEQ));
            eprintln!("note: this filesystem rejects non-UTF-8 names; scanner path not exercised");
        }
    }
}

#[test]
fn unreadable_directory_makes_the_scan_incomplete_not_empty() {
    let d = tempdir().unwrap();
    fs::create_dir(d.path().join("locked")).unwrap();
    fs::write(d.path().join("locked/inside"), "x").unwrap();
    fs::write(d.path().join("ok.txt"), "x").unwrap();
    fs::set_permissions(d.path().join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let r = done(run(d.path(), false));
    fs::set_permissions(d.path().join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!r.complete);
    assert_eq!(
        r.unreadable,
        vec![RelPath::from_bytes(&b"locked"[..]).unwrap()]
    );
    assert!(names(&r).contains(&"ok.txt".to_string()));
    assert!(r.covers(&RelPath::from_bytes(&b"ok.txt"[..]).unwrap()));
    assert!(!r.covers(&RelPath::from_bytes(&b"locked/inside"[..]).unwrap()));
}

#[test]
fn missing_or_non_folder_source_is_unavailable_not_empty() {
    let d = tempdir().unwrap();
    assert!(matches!(
        run(&d.path().join("nope"), false),
        ScanOutcome::SourceUnavailable(_)
    ));
    fs::write(d.path().join("file"), "x").unwrap();
    assert!(matches!(
        run(&d.path().join("file"), false),
        ScanOutcome::SourceUnavailable(_)
    ));
    fs::create_dir(d.path().join("noaccess")).unwrap();
    fs::set_permissions(d.path().join("noaccess"), fs::Permissions::from_mode(0o000)).unwrap();
    let o = run(&d.path().join("noaccess"), false);
    fs::set_permissions(d.path().join("noaccess"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(o, ScanOutcome::SourceUnavailable(_)));
}

#[test]
fn hash_all_hashes_content_and_default_scan_does_not() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("f"), "hello").unwrap();
    assert!(done(run(d.path(), false)).entries[0].hash.is_none());
    let h = done(run(d.path(), true)).entries[0].hash.unwrap();
    assert_eq!(h.0, *blake3::hash(b"hello").as_bytes());
}

#[test]
fn cancellation_stops_the_scan() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("f"), "x").unwrap();
    let ex = Exclusions::new(&[]).unwrap();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        scan_source(d.path(), &ex, false, &cancel),
        Err(twinsync::errors::Error::Cancelled)
    ));
}

#[test]
fn exclusion_matching_works_on_event_paths_below_the_source() {
    let root = std::path::Path::new("/Users/me/work");
    let ex = Exclusions::new(&default_exclusions()).unwrap();
    let hit = |p: &str| ex.matches_path(root, std::path::Path::new(p));
    assert!(hit("/Users/me/work/.DS_Store"));
    assert!(hit("/Users/me/work/photos/.DS_Store"));
    assert!(hit("/Users/me/work/a/._resource"));
    assert!(
        hit("/Users/me/work/.Spotlight-V100/Store-V2/x"),
        "anything below an excluded folder"
    );
    assert!(hit("/Users/me/work/doc/~$lock.docx"));
    // real work and build folders are not excluded unless the user adds them
    assert!(!hit("/Users/me/work/photos/a.jpg"));
    assert!(!hit("/Users/me/work/node_modules/pkg/index.js"));
    // outside the source, and the source folder itself, are never "excluded"
    assert!(!hit("/Users/me/other/.DS_Store"));
    assert!(!hit("/Users/me/work"));
}

#[test]
fn scans_with_the_same_content_compare_equal_even_if_excluded_counts_differ() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("a.txt"), "x").unwrap();
    let first = done(run(d.path(), false));
    fs::write(d.path().join(".DS_Store"), "junk").unwrap(); // excluded: not content
    let second = done(run(d.path(), false));
    assert_ne!(first.excluded_count, second.excluded_count);
    assert!(first.same_content_as(&second));
    fs::write(d.path().join("b.txt"), "y").unwrap();
    assert!(!first.same_content_as(&done(run(d.path(), false))));
    fs::write(d.path().join("a.txt"), "changed").unwrap();
    assert!(
        !second.same_content_as(&done(run(d.path(), false))),
        "a changed file is a change"
    );
}
