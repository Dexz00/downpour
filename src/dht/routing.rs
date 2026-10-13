//! Kademlia routing table.
//!
//! 160 buckets, one per bit of common prefix with our id. Each bucket holds
//! up to `K` nodes and prefers the old ones: a node that has already been up
//! for hours is more likely to stay up than a newcomer.

use super::krpc::NodeInfo;
use crate::id::Id20;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Bucket size, fixed at 8 by BEP-5.
pub const K: usize = 8;

/// With no news for this long, the node becomes "questionable" and needs a ping.
const QUESTIONABLE_AFTER: Duration = Duration::from_secs(15 * 60);
/// Consecutive failures before discarding the node.
const MAX_FAILURES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Good,
    Questionable,
    Bad,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub info: NodeInfo,
    pub last_seen: Instant,
    pub failures: u32,
}

impl Node {
    fn new(info: NodeInfo) -> Node {
        Node {
            info,
            last_seen: Instant::now(),
            failures: 0,
        }
    }

    pub fn state(&self) -> NodeState {
        if self.failures >= MAX_FAILURES {
            NodeState::Bad
        } else if self.last_seen.elapsed() > QUESTIONABLE_AFTER {
            NodeState::Questionable
        } else {
            NodeState::Good
        }
    }
}

#[derive(Debug, Default)]
struct Bucket {
    nodes: Vec<Node>,
    last_changed: Option<Instant>,
}

pub struct RoutingTable {
    own_id: Id20,
    buckets: Vec<Bucket>,
}

impl RoutingTable {
    pub fn new(own_id: Id20) -> RoutingTable {
        RoutingTable {
            own_id,
            buckets: (0..160).map(|_| Bucket::default()).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.nodes.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn count_good(&self) -> usize {
        self.buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|n| n.state() == NodeState::Good)
            .count()
    }

    fn bucket_for(&self, id: &Id20) -> usize {
        self.own_id.bucket_index(id).min(159)
    }

    /// Records a node seen on the network.
    ///
    /// Returns `true` if it entered the table. Full buckets only make room by
    /// discarding a bad node - swapping a stable node for an unknown one would
    /// only make life easier for anyone trying to pollute the table.
    pub fn add(&mut self, info: NodeInfo) -> bool {
        if info.id == self.own_id || !crate::compact::is_usable(&info.addr) {
            return false;
        }
        let idx = self.bucket_for(&info.id);
        let bucket = &mut self.buckets[idx];

        if let Some(existing) = bucket.nodes.iter_mut().find(|n| n.info.id == info.id) {
            existing.info.addr = info.addr;
            existing.last_seen = Instant::now();
            existing.failures = 0;
            bucket.last_changed = Some(Instant::now());
            return true;
        }
        if bucket.nodes.len() < K {
            bucket.nodes.push(Node::new(info));
            bucket.last_changed = Some(Instant::now());
            return true;
        }
        if let Some(bad) = bucket
            .nodes
            .iter()
            .position(|n| n.state() == NodeState::Bad)
        {
            bucket.nodes[bad] = Node::new(info);
            bucket.last_changed = Some(Instant::now());
            return true;
        }
        false
    }

    /// The node responded: renew its validity.
    pub fn touch(&mut self, id: &Id20) {
        let idx = self.bucket_for(id);
        if let Some(node) = self.buckets[idx]
            .nodes
            .iter_mut()
            .find(|n| n.info.id == *id)
        {
            node.last_seen = Instant::now();
            node.failures = 0;
        }
    }

    /// The node did not respond; after a few times it is removed.
    pub fn mark_failed(&mut self, id: &Id20) {
        let idx = self.bucket_for(id);
        let bucket = &mut self.buckets[idx];
        if let Some(pos) = bucket.nodes.iter().position(|n| n.info.id == *id) {
            bucket.nodes[pos].failures += 1;
            if bucket.nodes[pos].state() == NodeState::Bad {
                bucket.nodes.remove(pos);
            }
        }
    }

    /// The `n` nodes closest to `target`, from closest to farthest.
    pub fn closest(&self, target: &Id20, n: usize) -> Vec<NodeInfo> {
        let mut candidates: Vec<&Node> = self
            .buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|node| node.state() != NodeState::Bad)
            .collect();
        candidates.sort_unstable_by_key(|node| node.info.id.distance(target));
        candidates
            .into_iter()
            .take(n)
            .map(|node| node.info)
            .collect()
    }

    /// Questionable nodes that deserve a ping before being discarded.
    pub fn questionable(&self) -> Vec<NodeInfo> {
        self.buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|n| n.state() == NodeState::Questionable)
            .map(|n| n.info)
            .collect()
    }

    /// Random ids inside stale buckets, to revive them with a `find_node`.
    /// Without this the table ages and lookups gradually lose reach.
    pub fn refresh_targets(&self) -> Vec<Id20> {
        let mut targets = Vec::new();
        for (i, bucket) in self.buckets.iter().enumerate() {
            let stale = bucket
                .last_changed
                .is_none_or(|t| t.elapsed() > QUESTIONABLE_AFTER);
            if stale && !bucket.nodes.is_empty() {
                targets.push(random_in_bucket(&self.own_id, i));
            }
        }
        targets
    }

    pub fn mark_bucket_refreshed(&mut self, target: &Id20) {
        let idx = self.bucket_for(target);
        self.buckets[idx].last_changed = Some(Instant::now());
    }

    /// Snapshot of live nodes, to write to disk and restart the DHT quickly on
    /// the next run.
    pub fn snapshot(&self) -> Vec<(Id20, SocketAddr)> {
        self.buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|n| n.state() == NodeState::Good)
            .map(|n| (n.info.id, n.info.addr))
            .collect()
    }
}

