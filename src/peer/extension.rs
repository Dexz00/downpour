//! Extension protocol (BEP-10) and the two extensions that matter here:
//! `ut_metadata` (BEP-9), which fetches the `info` dictionary from a magnet,
//! and `ut_pex` (BEP-11), which exchanges peers directly between clients.

use crate::bencode::{self, Value};
use crate::compact;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// Id we announce for `ut_metadata` in our extension handshake.
pub const UT_METADATA_ID: u8 = 1;
/// Id we announce for `ut_pex`.
pub const UT_PEX_ID: u8 = 2;
/// Size of each metadata piece (BEP-9 fixes it at 16 KiB).
pub const METADATA_PIECE: usize = 16 * 1024;

#[derive(Debug, Clone, Default)]
pub struct ExtensionHandshake {
    /// Extensions the peer supports: name -> id that *it* expects to receive.
    pub ids: BTreeMap<String, u8>,
    /// Total size of the `info` dictionary, when the peer knows it.
    pub metadata_size: Option<usize>,
    pub client: Option<String>,
    /// How many simultaneous requests the peer accepts.
    pub reqq: Option<usize>,
    /// TCP port where the peer listens (useful when we are the ones connecting).
    pub port: Option<u16>,
}

impl ExtensionHandshake {
    /// Handshake we send: we announce ut_metadata and, unless this is a private
    /// torrent, ut_pex.
    pub fn ours(metadata_size: Option<usize>, pex: bool, port: u16) -> ExtensionHandshake {
        let mut ids = BTreeMap::new();
        ids.insert("ut_metadata".to_string(), UT_METADATA_ID);
        if pex {
            ids.insert("ut_pex".to_string(), UT_PEX_ID);
        }
        ExtensionHandshake {
            ids,
            metadata_size,
            client: Some(format!("downpour/{}", env!("CARGO_PKG_VERSION"))),
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
            .context("invalid extension handshake")?
            .value;
        let ids = root
            .get("m")
            .and_then(|m| m.as_dict())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let id = v.as_int()?;
                        (0..=255)
                            .contains(&id)
                            .then_some((String::from_utf8_lossy(k).into_owned(), id as u8))
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

    /// Id to use when sending a message for this extension to the peer.
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
    Request {
        piece: usize,
    },
    Data {
        piece: usize,
        total: usize,
        data: Vec<u8>,
    },
    Reject {
        piece: usize,
    },
}

impl MetadataMessage {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, piece) = match self {
            MetadataMessage::Request { piece } => (0, *piece),
            MetadataMessage::Data { piece, .. } => (1, *piece),
            MetadataMessage::Reject { piece } => (2, *piece),
        };
        let mut root: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        root.insert(b"msg_type".to_vec(), Value::Int(kind));
        root.insert(b"piece".to_vec(), Value::Int(piece as i64));
        if let MetadataMessage::Data { total, .. } = self {
            root.insert(b"total_size".to_vec(), Value::Int(*total as i64));
        }
        let mut out = bencode::encode(&Value::Dict(root));
        // The data goes raw, immediately after the dictionary.
        if let MetadataMessage::Data { data, .. } = self {
            out.extend_from_slice(data);
        }
        out
    }

