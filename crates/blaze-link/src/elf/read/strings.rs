//! ELF string tables (`.shstrtab`, `.strtab`, `.dynstr`) read into a file's [`Names`], each
//! string pushed once.
//!
//! A name at any offset inside a string is a suffix of it: a tail-merged table (LLVM's, lld's)
//! names `.text` inside `.rela.text`, and a hostile one may name every offset of one long
//! string. So each string is pushed whole the first time any offset in it is named, and every
//! name is a [`Names::suffix`] of it: the arena holds at most the table's bytes, whatever the
//! headers say. A string is found by the rank of the NUL that ends it (a bitset of the NULs and
//! a count per word), which is direct indexing: nothing is hashed, so no input can make a
//! lookup slow.
//!
//! The input's name is taken as [`fmt::Display`] and formatted only into an error.

use std::fmt;

use blaze_ids::bits::{BitSet, rank_in};

use crate::model::{Name, Names};

/// The string ending at this NUL is not pushed yet.
const UNSEEN: u32 = u32::MAX;

pub(super) struct Table<'data> {
    bytes: &'data [u8],
    /// Whether byte `i` is a NUL. 1 bit per byte.
    nuls: BitSet,
    /// The NULs before each word of `nuls` (`blaze_ids::bits::ranks_of`'s counts, made in the
    /// pass that finds the NULs): 4 bytes per 64 bytes of table.
    before: Vec<u32>,
    /// By the rank of a NUL, the index in `pushed` of the string ending there, or [`UNSEEN`]:
    /// 4 bytes per string of the table.
    strings: Vec<u32>,
    /// Each string pushed: where in the table its pushed part starts, and its name. 12 bytes
    /// per string named, reserved for every string so it never grows.
    pushed: Vec<(u32, Name)>,
}

