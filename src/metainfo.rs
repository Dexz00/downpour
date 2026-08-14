//! Leitura de arquivos `.torrent` (BEP-3) e do dicionário `info` isolado
//! (que também chega pela rede via `ut_metadata`, BEP-9).

use crate::bencode::{self, Value};
use crate::id::Id20;
use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

/// Um arquivo dentro do torrent, já posicionado no fluxo concatenado de bytes
/// que as peças cobrem.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Caminho relativo já saneado, seguro para juntar ao diretório de saída.
    pub path: PathBuf,
    pub length: u64,
    /// Deslocamento do início deste arquivo dentro do fluxo do torrent.
    pub offset: u64,
}

impl FileEntry {
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// O dicionário `info` decodificado — tudo que define *o conteúdo* do torrent.
#[derive(Debug, Clone)]
pub struct Info {
    pub info_hash: Id20,
    pub name: String,
    pub piece_length: u32,
    pub piece_hashes: Vec<Id20>,
    pub files: Vec<FileEntry>,
    pub total_length: u64,
    /// Torrent privado: proíbe DHT e PEX (BEP-27).
    pub private: bool,
    /// Bytes originais do dicionário, servidos a outros peers via `ut_metadata`.
    pub raw: Vec<u8>,
}

impl Info {
    /// Decodifica um dicionário `info` a partir dos seus bytes bencode crus.
    pub fn from_raw(raw: &[u8]) -> Result<Info> {
        let decoded = bencode::decode(raw).context("dicionário `info` inválido")?;
        Info::from_value(&decoded.value, raw)
    }

