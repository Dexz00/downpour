//! Connection to a peer.
//!
//! Each peer is an independent task. It decides nothing about the download —
//! it only translates the protocol into events for the engine and executes
//! the commands it receives back. All policy (what to request, whom to
//! choke) lives in the engine, which is what sees the whole swarm.

pub mod extension;
pub mod message;

use crate::id::Id20;
use crate::picker::BlockRequest;
use anyhow::{Context, Result, bail};
use extension::{ExtensionHandshake, MetadataMessage, PexMessage};
use message::{Handshake, Message, read_message, reserved};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

/// How long we wait for a handshake before giving up on the peer.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(12);
/// Total silence for this long means a dead connection.
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);
/// Keep-alive interval; the de facto standard is 2 minutes.
const KEEPALIVE: Duration = Duration::from_secs(100);
/// How many upload requests we queue from a single peer before cutting it off.
const MAX_INCOMING_REQUESTS: usize = 500;
/// Largest block we are willing to serve.
const MAX_SERVE_LENGTH: u32 = 128 * 1024;

/// Local identifier of the peer within the torrent (slot index).
pub type Slot = u32;

#[derive(Debug)]
pub enum PeerCommand {
    /// Requests to send, batched (pipelining).
    Request(Vec<BlockRequest>),
    Cancel(BlockRequest),
    Have(u32),
    /// `true` = choke the peer (stop sending data to it).
    Choke(bool),
    Interested(bool),
    /// Data already read from disk, ready to go to the peer.
    SendBlock(BlockRequest, Vec<u8>),
    /// Explicit refusal of a request (fast extension).
    Reject(BlockRequest),
    RequestMetadata(usize),
    SendMetadata(MetadataMessage),
    SendPex(PexMessage),
    Shutdown,
}

#[derive(Debug)]
pub enum PeerEvent {
    Connected {
        slot: Slot,
        peer_id: Id20,
        supports_dht: bool,
    },
    /// The peer's extension handshake — arrives right after `Connected`, and
    /// may be resent at any time to enable/disable extensions.
    Extensions(Slot, Box<ExtensionHandshake>),
    /// Raw bitfield: with a magnet we do not yet know how many pieces exist.
    Bitfield(Slot, Vec<u8>),
    HaveAll(Slot),
    HaveNone(Slot),
    Have(Slot, u32),
    /// The peer choked us (`true`) or unchoked us (`false`).
    Choked(Slot, bool),
    Interested(Slot, bool),
    Block {
        slot: Slot,
        request: BlockRequest,
        data: Vec<u8>,
    },
    /// The peer rejected one of our requests (fast extension).
    Rejected(Slot, BlockRequest),
    /// The peer wants a block from us.
    BlockRequested(Slot, BlockRequest),
    Metadata(Slot, MetadataMessage),
    Pex(PexMessage),
    /// DHT port announced by the peer.
    DhtPort(Slot, u16),
    /// Bytes sent to this peer since the last report.
    Uploaded(Slot, u64),
    Disconnected(Slot, String),
}

#[derive(Clone)]
pub struct PeerConfig {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub listen_port: u16,
    /// Size of `info`, when we already have it (announced via ut_metadata).
    pub metadata_size: Option<usize>,
    /// Private torrents disable PEX and DHT.
    pub allow_pex: bool,
    pub dht: bool,
    /// Pieces we already have. The protocol only accepts `bitfield` as the
    /// first message after the handshake, so it must come ready from here —
    /// anything we learn later goes out via `have`.
    pub initial_bitfield: Option<Vec<u8>>,
}

/// The engine's handle for talking to a peer task.
pub struct PeerHandle {
    tx: mpsc::Sender<PeerCommand>,
}

impl PeerHandle {
    /// Queues a command. Returns `false` if the peer's queue is full or the
    /// connection died — in both cases the command is disposable.
    pub fn send(&self, cmd: PeerCommand) -> bool {
        self.tx.try_send(cmd).is_ok()
    }
}

