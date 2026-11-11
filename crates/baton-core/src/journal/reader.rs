use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::iter;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::journal::tree::Tree;
use crate::journal::{Line, Parser};
use crate::{Cursor, NativeRead, Progress};

const HASH_VERSION: &str = "v1:";

/// One complete line of a JSON Lines file.
pub struct JsonLine<'a> {
    /// Byte offset where the line starts.
    pub start: usize,
    /// Byte offset just past the line's newline.
    pub end: usize,
    /// The line without its newline.
    pub bytes: &'a [u8],
}

/// Whether `path` is a zstd compressed file, which is always read in full.
pub fn is_compressed(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "zst" || extension == "zstd")
}

/// The complete lines of `bytes`; a last line without a newline is still being written and is left out.
pub fn lines(bytes: &[u8]) -> impl Iterator<Item = JsonLine<'_>> {
    let mut start = 0;
    iter::from_fn(move || {
        let length = bytes[start..].iter().position(|&byte| byte == b'\n')?;
        let line = JsonLine {
            start,
            end: start + length + 1,
            bytes: &bytes[start..start + length],
        };
        start = line.end;
        Some(line)
    })
}

/// Reads a native session file with parsers made by `make_parser`.
///
/// When `cursor` refers to the same file, the file only grew since, and every new entry continues the branch the cursor ended on, only the new lines are parsed and the result is marked as continued. Otherwise the whole file is parsed and the active branch, the ancestry of the last entry, is returned. `make_parser` receives the native id of the entry an incremental read continues.
pub fn read<P: Parser>(
    path: &Path,
    cursor: Option<&Cursor>,
    mut make_parser: impl FnMut(Option<&str>) -> P,
) -> Result<NativeRead> {
    if let Some(cursor) = cursor
        && cursor.path == path
        && !is_compressed(path)
    {
        let parser = make_parser(cursor.progress.leaf.as_deref());
        if let Some(read) = read_tail(path, &cursor.progress, parser)? {
            return Ok(read);
        }
    }
    read_full(path, make_parser(None))
}

fn read_tail<P: Parser>(
    path: &Path,
    progress: &Progress,
    mut parser: P,
) -> Result<Option<NativeRead>> {
    if !progress.tail_hash.starts_with(HASH_VERSION) {
        return Ok(None);
    }
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut prefix = (&mut file).take(progress.offset);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let count = prefix.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if prefix.limit() != 0 || prefix_hash(&hasher) != progress.tail_hash {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let mut leaf = progress.leaf.clone();
    let mut drafts = Vec::new();
    let mut end = 0;
    for line in lines(&bytes) {
        end = line.end;
        let Ok(value) = serde_json::from_slice(line.bytes) else {
            continue;
        };
        match parser.line(progress.offset + line.start as u64, value)? {
            Line::Node(node) => {
                let continues =
                    node.parent == leaf && node.anchor.is_none_or(|anchor| anchor == node.id);
                if !continues {
                    return Ok(None);
                }
                drafts.extend(node.drafts);
                leaf = Some(node.id);
            }
            Line::Skip => {}
            Line::Restart => return Ok(None),
        }
    }
    hasher.update(&bytes[..end]);
    Ok(Some(NativeRead {
        info: parser.finish(),
        drafts,
        continued: true,
        progress: Progress {
            offset: progress.offset + end as u64,
            tail_hash: prefix_hash(&hasher),
            leaf,
        },
    }))
}

fn read_full<P: Parser>(path: &Path, mut parser: P) -> Result<NativeRead> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let compressed = is_compressed(path);
    let mut reader: Box<dyn BufRead> = if compressed {
        Box::new(BufReader::new(
            zstd::Decoder::new(file)
                .with_context(|| format!("decompressing {}", path.display()))?,
        ))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut tree = Tree::default();
    let mut end = 0u64;
    let mut hasher = Sha256::new();
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        if !compressed && line.last() != Some(&b'\n') {
            break;
        }
        let start = end;
        end += line.len() as u64;
        hasher.update(&line);
        let bytes = line.strip_suffix(b"\n").unwrap_or(&line);
        if let Ok(value) = serde_json::from_slice(bytes)
            && let Line::Node(node) = parser.line(start, value)?
        {
            tree.push(node);
        }
        line.clear();
    }
    let active = tree.active_path();
    Ok(NativeRead {
        info: parser.finish(),
        drafts: active.drafts,
        continued: false,
        progress: Progress {
            offset: end,
            tail_hash: prefix_hash(&hasher),
            leaf: active.leaf,
        },
    })
}

