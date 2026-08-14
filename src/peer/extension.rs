//! Protocolo de extensões (BEP-10) e as duas extensões que importam aqui:
//! `ut_metadata` (BEP-9), que traz o `info` a partir de um magnet, e `ut_pex`
//! (BEP-11), que troca peers diretamente entre clientes.

use crate::bencode::{self, Value};
use crate::compact;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// Id que anunciamos para `ut_metadata` no nosso handshake de extensões.
pub const UT_METADATA_ID: u8 = 1;
/// Id que anunciamos para `ut_pex`.
pub const UT_PEX_ID: u8 = 2;
/// Tamanho de cada pedaço de metadata (BEP-9 fixa em 16 KiB).
pub const METADATA_PIECE: usize = 16 * 1024;

#[derive(Debug, Clone, Default)]
pub struct ExtensionHandshake {
    /// Extensões que o peer suporta: nome -> id que *ele* espera receber.
    pub ids: BTreeMap<String, u8>,
    /// Tamanho total do dicionário `info`, quando o peer o conhece.
    pub metadata_size: Option<usize>,
    pub client: Option<String>,
    /// Quantos pedidos simultâneos o peer aceita.
    pub reqq: Option<usize>,
    /// Porta TCP onde o peer escuta (útil quando somos nós que conectamos).
    pub port: Option<u16>,
}

impl ExtensionHandshake {
    /// Handshake que enviamos: anunciamos ut_metadata e, se não for um torrent
    /// privado, ut_pex.
    pub fn ours(metadata_size: Option<usize>, pex: bool, port: u16) -> ExtensionHandshake {
        let mut ids = BTreeMap::new();
        ids.insert("ut_metadata".to_string(), UT_METADATA_ID);
        if pex {
            ids.insert("ut_pex".to_string(), UT_PEX_ID);
        }
        ExtensionHandshake {
            ids,
            metadata_size,
            client: Some(format!("torrra/{}", env!("CARGO_PKG_VERSION"))),
            reqq: Some(250),
            port: Some(port),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let m = Value::Dict(
            self.ids
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), Value::Int(*v as i64)))
                .collect(),
        );
        let mut root: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        root.insert(b"m".to_vec(), m);
        if let Some(size) = self.metadata_size {
            root.insert(b"metadata_size".to_vec(), Value::Int(size as i64));
        }
        if let Some(client) = &self.client {
            root.insert(b"v".to_vec(), Value::bytes(client.as_str()));
        }
        if let Some(reqq) = self.reqq {
            root.insert(b"reqq".to_vec(), Value::Int(reqq as i64));
        }
        if let Some(port) = self.port {
            root.insert(b"p".to_vec(), Value::Int(port as i64));
        }
        bencode::encode(&Value::Dict(root))
    }

    pub fn decode(payload: &[u8]) -> Result<ExtensionHandshake> {
        let root = bencode::decode(payload)
            .context("handshake de extensões inválido")?
            .value;
        let ids = root
            .get("m")
            .and_then(|m| m.as_dict())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let id = v.as_int()?;
                        (0..=255).contains(&id).then_some((
                            String::from_utf8_lossy(k).into_owned(),
                            id as u8,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ExtensionHandshake {
            ids,
            metadata_size: root
                .int_at("metadata_size")
                .filter(|s| *s > 0)
                .map(|s| s as usize),
            client: root.str_at("v"),
            reqq: root.int_at("reqq").filter(|r| *r > 0).map(|r| r as usize),
            port: root
                .int_at("p")
                .filter(|p| (1..=65535).contains(p))
                .map(|p| p as u16),
        })
    }

    /// Id a usar ao enviar uma mensagem desta extensão para o peer.
    pub fn id_of(&self, name: &str) -> Option<u8> {
        self.ids.get(name).copied().filter(|id| *id != 0)
    }

    pub fn supports_metadata(&self) -> bool {
        self.id_of("ut_metadata").is_some()
    }

    pub fn supports_pex(&self) -> bool {
        self.id_of("ut_pex").is_some()
    }
}

