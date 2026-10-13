//! Disk layer: translates piece indices into positional reads and writes
//! over the torrent's files.
//!
//! Pieces cover the *concatenation* of all files, so a piece may span
//! several of them. All operations use positional I/O (`write_at`
//! / `seek_write`), which avoids mutexes on the handles and allows multiple
//! tasks to write in parallel.

use crate::bitfield::Bitfield;
use crate::id::Id20;
use crate::metainfo::Info;
use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Storage {
    info: Arc<Info>,
    files: Vec<File>,
    /// Kept only so error messages can say which file failed.
    paths: Vec<PathBuf>,
}

impl Storage {
    /// Opens (creating if needed) all of the torrent's files under `dest`.
    pub fn open(info: Arc<Info>, dest: &Path) -> Result<Storage> {
        let mut files = Vec::with_capacity(info.files.len());
        let mut paths = Vec::with_capacity(info.files.len());

        for entry in &info.files {
            let path = dest.join(&entry.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("could not create {}", parent.display()))?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                // Never truncate: the content already there may be a previous
                // download that verification will reuse.
                .truncate(false)
                .open(&path)
                .with_context(|| format!("could not open {}", path.display()))?;

            // Reserving the final size avoids fragmentation and fails early if
            // there is no space. We never shrink: a larger file may be a
            // previous download we still want to reuse on resume.
            if file.metadata()?.len() < entry.length {
                file.set_len(entry.length).with_context(|| {
                    format!(
                        "could not reserve {} bytes in {}",
                        entry.length,
                        path.display()
                    )
                })?;
            }
            files.push(file);
            paths.push(path);
        }

        Ok(Storage { info, files, paths })
    }

    pub fn write_piece(&self, index: usize, data: &[u8]) -> Result<()> {
        let expected = self.info.piece_size(index) as usize;
        if data.len() != expected {
            bail!(
                "piece {index}: expected {expected} bytes, got {}",
                data.len()
            );
        }
        let start = index as u64 * self.info.piece_length as u64;
        self.for_each_segment(start, data.len(), |file_idx, offset, range| {
            write_at(&self.files[file_idx], &data[range], offset)
        })
    }

    pub fn read_piece(&self, index: usize) -> Result<Vec<u8>> {
        self.read_block(index, 0, self.info.piece_size(index))
    }

    pub fn read_block(&self, index: usize, offset: u32, len: u32) -> Result<Vec<u8>> {
        let piece_size = self.info.piece_size(index);
        if offset.saturating_add(len) > piece_size {
            bail!("block {offset}+{len} exceeds piece {index} ({piece_size} bytes)");
        }
        let mut buf = vec![0u8; len as usize];
        let start = index as u64 * self.info.piece_length as u64 + offset as u64;
        self.for_each_segment(start, buf.len(), |file_idx, off, range| {
            read_at(&self.files[file_idx], &mut buf[range], off)
        })?;
        Ok(buf)
    }

    /// Reads the piece from disk and checks its SHA-1 against the metainfo.
    pub fn verify_piece(&self, index: usize) -> Result<bool> {
        let data = self.read_piece(index)?;
        Ok(Id20(Sha1::digest(&data).into()) == self.info.piece_hashes[index])
    }

    /// Scans the entire disk to discover what already exists (resume).
    ///
    /// `done` is incremented for each verified piece, so the UI can show progress.
    pub fn verify_all(&self, done: &AtomicUsize) -> Result<Bitfield> {
        let total = self.info.num_pieces();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(4)
            .min(total.max(1));

        let next = AtomicUsize::new(0);
        let mut have = Bitfield::new(total);

        let parts: Vec<Vec<usize>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut found = Vec::new();
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            if index >= total {
                                break;
                            }
                            // A read error here only means "we don't have
                            // this piece" — the download will fetch it again.
                            if self.verify_piece(index).unwrap_or(false) {
                                found.push(index);
                            }
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                        found
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        for index in parts.into_iter().flatten() {
            have.set(index);
        }
        Ok(have)
    }

