//! The engine of a single torrent: it brings together trackers, DHT, peers,
//! the picker and disk.
//!
//! Everything that decides policy lives here and runs in a single task. Peers
//! are separate tasks that only speak the protocol; disk runs in
//! `spawn_blocking`. That way the decision loop never blocks, and there is no
//! shared state between peers to go wrong.

mod candidates;
mod helpers;
mod types;

use candidates::Candidates;
use helpers::{file_downloaded_bytes, ideal_depth, pick_optimistic};
pub use types::{Command, Config, FileInfo, PeerInfo, Snapshot, Source, State, TorrentHandle};

use crate::bitfield::Bitfield;
use crate::dht::Dht;
use crate::id::{Id20, generate_peer_id};
use crate::metainfo::Info;
use crate::peer::extension::{
    ExtensionHandshake, METADATA_PIECE, MetadataBuffer, MetadataMessage, PexMessage,
};
use crate::peer::{self, PeerCommand, PeerEvent, PeerHandle, Slot};
use crate::picker::{BLOCK_SIZE, BlockRequest, PickMode, Picker};
use crate::stats::{History, RateMeter};
use crate::storage::Storage;
use crate::tracker::{Event, TrackerConfig, Trackers};
use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Pace of the decision loop.
const TICK: Duration = Duration::from_millis(250);
/// Frequency of the choking algorithm (BEP-3 recommends 10 s).
const CHOKE_INTERVAL: Duration = Duration::from_secs(10);
/// How many choke cycles between optimistic unchoke rotations.
const OPTIMISTIC_CYCLES: u32 = 3;
/// A peer that accepted requests and delivers nothing for this long is "snubbed".
const SNUB_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for a requested block to return to the pool.
const BLOCK_TIMEOUT: Duration = Duration::from_secs(45);
/// Interval between DHT lookups when the swarm is already healthy.
const DHT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Interval while peers are still missing or the DHT is still forming.
const DHT_RETRY: Duration = Duration::from_secs(60);
/// Requests in flight per peer, minimum and maximum.
const QUEUE_MIN: usize = 4;
const QUEUE_MAX: usize = 500;
/// Simultaneous connections in progress (handshake not yet complete).
///
/// Most addresses the DHT returns are dead or behind NAT, so the cost of
/// dialing is waiting time, not bandwidth. Dialing wide with a short deadline
/// finds live peers much faster than a narrow queue with a generous deadline.
const MAX_CONNECTING: usize = 60;
/// Deadline for TCP to connect before giving up on a candidate.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// Interval between PEX peer exchanges. BEP-11 requires at least 60 s.
const PEX_INTERVAL: Duration = Duration::from_secs(90);
/// Cap on addresses per PEX message, so the packet does not blow up.
const PEX_MAX: usize = 50;
/// Maximum size of the queue of addresses to try.
const MAX_CANDIDATES: usize = 2000;
/// How long an address stays out of the queue while queued or connected.
const RESERVATION: Duration = Duration::from_secs(60 * 60);
/// Wait before retrying a peer that actually talked to us.
const RETRY_GOOD: Duration = Duration::from_secs(60);
/// Wait before retrying an address that never even completed the handshake.
const RETRY_BAD: Duration = Duration::from_secs(10 * 60);

// -- internal state --------------------------------------------------------

struct PeerState {
    handle: PeerHandle,
    addr: SocketAddr,
    client: String,
    bitfield: Bitfield,
    /// Peer that sent `have all` before we knew how many pieces exist.
    have_all: bool,
    /// Pieces announced before we had the metadata.
    pending_haves: Vec<u32>,
    counted: bool,
    /// Is the handshake finished? While `false`, this peer occupies a slot
    /// in `connecting`.
    handshake_ok: bool,
    /// Did we dial this peer? Only those addresses are usable for PEX: on an
    /// incoming connection, the source port is not the port the peer listens on.
    dialed: bool,
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    in_flight: Vec<BlockRequest>,
    queue_depth: usize,
    down: RateMeter,
    up: RateMeter,
    extensions: ExtensionHandshake,
    metadata_requested_at: Option<Instant>,
    /// The peer announced DHT support in the handshake.
    speaks_dht: bool,
    last_block_at: Instant,
    connected_at: Instant,
    snubbed: bool,
}

impl PeerState {
    fn progress(&self) -> f64 {
        if self.have_all {
            return 1.0;
        }
        if self.bitfield.is_empty() {
            return 0.0;
        }
        self.bitfield.count() as f64 / self.bitfield.len() as f64
    }

    fn is_seed(&self) -> bool {
        self.have_all || (!self.bitfield.is_empty() && self.bitfield.is_complete())
    }
}

/// State that only exists once the `info` is known.
struct Download {
    info: Arc<Info>,
    storage: Arc<Storage>,
    picker: Picker,
}

enum DiskEvent {
    Verified {
        piece: u32,
        ok: bool,
    },
    WriteError(String),
    CheckComplete(Box<Bitfield>),
    BlockRead {
        slot: Slot,
        request: BlockRequest,
        data: Vec<u8>,
    },
}

struct Engine {
    cfg: Config,
    info_hash: Id20,
    peer_id: Id20,
    name: String,
    state: State,
    error: Option<String>,

    download: Option<Download>,
    metadata: Option<MetadataBuffer>,
    metadata_size: Option<usize>,
    private: bool,

    peers: HashMap<Slot, PeerState>,
    next_slot: Slot,
    candidates: Candidates,
    connecting: usize,

    trackers: Arc<Trackers>,
    dht: Option<Arc<Dht>>,
    next_dht: Instant,
    /// Prevents firing several DHT lookups at the same time.
    dht_busy: Arc<std::sync::atomic::AtomicBool>,
    next_pex: Instant,
    /// Addresses already announced via PEX, so we only send the difference.
    pex_sent: HashSet<SocketAddr>,

    down: RateMeter,
    up: RateMeter,
    hist_down: History,
    hist_up: History,
    last_hist: Instant,

    choke_cycle: u32,
    next_choke: Instant,
    optimistic: Option<Slot>,
    check_progress: Arc<AtomicUsize>,

