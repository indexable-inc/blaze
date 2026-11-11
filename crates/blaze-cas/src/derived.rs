//! The recipe of a derived value: a file that is another the store holds (its base) with some
//! ranges of its bytes written over and a new length. A linked image that differs from the last
//! one by a few blocks is kept as its recipe, a few kilobytes under its own key
//! ([`crate::Kind::Derived`]), never as the whole file again.
//!
//! The recipe is an ordinary value of the store: put, made durable, hashed and read as every
//! value is. What it stands for is made only to be placed ([`crate::Store::place`]): the
//! store clones the whole file it keeps of the base, writes the ranges into the clone, sets its
//! length and renames it to the derived value's own whole file, which later places clone.
//! Nothing reads or hashes the base's bytes or the result's. The whole file is how the bytes are
//! held, never the record of them: one that a crash lost or tore is made again from the base's
//! (itself checked against its content, or made again from its recipe) and the recipe, so the
//! bytes placed are always the base's with the ranges written, in every run.
//!
//! The encoding, little endian: [`MAGIC`], the base's key (32 bytes), the file's length
//! (`u64`), the count of ranges (`u32`) and four zero bytes; then each range's offset (`u64`)
//! and length (`u32`) and four zero bytes, by offset, none overlapping, each ending by the
//! file's length; then the ranges' bytes in the same order.

#![forbid(unsafe_code)]

use std::fmt;

use crate::Key;
use crate::disk::{id_at, u32_at, u64_at};

/// What a recipe starts with, its format's number in it.
const MAGIC: &[u8; 16] = b"blaze derived 1\0";
/// The fixed part: the magic, the base, the length, the count and its padding.
const FIXED: usize = 16 + 32 + 8 + 4 + 4;
/// One range's row: its offset, its length and padding.
const ROW: usize = 8 + 4 + 4;

/// The most derived values in a row with no whole file kept of any that the store makes one
/// from the next: [`crate::Reader::derivable`] says a base at half of it is no base, so a
/// caller that asks keeps every chain shorter, and a place of a longer one is refused naming it
/// (a bound on the work one place does, and on how deep the store's steps nest).
pub const CHAIN_MOST: u32 = 128;

/// Why bytes are no recipe, or ranges make none: the rule and the range it is about.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BadRecipe(String);

impl fmt::Display for BadRecipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadRecipe {}

/// A recipe read in place.
#[derive(Clone, Copy, Debug)]
pub struct Recipe<'a> {
    /// The key of the value whose bytes it starts from.
    pub base: Key,
    /// The derived file's length.
    pub len: u64,
    rows: &'a [u8],
    bytes: &'a [u8],
}

