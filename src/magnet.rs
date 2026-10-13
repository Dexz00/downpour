//! Magnet links (BEP-9 / BEP-53): identify a torrent solely by its
//! `info_hash`; the `info` dictionary is fetched later, over the network.

use crate::id::Id20;
use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Magnet {
    pub info_hash: Id20,
    /// `dn` — suggested name, only for display until the `info` arrives.
    pub name: Option<String>,
    pub trackers: Vec<String>,
    /// `x.pe` — embedded peers, a shortcut to start without tracker or DHT.
    pub peers: Vec<SocketAddr>,
}

impl Magnet {
    pub fn parse(uri: &str) -> Result<Magnet> {
        let query = uri
            .strip_prefix("magnet:?")
            .context("URI does not start with `magnet:?`")?;

        let mut info_hash = None;
        let mut name = None;
        let mut trackers = Vec::new();
        let mut peers = Vec::new();

        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let value = percent_decode_str(value).decode_utf8_lossy().into_owned();
            match key {
                "xt" | "xt.1" => {
                    if let Some(hash) = value.strip_prefix("urn:btih:") {
                        info_hash = Some(parse_btih(hash)?);
                    } else if value.starts_with("urn:btmh:") {
                        // BitTorrent v2: we recognize it, but the download is v1.
                        continue;
                    }
                }
                "dn" => name = Some(value),
                "tr" if !value.is_empty() => trackers.push(value),
                "x.pe" => {
                    if let Ok(addr) = value.parse::<SocketAddr>() {
                        peers.push(addr);
                    }
                }
                _ => {}
            }
        }

        let Some(info_hash) = info_hash else {
            bail!("magnet without `xt=urn:btih:` (v2-only is not supported)");
        };

        Ok(Magnet {
            info_hash,
            name,
            trackers,
            peers,
        })
    }
}

fn parse_btih(hash: &str) -> Result<Id20> {
    match hash.len() {
        40 => Id20::from_hex(hash).context("invalid hex info_hash"),
        32 => Id20::from_base32(hash).context("invalid base32 info_hash"),
        n => bail!("info_hash with {n} characters; expected 40 (hex) or 32 (base32)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_magnet() {
        let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567\
                   &dn=My%20File&tr=udp%3A%2F%2Ftracker.example%3A6969\
                   &tr=http%3A%2F%2Fother%2Fann&x.pe=1.2.3.4:51413";
        let m = Magnet::parse(uri).unwrap();
        assert_eq!(
            m.info_hash.to_hex(),
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(m.name.as_deref(), Some("My File"));
        assert_eq!(
            m.trackers,
            ["udp://tracker.example:6969", "http://other/ann"]
        );
        assert_eq!(m.peers, ["1.2.3.4:51413".parse::<SocketAddr>().unwrap()]);
    }

    #[test]
    fn accepts_base32_and_rejects_missing_hash() {
        let m = Magnet::parse("magnet:?xt=urn:btih:AEBAGBAFAYDQQCIKBMGA2DQPCAIREEYU").unwrap();
        assert_eq!(
            m.info_hash.to_hex(),
            "0102030405060708090a0b0c0d0e0f1011121314"
        );
        assert!(Magnet::parse("magnet:?dn=nothing").is_err());
        assert!(Magnet::parse("http://example/x.torrent").is_err());
    }
}
