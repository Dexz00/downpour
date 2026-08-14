//! Formato compacto de endereços — 6 bytes por peer IPv4, 18 por IPv6.
//! Trackers (BEP-23), DHT (BEP-5) e PEX (BEP-11) usam todos o mesmo layout.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub fn parse_v4(buf: &[u8]) -> Vec<SocketAddr> {
    buf.chunks_exact(6)
        .map(|c| {
            let ip = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = u16::from_be_bytes([c[4], c[5]]);
            SocketAddr::new(IpAddr::V4(ip), port)
        })
        .filter(is_usable)
        .collect()
}

pub fn parse_v6(buf: &[u8]) -> Vec<SocketAddr> {
    buf.chunks_exact(18)
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

/// Descarta o que nunca é um endereço de peer válido: porta zero, endereço não
/// especificado, broadcast e multicast. Trackers devolvem lixo com frequência.
pub fn is_usable(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast(),
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// Vale a pena discar para este endereço?
///
/// Mais estrito que [`is_usable`]: exclui loopback e link-local. É uma decisão
/// de *conexão*, não de parsing — um peer em 127.0.0.1 anunciado por um tracker
/// remoto é inútil, mas o mesmo endereço vindo de um nó DHT rodando na própria
/// máquina é legítimo, e descartá-lo na decodificação impediria testes locais.
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
    fn decodifica_peers_v4() {
        let buf = [192, 168, 1, 5, 0x1a, 0xe1, 10, 0, 0, 1, 0x00, 0x50];
        let peers = parse_v4(&buf);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].to_string(), "192.168.1.5:6881");
        assert_eq!(peers[1].port(), 80);
    }

    #[test]
    fn descarta_entradas_invalidas() {
        // Porta 0 e 0.0.0.0 nunca são endereços de peer.
        let buf = [1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0x1a, 0xe1];
        assert!(parse_v4(&buf).is_empty());
        assert_eq!(parse_v4(&[1, 2, 3]).len(), 0, "resto incompleto é ignorado");
    }

    #[test]
    fn loopback_passa_no_parsing_mas_nao_e_discavel() {
        let local: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        assert!(is_usable(&local), "decodificar não é decidir");
        assert!(!is_routable(&local));

        assert!(is_routable(&"8.8.8.8:6881".parse().unwrap()));
        assert!(!is_routable(&"169.254.1.1:6881".parse().unwrap()), "link-local");
        assert!(!is_routable(&"[::1]:6881".parse().unwrap()));
    }

    #[test]
    fn round_trip_v4_e_v6() {
        let addrs: Vec<SocketAddr> = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "[2001:db8::1]:51413".parse().unwrap(),
        ];
        let (v4, v6) = encode(&addrs);
        assert_eq!(parse_v4(&v4), vec![addrs[0]]);
        assert_eq!(parse_v6(&v6), vec![addrs[1]]);
    }
}
