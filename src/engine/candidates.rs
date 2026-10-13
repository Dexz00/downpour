//! Candidate address queue with time-based quarantine.

use super::{MAX_CANDIDATES, RESERVATION};
use crate::compact;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::Instant;

/// Queue of addresses to dial, with a time-based quarantine.
///
/// The quarantine replaces a simple "already seen this address": marking
/// addresses as seen forever seems to avoid duplicates, but in practice it
/// burns the swarm — every peer that drops stays banned, and after pausing
/// and resuming nobody is left to dial, because the tracker returns exactly
/// the same addresses as before.
#[derive(Default)]
pub(super) struct Candidates {
    queue: VecDeque<SocketAddr>,
    /// Address -> instant from which it may return to the queue.
    quarantine: HashMap<SocketAddr, Instant>,
}

impl Candidates {
    /// Enqueues the address if it is dialable and not quarantined.
    pub(super) fn offer(&mut self, addr: SocketAddr) -> bool {
        if !compact::is_routable(&addr) {
            return false;
        }
        let now = Instant::now();
        if self.quarantine.get(&addr).is_some_and(|t| *t > now) {
            return false;
        }
        // While queued (or connected) the address stays reserved;
        // that is what prevents duplicates.
        self.quarantine.insert(addr, now + RESERVATION);
        self.queue.push_back(addr);
        if self.queue.len() > MAX_CANDIDATES
            && let Some(old) = self.queue.pop_front()
        {
            self.quarantine.remove(&old);
        }
        true
    }

    pub(super) fn next(&mut self) -> Option<SocketAddr> {
        self.queue.pop_front()
    }

    /// The peer dropped: releases the address for a later retry.
    pub(super) fn schedule_retry(&mut self, addr: SocketAddr, delay: Duration) {
        self.quarantine.insert(addr, Instant::now() + delay);
    }

    /// Releases everything now — used when pausing, so resuming dials again
    /// immediately instead of waiting for the next announce.
    pub(super) fn release_all(&mut self) {
        self.queue.clear();
        let now = Instant::now();
        for until in self.quarantine.values_mut() {
            *until = now;
        }
    }

    /// Puts addresses whose quarantine has expired back into the queue.
    pub(super) fn requeue_expired(&mut self) {
        let now = Instant::now();
        let released: Vec<SocketAddr> = self
            .quarantine
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(addr, _)| *addr)
            .take(MAX_CANDIDATES)
            .collect();
        for addr in released {
            self.offer(addr);
        }
    }

    pub(super) fn len(&self) -> usize {
        self.queue.len()
    }
}
