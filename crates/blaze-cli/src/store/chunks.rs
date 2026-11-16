//! A command's own records before its seal, held so that a put moves each byte once
//! (drive.md, "The system side", "Store").
//!
//! What it replaces, and why (2026-11-16, `blaze check` on blaze's own workspace: "effect put
//! 114429x 2438944629" ns, 832 of 849 samples of `PackStore::put` in `memmove`): the store kept
//! its rows in one vector SORTED by key, so each put shifted half the rows: 48 bytes a row,
//! n^2/4 rows moved over n puts, about 157 GB at n = 114,429. The record's own copy was
//! milliseconds of it.
//!
//! The form here:
//! - a record's bytes are appended to the last of a list of fixed-size chunks and never move
//!   again (a record never straddles two chunks; one longer than a chunk has its own);
//! - its row is appended, in put order, to the last of a list of fixed-size blocks: no row
//!   moves either, and nothing is sorted until the seal;
//! - a lookup between puts ([`Records::find`], asked by `held` and `get_into`) goes through an
//!   open-addressed table of row numbers, keyed by the key's first 8 bytes (a key is a BLAKE3
//!   digest: uniform), the whole key compared on a hit. The table is the one thing that moves
//!   when it doubles: 4 bytes a row, counted in [`Moved::index`];
//! - the seal ([`PackStore::seal_records`]) sorts row NUMBERS once, hashes each payload once
//!   and hands the chunks' own slices to one gathered publish ([`Content::parts`]): no pack is
//!   built in memory first. The bytes on disk are those `pack_bytes` makes (rule 25).
//!
//! The cost that is left of a put, by name: one copy of the record into its chunk; one probe
//! of the table (a cache miss); a binary search in each pack the open listed (`Pack::find`,
//! up to `packs_most`); and, in the chain, `Sharing::place`, which opens the record to route
//! it. Of a seal: one hash of every byte, one copy of every byte into the effect that writes
//! it, the sort of the row numbers.
//!
//! A record may be built in place ([`Records::reserve`]): the range is handed out and the row
//! is filed only by [`Reserved::commit`], so a half-written record has no row.

use core::num::NonZero;

use blaze_drive::Key;
use blaze_ids::Hasher;
use blaze_own::alloc_ref::SharedAlloc;
use blaze_own::{AllocFailure, Vec};
use bstd::cx::Cx;
use bstd::fs::{Content, Durable};

use super::{CliError, FOOTER, FORMAT, MAGIC, PackStore, ROW, RemoteError, StorePlant, Untaken, index_number, pack_name};

/// The domain of a pack's payloads' digest (the footer's).
pub(super) const PAYLOADS: &str = "blaze-cli pack payloads";
/// A chunk of record bytes on a command.
pub(crate) const CHUNK: usize = 1 << 20;
/// The store's chunk in this build: small under test, so the simulations cross chunk ends and
/// meet records longer than one.
pub(crate) const CHUNK_HERE: usize = if cfg!(test) { 1 << 10 } else { CHUNK };
/// The most bytes one append of the seal's gathered publish holds: small under test, so the
/// crash simulations cut between a pack's appends.
pub(crate) const PART: usize = if cfg!(test) { 512 } else { 1 << 20 };
/// Rows a block.
const BLOCK: usize = 4096;
/// The table's first size; always a power of two, at most half full.
const SLOTS_FIRST: usize = 1024;
/// What pads a payload to 8 bytes in a pack.
static ZEROS: [u8; 8] = [0; 8];

