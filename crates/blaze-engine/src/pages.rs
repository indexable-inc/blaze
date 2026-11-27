//! The stored answer (engine.md E6, "the incremental half"): the engine's [`Answer`] and why each
//! of its rows was last entered ([`Kept`]), as content-keyed pages under a root, in the form of
//! blaze-drive's answer table (answers.rs: pages sized by bytes, [`PAGE_BYTES`] on average, a
//! name's page its subject's leading bits, [`page_of`]).
//!
//! # The form
//!
//! Every field little-endian and fixed-width; a name is its kind byte and its 32-byte subject.
//!
//! ```text
//! root = "BER6" | serial u64 | bits u8 (>= 1) | bytes u64 | flags u8 | live_pages u32 | outlines u32
//!        if flags == 0 (flat, bits <= RADIX_FLAT_MAX_BITS):
//!          | pages u32 (= 1 << bits) | pages x key [32]
//!          | files | places | units | findings | owed tasks | owed cells
//!        if flags == 1 (2-level radix, bits > RADIX_FLAT_MAX_BITS):
//!          | shards u32 (= 1 << (bits - RADIX_LEAF_BITS)) | shards x key [32]
//!          | 16 x file_shard_key [32] | places_key [32] | summary_key [32]
//!          | owed tasks | owed cells
//!        page index space (`0 .. (1 << bits)`):
//!          - lower half `0 .. (1 << (bits - 1))`: content (`ROWS`, `CELLS`, `OWED_CELLS`,
//!            `OWED_TASKS`, `FILES`, `CAUSES`, `RECORDS`) via [`content_page_of`]
//!          - upper half `(1 << (bits - 1)) .. (1 << bits)`: `READERS` only via [`readers_page_of`]
//! radix shard  = "BEX1" | bits u8 | shard u32 | 256 x key [32]  (all-zero shard = zero key)
//! file shard   = "BEF1" | shard u8 | count u32 | count x (file | has u8 | [digest [32]])
//! places rec   = "BEPL" | count u32 | count x (file | unit | edition u8 | flags u8)
//! summary rec  = "BES1" | units u32 | units x Name | findings u32 | findings x (Name | u32)
//! file places = count u32 | count x (file | unit | edition u8 | flags u8), by file, strictly
//!        ([`FilePlace`]: one a source file the driver planned, each file of the plan digests)
//! page = "BEP6" | bits u8 | index u32 | names u32 | cells u32 | ids u32 | 8 x count u32 | (n + 1) x start u32
//!        | sorted unique dictionaries (names, cells, ids) | the eight sections:
//!   rows        name | key | stage u8 | wave u16 | file | source | out | serial u64 |
//!               findings u32 | flags u8 (1 inline, 2 failed, 4 indexed) | defines u32 | reads u32 |
//!               defines x (cell [16], digest) | reads x (cell id, digest) | if indexed: reads x u32
//!   cells       cell id | digest | gen u64
//!   owed cells  cell id | gen u64
//!   owed tasks  name | stage u8 | wave u16 | source | file
//!   files       file | members u32 | members x (name | standing u8 | key | phase | source)
//!   causes      name | gen u64 | tag u8 | cell id | was | now   (tag: new, key moved,
//!               read moved, owed, inline; the cell and digests zero but for a moved read)
//!   records     name | key   (the store key of the record the row's last entry made: the
//!               driver's, which a later request reads a row that stands by)
//!   readers     cell id | count u32 | count x reader name
//! cell id = name (its task) | cell [16]
//! ```
//!
//! Each section sorted by its first field, strictly; an entry is on the page of its first
//! field's name (a cell's: its task's), but for a readers entry, which is on its cell's own
//! page ([`readers_slot`]). So everything about one task but who reads its cells is on one page,
//! and an edit rewrites the pages of the tasks it entered and of the cells whose readers it
//! moved, and nothing else ([`encode`] puts only pages whose key is not the last root's).
//! [`encode_changes`] merges the request's overlay into only the affected pages and carries
//! every other key forward. The reverse index is keyed by cell so the engine finds a moved
//! cell's readers directly after a restart, one page a cell, and a reader added to one list of
//! a task that many read rewrites that list's page, not the page of every list the task has.
//!
//! # Checked once (rule 74)
//!
//! The threat is a write torn by a crash or a disk's error, not an attacker. Every record the
//! pages read comes as a [`Sealed`]: its bytes with how they are known to be the ones put under
//! their key. [`Sealed::by_store`] takes the store's proof ([`StoreProof`]: blaze-cli
//! plain_place.rs `Checked`), which the store earns once a boot for a whole segment (blaze-cas
//! plain verified.rs: written or checked whole in this boot, sealed by a barrier before the flip
//! that named it, or checked whole at its first read after a reboot, BLAKE3 over every byte),
//! so a page read through it hashes nothing. [`Sealed::hashed`] hashes every byte against the
//! key: for bytes with no store behind them (the simulations, a `--verify` read), and a
//! mismatch is [`PagesError::Digest`]. A fetched record keeps its own check at its fetch.
//!
//! The structure is covered by the same proof: the writer puts each entry's start in the page's
//! head, so [`VerifiedPage::open`] checks the head alone (form, bits, index, that the start table
//! fits and ends at the page's length) and walks nothing. [`VerifiedPage::open_whole`] walks
//! every entry (each count, kind byte, order, page membership, and each start against the walk)
//! for an explicit verify and for [`decode`]'s exhaustive read. A refusal names the page and the
//! section (rule 19); the caller reports it and answers fresh (rule 3: a stated path, never a
//! quiet one).
//!
//! # Durable order (rule 43; simulated in pages_sim.rs)
//!
//! [`Encoded::puts`] lists the records to put, every new page before the root; [`Encoded::pins`]
//! the keys the checkout's root names, the root first then every page, pinned after the puts and
//! made visible by the store's one commit (its generation flip, blaze-cas plain). A crash before
//! that commit leaves the last root naming the last pages; after it, the new root naming the new.

use core::fmt;

use blaze_drive::answers::{bits_kept, page_of};
use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Kind, Name, content_key};
use blaze_id::ContentId;
use blaze_own::{BTreeMap, BTreeSet, SharedAlloc, Vec};

use crate::engine::{Answer, Cause, CellId, Entered, Gen, Phase, Planned, Readers, Row, SMap, SVec, Stage};

mod bep6;
mod view;
mod changes;
mod headers;
pub use headers::{Header, HeaderPages, HeaderShardBuilt, HeaderShardJob, Headers, HeadersPlant, IndexedHeaders, Writer};
pub use changes::{PageBuilt, PageJob, encode_changes, encode_changes_radix, plan_digest};
pub use view::{StoredDefines, StoredNames, StoredReads, StoredRow, VerifiedPage};

/// A page's form. A page of another form is never read: it is a [`PagesError::Magic`]. `BEP3`
/// had no start table: each open walked every entry to find them. `BEP4` kept a cell's readers
/// on its task's page. `BEP5` placed readers by [`readers_slot`] with uncompressed 33-byte names,
/// 49-byte cell ids, and 155-byte fixed causes. `BEP6` adds per-page sorted unique dictionaries
/// (`names`, `cells`, `ids`) and sparse `causes` encoding while remaining zero-alloc in-place.
/// `BEP5` is refused cleanly by magic (Rule 3, Rule 26).
pub const PAGE_MAGIC: &[u8; 4] = b"BEP6";

/// The name whose page holds `id`'s readers entry: a placement key, never a task's name. Its
/// subject is the task's with its first eight bytes replaced by a mix of the task's first word,
/// its kind and the cell's two words (splitmix64's finalizer over a fold, part of the form: two
/// builders place one cell on one page, rule 25), so the cells of one task are spread over the
/// upper-half readers pages ([`readers_page_of`]) like the tasks are over the lower-half content
/// pages ([`content_page_of`]), and a change to one cell's readers rewrites only that readers
/// page and never any content page.
pub fn readers_slot(id: &CellId) -> Name {
    let word = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
    let mut subject = id.of.subject.0;
    let mut h = word(&subject[..8]) ^ (id.of.kind as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^= word(&id.cell.0[..8]).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= word(&id.cell.0[8..]).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    subject[..8].copy_from_slice(&h.to_be_bytes());
    Name::new(id.of.kind, ContentId(subject))
}

/// Number of pages in each half of a `BER6` root (`1 << (bits - 1)` for `bits >= 1`).
#[inline]
pub const fn half_pages(bits: u8) -> u32 {
    if bits == 0 {
        1
    } else {
        1u32 << (bits - 1)
    }
}

/// Which page of a `BER6` root of `bits` (`>= 1`) holds a content entry (`ROWS`, `CELLS`,
/// `OWED_CELLS`, `OWED_TASKS`, `FILES`, `CAUSES`, `RECORDS`) for `name`: always in the lower
/// half `0 .. (1 << (bits - 1))`.
#[inline]
pub fn content_page_of(name: Name, bits: u8) -> u32 {
    page_of(name, bits.saturating_sub(1))
}

/// Which page of a `BER6` root of `bits` (`>= 1`) holds a `READERS` entry for `slot` (computed
/// via [`readers_slot`]): always in the upper half `(1 << (bits - 1)) .. (1 << bits)`.
#[inline]
pub fn readers_slot_page_of(slot: Name, bits: u8) -> u32 {
    let half_bits = bits.saturating_sub(1).min(blaze_drive::answers::MOST_BITS - 1);
    (1u32 << half_bits) + page_of(slot, half_bits)
}

/// Which page of a `BER6` root of `bits` (`>= 1`) holds `id`'s `READERS` entry: always in the
/// upper half `(1 << (bits - 1)) .. (1 << bits)`.
#[inline]
pub fn readers_page_of(id: &CellId, bits: u8) -> u32 {
    readers_slot_page_of(readers_slot(id), bits)
}

/// Which page of a `BER6` root of `bits` (`>= 1`) answers `ask` for `name`:
/// - [`Ask::Readers`](crate::Ask::Readers): upper half via [`readers_slot_page_of`] (`name` is
///   `readers_slot(&id)`);
/// - [`Ask::Witness`](crate::Ask::Witness): full `0 .. (1 << bits)` space via [`page_of`]
///   (`name` is a synthetic page witness);
/// - every other ask: lower half via [`content_page_of`].
#[inline]
pub fn page_for_ask(name: Name, ask: crate::Ask, bits: u8) -> u32 {
    match ask {
        crate::Ask::Readers => readers_slot_page_of(name, bits),
        crate::Ask::Witness => page_of(name, bits),
        _ => content_page_of(name, bits),
    }
}

/// Partitioned `BER6` bit count for `total` estimated `BEP6` bytes (always in `1..=MOST_BITS`):
/// keeps `old` while its per-half bit count stays in `bits_kept`'s hysteresis band, giving
/// content `1 << (bits - 1)` pages and `READERS` `1 << (bits - 1)` pages.
#[inline]
pub fn bits_kept_partitioned(total: u64, old: Option<u8>) -> u8 {
    let old_half = old.map(|b| b.saturating_sub(1));
    let half = bits_kept(total, old_half).min(blaze_drive::answers::MOST_BITS - 1);
    half + 1
}

/// The root's form. `BER3` stored a flat array of up to 65,536 page keys (2 MiB) plus all
/// directory sections inline; `BER4` kept small answers inline and stored large answers as a
/// 2-level 256-way radix tree over `BEP5` pages; `BER5` stored the same directory over `BEP6`
/// dictionary-encoded pages with content and `READERS` mixed on every page; `BER6` partitions the
/// `1 << bits` (`bits >= 1`) page address space into lower-half content pages (`0..half`) and
/// upper-half `READERS`-only pages (`half..2*half`) so adding reader edges never rewrites content
/// pages. A `BER5` (or older) root is a [`PagesError::Magic`], reported at `open_root` and
/// rebuilt cleanly (Rule 3, Rule 26).
pub const ROOT_MAGIC: &[u8; 4] = b"BER6";
/// Leaf-shard form of a 2-level radix answer root: 256 page keys (8,192 bytes + 9-byte header).
pub const RADIX_SHARD_MAGIC: &[u8; 4] = b"BEX1";
/// File-directory shard form of a radix answer root (16 shards by subject's leading nibble).
pub const FILE_SHARD_MAGIC: &[u8; 4] = b"BEF1";
/// File-places sub-record form of a radix answer root.
pub const PLACES_MAGIC: &[u8; 4] = b"BEPL";
/// Units-and-findings summary sub-record form of a radix answer root.
pub const SUMMARY_MAGIC: &[u8; 4] = b"BES1";

/// Bits addressed inside one radix leaf shard (`1 << 8 = 256` page keys = 8 KiB).
pub const RADIX_LEAF_BITS: u8 = 8;
/// Number of page slots per radix leaf shard (`256`).
pub const RADIX_LEAF_PAGES: usize = 1 << RADIX_LEAF_BITS;
/// Maximum `bits` kept inline as a flat root (`<= 512` pages = `<= 16 KiB`); above this
/// (`bits >= 10`, `1,024..=65,536` pages), the root uses the 2-level 256-way radix tree.
pub const RADIX_FLAT_MAX_BITS: u8 = 9;
/// Number of file-directory shards in a radix root (`file.subject.0[0] >> 4`).
pub const FILE_SHARDS: usize = 16;

const ROOT_FLAG_FLAT: u8 = 0;
const ROOT_FLAG_RADIX: u8 = 1;
const RADIX_SHARD_HEAD: usize = 4 + 1 + 4;
const RADIX_SHARD_BYTES: usize = RADIX_SHARD_HEAD + RADIX_LEAF_PAGES * ID;

/// Where the driver found a source file of its plan, kept beside the file's plan digest in the
/// root ([`Root::places`]) so an edit of the file re-plans it alone (D187): the file's own walk
/// from its crate's root, which the driver made to reach it, is not made again. The engine keeps
/// it and never reads it: its fields are the driver's (blaze-cli engine_check.rs `plan`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FilePlace {
    /// The root file of the crate target whose module tree reached it first.
    pub unit: Name,
    /// The edition byte its crate's part gave it.
    pub edition: u8,
    /// The driver's own flags of it.
    pub flags: u8,
}

