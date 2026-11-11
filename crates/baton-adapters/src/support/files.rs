use std::env;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use baton_core::{NativeSession, Stamp, journal};
use serde_json::Value;
use uuid::Uuid;

// The home directory as Node reports it to the agents, which on Windows prefers `USERPROFILE` to the profile folder
pub(crate) fn home() -> Result<PathBuf> {
    env::home_dir().context("cannot locate the home directory")
}

// Agents ignore an empty override, and so does baton
pub(crate) fn env_dir(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

// Agents following the XDG layout use it on every platform, Windows and macOS included
pub(crate) fn xdg_data_home() -> Result<PathBuf> {
    match env_dir("XDG_DATA_HOME") {
        Some(dir) => Ok(dir),
        None => Ok(home()?.join(".local").join("share")),
    }
}

// Where VS Code based editors keep their user data
pub(crate) fn editor_data(name: &str) -> Result<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        home()?.join("Library").join("Application Support")
    } else if cfg!(windows) {
        roaming_app_data()?
    } else {
        match env_dir("XDG_CONFIG_HOME") {
            Some(dir) => dir,
            None => home()?.join(".config"),
        }
    };
    Ok(base.join(name))
}

pub(crate) fn roaming_app_data() -> Result<PathBuf> {
    windows_dir("APPDATA", "Roaming")
}

pub(crate) fn local_app_data() -> Result<PathBuf> {
    windows_dir("LOCALAPPDATA", "Local")
}

// Node and Python agents read the environment before falling back to the folder in the profile
fn windows_dir(variable: &str, folder: &str) -> Result<PathBuf> {
    match env_dir(variable) {
        Some(dir) => Ok(dir),
        None => Ok(home()?.join("AppData").join(folder)),
    }
}

// Sorted entries, or none when the directory does not exist yet
pub(crate) fn read_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut paths = Vec::new();
    for entry in entries {
        paths.push(entry?.path());
    }
    paths.sort();
    Ok(paths)
}

// Sessions are named after their UUID, excluding other transcripts such as Claude's subagent agent-<id>.jsonl
pub(crate) fn uuid_jsonl_session(file: &Path) -> Result<Option<NativeSession>> {
    let Some(id) = file
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".jsonl"))
        .filter(|id| Uuid::parse_str(id).is_ok())
    else {
        return Ok(None);
    };

    Ok(Some(NativeSession {
        id: id.to_owned(),
        path: file.to_path_buf(),
        stamp: Stamp::of(file)?,
        title: None,
    }))
}

// A path with every character other than an ASCII letter or digit turned into a dash, per UTF-16 unit as JavaScript agents count them
pub(crate) fn dashed(path: &str) -> String {
    let mut dashed = String::with_capacity(path.len());
    for unit in path.encode_utf16() {
        dashed.push(match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        });
    }
    dashed
}

pub(crate) fn create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let dir = path
        .parent()
        .expect("native session paths have a directory");
    fs::create_dir_all(dir)?;
    let mut scratch = tempfile::NamedTempFile::new_in(dir)?;
    scratch.write_all(bytes)?;
    fs::hard_link(scratch.path(), path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

pub(crate) fn create_dir_with<T>(dir: &Path, write: impl FnOnce() -> Result<T>) -> Result<T> {
    fs::create_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    match write() {
        Ok(value) => Ok(value),
        Err(error) => {
            fs::remove_dir_all(dir)
                .with_context(|| format!("cleaning {} after {error:#}", dir.display()))?;
            Err(error)
        }
    }
}

// For files whose names determine their content
pub(crate) fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let dir = path.parent().expect("published paths have a directory");
    fs::create_dir_all(dir)?;
    let mut scratch = tempfile::NamedTempFile::new_in(dir)?;
    scratch.write_all(bytes)?;

    match fs::hard_link(scratch.path(), path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("creating {}", path.display())),
    }
}

pub(crate) fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

pub(crate) fn remove_empty_dir(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

// The first JSON lines of a session, decompressing on the fly and without reading the rest of the file
pub(crate) fn head(path: &Path, count: usize) -> Result<Vec<Value>> {
    use std::io::{BufRead, BufReader, Read};

    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader: Box<dyn Read> = if journal::is_compressed(path) {
        Box::new(zstd::Decoder::new(file)?)
    } else {
        Box::new(file)
    };
    let lines = BufReader::new(reader).lines().take(count);
    let mut values = Vec::with_capacity(count);
    for line in lines {
        values.push(serde_json::from_str(&line?).unwrap_or(Value::Null));
    }
    Ok(values)
}