/// Why a record was not taken, or a seal not made: the record's key and the rule.
#[derive(Debug)]
pub enum PutError {
    /// Memory for the record or its row could not be had (rule 3: nothing is dropped quietly).
    NoRoom { key: Key, len: usize, why: AllocFailure },
    /// A pack's row holds a 32-bit length.
    TooLong { key: Key, len: usize },
    /// A command's rows are numbered in 32 bits.
    TooMany { key: Key },
    /// Memory for the seal's index of `records` rows could not be had.
    SealNoRoom { records: u32, why: AllocFailure },
    /// Memory for the merge's (or the compaction rule's) table of `rows` index rows over
    /// `packs` packs could not be had, in the folder `folder` (store/compact.rs).
    MergeNoRoom { folder: &'static str, packs: u32, rows: u64 },
}

impl core::fmt::Display for PutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PutError::NoRoom { key, len, why } => write!(f, "the store's put of {key} ({len} bytes): no memory for it (rule 3, no-fallback: a record is never dropped quietly): {why}"),
            PutError::TooLong { key, len } => write!(f, "the store's put of {key}: {len} bytes, and a pack's row holds a 32-bit length (a record past 4 GiB)"),
            PutError::TooMany { key } => write!(f, "the store's put of {key}: past 2^32 records in one command (rule 18, bounded-progress: the table's cap)"),
            PutError::SealNoRoom { records, why } => write!(f, "the store's seal of {records} records: no memory for the pack's index (rule 3, no-fallback): {why}"),
            PutError::MergeNoRoom { folder, packs, rows } => write!(f, "the store's merge of {packs} packs under {folder}: no memory for the table of their {rows} index rows (48 bytes each; rule 3, no-fallback: nothing is merged, every pack stays readable)"),
        }
    }
}

fn refused(error: PutError) -> CliError {
    CliError::Remote(RemoteError::Put(error))
}

/// What a put did to memory, counted: the proof that a byte moves once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Moved {
    /// Record bytes copied from a caller into a chunk: each byte put, once.
    pub payload: u64,
    /// Bytes zeroed for a reservation before its maker wrote them.
    pub zeroed: u64,
    /// Index bytes written again when the table doubled: 4 a row a doubling.
    pub index: u64,
}

#[derive(Clone, Copy)]
struct Row {
    key: Key,
    chunk: u32,
    at: u32,
    len: u32,
}

enum Probe {
    Held(u32),
    Free(usize),
    /// No free slot in a whole turn of the table: never while it is at most half full.
    Full,
}

fn home(key: Key, mask: usize) -> usize {
    let b = &key.0.0;
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as usize & mask
}

/// A command's records: see the module.
pub(crate) struct Records {
    chunk: usize,
    /// Each made with its whole capacity and never grown: its bytes never move.
    chunks: Vec<Vec<u8, SharedAlloc<'static>>, SharedAlloc<'static>>,
    /// The chunk appends go to; a record longer than a chunk does not change it.
    open: Option<u32>,
    /// Blocks of [`BLOCK`] rows, in put order.
    rows: Vec<Vec<Row, SharedAlloc<'static>>, SharedAlloc<'static>>,
    count: u32,
    /// A row's number plus one, or 0.
    slots: Vec<u32, SharedAlloc<'static>>,
    moved: Moved,
}

