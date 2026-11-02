//! A binary fuse filter of 8-bit fingerprints (Graf and Lemire, "Binary Fuse Filters: Fast and
//! Smaller Than Xor Filters", 2022): a set of `u64` keys that says a key it was built of is
//! in it, always, and one it was not is in it with probability about 1/256, in about 9 bits a
//! key. A probe reads three bytes, one in each of three segments in a row, so one or two
//! cache lines at the sizes a store's keys come in.
//!
//! Each key hashes to three slots, one in each of segments `s`, `s + 1` and `s + 2`; it is in
//! the filter when the xor of its slots is its fingerprint. Building peels: a slot only one
//! key still hashes to is that key's to set, so the key is set last, after the keys that
//! share its other slots. A seed whose hashes do not peel is retried with the next one.
//!
//! The keys must already be uniform (the first word of a BLAKE3 key is). The hash is
//! [`mix`] of the key plus the seed, a bijection, so distinct keys never share a hash.

use crate::rng::{SplitMix64, mix};

#[cfg(test)]
mod tests;

/// The longest a segment is: past it, the three slots of a probe no longer share pages.
const SEGMENT_MOST: u32 = 1 << 18;
/// The first seed's stream, as the reference implementation starts it.
const SEEDS: u64 = 0x726b_2b9d_438b_9d4d;
/// Failed seeds after which the array grows a segment, so a build always ends.
const TRIES_PER_SIZE: u32 = 16;
/// How a filter starts its encoding: `seed`, segment length, segment count, fingerprints.
pub const HEADER: usize = 16;

#[derive(Clone, PartialEq, Eq)]
pub struct Fuse8 {
    seed: u64,
    segment_length: u32,
    /// Slots the first of a key's three slots falls in: the segment count times its length.
    first_slots: u32,
    /// `first_slots` plus two segments.
    fingerprints: Vec<u8>,
}

impl std::fmt::Debug for Fuse8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fuse8 {{ seed: {:#x}, {} bytes }}", self.seed, self.fingerprints.len())
    }
}

impl Fuse8 {
    /// The filter of `keys`; a key given twice is in it once.
    pub fn of(mut keys: Vec<u64>) -> Fuse8 {
        keys.sort_unstable();
        keys.dedup();
        let (segment_length, mut segments) = sizes(keys.len());
        let mut seeds = SplitMix64::new(SEEDS);
        let mut failed = 0;
        loop {
            let first_slots = segments.checked_mul(segment_length).expect("a filter of fewer than 2^31 keys");
            let mut filter = Fuse8 {
                seed: seeds.next_u64(),
                segment_length,
                first_slots,
                // alloc: the filter's fingerprints, once per build
                fingerprints: vec![0; (first_slots + 2 * segment_length) as usize],
            };
            if filter.fill(&keys) {
                return filter;
            }
            failed += 1;
            if failed % TRIES_PER_SIZE == 0 {
                segments += 1;
            }
        }
    }

    /// Whether `key` may be in it: always when it was built of `key`.
    #[inline]
    pub fn contains(&self, key: u64) -> bool {
        let hash = mix(key.wrapping_add(self.seed));
        let [a, b, c] = self.slots(hash);
        fingerprint(hash) ^ self.fingerprints[a] ^ self.fingerprints[b] ^ self.fingerprints[c] == 0
    }

    /// The bytes of its encoding.
    pub fn encoded_len(&self) -> usize {
        HEADER + self.fingerprints.len()
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.extend_from_slice(&self.segment_length.to_le_bytes());
        out.extend_from_slice(&(self.first_slots / self.segment_length).to_le_bytes());
        out.extend_from_slice(&self.fingerprints);
    }

