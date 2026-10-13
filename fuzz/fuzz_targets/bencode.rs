#![no_main]

use libfuzzer_sys::fuzz_target;

// The bencode decoder takes bytes straight from `.torrent` files and from
// the network (KRPC, tracker responses, extension handshakes): it must
// never panic, only return `Err`.
fuzz_target!(|data: &[u8]| {
    let _ = torrra::bencode::decode(data);
    let _ = torrra::bencode::decode_prefix(data);
});