impl Records {
    /// No records, nothing allocated; `chunk` bytes a chunk ([`CHUNK`]).
    pub(crate) fn new(chunk: usize) -> Records {
        let alloc = SharedAlloc::global();
        Records { chunk: chunk.max(1), chunks: Vec::new_in(alloc), open: None, rows: Vec::new_in(alloc), count: 0, slots: Vec::new_in(alloc), moved: Moved::default() }
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn moved(&self) -> Moved {
        self.moved
    }

    fn row(&self, row: u32) -> Row {
        self.rows[row as usize / BLOCK][row as usize % BLOCK]
    }

    /// Row `row`'s key, rows numbered in put order.
    pub(crate) fn key(&self, row: u32) -> Key {
        self.row(row).key
    }

    /// Row `row`'s key and bytes, in place.
    pub(crate) fn record(&self, row: u32) -> (Key, &[u8]) {
        let row = self.row(row);
        (row.key, &self.chunks[row.chunk as usize][row.at as usize..row.at as usize + row.len as usize])
    }

    fn probe(&self, key: Key) -> Probe {
        if self.slots.is_empty() {
            return Probe::Full;
        }
        let mask = self.slots.len() - 1;
        let mut slot = home(key, mask);
        for _ in 0..self.slots.len() {
            match self.slots[slot] {
                0 => return Probe::Free(slot),
                held if self.row(held - 1).key == key => return Probe::Held(held - 1),
                _ => slot = (slot + 1) & mask,
            }
        }
        Probe::Full
    }

    /// `key`'s bytes, in place, if this command put it.
    pub(crate) fn find(&self, key: Key) -> Option<&[u8]> {
        match self.probe(key) {
            Probe::Held(row) => Some(self.record(row).1),
            Probe::Free(_) | Probe::Full => None,
        }
    }

    /// Room for one more row and its slot, then where `key` is or would go. The slot stays
    /// true until the next row is filed.
    fn room_for_row(&mut self, key: Key, len: usize) -> Result<Probe, PutError> {
        let alloc = SharedAlloc::global();
        let no_room = |why: AllocFailure| PutError::NoRoom { key, len, why };
        if self.count == u32::MAX - 1 {
            return Err(PutError::TooMany { key });
        }
        if self.count as usize % BLOCK == 0 && self.rows.len() == self.count as usize / BLOCK {
            let block = Vec::try_with_capacity_in(BLOCK, alloc).map_err(no_room)?;
            self.rows.try_push(block).map_err(|refused| no_room(refused.why))?;
        }
        if (self.count as usize + 1) * 2 > self.slots.len() {
            let len = (self.slots.len() * 2).max(SLOTS_FIRST);
            let mut slots: Vec<u32, SharedAlloc<'static>> = Vec::try_with_capacity_in(len, alloc).map_err(no_room)?;
            slots.resize(len, 0);
            let mask = len - 1;
            for row in 0..self.count {
                let mut slot = home(self.row(row).key, mask);
                // At most half full: a free slot within one turn.
                for _ in 0..len {
                    if slots[slot] == 0 {
                        break;
                    }
                    slot = (slot + 1) & mask;
                }
                slots[slot] = row + 1;
            }
            self.moved.index += 4 * u64::from(self.count);
            self.slots = slots;
        }
        Ok(self.probe(key))
    }

    /// The chunk with `len` bytes free at its end: the open one, a new one, or one of its own
    /// for a record longer than a chunk.
    fn room(&mut self, key: Key, len: usize) -> Result<u32, PutError> {
        let alloc = SharedAlloc::global();
        let no_room = |why: AllocFailure| PutError::NoRoom { key, len, why };
        if let Some(open) = self.open {
            if self.chunk - self.chunks[open as usize].len() >= len {
                return Ok(open);
            }
        }
        let at = u32::try_from(self.chunks.len()).map_err(|_| PutError::TooMany { key })?;
        let made = Vec::try_with_capacity_in(len.max(self.chunk), alloc).map_err(no_room)?;
        self.chunks.try_push(made).map_err(|refused| no_room(refused.why))?;
        if len <= self.chunk {
            self.open = Some(at);
        }
        Ok(at)
    }

    /// `row` filed at the slot [`Records::room_for_row`] gave for its key.
    fn file(&mut self, slot: usize, row: Row) -> Result<(), PutError> {
        let block = self.count as usize / BLOCK;
        self.rows[block].try_push(row).map_err(|refused| PutError::NoRoom { key: row.key, len: row.len as usize, why: refused.why })?;
        self.count += 1;
        self.slots[slot] = self.count;
        Ok(())
    }

    /// `bytes` held as `key`'s record: copied once, to where they stay. `false` when this
    /// command already put the key (a key holds one value, rule 21): nothing is copied.
    pub(crate) fn put(&mut self, key: Key, bytes: &[u8]) -> Result<bool, PutError> {
        let len = u32::try_from(bytes.len()).map_err(|_| PutError::TooLong { key, len: bytes.len() })?;
        let slot = match self.room_for_row(key, bytes.len())? {
            Probe::Held(_) => return Ok(false),
            Probe::Free(slot) => slot,
            Probe::Full => return Err(PutError::TooMany { key }),
        };
        let chunk = self.room(key, bytes.len())?;
        let into = &mut self.chunks[chunk as usize];
        let at = into.len() as u32;
        // Within the chunk's capacity: nothing is allocated and nothing moves.
        into.try_extend_from_slice(bytes).map_err(|why| PutError::NoRoom { key, len: bytes.len(), why })?;
        self.moved.payload += bytes.len() as u64;
        self.file(slot, Row { key, chunk, at, len })?;
        Ok(true)
    }

    /// `len` zeroed bytes at the tail, for `key`'s record to be built in: no row until
    /// [`Reserved::commit`]. `None` when this command already put the key.
    pub(crate) fn reserve(&mut self, key: Key, len: usize) -> Result<Option<Reserved<'_>>, PutError> {
        let len32 = u32::try_from(len).map_err(|_| PutError::TooLong { key, len })?;
        let slot = match self.room_for_row(key, len)? {
            Probe::Held(_) => return Ok(None),
            Probe::Free(slot) => slot,
            Probe::Full => return Err(PutError::TooMany { key }),
        };
        let chunk = self.room(key, len)?;
        let into = &mut self.chunks[chunk as usize];
        let at = into.len();
        // Within the chunk's capacity.
        into.resize(at + len, 0);
        self.moved.zeroed += len as u64;
        Ok(Some(Reserved { records: self, slot, row: Row { key, chunk, at: at as u32, len: len32 }, filed: false }))
    }
}

