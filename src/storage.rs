//! Camada de disco: traduz índices de peça em escritas e leituras posicionais
//! sobre os arquivos do torrent.
//!
//! As peças cobrem a *concatenação* de todos os arquivos, então uma peça pode
//! atravessar vários deles. Todas as operações usam I/O posicional (`write_at`
//! / `seek_write`), o que dispensa mutex nos handles e permite que várias
//! tarefas gravem em paralelo.

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
    /// Guardado só para as mensagens de erro dizerem qual arquivo falhou.
    paths: Vec<PathBuf>,
}

impl Storage {
    /// Abre (criando se preciso) todos os arquivos do torrent sob `dest`.
    pub fn open(info: Arc<Info>, dest: &Path) -> Result<Storage> {
        let mut files = Vec::with_capacity(info.files.len());
        let mut paths = Vec::with_capacity(info.files.len());

        for entry in &info.files {
            let path = dest.join(&entry.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("não consegui criar {}", parent.display()))?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                // Nunca truncar: o conteúdo que já está aí pode ser um download
                // anterior que a verificação vai aproveitar.
                .truncate(false)
                .open(&path)
                .with_context(|| format!("não consegui abrir {}", path.display()))?;

            // Reservar o tamanho final evita fragmentação e falha cedo se não
            // houver espaço. Só encolhemos nunca: um arquivo maior pode ser um
            // download anterior que ainda queremos aproveitar no resume.
            if file.metadata()?.len() < entry.length {
                file.set_len(entry.length).with_context(|| {
                    format!("não consegui reservar {} bytes em {}", entry.length, path.display())
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
            bail!("peça {index}: esperava {expected} bytes, recebi {}", data.len());
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
            bail!("bloco {offset}+{len} ultrapassa a peça {index} ({piece_size} bytes)");
        }
        let mut buf = vec![0u8; len as usize];
        let start = index as u64 * self.info.piece_length as u64 + offset as u64;
        self.for_each_segment(start, buf.len(), |file_idx, off, range| {
            read_at(&self.files[file_idx], &mut buf[range], off)
        })?;
        Ok(buf)
    }

    /// Lê a peça do disco e confere o SHA-1 contra o do metainfo.
    pub fn verify_piece(&self, index: usize) -> Result<bool> {
        let data = self.read_piece(index)?;
        Ok(Id20(Sha1::digest(&data).into()) == self.info.piece_hashes[index])
    }

    /// Varre o disco inteiro para descobrir o que já existe (resume).
    ///
    /// `done` é incrementado a cada peça conferida, para a UI mostrar progresso.
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
                            // Um erro de leitura aqui só significa "não temos
                            // esta peça" — o download a busca de novo.
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

    /// Percorre os trechos de arquivo que cobrem `len` bytes a partir de
    /// `start` (offset no fluxo concatenado do torrent).
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
                bail!("offset {pos} está além do fim do torrent");
            };
            // Arquivos de tamanho zero são válidos e não consomem bytes.
            if entry.length == 0 {
                index += 1;
                continue;
            }
            let in_file = pos - entry.offset;
            let take = ((entry.length - in_file) as usize).min(len - written);
            visit(index, in_file, written..written + take).with_context(|| {
                format!("erro de I/O em {}", self.paths[index].display())
            })?;
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
            return Err(io::Error::new(io::ErrorKind::WriteZero, "escrita parcial"));
        }
        done += n;
    }
    Ok(())
}

/// Lê preenchendo com zeros o que faltar: um arquivo truncado por um download
/// interrompido deve reprovar na verificação, não abortar a varredura.
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

    /// Torrent de 3 peças de 4 bytes espalhadas por dois arquivos (10 bytes).
    fn info_de_teste(dados: &[u8]) -> Arc<Info> {
        let piece_len = 4usize;
        let mut pieces = Vec::new();
        for chunk in dados.chunks(piece_len) {
            pieces.extend_from_slice(&Sha1::digest(chunk));
        }
        let arquivo = |len: i64, nome: &str| {
            Value::dict([
                ("length", Value::Int(len)),
                ("path", Value::List(vec![Value::bytes(nome)])),
            ])
        };
        let info = Value::dict([
            (
                "files",
                Value::List(vec![arquivo(6, "a.bin"), arquivo(4, "b.bin")]),
            ),
            ("name", Value::bytes("pacote")),
            ("piece length", Value::Int(piece_len as i64)),
            ("pieces", Value::Bytes(pieces)),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        Arc::new(Metainfo::from_bytes(&raw).unwrap().info)
    }

    fn temp_dir(nome: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("torrra-test-{nome}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn escreve_peca_que_cruza_arquivos() {
        let dados: Vec<u8> = (0..10u8).collect();
        let info = info_de_teste(&dados);
        let dir = temp_dir("cross");
        let st = Storage::open(info.clone(), &dir).unwrap();

        for i in 0..info.num_pieces() {
            let ini = i * info.piece_length as usize;
            let fim = (ini + info.piece_size(i) as usize).min(dados.len());
            st.write_piece(i, &dados[ini..fim]).unwrap();
        }

        // A peça 1 (bytes 4..8) começa em a.bin e termina em b.bin.
        assert_eq!(std::fs::read(dir.join("pacote/a.bin")).unwrap(), &dados[..6]);
        assert_eq!(std::fs::read(dir.join("pacote/b.bin")).unwrap(), &dados[6..]);
        assert_eq!(st.read_piece(1).unwrap(), &dados[4..8]);
        assert_eq!(st.read_block(0, 1, 2).unwrap(), &dados[1..3]);
        assert_eq!(st.info.piece_size(2), 2, "última peça é parcial");
    }

    #[test]
    fn verifica_e_detecta_corrupcao() {
        let dados: Vec<u8> = (0..10u8).collect();
        let info = info_de_teste(&dados);
        let dir = temp_dir("verify");
        let st = Storage::open(info.clone(), &dir).unwrap();

        let done = AtomicUsize::new(0);
        assert_eq!(st.verify_all(&done).unwrap().count(), 0, "disco vazio");
        assert_eq!(done.load(Ordering::Relaxed), info.num_pieces());

        for i in 0..info.num_pieces() {
            let ini = i * 4;
            st.write_piece(i, &dados[ini..ini + info.piece_size(i) as usize])
                .unwrap();
        }
        assert!(st.verify_all(&AtomicUsize::new(0)).unwrap().is_complete());

        st.write_piece(1, b"XXXX").unwrap();
        assert!(!st.verify_piece(1).unwrap());
        assert!(st.verify_piece(0).unwrap(), "peça vizinha intacta");
    }

    #[test]
    fn rejeita_tamanhos_invalidos() {
        let info = info_de_teste(&(0..10u8).collect::<Vec<_>>());
        let st = Storage::open(info, &temp_dir("bounds")).unwrap();
        assert!(st.write_piece(0, b"abc").is_err());
        assert!(st.read_block(0, 3, 2).is_err());
    }
}
