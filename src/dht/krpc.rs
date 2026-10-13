//! KRPC - the DHT protocol: bencode over UDP (BEP-5).

use crate::bencode::{self, Value};
use crate::id::Id20;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// A DHT node in compact form: 20 bytes of id + address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeInfo {
    pub id: Id20,
    pub addr: SocketAddr,
}

pub fn parse_nodes(buf: &[u8]) -> Vec<NodeInfo> {
    buf.as_chunks::<26>()
        .0
        .iter()
        .filter_map(|c| {
            let id = Id20::from_slice(&c[..20]).ok()?;
            let addr = SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(c[20], c[21], c[22], c[23])),
                u16::from_be_bytes([c[24], c[25]]),
            );
            crate::compact::is_usable(&addr).then_some(NodeInfo { id, addr })
        })
        .collect()
}

pub fn parse_nodes6(buf: &[u8]) -> Vec<NodeInfo> {
    buf.as_chunks::<38>()
        .0
        .iter()
        .filter_map(|c| {
            let id = Id20::from_slice(&c[..20]).ok()?;
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&c[20..36]);
            let addr = SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(octets)),
                u16::from_be_bytes([c[36], c[37]]),
            );
            crate::compact::is_usable(&addr).then_some(NodeInfo { id, addr })
        })
        .collect()
}

pub fn encode_nodes(nodes: &[NodeInfo]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nodes.len() * 26);
    for n in nodes {
        if let IpAddr::V4(ip) = n.addr.ip() {
            out.extend_from_slice(n.id.as_bytes());
            out.extend_from_slice(&ip.octets());
            out.extend_from_slice(&n.addr.port().to_be_bytes());
        }
    }
    out
}

/// Queries that we send and receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Ping {
        id: Id20,
    },
    FindNode {
        id: Id20,
        target: Id20,
    },
    GetPeers {
        id: Id20,
        info_hash: Id20,
    },
    AnnouncePeer {
        id: Id20,
        info_hash: Id20,
        port: u16,
        token: Vec<u8>,
        /// When set, the node asks that the UDP source port be used.
        implied_port: bool,
    },
}

impl Query {
    pub fn method(&self) -> &'static str {
        match self {
            Query::Ping { .. } => "ping",
            Query::FindNode { .. } => "find_node",
            Query::GetPeers { .. } => "get_peers",
            Query::AnnouncePeer { .. } => "announce_peer",
        }
    }

    fn args(&self) -> BTreeMap<Vec<u8>, Value> {
        let mut a: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        match self {
            Query::Ping { id } => {
                a.insert(b"id".to_vec(), Value::bytes(*id.as_bytes()));
            }
            Query::FindNode { id, target } => {
                a.insert(b"id".to_vec(), Value::bytes(*id.as_bytes()));
                a.insert(b"target".to_vec(), Value::bytes(*target.as_bytes()));
            }
            Query::GetPeers { id, info_hash } => {
                a.insert(b"id".to_vec(), Value::bytes(*id.as_bytes()));
                a.insert(b"info_hash".to_vec(), Value::bytes(*info_hash.as_bytes()));
            }
            Query::AnnouncePeer {
                id,
                info_hash,
                port,
                token,
                implied_port,
            } => {
                a.insert(b"id".to_vec(), Value::bytes(*id.as_bytes()));
                a.insert(b"info_hash".to_vec(), Value::bytes(*info_hash.as_bytes()));
                a.insert(b"port".to_vec(), Value::Int(*port as i64));
                a.insert(b"token".to_vec(), Value::Bytes(token.clone()));
                a.insert(
                    b"implied_port".to_vec(),
                    Value::Int(if *implied_port { 1 } else { 0 }),
                );
            }
        }
        a
    }

    fn from_parts(method: &str, a: &Value) -> Result<Query> {
        let id = id_at(a, "id")?;
        Ok(match method {
            "ping" => Query::Ping { id },
            "find_node" => Query::FindNode {
                id,
                target: id_at(a, "target")?,
            },
            "get_peers" => Query::GetPeers {
                id,
                info_hash: id_at(a, "info_hash")?,
            },
            "announce_peer" => Query::AnnouncePeer {
                id,
                info_hash: id_at(a, "info_hash")?,
                port: a.int_at("port").unwrap_or(0).clamp(0, 65535) as u16,
                token: a
                    .bytes_at("token")
                    .context("announce_peer without token")?
                    .to_vec(),
                implied_port: a.int_at("implied_port").unwrap_or(0) != 0,
            },
            other => bail!("unknown DHT query: {other}"),
        })
    }
}

