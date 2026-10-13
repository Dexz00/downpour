//! Reading `.torrent` files (BEP-3) and the isolated `info` dictionary
//! (which also arrives over the network via `ut_metadata`, BEP-9).

use crate::bencode::{self, Value};
use crate::id::Id20;
use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

/// A file inside the torrent, already positioned in the concatenated byte
/// stream that the pieces cover.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Already sanitized relative path, safe to join to the output directory.
    pub path: PathBuf,
    pub length: u64,
    /// Offset of the start of this file within the torrent stream.
    pub offset: u64,
}

impl FileEntry {
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// The decoded `info` dictionary — everything that defines the *content* of the torrent.
#[derive(Debug, Clone)]
pub struct Info {
    pub info_hash: Id20,
    pub name: String,
    pub piece_length: u32,
    pub piece_hashes: Vec<Id20>,
    pub files: Vec<FileEntry>,
    pub total_length: u64,
    /// Private torrent: forbids DHT and PEX (BEP-27).
    pub private: bool,
    /// Original dictionary bytes, served to other peers via `ut_metadata`.
    pub raw: Vec<u8>,
}

impl Info {
    /// Decodes an `info` dictionary from its raw bencode bytes.
    pub fn from_raw(raw: &[u8]) -> Result<Info> {
        let decoded = bencode::decode(raw).context("invalid `info` dictionary")?;
        Info::from_value(&decoded.value, raw)
    }

    fn from_value(info: &Value, raw: &[u8]) -> Result<Info> {
        let info_hash = Id20(Sha1::digest(raw).into());

        let name = info
            .str_at("name")
            .or_else(|| info.str_at("name.utf-8"))
            .context("missing `info.name`")?;

        let piece_length = info
            .int_at("piece length")
            .context("missing `info.piece length`")?;
        if piece_length <= 0 || piece_length > 128 << 20 {
            bail!("`piece length` out of acceptable range: {piece_length}");
        }
        let piece_length = piece_length as u32;

        let pieces = info
            .bytes_at("pieces")
            .context("missing `info.pieces` — v2-only torrents are not supported")?;
        if pieces.len() % 20 != 0 {
            bail!(
                "`info.pieces` is {} bytes, not a multiple of 20",
                pieces.len()
            );
        }
        let piece_hashes: Vec<Id20> = pieces
            .as_chunks::<20>()
            .0
            .iter()
            .map(|c| Id20::from_slice(c).expect("20-byte chunk"))
            .collect();

        let root = sanitize_component(&name).context("invalid torrent name")?;
        let mut files = Vec::new();
        let mut offset = 0u64;

        match info.list_at("files") {
            // Multi-file: `name` becomes the root directory.
            Some(entries) => {
                for entry in entries {
                    let length = entry
                        .int_at("length")
                        .context("file without `length`")?
                        .max(0) as u64;
                    let components = entry
                        .list_at("path")
                        .or_else(|| entry.list_at("path.utf-8"))
                        .context("file without `path`")?;
                    let mut path = PathBuf::from(&root);
                    for c in components {
                        let part = c.as_str().context("path component is not a string")?;
                        path.push(sanitize_component(&part)?);
                    }
                    files.push(FileEntry {
                        path,
                        length,
                        offset,
                    });
                    offset += length;
                }
            }
            // Single file: `name` is the file itself.
            None => {
                let length = info
                    .int_at("length")
                    .context("torrent without `length` or `files`")?
                    .max(0) as u64;
                files.push(FileEntry {
                    path: PathBuf::from(&root),
                    length,
                    offset: 0,
                });
                offset = length;
            }
        }

        let total_length = offset;
        let expected = piece_hashes.len() as u64 * piece_length as u64;
        if total_length > expected || expected.saturating_sub(total_length) >= piece_length as u64 {
            bail!(
                "total length ({total_length}) does not match {} pieces of {piece_length} bytes",
                piece_hashes.len()
            );
        }

        Ok(Info {
            info_hash,
            name,
            piece_length,
            piece_hashes,
            files,
            total_length,
            private: info.int_at("private").unwrap_or(0) == 1,
            raw: raw.to_vec(),
        })
    }

    pub fn num_pieces(&self) -> usize {
        self.piece_hashes.len()
    }

    /// The last piece is usually smaller than the others.
    pub fn piece_size(&self, index: usize) -> u32 {
        let start = index as u64 * self.piece_length as u64;
        let remaining = self.total_length.saturating_sub(start);
        remaining.min(self.piece_length as u64) as u32
    }
}

/// The complete `.torrent` file: the `info` plus the auxiliary metadata.
#[derive(Debug, Clone)]
pub struct Metainfo {
    pub info: Info,
    /// Trackers grouped in tiers (BEP-12): try the whole tier 1, then tier 2…
    pub tiers: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
    /// Web seeds (BEP-19), used as an additional HTTP source.
    pub web_seeds: Vec<String>,
    /// DHT nodes embedded in the torrent (BEP-5), useful for bootstrap.
    pub dht_nodes: Vec<String>,
}

impl Metainfo {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Metainfo> {
        let path = path.as_ref();
        let raw =
            std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        Metainfo::from_bytes(&raw)
            .with_context(|| format!("`{}` is not a valid .torrent", path.display()))
    }

