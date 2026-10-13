//! Messages of the peer wire protocol (BEP-3), plus the `fast` (BEP-6)
//! and `extended` (BEP-10) extensions.

use crate::picker::BlockRequest;
use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Cap on the announced size of a message. A bitfield for a torrent with
/// 8 million pieces still fits comfortably here; anything larger means a
/// hostile peer or a desynchronized stream.
const MAX_MESSAGE: u32 = 1 << 20;

pub const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";

/// `reserved` bits that we announce in the handshake.
pub mod reserved {
    /// BEP-10: extension protocol (enables ut_metadata and ut_pex).
    pub const EXTENSION: (usize, u8) = (5, 0x10);
    /// BEP-6: fast extension.
    pub const FAST: (usize, u8) = (7, 0x04);
    /// BEP-5: this client supports DHT.
    pub const DHT: (usize, u8) = (7, 0x01);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request(BlockRequest),
    Piece {
        index: u32,
        begin: u32,
        block: Vec<u8>,
    },
    Cancel(BlockRequest),
    /// The peer's DHT port (BEP-5).
    Port(u16),
    // -- fast extension (BEP-6) --
    SuggestPiece(u32),
    HaveAll,
    HaveNone,
    RejectRequest(BlockRequest),
    AllowedFast(u32),
    // -- extension protocol (BEP-10) --
    Extended {
        id: u8,
        payload: Vec<u8>,
    },
}

impl Message {
    fn id(&self) -> Option<u8> {
        Some(match self {
            Message::KeepAlive => return None,
            Message::Choke => 0,
            Message::Unchoke => 1,
            Message::Interested => 2,
            Message::NotInterested => 3,
            Message::Have(_) => 4,
            Message::Bitfield(_) => 5,
            Message::Request(_) => 6,
            Message::Piece { .. } => 7,
            Message::Cancel(_) => 8,
            Message::Port(_) => 9,
            Message::SuggestPiece(_) => 0x0D,
            Message::HaveAll => 0x0E,
            Message::HaveNone => 0x0F,
            Message::RejectRequest(_) => 0x10,
            Message::AllowedFast(_) => 0x11,
            Message::Extended { .. } => 20,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0, 0, 0, 0]); // reserved for the length

        if let Some(id) = self.id() {
            out.push(id);
            match self {
                Message::Have(i) | Message::SuggestPiece(i) | Message::AllowedFast(i) => {
                    out.extend_from_slice(&i.to_be_bytes());
                }
                Message::Bitfield(bits) => out.extend_from_slice(bits),
                Message::Request(r) | Message::Cancel(r) | Message::RejectRequest(r) => {
                    out.extend_from_slice(&r.piece.to_be_bytes());
                    out.extend_from_slice(&r.offset.to_be_bytes());
                    out.extend_from_slice(&r.length.to_be_bytes());
                }
                Message::Piece {
                    index,
                    begin,
                    block,
                } => {
                    out.extend_from_slice(&index.to_be_bytes());
                    out.extend_from_slice(&begin.to_be_bytes());
                    out.extend_from_slice(block);
                }
                Message::Port(p) => out.extend_from_slice(&p.to_be_bytes()),
                Message::Extended { id, payload } => {
                    out.push(*id);
                    out.extend_from_slice(payload);
                }
                _ => {}
            }
        }

        let len = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }

    pub fn decode(buf: &[u8]) -> Result<Message> {
        if buf.is_empty() {
            return Ok(Message::KeepAlive);
        }
        let id = buf[0];
        let body = &buf[1..];

        let u32_at = |off: usize| -> Result<u32> {
            match body.get(off..off + 4) {
                Some(s) => Ok(u32::from_be_bytes(s.try_into().unwrap())),
                None => bail!("message id={id} truncated"),
            }
        };
        let block_req = || -> Result<BlockRequest> {
            Ok(BlockRequest {
                piece: u32_at(0)?,
                offset: u32_at(4)?,
                length: u32_at(8)?,
            })
        };

        Ok(match id {
            0 => Message::Choke,
            1 => Message::Unchoke,
            2 => Message::Interested,
            3 => Message::NotInterested,
            4 => Message::Have(u32_at(0)?),
            5 => Message::Bitfield(body.to_vec()),
            6 => Message::Request(block_req()?),
            7 => Message::Piece {
                index: u32_at(0)?,
                begin: u32_at(4)?,
                block: body.get(8..).unwrap_or_default().to_vec(),
            },
            8 => Message::Cancel(block_req()?),
            9 => {
                let bytes: [u8; 2] = match body.get(..2) {
                    Some(s) => s.try_into().unwrap(),
                    None => bail!("`port` message truncated"),
                };
                Message::Port(u16::from_be_bytes(bytes))
            }
            0x0D => Message::SuggestPiece(u32_at(0)?),
            0x0E => Message::HaveAll,
            0x0F => Message::HaveNone,
            0x10 => Message::RejectRequest(block_req()?),
            0x11 => Message::AllowedFast(u32_at(0)?),
            20 => match body.split_first() {
                Some((id, payload)) => Message::Extended {
                    id: *id,
                    payload: payload.to_vec(),
                },
                None => bail!("`extended` message without id"),
            },
            other => bail!("unknown message id={other}"),
        })
    }
}