// -- ut_metadata (BEP-9) ---------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataMessage {
    Request { piece: usize },
    Data { piece: usize, total: usize, data: Vec<u8> },
    Reject { piece: usize },
}

impl MetadataMessage {
    pub fn encode(&self) -> Vec<u8> {
        let (tipo, piece) = match self {
            MetadataMessage::Request { piece } => (0, *piece),
            MetadataMessage::Data { piece, .. } => (1, *piece),
            MetadataMessage::Reject { piece } => (2, *piece),
        };
        let mut root: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        root.insert(b"msg_type".to_vec(), Value::Int(tipo));
        root.insert(b"piece".to_vec(), Value::Int(piece as i64));
        if let MetadataMessage::Data { total, .. } = self {
            root.insert(b"total_size".to_vec(), Value::Int(*total as i64));
        }
        let mut out = bencode::encode(&Value::Dict(root));
        // Os dados vão crus, logo após o dicionário.
        if let MetadataMessage::Data { data, .. } = self {
            out.extend_from_slice(data);
        }
        out
    }

    pub fn decode(payload: &[u8]) -> Result<MetadataMessage> {
        let (header, consumido) =
            bencode::decode_prefix(payload).context("ut_metadata sem cabeçalho válido")?;
        let piece = header.int_at("piece").context("ut_metadata sem `piece`")?;
        if piece < 0 {
            bail!("ut_metadata com peça negativa");
        }
        let piece = piece as usize;

        match header.int_at("msg_type") {
            Some(0) => Ok(MetadataMessage::Request { piece }),
            Some(1) => {
                let total = header
                    .int_at("total_size")
                    .filter(|t| *t > 0)
                    .context("ut_metadata sem `total_size`")? as usize;
                Ok(MetadataMessage::Data {
                    piece,
                    total,
                    data: payload[consumido..].to_vec(),
                })
            }
            Some(2) => Ok(MetadataMessage::Reject { piece }),
            other => bail!("ut_metadata com msg_type inesperado: {other:?}"),
        }
    }
}

// -- ut_pex (BEP-11) -------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PexMessage {
    pub added: Vec<SocketAddr>,
    pub dropped: Vec<SocketAddr>,
}

