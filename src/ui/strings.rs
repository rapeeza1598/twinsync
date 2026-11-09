//! Every user-visible string, in one place so the UI can be localized later.
//! English only for the MVP.

use crate::domain::{
    Connectivity, DestStatus, DriveMediaType, DrivePerformanceProfile, WorkerState,
};
use crate::transfer::fsops::utc_stamp;

pub const TAGLINE: &str = crate::config::TAGLINE;
pub const NAV_OVERVIEW: &str = "Overview";
pub const NAV_ACTIVITY: &str = "Activity";
pub const NAV_SETTINGS: &str = "Settings";

pub const ALL_UP_TO_DATE: &str = "All up to date";
pub const NOT_CHECKED: &str = "Not checked yet";
pub const SOURCE_TITLE: &str = "Source folder (on this Mac)";
pub const CHOOSE_FOLDER: &str = "Choose folder…";
pub const PREVIEW_SYNC: &str = "Preview Sync";
pub const SYNC_NOW: &str = "Sync Now";
pub const PAUSE: &str = "Pause";
pub const RESUME: &str = "Resume";
pub const RETRY_FAILED: &str = "Retry Failed";
pub const AUTO_SYNC: &str = "Auto Sync";
pub const NEEDS_ATTENTION: &str = "Needs Attention";
pub const PREPARE_DISCONNECT: &str = "Prepare to Disconnect";
pub const USE_DRIVE_AGAIN: &str = "Use drive again";
pub const EJECT: &str = "Eject…";
pub const FULL_VERIFY: &str = "Full Verify";
pub const CONFIRM_DRIVE: &str = "Confirm this drive";
pub const AUTO_SYNC_HDD_NOTE: &str =
    "Periodic checks read the drive and may wake a sleeping hard disk.";
pub const PRIVACY_NOTE: &str = "TwinSync works entirely on this Mac. It sends nothing over the network and \
     never writes file contents to its logs. It never changes or deletes anything in the source folder.";
pub const UNKNOWN_MEDIA_NOTE: &str = "macOS does not say whether this is an SSD or a hard disk, so TwinSync uses careful settings. You can change the profile.";
pub const PREVIEW_STALE: &str = "This preview is out of date. Review the new plan before syncing.";
pub const INCOMPLETE_SCAN: &str =
    "Some folders could not be read. Files in them are treated as unknown, never as deleted.";

pub fn status_label(s: DestStatus) -> &'static str {
    match s {
        DestStatus::NotConfigured => "Not set up",
        DestStatus::Offline => "Offline",
        DestStatus::Idle => "Idle",
        DestStatus::Scanning => "Checking",
        DestStatus::Syncing => "Syncing",
        DestStatus::UpToDate => "Up to date",
        DestStatus::Paused => "Paused",
        DestStatus::Stopping => "Stopping…",
        DestStatus::WaitingForDrive => "Waiting for drive",
        DestStatus::NeedsAttention => "Needs attention",
        DestStatus::SourceUnavailable => "Source unavailable",
        DestStatus::PermissionDenied => "Permission denied",
        DestStatus::Error => "Error",
    }
}

pub fn worker_label(s: WorkerState) -> &'static str {
    match s {
        WorkerState::Idle => "Idle",
        WorkerState::Scanning => "Checking the drive",
        WorkerState::Syncing => "Copying",
        WorkerState::Verifying => "Verifying",
        WorkerState::Stopping => "Stopping…",
        WorkerState::WaitingForDrive => "Waiting for the drive to respond",
        WorkerState::Paused => "Paused",
    }
}

pub fn media_label(m: DriveMediaType) -> &'static str {
    match m {
        DriveMediaType::Ssd => "SSD",
        DriveMediaType::Hdd => "HDD",
        DriveMediaType::Unknown => "Unknown",
    }
}

pub fn profile_label(p: DrivePerformanceProfile) -> &'static str {
    match p {
        DrivePerformanceProfile::Auto => "Auto",
        DrivePerformanceProfile::Conservative => "Conservative",
        DrivePerformanceProfile::Balanced => "Balanced",
    }
}

pub fn connectivity_label(c: &Connectivity) -> String {
    match c {
        Connectivity::Online { mount } => format!("Connected at {}", mount.display()),
        Connectivity::Offline => "Not connected (showing last known state)".into(),
        Connectivity::IdentityMismatch(why) => format!("Not the expected drive: {why}"),
        Connectivity::NeedsConfirmation(why) => format!("Confirmation needed: {why}"),
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `2023-11-14 22:13:20 UTC`
pub fn timestamp(secs: i64) -> String {
    let s = utc_stamp(secs); // 20231114T221320Z
    format!(
        "{}-{}-{} {}:{}:{} UTC",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[9..11],
        &s[11..13],
        &s[13..15]
    )
}

pub fn ago(now: i64, then: Option<i64>) -> String {
    let Some(t) = then else { return "never".into() };
    let d = now.saturating_sub(t).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_times() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_500), "1.5 KB");
        assert_eq!(human_bytes(2_000_000_000), "2.0 GB");
        assert_eq!(timestamp(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(ago(1000, None), "never");
        assert_eq!(ago(1000, Some(990)), "just now");
        assert_eq!(ago(10_000, Some(10_000 - 7_200)), "2 h ago");
    }
}