/// Reads a complete message from the socket.
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE {
        bail!("peer announced a message of {len} bytes");
    }
    if len == 0 {
        return Ok(Message::KeepAlive);
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Message::decode(&buf)
}

pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, msg: &Message) -> Result<()> {
    let mut buf = Vec::with_capacity(64);
    msg.encode(&mut buf);
    writer.write_all(&buf).await?;
    Ok(())
}

/// BitTorrent handshake: a fixed 68 bytes.
#[derive(Debug, Clone)]
pub struct Handshake {
    pub reserved: [u8; 8],
    pub info_hash: crate::id::Id20,
    pub peer_id: crate::id::Id20,
}

impl Handshake {
    pub fn new(info_hash: crate::id::Id20, peer_id: crate::id::Id20, dht: bool) -> Handshake {
        let mut reserved = [0u8; 8];
        reserved[reserved::EXTENSION.0] |= reserved::EXTENSION.1;
        reserved[reserved::FAST.0] |= reserved::FAST.1;
        if dht {
            reserved[reserved::DHT.0] |= reserved::DHT.1;
        }
        Handshake {
            reserved,
            info_hash,
            peer_id,
        }
    }

    pub fn supports(&self, bit: (usize, u8)) -> bool {
        self.reserved[bit.0] & bit.1 != 0
    }

    pub fn to_bytes(&self) -> [u8; 68] {
        let mut buf = [0u8; 68];
        buf[0] = 19;
        buf[1..20].copy_from_slice(PROTOCOL);
        buf[20..28].copy_from_slice(&self.reserved);
        buf[28..48].copy_from_slice(self.info_hash.as_bytes());
        buf[48..68].copy_from_slice(self.peer_id.as_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; 68]) -> Result<Handshake> {
        if buf[0] != 19 || &buf[1..20] != PROTOCOL {
            bail!("handshake is not BitTorrent");
        }
        Ok(Handshake {
            reserved: buf[20..28].try_into().unwrap(),
            info_hash: crate::id::Id20::from_slice(&buf[28..48])?,
            peer_id: crate::id::Id20::from_slice(&buf[48..68])?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Id20;

    fn round_trip(msg: Message) {
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(len, buf.len() - 4, "length prefix matches the body");
        assert_eq!(Message::decode(&buf[4..]).unwrap(), msg);
    }

    #[test]
    fn encodes_and_decodes_all_messages() {
        let req = BlockRequest {
            piece: 7,
            offset: 16384,
            length: 16384,
        };
        for msg in [
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(42),
            Message::Bitfield(vec![0xff, 0x0f]),
            Message::Request(req),
            Message::Piece {
                index: 1,
                begin: 32,
                block: vec![9; 100],
            },
            Message::Cancel(req),
            Message::Port(6881),
            Message::SuggestPiece(3),
            Message::HaveAll,
            Message::HaveNone,
            Message::RejectRequest(req),
            Message::AllowedFast(5),
            Message::Extended {
                id: 2,
                payload: b"d1:ai1ee".to_vec(),
            },
        ] {
            round_trip(msg);
        }
    }

    #[test]
    fn rejects_truncated_and_unknown_messages() {
        assert!(Message::decode(&[4, 0, 0]).is_err(), "short have");
        assert!(Message::decode(&[6, 0, 0, 0, 1]).is_err(), "short request");
        assert!(Message::decode(&[20]).is_err(), "extended without id");
        assert!(Message::decode(&[99]).is_err(), "unknown id");
    }

    #[test]
    fn handshake_announces_extensions() {
        let ih = Id20::random();
        let pid = Id20::random();
        let hs = Handshake::new(ih, pid, true);
        let bytes = hs.to_bytes();
        let parsed = Handshake::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.info_hash, ih);
        assert_eq!(parsed.peer_id, pid);
        assert!(parsed.supports(reserved::EXTENSION));
        assert!(parsed.supports(reserved::FAST));
        assert!(parsed.supports(reserved::DHT));

        let without_dht = Handshake::new(ih, pid, false);
        assert!(!without_dht.supports(reserved::DHT));

        let mut bad = bytes;
        bad[1] = b'X';
        assert!(Handshake::from_bytes(&bad).is_err());
    }

    #[tokio::test]
    async fn reads_from_socket_in_sequence() {
        let mut stream = Vec::new();
        Message::Unchoke.encode(&mut stream);
        Message::KeepAlive.encode(&mut stream);
        Message::Have(9).encode(&mut stream);

        let mut cursor = std::io::Cursor::new(stream);
        assert_eq!(read_message(&mut cursor).await.unwrap(), Message::Unchoke);
        assert_eq!(read_message(&mut cursor).await.unwrap(), Message::KeepAlive);
        assert_eq!(read_message(&mut cursor).await.unwrap(), Message::Have(9));
        assert!(read_message(&mut cursor).await.is_err(), "end of stream");
    }

    #[tokio::test]
    async fn rejects_giant_message() {
        let mut cursor = std::io::Cursor::new((MAX_MESSAGE + 1).to_be_bytes().to_vec());
        assert!(read_message(&mut cursor).await.is_err());
    }
}