impl PexMessage {
    pub fn decode(payload: &[u8]) -> Result<PexMessage> {
        let root = bencode::decode(payload).context("ut_pex inválido")?.value;
        let coletar = |v4: &str, v6: &str| {
            let mut addrs = root.bytes_at(v4).map(compact::parse_v4).unwrap_or_default();
            addrs.extend(root.bytes_at(v6).map(compact::parse_v6).unwrap_or_default());
            addrs
        };
        Ok(PexMessage {
            added: coletar("added", "added6"),
            dropped: coletar("dropped", "dropped6"),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let (add4, add6) = compact::encode(&self.added);
        let (drop4, drop6) = compact::encode(&self.dropped);
        let mut root: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        root.insert(b"added".to_vec(), Value::Bytes(add4));
        root.insert(b"added6".to_vec(), Value::Bytes(add6));
        root.insert(b"dropped".to_vec(), Value::Bytes(drop4));
        root.insert(b"dropped6".to_vec(), Value::Bytes(drop6));
        bencode::encode(&Value::Dict(root))
    }
}

/// Acumula os pedaços do `info` recebidos por `ut_metadata`.
pub struct MetadataBuffer {
    buffer: Vec<u8>,
    recebidos: Vec<bool>,
    faltando: usize,
}

impl MetadataBuffer {
    pub fn new(total: usize) -> MetadataBuffer {
        let pedacos = total.div_ceil(METADATA_PIECE);
        MetadataBuffer {
            buffer: vec![0; total],
            recebidos: vec![false; pedacos],
            faltando: pedacos,
        }
    }

    pub fn missing(&self) -> impl Iterator<Item = usize> + '_ {
        self.recebidos
            .iter()
            .enumerate()
            .filter(|(_, r)| !**r)
            .map(|(i, _)| i)
    }

    pub fn is_complete(&self) -> bool {
        self.faltando == 0
    }

    /// Guarda um pedaço; devolve `true` se ele era novo e tinha o tamanho certo.
    pub fn add(&mut self, piece: usize, data: &[u8]) -> bool {
        let Some(slot) = self.recebidos.get_mut(piece) else {
            return false;
        };
        if *slot {
            return false;
        }
        let inicio = piece * METADATA_PIECE;
        let fim = (inicio + METADATA_PIECE).min(self.buffer.len());
        // Todo pedaço tem 16 KiB, menos o último. Tamanho diferente disso
        // significa peer confuso ou malicioso.
        if inicio >= self.buffer.len() || data.len() != fim - inicio {
            return false;
        }
        self.buffer[inicio..fim].copy_from_slice(data);
        *slot = true;
        self.faltando -= 1;
        true
    }

    pub fn take(self) -> Vec<u8> {
        self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_de_extensoes_round_trip() {
        let nosso = ExtensionHandshake::ours(Some(4096), true, 6881);
        let lido = ExtensionHandshake::decode(&nosso.encode()).unwrap();
        assert_eq!(lido.id_of("ut_metadata"), Some(UT_METADATA_ID));
        assert_eq!(lido.id_of("ut_pex"), Some(UT_PEX_ID));
        assert_eq!(lido.metadata_size, Some(4096));
        assert_eq!(lido.port, Some(6881));
        assert!(lido.client.unwrap().starts_with("torrra/"));

        let privado = ExtensionHandshake::ours(None, false, 6881);
        assert!(!ExtensionHandshake::decode(&privado.encode())
            .unwrap()
            .supports_pex());
    }

    #[test]
    fn id_zero_significa_extensao_desligada() {
        // BEP-10: o peer usa id 0 para dizer que removeu a extensão.
        let payload = b"d1:md11:ut_metadatai0e6:ut_pexi3eee";
        let hs = ExtensionHandshake::decode(payload).unwrap();
        assert!(!hs.supports_metadata());
        assert_eq!(hs.id_of("ut_pex"), Some(3));
    }

    #[test]
    fn ut_metadata_round_trip() {
        let dados = vec![7u8; 100];
        let msg = MetadataMessage::Data {
            piece: 2,
            total: 40000,
            data: dados.clone(),
        };
        match MetadataMessage::decode(&msg.encode()).unwrap() {
            MetadataMessage::Data { piece, total, data } => {
                assert_eq!((piece, total), (2, 40000));
                assert_eq!(data, dados);
            }
            outro => panic!("esperava Data, veio {outro:?}"),
        }

        let pedido = MetadataMessage::Request { piece: 1 };
        assert_eq!(MetadataMessage::decode(&pedido.encode()).unwrap(), pedido);
        let recusa = MetadataMessage::Reject { piece: 0 };
        assert_eq!(MetadataMessage::decode(&recusa.encode()).unwrap(), recusa);
    }

    #[test]
    fn monta_metadata_em_pedacos() {
        let total = METADATA_PIECE + 500;
        let mut buf = MetadataBuffer::new(total);
        assert_eq!(buf.missing().count(), 2);

        assert!(!buf.add(0, &[1u8; 10]), "tamanho errado é recusado");
        assert!(buf.add(0, &[1u8; METADATA_PIECE]));
        assert!(!buf.add(0, &[1u8; METADATA_PIECE]), "duplicado");
        assert!(!buf.add(9, &[1u8; 500]), "peça inexistente");
        assert_eq!(buf.missing().collect::<Vec<_>>(), vec![1]);

        assert!(buf.add(1, &[2u8; 500]));
        assert!(buf.is_complete());
        let pronto = buf.take();
        assert_eq!(pronto.len(), total);
        assert_eq!(pronto[METADATA_PIECE], 2);
    }

    #[test]
    fn pex_round_trip() {
        let msg = PexMessage {
            added: vec!["1.2.3.4:6881".parse().unwrap()],
            dropped: vec!["[2001:db8::5]:51413".parse().unwrap()],
        };
        assert_eq!(PexMessage::decode(&msg.encode()).unwrap(), msg);
    }
}
