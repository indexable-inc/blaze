//! The static table (RFC 7541 Appendix A) and the dynamic table (section 2.3.2, 4).
//!
//! Section 2.3.3, the index address space: "Indices between 1 and the length of the static table
//! (inclusive) refer to elements in the static table. Indices strictly greater than the length of
//! the static table refer to elements in the dynamic table. The length of the static table is
//! subtracted to find the index into the dynamic table." The dynamic table is newest first: "The
//! first and newest entry in a dynamic table is at the lowest index".
//!
//! Section 4.1, size: "The size of an entry is the sum of its name's length in octets ..., its
//! value's length in octets, and 32." Section 4.4: "Before a new entry is added to the dynamic
//! table, entries are evicted from the end of the dynamic table until the size of the dynamic
//! table is less than or equal to (maximum size - new entry size) or until the table is empty. If
//! the size of the new entry is less than or equal to the maximum size, that entry is added to
//! the table. It is not an error to attempt to add an entry that is larger than the maximum size;
//! an attempt to add an entry larger than the maximum size causes the table to be emptied of all
//! existing entries and results in an empty table." Section 4.3: a lowered maximum evicts the
//! same way.
//!
//! Storage: the live entries' bytes lie contiguous and oldest first in one buffer, from a moving
//! start; an eviction only moves the start, and the buffer is compacted when its dead prefix is
//! longer than its live part, so it holds at most about twice the maximum size and each byte is
//! moved a bounded number of times. Every entry is one slice: no copy on a lookup.

#![blaze::sans_io]

use blaze_own::{ADeque, AVec, Alloc, AllocFailure, VecDeque};

use crate::plant::{self, Plant};

/// RFC 7541 Appendix A, in index order (index 1 first). TO VERIFY AGAINST THE RFC TEXT: written
/// from memory of the Appendix, not derived from blaze-http (QPACK's static table is another
/// table, RFC 9204 Appendix A). The Appendix C examples exercise entries 1, 2, 4, 5, 6, 7, 8,
/// 24, 26, 33, 46 and 55 byte for byte; the test `the_static_table_is_appendix_a` names a
/// sample of entries again by their index.
pub const STATIC: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

/// The static table's length.
pub const STATIC_LEN: u32 = 61;

/// An entry's overhead in the size accounting (section 4.1).
pub const ENTRY_OVERHEAD: u32 = 32;

/// The size of an entry of `name` and `value` (section 4.1), saturating.
pub fn entry_size(name: &[u8], value: &[u8]) -> u64 {
    // usize to u64 cannot lose bits on any target blaze builds for.
    u64::try_from(name.len()).unwrap_or(u64::MAX).saturating_add(u64::try_from(value.len()).unwrap_or(u64::MAX))
        .saturating_add(u64::from(ENTRY_OVERHEAD))
}

/// Which side owns a table: a planted bug touches only the decoder's, so the two tables of a
/// round trip part ways.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Encoder,
    Decoder,
}

/// One live entry: where its bytes start in the logical byte stream, and its lengths.
#[derive(Clone, Copy, Debug)]
struct Entry {
    at: u64,
    name: u32,
    value: u32,
}

/// A dynamic table.
pub struct Table<'a> {
    side: Side,
    /// Live bytes, oldest entry first, from `bytes[start]`. `bytes[0]` is logical offset
    /// `origin`.
    bytes: AVec<'a, u8>,
    origin: u64,
    /// The logical offset one past the newest entry's bytes.
    end: u64,
    /// Oldest first.
    entries: ADeque<'a, Entry>,
    /// The sum of the entries' sizes (section 4.1).
    size: u64,
    /// The maximum size (section 4.2).
    max: u32,
}

