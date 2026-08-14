//! Announce por HTTP/HTTPS (BEP-3, com peers compactos do BEP-23).

use super::{AnnounceRequest, AnnounceResponse, Event};
use crate::bencode;
use crate::compact;
use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// `info_hash` e `peer_id` são bytes crus, não texto: tudo que não for
/// *unreserved* (RFC 3986) precisa ir percent-encoded.
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
    if let Some(evento) = req.event.as_str() {
        url.push_str("&event=");
        url.push_str(evento);
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
    let resposta = client
        .get(&url)
        .send()
        .await
        .context("falha ao contatar o tracker")?;
    let status = resposta.status();
    let corpo = resposta.bytes().await.context("resposta interrompida")?;
    if !status.is_success() {
        bail!("tracker respondeu HTTP {}", status.as_u16());
    }
    parse_response(&corpo)
}

pub fn parse_response(corpo: &[u8]) -> Result<AnnounceResponse> {
    let root = bencode::decode(corpo)
        .context("resposta do tracker não é bencode válido")?
        .value;

    if let Some(motivo) = root.str_at("failure reason") {
        bail!("tracker recusou: {motivo}");
    }

    let mut peers = match root.get("peers") {
        // Formato compacto (BEP-23): 6 bytes por peer.
        Some(bencode::Value::Bytes(b)) => compact::parse_v4(b),
        // Formato antigo: lista de dicionários.
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

    let segundos = |chave: &str| root.int_at(chave).filter(|v| *v > 0).map(|v| v as u64);
    Ok(AnnounceResponse {
        // Tracker sem `interval` é raro mas acontece; 30 min é o padrão de fato.
        interval: Duration::from_secs(segundos("interval").unwrap_or(1800).clamp(60, 3 * 3600)),
        min_interval: segundos("min interval").map(Duration::from_secs),
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

    fn pedido() -> AnnounceRequest {
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
    fn monta_query_com_bytes_escapados() {
        let url = build_url("http://t.exemplo/announce", &pedido());
        assert!(url.starts_with("http://t.exemplo/announce?info_hash=%12%12"));
        assert!(url.contains("&peer_id=-TR0001-abcdefghijkl"), "unreserved passa cru");
        assert!(url.contains("&port=6881&uploaded=10&downloaded=20&left=30"));
        assert!(url.contains("&compact=1"));
        assert!(url.contains("&event=started"));
        assert!(url.contains("&key=deadbeef"));

        // Um announce que já tem query recebe `&` em vez de `?`.
        let com_query = build_url("http://t.exemplo/ann?x=1", &pedido());
        assert!(com_query.contains("?x=1&info_hash="));

        // O announce periódico não manda `event`.
        let mut periodico = pedido();
        periodico.event = Event::Periodic;
        assert!(!build_url("http://t/a", &periodico).contains("event="));
    }

    #[test]
    fn le_resposta_compacta() {
        let corpo = bencode::encode(&Value::dict([
            ("interval", Value::Int(900)),
            ("min interval", Value::Int(300)),
            ("complete", Value::Int(5)),
            ("incomplete", Value::Int(7)),
            (
                "peers",
                Value::Bytes(vec![192, 168, 0, 1, 0x1a, 0xe1, 10, 1, 2, 3, 0x1a, 0xe2]),
            ),
        ]));
        let r = parse_response(&corpo).unwrap();
        assert_eq!(r.interval, Duration::from_secs(900));
        assert_eq!(r.min_interval, Some(Duration::from_secs(300)));
        assert_eq!(r.seeders, 5);
        assert_eq!(r.leechers, 7);
        assert_eq!(r.peers.len(), 2);
        assert_eq!(r.peers[0].to_string(), "192.168.0.1:6881");
    }

    #[test]
    fn le_resposta_em_lista_de_dicionarios() {
        let peer = |ip: &str, porta: i64| {
            Value::dict([("ip", Value::bytes(ip)), ("port", Value::Int(porta))])
        };
        let corpo = bencode::encode(&Value::dict([
            ("interval", Value::Int(600)),
            (
                "peers",
                Value::List(vec![peer("1.2.3.4", 51413), peer("5.6.7.8", 0)]),
            ),
        ]));
        let r = parse_response(&corpo).unwrap();
        assert_eq!(r.peers.len(), 1, "porta 0 é descartada");
        assert_eq!(r.peers[0].to_string(), "1.2.3.4:51413");
    }

    #[test]
    fn propaga_falha_do_tracker() {
        let corpo = bencode::encode(&Value::dict([(
            "failure reason",
            Value::bytes("torrent não registrado"),
        )]));
        let erro = parse_response(&corpo).unwrap_err().to_string();
        assert!(erro.contains("torrent não registrado"));
        assert!(parse_response(b"nao-e-bencode").is_err());
    }

    #[test]
    fn intervalo_absurdo_e_limitado() {
        let corpo = bencode::encode(&Value::dict([("interval", Value::Int(999_999))]));
        assert_eq!(parse_response(&corpo).unwrap().interval.as_secs(), 3 * 3600);
        let corpo = bencode::encode(&Value::dict([("interval", Value::Int(1))]));
        assert_eq!(parse_response(&corpo).unwrap().interval.as_secs(), 60);
    }
}