/// Response to a query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    pub id: Id20,
    pub nodes: Vec<NodeInfo>,
    pub values: Vec<SocketAddr>,
    pub token: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Query {
        transaction: Vec<u8>,
        query: Query,
    },
    Response {
        transaction: Vec<u8>,
        response: Response,
    },
    Error {
        transaction: Vec<u8>,
        code: i64,
        message: String,
    },
}

impl Message {
    pub fn transaction(&self) -> &[u8] {
        match self {
            Message::Query { transaction, .. }
            | Message::Response { transaction, .. }
            | Message::Error { transaction, .. } => transaction,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut root: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
        root.insert(b"t".to_vec(), Value::Bytes(self.transaction().to_vec()));
        match self {
            Message::Query { query, .. } => {
                root.insert(b"y".to_vec(), Value::bytes("q"));
                root.insert(b"q".to_vec(), Value::bytes(query.method()));
                root.insert(b"a".to_vec(), Value::Dict(query.args()));
                // Identifies the client; purely informational.
                root.insert(b"v".to_vec(), Value::bytes("TR01"));
            }
            Message::Response { response, .. } => {
                let mut r: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
                r.insert(b"id".to_vec(), Value::bytes(*response.id.as_bytes()));
                if !response.nodes.is_empty() {
                    r.insert(
                        b"nodes".to_vec(),
                        Value::Bytes(encode_nodes(&response.nodes)),
                    );
                }
                if !response.values.is_empty() {
                    let (v4, _) = crate::compact::encode(&response.values);
                    r.insert(
                        b"values".to_vec(),
                        Value::List(
                            v4.as_chunks::<6>()
                                .0
                                .iter()
                                .map(|c| Value::bytes(c.as_slice()))
                                .collect(),
                        ),
                    );
                }
                if let Some(token) = &response.token {
                    r.insert(b"token".to_vec(), Value::Bytes(token.clone()));
                }
                root.insert(b"y".to_vec(), Value::bytes("r"));
                root.insert(b"r".to_vec(), Value::Dict(r));
            }
            Message::Error { code, message, .. } => {
                root.insert(b"y".to_vec(), Value::bytes("e"));
                root.insert(
                    b"e".to_vec(),
                    Value::List(vec![Value::Int(*code), Value::bytes(message.as_str())]),
                );
            }
        }
        bencode::encode(&Value::Dict(root))
    }

    pub fn decode(buf: &[u8]) -> Result<Message> {
        let root = bencode::decode(buf)
            .context("DHT packet is not bencode")?
            .value;
        let transaction = root
            .bytes_at("t")
            .context("DHT packet without transaction id")?
            .to_vec();

        match root.str_at("y").as_deref() {
            Some("q") => {
                let method = root.str_at("q").context("query without method")?;
                let args = root.require("a")?;
                Ok(Message::Query {
                    transaction,
                    query: Query::from_parts(&method, args)?,
                })
            }
            Some("r") => {
                let r = root.require("r")?;
                let mut nodes = r.bytes_at("nodes").map(parse_nodes).unwrap_or_default();
                nodes.extend(r.bytes_at("nodes6").map(parse_nodes6).unwrap_or_default());

                // `values` is a list of 6-byte strings, one per peer.
                let values = r
                    .list_at("values")
                    .map(|list| {
                        list.iter()
                            .filter_map(|v| v.as_bytes())
                            .flat_map(crate::compact::parse_v4)
                            .collect()
                    })
                    .unwrap_or_default();

                Ok(Message::Response {
                    transaction,
                    response: Response {
                        id: id_at(r, "id")?,
                        nodes,
                        values,
                        token: r.bytes_at("token").map(|t| t.to_vec()),
                    },
                })
            }
            Some("e") => {
                let e = root.list_at("e").unwrap_or_default();
                Ok(Message::Error {
                    transaction,
                    code: e.first().and_then(|v| v.as_int()).unwrap_or(201),
                    message: e
                        .get(1)
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| "error without description".into()),
                })
            }
            other => bail!("DHT packet with unexpected `y` type: {other:?}"),
        }
    }
}

