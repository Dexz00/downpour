//! Transfer rate measurement.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Resolution of the sliding window buckets.
const BUCKET: Duration = Duration::from_millis(500);
/// How many buckets the window keeps (8 seconds).
const BUCKETS: usize = 16;

/// Rate in bytes per second, measured over a sliding window.
///
/// An average over the entire download hides exactly what matters: whether the
/// speed dropped *right now*. The short window responds quickly without
/// jittering with every received block.
pub struct RateMeter {
    buckets: [u64; BUCKETS],
    current: usize,
    rollover: Instant,
    total: u64,
}

impl Default for RateMeter {
    fn default() -> Self {
        RateMeter::new()
    }
}

impl RateMeter {
    pub fn new() -> RateMeter {
        RateMeter {
            buckets: [0; BUCKETS],
            current: 0,
            rollover: Instant::now(),
            total: 0,
        }
    }

    pub fn add(&mut self, bytes: u64) {
        self.rotate();
        self.buckets[self.current] += bytes;
        self.total += bytes;
    }

    /// Bytes per second over the recent window.
    pub fn rate(&mut self) -> f64 {
        self.rotate();
        let sum: u64 = self.buckets.iter().sum();
        sum as f64 / (BUCKETS as f64 * BUCKET.as_secs_f64())
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    fn rotate(&mut self) {
        let elapsed = self.rollover.elapsed();
        if elapsed < BUCKET {
            return;
        }
        let steps = (elapsed.as_nanos() / BUCKET.as_nanos()) as usize;
        if steps >= BUCKETS {
            self.buckets = [0; BUCKETS];
            self.current = 0;
        } else {
            for _ in 0..steps {
                self.current = (self.current + 1) % BUCKETS;
                self.buckets[self.current] = 0;
            }
        }
        self.rollover += BUCKET * steps as u32;
    }
}

/// Short time series, for drawing the speed graph.
pub struct History {
    samples: VecDeque<f64>,
    capacity: usize,
}

impl History {
    pub fn new(capacity: usize) -> History {
        History {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, value: f64) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    pub fn iter(&self) -> impl Iterator<Item = f64> + '_ {
        self.samples.iter().copied()
    }
}

/// Formats bytes in a readable unit: `1.4 MiB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value < 10.0 {
        format!("{value:.2} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats a rate: `2.3 MiB/s`.
pub fn rate(bytes_per_second: f64) -> String {
    if bytes_per_second < 1.0 {
        return "—".to_string();
    }
    format!("{}/s", bytes(bytes_per_second as u64))
}

/// Formats a duration as remaining time: `2h 14m`.
pub fn eta(seconds: Option<u64>) -> String {
    let Some(s) = seconds else {
        return "∞".to_string();
    };
    match s {
        0 => "now".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s if s < 86400 * 99 => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
        _ => "∞".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_considers_entire_window() {
        let mut m = RateMeter::new();
        m.add(8 * 1024);
        // 8 KiB over an 8-second window = 1 KiB/s.
        assert!((m.rate() - 1024.0).abs() < 1.0, "got {}", m.rate());
        assert_eq!(m.total(), 8 * 1024);

        m.add(8 * 1024);
        assert!((m.rate() - 2048.0).abs() < 1.0);
        assert_eq!(m.total(), 16 * 1024);
    }

    #[test]
    fn window_empties_over_time() {
        let mut m = RateMeter::new();
        m.add(1000);
        assert!(m.rate() > 0.0);
        // Push the rollover into the past beyond the entire window.
        m.rollover = Instant::now() - BUCKET * (BUCKETS as u32 + 2);
        assert_eq!(m.rate(), 0.0, "old data leaves the window");
        assert_eq!(m.total(), 1000, "the accumulated total remains");
    }

    #[test]
    fn history_respects_capacity() {
        let mut h = History::new(3);
        for v in [1.0, 2.0, 3.0, 4.0] {
            h.push(v);
        }
        assert_eq!(h.iter().collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn formats_sizes_and_times() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(20 * 1024), "20.0 KiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");

        assert_eq!(rate(0.5), "—");
        assert_eq!(rate(2.0 * 1024.0 * 1024.0), "2.00 MiB/s");

        assert_eq!(eta(None), "∞");
        assert_eq!(eta(Some(0)), "now");
        assert_eq!(eta(Some(45)), "45s");
        assert_eq!(eta(Some(90)), "1m 30s");
        assert_eq!(eta(Some(3700)), "1h 1m");
        assert_eq!(eta(Some(90000)), "1d 1h");
    }
}
