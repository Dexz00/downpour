#![no_main]

use libfuzzer_sys::fuzz_target;

// Everything a remote peer sends us lands here first: wire messages, the
// handshake and the extension payloads. None of it may panic.
fuzz_target!(|data: &[u8]| {
    let _ = downpour::peer::message::Message::decode(data);
    let _ = downpour::peer::extension::ExtensionHandshake::decode(data);
    let _ = downpour::peer::extension::MetadataMessage::decode(data);
    let _ = downpour::peer::extension::PexMessage::decode(data);
    if let Ok(handshake) = <[u8; 68]>::try_from(data) {
        let _ = downpour::peer::message::Handshake::from_bytes(&handshake);
    }
});
