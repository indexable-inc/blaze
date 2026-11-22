//! `engine-check`'s stored answer (engine.md E6, the incremental half): the engine's answer and
//! causes as pages (blaze-engine pages.rs), the leaf table of what the request read (sweep.rs
//! `Leaves`), and a sealed pointer naming both (seal.rs), under one root of the store per
//! checkout ([`root_name`]).
//!
//! THE TOOLS are not in the root's name: the answer is one per checkout whichever blaze wrote it.
//! Every row's key holds its own kind's producer (blaze-engine `Tools::key`), and each file's
//! plan digest holds every kind's (pages.rs `plan_digest`), so a request under other tools reads
//! the same answer: a file whose plan digest moved is in its edit, a task whose key moved is
//! entered, and the rest stand by their keys and reads (a moved names solver enters the names,
//! and a body whose scopes came out the same is not entered). Only the certificate, which reads
//! no row, needs the tools: the pointer keeps their digest ([`tools_digest`]), and it stands on
//! equal tools alone. Before (BEL6), the root hashed the tools, so every new blaze build (every
//! producer that moved) started from no answer and entered every task: a whole check of tempo
//! per new binary.
//!
//! Each changed page gets a CAS reach index containing its key, row payloads, and the
//! expansion records its names rows own. The checkout pins the sealed pointer alone,
//! and `<name>.pages` pins the root, printed lines and all live page indexes. The store's
//! generation flip publishes both roots together (pages_sim.rs).
//!
//! The read, before any work: the root name's one key is the sealed pointer; the sweep
//! follows its leaf table; nothing moved, nothing owed and no findings kept is the certificate,
//! from the pointer and the printed lines' record (engine_lines.rs, one get), no page read.
//! Else [`open_root`] reads the directory. [`Stored`] authenticates a page on its first
//! lookup. All consumers share that page's proof for the rest of the request. A refused
//! page stops the edit. A pointer of an older form is reported and rebuilt (rule 26).
//!
//! ```text
//! the root name   engine.<16 hex of H(checkout)>
//! it names        the sealed pointer's key; `<it>.pages` reaches the root, lines and page
//!                 indexes, with their payloads
//! sealed pointer  seal.rs form: last = "BEL7" | answer root key [32] | lines key [32] |
//!                 files u32 | printed u8 (bit 0 names checked, 1 bodies whole, 2 every
//!                 printed line is in the lines record) | findings u32 | owed u32 (the
//!                 certificate's facts) | tools digest [32] | headers u8 (0 current, 1
//!                 absent) | since u64 | 128 x producer header shard key [32] (all zero when
//!                 absent) | the journal's places; leaves = the BCL table
//! ```
//!
//! BEL6 had no tools digest: its root's name held the tools. A BEL6 pointer is an old home's
//! answer, reported and rebuilt ([`Unreadable::Older`]).
//!
//! BEL4 named pages without stored reverse-reader runs or page-local payload reach indexes.
//! BEL5 named 128 shard keys and no state: it could not say that the `Ready` driver's commit
//! (D187: pages alone, no shard read or written) left the shards behind the pages, so a BEL5
//! pointer is an old home's answer, reported and rebuilt ([`Unreadable::Older`]), never read
//! as BEL6 (the form is the pointer's place, D160: the magic is checked before any byte).

use core::fmt;
use core::fmt::Write as _;
use std::sync::OnceLock;

use blaze_cas::plain::Snapshot;
use blaze_core::Driver;
use blaze_drive::{Key, Name};
use blaze_engine::{StoredPages, Tools};
use blaze_engine::pages::{self, Header, HeaderPages, Headers, HeadersPlant, IndexedHeaders, Kept, PagesError, Root, Sealed, VerifiedPage, Writer};

/// The store's proof that a held record's bytes are its key's (rule 74): what the pages take
/// in place of hashing each page at its open (blaze-engine pages.rs, "Checked once").
impl pages::StoreProof for crate::store::plain_place::Checked<'_> {}
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::Cx;
use bstd::mem::Frame;

use crate::store::{CommandStore, Sharing, Store as _};
use crate::sweep::{Leaves, Rows};
use crate::{CliError, seal};

/// The pointer's form inside its seal.
pub(crate) const POINTER_MAGIC: &[u8; 4] = b"BEL7";
/// The magic, the answer's root, the lines record's key, then the [`Facts`]: files u32, printed
/// u8, findings u32, owed u32; then the tools' digest ([`tools_digest`]). The header index's
/// state ([`Headers`]) follows: its tag, its `since`, then every shard's key; then the
/// journal's places, of their own length.
const TOOLS_AT: usize = 4 + 32 + 32 + 4 + 1 + 4 + 4;
const HEADERS_AT: usize = TOOLS_AT + 32;
const SHARDS_AT: usize = HEADERS_AT + 1 + 8;
const POINTER_BYTES: usize = SHARDS_AT + 32 * HeaderPages::COUNT;
const HEADERS_CURRENT: u8 = 0;
const HEADERS_ABSENT: u8 = 1;
const POINTER_DOMAIN: &str = "blaze-cli engine-check pointer";

/// The root the checkout's stored answer hangs from, whichever tools wrote it (the module's doc,
/// "THE TOOLS"; a machine's path is hashed here alone: a root is local, never shared).
pub(crate) fn root_name<'f>(checkout: &str, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let id = ContentId::of("blaze-cli engine-check root 2", checkout.as_bytes());
    let mut name = blaze_own::String::new_in(frame);
    name.push_str("engine.");
    // A home made before the root stopped hashing the tools holds its answers under names no
    // request asks for again: dead roots, whose records the store's cleanup takes when their
    // pins expire, `ROOT_LIFE_NS` after their last renewal (store/plain_place.rs `pin`).
    // Nothing reads or migrates them (AGENTS.md: an old home's
    // answer is rebuilt, never migrated).
    for byte in &id.0[..8] {
        // A frame string's write fails only when the frame is full: the name is then short and
        // names no root, so the request answers in full.
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// The control of main.rs tests `a_moved_body_checker_enters_its_bodies_alone` (rule 23):
/// [`RootPlant::PerTools`] reads no stored answer written under other tools, as the root did
/// when its name hashed the tools (BEL6).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RootPlant {
    None,
    PerTools,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static ROOT_PLANT: core::cell::Cell<RootPlant> = const { core::cell::Cell::new(RootPlant::None) };
}

/// The root naming the home's latest sealed pointer, whichever checkout saved it ([`save`]). A
/// checkout with no answer of its own (a second worktree, a new clone) starts from it: every row
/// is valid by its key and reads against today's facts, and the pointer's leaves are
/// checkout-relative, so the sweep follows them here, every row looked at (its journal places
/// are the other checkout's and are not read), and the certificate never stands on it
/// (engine_check.rs, `StoredBase::OtherCheckout`). Its pages stay reachable while the checkout
/// that wrote it keeps them under its own `<root>.pages`; a page that is gone makes the answer
/// unreadable, reported, and the request answers in full (rule 3: a stated path).
pub(crate) const LATEST_ROOT: &str = "engine.latest";

