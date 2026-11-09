use crate::errors::{Error, Result};
use rusqlite::Connection;

/// Append only. Never edit a released migration.
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("schema_v1.sql"))];

pub fn latest_version() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.0)
}

pub fn run(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?;
    if current > latest_version() {
        return Err(Error::Config(format!(
            "database schema v{current} is newer than this TwinSync (v{}); refusing to open it",
            latest_version()
        )));
    }
    for (version, sql) in MIGRATIONS.iter().filter(|m| m.0 > current) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, super::now_secs()],
        )?;
        tx.commit()?;
    }
    Ok(())
}
