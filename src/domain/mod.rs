//! Core value types shared by every layer. No I/O happens here.

use crate::errors::{Error, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub type Id = String;

/// Per-destination directory that holds markers, temp files and version history.
/// It is never scanned, planned or synced.
pub const RESERVED_DIR: &str = ".twinsync";

pub fn new_id() -> Id {
    uuid::Uuid::new_v4().to_string()
}

/// Relative path stored as raw bytes so non-UTF-8 names round-trip exactly.
/// Construction rejects absolute paths, empty/`.`/`..` components and NUL.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(Vec<u8>);

impl RelPath {
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        let bad = || Error::BadRelPath(String::from_utf8_lossy(&bytes).into_owned());
        if bytes.is_empty() || bytes[0] == b'/' || bytes.contains(&0) {
            return Err(bad());
        }
        if bytes
            .split(|&b| b == b'/')
            .any(|c| c.is_empty() || c == b"." || c == b"..")
        {
            return Err(bad());
        }
        Ok(Self(bytes))
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        Self::from_bytes(path.as_os_str().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|&b| b == b'/')
    }

    pub fn file_name(&self) -> &[u8] {
        self.0.rsplit(|&b| b == b'/').next().unwrap_or(&[])
    }

    pub fn parent(&self) -> Option<RelPath> {
        self.0
            .iter()
            .rposition(|&b| b == b'/')
            .map(|i| RelPath(self.0[..i].to_vec()))
    }

    pub fn join(&self, name: &[u8]) -> Result<RelPath> {
        let mut v = self.0.clone();
        v.push(b'/');
        v.extend_from_slice(name);
        RelPath::from_bytes(v)
    }

    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.0.clone()))
    }

    pub fn is_reserved(&self) -> bool {
        self.components().next() == Some(RESERVED_DIR.as_bytes())
    }

    /// True if `self` equals `prefix` or lies below it.
    pub fn starts_with(&self, prefix: &RelPath) -> bool {
        self.0 == prefix.0
            || (self.0.len() > prefix.0.len()
                && self.0.starts_with(&prefix.0)
                && self.0[prefix.0.len()] == b'/')
    }

    /// Display only. Never use this string as an identity.
    pub fn display(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelPath({:?})", self.display())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn from_slice(b: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(b).ok().map(Hash)
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", &self.to_hex()[..12])
    }
}

/// Cheap change detector. `st_dev` and `ctime` are deliberately absent: the
/// device number changes on every replug and ctime is bumped by Spotlight and
/// Finder, so persisting either would make a reconnect look like mass change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i64,
    pub inode: u64,
}

impl Fingerprint {
    pub fn from_metadata(m: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            size: m.len(),
            mtime_ns: m.mtime().saturating_mul(1_000_000_000) + m.mtime_nsec(),
            inode: m.ino(),
        }
    }

    /// `use_inode` is false on filesystems where inode stability across
    /// remounts is not trusted (FAT family).
    pub fn same_as(&self, other: &Fingerprint, use_inode: bool) -> bool {
        self.size == other.size
            && self.mtime_ns == other.mtime_ns
            && (!use_inode || self.inode == other.inode)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    File,
    Dir,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file" => Some(EntryKind::File),
            "dir" => Some(EntryKind::Dir),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DriveMediaType {
    Ssd,
    Hdd,
    Unknown,
}

impl DriveMediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ssd => "ssd",
            Self::Hdd => "hdd",
            Self::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "ssd" => Self::Ssd,
            "hdd" => Self::Hdd,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DrivePerformanceProfile {
    Auto,
    Conservative,
    Balanced,
}

impl DrivePerformanceProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "conservative" => Self::Conservative,
            "balanced" => Self::Balanced,
            _ => Self::Auto,
        }
    }

    /// Resolve `Auto` using the media type. Only a confirmed SSD gets Balanced;
    /// HDD and Unknown stay conservative. Safety checks never depend on this.
    pub fn effective(self, media: DriveMediaType) -> Self {
        match (self, media) {
            (Self::Auto, DriveMediaType::Ssd) => Self::Balanced,
            (Self::Auto, _) => Self::Conservative,
            (p, _) => p,
        }
    }

    pub fn chunk_size(self, media: DriveMediaType) -> usize {
        match self.effective(media) {
            Self::Balanced => 4 << 20,
            _ => 1 << 20,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::A => "A",
            Slot::B => "B",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "A" => Some(Slot::A),
            "B" => Some(Slot::B),
            _ => None,
        }
    }
    pub fn default_label(self) -> String {
        format!("Drive {}", self.as_str())
    }
}