/// The digest of every kind's tool, in kind order: what the pointer keeps so the certificate
/// stands only under the tools that wrote the answer (the module's doc, "THE TOOLS").
pub(crate) fn tools_digest(tools: &Tools) -> ContentId {
    let mut h = blaze_ids::Hasher::new("blaze-cli engine-check pointer tools 1");
    for tool in tools.0.iter() {
        h.update(&tool.0.0);
    }
    h.finish()
}

/// Why the stored answer is not read: the request answers in full and writes it anew.
#[derive(Debug)]
pub(crate) enum Unreadable {
    /// The root names other than one key (its pointer's).
    Names { bytes: usize },
    /// A record the root names is not in the store.
    Missing { key: Key },
    /// The pointer is of an older form (`BEL5`, `BEL4`, or older still): an old home's answer.
    Older,
    /// The printed lines' record (engine_lines.rs).
    Lines(crate::engine_lines::LinesError),
    /// The pointer's write was cut short.
    Torn(seal::Torn),
    /// A pointer of this form whose header index field no writer makes: a tag of no state, or an
    /// absent index naming a shard key.
    Headers { tag: u8 },
    /// A page or the answer's root (pages.rs).
    Pages(PagesError),
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Unreadable::Older = self {
            return write!(f, "the stored engine answer is an old home's answer (its pointer is of an older form than `BEL7`), rebuilt (rule 26)");
        }
        write!(f, "the stored engine answer is not read (engine_answer.rs `load`): ")?;
        match self {
            Unreadable::Names { bytes } => write!(f, "its root names {bytes} bytes, not its pointer's one key")?,
            Unreadable::Missing { key } => write!(f, "{key:?}, which its root names, is not in the store")?,
            Unreadable::Older => {}
            Unreadable::Lines(why) => write!(f, "{why}")?,
            Unreadable::Torn(torn) => write!(f, "{torn}")?,
            Unreadable::Headers { tag } => write!(f, "its producer header index field (tag {tag}) is no state a writer makes (engine_answer.rs `load_pointer`)")?,
            Unreadable::Pages(why) => write!(f, "{why}")?,
        }
        write!(f, "; answering in full and writing it anew")
    }
}

/// What the certificate prints, kept in the pointer so a request with nothing changed reads no
/// page: the files outlined, whether the crates' names were checked, the rows with findings,
/// and the cells and tasks owed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Facts {
    pub(crate) files: u32,
    pub(crate) printed: Printed,
    pub(crate) findings: u32,
    pub(crate) owed: u32,
}

/// What the last request printed that the answer's rows do not say, from the driver: which
/// stages it checked whole, and whether every line it printed beyond the closing one is in the
/// lines record (`replayable`: no note of a count the rows do not keep). The certificate stands
/// only after a replayable request: it prints the lines record and its coverage.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Printed {
    pub(crate) names: bool,
    /// Every body's TYPES were checked to the end (engine_lines.rs `not_checked_whole`). Whether
    /// their borrows were too is not kept here: it is a count of the lines record itself (its
    /// `Class::Borrows` notes, `Tally::borrows`), which the certificate prints whole, so the
    /// two can never disagree.
    pub(crate) bodies_whole: bool,
    pub(crate) replayable: bool,
}

impl Printed {
    fn byte(self) -> u8 {
        u8::from(self.names) | u8::from(self.bodies_whole) << 1 | u8::from(self.replayable) << 2
    }

    fn of_byte(byte: u8) -> Printed {
        Printed { names: byte & 1 != 0, bodies_whole: byte & 2 != 0, replayable: byte & 4 != 0 }
    }
}

/// The pointer as read: the answer's root, the certificate's facts, and the leaf table of what
/// the last request read.
pub(crate) struct Pointer<'m> {
    /// The sealed pointer's own key, as the root named it when read: what a journal
    /// checkpoint extends (journal_checkpoint.rs).
    pub(crate) key: Key,
    pub(crate) root: Key,
    /// The producer header index: current with the pages, or absent since a ready commit.
    pub(crate) headers: Headers,
    /// The printed lines' record (engine_lines.rs).
    pub(crate) lines: Key,
    pub(crate) facts: Facts,
    /// The digest of the tools that wrote the answer ([`tools_digest`]): the certificate stands
    /// only when today's are equal ([`Pointer::certifies_under`]).
    pub(crate) tools: ContentId,
    pub(crate) leaves: Leaves<'m>,
    /// Where the journal of changes stood when the request that wrote it began its reads.
    pub(crate) journal: crate::journal_sweep::Stored<'m>,
}

impl Pointer<'_> {
    /// THE ONE PLACE the tools decide anything of a stored answer (the module's doc, "THE
    /// TOOLS"): whether the certificate, which reads no row, may stand under `tools`. Equal
    /// digests: yes. Else the request takes the phase path over this same answer, where each
    /// row stands or is entered by its own key (its kind's producer) and reads.
    pub(crate) fn certifies_under(&self, tools: &Tools) -> bool {
        self.tools == tools_digest(tools)
    }
}

fn key_at(bytes: &[u8], at: usize) -> Key {
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[at..at + 32]);
    Key(ContentId(id))
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Only the directory the pointer names. The certificate does not need even this read.
pub(crate) fn open_root<'a, D: Driver, S: Sharing>(
    cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, key: Key, alloc: SharedAlloc<'a>,
) -> Result<Result<Root<'a>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    Ok(pages::decode_root(Sealed::by_store(key, held.bytes, held.proof), alloc).map_err(Unreadable::Pages))
}

pub(crate) fn page_error(why: PagesError) -> CliError {
    blaze_engine::EngineError::from(blaze_engine::last::LastError::Pages(why)).into()
}

// A small directory of slots, with one block allocated only if a lookup reaches it. The
// snapshot owns the mappings even while workers publish later generations of the store.
const BLOCK: usize = 64;
type PageBox<'a> = blaze_own::Box<VerifiedPage<'a>, SharedAlloc<'a>>;
type PageSlot<'a> = OnceLock<Option<PageBox<'a>>>;
type PageBlock<'a> = blaze_own::Box<[PageSlot<'a>; BLOCK], SharedAlloc<'a>>;
/// The producer header shards' slots, one each, in the request's memory: 128 slots of about a
/// hundred bytes, which held inline made `Stored` some 12 KiB, copied at each move of it through
/// the command's top-level check frame in a debug build (the stack overflow of
/// `a_changed_archive_checksum_enters_the_body_through_the_graph`, wave-1119 t26).
type HeaderSlots<'a> = blaze_own::Box<[OnceLock<Result<IndexedHeaders<'a>, PagesError>>; HeaderPages::COUNT], SharedAlloc<'a>>;