/// Connects, performs the handshake, and runs the peer until the connection dies.
pub async fn spawn_outgoing(
    slot: Slot,
    addr: SocketAddr,
    cfg: PeerConfig,
    events: mpsc::Sender<PeerEvent>,
    connect_timeout: Duration,
) -> PeerHandle {
    let (tx, rx) = mpsc::channel(256);
    let handle = PeerHandle { tx };
    tokio::spawn(async move {
        let reason = match timeout(connect_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(socket)) => run(slot, socket, cfg, events.clone(), rx, None)
                .await
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "closed".to_string()),
            Ok(Err(e)) => format!("connection failed: {e}"),
            Err(_) => "timed out connecting".to_string(),
        };
        let _ = events.send(PeerEvent::Disconnected(slot, reason)).await;
    });
    handle
}

/// Takes over a connection that reached us, whose handshake the listener has
/// already read (it is the listener that discovers which torrent the
/// connection belongs to).
pub async fn spawn_incoming(
    slot: Slot,
    socket: TcpStream,
    addr: SocketAddr,
    handshake: Handshake,
    cfg: PeerConfig,
    events: mpsc::Sender<PeerEvent>,
) -> PeerHandle {
    let (tx, rx) = mpsc::channel(256);
    let handle = PeerHandle { tx };
    let _ = addr;
    tokio::spawn(async move {
        let reason = run(slot, socket, cfg, events.clone(), rx, Some(handshake))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "closed".to_string());
        let _ = events.send(PeerEvent::Disconnected(slot, reason)).await;
    });
    handle
}

/// Reads the handshake of an incoming connection before knowing which torrent
/// it belongs to. Returns the handshake and the socket, without consuming
/// anything further.
pub async fn read_incoming_handshake(socket: &mut TcpStream) -> Result<Handshake> {
    let mut buf = [0u8; 68];
    timeout(HANDSHAKE_TIMEOUT, socket.read_exact(&mut buf))
        .await
        .context("timed out waiting for handshake")??;
    Handshake::from_bytes(&buf)
}