    /// The filter `bytes` encode, all of them; refused, saying why, unless its sizes are ones
    /// a build makes and its fingerprints are exactly as long as they say.
    pub fn decode(bytes: &[u8]) -> Result<Fuse8, &'static str> {
        let Some((header, fingerprints)) = bytes.split_first_chunk::<HEADER>() else {
            return Err("is shorter than its header");
        };
        let word = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().expect("four bytes"));
        let seed = u64::from_le_bytes(header[..8].try_into().expect("eight bytes"));
        let (segment_length, segments) = (word(8), word(12));
        if !segment_length.is_power_of_two() || segment_length > SEGMENT_MOST {
            return Err("has a segment length that is not a power of two up to 2^18");
        }
        let Some(first_slots) = segments.checked_mul(segment_length).filter(|_| segments > 0) else {
            return Err("has no segments or more than 2^32 slots");
        };
        if fingerprints.len() as u64 != u64::from(first_slots) + 2 * u64::from(segment_length) {
            return Err("has fingerprints of another length than its segments take");
        }
        // alloc: the decoded filter owns its fingerprints, once per filter received
        Ok(Fuse8 { seed, segment_length, first_slots, fingerprints: fingerprints.to_vec() })
    }

    /// The three slots of `hash`: the first by its high bits across the first slots, the
    /// other two one and two segments on, moved within their segment by other bits of it.
    #[inline]
    fn slots(&self, hash: u64) -> [usize; 3] {
        let first = ((u128::from(hash) * u128::from(self.first_slots)) >> 64) as u64;
        let length = u64::from(self.segment_length);
        let mask = length - 1;
        [
            first as usize,
            ((first + length) ^ ((hash >> 18) & mask)) as usize,
            ((first + 2 * length) ^ (hash & mask)) as usize,
        ]
    }

    /// Sets the fingerprints so each of `keys` (distinct) is in it; false when its hashes
    /// under this seed do not peel.
    fn fill(&mut self, keys: &[u64]) -> bool {
        let slots = self.fingerprints.len();
        // alloc: a build's scratch, once per seed tried
        let mut hashes: Vec<u64> = keys.iter().map(|&key| mix(key.wrapping_add(self.seed))).collect();
        // In order of first slot, so the counts fill a segment at a time.
        hashes.sort_unstable();
        // Per slot: the keys hashing to it times four, plus the xor of which of each key's
        // three slots it is; and the xor of their hashes, which is the key's when one is left.
        // alloc: a build's scratch, once per seed tried
        let mut count = vec![0u8; slots];
        // alloc: a build's scratch, once per seed tried
        let mut xor = vec![0u64; slots];
        for &hash in &hashes {
            for (which, at) in self.slots(hash).into_iter().enumerate() {
                if count[at] >= u8::MAX - 4 {
                    return false;
                }
                count[at] = (count[at] + 4) ^ which as u8;
                xor[at] ^= hash;
            }
        }
        // alloc: a build's scratch, once per seed tried
        let mut alone: Vec<u32> = (0..slots as u32).filter(|&at| count[at as usize] >> 2 == 1).collect();
        // alloc: a build's scratch, once per seed tried
        let mut peeled: Vec<(u64, u8)> = Vec::with_capacity(keys.len());
        while let Some(at) = alone.pop() {
            let at = at as usize;
            if count[at] >> 2 != 1 {
                continue;
            }
            let (hash, found) = (xor[at], count[at] & 3);
            peeled.push((hash, found));
            for (which, other) in self.slots(hash).into_iter().enumerate() {
                if which as u8 != found {
                    count[other] = (count[other] - 4) ^ which as u8;
                    xor[other] ^= hash;
                    if count[other] >> 2 == 1 {
                        alone.push(other as u32);
                    }
                }
            }
        }
        if peeled.len() != keys.len() {
            return false;
        }
        for &(hash, found) in peeled.iter().rev() {
            let slots = self.slots(hash);
            let mut value = fingerprint(hash);
            for (which, at) in slots.into_iter().enumerate() {
                if which as u8 != found {
                    value ^= self.fingerprints[at];
                }
            }
            self.fingerprints[slots[found as usize]] = value;
        }
        true
    }
}

#[inline]
fn fingerprint(hash: u64) -> u8 {
    (hash ^ (hash >> 32)) as u8
}

/// The segment length and count for `keys` keys, as the reference sizes a filter of three
/// slots a key: segments longer as the filter grows, and a little more room than keys, more
/// for a small filter, which peels less readily.
fn sizes(keys: usize) -> (u32, u32) {
    if keys <= 1 {
        return (4, 1);
    }
    let size = keys as f64;
    let length = (1u32 << ((size.ln() / 3.33f64.ln() + 2.25).floor() as u32).min(18)).min(SEGMENT_MOST);
    let factor = (0.875 + 0.25 * 1e6f64.ln() / size.ln()).max(1.125);
    let room = (size * factor).round() as u64;
    let segments = room.div_ceil(u64::from(length)).saturating_sub(2).max(1);
    (length, u32::try_from(segments).expect("a filter of fewer than 2^31 keys"))
}
