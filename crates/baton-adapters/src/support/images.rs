use std::fs;
use std::path::Path;

use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use baton_core::{Blobs, Image};

pub(crate) fn decode(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;

    STANDARD.decode(data.trim()).ok()
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    use base64::Engine;

    STANDARD.encode(bytes)
}

pub(crate) fn store(blobs: &Blobs, mime: &str, bytes: &[u8]) -> Result<Image> {
    Ok(Image {
        blob: blobs.put(bytes)?,
        mime: mime.to_owned(),
    })
}

// Only base64 data URLs carry the image itself; remote URLs are left out
pub(crate) fn from_data_url(blobs: &Blobs, url: &str) -> Result<Option<Image>> {
    let Some((mime, data)) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
    else {
        return Ok(None);
    };
    match decode(data) {
        Some(bytes) => store(blobs, mime, &bytes).map(Some),
        None => Ok(None),
    }
}

// A local file an agent attached by path, skipped once it no longer exists
pub(crate) fn from_file(blobs: &Blobs, path: &Path) -> Result<Option<Image>> {
    let mime = match path.extension().and_then(|extension| extension.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Ok(None),
    };
    match fs::read(path) {
        Ok(bytes) => store(blobs, mime, &bytes).map(Some),
        Err(_) => Ok(None),
    }
}

pub(crate) fn data_url(blobs: &Blobs, image: &Image) -> Result<String> {
    let data = encode(&blobs.get(&image.blob)?);
    Ok(format!("data:{};base64,{data}", image.mime))
}

pub(crate) fn extension(mime: &str) -> &str {
    mime.rsplit_once('/').map_or(mime, |(_, subtype)| subtype)
}