/// A record being built in the store's own bytes. Dropped without [`Reserved::commit`], its
/// bytes are given back and no row was ever filed: a half-written record is not a record.
pub(crate) struct Reserved<'r> {
    records: &'r mut Records,
    slot: usize,
    row: Row,
    filed: bool,
}

impl Reserved<'_> {
    /// The record's bytes, zeroed, to write.
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (at, len) = (self.row.at as usize, self.row.len as usize);
        &mut self.records.chunks[self.row.chunk as usize][at..at + len]
    }

    /// The record is whole: its row is filed.
    pub(crate) fn commit(mut self) -> Result<(), PutError> {
        self.records.file(self.slot, self.row)?;
        self.filed = true;
        Ok(())
    }
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        if !self.filed {
            self.records.chunks[self.row.chunk as usize].truncate(self.row.at as usize);
        }
    }
}

impl PackStore {
    /// `key`'s record put by this command, if it is new to the store.
    pub(super) fn put_record(&mut self, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        if self.packs.iter().any(|(_, pack)| pack.find(key).is_some()) {
            return Ok(());
        }
        self.own.put(key, bytes).map(|_| ()).map_err(refused)
    }

    /// `len` bytes of the command's pack to build `key`'s record in, for a maker that would
    /// otherwise build it elsewhere and have [`Store::put`](super::Store::put) copy it: write
    /// [`Reserved::bytes`], then [`Reserved::commit`]. `None` when the key is held (rule 21:
    /// it is not written again). Dropped uncommitted, nothing was put.
    pub(crate) fn reserve(&mut self, key: Key, len: usize) -> Result<Option<Reserved<'_>>, CliError> {
        if self.packs.iter().any(|(_, pack)| pack.find(key).is_some()) {
            return Ok(None);
        }
        self.own.reserve(key, len).map_err(refused)
    }

    /// The pack `number`, just published, taken as known whole in this boot.
    pub(super) fn take_pack(&mut self, cx: &Cx, number: u64) -> Result<(), CliError> {
        let taken = PackStore::read_pack(cx, &self.packs_dir, number, false, &self.scratch, self.plant);
        self.scratch.reset();
        match taken {
            Ok(pack) => self.packs.push((number, pack)),
            // Another command's merge took it already: its records are in that merge's pack.
            Err(Untaken::Gone) => {}
            Err(Untaken::Refused) => return Err(CliError::Text { what: "a pack under .blaze/v2/packs read back as torn just after its publish" }),
            Err(Untaken::Failed(error)) => return Err(error),
        }
        Ok(())
    }

    /// This command's records as one pack: the bytes `pack_bytes` makes of them (payloads in
    /// key order, each at an 8-aligned offset, the index, the footer), written by one gathered
    /// publish of the records where they lie. The order of the writes is the proof's subject:
    /// the pack's name holds index and payloads together or not at all (one rename), and the
    /// boot record names the pack only after.
    pub(super) fn seal_records(&mut self, cx: &Cx) -> Result<(), CliError> {
        let count = self.own.count();
        if count == 0 {
            return Ok(());
        }
        let cx = cx.span("store seal");
        let alloc = SharedAlloc::global();
        let no_room = |why: AllocFailure| refused(PutError::SealNoRoom { records: count, why });
        let mut order: Vec<u32, SharedAlloc<'static>> = Vec::try_with_capacity_in(count as usize, alloc).map_err(no_room)?;
        for row in 0..count {
            order.try_push(row).map_err(|refused| no_room(refused.why))?;
        }
        {
            let own = &self.own;
            order.sort_unstable_by(|a, b| own.key(*a).cmp(&own.key(*b)));
        }
        // The index and footer, and the payloads' digest: each byte hashed once, none copied.
        let mut tail: Vec<u8, SharedAlloc<'static>> = Vec::try_with_capacity_in(count as usize * ROW + FOOTER, alloc).map_err(no_room)?;
        let mut hasher = Hasher::new(PAYLOADS);
        let mut at = 0u64;
        for row in order.iter() {
            let (key, bytes) = self.own.record(*row);
            let pad = bytes.len().next_multiple_of(8) - bytes.len();
            hasher.update(bytes).update(&ZEROS[..pad]);
            tail.try_extend_from_slice(&key.0.0).map_err(no_room)?;
            tail.try_extend_from_slice(&at.to_le_bytes()).map_err(no_room)?;
            tail.try_extend_from_slice(&(bytes.len() as u32).to_le_bytes()).map_err(no_room)?;
            tail.try_extend_from_slice(&[0; 4]).map_err(no_room)?;
            at += (bytes.len() + pad) as u64;
        }
        tail.try_extend_from_slice(MAGIC).map_err(no_room)?;
        tail.try_extend_from_slice(&FORMAT.to_le_bytes()).map_err(no_room)?;
        tail.try_extend_from_slice(&u64::from(count).to_le_bytes()).map_err(no_room)?;
        tail.try_extend_from_slice(&at.to_le_bytes()).map_err(no_room)?;
        tail.try_extend_from_slice(&hasher.finish().0).map_err(no_room)?;
        let number = index_number(tail.as_slice());
        let name = pack_name(number)?;
        let most = NonZero::new(PART).unwrap_or(NonZero::<usize>::MIN);
        if self.plant == StorePlant::IndexBeforePayloads {
            // The planted bug: the pack's name holds its index and footer over a hole where the
            // payloads go, and the boot record vouches for it, before the payloads are written.
            let mut hole: Vec<u8, SharedAlloc<'static>> = Vec::try_with_capacity_in(at as usize, alloc).map_err(no_room)?;
            hole.resize(at as usize, 0);
            let first = [hole.as_slice(), tail.as_slice()];
            self.packs_dir.publish(&cx, &name, Content::parts(&first, most), Durable::No)?;
            let before = self.packs.len();
            self.take_pack(&cx, number)?;
            self.write_boot(&cx)?;
            self.packs.truncate(before);
        }
        {
            let mut parts: Vec<&[u8], SharedAlloc<'static>> = Vec::try_with_capacity_in(count as usize * 2 + 1, alloc).map_err(no_room)?;
            for row in order.iter() {
                let bytes = self.own.record(*row).1;
                let pad = bytes.len().next_multiple_of(8) - bytes.len();
                if !bytes.is_empty() {
                    parts.try_push(bytes).map_err(|refused| no_room(refused.why))?;
                }
                if pad > 0 {
                    parts.try_push(&ZEROS[..pad]).map_err(|refused| no_room(refused.why))?;
                }
            }
            parts.try_push(tail.as_slice()).map_err(|refused| no_room(refused.why))?;
            self.packs_dir.publish(&cx, &name, Content::parts(parts.as_slice(), most), Durable::No)?;
        }
        self.take_pack(&cx, number)?;
        self.write_boot(&cx)?;
        self.own = Records::new(self.own.chunk);
        Ok(())
    }
}

#[cfg(test)]
#[path = "chunks_tests.rs"]
mod tests;
