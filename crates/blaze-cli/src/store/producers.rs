//! Who made each record of a pack: pack format 3 (remote-read.md, "End state: what a record's
//! key says about its producer", piece 7).
//!
//! A producer is (the record's kind tag, a 32-byte tool id). Until remote-read piece 1 lands
//! (`Producers` in blaze-drive key.rs), the tool id is today's [`Tool`] for every kind: the one
//! the command keys its records by, named to the store by [`PackStore::name_tool`]. The kind
//! tag is the record's own, read by blaze-drive `record::open` (as `Sharing::place` reads it);
//! a record that is not a drive record (a certificate, a root, an answer page) has tag
//! [`NOT_A_DRIVE_RECORD`], which no `Kind` is (their discriminants start at 1). Certificates and
//! pages keep their keys: no `digest()` until piece 1.
//!
//! The layout (format 3), every byte under the pack's two digests:
//!
//! ```text
//! payloads (each 8-aligned) | producer table | rows | footer
//!                            ^ table_at       ^ index_at
//! ```
//!
//! - the producer table: [`ENTRY`]-byte entries (kind tag u16 LE, tool id), sorted by
//!   (kind, tool) and without repeats, then zeros to a multiple of 8. It lies before the index,
//!   so the footer's payloads digest covers it, and the open reads none of it: a get pays
//!   nothing for it (store/scale_tests.rs `one_get_reads_its_record_and_the_indexes_only`
//!   keeps its count). It is read with one positioned read when asked ([`Pack::producers`]):
//!   by the merge, and by the collector (remote-read piece 10).
//! - a row: key (32), offset (u64), length (u32), the producer's index in the table (u16), two
//!   zero bytes: [`ROW`](super::ROW) stays 48.
//! - the footer: `BLZP`, format 3 (u32), the row count (u32), the producer count (u32), the
//!   index's offset (u64), the payloads digest (32): [`FOOTER`](super::FOOTER) stays 56.
//!
//! Placed at the pack's head, the table would need a leading slice the merge's gathered publish
//! (`Content::parts_of`: file ranges, then one tail) has no form for; at the payloads' end it
//! is part of that tail and the head stays the first record.
//!
//! A pack of an older format with a valid footer of its own format (1 or 2) is OLD: refused
//! by name at the open ([`PackError::OldFormat`]), never read, never merged from, never removed,
//! never rewritten. Another installed blaze (`blaze-old`) may still read it: removing a dead
//! pack is the collector's (remote-read piece 10), by the roster of live binaries. The command
//! says them once, a note per layer ([`PackStore::old_note`]).
//!
//! Old is told from torn first: a pack whose format word reads 1 or 2 but whose bytes, with that
//! word set back to 3, digest to its name is a format 3 pack with one torn word (one flipped bit:
//! 3 ^ 1 = 2, 3 ^ 2 = 1), refused and removed as torn by the store's existing rule (store.rs
//! `tests::torn`). A format 2 footer is valid when its count and index offset meet the file's
//! length and its index and footer digest to its name, as format 2 named its packs. Format 1's
//! footer was never committed (it lived between commits on 2026-11-16 before 10:00): its own
//! checks are not known here, so a format 1 word that is not a torn format 3 is old by its
//! magic and word alone; nothing of it is read.

use core::fmt;

use blaze_drive::Tool;
use blaze_drive::record::{Stored, open};
use blaze_ids::{ContentId, Hasher};
use blaze_own::Vec;
use blaze_own::alloc_ref::SharedAlloc;
use bstd::cx::Cx;
use bstd::fs::{File, FsKind, ReadOnly};
use bstd::mem::{Bytes, Frame};

use super::chunks::PutError;
use super::{CliError, FOOTER, FORMAT, Key, Layered, Pack, PackStore, ROW, RemoteError, Sharing, Untaken, index_number, le_u32, le_u64};

/// Bytes of one table entry: the kind tag (u16) and the tool id (32).
pub(super) const ENTRY: usize = 34;
/// The kind tag of a record that is not a drive record (a certificate, a root, a page).
pub(crate) const NOT_A_DRIVE_RECORD: u8 = 0;
/// The most entries a table holds: a row names its producer in a u16.
pub(super) const PRODUCERS_MOST: usize = 1 << 16;
/// The tool id of a store that was named none (a test's store, or a command before main.rs
/// names its tool): written as 32 zero bytes, read back as `None`, never as a tool. A tool
/// id is a BLAKE3 digest; all zeros is none of them.
const UNNAMED: [u8; 32] = [0; 32];

/// One producer: a record's kind tag and the tool that made it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Producer {
    /// `blaze_drive::Kind as u8`, or [`NOT_A_DRIVE_RECORD`]; a u16 on the wire.
    pub(crate) kind: u16,
    tool: [u8; 32],
}

