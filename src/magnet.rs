//! Magnet links (BEP-9 / BEP-53): identificam um torrent apenas pelo
//! `info_hash`; o dicionário `info` é buscado depois, pela rede.

use crate::id::Id20;
use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Magnet {
    pub info_hash: Id20,
    /// `dn` — nome sugerido, apenas para exibição até o `info` chegar.
    pub name: Option<String>,
    pub trackers: Vec<String>,
    /// `x.pe` — peers embutidos, atalho para começar sem tracker nem DHT.
    pub peers: Vec<SocketAddr>,
}

impl Magnet {
    pub fn parse(uri: &str) -> Result<Magnet> {
        let query = uri
            .strip_prefix("magnet:?")
            .context("URI não começa com `magnet:?`")?;

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
                        // BitTorrent v2: reconhecemos, mas o download é v1.
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
            bail!("magnet sem `xt=urn:btih:` (apenas v2 não é suportado)");
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
        40 => Id20::from_hex(hash).context("info_hash hex inválido"),
        32 => Id20::from_base32(hash).context("info_hash base32 inválido"),
        n => bail!("info_hash com {n} caracteres; esperava 40 (hex) ou 32 (base32)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parseia_magnet_completo() {
        let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567\
                   &dn=Meu%20Arquivo&tr=udp%3A%2F%2Ftracker.exemplo%3A6969\
                   &tr=http%3A%2F%2Foutro%2Fann&x.pe=1.2.3.4:51413";
        let m = Magnet::parse(uri).unwrap();
        assert_eq!(m.info_hash.to_hex(), "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(m.name.as_deref(), Some("Meu Arquivo"));
        assert_eq!(m.trackers, ["udp://tracker.exemplo:6969", "http://outro/ann"]);
        assert_eq!(m.peers, ["1.2.3.4:51413".parse::<SocketAddr>().unwrap()]);
    }

    #[test]
    fn aceita_base32_e_rejeita_sem_hash() {
        let m = Magnet::parse("magnet:?xt=urn:btih:AEBAGBAFAYDQQCIKBMGA2DQPCAIREEYU").unwrap();
        assert_eq!(m.info_hash.to_hex(), "0102030405060708090a0b0c0d0e0f1011121314");
        assert!(Magnet::parse("magnet:?dn=nada").is_err());
        assert!(Magnet::parse("http://exemplo/x.torrent").is_err());
    }
}
