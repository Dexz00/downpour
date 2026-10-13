//! Bencode: decoding and encoding.
//!
//! The decoder records the byte range of each key in the root dictionary.
//! This allows computing the `info_hash` over the original bytes of the
//! `info` dictionary, instead of relying on a faithful re-serialization —
//! `.torrent` files in the wild contain out-of-order keys and fields that
//! do not survive a round-trip.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::ops::Range;

/// Maximum nesting depth accepted, to contain malicious input.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<String> {
        self.as_bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Access to a dictionary key. Returns `None` if `self` is not a dict.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict().and_then(|d| d.get(key.as_bytes()))
    }

    /// Like [`Value::get`], but errors with a useful message when missing.
    pub fn require(&self, key: &str) -> Result<&Value> {
        match self.get(key) {
            Some(v) => Ok(v),
            None => bail!("missing bencode key: `{key}`"),
        }
    }

    pub fn str_at(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.as_str())
    }

    pub fn int_at(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.as_int())
    }

    pub fn bytes_at(&self, key: &str) -> Option<&[u8]> {
        self.get(key).and_then(|v| v.as_bytes())
    }

    pub fn list_at(&self, key: &str) -> Option<&[Value]> {
        self.get(key).and_then(|v| v.as_list())
    }

    pub fn bytes(b: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(b.into())
    }

    /// Construction shortcut used by tests to build example torrents.
    #[cfg(test)]
    pub fn dict(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Dict(
            pairs
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect(),
        )
    }
}

/// Result of a top-level decoding.
pub struct Decoded {
    pub value: Value,
    /// Range of the original bytes of each key in the root dictionary.
    spans: BTreeMap<Vec<u8>, Range<usize>>,
}

impl Decoded {
    /// Raw bytes of the value associated with a root dictionary key.
    pub fn raw<'a>(&self, buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
        self.spans.get(key.as_bytes()).map(|r| &buf[r.clone()])
    }
}

/// Decodes a bencode value, requiring it to consume the entire input.
pub fn decode(buf: &[u8]) -> Result<Decoded> {
    let mut d = Decoder::new(buf);
    let value = d.value()?;
    if d.pos != buf.len() {
        bail!(
            "{} trailing bytes after the root bencode value",
            buf.len() - d.pos
        );
    }
    Ok(Decoded {
        value,
        spans: d.root_spans,
    })
}

/// Decodes a value, tolerating extra bytes after it (used in KRPC/UDP).
pub fn decode_prefix(buf: &[u8]) -> Result<(Value, usize)> {
    let mut d = Decoder::new(buf);
    let value = d.value()?;
    Ok((value, d.pos))
}

struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: usize,
    root_spans: BTreeMap<Vec<u8>, Range<usize>>,
}

