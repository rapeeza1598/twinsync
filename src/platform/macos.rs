use crate::errors::{Error, Result};
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const DISKUTIL: &str = "/usr/sbin/diskutil";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub from_name: String,
    pub fs_type: String,
    pub flags: u32,
}

impl MountEntry {
    pub fn read_only(&self) -> bool {
        self.flags & libc::MNT_RDONLY as u32 != 0
    }
    pub fn local(&self) -> bool {
        self.flags & libc::MNT_LOCAL as u32 != 0
    }
}

fn field(buf: &[libc::c_char]) -> Vec<u8> {
    buf.iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect()
}

fn entry(s: &libc::statfs) -> MountEntry {
    MountEntry {
        mount_point: PathBuf::from(OsString::from_vec(field(&s.f_mntonname))),
        from_name: String::from_utf8_lossy(&field(&s.f_mntfromname)).into_owned(),
        fs_type: String::from_utf8_lossy(&field(&s.f_fstypename)).into_owned(),
        flags: s.f_flags,
    }
}

/// The mount table. `MNT_NOWAIT` returns cached data and never touches a drive,
/// so polling this cannot wake a sleeping HDD.
pub fn mounts() -> io::Result<Vec<MountEntry>> {
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; n as usize];
    let bytes = (buf.len() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(got as usize);
    Ok(buf.iter().map(entry).collect())
}

/// Mount entry of the filesystem containing `path`.
pub fn mount_of_path(path: &Path) -> io::Result<MountEntry> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(entry(&s))
}

/// `disk<N>(s<M>)*` and nothing else. This is the only string ever passed to
/// `diskutil` that originated outside our own constants; it cannot start with
/// `-`, contain a path separator, or carry whitespace.
pub fn valid_bsd_name(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("disk") else {
        return false;
    };
    let mut parts = rest.split('s');
    let first_ok = parts
        .next()
        .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    first_ok && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `/dev/disk4s1` -> `disk4s1`
pub fn bsd_name_of(from_name: &str) -> Option<String> {
    let n = from_name.strip_prefix("/dev/")?;
    valid_bsd_name(n).then(|| n.to_string())
}

/// `disk4s1` -> `disk4`
pub fn whole_disk_of(bsd: &str) -> String {
    match bsd.strip_prefix("disk") {
        Some(rest) => {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            format!("disk{digits}")
        }
        None => bsd.to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    pub bsd_name: String,
    pub volume_uuid: Option<String>,
    pub volume_name: String,
    pub fs_type: String,
    /// `None` when macOS does not report it (common behind USB bridges).
    pub solid_state: Option<bool>,
    pub bus_protocol: Option<String>,
    pub internal: bool,
    pub writable: bool,
    pub ejectable: bool,
    /// Physical whole disk when known; for APFS, the disk behind the container.
    pub physical_disk: Option<String>,
    pub media_name: Option<String>,
    pub total_bytes: u64,
}

/// Parse `diskutil info -plist` output. Pure, so it is tested against fixtures.
pub fn parse_diskutil_info(xml: &[u8]) -> Result<DiskInfo> {
    let value = plist::Value::from_reader(io::Cursor::new(xml))
        .map_err(|e| Error::Identity(format!("unreadable diskutil output: {e}")))?;
    let d = value
        .as_dictionary()
        .ok_or_else(|| Error::Identity("diskutil output is not a dictionary".into()))?;
    let s = |k: &str| {
        d.get(k)
            .and_then(|v| v.as_string())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let b = |k: &str| d.get(k).and_then(|v| v.as_boolean());
    let bsd_name = s("DeviceIdentifier")
        .filter(|n| valid_bsd_name(n))
        .ok_or_else(|| Error::Identity("diskutil reported no device identifier".into()))?;
    let physical_disk = d
        .get("APFSPhysicalStores")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_dictionary())
        .and_then(|st| st.get("APFSPhysicalStore"))
        .and_then(|v| v.as_string())
        .map(whole_disk_of)
        .or_else(|| s("ParentWholeDisk"));
    Ok(DiskInfo {
        bsd_name,
        volume_uuid: s("VolumeUUID"),
        volume_name: s("VolumeName").unwrap_or_default(),
        fs_type: s("FilesystemType").unwrap_or_default().to_lowercase(),
        solid_state: b("SolidState"),
        bus_protocol: s("BusProtocol"),
        internal: b("Internal").unwrap_or(false),
        writable: b("WritableVolume").or(b("Writable")).unwrap_or(false),
        ejectable: b("Ejectable").unwrap_or(false),
        physical_disk,
        media_name: s("MediaName"),
        total_bytes: d
            .get("TotalSize")
            .or_else(|| d.get("Size"))
            .and_then(|v| v.as_unsigned_integer())
            .unwrap_or(0),
    })
}

/// Arguments go straight to `execve`; no shell is involved, and the only
/// variable part is a validated BSD name.
pub fn diskutil_info(bsd: &str) -> Result<DiskInfo> {
    if !valid_bsd_name(bsd) {
        return Err(Error::Identity(format!("refusing to query {bsd:?}")));
    }
    let out = Command::new(DISKUTIL)
        .args(["info", "-plist", bsd])
        .output()
        .map_err(|e| Error::io("run diskutil", e))?;
    if !out.status.success() {
        return Err(Error::Identity(format!(
            "diskutil info {bsd} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    parse_diskutil_info(&out.stdout)
}

/// Ask macOS to eject a whole disk. No force flag. Success means diskutil
/// exited 0; callers must still confirm the volume left the mount table.
pub fn eject_disk(whole_disk: &str) -> Result<()> {
    if !valid_bsd_name(whole_disk) || whole_disk != whole_disk_of(whole_disk) {
        return Err(Error::Identity(format!("refusing to eject {whole_disk:?}")));
    }
    let out = Command::new(DISKUTIL)
        .args(["eject", whole_disk])
        .output()
        .map_err(|e| Error::io("run diskutil", e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Identity(format!(
            "eject failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsd_names_are_strictly_validated() {
        for ok in ["disk0", "disk4s1", "disk12s3s2"] {
            assert!(valid_bsd_name(ok), "{ok}");
        }
        for bad in [
            "",
            "disk",
            "diskS1",
            "disk4s",
            "disk4 s1",
            "-fdisk4",
            "disk4;rm",
            "/dev/disk4",
            "../disk4",
            "disk4\n",
            "disk4s1/..",
            "Disk4",
            "--help",
        ] {
            assert!(!valid_bsd_name(bad), "{bad:?}");
        }
        assert_eq!(bsd_name_of("/dev/disk4s1").as_deref(), Some("disk4s1"));
        assert_eq!(bsd_name_of("map auto_home"), None);
        assert_eq!(whole_disk_of("disk4s1"), "disk4");
        assert_eq!(whole_disk_of("disk12"), "disk12");
    }

    #[test]
    fn eject_refuses_non_whole_disk_names() {
        assert!(eject_disk("disk4s1").is_err());
        assert!(eject_disk("-force").is_err());
    }
}
