//! Session: what is shared between all torrents — the listening port, the
//! DHT node, and the list of active downloads.

use crate::dht::{self, Dht};
use crate::engine::{self, Command, Config, Snapshot, Source, TorrentHandle};
use crate::id::Id20;
use crate::peer;
use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

pub struct Session {
    torrents: Mutex<Vec<TorrentHandle>>,
    dht: Option<Arc<Dht>>,
    cfg: Config,
    port: u16,
}

impl Session {
    /// Opens the listening port and, if requested, joins the DHT.
    ///
    /// Neither is mandatory: without an open port the client still downloads
    /// (it just cannot receive connections), and without DHT it still works
    /// with trackers alone.
    pub async fn start(mut cfg: Config) -> Result<Arc<Session>> {
        let listener = bind_listener(cfg.listen_port).await;
        let port = match &listener {
            Some(l) => l.local_addr().map(|a| a.port()).unwrap_or(cfg.listen_port),
            None => cfg.listen_port,
        };
        cfg.listen_port = port;

        let dht = if cfg.use_dht {
            match Dht::bind(port).await {
                Ok(d) => {
                    // Nodes saved from the previous run are usually still up,
                    // and enter the table at no network cost. The public
                    // routers stay as plan B.
                    let saved = load_dht_nodes();
                    for (id, addr) in saved {
                        d.add_node(crate::dht::krpc::NodeInfo { id, addr });
                    }

                    let d2 = Arc::clone(&d);
                    tokio::spawn(async move {
                        let bootstrap: Vec<String> =
                            dht::BOOTSTRAP.iter().map(|s| s.to_string()).collect();
                        d2.bootstrap(&bootstrap).await;
                        save_periodically(d2).await;
                    });
                    Some(d)
                }
                // A busy UDP port is no reason to abort the download.
                Err(_) => None,
            }
        } else {
            None
        };

        let session = Arc::new(Session {
            torrents: Mutex::new(Vec::new()),
            dht,
            cfg,
            port,
        });

        if let Some(listener) = listener {
            let s = Arc::clone(&session);
            tokio::spawn(async move { accept_loop(s, listener).await });
        }
        Ok(session)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn add(&self, source: Source) -> Id20 {
        let info_hash = source.info_hash();
        let handle = engine::spawn(source, self.cfg.clone(), self.dht.clone());
        self.torrents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(handle);
        info_hash
    }

    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.torrents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|t| t.snapshot())
            .collect()
    }

    pub fn count(&self) -> usize {
        self.torrents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Sends a command to the torrent at index `i`.
    pub fn command(&self, i: usize, cmd: Command) {
        if let Some(t) = self
            .torrents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(i)
        {
            t.try_send(cmd);
        }
    }

    pub async fn shutdown(&self) {
        {
            let torrents = self.torrents.lock().unwrap_or_else(|e| e.into_inner());
            for t in torrents.iter() {
                t.try_send(Command::Shutdown);
            }
        }
        if let Some(dht) = &self.dht {
            save_dht_nodes(&dht.snapshot());
        }
        // Gives `stopped` a moment to reach the trackers before exiting.
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
}

/// File where we keep DHT nodes between runs.
fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("downpour"))
}

fn load_dht_nodes() -> Vec<(Id20, SocketAddr)> {
    // Fall back to the pre-rename cache directory (`torrra`) so saved nodes
    // survive the rename to downpour.
    let file = cache_path()
        .map(|d| d.join("dht-nodes.txt"))
        .filter(|f| f.exists())
        .or_else(|| {
            cache_path()
                .and_then(|d| d.parent().map(|p| p.join("torrra")))
                .map(|d| d.join("dht-nodes.txt"))
        });
    let Some(file) = file else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (id, addr) = line.split_once(' ')?;
            Some((Id20::from_hex(id).ok()?, addr.trim().parse().ok()?))
        })
        .take(400)
        .collect()
}

fn save_dht_nodes(nodes: &[(Id20, SocketAddr)]) {
    if nodes.is_empty() {
        return;
    }
    let Some(dir) = cache_path() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let contents: String = nodes
        .iter()
        .take(400)
        .map(|(id, addr)| format!("{} {addr}\n", id.to_hex()))
        .collect();
    // Failing here does not affect the download; the DHT just restarts from zero.
    let _ = std::fs::write(dir.join("dht-nodes.txt"), contents);
}