async fn run(
    slot: Slot,
    socket: TcpStream,
    cfg: PeerConfig,
    events: mpsc::Sender<PeerEvent>,
    mut cmds: mpsc::Receiver<PeerCommand>,
    handshake_already_read: Option<Handshake>,
) -> Result<()> {
    // Latency matters more than aggregate throughput here: without this, a
    // small `request` can get stuck in Nagle's buffer waiting for company.
    socket.set_nodelay(true).ok();

    let (read_half, write_half) = socket.into_split();
    let mut reader = BufReader::with_capacity(64 * 1024, read_half);
    let mut writer = BufWriter::with_capacity(64 * 1024, write_half);

    // -- handshake ---------------------------------------------------------
    let ours = Handshake::new(cfg.info_hash, cfg.peer_id, cfg.dht);
    writer.write_all(&ours.to_bytes()).await?;
    writer.flush().await?;

    let theirs = match handshake_already_read {
        Some(hs) => hs,
        None => {
            let mut buf = [0u8; 68];
            timeout(HANDSHAKE_TIMEOUT, reader.read_exact(&mut buf))
                .await
                .context("timed out waiting for handshake")??;
            Handshake::from_bytes(&buf)?
        }
    };
    if theirs.info_hash != cfg.info_hash {
        bail!("peer replied with a different info_hash");
    }
    if theirs.peer_id == cfg.peer_id {
        bail!("connection to ourselves");
    }

    let supports_ext = theirs.supports(reserved::EXTENSION);
    let supports_dht = theirs.supports(reserved::DHT);

    let mut extensions = ExtensionHandshake::default();
    if supports_ext {
        let our_ext = ExtensionHandshake::ours(cfg.metadata_size, cfg.allow_pex, cfg.listen_port);
        write_msg(
            &mut writer,
            &Message::Extended {
                id: 0,
                payload: our_ext.encode(),
            },
        )
        .await?;
    }
    if let Some(bits) = &cfg.initial_bitfield
        && bits.iter().any(|b| *b != 0)
    {
        write_msg(&mut writer, &Message::Bitfield(bits.clone())).await?;
    }
    if cfg.dht {
        write_msg(&mut writer, &Message::Port(cfg.listen_port)).await?;
    }
    writer.flush().await?;

    events
        .send(PeerEvent::Connected {
            slot,
            peer_id: theirs.peer_id,
            supports_dht,
        })
        .await
        .ok();

    // -- main loop ---------------------------------------------------------
    //
    // Reading runs in its own task, not inside the `select!`, because
    // `read_message` is not cancel-safe: it consumes the 4-byte length before
    // the body. Cancelled between the two, those bytes vanish from the buffer
    // and the next message is read starting from the body — the stream loses
    // sync and the connection dies announcing a "1.6 GB message". Receiving
    // from a channel, by contrast, is cancel-safe.
    let (msgs_tx, mut msgs_rx) = mpsc::channel::<Result<Message>>(512);
    let reader_task = tokio::spawn(async move {
        loop {
            let result = match timeout(IDLE_TIMEOUT, read_message(&mut reader)).await {
                Ok(Ok(msg)) => Ok(msg),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow::anyhow!("peer went silent")),
            };
            let done = result.is_err();
            if msgs_tx.send(result).await.is_err() || done {
                return;
            }
        }
    });
    let _reader_task = AbortOnDrop(reader_task);

    let mut keepalive = tokio::time::interval_at(Instant::now() + KEEPALIVE, KEEPALIVE);
    let mut received_requests = 0usize;
    let mut sent_bytes = 0u64;
    let mut last_report = Instant::now();

    loop {
        tokio::select! {
            // Prioritize draining commands: stalled requests are idle bandwidth.
            biased;

            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                if matches!(cmd, PeerCommand::Shutdown) {
                    return Ok(());
                }
                let bytes = apply_command(&mut writer, cmd, &extensions, &mut received_requests).await?;
                sent_bytes += bytes;

                // Drain the rest of the queue before flushing: one flush per
                // batch instead of one per message.
                while let Ok(cmd) = cmds.try_recv() {
                    if matches!(cmd, PeerCommand::Shutdown) {
                        writer.flush().await?;
                        return Ok(());
                    }
                    sent_bytes += apply_command(&mut writer, cmd, &extensions, &mut received_requests).await?;
                }
                writer.flush().await?;

                if sent_bytes > 0 && last_report.elapsed() > Duration::from_millis(500) {
                    events.send(PeerEvent::Uploaded(slot, std::mem::take(&mut sent_bytes))).await.ok();
                    last_report = Instant::now();
                }
            }

            msg = msgs_rx.recv() => {
                // `None` means the reader task has ended.
                let Some(msg) = msg else { return Ok(()) };
                let msg = msg?;
                if let Some(event) = handle_message(slot, msg, &mut extensions, &mut received_requests)?
                    && events.send(event).await.is_err() {
                        return Ok(());
                    }
            }

            _ = keepalive.tick() => {
                write_msg(&mut writer, &Message::KeepAlive).await?;
                writer.flush().await?;
            }
        }
    }
}

/// Ensures the reader task dies along with the peer, instead of staying
/// stuck in `read_message` until the idle timeout.
struct AbortOnDrop(#[allow(dead_code)] tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn write_msg<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, msg: &Message) -> Result<()> {
    message::write_message(writer, msg).await
}

