#![no_main]

use libfuzzer_sys::fuzz_target;

// KRPC datagrams arrive from arbitrary UDP senders, and the compact node
// lists inside them are split by fixed strides. No input may panic.
fuzz_target!(|data: &[u8]| {
    let _ = torrra::dht::krpc::Message::decode(data);
    let _ = torrra::dht::krpc::parse_nodes(data);
    let _ = torrra::dht::krpc::parse_nodes6(data);
    let _ = torrra::compact::parse_v4(data);
    let _ = torrra::compact::parse_v6(data);
});