const FILE_PLACE: usize = NAME + NAME + 2;

const NAME: usize = 1 + 32;
const ID: usize = 32;
const CELL: usize = 16;
const CELL_ID: usize = NAME + CELL;
const SECTIONS: usize = 8;
const ROWS: usize = 0;
const CELLS: usize = 1;
const OWED_CELLS: usize = 2;
const OWED_TASKS: usize = 3;
const FILES: usize = 4;
const CAUSES: usize = 5;
const RECORDS: usize = 6;
const READERS: usize = 7;
const SECTION_NAMES: [&str; SECTIONS] = ["rows", "cells", "owed cells", "owed tasks", "files", "causes", "records", "readers"];
const ROW_FIXED: usize = NAME + ID + 1 + 2 + NAME + ID + ID + 8 + 4 + 1 + 4 + 4;
const DEFINE: usize = CELL + ID;
const READ: usize = CELL_ID + ID;
const CELL_ENTRY: usize = CELL_ID + ID + 8;
const OWED_CELL: usize = CELL_ID + 8;
const OWED_TASK: usize = NAME + 1 + 2 + ID + NAME;
const FILE_MEMBER: usize = NAME + 1 + ID + 3 + ID;
const CAUSE: usize = NAME + 8 + 1 + CELL_ID + ID + ID;
const ROOT_HEAD: usize = 4 + 8 + 1 + 8 + 1 + 4 + 4;
/// The page index a root's own errors name.
pub const ROOT_PAGE: u32 = u32::MAX;

const INLINE: u8 = 1;
const FAILED: u8 = 2;
const INDEXED: u8 = 4;

/// What a store's read returns beside a record's bytes as its proof that they are the bytes put
/// under the key asked, whole (rule 74, rule 76): earned once a boot for the segment holding
/// them, never per read (the module docs, "Checked once"). Implemented by the one store proof a
/// driver holds (blaze-cli plain_place.rs `Checked`, made only there) and by the simulations'.
/// Its meaning is the implementor's promise: nothing here can check it, so nothing else
/// implements it.
pub trait StoreProof: Copy {}

/// How a [`Sealed`] record's bytes are known to be its key's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Check {
    /// The store's proof ([`Sealed::by_store`]): no byte read.
    Store,
    /// Every byte hashed here ([`Sealed::hashed`]).
    Hashed,
}

/// A record's bytes that are the ones put under `key`, and how that is known: the one input of
/// [`decode_root`], [`VerifiedPage::open`] and [`IndexedHeaders::open`]. Made only by
/// [`Sealed::by_store`] (a store's proof) or [`Sealed::hashed`] (a hash of every byte).
#[derive(Clone, Copy, Debug)]
pub struct Sealed<'b> {
    key: Key,
    bytes: &'b [u8],
    check: Check,
}

/// Bytes that are not their key's: a write a crash cut short or a disk's flipped bit, found by
/// [`Sealed::hashed`]. [`Torn::at`] names the page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Torn {
    pub want: Key,
    pub got: Key,
}

impl Torn {
    /// The refusal of page `page` ([`ROOT_PAGE`] for the root).
    pub fn at(self, page: u32) -> PagesError {
        PagesError::Digest { page, want: self.want, got: self.got }
    }
}

impl<'b> Sealed<'b> {
    /// `bytes` as a store's read returned them for `key`, with its proof: nothing is hashed.
    pub fn by_store<P: StoreProof>(key: Key, bytes: &'b [u8], _proof: P) -> Sealed<'b> {
        Sealed { key, bytes, check: Check::Store }
    }

    /// `bytes` hashed whole against `key` (BLAKE3, the key's own form): for bytes no store
    /// proves, and for an explicit verify.
    pub fn hashed(key: Key, bytes: &'b [u8]) -> Result<Sealed<'b>, Torn> {
        let got = content_key(bytes);
        if got != key {
            return Err(Torn { want: key, got });
        }
        Ok(Sealed { key, bytes, check: Check::Hashed })
    }

    pub fn key(&self) -> Key {
        self.key
    }

    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    pub fn check(&self) -> Check {
        self.check
    }

    /// The bytes this record's making hashed: all of them for [`Sealed::hashed`], none for the
    /// store's proof (the profile's `pages: N read, M bytes, K hashed`).
    pub fn hashed_bytes(&self) -> usize {
        match self.check {
            Check::Store => 0,
            Check::Hashed => self.bytes.len(),
        }
    }
}

/// What a request keeps: the engine's answer, and why each row's task was last entered and in
/// which request (`blaze why` walks these back to the edit).
pub struct Kept<'a> {
    pub answer: Answer<'a>,
    pub causes: SMap<'a, Name, (Gen, Cause)>,
    /// Each row's record in the store, by the key the driver put it under (one a row that
    /// stands is read by); the engine never reads it. One whose row went is not stored.
    pub records: SMap<'a, Name, Key>,
    /// The places of the source files this request planned ([`FilePlace`]): every one the
    /// phase path walked, or only the edited ones on the ready path. A file not here keeps the
    /// last root's place ([`encode_changes`]); a file the root no longer plans keeps none.
    pub places: SMap<'a, Name, FilePlace>,
}

impl<'a> Kept<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Kept<'a> {
        Kept { answer: Answer::new_in(alloc), causes: BTreeMap::new_in(alloc), records: BTreeMap::new_in(alloc), places: BTreeMap::new_in(alloc) }
    }

    /// The request's answer kept with its causes over the last ones: each entered task's cause
    /// at the answer's serial. A cause whose row went is not stored ([`encode`] skips it).
    pub fn after(mut causes: SMap<'a, Name, (Gen, Cause)>, answer: Answer<'a>, entered: &[Entered], records: SMap<'a, Name, Key>) -> Kept<'a> {
        for one in entered.iter() {
            causes.insert(one.name, (answer.serial, one.cause));
        }
        let places = BTreeMap::new_in(*causes.allocator());
        Kept { answer, causes, records, places }
    }
}

/// Why stored pages are not an answer of this form. Each names the page (its index;
/// [`ROOT_PAGE`] for the root) and what in it (rule 19); nothing past a refusal is read (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PagesError {
    /// The store has no record of the key the root names.
    Missing { page: u32, key: Key },
    /// The page key selected a CAS pointer instead of a record.
    PageRecord { page: u32, key: Key },
    Base { serial: Gen },
    Reach { owner: Key, index: Key, what: &'static str },
    /// The bytes are not the key's: a flipped bit, or a write a crash cut short.
    Digest { page: u32, want: Key, got: Key },
    /// Another form's magic, or a page another root's (its bits or index not the root's).
    Magic { page: u32 },
    /// Fewer bytes than its counts need.
    Short { page: u32, at: u32 },
    /// Bytes past its last section.
    Trailing { page: u32, at: u32 },
    /// A kind, stage, flag or cause byte no value has.
    Tag { page: u32, at: u32, byte: u8 },
    /// A section not strictly sorted by its first field.
    Unsorted { page: u32, section: &'static str, at: u32 },
    /// An entry on a page its name's bits do not choose.
    Misplaced { page: u32, section: &'static str, name: Name },
    /// A root whose page count is not `1 << bits`, or whose bits pass the most.
    Count { page: u32, count: u32 },
    /// A row's observations exceed the engine's supported limit.
    Reads { page: u32, at: u32, count: u32 },
    /// Applying a page edit would overflow or underflow the claimed directory total.
    Total { page: u32, field: &'static str },
    /// A page's start table does not delimit its entries: entry `entry`'s start is not where
    /// the one before it ends (or the table does not begin after itself or end at the page's
    /// length).
    Starts { page: u32, entry: u32 },
    /// Producer header shard `page` was asked of an answer whose index is
    /// [`Headers::Absent`] (headers.rs): a lookup that should have read the row's page.
    HeadersAbsent { page: u32, since: Gen },
}

impl fmt::Display for PagesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let page = |f: &mut fmt::Formatter<'_>, page: u32| match page {
            ROOT_PAGE => write!(f, "the stored answer's root"),
            page => write!(f, "the stored answer's page {page}"),
        };
        match self {
            PagesError::Base { serial } => write!(f, "answer {:?} needs its stored base and request changes for incremental publication", serial),
            PagesError::Reach { owner, index, what } => write!(f, "answer record {owner:?}, reach index {index:?}: {what}"),
            PagesError::PageRecord { page: p, key } => {
                page(f, *p)?;
                write!(f, " ({key:?}) names a store pointer where a page record is required")
            }
            PagesError::Missing { page: p, key } => {
                page(f, *p)?;
                write!(f, " ({key:?}) is not in the store (blaze-engine pages.rs `decode`)")
            }
            PagesError::Digest { page: p, want, got } => {
                page(f, *p)?;
                write!(f, " is not the bytes its key names: want {want:?}, got {got:?}: a flipped bit or a cut write (pages.rs `decode`)")
            }
            PagesError::Magic { page: p } => {
                page(f, *p)?;
                write!(f, " is of another form or another root (pages.rs `decode`)")
            }
            PagesError::Short { page: p, at } => {
                page(f, *p)?;
                write!(f, " ends at byte {at} before its counts do (pages.rs `decode`)")
            }
            PagesError::Trailing { page: p, at } => {
                page(f, *p)?;
                write!(f, " has bytes past its last section, from {at} (pages.rs `decode`)")
            }
            PagesError::Tag { page: p, at, byte } => {
                page(f, *p)?;
                write!(f, ": byte {at} is {byte}, which no value has (pages.rs `decode`)")
            }
            PagesError::Unsorted { page: p, section, at } => {
                page(f, *p)?;
                write!(f, ": its {section} are not sorted at byte {at} (pages.rs `decode`)")
            }
            PagesError::Misplaced { page: p, section, name } => {
                page(f, *p)?;
                write!(f, ": {name:?} in its {section} belongs on another page (pages.rs `decode`)")
            }
            PagesError::Count { page: p, count } => {
                page(f, *p)?;
                write!(f, " names {count} pages, not 1 << its bits (pages.rs `decode`)")
            }
            PagesError::Reads { page: p, at, count } => {
                page(f, *p)?;
                write!(f, " has a row at {at} with {count} reads, past the limit {}", crate::READS_CAP)
            }
            PagesError::Total { page: p, field } => {
                page(f, *p)?;
                write!(f, " contradicts the answer directory's {field}")
            }
            PagesError::Starts { page: p, entry } => {
                page(f, *p)?;
                write!(f, ": its start table does not delimit entry {entry} (pages.rs `VerifiedPage::open`)")
            }
            PagesError::HeadersAbsent { page, since } => {
                write!(f, "producer header shard {page} was asked, and the answer has no current shards since request {} (headers.rs `Headers::Absent`): the row's page answers it", since.0)
            }
        }
    }
}

/// One page slot whose key moved between the previous root and this commit (used by reach-shard
/// publication without scanning all `1 << bits` slots).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageDelta {
    pub index: u32,
    pub old: Key,
    pub new: Key,
}

