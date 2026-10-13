//! Free-standing helpers of the engine: per-file progress and queue sizing.

use super::{Download, QUEUE_MAX, QUEUE_MIN};
use crate::peer::Slot;
use crate::picker::BLOCK_SIZE;

/// How much of a file is already on disk, counting only complete pieces.
pub(super) fn file_downloaded_bytes(d: &Download, offset: u64, length: u64) -> u64 {
    if length == 0 {
        return 0;
    }
    let piece_len = d.info.piece_length as u64;
    let first = offset / piece_len;
    let last = (offset + length - 1) / piece_len;
    let mut total = 0;
    for piece in first..=last {
        if d.picker.has_piece(piece as u32) {
            let start = (piece * piece_len).max(offset);
            let end = ((piece + 1) * piece_len).min(offset + length);
            total += end.saturating_sub(start);
        }
    }
    total
}

/// How many requests to keep in flight for a peer delivering `rate` bytes/s.
///
/// The idea is to cover the round-trip time: with too little in flight the peer
/// idles between responses, and with too much the requests age in its queue —
/// and we lose the chance to ask a faster peer for the piece.
pub(super) fn ideal_depth(rate: f64, current: usize) -> usize {
    // ~2 seconds of data in transit.
    let target = ((rate * 2.0) / BLOCK_SIZE as f64).ceil() as usize;
    let target = target.clamp(QUEUE_MIN, QUEUE_MAX);
    // Rising slowly and falling fast avoids oscillation.
    if target > current {
        (current + 2).min(target)
    } else {
        target
    }
}

/// Picks the optimistic unchoke peer in rotation.
pub(super) fn pick_optimistic(candidates: &[Slot], cycle: u32) -> Option<Slot> {
    if candidates.is_empty() {
        return None;
    }
    Some(candidates[(cycle as usize) % candidates.len()])
}
