//! Drive identity, destination validation and filesystem capability detection.
//!
//! Drives are identified by volume UUID (plus an on-drive marker, checked by the
//! engine), never by name or mount path. Media type comes only from what macOS
//! reports; when it reports nothing the drive is `Unknown` and treated
//! conservatively. Nothing here guesses SSD/HDD from a name or a size.

use crate::domain::{DriveMediaType, FsCapabilities, RESERVED_DIR};
use crate::errors::{Error, IoContext, Result};
use crate::storage::DestinationRow;
use crate::transfer::fsops::DirFd;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub mount_point: PathBuf,
    pub bsd_name: String,
    pub volume_uuid: Option<String>,
    pub volume_name: String,
    pub fs_type: String,
    pub media: DriveMediaType,
    pub bus_protocol: Option<String>,
    pub internal: bool,
    pub writable: bool,
    pub ejectable: bool,
    /// False for network and virtual filesystems.
    pub local_disk: bool,
    /// Physical whole disk (e.g. `disk4`) when known. Two volumes with the same
    /// value share a device; `None` means "cannot tell", not "different".
    pub physical_disk: Option<String>,
    pub media_name: Option<String>,
    pub total_bytes: u64,
    /// `st_dev` of the mount point. Only meaningful while mounted; compared
    /// against the opened destination root before every write batch.
    pub dev: u64,
}

pub trait DeviceResolver: Send + Sync {
    fn volume_for_path(&self, path: &Path) -> Result<VolumeInfo>;
    fn mounted_volumes(&self) -> Result<Vec<VolumeInfo>>;
}

pub trait Ejector: Send + Sync {
    /// Every mounted volume that would go away if `vol`'s disk were ejected.
    fn siblings(&self, vol: &VolumeInfo) -> Result<Vec<VolumeInfo>>;
    /// Ask the OS to eject. `Ok` means the OS said so; the caller still checks
    /// that the volume actually left the mount table. Never forces.
    fn eject(&self, vol: &VolumeInfo) -> Result<()>;
}

// ---------------------------------------------------------------- macOS

pub fn media_from_solid_state(solid_state: Option<bool>) -> DriveMediaType {
    match solid_state {
        Some(true) => DriveMediaType::Ssd,
        Some(false) => DriveMediaType::Hdd,
        None => DriveMediaType::Unknown,
    }
}

#[derive(Default)]
pub struct MacResolver {
    cache: Mutex<HashMap<(String, PathBuf), VolumeInfo>>,
}

impl MacResolver {
    pub fn new() -> Self {
        Self::default()
    }

    fn describe(&self, m: &crate::platform::macos::MountEntry) -> Result<VolumeInfo> {
        use crate::platform::macos as mac;
        let dev = std::fs::metadata(&m.mount_point)
            .map(|md| md.dev())
            .ctx("stat mount point")?;
        let Some(bsd) = mac::bsd_name_of(&m.from_name) else {
            // Network or virtual filesystem.
            return Ok(VolumeInfo {
                mount_point: m.mount_point.clone(),
                bsd_name: String::new(),
                volume_uuid: None,
                volume_name: m
                    .mount_point
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                fs_type: m.fs_type.clone(),
                media: DriveMediaType::Unknown,
                bus_protocol: None,
                internal: false,
                writable: !m.read_only(),
                ejectable: false,
                local_disk: false,
                physical_disk: None,
                media_name: None,
                total_bytes: 0,
                dev,
            });
        };
        let key = (bsd.clone(), m.mount_point.clone());
        if let Some(v) = self.cache.lock().expect("cache lock").get(&key) {
            // Re-read the cheap parts: dev changes on remount, flags can change.
            let mut v = v.clone();
            v.dev = dev;
            v.writable = v.writable && !m.read_only();
            return Ok(v);
        }
        let info = mac::diskutil_info(&bsd)?;
        let v = VolumeInfo {
            mount_point: m.mount_point.clone(),
            bsd_name: bsd.clone(),
            volume_uuid: info.volume_uuid,
            volume_name: info.volume_name,
            fs_type: if info.fs_type.is_empty() {
                m.fs_type.clone()
            } else {
                info.fs_type
            },
            media: media_from_solid_state(info.solid_state),
            bus_protocol: info.bus_protocol,
            internal: info.internal,
            writable: info.writable && !m.read_only(),
            ejectable: info.ejectable,
            local_disk: true,
            physical_disk: info.physical_disk,
            media_name: info.media_name,
            total_bytes: info.total_bytes,
            dev,
        };
        self.cache
            .lock()
            .expect("cache lock")
            .insert(key, v.clone());
        Ok(v)
    }
}