impl<'a> Decoder<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            depth: 0,
            root_spans: BTreeMap::new(),
        }
    }

    fn peek(&self) -> Result<u8> {
        match self.buf.get(self.pos) {
            Some(b) => Ok(*b),
            None => bail!("unexpected end of bencode input"),
        }
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.peek()? != byte {
            bail!(
                "expected `{}` at position {}, found `{}`",
                byte as char,
                self.pos,
                self.buf[self.pos] as char
            );
        }
        self.pos += 1;
        Ok(())
    }

    fn value(&mut self) -> Result<Value> {
        if self.depth > MAX_DEPTH {
            bail!("bencode nesting exceeds {MAX_DEPTH} levels");
        }
        match self.peek()? {
            b'i' => self.integer(),
            b'l' => self.list(),
            b'd' => self.dict(),
            b'0'..=b'9' => Ok(Value::Bytes(self.byte_string()?)),
            other => bail!("invalid byte `{}` at position {}", other as char, self.pos),
        }
    }

    fn integer(&mut self) -> Result<Value> {
        self.expect(b'i')?;
        let start = self.pos;
        let end = match self.buf[start..].iter().position(|b| *b == b'e') {
            Some(off) => start + off,
            None => bail!("bencode integer without terminator at position {start}"),
        };
        let text = std::str::from_utf8(&self.buf[start..end])?;
        // "i-0e" and leading zeros are invalid per the spec; we reject them
        // because accepting them would break info_hash uniqueness.
        if text == "-0" || (text.len() > 1 && text.starts_with('0')) {
            bail!("malformed bencode integer: `{text}`");
        }
        let n: i64 = text.parse()?;
        self.pos = end + 1;
        Ok(Value::Int(n))
    }

    fn byte_string(&mut self) -> Result<Vec<u8>> {
        let start = self.pos;
        let colon = match self.buf[start..].iter().position(|b| *b == b':') {
            Some(off) => start + off,
            None => bail!("bencode string without `:` at position {start}"),
        };
        let len: usize = std::str::from_utf8(&self.buf[start..colon])?.parse()?;
        let from = colon + 1;
        let to = match from.checked_add(len) {
            Some(t) if t <= self.buf.len() => t,
            _ => bail!("bencode string of {len} bytes exceeds the input"),
        };
        self.pos = to;
        Ok(self.buf[from..to].to_vec())
    }

    fn list(&mut self) -> Result<Value> {
        self.expect(b'l')?;
        self.depth += 1;
        let mut items = Vec::new();
        while self.peek()? != b'e' {
            items.push(self.value()?);
        }
        self.pos += 1;
        self.depth -= 1;
        Ok(Value::List(items))
    }

    fn dict(&mut self) -> Result<Value> {
        self.expect(b'd')?;
        let is_root = self.depth == 0;
        self.depth += 1;
        let mut map = BTreeMap::new();
        while self.peek()? != b'e' {
            let key = self.byte_string()?;
            let value_start = self.pos;
            let value = self.value()?;
            if is_root {
                self.root_spans.insert(key.clone(), value_start..self.pos);
            }
            map.insert(key, value);
        }
        self.pos += 1;
        self.depth -= 1;
        Ok(Value::Dict(map))
    }
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

pub fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(map) => {
            out.push(b'd');
            // BTreeMap already iterates in lexicographic byte order, which is
            // exactly the order the spec requires.
            for (k, v) in map {
                encode_into(&Value::Bytes(k.clone()), out);
                encode_into(v, out);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_basic_types() {
        assert_eq!(decode(b"i42e").unwrap().value, Value::Int(42));
        assert_eq!(decode(b"i-13e").unwrap().value, Value::Int(-13));
        assert_eq!(decode(b"4:spam").unwrap().value, Value::bytes("spam"));
        assert_eq!(decode(b"0:").unwrap().value, Value::bytes(""));
        assert_eq!(
            decode(b"li1ei2ee").unwrap().value,
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
    }

    #[test]
    fn decodes_dictionary() {
        let d = decode(b"d3:cow3:moo4:spam4:eggse").unwrap();
        assert_eq!(d.value.str_at("cow").as_deref(), Some("moo"));
        assert_eq!(d.value.str_at("spam").as_deref(), Some("eggs"));
    }

    #[test]
    fn preserves_raw_info_bytes() {
        let raw = b"d4:infod6:lengthi3ee4:name2:hie";
        let d = decode(raw).unwrap();
        assert_eq!(d.raw(raw, "info").unwrap(), b"d6:lengthi3ee");
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode(b"i-0e").is_err());
        assert!(decode(b"i03e").is_err());
        assert!(decode(b"5:abc").is_err());
        assert!(decode(b"4:spamextra").is_err());
        assert!(decode(b"d3:keye").is_err());
    }

    #[test]
    fn round_trip_preserves_key_order() {
        let raw = b"d1:ai1e1:bl1:x1:yee";
        let d = decode(raw).unwrap();
        assert_eq!(encode(&d.value), raw);
    }
}
