//! Piece and block selection.
//!
//! Three decisions determine the real download speed:
//!
//! * **Rarest-first** — fetching first the pieces that few peers have keeps the
//!   swarm healthy and avoids getting stuck at the end waiting for the single rare piece.
//! * **Finish in-progress pieces** — before opening a new piece, finish the
//!   ones that have already started. An incomplete piece is spent bandwidth that does not count yet.
//! * **Endgame** — at the end of the download, request the last blocks from several peers
//!   at the same time, so a slow peer does not hold everything up.

use crate::bitfield::Bitfield;
use crate::metainfo::Info;
use rand::RngExt;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Standard block size of the protocol. Practically every client refuses
/// requests larger than 16 KiB.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Maximum memory dedicated to partially downloaded pieces.
const MAX_PARTIAL_BYTES: u64 = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRequest {
    pub piece: u32,
    pub offset: u32,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickMode {
    /// Default: prioritizes the rarest pieces in the swarm.
    #[default]
    Rarest,
    /// Linear order, for watching/listening while downloading.
    Sequential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    Skip = 0,
    Low = 1,
    #[default]
    Normal = 2,
    High = 3,
}

/// What happened to a received block.
#[derive(Debug, Default)]
pub struct Received {
    /// Peers that also had this block requested (endgame) — send them `cancel`.
    pub cancel_from: Vec<u32>,
    /// Filled when the block completed the piece; ready for verification.
    pub piece_data: Option<Vec<u8>>,
    /// `false` when the block was a duplicate or was not part of a request.
    pub wanted: bool,
}

#[derive(Debug)]
enum BlockState {
    Pending,
    Requested { by: Vec<u32>, since: Instant },
    Done,
}

struct Partial {
    blocks: Vec<BlockState>,
    buffer: Vec<u8>,
    done: usize,
}

impl Partial {
    fn new(size: u32) -> Partial {
        let blocks = size.div_ceil(BLOCK_SIZE) as usize;
        Partial {
            blocks: (0..blocks).map(|_| BlockState::Pending).collect(),
            buffer: vec![0; size as usize],
            done: 0,
        }
    }

    fn is_complete(&self) -> bool {
        self.done == self.blocks.len()
    }
}

// -- rarity index ----------------------------------------------------------

/// Pieces with availability above this all fall into the same bucket: at that
/// point the difference between "40 peers have it" and "50 peers have it" changes nothing.
const AVAIL_BUCKETS: usize = 33;
const RANKS: usize = 3 * AVAIL_BUCKETS;

/// Wanted pieces grouped by (priority, availability).
///
/// Random sampling over all pieces seems reasonable, but misses the target:
/// with 48 draws among 64 pieces the rare piece escapes almost half the time, and
/// the proportion only gets worse in large torrents. The buckets give the exact choice without
/// scanning anything — the lowest-rank bucket is, by construction, the rarest.
struct RarityIndex {
    buckets: Vec<Vec<u32>>,
    at: HashMap<u32, (usize, usize)>,
}

impl RarityIndex {
    fn new() -> RarityIndex {
        RarityIndex {
            buckets: vec![Vec::new(); RANKS],
            at: HashMap::new(),
        }
    }

    fn rank_of(prio: Priority, avail: u32) -> Option<usize> {
        let tier = match prio {
            Priority::High => 0,
            Priority::Normal => 1,
            Priority::Low => 2,
            Priority::Skip => return None,
        };
        Some(tier * AVAIL_BUCKETS + (avail as usize).min(AVAIL_BUCKETS - 1))
    }

    fn len(&self) -> usize {
        self.at.len()
    }

    fn contains(&self, piece: u32) -> bool {
        self.at.contains_key(&piece)
    }

    fn insert(&mut self, piece: u32, rank: usize) {
        if self.at.contains_key(&piece) {
            self.relocate(piece, rank);
            return;
        }
        let bucket = &mut self.buckets[rank];
        self.at.insert(piece, (rank, bucket.len()));
        bucket.push(piece);
    }

    fn remove(&mut self, piece: u32) -> bool {
        let Some((rank, pos)) = self.at.remove(&piece) else {
            return false;
        };
        let bucket = &mut self.buckets[rank];
        bucket.swap_remove(pos);
        // The swap brought another piece to `pos`; its index needs to follow.
        if let Some(moved) = bucket.get(pos).copied() {
            self.at.insert(moved, (rank, pos));
        }
        true
    }

    fn relocate(&mut self, piece: u32, rank: usize) {
        if self.at.get(&piece).is_some_and(|(r, _)| *r == rank) {
            return;
        }
        if self.remove(piece) {
            self.insert(piece, rank);
        }
    }

    fn clear(&mut self) {
        for b in &mut self.buckets {
            b.clear();
        }
        self.at.clear();
    }

    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.buckets.iter().flatten().copied()
    }

    /// Rarest piece satisfying `accepts`, starting from the lowest-rank bucket.
    fn find(&self, accepts: impl Fn(u32) -> bool) -> Option<u32> {
        // A bucket can be huge; scanning everything on each choice would be expensive
        // when the peer has none of it. We do a limited pass over
        // all buckets and only afterwards, if nobody served, the full scan.
        const LIMIT: usize = 512;
        let mut rng = rand::rng();

        for bucket in self.buckets.iter().filter(|b| !b.is_empty()) {
            // Starting at a random point avoids all peers attacking the
            // same piece and wasting duplicate requests.
            let start = rng.random_range(0..bucket.len());
            for k in 0..bucket.len().min(LIMIT) {
                let piece = bucket[(start + k) % bucket.len()];
                if accepts(piece) {
                    return Some(piece);
                }
            }
        }
        self.buckets
            .iter()
            .filter(|b| b.len() > LIMIT)
            .flat_map(|b| b.iter().copied())
            .find(|p| accepts(*p))
    }
}