pub(crate) struct Stored<'a> {
    pub(crate) root: &'a Root<'a>,
    snapshot: &'a Snapshot<SharedAlloc<'a>>,
    /// The producer header index the pointer names: its shards read when current, else every
    /// header and planner row read from its row's page (blaze-engine headers.rs `Headers`).
    headers: &'a Headers,
    header_pages: HeaderSlots<'a>,
    alloc: SharedAlloc<'a>,
    blocks: blaze_own::Vec<OnceLock<PageBlock<'a>>, SharedAlloc<'a>>,
    refused: OnceLock<PagesError>,
    /// How a page's bytes are known to be its key's: the store's proof handed through
    /// ([`pages::Check::Store`], the command's path: no byte hashed, a page's head alone checked)
    /// or every byte hashed and every entry walked ([`pages::Check::Hashed`], `--verify`).
    check: pages::Check,
    task_reads: core::sync::atomic::AtomicUsize,
    header_reads: core::sync::atomic::AtomicUsize,
    /// The bytes of the task and header pages read, and of those hashed (zero on the store's
    /// proof): the `pages:` line ([`PagesRead`]).
    task_bytes: core::sync::atomic::AtomicU64,
    header_bytes: core::sync::atomic::AtomicU64,
    hashed_bytes: core::sync::atomic::AtomicU64,
    /// Rows asked by name ([`Stored::asks`]).
    asks: core::sync::atomic::AtomicUsize,
    /// Each name asked, in ask order, the first [`MOST_ASKS_KEPT`]: what [`Stored::asked`]
    /// tells by kind, repeats and distinct names apart (rule 17: which rows a request's reads
    /// of the answer are, before why they scale).
    asked: blaze_core::lock::Lock<blaze_own::Vec<(Name, blaze_engine::Ask), SharedAlloc<'a>>>,
}

/// The names [`Stored::asked`] keeps (rule 18: a table has a cap): a ready edit asks hundreds;
/// asks past it are counted ([`Stored::asks`]) and not told by kind.
const MOST_ASKS_KEPT: usize = 1 << 16;

/// A request's rows asked of the stored answer, by kind ([`Stored::asked`]): each kind's asks
/// (repeats included) and distinct names, and the asks past the cap not told; and by purpose
/// ([`blaze_engine::Ask`]): each purpose's asks and its distinct reads (a name and purpose, a
/// cell ask's cell too), so a repeat is the same read asked again, never another cell's read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AskedByKind {
    pub(crate) asks: [u32; blaze_drive::key::KINDS],
    pub(crate) distinct: [u32; blaze_drive::key::KINDS],
    pub(crate) by_purpose: [u32; blaze_engine::Ask::PURPOSES],
    pub(crate) purpose_distinct: [u32; blaze_engine::Ask::PURPOSES],
    pub(crate) untold: u32,
}

impl Default for AskedByKind {
    fn default() -> AskedByKind {
        AskedByKind {
            asks: [0; blaze_drive::key::KINDS],
            distinct: [0; blaze_drive::key::KINDS],
            by_purpose: [0; blaze_engine::Ask::PURPOSES],
            purpose_distinct: [0; blaze_engine::Ask::PURPOSES],
            untold: 0,
        }
    }
}

impl AskedByKind {
    /// The purposes whose distinct reads differ between `self` and `other`, by word: what a
    /// world test names when a request's reads grow.
    #[cfg(test)]
    pub(crate) fn purposes_moved(&self, other: &AskedByKind) -> std::vec::Vec<&'static str> {
        (0..blaze_engine::Ask::PURPOSES)
            .filter(|&at| self.purpose_distinct[at] != other.purpose_distinct[at])
            .map(|at| blaze_engine::Ask::WORDS[at])
            .collect()
    }
}

/// `Kind asks/distinct` for each kind asked, then the untold asks: the profile's `rows asked:`
/// words and the world test's message.
impl core::fmt::Display for AskedByKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (asks, distinct): (u32, u32) = (self.asks.iter().sum(), self.distinct.iter().sum());
        write!(f, "rows asked {asks} ({distinct} distinct):")?;
        for kind in blaze_drive::Kind::ALL {
            let at = kind.index();
            if self.asks[at] > 0 {
                write!(f, " {kind:?} {}/{}", self.asks[at], self.distinct[at])?;
            }
        }
        write!(f, "; by purpose (asks/distinct reads):")?;
        for at in 0..blaze_engine::Ask::PURPOSES {
            if self.by_purpose[at] > 0 {
                write!(f, " {} {}/{}", blaze_engine::Ask::WORDS[at], self.by_purpose[at], self.purpose_distinct[at])?;
            }
        }
        if self.untold > 0 {
            write!(f, "; {} past the cap not told", self.untold)?;
        }
        Ok(())
    }
}

/// What a request read of its stored answer's pages ([`Stored::pages_read_line`]): the profile's
/// `pages: N read, M bytes, K hashed` line, the one the engine's simulation prints for its cases.
/// Its floor on an edit is the pages the edited rows, their files' plans and the producer header
/// pages they name sit on, and `K` is 0 on the command's path.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct PagesRead {
    pub(crate) task: usize,
    pub(crate) header: usize,
    pub(crate) bytes: u64,
    pub(crate) hashed: u64,
}

impl fmt::Display for PagesRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pages: {} read ({} task, {} producer header), {} bytes, {} hashed", self.task + self.header, self.task, self.header, self.bytes, self.hashed)
    }
}

impl<'a> Stored<'a> {
    /// `check`: [`pages::Check::Store`] on the command's path (each page's bytes with the
    /// store's proof, plain_place.rs `snapshot_get`), [`pages::Check::Hashed`] for an explicit
    /// verify (every byte hashed, every entry walked: `VerifiedPage::open_whole`).
    pub(crate) fn new(root: &'a Root<'a>, snapshot: &'a Snapshot<SharedAlloc<'a>>, headers: &'a Headers, check: pages::Check, alloc: SharedAlloc<'a>) -> Self {
        use core::sync::atomic::{AtomicU64, AtomicUsize};
        let blocks = blaze_own::Vec::from_iter_in((0..root.pages.len().div_ceil(BLOCK)).map(|_| OnceLock::new()), alloc);
        Self {
            root, snapshot, headers, header_pages: blaze_own::Box::new_in(core::array::from_fn(|_| OnceLock::new()), alloc), alloc, blocks, refused: OnceLock::new(), check,
            task_reads: AtomicUsize::new(0),
            header_reads: AtomicUsize::new(0),
            task_bytes: AtomicU64::new(0),
            header_bytes: AtomicU64::new(0),
            hashed_bytes: AtomicU64::new(0),
            asks: AtomicUsize::new(0),
            asked: blaze_core::lock::Lock::new("engine.answer: the rows asked by name", blaze_own::Vec::with_capacity_in(0, alloc)),
        }
    }

