//! 20-byte identifiers — info_hash, peer id and DHT node id
//! share the same format, so they share the same type.

use anyhow::{Result, bail};
use rand::RngExt;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Id20(pub [u8; 20]);

impl Id20 {
    pub fn random() -> Self {
        let mut bytes = [0u8; 20];
        rand::rng().fill(&mut bytes);
        Id20(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let raw = hex::decode(s)?;
        Self::from_slice(&raw)
    }

    pub fn from_slice(raw: &[u8]) -> Result<Self> {
        if raw.len() != 20 {
            bail!("identifier must be 20 bytes, got {}", raw.len());
        }
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(raw);
        Ok(Id20(bytes))
    }

    /// Old magnets encode the info_hash in base32 (32 characters).
    pub fn from_base32(s: &str) -> Result<Self> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = Vec::with_capacity(20);
        let mut acc: u32 = 0;
        let mut bits: u32 = 0;
        for c in s.bytes() {
            let c = c.to_ascii_uppercase();
            if c == b'=' {
                continue;
            }
            let Some(v) = ALPHABET.iter().position(|a| *a == c) else {
                bail!("invalid base32 character: `{}`", c as char);
            };
            acc = (acc << 5) | v as u32;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        Self::from_slice(&out)
    }

    /// XOR distance used by Kademlia to order nodes.
    pub fn distance(&self, other: &Id20) -> [u8; 20] {
        let mut d = [0u8; 20];
        for ((dest, a), b) in d.iter_mut().zip(&self.0).zip(&other.0) {
            *dest = a ^ b;
        }
        d
    }

    /// DHT bucket index: number of identical bits in the prefix.
    pub fn bucket_index(&self, other: &Id20) -> usize {
        let d = self.distance(other);
        for (i, byte) in d.iter().enumerate() {
            if *byte != 0 {
                return i * 8 + byte.leading_zeros() as usize;
            }
        }
        159
    }
}

impl fmt::Display for Id20 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Debug for Id20 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id20({})", &self.to_hex()[..8])
    }
}

/// Azureus-style peer id: `-TR0001-` followed by 12 random bytes.
/// The prefix matters: some peers reject clients they do not recognize.
pub fn generate_peer_id() -> Id20 {
    let mut bytes = [0u8; 20];
    bytes[..8].copy_from_slice(b"-TR0001-");
    rand::rng().fill(&mut bytes[8..]);
    Id20(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_hex_and_base32() {
        let hexed = "0123456789abcdef0123456789abcdef01234567";
        let id = Id20::from_hex(hexed).unwrap();
        assert_eq!(id.to_hex(), hexed);

        // Known vector: the bytes 0x01..0x14 in base32.
        let b32 = Id20::from_base32("AEBAGBAFAYDQQCIKBMGA2DQPCAIREEYU").unwrap();
        assert_eq!(b32.to_hex(), "0102030405060708090a0b0c0d0e0f1011121314");
        assert!(Id20::from_base32("!!!").is_err());
        assert!(Id20::from_base32("AEBA").is_err());
    }

    #[test]
    fn bucket_index_counts_common_prefix() {
        let a = Id20([0u8; 20]);
        let mut b = [0u8; 20];
        b[0] = 0b0100_0000;
        assert_eq!(a.bucket_index(&Id20(b)), 1);
        assert_eq!(a.bucket_index(&a), 159);
    }
}