// -- picker ----------------------------------------------------------------

pub struct Picker {
    piece_length: u32,
    last_piece_size: u32,
    num_pieces: usize,
    have: Bitfield,
    /// How many connected peers announce each piece.
    availability: Vec<u32>,
    priority: Vec<Priority>,
    file_priority: Vec<Priority>,
    index: RarityIndex,
    partials: HashMap<u32, Partial>,
    max_partials: usize,
    mode: PickMode,
    cursor: u32,
    endgame: bool,
    downloaded: u64,
}

impl Picker {
    pub fn new(info: &Info, have: Bitfield) -> Picker {
        let num_pieces = info.num_pieces();
        let max_partials =
            (MAX_PARTIAL_BYTES / info.piece_length.max(1) as u64).clamp(8, 1024) as usize;

        let mut picker = Picker {
            piece_length: info.piece_length,
            last_piece_size: info.piece_size(num_pieces.saturating_sub(1)),
            num_pieces,
            have,
            availability: vec![0; num_pieces],
            priority: vec![Priority::Normal; num_pieces],
            file_priority: vec![Priority::Normal; info.files.len()],
            index: RarityIndex::new(),
            partials: HashMap::new(),
            max_partials,
            mode: PickMode::default(),
            cursor: 0,
            endgame: false,
            downloaded: 0,
        };
        picker.downloaded = picker
            .have
            .iter_set()
            .map(|i| picker.piece_size(i as u32) as u64)
            .sum();
        picker.rebuild_index();
        picker
    }

    pub fn set_mode(&mut self, mode: PickMode) {
        self.mode = mode;
    }

    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    pub fn is_complete(&self) -> bool {
        self.index.len() == 0
    }

    pub fn in_endgame(&self) -> bool {
        self.endgame
    }

    /// Bytes confirmed on disk.
    pub fn downloaded(&self) -> u64 {
        self.downloaded
    }

    /// Bytes still missing, counting only what is marked for download.
    pub fn remaining(&self) -> u64 {
        self.index
            .iter()
            .map(|i| self.piece_size(i) as u64)
            .sum::<u64>()
            .saturating_sub(self.partial_bytes())
    }