    /// One ask of `name`'s page for `ask`: counted, and kept under the cap for [`Stored::asked`].
    fn note_ask(&self, name: Name, ask: blaze_engine::Ask) {
        self.asks.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let mut asked = self.asked.lock();
        if asked.len() < MOST_ASKS_KEPT {
            // A refused growth leaves the name untold, counted by `asks` (rule 3: the count
            // says so, `AskedByKind::untold`).
            let _ = asked.try_push((name, ask));
        }
    }

    /// The rows asked so far by kind (repeats and distinct names apart) and by purpose (asks
    /// and distinct reads apart) ([`AskedByKind`]).
    pub(crate) fn asked(&self) -> AskedByKind {
        let mut by = AskedByKind::default();
        let asked = self.asked.lock();
        let mut sorted: blaze_own::Vec<(Name, blaze_engine::Ask), SharedAlloc<'a>> = blaze_own::Vec::with_capacity_in(asked.len(), self.alloc);
        for &(name, ask) in asked.iter() {
            by.asks[name.kind.index()] += 1;
            by.by_purpose[ask.purpose()] += 1;
            let _ = sorted.try_push((name, ask));
        }
        by.untold = u32::try_from(self.asks().saturating_sub(asked.len())).unwrap_or(u32::MAX);
        drop(asked);
        sorted.sort_unstable();
        let mut last_name = None;
        let mut last_read = None;
        for &(name, ask) in sorted.iter() {
            if last_name != Some(name) {
                by.distinct[name.kind.index()] += 1;
            }
            if last_read != Some((name, ask)) {
                by.purpose_distinct[ask.purpose()] += 1;
            }
            last_name = Some(name);
            last_read = Some((name, ask));
        }
        by
    }

    /// How many times a reader asked this answer for a row by name ([`StoredPages::header`],
    /// [`StoredPages::page`]), repeats included: what a request's reads of the answer scale
    /// with, apart from the page packing (a page holds the rows its names' leading bits pick, so
    /// one set of names asked spans more pages in a larger answer: `pages_read` is not the measure).
    pub(crate) fn asks(&self) -> usize {
        self.asks.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The page holding `name`'s row, asked by name for `ask` (counted in [`Stored::asks`] and
    /// told by purpose in [`Stored::asked`]).
    pub(crate) fn named_page(&self, name: Name, ask: blaze_engine::Ask) -> Result<Option<&VerifiedPage<'a>>, PagesError> {
        self.note_ask(name, ask);
        self.page_at(blaze_drive::answers::page_of(name, self.root.bits) as usize)
    }

    /// Page `page`'s record under `key` from the snapshot, sealed by this request's `check`:
    /// the store's proof handed through, or every byte hashed (and counted as hashed).
    fn sealed(&self, key: Key, page: u32, bytes_read: &core::sync::atomic::AtomicU64) -> Result<pages::Sealed<'a>, PagesError> {
        use core::sync::atomic::Ordering::Relaxed;
        let held = crate::store::plain_place::snapshot_get(self.snapshot, key)
            .map_err(|_| PagesError::PageRecord { page, key })?
            .ok_or(PagesError::Missing { page, key })?;
        bytes_read.fetch_add(held.bytes.len() as u64, Relaxed);
        match self.check {
            pages::Check::Store => Ok(Sealed::by_store(key, held.bytes, held.proof)),
            pages::Check::Hashed => {
                self.hashed_bytes.fetch_add(held.bytes.len() as u64, Relaxed);
                Sealed::hashed(key, held.bytes).map_err(|torn| torn.at(page))
            }
        }
    }

    /// The record under `key` in this answer's snapshot, in place, under the store's proof (the
    /// one check of its segment, rule 74): what a row of the answer names, read for the request.
    pub(crate) fn record_bytes(&self, key: Key) -> Result<Option<&'a [u8]>, blaze_cas::plain::SnapshotError> {
        Ok(crate::store::plain_place::snapshot_get(self.snapshot, key)?.map(|held| held.bytes))
    }

    /// The `pages:` line's counts so far.
    pub(crate) fn pages_read_line(&self) -> PagesRead {
        use core::sync::atomic::Ordering::Relaxed;
        let (task, header) = self.read_counts();
        PagesRead { task, header, bytes: self.task_bytes.load(Relaxed) + self.header_bytes.load(Relaxed), hashed: self.hashed_bytes.load(Relaxed) }
    }

    pub(crate) fn answer(&self) -> blaze_engine::Answer<'_> {
        let mut answer = blaze_engine::Answer::from_pages(self, self.root.serial, self.alloc);
        answer.owed_tasks.extend(self.root.owed_tasks.iter().map(|(&name, &plan)| (name, plan)));
        answer.owed_cells.extend(self.root.owed_cells.iter().map(|(&id, &at)| (id, at)));
        answer.with_findings.extend(self.root.findings.keys().copied());
        answer
    }

    /// Shard `index`, opened once. Asked only of a current index (headers.rs `Headers::header`,
    /// `Headers::planner_row`, `Headers::after` ask a shard of nothing else); asked of an
    /// absent one it is refused by name, never answered from a stale key.
    fn headers_at(&self, index: usize) -> Result<Option<&IndexedHeaders<'a>>, PagesError> {
        let key = match self.headers {
            Headers::Current(keys) => keys.keys[index],
            Headers::Absent { since } => {
                return Err(*self.refused.get_or_init(|| PagesError::HeadersAbsent { page: u32::try_from(index).unwrap_or(u32::MAX), since: *since }));
            }
        };
        if key == Key::default() {
            return Ok(None);
        }
        let proof = self.header_pages[index].get_or_init(|| {
            self.header_reads.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let page = index as u32;
            IndexedHeaders::open(self.sealed(key, page, &self.header_bytes)?, index)
        });
        match proof {
            Ok(page) => Ok(Some(page)),
            Err(error) => Err(*self.refused.get_or_init(|| *error)),
        }
    }

    /// The units planner and script ordering consume these full prior results: from a current
    /// shard, or from the row's page when the index is absent.
    pub(crate) fn planner_row<'r>(&self, name: Name, alloc: SharedAlloc<'r>) -> Result<Option<blaze_engine::Row<'r>>, PagesError> {
        self.headers.planner_row(name, |index| self.headers_at(index), || self.page(name), alloc)
    }

    /// The header index this answer names (the profile's `headers:` word).
    pub(crate) fn headers(&self) -> &'a Headers {
        self.headers
    }

    pub(crate) fn read_counts(&self) -> (usize, usize) {
        use core::sync::atomic::Ordering::Relaxed;
        (self.task_reads.load(Relaxed), self.header_reads.load(Relaxed))
    }

    #[cfg(test)]
    pub(crate) fn pages_read(&self) -> usize {
        let (tasks, headers) = self.read_counts();
        tasks + headers
    }

    pub(crate) fn page_at(&self, index: usize) -> Result<Option<&VerifiedPage<'a>>, PagesError> {
        let page = u32::try_from(index).unwrap_or(u32::MAX);
        let Some(&key) = self.root.pages.get(index) else {
            return Err(*self.refused.get_or_init(|| PagesError::Count { page: pages::ROOT_PAGE, count: page }));
        };
        if key == Key::default() {
            return Ok(None);
        }
        let block = self.blocks[index / BLOCK].get_or_init(|| blaze_own::Box::new_in(core::array::from_fn(|_| OnceLock::new()), self.alloc));
        let proof = block[index % BLOCK].get_or_init(|| {
            self.task_reads.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // On the store's proof the head alone is checked, no byte hashed and no entry walked
            // (pages/view.rs `open`); an explicit verify hashes and walks every entry.
            let opened = self.sealed(key, page, &self.task_bytes).and_then(|sealed| match self.check {
                pages::Check::Store => VerifiedPage::open(sealed, page, self.root.bits),
                pages::Check::Hashed => VerifiedPage::open_whole(sealed, page, self.root.bits),
            });
            match opened {
                Ok(page) => Some(blaze_own::Box::new_in(page, self.alloc)),
                Err(why) => {
                    self.refused.get_or_init(|| why);
                    None
                }
            }
        });
        match proof {
            Some(page) => Ok(Some(page)),
            None => Err(*self.refused.get().expect("a refused page records its error before publishing its slot")),
        }
    }
}

