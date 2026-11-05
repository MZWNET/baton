use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

// Opens a cache database for one data directory, emptying `tables` when it was filled for another one
pub(super) fn open(path: &Path, data: &Path, schema: &str, tables: &[&str]) -> Result<Connection> {
    use rusqlite::OptionalExtension;

    let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    db.execute_batch(schema)?;
    let data = data.to_string_lossy();
    let filled_for: Option<String> = db
        .query_row("SELECT value FROM config WHERE key = 'data'", [], |row| {
            row.get(0)
        })
        .optional()?;
    if filled_for.as_deref() != Some(data.as_ref()) {
        for table in tables {
            db.execute(&format!("DELETE FROM {table}"), [])?;
        }
        db.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES ('data', ?1)",
            [data.as_ref()],
        )?;
    }
    Ok(db)
}