/// One decoded 256-page leaf shard of a 2-level radix answer root (`BEX1`).
pub struct RadixShard<'a> {
    pub keys: blaze_own::Box<[Key; RADIX_LEAF_PAGES], SharedAlloc<'a>>,
}

/// A [`Kept`] as records: what to put, in order, and what the checkout's root names.
pub struct Encoded<'a> {
    pub serial: Gen,
    pub bits: u8,
    /// The pages' bytes in all.
    pub bytes: u64,
    pub root: Key,
    /// Flat page keys when `bits <= RADIX_FLAT_MAX_BITS` (`1 << bits` entries); empty in radix mode.
    pub pages: SVec<'a, Key>,
    /// Leaf-shard keys when `bits > RADIX_FLAT_MAX_BITS` (`1 << (bits - RADIX_LEAF_BITS)` entries); empty in flat mode.
    pub page_shards: SVec<'a, Key>,
    /// Number of non-zero task pages in this answer.
    pub live_pages: u32,
    /// The records not under the last root, ordered durably: task pages first (`puts[..task_puts]`),
    /// then radix leaf shards and directory sub-records, then the top root last.
    pub puts: SVec<'a, (Key, SVec<'a, u8>)>,
    /// Prefix length of [`Encoded::puts`] that are new task pages (excluding radix shards, directory
    /// sub-records, and the top root).
    pub task_puts: u32,
    /// Non-zero auxiliary records owned by the root (radix page leaf shards, file directory shards,
    /// places record, and summary record) that GC reachability MUST pin alongside [`Encoded::root`].
    pub tree_keys: SVec<'a, Key>,
    /// On an incremental commit without repaging: the exact `(index, old, new)` page transitions.
    /// `None` on a cold encode or repage.
    pub page_deltas: Option<SVec<'a, PageDelta>>,
    pub files: u32,
    pub findings: u32,
    pub owed: u32,
    /// Of a commit folded into the last directory ([`encode_changes`]): the entries its delta
    /// wrote (rows, records, causes, cells, readers, obligations and file plans, each one key
    /// of one section), and the pages they fell on, each read once as its witness
    /// (`Ask::Witness`). The pages touched are at most the entries: as the directory grows by
    /// the world (`bits_kept`), the same entries spread over more pages, never past their
    /// count, so the witness reads are bounded by the edit's own writes. 0 for a whole encode.
    pub changed_entries: u32,
    pub changed_pages: u32,
    records: SVec<'a, (Key, SVec<'a, (Name, Key)>)>,
    plant: PagesPlant,
}

impl<'a> Encoded<'a> {
    /// Whether this answer root uses the 2-level 256-way radix tree (`bits > RADIX_FLAT_MAX_BITS`).
    pub fn is_radix(&self) -> bool {
        self.bits > RADIX_FLAT_MAX_BITS
    }

    /// Total page slots (`1 << bits`).
    pub fn page_count(&self) -> usize {
        1usize << self.bits
    }

    /// The complete payload index of each newly written page, for CAS reach.
    pub fn page_records(&self) -> impl Iterator<Item = (Key, &[(Name, Key)])> {
        self.records.iter().map(|(key, rows)| (*key, rows.as_slice()))
    }

    /// All non-zero auxiliary sub-records referenced by the root (leaf shards, file shards,
    /// places, summary), to be pinned in `{name}.answer` alongside [`Encoded::root`].
    pub fn tree_keys(&self) -> &[Key] {
        &self.tree_keys
    }

    /// The keys the checkout's root names after the puts: the root, then its auxiliary tree
    /// records and (for flat roots) every non-zero page, so compaction keeps the whole answer.
    pub fn pins(&self, alloc: SharedAlloc<'a>) -> SVec<'a, Key> {
        let mut pins = Vec::with_capacity_in(1 + self.tree_keys.len() + self.pages.len(), alloc);
        pins.push(self.root);
        if self.plant == PagesPlant::PagesNotPinned {
            return pins;
        }
        pins.extend(self.tree_keys.iter().copied());
        pins.extend(self.pages.iter().copied().filter(|key| *key != Key::default()));
        pins
    }
}

/// The root as stored: its serial, its paging, and either inline page keys (flat, `bits <= 9`) or
/// 256-way radix leaf-shard keys (`bits > 9`).
pub struct Root<'a> {
    pub serial: Gen,
    pub bits: u8,
    pub bytes: u64,
    /// Flat page keys when `bits <= RADIX_FLAT_MAX_BITS`; empty in radix mode (`bits > RADIX_FLAT_MAX_BITS`).
    pub pages: SVec<'a, Key>,
    /// Leaf-shard keys when `bits > RADIX_FLAT_MAX_BITS` (`1 << (bits - RADIX_LEAF_BITS)` entries); empty in flat mode.
    pub page_shards: SVec<'a, Key>,
    /// Exact count of non-zero pages across this root.
    pub live_pages: u32,
    /// File membership and the digest of its standing plan, when every member has a row.
    pub files: SMap<'a, Name, Option<ContentId>>,
    /// The place of each source file of `files` the driver planned ([`FilePlace`]).
    pub places: SMap<'a, Name, FilePlace>,
    pub units: SVec<'a, Name>,
    pub findings: SMap<'a, Name, u32>,
    pub owed_tasks: SMap<'a, Name, Planned>,
    pub owed_cells: SMap<'a, CellId, Gen>,
    pub outlines: u32,
    /// Cached radix directory sub-record keys so incremental edits skip serializing unchanged sub-records.
    pub(crate) file_shards: [Key; FILE_SHARDS],
    pub(crate) places_key: Key,
    pub(crate) summary_key: Key,
}

impl Root<'_> {
    /// Whether this root uses the 2-level 256-way radix tree (`bits > RADIX_FLAT_MAX_BITS`).
    pub fn is_radix(&self) -> bool {
        self.bits > RADIX_FLAT_MAX_BITS
    }

    /// Total page slots (`1 << bits`).
    pub fn page_count(&self) -> usize {
        1usize << self.bits
    }

    /// The `(shard_index, slot_in_shard)` pair for page index `index` in radix mode.
    pub fn shard_of(index: usize) -> (usize, usize) {
        (index >> RADIX_LEAF_BITS, index & (RADIX_LEAF_PAGES - 1))
    }
}

fn put_id(out: &mut SVec<'_, u8>, id: &ContentId) {
    out.extend_from_slice(&id.0);
}

fn put_name(out: &mut SVec<'_, u8>, name: Name) {
    out.push(name.kind as u8);
    put_id(out, &name.subject);
}

fn put_cell_id(out: &mut SVec<'_, u8>, id: &CellId) {
    put_name(out, id.of);
    out.extend_from_slice(&id.cell.0);
}

fn put_phase(out: &mut SVec<'_, u8>, phase: Phase) {
    out.push(phase.stage as u8);
    out.extend_from_slice(&phase.wave.to_le_bytes());
}

fn put_u32(out: &mut SVec<'_, u8>, value: usize) {
    out.extend_from_slice(&u32::try_from(value).expect("a page's counts fit u32 (a page holds about PAGE_BYTES)").to_le_bytes());
}

fn row_is_indexed(row: &Row<'_>) -> bool {
    row.by_cell.iter().enumerate().any(|(i, &at)| i != usize::try_from(at).unwrap_or(usize::MAX))
}

fn put_row(out: &mut SVec<'_, u8>, name: Name, row: &Row<'_>) {
    put_row_indexed(out, name, row, row_is_indexed(row));
}

fn put_row_indexed(out: &mut SVec<'_, u8>, name: Name, row: &Row<'_>, indexed: bool) {
    put_name(out, name);
    put_id(out, &row.key.0);
    put_phase(out, row.phase);
    put_name(out, row.file);
    put_id(out, &row.source);
    put_id(out, &row.out);
    out.extend_from_slice(&row.serial.0.to_le_bytes());
    out.extend_from_slice(&row.findings.to_le_bytes());
    // Preserve observation order. Canonically sorted reads are their own index.
    out.push(u8::from(row.inline) * INLINE | u8::from(row.failed) * FAILED | u8::from(indexed) * INDEXED);
    put_u32(out, row.defines.len());
    put_u32(out, row.reads.len());
    for (cell, digest) in row.defines.iter() {
        out.extend_from_slice(&cell.0);
        put_id(out, digest);
    }
    for (id, digest) in row.reads.iter() {
        put_cell_id(out, id);
        put_id(out, digest);
    }
    if indexed {
        for at in &row.by_cell { out.extend_from_slice(&at.to_le_bytes()); }
    }
}

fn put_cause(out: &mut SVec<'_, u8>, name: Name, serial: Gen, cause: Cause) {
    put_name(out, name);
    out.extend_from_slice(&serial.0.to_le_bytes());
    let (tag, read, was, now) = match cause {
        Cause::New => (0, None, ContentId([0; 32]), ContentId([0; 32])),
        Cause::KeyMoved => (1, None, ContentId([0; 32]), ContentId([0; 32])),
        Cause::ReadMoved { read, was, now } => (2, Some(read), was, now),
        Cause::Owed => (3, None, ContentId([0; 32]), ContentId([0; 32])),
        Cause::Inline => (4, None, ContentId([0; 32]), ContentId([0; 32])),
    };
    out.push(tag);
    match read { Some(id) => put_cell_id(out, &id), None => out.extend_from_slice(&[0; CELL_ID]) }
    put_id(out, &was);
    put_id(out, &now);
}

#[derive(Clone, Copy)]
enum BufEntry<'p> {
    Flat { start: usize, end: usize },
    Dict { dicts: view::PageDicts<'p>, bytes: &'p [u8] },
}

/// A page being written during re-paging: flat newly written entries in `parts`, plus zero-copy
/// references to surviving `BEP6` entries from old pages in `entries`. Finished via
/// [`PageBuf::finish_in`] into canonical `BEP6` bytes.
struct PageBuf<'p, 'a> {
    parts: [SVec<'a, u8>; SECTIONS],
    entries: [SVec<'a, BufEntry<'p>>; SECTIONS],
}

impl<'p, 'a> PageBuf<'p, 'a> {
    fn new_in(alloc: SharedAlloc<'a>) -> PageBuf<'p, 'a> {
        PageBuf {
            parts: core::array::from_fn(|_| Vec::new_in(alloc)),
            entries: core::array::from_fn(|_| Vec::new_in(alloc)),
        }
    }

