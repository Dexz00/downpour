//! UDP announce (BEP-15).
//!
//! The protocol has two stages: `connect` returns a `connection_id` that is
//! valid for one minute, and `announce` uses that id. Since UDP does not
//! guarantee delivery, each stage is resent with backoff until the response arrives.

use super::{AnnounceRequest, AnnounceResponse, Event};
use crate::compact;
use anyhow::{Context, Result, bail};
use rand::RngExt;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{UdpSocket, lookup_host};
use tokio::time::timeout;

/// Magic constant that opens the conversation (fixed by BEP-15).
const PROTOCOL_ID: u64 = 0x0000_0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// Attempts per stage, with the wait time for each one.
const ATTEMPTS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

pub fn event_code(event: Event) -> u32 {
    match event {
        Event::Periodic => 0,
        Event::Completed => 1,
        Event::Started => 2,
        Event::Stopped => 3,
    }
}

pub fn build_connect(transaction: u32) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    buf[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    buf[12..16].copy_from_slice(&transaction.to_be_bytes());
    buf
}

pub fn parse_connect(buf: &[u8], transaction: u32) -> Result<u64> {
    check_header(buf, ACTION_CONNECT, transaction)?;
    if buf.len() < 16 {
        bail!("truncated connect response");
    }
    Ok(u64::from_be_bytes(buf[8..16].try_into().unwrap()))
}

pub fn build_announce(connection_id: u64, transaction: u32, req: &AnnounceRequest) -> [u8; 98] {
    let mut buf = [0u8; 98];
    buf[..8].copy_from_slice(&connection_id.to_be_bytes());
    buf[8..12].copy_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    buf[12..16].copy_from_slice(&transaction.to_be_bytes());
    buf[16..36].copy_from_slice(req.info_hash.as_bytes());
    buf[36..56].copy_from_slice(req.peer_id.as_bytes());
    buf[56..64].copy_from_slice(&req.downloaded.to_be_bytes());
    buf[64..72].copy_from_slice(&req.left.to_be_bytes());
    buf[72..80].copy_from_slice(&req.uploaded.to_be_bytes());
    buf[80..84].copy_from_slice(&event_code(req.event).to_be_bytes());
    // IP 0 = "use the packet's source address".
    buf[84..88].copy_from_slice(&0u32.to_be_bytes());
    buf[88..92].copy_from_slice(&req.key.to_be_bytes());
    buf[92..96].copy_from_slice(&req.num_want.to_be_bytes());
    buf[96..98].copy_from_slice(&req.port.to_be_bytes());
    buf
}

pub fn parse_announce(buf: &[u8], transaction: u32) -> Result<AnnounceResponse> {
    check_header(buf, ACTION_ANNOUNCE, transaction)?;
    if buf.len() < 20 {
        bail!("truncated announce response");
    }
    let u32_at = |i: usize| u32::from_be_bytes(buf[i..i + 4].try_into().unwrap());
    Ok(AnnounceResponse {
        interval: Duration::from_secs((u32_at(8).max(1) as u64).clamp(60, 3 * 3600)),
        min_interval: None,
        leechers: u32_at(12),
        seeders: u32_at(16),
        peers: compact::parse_v4(&buf[20..]),
        tracker_id: None,
        warning: None,
    })
}

fn check_header(buf: &[u8], expected: u32, transaction: u32) -> Result<()> {
    if buf.len() < 8 {
        bail!("UDP response too short ({} bytes)", buf.len());
    }
    let action = u32::from_be_bytes(buf[..4].try_into().unwrap());
    let tid = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if tid != transaction {
        bail!("UDP response with mismatched transaction id");
    }
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&buf[8..]);
        bail!("tracker rejected: {}", msg.trim());
    }
    if action != expected {
        bail!("tracker responded with action={action}, expected {expected}");
    }
    Ok(())
}

/// `udp://host:port/announce` -> `host:port`.
pub fn parse_endpoint(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("udp://")
        .context("UDP tracker URL missing `udp://` scheme")?;
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    if authority.is_empty() {
        bail!("UDP tracker URL missing host");
    }
    // Without an explicit port, the protocol default is 80.
    let has_port = match authority.rfind(':') {
        Some(i) => !authority[i..].contains(']'),
        None => false,
    };
    Ok(if has_port {
        authority.to_string()
    } else {
        format!("{authority}:80")
    })
}

pub async fn announce(url: &str, req: &AnnounceRequest) -> Result<AnnounceResponse> {
    let endpoint = parse_endpoint(url)?;
    let target: SocketAddr = lookup_host(&endpoint)
        .await
        .with_context(|| format!("could not resolve `{endpoint}`"))?
        .next()
        .with_context(|| format!("`{endpoint}` did not resolve to any address"))?;

    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(target).await?;

    let tid = rand::rng().random::<u32>();
    let response = exchange(&socket, &build_connect(tid), 16).await?;
    let connection_id = parse_connect(&response, tid)?;

    let tid = rand::rng().random::<u32>();
    let packet = build_announce(connection_id, tid, req);
    let response = exchange(&socket, &packet, 20).await?;
    parse_announce(&response, tid)
}