impl<'data> Table<'data> {
    /// The table whose bytes are `bytes` (its section's data), in the input `file`. Four
    /// buffers, none for an empty table.
    pub fn new(file: impl fmt::Display, bytes: &'data [u8]) -> Result<Table<'data>, String> {
        if u32::try_from(bytes.len()).is_err() {
            return Err(format!("{file}: a string table of more than 4 GiB")); // alloc: error path
        }
        let words = bytes.len().div_ceil(64);
        let mut nuls = Vec::with_capacity(words);
        let mut before = Vec::with_capacity(words);
        let mut count = 0u32;
        for chunk in bytes.chunks(64) {
            let word = nul_mask(chunk);
            nuls.push(word);
            before.push(count);
            count += word.count_ones();
        }
        Ok(Table {
            bytes,
            nuls: BitSet::from_words(nuls),
            before,
            // alloc: once per string table, a slot per string
            strings: vec![UNSEEN; count as usize],
            pushed: Vec::with_capacity(count as usize),
        })
    }

    /// Its size in bytes: the most its strings add to an arena.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// The name at `offset` in the table (`sh_name`, `st_name`), pushed into `names` if its
    /// string is not there yet; `what` says what it names, for an error.
    pub fn name(
        &mut self,
        names: &mut Names,
        file: impl fmt::Display,
        what: &str,
        offset: u32,
    ) -> Result<Name, String> {
        let bytes = self.bytes;
        let at = offset as usize;
        let Some(end) = self.next_nul(at) else {
            // alloc: error path
            return Err(format!(
                "{file}: {what} at offset {offset} has no end in its {}-byte string table",
                bytes.len()
            ));
        };
        // alloc: error path, called only for an error
        let not_utf8 = || format!("{file}: {what} {} is not UTF-8", String::from_utf8_lossy(&bytes[at..end]));
        let rank = self.rank(end);
        let slot = self.strings[rank];
        let (from, name) = match slot {
            UNSEEN => {
                // The whole string from the NUL before it, less any bytes up to its last one
                // that is not UTF-8. UTF-8 is self-synchronizing, so a name inside the string is
                // UTF-8 exactly when it starts at a character boundary of what is left.
                let mut from = self.previous_nul(at).map_or(0, |nul| nul + 1);
                let text = loop {
                    match std::str::from_utf8(&bytes[from..end]) {
                        Ok(text) => break text,
                        Err(error) => match error.error_len() {
                            Some(len) => from += error.valid_up_to() + len,
                            None => from = end,
                        },
                    }
                };
                let name = names.push(text).map_err(|why| format!("{file}: {why}"))?; // alloc: error path
                self.strings[rank] = self.pushed.len() as u32;
                self.pushed.push((from as u32, name));
                (from as u32, name)
            }
            _ => self.pushed[slot as usize],
        };
        let skip = offset.checked_sub(from).ok_or_else(not_utf8)?;
        names.suffix(name, skip).ok_or_else(not_utf8)
    }

    /// The first NUL at or after `at`.
    fn next_nul(&self, at: usize) -> Option<usize> {
        self.nuls.first_in(at..self.bytes.len())
    }

    /// The last NUL before `at`, which is inside the table.
    fn previous_nul(&self, at: usize) -> Option<usize> {
        self.nuls.last_in(0..at)
    }

    /// How many NULs come before the one at `nul`.
    fn rank(&self, nul: usize) -> usize {
        rank_in(self.nuls.words(), &self.before, nul)
    }
}

/// Bit `i` set exactly where `chunk[i]` (at most 64 bytes) is NUL: eight bytes a step, about
/// 9 integer operations where a byte at a time takes 32.
fn nul_mask(chunk: &[u8]) -> u64 {
    const LOW7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    // Bit 8k (k = 0..8) times this lands on bit 56 + k, every other product on a bit of its
    // own outside 56..64, so nothing carries into the top byte.
    const GATHER: u64 = 0x0102_0408_1020_4080;
    debug_assert!(chunk.len() <= 64);
    let mut word = 0u64;
    let mut lanes = chunk.chunks_exact(8);
    for (lane, bytes) in (&mut lanes).enumerate() {
        let x = u64::from_le_bytes(bytes.try_into().expect("8 bytes"));
        // A byte's high bit ends up set exactly when the byte is 0: its low 7 bits plus 0x7f
        // reach the high bit unless they are 0, without carrying out of the byte, and `| x`
        // covers a set high bit.
        let zero = !((x & LOW7).wrapping_add(LOW7) | x | LOW7);
        word |= ((zero >> 7).wrapping_mul(GATHER) >> 56) << (8 * lane);
    }
    let tail = lanes.remainder();
    let start = chunk.len() - tail.len();
    for (bit, &byte) in tail.iter().enumerate() {
        word |= u64::from(byte == 0) << (start + bit);
    }
    word
}

#[cfg(test)]
mod tests {
    use super::nul_mask;

    /// The byte-at-a-time loop the table was built with before.
    fn reference(chunk: &[u8]) -> u64 {
        let mut word = 0u64;
        for (bit, &byte) in chunk.iter().enumerate() {
            word |= u64::from(byte == 0) << bit;
        }
        word
    }

    #[test]
    fn the_mask_is_the_byte_loop() {
        // Every byte value at every place, beside a zero and a 0x80 (the carries' edge cases).
        for value in 0..=255u8 {
            for at in 0..64 {
                for len in [at + 1, 64] {
                    let mut chunk = [0x80u8; 64];
                    chunk[at] = value;
                    if at > 0 {
                        chunk[at - 1] = 0;
                    }
                    let chunk = &chunk[..len];
                    assert_eq!(nul_mask(chunk), reference(chunk), "{value:#x} at {at}, {len} bytes");
                }
            }
        }
        let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        let mut next = || rng.next_u64();
        for _ in 0..20_000 {
            let len = (next() % 65) as usize;
            let density = next() % 4;
            let chunk: Vec<u8> = (0..len)
                .map(|_| {
                    let it = next();
                    if it % 4 < density { 0 } else { (it >> 8) as u8 }
                })
                .collect();
            assert_eq!(nul_mask(&chunk), reference(&chunk), "{chunk:?}");
        }
        assert_eq!(nul_mask(&[]), 0);
    }
}