    /// Append a newly serialized flat entry into `section`.
    fn push_flat(&mut self, section: usize, write: impl FnOnce(&mut SVec<'a, u8>)) {
        let start = self.parts[section].len();
        write(&mut self.parts[section]);
        let end = self.parts[section].len();
        self.entries[section].push(BufEntry::Flat { start, end });
    }

    /// Forward a surviving `BEP6` entry from an old page without copying its bytes.
    fn push_dict(&mut self, section: usize, page: &'p VerifiedPage<'p>, bytes: &'p [u8]) {
        self.entries[section].push(BufEntry::Dict { dicts: page.dicts(), bytes });
    }

    fn entries_count(&self) -> usize {
        self.entries.iter().map(Vec::len).sum()
    }

    /// The page in its `BEP6` form: its head, start table, sorted dictionaries, and sections.
    /// `None` when it holds nothing (a root names no page there).
    fn finish_in<'out, A: core::alloc::Allocator + Copy>(
        &self,
        bits: u8,
        index: u32,
        alloc: SharedAlloc<'out>,
        scratch: A,
    ) -> Option<(SVec<'out, u8>, SVec<'out, (Name, Key)>)> {
        if self.entries_count() == 0 {
            return None;
        }
        let mut sources: [Vec<bep6::EntrySource<'_>, A>; SECTIONS] =
            core::array::from_fn(|s| Vec::with_capacity_in(self.entries[s].len(), scratch));
        for s in 0..SECTIONS {
            let part = self.parts[s].as_slice();
            for &entry in self.entries[s].iter() {
                match entry {
                    BufEntry::Flat { start, end } => {
                        if let Some(slice) = part.get(start..end) {
                            sources[s].push(bep6::EntrySource::Flat(slice));
                        }
                    }
                    BufEntry::Dict { dicts, bytes } => {
                        sources[s].push(bep6::EntrySource::Dict { dicts, bytes });
                    }
                }
            }
        }
        let slices: [&[bep6::EntrySource<'_>]; SECTIONS] =
            core::array::from_fn(|s| sources[s].as_slice());
        let (page, records) = bep6::encode_page_from_sources(bits, index, slices, alloc, scratch);
        if page.is_empty() { None } else { Some((page, records)) }
    }
}

/// A bug the simulations plant to show their oracles catch it. Always `None` outside the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PagesPlant {
    None,
    /// Each file's planned tasks are not stored: a task an edit removes from its file keeps its
    /// row after a restore.
    FilesDropped,
    /// The checkout's root names the answer's root and not its pages: compaction takes them.
    PagesNotPinned,
}

/// One non-empty page of a whole-answer encode ([`encode_par`]), holding only borrowed slices into
/// its eight already-sorted sections so serialization, per-file plan digests, and page BLAKE3
/// hashing run independently across worker threads with zero intermediate `PageBuf` copies.
pub struct WholePageJob<'k, 'm> {
    index: u32,
    bits: u8,
    last_flat_key: Option<Key>,
    all_rows: &'k SMap<'m, Name, Row<'m>>,
    all_owed_tasks: &'k SMap<'m, Name, Planned>,
    rows: &'k [(&'k Name, &'k Row<'m>)],
    cells: &'k [(&'k CellId, &'k (ContentId, Gen))],
    owed_cells: &'k [(&'k CellId, &'k Gen)],
    owed_tasks: &'k [(&'k Name, &'k Planned)],
    files: &'k [(&'k Name, &'k SVec<'m, Name>)],
    causes: &'k [(&'k Name, &'k (Gen, Cause))],
    records: &'k [(&'k Name, &'k Key)],
    readers: &'k [(&'k CellId, &'k [Name])],
}

/// The serialized and content-hashed result of one [`WholePageJob`].
pub struct WholePageBuilt<'a> {
    index: u32,
    byte_len: u64,
    key: Key,
    bytes: SVec<'a, u8>,
    records: SVec<'a, (Name, Key)>,
    files: SVec<'a, (Name, Option<ContentId>)>,
    changed: bool,
}

impl WholePageJob<'_, '_> {
    /// Directly serialize all eight sections of this page into one exact-sized `BEP6` buffer,
    /// compute per-file plan digests for any `FILES` entries on this page, and BLAKE3-hash the
    /// finished page.
    pub fn run<'a>(&self, alloc: SharedAlloc<'a>) -> WholePageBuilt<'a> {
        self.run_in(alloc, alloc)
    }

    /// [`WholePageJob::run`] with a separate reusable `scratch` allocator (such as a per-worker
    /// bump `Frame` reset between pages) so temporary dictionary tables never contend on the
    /// shared output allocator.
    pub fn run_in<'a, A: core::alloc::Allocator + Copy>(&self, alloc: SharedAlloc<'a>, scratch: A) -> WholePageBuilt<'a> {
        let counts = [
            self.rows.len(),
            self.cells.len(),
            self.owed_cells.len(),
            self.owed_tasks.len(),
            self.files.len(),
            self.causes.len(),
            self.records.len(),
            self.readers.len(),
        ];
        let entries: usize = counts.iter().sum();
        let mut dicts = bep6::DictBuilder::new_in(scratch);
        let mut row_indexed: Vec<bool, A> = Vec::with_capacity_in(self.rows.len(), scratch);
        let mut lengths: Vec<usize, A> = Vec::with_capacity_in(entries, scratch);
        let mut body_len = 0usize;

        for &(&name, row) in self.rows {
            let indexed = row_is_indexed(row);
            row_indexed.push(indexed);
            let len = bep6::collect_row_v6(name, row, indexed, &mut dicts);
            lengths.push(len);
            body_len = body_len.saturating_add(len);
        }
        for &(id, &(digest, _)) in self.cells {
            dicts.add_cell(*id);
            dicts.add_id(digest);
            lengths.push(bep6::CELL_ENTRY_V6);
            body_len = body_len.saturating_add(bep6::CELL_ENTRY_V6);
        }
        for &(id, _) in self.owed_cells {
            dicts.add_cell(*id);
            lengths.push(bep6::OWED_CELL_V6);
            body_len = body_len.saturating_add(bep6::OWED_CELL_V6);
        }
        for &(&name, planned) in self.owed_tasks {
            dicts.add_name(name);
            dicts.add_id(planned.source);
            dicts.add_name(planned.file);
            lengths.push(bep6::OWED_TASK_V6);
            body_len = body_len.saturating_add(bep6::OWED_TASK_V6);
        }
        let total_file_members: usize = self.files.iter().map(|&(_, names)| names.len()).sum();
        let mut file_claims: Vec<Option<changes::Claim>, A> = Vec::with_capacity_in(total_file_members, scratch);
        let mut file_digests: SVec<'a, (Name, Option<ContentId>)> = Vec::with_capacity_in(self.files.len(), alloc);
        let mut file_buf: Vec<u8, A> = Vec::new_in(scratch);
        for &(&file, names) in self.files {
            dicts.add_name(file);
            file_buf.clear();
            file_buf.push(bep6::kind_u8(file.kind));
            file_buf.extend_from_slice(&file.subject.0);
            file_buf.extend_from_slice(&u32::try_from(names.len()).unwrap_or(u32::MAX).to_le_bytes());
            let mut standing = true;
            for &name in names.iter() {
                dicts.add_name(name);
                let claim = self
                    .all_rows
                    .get(&name)
                    .and_then(|row| changes::claim(row, file, self.all_owed_tasks.contains_key(&name)));
                standing &= claim.is_some();
                file_claims.push(claim);
                file_buf.push(bep6::kind_u8(name.kind));
                file_buf.extend_from_slice(&name.subject.0);
                match claim {
                    Some(c) => {
                        dicts.add_id(c.key.0);
                        dicts.add_id(c.source);
                        file_buf.push(1);
                        file_buf.extend_from_slice(&c.key.0.0);
                        file_buf.push(c.phase.stage as u8);
                        file_buf.extend_from_slice(&c.phase.wave.to_le_bytes());
                        file_buf.extend_from_slice(&c.source.0);
                    }
                    None => {
                        file_buf.push(0);
                        file_buf.extend_from_slice(&[0u8; ID + 3 + ID]);
                    }
                }
            }
            file_digests.push((file, standing.then(|| changes::file_digest(&file_buf))));
            let len = bep6::IDX + 4 + bep6::FILE_MEMBER_V6 * names.len();
            lengths.push(len);
            body_len = body_len.saturating_add(len);
        }
        for &(&name, &(_, cause)) in self.causes {
            let len = bep6::collect_cause_v6(name, cause, &mut dicts);
            lengths.push(len);
            body_len = body_len.saturating_add(len);
        }
        for &(&name, &rec_key) in self.records {
            dicts.add_name(name);
            dicts.add_id(rec_key.0);
            lengths.push(bep6::RECORD_V6);
            body_len = body_len.saturating_add(bep6::RECORD_V6);
        }
        for &(id, names) in self.readers {
            dicts.add_cell(*id);
            for &name in names.iter() {
                dicts.add_name(name);
            }
            let len = bep6::IDX + 4 + bep6::IDX * names.len();
            lengths.push(len);
            body_len = body_len.saturating_add(len);
        }

        dicts.finalize();
        let body_at = bep6::body_start_v6(entries, dicts.names_len(), dicts.cells_len(), dicts.ids_len()).unwrap_or(0);
        let total_len = body_at.saturating_add(body_len);
        let mut page: SVec<'a, u8> = Vec::with_capacity_in(total_len, alloc);
        bep6::write_header_v6(&mut page, self.bits, self.index, &dicts, counts);

        let mut at = body_at;
        for &len in lengths.iter() {
            put_u32(&mut page, at);
            at = at.saturating_add(len);
        }
        put_u32(&mut page, total_len);
        debug_assert_eq!(at, total_len, "a page's start table reaches its total length");

        dicts.write_dicts(&mut page);

        for (&(&name, row), &indexed) in self.rows.iter().zip(row_indexed.iter()) {
            bep6::write_row_v6(&mut page, name, row, indexed, &dicts);
        }
        for &(id, &(digest, moved)) in self.cells {
            bep6::put_idx(&mut page, dicts.cell_idx(id));
            bep6::put_idx(&mut page, dicts.id_idx(&digest));
            page.extend_from_slice(&moved.0.to_le_bytes());
        }
        for &(id, &moved) in self.owed_cells {
            bep6::put_idx(&mut page, dicts.cell_idx(id));
            page.extend_from_slice(&moved.0.to_le_bytes());
        }
        for &(&name, planned) in self.owed_tasks {
            bep6::put_idx(&mut page, dicts.name_idx(name));
            put_phase(&mut page, planned.phase);
            bep6::put_idx(&mut page, dicts.id_idx(&planned.source));
            bep6::put_idx(&mut page, dicts.name_idx(planned.file));
        }
        let mut claim_cursor = 0usize;
        for &(&file, names) in self.files {
            bep6::put_idx(&mut page, dicts.name_idx(file));
            put_u32(&mut page, names.len());
            for &name in names.iter() {
                bep6::put_idx(&mut page, dicts.name_idx(name));
                let claim = file_claims.get(claim_cursor).copied().flatten();
                claim_cursor = claim_cursor.saturating_add(1);
                match claim {
                    Some(c) => {
                        page.push(1);
                        bep6::put_idx(&mut page, dicts.id_idx(&c.key.0));
                        put_phase(&mut page, c.phase);
                        bep6::put_idx(&mut page, dicts.id_idx(&c.source));
                    }
                    None => {
                        page.push(0);
                        page.extend_from_slice(&[0u8; bep6::IDX + 3 + bep6::IDX]);
                    }
                }
            }
        }
        for &(&name, &(serial, cause)) in self.causes {
            bep6::write_cause_v6(&mut page, name, serial, cause, &dicts);
        }
        for &(&name, &rec_key) in self.records {
            bep6::put_idx(&mut page, dicts.name_idx(name));
            bep6::put_idx(&mut page, dicts.id_idx(&rec_key.0));
        }
        for &(id, names) in self.readers {
            bep6::put_idx(&mut page, dicts.cell_idx(id));
            put_u32(&mut page, names.len());
            for &name in names.iter() {
                bep6::put_idx(&mut page, dicts.name_idx(name));
            }
        }
        debug_assert_eq!(page.len(), total_len, "a BEP6 page's start table ends at its length");

        let key = content_key(&page);
        let byte_len = u64::try_from(page.len()).unwrap_or(u64::MAX);
        let changed = self.last_flat_key != Some(key);
        let (bytes, records) = if changed {
            let mut recs: SVec<'a, (Name, Key)> = Vec::with_capacity_in(self.records.len(), alloc);
            for &(&name, &rec_key) in self.records {
                recs.push((name, rec_key));
            }
            (page, recs)
        } else {
            (Vec::new_in(alloc), Vec::new_in(alloc))
        };
        WholePageBuilt { index: self.index, byte_len, key, bytes, records, files: file_digests, changed }
    }
}

/// Stable CSR partition of an already-sorted stream into `count` page buckets, preserving exact
/// ascending key order within every page while allocating only two flat buffers (zero for empty
/// sections).
struct Partitioned<'a, T> {
    items: SVec<'a, T>,
    offsets: SVec<'a, usize>,
}

impl<'a, T> Partitioned<'a, T> {
    fn empty(alloc: SharedAlloc<'a>) -> Self {
        Self { items: Vec::new_in(alloc), offsets: Vec::new_in(alloc) }
    }

    fn slice(&self, page: usize) -> &[T] {
        let start = self.offsets.get(page).copied().unwrap_or(0);
        let end = self.offsets.get(page + 1).copied().unwrap_or(start);
        self.items.get(start..end).unwrap_or(&[])
    }
}

