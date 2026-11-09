//! What the UI sees and sends. Plain data: no SQLite, no file descriptors, no FFI.

use crate::config::Settings;
use crate::devices::Finding;
use crate::domain::{
    Connectivity, DestStatus, DriveMediaType, DrivePerformanceProfile, RelPath, Slot, WorkerState,
};
use crate::planner::PlanSummary;
use crate::storage::{ActivityEvent, ActivityFilter};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Overwrite the drive's file with the source; the existing file goes to version history.
    ReplaceWithSource,
    /// Leave this path alone until the source changes again.
    Ignore,
    /// If the drive's file is byte-identical to the source, start managing it.
    AdoptIfIdentical,
    /// The copy was deleted from the drive; copy it again from the source.
    RestoreFromSource,
    /// The copy was deleted from the drive; do not recreate it.
    StopManaging,
    /// The source file is gone; keep the copy and stop reminding.
    Acknowledge,
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReplaceWithSource => "Replace with source (keep existing as a version)",
            Self::Ignore => "Ignore for now",
            Self::AdoptIfIdentical => "Adopt if identical",
            Self::RestoreFromSource => "Restore from source",
            Self::StopManaging => "Don't recreate",
            Self::Acknowledge => "Keep copy, dismiss",
        }
    }
}

/// UI -> engine.
#[derive(Clone, Debug)]
pub enum Command {
    SetSource(PathBuf),
    ConfigureDestination {
        slot: Slot,
        path: PathBuf,
        label: String,
    },
    /// Accept the findings that need confirmation and finish the setup.
    ConfirmSetup {
        slot: Slot,
    },
    CancelSetup {
        slot: Slot,
    },
    /// Confirm the identity of a drive that has none recorded or no marker yet.
    ConfirmIdentity {
        slot: Slot,
    },
    RenameDestination {
        slot: Slot,
        label: String,
    },
    SetProfile {
        slot: Slot,
        profile: DrivePerformanceProfile,
    },
    Preview,
    /// `preview_signature`: signature of the preview the user reviewed; if the
    /// fresh plan differs, nothing runs and a new preview is shown instead.
    SyncNow {
        preview_signature: Option<u64>,
    },
    Pause,
    Resume,
    RetryFailed,
    SetAutoSync(bool),
    SaveSettings(Settings),
    FullVerify {
        slot: Slot,
    },
    PrepareDisconnect {
        slot: Slot,
    },
    /// Let TwinSync use the drive again after Prepare to Disconnect.
    UseDriveAgain {
        slot: Slot,
    },
    Eject {
        slot: Slot,
        confirm_siblings: bool,
    },
    Resolve {
        slot: Slot,
        rel: RelPath,
        resolution: Resolution,
    },
    LoadActivity(ActivityFilter),
    RefreshMounts,
    DismissNotice(usize),
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceState {
    NotConfigured,
    /// Configured, not scanned yet.
    Unknown,
    Ok,
    Unavailable(String),
}

#[derive(Clone, Debug)]
pub struct SourceView {
    pub path: Option<PathBuf>,
    pub state: SourceState,
    pub last_scan_at: Option<i64>,
    pub last_scan_complete: bool,
    pub unreadable: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ProgressView {
    pub current: Option<String>,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Only once enough data exists to be meaningful.
    pub bytes_per_sec: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct PendingView {
    /// True only when computed against the latest source scan.
    pub up_to_date_with_source: bool,
    pub summary: PlanSummary,
}

#[derive(Clone, Debug)]
pub struct AttentionItem {
    pub rel: RelPath,
    pub action: String,
    pub reason: String,
    pub resolutions: Vec<Resolution>,
}

#[derive(Clone, Debug)]
pub struct DestView {
    pub slot: Slot,
    pub configured: bool,
    pub label: String,
    pub volume_name: Option<String>,
    pub media: DriveMediaType,
    pub fs_type: Option<String>,
    pub connectivity: Connectivity,
    pub free_bytes: Option<u64>,
    pub free_checked_at: Option<i64>,
    pub profile: DrivePerformanceProfile,
    pub last_sync_at: Option<i64>,
    /// Set only by a Full Verify (content hashes), never by a metadata check.
    pub last_verify_at: Option<i64>,
    pub state: WorkerState,
    pub status: DestStatus,
    pub progress: Option<ProgressView>,
    pub pending: Option<PendingView>,
    pub attention: Arc<Vec<AttentionItem>>,
    pub version_count: usize,
    pub version_bytes: u64,
    /// Set after Prepare to Disconnect finished: TwinSync has stopped using the drive.
    pub released: bool,
    pub disconnect_note: Option<String>,
    pub last_error: Option<String>,
    pub shares_disk_note: Option<String>,
    pub last_verify_summary: Option<String>,
}

impl DestView {
    pub fn empty(slot: Slot) -> Self {
        Self {
            slot,
            configured: false,
            label: slot.default_label(),
            volume_name: None,
            media: DriveMediaType::Unknown,
            fs_type: None,
            connectivity: Connectivity::Offline,
            free_bytes: None,
            free_checked_at: None,
            profile: DrivePerformanceProfile::Auto,
            last_sync_at: None,
            last_verify_at: None,
            state: WorkerState::Idle,
            status: DestStatus::NotConfigured,
            progress: None,
            pending: None,
            attention: Arc::new(Vec::new()),
            version_count: 0,
            version_bytes: 0,
            released: false,
            disconnect_note: None,
            last_error: None,
            shares_disk_note: None,
            last_verify_summary: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreviewRow {
    pub slot: Slot,
    pub rel: RelPath,
    pub action: String,
    pub size: u64,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct PreviewView {
    pub signature: u64,
    pub rows: Vec<PreviewRow>,
    pub per_dest: Vec<(Slot, PlanSummary)>,
    pub source_scan_complete: bool,
    /// The source or a drive changed after this preview was made, or Sync Now
    /// found a different plan than the one reviewed.
    pub out_of_date: bool,
}

#[derive(Clone, Debug)]
pub struct SetupReport {
    pub slot: Slot,
    pub path: PathBuf,
    pub findings: Vec<Finding>,
    /// Waiting for the user to confirm (no blocking errors).
    pub awaiting_confirmation: bool,
}

#[derive(Clone, Debug)]
pub struct EjectPrompt {
    pub slot: Slot,
    pub also_unmounts: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct UiSnapshot {
    /// Increments on every change; lets the UI skip work when nothing moved.
    pub rev: u64,
    pub source: SourceView,
    pub destinations: Vec<DestView>,
    pub settings: Settings,
    pub auto_sync: bool,
    pub paused: bool,
    pub scanning: bool,
    pub preview: Option<Arc<PreviewView>>,
    pub activity: Arc<Vec<ActivityEvent>>,
    pub notices: Vec<Notice>,
    pub setup: Option<SetupReport>,
    pub eject_prompt: Option<EjectPrompt>,
    /// True only when every configured destination is UpToDate against the latest source scan.
    pub all_up_to_date: bool,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            rev: 0,
            source: SourceView {
                path: None,
                state: SourceState::NotConfigured,
                last_scan_at: None,
                last_scan_complete: false,
                unreadable: Vec::new(),
            },
            destinations: vec![DestView::empty(Slot::A), DestView::empty(Slot::B)],
            settings: Settings::default(),
            auto_sync: false,
            paused: false,
            scanning: false,
            preview: None,
            activity: Arc::new(Vec::new()),
            notices: Vec::new(),
            setup: None,
            eject_prompt: None,
            all_up_to_date: false,
        }
    }
}