    fn partial_bytes(&self) -> u64 {
        self.partials
            .values()
            .map(|p| p.done as u64 * BLOCK_SIZE as u64)
            .sum()
    }

    pub fn piece_size(&self, index: u32) -> u32 {
        if index as usize + 1 == self.num_pieces {
            self.last_piece_size
        } else {
            self.piece_length
        }
    }

    // -- availability --------------------------------------------------------

    pub fn peer_added(&mut self, bf: &Bitfield) {
        for i in bf.iter_set() {
            if i < self.num_pieces {
                self.availability[i] += 1;
                self.reindex(i as u32);
            }
        }
    }

    pub fn peer_removed(&mut self, bf: &Bitfield) {
        for i in bf.iter_set() {
            if i < self.num_pieces {
                self.availability[i] = self.availability[i].saturating_sub(1);
                self.reindex(i as u32);
            }
        }
    }

    pub fn peer_has(&mut self, piece: u32) {
        if (piece as usize) < self.num_pieces {
            self.availability[piece as usize] += 1;
            self.reindex(piece);
        }
    }

    #[cfg(test)]
    pub fn availability(&self, piece: u32) -> u32 {
        self.availability.get(piece as usize).copied().unwrap_or(0)
    }

    /// Do we have this piece? Used to answer upload requests.
    pub fn has_piece(&self, piece: u32) -> bool {
        self.have.get(piece as usize)
    }

    fn reindex(&mut self, piece: u32) {
        if !self.index.contains(piece) {
            return;
        }
        let prio = self.priority[piece as usize];
        match RarityIndex::rank_of(prio, self.availability[piece as usize]) {
            Some(rank) => self.index.relocate(piece, rank),
            None => {
                self.index.remove(piece);
            }
        }
    }

    // -- priorities ----------------------------------------------------------

    /// Adjusts the priority of a file, propagating to the pieces it occupies.
    pub fn set_file_priority(&mut self, info: &Info, file_index: usize, prio: Priority) {
        if let Some(slot) = self.file_priority.get_mut(file_index) {
            *slot = prio;
        }
        self.recompute_piece_priorities(info);
        self.rebuild_index();
    }

    pub fn file_priority(&self, file_index: usize) -> Priority {
        self.file_priority
            .get(file_index)
            .copied()
            .unwrap_or_default()
    }

    /// Pieces on the boundary between two files inherit the higher of the priorities —
    /// you cannot download half a piece, so skipping one of them would skip part of a
    /// file the user wants.
    fn recompute_piece_priorities(&mut self, info: &Info) {
        self.priority.fill(Priority::Skip);
        for (i, f) in info.files.iter().enumerate() {
            if f.length == 0 {
                continue;
            }
            let prio = self.file_priority(i);
            let first = (f.offset / self.piece_length as u64) as usize;
            let last =
                (((f.end() - 1) / self.piece_length as u64) as usize).min(self.num_pieces - 1);
            for slot in &mut self.priority[first..=last] {
                *slot = (*slot).max(prio);
            }
        }
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for i in 0..self.num_pieces {
            if self.have.get(i) {
                continue;
            }
            if let Some(rank) = RarityIndex::rank_of(self.priority[i], self.availability[i]) {
                self.index.insert(i as u32, rank);
            }
        }
        self.partials.retain(|p, _| self.index.contains(*p));
    }

    // -- selection -----------------------------------------------------------