    /// Iterates over the file segments covering `len` bytes starting at
    /// `start` (offset in the torrent's concatenated stream).
    fn for_each_segment(
        &self,
        start: u64,
        len: usize,
        mut visit: impl FnMut(usize, u64, std::ops::Range<usize>) -> io::Result<()>,
    ) -> Result<()> {
        let entries = &self.info.files;
        let mut index = entries.partition_point(|f| f.end() <= start);
        let mut pos = start;
        let mut written = 0usize;

        while written < len {
            let Some(entry) = entries.get(index) else {
                bail!("offset {pos} is beyond the end of the torrent");
            };
            // Zero-length files are valid and consume no bytes.
            if entry.length == 0 {
                index += 1;
                continue;
            }
            let in_file = pos - entry.offset;
            let take = ((entry.length - in_file) as usize).min(len - written);
            visit(index, in_file, written..written + take)
                .with_context(|| format!("I/O error in {}", self.paths[index].display()))?;
            written += take;
            pos += take as u64;
            index += 1;
        }
        Ok(())
    }
}

fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = write_some(file, &buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "partial write"));
        }
        done += n;
    }
    Ok(())
}

/// Reads, filling whatever is missing with zeros: a file truncated by an
/// interrupted download must fail verification, not abort the scan.
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = read_some(file, &mut buf[done..], offset + done as u64)?;
        if n == 0 {
            buf[done..].fill(0);
            break;
        }
        done += n;
    }
    Ok(())
}

#[cfg(windows)]
fn write_some(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

#[cfg(windows)]
fn read_some(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn write_some(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

#[cfg(unix)]
fn read_some(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{self, Value};
    use crate::metainfo::Metainfo;

    /// Torrent of 3 pieces of 4 bytes spread across two files (10 bytes).
    fn test_info(data: &[u8]) -> Arc<Info> {
        let piece_len = 4usize;
        let mut pieces = Vec::new();
        for chunk in data.chunks(piece_len) {
            pieces.extend_from_slice(&Sha1::digest(chunk));
        }
        let file = |len: i64, name: &str| {
            Value::dict([
                ("length", Value::Int(len)),
                ("path", Value::List(vec![Value::bytes(name)])),
            ])
        };
        let info = Value::dict([
            (
                "files",
                Value::List(vec![file(6, "a.bin"), file(4, "b.bin")]),
            ),
            ("name", Value::bytes("bundle")),
            ("piece length", Value::Int(piece_len as i64)),
            ("pieces", Value::Bytes(pieces)),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        Arc::new(Metainfo::from_bytes(&raw).unwrap().info)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("torrra-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_piece_spanning_files() {
        let data: Vec<u8> = (0..10u8).collect();
        let info = test_info(&data);
        let dir = temp_dir("cross");
        let st = Storage::open(info.clone(), &dir).unwrap();

        for i in 0..info.num_pieces() {
            let start = i * info.piece_length as usize;
            let end = (start + info.piece_size(i) as usize).min(data.len());
            st.write_piece(i, &data[start..end]).unwrap();
        }

        // Piece 1 (bytes 4..8) starts in a.bin and ends in b.bin.
        assert_eq!(std::fs::read(dir.join("bundle/a.bin")).unwrap(), &data[..6]);
        assert_eq!(std::fs::read(dir.join("bundle/b.bin")).unwrap(), &data[6..]);
        assert_eq!(st.read_piece(1).unwrap(), &data[4..8]);
        assert_eq!(st.read_block(0, 1, 2).unwrap(), &data[1..3]);
        assert_eq!(st.info.piece_size(2), 2, "last piece is partial");
    }

    #[test]
    fn verifies_and_detects_corruption() {
        let data: Vec<u8> = (0..10u8).collect();
        let info = test_info(&data);
        let dir = temp_dir("verify");
        let st = Storage::open(info.clone(), &dir).unwrap();

        let done = AtomicUsize::new(0);
        assert_eq!(st.verify_all(&done).unwrap().count(), 0, "empty disk");
        assert_eq!(done.load(Ordering::Relaxed), info.num_pieces());

        for i in 0..info.num_pieces() {
            let start = i * 4;
            st.write_piece(i, &data[start..start + info.piece_size(i) as usize])
                .unwrap();
        }
        assert!(st.verify_all(&AtomicUsize::new(0)).unwrap().is_complete());

        st.write_piece(1, b"XXXX").unwrap();
        assert!(!st.verify_piece(1).unwrap());
        assert!(st.verify_piece(0).unwrap(), "neighboring piece intact");
    }

    #[test]
    fn rejects_invalid_sizes() {
        let info = test_info(&(0..10u8).collect::<Vec<_>>());
        let st = Storage::open(info, &temp_dir("bounds")).unwrap();
        assert!(st.write_piece(0, b"abc").is_err());
        assert!(st.read_block(0, 3, 2).is_err());
    }
}
