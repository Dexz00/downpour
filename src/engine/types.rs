//! Public types shared between the engine, the session and the UI.

use crate::id::Id20;
use crate::magnet::Magnet;
use crate::metainfo::Metainfo;
use crate::peer::message::Handshake;
use crate::picker::{PickMode, Priority};
use crate::tracker::TrackerStatus;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct Config {
    pub dest: PathBuf,
    pub max_peers: usize,
    pub upload_slots: usize,
    pub listen_port: u16,
    pub sequential: bool,
    /// Limits in bytes/s; `None` = unlimited.
    pub max_download: Option<u64>,
    pub max_upload: Option<u64>,
    pub use_dht: bool,
    /// Stop sending data as soon as the download finishes.
    pub seed_after_complete: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            dest: PathBuf::from("."),
            max_peers: 100,
            upload_slots: 6,
            listen_port: 6881,
            sequential: false,
            max_download: None,
            max_upload: None,
            use_dht: true,
            seed_after_complete: true,
        }
    }
}

/// Where the torrent came from.
pub enum Source {
    Torrent(Box<Metainfo>),
    Magnet(Box<Magnet>),
}

impl Source {
    pub fn info_hash(&self) -> Id20 {
        match self {
            Source::Torrent(m) => m.info.info_hash,
            Source::Magnet(m) => m.info_hash,
        }
    }

    pub fn name(&self) -> String {
        match self {
            Source::Torrent(m) => m.info.name.clone(),
            Source::Magnet(m) => m
                .name
                .clone()
                .unwrap_or_else(|| format!("magnet:{}", &m.info_hash.to_hex()[..16])),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Magnet: downloading the `info` dictionary from peers.
    FetchingMetadata,
    /// Checking what already exists on disk.
    Checking,
    Downloading,
    Seeding,
    Paused,
    Error,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::FetchingMetadata => "metadata",
            State::Checking => "checking",
            State::Downloading => "downloading",
            State::Seeding => "seeding",
            State::Paused => "paused",
            State::Error => "error",
        }
    }
}

#[derive(Debug)]
pub enum Command {
    Pause,
    Resume,
    SetMode(PickMode),
    SetFilePriority(usize, Priority),
    /// Incoming connection whose handshake the listener already validated.
    Incoming(Box<(TcpStream, SocketAddr, Handshake)>),
    Recheck,
    Shutdown,
}

// -- snapshot for the UI ---------------------------------------------------

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    pub client: String,
    pub progress: f64,
    pub download_rate: f64,
    pub upload_rate: f64,
    pub choking_us: bool,
    pub interested_in_us: bool,
    pub we_choke: bool,
    pub in_flight: usize,
    pub snubbed: bool,
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub length: u64,
    pub downloaded: u64,
    pub priority: Priority,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub name: String,
    pub info_hash: String,
    pub state_label: String,
    pub error: Option<String>,
    pub have_pieces: usize,
    pub total_pieces: usize,
    /// Bitfield of the pieces we have, for the detailed progress bar.
    pub pieces: Vec<u8>,
    /// Pieces currently open, with their completed fraction.
    pub partials: Vec<(u32, f32)>,
    pub downloaded: u64,
    pub uploaded: u64,
    pub wanted: u64,
    pub total_size: u64,
    pub download_rate: f64,
    pub upload_rate: f64,
    pub eta: Option<u64>,
    pub peers: Vec<PeerInfo>,
    pub connected: usize,
    pub candidates: usize,
    pub seeds_in_swarm: u32,
    pub peers_in_swarm: u32,
    pub files: Vec<FileInfo>,
    pub trackers: Vec<TrackerStatus>,
    pub dht_nodes: usize,
    /// DHT nodes that responded recently.
    pub dht_good: usize,
    pub dht_queries: u64,
    /// Port we are actually listening on.
    pub listen_port: u16,
    pub down_history: Vec<f64>,
    pub up_history: Vec<f64>,
    pub sequential: bool,
    pub endgame: bool,
    /// Progress of the initial check, when applicable.
    pub check_progress: f64,
}

pub struct TorrentHandle {
    pub info_hash: Id20,
    pub(super) cmds: mpsc::Sender<Command>,
    pub(super) snapshot: Arc<Mutex<Snapshot>>,
}

impl TorrentHandle {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn try_send(&self, cmd: Command) {
        let _ = self.cmds.try_send(cmd);
    }
}
