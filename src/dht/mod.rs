//! DHT node (BEP-5).
//!
//! It serves two purposes: discovering peers for an info_hash without relying
//! on a tracker, and answering queries from other nodes - a DHT where nobody
//! answers does not exist.

pub mod krpc;
pub mod routing;

use crate::id::Id20;
use anyhow::{Context, Result};
use krpc::{Message, NodeInfo, Query, Response};
use rand::RngExt;
use routing::{K, RoutingTable};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{UdpSocket, lookup_host};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

/// Public routers used to join the network.
pub const BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
];

/// Simultaneous queries during a lookup (Kademlia's "alpha").
const ALPHA: usize = 8;
/// How long we wait for a response.
const QUERY_TIMEOUT: Duration = Duration::from_secs(4);
/// Maximum rounds of an iterative lookup, so it always terminates.
const MAX_ROUNDS: usize = 12;
/// Lifetime of a peer announced to us.
const PEER_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Default)]
pub struct DhtStats {
    pub nodes: usize,
    pub good_nodes: usize,
    pub queries_sent: u64,
}

pub struct Dht {
    socket: UdpSocket,
    node_id: Id20,
    routing: Mutex<RoutingTable>,
    /// Our queries that are still unanswered, by transaction id.
    pending: Mutex<HashMap<u16, oneshot::Sender<Result<Response, String>>>>,
    next_tid: Mutex<u16>,
    /// Peers that other nodes announced to us.
    peer_store: Mutex<HashMap<Id20, Vec<(SocketAddr, Instant)>>>,
    /// Secrets used to sign tokens (the current one and the previous one).
    secrets: Mutex<[[u8; 16]; 2]>,
    stats: Statistics,
}

#[derive(Default)]
struct Statistics {
    queries_sent: AtomicU64,
    queries_received: AtomicU64,
    peers_found: AtomicU64,
}

impl Dht {
    /// Opens the UDP socket and starts the receive and maintenance tasks.
    pub async fn bind(port: u16) -> Result<Arc<Dht>> {
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .await
            .with_context(|| format!("could not open UDP port {port} for the DHT"))?;
        let node_id = Id20::random();

        let dht = Arc::new(Dht {
            socket,
            node_id,
            routing: Mutex::new(RoutingTable::new(node_id)),
            pending: Mutex::new(HashMap::new()),
            next_tid: Mutex::new(0),
            peer_store: Mutex::new(HashMap::new()),
            secrets: Mutex::new([rand::rng().random(), rand::rng().random()]),
            stats: Statistics::default(),
        });

        tokio::spawn(recv_loop(Arc::clone(&dht)));
        tokio::spawn(maintenance_loop(Arc::clone(&dht)));
        Ok(dht)
    }

    #[cfg(test)]
    pub fn node_id(&self) -> Id20 {
        self.node_id
    }

