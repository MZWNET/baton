use std::env;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use baton_core::journal;
use ruzstd::decoding::StreamingDecoder;
use serde_json::Value;

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

pub(crate) fn create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    fs::create_dir_all(
        path.parent()
            .expect("native session paths have a directory"),
    )?;
    let mut file =
        File::create_new(path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

// The first JSON lines of a session, decompressing on the fly and without reading the rest of the file
pub(crate) fn head(path: &Path, count: usize) -> Result<Vec<Value>> {
    use std::io::{BufRead, BufReader, Read};

    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader: Box<dyn Read> = if journal::is_compressed(path) {
        Box::new(StreamingDecoder::new(file)?)
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