fn partition_by_page<'a, T: Copy>(
    count: usize,
    iter: impl Iterator<Item = (u32, T)>,
    alloc: SharedAlloc<'a>,
) -> Partitioned<'a, T> {
    let mut iter = iter.peekable();
    if iter.peek().is_none() {
        return Partitioned::empty(alloc);
    }
    let (lower, _) = iter.size_hint();
    let mut tagged: SVec<'a, (u32, T)> = Vec::with_capacity_in(lower, alloc);
    let mut counts: SVec<'a, usize> = Vec::from_elem_in(0usize, count + 1, alloc);
    for (page, item) in iter {
        let p = usize::try_from(page).unwrap_or(0);
        if let Some(slot) = counts.get_mut(p + 1) {
            *slot += 1;
        }
        tagged.push((page, item));
    }
    let mut sum = 0usize;
    for slot in counts.iter_mut() {
        sum += *slot;
        *slot = sum;
    }
    let Some(&(_, first_item)) = tagged.first() else {
        return Partitioned::empty(alloc);
    };
    let mut cursors: SVec<'a, usize> = Vec::from_slice_in(counts.get(..count).unwrap_or(&[]), alloc);
    let mut items: SVec<'a, T> = Vec::from_elem_in(first_item, tagged.len(), alloc);
    for (page, item) in tagged {
        let p = usize::try_from(page).unwrap_or(0);
        if let Some(cursor) = cursors.get_mut(p) {
            let at = *cursor;
            *cursor = at + 1;
            if let Some(dst) = items.get_mut(at) {
                *dst = item;
            }
        }
    }
    Partitioned { items, offsets: counts }
}

fn run_whole_serial<'a>(jobs: &[WholePageJob<'_, '_>], alloc: SharedAlloc<'a>) -> SVec<'a, WholePageBuilt<'a>> {
    let mut out = Vec::with_capacity_in(jobs.len(), alloc);
    for job in jobs {
        out.push(job.run(alloc));
    }
    out
}

/// `kept` as pages of about [`PAGE_BYTES`](blaze_drive::answers::PAGE_BYTES), paged as the last
/// root was while its bits still fit (answers.rs `bits_kept`); `last` is that root, whose pages
/// are not put again.
pub fn encode<'a>(kept: &Kept<'_>, last: Option<&Root<'_>>, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    encode_planted(kept, last, PagesPlant::None, alloc)
}

/// [`encode`] with a caller-supplied executor for independent [`WholePageJob`]s so a
/// multi-threaded driver can serialize, compute per-file plan digests, and BLAKE3-hash every
/// non-empty page in parallel while producing output byte-for-byte identical to serial [`encode`]
/// (Rule 25).
pub fn encode_par<'a, F>(kept: &Kept<'_>, last: Option<&Root<'_>>, alloc: SharedAlloc<'a>, run_pages: F) -> Encoded<'a>
where
    F: FnOnce(&[WholePageJob<'_, '_>]) -> SVec<'a, WholePageBuilt<'a>>,
{
    encode_inner(kept, last, None, PagesPlant::None, alloc, run_pages)
}

/// `kept` encoded at an explicit `bits` count (clamped to `1..=MOST_BITS`), primarily for testing
/// large-paging radix trees (`bits = 10..=16`) without allocating gigabytes of synthetic rows.
pub fn encode_at_bits<'a>(kept: &Kept<'_>, bits: u8, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    encode_inner(kept, None, Some(bits.clamp(1, blaze_drive::answers::MOST_BITS)), PagesPlant::None, alloc, |jobs| run_whole_serial(jobs, alloc))
}

/// [`encode`] with a planted bug ([`PagesPlant`]).
pub fn encode_planted<'a>(kept: &Kept<'_>, last: Option<&Root<'_>>, plant: PagesPlant, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    encode_inner(kept, last, None, plant, alloc, |jobs| run_whole_serial(jobs, alloc))
}

fn encode_inner<'a, F>(
    kept: &Kept<'_>,
    last: Option<&Root<'_>>,
    forced_bits: Option<u8>,
    plant: PagesPlant,
    alloc: SharedAlloc<'a>,
    run_pages: F,
) -> Encoded<'a>
where
    F: FnOnce(&[WholePageJob<'_, '_>]) -> SVec<'a, WholePageBuilt<'a>>,
{
    let answer = &kept.answer;
    let files_kept = plant != PagesPlant::FilesDropped;
    debug_assert!(
        !files_kept
            || answer.rows.iter().all(|(name, row)| {
                row.inline
                    || answer.by_file.get(&row.file).is_some_and(|members| members.binary_search(name).is_ok())
            }),
        "every non-inline row in Answer.rows must be listed under its `row.file` in Answer.by_file (otherwise Edit.files cannot retire stale rows)"
    );
    let has_row = |name: &Name| answer.rows.contains_key(name);
    let bits = match forced_bits {
        Some(bits) => bits.clamp(1, blaze_drive::answers::MOST_BITS),
        None => {
            // The estimated BEP6 bytes in all, which size the paging.
            let mut total = 0u64;
            for row in answer.rows.values() {
                total = total.saturating_add(bep6::estimate_row_v6(row.defines.len(), row.reads.len()));
            }
            let fixed = (bep6::CELL_ENTRY_V6 + bep6::DICT_CELL) * answer.cells.len()
                + (bep6::OWED_CELL_V6 + bep6::DICT_CELL) * answer.owed_cells.len()
                + (bep6::OWED_TASK_V6 + NAME) * answer.owed_tasks.len();
            total = total.saturating_add(u64::try_from(fixed).unwrap_or(u64::MAX));
            for names in answer.by_file.values().filter(|_| files_kept) {
                let f = NAME + 4 + bep6::FILE_MEMBER_V6 * names.len();
                total = total.saturating_add(u64::try_from(f).unwrap_or(u64::MAX));
            }
            for names in answer.readers.values() {
                let r = bep6::IDX + 4 + bep6::IDX * names.len();
                total = total.saturating_add(u64::try_from(r).unwrap_or(u64::MAX));
            }
            let cause_bytes: usize = kept
                .causes
                .iter()
                .filter(|(name, _)| has_row(name))
                .map(|(_, &(_, cause))| bep6::cause_len_v6(cause))
                .sum();
            let rec_bytes = (bep6::RECORD_V6 + ID) * kept.records.keys().filter(|name| has_row(*name)).count();
            total = total.saturating_add(u64::try_from(cause_bytes.saturating_add(rec_bytes)).unwrap_or(u64::MAX));
            bits_kept_partitioned(total, last.map(|root| root.bits))
        }
    };
    let count = 1usize << bits;
    let rows_part = partition_by_page(count, answer.rows.iter().map(|(name, row)| (content_page_of(*name, bits), (name, row))), alloc);
    let cells_part = partition_by_page(count, answer.cells.iter().map(|(id, val)| (content_page_of(id.of, bits), (id, val))), alloc);
    let owed_cells_part = partition_by_page(count, answer.owed_cells.iter().map(|(id, moved)| (content_page_of(id.of, bits), (id, moved))), alloc);
    let owed_tasks_part = partition_by_page(count, answer.owed_tasks.iter().map(|(name, planned)| (content_page_of(*name, bits), (name, planned))), alloc);
    let files_part = if files_kept {
        partition_by_page(count, answer.by_file.iter().map(|(file, names)| (content_page_of(*file, bits), (file, names))), alloc)
    } else {
        Partitioned::empty(alloc)
    };
    let causes_part = partition_by_page(
        count,
        kept.causes.iter().filter(|(name, _)| has_row(name)).map(|(name, cause)| (content_page_of(*name, bits), (name, cause))),
        alloc,
    );
    let records_part = partition_by_page(
        count,
        kept.records.iter().filter(|(name, _)| has_row(name)).map(|(name, key)| (content_page_of(*name, bits), (name, key))),
        alloc,
    );
    let readers_part = partition_by_page(
        count,
        answer.readers.iter().map(|(id, names)| (readers_page_of(id, bits), (id, names))),
        alloc,
    );

    let same_paging = last.filter(|root| root.bits == bits);
    let radix_same_paging = bits > RADIX_FLAT_MAX_BITS && same_paging.is_some();
    let mut jobs: SVec<'_, WholePageJob<'_, '_>> = Vec::new_in(alloc);
    for index in 0..count {
        let rows = rows_part.slice(index);
        let cells = cells_part.slice(index);
        let owed_cells = owed_cells_part.slice(index);
        let owed_tasks = owed_tasks_part.slice(index);
        let files = files_part.slice(index);
        let causes = causes_part.slice(index);
        let records = records_part.slice(index);
        let readers = readers_part.slice(index);
        if rows.is_empty()
            && cells.is_empty()
            && owed_cells.is_empty()
            && owed_tasks.is_empty()
            && files.is_empty()
            && causes.is_empty()
            && records.is_empty()
            && readers.is_empty()
        {
            continue;
        }
        let last_flat_key = if radix_same_paging {
            None
        } else {
            same_paging.and_then(|root| root.pages.get(index).copied())
        };
        jobs.push(WholePageJob {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            bits,
            last_flat_key,
            all_rows: &answer.rows,
            all_owed_tasks: &answer.owed_tasks,
            rows,
            cells,
            owed_cells,
            owed_tasks,
            files,
            causes,
            records,
            readers,
        });
    }

    let mut built = run_pages(&jobs);
    drop(jobs);
    drop(rows_part);
    drop(cells_part);
    drop(owed_cells_part);
    drop(owed_tasks_part);
    drop(files_part);
    drop(causes_part);
    drop(records_part);
    drop(readers_part);

    // Guarantee deterministic ascending page-index order regardless of worker completion order (Rule 25).
    if !built.windows(2).all(|pair| pair.first().map(|a| a.index) < pair.get(1).map(|b| b.index)) {
        built.sort_unstable_by_key(|one| one.index);
    }

    let mut directory = changes::directory(kept, alloc);
    let mut pages: SVec<'a, Key> = Vec::from_elem_in(Key::default(), count, alloc);
    let mut puts: SVec<'a, (Key, SVec<'a, u8>)> = Vec::new_in(alloc);
    let mut records = Vec::new_in(alloc);
    let mut bytes = 0u64;
    let mut live_pages = 0u32;

    if radix_same_paging {
        for one in built.iter_mut() {
            let idx = usize::try_from(one.index).unwrap_or(0);
            if let Some(slot) = pages.get_mut(idx) {
                *slot = one.key;
            }
            bytes += one.byte_len;
            live_pages = live_pages.saturating_add(1);
            for (file, digest) in one.files.drain(..) {
                directory.files.insert(file, digest);
            }
        }
        let shard_count = 1usize << (bits - RADIX_LEAF_BITS);
        let mut shard_unchanged: SVec<'a, bool> = Vec::with_capacity_in(shard_count, alloc);
        for shard_idx in 0..shard_count {
            let from = shard_idx * RADIX_LEAF_PAGES;
            let to = from + RADIX_LEAF_PAGES;
            let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
            let slice = pages.get(from..to).unwrap_or(&[]);
            let unchanged = changes::encode_radix_shard(bits, shard_u32, slice, alloc)
                .is_some_and(|(shard_key, _)| same_paging.is_some_and(|r| r.page_shards.get(shard_idx) == Some(&shard_key)));
            shard_unchanged.push(unchanged);
        }
        for one in built {
            let (shard_idx, _) = Root::shard_of(usize::try_from(one.index).unwrap_or(0));
            if !shard_unchanged.get(shard_idx).copied().unwrap_or(false) {
                records.push((one.key, one.records));
                puts.push((one.key, one.bytes));
            }
        }
    } else {
        for one in built {
            let idx = usize::try_from(one.index).unwrap_or(0);
            if let Some(slot) = pages.get_mut(idx) {
                *slot = one.key;
            }
            bytes += one.byte_len;
            live_pages = live_pages.saturating_add(1);
            for (file, digest) in one.files {
                directory.files.insert(file, digest);
            }
            if one.changed {
                records.push((one.key, one.records));
                puts.push((one.key, one.bytes));
            }
        }
    }
    // Each planned file's place beside its digest; a place of no file the root plans is not kept.
    directory.places.extend(kept.places.iter().filter(|(file, _)| directory.files.contains_key(*file)).map(|(&file, &place)| (file, place)));
    directory.serial = answer.serial;
    directory.bits = bits;
    directory.bytes = bytes;
    directory.live_pages = live_pages;
    directory.finish_whole(pages, same_paging, puts, records, plant, alloc)
}