impl Producer {
    /// The tool, or `None` for a store that was named none ([`UNNAMED`]).
    pub(crate) fn tool(&self) -> Option<Tool> {
        if self.tool == UNNAMED { None } else { Some(Tool(ContentId(self.tool))) }
    }

    fn entry(&self) -> [u8; ENTRY] {
        let mut entry = [0u8; ENTRY];
        entry[..2].copy_from_slice(&self.kind.to_le_bytes());
        entry[2..].copy_from_slice(&self.tool);
        entry
    }
}

fn tool_bytes(tool: Option<Tool>) -> [u8; 32] {
    match tool {
        Some(tool) => tool.0.0,
        None => UNNAMED,
    }
}

/// The kind tag of `bytes`, a record about to be sealed: its drive name's kind, or
/// [`NOT_A_DRIVE_RECORD`] for anything blaze-drive's `record::open` does not take (the store's
/// one rule for a record's kind, `Sharing::place`'s too).
pub(super) fn kind_of(bytes: &[u8]) -> u8 {
    match open(bytes) {
        Ok(Stored::Whole(view)) => view.name.kind as u8,
        Ok(Stored::Shape(view)) => view.name.kind as u8,
        Err(_) => NOT_A_DRIVE_RECORD,
    }
}

/// Bytes of a table of `producers` entries, padded to 8.
pub(super) fn table_len(producers: u32) -> u64 {
    (u64::from(producers) * ENTRY as u64).next_multiple_of(8)
}

/// The table of a pack whose records all have one tool (a seal: one command, one tool): each
/// kind seen, ranked in ascending order, which is the table's (kind, tool) order.
pub(super) struct OneTool {
    seen: [bool; 256],
    rank: [u16; 256],
    count: u16,
}

impl OneTool {
    pub(super) fn new() -> OneTool {
        OneTool { seen: [false; 256], rank: [0; 256], count: 0 }
    }

    pub(super) fn see(&mut self, kind: u8) {
        self.seen[kind as usize] = true;
    }

    /// After every kind is seen: each one's index, in kind order.
    pub(super) fn rank(&mut self) {
        let mut next = 0u16;
        for kind in 0..256 {
            if self.seen[kind] {
                self.rank[kind] = next;
                next += 1;
            }
        }
        self.count = next;
    }

    pub(super) fn index(&self, kind: u8) -> u16 {
        self.rank[kind as usize]
    }

    pub(super) fn count(&self) -> u32 {
        u32::from(self.count)
    }

    /// The table's bytes, pad included, each piece handed to `push`.
    pub(super) fn write<E>(&self, tool: Option<Tool>, push: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let tool = tool_bytes(tool);
        for kind in 0..256u16 {
            if self.seen[kind as usize] {
                push(&Producer { kind, tool }.entry())?;
            }
        }
        push(&[0u8; 8][..pad(self.count())])
    }
}

/// Zero bytes after a table of `producers` entries.
pub(super) fn pad(producers: u32) -> usize {
    (table_len(producers) - u64::from(producers) * ENTRY as u64) as usize
}

/// A table's bytes from its sorted entries, pad included, each piece handed to `push`.
pub(super) fn write_table<E>(table: &[Producer], push: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
    for producer in table {
        push(&producer.entry())?;
    }
    push(&[0u8; 8][..pad(table.len() as u32)])
}

/// Why a pack, or its producer table, was refused: by name, never read around.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackError {
    /// A pack of an older format with a valid footer of its own (the module's doc), of `bytes`
    /// bytes: refused at the open, never read, merged from, removed or rewritten.
    OldFormat { folder: &'static str, pack: u64, format: u32, bytes: u64 },
    /// A pack's producer table that does not hold: short, unsorted or repeated, or padded with
    /// other than zeros.
    BadTable { folder: &'static str, pack: u64, why: &'static str },
    /// A merge over more distinct producers than a row's u16 names.
    TooManyProducers { folder: &'static str, producers: usize },
    /// A merge of more rows than the footer's u32 counts.
    TooManyRows { folder: &'static str, rows: usize },
    /// No memory for `what`, of `entries` entries (rule 3: nothing is dropped).
    NoRoom { folder: &'static str, what: &'static str, entries: usize },
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackError::OldFormat { folder, pack, format, bytes } => write!(f, "the pack {pack:016x} under {folder} ({bytes} bytes) is of pack format {format}, and this blaze reads format 3 (rule 26, persisted-state-round-trips: an old form is refused by name, never read, migrated or removed; another installed blaze may read it, and only the collector removes it)"),
            PackError::BadTable { folder, pack, why } => write!(f, "the producer table of the pack {pack:016x} under {folder}: {why} (pack format 3; rule 21, keys-cover-every-input: a row names exactly one producer)"),
            PackError::TooManyProducers { folder, producers } => write!(f, "the store's merge under {folder}: {producers} distinct producers, and a row names its producer in 16 bits (rule 18, bounded-progress: the table's cap; nothing merged)"),
            PackError::TooManyRows { folder, rows } => write!(f, "the store's merge under {folder}: {rows} rows, and a pack's footer counts them in 32 bits (rule 18, bounded-progress; nothing merged)"),
            PackError::NoRoom { folder, what, entries } => write!(f, "the store under {folder}: no memory for {what}, {entries} entries (rule 3, no-fallback: nothing merged, every pack stays readable)"),
        }
    }
}

