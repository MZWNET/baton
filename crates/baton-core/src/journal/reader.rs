use std::fs::{self, File};
use std::iter;
use std::path::Path;

use anyhow::{Context, Result};
use ruzstd::decoding::StreamingDecoder;

use crate::journal::tree::Tree;
use crate::journal::{Line, Parser};
use crate::{Cursor, NativeRead, Progress, hash};

// Bytes before the cursor offset that must be unchanged for an incremental read
const TAIL_WINDOW: usize = 1024;

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
    path.extension().is_some_and(|extension| extension == "zst")
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
    use std::io::{Read, Seek, SeekFrom};

    let window_start = progress.offset.saturating_sub(TAIL_WINDOW as u64);
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    file.seek(SeekFrom::Start(window_start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let known = (progress.offset - window_start) as usize;
    if bytes.len() <= known || hash_tail(&bytes[..known]) != progress.tail_hash {
        return Ok(None);
    }
    let mut leaf = progress.leaf.clone();
    let mut drafts = Vec::new();
    let mut end = known;
    for line in lines(&bytes[known..]) {
        end = known + line.end;
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
    Ok(Some(NativeRead {
        info: parser.finish(),
        drafts,
        continued: true,
        progress: Progress {
            offset: window_start + end as u64,
            tail_hash: hash_tail(&bytes[..end]),
            leaf,
        },
    }))
}

fn read_full<P: Parser>(path: &Path, mut parser: P) -> Result<NativeRead> {
    let bytes = read_file(path)?;
    let mut tree = Tree::default();
    let mut end = 0;
    for line in lines(&bytes) {
        end = line.end;
        let Ok(value) = serde_json::from_slice(line.bytes) else {
            continue;
        };
        if let Line::Node(node) = parser.line(line.start as u64, value)? {
            tree.push(node);
        }
    }
    let active = tree.active_path();
    Ok(NativeRead {
        info: parser.finish(),
        drafts: active.drafts,
        continued: false,
        progress: Progress {
            offset: end as u64,
            tail_hash: hash_tail(&bytes[..end]),
            leaf: active.leaf,
        },
    })
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;

    if !is_compressed(path) {
        return fs::read(path).with_context(|| format!("reading {}", path.display()));
    }
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut bytes = Vec::new();
    StreamingDecoder::new(&mut file)
        .with_context(|| format!("decompressing {}", path.display()))?
        .read_to_end(&mut bytes)?;
    if bytes.last().is_some_and(|&byte| byte != b'\n') {
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn hash_tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(TAIL_WINDOW);
    hash::sha256_hex(&bytes[start..])
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::{Value, json};

    use super::*;
    use crate::journal::Node;
    use crate::{Assistant, Body, Draft, Info, Stamp};

    struct Echo;

    impl Parser for Echo {
        fn line(&mut self, _offset: u64, value: Value) -> Result<Line> {
            let id: String = value["id"].as_str().unwrap().into();
            Ok(Line::Node(Node {
                drafts: vec![Draft {
                    timestamp: "2026-01-01T00:00:00.000Z".into(),
                    native_entry_id: None,
                    body: Body::Assistant(Assistant { text: id.clone() }),
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
        read.drafts.iter().map(|draft| draft.body.text()).collect()
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
}