fn id_at(v: &Value, key: &str) -> Result<Id20> {
    let raw = v
        .bytes_at(key)
        .with_context(|| format!("field `{key}` missing"))?;
    Id20::from_slice(raw).with_context(|| format!("field `{key}` is not 20 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: Message) -> Message {
        Message::decode(&msg.encode()).unwrap()
    }

    #[test]
    fn queries_round_trip() {
        let id = Id20::random();
        let target = Id20::random();
        for query in [
            Query::Ping { id },
            Query::FindNode { id, target },
            Query::GetPeers {
                id,
                info_hash: target,
            },
            Query::AnnouncePeer {
                id,
                info_hash: target,
                port: 6881,
                token: b"abcd".to_vec(),
                implied_port: true,
            },
        ] {
            let msg = Message::Query {
                transaction: b"aa".to_vec(),
                query: query.clone(),
            };
            match round_trip(msg) {
                Message::Query {
                    query: parsed,
                    transaction,
                } => {
                    assert_eq!(parsed, query);
                    assert_eq!(transaction, b"aa");
                }
                other => panic!("expected Query, got {other:?}"),
            }
        }
    }

    #[test]
    fn response_with_nodes_and_peers() {
        let resp = Response {
            id: Id20::random(),
            nodes: vec![NodeInfo {
                id: Id20::random(),
                addr: "1.2.3.4:6881".parse().unwrap(),
            }],
            values: vec!["9.8.7.6:51413".parse().unwrap()],
            token: Some(b"tok".to_vec()),
        };
        let msg = Message::Response {
            transaction: b"xy".to_vec(),
            response: resp.clone(),
        };
        match round_trip(msg) {
            Message::Response { response, .. } => assert_eq!(response, resp),
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn error_round_trip() {
        let msg = Message::Error {
            transaction: b"t1".to_vec(),
            code: 201,
            message: "Generic Error".to_string(),
        };
        assert_eq!(round_trip(msg.clone()), msg);
    }

    #[test]
    fn rejects_invalid_packets() {
        assert!(Message::decode(b"xyz").is_err());
        // Query without the `a` field.
        assert!(Message::decode(b"d1:t2:aa1:y1:q1:q4:pinge").is_err());
        // Response with an id of the wrong size.
        assert!(Message::decode(b"d1:rd2:id3:abce1:t2:aa1:y1:re").is_err());
    }

    #[test]
    fn compact_nodes_round_trip() {
        let nodes = vec![
            NodeInfo {
                id: Id20([1; 20]),
                addr: "10.0.0.1:6881".parse().unwrap(),
            },
            NodeInfo {
                id: Id20([2; 20]),
                addr: "10.0.0.2:1234".parse().unwrap(),
            },
        ];
        assert_eq!(parse_nodes(&encode_nodes(&nodes)), nodes);
        // Incomplete trailing bytes are ignored instead of breaking parsing.
        assert!(parse_nodes(&[0u8; 25]).is_empty());
    }
}