pub(super) fn pack_refused(error: PackError) -> CliError {
    CliError::Remote(RemoteError::Put(PutError::Pack(error)))
}

impl Pack {
    /// Its producer table, sorted, read with one positioned read into `scratch` (the caller
    /// resets it); the table itself on `heap`, the caller's. `None` when another command's merge
    /// removed the pack since the open (its records are in that merge's pack); a table that does
    /// not hold is refused by name.
    pub(super) fn producers(&self, cx: &Cx, scratch: &Frame, folder: &'static str, pack: u64, heap: SharedAlloc<'static>) -> Result<Option<Vec<Producer, SharedAlloc<'static>>>, CliError> {
        let entries = self.producers as usize;
        let len = table_len(self.producers) as usize;
        let mut table: Vec<Producer, SharedAlloc<'static>> = Vec::try_with_capacity_in(entries, heap).map_err(|_| pack_refused(PackError::NoRoom { folder, what: "a pack's producer table", entries }))?;
        let mut bytes = Bytes::new_in(scratch, "a pack's producer table");
        let read = self.file.read_at(cx, self.table_at, len, &mut bytes);
        if !matches!(read, Ok(_)) || bytes.len() != len {
            if self.removed(cx)? {
                return Ok(None);
            }
            read?;
            return Err(pack_refused(PackError::BadTable { folder, pack, why: "shorter than its footer says" }));
        }
        let bytes = bytes.as_slice();
        for entry in bytes[..entries * ENTRY].chunks_exact(ENTRY) {
            let mut tool = [0u8; 32];
            tool.copy_from_slice(&entry[2..]);
            let producer = Producer { kind: u16::from_le_bytes([entry[0], entry[1]]), tool };
            if table.last().is_some_and(|last| *last >= producer) {
                return Err(pack_refused(PackError::BadTable { folder, pack, why: "its entries are not sorted without repeats" }));
            }
            // Within the capacity reserved above: nothing allocated.
            table.push(producer);
        }
        if bytes[entries * ENTRY..].iter().any(|byte| *byte != 0) {
            return Err(pack_refused(PackError::BadTable { folder, pack, why: "padded with other than zeros" }));
        }
        Ok(Some(table))
    }
}

/// The domain of a pack's name (store.rs `index_number`).
const NAME_DOMAIN: &str = "blaze-cli pack index";

/// What the open makes of a pack whose footer has the magic and a format word of 1 or 2 (the
/// module's doc): torn ([`Untaken::Refused`], the existing rule) or old ([`Untaken::Old`]).
/// `footer` is its last [`FOOTER`] bytes, `len` its length. Reads its index and footer once
/// (one positioned read) when the footer's numbers meet its length; never a record.
pub(super) fn old_or_torn(cx: &Cx, file: &File<ReadOnly>, len: u64, footer: &[u8], folder: &'static str, number: u64, scratch: &Frame) -> Untaken {
    let word = le_u32(&footer[4..]);
    let at_index = len - FOOTER as u64;
    // A format 3 pack with its word torn: format 3's numbers (count u32, producers u32, index
    // offset) meet the length, and its index and footer, the word set back to 3, digest to its
    // name. The digest is streamed in pieces; BLAKE3's derive-key mode hashes pieces as the
    // whole (`ContentId::of` is that mode over one piece: store.rs `tests::torn` proves the two
    // agree, since a torn word must be refused there).
    let rows3 = u64::from(le_u32(&footer[8..])).checked_mul(ROW as u64);
    let index3 = le_u64(&footer[16..]);
    if rows3.is_some_and(|rows| index3.checked_add(rows) == Some(at_index)) {
        let rows = (at_index - index3) as usize;
        let mut index = Bytes::new_in(scratch, "a pack's index, to tell a torn word from an old pack");
        match file.read_at(cx, index3, rows, &mut index) {
            Ok(_) if index.len() == rows => {}
            Ok(_) => return Untaken::Refused,
            Err(error) => return Untaken::from(error),
        }
        let mut name = Hasher::new(NAME_DOMAIN);
        name.update(index.as_slice()).update(&footer[..4]).update(&FORMAT.to_le_bytes()).update(&footer[8..]);
        if le_u64(&name.finish().0[..8]) == number {
            return Untaken::Refused;
        }
    }
    let bytes = len;
    match word {
        // Format 2's footer: the count (u64), the index's offset, the payloads' digest; the pack
        // named by the digest of its index and footer. Valid when both hold.
        2 => {
            let rows2 = le_u64(&footer[8..]).checked_mul(ROW as u64);
            let index2 = le_u64(&footer[16..]);
            if !rows2.is_some_and(|rows| index2.checked_add(rows) == Some(at_index)) {
                return Untaken::Refused;
            }
            let Ok(rows) = usize::try_from(at_index - index2) else { return Untaken::Refused };
            let mut index = Bytes::new_in(scratch, "an old pack's index, to check its name");
            match file.read_at(cx, index2, rows + FOOTER, &mut index) {
                Ok(_) if index.len() == rows + FOOTER => {}
                Ok(_) => return Untaken::Refused,
                Err(error) => return Untaken::from(error),
            }
            if index_number(index.as_slice()) != number {
                return Untaken::Refused;
            }
            Untaken::Old(PackError::OldFormat { folder, pack: number, format: 2, bytes })
        }
        // Format 1: its own checks are not known (the module's doc): old by magic and word.
        1 => Untaken::Old(PackError::OldFormat { folder, pack: number, format: 1, bytes }),
        _ => Untaken::Refused,
    }
}