impl StoredPages for Stored<'_> {
    fn header(&self, name: Name) -> Result<Option<Header>, PagesError> {
        self.note_ask(name, blaze_engine::Ask::Header);
        self.headers.header(name, |index| self.headers_at(index), || self.page_at(blaze_drive::answers::page_of(name, self.root.bits) as usize))
    }
    fn page(&self, name: Name) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        self.named_page(name, blaze_engine::Ask::Unsaid)
    }
    fn page_for(&self, name: Name, ask: blaze_engine::Ask) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        self.named_page(name, ask)
    }

    fn refusal(&self) -> Option<PagesError> {
        self.refused.get().copied()
    }

    fn finding(&self, name: Name) -> Option<u32> {
        self.root.findings.get(&name).copied()
    }

    fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), blaze_engine::LastError>) -> Result<(), blaze_engine::LastError> {
        for index in 0..self.root.pages.len() {
            if let Some(page) = self.page_at(index).map_err(blaze_engine::LastError::Pages)? {
                for name in page.row_names() {
                    visit(name)?;
                }
            }
        }
        Ok(())
    }
}

/// Exhaustive inspection for commands and tests which explicitly ask for all rows.
pub(crate) fn load_answer<'m, 'a, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, root_key: Key, alloc: SharedAlloc<'a>) -> Result<Result<(Kept<'a>, Root<'a>), Unreadable>, CliError> {
    // Each record through the store; a store's own error stops the command after the decode.
    let mut failed: Option<CliError> = None;
    let root = match open_root(cx, store, root_key, alloc)? {
        Ok(root) => root,
        Err(why) => return Ok(Err(why)),
    };
    let decoded = pages::decode_at(
        &root,
        |key| match store.get(cx, key) {
            Ok(found) => found.map(|held| Ok(Sealed::by_store(key, held.bytes, held.proof))),
            Err(error) => {
                failed.get_or_insert(error);
                None
            }
        },
        alloc,
    );
    if let Some(error) = failed {
        return Err(error);
    }
    let kept = match decoded {
        Ok(kept) => kept,
        Err(why) => return Ok(Err(Unreadable::Pages(why))),
    };
    Ok(Ok((kept, root)))
}

/// The pointer the root `name` names, read in place: `None` when there is none.
pub(crate) fn load_pointer<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, name: &str) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    let Some(keys) = store.named(name)? else { return Ok(None) };
    if keys.len() != 32 {
        return Ok(Some(Err(Unreadable::Names { bytes: keys.len() })));
    }
    let first = key_at(keys, 0);
    let Some(held) = store.get(cx, first)? else { return Ok(Some(Err(Unreadable::Missing { key: first }))) };
    let sealed: &'m [u8] = held.bytes;
    if !sealed.starts_with(seal::SEAL_MAGIC) {
        return Ok(Some(Err(Unreadable::Older)));
    }
    let (pointer, leaves) = match seal::open(sealed, seal::Plant::None) {
        Ok(opened) => opened,
        Err(torn) => return Ok(Some(Err(Unreadable::Torn(torn)))),
    };
    if pointer.len() < POINTER_BYTES || !pointer.starts_with(POINTER_MAGIC) {
        return Ok(Some(Err(Unreadable::Older)));
    }
    let Some(journal) = crate::journal_sweep::Stored::of(&pointer[POINTER_BYTES..]) else {
        return Ok(Some(Err(Unreadable::Older)));
    };
    let (leaves, older) = Leaves::of(leaves)?;
    if older.is_some() {
        return Ok(Some(Err(Unreadable::Older)));
    }
    let facts = Facts { files: word(pointer, 68), printed: Printed::of_byte(pointer[72]), findings: word(pointer, 73), owed: word(pointer, 77) };
    if pointer[72] > 7 {
        return Ok(Some(Err(Unreadable::Older)));
    }
    let keys = HeaderPages { keys: core::array::from_fn(|i| key_at(pointer, SHARDS_AT + i * 32)) };
    let since = u64::from_le_bytes(pointer[HEADERS_AT + 1..SHARDS_AT].try_into().expect("eight bytes"));
    let headers = match pointer[HEADERS_AT] {
        HEADERS_CURRENT if since == 0 => Headers::Current(keys),
        HEADERS_ABSENT if keys.keys.iter().all(|key| *key == Key::default()) => Headers::Absent { since: blaze_engine::Gen(since) },
        tag => return Ok(Some(Err(Unreadable::Headers { tag }))),
    };
    let tools = key_at(pointer, TOOLS_AT).0;
    Ok(Some(Ok(Pointer { key: first, root: key_at(pointer, 4), headers, lines: key_at(pointer, 36), facts, tools, leaves, journal })))
}

/// The printed lines' record the pointer names, every entry checked (engine_lines.rs `decode`).
pub(crate) fn load_lines<'m, 'a, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    key: Key,
    alloc: SharedAlloc<'a>,
) -> Result<Result<crate::engine_lines::Last<'m, 'a>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    let bytes: &'m [u8] = held.bytes;
    Ok(crate::engine_lines::decode(bytes, alloc).map_err(Unreadable::Lines))
}