impl<'a> Recipe<'a> {
    /// Each range: its offset in the file and the bytes written there, by offset.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, &'a [u8])> + use<'a> {
        let (rows, bytes) = (self.rows, self.bytes);
        let mut from = 0;
        rows.chunks_exact(ROW).map(move |row| {
            let (at, len) = (u64_at(row, 0), u32_at(row, 8) as usize);
            let range = &bytes[from..from + len];
            from += len;
            (at, range)
        })
    }

    /// How many ranges it writes.
    pub fn count(&self) -> usize {
        self.rows.len() / ROW
    }

    /// How many bytes its ranges write.
    pub fn written(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// The rule every recipe keeps, checked as one is made and as one is read: `ranges` (offset
/// and length) are in order, none overlaps the one before, and each ends by `len`.
fn check(len: u64, ranges: impl Iterator<Item = (u64, u64)>) -> Result<(), BadRecipe> {
    let mut end = 0u64;
    for (n, (at, bytes)) in ranges.enumerate() {
        if at < end {
            // alloc: an error.
            return Err(BadRecipe(format!(
                "range {n} at {at:#x} starts before {end:#x}, where the range before it ends: \
                 ranges are by offset and none overlaps"
            )));
        }
        end = match at.checked_add(bytes).filter(|end| *end <= len) {
            Some(end) => end,
            None => {
                // alloc: an error.
                return Err(BadRecipe(format!(
                    "range {n} of {bytes:#x} bytes at {at:#x} ends past the file's {len:#x} \
                     bytes: every range is inside the file"
                )));
            }
        };
    }
    Ok(())
}

/// The recipe of the file of `len` bytes that is `base`'s value with each of `writes` (offset
/// and bytes) written over it: what is put under the derived file's key as a
/// [`crate::Kind::Derived`]. `Err` names the range that is out of order, overlaps another, is
/// longer than a `u32` says or ends past `len`.
pub fn encode(base: &Key, len: u64, writes: &[(u64, Vec<u8>)]) -> Result<Vec<u8>, BadRecipe> {
    check(len, writes.iter().map(|(at, bytes)| (*at, bytes.len() as u64)))?;
    let count = u32::try_from(writes.len())
        // alloc: an error.
        .map_err(|_| BadRecipe(format!("{} ranges are more than a recipe counts", writes.len())))?;
    let payload: usize = writes.iter().map(|it| it.1.len()).sum();
    // alloc: the recipe, which the store's put takes.
    let mut out = Vec::with_capacity(FIXED + writes.len() * ROW + payload);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&base.0.0);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for (n, (at, bytes)) in writes.iter().enumerate() {
        let range = u32::try_from(bytes.len()).map_err(|_| {
            // alloc: an error.
            BadRecipe(format!(
                "range {n} at {at:#x} is {} bytes, more than a range's `u32` length says",
                bytes.len()
            ))
        })?;
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&range.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
    }
    for (_, bytes) in writes {
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

/// The recipe `bytes` hold, checked: its magic, its table inside it, its ranges in order and
/// inside the file, and its ranges' bytes all there and nothing after them.
pub fn decode(bytes: &[u8]) -> Result<Recipe<'_>, BadRecipe> {
    if bytes.len() < FIXED || bytes[..MAGIC.len()] != MAGIC[..] {
        // alloc: an error.
        return Err(BadRecipe(format!(
            "{} bytes that do not start as a recipe does: a derived value is its base's key, \
             its length and its ranges",
            bytes.len()
        )));
    }
    let base = Key(id_at(bytes, 16));
    let len = u64_at(bytes, 48);
    let count = u32_at(bytes, 56) as usize;
    let table = count.checked_mul(ROW).and_then(|it| it.checked_add(FIXED));
    let Some(table) = table.filter(|end| *end <= bytes.len()) else {
        // alloc: an error.
        return Err(BadRecipe(format!(
            "a table of {count} ranges does not fit its {} bytes",
            bytes.len()
        )));
    };
    let (rows, payload) = (&bytes[FIXED..table], &bytes[table..]);
    let lengths = || rows.chunks_exact(ROW).map(|row| (u64_at(row, 0), u64::from(u32_at(row, 8))));
    check(len, lengths())?;
    let total: u64 = lengths().map(|it| it.1).sum();
    if total != payload.len() as u64 {
        // alloc: an error.
        return Err(BadRecipe(format!(
            "its ranges are {total:#x} bytes in all and {:#x} bytes follow its table",
            payload.len()
        )));
    }
    Ok(Recipe { base, len, rows, bytes: payload })
}

/// The file a recipe of `ranges` and `len` gives of `base`: what the store's effects make of a
/// clone of the base's whole file, here on bytes in memory. For tests and models, which
/// compare a placed file against it; the store itself never holds the file's bytes.
pub fn applied(base: &[u8], len: u64, ranges: impl Iterator<Item = (u64, impl AsRef<[u8]>)>) -> Vec<u8> {
    // alloc: the whole file, for a test or a model.
    let mut out = base.to_vec();
    out.resize(len as usize, 0);
    for (at, bytes) in ranges {
        let (at, bytes) = (at as usize, bytes.as_ref());
        out[at..at + bytes.len()].copy_from_slice(bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use blaze_ids::ContentId;

    use super::*;

    fn key(n: u8) -> Key {
        Key(ContentId([n; 32]))
    }

    #[test]
    fn a_recipe_reads_back_as_it_was_made() {
        let writes = vec![(0, vec![1, 2, 3]), (8, Vec::new()), (4096, vec![9; 100])];
        let bytes = encode(&key(7), 5000, &writes).unwrap();
        let recipe = decode(&bytes).unwrap();
        assert_eq!((recipe.base, recipe.len, recipe.count(), recipe.written()), (key(7), 5000, 3, 103));
        let read: Vec<(u64, Vec<u8>)> = recipe.ranges().map(|(at, it)| (at, it.to_vec())).collect();
        assert_eq!(read, writes);
        // Shorter than its base, and longer: cut, and filled with zeros.
        let base = vec![5u8; 6000];
        let made = applied(&base, 5000, recipe.ranges());
        assert_eq!((made.len(), &made[..4], made[4096], made[4999]), (5000, &[1, 2, 3, 5][..], 9, 5));
        let grown = applied(&[5u8; 10], 5000, recipe.ranges());
        assert_eq!((grown[9], grown[10], grown[4195]), (5, 0, 9));
    }

    #[test]
    fn ranges_out_of_order_overlapping_or_past_the_end_are_refused_naming_the_range() {
        let refused = |len, writes: &[(u64, Vec<u8>)]| encode(&key(1), len, writes).unwrap_err().to_string();
        assert!(refused(100, &[(8, vec![0; 4]), (0, vec![0; 4])]).contains("range 1 at 0x0"));
        assert!(refused(100, &[(0, vec![0; 8]), (4, vec![0; 4])]).contains("range 1 at 0x4"));
        assert!(refused(10, &[(8, vec![0; 4])]).contains("ends past the file's 0xa bytes"));
        // Bytes cut short, bytes after the ranges and other bytes are no recipe.
        let whole = encode(&key(1), 100, &[(0, vec![7; 8])]).unwrap();
        assert!(decode(&whole[..whole.len() - 1]).is_err());
        let mut longer = whole.clone(); // clone: a test's bytes.
        longer.push(0);
        assert!(decode(&longer).is_err());
        assert!(decode(b"an output's own bytes, which are no recipe").is_err());
        assert!(decode(&whole).is_ok());
    }
}
