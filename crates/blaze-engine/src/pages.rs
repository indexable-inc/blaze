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
//! root = "BER4" | serial u64 | bits u8 | bytes u64 | flags u8 | live_pages u32 | outlines u32
//!        if flags == 0 (flat, bits <= RADIX_FLAT_MAX_BITS):
//!          | pages u32 (= 1 << bits) | pages x key [32]
//!          | files | places | units | findings | owed tasks | owed cells
//!        if flags == 1 (2-level radix, bits > RADIX_FLAT_MAX_BITS):
//!          | shards u32 (= 1 << (bits - RADIX_LEAF_BITS)) | shards x key [32]
//!          | 16 x file_shard_key [32] | places_key [32] | summary_key [32]
//!          | owed tasks | owed cells
//! radix shard  = "BEX1" | bits u8 | shard u32 | 256 x key [32]  (all-zero shard = zero key)
//! file shard   = "BEF1" | shard u8 | count u32 | count x (file | has u8 | [digest [32]])
//! places rec   = "BEPL" | count u32 | count x (file | unit | edition u8 | flags u8)
//! summary rec  = "BES1" | units u32 | units x Name | findings u32 | findings x (Name | u32)
//! file places = count u32 | count x (file | unit | edition u8 | flags u8), by file, strictly
//!        ([`FilePlace`]: one a source file the driver planned, each file of the plan digests)
//! page = "BEP5" | bits u8 | index u32 | 8 x count u32 | (n + 1) x start u32 | the eight
//!        sections, where n is the counts' sum, start j is entry j's first byte in the page
//!        (entries numbered across the sections in order) and start n is the page's length;
//!        the sections, in this order:
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

use crate::engine::{Answer, Cause, CellId, Entered, Gen, Phase, Planned, Row, SMap, SVec, Stage};

mod view;
mod changes;
mod headers;
pub use headers::{Header, HeaderPages, HeaderShardBuilt, HeaderShardJob, Headers, HeadersPlant, IndexedHeaders, Writer};
pub use changes::{PageBuilt, PageJob, encode_changes, encode_changes_radix, plan_digest};
pub use view::{StoredDefines, StoredNames, StoredReads, StoredRow, VerifiedPage};

/// A page's form. A page of another form is never read: it is a [`PagesError::Magic`]. `BEP3`
/// had no start table: each open walked every entry to find them. `BEP4` kept a cell's readers
/// on its task's page, so every cell of a task many read (a std crate's names) shared one page
/// of every reader of them all, and a body that gained or lost one read of one such cell put
/// that whole page again (tempo, a refused body's read list moved: 7 pages, 68 MB hashed, the
/// commit 108 ms against the phase path's 15). `BEP5` places it by [`readers_slot`].
pub const PAGE_MAGIC: &[u8; 4] = b"BEP5";

/// The name whose page ([`page_of`]) holds `id`'s readers entry: a placement key, never a task's
/// name. Its subject is the task's with its first eight bytes replaced by a mix of the task's
/// first word, its kind and the cell's two words (splitmix64's finalizer over a fold, part of the
/// form: two builders place one cell on one page, rule 25), so the cells of one task are spread
/// over the pages like the tasks are, and a change to one cell's readers rewrites the page of
/// that one list and not of every list its task defines.
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
/// The root's form. `BER3` stored a flat array of up to 65,536 page keys (2 MiB) plus all
/// directory sections inline; `BER4` keeps small answers (`bits <= RADIX_FLAT_MAX_BITS`) inline
/// and stores large answers (`bits > RADIX_FLAT_MAX_BITS`) as a 2-level 256-way radix tree with
/// sharded file directory and immutable `places`/`summary` sub-records. A `BER3` root is a
/// [`PagesError::Magic`], reported, and the answer made again (D160), never read as this one.
pub const ROOT_MAGIC: &[u8; 4] = b"BER4";
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
const HEAD: usize = 4 + 1 + 4 + 4 * SECTIONS;
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

