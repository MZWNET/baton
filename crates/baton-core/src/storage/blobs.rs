use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{hash, storage};

/// Images and other binary attachments, stored once under the SHA-256 of their bytes.
pub struct Blobs {
    dir: PathBuf,
}

impl Blobs {
    /// Blobs kept in `dir`, which is created on the first write.
    pub fn new(dir: PathBuf) -> Blobs {
        Blobs { dir }
    }

    /// Stores `bytes` unless an identical blob exists, and returns their SHA-256.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let sha = hash::sha256_hex(bytes);
        let path = self.dir.join(&sha);
        if !path.exists() {
            fs::create_dir_all(&self.dir)?;
            storage::write_atomic(&path, bytes)?;
        }
        Ok(sha)
    }

    /// Reads the blob stored under `sha`.
    pub fn get(&self, sha: &str) -> Result<Vec<u8>> {
        let path = self.dir.join(sha);
        fs::read(&path).with_context(|| format!("reading blob {}", path.display()))
    }
}
