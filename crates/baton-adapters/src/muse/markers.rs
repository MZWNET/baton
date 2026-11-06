// Muse logs keep no image bytes or summaries and mark failed tools only in text, so baton spells them out where Muse replays them verbatim

use baton_core::Image;

use crate::support::normalize::Content;

// The prompt of a turn baton opens for assistant events that came before any user message
pub(super) const CONTINUATION: &str = "<baton-continuation/>";
const SUMMARY_OPEN: &str = "<baton-compaction-summary>\n";
const SUMMARY_CLOSE: &str = "\n</baton-compaction-summary>";
const TOOL_ERROR: &str = "<baton-tool-error/>";

pub(super) fn summary(summary: &str) -> String {
    format!("{SUMMARY_OPEN}{summary}{SUMMARY_CLOSE}")
}

pub(super) fn unwrap_summary(text: &str) -> Option<&str> {
    text.strip_prefix(SUMMARY_OPEN)?.strip_suffix(SUMMARY_CLOSE)
}

pub(super) fn encode(text: &str, images: &[Image]) -> String {
    encode_result(text, images, false)
}

// Image markers name the blobs baton keeps their bytes in; the error marker comes first because decode strips images from the end
pub(super) fn encode_result(text: &str, images: &[Image], is_error: bool) -> String {
    let mut lines = Vec::with_capacity(images.len() + 1 + usize::from(is_error));
    if !text.is_empty() || (images.is_empty() && !is_error) {
        lines.push(text.to_owned());
    }
    if is_error {
        lines.push(TOOL_ERROR.to_owned());
    }
    for image in images {
        lines.push(format!(
            "<baton-image sha256=\"{}\" mime=\"{}\"/>",
            image.blob, image.mime
        ));
    }
    lines.join("\n")
}

pub(super) fn decode(text: &str) -> Content {
    let mut rest = text;
    let mut images = Vec::new();
    loop {
        let (before, last) = rest.rsplit_once('\n').unwrap_or(("", rest));
        let Some(image) = image(last) else {
            break;
        };
        images.push(image);
        rest = before;
    }
    images.reverse();
    Content {
        text: rest.to_owned(),
        images,
    }
}

pub(super) fn decode_result(text: &str) -> (Content, bool) {
    let mut content = decode(text);
    let (before, last) = content
        .text
        .rsplit_once('\n')
        .unwrap_or(("", &content.text));
    let is_error = last == TOOL_ERROR;
    if is_error {
        content.text = before.to_owned();
    }
    (content, is_error)
}

fn image(line: &str) -> Option<Image> {
    let rest = line.strip_prefix("<baton-image sha256=\"")?;
    let (blob, rest) = rest.split_at_checked(64)?;
    let mime = rest.strip_prefix("\" mime=\"")?.strip_suffix("\"/>")?;
    let hex = blob
        .chars()
        .all(|char| char.is_ascii_digit() || ('a'..='f').contains(&char));
    (hex && !mime.is_empty() && !mime.contains('"')).then(|| Image {
        blob: blob.to_owned(),
        mime: mime.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_decode_to_what_was_encoded() {
        let image = Image {
            blob: "6ec9c2b0eb14010746c8bce8939303b382344b296206612eb8a907a37b2b2f37".into(),
            mime: "image/png".into(),
        };
        for (text, images) in [
            ("look", vec![image.clone()]),
            ("", vec![image.clone(), image.clone()]),
            ("two\nlines", vec![]),
            ("", vec![]),
        ] {
            let decoded = decode(&encode(text, &images));
            assert_eq!((decoded.text.as_str(), &decoded.images), (text, &images));
            for is_error in [false, true] {
                let (decoded, decoded_error) =
                    decode_result(&encode_result(text, &images, is_error));
                assert_eq!(
                    (decoded.text.as_str(), decoded.images, decoded_error),
                    (text, images.clone(), is_error)
                );
            }
        }
        assert_eq!(
            encode_result("failed", std::slice::from_ref(&image), true),
            format!(
                "failed\n<baton-tool-error/>\n<baton-image sha256=\"{}\" mime=\"image/png\"/>",
                image.blob
            )
        );
        assert_eq!(unwrap_summary(&summary("so far")), Some("so far"));
    }
}
