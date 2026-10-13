//! HTTP/HTTPS announce (BEP-3, with compact peers from BEP-23).

use super::{AnnounceRequest, AnnounceResponse, Event};
use crate::bencode;
use crate::compact;
use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// `info_hash` and `peer_id` are raw bytes, not text: everything that is not
/// *unreserved* (RFC 3986) must be percent-encoded.
const RAW: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub fn build_url(base: &str, req: &AnnounceRequest) -> String {
    let mut url = String::with_capacity(base.len() + 256);
    url.push_str(base);
    url.push(if base.contains('?') { '&' } else { '?' });

    url.push_str("info_hash=");
    url.extend(percent_encode(req.info_hash.as_bytes(), RAW));
    url.push_str("&peer_id=");
    url.extend(percent_encode(req.peer_id.as_bytes(), RAW));

    url.push_str(&format!(
        "&port={}&uploaded={}&downloaded={}&left={}&compact=1&no_peer_id=1&numwant={}&key={:08x}",
        req.port, req.uploaded, req.downloaded, req.left, req.num_want, req.key
    ));
    if let Some(event) = req.event.as_str() {
        url.push_str("&event=");
        url.push_str(event);
    }
    if let Some(id) = &req.tracker_id {
        url.push_str("&trackerid=");
        url.extend(percent_encode(id.as_bytes(), RAW));
    }
    url
}

pub async fn announce(
    client: &reqwest::Client,
    base: &str,
    req: &AnnounceRequest,
) -> Result<AnnounceResponse> {
    let url = build_url(base, req);
    let response = client
        .get(&url)
        .send()
        .await
        .context("failed to contact the tracker")?;
    let status = response.status();
    let body = response.bytes().await.context("interrupted response")?;
    if !status.is_success() {
        bail!("tracker responded with HTTP {}", status.as_u16());
    }
    parse_response(&body)
}

pub fn parse_response(body: &[u8]) -> Result<AnnounceResponse> {
    let root = bencode::decode(body)
        .context("tracker response is not valid bencode")?
        .value;

    if let Some(reason) = root.str_at("failure reason") {
        bail!("tracker rejected: {reason}");
    }

    let mut peers = match root.get("peers") {
        // Compact format (BEP-23): 6 bytes per peer.
        Some(bencode::Value::Bytes(b)) => compact::parse_v4(b),
        // Legacy format: list of dictionaries.
        Some(bencode::Value::List(list)) => list
            .iter()
            .filter_map(|p| {
                let ip: IpAddr = p.str_at("ip")?.parse().ok()?;
                let port = u16::try_from(p.int_at("port")?).ok()?;
                Some(SocketAddr::new(ip, port))
            })
            .filter(compact::is_usable)
            .collect(),
        _ => Vec::new(),
    };
    if let Some(b) = root.bytes_at("peers6") {
        peers.extend(compact::parse_v6(b));
    }

    let seconds = |key: &str| root.int_at(key).filter(|v| *v > 0).map(|v| v as u64);
    Ok(AnnounceResponse {
        // A tracker without `interval` is rare but does happen; 30 min is the de facto default.
        interval: Duration::from_secs(seconds("interval").unwrap_or(1800).clamp(60, 3 * 3600)),
        min_interval: seconds("min interval").map(Duration::from_secs),
        peers,
        seeders: root.int_at("complete").unwrap_or(0).max(0) as u32,
        leechers: root.int_at("incomplete").unwrap_or(0).max(0) as u32,
        tracker_id: root.str_at("tracker id"),
        warning: root.str_at("warning message"),
    })
}

impl Event {
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Event::Started => Some("started"),
            Event::Stopped => Some("stopped"),
            Event::Completed => Some("completed"),
            Event::Periodic => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::Value;
    use crate::id::Id20;

    fn request() -> AnnounceRequest {
        AnnounceRequest {
            info_hash: Id20([0x12; 20]),
            peer_id: Id20(*b"-TR0001-abcdefghijkl"),
            port: 6881,
            uploaded: 10,
            downloaded: 20,
            left: 30,
            event: Event::Started,
            num_want: 200,
            key: 0xdead_beef,
            tracker_id: None,
        }
    }

    #[test]
    fn builds_query_with_escaped_bytes() {
        let url = build_url("http://t.example/announce", &request());
        assert!(url.starts_with("http://t.example/announce?info_hash=%12%12"));
        assert!(
            url.contains("&peer_id=-TR0001-abcdefghijkl"),
            "unreserved passes through raw"
        );
        assert!(url.contains("&port=6881&uploaded=10&downloaded=20&left=30"));
        assert!(url.contains("&compact=1"));
        assert!(url.contains("&event=started"));
        assert!(url.contains("&key=deadbeef"));

        // An announce that already has a query gets `&` instead of `?`.
        let with_query = build_url("http://t.example/ann?x=1", &request());
        assert!(with_query.contains("?x=1&info_hash="));

        // The periodic announce does not send `event`.
        let mut periodic = request();
        periodic.event = Event::Periodic;
        assert!(!build_url("http://t/a", &periodic).contains("event="));
    }

    #[test]
    fn parses_compact_response() {
        let body = bencode::encode(&Value::dict([
            ("interval", Value::Int(900)),
            ("min interval", Value::Int(300)),
            ("complete", Value::Int(5)),
            ("incomplete", Value::Int(7)),
            (
                "peers",
                Value::Bytes(vec![192, 168, 0, 1, 0x1a, 0xe1, 10, 1, 2, 3, 0x1a, 0xe2]),
            ),
        ]));
        let r = parse_response(&body).unwrap();
        assert_eq!(r.interval, Duration::from_secs(900));
        assert_eq!(r.min_interval, Some(Duration::from_secs(300)));
        assert_eq!(r.seeders, 5);
        assert_eq!(r.leechers, 7);
        assert_eq!(r.peers.len(), 2);
        assert_eq!(r.peers[0].to_string(), "192.168.0.1:6881");
    }

    #[test]
    fn parses_dictionary_list_response() {
        let peer = |ip: &str, port: i64| {
            Value::dict([("ip", Value::bytes(ip)), ("port", Value::Int(port))])
        };
        let body = bencode::encode(&Value::dict([
            ("interval", Value::Int(600)),
            (
                "peers",
                Value::List(vec![peer("1.2.3.4", 51413), peer("5.6.7.8", 0)]),
            ),
        ]));
        let r = parse_response(&body).unwrap();
        assert_eq!(r.peers.len(), 1, "port 0 is discarded");
        assert_eq!(r.peers[0].to_string(), "1.2.3.4:51413");
    }

    #[test]
    fn propagates_tracker_failure() {
        let body = bencode::encode(&Value::dict([(
            "failure reason",
            Value::bytes("torrent not registered"),
        )]));
        let error = parse_response(&body).unwrap_err().to_string();
        assert!(error.contains("torrent not registered"));
        assert!(parse_response(b"not-bencode").is_err());
    }

    #[test]
    fn absurd_interval_is_clamped() {
        let body = bencode::encode(&Value::dict([("interval", Value::Int(999_999))]));
        assert_eq!(parse_response(&body).unwrap().interval.as_secs(), 3 * 3600);
        let body = bencode::encode(&Value::dict([("interval", Value::Int(1))]));
        assert_eq!(parse_response(&body).unwrap().interval.as_secs(), 60);
    }
}