    pub fn decode(payload: &[u8]) -> Result<MetadataMessage> {
        let (header, consumed) =
            bencode::decode_prefix(payload).context("ut_metadata without a valid header")?;
        let piece = header
            .int_at("piece")
            .context("ut_metadata without `piece`")?;
        if piece < 0 {
            bail!("ut_metadata with a negative piece");
        }
        let piece = piece as usize;

        match header.int_at("msg_type") {
            Some(0) => Ok(MetadataMessage::Request { piece }),
            Some(1) => {
                let total = header
                    .int_at("total_size")
                    .filter(|t| *t > 0)
                    .context("ut_metadata without `total_size`")?
                    as usize;
                Ok(MetadataMessage::Data {
                    piece,
                    total,
                    data: payload[consumed..].to_vec(),
                })
            }
            Some(2) => Ok(MetadataMessage::Reject { piece }),
            other => bail!("ut_metadata with unexpected msg_type: {other:?}"),
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
        let root = bencode::decode(payload).context("invalid ut_pex")?.value;
        let collect = |v4: &str, v6: &str| {
            let mut addrs = root.bytes_at(v4).map(compact::parse_v4).unwrap_or_default();
            addrs.extend(root.bytes_at(v6).map(compact::parse_v6).unwrap_or_default());
            addrs
        };
        Ok(PexMessage {
            added: collect("added", "added6"),
            dropped: collect("dropped", "dropped6"),
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

/// Accumulates the pieces of `info` received via `ut_metadata`.
pub struct MetadataBuffer {
    buffer: Vec<u8>,
    received: Vec<bool>,
    remaining: usize,
}

impl MetadataBuffer {
    pub fn new(total: usize) -> MetadataBuffer {
        let pieces = total.div_ceil(METADATA_PIECE);
        MetadataBuffer {
            buffer: vec![0; total],
            received: vec![false; pieces],
            remaining: pieces,
        }
    }

    pub fn missing(&self) -> impl Iterator<Item = usize> + '_ {
        self.received
            .iter()
            .enumerate()
            .filter(|(_, r)| !**r)
            .map(|(i, _)| i)
    }

    pub fn is_complete(&self) -> bool {
        self.remaining == 0
    }

    /// Stores a piece; returns `true` if it was new and had the right size.
    pub fn add(&mut self, piece: usize, data: &[u8]) -> bool {
        let Some(slot) = self.received.get_mut(piece) else {
            return false;
        };
        if *slot {
            return false;
        }
        let start = piece * METADATA_PIECE;
        let end = (start + METADATA_PIECE).min(self.buffer.len());
        // Every piece is 16 KiB, except the last one. Any other size means a
        // confused or malicious peer.
        if start >= self.buffer.len() || data.len() != end - start {
            return false;
        }
        self.buffer[start..end].copy_from_slice(data);
        *slot = true;
        self.remaining -= 1;
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
    fn extension_handshake_round_trip() {
        let ours = ExtensionHandshake::ours(Some(4096), true, 6881);
        let parsed = ExtensionHandshake::decode(&ours.encode()).unwrap();
        assert_eq!(parsed.id_of("ut_metadata"), Some(UT_METADATA_ID));
        assert_eq!(parsed.id_of("ut_pex"), Some(UT_PEX_ID));
        assert_eq!(parsed.metadata_size, Some(4096));
        assert_eq!(parsed.port, Some(6881));
        assert!(parsed.client.unwrap().starts_with("downpour/"));

        let private = ExtensionHandshake::ours(None, false, 6881);
        assert!(
            !ExtensionHandshake::decode(&private.encode())
                .unwrap()
                .supports_pex()
        );
    }

    #[test]
    fn zero_id_means_extension_disabled() {
        // BEP-10: the peer uses id 0 to say it removed the extension.
        let payload = b"d1:md11:ut_metadatai0e6:ut_pexi3eee";
        let hs = ExtensionHandshake::decode(payload).unwrap();
        assert!(!hs.supports_metadata());
        assert_eq!(hs.id_of("ut_pex"), Some(3));
    }

    #[test]
    fn ut_metadata_round_trip() {
        let data_bytes = vec![7u8; 100];
        let msg = MetadataMessage::Data {
            piece: 2,
            total: 40000,
            data: data_bytes.clone(),
        };
        match MetadataMessage::decode(&msg.encode()).unwrap() {
            MetadataMessage::Data { piece, total, data } => {
                assert_eq!((piece, total), (2, 40000));
                assert_eq!(data, data_bytes);
            }
            other => panic!("expected Data, got {other:?}"),
        }

        let request = MetadataMessage::Request { piece: 1 };
        assert_eq!(MetadataMessage::decode(&request.encode()).unwrap(), request);
        let reject = MetadataMessage::Reject { piece: 0 };
        assert_eq!(MetadataMessage::decode(&reject.encode()).unwrap(), reject);
    }

    #[test]
    fn assembles_metadata_from_pieces() {
        let total = METADATA_PIECE + 500;
        let mut buf = MetadataBuffer::new(total);
        assert_eq!(buf.missing().count(), 2);

        assert!(!buf.add(0, &[1u8; 10]), "wrong size is rejected");
        assert!(buf.add(0, &[1u8; METADATA_PIECE]));
        assert!(!buf.add(0, &[1u8; METADATA_PIECE]), "duplicate");
        assert!(!buf.add(9, &[1u8; 500]), "nonexistent piece");
        assert_eq!(buf.missing().collect::<Vec<_>>(), vec![1]);

        assert!(buf.add(1, &[2u8; 500]));
        assert!(buf.is_complete());
        let ready = buf.take();
        assert_eq!(ready.len(), total);
        assert_eq!(ready[METADATA_PIECE], 2);
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