/// What one write put.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Saved {
    /// The new pages and their reach indexes, the root, the lines and the sealed pointer.
    pub(crate) puts: u32,
    /// Task pages in the new root and their total bytes.
    pub(crate) pages: u32,
    pub(crate) bytes: u64,
    /// Task pages this write put.
    pub(crate) new_pages: u32,
    /// The bytes of the task pages this write put, and of the largest: a commit costs these
    /// (each page is merged whole, keyed by its bytes, then put), so the profile names a page
    /// that is large for what moved on it (a hot cell's reader list, pages.rs `READERS`).
    pub(crate) new_page_bytes: u64,
    pub(crate) largest_new_page: u64,
    pub(crate) new_headers: u32,
    /// The new root's key (pages.rs `Root`): a function of every page's key, the owed sets and
    /// the findings, so two drivers' answers of one checkout are equal when their roots are.
    pub(crate) root: Key,
    /// The digest of the root's page keys in order, the zero key of an empty slot too
    /// ([`PAGES_DOMAIN`]): the world test names which side moved when two roots differ. Made in
    /// a test build alone: 2 MB hashed a commit on tempo's 65,536 pages (edit12 profile,
    /// c0340wave89), and the root's key already covers every page's.
    pub(crate) pages_digest: Option<ContentId>,
    /// The pages this commit keyed into the reach root, and why it was made whole if it was
    /// ([`page_indexes`]).
    pub(crate) rekeyed: Rekeyed,
    /// Of a commit folded into the last answer: the entries its delta wrote and the pages they
    /// fell on (blaze-engine pages.rs `Encoded::changed_entries`), each page read once as its
    /// witness; (0, 0) for a whole encode.
    pub(crate) changed: (u32, u32),
}

/// Which pages a commit keys into the answer's reach root ([`page_indexes`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rekey {
    /// Only the pages whose key moved since the last answer: every other page's index is
    /// carried from the last commit's reach root, read in place.
    Changed,
    /// Every page, as a cold commit does: the planted control of the page edit test (a commit
    /// that re-keys what did not move).
    #[cfg(test)]
    All,
}

/// What [`page_indexes`] did: the pages it keyed (their index computed now: one digest for the
/// new key, one for the old a page replaced), and why the reach root was made whole if it was.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Rekeyed {
    pub(crate) pages: u32,
    pub(crate) whole: Option<WhyWhole>,
}

/// Why a commit's reach root was made from every page rather than carried: each a first commit
/// of its kind, said on the profile's line, never a quiet second route.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WhyWhole {
    /// No last answer: the first commit of this checkout's answer.
    NoLast,
    /// The last answer's paging differs (its `bits`): every page's index moved.
    Paging,
    /// No unexpired reach root or answer root under the checkout's names (`{name}.reach`,
    /// `{name}.answer`): the first commit of this form, or one past the roots' life.
    NoRoot,
    /// The answer root names another answer than the one this commit is made over: another
    /// command committed between this one's read and its write.
    NotLast,
    /// The planted control ([`Rekey::All`]).
    Planted,
}

impl fmt::Display for Rekeyed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.whole {
            None => write!(f, "{} page(s) re-keyed, the rest carried from the last reach root", self.pages),
            Some(why) => write!(f, "{} page(s) keyed, the reach root made whole ({why:?})", self.pages),
        }
    }
}

/// The checkout's reach root's version: the pages' indexes alone (the root, lines, header
/// shards and member rows under [`ANSWER_ROOT`]). The form before it (`{name}.pages`, the direct
/// index among the pages') is never read: it expires by its life.
pub(crate) const REACH_ROOT: &str = "reach";
pub(crate) const ANSWER_ROOT: &str = "answer";

const PAGES_DOMAIN: &str = "blaze-cli engine-check page keys";

/// What a request wrote of the stored answer and read of the last (engine_check.rs
/// `Checked::stored`): the oracle of the drivers' world test (engine_ready_edit_tests.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StoredKeys {
    /// The new root's key ([`Saved::root`]).
    pub(crate) root: Key,
    /// The digest of its page keys in order ([`Saved::pages_digest`], a test build's alone) and
    /// how many there are.
    pub(crate) pages: (Option<ContentId>, u32),
    /// What the commit keyed into its reach root ([`Saved::rekeyed`]).
    pub(crate) rekeyed: Rekeyed,
    /// Task pages of the last answer this request read (`Stored::read_counts`'s first).
    pub(crate) pages_read: usize,
    /// Producer header shards of the last answer this request read (its second): 0 under the
    /// ready driver, which answers every header from the row's page.
    pub(crate) header_shards_read: usize,
    /// Rows of the last answer this request asked by name, by kind, repeats and distinct names
    /// apart ([`Stored::asked`]).
    pub(crate) asked: AskedByKind,
    /// The commit's delta: entries written and the pages they fell on ([`Saved::changed`]).
    pub(crate) changed: (u32, u32),
}

/// The profile's `answer keys:` line: what two drivers' runs of one edit are compared by.
impl core::fmt::Display for StoredKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (digest, count) = self.pages;
        write!(f, "answer keys: root {}, {count} page(s)", self.root)?;
        if let Some(digest) = digest {
            write!(f, " digest {}", Key(digest))?;
        }
        write!(f, "; read of the last: {} task page(s), {} producer header shard(s); reach: {}", self.pages_read, self.header_shards_read, self.rekeyed)
    }
}

impl StoredKeys {
    pub(crate) fn of(saved: &Saved, last: Option<&Stored<'_>>) -> StoredKeys {
        let (pages_read, header_shards_read) = last.map_or((0, 0), Stored::read_counts);
        StoredKeys { root: saved.root, pages: (saved.pages_digest, saved.pages), rekeyed: saved.rekeyed, pages_read, header_shards_read, asked: last.map_or_else(AskedByKind::default, Stored::asked), changed: saved.changed }
    }
}

/// `kept` and the leaf table of `rows` stored under the root `name` over `last` (its pages not
/// put again), by the phase path: the producer header shards its rows sit on rewritten when
/// current (blaze-engine headers.rs `Headers::after`); visible from the store's commit after
/// the answer.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_edit<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    last_root: Option<Key>,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, last, Writer::Phases, rows, journal, alloc, frame, row_keys, (last_root, Rekey::Changed))
}

/// [`save_edit`] keying every page into the reach root: the page edit test's planted control.
#[cfg(test)]
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_edit_rekeying_all<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    last_root: Option<Key>,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, last, Writer::Phases, rows, journal, alloc, frame, &[], (last_root, Rekey::All))
}

/// The `Ready` driver's commit of an edit (D187, engine_ready_check.rs): `kept`'s changed rows
/// merged into only the pages they sit on over `last` (pages.rs `encode_changes`), the lines,
/// then the sealed pointer last, in one store commit. No producer header shard is read or
/// written: the pointer names the index [`Headers::Absent`] (since this request, or since the
/// commit that first left it so), and every later header lookup reads the row's page.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_changes<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: &Stored<'_>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    last_root: Option<Key>,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, Some(last), Writer::Ready, rows, journal, alloc, frame, row_keys, (last_root, Rekey::Changed))
}

