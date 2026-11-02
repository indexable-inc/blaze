//! What a run reached, as features: a counter in one of AFL's buckets of hit counts, or a bit
//! of the value profile. An input is kept when it reaches a feature no kept input did.

/// AFL's buckets of a counter's value: 1, 2, 3, 4-7, 8-15, 16-31, 32-127, 128-255.
const BUCKETS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut count = 1;
    while count < 256 {
        table[count] = match count {
            1 => 0,
            2 => 1,
            3 => 2,
            4..=7 => 3,
            8..=15 => 4,
            16..=31 => 5,
            32..=127 => 6,
            _ => 7,
        };
        count += 1;
    }
    table
};

/// The bucket of a counter that reached `count` (not 0).
#[inline(always)]
pub(crate) fn bucket(count: u8) -> u8 {
    BUCKETS[count as usize]
}

/// Every feature some kept input reached.
pub(crate) struct Seen {
    bits: Vec<u64>,
    count: usize,
}

impl Seen {
    pub(crate) fn new(features: usize) -> Self {
        Seen { bits: vec![0; features.div_ceil(64)], count: 0 }  // alloc: the seen set, once per process
    }

    /// How many of `features` (each once) are new; they are seen from now on.
    pub(crate) fn merge(&mut self, features: &[u32]) -> usize {
        let mut new = 0;
        for &feature in features {
            let (word, bit) = (feature as usize / 64, 1u64 << (feature % 64));
            new += (self.bits[word] & bit == 0) as usize;
            self.bits[word] |= bit;
        }
        self.count += new;
        new
    }

    /// How many features are seen.
    pub(crate) fn count(&self) -> usize {
        self.count
    }
}
