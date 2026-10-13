//! torrra — a terminal BitTorrent client, written from scratch.
//!
//! This library crate holds the whole implementation (protocol, DHT, piece
//! selection, storage, terminal UI); the `torrra` binary is a thin CLI front
//! end over [`session::Session`].

pub mod bencode;
pub mod bitfield;
pub mod client_id;
pub mod compact;
pub mod dht;
pub mod engine;
pub mod id;
#[macro_use]
pub mod log;
pub mod magnet;
pub mod metainfo;
pub mod peer;
pub mod picker;
pub mod platform;
pub mod session;
pub mod stats;
pub mod storage;
pub mod tracker;
pub mod ui;