    /// Fills `out` with up to `want` blocks this peer can serve.
    pub fn pick(
        &mut self,
        peer: u32,
        peer_bf: &Bitfield,
        want: usize,
        out: &mut Vec<BlockRequest>,
    ) {
        if want == 0 {
            return;
        }
        // 1. Finish already started pieces that this peer has.
        let mut open_pieces: Vec<u32> = self
            .partials
            .keys()
            .copied()
            .filter(|p| peer_bf.get(*p as usize))
            .collect();
        open_pieces.sort_unstable_by_key(|p| (std::cmp::Reverse(self.priority[*p as usize]), *p));
        for piece in open_pieces {
            if out.len() >= want {
                return;
            }
            self.take_pending(piece, peer, want, out);
        }

        // 2. Open new pieces while there is room.
        while out.len() < want && self.partials.len() < self.max_partials {
            let Some(piece) = self.choose_piece(peer_bf) else {
                break;
            };
            let size = self.piece_size(piece);
            self.partials
                .entry(piece)
                .or_insert_with(|| Partial::new(size));
            let before = out.len();
            self.take_pending(piece, peer, want, out);
            if out.len() == before {
                break;
            }
        }

        // 3. Endgame: repeat pending requests on other peers.
        if out.is_empty() && self.should_endgame() {
            self.endgame = true;
            self.take_endgame(peer, peer_bf, want, out);
        }
    }

    /// The endgame is only worthwhile when what remains fits in a few requests —
    /// before that, duplicate requests only waste bandwidth.
    fn should_endgame(&self) -> bool {
        if self.index.len() == 0 {
            return false;
        }
        let remaining_blocks: usize = self
            .partials
            .values()
            .map(|p| p.blocks.len() - p.done)
            .sum();
        self.index.len() == self.partials.len() && remaining_blocks <= 64
    }

    fn take_pending(&mut self, piece: u32, peer: u32, want: usize, out: &mut Vec<BlockRequest>) {
        let piece_size = self.piece_size(piece);
        let Some(partial) = self.partials.get_mut(&piece) else {
            return;
        };
        for (i, state) in partial.blocks.iter_mut().enumerate() {
            if out.len() >= want {
                return;
            }
            if let BlockState::Pending = state {
                let offset = i as u32 * BLOCK_SIZE;
                *state = BlockState::Requested {
                    by: vec![peer],
                    since: Instant::now(),
                };
                out.push(BlockRequest {
                    piece,
                    offset,
                    length: BLOCK_SIZE.min(piece_size - offset),
                });
            }
        }
    }

    fn take_endgame(
        &mut self,
        peer: u32,
        peer_bf: &Bitfield,
        want: usize,
        out: &mut Vec<BlockRequest>,
    ) {
        let pieces: Vec<u32> = self
            .partials
            .keys()
            .copied()
            .filter(|p| peer_bf.get(*p as usize))
            .collect();
        for piece in pieces {
            let piece_size = self.piece_size(piece);
            let Some(partial) = self.partials.get_mut(&piece) else {
                continue;
            };
            for (i, state) in partial.blocks.iter_mut().enumerate() {
                if out.len() >= want {
                    return;
                }
                if let BlockState::Requested { by, .. } = state
                    && !by.contains(&peer)
                {
                    by.push(peer);
                    let offset = i as u32 * BLOCK_SIZE;
                    out.push(BlockRequest {
                        piece,
                        offset,
                        length: BLOCK_SIZE.min(piece_size - offset),
                    });
                }
            }
        }
    }

    /// Chooses the next piece to open, according to the active mode.
    fn choose_piece(&mut self, peer_bf: &Bitfield) -> Option<u32> {
        if self.mode == PickMode::Sequential {
            // Advance the cursor past resolved pieces and stop at the first one
            // still missing — even if this peer does not have it, another may.
            while (self.cursor as usize) < self.num_pieces && !self.index.contains(self.cursor) {
                self.cursor += 1;
            }
            let available = (self.cursor as usize..self.num_pieces)
                .map(|p| p as u32)
                .take(64)
                .find(|p| self.is_candidate(*p, peer_bf));
            if available.is_some() {
                return available;
            }
        }
        self.index.find(|p| self.is_candidate(p, peer_bf))
    }

    fn is_candidate(&self, piece: u32, peer_bf: &Bitfield) -> bool {
        self.index.contains(piece)
            && !self.partials.contains_key(&piece)
            && peer_bf.get(piece as usize)
    }

    // -- receiving -----------------------------------------------------------