impl<'a> Table<'a> {
    /// An empty table of maximum size `max`.
    pub fn new(side: Side, max: u32, alloc: Alloc<'a>) -> Table<'a> {
        Table {
            side,
            bytes: AVec::new_in(alloc),
            origin: 0,
            end: 0,
            entries: VecDeque::new_in(alloc),
            size: 0,
            max,
        }
    }

    /// How many entries it holds.
    pub fn len(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }

    /// The sum of its entries' sizes (section 4.1).
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Its maximum size.
    pub fn max(&self) -> u32 {
        self.max
    }

    /// The entry at dynamic index `i` (1 is the newest), as (name, value).
    pub fn get(&self, i: u32) -> Option<(&[u8], &[u8])> {
        let len = self.entries.len();
        let i = usize::try_from(i).ok()?;
        if i == 0 || i > len {
            return None;
        }
        let entry = *self.entries.get(len - i)?;
        let start = usize::try_from(entry.at - self.origin).ok()?;
        let name_end = start + usize::try_from(entry.name).ok()?;
        let value_end = name_end + usize::try_from(entry.value).ok()?;
        Some((&self.bytes.as_slice()[start..name_end], &self.bytes.as_slice()[name_end..value_end]))
    }

    /// The entries newest first, as (name, value).
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> + '_ {
        (1..=self.len()).filter_map(move |i| self.get(i))
    }

    fn evict_oldest(&mut self) {
        if let Some(entry) = self.entries.pop_front() {
            self.size -= u64::from(entry.name) + u64::from(entry.value) + u64::from(ENTRY_OVERHEAD);
        }
        if self.entries.is_empty() {
            // Nothing live: the whole buffer is dead.
            self.bytes.clear();
            self.origin = self.end;
        }
    }

    /// Sets the maximum size, evicting the oldest entries until the table fits (section 4.3).
    pub fn set_max(&mut self, max: u32) {
        self.max = max;
        while self.size > u64::from(self.max) {
            self.evict_oldest();
        }
    }

    /// Adds an entry of `name` and `value` as section 4.4 says. `name` and `value` must not
    /// borrow this table (the caller copies a name taken from it first: an eviction here may
    /// drop the entry it came from, the section's caution).
    pub fn insert(&mut self, name: &[u8], value: &[u8]) -> Result<(), AllocFailure> {
        let new = entry_size(name, value);
        if new > u64::from(self.max) {
            while !self.entries.is_empty() {
                self.evict_oldest();
            }
            return Ok(());
        }
        // How many of the oldest entries must go so that size <= max - new.
        let mut evict = 0usize;
        let mut size = self.size;
        while size + new > u64::from(self.max) {
            let Some(entry) = self.entries.get(evict) else { break };
            size -= u64::from(entry.name) + u64::from(entry.value) + u64::from(ENTRY_OVERHEAD);
            evict += 1;
        }
        if evict > 0 && self.side == Side::Decoder && plant::on(Plant::EvictOneTooFew) {
            evict -= 1;
        }
        for _ in 0..evict {
            self.evict_oldest();
        }
        self.compact();
        let at = self.end;
        self.bytes.try_reserve(name.len() + value.len())?;
        self.bytes.try_extend_from_slice(name)?;
        self.bytes.try_extend_from_slice(value)?;
        let name_len = u32::try_from(name.len()).unwrap_or(u32::MAX);
        let value_len = u32::try_from(value.len()).unwrap_or(u32::MAX);
        if let Err(refused) = self.entries.try_push_back(Entry { at, name: name_len, value: value_len }) {
            // Take the bytes back so the table stays as it was before the call.
            let keep = self.bytes.len() - name.len() - value.len();
            self.bytes.truncate(keep);
            return Err(refused.why);
        }
        self.end += new - u64::from(ENTRY_OVERHEAD);
        self.size += new;
        Ok(())
    }

    /// Drops the dead prefix when it is longer than the live part.
    fn compact(&mut self) {
        let Some(oldest) = self.entries.front() else {
            self.bytes.clear();
            self.origin = self.end;
            return;
        };
        let dead = usize::try_from(oldest.at - self.origin).unwrap_or(0);
        let live = self.bytes.len() - dead;
        if dead > live && dead > 0 {
            self.bytes.drain(..dead);
            self.origin = oldest.at;
        }
    }
}

/// Where a field is found in the static table: a full match's index, else a name match's.
pub fn find_static(name: &[u8], value: &[u8]) -> (Option<u32>, Option<u32>) {
    let mut name_at = None;
    for (i, (n, v)) in STATIC.iter().enumerate() {
        if *n == name {
            let index = u32::try_from(i).unwrap_or(0) + 1;
            if *v == value {
                return (Some(index), name_at.or(Some(index)));
            }
            name_at = name_at.or(Some(index));
        }
    }
    (None, name_at)
}
