mod common;

use common::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use twinsync::devices::*;
use twinsync::domain::*;
use twinsync::platform::macos::{parse_diskutil_info, valid_bsd_name};
use twinsync::storage::DestinationRow;
use twinsync::transfer::fsops::DirFd;

// ------------------------------------------------------------ diskutil plist

fn plist(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict>",
    );
    for (k, v) in entries {
        s.push_str(&format!("<key>{k}</key>{v}"));
    }
    s.push_str("</dict></plist>");
    s.into_bytes()
}

fn st(v: &str) -> String {
    format!("<string>{v}</string>")
}

const T: &str = "<true/>";
const F: &str = "<false/>";

#[test]
fn media_type_comes_only_from_what_macos_reports() {
    let base = |solid: Option<&'static str>| {
        let mut e = vec![
            ("DeviceIdentifier", st("disk5s1")),
            ("VolumeUUID", st("AAAA-1")),
            ("VolumeName", st("Backup")),
            ("FilesystemType", st("exfat")),
            ("BusProtocol", st("USB")),
            ("Internal", F.to_string()),
            ("Writable", T.to_string()),
            ("TotalSize", "<integer>500107862016</integer>".to_string()),
            ("ParentWholeDisk", st("disk5")),
            ("MediaName", st("External SSD")), // a name that says SSD must not matter
        ];
        if let Some(v) = solid {
            e.push(("SolidState", v.to_string()));
        }
        let refs: Vec<(&str, &str)> = e.iter().map(|(k, v)| (*k, v.as_str())).collect();
        parse_diskutil_info(&plist(&refs)).unwrap()
    };
    assert_eq!(
        media_from_solid_state(base(Some(T)).solid_state),
        DriveMediaType::Ssd
    );
    assert_eq!(
        media_from_solid_state(base(Some(F)).solid_state),
        DriveMediaType::Hdd
    );
    let unknown = base(None);
    assert_eq!(
        media_from_solid_state(unknown.solid_state),
        DriveMediaType::Unknown
    );
    assert_eq!(unknown.volume_uuid.as_deref(), Some("AAAA-1"));
    assert_eq!(unknown.fs_type, "exfat");
    assert_eq!(unknown.physical_disk.as_deref(), Some("disk5"));
    assert_eq!(unknown.total_bytes, 500_107_862_016);
}

#[test]
fn apfs_volume_resolves_to_the_physical_disk_behind_its_container() {
    let info = parse_diskutil_info(&plist(&[
        ("DeviceIdentifier", &st("disk6s1")),
        ("FilesystemType", &st("apfs")),
        ("ParentWholeDisk", &st("disk6")),
        (
            "APFSPhysicalStores",
            "<array><dict><key>APFSPhysicalStore</key><string>disk5s2</string></dict></array>",
        ),
    ]))
    .unwrap();
    assert_eq!(info.physical_disk.as_deref(), Some("disk5"));
}

#[test]
fn hostile_or_missing_device_identifiers_are_rejected() {
    for id in ["../etc/passwd", "-rf", "disk4; reboot", ""] {
        assert!(
            parse_diskutil_info(&plist(&[("DeviceIdentifier", &st(id))])).is_err(),
            "{id:?}"
        );
    }
    assert!(parse_diskutil_info(&plist(&[("VolumeName", &st("x"))])).is_err());
    assert!(parse_diskutil_info(b"not a plist").is_err());
    assert!(valid_bsd_name("disk3s1s1"));
}

#[test]
fn live_read_only_query_of_the_boot_volume_works() {
    let r = MacResolver::new();
    let v = r.volume_for_path(Path::new("/")).unwrap();
    assert!(v.local_disk && valid_bsd_name(&v.bsd_name));
    assert!(v.volume_uuid.is_some() && v.total_bytes > 0 && v.internal);
    assert!(!r.mounted_volumes().unwrap().is_empty());
}

// ---------------------------------------------------------------- presence

struct Drives {
    _tmp: tempfile::TempDir,
    /// Canonical: /var/folders is a symlink to /private/var on macOS, and real
    /// mount points are always canonical.
    base: std::path::PathBuf,
}