/// Bytes read in order, each refusal naming its page and offset.
struct Rd<'b> {
    bytes: &'b [u8],
    at: usize,
    page: u32,
}

impl<'b> Rd<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], PagesError> {
        if self.bytes.len() - self.at < n {
            return Err(PagesError::Short { page: self.page, at: self.bytes.len() as u32 });
        }
        let taken = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, PagesError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, PagesError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, PagesError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, PagesError> {
        let b = self.take(8)?;
        let mut word = [0; 8];
        word.copy_from_slice(b);
        Ok(u64::from_le_bytes(word))
    }

    fn id(&mut self) -> Result<ContentId, PagesError> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(ID)?);
        Ok(ContentId(id))
    }

    fn tag(&self, byte: u8) -> PagesError {
        PagesError::Tag { page: self.page, at: self.at.saturating_sub(1) as u32, byte }
    }

    fn name(&mut self) -> Result<Name, PagesError> {
        let byte = self.u8()?;
        let kind = Kind::from_u8(byte).ok_or(self.tag(byte))?;
        Ok(Name::new(kind, self.id()?))
    }

    fn cell(&mut self) -> Result<Cell, PagesError> {
        let mut cell = [0; CELL];
        cell.copy_from_slice(self.take(CELL)?);
        Ok(Cell(cell))
    }

    fn cell_id(&mut self) -> Result<CellId, PagesError> {
        Ok(CellId { of: self.name()?, cell: self.cell()? })
    }

    fn phase(&mut self) -> Result<Phase, PagesError> {
        let byte = self.u8()?;
        let stage = *Stage::ALL.get(usize::from(byte)).ok_or(self.tag(byte))?;
        Ok(Phase::new(stage, self.u16()?))
    }

    /// A count of entries of `width` bytes each that the page still holds.
    fn count(&mut self, width: usize) -> Result<usize, PagesError> {
        let n = self.u32()? as usize;
        match n.checked_mul(width) {
            Some(need) if need <= self.bytes.len() - self.at => Ok(n),
            _ => Err(PagesError::Short { page: self.page, at: self.bytes.len() as u32 }),
        }
    }
}

/// Decodes one 256-page radix leaf shard (`BEX1`) whose bytes are proved by `sealed`.
pub fn decode_radix_shard<'a>(sealed: Sealed<'_>, shard: u32, bits: u8, alloc: SharedAlloc<'a>) -> Result<RadixShard<'a>, PagesError> {
    let bytes = sealed.bytes();
    let mut r = Rd { bytes, at: 0, page: ROOT_PAGE };
    if r.take(4)? != RADIX_SHARD_MAGIC || r.u8()? != bits || r.u32()? != shard {
        return Err(PagesError::Magic { page: ROOT_PAGE });
    }
    if bytes.len() != RADIX_SHARD_BYTES {
        return Err(if bytes.len() < RADIX_SHARD_BYTES {
            PagesError::Short { page: ROOT_PAGE, at: bytes.len() as u32 }
        } else {
            PagesError::Trailing { page: ROOT_PAGE, at: RADIX_SHARD_BYTES as u32 }
        });
    }
    let mut keys = blaze_own::Box::new_in([Key::default(); RADIX_LEAF_PAGES], alloc);
    for slot in keys.iter_mut() {
        *slot = Key(r.id()?);
    }
    Ok(RadixShard { keys })
}

/// The root's bytes, known to be its key's ([`Sealed`]), read whole for inline flat roots
/// (`bits <= RADIX_FLAT_MAX_BITS`). Radix roots (`bits > RADIX_FLAT_MAX_BITS`) require
/// [`decode_root_with`] so their directory sub-records can be read from the store.
pub fn decode_root<'a>(root: Sealed<'_>, alloc: SharedAlloc<'a>) -> Result<Root<'a>, PagesError> {
    decode_root_with(root, |_| None, alloc)
}

/// Decodes a flat (`flags == 0`) or 2-level radix (`flags == 1`) answer root. In radix mode, reads
/// only the top root and its directory sub-records (`places`, `summary`, non-empty `file_shards`)
/// through `get` — ZERO page leaf shards are fetched upfront.
pub fn decode_root_with<'a, 'g>(root: Sealed<'_>, get: impl FnMut(Key) -> Got<'g>, alloc: SharedAlloc<'a>) -> Result<Root<'a>, PagesError> {
    let mut r = Rd { bytes: root.bytes(), at: 0, page: ROOT_PAGE };
    if r.take(4)? != ROOT_MAGIC {
        return Err(PagesError::Magic { page: ROOT_PAGE });
    }
    let serial = Gen(r.u64()?);
    let bits = r.u8()?;
    let total = r.u64()?;
    let flags = r.u8()?;
    let live_pages = r.u32()?;
    let outlines = r.u32()?;
    if bits == 0 || bits > blaze_drive::answers::MOST_BITS {
        return Err(PagesError::Count { page: ROOT_PAGE, count: u32::from(bits) });
    }
    let max_pages = 1u32 << bits;
    if live_pages > max_pages {
        return Err(PagesError::Count { page: ROOT_PAGE, count: live_pages });
    }
    match flags {
        ROOT_FLAG_FLAT => {
            if bits > RADIX_FLAT_MAX_BITS {
                return Err(PagesError::Tag { page: ROOT_PAGE, at: 21, byte: flags });
            }
            let count = r.count(ID)?;
            if count != 1usize << bits {
                return Err(PagesError::Count { page: ROOT_PAGE, count: count as u32 });
            }
            let mut pages = Vec::with_capacity_in(count, alloc);
            let mut nonzero = 0u32;
            for _ in 0..count {
                let key = Key(r.id()?);
                if key != Key::default() {
                    nonzero = nonzero.saturating_add(1);
                }
                pages.push(key);
            }
            if live_pages != nonzero {
                return Err(PagesError::Count { page: ROOT_PAGE, count: live_pages });
            }
            changes::read_flat_directory(r, serial, bits, total, live_pages, outlines, pages, alloc)
        }
        ROOT_FLAG_RADIX => {
            if bits <= RADIX_FLAT_MAX_BITS {
                return Err(PagesError::Tag { page: ROOT_PAGE, at: 21, byte: flags });
            }
            let expected_shards = 1usize << (bits - RADIX_LEAF_BITS);
            let shard_count = r.count(ID)?;
            if shard_count != expected_shards {
                return Err(PagesError::Count { page: ROOT_PAGE, count: shard_count as u32 });
            }
            let mut page_shards = Vec::with_capacity_in(shard_count, alloc);
            let mut nonempty_shards = 0u32;
            for _ in 0..shard_count {
                let key = Key(r.id()?);
                if key != Key::default() {
                    nonempty_shards = nonempty_shards.saturating_add(1);
                }
                page_shards.push(key);
            }
            let radix_cap = nonempty_shards.saturating_mul(u32::try_from(RADIX_LEAF_PAGES).unwrap_or(u32::MAX));
            if live_pages > radix_cap {
                return Err(PagesError::Count { page: ROOT_PAGE, count: live_pages });
            }
            changes::read_radix_directory(r, serial, bits, total, live_pages, outlines, page_shards, get, alloc)
        }
        other => Err(PagesError::Tag { page: ROOT_PAGE, at: 21, byte: other }),
    }
}


/// What [`decode`] asks for a key: `None` when the store has no record of it; else the record
/// as [`Sealed`], or the [`Torn`] that [`Sealed::hashed`] found.
pub type Got<'g> = Option<Result<Sealed<'g>, Torn>>;

/// The answer the root `key` names, every record through `get`, each known once to be its
/// key's ([`Sealed`]), then walked whole (the module docs, "Checked once").
pub fn decode<'a, 'g>(key: Key, mut get: impl FnMut(Key) -> Got<'g>, alloc: SharedAlloc<'a>) -> Result<Kept<'a>, PagesError> {
    let root = get(key).ok_or(PagesError::Missing { page: ROOT_PAGE, key })?.map_err(|torn| torn.at(ROOT_PAGE))?;
    let root = decode_root_with(root, &mut get, alloc)?;
    decode_at(&root, get, alloc)
}

/// Exhaustive read for inspection and tooling that explicitly asks for every row: each page
/// walked whole ([`VerifiedPage::open_whole`]), as it restores every entry anyway.
pub fn decode_at<'a, 'g>(root: &Root<'_>, mut get: impl FnMut(Key) -> Got<'g>, alloc: SharedAlloc<'a>) -> Result<Kept<'a>, PagesError> {
    let mut kept = Kept::new_in(alloc);
    kept.answer.serial = root.serial;
    kept.places.extend(root.places.iter().map(|(&file, &place)| (file, place)));
    let mut reader_pairs: SVec<'a, (CellId, Name)> = Vec::new_in(alloc);
    if !root.is_radix() {
        for (index, &key) in root.pages.iter().enumerate() {
            if key == Key::default() { continue; }
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let sealed = get(key).ok_or(PagesError::Missing { page: index, key })?.map_err(|torn| torn.at(index))?;
            let page = VerifiedPage::open_whole(sealed, index, root.bits)?;
            page.restore(&mut kept, &mut reader_pairs, alloc);
        }
    } else {
        for (shard_idx, &shard_key) in root.page_shards.iter().enumerate() {
            if shard_key == Key::default() { continue; }
            let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
            let sealed_shard = get(shard_key).ok_or(PagesError::Missing { page: ROOT_PAGE, key: shard_key })?.map_err(|torn| torn.at(ROOT_PAGE))?;
            let shard = decode_radix_shard(sealed_shard, shard_u32, root.bits, alloc)?;
            let base_index = shard_idx << RADIX_LEAF_BITS;
            for (slot, &key) in shard.keys.iter().enumerate() {
                if key == Key::default() { continue; }
                let index = u32::try_from(base_index + slot).unwrap_or(u32::MAX);
                let sealed = get(key).ok_or(PagesError::Missing { page: index, key })?.map_err(|torn| torn.at(index))?;
                let page = VerifiedPage::open_whole(sealed, index, root.bits)?;
                page.restore(&mut kept, &mut reader_pairs, alloc);
            }
        }
    }
    kept.answer.readers = Readers::from_pairs(reader_pairs, alloc);
    Ok(kept)
}

/// The simulations' read of a root they hold as bytes: hashed against its key (no store proves
/// it), then read.
#[cfg(test)]
pub(crate) fn decode_root_hashed<'a>(key: Key, bytes: &[u8], alloc: SharedAlloc<'a>) -> Result<Root<'a>, PagesError> {
    decode_root(Sealed::hashed(key, bytes).map_err(|torn| torn.at(ROOT_PAGE))?, alloc)
}

/// A page's own structure walked whole, its bytes taken as its key's: what the cut-page test
/// refuses by the form alone.
#[cfg(test)]
pub(crate) fn decode_page_for_test(bytes: &[u8], page: u32, bits: u8) -> Result<(), PagesError> {
    let sealed = Sealed { key: content_key(bytes), bytes, check: Check::Hashed };
    VerifiedPage::open_whole(sealed, page, bits).map(|_| ())
}

#[cfg(test)]
mod codex_regression_tests {
    use super::*;

    #[test]
    fn decode_root_rejects_unbounded_live_pages() {
        let alloc = blaze_own::testing::heap().shared();
        let make_flat = |bits: u8, live_pages: u32| {
            let mut flat = Vec::new_in(alloc);
            flat.extend_from_slice(ROOT_MAGIC);
            flat.extend_from_slice(&0u64.to_le_bytes()); // serial
            flat.push(bits);
            flat.extend_from_slice(&0u64.to_le_bytes()); // total
            flat.push(ROOT_FLAG_FLAT);
            flat.extend_from_slice(&live_pages.to_le_bytes());
            flat.extend_from_slice(&0u32.to_le_bytes()); // outlines
            let n = 1u32 << bits;
            flat.extend_from_slice(&n.to_le_bytes()); // count
            for _ in 0..n {
                flat.extend_from_slice(&[0u8; 32]);
            }
            flat
        };

        let flat = make_flat(2, 999_999);
        let key = content_key(&flat);
        assert!(
            matches!(decode_root_hashed(key, &flat, alloc), Err(PagesError::Count { page: ROOT_PAGE, .. })),
            "flat root with live_pages > 1 << bits must be rejected"
        );

        let flat_mismatch = make_flat(2, 2);
        let key2 = content_key(&flat_mismatch);
        assert!(
            matches!(decode_root_hashed(key2, &flat_mismatch, alloc), Err(PagesError::Count { page: ROOT_PAGE, .. })),
            "flat root whose live_pages disagrees with nonzero page keys must be rejected"
        );
    }

