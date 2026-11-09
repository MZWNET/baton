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

pub(crate) fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let be_u16 =
        |offset| Some(u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as u32);
    let le_u16 =
        |offset| Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as u32);

    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR") {
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);

        Some((width, height))
    } else if bytes.starts_with(b"GIF8") {
        Some((le_u16(6)?, le_u16(8)?))
    } else if bytes.starts_with(b"\xff\xd8") {
        let mut offset = 2;
        loop {
            if *bytes.get(offset)? != 0xff {
                return None;
            }
            while *bytes.get(offset)? == 0xff {
                offset += 1;
            }
            let marker = *bytes.get(offset)?;
            offset += 1;
            match marker {
                0xd9 | 0xda => return None,
                0x01 | 0xd0..=0xd8 => continue,
                _ => {}
            }
            let length = be_u16(offset)? as usize;
            if length < 2 || bytes.get(offset..offset + length).is_none() {
                return None;
            }
            if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                if length < 7 {
                    return None;
                }

                return Some((be_u16(offset + 5)?, be_u16(offset + 3)?));
            }
            offset += length;
        }
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        match bytes.get(12..16)? {
            b"VP8 " => Some((le_u16(26)? & 0x3fff, le_u16(28)? & 0x3fff)),
            b"VP8L" if *bytes.get(20)? == 0x2f => {
                let bits = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);

                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => {
                let width = bytes.get(24..27)?;
                let height = bytes.get(27..30)?;

                Some((
                    u32::from_le_bytes([width[0], width[1], width[2], 0]) + 1,
                    u32::from_le_bytes([height[0], height[1], height[2], 0]) + 1,
                ))
            }
            _ => None,
        }
    } else {
        None
    }
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