    pub fn received(&mut self, peer: u32, req: BlockRequest, data: &[u8]) -> Received {
        let mut result = Received::default();
        if !req.offset.is_multiple_of(BLOCK_SIZE) {
            return result;
        }
        let Some(partial) = self.partials.get_mut(&req.piece) else {
            return result;
        };
        let idx = (req.offset / BLOCK_SIZE) as usize;
        let Some(state) = partial.blocks.get_mut(idx) else {
            return result;
        };
        if matches!(state, BlockState::Done) {
            return result;
        }
        let end = req.offset as usize + data.len();
        if end > partial.buffer.len() {
            return result;
        }

        if let BlockState::Requested { by, .. } = state {
            result.cancel_from = by.iter().copied().filter(|p| *p != peer).collect();
        }
        partial.buffer[req.offset as usize..end].copy_from_slice(data);
        *state = BlockState::Done;
        partial.done += 1;
        result.wanted = true;

        if partial.is_complete() {
            let partial = self.partials.remove(&req.piece).expect("just existed");
            result.piece_data = Some(partial.buffer);
        }
        result
    }

    /// The piece passed SHA-1: it leaves the wanted list forever.
    pub fn piece_verified(&mut self, piece: u32) {
        if self.have.set(piece as usize) {
            self.downloaded += self.piece_size(piece) as u64;
        }
        self.index.remove(piece);
        self.partials.remove(&piece);
        if self.mode == PickMode::Sequential && piece == self.cursor {
            self.cursor += 1;
        }
    }

    /// The piece failed SHA-1: it restarts from scratch.
    pub fn piece_failed(&mut self, piece: u32) {
        self.partials.remove(&piece);
    }

    /// Returns to the pool everything this peer had requested and did not deliver.
    pub fn release_peer(&mut self, peer: u32) {
        for partial in self.partials.values_mut() {
            for state in &mut partial.blocks {
                if let BlockState::Requested { by, .. } = state {
                    by.retain(|p| *p != peer);
                    if by.is_empty() {
                        *state = BlockState::Pending;
                    }
                }
            }
        }
    }

    /// Blocks requested too long ago become available again.
    ///
    /// Without this, a peer that accepts the request and never responds freezes the piece
    /// until disconnect — the most common way for a download to "get stuck at 99%".
    pub fn sweep_timeouts(&mut self, timeout: Duration) -> usize {
        let now = Instant::now();
        let mut released = 0;
        for partial in self.partials.values_mut() {
            for state in &mut partial.blocks {
                if let BlockState::Requested { since, .. } = state
                    && now.duration_since(*since) > timeout
                {
                    *state = BlockState::Pending;
                    released += 1;
                }
            }
        }
        released
    }