impl PackStore {
    /// The tool this command keys its records by (main.rs `home::tool`), which every record it
    /// seals from now on names as its producer's. A store named none writes [`UNNAMED`].
    pub(crate) fn name_tool(&mut self, tool: Tool) {
        self.tool = Some(tool);
    }

    /// The packs of an older format the open refused by name: kept as they are.
    pub(crate) fn old_packs(&self) -> &[PackError] {
        self.old.as_slice()
    }

    /// Who made `key`'s record in the packs this store holds: `None` when no pack holds it (a
    /// record put and not yet sealed has no producer row yet). What the collector (piece 10)
    /// reads; one positioned read of the holding pack's table, made on the caller's `heap`.
    pub(crate) fn producer_of(&mut self, cx: &Cx, key: Key, heap: &'static blaze_own::Heap) -> Result<Option<Producer>, CliError> {
        for (number, pack) in self.packs.iter().filter(|(_, pack)| !pack.gone) {
            let Ok(at) = pack.rows.binary_search_by(|row| row.0.cmp(&key)) else { continue };
            let table = pack.producers(cx, &self.scratch, self.folder, *number, heap.shared());
            // The table owns its entries on the heap; the frame's read is spent.
            self.scratch.reset();
            let table = match table {
                Ok(Some(table)) => table,
                Ok(None) => continue,
                Err(error) => return Err(error),
            };
            let index = pack.producer_of[at] as usize;
            return match table.get(index) {
                Some(producer) => Ok(Some(*producer)),
                None => Err(pack_refused(PackError::BadTable { folder: self.folder, pack: *number, why: "a row names an entry past its end" })),
            };
        }
        Ok(None)
    }

    /// The once-a-command note on this layer's old packs: their count and total bytes, `None`
    /// when there are none. Said by `Layered::open_chain` as "N packs of an older store format
    /// (M bytes) are not read by this blaze", with the layer's directory.
    pub(crate) fn old_note(&self) -> Option<(usize, u64)> {
        let bytes = self.old.iter().map(|old| match old {
            PackError::OldFormat { bytes, .. } => *bytes,
            _ => 0,
        });
        match self.old.len() {
            0 => None,
            count => Some((count, bytes.fold(0u64, u64::saturating_add))),
        }
    }

    /// The planted bug `StorePlant::OldPackMerged` (store/scale_tests.rs
    /// `the_old_pack_oracle_catches_a_merge_that_removes_them`): the merge removes the old
    /// packs, as a merge that took them for its own would. Never called without the plant.
    pub(super) fn planted_remove_old(&mut self, cx: &Cx) -> Result<(), CliError> {
        for old in self.old.iter() {
            let PackError::OldFormat { pack, .. } = old else { continue };
            match self.packs_dir.remove(cx, &super::pack_name(*pack)?, bstd::fs::Durable::No) {
                Ok(()) => {}
                Err(error) if error.kind == FsKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

impl<S: Sharing> Layered<'_, S> {
    /// [`PackStore::name_tool`] for every layer of the chain: what main.rs calls once after
    /// `Layered::open`, with the tool its keys hold.
    pub fn name_tool(&mut self, tool: Tool) {
        self.local.name_tool(tool);
        if let Some(machine) = self.machine.as_mut() {
            machine.name_tool(tool);
        }
        if let Some(own) = self.own.as_mut() {
            own.name_tool(tool);
        }
    }
}