fn prefix_hash(hasher: &Sha256) -> String {
    format!(
        "{HASH_VERSION}{}",
        base16ct::lower::encode_string(&hasher.clone().finalize())
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use serde_json::{Value, json};

    use super::*;
    use crate::journal::Node;
    use crate::{Assistant, Body, Draft, Info, Stamp};

    struct Echo;

    impl Parser for Echo {
        fn line(&mut self, _offset: u64, value: Value) -> Result<Line> {
            let id: String = value["id"].as_str().unwrap().into();
            let text = value["text"].as_str().unwrap_or(&id).to_owned();
            Ok(Line::Node(Node {
                drafts: vec![Draft {
                    timestamp: "2026-01-01T00:00:00.000Z".into(),
                    native_entry_id: None,
                    body: Body::Assistant(Assistant { text }),
                }],
                id,
                parent: value["parent"].as_str().map(Into::into),
                anchor: value["anchor"].as_str().map(Into::into),
            }))
        }

        fn finish(self) -> Info {
            Info::default()
        }
    }

    fn texts(read: &NativeRead) -> Vec<String> {
        read.drafts
            .iter()
            .map(|draft| draft.body.text().into_owned())
            .collect()
    }

    fn cursor(path: &Path, progress: Progress) -> Cursor {
        Cursor {
            path: path.into(),
            stamp: Stamp { size: 0, mtime: 0 },
            progress,
            event: None,
            session: String::new(),
        }
    }

    #[test]
    fn reads_all_concatenated_zstd_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl.zstd");
        let header = b"{\"type\":\"session\",\"id\":\"header\"}\n";
        let events = b"{\"id\":\"a\",\"parent\":\"header\"}\n{\"id\":\"b\",\"parent\":\"a\"}\n";
        let mut compressed = zstd::bulk::compress(header, 1).unwrap();
        compressed.extend(zstd::bulk::compress(events, 1).unwrap());
        fs::write(&path, compressed).unwrap();

        let read = read(&path, None, |_| Echo).unwrap();
        assert_eq!(texts(&read), ["header", "a", "b"]);
        assert_eq!(read.progress.leaf.as_deref(), Some("b"));
    }

    #[test]
    fn full_read_follows_the_last_branch_and_moves_anchored_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let records = [
            json!({"id": "a"}),
            json!({"id": "b", "parent": "a"}),
            json!({"id": "dead", "parent": "b"}),
            json!({"id": "c", "parent": "b"}),
            json!({"id": "compact", "parent": "c", "anchor": "b"}),
            json!({"id": "d", "parent": "compact"}),
        ];
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(&path, format!("{text}not json\n{{\"id\":\"partial\"")).unwrap();
        let read = read(&path, None, |_| Echo).unwrap();
        assert!(
            !read.continued,
            "a read without a cursor covers the whole file"
        );
        assert_eq!(texts(&read), ["a", "compact", "b", "c", "d"]);
        assert_eq!(read.progress.leaf.as_deref(), Some("d"));
        assert_eq!(
            read.progress.offset,
            (text.len() + "not json\n".len()) as u64
        );
    }

    #[test]
    fn tail_read_continues_linear_appends_and_rejects_branches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, "{\"id\":\"a\"}\n{\"id\":\"b\",\"parent\":\"a\"}\n").unwrap();
        let first = read(&path, None, |_| Echo).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"id\":\"c\",\"parent\":\"b\"}\n")
            .unwrap();
        let second = read(&path, Some(&cursor(&path, first.progress.clone())), |_| {
            Echo
        })
        .unwrap();
        assert!(
            second.continued,
            "an append continuing the leaf is read incrementally"
        );
        assert_eq!(texts(&second), ["c"]);
        file.write_all(b"{\"id\":\"x\",\"parent\":\"a\"}\n")
            .unwrap();
        let third = read(&path, Some(&cursor(&path, second.progress)), |_| Echo).unwrap();
        assert!(!third.continued, "a new branch needs the whole tree");
        assert_eq!(texts(&third), ["a", "x"]);
        fs::write(
            &path,
            "{\"id\":\"z\"}\n{\"id\":\"b\",\"parent\":\"a\"}\n{\"id\":\"q\",\"parent\":\"b\"}\n",
        )
        .unwrap();
        let rewritten = read(&path, Some(&cursor(&path, first.progress)), |_| Echo).unwrap();
        assert!(!rewritten.continued, "a rewritten prefix is read in full");
    }

    #[test]
    fn rewriting_early_history_before_an_append_reads_the_whole_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let first_line = json!({"id": "a", "text": "old", "zpadding": "x".repeat(2048)});
        let second_line = json!({"id": "b", "parent": "a"});
        fs::write(&path, format!("{first_line}\n{second_line}\n")).unwrap();
        let first = read(&path, None, |_| Echo).unwrap();

        let rewritten = json!({"id": "a", "text": "new", "zpadding": "x".repeat(2048)});
        let appended = json!({"id": "c", "parent": "b"});
        fs::write(&path, format!("{rewritten}\n{second_line}\n{appended}\n")).unwrap();
        let result = read(&path, Some(&cursor(&path, first.progress)), |_| Echo).unwrap();
        assert!(!result.continued);
        assert_eq!(texts(&result), ["new", "b", "c"]);
    }

    #[test]
    fn old_cursor_reads_in_full_before_resuming_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, "{\"id\":\"a\"}\n").unwrap();
        let mut progress = read(&path, None, |_| Echo).unwrap().progress;
        progress.tail_hash = "old-unversioned-hash".into();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"id\":\"b\",\"parent\":\"a\"}\n")
            .unwrap();

        let result = read(&path, Some(&cursor(&path, progress)), |_| Echo).unwrap();
        assert!(!result.continued);
        assert_eq!(texts(&result), ["a", "b"]);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"id\":\"c\",\"parent\":\"b\"}\n")
            .unwrap();
        let resumed = read(&path, Some(&cursor(&path, result.progress)), |_| Echo).unwrap();
        assert!(resumed.continued);
        assert_eq!(texts(&resumed), ["c"]);
    }

    #[test]
    fn incomplete_append_is_consumed_only_after_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, "{\"id\":\"a\"}\n").unwrap();
        let first = read(&path, None, |_| Echo).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"id\":\"b\",\"parent\":\"a\"}").unwrap();

        let partial = read(&path, Some(&cursor(&path, first.progress.clone())), |_| {
            Echo
        })
        .unwrap();
        assert!(partial.continued);
        assert!(partial.drafts.is_empty());
        assert_eq!(partial.progress.offset, first.progress.offset);
        file.write_all(b"\n").unwrap();
        let complete = read(&path, Some(&cursor(&path, partial.progress)), |_| Echo).unwrap();
        assert!(complete.continued);
        assert_eq!(texts(&complete), ["b"]);
    }
}
