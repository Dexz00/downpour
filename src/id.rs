//! Identificadores de 20 bytes — info_hash, peer id e node id do DHT
//! compartilham o mesmo formato, então compartilham o mesmo tipo.

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
            bail!("identificador deve ter 20 bytes, recebi {}", raw.len());
        }
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(raw);
        Ok(Id20(bytes))
    }

    /// Magnets antigos codificam o info_hash em base32 (32 caracteres).
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
                bail!("caractere base32 inválido: `{}`", c as char);
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

    /// Distância XOR usada pelo Kademlia para ordenar nós.
    pub fn distance(&self, other: &Id20) -> [u8; 20] {
        let mut d = [0u8; 20];
        for ((destino, a), b) in d.iter_mut().zip(&self.0).zip(&other.0) {
            *destino = a ^ b;
        }
        d
    }

    /// Índice do bucket do DHT: número de bits idênticos no prefixo.
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

/// Peer id no estilo Azureus: `-TR0001-` seguido de 12 bytes aleatórios.
/// O prefixo importa: alguns peers rejeitam clientes que não reconhecem.
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
    fn converte_hex_e_base32() {
        let hexed = "0123456789abcdef0123456789abcdef01234567";
        let id = Id20::from_hex(hexed).unwrap();
        assert_eq!(id.to_hex(), hexed);

        // Vetor conhecido: os bytes 0x01..0x14 em base32.
        let b32 = Id20::from_base32("AEBAGBAFAYDQQCIKBMGA2DQPCAIREEYU").unwrap();
        assert_eq!(b32.to_hex(), "0102030405060708090a0b0c0d0e0f1011121314");
        assert!(Id20::from_base32("!!!").is_err());
        assert!(Id20::from_base32("AEBA").is_err());
    }

    #[test]
    fn bucket_index_conta_prefixo_comum() {
        let a = Id20([0u8; 20]);
        let mut b = [0u8; 20];
        b[0] = 0b0100_0000;
        assert_eq!(a.bucket_index(&Id20(b)), 1);
        assert_eq!(a.bucket_index(&a), 159);
    }
}
