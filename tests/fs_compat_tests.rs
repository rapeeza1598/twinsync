//! Real-filesystem compatibility checks. Ignored by default: they need a mounted
//! volume, which `scripts/fs-compat-test.sh` provides using throwaway disk images.
//! Required environment: TWINSYNC_COMPAT_VOLUME, TWINSYNC_COMPAT_STATE, TWINSYNC_COMPAT_SRC.
//! They write only below the given volume path.

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use twinsync::devices::{DeviceResolver, MacResolver, detect_caps};
use twinsync::domain::*;
use twinsync::planner::Action;
use twinsync::transfer::fsops::DirFd;

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

struct Setup {
    fx: Fixture,
    volume: PathBuf,
}

fn setup() -> Option<Setup> {
    let (volume, state, src) = (
        env("TWINSYNC_COMPAT_VOLUME")?,
        env("TWINSYNC_COMPAT_STATE")?,
        env("TWINSYNC_COMPAT_SRC")?,
    );
    let mut fx = Fixture::on_volume(volume.join("Backup"), state, src);
    let info = MacResolver::new()
        .volume_for_path(&volume)
        .expect("resolve volume");
    let root = DirFd::open_path(&volume.join("Backup")).unwrap();
    fx.caps = detect_caps(&root, &info);
    eprintln!(
        "COMPAT volume={} fs={} media={:?} bus={:?} uuid={} case_insensitive={} max_file={:?} restricted_names={} inode_trusted={}",
        volume.display(),
        info.fs_type,
        info.media,
        info.bus_protocol,
        info.volume_uuid.is_some(),
        fx.caps.case_insensitive,
        fx.caps.max_file_size,
        fx.caps.restricted_names,
        fx.caps.inode_stable
    );
    Some(Setup { fx, volume })
}

fn binary(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

const THAI: &str = "ทดสอบ-ไฟล์.txt";
const NFC: &str = "caf\u{e9}-nfc.txt";
const NFD: &str = "cafe\u{301}-nfd.txt";

#[test]
#[ignore = "needs a mounted volume; run scripts/fs-compat-test.sh"]
fn phase_write() {
    let Some(Setup { fx, volume }) = setup() else {
        eprintln!("COMPAT skipped: environment not set");
        return;
    };
    let long_name = format!("{}.txt", "x".repeat(200));
    let files: Vec<(String, Vec<u8>)> = vec![
        ("plain.txt".into(), b"plain".to_vec()),
        (NFC.into(), b"nfc".to_vec()),
        (NFD.into(), b"nfd".to_vec()),
        (THAI.into(), "สวัสดี".as_bytes().to_vec()),
        ("emoji-😀.txt".into(), b"emoji".to_vec()),
        (long_name.clone(), b"long".to_vec()),
        ("empty.bin".into(), vec![]),
        ("big.bin".into(), binary(5 * 1024 * 1024 + 17)),
        ("dir/sub/deep.bin".into(), binary(300_000)),
        ("with space & (parens).txt".into(), b"space".to_vec()),
    ];
    for (n, c) in &files {
        fx.write_src(n, c);
    }
    fs::create_dir_all(fx.src_dir.join("emptydir")).unwrap();
    // a name FAT/exFAT cannot store: must be blocked, never silently renamed or mangled
    fx.write_src("bad:name.txt", b"colon");

    let plan = fx.plan(0);
    let colon = plan
        .items
        .iter()
        .find(|i| i.rel == rel("bad:name.txt"))
        .unwrap();
    if fx.caps.restricted_names {
        assert!(matches!(colon.action, Action::Blocked(_)), "{colon:?}");
    } else {
        assert_eq!(colon.action, Action::Copy);
    }
    for (r, res) in fx.sync(0) {
        if r == rel("bad:name.txt") && fx.caps.restricted_names {
            continue;
        }
        res.unwrap_or_else(|e| panic!("{r}: {e}"));
    }
    for (n, c) in &files {
        assert_eq!(
            &fx.read_dst(0, n).unwrap_or_else(|| panic!("missing {n}")),
            c,
            "{n}"
        );
    }
    assert!(fx.dst_path(0, "emptydir").is_dir());
    if fx.caps.restricted_names {
        assert!(fx.read_dst(0, "bad:name.txt").is_none());
    }

    // update keeps the previous version on this filesystem
    fx.write_src("plain.txt", b"plain, changed");
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "plain.txt").unwrap(), b"plain, changed");
    // On FAT-family volumes macOS itself keeps extended attributes in "._name" AppleDouble
    // sidecars and moves them along with a renamed file; they are not TwinSync's files.
    let is_sidecar = |n: &str| n.rsplit('/').next().is_some_and(|b| b.starts_with("._"));
    let versions: Vec<_> = fx
        .version_files(0)
        .into_iter()
        .filter(|(p, _)| !is_sidecar(p))
        .collect();
    assert_eq!(versions.len(), 1, "{versions:?}");
    assert_eq!(versions[0].1, b"plain");
    let leftovers: Vec<_> = fx
        .tmp_files(0)
        .into_iter()
        .filter(|n| !is_sidecar(n))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // collision with a file that was already there is reported, never overwritten
    fs::write(fx.dst_path(0, "pre-existing.txt"), b"theirs").unwrap();
    fx.write_src("pre-existing.txt", b"mine");
    assert!(matches!(
        fx.action(0, "pre-existing.txt"),
        Some(Action::Collision(_))
    ));
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "pre-existing.txt").unwrap(), b"theirs");

    // an unmanaged file is left alone
    fs::write(fx.dst_path(0, "stranger.txt"), b"unmanaged").unwrap();
    fx.sync_ok(0);
    assert_eq!(fx.read_dst(0, "stranger.txt").unwrap(), b"unmanaged");

    // renaming a source file by letter case only (foo -> Foo) must never destroy data on a
    // case-insensitive drive, where both names are the same file
    if fx.caps.case_insensitive {
        fx.write_src("Case.txt", b"upper");
        fx.sync_ok(0);
        fs::rename(fx.src_dir.join("Case.txt"), fx.src_dir.join("case.txt")).unwrap();
        let a = fx.action(0, "case.txt");
        assert!(
            matches!(
                a,
                Some(Action::Collision(
                    twinsync::planner::CollisionKind::CaseOnlyRename
                ))
            ),
            "{a:?}"
        );
        fx.sync_ok(0);
        assert_eq!(
            fx.read_dst(0, "Case.txt").unwrap(),
            b"upper",
            "existing copy kept intact"
        );
    }
    eprintln!("COMPAT write phase ok on {}", volume.display());
}

