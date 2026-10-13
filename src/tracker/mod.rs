//! Tracker coordination.
//!
//! Follows BEP-12: trackers come in *tiers*, and within each tier only the
//! first one to respond is used -- the one that worked moves to the top. Each tier
//! runs in its own task and dumps peers into the engine through a channel.

pub mod http;
pub mod udp;

use crate::id::Id20;
use anyhow::{Result, bail};
use rand::RngExt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Started,
    Completed,
    Stopped,
    /// Routine announce, with no associated event.
    Periodic,
}

#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
    pub num_want: i32,
    /// Identifies the client across IP changes (BEP-3).
    pub key: u32,
    pub tracker_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AnnounceResponse {
    pub interval: Duration,
    pub min_interval: Option<Duration>,
    pub peers: Vec<SocketAddr>,
    pub seeders: u32,
    pub leechers: u32,
    pub tracker_id: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerState {
    Waiting,
    Querying,
    Ok,
    Failed,
    Disabled,
}

#[derive(Debug, Clone)]
pub struct TrackerStatus {
    pub url: String,
    pub tier: usize,
    pub state: TrackerState,
    pub message: Option<String>,
    pub seeders: u32,
    pub leechers: u32,
    pub peers: usize,
    /// Seconds until the next announce.
    pub next_in: u64,
}

/// Counters that the engine keeps up to date and trackers read at announce
/// time.
#[derive(Default)]
pub struct Counters {
    pub uploaded: AtomicU64,
    pub downloaded: AtomicU64,
    pub left: AtomicU64,
}

impl Counters {
    pub fn set(&self, uploaded: u64, downloaded: u64, left: u64) {
        self.uploaded.store(uploaded, Ordering::Relaxed);
        self.downloaded.store(downloaded, Ordering::Relaxed);
        self.left.store(left, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct TrackerConfig {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub port: u16,
    pub num_want: i32,
    pub client: reqwest::Client,
}

#[derive(Debug, Clone, Copy)]
enum Command {
    Announce(Event),
    Shutdown,
}

/// The engine's handle to the set of trackers.
pub struct Trackers {
    cfg: TrackerConfig,
    counters: Arc<Counters>,
    status: Arc<Mutex<Vec<TrackerStatus>>>,
    cmds: broadcast::Sender<Command>,
    peers_tx: mpsc::Sender<Vec<SocketAddr>>,
    tiers: Mutex<usize>,
}

impl Trackers {
    pub fn new(
        cfg: TrackerConfig,
        tiers: Vec<Vec<String>>,
        peers_tx: mpsc::Sender<Vec<SocketAddr>>,
    ) -> Arc<Trackers> {
        let (cmds, _) = broadcast::channel(16);
        let trackers = Arc::new(Trackers {
            cfg,
            counters: Arc::new(Counters::default()),
            status: Arc::new(Mutex::new(Vec::new())),
            cmds,
            peers_tx,
            tiers: Mutex::new(0),
        });
        for tier in tiers {
            trackers.add_tier(tier);
        }
        trackers
    }

    pub fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    /// Spawns a new tier task. Also used for trackers that appear later,
    /// coming from a magnet link.
    pub fn add_tier(self: &Arc<Self>, urls: Vec<String>) {
        let urls: Vec<String> = urls
            .into_iter()
            .filter(|u| {
                u.starts_with("http://") || u.starts_with("https://") || u.starts_with("udp://")
            })
            .collect();
        if urls.is_empty() {
            return;
        }
        // A tracker that is already known does not get a second task.
        {
            let current = self.status.lock().unwrap_or_else(|e| e.into_inner());
            if urls.iter().all(|u| current.iter().any(|s| s.url == *u)) {
                return;
            }
        }

        let tier = {
            let mut n = self.tiers.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
            *n - 1
        };
        {
            let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            for url in &urls {
                status.push(TrackerStatus {
                    url: url.clone(),
                    tier,
                    state: TrackerState::Waiting,
                    message: None,
                    seeders: 0,
                    leechers: 0,
                    peers: 0,
                    next_in: 0,
                });
            }
        }

        let this = Arc::clone(self);
        tokio::spawn(async move {
            tier_loop(this, tier, urls).await;
        });
    }

    pub fn announce(&self, event: Event) {
        let _ = self.cmds.send(Command::Announce(event));
    }

    pub fn shutdown(&self) {
        let _ = self.cmds.send(Command::Shutdown);
    }

    pub fn status(&self) -> Vec<TrackerStatus> {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Tells the trackers we are leaving, with a short deadline -- nobody waits
    /// half a minute for the program to close.
    pub async fn announce_stopped(&self, time_limit: Duration) {
        let urls: Vec<String> = {
            let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            status
                .iter()
                .filter(|s| s.state == TrackerState::Ok)
                .map(|s| s.url.clone())
                .collect()
        };
        let req = self.build_request(Event::Stopped);
        let tasks: Vec<_> = urls
            .into_iter()
            .map(|url| {
                let cfg = self.cfg.clone();
                let req = req.clone();
                tokio::spawn(async move { announce_one(&cfg, &url, &req).await })
            })
            .collect();
        let _ = tokio::time::timeout(time_limit, async {
            for t in tasks {
                let _ = t.await;
            }
        })
        .await;
    }

    fn build_request(&self, event: Event) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: self.cfg.info_hash,
            peer_id: self.cfg.peer_id,
            port: self.cfg.port,
            uploaded: self.counters.uploaded.load(Ordering::Relaxed),
            downloaded: self.counters.downloaded.load(Ordering::Relaxed),
            left: self.counters.left.load(Ordering::Relaxed),
            event,
            num_want: self.cfg.num_want,
            key: 0,
            tracker_id: None,
        }
    }

    fn update<F: FnOnce(&mut TrackerStatus)>(&self, url: &str, f: F) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = status.iter_mut().find(|s| s.url == url) {
            f(s);
        }
    }
}

/// Delay applied after `n` consecutive failures, with a bit of randomness
/// so the trackers do not all come back in the same second.
fn backoff(failures: u32) -> Duration {
    let base = 30u64.saturating_mul(1 << failures.min(6));
    let jitter = rand::rng().random_range(0..=base / 4);
    Duration::from_secs((base + jitter).min(30 * 60))
}

async fn tier_loop(trackers: Arc<Trackers>, tier: usize, mut urls: Vec<String>) {
    let mut cmds = trackers.cmds.subscribe();
    let mut event = Event::Started;
    let mut key = rand::rng().random::<u32>();
    let mut next = Instant::now();
    let mut failures = 0u32;
    // The tracker may give us a session identifier to present again in the
    // next announces, and an interval floor that applies even to
    // event-triggered announces.
    let mut tracker_id: Option<String> = None;
    let mut min_interval: Option<Duration> = None;
    let mut last: Option<Instant> = None;

    loop {
        let wait = next.saturating_duration_since(Instant::now());
        {
            // Lets the UI show the countdown.
            let mut status = trackers.status.lock().unwrap_or_else(|e| e.into_inner());
            for s in status.iter_mut().filter(|s| s.tier == tier) {
                s.next_in = wait.as_secs();
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            cmd = cmds.recv() => match cmd {
                Ok(Command::Announce(e)) => {
                    event = e;
                    // Respecting the tracker's floor avoids getting a "too many
                    // requests" right after changing state.
                    next = match (last, min_interval) {
                        (Some(t), Some(m)) => Instant::now().max(t + m),
                        _ => Instant::now(),
                    };
                    continue;
                }
                Ok(Command::Shutdown) | Err(broadcast::error::RecvError::Closed) => {
                    let mut status = trackers.status.lock().unwrap_or_else(|e| e.into_inner());
                    for s in status.iter_mut().filter(|s| s.tier == tier) {
                        s.state = TrackerState::Disabled;
                    }
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }

        let mut req = trackers.build_request(event);
        req.key = key;
        req.tracker_id = tracker_id.clone();
        last = Some(Instant::now());

        let mut succeeded = false;
        for i in 0..urls.len() {
            let url = urls[i].clone();
            trackers.update(&url, |s| {
                s.state = TrackerState::Querying;
                s.message = None;
            });

            match announce_one(&trackers.cfg, &url, &req).await {
                Ok(response) => {
                    trackers.update(&url, |s| {
                        s.state = TrackerState::Ok;
                        s.seeders = response.seeders;
                        s.leechers = response.leechers;
                        s.peers = response.peers.len();
                        s.message = response.warning.clone();
                    });
                    if !response.peers.is_empty()
                        && trackers.peers_tx.send(response.peers).await.is_err()
                    {
                        return; // engine has shut down
                    }
                    // BEP-12: the tracker that responded becomes the first in the tier.
                    urls.swap(0, i);
                    if response.tracker_id.is_some() {
                        tracker_id = response.tracker_id.clone();
                    }
                    min_interval = response.min_interval;
                    next = Instant::now() + response.interval;
                    failures = 0;
                    succeeded = true;
                    break;
                }
                Err(e) => {
                    trackers.update(&url, |s| {
                        s.state = TrackerState::Failed;
                        s.message = Some(format!("{e:#}"));
                    });
                }
            }
        }

        if !succeeded {
            failures += 1;
            next = Instant::now() + backoff(failures);
            // A new identity avoids getting stuck with a `key` ban.
            if failures.is_multiple_of(4) {
                key = rand::rng().random::<u32>();
            }
        }
        if event == Event::Stopped {
            // We have already announced that we left; re-announcing "stopped"
            // every half hour makes no sense. The loop acts again when the
            // engine sends a new announce.
            next = Instant::now() + Duration::from_secs(24 * 3600);
        } else {
            // After the first announce, the following ones are routine.
            event = Event::Periodic;
        }
    }
}

async fn announce_one(
    cfg: &TrackerConfig,
    url: &str,
    req: &AnnounceRequest,
) -> Result<AnnounceResponse> {
    if url.starts_with("udp://") {
        udp::announce(url, req).await
    } else if url.starts_with("http://") || url.starts_with("https://") {
        http::announce(&cfg.client, url, req).await
    } else {
        bail!("unsupported tracker scheme: {url}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_has_a_cap() {
        let first = backoff(1).as_secs();
        assert!((60..=75).contains(&first), "60s + jitter, got {first}");
        assert!(backoff(3).as_secs() > backoff(1).as_secs());
        assert_eq!(backoff(30).as_secs(), 30 * 60, "must not exceed 30 minutes");
    }

    fn test_trackers(tiers: Vec<Vec<String>>) -> (Arc<Trackers>, mpsc::Receiver<Vec<SocketAddr>>) {
        let (tx, rx) = mpsc::channel(8);
        let cfg = TrackerConfig {
            info_hash: Id20::random(),
            peer_id: Id20::random(),
            port: 6881,
            num_want: 200,
            client: reqwest::Client::new(),
        };
        (Trackers::new(cfg, tiers, tx), rx)
    }

    #[tokio::test]
    async fn registers_tiers_and_ignores_unfamiliar_schemes() {
        let (trackers, _rx) = test_trackers(vec![
            vec!["http://a/ann".into(), "udp://b:6969".into()],
            vec!["wss://c/ann".into()],
        ]);
        let status = trackers.status();
        assert_eq!(status.len(), 2, "the websocket-only tier is discarded");
        assert_eq!(status[0].tier, 0);
        assert_eq!(status[1].tier, 0);
        assert!(status.iter().all(|s| s.state != TrackerState::Disabled));

        // A repeated tracker does not become a second task.
        trackers.add_tier(vec!["http://a/ann".into()]);
        assert_eq!(trackers.status().len(), 2);

        trackers.add_tier(vec!["http://new/ann".into()]);
        let status = trackers.status();
        assert_eq!(status.len(), 3);
        assert_eq!(status[2].tier, 1, "new tracker goes into its own tier");
    }

    #[tokio::test]
    async fn counters_are_included_in_the_request() {
        let (trackers, _rx) = test_trackers(vec![]);
        trackers.counters().set(111, 222, 333);
        let req = trackers.build_request(Event::Completed);
        assert_eq!((req.uploaded, req.downloaded, req.left), (111, 222, 333));
        assert_eq!(req.event, Event::Completed);
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use crate::metainfo::Metainfo;

    /// Manual diagnostic: `cargo test announce_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn announce_real() {
        let path = std::env::var("TORRENT").expect("set TORRENT=<path>.torrent");
        let meta = Metainfo::from_file(&path).unwrap();
        let cfg = TrackerConfig {
            info_hash: meta.info.info_hash,
            peer_id: crate::id::generate_peer_id(),
            port: 6881,
            num_want: 200,
            client: reqwest::Client::builder()
                .user_agent("downpour/0.1.0")
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        };
        let req = AnnounceRequest {
            info_hash: cfg.info_hash,
            peer_id: cfg.peer_id,
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: meta.info.total_length,
            event: Event::Started,
            num_want: 200,
            key: 12345,
            tracker_id: None,
        };
        for tier in &meta.tiers {
            for url in tier {
                println!("--> {url}");
                match announce_one(&cfg, url, &req).await {
                    Ok(r) => println!("    OK: {} peers, {} seeds", r.peers.len(), r.seeders),
                    Err(e) => println!("    ERROR: {e:#}"),
                }
            }
        }
    }
}