/// Sends and waits for a response, resending while attempts remain.
async fn exchange(socket: &UdpSocket, packet: &[u8], minimum: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let mut last_error = None;

    for wait in ATTEMPTS {
        socket.send(packet).await?;
        match timeout(wait, socket.recv(&mut buf)).await {
            Ok(Ok(n)) if n >= minimum => {
                buf.truncate(n);
                return Ok(buf);
            }
            Ok(Ok(n)) => last_error = Some(format!("{n}-byte response")),
            Ok(Err(e)) => last_error = Some(e.to_string()),
            Err(_) => last_error = Some("no response".to_string()),
        }
    }
    bail!(
        "UDP tracker did not respond ({})",
        last_error.unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Id20;

    fn request() -> AnnounceRequest {
        AnnounceRequest {
            info_hash: Id20([0xab; 20]),
            peer_id: Id20([0xcd; 20]),
            port: 51413,
            uploaded: 1000,
            downloaded: 2000,
            left: 3000,
            event: Event::Started,
            num_want: -1,
            key: 0x1234_5678,
            tracker_id: None,
        }
    }

    #[test]
    fn builds_connect_packet() {
        let buf = build_connect(0xaabb_ccdd);
        assert_eq!(&buf[..8], &PROTOCOL_ID.to_be_bytes());
        assert_eq!(&buf[8..12], &[0, 0, 0, 0]);
        assert_eq!(&buf[12..16], &[0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn builds_announce_packet() {
        let buf = build_announce(0x1122_3344_5566_7788, 42, &request());
        assert_eq!(&buf[..8], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(u32::from_be_bytes(buf[8..12].try_into().unwrap()), 1);
        assert_eq!(&buf[16..36], &[0xab; 20]);
        assert_eq!(&buf[36..56], &[0xcd; 20]);
        assert_eq!(u64::from_be_bytes(buf[56..64].try_into().unwrap()), 2000);
        assert_eq!(u64::from_be_bytes(buf[64..72].try_into().unwrap()), 3000);
        assert_eq!(u64::from_be_bytes(buf[72..80].try_into().unwrap()), 1000);
        assert_eq!(u32::from_be_bytes(buf[80..84].try_into().unwrap()), 2);
        assert_eq!(i32::from_be_bytes(buf[92..96].try_into().unwrap()), -1);
        assert_eq!(u16::from_be_bytes(buf[96..98].try_into().unwrap()), 51413);
    }

    #[test]
    fn parses_connect_and_announce_responses() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        resp.extend_from_slice(&7u32.to_be_bytes());
        resp.extend_from_slice(&0xfeed_face_dead_beefu64.to_be_bytes());
        assert_eq!(parse_connect(&resp, 7).unwrap(), 0xfeed_face_dead_beef);
        assert!(parse_connect(&resp, 8).is_err(), "wrong transaction id");

        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        resp.extend_from_slice(&9u32.to_be_bytes());
        resp.extend_from_slice(&1800u32.to_be_bytes());
        resp.extend_from_slice(&3u32.to_be_bytes());
        resp.extend_from_slice(&11u32.to_be_bytes());
        resp.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1]);
        let r = parse_announce(&resp, 9).unwrap();
        assert_eq!(r.interval, Duration::from_secs(1800));
        assert_eq!((r.leechers, r.seeders), (3, 11));
        assert_eq!(r.peers[0].to_string(), "1.2.3.4:6881");
    }

    #[test]
    fn propagates_tracker_error() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_ERROR.to_be_bytes());
        resp.extend_from_slice(&5u32.to_be_bytes());
        resp.extend_from_slice(b"torrent not allowed");
        let error = parse_announce(&resp, 5).unwrap_err().to_string();
        assert!(error.contains("torrent not allowed"));
    }

    #[test]
    fn extracts_endpoint_from_url() {
        assert_eq!(
            parse_endpoint("udp://t.br:6969/announce").unwrap(),
            "t.br:6969"
        );
        assert_eq!(parse_endpoint("udp://t.br/announce").unwrap(), "t.br:80");
        assert_eq!(parse_endpoint("udp://t.br:1337").unwrap(), "t.br:1337");
        assert_eq!(parse_endpoint("udp://[::1]:80/ann").unwrap(), "[::1]:80");
        assert_eq!(parse_endpoint("udp://[::1]/ann").unwrap(), "[::1]:80");
        assert!(parse_endpoint("http://t.br/announce").is_err());
    }
}