    /// Pieces currently open, for display.
    pub fn partial_pieces(&self) -> Vec<(u32, f32)> {
        let mut v: Vec<(u32, f32)> = self
            .partials
            .iter()
            .map(|(p, partial)| (*p, partial.done as f32 / partial.blocks.len() as f32))
            .collect();
        v.sort_unstable_by_key(|(p, _)| *p);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{self, Value};
    use crate::metainfo::Metainfo;
    use sha1::{Digest, Sha1};

    fn test_info(num_pieces: usize, piece_len: u32) -> Info {
        let total = num_pieces as i64 * piece_len as i64;
        let info = Value::dict([
            ("length", Value::Int(total)),
            ("name", Value::bytes("test")),
            ("piece length", Value::Int(piece_len as i64)),
            ("pieces", Value::Bytes(vec![0u8; num_pieces * 20])),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        Metainfo::from_bytes(&raw).unwrap().info
    }

    fn full_bitfield(n: usize) -> Bitfield {
        let mut bf = Bitfield::new(n);
        bf.set_all();
        bf
    }

    #[test]
    fn requests_blocks_and_completes_piece() {
        let info = test_info(4, BLOCK_SIZE * 2);
        let mut picker = Picker::new(&info, Bitfield::new(4));
        let peer_bf = full_bitfield(4);
        picker.peer_added(&peer_bf);

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 2, &mut reqs);
        assert_eq!(reqs.len(), 2, "two blocks per piece");
        assert_eq!(reqs[0].length, BLOCK_SIZE);
        assert_eq!(reqs[1].offset, BLOCK_SIZE);

        // A repeated request must not return the same blocks.
        let mut others = Vec::new();
        picker.pick(1, &peer_bf, 2, &mut others);
        assert!(others.iter().all(|r| !reqs.contains(r)));

        let data = vec![7u8; BLOCK_SIZE as usize];
        assert!(picker.received(1, reqs[0], &data).piece_data.is_none());
        let last = picker.received(1, reqs[1], &data);
        let complete = last.piece_data.expect("piece should complete");
        assert_eq!(complete.len(), (BLOCK_SIZE * 2) as usize);
        assert!(complete.iter().all(|b| *b == 7));
    }

    #[test]
    fn ignores_duplicate_or_unexpected_block() {
        let info = test_info(2, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(2));
        let peer_bf = full_bitfield(2);
        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 1, &mut reqs);

        let data = vec![1u8; BLOCK_SIZE as usize];
        assert!(picker.received(1, reqs[0], &data).wanted);
        assert!(!picker.received(1, reqs[0], &data).wanted, "duplicate");

        let stray = BlockRequest {
            piece: 1,
            offset: 999,
            length: 16,
        };
        assert!(
            !picker.received(1, stray, &data).wanted,
            "misaligned offset"
        );
    }

    #[test]
    fn prefers_rare_pieces() {
        let info = test_info(64, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(64));

        // All peers have everything, except piece 42, which only one has.
        let mut common = full_bitfield(64);
        common.clear(42);
        for _ in 0..30 {
            picker.peer_added(&common);
        }
        let rare = full_bitfield(64);
        picker.peer_added(&rare);

        for _ in 0..20 {
            let mut reqs = Vec::new();
            let mut fresh = Picker::new(&info, Bitfield::new(64));
            fresh.availability.copy_from_slice(&picker.availability);
            fresh.rebuild_index();
            fresh.pick(1, &rare, 1, &mut reqs);
            assert_eq!(reqs[0].piece, 42, "the rarest piece always comes first");
        }
    }

    #[test]
    fn rarity_tracks_departing_peers() {
        let info = test_info(8, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(8));

        let mut only_piece_3 = Bitfield::new(8);
        only_piece_3.set(3);
        let full = full_bitfield(8);
        picker.peer_added(&full);
        picker.peer_added(&only_piece_3);
        assert_eq!(picker.availability(3), 2);

        // With piece 3 being the most common, it must not be the chosen one.
        let mut reqs = Vec::new();
        picker.pick(1, &full, 1, &mut reqs);
        assert_ne!(reqs[0].piece, 3);

        picker.peer_removed(&full);
        assert_eq!(picker.availability(3), 1);
        assert_eq!(picker.availability(0), 0);
    }

    #[test]
    fn sequential_mode_follows_order() {
        let info = test_info(16, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(16));
        picker.set_mode(PickMode::Sequential);
        let peer_bf = full_bitfield(16);
        picker.peer_added(&peer_bf);

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 3, &mut reqs);
        let mut pieces: Vec<u32> = reqs.iter().map(|r| r.piece).collect();
        pieces.sort_unstable();
        assert_eq!(pieces, vec![0, 1, 2]);
    }

    #[test]
    fn timeout_and_disconnect_release_blocks() {
        let info = test_info(2, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(2));
        let peer_bf = full_bitfield(2);

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 1, &mut reqs);
        let mut empty = Vec::new();
        picker.pick(2, &peer_bf, 1, &mut empty);
        assert_ne!(empty[0], reqs[0], "peer 2 does not repeat peer 1's block");

        picker.release_peer(1);
        let mut again = Vec::new();
        picker.pick(3, &peer_bf, 4, &mut again);
        assert!(
            again.contains(&reqs[0]),
            "peer 1's block returned to the pool"
        );

