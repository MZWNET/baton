use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
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

// The media type of an image file, by its extension
pub(crate) fn mime(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    [
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("jpe", "image/jpeg"),
        ("jfif", "image/jpeg"),
        ("gif", "image/gif"),
        ("webp", "image/webp"),
        ("svg", "image/svg+xml"),
        ("heic", "image/heic"),
        ("heif", "image/heif"),
        ("avif", "image/avif"),
        ("bmp", "image/bmp"),
        ("tif", "image/tiff"),
        ("tiff", "image/tiff"),
        ("ico", "image/vnd.microsoft.icon"),
        ("jxl", "image/jxl"),
    ]
    .iter()
    .find_map(|(suffix, mime)| extension.eq_ignore_ascii_case(suffix).then_some(*mime))
}

// A local file an agent attached by path, skipped once it no longer exists
pub(crate) fn from_file(blobs: &Blobs, path: &Path) -> Result<Option<Image>> {
    let Some(mime) = mime(path) else {
        return Ok(None);
    };
    match fs::read(path) {
        Ok(bytes) => store(blobs, mime, &bytes).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading image file {}", path.display())),
    }
}

pub(crate) fn data_url(blobs: &Blobs, image: &Image) -> Result<String> {
    let data = encode(&blobs.get(&image.blob)?);
    Ok(format!("data:{};base64,{data}", image.mime))
}

pub(crate) fn extension(mime: &str) -> &str {
    [
        ("image/png", "png"),
        ("image/x-png", "png"),
        ("image/jpeg", "jpg"),
        ("image/jpg", "jpg"),
        ("image/jpe", "jpg"),
        ("image/pjpeg", "jpg"),
        ("image/x-jpeg", "jpg"),
        ("image/gif", "gif"),
        ("image/webp", "webp"),
        ("image/svg+xml", "svg"),
        ("image/heic", "heic"),
        ("image/heif", "heif"),
        ("image/avif", "avif"),
        ("image/bmp", "bmp"),
        ("image/x-ms-bmp", "bmp"),
        ("image/tiff", "tiff"),
        ("image/x-icon", "ico"),
        ("image/vnd.microsoft.icon", "ico"),
        ("image/jxl", "jxl"),
    ]
    .iter()
    .find_map(|(media_type, extension)| mime.eq_ignore_ascii_case(media_type).then_some(*extension))
    .unwrap_or("img")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_files_round_trip_with_supported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(dir.path().join("blobs"));
        for (name, media_type, bytes) in [
            ("source.PNG", "image/png", &b"png data"[..]),
            ("source.SvG", "image/svg+xml", &b"svg data"[..]),
            ("source.HEIC", "image/heic", &b"heic data"[..]),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            let image = from_file(&blobs, &path).unwrap().unwrap();
            assert_eq!(image.mime, media_type);
            let output = dir
                .path()
                .join(format!("output.{}", extension(&image.mime)));
            fs::write(&output, blobs.get(&image.blob).unwrap()).unwrap();
            let reread = from_file(&blobs, &output).unwrap().unwrap();
            assert_eq!(reread.mime, media_type);
            assert_eq!(blobs.get(&reread.blob).unwrap(), bytes);
        }
    }

    #[test]
    fn missing_files_are_skipped_but_other_io_errors_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::new(dir.path().join("blobs"));
        assert!(
            from_file(&blobs, &dir.path().join("missing.png"))
                .unwrap()
                .is_none()
        );
        let path = dir.path().join("directory.png");
        fs::create_dir(&path).unwrap();
        let error = from_file(&blobs, &path).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }

    #[test]
    fn unknown_media_type_cannot_inject_a_filename() {
        assert_eq!(extension("image/../../outside"), "img");
    }
}