/// Run after the volume was detached and attached again (new device number).
#[test]
#[ignore = "needs a mounted volume; run scripts/fs-compat-test.sh"]
fn phase_verify() {
    let Some(Setup { fx, volume }) = setup() else {
        eprintln!("COMPAT skipped: environment not set");
        return;
    };
    let entries = fx.entries(0);
    assert!(
        !entries.is_empty(),
        "state from the write phase must persist"
    );
    // 1. after a remount nothing may look changed: no mass re-hash of the drive
    let plan = fx.plan(0);
    let checks = plan
        .items
        .iter()
        .filter(|i| i.action == Action::CheckDestination)
        .count();
    let others: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.action.is_transfer() || i.action == Action::CheckDestination)
        .map(|i| (i.rel.display(), i.action.label()))
        .collect();
    // 2. report how stable inode numbers really are on this filesystem
    let (mut same, mut total) = (0, 0);
    for (r, e) in &entries {
        if let (Some(fp), Ok(md)) = (e.dest_fp, fs::metadata(fx.dst_path(0, &r.display()))) {
            total += 1;
            if md.ino() == fp.inode {
                same += 1;
            }
        }
    }
    eprintln!(
        "COMPAT remount volume={} check_destination_after_remount={checks} inode_unchanged={same}/{total} trusted_by_caps={}",
        volume.display(),
        fx.caps.inode_stable
    );
    assert!(
        others.is_empty(),
        "a remount must not trigger work: {others:?}"
    );
    // 3. content still matches what was copied
    for (r, e) in &entries {
        if e.kind != EntryKind::File || e.status != twinsync::storage::EntryStatus::Synced {
            continue;
        }
        let data = fs::read(fx.dst_path(0, &r.display())).unwrap();
        assert_eq!(
            Some(Hash(*blake3::hash(&data).as_bytes())),
            e.synced_hash,
            "{r}"
        );
    }
    eprintln!("COMPAT verify phase ok on {}", volume.display());
}
