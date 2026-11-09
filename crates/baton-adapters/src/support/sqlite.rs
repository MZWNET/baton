use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use baton_core::Stamp;
use rusqlite::{Connection, OpenFlags};

// Agents keep their databases open while they run, so baton waits out their short write locks
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn resolve_path(path: Option<&Path>, default_db: &Path, name: &str) -> PathBuf {
    match path {
        Some(path) if path.is_file() => path.to_path_buf(),
        Some(dir) => dir.join(name),
        None => default_db.to_path_buf(),
    }
}

// Build the database aside and link it in whole so the agent never sees half of it
pub(crate) fn publish_new(
    path: &Path,
    fill: impl FnOnce(&mut Connection) -> Result<()>,
) -> Result<()> {
    let dir = path.parent().expect("database paths have a directory");
    fs::create_dir_all(dir)?;
    let scratch = tempfile::NamedTempFile::new_in(dir)?;
    let mut connection = Connection::open(scratch.path())?;
    fill(&mut connection)?;
    connection.close().map_err(|(_, error)| error)?;
    fs::hard_link(scratch.path(), path).with_context(|| format!("creating {}", path.display()))?;
    scratch.close()?;
    Ok(())
}

pub(crate) fn open_read_only(path: &Path) -> Result<Connection> {
    open(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

// Never creates the file: a database the agent has not created yet lacks its schema
pub(crate) fn open_read_write(path: &Path) -> Result<Connection> {
    open(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
}

// For agents whose whole schema baton can create when the database does not exist yet
pub(crate) fn open_or_create(path: &Path) -> Result<Connection> {
    fs::create_dir_all(path.parent().expect("database paths have a directory"))?;
    open(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
}

// A running agent leaves new rows in the write-ahead log until it checkpoints
pub(crate) fn stamp(db: &Path) -> Result<Stamp> {
    let mut stamp = Stamp::of(db)?;
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    if let Ok(wal) = Stamp::of(Path::new(&wal)) {
        stamp.size += wal.size;
        stamp.mtime = stamp.mtime.max(wal.mtime);
    }
    Ok(stamp)
}

fn open(path: &Path, flags: OpenFlags) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, flags)
        .with_context(|| format!("opening {}", path.display()))?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(connection)
}