/// Executes an engine command; returns how many *payload* bytes were sent
/// to the peer (only counts real upload, not protocol overhead).
async fn apply_command<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    cmd: PeerCommand,
    ext: &ExtensionHandshake,
    received_requests: &mut usize,
) -> Result<u64> {
    let mut sent_bytes = 0;
    match cmd {
        PeerCommand::Request(reqs) => {
            for req in reqs {
                write_msg(writer, &Message::Request(req)).await?;
            }
        }
        PeerCommand::Cancel(req) => write_msg(writer, &Message::Cancel(req)).await?,
        PeerCommand::Have(piece) => write_msg(writer, &Message::Have(piece)).await?,
        PeerCommand::Choke(true) => write_msg(writer, &Message::Choke).await?,
        PeerCommand::Choke(false) => write_msg(writer, &Message::Unchoke).await?,
        PeerCommand::Interested(true) => write_msg(writer, &Message::Interested).await?,
        PeerCommand::Interested(false) => write_msg(writer, &Message::NotInterested).await?,
        PeerCommand::SendBlock(req, data) => {
            *received_requests = received_requests.saturating_sub(1);
            sent_bytes = data.len() as u64;
            write_msg(
                writer,
                &Message::Piece {
                    index: req.piece,
                    begin: req.offset,
                    block: data,
                },
            )
            .await?;
        }
        PeerCommand::Reject(req) => {
            *received_requests = received_requests.saturating_sub(1);
            write_msg(writer, &Message::RejectRequest(req)).await?;
        }
        PeerCommand::RequestMetadata(piece) => {
            if let Some(id) = ext.id_of("ut_metadata") {
                write_msg(
                    writer,
                    &Message::Extended {
                        id,
                        payload: MetadataMessage::Request { piece }.encode(),
                    },
                )
                .await?;
            }
        }
        PeerCommand::SendMetadata(msg) => {
            if let Some(id) = ext.id_of("ut_metadata") {
                write_msg(
                    writer,
                    &Message::Extended {
                        id,
                        payload: msg.encode(),
                    },
                )
                .await?;
            }
        }
        PeerCommand::SendPex(pex) => {
            if let Some(id) = ext.id_of("ut_pex") {
                write_msg(
                    writer,
                    &Message::Extended {
                        id,
                        payload: pex.encode(),
                    },
                )
                .await?;
            }
        }
        PeerCommand::Shutdown => {}
    }
    Ok(sent_bytes)
}