impl DeviceResolver for MacResolver {
    fn volume_for_path(&self, path: &Path) -> Result<VolumeInfo> {
        let m = crate::platform::macos::mount_of_path(path).ctx("statfs")?;
        self.describe(&m)
    }

    fn mounted_volumes(&self) -> Result<Vec<VolumeInfo>> {
        let all = crate::platform::macos::mounts().ctx("read mount table")?;
        let mut out = Vec::new();
        let mut live = Vec::new();
        for m in all.iter().filter(|m| m.local()) {
            let p = m.mount_point.to_string_lossy();
            let system_noise = (p.starts_with("/System/Volumes/") && p != "/System/Volumes/Data")
                || p.starts_with("/private/")
                || p == "/dev";
            if system_noise || crate::platform::macos::bsd_name_of(&m.from_name).is_none() {
                continue;
            }
            live.push((
                crate::platform::macos::bsd_name_of(&m.from_name).unwrap_or_default(),
                m.mount_point.clone(),
            ));
            // A volume that fails to describe (just ejected, diskutil hiccup) is skipped.
            if let Ok(v) = self.describe(m) {
                out.push(v);
            }
        }
        self.cache
            .lock()
            .expect("cache lock")
            .retain(|k, _| live.contains(k));
        Ok(out)
    }
}

pub struct MacEjector<R: DeviceResolver>(pub std::sync::Arc<R>);

fn eject_target(vol: &VolumeInfo) -> String {
    vol.physical_disk
        .clone()
        .unwrap_or_else(|| crate::platform::macos::whole_disk_of(&vol.bsd_name))
}

impl<R: DeviceResolver> Ejector for MacEjector<R> {
    fn siblings(&self, vol: &VolumeInfo) -> Result<Vec<VolumeInfo>> {
        let target = eject_target(vol);
        Ok(self
            .0
            .mounted_volumes()?
            .into_iter()
            .filter(|v| eject_target(v) == target)
            .collect())
    }

    fn eject(&self, vol: &VolumeInfo) -> Result<()> {
        crate::platform::macos::eject_disk(&eject_target(vol))
    }
}

// ------------------------------------------------------------------ fakes

/// In-memory resolver for tests and headless engine runs.
#[derive(Default)]
pub struct FakeResolver {
    volumes: Mutex<Vec<VolumeInfo>>,
}

impl FakeResolver {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, v: Vec<VolumeInfo>) {
        *self.volumes.lock().expect("fake lock") = v;
    }
    pub fn add(&self, v: VolumeInfo) {
        self.volumes.lock().expect("fake lock").push(v);
    }
    pub fn remove_mount(&self, mount: &Path) {
        self.volumes
            .lock()
            .expect("fake lock")
            .retain(|v| v.mount_point != mount);
    }
    /// Same drive reappearing somewhere else.
    pub fn move_volume(&self, uuid: &str, new_mount: &Path) {
        for v in self.volumes.lock().expect("fake lock").iter_mut() {
            if v.volume_uuid.as_deref() == Some(uuid) {
                v.mount_point = new_mount.to_path_buf();
                v.dev = std::fs::metadata(new_mount).map_or(v.dev, |m| m.dev());
            }
        }
    }
}

impl DeviceResolver for FakeResolver {
    fn volume_for_path(&self, path: &Path) -> Result<VolumeInfo> {
        self.volumes
            .lock()
            .expect("fake lock")
            .iter()
            .filter(|v| path.starts_with(&v.mount_point))
            .max_by_key(|v| v.mount_point.as_os_str().len())
            .cloned()
            .ok_or_else(|| Error::Identity(format!("no volume contains {}", path.display())))
    }
    fn mounted_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.volumes.lock().expect("fake lock").clone())
    }
}