/// Reachability of a destination, independent of what its worker is doing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Connectivity {
    Online {
        mount: PathBuf,
    },
    Offline,
    /// Volume present but identity does not match what was configured. Never written to.
    IdentityMismatch(String),
    /// Duplicate UUIDs or no UUID: stop and ask the user.
    NeedsConfirmation(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkerState {
    Idle,
    Scanning,
    Syncing,
    Verifying,
    Stopping,
    WaitingForDrive,
    Paused,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DestStatus {
    NotConfigured,
    Offline,
    Idle,
    Scanning,
    Syncing,
    UpToDate,
    Paused,
    Stopping,
    WaitingForDrive,
    NeedsAttention,
    SourceUnavailable,
    PermissionDenied,
    Error,
}

/// Everything needed to derive the single status shown on a destination card.
pub struct StatusInputs {
    pub configured: bool,
    pub connectivity: Connectivity,
    pub worker: WorkerState,
    pub source_unavailable: bool,
    pub permission_denied: bool,
    pub has_error: bool,
    pub attention_items: usize,
    /// True only when the latest source generation was scanned completely and
    /// this destination was planned against it with nothing pending.
    pub verified_up_to_date: bool,
}

pub fn derive_status(i: &StatusInputs) -> DestStatus {
    if !i.configured {
        return DestStatus::NotConfigured;
    }
    match &i.connectivity {
        Connectivity::Offline => return DestStatus::Offline,
        Connectivity::IdentityMismatch(_) | Connectivity::NeedsConfirmation(_) => {
            return DestStatus::NeedsAttention;
        }
        Connectivity::Online { .. } => {}
    }
    if i.source_unavailable {
        return DestStatus::SourceUnavailable;
    }
    if i.permission_denied {
        return DestStatus::PermissionDenied;
    }
    match i.worker {
        WorkerState::Stopping => return DestStatus::Stopping,
        WorkerState::WaitingForDrive => return DestStatus::WaitingForDrive,
        WorkerState::Scanning | WorkerState::Verifying => return DestStatus::Scanning,
        WorkerState::Syncing => return DestStatus::Syncing,
        WorkerState::Paused => return DestStatus::Paused,
        WorkerState::Idle => {}
    }
    if i.has_error {
        DestStatus::Error
    } else if i.attention_items > 0 {
        DestStatus::NeedsAttention
    } else if i.verified_up_to_date {
        DestStatus::UpToDate
    } else {
        DestStatus::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_rejects_unsafe() {
        for bad in ["", "/abs", "a//b", "a/./b", "../x", "a/..", "a\0b", "a/"] {
            assert!(RelPath::from_bytes(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert!(RelPath::from_bytes(&b"a/b c/\xff\xfe"[..]).is_ok());
    }

    #[test]
    fn relpath_roundtrips_non_utf8() {
        let raw = b"dir/\xff\xfe.txt".to_vec();
        let p = RelPath::from_bytes(raw.clone()).unwrap();
        assert_eq!(p.as_bytes(), raw.as_slice());
        assert_eq!(RelPath::from_path(&p.to_path_buf()).unwrap(), p);
    }

    #[test]
    fn relpath_prefix_and_parent() {
        let p = RelPath::from_bytes(&b"a/b/c"[..]).unwrap();
        let a = RelPath::from_bytes(&b"a"[..]).unwrap();
        let ab = RelPath::from_bytes(&b"a/bb"[..]).unwrap();
        assert!(p.starts_with(&a));
        assert!(!ab.starts_with(&RelPath::from_bytes(&b"a/b"[..]).unwrap()));
        assert_eq!(p.parent().unwrap().as_bytes(), b"a/b");
        assert!(
            RelPath::from_bytes(&b".twinsync/x"[..])
                .unwrap()
                .is_reserved()
        );
    }

    #[test]
    fn unknown_media_stays_conservative() {
        let auto = DrivePerformanceProfile::Auto;
        assert_eq!(auto.chunk_size(DriveMediaType::Unknown), 1 << 20);
        assert_eq!(auto.chunk_size(DriveMediaType::Hdd), 1 << 20);
        assert_eq!(auto.chunk_size(DriveMediaType::Ssd), 4 << 20);
    }
}
