//! Read-only walk of the source folder. Never writes, never follows symlinks.
//!
//! A scan that could not read everything says so (`complete = false`) and
//! lists the unreadable prefixes. Callers must treat unseen paths as unknown,
//! never as deleted. A missing or unreadable root is `SourceUnavailable`, never
//! an empty folder.

use crate::domain::{EntryKind, Fingerprint, Hash, RelPath};
use crate::errors::{Error, Result};
use crate::storage::SourceEntry;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkipReason {
    Symlink,
    /// fifo, socket, block/char device.
    SpecialFile,
    /// iCloud-style placeholder: reading it would trigger a download.
    CloudPlaceholder,
    /// Mount point of another filesystem below the source.
    OtherFilesystem,
}

impl SkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Symlink => "symbolic links are not followed",
            Self::SpecialFile => "special files (device, socket, fifo) are not supported",
            Self::CloudPlaceholder => "cloud placeholder, content is not stored locally",
            Self::OtherFilesystem => "another filesystem is mounted here",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    /// Sorted path-wise, so a directory's contents are contiguous.
    pub entries: Vec<SourceEntry>,
    pub complete: bool,
    /// Directories or files that could not be read. Everything below is unknown.
    pub unreadable: Vec<RelPath>,
    pub skipped: Vec<(RelPath, SkipReason)>,
    pub excluded_count: u64,
}

impl ScanResult {
    /// True when nothing TwinSync would act on differs: same entries (size, mtime,
    /// inode, hash), same unreadable and skipped paths. The count of excluded files is
    /// deliberately ignored: a new `.DS_Store` is not a change.
    pub fn same_content_as(&self, other: &ScanResult) -> bool {
        self.complete == other.complete
            && self.entries == other.entries
            && self.unreadable == other.unreadable
            && self.skipped == other.skipped
    }

    pub fn covers(&self, rel: &RelPath) -> bool {
        self.complete || !self.unreadable.iter().any(|u| rel.starts_with(u))
    }
}

#[derive(Debug)]
pub enum ScanOutcome {
    SourceUnavailable(String),
    Done(ScanResult),
}

pub struct Exclusions(GlobSet);

impl Exclusions {
    pub fn new(patterns: &[String]) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(Glob::new(p).map_err(|e| Error::Config(format!("bad exclusion {p:?}: {e}")))?);
        }
        Ok(Self(b.build().map_err(|e| Error::Config(e.to_string()))?))
    }

    /// Whether a path below `root` is excluded, or lies inside an excluded folder.
    /// Used on file-system events so changes to ignored files never wake the drives.
    pub fn matches_path(&self, root: &Path, abs: &Path) -> bool {
        let Ok(rel) = abs.strip_prefix(root) else {
            return false;
        };
        let mut prefix = std::path::PathBuf::new();
        for comp in rel.components() {
            prefix.push(comp);
            if self.0.is_match(comp.as_os_str()) || self.0.is_match(&prefix) {
                return true;
            }
        }
        false
    }

    fn is_excluded(&self, name: &OsStr, rel: &RelPath) -> bool {
        self.0.is_match(Path::new(name)) || self.0.is_match(rel.to_path_buf())
    }
}

#[cfg(target_os = "macos")]
fn is_dataless(m: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    m.st_flags() & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
fn is_dataless(_: &fs::Metadata) -> bool {
    false
}

pub fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<Hash> {
    let mut f =
        fs::File::open(path).map_err(|e| Error::io(format!("open {}", path.display()), e))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = f
            .read(&mut buf)
            .map_err(|e| Error::io(format!("read {}", path.display()), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Hash(*h.finalize().as_bytes()))
}

/// Walk `root`. With `hash_all`, every file is also content-hashed (slow; used
/// to catch changes that left size/mtime/inode untouched).
pub fn scan_source(
    root: &Path,
    exclusions: &Exclusions,
    hash_all: bool,
    cancel: &AtomicBool,
) -> Result<ScanOutcome> {
    let root_meta = match fs::metadata(root) {
        Ok(m) if m.is_dir() => m,
        Ok(_) => {
            return Ok(ScanOutcome::SourceUnavailable(
                "source is not a folder".into(),
            ));
        }
        Err(e) => return Ok(ScanOutcome::SourceUnavailable(e.to_string())),
    };
    if let Err(e) = fs::read_dir(root) {
        return Ok(ScanOutcome::SourceUnavailable(e.to_string()));
    }
    let root_dev = root_meta.dev();

    let mut out = ScanResult {
        complete: true,
        ..Default::default()
    };
    // (absolute dir, its rel path or None for root)
    let mut stack: Vec<(std::path::PathBuf, Option<RelPath>)> = vec![(root.to_path_buf(), None)];

    while let Some((dir, dir_rel)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => match dir_rel {
                // The root vanished or became unreadable mid-scan.
                None => return Ok(ScanOutcome::SourceUnavailable(e.to_string())),
                Some(rel) => {
                    mark_unreadable(&mut out, rel);
                    continue;
                }
            },
        };
        for item in rd {
            let Ok(item) = item else {
                out.complete = false;
                if let Some(d) = &dir_rel {
                    out.unreadable.push(d.clone());
                }
                continue;
            };
            let name = item.file_name();
            let rel = match &dir_rel {
                Some(d) => d.join(name.as_bytes()),
                None => RelPath::from_bytes(name.as_bytes()),
            };
            let Ok(rel) = rel else {
                out.complete = false;
                continue;
            };
            if exclusions.is_excluded(&name, &rel) {
                out.excluded_count += 1;
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(item.path()) else {
                mark_unreadable(&mut out, rel);
                continue;
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                out.skipped.push((rel, SkipReason::Symlink));
            } else if ft.is_dir() {
                if meta.dev() != root_dev {
                    out.skipped.push((rel, SkipReason::OtherFilesystem));
                    continue;
                }
                out.entries.push(SourceEntry {
                    rel: rel.clone(),
                    kind: EntryKind::Dir,
                    fp: Fingerprint {
                        size: 0,
                        mtime_ns: 0,
                        inode: meta.ino(),
                    },
                    hash: None,
                });
                stack.push((item.path(), Some(rel)));
            } else if ft.is_file() {
                if is_dataless(&meta) {
                    out.skipped.push((rel, SkipReason::CloudPlaceholder));
                    continue;
                }
                let hash = if hash_all {
                    match hash_file(&item.path(), cancel) {
                        Ok(h) => Some(h),
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(_) => {
                            mark_unreadable(&mut out, rel);
                            continue;
                        }
                    }
                } else {
                    None
                };
                out.entries.push(SourceEntry {
                    rel,
                    kind: EntryKind::File,
                    fp: Fingerprint::from_metadata(&meta),
                    hash,
                });
            } else {
                out.skipped.push((rel, SkipReason::SpecialFile));
            }
        }
    }
    out.entries
        .sort_by(|a, b| a.rel.components().cmp(b.rel.components()));
    Ok(ScanOutcome::Done(out))
}

fn mark_unreadable(out: &mut ScanResult, rel: RelPath) {
    out.complete = false;
    out.unreadable.push(rel);
}