#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
fn save<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    writer: Writer,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    (last_root, rekey): (Option<Key>, Rekey),
) -> Result<Saved, CliError> {
    let encoded = match last {
        Some(stored) => pages::encode_changes(kept, stored.root, alloc).map_err(page_error)?,
        None => pages::encode(kept, None, alloc),
    };
    // The encoder's puts are the new pages and the root (pages.rs `encode`).
    let new_pages = u32::try_from(encoded.puts.len().saturating_sub(1)).unwrap_or(u32::MAX);
    #[cfg(test)]
    let pages_digest = {
        let mut page_keys: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(encoded.pages.len() * 32, frame);
        for page in encoded.pages.iter() {
            page_keys.extend_from_slice(&page.0.0);
        }
        Some(ContentId::of(PAGES_DOMAIN, &page_keys))
    };
    #[cfg(not(test))]
    let pages_digest = None;
    let mut saved = Saved { bytes: encoded.bytes, new_pages, root: encoded.root, pages_digest, changed: (encoded.changed_entries, encoded.changed_pages), ..Saved::default() };
    for (key, bytes) in encoded.puts.iter() {
        store.put(cx, *key, bytes)?;
        saved.puts += 1;
        // The root is among the puts (`new_pages` above leaves it out); a page is keyed by
        // its bytes and the root is not a page's key.
        if *key != encoded.root {
            let len = bytes.len() as u64;
            saved.new_page_bytes += len;
            saved.largest_new_page = saved.largest_new_page.max(len);
        }
    }
    let (headers, writes) = Headers::after(last.map(|base| base.headers), kept, writer, kept.answer.serial, HeadersPlant::None, |index| match last {
        Some(base) => base.headers_at(index),
        None => Ok(None),
    }, alloc).map_err(page_error)?;
    saved.new_headers = writes.len() as u32;
    for (key, bytes) in &writes {
        store.put(cx, *key, bytes)?;
        saved.puts += 1;
    }
    // The printed lines, under the key of their bytes: equal lines are one record.
    store.put(cx, lines.0, lines.1)?;
    saved.puts += 1;
    let leaves = Leaves::bytes_in(rows, frame)?;
    let mut pointer: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
    pointer.extend_from_slice(POINTER_MAGIC);
    pointer.extend_from_slice(&encoded.root.0.0);
    pointer.extend_from_slice(&lines.0.0.0);
    let facts = Facts { files: encoded.files, printed, findings: encoded.findings, owed: encoded.owed };
    pointer.extend_from_slice(&facts.files.to_le_bytes());
    pointer.push(facts.printed.byte());
    pointer.extend_from_slice(&facts.findings.to_le_bytes());
    pointer.extend_from_slice(&facts.owed.to_le_bytes());
    pointer.extend_from_slice(&tools_digest(tools).0);
    match &headers {
        Headers::Current(keys) => {
            pointer.push(HEADERS_CURRENT);
            pointer.extend_from_slice(&0u64.to_le_bytes());
            for key in &keys.keys {
                pointer.extend_from_slice(&key.0.0);
            }
        }
        Headers::Absent { since } => {
            pointer.push(HEADERS_ABSENT);
            pointer.extend_from_slice(&since.0.to_le_bytes());
            // No shard key: the zero key in each of the index's slots.
            pointer.extend_from_slice(&[0u8; 32 * HeaderPages::COUNT]);
        }
    }
    journal.encode(&mut pointer);
    let sealed = seal::seal(&pointer, &leaves, frame)?;
    let key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
    store.put(cx, key, &sealed)?;
    saved.puts += 1;
    // A page and the payloads of *every* row on it share one immutable reach index.
    // StoreSink also includes each names payload's expansion index and its children.
    // The store follows exactly one index level from an indexed root.
    use crate::engine_answer_rows::{AnswerSink as _, StoreSink};
    let mut sink = StoreSink { cx, store, lent, toolchain };
    let mut records = blaze_own::Vec::new_in(alloc);
    let mut scopes = blaze_own::Vec::new_in(alloc);
    let mut toolchain_records = blaze_own::Vec::new_in(alloc);
    for (page, rows) in encoded.page_records() {
        records.clear();
        scopes.clear();
        records.push(page);
        for &(name, record) in rows {
            records.push(record);
            if name.kind == blaze_drive::Kind::CrateScope {
                scopes.push((name, record));
                if sink.store.is_toolchain(name) {
                    toolchain_records.push(record);
                }
            }
        }
        sink.index_rows(page, &records, &scopes, alloc.into()).map_err(reach_error)?;
        saved.puts += 1;
    }
    let reach_name = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}");
    let answer_name = bstd::mem::format_in!(frame, "{name}.{ANSWER_ROOT}");
    let last_pages = match (last, last_root) {
        (Some(base), Some(key)) => Some((&base.root.pages[..], key)),
        _ => None,
    };
    let (indexes, rekeyed) = page_indexes(lent, (reach_name.as_str(), answer_name.as_str()), last_pages, &encoded.pages, rekey, alloc)?;
    saved.pages = u32::try_from(indexes.len()).unwrap_or(u32::MAX);
    saved.rekeyed = rekeyed;
    let mut direct = blaze_own::Vec::new_in(alloc);
    direct.push(blaze_cas::Key(encoded.root.0));
    direct.push(blaze_cas::Key(lines.0.0));
    if let Some(keys) = headers.current() {
        direct.extend(keys.keys.iter().filter(|key| **key != Key::default()).map(|key| blaze_cas::Key(key.0)));
    }
    // The member listing rows this request read or put (engine_bodies.rs `member_row_keys`,
    // engine_member_rows.rs): named here so they live while the answer does, and the next
    // command reads them by key instead of opening and listing each member again.
    direct.extend(row_keys.iter().map(|key| blaze_cas::Key(key.0)));
    // Two roots, one commit: the pages' indexes (at most one a page: `ROOT_INDEXES_MOST` is
    // the most pages a root's paging makes), and the answer's own index, which names this
    // answer's root key: the next commit's proof that the reach root is this answer's
    // ([`page_indexes`]).
    let answer_index = lent.index(&direct).map_err(crate::store::PlaceError::from)?;
    lent.root_through(reach_name.as_str(), &indexes, blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
    lent.root_through(answer_name.as_str(), &[answer_index], blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
    // This root accumulates indexes across commands. Unchanged pages' toolchain
    // payloads were admitted with the page and this renewal extends their lifetime.
    store.keep_toolchain(toolchain, &toolchain_records)?;
    store.pin(name, &[key])?;
    // The home's latest answer, whichever checkout wrote it: a checkout with none of its own
    // starts from it ([`LATEST_ROOT`]).
    store.pin(LATEST_ROOT, &[key])?;
    Ok(saved)
}

/// The reach root's page indexes for the pages `now` (one a page with anything on it, its
/// blaze-cas `beside_key`), sorted, and what was keyed for them. Under [`Rekey::Changed`] over a
/// last answer (its pages and its root's key), only the pages whose key moved are keyed (the old
/// key's index out, the new one's in); every other index is carried from the last commit's reach
/// root, read in place, once its answer root (`names.1`) proves that commit was the last answer's
/// (its one index names `last`'s root key: both roots are set in one commit). Before, every
/// commit keyed every page: 65,536 digests on tempo for a one-body edit (edit12 profile,
/// c0340wave89, `answer stored` 10.5 ms). The floor is the changed pages' digests plus one pass
/// over the carried keys (a merge of sorted lists); the comparison of `last`'s and `now`'s keys
/// is one more such pass. A reach root that is not the last answer's, or none, is made whole and
/// says why ([`WhyWhole`]); a carried root that lacks a changed page's old index is refused.
fn page_indexes<'h, D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    names: (&str, &str),
    last: Option<(&[Key], Key)>,
    now: &[Key],
    rekey: Rekey,
    alloc: SharedAlloc<'h>,
) -> Result<(blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>>, Rekeyed), CliError> {
    let index_of = |page: Key| blaze_cas::plain::beside_key(blaze_cas::Key(page.0));
    let whole = |why: WhyWhole| {
        let mut indexes = blaze_own::Vec::new_in(alloc);
        indexes.extend(now.iter().filter(|page| **page != Key::default()).map(|page| index_of(*page)));
        indexes.sort_unstable();
        let pages = u32::try_from(indexes.len()).unwrap_or(u32::MAX);
        Ok((indexes, Rekeyed { pages, whole: Some(why) }))
    };
    #[cfg(test)]
    if rekey == Rekey::All {
        return whole(WhyWhole::Planted);
    }
    #[cfg(not(test))]
    let Rekey::Changed = rekey;
    let Some((old, last_root)) = last else { return whole(WhyWhole::NoLast) };
    if old.len() != now.len() {
        return whole(WhyWhole::Paging);
    }
    fn held<'l, D: Driver>(lent: &'l blaze_cas::plain::Lent<D, crate::store::StoreAlloc>, name: &str) -> Result<Option<&'l [u8]>, CliError> {
        let Some(key) = lent.root_named(name) else { return Ok(None) };
        let held = lent.get(&key).map_err(crate::store::PlaceError::from)?;
        let held = held.ok_or_else(|| page_error(PagesError::Reach { owner: Key(key.0), index: Key(key.0), what: "a named root's record is not held" }))?;
        if held.bytes.len() % 32 != 0 {
            return Err(page_error(PagesError::Reach { owner: Key(key.0), index: Key(key.0), what: "a root's record is not whole keys" }));
        }
        Ok(Some(held.bytes))
    }
    let key_at = |bytes: &[u8], at: usize| blaze_cas::Key(ContentId(bytes[at * 32..at * 32 + 32].try_into().expect("a whole key")));
    // The proof: the answer root's one index names the last answer's root key.
    let Some(answer) = held(lent, names.1)? else { return whole(WhyWhole::NoRoot) };
    if answer.len() != 32 {
        return whole(WhyWhole::NotLast);
    }
    let direct = lent.get(&key_at(answer, 0)).map_err(crate::store::PlaceError::from)?;
    let Some(direct) = direct.filter(|got| got.tag == blaze_cas::plain::INDEX_TAG && got.bytes.len() % 32 == 0) else { return whole(WhyWhole::NotLast) };
    // An index's keys are sorted (blaze-cas `index`): found by halving.
    let wanted = blaze_cas::Key(last_root.0);
    let (mut low, mut high) = (0usize, direct.bytes.len() / 32);
    let mut names_last = false;
    while low < high {
        let mid = low + (high - low) / 2;
        match key_at(direct.bytes, mid).cmp(&wanted) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => {
                names_last = true;
                break;
            }
        }
    }
    if !names_last {
        return whole(WhyWhole::NotLast);
    }
    let Some(carried) = held(lent, names.0)? else { return whole(WhyWhole::NoRoot) };
    // The changed pages: the old key's index out, the new one's in.
    let mut out: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::new_in(alloc);
    let mut into: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::new_in(alloc);
    let mut pages = 0u32;
    for (was, is) in old.iter().zip(now.iter()).filter(|(was, is)| was != is) {
        pages = pages.saturating_add(1);
        if *was != Key::default() {
            out.push(index_of(*was));
        }
        if *is != Key::default() {
            into.push(index_of(*is));
        }
    }
    out.sort_unstable();
    into.sort_unstable();
    // One merge of three sorted lists: the carried keys but `out`, and `into`.
    let count = carried.len() / 32;
    let mut indexes: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::with_capacity_in((count + into.len()).saturating_sub(out.len()), alloc);
    let (mut gone, mut added) = (0usize, 0usize);
    for at in 0..count {
        let key = key_at(carried, at);
        while added < into.len() && into[added] < key {
            indexes.push(into[added]);
            added += 1;
        }
        if gone < out.len() && out[gone] == key {
            gone += 1;
            continue;
        }
        if gone < out.len() && out[gone] < key {
            return Err(page_error(PagesError::Reach { owner: Key(last_root.0), index: Key(out[gone].0), what: "the last answer's reach root lacks a changed page's old index" }));
        }
        indexes.push(key);
    }
    if gone < out.len() {
        return Err(page_error(PagesError::Reach { owner: Key(last_root.0), index: Key(out[gone].0), what: "the last answer's reach root lacks a changed page's old index" }));
    }
    indexes.extend_from_slice(&into[added..]);
    Ok((indexes, Rekeyed { pages, whole: None }))
}

fn reach_error(error: crate::engine_answer_rows::PublishError) -> CliError {
    use crate::engine_answer_rows::PublishError;
    match error {
        PublishError::Command(error) => error,
        PublishError::Cas(error) => crate::store::PlaceError::from(error).into(),
        PublishError::ExpansionIndex { record, index, .. } => page_error(PagesError::Reach { owner: record, index, what: "invalid expansion index" }),
        PublishError::RowsIndex { segment, index } => page_error(PagesError::Reach { owner: segment, index, what: "missing payload index" }),
        PublishError::RowsReach { segment, .. } => page_error(PagesError::Reach {
            owner: segment, index: Key(blaze_cas::plain::beside_key(blaze_cas::Key(segment.0)).0), what: "page payload reach exceeds the CAS index limit",
        }),
    }
}

#[cfg(test)]
#[path = "tests/engine_answer_page_edit.rs"]
mod page_edit;
