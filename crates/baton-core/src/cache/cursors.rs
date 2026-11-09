use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::cache::database;
use crate::{Agent, Cursor, Progress, Stamp};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cursors (
    agent TEXT NOT NULL,
    native_id TEXT NOT NULL,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    tail_hash TEXT NOT NULL,
    leaf TEXT,
    event TEXT,
    session TEXT NOT NULL,
    PRIMARY KEY (agent, native_id)
);
";

// Import cursors in `state.db`; without its cursor a native session is read in full, and a branch it switched to meanwhile imports as a fork
pub(crate) struct Cursors {
    db: Connection,
}

impl Cursors {
    pub(crate) fn open(cache: &Path, data: &Path) -> Result<Cursors> {
        let db = database::open(&cache.join("state.db"), data, SCHEMA, &["cursors"])?;
        Ok(Cursors { db })
    }

    pub(crate) fn get(&self, agent: Agent, native_id: &str) -> Result<Option<Cursor>> {
        use rusqlite::OptionalExtension;

        let cursor = self
            .db
            .prepare_cached(
                "SELECT path, size, mtime, offset, tail_hash, leaf, event, session FROM cursors
                 WHERE agent = ?1 AND native_id = ?2",
            )?
            .query_row(params![agent.name(), native_id], |row| {
                Ok(Cursor {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    stamp: Stamp {
                        size: row.get::<_, i64>(1)? as u64,
                        mtime: row.get(2)?,
                    },
                    progress: Progress {
                        offset: row.get::<_, i64>(3)? as u64,
                        tail_hash: row.get(4)?,
                        leaf: row.get(5)?,
                    },
                    event: row.get(6)?,
                    session: row.get(7)?,
                })
            })
            .optional()?;
        Ok(cursor)
    }

    pub(crate) fn save(&self, agent: Agent, native_id: &str, cursor: &Cursor) -> Result<()> {
        self.db.prepare_cached(
            "INSERT OR REPLACE INTO cursors (agent, native_id, path, size, mtime, offset, tail_hash, leaf, event, session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?.execute(
            params![
                agent.name(),
                native_id,
                cursor.path.to_string_lossy(),
                cursor.stamp.size as i64,
                cursor.stamp.mtime,
                cursor.progress.offset as i64,
                cursor.progress.tail_hash,
                cursor.progress.leaf,
                cursor.event,
                cursor.session,
            ],
        )?;
        Ok(())
    }
}
