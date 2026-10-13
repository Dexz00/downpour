//! Compact address format — 6 bytes per IPv4 peer, 18 per IPv6.
//! Trackers (BEP-23), DHT (BEP-5) and PEX (BEP-11) all use the same layout.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub fn parse_v4(buf: &[u8]) -> Vec<SocketAddr> {
    buf.as_chunks::<6>()
        .0
        .iter()
        .map(|c| {
            let ip = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = u16::from_be_bytes([c[4], c[5]]);
            SocketAddr::new(IpAddr::V4(ip), port)
        })
        .filter(is_usable)
        .collect()
}

pub fn parse_v6(buf: &[u8]) -> Vec<SocketAddr> {
    buf.as_chunks::<18>()
        .0
        .iter()
        .map(|c| {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&c[..16]);
            let port = u16::from_be_bytes([c[16], c[17]]);
            SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port)
        })
        .filter(is_usable)
        .collect()
}

pub fn encode(addrs: &[SocketAddr]) -> (Vec<u8>, Vec<u8>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for addr in addrs {
        match addr.ip() {
            IpAddr::V4(ip) => {
                v4.extend_from_slice(&ip.octets());
                v4.extend_from_slice(&addr.port().to_be_bytes());
            }
            IpAddr::V6(ip) => {
                v6.extend_from_slice(&ip.octets());
                v6.extend_from_slice(&addr.port().to_be_bytes());
            }
        }
    }
    (v4, v6)
}

/// Discards what is never a valid peer address: zero port, unspecified
/// address, broadcast and multicast. Trackers return garbage frequently.
pub fn is_usable(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast(),
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// Is it worth dialing this address?
///
/// Stricter than [`is_usable`]: excludes loopback and link-local. It is a
/// *connection* decision, not a parsing one — a peer at 127.0.0.1 announced
/// by a remote tracker is useless, but the same address coming from a DHT
/// node running on the same machine is legitimate, and discarding it during
/// decoding would prevent local tests.
pub fn is_routable(addr: &SocketAddr) -> bool {
    if !is_usable(addr) {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => !ip.is_loopback() && !ip.is_link_local(),
        IpAddr::V6(ip) => !ip.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_v4_peers() {
        let buf = [192, 168, 1, 5, 0x1a, 0xe1, 10, 0, 0, 1, 0x00, 0x50];
        let peers = parse_v4(&buf);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].to_string(), "192.168.1.5:6881");
        assert_eq!(peers[1].port(), 80);
    }

    #[test]
    fn discards_invalid_entries() {
        // Port 0 and 0.0.0.0 are never peer addresses.
        let buf = [1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0x1a, 0xe1];
        assert!(parse_v4(&buf).is_empty());
        assert_eq!(
            parse_v4(&[1, 2, 3]).len(),
            0,
            "incomplete remainder is ignored"
        );
    }

    #[test]
    fn loopback_passes_parsing_but_is_not_dialable() {
        let local: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        assert!(is_usable(&local), "decoding is not deciding");
        assert!(!is_routable(&local));

        assert!(is_routable(&"8.8.8.8:6881".parse().unwrap()));
        assert!(
            !is_routable(&"169.254.1.1:6881".parse().unwrap()),
            "link-local"
        );
        assert!(!is_routable(&"[::1]:6881".parse().unwrap()));
    }

    #[test]
    fn round_trip_v4_and_v6() {
        let addrs: Vec<SocketAddr> = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "[2001:db8::1]:51413".parse().unwrap(),
        ];
        let (v4, v6) = encode(&addrs);
        assert_eq!(parse_v4(&v4), vec![addrs[0]]);
        assert_eq!(parse_v6(&v6), vec![addrs[1]]);
    }
}
