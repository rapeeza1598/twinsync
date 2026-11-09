use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "TwinSync";
pub const TAGLINE: &str = "One Mac. Two drives. Files in sync.";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub auto_sync: bool,
    pub debounce_ms: u64,
    pub reconcile_minutes: u64,
    pub exclusions: Vec<String>,
    /// Re-hash every source file on each scan instead of trusting size+mtime+inode.
    pub hash_all_source_files: bool,
    pub free_space_reserve_mb: u64,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_sync: false,
            debounce_ms: 1500,
            reconcile_minutes: 15,
            exclusions: default_exclusions(),
            hash_all_source_files: false,
            free_space_reserve_mb: 256,
            theme: Theme::System,
        }
    }
}

/// Junk the OS or editors create. Build folders such as `node_modules` are
/// intentionally NOT here; users opt in through Settings.
pub fn default_exclusions() -> Vec<String> {
    [
        ".DS_Store",
        "._*",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        ".TemporaryItems",
        ".DocumentRevisions-V100",
        "~$*",
        ".~lock.*#",
        "*.swp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Locations are passed in explicitly so tests never touch the real user
/// directories (and never need to mutate process environment).
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl AppPaths {
    pub fn macos_default() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(Self {
            data_dir: home.join("Library/Application Support/TwinSync"),
            log_dir: home.join("Library/Logs/TwinSync"),
        })
    }

    pub fn under(root: &Path) -> Self {
        Self {
            data_dir: root.join("data"),
            log_dir: root.join("logs"),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("twinsync.sqlite3")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.data_dir.join("twinsync.lock")
    }
    pub fn legacy_config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }
}

/// Hold this for the life of the process: a second TwinSync must not write the
/// same project. The lock is released by the OS when the process exits.
pub fn acquire_instance_lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(f)
}