    #[test]
    fn encode_par_matches_serial_and_round_trips_out_of_order() {
        let alloc = blaze_own::testing::heap().shared();
        let mut kept = Kept::new_in(alloc);
        kept.answer.serial = Gen(7);
        let phase = Phase::new(Stage::Bodies, 0);
        let mut reader_pairs = Vec::new_in(alloc);
        for i in 0..96u32 {
            let file = Name::new(Kind::FileItems, ContentId::of("par file", &(i % 8).to_le_bytes()));
            let name = Name::new(Kind::BodyCheck, ContentId::of("par task", &i.to_le_bytes()));
            let src = ContentId::of("par src", &i.to_le_bytes());
            let out = ContentId::of("par out", &i.to_le_bytes());
            let rec_key = Key(ContentId::of("par rec", &i.to_le_bytes()));
            let cell = Cell(*ContentId::of("par cell", &i.to_le_bytes()).0.first_chunk::<16>().unwrap_or(&[0; 16]));
            let cid = CellId { of: name, cell };
            let mut defines = Vec::new_in(alloc);
            defines.push((cell, out));
            let mut reads = Vec::new_in(alloc);
            let mut by_cell = Vec::new_in(alloc);
            if i > 0 {
                let prev = Name::new(Kind::BodyCheck, ContentId::of("par task", &(i - 1).to_le_bytes()));
                let prev_cell = Cell(*ContentId::of("par cell", &(i - 1).to_le_bytes()).0.first_chunk::<16>().unwrap_or(&[0; 16]));
                let prev_id = CellId { of: prev, cell: prev_cell };
                reads.push((prev_id, ContentId::of("par out", &(i - 1).to_le_bytes())));
                by_cell.push(0u32);
                reader_pairs.push((prev_id, name));
            }
            kept.answer.cells.insert(cid, (out, Gen(7)));
            kept.answer.by_file.entry(file).or_insert_with(|| Vec::new_in(alloc)).push(name);
            kept.places.insert(file, FilePlace { unit: file, edition: 3, flags: 1 });
            kept.answer.rows.insert(name, Row {
                key: Key(src), phase, file, source: src, out, serial: Gen(7),
                defines, reads, by_cell, findings: i % 3, inline: i % 5 == 0, failed: i % 7 == 0,
            });
            kept.causes.insert(name, (Gen(7), Cause::New));
            kept.records.insert(name, rec_key);
        }
        kept.answer.readers = Readers::from_pairs(reader_pairs, alloc);
        for names in kept.answer.by_file.values_mut() {
            names.sort_unstable();
        }
        let owed_file = Name::new(Kind::FileItems, ContentId::of("par owed file", b"f"));
        let owed_name = Name::new(Kind::BodyCheck, ContentId::of("par owed task", b"t"));
        kept.answer.owed_tasks.insert(owed_name, Planned { name: owed_name, phase, source: ContentId::of("par owed src", b"s"), file: owed_file });
        let owed_cid = CellId { of: owed_name, cell: Cell([3; 16]) };
        kept.answer.owed_cells.insert(owed_cid, Gen(7));

        let serial = encode(&kept, None, alloc);
        let par_rev = encode_par(&kept, None, alloc, |jobs| {
            let mut out = Vec::with_capacity_in(jobs.len(), alloc);
            for job in jobs.iter().rev() {
                out.push(job.run(alloc));
            }
            out
        });
        assert_eq!(serial.root, par_rev.root, "encode_par must match serial encode root byte-for-byte");
        assert_eq!(serial.bits, par_rev.bits);
        assert_eq!(serial.bytes, par_rev.bytes);
        assert_eq!(serial.live_pages, par_rev.live_pages);
        assert_eq!(serial.pages, par_rev.pages);
        assert_eq!(serial.page_shards, par_rev.page_shards);
        assert_eq!(serial.tree_keys, par_rev.tree_keys);
        assert_eq!(serial.puts.len(), par_rev.puts.len());
        for ((sk, sb), (pk, pb)) in serial.puts.iter().zip(par_rev.puts.iter()) {
            assert_eq!(sk, pk);
            assert_eq!(sb.as_slice(), pb.as_slice());
        }

        // Round-trip decode over `par_rev.puts` must produce the exact same root (Rule 26).
        let mut store: BTreeMap<Key, &[u8], _> = BTreeMap::new_in(alloc);
        for (k, bytes) in par_rev.puts.iter() {
            store.insert(*k, bytes.as_slice());
        }
        let decoded = match decode(par_rev.root, |k| store.get(&k).map(|b| Sealed::hashed(k, b)), alloc) {
            Ok(back) => back,
            Err(why) => panic!("decode after encode_par failed: {why}"),
        };
        let reencoded = encode(&decoded, None, alloc);
        assert_eq!(reencoded.root, par_rev.root, "round-trip after encode_par must preserve root");
    }

    #[test]
    fn previous_root_magic_ber5_is_rejected() {
        let alloc = blaze_own::testing::heap().shared();
        let mut ber5 = Vec::new_in(alloc);
        ber5.extend_from_slice(b"BER5");
        ber5.extend_from_slice(&[0u8; 64]);
        let key = content_key(&ber5);
        assert_eq!(
            decode_root_hashed(key, &ber5, alloc).err(),
            Some(PagesError::Magic { page: ROOT_PAGE }),
            "old BER5 mixed root magic must be rejected with PagesError::Magic"
        );

        // `BER6` with `bits == 0` is invalid (partitioned roots require `bits >= 1`).
        let mut zero_bits = Vec::new_in(alloc);
        zero_bits.extend_from_slice(ROOT_MAGIC);
        zero_bits.extend_from_slice(&0u64.to_le_bytes()); // serial
        zero_bits.push(0); // bits == 0
        zero_bits.extend_from_slice(&0u64.to_le_bytes()); // total
        zero_bits.push(ROOT_FLAG_FLAT);
        zero_bits.extend_from_slice(&0u32.to_le_bytes()); // live_pages
        zero_bits.extend_from_slice(&0u32.to_le_bytes()); // outlines
        zero_bits.extend_from_slice(&1u32.to_le_bytes()); // count
        zero_bits.extend_from_slice(&[0u8; 32]);
        let key0 = content_key(&zero_bits);
        assert_eq!(
            decode_root_hashed(key0, &zero_bits, alloc).err(),
            Some(PagesError::Count { page: ROOT_PAGE, count: 0 }),
            "BER6 root with bits == 0 must be rejected"
        );
    }

    #[test]
    fn partitioned_content_and_readers_round_trip_flat_and_radix() {
        let alloc = blaze_own::testing::heap().shared();
        let mut kept = Kept::new_in(alloc);
        kept.answer.serial = Gen(3);
        let phase = Phase::new(Stage::Bodies, 0);
        let mut reader_pairs = Vec::new_in(alloc);
        for i in 0..64u32 {
            let file = Name::new(Kind::FileItems, ContentId::of("part file", &(i % 6).to_le_bytes()));
            let name = Name::new(Kind::BodyCheck, ContentId::of("part task", &i.to_le_bytes()));
            let src = ContentId::of("part src", &i.to_le_bytes());
            let out = ContentId::of("part out", &i.to_le_bytes());
            let rec = Key(ContentId::of("part rec", &i.to_le_bytes()));
            let cell = Cell(*ContentId::of("part cell", &i.to_le_bytes()).0.first_chunk::<16>().unwrap_or(&[0; 16]));
            let cid = CellId { of: name, cell };
            let mut defines = Vec::new_in(alloc);
            defines.push((cell, out));
            let mut reads = Vec::new_in(alloc);
            let mut by_cell = Vec::new_in(alloc);
            if i > 0 {
                let prev = Name::new(Kind::BodyCheck, ContentId::of("part task", &(i - 1).to_le_bytes()));
                let prev_cell = Cell(*ContentId::of("part cell", &(i - 1).to_le_bytes()).0.first_chunk::<16>().unwrap_or(&[0; 16]));
                let prev_id = CellId { of: prev, cell: prev_cell };
                reads.push((prev_id, ContentId::of("part out", &(i - 1).to_le_bytes())));
                by_cell.push(0u32);
                reader_pairs.push((prev_id, name));
            }
            kept.answer.cells.insert(cid, (out, Gen(3)));
            kept.answer.by_file.entry(file).or_insert_with(|| Vec::new_in(alloc)).push(name);
            kept.places.insert(file, FilePlace { unit: file, edition: 3, flags: 1 });
            kept.answer.rows.insert(name, Row {
                key: Key(src), phase, file, source: src, out, serial: Gen(3),
                defines, reads, by_cell, findings: 0, inline: false, failed: false,
            });
            kept.causes.insert(name, (Gen(3), Cause::New));
            kept.records.insert(name, rec);
        }
        kept.answer.readers = Readers::from_pairs(reader_pairs, alloc);
        for names in kept.answer.by_file.values_mut() {
            names.sort_unstable();
        }

        for bits in [4u8, 10u8, 16u8] {
            let encoded = encode_at_bits(&kept, bits, alloc);
            assert_eq!(encoded.bits, bits);
            assert_eq!(encoded.is_radix(), bits > RADIX_FLAT_MAX_BITS);
            let half = half_pages(bits);

            let mut store: BTreeMap<Key, &[u8], _> = BTreeMap::new_in(alloc);
            for (k, bytes) in encoded.puts.iter() {
                store.insert(*k, bytes.as_slice());
            }

            // Build lookup of page_key -> page_records len.
            let mut recs_len: BTreeMap<Key, usize, _> = BTreeMap::new_in(alloc);
            for (k, recs) in encoded.page_records() {
                recs_len.insert(k, recs.len());
            }

            let root = decode_root_with(
                Sealed::hashed(encoded.root, store[&encoded.root]).expect("sealed root"),
                |k| store.get(&k).map(|b| Sealed::hashed(k, b)),
                alloc,
            )
            .expect("decode root");

            let mut seen_content_pages = 0usize;
            let mut seen_readers_pages = 0usize;
            let mut check_page = |idx: u32, key: Key| {
                let sealed = Sealed::hashed(key, store[&key]).expect("sealed page");
                let vp = VerifiedPage::open_whole(sealed, idx, bits).expect("open_whole");
                if idx < half {
                    seen_content_pages += 1;
                    assert!(vp.entries(READERS).next().is_none(), "lower-half content page {idx} must hold 0 READERS");
                } else {
                    seen_readers_pages += 1;
                    assert!(vp.row_names().next().is_none(), "upper-half readers page {idx} must hold 0 ROWS");
                    assert!(vp.entries(CELLS).next().is_none(), "upper-half readers page {idx} must hold 0 CELLS");
                    assert!(vp.records().next().is_none(), "upper-half readers page {idx} must hold 0 RECORDS");
                    assert_eq!(recs_len.get(&key).copied().unwrap_or(0), 0, "upper-half readers page {idx} must publish 0 page_records");
                    assert!(vp.entries(READERS).next().is_some(), "live upper-half page {idx} must hold READERS");
                }
            };

            if !root.is_radix() {
                for (idx, &key) in root.pages.iter().enumerate() {
                    if key != Key::default() {
                        check_page(u32::try_from(idx).unwrap_or(u32::MAX), key);
                    }
                }
            } else {
                for (shard_idx, &shard_key) in root.page_shards.iter().enumerate() {
                    if shard_key == Key::default() {
                        continue;
                    }
                    let shard = decode_radix_shard(
                        Sealed::hashed(shard_key, store[&shard_key]).expect("sealed shard"),
                        u32::try_from(shard_idx).unwrap_or(u32::MAX),
                        bits,
                        alloc,
                    )
                    .expect("decode shard");
                    let base = shard_idx << RADIX_LEAF_BITS;
                    for (slot, &key) in shard.keys.iter().enumerate() {
                        if key != Key::default() {
                            check_page(u32::try_from(base + slot).unwrap_or(u32::MAX), key);
                        }
                    }
                }
            }
            assert!(seen_content_pages > 0 && seen_readers_pages > 0);

            let back = decode(encoded.root, |k| store.get(&k).map(|b| Sealed::hashed(k, b)), alloc)
                .expect("round-trip decode");
            assert_eq!(encode_at_bits(&back, bits, alloc).root, encoded.root, "round-trip at bits={bits} must preserve root");
        }
    }