/// Fake ejector: records calls, optionally fails, and on success unmounts from a `FakeResolver`.
pub struct FakeEjector {
    pub resolver: std::sync::Arc<FakeResolver>,
    pub fail_with: Mutex<Option<String>>,
    /// Report success but leave the volume mounted (a lying eject).
    pub leave_mounted: Mutex<bool>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeEjector {
    pub fn new(resolver: std::sync::Arc<FakeResolver>) -> Self {
        Self {
            resolver,
            fail_with: Mutex::new(None),
            leave_mounted: Mutex::new(false),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl Ejector for FakeEjector {
    fn siblings(&self, vol: &VolumeInfo) -> Result<Vec<VolumeInfo>> {
        Ok(self
            .resolver
            .mounted_volumes()?
            .into_iter()
            .filter(|v| v.physical_disk.is_some() && v.physical_disk == vol.physical_disk)
            .collect())
    }
    fn eject(&self, vol: &VolumeInfo) -> Result<()> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(vol.mount_point.display().to_string());
        if let Some(msg) = self.fail_with.lock().expect("fail lock").clone() {
            return Err(Error::Identity(msg));
        }
        if !*self.leave_mounted.lock().expect("lie lock") {
            self.resolver.remove_mount(&vol.mount_point);
        }
        Ok(())
    }
}

// ---------------------------------------------------------- identity match

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    Offline,
    /// Exactly one volume carries the recorded UUID. The marker still has to be verified.
    Candidate(VolumeInfo),
    /// Duplicate UUIDs: cannot tell which is the drive. Stop and ask.
    Ambiguous(String),
    /// Nothing recorded to match on (legacy import, volume without UUID).
    NoIdentity(String),
}

pub fn find_volume(volumes: &[VolumeInfo], dest: &DestinationRow) -> Presence {
    let Some(uuid) = &dest.volume_uuid else {
        return Presence::NoIdentity(
            "no volume UUID is recorded for this drive; confirm it before TwinSync writes to it"
                .into(),
        );
    };
    let matches: Vec<&VolumeInfo> = volumes
        .iter()
        .filter(|v| v.volume_uuid.as_deref() == Some(uuid))
        .collect();
    match matches.as_slice() {
        [] => Presence::Offline,
        [one] => Presence::Candidate((*one).clone()),
        many => Presence::Ambiguous(format!("{} mounted volumes share UUID {uuid}", many.len())),
    }
}

// ------------------------------------------------------------- validation

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Cannot be used.
    Error,
    /// Usable only if the user explicitly confirms.
    NeedsConfirm,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

fn f(severity: Severity, message: impl Into<String>) -> Finding {
    Finding {
        severity,
        message: message.into(),
    }
}

#[derive(Clone, Debug)]
pub struct DestCheck {
    pub canonical: PathBuf,
    pub volume: VolumeInfo,
    /// Destination folder relative to the volume's mount point.
    pub root_rel: PathBuf,
    pub findings: Vec<Finding>,
    /// Both destinations are on one physical disk: use a shared I/O limiter.
    pub shares_disk_with_other: bool,
}

impl DestCheck {
    pub fn has_error(&self) -> bool {
        self.findings.iter().any(|x| x.severity == Severity::Error)
    }
    pub fn needs_confirmation(&self) -> bool {
        self.findings
            .iter()
            .any(|x| x.severity == Severity::NeedsConfirm)
    }
}

const NETWORK_FS: &[&str] = &["smbfs", "nfs", "afpfs", "webdav", "cifs", "ftp", "autofs"];
const TESTED_FS: &[&str] = &["apfs", "hfs", "exfat", "msdos"];

fn volumes_same(a: &VolumeInfo, b: &VolumeInfo) -> bool {
    match (&a.volume_uuid, &b.volume_uuid) {
        (Some(x), Some(y)) => x == y,
        _ => a.mount_point == b.mount_point,
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Validate one chosen destination folder. Pure apart from `canonicalize`;
/// nothing is written here (see `probe_writable` for the write probe).
pub fn check_destination_choice(
    resolver: &dyn DeviceResolver,
    source: &Path,
    dest: &Path,
    other: Option<(&Path, &VolumeInfo)>,
) -> Result<DestCheck> {
    let src = std::fs::canonicalize(source).ctx("resolve source folder")?;
    let dst = std::fs::canonicalize(dest).ctx("resolve destination folder")?;
    if !dst.is_dir() {
        return Err(Error::PathSafety("destination is not a folder".into()));
    }
    let vol = resolver.volume_for_path(&dst)?;
    let root_rel = match dst.strip_prefix(&vol.mount_point) {
        Ok(r) => r.to_path_buf(),
        Err(_) if vol.mount_point == Path::new("/System/Volumes/Data") => {
            dst.strip_prefix("/").unwrap_or(&dst).to_path_buf()
        }
        Err(_) => {
            return Err(Error::PathSafety(
                "destination is not inside the volume it resolves to".into(),
            ));
        }
    };

    let mut findings = Vec::new();
    if overlaps(&src, &dst) {
        findings.push(f(
            Severity::Error,
            "source and destination overlap: one is inside the other",
        ));
    }
    if !vol.local_disk || NETWORK_FS.contains(&vol.fs_type.as_str()) {
        findings.push(f(
            Severity::Error,
            "network shares and virtual filesystems are not supported; use a drive connected by USB or Thunderbolt",
        ));
    }
    if !vol.writable {
        findings.push(f(Severity::Error, "this volume is read-only"));
    }
    if vol.local_disk && !TESTED_FS.contains(&vol.fs_type.as_str()) {
        let msg = if vol.fs_type == "ntfs" {
            "NTFS is not tested. If it is mounted read-only it cannot be used; if a driver makes it writable, \
             TwinSync does not claim full support"
                .to_string()
        } else {
            format!(
                "filesystem {:?} has not been tested with TwinSync",
                vol.fs_type
            )
        };
        findings.push(f(Severity::NeedsConfirm, msg));
    }
    if vol.volume_uuid.is_none() {
        findings.push(f(
            Severity::NeedsConfirm,
            "this volume reports no UUID, so TwinSync can only recognize it by a marker file plus its name and size; \
             you will be asked to confirm it each time it is mounted",
        ));
    }
    if vol.internal {
        findings.push(f(
            Severity::Warning,
            "this folder is on a disk inside this Mac, not on an external drive",
        ));
    }
    if let Ok(sv) = resolver.volume_for_path(&src)
        && volumes_same(&sv, &vol)
    {
        findings.push(f(
            Severity::NeedsConfirm,
            "destination is on the same volume as the source, so it is not a copy on another drive",
        ));
    }

    let mut shares = false;
    if let Some((other_path, other_vol)) = other {
        if overlaps(&dst, other_path) {
            findings.push(f(Severity::Error, "Drive A and Drive B folders overlap"));
        }
        if volumes_same(&vol, other_vol) {
            findings.push(f(
                Severity::Error,
                "Drive A and Drive B are the same volume; choose two different drives",
            ));
        } else {
            match (&vol.physical_disk, &other_vol.physical_disk) {
                (Some(a), Some(b)) if a == b => {
                    shares = true;
                    findings.push(f(
                        Severity::NeedsConfirm,
                        "both destinations are on the same physical disk, so this is not two separate devices; \
                         copies will share one I/O queue",
                    ));
                }
                (Some(_), Some(_)) => {}
                _ => findings.push(f(
                    Severity::Warning,
                    "TwinSync cannot confirm that these are two separate physical devices",
                )),
            }
        }
    }
    Ok(DestCheck {
        canonical: dst,
        volume: vol,
        root_rel,
        findings,
        shares_disk_with_other: shares,
    })
}

/// Create and remove a probe file under `.twinsync/` to confirm the folder is writable.
pub fn probe_writable(root: &DirFd) -> Result<()> {
    let dir = root
        .walk(&[RESERVED_DIR.as_bytes()], true)
        .ctx("create .twinsync")?;
    let name = format!(".write-probe-{}", crate::domain::new_id());
    drop(dir.create_new(name.as_bytes()).ctx("write probe file")?);
    dir.unlink(name.as_bytes()).ctx("remove probe file")?;
    Ok(())
}

/// Capabilities of the mounted volume, with case sensitivity measured rather than assumed.
pub fn detect_caps(root: &DirFd, vol: &VolumeInfo) -> FsCapabilities {
    let mut caps = FsCapabilities::for_fs_type(&vol.fs_type);
    if let Ok(dir) = root.walk(&[RESERVED_DIR.as_bytes()], true) {
        let id = crate::domain::new_id();
        let upper = format!(".case-probe-{id}-A");
        let lower = format!(".case-probe-{id}-a");
        if dir.create_new(upper.as_bytes()).is_ok() {
            if let Ok(found) = dir.stat(lower.as_bytes()) {
                caps.case_insensitive = found.is_some();
            }
            let _ = dir.unlink(upper.as_bytes());
        }
    }
    caps
}
