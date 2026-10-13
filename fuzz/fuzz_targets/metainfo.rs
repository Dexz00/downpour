#![no_main]

use libfuzzer_sys::fuzz_target;

// A `.torrent` is a third-party file, and the `info` dictionary also arrives
// over the wire via `ut_metadata`. Its paths are sanitized before use, and
// no malformed input may panic the parser.
fuzz_target!(|data: &[u8]| {
    let _ = torrra::metainfo::Metainfo::from_bytes(data);
    let _ = torrra::metainfo::Info::from_raw(data);
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = torrra::magnet::Magnet::parse(text);
    }
});
