//! Import of the pre-TwinSync "SSD A / SSD B" configuration.
//!
//! No real legacy file was available when this was written, so the expected
//! shape is an assumption: `{"source": "...", "ssd_a": "...", "ssd_b": "..."}`.
//! Unknown extra fields are ignored. The original file is renamed, never deleted.

use super::{Db, DestinationRow};
use crate::domain::Slot;
use crate::errors::{Error, IoContext, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct LegacyConfig {
    source: PathBuf,
    ssd_a: PathBuf,
    ssd_b: PathBuf,
}

/// Returns true if a legacy file was found and imported.
///
/// Imported destinations have no volume identity yet, so the engine treats
/// them as needing confirmation and writes nothing until the user confirms
/// each drive.
pub fn import_if_present(db: &Db, legacy_file: &Path) -> Result<bool> {
    if !legacy_file.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(legacy_file).ctx("read legacy config")?;
    let cfg: LegacyConfig = serde_json::from_str(&text)
        .map_err(|e| Error::Config(format!("legacy config is not in the expected format: {e}")))?;
    if db.first_project()?.is_some() {
        return Err(Error::Config(
            "legacy config found but a project already exists; not importing".into(),
        ));
    }
    let project = db.create_project("TwinSync", Some(&cfg.source))?;
    for (slot, label, path) in [(Slot::A, "SSD A", cfg.ssd_a), (Slot::B, "SSD B", cfg.ssd_b)] {
        let mut d = DestinationRow::new(&project.id, slot);
        d.label = label.to_string();
        d.last_mount_path = Some(path);
        db.upsert_destination(&d)?;
    }
    db.log_event(
        None,
        "migration",
        None,
        "ok",
        "imported legacy SSD A / SSD B config",
        false,
    )?;
    let mut migrated = legacy_file.as_os_str().to_owned();
    migrated.push(".migrated");
    std::fs::rename(legacy_file, PathBuf::from(migrated)).ctx("rename legacy config")?;
    Ok(true)
}