/// Translates a peer message into an event for the engine.
fn handle_message(
    slot: Slot,
    msg: Message,
    ext: &mut ExtensionHandshake,
    received_requests: &mut usize,
) -> Result<Option<PeerEvent>> {
    Ok(match msg {
        Message::KeepAlive => None,
        Message::Choke => Some(PeerEvent::Choked(slot, true)),
        Message::Unchoke => Some(PeerEvent::Choked(slot, false)),
        Message::Interested => Some(PeerEvent::Interested(slot, true)),
        Message::NotInterested => {
            // Lost interest: its pending requests die with it.
            *received_requests = 0;
            Some(PeerEvent::Interested(slot, false))
        }
        Message::Have(piece) => Some(PeerEvent::Have(slot, piece)),
        Message::Bitfield(bits) => Some(PeerEvent::Bitfield(slot, bits)),
        Message::HaveAll => Some(PeerEvent::HaveAll(slot)),
        Message::HaveNone => Some(PeerEvent::HaveNone(slot)),
        Message::Request(req) => {
            if req.length > MAX_SERVE_LENGTH {
                bail!("peer requested a block of {} bytes", req.length);
            }
            *received_requests += 1;
            if *received_requests > MAX_INCOMING_REQUESTS {
                bail!("peer queued {received_requests} requests without waiting");
            }
            Some(PeerEvent::BlockRequested(slot, req))
        }
        // A cancel only undoes the pending-request count; if the block is
        // already on its way, sending it anyway is legitimate and the peer
        // discards it.
        Message::Cancel(_) => {
            *received_requests = received_requests.saturating_sub(1);
            None
        }
        Message::Piece {
            index,
            begin,
            block,
        } => Some(PeerEvent::Block {
            slot,
            request: BlockRequest {
                piece: index,
                offset: begin,
                length: block.len() as u32,
            },
            data: block,
        }),
        Message::RejectRequest(req) => Some(PeerEvent::Rejected(slot, req)),
        Message::Port(port) => Some(PeerEvent::DhtPort(slot, port)),
        // Fast-extension suggestions are the peer's cache hints; the picker
        // already decides better than that, so we only record that it has
        // the piece.
        Message::SuggestPiece(piece) | Message::AllowedFast(piece) => {
            Some(PeerEvent::Have(slot, piece))
        }
        Message::Extended { id: 0, payload } => {
            // The extension handshake may be resent at any time to enable or
            // disable extensions; the latest one wins.
            *ext = ExtensionHandshake::decode(&payload)?;
            Some(PeerEvent::Extensions(slot, Box::new(ext.clone())))
        }
        Message::Extended { id, payload } => match id {
            extension::UT_METADATA_ID => Some(PeerEvent::Metadata(
                slot,
                MetadataMessage::decode(&payload)?,
            )),
            extension::UT_PEX_ID => Some(PeerEvent::Pex(PexMessage::decode(&payload)?)),
            // Not an extension we announced. Ignoring it is the correct behavior.
            _ => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::BLOCK_SIZE;

    fn empty_ext() -> ExtensionHandshake {
        ExtensionHandshake::default()
    }

    #[test]
    fn translates_messages_into_events() {
        let mut ext = empty_ext();
        let mut pending = 0;

        let ev = handle_message(3, Message::Unchoke, &mut ext, &mut pending).unwrap();
        assert!(matches!(ev, Some(PeerEvent::Choked(3, false))));

        let ev = handle_message(3, Message::Have(7), &mut ext, &mut pending).unwrap();
        assert!(matches!(ev, Some(PeerEvent::Have(3, 7))));

        let ev = handle_message(
            3,
            Message::Piece {
                index: 1,
                begin: BLOCK_SIZE,
                block: vec![0; 32],
            },
            &mut ext,
            &mut pending,
        )
        .unwrap();
        match ev {
            Some(PeerEvent::Block { request, data, .. }) => {
                assert_eq!(request.piece, 1);
                assert_eq!(request.offset, BLOCK_SIZE);
                assert_eq!(request.length, 32, "length comes from the received block");
                assert_eq!(data.len(), 32);
            }
            other => panic!("expected Block, got {other:?}"),
        }

        assert!(
            handle_message(3, Message::KeepAlive, &mut ext, &mut pending)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cuts_off_peer_that_abuses_requests() {
        let mut ext = empty_ext();
        let mut pending = 0;
        let req = BlockRequest {
            piece: 0,
            offset: 0,
            length: BLOCK_SIZE,
        };

        for _ in 0..MAX_INCOMING_REQUESTS {
            handle_message(0, Message::Request(req), &mut ext, &mut pending).unwrap();
        }
        assert!(
            handle_message(0, Message::Request(req), &mut ext, &mut pending).is_err(),
            "a full queue drops the peer"
        );

        let giant = BlockRequest {
            piece: 0,
            offset: 0,
            length: MAX_SERVE_LENGTH + 1,
        };
        let mut pending = 0;
        assert!(handle_message(0, Message::Request(giant), &mut ext, &mut pending).is_err());
    }

    #[test]
    fn cancel_and_not_interested_drain_the_queue() {
        let mut ext = empty_ext();
        let mut pending = 0;
        let req = BlockRequest {
            piece: 0,
            offset: 0,
            length: 16,
        };

        handle_message(0, Message::Request(req), &mut ext, &mut pending).unwrap();
        handle_message(0, Message::Request(req), &mut ext, &mut pending).unwrap();
        assert_eq!(pending, 2);

        handle_message(0, Message::Cancel(req), &mut ext, &mut pending).unwrap();
        assert_eq!(pending, 1);
        handle_message(0, Message::NotInterested, &mut ext, &mut pending).unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn extension_handshake_updates_the_ids() {
        let mut ext = empty_ext();
        let mut pending = 0;
        assert!(!ext.supports_metadata());

        let payload = ExtensionHandshake::ours(Some(1000), true, 6881).encode();
        let ev = handle_message(
            0,
            Message::Extended { id: 0, payload },
            &mut ext,
            &mut pending,
        )
        .unwrap();
        assert!(matches!(ev, Some(PeerEvent::Extensions(0, _))));
        assert!(ext.supports_metadata() && ext.supports_pex());
        assert_eq!(ext.metadata_size, Some(1000));

        // Now that we know the ids, a metadata message is recognized.
        let data_bytes = MetadataMessage::Request { piece: 0 }.encode();
        let ev = handle_message(
            0,
            Message::Extended {
                id: extension::UT_METADATA_ID,
                payload: data_bytes,
            },
            &mut ext,
            &mut pending,
        )
        .unwrap();
        assert!(matches!(ev, Some(PeerEvent::Metadata(0, _))));

        // An unknown extension does not drop the connection.
        let ev = handle_message(
            0,
            Message::Extended {
                id: 77,
                payload: vec![],
            },
            &mut ext,
            &mut pending,
        )
        .unwrap();
        assert!(ev.is_none());
    }
}