        assert_eq!(picker.sweep_timeouts(Duration::ZERO), 2);
    }

    #[test]
    fn endgame_duplicates_last_blocks() {
        let info = test_info(1, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(1));
        let peer_bf = full_bitfield(1);

        let mut slow = Vec::new();
        picker.pick(1, &peer_bf, 4, &mut slow);
        assert_eq!(slow.len(), 1);
        assert!(!picker.in_endgame());

        let mut fast = Vec::new();
        picker.pick(2, &peer_bf, 4, &mut fast);
        assert_eq!(fast, slow, "in endgame the same block is requested again");
        assert!(picker.in_endgame());

        // Whoever delivers first tells the other to cancel.
        let data = vec![0u8; BLOCK_SIZE as usize];
        let r = picker.received(2, fast[0], &data);
        assert_eq!(r.cancel_from, vec![1]);
        assert!(r.piece_data.is_some());
    }

    #[test]
    fn verified_piece_leaves_queue() {
        let info = test_info(3, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(3));
        assert_eq!(picker.remaining(), 3 * BLOCK_SIZE as u64);
        picker.piece_verified(1);
        assert!(picker.has_piece(1));
        assert_eq!(picker.downloaded(), BLOCK_SIZE as u64);
        assert_eq!(picker.remaining(), 2 * BLOCK_SIZE as u64);

        picker.piece_verified(0);
        picker.piece_verified(2);
        assert!(picker.is_complete());
    }

    #[test]
    fn skipped_file_is_excluded_from_download() {
        let file = |len: i64, name: &str| {
            Value::dict([
                ("length", Value::Int(len)),
                ("path", Value::List(vec![Value::bytes(name)])),
            ])
        };
        let piece_len = BLOCK_SIZE as i64;
        let mut pieces = Vec::new();
        pieces.extend_from_slice(&Sha1::digest(vec![0u8; piece_len as usize]));
        pieces.extend_from_slice(&Sha1::digest(vec![0u8; piece_len as usize]));
        let info = Value::dict([
            (
                "files",
                Value::List(vec![file(piece_len, "a"), file(piece_len, "b")]),
            ),
            ("name", Value::bytes("two")),
            ("piece length", Value::Int(piece_len)),
            ("pieces", Value::Bytes(pieces)),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        let info = Metainfo::from_bytes(&raw).unwrap().info;

        let mut picker = Picker::new(&info, Bitfield::new(2));
        picker.set_file_priority(&info, 1, Priority::Skip);
        assert_eq!(
            picker.remaining(),
            piece_len as u64,
            "only the wanted file counts"
        );

        let peer_bf = full_bitfield(2);
        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 8, &mut reqs);
        assert!(
            reqs.iter().all(|r| r.piece == 0),
            "piece of the skipped file is not requested"
        );
        picker.piece_verified(0);
        assert!(picker.is_complete(), "only the wanted file counts");
    }

    #[test]
    fn high_priority_jumps_queue() {
        let file = |len: i64, name: &str| {
            Value::dict([
                ("length", Value::Int(len)),
                ("path", Value::List(vec![Value::bytes(name)])),
            ])
        };
        let piece_len = BLOCK_SIZE as i64;
        let info = Value::dict([
            (
                "files",
                Value::List((0..4).map(|i| file(piece_len, &format!("f{i}"))).collect()),
            ),
            ("name", Value::bytes("four")),
            ("piece length", Value::Int(piece_len)),
            ("pieces", Value::Bytes(vec![0u8; 4 * 20])),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        let info = Metainfo::from_bytes(&raw).unwrap().info;

        let mut picker = Picker::new(&info, Bitfield::new(4));
        let peer_bf = full_bitfield(4);
        picker.peer_added(&peer_bf);
        // The last file is the most common in the swarm, but the user wants it now.
        picker.set_file_priority(&info, 3, Priority::High);
        for _ in 0..10 {
            picker.peer_has(3);
        }

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 1, &mut reqs);
        assert_eq!(reqs[0].piece, 3, "priority beats rarity");
    }
}
