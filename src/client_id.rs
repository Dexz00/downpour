//! Identification of the software on the other side, from the peer id.
//!
//! There is no mandatory field for this in the protocol: the client name is
//! a convention over the 20 bytes of the peer id. Knowing who you are
//! talking to helps to understand swarm behavior — and it is the kind of
//! detail the interface needs to show.

use crate::id::Id20;

/// Two-letter codes in the Azureus style (`-qB4650-…`).
const KNOWN_CLIENTS: &[(&[u8; 2], &str)] = &[
    (b"7T", "aTorrent"),
    (b"AZ", "Vuze"),
    (b"BC", "BitComet"),
    (b"BT", "BitTorrent"),
    (b"DE", "Deluge"),
    (b"FD", "Free Download Manager"),
    (b"KT", "KTorrent"),
    (b"LT", "libtorrent"),
    (b"lt", "libTorrent"),
    (b"PI", "PicoTorrent"),
    (b"qB", "qBittorrent"),
    (b"TR", "Transmission"),
    (b"TX", "Tixati"),
    (b"UM", "µTorrent Mac"),
    (b"UT", "µTorrent"),
    (b"UW", "µTorrent Web"),
    (b"WW", "WebTorrent"),
    (b"XL", "Xunlei"),
    (b"lh", "libtorrent-rasterbar"),
];

/// Readable client name, or the peer id in printable form.
pub fn name(peer_id: &Id20) -> String {
    let b = peer_id.as_bytes();

    // Azureus style: '-' + 2 letters + 4 version digits + '-'.
    if b[0] == b'-' && b[7] == b'-' {
        let code = [b[1], b[2]];
        if let Some((_, client_name)) = KNOWN_CLIENTS.iter().find(|(c, _)| **c == code) {
            return match version(&b[3..7]) {
                Some(v) => format!("{client_name} {v}"),
                None => client_name.to_string(),
            };
        }
        if code.iter().all(|c| c.is_ascii_alphanumeric()) {
            return format!("{}{}", code[0] as char, code[1] as char);
        }
    }

    // Shadow style: one letter followed by a base64-ish version.
    if b[0].is_ascii_alphabetic() && b[4..8].iter().all(|c| *c == b'-' || *c == 0) {
        let client_name = match b[0] {
            b'A' => "ABC",
            b'T' => "BitTornado",
            b'U' => "UPnP NAT",
            b'S' => "Shadow",
            _ => "",
        };
        if !client_name.is_empty() {
            return client_name.to_string();
        }
    }

    // Old BitComet uses its own prefix.
    if b.starts_with(b"exbc") || b.starts_with(b"FUTB") {
        return "BitComet".to_string();
    }
    if b.starts_with(b"OP") {
        return "Opera".to_string();
    }

    printable(b)
}

/// `4650` → `4.6.5.0`, discarding irrelevant trailing zeros.
fn version(digits: &[u8]) -> Option<String> {
    let parts: Vec<u32> = digits
        .iter()
        .map(|d| match d {
            b'0'..=b'9' => Some((d - b'0') as u32),
            // Versions above 9 use letters.
            b'A'..=b'Z' => Some((d - b'A') as u32 + 10),
            b'a'..=b'z' => Some((d - b'a') as u32 + 36),
            _ => None,
        })
        .collect::<Option<Vec<u32>>>()?;

    let mut end = parts.len();
    while end > 2 && parts[end - 1] == 0 {
        end -= 1;
    }
    Some(
        parts[..end]
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// Last resort: shows the readable bytes of the peer id.
fn printable(b: &[u8; 20]) -> String {
    let text: String = b
        .iter()
        .take(8)
        .map(|c| {
            if c.is_ascii_graphic() {
                *c as char
            } else {
                '·'
            }
        })
        .collect();
    if text.chars().all(|c| c == '·') {
        "unknown".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_id(prefix: &[u8]) -> Id20 {
        let mut b = [b'x'; 20];
        b[..prefix.len()].copy_from_slice(prefix);
        Id20(b)
    }

    #[test]
    fn recognizes_common_clients() {
        assert_eq!(name(&peer_id(b"-qB4650-")), "qBittorrent 4.6.5");
        assert_eq!(name(&peer_id(b"-TR3000-")), "Transmission 3.0");
        assert_eq!(name(&peer_id(b"-lt0D60-")), "libTorrent 0.13.6");
        assert_eq!(name(&peer_id(b"-UT3550-")), "µTorrent 3.5.5");
        assert_eq!(name(&peer_id(b"-DE2100-")), "Deluge 2.1");
    }

    #[test]
    fn handles_unknown_clients() {
        // An uncatalogued two-letter code still appears.
        assert_eq!(name(&peer_id(b"-ZZ1000-")), "ZZ");
        assert_eq!(name(&peer_id(b"exbc")), "BitComet");

        // With no convention at all, shows the readable bytes.
        let raw = name(&peer_id(b"abc"));
        assert!(raw.starts_with("abc"), "got `{raw}`");

        let zeroed = name(&Id20([0; 20]));
        assert_eq!(zeroed, "unknown");
    }

    #[test]
    fn version_discards_trailing_zeros() {
        assert_eq!(version(b"4650").unwrap(), "4.6.5");
        assert_eq!(version(b"3000").unwrap(), "3.0");
        assert_eq!(version(b"1234").unwrap(), "1.2.3.4");
        // Letters represent numbers above 9.
        assert_eq!(version(b"0D60").unwrap(), "0.13.6");
        assert!(version(b"!!!!").is_none());
    }
}