    #[cfg(test)]
    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn stats(&self) -> DhtStats {
        let routing = self.routing.lock().unwrap_or_else(|e| e.into_inner());
        DhtStats {
            nodes: routing.len(),
            good_nodes: routing.count_good(),
            queries_sent: self.stats.queries_sent.load(Ordering::Relaxed),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .count_good()
            > 0
    }

    /// Known nodes, to save between runs.
    pub fn snapshot(&self) -> Vec<(Id20, SocketAddr)> {
        self.routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    pub fn add_node(&self, info: NodeInfo) {
        self.routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add(info);
    }

    /// Joins the network by querying the known routers for ourselves.
    pub async fn bootstrap(self: &Arc<Self>, hosts: &[String]) -> usize {
        let mut tasks = JoinSet::new();
        for host in hosts {
            let host = host.clone();
            let this = Arc::clone(self);
            tasks.spawn(async move {
                let Ok(addrs) = lookup_host(&host).await else {
                    return 0;
                };
                let mut found = 0;
                for addr in addrs.take(2) {
                    if let Ok(resp) = this
                        .query(
                            addr,
                            Query::FindNode {
                                id: this.node_id,
                                target: this.node_id,
                            },
                        )
                        .await
                    {
                        let mut routing = this.routing.lock().unwrap_or_else(|e| e.into_inner());
                        // The router itself does not enter the table (it does not store
                        // data); only the nodes it pointed to do.
                        for node in &resp.nodes {
                            if routing.add(*node) {
                                found += 1;
                            }
                        }
                    }
                }
                found
            });
        }
        let mut total = 0;
        while let Some(r) = tasks.join_next().await {
            total += r.unwrap_or(0);
        }
        crate::log::log!("dht", "bootstrap brought in {total} nodes");
        // A lookup for a random target spreads the table across several buckets.
        if total > 0 {
            self.find_node(Id20::random()).await;
        }
        total
    }

    /// Iterative lookup for peers of an info_hash.
    ///
    /// Each peer found is dispatched immediately over the channel - the download
    /// can start while the lookup is still walking the network.
    pub async fn get_peers(
        self: &Arc<Self>,
        info_hash: Id20,
        peers_tx: Option<mpsc::Sender<Vec<SocketAddr>>>,
    ) -> Vec<(NodeInfo, Vec<u8>)> {
        let mut candidates = Candidates::new(info_hash);
        candidates.offer(
            self.routing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closest(&info_hash, K * 2),
        );
        let mut with_token: Vec<(NodeInfo, Vec<u8>)> = Vec::new();

        for _ in 0..MAX_ROUNDS {
            let batch = candidates.next_batch(ALPHA);
            if batch.is_empty() {
                break;
            }

            let mut tasks = JoinSet::new();
            for node in batch {
                let this = Arc::clone(self);
                tasks.spawn(async move {
                    let r = this
                        .query(
                            node.addr,
                            Query::GetPeers {
                                id: this.node_id,
                                info_hash,
                            },
                        )
                        .await;
                    (node, r)
                });
            }

            let mut found_anything = false;
            while let Some(Ok((node, result))) = tasks.join_next().await {
                match result {
                    Ok(resp) => {
                        candidates.mark_ok(node.id);
                        found_anything |= candidates.offer(resp.nodes);
                        if let Some(token) = resp.token {
                            with_token.push((node, token));
                        }
                        if !resp.values.is_empty() {
                            self.stats
                                .peers_found
                                .fetch_add(resp.values.len() as u64, Ordering::Relaxed);
                            if let Some(tx) = &peers_tx
                                && tx.send(resp.values).await.is_err()
                            {
                                return with_token;
                            }
                        }
                    }
                    Err(_) => candidates.mark_failed(node.id),
                }
            }
            if !found_anything && candidates.is_exhausted() {
                break;
            }
        }

        crate::log::log!(
            "dht",
            "lookup for {} finished: {} nodes with token, table has {} nodes",
            &info_hash.to_hex()[..8],
            with_token.len(),
            self.routing.lock().unwrap_or_else(|e| e.into_inner()).len()
        );
        // Only the closest ones matter for announcing afterwards.
        with_token.sort_unstable_by_key(|(node, _)| node.id.distance(&info_hash));
        with_token.truncate(K);
        with_token
    }

    /// Announces that we have this torrent, so others can find us.
    pub async fn announce_peer(self: &Arc<Self>, info_hash: Id20, port: u16) -> usize {
        let targets = self.get_peers(info_hash, None).await;
        let mut tasks = JoinSet::new();
        for (node, token) in targets {
            let this = Arc::clone(self);
            tasks.spawn(async move {
                this.query(
                    node.addr,
                    Query::AnnouncePeer {
                        id: this.node_id,
                        info_hash,
                        port,
                        token,
                        implied_port: false,
                    },
                )
                .await
                .is_ok()
            });
        }
        let mut accepted = 0;
        while let Some(r) = tasks.join_next().await {
            accepted += usize::from(r.unwrap_or(false));
        }
        accepted
    }

    /// Looks up nodes close to a target - used during bootstrap and bucket
    /// refresh; the result feeds the routing table.
    async fn find_node(self: &Arc<Self>, target: Id20) {
        let mut candidates = Candidates::new(target);
        candidates.offer(
            self.routing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closest(&target, K * 2),
        );

        for _ in 0..MAX_ROUNDS / 2 {
            let batch = candidates.next_batch(ALPHA);
            if batch.is_empty() {
                break;
            }
            let mut tasks = JoinSet::new();
            for node in batch {
                let this = Arc::clone(self);
                tasks.spawn(async move {
                    let r = this
                        .query(
                            node.addr,
                            Query::FindNode {
                                id: this.node_id,
                                target,
                            },
                        )
                        .await;
                    (node, r)
                });
            }
            while let Some(Ok((node, result))) = tasks.join_next().await {
                match result {
                    Ok(resp) => {
                        candidates.mark_ok(node.id);
                        candidates.offer(resp.nodes);
                    }
                    Err(_) => candidates.mark_failed(node.id),
                }
            }
        }
        for node in candidates.responded() {
            self.routing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add(node);
        }
    }

    /// Sends a query and waits for the response.
    async fn query(&self, addr: SocketAddr, query: Query) -> Result<Response, String> {
        let tid = {
            let mut n = self.next_tid.lock().unwrap_or_else(|e| e.into_inner());
            *n = n.wrapping_add(1);
            *n
        };
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tid, tx);

        let msg = Message::Query {
            transaction: tid.to_be_bytes().to_vec(),
            query,
        };
        let sent = self.socket.send_to(&msg.encode(), addr).await;
        self.stats.queries_sent.fetch_add(1, Ordering::Relaxed);

        if let Err(e) = sent {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&tid);
            return Err(format!("send failed: {e}"));
        }

        let result = match tokio::time::timeout(QUERY_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("response dropped".to_string()),
            Err(_) => Err("no response".to_string()),
        };
        // On timeout the entry is still there; without this the map would grow
        // forever in a network with many dead nodes.
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&tid);
        result
    }

    /// Token that proves the node really is at the IP it claims to be.
    fn make_token(&self, addr: &SocketAddr, index: usize) -> Vec<u8> {
        let secret = self.secrets.lock().unwrap_or_else(|e| e.into_inner())[index];
        let mut hasher = Sha1::new();
        hasher.update(secret);
        match addr.ip() {
            std::net::IpAddr::V4(ip) => hasher.update(ip.octets()),
            std::net::IpAddr::V6(ip) => hasher.update(ip.octets()),
        }
        hasher.finalize()[..8].to_vec()
    }

    /// Accepts the current secret and the previous one: a token issued shortly
    /// before the rotation would still be valid for whoever received it.
    fn check_token(&self, addr: &SocketAddr, token: &[u8]) -> bool {
        (0..2).any(|i| self.make_token(addr, i) == token)
    }

    fn store_peer(&self, info_hash: Id20, addr: SocketAddr) {
        let mut store = self.peer_store.lock().unwrap_or_else(|e| e.into_inner());
        let list = store.entry(info_hash).or_default();
        list.retain(|(a, _)| *a != addr);
        list.push((addr, Instant::now()));
        // A popular info_hash must not turn into a memory leak.
        if list.len() > 128 {
            list.remove(0);
        }
    }

    fn lookup_peers(&self, info_hash: Id20) -> Vec<SocketAddr> {
        let store = self.peer_store.lock().unwrap_or_else(|e| e.into_inner());
        store
            .get(&info_hash)
            .map(|list| {
                list.iter()
                    .filter(|(_, t)| t.elapsed() < PEER_TTL)
                    .map(|(a, _)| *a)
                    .take(50)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Builds the response to a query from another node.
    fn answer(&self, from: SocketAddr, query: &Query) -> Message {
        let respond = |response| Message::Response {
            transaction: Vec::new(),
            response,
        };
        match query {
            Query::Ping { .. } => respond(Response {
                id: self.node_id,
                ..Default::default()
            }),
            Query::FindNode { target, .. } => respond(Response {
                id: self.node_id,
                nodes: self
                    .routing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .closest(target, K),
                ..Default::default()
            }),
            Query::GetPeers { info_hash, .. } => {
                let peers = self.lookup_peers(*info_hash);
                respond(Response {
                    id: self.node_id,
                    // Whoever has peers sends peers; whoever does not sends the path.
                    nodes: if peers.is_empty() {
                        self.routing
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .closest(info_hash, K)
                    } else {
                        Vec::new()
                    },
                    values: peers,
                    token: Some(self.make_token(&from, 0)),
                })
            }
            Query::AnnouncePeer {
                info_hash,
                port,
                token,
                implied_port,
                ..
            } => {
                if !self.check_token(&from, token) {
                    return Message::Error {
                        transaction: Vec::new(),
                        code: 203,
                        message: "invalid token".to_string(),
                    };
                }
                let port = if *implied_port { from.port() } else { *port };
                if port != 0 {
                    self.store_peer(*info_hash, SocketAddr::new(from.ip(), port));
                }
                respond(Response {
                    id: self.node_id,
                    ..Default::default()
                })
            }
        }
    }
}

/// Set of nodes in a lookup, sorted by distance to the target.
struct Candidates {
    target: Id20,
    queue: Vec<NodeInfo>,
    queried: HashSet<Id20>,
    alive: HashSet<Id20>,
    seen: HashSet<Id20>,
}

impl Candidates {
    fn new(target: Id20) -> Candidates {
        Candidates {
            target,
            queue: Vec::new(),
            queried: HashSet::new(),
            alive: HashSet::new(),
            seen: HashSet::new(),
        }
    }

    /// Adds nodes not yet known; returns `true` if any was new.
    fn offer(&mut self, nodes: impl IntoIterator<Item = NodeInfo>) -> bool {
        let mut any_new = false;
        for node in nodes {
            if self.seen.insert(node.id) {
                self.queue.push(node);
                any_new = true;
            }
        }
        if any_new {
            let target = self.target;
            self.queue.sort_unstable_by_key(|n| n.id.distance(&target));
            // The lookup only needs to keep the horizon close to the target.
            self.queue.truncate(64);
        }
        any_new
    }

    fn next_batch(&mut self, n: usize) -> Vec<NodeInfo> {
        let chosen: Vec<NodeInfo> = self
            .queue
            .iter()
            .filter(|node| !self.queried.contains(&node.id))
            .take(n)
            .copied()
            .collect();
        for node in &chosen {
            self.queried.insert(node.id);
        }
        chosen
    }

    fn mark_ok(&mut self, id: Id20) {
        self.alive.insert(id);
    }

    fn mark_failed(&mut self, id: Id20) {
        self.queue.retain(|n| n.id != id);
    }

    fn is_exhausted(&self) -> bool {
        self.queue.iter().all(|n| self.queried.contains(&n.id))
    }

    /// Nodes that responded - the only ones that deserve to enter the table.
    fn responded(&self) -> Vec<NodeInfo> {
        self.queue
            .iter()
            .filter(|n| self.alive.contains(&n.id))
            .copied()
            .collect()
    }
}

async fn recv_loop(dht: Arc<Dht>) {
    let mut buf = vec![0u8; 4096];
    loop {
        let (n, from) = match dht.socket.recv_from(&mut buf).await {
            Ok(r) => r,
            // ICMP port unreachable becomes a recv error on Windows; ignore it and
            // carry on, otherwise the whole DHT dies because of one offline node.
            Err(_) => continue,
        };

        let Ok(msg) = Message::decode(&buf[..n]) else {
            continue;
        };
        match msg {
            Message::Query { transaction, query } => {
                dht.stats.queries_received.fetch_add(1, Ordering::Relaxed);
                // Whoever talks to us enters the table: it is a provably live node.
                if let Some(id) = query_id(&query) {
                    dht.routing
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .add(NodeInfo { id, addr: from });
                }
                let mut response = dht.answer(from, &query);
                match &mut response {
                    Message::Response { transaction: t, .. }
                    | Message::Error { transaction: t, .. } => *t = transaction,
                    Message::Query { .. } => unreachable!(),
                }
                let _ = dht.socket.send_to(&response.encode(), from).await;
            }
            Message::Response {
                transaction,
                response,
            } => {
                {
                    let mut routing = dht.routing.lock().unwrap_or_else(|e| e.into_inner());
                    routing.add(NodeInfo {
                        id: response.id,
                        addr: from,
                    });
                    routing.touch(&response.id);
                }
                if let Some(tx) = take_pending(&dht, &transaction) {
                    let _ = tx.send(Ok(response));
                }
            }
            Message::Error {
                transaction,
                code,
                message,
            } => {
                if let Some(tx) = take_pending(&dht, &transaction) {
                    let _ = tx.send(Err(format!("error {code}: {message}")));
                }
            }
        }
    }
}

fn take_pending(
    dht: &Dht,
    transaction: &[u8],
) -> Option<oneshot::Sender<Result<Response, String>>> {
    let tid = u16::from_be_bytes(transaction.get(..2)?.try_into().ok()?);
    dht.pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&tid)
}

fn query_id(query: &Query) -> Option<Id20> {
    Some(match query {
        Query::Ping { id }
        | Query::FindNode { id, .. }
        | Query::GetPeers { id, .. }
        | Query::AnnouncePeer { id, .. } => *id,
    })
}

async fn maintenance_loop(dht: Arc<Dht>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut cycles = 0u32;
    loop {
        tick.tick().await;
        cycles += 1;

        // Rotates the token secret every 5 minutes.
        if cycles.is_multiple_of(5) {
            let mut secrets = dht.secrets.lock().unwrap_or_else(|e| e.into_inner());
            secrets[1] = secrets[0];
            secrets[0] = rand::rng().random();
        }

        // Discards expired peers.
        {
            let mut store = dht.peer_store.lock().unwrap_or_else(|e| e.into_inner());
            for list in store.values_mut() {
                list.retain(|(_, t)| t.elapsed() < PEER_TTL);
            }
            store.retain(|_, list| !list.is_empty());
        }

        // Checks questionable nodes before losing them.
        let questionable = dht
            .routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .questionable();
        for node in questionable.into_iter().take(ALPHA) {
            let this = Arc::clone(&dht);
            tokio::spawn(async move {
                if this
                    .query(node.addr, Query::Ping { id: this.node_id })
                    .await
                    .is_err()
                {
                    this.routing
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .mark_failed(&node.id);
                }
            });
        }

        // Revives stale buckets.
        let targets = dht
            .routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refresh_targets();
        for target in targets.into_iter().take(2) {
            let this = Arc::clone(&dht);
            tokio::spawn(async move {
                this.find_node(target).await;
                this.routing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mark_bucket_refreshed(&target);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn local_dht() -> Arc<Dht> {
        Dht::bind(0).await.expect("bind on an ephemeral port")
    }

    #[tokio::test]
    async fn answers_ping_and_find_node() {
        let a = local_dht().await;
        let b = local_dht().await;
        let addr_b = SocketAddr::from(([127, 0, 0, 1], b.local_port()));

        let resp = a
            .query(addr_b, Query::Ping { id: a.node_id() })
            .await
            .expect("ping must be answered");
        assert_eq!(resp.id, b.node_id());

        // The ping made `b` learn about `a`.
        assert!(b.stats().nodes >= 1, "whoever talks to us enters the table");

        let resp = a
            .query(
                addr_b,
                Query::FindNode {
                    id: a.node_id(),
                    target: Id20::random(),
                },
            )
            .await
            .unwrap();
        assert_eq!(resp.id, b.node_id());
    }

    #[tokio::test]
    async fn get_peers_and_announce_with_token() {
        let a = local_dht().await;
        let b = local_dht().await;
        let addr_b = SocketAddr::from(([127, 0, 0, 1], b.local_port()));
        let info_hash = Id20::random();

        // With nobody announced, we get a token and nodes (none, here) but no peers.
        let resp = a
            .query(
                addr_b,
                Query::GetPeers {
                    id: a.node_id(),
                    info_hash,
                },
            )
            .await
            .unwrap();
        let token = resp.token.clone().expect("get_peers returns a token");
        assert!(resp.values.is_empty());

        let ok = a
            .query(
                addr_b,
                Query::AnnouncePeer {
                    id: a.node_id(),
                    info_hash,
                    port: 51413,
                    token,
                    implied_port: false,
                },
            )
            .await;
        assert!(ok.is_ok(), "announce with a valid token is accepted");

        // Now `b` knows a peer for this info_hash.
        let resp = a
            .query(
                addr_b,
                Query::GetPeers {
                    id: a.node_id(),
                    info_hash,
                },
            )
            .await
            .unwrap();
        assert_eq!(resp.values.len(), 1);
        assert_eq!(resp.values[0].port(), 51413);
    }

    #[tokio::test]
    async fn rejects_announce_with_bad_token() {
        let a = local_dht().await;
        let b = local_dht().await;
        let addr_b = SocketAddr::from(([127, 0, 0, 1], b.local_port()));

        let error = a
            .query(
                addr_b,
                Query::AnnouncePeer {
                    id: a.node_id(),
                    info_hash: Id20::random(),
                    port: 6881,
                    token: b"faketoken".to_vec(),
                    implied_port: false,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("203"), "error came back: {error}");
    }

    #[tokio::test]
    async fn unanswered_query_times_out_and_cleans_up() {
        let a = local_dht().await;
        // Port where nobody is listening.
        let dead = SocketAddr::from(([127, 0, 0, 1], 1));
        let r = tokio::time::timeout(
            QUERY_TIMEOUT + Duration::from_secs(2),
            a.query(dead, Query::Ping { id: a.node_id() }),
        )
        .await
        .expect("the query must give up on its own");
        assert!(r.is_err());
        assert!(
            a.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "no leak"
        );
    }

    #[test]
    fn candidates_sort_and_do_not_repeat() {
        let target = Id20([0xff; 20]);
        let mut c = Candidates::new(target);
        let near = NodeInfo {
            id: Id20([0xfe; 20]),
            addr: "1.1.1.1:1".parse().unwrap(),
        };
        let far = NodeInfo {
            id: Id20([0x00; 20]),
            addr: "2.2.2.2:2".parse().unwrap(),
        };

        assert!(c.offer([far, near]));
        assert!(!c.offer([near]), "a repeated node does not count as new");

        let batch = c.next_batch(1);
        assert_eq!(batch[0].id, near.id, "the closest to the target goes first");
        assert_eq!(c.next_batch(1)[0].id, far.id, "does not repeat a query");
        assert!(c.is_exhausted());

        c.mark_ok(near.id);
        assert_eq!(c.responded().len(), 1, "only whoever responded counts");
    }
}