    events_tx: mpsc::Sender<PeerEvent>,
    disk_tx: mpsc::Sender<DiskEvent>,
    /// Same end the trackers use; the DHT also dumps peers here.
    peers_tx: mpsc::Sender<Vec<SocketAddr>>,
    snapshot: Arc<Mutex<Snapshot>>,
}

/// Starts the engine for a torrent and returns the control handle.
pub fn spawn(source: Source, cfg: Config, dht: Option<Arc<Dht>>) -> TorrentHandle {
    let info_hash = source.info_hash();
    let (cmds_tx, cmds_rx) = mpsc::channel(64);
    let snapshot = Arc::new(Mutex::new(Snapshot {
        name: source.name(),
        info_hash: info_hash.to_hex(),
        state_label: "starting".to_string(),
        ..Default::default()
    }));

    let handle = TorrentHandle {
        info_hash,
        cmds: cmds_tx,
        snapshot: Arc::clone(&snapshot),
    };

    tokio::spawn(async move {
        if let Err(e) = run(source, cfg, dht, cmds_rx, Arc::clone(&snapshot)).await {
            let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
            snap.state_label = State::Error.label().to_string();
            snap.error = Some(format!("{e:#}"));
        }
    });
    handle
}

async fn run(
    source: Source,
    cfg: Config,
    dht: Option<Arc<Dht>>,
    mut cmds: mpsc::Receiver<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
) -> Result<()> {
    let info_hash = source.info_hash();
    let peer_id = generate_peer_id();
    let (events_tx, mut events_rx) = mpsc::channel(4096);
    let (disk_tx, mut disk_rx) = mpsc::channel(256);
    let (peers_tx, mut peers_rx) = mpsc::channel(64);

    let (tiers, metainfo, magnet_peers, private, dht_nodes) = match source {
        Source::Torrent(m) => {
            let private = m.info.private;
            (
                m.tiers.clone(),
                Some(m.info.clone()),
                Vec::new(),
                private,
                m.dht_nodes.clone(),
            )
        }
        Source::Magnet(m) => (
            m.trackers.iter().map(|t| vec![t.clone()]).collect(),
            None,
            m.peers.clone(),
            false,
            Vec::new(),
        ),
    };

    // Torrents may carry bootstrap DHT nodes (BEP-5); using them shortens
    // the bootstrap, especially in small swarms.
    if let Some(dht) = dht.as_ref().filter(|_| !private && !dht_nodes.is_empty()) {
        let dht = Arc::clone(dht);
        tokio::spawn(async move {
            for host in dht_nodes {
                if let Ok(mut addrs) = tokio::net::lookup_host(&host).await
                    && let Some(addr) = addrs.next()
                {
                    dht.add_node(crate::dht::krpc::NodeInfo {
                        id: Id20::random(),
                        addr,
                    });
                }
            }
        });
    }

    let trackers = Trackers::new(
        TrackerConfig {
            info_hash,
            peer_id,
            port: cfg.listen_port,
            num_want: 200,
            client: reqwest::Client::builder()
                .user_agent(concat!("downpour/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        },
        tiers,
        peers_tx.clone(),
    );

    let mut engine = Engine {
        name: metainfo
            .as_ref()
            .map(|i| i.name.clone())
            .unwrap_or_else(|| {
                snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .name
                    .clone()
            }),
        info_hash,
        peer_id,
        state: if metainfo.is_some() {
            State::Checking
        } else {
            State::FetchingMetadata
        },
        error: None,
        download: None,
        metadata: None,
        metadata_size: None,
        private,
        peers: HashMap::new(),
        next_slot: 0,
        candidates: Candidates::default(),
        connecting: 0,
        trackers: Arc::clone(&trackers),
        dht: dht.filter(|_| cfg.use_dht && !private),
        next_dht: Instant::now(),
        dht_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        next_pex: Instant::now() + PEX_INTERVAL,
        pex_sent: HashSet::new(),
        down: RateMeter::new(),
        up: RateMeter::new(),
        hist_down: History::new(120),
        hist_up: History::new(120),
        last_hist: Instant::now(),
        choke_cycle: 0,
        next_choke: Instant::now() + CHOKE_INTERVAL,
        optimistic: None,
        check_progress: Arc::new(AtomicUsize::new(0)),
        events_tx,
        disk_tx,
        peers_tx,
        snapshot: Arc::clone(&snapshot),
        cfg,
    };

    engine.add_candidates(magnet_peers);

    // With the `.torrent` in hand we can already prepare the disk and check
    // what exists; with a magnet this waits for the metadata to arrive.
    if let Some(info) = metainfo {
        engine.prepare_download(info).await?;
    }
    engine.trackers.announce(Event::Started);

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some(ev) = events_rx.recv() => engine.on_peer_event(ev).await,
            Some(ev) = disk_rx.recv() => engine.on_disk_event(ev).await,
            Some(addrs) = peers_rx.recv() => engine.add_candidates(addrs),
            cmd = cmds.recv() => match cmd {
                Some(Command::Shutdown) | None => break,
                Some(cmd) => engine.on_command(cmd).await,
            },
            _ = tick.tick() => engine.tick().await,
        }
    }

    engine.finish().await;
    Ok(())
}

impl Engine {
    // -- lifecycle ---------------------------------------------------------

    /// Opens the files and kicks off checking what is already on disk.
    async fn prepare_download(&mut self, info: Info) -> Result<()> {
        let info = Arc::new(info);
        self.name = info.name.clone();
        self.private = self.private || info.private;
        if self.private {
            self.dht = None;
        }

        let storage = Arc::new(
            Storage::open(Arc::clone(&info), &self.cfg.dest)
                .context("failed to prepare the destination files")?,
        );

        self.state = State::Checking;
        let progress = Arc::clone(&self.check_progress);
        progress.store(0, Ordering::Relaxed);

        let st = Arc::clone(&storage);
        let disk = self.disk_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = st.verify_all(&progress);
            let bitfield = result.unwrap_or_else(|_| Bitfield::new(0));
            let _ = disk.blocking_send(DiskEvent::CheckComplete(Box::new(bitfield)));
        });

        let picker = Picker::new(&info, Bitfield::new(info.num_pieces()));
        self.download = Some(Download {
            info,
            storage,
            picker,
        });
        Ok(())
    }

    async fn finish(&mut self) {
        for peer in self.peers.values() {
            peer.handle.send(PeerCommand::Shutdown);
        }
        self.update_counters();
        self.trackers.announce_stopped(Duration::from_secs(3)).await;
        self.trackers.shutdown();
    }

    // -- peer events -------------------------------------------------------

    async fn on_peer_event(&mut self, ev: PeerEvent) {
        match ev {
            PeerEvent::Connected {
                slot,
                peer_id,
                supports_dht,
                ..
            } => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    crate::log::log!("peer", "{} handshake ok", peer.addr);
                    peer.connected_at = Instant::now();
                    // The client name comes from the peer id; if the extension
                    // handshake carries a `v`, it replaces it later.
                    peer.client = crate::client_id::name(&peer_id);
                    peer.speaks_dht = supports_dht;
                    if !std::mem::replace(&mut peer.handshake_ok, true) {
                        self.connecting = self.connecting.saturating_sub(1);
                    }
                }
            }

            PeerEvent::Extensions(slot, ext) => {
                let size = ext.metadata_size;
                if let Some(peer) = self.peers.get_mut(&slot) {
                    if let Some(v) = &ext.client {
                        peer.client = v.clone();
                    }
                    // The peer says how many requests it can handle; respecting
                    // that avoids being disconnected for excess.
                    if let Some(reqq) = ext.reqq {
                        peer.queue_depth = peer.queue_depth.min(reqq.max(QUEUE_MIN));
                    }
                    peer.extensions = *ext;
                }
                if self.download.is_none()
                    && let Some(size) = size
                {
                    self.start_metadata(size);
                    self.request_metadata(slot);
                }
            }

            PeerEvent::Bitfield(slot, bits) => {
                let Some(n) = self.num_pieces() else {
                    // No metadata yet: store it and reinterpret later.
                    if let Some(peer) = self.peers.get_mut(&slot) {
                        peer.have_all = !bits.is_empty() && bits.iter().all(|b| *b == 0xff);
                    }
                    return;
                };
                match Bitfield::from_bytes(&bits, n) {
                    Some(bf) => {
                        self.remove_availability(slot);
                        if let Some(peer) = self.peers.get_mut(&slot) {
                            peer.bitfield = bf;
                        }
                        self.apply_availability(slot);
                    }
                    // A bitfield with the wrong size means a confused peer: drop it.
                    None => self.disconnect(slot, "invalid bitfield").await,
                }
            }

            PeerEvent::HaveAll(slot) => {
                self.remove_availability(slot);
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.have_all = true;
                    peer.bitfield.set_all();
                }
                self.apply_availability(slot);
            }

            PeerEvent::HaveNone(slot) => {
                self.remove_availability(slot);
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.have_all = false;
                    peer.bitfield = Bitfield::new(peer.bitfield.len());
                }
            }

            PeerEvent::Have(slot, piece) => {
                let Some(peer) = self.peers.get_mut(&slot) else {
                    return;
                };
                if peer.bitfield.is_empty() {
                    peer.pending_haves.push(piece);
                    return;
                }
                if peer.bitfield.set(piece as usize)
                    && peer.counted
                    && let Some(d) = &mut self.download
                {
                    d.picker.peer_has(piece);
                }
            }

            PeerEvent::Choked(slot, choked) => {
                let mut released = Vec::new();
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.peer_choking = choked;
                    if choked {
                        // Choked, the requests in flight will never arrive.
                        released = std::mem::take(&mut peer.in_flight);
                    }
                }
                if !released.is_empty()
                    && let Some(d) = &mut self.download
                {
                    d.picker.release_peer(slot);
                }
            }

            PeerEvent::Interested(slot, interested) => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.peer_interested = interested;
                }
            }

            PeerEvent::Block {
                slot,
                request,
                data,
            } => self.on_block(slot, request, data).await,

            PeerEvent::Rejected(slot, req) => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.in_flight.retain(|r| *r != req);
                }
                if let Some(d) = &mut self.download {
                    // Returns only this block to the pool, without punishing the peer.
                    d.picker.release_peer(slot);
                }
            }

            PeerEvent::BlockRequested(slot, req) => self.serve_block(slot, req),

            PeerEvent::Metadata(slot, msg) => self.on_metadata(slot, msg).await,

            PeerEvent::Pex(pex) => {
                if !self.private {
                    self.add_candidates(pex.added);
                }
            }

            PeerEvent::DhtPort(slot, port) => {
                // A node with a random id enters the table as a candidate: the
                // first `ping` fixes the real id.
                if let (Some(dht), Some(peer)) = (&self.dht, self.peers.get(&slot))
                    && peer.speaks_dht
                    && port != 0
                {
                    dht.add_node(crate::dht::krpc::NodeInfo {
                        id: Id20::random(),
                        addr: SocketAddr::new(peer.addr.ip(), port),
                    });
                }
            }

            PeerEvent::Uploaded(slot, bytes) => {
                self.up.add(bytes);
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.up.add(bytes);
                }
            }

            PeerEvent::Disconnected(slot, reason) => {
                if let Some(p) = self.peers.get(&slot) {
                    crate::log::log!(
                        "peer",
                        "{} dropped after {:?} (handshake={}): {reason}",
                        p.addr,
                        p.connected_at.elapsed(),
                        p.handshake_ok
                    );
                }
                self.remove_peer(slot);
            }
        }
    }

    async fn on_block(&mut self, slot: Slot, request: BlockRequest, data: Vec<u8>) {
        let bytes = data.len() as u64;
        self.down.add(bytes);
        if let Some(peer) = self.peers.get_mut(&slot) {
            peer.down.add(bytes);
            peer.last_block_at = Instant::now();
            peer.snubbed = false;
            peer.in_flight
                .retain(|r| !(r.piece == request.piece && r.offset == request.offset));
        }

        let Some(d) = &mut self.download else {
            return;
        };
        let result = d.picker.received(slot, request, &data);
        if !result.wanted {
            return;
        }

        // In endgame, whoever arrived first cancels the twin requests.
        for other in result.cancel_from {
            if let Some(peer) = self.peers.get_mut(&other) {
                peer.in_flight
                    .retain(|r| !(r.piece == request.piece && r.offset == request.offset));
                peer.handle.send(PeerCommand::Cancel(request));
            }
        }

        if let Some(piece_data) = result.piece_data {
            let piece = request.piece;
            let expected = d.info.piece_hashes[piece as usize];
            let storage = Arc::clone(&d.storage);
            let disk = self.disk_tx.clone();
            tokio::task::spawn_blocking(move || {
                // Verify before writing: a bad piece never reaches disk.
                let ok = Id20(Sha1::digest(&piece_data).into()) == expected;
                if ok && let Err(e) = storage.write_piece(piece as usize, &piece_data) {
                    let _ = disk.blocking_send(DiskEvent::WriteError(format!("{e:#}")));
                    return;
                }
                let _ = disk.blocking_send(DiskEvent::Verified { piece, ok });
            });
        }
    }

    async fn on_disk_event(&mut self, ev: DiskEvent) {
        match ev {
            DiskEvent::CheckComplete(bitfield) => {
                if let Some(d) = &mut self.download {
                    let mut picker = Picker::new(&d.info, *bitfield);
                    picker.set_mode(if self.cfg.sequential {
                        PickMode::Sequential
                    } else {
                        PickMode::Rarest
                    });
                    // Availability goes away together with the old picker.
                    for peer in self.peers.values_mut() {
                        peer.counted = false;
                    }
                    d.picker = picker;
                }
                self.state = if self.is_complete() {
                    State::Seeding
                } else {
                    State::Downloading
                };
                // Peers that connected during the check received an empty
                // bitfield; tell them what we found on disk.
                let slots: Vec<Slot> = self.peers.keys().copied().collect();
                for slot in slots {
                    self.announce_pieces(slot);
                    self.apply_availability(slot);
                }
                if self.is_complete() {
                    self.trackers.announce(Event::Completed);
                }
            }

            DiskEvent::Verified { piece, ok } => {
                let Some(d) = &mut self.download else {
                    return;
                };
                if !ok {
                    d.picker.piece_failed(piece);
                    return;
                }
                d.picker.piece_verified(piece);
                let complete = d.picker.is_complete();

                for peer in self.peers.values() {
                    peer.handle.send(PeerCommand::Have(piece));
                }
                if complete {
                    self.on_complete().await;
                }
            }

            DiskEvent::WriteError(msg) => {
                self.state = State::Error;
                self.error = Some(msg);
            }

            DiskEvent::BlockRead {
                slot,
                request,
                data,
            } => {
                if let Some(peer) = self.peers.get(&slot) {
                    peer.handle.send(PeerCommand::SendBlock(request, data));
                }
            }
        }
    }

    async fn on_complete(&mut self) {
        self.state = State::Seeding;
        self.update_counters();
        self.trackers.announce(Event::Completed);

        // Peers that also have everything are no longer useful for anything.
        let useless: Vec<Slot> = self
            .peers
            .iter()
            .filter(|(_, p)| p.is_seed())
            .map(|(s, _)| *s)
            .collect();
        for slot in useless {
            self.disconnect(slot, "both complete").await;
        }
        if !self.cfg.seed_after_complete {
            let all: Vec<Slot> = self.peers.keys().copied().collect();
            for slot in all {
                self.disconnect(slot, "download complete").await;
            }
        }
    }

    // -- metadata (magnet) -------------------------------------------------

    fn start_metadata(&mut self, size: usize) {
        // 16 MiB of `info` would already be an absurd torrent; above that is abuse.
        if self.metadata.is_some() || size == 0 || size > 16 << 20 {
            return;
        }
        self.metadata_size = Some(size);
        self.metadata = Some(MetadataBuffer::new(size));
    }

    fn request_metadata(&mut self, slot: Slot) {
        let Some(buffer) = &self.metadata else {
            return;
        };
        let missing: Vec<usize> = buffer.missing().take(8).collect();
        if let Some(peer) = self.peers.get_mut(&slot) {
            if !peer.extensions.supports_metadata() {
                return;
            }
            peer.metadata_requested_at = Some(Instant::now());
            for piece in missing {
                peer.handle.send(PeerCommand::RequestMetadata(piece));
            }
        }
    }

    async fn on_metadata(&mut self, slot: Slot, msg: MetadataMessage) {
        match msg {
            MetadataMessage::Data { piece, total, data } => {
                self.start_metadata(total);
                let Some(buffer) = &mut self.metadata else {
                    return;
                };
                if !buffer.add(piece, &data) || !buffer.is_complete() {
                    return;
                }

                let raw = self
                    .metadata
                    .take()
                    .expect("metadata buffer must exist")
                    .take();
                // The received `info` has to match the magnet's info_hash —
                // without this check any peer could hand us a different torrent.
                if Id20(Sha1::digest(&raw).into()) != self.info_hash {
                    self.metadata_size = None;
                    return;
                }
                match Info::from_raw(&raw) {
                    Ok(info) => {
                        if let Err(e) = self.prepare_download(info).await {
                            self.state = State::Error;
                            self.error = Some(format!("{e:#}"));
                        }
                    }
                    Err(e) => {
                        self.state = State::Error;
                        self.error = Some(format!("invalid metadata: {e:#}"));
                    }
                }
            }

            // A peer asked for our `info` — we answer if we already have it.
            MetadataMessage::Request { piece } => {
                let (Some(d), Some(peer)) = (&self.download, self.peers.get(&slot)) else {
                    return;
                };
                let raw = &d.info.raw;
                let start = piece * METADATA_PIECE;
                if start >= raw.len() {
                    peer.handle
                        .send(PeerCommand::SendMetadata(MetadataMessage::Reject { piece }));
                    return;
                }
                let end = (start + METADATA_PIECE).min(raw.len());
                peer.handle
                    .send(PeerCommand::SendMetadata(MetadataMessage::Data {
                        piece,
                        total: raw.len(),
                        data: raw[start..end].to_vec(),
                    }));
            }

            MetadataMessage::Reject { .. } => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.metadata_requested_at = None;
                }
            }
        }
    }

    // -- upload ------------------------------------------------------------

    fn serve_block(&mut self, slot: Slot, req: BlockRequest) {
        let Some(d) = &self.download else {
            return;
        };
        let Some(peer) = self.peers.get(&slot) else {
            return;
        };
        let valid = req.length <= BLOCK_SIZE * 8
            && (req.piece as usize) < d.info.num_pieces()
            && req.offset.saturating_add(req.length) <= d.info.piece_size(req.piece as usize);

        if peer.am_choking || !valid || !d.picker.has_piece(req.piece) {
            peer.handle.send(PeerCommand::Reject(req));
            return;
        }
        // Blowing the upload limit now would delay everyone's download.
        if let Some(limit) = self.cfg.max_upload
            && self.up.rate() > limit as f64
        {
            peer.handle.send(PeerCommand::Reject(req));
            return;
        }

        let storage = Arc::clone(&d.storage);
        let disk = self.disk_tx.clone();
        tokio::task::spawn_blocking(move || {
            match storage.read_block(req.piece as usize, req.offset, req.length) {
                Ok(data) => {
                    let _ = disk.blocking_send(DiskEvent::BlockRead {
                        slot,
                        request: req,
                        data,
                    });
                }
                Err(_) => {
                    let _ = disk.blocking_send(DiskEvent::BlockRead {
                        slot,
                        request: req,
                        data: Vec::new(),
                    });
                }
            }
        });
    }

    // -- periodic loop -----------------------------------------------------

    async fn tick(&mut self) {
        if self.state != State::Paused {
            self.connect_candidates().await;
            self.release_stuck();
            self.update_interest();
            self.request_blocks();
            self.poll_metadata();

            if Instant::now() >= self.next_pex {
                self.next_pex = Instant::now() + PEX_INTERVAL;
                self.send_pex();
            }
            if Instant::now() >= self.next_choke {
                self.run_choking();
                self.next_choke = Instant::now() + CHOKE_INTERVAL;
            }
            if let Some(dht) = self.dht.clone()
                && Instant::now() >= self.next_dht
                && !self.dht_busy.load(Ordering::Relaxed)
            {
                // While the DHT is still forming, or while peers are missing,
                // keep insisting every minute. Waiting the full interval would
                // waste the first 15 minutes of download, which is exactly
                // when the tracker has not yet given much.
                if !dht.is_ready() {
                    // Searching with an empty table finds nothing and only burns
                    // the interval; wait for the bootstrap to show signs of life.
                    self.next_dht = Instant::now() + Duration::from_secs(3);
                } else {
                    let starved = self.peers.len() < self.cfg.max_peers / 4;
                    self.next_dht = Instant::now() + if starved { DHT_RETRY } else { DHT_INTERVAL };
                    self.search_dht(dht);
                }
            }
        }

        self.update_counters();
        if self.last_hist.elapsed() >= Duration::from_secs(1) {
            if crate::log::active("state") {
                let unchoked_us = self.peers.values().filter(|p| !p.peer_choking).count();
                let interested_in_us = self.peers.values().filter(|p| p.am_interested).count();
                let in_flight: usize = self.peers.values().map(|p| p.in_flight.len()).sum();
                let queue_total: usize = self.peers.values().map(|p| p.queue_depth).sum();
                let with_bf = self
                    .peers
                    .values()
                    .filter(|p| p.bitfield.count() > 0)
                    .count();
                crate::log::log!(
                    "state",
                    "{} peers · {} unchoked us · {} interested in us · {} with bitfield · {} blocks in flight · combined queue {}",
                    self.peers.len(),
                    unchoked_us,
                    interested_in_us,
                    with_bf,
                    in_flight,
                    queue_total
                );
            }
            self.last_hist = Instant::now();
            let (d, u) = (self.down.rate(), self.up.rate());
            self.hist_down.push(d);
            self.hist_up.push(u);
        }
        self.publish_snapshot();
    }

    /// Requested blocks that never arrived return to the pool, and the guilty
    /// peer is marked as "snubbed" so it loses priority in choking.
    fn release_stuck(&mut self) {
        let Some(d) = &mut self.download else {
            return;
        };
        d.picker.sweep_timeouts(BLOCK_TIMEOUT);

        for (slot, peer) in self.peers.iter_mut() {
            if !peer.in_flight.is_empty() && peer.last_block_at.elapsed() > SNUB_TIMEOUT {
                peer.snubbed = true;
                peer.in_flight.clear();
                peer.queue_depth = QUEUE_MIN;
                d.picker.release_peer(*slot);
            }
        }
    }

    /// Tells each peer whether it has something we want.
    fn update_interest(&mut self) {
        let Some(d) = &self.download else {
            return;
        };
        let ours = d.picker.have();
        for peer in self.peers.values_mut() {
            let wants = peer.have_all || ours.has_anything_we_lack(&peer.bitfield);
            if wants != peer.am_interested {
                peer.am_interested = wants;
                peer.handle.send(PeerCommand::Interested(wants));
            }
        }
    }

    /// Refills the request queue of each unchoked peer.
    fn request_blocks(&mut self) {
        let Some(d) = &mut self.download else {
            return;
        };
        if d.picker.is_complete() {
            return;
        }
        // Blowing the download limit: stopping asking is the most direct brake.
        if let Some(limit) = self.cfg.max_download
            && self.down.rate() > limit as f64
        {
            return;
        }

        let mut slots: Vec<Slot> = self
            .peers
            .iter()
            .filter(|(_, p)| !p.peer_choking && p.am_interested)
            .map(|(s, _)| *s)
            .collect();
        // The fastest pick first: rare pieces go to whoever delivers them sooner.
        slots.sort_unstable_by(|a, b| {
            let rate = |s: &Slot| self.peers.get(s).map(|p| p.down.total()).unwrap_or(0);
            rate(b).cmp(&rate(a))
        });

        let mut requests = Vec::new();
        for slot in slots {
            let Some(peer) = self.peers.get_mut(&slot) else {
                continue;
            };
            peer.queue_depth = ideal_depth(peer.down.rate(), peer.queue_depth);
            let missing = peer.queue_depth.saturating_sub(peer.in_flight.len());
            if missing == 0 {
                continue;
            }

            requests.clear();
            d.picker.pick(slot, &peer.bitfield, missing, &mut requests);
            if requests.is_empty() {
                continue;
            }
            peer.in_flight.extend(requests.iter().copied());
            peer.handle.send(PeerCommand::Request(requests.clone()));
        }
    }

    /// Tells peers who joined and who left since the last exchange.
    ///
    /// Exchanging peers directly between clients is what keeps a swarm alive
    /// when the tracker is stingy — and most of them are.
    fn send_pex(&mut self) {
        if self.private {
            return;
        }
        let current: HashSet<SocketAddr> = self
            .peers
            .values()
            .filter(|p| p.handshake_ok && p.dialed)
            .map(|p| p.addr)
            .collect();

        let msg = PexMessage {
            added: current
                .difference(&self.pex_sent)
                .copied()
                .take(PEX_MAX)
                .collect(),
            dropped: self
                .pex_sent
                .difference(&current)
                .copied()
                .take(PEX_MAX)
                .collect(),
        };
        if msg.added.is_empty() && msg.dropped.is_empty() {
            return;
        }
        for peer in self.peers.values().filter(|p| p.extensions.supports_pex()) {
            peer.handle.send(PeerCommand::SendPex(msg.clone()));
        }
        self.pex_sent = current;
    }

    fn poll_metadata(&mut self) {
        if self.download.is_some() || self.metadata.is_none() {
            return;
        }
        let targets: Vec<Slot> = self
            .peers
            .iter()
            .filter(|(_, p)| {
                p.extensions.supports_metadata()
                    && p.metadata_requested_at
                        .is_none_or(|t| t.elapsed() > Duration::from_secs(10))
            })
            .map(|(s, _)| *s)
            .take(4)
            .collect();
        for slot in targets {
            self.request_metadata(slot);
        }
    }

    /// Tit-for-tat: we unchoke whoever delivers us the most, plus one randomly
    /// picked peer to give a chance to whoever has not proven anything yet.
    fn run_choking(&mut self) {
        self.choke_cycle += 1;
        let seeding = self.is_complete();

        let mut ranking: Vec<(Slot, f64)> = self
            .peers
            .iter_mut()
            .filter(|(_, p)| p.peer_interested)
            .map(|(slot, p)| {
                // Downloading, what counts is how much the peer gives us. Seeding,
                // there is no reciprocity to measure: we reward whoever consumes fastest.
                let score = if seeding { p.up.rate() } else { p.down.rate() };
                let score = if p.snubbed { 0.0 } else { score };
                (*slot, score)
            })
            .collect();
        ranking.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));

        let mut unchoked: HashSet<Slot> = ranking
            .iter()
            .take(self.cfg.upload_slots.saturating_sub(1))
            .map(|(s, _)| *s)
            .collect();

        // Optimistic unchoke, rotated every ~30 s.
        if self.choke_cycle % OPTIMISTIC_CYCLES == 1
            || self.optimistic.is_none_or(|s| !self.peers.contains_key(&s))
        {
            let candidates: Vec<Slot> = self
                .peers
                .iter()
                .filter(|(s, p)| p.peer_interested && !unchoked.contains(s))
                .map(|(s, _)| *s)
                .collect();
            self.optimistic = pick_optimistic(&candidates, self.choke_cycle);
        }
        if let Some(slot) = self.optimistic {
            unchoked.insert(slot);
        }

        for (slot, peer) in self.peers.iter_mut() {
            let should_choke = !unchoked.contains(slot);
            if should_choke != peer.am_choking {
                peer.am_choking = should_choke;
                peer.handle.send(PeerCommand::Choke(should_choke));
            }
        }
    }

    /// Looks up peers in the DHT. Results enter through the same channel as
    /// the trackers, so they reach the engine without special handling.
    fn search_dht(&self, dht: Arc<Dht>) {
        let info_hash = self.info_hash;
        let port = self.cfg.listen_port;
        let complete = self.is_complete();
        let peers_tx = self.peers_tx.clone();
        let busy = Arc::clone(&self.dht_busy);

        busy.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            dht.get_peers(info_hash, Some(peers_tx)).await;
            // We only announce when we have something to offer; a leecher that
            // announces itself attracts connections it cannot serve.
            if complete {
                dht.announce_peer(info_hash, port).await;
            }
            busy.store(false, Ordering::Relaxed);
        });
    }

    // -- connections ---------------------------------------------------------

    fn add_candidates(&mut self, addrs: Vec<SocketAddr>) {
        crate::log::log!("peers", "discovered {} addresses", addrs.len());
        for addr in addrs {
            self.candidates.offer(addr);
        }
    }

    async fn connect_candidates(&mut self) {
        let alive = self.peers.len().saturating_sub(self.connecting);
        while alive + self.connecting < self.cfg.max_peers && self.connecting < MAX_CONNECTING {
            let Some(addr) = self.candidates.next() else {
                break;
            };
            if self.peers.values().any(|p| p.addr == addr) {
                continue;
            }

            let slot = self.new_slot();
            let handle = peer::spawn_outgoing(
                slot,
                addr,
                self.peer_config(),
                self.events_tx.clone(),
                CONNECT_TIMEOUT,
            )
            .await;
            self.register_peer(slot, addr, handle, true);
            self.connecting += 1;
        }
    }

    fn peer_config(&self) -> peer::PeerConfig {
        peer::PeerConfig {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            listen_port: self.cfg.listen_port,
            metadata_size: self
                .download
                .as_ref()
                .map(|d| d.info.raw.len())
                .or(self.metadata_size),
            allow_pex: !self.private,
            dht: self.dht.is_some(),
            initial_bitfield: self
                .download
                .as_ref()
                .filter(|d| !d.picker.have().is_empty())
                .map(|d| d.picker.have().as_bytes().to_vec()),
        }
    }

    fn new_slot(&mut self) -> Slot {
        self.next_slot += 1;
        self.next_slot
    }

    fn register_peer(&mut self, slot: Slot, addr: SocketAddr, handle: PeerHandle, dialed: bool) {
        let n = self.num_pieces().unwrap_or(0);
        self.peers.insert(
            slot,
            PeerState {
                handle,
                addr,
                client: String::new(),
                bitfield: Bitfield::new(n),
                have_all: false,
                pending_haves: Vec::new(),
                counted: false,
                handshake_ok: false,
                dialed,
                am_choking: true,
                am_interested: false,
                peer_choking: true,
                peer_interested: false,
                in_flight: Vec::new(),
                queue_depth: QUEUE_MIN * 2,
                down: RateMeter::new(),
                up: RateMeter::new(),
                extensions: ExtensionHandshake::default(),
                metadata_requested_at: None,
                speaks_dht: false,
                last_block_at: Instant::now(),
                connected_at: Instant::now(),
                snubbed: false,
            },
        );
    }

    fn remove_peer(&mut self, slot: Slot) {
        self.remove_availability(slot);
        // A peer that dies before the handshake has to give back the connection
        // slot it reserved — without this the counter saturates and the engine
        // stops dialing forever, with the candidate queue full.
        if let Some(peer) = self.peers.remove(&slot) {
            if !peer.handshake_ok {
                self.connecting = self.connecting.saturating_sub(1);
            }
            // Whoever actually talked to us deserves another chance sooner than
            // an address that never answered the handshake.
            if peer.dialed {
                let delay = if peer.handshake_ok {
                    RETRY_GOOD
                } else {
                    RETRY_BAD
                };
                self.candidates.schedule_retry(peer.addr, delay);
            }
            if let Some(d) = &mut self.download {
                d.picker.release_peer(slot);
            }
        }
        if self.optimistic == Some(slot) {
            self.optimistic = None;
        }
    }

    async fn disconnect(&mut self, slot: Slot, _reason: &str) {
        if let Some(peer) = self.peers.get(&slot) {
            peer.handle.send(PeerCommand::Shutdown);
        }
        self.remove_peer(slot);
    }

    /// Tells an already-connected peer the pieces we discovered after the
    /// handshake — the initial `bitfield` was already sent (or not) by its task.
    fn announce_pieces(&self, slot: Slot) {
        let (Some(d), Some(peer)) = (&self.download, self.peers.get(&slot)) else {
            return;
        };
        for piece in d.picker.have().iter_set() {
            peer.handle.send(PeerCommand::Have(piece as u32));
        }
    }

    fn apply_availability(&mut self, slot: Slot) {
        let Some(d) = &mut self.download else {
            return;
        };
        let Some(peer) = self.peers.get_mut(&slot) else {
            return;
        };
        if peer.bitfield.is_empty() {
            peer.bitfield = Bitfield::new(d.info.num_pieces());
            if peer.have_all {
                peer.bitfield.set_all();
            }
            for piece in std::mem::take(&mut peer.pending_haves) {
                peer.bitfield.set(piece as usize);
            }
        }
        if !peer.counted {
            d.picker.peer_added(&peer.bitfield);
            peer.counted = true;
        }
    }

    fn remove_availability(&mut self, slot: Slot) {
        let Some(d) = &mut self.download else {
            return;
        };
        let Some(peer) = self.peers.get_mut(&slot) else {
            return;
        };
        if peer.counted {
            d.picker.peer_removed(&peer.bitfield);
            peer.counted = false;
        }
    }

    // -- commands ------------------------------------------------------------

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Pause => {
                if self.state == State::Paused {
                    return;
                }
                self.state = State::Paused;
                let all: Vec<Slot> = self.peers.keys().copied().collect();
                for slot in all {
                    self.disconnect(slot, "paused").await;
                }
                // Pausing must not cost the swarm: addresses leave quarantine
                // so resuming dials again right away.
                self.candidates.release_all();
                self.trackers.announce(Event::Stopped);
            }
            Command::Resume => {
                if self.state != State::Paused {
                    return;
                }
                self.state = if self.is_complete() {
                    State::Seeding
                } else {
                    State::Downloading
                };
                self.candidates.requeue_expired();
                self.next_dht = Instant::now();
                self.trackers.announce(Event::Started);
            }
            Command::SetMode(mode) => {
                self.cfg.sequential = mode == PickMode::Sequential;
                if let Some(d) = &mut self.download {
                    d.picker.set_mode(mode);
                }
            }
            Command::SetFilePriority(index, priority) => {
                if let Some(d) = &mut self.download {
                    let info = Arc::clone(&d.info);
                    d.picker.set_file_priority(&info, index, priority);
                }
            }
            Command::Incoming(data) => {
                let (socket, addr, handshake) = *data;
                if self.peers.len() >= self.cfg.max_peers {
                    return;
                }
                let slot = self.new_slot();
                let handle = peer::spawn_incoming(
                    slot,
                    socket,
                    addr,
                    handshake,
                    self.peer_config(),
                    self.events_tx.clone(),
                )
                .await;
                self.register_peer(slot, addr, handle, false);
            }
            Command::Recheck => {
                if let Some(d) = &self.download {
                    let info = Arc::clone(&d.info);
                    let _ = self.prepare_download((*info).clone()).await;
                }
            }
            Command::Shutdown => {}
        }
    }

    // -- queries -------------------------------------------------------------

    fn num_pieces(&self) -> Option<usize> {
        self.download.as_ref().map(|d| d.info.num_pieces())
    }

    fn is_complete(&self) -> bool {
        self.download
            .as_ref()
            .is_some_and(|d| d.picker.is_complete())
    }

    fn update_counters(&self) {
        let remaining = match &self.download {
            Some(d) => d.picker.remaining(),
            None => 0,
        };
        // `downloaded` in the announce is what came from the network in this
        // session, not what is on disk — the tracker uses it to compute the ratio.
        self.trackers
            .counters()
            .set(self.up.total(), self.down.total(), remaining);
    }

    fn publish_snapshot(&mut self) {
        let down_rate = self.down.rate();
        let up_rate = self.up.rate();

        let (have, total_pieces, pieces, partials, downloaded, remaining, total, endgame) =
            match &self.download {
                Some(d) => (
                    d.picker.have().count(),
                    d.info.num_pieces(),
                    d.picker.have().as_bytes().to_vec(),
                    d.picker.partial_pieces(),
                    d.picker.downloaded(),
                    d.picker.remaining(),
                    d.info.total_length,
                    d.picker.in_endgame(),
                ),
                None => (0, 0, Vec::new(), Vec::new(), 0, 0, 0, false),
            };

        let files = match &self.download {
            Some(d) => d
                .info
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| FileInfo {
                    path: f.path.clone(),
                    length: f.length,
                    downloaded: file_downloaded_bytes(d, f.offset, f.length),
                    priority: d.picker.file_priority(i),
                })
                .collect(),
            None => Vec::new(),
        };

        // Only peers with a completed handshake count: the rest are still
        // dialing, and showing them would make the list look full while the
        // download goes nowhere.
        let mut peers: Vec<PeerInfo> = self
            .peers
            .values_mut()
            .filter(|p| p.handshake_ok)
            .map(|p| PeerInfo {
                addr: p.addr,
                client: if p.client.is_empty() {
                    "—".to_string()
                } else {
                    p.client.clone()
                },
                progress: p.progress(),
                download_rate: p.down.rate(),
                upload_rate: p.up.rate(),
                choking_us: p.peer_choking,
                interested_in_us: p.peer_interested,
                we_choke: p.am_choking,
                in_flight: p.in_flight.len(),
                snubbed: p.snubbed,
            })
            .collect();
        peers.sort_unstable_by(|a, b| b.download_rate.total_cmp(&a.download_rate));

        let dht_stats = self.dht.as_ref().map(|d| d.stats()).unwrap_or_default();
        let tracker_statuses = self.trackers.status();
        let (seeds, leechers) = tracker_statuses
            .iter()
            .fold((0, 0), |(s, l), t| (s.max(t.seeders), l.max(t.leechers)));

        let eta = if down_rate > 1.0 && remaining > 0 {
            Some((remaining as f64 / down_rate) as u64)
        } else if remaining == 0 {
            Some(0)
        } else {
            None
        };

        let check = if self.state == State::Checking && total_pieces > 0 {
            self.check_progress.load(Ordering::Relaxed) as f64 / total_pieces as f64
        } else {
            0.0
        };

        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Snapshot {
            name: self.name.clone(),
            info_hash: self.info_hash.to_hex(),
            state_label: self.state.label().to_string(),
            error: self.error.clone(),
            have_pieces: have,
            total_pieces,
            pieces,
            partials,
            downloaded,
            uploaded: self.up.total(),
            wanted: downloaded + remaining,
            total_size: total,
            download_rate: down_rate,
            upload_rate: up_rate,
            eta,
            connected: peers.len(),
            candidates: self.candidates.len(),
            peers,
            seeds_in_swarm: seeds,
            peers_in_swarm: leechers,
            files,
            trackers: tracker_statuses,
            dht_nodes: dht_stats.nodes,
            dht_good: dht_stats.good_nodes,
            dht_queries: dht_stats.queries_sent,
            listen_port: self.cfg.listen_port,
            down_history: self.hist_down.iter().collect(),
            up_history: self.hist_up.iter().collect(),
            sequential: self.cfg.sequential,
            endgame,
            check_progress: check,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_follows_speed() {
        // A slow peer stays at the minimum.
        assert_eq!(ideal_depth(0.0, QUEUE_MIN), QUEUE_MIN);
        // 10 MiB/s ≈ 640 blocks/s, two seconds = 1280 → capped at the ceiling,
        // but rising two at a time.
        assert_eq!(ideal_depth(10.0 * 1024.0 * 1024.0, 10), 12);
        // A speed drop shrinks the queue at once.
        assert_eq!(ideal_depth(0.0, 200), QUEUE_MIN);
        assert!(ideal_depth(f64::MAX, QUEUE_MAX) <= QUEUE_MAX);
    }

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 6881))
    }

    #[test]
    fn queue_rejects_duplicates_and_garbage() {
        let mut c = Candidates::default();
        assert!(c.offer(addr(1)));
        assert!(!c.offer(addr(1)), "address already queued must not repeat");
        assert!(!c.offer("127.0.0.1:6881".parse().unwrap()), "loopback");
        assert!(!c.offer("1.2.3.4:0".parse().unwrap()), "zero port");
        assert_eq!(c.len(), 1);

        assert_eq!(c.next(), Some(addr(1)));
        assert_eq!(c.next(), None);
        // After leaving the queue the address stays reserved: it is
        // connected now.
        assert!(!c.offer(addr(1)));
    }

    #[test]
    fn dropped_peer_returns_after_quarantine() {
        let mut c = Candidates::default();
        c.offer(addr(7));
        c.next();

        // Still quarantined.
        c.schedule_retry(addr(7), Duration::from_secs(60));
        assert!(!c.offer(addr(7)));

        // Quarantine expired: it can try again.
        c.schedule_retry(addr(7), Duration::ZERO);
        assert!(c.offer(addr(7)));
        assert_eq!(c.len(), 1);
    }

    /// The cycle that used to break: pausing disconnected everyone and the
    /// addresses stayed banned, so resuming had nobody to dial.
    #[test]
    fn pause_and_resume_recovers_peers() {
        let mut c = Candidates::default();
        for i in 1..=5 {
            c.offer(addr(i));
        }
        // Everyone leaves the queue to connect.
        while c.next().is_some() {}
        assert_eq!(c.len(), 0);

        // Pause: disconnects everyone and releases the addresses.
        c.release_all();
        assert_eq!(c.len(), 0, "the queue stays empty while paused");

        // Resume: all five return to the queue.
        c.requeue_expired();
        assert_eq!(c.len(), 5, "known peers return on resume");

        // And a tracker announce with the same addresses does not duplicate.
        for i in 1..=5 {
            assert!(!c.offer(addr(i)));
        }
        assert_eq!(c.len(), 5);
    }

    #[test]
    fn queue_has_a_cap() {
        let mut c = Candidates::default();
        for i in 0..(MAX_CANDIDATES + 50) {
            let ip = (i / 250) as u8;
            let port = 1024 + (i % 250) as u16;
            c.offer(SocketAddr::from(([10, 1, ip, 1], port)));
        }
        assert_eq!(
            c.len(),
            MAX_CANDIDATES,
            "the queue does not grow without limit"
        );
    }

    #[test]
    fn optimistic_rotates_among_candidates() {
        let candidates = vec![1, 2, 3];
        let picked: Vec<Slot> = (0..6)
            .map(|c| pick_optimistic(&candidates, c).unwrap())
            .collect();
        assert_eq!(picked, vec![1, 2, 3, 1, 2, 3]);
        assert_eq!(pick_optimistic(&[], 0), None);
    }
}