    pub fn from_bytes(raw: &[u8]) -> Result<Metainfo> {
        let decoded = bencode::decode(raw)?;
        let root = &decoded.value;

        let info_raw = decoded
            .raw(raw, "info")
            .context("`.torrent` without `info` dictionary")?;
        let info = Info::from_value(root.require("info")?, info_raw)?;

        let mut tiers: Vec<Vec<String>> = Vec::new();
        if let Some(list) = root.list_at("announce-list") {
            for tier in list {
                let urls: Vec<String> = tier
                    .as_list()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|u| u.as_str())
                    .filter(|u| !u.is_empty())
                    .collect();
                if !urls.is_empty() {
                    tiers.push(urls);
                }
            }
        }
        // `announce` only enters if not already covered by the announce-list.
        if let Some(announce) = root.str_at("announce")
            && !announce.is_empty()
            && !tiers.iter().flatten().any(|u| *u == announce)
        {
            tiers.insert(0, vec![announce]);
        }

        let web_seeds = match root.get("url-list") {
            Some(Value::Bytes(b)) => vec![String::from_utf8_lossy(b).into_owned()],
            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str()).collect(),
            _ => Vec::new(),
        };

        let dht_nodes = root
            .list_at("nodes")
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|n| {
                        let pair = n.as_list()?;
                        let host = pair.first()?.as_str()?;
                        let port = pair.get(1)?.as_int()?;
                        Some(format!("{host}:{port}"))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Metainfo {
            info,
            tiers,
            comment: root.str_at("comment"),
            created_by: root.str_at("created by"),
            creation_date: root.int_at("creation date"),
            web_seeds,
            dht_nodes,
        })
    }
}

/// Names reserved by Windows, regardless of the extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Converts a path component coming from the torrent into something safe to write.
///
/// A `.torrent` is a third-party file: without this, a `path` like
/// `["..", "..", "autorun.inf"]` would write outside the destination directory.
fn sanitize_component(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if (c as u32) < 0x20 => out.push('_'),
            c => out.push(c),
        }
    }
    // Trailing spaces and dots are silently removed by Windows, which would
    // make the written path diverge from the verified path.
    let trimmed = out.trim_end_matches([' ', '.']).to_string();
    let out = if trimmed.is_empty() { out } else { trimmed };

    if out.is_empty() || out == "." || out == ".." {
        bail!("unsafe path component: `{raw}`");
    }
    let stem = out.split('.').next().unwrap_or(&out).to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return Ok(format!("_{out}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal two-file `.torrent`.
    fn test_torrent() -> Vec<u8> {
        let file = |len: i64, parts: &[&str]| {
            Value::dict([
                ("length", Value::Int(len)),
                (
                    "path",
                    Value::List(parts.iter().map(|p| Value::bytes(*p)).collect()),
                ),
            ])
        };
        let info = Value::dict([
            (
                "files",
                Value::List(vec![file(6, &["a.txt"]), file(4, &["sub", "b.txt"])]),
            ),
            ("name", Value::bytes("demo")),
            ("piece length", Value::Int(16384)),
            ("pieces", Value::Bytes(vec![0u8; 20])),
        ]);
        bencode::encode(&Value::dict([
            ("announce", Value::bytes("http://tracker/ann")),
            ("info", info),
        ]))
    }

    #[test]
    fn reads_multi_file_torrent() {
        let meta = Metainfo::from_bytes(&test_torrent()).unwrap();
        assert_eq!(meta.info.name, "demo");
        assert_eq!(meta.info.total_length, 10);
        assert_eq!(meta.info.files.len(), 2);
        assert_eq!(meta.info.files[0].path, PathBuf::from("demo").join("a.txt"));
        assert_eq!(meta.info.files[1].offset, 6);
        assert_eq!(meta.tiers, vec![vec!["http://tracker/ann".to_string()]]);
    }

    #[test]
    fn info_hash_comes_from_original_bytes() {
        let meta = Metainfo::from_bytes(&test_torrent()).unwrap();
        let expected = Id20(Sha1::digest(&meta.info.raw).into());
        assert_eq!(meta.info.info_hash, expected);
    }

    #[test]
    fn last_piece_is_smaller() {
        let meta = Metainfo::from_bytes(&test_torrent()).unwrap();
        assert_eq!(meta.info.num_pieces(), 1);
        assert_eq!(meta.info.piece_size(0), 10);
    }

    #[test]
    fn sanitizes_directory_traversal() {
        assert!(sanitize_component("..").is_err());
        assert_eq!(sanitize_component("a/b").unwrap(), "a_b");
        assert_eq!(sanitize_component("nul").unwrap(), "_nul");
        assert_eq!(sanitize_component("end. ").unwrap(), "end");
    }
}