impl Drives {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        Self { _tmp: tmp, base }
    }
    fn mount(&self, name: &str) -> std::path::PathBuf {
        let p = self.base.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }
}

fn dest(uuid: Option<&str>, slot: Slot) -> DestinationRow {
    let mut d = DestinationRow::new("p", slot);
    d.volume_uuid = uuid.map(str::to_string);
    d
}

#[test]
fn every_media_combination_is_found_by_uuid() {
    let dr = Drives::new();
    use DriveMediaType::*;
    for (ma, mb) in [
        (Ssd, Ssd),
        (Hdd, Hdd),
        (Ssd, Hdd),
        (Unknown, Hdd),
        (Unknown, Unknown),
    ] {
        let (a, b) = (dr.mount("A"), dr.mount("B"));
        let vols = vec![
            vol(&a, "UA", ma, Some("disk4")),
            vol(&b, "UB", mb, Some("disk8")),
        ];
        match (
            find_volume(&vols, &dest(Some("UA"), Slot::A)),
            find_volume(&vols, &dest(Some("UB"), Slot::B)),
        ) {
            (Presence::Candidate(x), Presence::Candidate(y)) => {
                assert_eq!((x.media, y.media), (ma, mb));
                assert_eq!((x.mount_point, y.mount_point), (a, b));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn one_drive_offline_leaves_the_other_online() {
    let dr = Drives::new();
    let b = dr.mount("B");
    let vols = vec![vol(&b, "UB", DriveMediaType::Hdd, None)];
    assert_eq!(
        find_volume(&vols, &dest(Some("UA"), Slot::A)),
        Presence::Offline
    );
    assert!(matches!(
        find_volume(&vols, &dest(Some("UB"), Slot::B)),
        Presence::Candidate(_)
    ));
}

#[test]
fn a_drive_that_returns_at_a_different_mount_path_is_still_the_same_drive() {
    let dr = Drives::new();
    let (old, new) = (dr.mount("OLD"), dr.mount("NEW"));
    let resolver = FakeResolver::new();
    resolver.add(vol(&old, "UA", DriveMediaType::Ssd, None));
    resolver.remove_mount(&old);
    assert_eq!(
        find_volume(
            &resolver.mounted_volumes().unwrap(),
            &dest(Some("UA"), Slot::A)
        ),
        Presence::Offline
    );
    resolver.add(vol(&new, "UA", DriveMediaType::Ssd, None));
    match find_volume(
        &resolver.mounted_volumes().unwrap(),
        &dest(Some("UA"), Slot::A),
    ) {
        Presence::Candidate(v) => assert_eq!(v.mount_point, new),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_different_drive_with_the_same_name_or_mount_path_is_not_accepted() {
    let dr = Drives::new();
    let m = dr.mount("Backup"); // a lookalike mounted where the real one used to be
    let mut impostor = vol(&m, "OTHER-UUID", DriveMediaType::Ssd, None);
    impostor.volume_name = "Backup".into();
    let mut configured = dest(Some("REAL-UUID"), Slot::A);
    configured.volume_name = Some("Backup".into());
    configured.last_mount_path = Some(m);
    assert_eq!(find_volume(&[impostor], &configured), Presence::Offline);
}

#[test]
fn duplicate_uuids_stop_and_ask() {
    let dr = Drives::new();
    let vols = vec![
        vol(&dr.mount("X"), "DUP", DriveMediaType::Ssd, None),
        vol(&dr.mount("Y"), "DUP", DriveMediaType::Hdd, None),
    ];
    assert!(matches!(
        find_volume(&vols, &dest(Some("DUP"), Slot::A)),
        Presence::Ambiguous(_)
    ));
}

#[test]
fn a_destination_without_identity_is_never_matched_by_guesswork() {
    let dr = Drives::new();
    let vols = vec![vol(&dr.mount("X"), "U", DriveMediaType::Ssd, None)];
    assert!(matches!(
        find_volume(&vols, &dest(None, Slot::A)),
        Presence::NoIdentity(_)
    ));
}

// -------------------------------------------------------------- validation

struct Setup {
    dr: Drives,
    resolver: FakeResolver,
    source: std::path::PathBuf,
}

fn setup() -> Setup {
    let dr = Drives::new();
    let source = dr.mount("home/source");
    let resolver = FakeResolver::new();
    resolver.add(vol(
        &dr.mount("home"),
        "HOME",
        DriveMediaType::Ssd,
        Some("disk0"),
    ));
    Setup {
        dr,
        resolver,
        source,
    }
}

impl Setup {
    fn drive(
        &self,
        name: &str,
        uuid: &str,
        phys: Option<&str>,
    ) -> (std::path::PathBuf, VolumeInfo) {
        let m = self.dr.mount(name);
        let v = vol(&m, uuid, DriveMediaType::Hdd, phys);
        self.resolver.add(v.clone());
        (self.dr.mount(&format!("{name}/Backup")), v)
    }
}

fn kinds(c: &DestCheck) -> Vec<Severity> {
    c.findings.iter().map(|f| f.severity).collect()
}

#[test]
fn a_clean_external_destination_has_no_findings() {
    let s = setup();
    let (dst, _) = s.drive("A", "UA", Some("disk4"));
    let c = check_destination_choice(&s.resolver, &s.source, &dst, None).unwrap();
    assert!(c.findings.is_empty(), "{:?}", c.findings);
    assert_eq!(c.root_rel, Path::new("Backup"));
    assert_eq!(c.volume.volume_uuid.as_deref(), Some("UA"));
}

#[test]
fn overlapping_source_and_destination_are_rejected_both_ways() {
    let s = setup();
    let inside = s.dr.mount("home/source/backup");
    let c = check_destination_choice(&s.resolver, &s.source, &inside, None).unwrap();
    assert!(c.has_error());
    let parent = s.dr.mount("home");
    let c = check_destination_choice(&s.resolver, &s.source, &parent, None).unwrap();
    assert!(c.has_error());
}

#[test]
fn a_symlink_cannot_smuggle_an_overlap_past_validation() {
    let s = setup();
    let (dst, _) = s.drive("A", "UA", Some("disk4"));
    fs::remove_dir(&dst).unwrap();
    symlink(&s.source, &dst).unwrap();
    let c = check_destination_choice(&s.resolver, &s.source, &dst, None).unwrap();
    assert!(c.has_error(), "canonicalization must reveal the overlap");
}

#[test]
fn drive_a_and_b_must_not_overlap_or_be_the_same_volume() {
    let s = setup();
    let (a, va) = s.drive("A", "UA", Some("disk4"));
    let b_inside_a = s.dr.mount("A/Backup/second");
    let c = check_destination_choice(&s.resolver, &s.source, &b_inside_a, Some((&a, &va))).unwrap();
    assert!(c.has_error(), "overlap");
    let b_same_volume = s.dr.mount("A/Other");
    let c =
        check_destination_choice(&s.resolver, &s.source, &b_same_volume, Some((&a, &va))).unwrap();
    assert!(c.has_error(), "same volume");
}

#[test]
fn two_volumes_on_one_physical_disk_need_confirmation_and_share_a_limiter() {
    let s = setup();
    let (a, va) = s.drive("A", "UA", Some("disk4"));
    let (b, _) = s.drive("B", "UB", Some("disk4"));
    let c = check_destination_choice(&s.resolver, &s.source, &b, Some((&a, &va))).unwrap();
    assert!(!c.has_error() && c.needs_confirmation());
    assert!(c.shares_disk_with_other);
}

#[test]
fn unknown_physical_identity_is_reported_not_assumed_separate() {
    let s = setup();
    let (a, va) = s.drive("A", "UA", None);
    let (b, _) = s.drive("B", "UB", Some("disk8"));
    let c = check_destination_choice(&s.resolver, &s.source, &b, Some((&a, &va))).unwrap();
    assert!(!c.has_error() && !c.needs_confirmation() && !c.shares_disk_with_other);
    assert!(
        c.findings
            .iter()
            .any(|f| f.severity == Severity::Warning && f.message.contains("cannot confirm"))
    );
    // distinct, known physical disks: silent
    let (a2, va2) = s.drive("A2", "UA2", Some("disk4"));
    let (b2, _) = s.drive("B2", "UB2", Some("disk8"));
    let c = check_destination_choice(&s.resolver, &s.source, &b2, Some((&a2, &va2))).unwrap();
    assert!(c.findings.is_empty());
}

#[test]
fn read_only_network_and_untested_filesystems() {
    let s = setup();
    let (dst, _) = s.drive("RO", "URO", Some("disk4"));
    let mut ro = s.resolver.mounted_volumes().unwrap();
    for v in &mut ro {
        if v.volume_uuid.as_deref() == Some("URO") {
            v.writable = false;
        }
    }
    s.resolver.set(ro);
    assert!(
        kinds(&check_destination_choice(&s.resolver, &s.source, &dst, None).unwrap())
            .contains(&Severity::Error)
    );

    let (dst, _) = s.drive("NAS", "UNAS", None);
    let mut v = s.resolver.mounted_volumes().unwrap();
    for x in &mut v {
        if x.volume_uuid.as_deref() == Some("UNAS") {
            x.fs_type = "smbfs".into();
            x.local_disk = false;
        }
    }
    s.resolver.set(v);
    assert!(
        check_destination_choice(&s.resolver, &s.source, &dst, None)
            .unwrap()
            .has_error()
    );

    let (dst, _) = s.drive("NTFS", "UNT", Some("disk5"));
    let mut v = s.resolver.mounted_volumes().unwrap();
    for x in &mut v {
        if x.volume_uuid.as_deref() == Some("UNT") {
            x.fs_type = "ntfs".into();
        }
    }
    s.resolver.set(v);
    let c = check_destination_choice(&s.resolver, &s.source, &dst, None).unwrap();
    assert!(!c.has_error() && c.needs_confirmation());
    assert!(c.findings[0].message.contains("not tested"));
}

#[test]
fn internal_disk_and_same_volume_as_source_are_flagged() {
    let s = setup();
    let on_internal = s.dr.mount("home/elsewhere");
    let mut v = s.resolver.mounted_volumes().unwrap();
    v[0].internal = true;
    s.resolver.set(v);
    let c = check_destination_choice(&s.resolver, &s.source, &on_internal, None).unwrap();
    assert!(c.findings.iter().any(|f| f.severity == Severity::Warning));
    assert!(c.needs_confirmation(), "same volume as the source");
}

// ------------------------------------------------------------- capabilities

#[test]
fn capability_probe_measures_case_sensitivity_and_cleans_up() {
    let d = tempfile::tempdir().unwrap();
    let root = DirFd::open_path(d.path()).unwrap();
    let v = vol(d.path(), "U", DriveMediaType::Ssd, None);
    let caps = detect_caps(&root, &v);
    fs::write(d.path().join("Aa"), b"x").unwrap();
    let really_insensitive = d.path().join("aa").exists();
    assert_eq!(caps.case_insensitive, really_insensitive);
    let leftovers: Vec<_> = fs::read_dir(d.path().join(".twinsync")).unwrap().collect();
    assert!(leftovers.is_empty(), "probe files must be removed");
}

#[test]
fn fat32_capabilities_come_from_the_filesystem_type_not_the_media() {
    let c = FsCapabilities::for_fs_type("msdos");
    assert_eq!(c.max_file_size, Some(u32::MAX as u64));
    assert!(c.restricted_names && !c.inode_stable);
    let c = FsCapabilities::for_fs_type("apfs");
    assert_eq!(c.max_file_size, None);
    assert!(c.inode_stable && !c.restricted_names);
}

#[test]
fn write_probe_detects_a_read_only_folder() {
    let d = tempfile::tempdir().unwrap();
    let root = DirFd::open_path(d.path()).unwrap();
    probe_writable(&root).unwrap();
    fs::remove_dir_all(d.path().join(".twinsync")).unwrap();
    fs::set_permissions(d.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let r = probe_writable(&root);
    fs::set_permissions(d.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.is_err());
}