    #[test]
    fn adding_reader_mutates_only_upper_half_leaving_content_pages_byte_identical() {
        let alloc = blaze_own::testing::heap().shared();
        let tools = crate::Tools([blaze_drive::key::Tool(ContentId([5; 32])); blaze_drive::key::KINDS]);
        let phase_names = Phase::new(Stage::Names, 0);
        let phase_bodies = Phase::new(Stage::Bodies, 0);

        for bits in [4u8, 10u8] {
            let half = half_pages(bits);
            let producer_file = Name::new(Kind::FileItems, ContentId::of("core file", b"core/lib.rs"));
            let producer = Name::new(Kind::CrateScope, ContentId::of("core scope", b"core"));
            let producer_page = content_page_of(producer, bits);
            assert!(producer_page < half);

            // Pick a workspace file and two body tasks on content pages distinct from `producer_page`.
            let ws_file = (0..1000u32)
                .map(|i| Name::new(Kind::FileItems, ContentId::of("ws file", &i.to_le_bytes())))
                .find(|f| content_page_of(*f, bits) != producer_page)
                .expect("distinct ws file page");
            let existing_body = (0..1000u32)
                .map(|i| Name::new(Kind::BodyCheck, ContentId::of("ws body 0", &i.to_le_bytes())))
                .find(|n| content_page_of(*n, bits) != producer_page)
                .expect("distinct existing body page");
            let added_body = (0..1000u32)
                .map(|i| Name::new(Kind::BodyCheck, ContentId::of("ws body 1", &i.to_le_bytes())))
                .find(|n| content_page_of(*n, bits) != producer_page && *n != existing_body)
                .expect("distinct added body page");

            let producer_src = ContentId::of("core src", b"v1");
            let producer_rec = Key(ContentId::of("core rec", b"v1"));
            let body_src = ContentId::of("body src", b"v1");
            let body_rec = Key(ContentId::of("body rec", b"v1"));

            // Producer defines 12 cells on `producer_page`.
            let mut producer_cells: Vec<(CellId, ContentId), _> = Vec::new_in(alloc);
            let mut defines: Vec<(Cell, ContentId), _> = Vec::new_in(alloc);
            for c in 0..12u8 {
                let cell = Cell([c + 1; 16]);
                let dig = ContentId::of("core cell dig", &[c]);
                defines.push((cell, dig));
                producer_cells.push((CellId { of: producer, cell }, dig));
            }

            let mut cold = Kept::new_in(alloc);
            cold.answer.serial = Gen(1);
            for &(cid, dig) in producer_cells.iter() {
                cold.answer.cells.insert(cid, (dig, Gen(1)));
            }
            cold.answer.rows.insert(producer, Row {
                key: tools.key(producer, &producer_src), phase: phase_names, file: producer_file,
                source: producer_src, out: producer_rec.0, serial: Gen(1),
                defines, reads: Vec::new_in(alloc), by_cell: Vec::new_in(alloc),
                findings: 0, inline: false, failed: false,
            });
            cold.causes.insert(producer, (Gen(1), Cause::New));
            cold.records.insert(producer, producer_rec);
            let mut prod_members = Vec::new_in(alloc);
            prod_members.push(producer);
            cold.answer.by_file.insert(producer_file, prod_members);
            cold.places.insert(producer_file, FilePlace { unit: producer_file, edition: 3, flags: 0 });

            // Existing body already reads cell 0.
            let mut b0_reads = Vec::new_in(alloc);
            b0_reads.push(producer_cells[0]);
            let mut b0_idx = Vec::new_in(alloc);
            b0_idx.push(0u32);
            cold.answer.rows.insert(existing_body, Row {
                key: tools.key(existing_body, &body_src), phase: phase_bodies, file: ws_file,
                source: body_src, out: body_rec.0, serial: Gen(1),
                defines: Vec::new_in(alloc), reads: b0_reads, by_cell: b0_idx,
                findings: 0, inline: false, failed: false,
            });
            cold.causes.insert(existing_body, (Gen(1), Cause::New));
            cold.records.insert(existing_body, body_rec);
            let mut ws_members = Vec::new_in(alloc);
            ws_members.push(existing_body);
            cold.answer.by_file.insert(ws_file, ws_members);
            cold.places.insert(ws_file, FilePlace { unit: ws_file, edition: 3, flags: 0 });
            let mut rpairs = Vec::new_in(alloc);
            rpairs.push((producer_cells[0].0, existing_body));
            cold.answer.readers = Readers::from_pairs(rpairs, alloc);

            let enc_v1 = encode_at_bits(&cold, bits, alloc);
            let mut store: BTreeMap<Key, SVec<'_, u8>, _> = BTreeMap::new_in(alloc);
            for (k, b) in enc_v1.puts.iter() {
                store.insert(*k, SVec::from_slice_in(b, alloc));
            }
            let root_v1 = decode_root_with(
                Sealed::hashed(enc_v1.root, &store[&enc_v1.root]).expect("root v1"),
                |k| store.get(&k).map(|b| Sealed::hashed(k, b)),
                alloc,
            )
            .expect("decode root v1");

            // Capture producer's exact lower-half page key before the edit.
            let page_key_in = |root: &Root<'_>, idx: u32, st: &BTreeMap<Key, SVec<'static, u8>, SharedAlloc<'static>>| -> Key {
                let i = usize::try_from(idx).unwrap_or(usize::MAX);
                if !root.is_radix() {
                    root.pages[i]
                } else {
                    let (s, slot) = Root::shard_of(i);
                    let sk = root.page_shards[s];
                    if sk == Key::default() {
                        return Key::default();
                    }
                    let shard = decode_radix_shard(
                        Sealed::hashed(sk, &st[&sk]).expect("shard"),
                        u32::try_from(s).unwrap_or(u32::MAX),
                        root.bits,
                        alloc,
                    )
                    .expect("decode shard");
                    shard.keys[slot]
                }
            };
            let producer_page_key_v1 = page_key_in(&root_v1, producer_page, &store);
            assert_ne!(producer_page_key_v1, Key::default());

            // Pre-open all live pages into a `StoredPages` map.
            let mut opened: BTreeMap<u32, VerifiedPage<'_>, SharedAlloc<'static>> = BTreeMap::new_in(alloc);
            for idx in 0..(1u32 << bits) {
                let k = page_key_in(&root_v1, idx, &store);
                if k != Key::default() {
                    let vp = VerifiedPage::open_whole(Sealed::hashed(k, &store[&k]).expect("page"), idx, bits).expect("open page");
                    opened.insert(idx, vp);
                }
            }
            struct TestStored<'p, 'm> {
                bits: u8,
                pages: &'m BTreeMap<u32, VerifiedPage<'p>, SharedAlloc<'static>>,
            }
            impl crate::StoredPages for TestStored<'_, '_> {
                fn page(&self, name: Name) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
                    Ok(self.pages.get(&content_page_of(name, self.bits)))
                }
                fn page_for(&self, name: Name, ask: crate::Ask) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
                    Ok(self.pages.get(&page_for_ask(name, ask, self.bits)))
                }
                fn finding(&self, _: Name) -> Option<u32> { None }
                fn refusal(&self) -> Option<PagesError> { None }
                fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), crate::LastError>) -> Result<(), crate::LastError> {
                    for (&idx, page) in self.pages.iter() {
                        if idx < half_pages(self.bits) {
                            for n in page.row_names() {
                                visit(n)?;
                            }
                        }
                    }
                    Ok(())
                }
            }
            let stored = TestStored { bits, pages: &opened };

            // Edit: add `added_body` to `ws_file`, reading ALL 12 cells of `producer`!
            let mut next_members = Vec::new_in(alloc);
            next_members.push(Planned { name: existing_body, phase: phase_bodies, file: ws_file, source: body_src });
            next_members.push(Planned { name: added_body, phase: phase_bodies, file: ws_file, source: body_src });
            next_members.sort_unstable_by_key(|p| p.name);
            let edit = crate::Edit { files: &[(ws_file, next_members.as_slice())] };
            let mut eng = crate::Engine::new_in(tools, Answer::from_pages(&stored, Gen(1), alloc), &edit, crate::Everything, crate::Plant::None, alloc).expect("engine");
            assert_eq!(eng.next_phase().expect("phase"), Some(phase_bodies));
            assert_eq!(eng.tasks().len(), 1);
            let task = eng.tasks()[0];
            assert_eq!(task.planned.name, added_body);
            let mut reads = Vec::new_in(alloc);
            for &(cid, dig) in producer_cells.iter() {
                reads.push((cid, dig));
            }
            let mut batch = crate::Buffer::new_in(alloc);
            batch.done.push(crate::Done {
                name: added_body, key: task.key, out: body_rec.0,
                defines: Vec::new_in(alloc), reads, findings: 0, failed: false,
            });
            eng.merge(core::slice::from_mut(&mut batch)).expect("merge");
            assert_eq!(eng.next_phase().expect("settled"), None);
            let crate::Finished { answer: edit_answer, causes: edit_causes, .. } = eng.finish().expect("finish");
            let mut payloads = BTreeMap::new_in(alloc);
            payloads.insert(added_body, body_rec);
            let edit_kept = Kept::after(BTreeMap::new_in(alloc), edit_answer, &edit_causes, payloads);

            let enc_v2 = encode_changes(&edit_kept, &root_v1, alloc, |k| store.get(&k).map(|b| Sealed::hashed(k, b)))
                .expect("encode_changes");
            let deltas = enc_v2.page_deltas.as_ref().expect("incremental deltas");
            // Producer's lower-half content page MUST be completely untouched!
            assert!(
                deltas.iter().all(|d| d.index != producer_page),
                "adding 12 reader edges to producer's cells must NOT mutate producer's content page {producer_page} (deltas: {deltas:?})"
            );
            // Every touched readers page must lie in the upper half and have 0 page_records!
            let mut rec_map: BTreeMap<Key, usize, _> = BTreeMap::new_in(alloc);
            for (k, recs) in enc_v2.page_records() {
                rec_map.insert(k, recs.len());
            }
            for &(cid, _) in producer_cells.iter() {
                let rp = readers_page_of(&cid, bits);
                assert!(rp >= half, "readers page {rp} must be in upper half >= {half}");
                let delta = deltas.iter().find(|d| d.index == rp).expect("readers page must be updated");
                assert_eq!(rec_map.get(&delta.new).copied().unwrap_or(0), 0, "readers page {rp} must have 0 RECORDS");
            }

            // Release borrows of `store`, store v2 puts, and verify producer's page key is byte-identical
            // and the full answer decodes and re-encodes identically.
            drop(edit_kept);
            drop(edit_causes);
            drop(stored);
            drop(opened);
            for (k, b) in enc_v2.puts.iter() {
                store.insert(*k, SVec::from_slice_in(b, alloc));
            }
            let root_v2 = decode_root_with(
                Sealed::hashed(enc_v2.root, &store[&enc_v2.root]).expect("root v2"),
                |k| store.get(&k).map(|b| Sealed::hashed(k, b)),
                alloc,
            )
            .expect("decode root v2");
            assert_eq!(
                page_key_in(&root_v2, producer_page, &store),
                producer_page_key_v1,
                "producer's content page key must stay byte-identical at bits={bits}"
            );
            // Verify every cell's readers list includes `added_body`.
            for &(cid, _) in producer_cells.iter() {
                let rp = readers_page_of(&cid, bits);
                let rk = page_key_in(&root_v2, rp, &store);
                let vp = VerifiedPage::open_whole(Sealed::hashed(rk, &store[&rk]).expect("rp"), rp, bits).expect("open_whole rp");
                assert!(vp.readers(&cid).any(|r| r == added_body), "added_body must be in readers of {cid:?}");
            }
            let decoded = decode(enc_v2.root, |k| store.get(&k).map(|b| Sealed::hashed(k, b)), alloc).expect("decode v2");
            assert_eq!(encode_at_bits(&decoded, bits, alloc).root, enc_v2.root, "round-trip v2 at bits={bits}");
        }
    }
}
