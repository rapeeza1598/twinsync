//! Directory-fd based filesystem primitives.
//!
//! Every destination (and source) access goes through a `DirFd` opened without
//! following symlinks, and every name is a single path component. Canonicalizing
//! a path once and then using it later does not stop a swapped directory or a
//! symlink planted in between; walking component by component with
//! `openat(O_NOFOLLOW)` does.

use crate::domain::Fingerprint;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn einval() -> io::Error {
    io::Error::from_raw_os_error(libc::EINVAL)
}

fn cname(name: &[u8]) -> io::Result<CString> {
    if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
        return Err(einval());
    }
    CString::new(name).map_err(|_| einval())
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// A symlink or non-directory where a directory was required.
pub fn is_path_escape(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
}

pub fn is_not_found(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENOENT)
}

#[derive(Clone, Copy)]
pub struct Stat(pub libc::stat);

impl Stat {
    fn mode_type(&self) -> libc::mode_t {
        self.0.st_mode & libc::S_IFMT
    }
    pub fn is_file(&self) -> bool {
        self.mode_type() == libc::S_IFREG
    }
    pub fn is_dir(&self) -> bool {
        self.mode_type() == libc::S_IFDIR
    }
    pub fn size(&self) -> u64 {
        self.0.st_size as u64
    }
    pub fn dev(&self) -> u64 {
        self.0.st_dev as u64
    }
    pub fn perm(&self) -> u32 {
        (self.0.st_mode & 0o777) as u32
    }
    pub fn mtime_ns(&self) -> i64 {
        self.0.st_mtime.saturating_mul(1_000_000_000) + self.0.st_mtime_nsec
    }
    pub fn ctime_ns(&self) -> i64 {
        self.0.st_ctime.saturating_mul(1_000_000_000) + self.0.st_ctime_nsec
    }
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            size: self.size(),
            mtime_ns: self.mtime_ns(),
            inode: self.0.st_ino,
        }
    }
}

pub fn fstat(file: &File) -> io::Result<Stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(file.as_raw_fd(), &mut st) })?;
    Ok(Stat(st))
}

/// Flush to stable storage as far as the platform allows. `F_FULLFSYNC` asks the
/// drive to flush its own cache; filesystems that lack it fall back to `fsync`.
pub fn sync_fd(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) } != -1 {
        return Ok(());
    }
    match cvt(unsafe { libc::fsync(fd) }) {
        Ok(_) => Ok(()),
        // Some filesystems cannot sync a directory at all; not a data error.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Best effort: ask the kernel to bypass the page cache for later reads of this fd.
pub fn no_cache(file: &File) {
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
    }
}

pub struct DirFd(OwnedFd);

impl DirFd {
    /// Open a directory by path. The final component must not be a symlink.
    pub fn open_path(path: &Path) -> io::Result<DirFd> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| einval())?;
        let fd = cvt(unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        Ok(DirFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub fn open_dir(&self, name: &[u8]) -> io::Result<DirFd> {
        let c = cname(name)?;
        let fd = cvt(unsafe {
            libc::openat(
                self.raw(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        Ok(DirFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn mkdir(&self, name: &[u8]) -> io::Result<()> {
        let c = cname(name)?;
        cvt(unsafe { libc::mkdirat(self.raw(), c.as_ptr(), 0o755) }).map(|_| ())
    }

    /// Open (creating if asked) every directory in `comps`, never following a symlink.
    pub fn walk(&self, comps: &[&[u8]], create: bool) -> io::Result<DirFd> {
        let mut cur = self.try_clone()?;
        for comp in comps {
            if create {
                match cur.mkdir(comp) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
                    Err(e) => return Err(e),
                }
            }
            cur = cur.open_dir(comp)?;
        }
        Ok(cur)
    }

    pub fn try_clone(&self) -> io::Result<DirFd> {
        Ok(DirFd(self.0.try_clone()?))
    }

    /// `lstat` of `name` inside this directory; `None` if it does not exist.
    pub fn stat(&self, name: &[u8]) -> io::Result<Option<Stat>> {
        let c = cname(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        match cvt(unsafe {
            libc::fstatat(self.raw(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
        }) {
            Ok(_) => Ok(Some(Stat(st))),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn stat_self(&self) -> io::Result<Stat> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstat(self.raw(), &mut st) })?;
        Ok(Stat(st))
    }

    pub fn open_read(&self, name: &[u8]) -> io::Result<File> {
        let c = cname(name)?;
        let fd = cvt(unsafe {
            libc::openat(
                self.raw(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Create a new file; fails if the name exists (never truncates or follows).
    pub fn create_new(&self, name: &[u8]) -> io::Result<File> {
        let c = cname(name)?;
        let fd = cvt(unsafe {
            libc::openat(
                self.raw(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        })?;
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Plain rename. Overwrites an existing target; only use where the target
    /// name is known to be unused (unique version paths).
    pub fn rename_to(&self, from: &[u8], to_dir: &DirFd, to: &[u8]) -> io::Result<()> {
        let (f, t) = (cname(from)?, cname(to)?);
        cvt(unsafe { libc::renameat(self.raw(), f.as_ptr(), to_dir.raw(), t.as_ptr()) }).map(|_| ())
    }

    /// Rename that fails with EEXIST instead of replacing the target.
    pub fn rename_excl(&self, from: &[u8], to_dir: &DirFd, to: &[u8]) -> io::Result<()> {
        let (f, t) = (cname(from)?, cname(to)?);
        cvt(unsafe {
            libc::renameatx_np(
                self.raw(),
                f.as_ptr(),
                to_dir.raw(),
                t.as_ptr(),
                libc::RENAME_EXCL,
            )
        })
        .map(|_| ())
    }

    pub fn unlink(&self, name: &[u8]) -> io::Result<()> {
        let c = cname(name)?;
        cvt(unsafe { libc::unlinkat(self.raw(), c.as_ptr(), 0) }).map(|_| ())
    }

    pub fn sync(&self) -> io::Result<()> {
        sync_fd(self.raw())
    }

    pub fn list(&self) -> io::Result<Vec<Vec<u8>>> {
        let dup = cvt(unsafe { libc::dup(self.raw()) })?;
        let dir = unsafe { libc::fdopendir(dup) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(dup) };
            return Err(e);
        }
        unsafe { libc::rewinddir(dir) };
        let mut out = Vec::new();
        loop {
            let ent = unsafe { libc::readdir(dir) };
            if ent.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                out.push(name.to_vec());
            }
        }
        unsafe { libc::closedir(dir) };
        Ok(out)
    }

    /// Bytes available to this user on the volume holding this directory.
    pub fn free_bytes(&self) -> io::Result<u64> {
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstatfs(self.raw(), &mut s) })?;
        Ok(s.f_bavail.saturating_mul(s.f_bsize as u64))
    }
}

/// `YYYYMMDDTHHMMSSZ` for a unix time, without a date library.
pub fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;

    #[test]
    fn stamp_matches_known_instants() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_700_000_000), "20231114T221320Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z"); // leap day
    }
}