/// Random id that falls in bucket `index` relative to `own_id`: same initial
/// `index` bits, next bit inverted, rest random.
fn random_in_bucket(own_id: &Id20, index: usize) -> Id20 {
    let mut bytes = Id20::random().0;
    for bit in 0..index.min(159) {
        let (byte, mask) = (bit / 8, 0x80 >> (bit % 8));
        bytes[byte] = (bytes[byte] & !mask) | (own_id.0[byte] & mask);
    }
    let (byte, mask) = (index / 8, 0x80 >> (index % 8));
    bytes[byte] ^= (bytes[byte] ^ !own_id.0[byte]) & mask;
    Id20(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: Id20, port: u16) -> NodeInfo {
        NodeInfo {
            id,
            addr: format!("1.2.3.4:{port}").parse().unwrap(),
        }
    }

    #[test]
    fn bucket_fills_and_stops_accepting() {
        let own = Id20([0; 20]);
        let mut table = RoutingTable::new(own);

        // All with the first bit set fall in the same bucket (index 0).
        for i in 0..K {
            let mut id = [0u8; 20];
            id[0] = 0x80;
            id[19] = i as u8;
            assert!(table.add(node(Id20(id), 1000 + i as u16)));
        }
        assert_eq!(table.len(), K);

        let mut extra = [0u8; 20];
        extra[0] = 0x80;
        extra[19] = 99;
        assert!(!table.add(node(Id20(extra), 9999)), "a full bucket refuses");

        // An id with a different prefix goes to another bucket and fits.
        let mut other = [0u8; 20];
        other[0] = 0x40;
        assert!(table.add(node(Id20(other), 8888)));
        assert_eq!(table.len(), K + 1);
    }

    #[test]
    fn bad_node_frees_space() {
        let mut table = RoutingTable::new(Id20([0; 20]));
        let mut ids = Vec::new();
        for i in 0..K {
            let mut id = [0u8; 20];
            id[0] = 0x80;
            id[19] = i as u8;
            ids.push(Id20(id));
            table.add(node(Id20(id), 1000 + i as u16));
        }

        // Three consecutive failures remove the node from the table.
        for _ in 0..MAX_FAILURES {
            table.mark_failed(&ids[0]);
        }
        assert_eq!(table.len(), K - 1);

        let mut new_id = [0u8; 20];
        new_id[0] = 0x80;
        new_id[19] = 77;
        assert!(table.add(node(Id20(new_id), 7777)));
    }

    #[test]
    fn rejects_self_and_garbage() {
        let own = Id20([9; 20]);
        let mut table = RoutingTable::new(own);
        assert!(!table.add(node(own, 6881)), "we ourselves do not enter");
        assert!(
            !table.add(NodeInfo {
                id: Id20::random(),
                addr: "1.2.3.4:0".parse().unwrap(),
            }),
            "port 0 is useless"
        );
        // Loopback is accepted: a DHT node on the same machine is legitimate.
        assert!(table.add(NodeInfo {
            id: Id20::random(),
            addr: "127.0.0.1:6881".parse().unwrap(),
        }));
    }

    #[test]
    fn closest_orders_by_distance() {
        let mut table = RoutingTable::new(Id20([0; 20]));
        let target = Id20([0xff; 20]);

        let near = Id20([0xfe; 20]);
        let far = Id20([0x01; 20]);
        table.add(node(far, 1));
        table.add(node(near, 2));

        let closest = table.closest(&target, 5);
        assert_eq!(closest.len(), 2);
        assert_eq!(closest[0].id, near, "the closest comes first");
        assert_eq!(table.closest(&target, 1).len(), 1, "respects the limit");
    }

    #[test]
    fn updates_address_of_known_node() {
        let mut table = RoutingTable::new(Id20([0; 20]));
        let id = Id20([0x80; 20]);
        table.add(node(id, 1111));
        table.add(node(id, 2222));
        assert_eq!(table.len(), 1, "same id does not duplicate");
        assert_eq!(table.closest(&id, 1)[0].addr.port(), 2222);
    }

    #[test]
    fn refresh_target_falls_in_the_right_bucket() {
        let own = Id20::random();
        let table = RoutingTable::new(own);
        for index in [0, 1, 7, 8, 63, 159] {
            let target = random_in_bucket(&own, index);
            assert_eq!(
                table.bucket_for(&target),
                index,
                "target for bucket {index} fell in the wrong bucket"
            );
        }
    }
}
