//! Piece bitfield, in the protocol layout: the most significant bit of
//! the first byte = piece 0.

#[derive(Clone, PartialEq, Eq)]
pub struct Bitfield {
    bits: Vec<u8>,
    len: usize,
    ones: usize,
}

impl Bitfield {
    pub fn new(len: usize) -> Self {
        Bitfield {
            bits: vec![0; len.div_ceil(8)],
            len,
            ones: 0,
        }
    }

    /// Builds from the bytes received from a peer.
    ///
    /// Rejects the wrong size and lit padding bits: peers that send this
    /// have a shifted index, and trusting them corrupts the picker.
    pub fn from_bytes(bytes: &[u8], len: usize) -> Option<Self> {
        if bytes.len() != len.div_ceil(8) {
            return None;
        }
        let padding = bytes.len() * 8 - len;
        if padding > 0 && bytes.last().is_some_and(|b| b & ((1 << padding) - 1) != 0) {
            return None;
        }
        let ones = bytes.iter().map(|b| b.count_ones() as usize).sum();
        Some(Bitfield {
            bits: bytes.to_vec(),
            len,
            ones,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn count(&self) -> usize {
        self.ones
    }

    pub fn is_empty(&self) -> bool {
        self.ones == 0
    }

    pub fn is_complete(&self) -> bool {
        self.ones == self.len
    }

    pub fn get(&self, index: usize) -> bool {
        index < self.len && self.bits[index / 8] & (0x80 >> (index % 8)) != 0
    }

    /// Marks the piece; returns `true` if it was not already marked.
    pub fn set(&mut self, index: usize) -> bool {
        if index >= self.len || self.get(index) {
            return false;
        }
        self.bits[index / 8] |= 0x80 >> (index % 8);
        self.ones += 1;
        true
    }

    #[cfg(test)]
    pub fn clear(&mut self, index: usize) -> bool {
        if index >= self.len || !self.get(index) {
            return false;
        }
        self.bits[index / 8] &= !(0x80 >> (index % 8));
        self.ones -= 1;
        true
    }

    pub fn set_all(&mut self) {
        for i in 0..self.len {
            self.set(i);
        }
    }

    pub fn iter_set(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len).filter(|i| self.get(*i))
    }

    /// Is there any piece that the other has and we do not?
    pub fn has_anything_we_lack(&self, other: &Bitfield) -> bool {
        self.bits
            .iter()
            .zip(&other.bits)
            .any(|(mine, theirs)| theirs & !mine != 0)
    }
}

impl std::fmt::Debug for Bitfield {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bitfield({}/{})", self.ones, self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_and_count() {
        let mut bf = Bitfield::new(10);
        assert!(bf.set(0));
        assert!(bf.set(9));
        assert!(!bf.set(9), "repeated set must not recount");
        assert!(!bf.set(10), "out-of-range index is ignored");
        assert!(bf.get(0) && bf.get(9) && !bf.get(5));
        assert_eq!(bf.count(), 2);
        assert_eq!(bf.as_bytes()[0], 0b1000_0000);
        assert!(bf.clear(0));
        assert_eq!(bf.count(), 1);
    }

    #[test]
    fn rejects_invalid_peer_bitfield() {
        assert!(Bitfield::from_bytes(&[0xff], 10).is_none(), "too short");
        assert!(
            Bitfield::from_bytes(&[0xff, 0xff], 10).is_none(),
            "padding bit set"
        );
        let bf = Bitfield::from_bytes(&[0xff, 0b1100_0000], 10).unwrap();
        assert_eq!(bf.count(), 10);
        assert!(bf.is_complete());
    }

    #[test]
    fn detects_pieces_of_interest() {
        let mut mine = Bitfield::new(16);
        let mut theirs = Bitfield::new(16);
        mine.set(0);
        theirs.set(0);
        assert!(!mine.has_anything_we_lack(&theirs));
        theirs.set(7);
        assert!(mine.has_anything_we_lack(&theirs));
    }
}