async fn save_periodically(dht: Arc<Dht>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
    tick.tick().await; // the first one fires immediately
    loop {
        tick.tick().await;
        save_dht_nodes(&dht.snapshot());
    }
}

/// Tries the requested port and, if busy, lets the OS pick one.
async fn bind_listener(port: u16) -> Option<TcpListener> {
    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => Some(l),
        Err(_) => TcpListener::bind(("0.0.0.0", 0)).await.ok(),
    }
}

/// Accepts connections and routes them to the torrent that owns the
/// handshake's info_hash.
async fn accept_loop(session: Arc<Session>, listener: TcpListener) {
    loop {
        let Ok((mut socket, addr)) = listener.accept().await else {
            continue;
        };
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            // Reading the handshake here is what lets us know which torrent
            // the connection belongs to before creating any state for it.
            let Ok(handshake) = peer::read_incoming_handshake(&mut socket).await else {
                return;
            };
            let target = {
                let torrents = session.torrents.lock().unwrap_or_else(|e| e.into_inner());
                torrents
                    .iter()
                    .position(|t| t.info_hash == handshake.info_hash)
            };
            let Some(i) = target else {
                return; // unknown torrent: drop it
            };
            session.command(i, Command::Incoming(Box::new((socket, addr, handshake))));
        });
    }
}

/// Reads a `.torrent` from disk or a magnet from what the user typed.
pub fn parse_source(input: &str) -> Result<Source> {
    if input.starts_with("magnet:") {
        return Ok(Source::Magnet(Box::new(crate::magnet::Magnet::parse(
            input,
        )?)));
    }
    // No extra context here: `from_file` already names the file, and repeating
    // the path at every level makes the message unreadable when it is long.
    Ok(Source::Torrent(Box::new(
        crate::metainfo::Metainfo::from_file(input)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network proof of the pause/resume cycle, against a real swarm.
    ///
    /// `TORRENT=x.torrent cargo test real_pause_resume_cycle -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn real_pause_resume_cycle() {
        let path = std::env::var("TORRENT").expect("set TORRENT=path.torrent");
        let dest_dir = std::env::temp_dir().join("downpour-pause");
        let _ = std::fs::remove_dir_all(&dest_dir);

        let s = Session::start(Config {
            dest: dest_dir.clone(),
            listen_port: 0,
            ..Default::default()
        })
        .await
        .unwrap();
        s.add(parse_source(&path).unwrap());

        let wait_for_download = |label: &'static str| {
            let s = Arc::clone(&s);
            async move {
                for _ in 0..60 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let snap = &s.snapshots()[0];
                    if snap.download_rate > 100.0 * 1024.0 {
                        println!(
                            "{label}: {:.1} MiB/s with {} peers",
                            snap.download_rate / 1024.0 / 1024.0,
                            snap.connected
                        );
                        return true;
                    }
                }
                false
            }
        };

        assert!(wait_for_download("before pause").await, "never took off");

        s.command(0, Command::Pause);
        tokio::time::sleep(Duration::from_secs(5)).await;
        let paused = &s.snapshots()[0];
        println!("paused: {} peers", paused.connected);
        assert_eq!(paused.state_label, "paused");
        assert_eq!(paused.connected, 0, "pausing drops the peers");

        s.command(0, Command::Resume);
        assert!(
            wait_for_download("after resume").await,
            "did not download again after resuming"
        );

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    #[test]
    fn identifies_magnet_and_file() {
        let m =
            parse_source("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(matches!(m, Source::Magnet(_)));
        assert!(parse_source("does-not-exist.torrent").is_err());
    }

    #[tokio::test]
    async fn session_opens_port_and_accepts_torrents() {
        let cfg = Config {
            listen_port: 0,
            use_dht: false,
            ..Default::default()
        };
        let s = Session::start(cfg).await.unwrap();
        assert!(s.port() > 0, "the OS picked a free port");
        assert_eq!(s.count(), 0);

        let magnet = crate::magnet::Magnet::parse(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=test",
        )
        .unwrap();
        let ih = s.add(Source::Magnet(Box::new(magnet)));
        assert_eq!(s.count(), 1);
        assert_eq!(ih.to_hex(), "0123456789abcdef0123456789abcdef01234567");

        let snaps = s.snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].name, "test");
    }
}