    fn from_value(info: &Value, raw: &[u8]) -> Result<Info> {
        let info_hash = Id20(Sha1::digest(raw).into());

        let name = info
            .str_at("name")
            .or_else(|| info.str_at("name.utf-8"))
            .context("`info.name` ausente")?;

        let piece_length = info
            .int_at("piece length")
            .context("`info.piece length` ausente")?;
        if piece_length <= 0 || piece_length > 128 << 20 {
            bail!("`piece length` fora do intervalo aceitável: {piece_length}");
        }
        let piece_length = piece_length as u32;

        let pieces = info
            .bytes_at("pieces")
            .context("`info.pieces` ausente — torrents somente v2 não são suportados")?;
        if pieces.len() % 20 != 0 {
            bail!("`info.pieces` tem {} bytes, não múltiplo de 20", pieces.len());
        }
        let piece_hashes: Vec<Id20> = pieces
            .chunks_exact(20)
            .map(|c| Id20::from_slice(c).expect("chunk de 20 bytes"))
            .collect();

        let root = sanitize_component(&name).context("nome do torrent inválido")?;
        let mut files = Vec::new();
        let mut offset = 0u64;

        match info.list_at("files") {
            // Multi-arquivo: `name` vira o diretório raiz.
            Some(entries) => {
                for entry in entries {
                    let length = entry
                        .int_at("length")
                        .context("arquivo sem `length`")?
                        .max(0) as u64;
                    let components = entry
                        .list_at("path")
                        .or_else(|| entry.list_at("path.utf-8"))
                        .context("arquivo sem `path`")?;
                    let mut path = PathBuf::from(&root);
                    for c in components {
                        let part = c.as_str().context("componente de path não é string")?;
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
            // Arquivo único: `name` é o próprio arquivo.
            None => {
                let length = info
                    .int_at("length")
                    .context("torrent sem `length` nem `files`")?
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
                "tamanho total ({total_length}) não bate com {} peças de {piece_length} bytes",
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

    /// A última peça costuma ser menor que as demais.
    pub fn piece_size(&self, index: usize) -> u32 {
        let start = index as u64 * self.piece_length as u64;
        let remaining = self.total_length.saturating_sub(start);
        remaining.min(self.piece_length as u64) as u32
    }

}

/// O arquivo `.torrent` completo: o `info` mais os metadados auxiliares.
#[derive(Debug, Clone)]
pub struct Metainfo {
    pub info: Info,
    /// Trackers agrupados em tiers (BEP-12): tenta o tier 1 inteiro, depois o 2…
    pub tiers: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
    /// Web seeds (BEP-19), usados como fonte HTTP adicional.
    pub web_seeds: Vec<String>,
    /// Nós DHT embutidos no torrent (BEP-5), úteis para bootstrap.
    pub dht_nodes: Vec<String>,
}

impl Metainfo {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Metainfo> {
        let path = path.as_ref();
        let raw = std::fs::read(path)
            .with_context(|| format!("não consegui ler {}", path.display()))?;
        Metainfo::from_bytes(&raw)
            .with_context(|| format!("`{}` não é um .torrent válido", path.display()))
    }

    pub fn from_bytes(raw: &[u8]) -> Result<Metainfo> {
        let decoded = bencode::decode(raw)?;
        let root = &decoded.value;

        let info_raw = decoded
            .raw(raw, "info")
            .context("`.torrent` sem dicionário `info`")?;
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
        // `announce` só entra se ainda não estiver coberto pelo announce-list.
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

/// Nomes reservados pelo Windows, independentemente da extensão.
const RESERVADOS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Converte um componente de caminho vindo do torrent em algo seguro de escrever.
///
/// Um `.torrent` é um arquivo de terceiros: sem isso, um `path` como
/// `["..", "..", "autorun.inf"]` escreveria fora do diretório de destino.
fn sanitize_component(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if (c as u32) < 0x20 => out.push('_'),
            c => out.push(c),
        }
    }
    // Espaços e pontos finais são silenciosamente removidos pelo Windows,
    // o que faria o caminho gravado divergir do caminho verificado.
    let trimmed = out.trim_end_matches([' ', '.']).to_string();
    let out = if trimmed.is_empty() { out } else { trimmed };

    if out.is_empty() || out == "." || out == ".." {
        bail!("componente de caminho inseguro: `{raw}`");
    }
    let stem = out.split('.').next().unwrap_or(&out).to_ascii_uppercase();
    if RESERVADOS.contains(&stem.as_str()) {
        return Ok(format!("_{out}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta um `.torrent` mínimo de dois arquivos.
    fn torrent_de_teste() -> Vec<u8> {
        let arquivo = |len: i64, partes: &[&str]| {
            Value::dict([
                ("length", Value::Int(len)),
                (
                    "path",
                    Value::List(partes.iter().map(|p| Value::bytes(*p)).collect()),
                ),
            ])
        };
        let info = Value::dict([
            (
                "files",
                Value::List(vec![arquivo(6, &["a.txt"]), arquivo(4, &["sub", "b.txt"])]),
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
    fn le_torrent_multi_arquivo() {
        let meta = Metainfo::from_bytes(&torrent_de_teste()).unwrap();
        assert_eq!(meta.info.name, "demo");
        assert_eq!(meta.info.total_length, 10);
        assert_eq!(meta.info.files.len(), 2);
        assert_eq!(meta.info.files[0].path, PathBuf::from("demo").join("a.txt"));
        assert_eq!(meta.info.files[1].offset, 6);
        assert_eq!(meta.tiers, vec![vec!["http://tracker/ann".to_string()]]);
    }

    #[test]
    fn info_hash_vem_dos_bytes_originais() {
        let meta = Metainfo::from_bytes(&torrent_de_teste()).unwrap();
        let esperado = Id20(Sha1::digest(&meta.info.raw).into());
        assert_eq!(meta.info.info_hash, esperado);
    }

    #[test]
    fn ultima_peca_e_menor() {
        let meta = Metainfo::from_bytes(&torrent_de_teste()).unwrap();
        assert_eq!(meta.info.num_pieces(), 1);
        assert_eq!(meta.info.piece_size(0), 10);
    }

    #[test]
    fn sanitiza_travessia_de_diretorio() {
        assert!(sanitize_component("..").is_err());
        assert_eq!(sanitize_component("a/b").unwrap(), "a_b");
        assert_eq!(sanitize_component("nul").unwrap(), "_nul");
        assert_eq!(sanitize_component("fim. ").unwrap(), "fim");
    }
}
