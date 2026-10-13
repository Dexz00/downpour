//! Optional diagnostics.
//!
//! Enabled by `DOWNPOUR_LOG=1` (or `=peer`, `=dht`, `=tracker`, `=state` to
//! filter by topic). Writes to stderr, which under the TUI can be redirected
//! to a file: `downpour x.torrent 2> log.txt`.

use std::sync::OnceLock;

static FILTER: OnceLock<Option<String>> = OnceLock::new();

fn filter() -> &'static Option<String> {
    FILTER.get_or_init(|| {
        // `TORRRA_LOG` is the pre-rename name; keep honoring it.
        std::env::var("DOWNPOUR_LOG")
            .or_else(|_| std::env::var("TORRRA_LOG"))
            .ok()
            .filter(|v| v != "0")
    })
}

/// Should this topic be logged?
pub fn active(topic: &str) -> bool {
    match filter() {
        Some(f) => {
            f == "1"
                || f == "all"
                || f.split(',').any(|part| {
                    let part = part.trim();
                    // Legacy Portuguese topic name, kept so old scripts keep working.
                    part == topic || (topic == "state" && part == "estado")
                })
        }
        None => false,
    }
}

/// `log!("peer", "handshake failed: {e}")`
macro_rules! log {
    ($topic:expr, $($arg:tt)*) => {
        if $crate::log::active($topic) {
            eprintln!("[{}] {}", $topic, format_args!($($arg)*));
        }
    };
}

pub(crate) use log;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_is_off_by_default() {
        // The test runs without the variable set in a normal environment.
        if std::env::var("DOWNPOUR_LOG").is_err() {
            assert!(!active("peer"));
        }
    }
}