fn put_row(out: &mut SVec<'_, u8>, name: Name, row: &Row<'_>) {
    put_name(out, name);
    put_id(out, &row.key.0);
    put_phase(out, row.phase);
    put_name(out, row.file);
    put_id(out, &row.source);
    put_id(out, &row.out);
    out.extend_from_slice(&row.serial.0.to_le_bytes());
    out.extend_from_slice(&row.findings.to_le_bytes());
    // Preserve observation order. Canonically sorted reads are their own index.
    let indexed = row.by_cell.iter().enumerate().any(|(i, &at)| i != at as usize);
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

/// A page being written: each section's bytes, and each entry's start in its section's bytes
/// (an entry's count is its starts'). [`encode`] fills the sections in any order of pages;
/// [`changes`]' merge fills one page's sections in order. Both finish by [`PageBuf::finish`].
struct PageBuf<'a> {
    parts: [SVec<'a, u8>; SECTIONS],
    starts: [SVec<'a, usize>; SECTIONS],
}

impl<'a> PageBuf<'a> {
    fn new_in(alloc: SharedAlloc<'a>) -> PageBuf<'a> {
        PageBuf { parts: core::array::from_fn(|_| Vec::new_in(alloc)), starts: core::array::from_fn(|_| Vec::new_in(alloc)) }
    }

    /// The bytes of a new entry of `section`, appended by the caller.
    fn entry(&mut self, section: usize) -> &mut SVec<'a, u8> {
        self.starts[section].push(self.parts[section].len());
        &mut self.parts[section]
    }

    fn entries(&self) -> usize {
        self.starts.iter().map(|starts| starts.len()).sum()
    }

    /// The page in its form: its head, its start table, its sections. `None` when it holds
    /// nothing (a root names no page there).
    fn finish(&self, bits: u8, index: u32, alloc: SharedAlloc<'a>) -> Option<SVec<'a, u8>> {
        let entries = self.entries();
        if entries == 0 {
            return None;
        }
        let body_at = starts_end(entries);
        let len = body_at + self.parts.iter().map(|part| part.len()).sum::<usize>();
        let mut page: SVec<'a, u8> = Vec::with_capacity_in(len, alloc);
        page.extend_from_slice(PAGE_MAGIC);
        page.push(bits);
        page.extend_from_slice(&index.to_le_bytes());
        for starts in self.starts.iter() {
            put_u32(&mut page, starts.len());
        }
        let mut at = body_at;
        for (part, starts) in self.parts.iter().zip(self.starts.iter()) {
            for start in starts.iter() {
                put_u32(&mut page, at + start);
            }
            at += part.len();
        }
        put_u32(&mut page, len);
        for part in self.parts.iter() {
            page.extend_from_slice(part);
        }
        debug_assert_eq!(page.len(), len, "a page's start table ends at its length");
        Some(page)
    }
}

/// Where a page of `entries` entries in all has its first section: after its head and its
/// start table of `entries + 1` words.
fn starts_end(entries: usize) -> usize {
    HEAD + 4 * (entries + 1)
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

/// `kept` as pages of about [`PAGE_BYTES`](blaze_drive::answers::PAGE_BYTES), paged as the last
/// root was while its bits still fit (answers.rs `bits_kept`); `last` is that root, whose pages
/// are not put again.
pub fn encode<'a>(kept: &Kept<'_>, last: Option<&Root<'_>>, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    encode_planted(kept, last, PagesPlant::None, alloc)
}

/// `kept` encoded at an explicit `bits` count (clamped to `MOST_BITS`), primarily for testing
/// large-paging radix trees (`bits = 10..=16`) without allocating gigabytes of synthetic rows.
pub fn encode_at_bits<'a>(kept: &Kept<'_>, bits: u8, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    encode_inner(kept, None, Some(bits.min(blaze_drive::answers::MOST_BITS)), PagesPlant::None, alloc)
}

/// [`encode`] with a planted bug ([`PagesPlant`]).
pub fn encode_planted<'a>(kept: &Kept<'_>, last: Option<&Root<'_>>, plant: PagesPlant, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    encode_inner(kept, last, None, plant, alloc)
}

fn encode_inner<'a>(kept: &Kept<'_>, last: Option<&Root<'_>>, forced_bits: Option<u8>, plant: PagesPlant, alloc: SharedAlloc<'a>) -> Encoded<'a> {
    let answer = &kept.answer;
    let files_kept = plant != PagesPlant::FilesDropped;
    let has_row = |name: &Name| answer.rows.contains_key(name);
    let bits = match forced_bits {
        Some(bits) => bits,
        None => {
            // The bytes in all, which size the paging.
            let mut total = 0u64;
            for row in answer.rows.values() {
                total += (ROW_FIXED + DEFINE * row.defines.len() + READ * row.reads.len()) as u64;
            }
            total += (CELL_ENTRY * answer.cells.len() + OWED_CELL * answer.owed_cells.len() + OWED_TASK * answer.owed_tasks.len()) as u64;
            for names in answer.by_file.values().filter(|_| files_kept) {
                total += (NAME + 4 + FILE_MEMBER * names.len()) as u64;
            }
            for names in answer.readers.values() {
                total += (CELL_ID + 4 + NAME * names.len()) as u64;
            }
            total += (CAUSE * kept.causes.keys().filter(|name| has_row(*name)).count()) as u64;
            total += ((NAME + ID) * kept.records.keys().filter(|name| has_row(*name)).count()) as u64;
            bits_kept(total, last.map(|root| root.bits))
        }
    };
    let count = 1usize << bits;
    let mut bufs: SVec<'a, PageBuf<'a>> = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        bufs.push(PageBuf::new_in(alloc));
    }
    fn at<'b, 'a>(bufs: &'b mut [PageBuf<'a>], bits: u8, name: Name, section: usize) -> &'b mut SVec<'a, u8> {
        bufs[page_of(name, bits) as usize].entry(section)
    }
    for (name, row) in answer.rows.iter() {
        let out = at(&mut bufs, bits, *name, ROWS);
        put_row(out, *name, row);
    }
    for (id, (digest, moved)) in answer.cells.iter() {
        let out = at(&mut bufs, bits, id.of, CELLS);
        put_cell_id(out, id);
        put_id(out, digest);
        out.extend_from_slice(&moved.0.to_le_bytes());
    }
    for (id, moved) in answer.owed_cells.iter() {
        let out = at(&mut bufs, bits, id.of, OWED_CELLS);
        put_cell_id(out, id);
        out.extend_from_slice(&moved.0.to_le_bytes());
    }
    for (name, planned) in answer.owed_tasks.iter() {
        let out = at(&mut bufs, bits, *name, OWED_TASKS);
        put_name(out, *name);
        put_phase(out, planned.phase);
        put_id(out, &planned.source);
        put_name(out, planned.file);
    }
    let mut directory = changes::directory(kept, alloc);
    for (file, names) in answer.by_file.iter().filter(|_| files_kept) {
        let out = at(&mut bufs, bits, *file, FILES);
        let start = out.len();
        put_name(out, *file);
        put_u32(out, names.len());
        let mut standing = true;
        for name in names.iter() {
            let claim = answer.rows.get(name).and_then(|row| changes::claim(row, *file, answer.owed_tasks.contains_key(name)));
            standing &= claim.is_some();
            changes::put_member(out, *name, claim);
        }
        directory.files.insert(*file, standing.then(|| changes::file_digest(&out[start..])));
    }
    // Each planned file's place beside its digest; a place of no file the root plans is not kept.
    directory.places.extend(kept.places.iter().filter(|(file, _)| directory.files.contains_key(*file)).map(|(&file, &place)| (file, place)));
    for (name, (serial, cause)) in kept.causes.iter() {
        if !has_row(name) {
            continue;
        }
        let out = at(&mut bufs, bits, *name, CAUSES);
        put_cause(out, *name, *serial, *cause);
    }
    for (name, key) in kept.records.iter() {
        if !has_row(name) {
            continue;
        }
        let out = at(&mut bufs, bits, *name, RECORDS);
        put_name(out, *name);
        put_id(out, &key.0);
    }
    for (id, names) in &answer.readers {
        let out = at(&mut bufs, bits, readers_slot(id), READERS);
        put_cell_id(out, id);
        put_u32(out, names.len());
        for &name in names { put_name(out, name); }
    }
    // Each page whole, keyed by its bytes; put when the last root did not name it.
    let same_paging = last.filter(|root| root.bits == bits);
    let mut pages: SVec<'a, Key> = Vec::with_capacity_in(count, alloc);
    let mut puts: SVec<'a, (Key, SVec<'a, u8>)> = Vec::new_in(alloc);
    let mut records = Vec::new_in(alloc);
    let mut bytes = 0u64;
    let mut live_pages = 0u32;
    if bits > RADIX_FLAT_MAX_BITS && same_paging.is_some() {
        // Radix whole encode against a same-bits root: build one 256-page shard at a time and
        // skip emitting page puts/records for any shard whose combined shard key matches `last`.
        let mut shard_keys: [Key; RADIX_LEAF_PAGES] = [Key::default(); RADIX_LEAF_PAGES];
        let mut shard_built: SVec<'a, (Key, SVec<'a, u8>, SVec<'a, u8>)> = Vec::with_capacity_in(RADIX_LEAF_PAGES, alloc);
        let mut bufs_iter = bufs.into_iter();
        let shard_count = 1usize << (bits - RADIX_LEAF_BITS);
        for shard_idx in 0..shard_count {
            shard_keys.fill(Key::default());
            shard_built.clear();
            for offset in 0..RADIX_LEAF_PAGES {
                let index = shard_idx * RADIX_LEAF_PAGES + offset;
                let index32 = u32::try_from(index).unwrap_or(u32::MAX);
                let Some(mut buf) = bufs_iter.next() else { break };
                let Some(page) = buf.finish(bits, index32, alloc) else {
                    pages.push(Key::default());
                    continue;
                };
                bytes += u64::try_from(page.len()).unwrap_or(u64::MAX);
                live_pages = live_pages.saturating_add(1);
                let key = content_key(&page);
                pages.push(key);
                if let Some(slot) = shard_keys.get_mut(offset) {
                    *slot = key;
                }
                let rec_part = match buf.parts.get_mut(RECORDS) {
                    Some(part) => core::mem::replace(part, Vec::new_in(alloc)),
                    None => Vec::new_in(alloc),
                };
                shard_built.push((key, page, rec_part));
            }
            let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
            let unchanged = changes::encode_radix_shard(bits, shard_u32, &shard_keys, alloc)
                .is_some_and(|(shard_key, _)| same_paging.is_some_and(|r| r.page_shards.get(shard_idx) == Some(&shard_key)));
            if !unchanged {
                for (key, page, rec_part) in shard_built.drain(..) {
                    records.push((key, view::page_records(&rec_part, alloc)));
                    puts.push((key, page));
                }
            }
        }
    } else {
        for (index, buf) in bufs.into_iter().enumerate() {
            let index32 = u32::try_from(index).unwrap_or(u32::MAX);
            let Some(page) = buf.finish(bits, index32, alloc) else {
                pages.push(Key::default());
                continue;
            };
            bytes += u64::try_from(page.len()).unwrap_or(u64::MAX);
            live_pages = live_pages.saturating_add(1);
            let key = content_key(&page);
            pages.push(key);
            if same_paging.is_none_or(|root| root.pages.get(index) != Some(&key)) {
                let rec_bytes = buf.parts.get(RECORDS).map_or(&[][..], Vec::as_slice);
                records.push((key, view::page_records(rec_bytes, alloc)));
                puts.push((key, page));
            }
        }
    }
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
    if bits > blaze_drive::answers::MOST_BITS {
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
    if !root.is_radix() {
        for (index, &key) in root.pages.iter().enumerate() {
            if key == Key::default() { continue; }
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let sealed = get(key).ok_or(PagesError::Missing { page: index, key })?.map_err(|torn| torn.at(index))?;
            let page = VerifiedPage::open_whole(sealed, index, root.bits)?;
            page.restore(&mut kept, alloc);
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
                page.restore(&mut kept, alloc);
            }
        }
    }
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
}
