//! The flight recorder: the one buffer of blaze's profiler, always on in every blaze process.
//! Each thread keeps its recent rows of the schema's `spans` and `counters` tables
//! ([`crate::schema`]) in a fixed ring of its own, made once when the thread first records, so
//! recording allocates nothing and does no I/O. Every reader reads these rings:
//! - [`read`], from a [`Cursor`]: the Parquet history and the live feed, each taking only what
//!   is new since it last read, and skipping ahead (counted) past what was written over;
//! - [`window`]: the last stretch of time, for `blaze debug profile` and a stall;
//! - [`threads_now`] and [`progress`]: what the stall watchdog reads.
//!
//! A ring is written by its thread alone and read by any. The writer publishes each row by
//! advancing `head` with release, after a release fence that orders the previous `head` before
//! the row's words. A reader that copied a row the writer was overwriting sees, after its own
//! acquire fence, a `head` that tells it so, and drops the row.
//!
//! A span's open state (when it began, its parent, depth, fields and allocation counts) is kept
//! from its start to its close in a slot of the ring of the thread that made it. Its id names
//! that ring and slot, so a span closed on another thread finds it. The row is written to the
//! ring of the thread that closes it.
//!
//! A ring's rows live in a [`Store`]: on the heap at first, and in a file once the daemon's
//! history persists the recorder ([`persist`]): `flight-<pid>.map` in the history's
//! directory, which also holds every callsite's name, target and level, the names rows hold,
//! threads' names, the clock's base and the builds running. The kernel keeps a dead process's
//! pages, so the next history to open there (or `blaze slow`) decodes the rows past the last
//! flush (each store keeps how far the history wrote, [`flushed`]), writes them as rows of the
//! dead process, marks its running builds `crashed`, and deletes the file
//! (`flight/postmortem.rs`). Recording stays as it was: a thread writes its own store with
//! plain atomics, no system call and no allocation; a ring a thread holds as the recorder is
//! persisted moves to the file as that thread next writes, by that thread.

use std::{
    cell::{Cell, RefCell},
    fmt::Write as _,
    num::NonZeroUsize,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering, fence},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_hash::{FixedHasher, HashMap, HashSet};
use blaze_core::lock::{Lock, LockGuard};
use std::alloc::Global;

use crate::{
    clock,
    dump::{Counter, Event, Part},
    span::{self, Said, TYPED},
    time::cpu::{self, Rate, Reading},
};

/// Rows a thread keeps: [`WORDS`] words, 64 bytes, each, so 2 MiB a thread, committed as rows
/// reach it. A self-build's busiest worker wrote about 110 k rows a second before short spans
/// were folded ([`fold_below`]), so 16 k rows wrapped in 150 ms while the history's one thread
/// both drained and wrote (profrows): 32 k rows and a drain thread of its own every
/// [`crate::history::DRAIN`] keep many times that rate.
pub const CAP: usize = 1 << 15;
const WORDS: usize = 8;
/// Spans a thread is inside that are named (the enter stack); deeper ones are counted.
const OPEN_MAX: usize = 32;
/// Spans a thread may have made and not yet closed. A span past them is not recorded, and
/// counted as lost ([`Read::lost`]). A fiber parked with its enter stack keeps its spans open
/// in the ring of the thread that made them, so one thread may hold the open spans of hundreds
/// of parked fibers: 64 slots lost spans on most workers of a self-build (spans37).
const SLOTS_OPEN: usize = 1 << SLOT_BITS;
/// Slots a chunk of [`Ring::open`] holds: a chunk is allocated once a thread's open spans
/// reach it, so a thread that holds few open spans holds one chunk.
const CHUNK: usize = 256;

// A span id: the ring of the thread that made it, its slot there, and the ring's count of
// spans made, so no two spans of a process alive at once share an id.
const RING_BITS: u32 = 12;
const SLOT_BITS: u32 = 14;
const SEQ_BITS: u32 = 64 - RING_BITS - SLOT_BITS;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;
/// Rings a process may have: one per thread alive at once. Numbers run from 1; the highest
/// number the bits hold is left out, so no span's id is [`UNRECORDED`].
const RINGS_MAX: usize = (1 << RING_BITS) - 2;
/// The id of a span that is not recorded: past the callsite table, or past [`SLOTS_OPEN`].
pub(crate) const UNRECORDED: u64 = u64::MAX;

const KIND_SPAN: u64 = 1;
const KIND_PLOT: u64 = 2;
const KIND_MARK: u64 = 3;

// Word 4 of a span's row: kind, callsite, depth, the ring of the thread that made it, and the
// allocations it made (saturating).
const SITE_BITS: u32 = 12;
const DEPTH_BITS: u32 = 12;
const ALLOCS_BITS: u32 = 64 - 2 - SITE_BITS - DEPTH_BITS - RING_BITS;
const ALLOCS_MAX: u64 = (1 << ALLOCS_BITS) - 1;
/// A span row's word 7: the bytes it allocated in its low bits (saturating at a TiB), and its
/// CPU share ([`cpu::share_word`]) in the [`cpu::SHARE_BITS`] above.
const SHARE_SHIFT: u32 = 64 - cpu::SHARE_BITS;
const BYTES_MAX: u64 = (1 << SHARE_SHIFT) - 1;

fn lock<T>(mutex: &Lock<T>) -> LockGuard<'_, T> {
    mutex.lock()
}

pub mod postmortem;

// --- Callsites --------------------------------------------------------------------------

/// Slots of the callsite table; a site is its slot plus one, so 0 is no site.
const SLOTS: usize = (1 << SITE_BITS) - 1;
/// The table is kept at most half full, so a lookup probes one or two slots.
const SITES_MAX: usize = SLOTS / 2;

type Meta = tracing::Metadata<'static>;

/// A callsite recorded, and which of the generic span columns its fields fill: bit `i` for
/// the column at [`span::Said::get`]'s `i` ([`span::generic_of`]). The typed columns only a
/// span kind fills ([`crate::span`]). `held`: its spans are [`HELD_FIELD`] ones.
struct Site {
    meta: &'static Meta,
    fields: u8,
    held: bool,
}

/// The field that makes a callsite's spans held: open across the steps of a state machine
/// (a build script's run on the daemon's loop, from its start to its end), entered only while
/// one of its steps runs. A held span's `self_cpu_ns` counts its CPU while it is entered, as
/// every span's does ([`crate::time::cpu`]), not all the while it is open, which is the loop's
/// other work; held, it is never folded.
pub const HELD_FIELD: &str = "held";

/// The bit of a slot's `what` (in its site's half, above [`SITE_BITS`]) that says its span is
/// held.
const HELD_BIT: u64 = 1 << 15;
const _: () = assert!(SITE_BITS < 15, "the held bit is above a site");

const _: () = assert!(span::GENERIC <= 8, "a callsite's generic fields are bits of a u8");

fn field_of(name: &str) -> u8 {
    span::generic_of(name).map_or(0, |at| 1 << at)
}

/// The callsites recorded, open-addressed by the address of their metadata: written under
/// [`TAKEN`] as a callsite registers, read with no lock by every span.
static SITES: [OnceLock<Site>; SLOTS] = [const { OnceLock::new() }; SLOTS];
static TAKEN: Lock<usize> = Lock::unrecorded("profile.flight.taken", 0);

fn home(meta: &tracing::Metadata<'_>) -> usize {
    let address = std::ptr::from_ref(meta).addr() as u64;
    (address.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % SLOTS
}

/// The site of a callsite [`register`]ed.
#[inline]
pub(crate) fn site(meta: &tracing::Metadata<'_>) -> Option<u16> {
    let mut at = home(meta);
    for _ in 0..SLOTS {
        match SITES[at].get() {
            Some(it) if std::ptr::addr_eq(it.meta, meta) => return Some(at as u16 + 1),
            Some(_) => at = (at + 1) % SLOTS,
            None => return None,
        }
    }
    None
}

/// Gives the callsite a site, once; `None` past [`SITES_MAX`] callsites.
pub(crate) fn register(meta: &'static Meta) -> Option<u16> {
    let mut taken = lock(&TAKEN);
    if let Some(site) = site(meta) {
        return Some(site);
    }
    if *taken >= SITES_MAX {
        return None;
    }
    let mut at = home(meta);
    while SITES[at].get().is_some() {
        at = (at + 1) % SLOTS;
    }
    let fields = meta.fields().iter().fold(0, |all, it| all | field_of(it.name()));
    let held = meta.fields().field(HELD_FIELD).is_some();
    _ = SITES[at].set(Site { meta, fields, held });
    *taken += 1;
    // Under `TAKEN`: the mapping's site records have one writer at a time.
    postmortem::site_registered(at as u16 + 1, meta);
    Some(at as u16 + 1)
}

/// The callsite of `site`.
pub fn meta(site: u16) -> Option<&'static Meta> {
    Some(SITES.get(usize::from(site).checked_sub(1)?)?.get()?.meta)
}

/// What a writer of a table names a site by: its callsite's name, target and level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SiteText {
    pub name: &'static str,
    pub target: &'static str,
    /// As `tracing::Level::as_str` says it: `TRACE` to `ERROR`.
    pub level: &'static str,
}

/// The name, target and level of `site`: a callsite of this process ([`meta`]), or one of a
/// process that died, whose rows the postmortem read back ([`foreign_site`]). The one way a
/// writer of a table names a row's site.
#[inline]
pub fn site_text(site: u16) -> Option<SiteText> {
    match usize::from(site).checked_sub(FOREIGN_FROM) {
        None => meta(site).map(|it| SiteText {
            name: it.name(),
            target: it.target(),
            level: it.level().as_str(),
        }),
        Some(at) => FOREIGN.get(at)?.get().copied(),
    }
}

/// Sites from here on are of processes that died: past every site of this process.
const FOREIGN_FROM: usize = 1 << SITE_BITS;
/// The distinct callsites of dead processes one process may name. Each is leaked (one
/// `SiteText` and its text), deduped by its text, so a daemon that recovers the same blaze's
/// rows after each crash names its sites once.
const FOREIGN_MAX: usize = 1 << 13;
const _: () = assert!(FOREIGN_FROM + FOREIGN_MAX <= u16::MAX as usize, "a foreign site is a u16");

/// The sites of dead processes, read with no lock by a writer of a table.
static FOREIGN: [OnceLock<SiteText>; FOREIGN_MAX] = [const { OnceLock::new() }; FOREIGN_MAX];
/// The [`FOREIGN`] slots of each hash of a text ([`foreign_key`]), and how many are taken.
// alloc: none until a postmortem names a site; grows to the distinct sites of dead processes
static FOREIGN_BY_TEXT: Lock<(usize, Option<HashMap<u64, Vec<u16>, Global>>)> =
    Lock::unrecorded("profile.flight.foreign_by_text", (0, None));

fn foreign_key(name: &str, target: &str, level: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = FixedHasher::default();
    (name, target, level).hash(&mut hasher);
    hasher.finish()
}

/// The site of this process that names a dead process's callsite: the same site for the same
/// name, target and level, for the process's life. `None` past [`FOREIGN_MAX`] distinct ones.
pub(crate) fn foreign_site(name: &str, target: &str, level: &str) -> Option<u16> {
    let level = match level {
        "TRACE" => "TRACE",
        "DEBUG" => "DEBUG",
        "INFO" => "INFO",
        "WARN" => "WARN",
        _ => "ERROR",
    };
    let key = foreign_key(name, target, level);
    let mut guard = lock(&FOREIGN_BY_TEXT);
    let (taken, by_text) = &mut *guard;
    let bucket = by_text.get_or_insert_with(|| HashMap::new_in(Global)).entry(key).or_default();
    let same = |site: &u16| {
        site_text(*site)
            .is_some_and(|it| it.name == name && it.target == target && it.level == level)
    };
    if let Some(&site) = bucket.iter().find(|it| same(it)) {
        return Some(site);
    }
    if *taken >= FOREIGN_MAX {
        return None;
    }
    let text = SiteText {
        // alloc: once per distinct callsite of a dead process, for the process's life
        name: Box::leak(name.into()),
        // alloc: as above
        target: Box::leak(target.into()),
        level,
    };
    _ = FOREIGN[*taken].set(text);
    let site = (FOREIGN_FROM + *taken) as u16;
    *taken += 1;
    // alloc: a site's place in its bucket, once per distinct callsite of a dead process
    bucket.push(site);
    Some(site)
}

// --- Names ------------------------------------------------------------------------------

pub mod names;

use names::{CLASS, KEPT_SYM, KEPT_TOKEN, Token};

/// A string a decoded row names (a field's value, a thread's name), as a number of this
/// process: 0 is none. Its top two bits say its table: [`KEPT_SYM`] the kept names (threads'),
/// [`READER_SYM`] the readers' ([`READER`]), [`RECOVERED_SYM`] a postmortem's. A ring's rows
/// hold ids of their own ring instead ([`names`]), which [`decode`] turns into these.
pub type Sym = u32;

/// The class of a name a reader gave ([`ReadTable`]): bits 24..30 its generation's epoch,
/// the index from 1 below.
const READER_SYM: u32 = 0b01 << 30;
/// The class of a name a postmortem gave ([`RECOVERED`]).
const RECOVERED_SYM: u32 = 0b11 << 30;
/// Names one generation of the readers' table holds: its index bits.
const READER_GENERATION: usize = (1 << 24) - 1;
/// The epochs a reader's sym can say: bits 24..30.
const READER_EPOCHS: u32 = 1 << 6;

/// Names numbered from 1 under `class`: the kept names, a postmortem's, or one generation of
/// the readers'. Each is under its lock, which only its givers and readers take, never a
/// thread that records.
#[derive(Default)]
struct Table {
    ids: HashMap<std::sync::Arc<str>, Sym, Global>,
    names: Vec<std::sync::Arc<str>>,
}

impl Table {
    /// The name of index `index` (from 1).
    fn get(&self, index: u32) -> Option<&std::sync::Arc<str>> {
        self.names.get(usize::try_from(index.checked_sub(1)?).ok()?)
    }

    fn add(&mut self, base: u32, name: &str) -> Sym {
        // alloc: once per distinct name a table holds, let go with it
        let kept: std::sync::Arc<str> = name.into();
        // clone: a refcount, the name's one string shared by both
        self.add_arc(base, kept.clone())
    }

    fn add_arc(&mut self, base: u32, name: std::sync::Arc<str>) -> Sym {
        self.names.push(name.clone());
        let id = base | self.names.len() as u32;
        self.ids.insert(name, id);
        id
    }
}

/// Names that live as long as the process: threads', and `&'static str`s
/// ([`span::Name::kept`]). Logged for a postmortem as they are given.
static KEPT: Lock<Option<Table>> = Lock::unrecorded("profile.flight.kept", None);
/// Names past [`KEPT`]'s room: the index bits of a kept sym.
const KEPT_MAX: usize = (1 << 30) - 1;

/// The number of `name` for as long as the process lives: a thread's name, which its ring's
/// rows hold for the thread's life, or a kept name. Only for names of which there are few: it
/// takes [`KEPT`]'s lock the first time a thread asks for each.
pub(crate) fn intern_kept(name: &str) -> Sym {
    let mut kept = lock(&KEPT);
    let kept = kept.get_or_insert_with(Table::default);
    if let Some(&id) = kept.ids.get(name) {
        return id;
    }
    if kept.names.len() >= KEPT_MAX {
        NAMES_LOST.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    let id = kept.add(KEPT_SYM, name);
    postmortem::named(id, name);
    id
}

/// One generation and the one before of the names readers give: a name read again after its
/// generation moved on is given a number of the current one, so a name in use outlives the
/// generation it was first given in; one in neither reads as none.
#[derive(Default)]
struct ReadTable {
    epoch: u32,
    current: Table,
    previous: Table,
}

impl ReadTable {
    fn base(epoch: u32) -> u32 {
        READER_SYM | epoch << 24
    }

    fn sym_of(&mut self, text: std::sync::Arc<str>) -> Sym {
        if let Some(&id) = self.current.ids.get(&text) {
            return id;
        }
        if self.current.names.len() >= READER_GENERATION {
            self.previous = std::mem::take(&mut self.current);
            self.epoch = (self.epoch + 1) % READER_EPOCHS;
        }
        self.current.add_arc(Self::base(self.epoch), text)
    }

    fn get(&self, sym: Sym) -> Option<&std::sync::Arc<str>> {
        let (epoch, index) = (sym >> 24 & (READER_EPOCHS - 1), sym & ((1 << 24) - 1));
        if epoch == self.epoch {
            self.current.get(index)
        } else if epoch == (self.epoch + READER_EPOCHS - 1) % READER_EPOCHS {
            self.previous.get(index)
        } else {
            None
        }
    }
}

/// What readers hold of the rings' names: each ring's definitions ([`names::Streams`]) and the
/// numbers of this process they were given ([`ReadTable`]). Taken by readers alone (a read of
/// the rings, a name's text, a stall's dump), never by a thread as it records.
struct Reader {
    streams: names::Streams,
    table: ReadTable,
}

static READER: Lock<Option<Reader>> = Lock::unrecorded("profile.flight.reader", None);

/// The reader, made on first use, under [`READER`]'s lock.
struct ReaderGuard(LockGuard<'static, Option<Reader>>);

impl std::ops::Deref for ReaderGuard {
    type Target = Reader;
    fn deref(&self) -> &Reader {
        self.0.as_ref().expect("`reader` made it")
    }
}

impl std::ops::DerefMut for ReaderGuard {
    fn deref_mut(&mut self) -> &mut Reader {
        self.0.as_mut().expect("`reader` made it")
    }
}

fn reader() -> ReaderGuard {
    let mut guard = lock(&READER);
    guard.get_or_insert_with(|| Reader { streams: names::Streams::default(), table: ReadTable::default() });
    ReaderGuard(guard)
}

/// The live rings, as the readers' [`names::Source`].
struct Rings;

impl names::Source for Rings {
    fn names_rows(
        &mut self,
        ring: u16,
        from: u64,
        into: &mut Vec<(u64, [u64; WORDS])>,
    ) -> (u64, u64) {
        match by_number(usize::from(ring)) {
            Some(ring) => ring.store().read_names(from, into),
            None => (from, from),
        }
    }
}

/// Names a thread gave ([`names::Count::Given`]) and the like, summed over rings in
/// [`name_counts`]: each ring's written by its thread alone.
#[derive(Default)]
struct RingNames {
    given: AtomicU64,
    linked: AtomicU64,
    /// The generation its last thread began last: where the next thread's names begin.
    generation: AtomicU32,
}

/// Names lost: a thread's table taken already (a `Debug` that names), no ring, a text cut,
/// or a row of more links than a generation's indexes.
static NAMES_LOST: AtomicU64 = AtomicU64::new(0);
/// Names a ring held that no reader's definitions resolved: [`Read::names_forgotten`].
static NAMES_FORGOTTEN: AtomicU64 = AtomicU64::new(0);

/// A thread's ring as the writer side of [`names`] sees it.
struct Writing<'a>(&'a Ring);

impl names::Out for Writing<'_> {
    #[inline]
    fn head(&self) -> u64 {
        self.0.store_mine().head.load(Ordering::Relaxed)
    }

    #[inline]
    fn put(&mut self, row: [u64; WORDS]) -> u64 {
        self.0.put(row)
    }

    fn began(&mut self, generation: u32) {
        self.0.names.generation.store(generation, Ordering::Relaxed);
    }

    fn counted(&mut self, count: names::Count) {
        let names = &self.0.names;
        let one = |it: &AtomicU64| it.store(it.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        match count {
            names::Count::Given => one(&names.given),
            names::Count::Linked => one(&names.linked),
            names::Count::Cut | names::Count::Lost => {
                NAMES_LOST.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

thread_local! {
    /// This thread's names ([`names::Local`]), for the ring it holds: made as it first names.
    static NAMES_HERE: RefCell<Option<names::Local>> = const { RefCell::new(None) };
}

/// Runs `with` on this thread's names and ring: `None` when the thread has no ring or its names
/// are taken (a `Debug` that names inside a name being given). Never inlined and never across a
/// wait: a fiber that parks resumes on another thread, whose names are its own
/// (`no-wait-across-thread-locals`).
#[inline(never)]
fn with_local<T>(with: impl FnOnce(&mut names::Local, &mut Writing<'_>) -> T) -> Option<T> {
    let ring = ring()?;
    let done = NAMES_HERE.try_with(|here| {
        let mut here = here.try_borrow_mut().ok()?;
        let local = match &mut *here {
            Some(local) if local.ring() == ring.number => local,
            here => here.insert(names::Local::new(
                ring.number,
                ring.names.generation.load(Ordering::Relaxed),
            )),
        };
        Some(with(local, &mut Writing(ring)))
    });
    let done = done.ok().flatten();
    if done.is_none() {
        NAMES_LOST.fetch_add(1, Ordering::Relaxed);
    }
    done
}

/// The token of `text` on this thread: its first use in this thread's generation writes its
/// definition into the thread's ring; nothing shared is touched. 0 when the thread can't name.
#[inline]
pub(crate) fn intern(text: &str) -> Token {
    with_local(|local, out| local.intern(text, out)).unwrap_or(0)
}

/// `words`, tokens a row this thread writes next will hold, as the row's own ids ([`names`]).
/// A word under 2^32 is a sym of this process's tables (a name read back from a table or a
/// row), named again here.
#[inline]
fn row_syms(words: &mut [u64]) {
    if words.iter().all(|it| *it == 0 || *it & KEPT_TOKEN != 0) {
        for word in words {
            if *word != 0 {
                *word = u64::from(KEPT_SYM) | *word & ((1 << 30) - 1);
            }
        }
        return;
    }
    for word in words.iter_mut() {
        if *word != 0 && *word >> 32 == 0 {
            *word = match name(*word as Sym) {
                _ if *word as u32 & CLASS == KEPT_SYM => KEPT_TOKEN | *word & ((1 << 30) - 1),
                Some(text) => intern(&text),
                None => 0,
            };
        }
    }
    if with_local(|local, out| local.row_syms(words, out)).is_none() {
        words.fill(0);
    }
}

/// The text of `token`, a [`span::Name`]'s or a slot's: a kept name, a sym of this process,
/// or a ring's name, whose ring a reader catches up to find it.
pub(crate) fn token_text(token: Token) -> Option<std::sync::Arc<str>> {
    if token == 0 {
        return None;
    }
    if token & KEPT_TOKEN != 0 {
        return name(KEPT_SYM | (token & ((1 << 30) - 1)) as u32);
    }
    if token >> 32 == 0 {
        return name(token as Sym);
    }
    let found = reader().streams.text(token, &mut Rings);
    if found.is_none() {
        NAMES_FORGOTTEN.fetch_add(1, Ordering::Relaxed);
    }
    found
}

/// A sym of this process for `text`, from the readers' table: what a name read back from a
/// table is given ([`span::Value::of_text`]).
pub fn read_sym(text: &str) -> Sym {
    reader().table.sym_of(text.into())
}

/// The key the hot sketches keep `token` under ([`names::hot_key`]).
pub(crate) fn hot_key(token: Token) -> u32 {
    if token != 0 && token >> 32 == 0 {
        // A sym of this process: named on this thread first.
        return names::hot_key(name(token as Sym).map_or(0, |it| intern(&it)));
    }
    names::hot_key(token)
}

/// The text of a hot sketch's key ([`hot_key`]): a kept name's, or a ring's name's.
pub(crate) fn hot_name(key: u32) -> Option<std::sync::Arc<str>> {
    if key == 0 {
        return None;
    }
    if key < 1 << 20 {
        return name(KEPT_SYM | key);
    }
    let mut reader = reader();
    reader.streams.catch_up((key >> 20) as u16, &mut Rings);
    reader.streams.hot_text(key)
}

/// Names given, linked and lost since the process began: what naming cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NameCounts {
    /// Definitions written: a text a thread named first in its generation.
    pub given: u64,
    /// Links written: a name a thread used that another thread or an older generation gave.
    pub linked: u64,
    pub lost: u64,
}

/// See [`NameCounts`].
pub fn name_counts() -> NameCounts {
    let rings = lock(&RINGS);
    let sum = |of: fn(&RingNames) -> &AtomicU64| {
        rings.iter().map(|it| of(&it.names).load(Ordering::Relaxed)).sum()
    };
    NameCounts {
        given: sum(|it| &it.given),
        linked: sum(|it| &it.linked),
        lost: NAMES_LOST.load(Ordering::Relaxed),
    }
}

/// Names of rows a postmortem read back from a process that died ([`postmortem`]), for as
/// long as one postmortem writes them. `None` between postmortems.
static RECOVERED: Lock<Option<Table>> = Lock::unrecorded("profile.flight.recovered", None);
/// Names one postmortem may give: the index bits of a recovered sym.
const RECOVERED_MAX: usize = (1 << 30) - 1;

/// A number of this process for `name`, a name of a dead process's rows, while its postmortem
/// runs; `None` past [`RECOVERED_MAX`].
fn recovered(name: &str) -> Option<Sym> {
    let mut recovered = lock(&RECOVERED);
    let recovered = recovered.get_or_insert_with(Table::default);
    if let Some(&id) = recovered.ids.get(name) {
        return Some(id);
    }
    (recovered.names.len() < RECOVERED_MAX).then(|| recovered.add(RECOVERED_SYM, name))
}

/// The string of `sym`, a sym of this process: none for 0, and for a name of a generation let
/// go (counted).
pub fn name(sym: Sym) -> Option<std::sync::Arc<str>> {
    if sym == 0 {
        return None;
    }
    let index = sym & !CLASS;
    // clone: a refcount
    let found = match sym & CLASS {
        KEPT_SYM => lock(&KEPT).as_ref().and_then(|it| it.get(index).cloned()),
        RECOVERED_SYM => lock(&RECOVERED).as_ref().and_then(|it| it.get(index).cloned()),
        READER_SYM => lock(&READER).as_ref().and_then(|it| it.table.get(sym).cloned()),
        // A ring's own id: only a row holds one, and decoding turns it into one of the above.
        _ => None,
    };
    if found.is_none() {
        NAMES_FORGOTTEN.fetch_add(1, Ordering::Relaxed);
    }
    found
}

/// How [`decode`] turns the ids a ring's rows hold into syms of this process.
trait Namer {
    /// The sym of `word`, an id the row at index `at` of ring `ring` holds: 0, a kept name's, or
    /// one of the ring's own.
    fn sym(&mut self, ring: u16, at: u64, word: u64) -> Sym;
}

/// This process's rings, through the readers' state.
struct LiveNamer<'a>(&'a mut Reader);

impl Namer for LiveNamer<'_> {
    fn sym(&mut self, ring: u16, at: u64, word: u64) -> Sym {
        let word = word as u32;
        if word == 0 || word & CLASS == KEPT_SYM {
            return word;
        }
        let Reader { streams, table } = &mut *self.0;
        match streams.resolve(ring, at, word, &mut Rings) {
            Some(text) => table.sym_of(text),
            None => 0,
        }
    }
}

// --- Plots ------------------------------------------------------------------------------

/// The values worth watching over time, as counters rather than spans: a span on a warm memo
/// hit (3-4 ns) would cost ten times the hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Plot {
    /// The nodes of the last build the memo settled, running nothing.
    MemoHits,
    /// The nodes of the last build that ran their task.
    MemoMisses,
    /// The nodes of the last build that took a task another node ran.
    Joined,
    /// Tasks of the plane ready and waiting for a worker.
    Queued,
    /// Tasks of the plane running on a worker.
    Running,
    /// Bytes the workers' frames hold (`blaze_mem::tag::Tag::Frames`).
    FrameBytes,
    /// Bytes every tag of `blaze_mem` holds.
    LiveBytes,
    /// Bytes a fiber's stack has reserved, at a job's end ([`fiber_stack`]).
    FiberStackReserved,
    /// Bytes below its base the job ran on its fiber's stack ([`FiberStack::deepest`]).
    FiberStackDeepest,
    /// Bytes of the fiber's stack committed so far ([`FiberStack::touched`]).
    FiberStackTouched,
    /// Bytes of segments the job grew past its fiber's stack ([`FiberStack::grown`]).
    FiberStackGrown,
    /// Bytes of the process's physical footprint, what the OS counts against it (compressed
    /// pages too): all it holds, not only what `blaze_mem` tags.
    Footprint,
    /// Proc-macro wasm modules the daemon had in its memo when loaded, so far.
    MacroModuleMemo,
    /// Proc-macro wasm modules whose machine code the daemon loaded from its own store, so far.
    MacroModuleStore,
    /// Proc-macro wasm modules the daemon compiled, so far.
    MacroModuleCompiled,
    /// Spans the process made and could not record, so far ([`Read::lost`]): past the open
    /// slots of a thread, the callsite table or the rings. Written by the history at each
    /// flush; a wait or a stretch of work with no span is one of these when it is above 0.
    ProfileSpansLost,
    /// Rows the history's reader found written over before it read them, so far
    /// ([`Read::skipped`]): a ring that wrapped between two drains.
    ProfileRowsSkipped,
    /// Bytes `blaze_mem` has committed in every region, whatever tag: [`Plot::Footprint`] less
    /// this is what the process holds outside `blaze_mem` (the global heap, code, stacks).
    CommittedBytes,
    /// Bytes of address space every live `blaze_mem` region reserves.
    ReservedBytes,
    /// Of [`Plot::ProfileRowsSkipped`], those written over while the history's drain had
    /// stopped because its writer was behind (`history::BACKLOG_ROWS` drained and not yet
    /// written); the rest wrapped a ring between two drains.
    ProfileRowsBehind,
    /// Bugs the process found in itself and went on past, so far
    /// (`stdx::assert::reported::total`): a value formatted with no database attached, a
    /// `never!` that failed. Written by the history with each build's flush and whenever it
    /// grew; each site that reported is also a row of `errors` of kind `blaze-bug`.
    BlazeBugs,
    /// Heap bytes of every span map kept as a query's value (a macro expansion's, from
    /// `parse_macro_expansion`) and still alive (`span::kept_maps`): what a packed layout of
    /// the maps would shrink. Plotted with the memory's plots.
    SpanMapBytes,
    /// Pieces (a text range and its span each) of the maps of [`Plot::SpanMapBytes`].
    SpanMapPieces,
    /// Nanoseconds the recorder spent on its own threads' spans so far ([`cost`]): making,
    /// entering, leaving and closing them (sampled 1 call in [`COST_SAMPLE`]) and reading the
    /// CPU clock at boundaries (every one). Written by the history with each build's flush,
    /// with [`Plot::ProfileSpansMade`] and [`Plot::ProfileCpuReads`].
    ProfileCostNs,
    /// Spans the process made so far, recorded or not ([`cost`]): what [`Plot::ProfileCostNs`]
    /// is divided by for a span's cost.
    ProfileSpansMade,
    /// Reads of a thread's CPU clock the recorder made so far, one a boundary
    /// ([`crate::time::cpu`]).
    ProfileCpuReads,
    /// Names the recorder's threads defined so far, each in its own ring ([`names`]): span
    /// fields' and events' text a thread named first in its generation. Written with
    /// [`Plot::ProfileCostNs`], as are the two after it ([`name_counts`]).
    ProfileNamesGiven,
    /// Links written: a name a thread used that another thread, or an older generation of its
    /// own, gave (a span closed on another thread than it began on, a `Name` passed along).
    ProfileNamesLinked,
    /// Names lost: a text cut, a thread with no ring, a `Debug` that names while it is named.
    ProfileNamesLost,
    /// A counter no other plot names, its name in its row ([`plot_named`],
    /// [`CounterRow::named`]): its `counters.name` is that name.
    Named,
}

impl Plot {
    pub const ALL: [Plot; 30] = [
        Plot::MemoHits,
        Plot::MemoMisses,
        Plot::Joined,
        Plot::Queued,
        Plot::Running,
        Plot::FrameBytes,
        Plot::LiveBytes,
        Plot::FiberStackReserved,
        Plot::FiberStackDeepest,
        Plot::FiberStackTouched,
        Plot::FiberStackGrown,
        Plot::Footprint,
        Plot::MacroModuleMemo,
        Plot::MacroModuleStore,
        Plot::MacroModuleCompiled,
        Plot::ProfileSpansLost,
        Plot::ProfileRowsSkipped,
        Plot::CommittedBytes,
        Plot::ReservedBytes,
        Plot::ProfileRowsBehind,
        Plot::BlazeBugs,
        Plot::SpanMapBytes,
        Plot::SpanMapPieces,
        Plot::ProfileCostNs,
        Plot::ProfileSpansMade,
        Plot::ProfileCpuReads,
        Plot::ProfileNamesGiven,
        Plot::ProfileNamesLinked,
        Plot::ProfileNamesLost,
        Plot::Named,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Plot::MemoHits => "memo hits",
            Plot::MemoMisses => "memo misses",
            Plot::Joined => "joined",
            Plot::Queued => "pool queued",
            Plot::Running => "pool running",
            Plot::FrameBytes => "frame bytes",
            Plot::LiveBytes => "live bytes",
            Plot::FiberStackReserved => "fiber stack reserved",
            Plot::FiberStackDeepest => "fiber stack deepest",
            Plot::FiberStackTouched => "fiber stack touched",
            Plot::FiberStackGrown => "fiber stack grown",
            Plot::Footprint => "footprint bytes",
            Plot::MacroModuleMemo => "macro modules from memo",
            Plot::MacroModuleStore => "macro modules from store",
            Plot::MacroModuleCompiled => "macro modules compiled",
            Plot::ProfileSpansLost => "profile spans lost",
            Plot::ProfileRowsSkipped => "profile rows skipped",
            Plot::CommittedBytes => "committed bytes",
            Plot::ReservedBytes => "reserved bytes",
            Plot::ProfileRowsBehind => "profile rows skipped behind",
            Plot::BlazeBugs => "blaze bugs",
            Plot::SpanMapBytes => "span map bytes",
            Plot::SpanMapPieces => "span map pieces",
            Plot::ProfileCostNs => "profile cost ns",
            Plot::ProfileSpansMade => "profile spans made",
            Plot::ProfileCpuReads => "profile cpu reads",
            Plot::ProfileNamesGiven => "profile names given",
            Plot::ProfileNamesLinked => "profile names linked",
            Plot::ProfileNamesLost => "profile names lost",
            Plot::Named => "named",
        }
    }

    pub fn of(at: u64) -> Option<Plot> {
        Plot::ALL.get(usize::try_from(at).ok()?).copied()
    }
}

/// Notes `value` of the counter named `name` now, in this thread's ring: a counter no
/// [`Plot`] names (a memory owner's bytes, say), its row's `plot` the name. The name is kept
/// once for the life of the process, as [`crate::span::Name::kept`] keeps one, so after a
/// thread's first use it costs a thread-local lookup.
#[inline]
pub fn plot_named(name: &'static str, value: u64) {
    let Some(ring) = ring() else { return };
    let token = crate::span::Value::word(crate::span::Name::kept(name));
    // A kept token's row sym: the class and its number (names::Local::row_syms).
    let sym = if token == 0 { 0 } else { u64::from(KEPT_SYM | (token & !KEPT_TOKEN) as u32) };
    let build = u64::from(BUILD.with(Cell::get));
    ring.put([clock::ticks(), value, 0, 0, KIND_PLOT << 62 | Plot::Named as u64, 0, build, sym]);
}

/// Notes `value` of `which` now, in this thread's ring.
#[inline]
pub fn plot(which: Plot, value: u64) {
    if let Some(ring) = ring() {
        let build = u64::from(BUILD.with(Cell::get));
        ring.put([clock::ticks(), value, 0, 0, KIND_PLOT << 62 | which as u64, 0, build, 0]);
    }
}

/// What a job left on its fiber's stack, read by the fiber driver when the job ends. Every
/// size is in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FiberStack {
    /// The fiber, numbered by the driver: the same number for the fiber's whole life.
    pub fiber: u32,
    /// The job that ran on it.
    pub job: u64,
    /// What the fiber's stack reserved, guard page left out.
    pub reserved: u64,
    /// How far below its base the job's stack pointer went, as far as the driver saw it.
    pub deepest: u64,
    /// How much of the fiber's stack is committed: the deepest any job on it went, to the page.
    pub touched: u64,
    /// Segments the job grew past the fiber's stack.
    pub grown: u64,
}

/// The plot byte of a [`fiber_stack`] row: one row a job, read as four counters.
const FIBER_STACK_ROW: u64 = 0xff;

/// Notes `stack` now, in this thread's ring: one row, read as the counters
/// [`Plot::FiberStackReserved`], [`Plot::FiberStackDeepest`], [`Plot::FiberStackTouched`] and
/// [`Plot::FiberStackGrown`] of its fiber and job.
#[inline]
pub fn fiber_stack(stack: FiberStack) {
    if let Some(ring) = ring() {
        let build = u64::from(BUILD.with(Cell::get));
        let what = KIND_PLOT << 62 | u64::from(stack.fiber) << 8 | FIBER_STACK_ROW;
        ring.put([
            clock::ticks(),
            stack.reserved,
            stack.deepest,
            stack.touched,
            what,
            stack.grown,
            build,
            stack.job,
        ]);
    }
}

/// Notes the end of the build `serial`: a mark on the timeline, and a CPU boundary, so the
/// thread's spans of the build have their rate ([`boundary`]).
pub(crate) fn mark(serial: u64) {
    if let Some(ring) = ring() {
        ring.put([clock::ticks(), serial, 0, 0, KIND_MARK << 62, 0, 0, 0]);
        boundary(ring, CpuWhy::Mark);
    }
}

// --- The build a thread works for ---------------------------------------------------------

thread_local! {
    /// The build this thread's work is for now: 0 for none.
    static BUILD: Cell<u32> = const { Cell::new(0) };
}

/// While the result lives, what this thread records is of build `build`: the plane sets it
/// around each task it runs for a request.
#[must_use]
pub fn building(build: u32) -> Building {
    if build != 0 {
        RUNNING.with(|it| it.set(OnThread { build, ..it.get() }));
    }
    Building(BUILD.with(|it| it.replace(build)))
}

/// While the result lives, the counters this thread plots ([`plot`], [`plot_named`]) are of
/// build `build` (0: none), and nothing else of the thread is: for a thread that samples
/// process-wide counters (the daemon's loop) while a build is under way, so its rows carry that
/// build's `build_id` and a build's counters are found by it, not by the daemon's pid. Unlike
/// [`building`] it does not make the thread's job the build's.
#[must_use]
pub fn counting_for(build: u32) -> Building {
    Building(BUILD.with(|it| it.replace(build)))
}

/// See [`building`]: gives back the build the thread was working for before.
pub struct Building(u32);

/// What the calling thread works for now: its build (0: none) and its innermost span (0:
/// none). For a sample taken on the thread from inside the allocator (`crate::heap`): it
/// claims no ring and allocates nothing, and on a thread being torn down says none.
pub(crate) fn here_now() -> (u32, u64) {
    let build = BUILD.try_with(Cell::get).unwrap_or(0);
    let span = MINE.try_with(Cell::get).ok().flatten().map_or(0, Ring::innermost);
    (build, span)
}

impl Drop for Building {
    fn drop(&mut self) {
        BUILD.with(|it| it.set(self.0));
    }
}

// --- Jobs on fibers -----------------------------------------------------------------------

/// What the recorder keeps of a job in its thread-locals, moved with the job when its fiber
/// parks and resumes on another thread: the spans its thread was inside (the enter stack, the
/// parents of the spans it makes next) and the build it works for.
#[derive(Clone, Copy)]
struct Carried {
    depth: u32,
    build: u32,
    entered: [u64; OPEN_MAX],
    /// The job's span, closed as it parked, to open again as it resumes.
    job: Option<JobSegment>,
    /// The job itself, as its switches name it, and its build ([`running_job`]).
    running: OnThread,
}

/// A job's span as its job parked ([`job_began`]): its kind's site and columns, and where it
/// was in the enter stack.
#[derive(Clone, Copy)]
struct JobSegment {
    site: u16,
    said: Said,
    at: u32,
}

thread_local! {
    /// The span of the job running on this thread, 0 for none: the span a park closes and a
    /// resume opens again, so a parked job's span never covers the time other jobs ran.
    static JOB: Cell<u64> = const { Cell::new(0) };
}

/// Notes the span `id`, a job's ([`crate::span::Job`]), just opened and entered, as the job
/// running on this thread: the span of the job it runs inside, for [`job_ended`].
pub(crate) fn job_began(id: u64) -> u64 {
    JOB.with(|it| it.replace(id))
}

/// The job running on this thread ended: the span to close, its last segment, and `outer`'s
/// job runs on.
pub(crate) fn job_ended(outer: u64) -> u64 {
    JOB.with(|it| it.replace(outer))
}

/// Closes the span of the job parking on this thread, taking it out of the enter stack: what
/// to open again as it resumes. `None` for a job with no span named on the stack.
fn park_job(ring: &'static Ring) -> Option<JobSegment> {
    let id = JOB.with(|it| it.replace(0));
    if id == 0 || id == UNRECORDED {
        return None;
    }
    let depth = (ring.depth.load(Ordering::Relaxed) as usize).min(OPEN_MAX);
    let at = ring.entered[..depth].iter().rposition(|it| it.load(Ordering::Relaxed) == id)?;
    let opened = by_number(ring_of(id))?.opened(id)?;
    exited(id);
    crate::closed(id);
    Some(JobSegment { site: opened.site, said: opened.said(), at: at as u32 })
}

/// The carried state of each parked job, by the number [`take_open`] gave it, less one; and the
/// numbers free again.
struct Parked {
    jobs: Vec<Carried>,
    free: Vec<usize>,
}

// alloc: none here; grows to the most jobs parked at once, then is kept.
static PARKED: Lock<Parked> = Lock::unrecorded("profile.flight.parked", Parked { jobs: Vec::new(), free: Vec::new() });

/// Moves the parking job's enter stack and build off this thread, leaving it inside no span and
/// working for no build: the `take` of `blaze_graph::sched::fiber::carry` ([`crate::install`]
/// registers it). `None` for a job inside no span and of no build: nothing to move.
#[inline(never)]
pub fn take_open() -> Option<NonZeroUsize> {
    let build = BUILD.with(|it| it.replace(0));
    let ring = MINE.with(Cell::get);
    let job = ring.and_then(park_job);
    // The spans leaving carry no ticks of this thread's stretch: the next job's time here is
    // never theirs, and their next ticks count on the thread they resume on.
    if let Some(ring) = ring {
        cpu_leave(ring);
    }
    let running = RUNNING.with(|it| it.replace(OnThread::default()));
    let depth = ring.map_or(0, |it| it.depth.load(Ordering::Relaxed));
    let nothing = depth == 0 && build == 0 && job.is_none() && running == OnThread::default();
    if nothing {
        if let Some(ring) = ring {
            // No job runs here once it parks: the samples after this are the worker's own.
            ring.set_running(0, 0);
        }
        return None;
    }
    let mut carried = Carried { depth, build, entered: [0; OPEN_MAX], job, running };
    if let Some(ring) = ring {
        for (to, from) in
            carried.entered.iter_mut().zip(&ring.entered[..(depth as usize).min(OPEN_MAX)])
        {
            *to = from.load(Ordering::Relaxed);
        }
        ring.depth.store(0, Ordering::Release);
        // Inside no span, running no job: what the context log says until the next resumes.
        ring.set_running(0, 0);
    }
    let mut parked = lock(&PARKED);
    let at = match parked.free.pop() {
        Some(at) => {
            parked.jobs[at] = carried;
            at
        }
        None => {
            // alloc: once per job parked at once past the most so far
            parked.jobs.push(carried);
            parked.jobs.len() - 1
        }
    };
    NonZeroUsize::MIN.checked_add(at)
}

/// Puts what [`take_open`] took back on this thread, the one resuming the job: the `put` of
/// `blaze_graph::sched::fiber::carry`. The thread was left inside no span by the job it ran
/// before, so the job's stack replaces an empty one.
#[inline(never)]
pub fn put_open(taken: NonZeroUsize) {
    let at = taken.get() - 1;
    let carried = {
        let mut parked = lock(&PARKED);
        let Some(&carried) = parked.jobs.get(at) else { return };
        // alloc: none past the most jobs parked at once
        parked.free.push(at);
        carried
    };
    BUILD.with(|it| it.set(carried.build));
    RUNNING.with(|it| it.set(carried.running));
    if let Some(ring) = ring() {
        let (mut entered, mut depth) = (carried.entered, carried.depth);
        if let Some(job) = carried.job {
            // The segment begins where the job's span was, under what it was under; the spans
            // the job is inside of it keep the parent they began under.
            let at = job.at as usize;
            ring.depth.store(job.at, Ordering::Relaxed);
            let parent = at.checked_sub(1).map_or(0, |below| entered[below]);
            let id = began(job.site, Some(parent), &job.said);
            if id != UNRECORDED {
                entered.copy_within(at..OPEN_MAX - 1, at + 1);
                entered[at] = id;
                depth += 1;
                JOB.with(|it| it.set(id));
            }
        }
        let named = (depth as usize).min(OPEN_MAX);
        for (to, from) in ring.entered[..named].iter().zip(&entered) {
            to.store(*from, Ordering::Relaxed);
        }
        ring.depth.store(depth, Ordering::Release);
        // Its spans are back, and its job (its fiber, its `resumed` switch says).
        ring.job.store(carried.running.job, Ordering::Relaxed);
        ring.note_context();
        // Its innermost span counts this thread's ticks from here.
        if counting() {
            ring.top_since.store(clock::ticks(), Ordering::Relaxed);
        }
    }
}

/// Frees the slot [`take_open`] gave a job that ended holding it (or whose parked record was
/// dropped): the `end` of `blaze_graph::sched::fiber::carry`. Its open spans were closed by
/// the job's own unwind, or are left unclosed, as a span a thread never exits is.
#[inline(never)]
pub fn end_open(taken: NonZeroUsize) {
    let at = taken.get() - 1;
    let mut parked = lock(&PARKED);
    if at < parked.jobs.len() {
        // alloc: none past the most jobs parked at once
        parked.free.push(at);
    }
}

/// A job's switch on its fiber ([`switched`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Switched {
    Started = 1,
    Parked,
    Resumed,
    Ended,
    /// A worker went idle, why in its `waited` ([`WaitedOn::Nothing`] or [`WaitedOn::Until`]);
    /// no job.
    Idle,
    /// The worker woke from its idle stretch.
    Woke,
    /// A parked job became runnable, on its waker's thread: `by` is the job running there.
    Ready,
    /// A pool job that never joined the graph ended: its whole life, from `since`.
    Ran,
}

impl Switched {
    pub const ALL: [Switched; 8] = [
        Switched::Started,
        Switched::Parked,
        Switched::Resumed,
        Switched::Ended,
        Switched::Idle,
        Switched::Woke,
        Switched::Ready,
        Switched::Ran,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Switched::Started => "started",
            Switched::Parked => "parked",
            Switched::Resumed => "resumed",
            Switched::Ended => "ended",
            Switched::Idle => "idle",
            Switched::Woke => "woke",
            Switched::Ready => "ready",
            Switched::Ran => "ran",
        }
    }

    fn of(byte: u64) -> Option<Switched> {
        Switched::ALL.into_iter().find(|it| *it as u64 == byte)
    }
}

/// What a parked job waits for ([`SwitchRow::waited`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WaitedOn {
    /// A job's end: the value is the job's number, as `job` names a job.
    Job = 1,
    /// A key's flight: the value is the key's hash, its first eight bytes.
    Key,
    /// A parallel map's counter: the value is the counter's number.
    Counter,
    /// The driver's clock reaching the value, in its nanoseconds.
    Until,
    /// Nothing: the job moves to another core. No value.
    Moved,
    /// An idle worker's core has nothing to run. No value.
    Nothing,
}

impl WaitedOn {
    pub const fn name(self) -> &'static str {
        match self {
            WaitedOn::Job => "job",
            WaitedOn::Key => "key",
            WaitedOn::Counter => "counter",
            WaitedOn::Until => "until",
            WaitedOn::Moved => "moved",
            WaitedOn::Nothing => "nothing",
        }
    }

    fn of(byte: u64) -> Option<WaitedOn> {
        [
            WaitedOn::Job,
            WaitedOn::Key,
            WaitedOn::Counter,
            WaitedOn::Until,
            WaitedOn::Moved,
            WaitedOn::Nothing,
        ]
        .into_iter()
        .find(|it| *it as u64 == byte)
    }
}

/// A switch as the fiber driver tells it ([`crate::install`] turns `blaze_graph`'s into this).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Switch {
    pub event: Switched,
    /// The job: its slot's generation << 32 | its slot, one job of a driver's life.
    pub job: u64,
    pub fiber: u32,
    pub core: u32,
    /// For [`Switched::Parked`], what it waits for and its value.
    pub waited: Option<(WaitedOn, u64)>,
}

/// The plot byte of a [`switched`] row.
const SWITCH_ROW: u64 = 0xfd;

/// The job running on a thread, as its switch rows name it (0: none), and the build it last
/// worked for: what its `ended` row is of, written after its code let go of the build. Moved
/// with the job when its fiber parks ([`Carried`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OnThread {
    job: u64,
    build: u32,
}

/// A pool job that began on this thread and has not joined the graph yet ([`job_begins`]):
/// what its `started` or `ran` row says of its start.
#[derive(Clone, Copy, Debug)]
struct Begun {
    ticks: u64,
    /// Its number until it joins the graph ([`UNJOINED_SLOT`]).
    unjoined: u64,
    spawner: u64,
    /// When it was given to the pool, in ticks; 0 when not said.
    given: u64,
    /// The span open where it was given ([`giving_span`]), 0 for none: its `job` span's
    /// `creator`.
    creator: u64,
    /// The build the thread worked for before the job began: what it works for again once a
    /// job that never joined the graph ran ([`Switched::Ran`]).
    outer: u32,
}

thread_local! {
    static RUNNING: Cell<OnThread> = const { Cell::new(OnThread { job: 0, build: 0 }) };
    static BEGUN: Cell<Option<Begun>> = const { Cell::new(None) };
}

/// The slot of a job's number before it joins the graph: `blaze_graph`'s unregistered slot,
/// which no job in the graph has, under a count of this process's own as its generation.
pub const UNJOINED_SLOT: u64 = 0xffff_ffff;
static UNJOINED: AtomicU64 = AtomicU64::new(1);

/// The job running on this thread as its switch rows name it, 0 for none: the spawner a pool
/// job given to the pool now is told of (`blaze_graph::sched::fiber::observe_spawner`).
pub fn running_job() -> u64 {
    RUNNING.with(Cell::get).job
}

/// The build this thread works for now: the one [`building`] set, else the one its running job
/// last worked for; 0 for none. What a pool job given now works for
/// (`blaze_graph::sched::Given::build`).
pub fn current_build() -> u32 {
    match BUILD.with(Cell::get) {
        0 => RUNNING.with(Cell::get).build,
        build => build,
    }
}

/// A pool job given to the pool by `spawner` at `given` (ticks, 0 for not said), inside the
/// span `creator` (0 for none), begins on this thread, working for `build`, the giver's
/// ([`current_build`]): its spans and switches are the build's, wherever it runs, until it
/// ends (a parked job carries it, [`take_open`]). No row: its `started` or `ran` row says when
/// (`since`) and how long it waited for a core (`queued`), and its `job` span its `creator`
/// ([`begun_creator`]), so a job's life costs no row more than its switches.
pub fn job_begins(spawner: u64, given: u64, build: u32, creator: u64) {
    let unjoined = UNJOINED.fetch_add(1, Ordering::Relaxed) << 32 | UNJOINED_SLOT;
    let outer = BUILD.with(|it| it.replace(build));
    BEGUN.with(|it| {
        it.set(Some(Begun { ticks: clock::ticks(), unjoined, spawner, given, creator, outer }))
    });
    RUNNING.with(|it| it.set(OnThread { job: unjoined, build }));
    // A pool job runs on the thread from here, by its number until it joins the graph; on no
    // fiber of the graph's.
    if let Some(ring) = MINE.with(Cell::get) {
        ring.set_running(unjoined, 0);
    }
}

/// The span the pool job beginning on this thread was given inside ([`job_begins`]): what its
/// `job` span names as its `creator`. `None` for a job with no such span, or none beginning.
pub fn begun_creator() -> Option<u64> {
    BEGUN.with(Cell::get).map(|it| it.creator).filter(|&it| it != 0 && it != UNRECORDED)
}

/// The span this thread is inside as it gives a pool job to the pool, 0 for none: the job's
/// creator ([`job_begins`]). Kept: a span a job names is written whole, never folded
/// ([`fold_below`]).
pub fn giving_span() -> u64 {
    span_now()
}

/// The span this thread is inside now, 0 for none (and for one the recorder could not open).
/// Kept: a span another row names (a job's creator, a lock wait's waiter or holder,
/// [`crate::locks::Recorder`]) is written whole, never folded ([`fold_below`]).
pub fn span_now() -> u64 {
    let Some(ring) = MINE.with(Cell::get) else { return 0 };
    let id = ring.innermost();
    if id == UNRECORDED {
        return 0;
    }
    keep(id);
    id
}

/// The bits of a switch row's word 4 above its kind, wait and event bytes, and of its word 6
/// above its build, that hold how long a `started` or `ran` job was queued: its ticks from
/// given to begun, plus one (0: not said), low bits in word 4.
const QUEUED_LOW_SHIFT: u32 = 24;
const QUEUED_LOW_BITS: u32 = 62 - QUEUED_LOW_SHIFT;

/// The ticks a job begun at `begun` waited since it was given to the pool, as the row holds
/// them ([`QUEUED_LOW_SHIFT`]): 0 when the pool did not say when.
fn queued_word(begun: &Begun) -> u64 {
    if begun.given == 0 { 0 } else { begun.ticks.saturating_sub(begun.given).saturating_add(1) }
}

/// Notes `switch` now, in this thread's ring: one row, the `switches` table's. Its words past
/// the switch's own: `by` (word 5), the spawner of a `started` or `ran` job and the waker of
/// a `ready` one; `since` (word 7), when a `started` or `ran` job began; for `started`, in the
/// value word, the number it had before it joined the graph; and for both, how long it was
/// queued before it began, in the spare bits of words 4 and 6 ([`QUEUED_LOW_SHIFT`]).
#[inline]
pub fn switched(switch: Switch) {
    let running = RUNNING.with(Cell::get);
    let build = match BUILD.with(Cell::get) {
        0 => running.build,
        build => build,
    };
    let (waited, mut value) = switch.waited.map_or((0, 0), |(on, value)| (on as u64, value));
    let (mut job, mut by, mut since, mut queued) = (switch.job, 0, 0, 0);
    match switch.event {
        Switched::Started => {
            if let Some(begun) = BEGUN.with(Cell::take) {
                (value, by, since, queued) =
                    (begun.unjoined, begun.spawner, begun.ticks, queued_word(&begun));
            }
            RUNNING.with(|it| it.set(OnThread { job: switch.job, build }));
        }
        Switched::Ran => {
            // A job that began before the observer was installed: its life is not in the record.
            let Some(begun) = BEGUN.with(Cell::take) else { return };
            (job, by, since, queued) =
                (begun.unjoined, begun.spawner, begun.ticks, queued_word(&begun));
            RUNNING.with(|it| it.set(OnThread::default()));
            // It ran where it began, inside what the thread worked for before.
            BUILD.with(|it| it.set(begun.outer));
        }
        Switched::Ended => {
            BEGUN.with(Cell::take);
            RUNNING.with(|it| it.set(OnThread::default()));
            // As a parked job leaves its thread ([`take_open`]): working for no build, so the
            // worker's own time after it is no build's.
            BUILD.with(|it| it.set(0));
        }
        Switched::Ready => by = running.job,
        Switched::Parked | Switched::Resumed | Switched::Idle | Switched::Woke => {}
    }
    // The thread's running job moves above whether or not the row is kept.
    let Some(ring) = ring() else { return };
    // What runs on the thread from here, for the sampler: the job and fiber a start or a
    // resume brings, none after a park, an end or an idle stretch; a wake-up told on the
    // waker's thread changes nothing here.
    match switch.event {
        Switched::Started | Switched::Resumed => ring.set_running(switch.job, switch.fiber),
        Switched::Parked | Switched::Ended | Switched::Ran | Switched::Idle | Switched::Woke => {
            ring.set_running(0, 0);
        }
        Switched::Ready => {}
    }
    let queued_low = (queued & ((1 << QUEUED_LOW_BITS) - 1)) << QUEUED_LOW_SHIFT;
    let what =
        KIND_PLOT << 62 | queued_low | waited << 16 | (switch.event as u64) << 8 | SWITCH_ROW;
    let where_ = u64::from(switch.core) << 32 | u64::from(switch.fiber);
    let build = (queued >> QUEUED_LOW_BITS) << 32 | u64::from(build);
    ring.put([clock::ticks(), job, value, where_, what, by, build, since]);
    // Every switch but a wake-up told on the waker's thread cuts the thread's CPU stretch: its
    // reading follows the switch row, which the reader gives it (`switches.cpu_ns`).
    if switch.event != Switched::Ready {
        boundary(ring, CpuWhy::Switch);
    }
}

// --- Events -------------------------------------------------------------------------------

/// The low byte of word 4 of an event's row ([`evented`]).
const EVENT_ROW: u64 = 0xfc;
/// The low byte of word 4 of the rows that follow an event's row when it has fields other than
/// its message: up to [`COL_WORDS`] values a row, at the words `COL_WORDS` names, the event's
/// id at word 3, and in word 4 above its low byte one [`FIELD_SLOT_BITS`] slot a value: the
/// field's index in its callsite << 3 | its [`FieldValue`] kind (0: no value in that word).
const EVENT_FIELDS_ROW: u64 = 0xf8;
const FIELD_SLOT_SHIFT: u32 = 8;
const FIELD_SLOT_BITS: u32 = 9;
const _: () = assert!(
    FIELD_SLOT_SHIFT + FIELD_SLOT_BITS * COL_WORDS.len() as u32 <= 62,
    "a fields row's slots sit below its kind"
);
/// Fields of an event kept: `tracing`'s macros write at most 32 a callsite. A field's index
/// is 6 bits of its slot.
pub const EVENT_FIELDS: usize = 32;
const _: () = assert!(EVENT_FIELDS <= 1 << (FIELD_SLOT_BITS - 3), "a field's index fits its slot");
/// The bits of an event's id that are its row's place in its ring; above them, the ring's number.
const EVENT_INDEX_BITS: u32 = 48;

/// An event's id: the ring it was written in and its row's place there, which never repeat in
/// a process (a ring's rows are numbered from its making, whichever thread holds it).
const fn event_id(ring: u16, index: u64) -> u64 {
    (ring as u64) << EVENT_INDEX_BITS | index & ((1 << EVENT_INDEX_BITS) - 1)
}

/// Fields of events past [`EVENT_FIELDS`] or recorded while their text could not be written
/// (a field's `Debug` that itself logs), since the process began: not kept.
static FIELDS_LOST: AtomicU64 = AtomicU64::new(0);

/// A field's value as an event recorded it: a number as itself, text as a [`Sym`] (in a ring's
/// row, an id of the ring's own, [`names`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldValue {
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    Str(Sym),
}

impl FieldValue {
    /// Its kind as a fields row's slot says it, from 1; 0 is no value.
    pub(crate) const fn kind(self) -> u64 {
        match self {
            FieldValue::I64(_) => 1,
            FieldValue::U64(_) => 2,
            FieldValue::F64(_) => 3,
            FieldValue::Bool(_) => 4,
            FieldValue::Str(_) => 5,
        }
    }

    const fn word(self) -> u64 {
        match self {
            FieldValue::I64(it) => it as u64,
            FieldValue::U64(it) => it,
            FieldValue::F64(it) => it.to_bits(),
            FieldValue::Bool(it) => it as u64,
            FieldValue::Str(it) => it as u64,
        }
    }

    fn of(kind: u64, word: u64) -> Option<FieldValue> {
        Some(match kind {
            1 => FieldValue::I64(word as i64),
            2 => FieldValue::U64(word),
            3 => FieldValue::F64(f64::from_bits(word)),
            4 => FieldValue::Bool(word != 0),
            5 => FieldValue::Str(word as Sym),
            _ => return None,
        })
    }

    /// Its kind as `event_fields.kind` names it.
    pub const fn kind_name(self) -> &'static str {
        match self {
            FieldValue::I64(_) => "i64",
            FieldValue::U64(_) => "u64",
            FieldValue::F64(_) => "f64",
            FieldValue::Bool(_) => "bool",
            FieldValue::Str(_) => "str",
        }
    }
}

/// What an event said: its message and its other fields, each by its index in its callsite,
/// as [`evented`] writes them. A fixed buffer, so a number is kept with no allocation; text is
/// named on the thread ([`intern`]), a field's [`FieldValue::Str`] holding its place in
/// `tokens` until [`evented`] gives the row its ids.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Logged {
    /// Its `message` field's token; 0 for none.
    message: Token,
    fields: [(u8, FieldValue); EVENT_FIELDS],
    len: usize,
    tokens: [Token; EVENT_FIELDS],
    texts: usize,
}

impl Logged {
    const EMPTY: Logged = Logged {
        message: 0,
        fields: [(0, FieldValue::Bool(false)); EVENT_FIELDS],
        len: 0,
        tokens: [0; EVENT_FIELDS],
        texts: 0,
    };

    /// Its fields other than its message, in the order it recorded them: each one's index in
    /// its callsite's fields and its value.
    pub(crate) fn fields(&self) -> &[(u8, FieldValue)] {
        &self.fields[..self.len]
    }

    fn put(&mut self, field: &tracing::field::Field, value: FieldValue) {
        let index = field.index();
        if self.len == EVENT_FIELDS || index >= EVENT_FIELDS {
            FIELDS_LOST.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.fields[self.len] = (index as u8, value);
        self.len += 1;
    }

    fn text(&mut self, field: &tracing::field::Field, token: Token) {
        if field.name() == "message" {
            self.message = token;
        } else if self.texts < EVENT_FIELDS {
            let at = self.texts;
            let len = self.len;
            self.put(field, FieldValue::Str(at as Sym));
            if self.len > len {
                self.tokens[at] = token;
                self.texts += 1;
            }
        } else {
            FIELDS_LOST.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Its tokens as the ids of the row its thread writes next ([`row_syms`]): the message's,
    /// and each text field's in place.
    fn named(&mut self) -> Sym {
        let mut words = [0; EVENT_FIELDS + 1];
        words[0] = self.message;
        words[1..=self.texts].copy_from_slice(&self.tokens[..self.texts]);
        row_syms(&mut words[..=self.texts]);
        for (_, value) in &mut self.fields[..self.len] {
            if let FieldValue::Str(at) = value {
                *at = words[1 + *at as usize] as Sym;
            }
        }
        words[0] as Sym
    }
}

impl tracing::field::Visit for Logged {
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.put(field, FieldValue::I64(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.put(field, FieldValue::U64(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.put(field, FieldValue::F64(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.put(field, FieldValue::Bool(value));
    }

    fn record_i128(&mut self, field: &tracing::field::Field, value: i128) {
        match i64::try_from(value) {
            Ok(it) => self.put(field, FieldValue::I64(it)),
            Err(_) => self.record_debug(field, &value),
        }
    }

    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        match u64::try_from(value) {
            Ok(it) => self.put(field, FieldValue::U64(it)),
            Err(_) => self.record_debug(field, &value),
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.text(field, intern(value));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let sym = TEXT
            .try_with(|text| {
                // A field's `Debug` that itself logs finds the text taken: that field is lost.
                let mut text = text.try_borrow_mut().ok()?;
                text.clear();
                // Writing to a `String` does not fail.
                _ = write!(text, "{value:?}");
                Some(intern(&text))
            })
            .ok()
            .flatten();
        match sym {
            Some(sym) => self.text(field, sym),
            None => _ = FIELDS_LOST.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// Notes the `tracing` event `event` of `site` now, in this thread's ring: one row, the `events`
/// table's, then a row each [`COL_WORDS`] of its fields other than its message, the
/// `event_fields` table's. The event's words: when, the job running here ([`running_job`]), the
/// span it happened in, its site (level, target and name), its message interned, and its
/// build; its id is its ring and its row's place there ([`event_id`]). A field's number is kept
/// as itself, with no allocation, and its text interned. Nothing when the thread has no ring
/// (counted as ringless).
pub(crate) fn evented(event: &tracing::Event<'_>, site: u16) {
    let Some(ring) = ring() else {
        RINGLESS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let span = if event.is_root() {
        0
    } else if let Some(parent) = event.parent() {
        parent.into_u64()
    } else {
        let depth = (ring.depth.load(Ordering::Relaxed) as usize).min(OPEN_MAX);
        ring.entered[..depth].last().map_or(0, |it| it.load(Ordering::Relaxed))
    };
    let span = if span == UNRECORDED { 0 } else { span };
    keep(span);
    // Every field is read and named before a row is written: a field's `Debug` that logs writes
    // its own event's rows first, and a definition goes in before the rows that name it, never
    // between this one's.
    let mut logged = Logged::EMPTY;
    event.record(&mut logged);
    let message = logged.named();
    let running = RUNNING.with(Cell::get);
    let build = match BUILD.with(Cell::get) {
        0 => running.build,
        build => build,
    };
    let now = clock::ticks();
    let what = KIND_PLOT << 62 | u64::from(site) << 8 | EVENT_ROW;
    let index =
        ring.put([now, running.job, span, 0, what, u64::from(message), u64::from(build), 0]);
    let id = event_id(ring.number, index);
    for chunk in logged.fields().chunks(COL_WORDS.len()) {
        let mut row = [now, 0, 0, id, 0, 0, 0, 0];
        let mut slots = 0;
        for (place, &(index, value)) in chunk.iter().enumerate() {
            row[COL_WORDS[place]] = value.word();
            slots |= (u64::from(index) << 3 | value.kind())
                << (FIELD_SLOT_SHIFT + FIELD_SLOT_BITS * place as u32);
        }
        row[4] = KIND_PLOT << 62 | slots | EVENT_FIELDS_ROW;
        ring.put(row);
    }
}

/// Fields of events not kept since the process began ([`FIELDS_LOST`]).
pub fn fields_lost() -> u64 {
    FIELDS_LOST.load(Ordering::Relaxed)
}

// --- Allocations --------------------------------------------------------------------------

/// What a thread has allocated so far: bytes and allocations, never decreasing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allocated {
    pub bytes: u64,
    pub count: u64,
}

/// The reader of the calling thread's [`Allocated`], from the global allocator's own
/// thread-local counts: given by the binary that installs that allocator, once.
static ALLOCATED: OnceLock<fn() -> Allocated> = OnceLock::new();

/// Has every span note what its thread allocated while it was open, read by `read` (the
/// global allocator's counts for the calling thread). Without it `alloc_bytes` and `allocs`
/// are null. Once a process; a second call is refused.
pub fn count_allocations(read: fn() -> Allocated) -> Result<(), fn() -> Allocated> {
    ALLOCATED.set(read)
}

#[inline]
fn allocated() -> Option<Allocated> {
    ALLOCATED.get().map(|read| read())
}

/// What the frames of a thread's ended tasks copied to grow blocks so far, never decreasing:
/// `blaze_mem::tag::copied_on_thread`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameCopied {
    pub copies: u64,
    pub bytes: u64,
}

/// The reader of the calling thread's [`FrameCopied`], given by the binary that links
/// blaze-mem, once.
static FRAME_COPIED: OnceLock<fn() -> FrameCopied> = OnceLock::new();

/// Has every span note what its thread's task frames copied growing blocks while it was open,
/// read by `read`. Without it `frame_grow_copies` and `frame_copied_bytes` are null. Once a
/// process; a second call is refused.
pub fn count_frame_copies(read: fn() -> FrameCopied) -> Result<(), fn() -> FrameCopied> {
    FRAME_COPIED.set(read)
}

#[inline]
fn frame_copied() -> FrameCopied {
    FRAME_COPIED.get().map_or_else(FrameCopied::default, |read| read())
}

/// The reader of the calling thread's CPU time in nanoseconds, given by the binary, once.
static THREAD_CPU: OnceLock<fn() -> Option<u64>> = OnceLock::new();

/// Has every span count its self CPU (`self_cpu_ns`): the CPU while it was the innermost span
/// its thread was inside, read by `read` (the calling thread's CPU time, `None` when it can't
/// be read). Without it `self_cpu_ns` is null. Each read is a system call (`clock_gettime` of
/// `CLOCK_THREAD_CPUTIME_ID`, about 100 ns), made only at a thread's boundaries (a switch, a
/// wait, the gaps between its root spans; [`crate::time::cpu`]), never per span: a span counts
/// its ticks and the reader turns them into CPU at its thread's next boundary. Once a process; a
/// second call is refused.
pub fn count_cpu(read: fn() -> Option<u64>) -> Result<(), fn() -> Option<u64>> {
    let scale = clock::Scale::now();
    for (ticks, of) in [
        (&CPU_AGE_TICKS, cpu::AGE),
        (&CPU_ROOT_AGE_TICKS, cpu::ROOT_AGE),
        (&CPU_GAP_TICKS, cpu::GAP),
        (&CPU_SLIVER_TICKS, cpu::SLIVER),
    ] {
        ticks.store(scale.ticks_in(of.as_nanos() as u64), Ordering::Relaxed);
    }
    THREAD_CPU.set(read)
}

/// Whether spans count their CPU ([`count_cpu`]).
#[inline]
fn counting() -> bool {
    THREAD_CPU.get().is_some()
}

/// [`cpu::AGE`], [`cpu::ROOT_AGE`], [`cpu::GAP`] and [`cpu::SLIVER`] in ticks, set by
/// [`count_cpu`].
static CPU_AGE_TICKS: AtomicU64 = AtomicU64::new(0);
static CPU_ROOT_AGE_TICKS: AtomicU64 = AtomicU64::new(0);
static CPU_GAP_TICKS: AtomicU64 = AtomicU64::new(0);
static CPU_SLIVER_TICKS: AtomicU64 = AtomicU64::new(0);

/// A slot's self CPU when it is not counted: the binary reads no CPU clock, or a read failed.
const UNCOUNTED_CPU: u64 = u64::MAX;
/// A ring's stretch with no reading to begin from.
const NO_CPU: u64 = u64::MAX;
/// Spans a thread left with ticks of its stretch and has not closed, kept to give them the
/// stretch's rate at its end ([`Ring::left`]); one more cuts the stretch.
const LEFT_MAX: usize = 16;

/// The plot byte of a boundary's row ([`boundary`]): words 0 and 1 the reading (ticks, CPU
/// nanoseconds), 2 and 3 the one its stretch began with; word 4 above the byte: why (bits
/// 8..16) and whether both readings were made ([`CPU_VALID`]). Each span row of the ring
/// before it with ticks waiting ([`cpu::share_word`]) takes its stretch's rate, and a switch
/// row right before it its reading.
const CPU_ROW: u64 = 0xf7;
const CPU_VALID: u64 = 1 << 16;
/// A boundary's row whose own reading (words 0 and 1) was made.
const CPU_READ: u64 = 1 << 17;

/// Why a thread's CPU stretch was cut ([`boundary`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum CpuWhy {
    Switch = 1,
    Wait,
    Gap,
    Root,
    Age,
    Claim,
    End,
    Mark,
    Read,
    Fold,
    Left,
    Park,
}

/// 1 in this many calls of the recorder's span paths is timed ([`Plot::ProfileCostNs`]),
/// drawn at random so no path is always the one timed.
pub const COST_SAMPLE: u64 = 64;

/// The ticks now when this call is one [`COST_SAMPLE`] times ([`cost_to`]), else 0.
#[inline]
fn cost_from(ring: &Ring) -> u64 {
    let mut x = ring.sample.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    ring.sample.store(x, Ordering::Relaxed);
    if x % COST_SAMPLE == 0 { clock::ticks() } else { 0 }
}

/// Adds the ticks since `from` ([`cost_from`]), for each of the [`COST_SAMPLE`] calls it stands
/// for, to the ring's cost.
#[inline]
fn cost_to(ring: &Ring, from: u64) {
    if from != 0 {
        let spent = clock::ticks().wrapping_sub(from).saturating_mul(COST_SAMPLE);
        ring.cost.store(ring.cost.load(Ordering::Relaxed).saturating_add(spent), Ordering::Relaxed);
    }
}

/// The ticks since `ring`'s thread's CPU stretch began.
#[inline]
fn stretch_age(ring: &Ring) -> u64 {
    clock::ticks().wrapping_sub(ring.stretch_ticks.load(Ordering::Relaxed))
}

/// Adds to the innermost span of `ring`'s thread the ticks it was innermost up to `now`, and
/// counts it from `now` on, so a second accrual (a boundary right after) adds none twice.
#[inline]
fn accrue(ring: &Ring, now: u64) {
    let since = ring.top_since.load(Ordering::Relaxed);
    let depth = ring.depth.load(Ordering::Relaxed) as usize;
    if since == 0 || depth == 0 || depth > OPEN_MAX {
        return;
    }
    if let Some(slot) = open_slot(ring.entered[depth - 1].load(Ordering::Relaxed)) {
        slot.pending.fetch_add(now.wrapping_sub(since), Ordering::Relaxed);
    }
    ring.top_since.store(now, Ordering::Relaxed);
}

/// The innermost span of `ring`'s thread, if one is named, counts its ticks from `now`.
#[inline]
fn counted_from(ring: &Ring, now: u64) {
    let depth = ring.depth.load(Ordering::Relaxed) as usize;
    ring.top_since.store(if (1..=OPEN_MAX).contains(&depth) { now } else { 0 }, Ordering::Relaxed);
}

/// Gives the open span `id` its ticks waiting, at `rate` (`None`: not counted).
fn resolve(id: u64, rate: Option<Rate>) {
    let Some(slot) = open_slot(id) else { return };
    let pending = slot.pending.swap(0, Ordering::Relaxed);
    let known = slot.cpu.load(Ordering::Relaxed);
    if pending == 0 || known == UNCOUNTED_CPU {
        return;
    }
    let known = match rate {
        Some(rate) => known.saturating_add(rate.cpu_of(pending)).min(UNCOUNTED_CPU - 1),
        None => UNCOUNTED_CPU,
    };
    slot.cpu.store(known, Ordering::Relaxed);
}

/// Cuts the CPU stretch of `ring`'s thread, the calling one: reads its CPU clock, writes the
/// reading's row ([`CPU_ROW`]) and gives every span with ticks of the stretch (those it is
/// inside, those it left open, its folded sums) the stretch's rate. Nothing when spans count no
/// CPU.
fn boundary(ring: &Ring, why: CpuWhy) {
    if !counting() {
        return;
    }
    let settled = FOLDS
        .try_with(|folds| match folds.try_borrow_mut() {
            Ok(mut folds) => {
                boundary_in(ring, why, Some(&mut *folds));
                true
            }
            Err(_) => false,
        })
        .unwrap_or(false);
    if !settled {
        // Its folded sums keep their ticks; written before a later boundary, their CPU says
        // not counted ([`put_fold`]).
        boundary_in(ring, why, None);
    }
}

/// [`boundary`], with the thread's folded sums `folds` when the caller holds them.
fn boundary_in(ring: &Ring, why: CpuWhy, folds: Option<&mut Folds>) {
    let Some(read) = THREAD_CPU.get() else { return };
    let before = clock::ticks();
    accrue(ring, before);
    let cpu = read();
    let after = clock::ticks();
    let at = before.wrapping_add(after.wrapping_sub(before) / 2);
    let (from_ticks, from_cpu) =
        (ring.stretch_ticks.load(Ordering::Relaxed), ring.stretch_cpu.load(Ordering::Relaxed));
    let rate = match cpu {
        Some(cpu) if from_cpu != NO_CPU => Rate::between(
            Reading { ticks: from_ticks, cpu_ns: from_cpu },
            Reading { ticks: at, cpu_ns: cpu },
        ),
        _ => None,
    };
    let valid =
        if rate.is_some() { CPU_VALID } else { 0 } | if cpu.is_some() { CPU_READ } else { 0 };
    let build = u64::from(BUILD.try_with(Cell::get).unwrap_or(0));
    let what = KIND_PLOT << 62 | valid | (why as u64) << 8 | CPU_ROW;
    ring.put([at, cpu.unwrap_or(0), from_ticks, from_cpu, what, 0, build, 0]);
    ring.stretch_ticks.store(at, Ordering::Relaxed);
    ring.stretch_cpu.store(cpu.unwrap_or(NO_CPU), Ordering::Relaxed);
    let depth = (ring.depth.load(Ordering::Relaxed) as usize).min(OPEN_MAX);
    for entered in &ring.entered[..depth] {
        resolve(entered.load(Ordering::Relaxed), rate);
    }
    let left = (ring.left_len.load(Ordering::Relaxed) as usize).min(LEFT_MAX);
    for id in &ring.left[..left] {
        resolve(id.load(Ordering::Relaxed), rate);
    }
    ring.left_len.store(0, Ordering::Relaxed);
    if let Some(folds) = folds {
        settle_folds(folds, rate);
    }
    let (rate_ticks, rate_cpu) = rate.map_or((0, 0), Rate::parts);
    ring.rate_ticks.store(rate_ticks, Ordering::Relaxed);
    ring.rate_cpu.store(rate_cpu, Ordering::Relaxed);
    // The read is the recorder's time, no span's.
    let end = clock::ticks();
    counted_from(ring, end);
    ring.cost.store(
        ring.cost.load(Ordering::Relaxed).saturating_add(end.wrapping_sub(before)),
        Ordering::Relaxed,
    );
    ring.cpu_reads.store(ring.cpu_reads.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
}

/// The spans of a parking job leave `ring`'s thread ([`take_open`]) with no ticks of its
/// stretch: just after the switch's boundary, the ticks since take that stretch's rate
/// ([`cpu::SLIVER`]); else (a job carried with no switch told) the stretch is cut here.
fn cpu_leave(ring: &Ring) {
    if !counting() || ring.depth.load(Ordering::Relaxed) == 0 {
        ring.top_since.store(0, Ordering::Relaxed);
        return;
    }
    let now = clock::ticks();
    let rate_ticks = ring.rate_ticks.load(Ordering::Relaxed);
    let fresh = now.wrapping_sub(ring.stretch_ticks.load(Ordering::Relaxed))
        < CPU_SLIVER_TICKS.load(Ordering::Relaxed);
    if fresh && rate_ticks != 0 {
        accrue(ring, now);
        let last = Reading { ticks: rate_ticks, cpu_ns: ring.rate_cpu.load(Ordering::Relaxed) };
        let rate = Rate::between(Reading { ticks: 0, cpu_ns: 0 }, last);
        let depth = (ring.depth.load(Ordering::Relaxed) as usize).min(OPEN_MAX);
        for entered in &ring.entered[..depth] {
            resolve(entered.load(Ordering::Relaxed), rate);
        }
    } else {
        boundary(ring, CpuWhy::Park);
    }
    ring.top_since.store(0, Ordering::Relaxed);
}

/// Notes that the span `id` left the stack of `ring`'s thread with ticks of its stretch: it
/// takes the stretch's rate at its end unless it closes first ([`closed`]). A full list cuts
/// the stretch first, the span still on the stack.
#[inline]
fn left_open(ring: &Ring, id: u64) {
    let len = (ring.left_len.load(Ordering::Relaxed) as usize).min(LEFT_MAX);
    if ring.left[..len].iter().any(|it| it.load(Ordering::Relaxed) == id) {
        return;
    }
    if len == LEFT_MAX {
        boundary(ring, CpuWhy::Left);
        return;
    }
    ring.left[len].store(id, Ordering::Relaxed);
    ring.left_len.store(len as u32 + 1, Ordering::Relaxed);
}

/// What the recorder cost so far, on every thread ([`cost`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    /// Nanoseconds spent making, entering, leaving and closing spans (sampled) and reading CPU
    /// clocks at boundaries.
    pub ns: u64,
    /// Spans made, recorded or lost.
    pub spans: u64,
    /// CPU clock reads.
    pub cpu_reads: u64,
}

/// What the recorder cost so far, on every thread: [`Plot::ProfileCostNs`],
/// [`Plot::ProfileSpansMade`] and [`Plot::ProfileCpuReads`]. What a span costs is `ns / spans`.
pub fn cost() -> Cost {
    let scale = clock::Scale::now();
    let rings = lock(&RINGS);
    let (mut ticks, mut spans, mut cpu_reads) = (0u64, RINGLESS.load(Ordering::Relaxed), 0u64);
    for ring in rings.iter() {
        ticks = ticks.saturating_add(ring.cost.load(Ordering::Relaxed));
        spans += ring.made.load(Ordering::Relaxed) + ring.lost.load(Ordering::Relaxed);
        cpu_reads += ring.cpu_reads.load(Ordering::Relaxed);
    }
    Cost { ns: scale.ns(ticks), spans, cpu_reads }
}

/// The plot byte of the row that follows a span's row when the span's frames copied: the
/// copies and bytes, and the span's id. A span closed on another thread than it began on has
/// one with `UNCOUNTED` set: the two threads' counts don't subtract.
const SPAN_COPIED_ROW: u64 = 0xfe;
/// The plot byte of the rows that follow a span's row when it outlived a CPU stretch (its self
/// CPU from the stretches that ended while it was open, [`crate::time::cpu`]) or a span kind
/// said its typed columns ([`span::TYPED_COLUMNS`]): up to [`COL_WORDS`] values a row, each
/// plus one, at the words `COL_WORDS` names, and the span's id at word 3. Word 4's bytes 1..6
/// say whose value each word holds: a typed column's index, [`CPU_COL`] for the span's self
/// CPU, [`THREAD_COL`] for the thread that made a span another closed, [`FOLDED_COL`] and
/// [`FOLDED_TICKS_COL`] for the children it folded, [`NO_COL`] for none.
const SPAN_COLS_ROW: u64 = 0xfa;
const COL_WORDS: [usize; 5] = [1, 2, 5, 6, 7];
/// The thread that made a span closed by another (on another ring, or on its ring after the
/// ring passed to the next thread), as [`Ring::current`] said it when the span began: its tid
/// << 32 | its name. The ring's rows name the thread that closed it.
const THREAD_COL: u64 = 0xfd;
const CPU_COL: u64 = 0xfe;
const NO_COL: u64 = 0xff;
/// How many of a span's children were folded ([`fold_below`]), and their ticks summed: said
/// only when some were.
const FOLDED_COL: u64 = 0xfc;
const FOLDED_TICKS_COL: u64 = 0xfb;
const _: () =
    assert!(TYPED < FOLDED_TICKS_COL as usize, "a typed column's index is a byte of its own");
const UNCOUNTED: u64 = 1;

const _: () = {
    let bytes = [
        FIBER_STACK_ROW,
        SPAN_COPIED_ROW,
        SWITCH_ROW,
        EVENT_ROW,
        EVENT_FIELDS_ROW,
        SPAN_COLS_ROW,
        FOLD_ROW,
        CPU_ROW,
        names::GEN_ROW,
        names::NAME_ROW,
        names::NAME_MORE_ROW,
        names::LINK_ROW,
    ];
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i] as usize >= Plot::ALL.len(), "a row's plot byte is no plot's");
        let mut j = i + 1;
        while j < bytes.len() {
            assert!(bytes[i] != bytes[j], "each kind of plot row has its own byte");
            j += 1;
        }
        i += 1;
    }
};

// --- Rings ------------------------------------------------------------------------------

/// Who wrote a ring from row `from` on: rings outlive threads and go to the next thread.
struct Owner {
    from: u64,
    tid: u32,
    name: Sym,
}

/// A span made and not closed: written by the thread that made it, read and freed by the one
/// that closes it. `id` is 0 while the slot is free, and published last with release.
struct Slot {
    id: AtomicU64,
    /// Handles of the span alive (`tracing` clones and closes each): it closes with the last.
    refs: AtomicU64,
    start: AtomicU64,
    parent: AtomicU64,
    /// Its site, depth and build.
    what: AtomicU64,
    /// Its crate, function and unit, as tokens ([`names::Token`]).
    krate: AtomicU64,
    function: AtomicU64,
    unit: AtomicU64,
    alloc_bytes: AtomicU64,
    allocs: AtomicU64,
    frame_copies: AtomicU64,
    frame_bytes: AtomicU64,
    /// The ring's [`Ring::lease`] when the span began: which thread's counts it began with.
    lease: AtomicU64,
    /// The thread that made it, as [`Ring::current`] said it then: the thread a row of it
    /// names when another thread closes it, by when the ring may have passed to the next.
    thread: AtomicU64,
    /// The words of its typed columns, as a span kind said them (0: not said).
    typed: [AtomicU64; TYPED],
    /// Its self CPU in nanoseconds from the stretches that ended while it was open
    /// ([`crate::time::cpu`]), [`UNCOUNTED_CPU`] when not counted; and the ticks it was the
    /// innermost span of the stretch now, which the stretch's end turns into CPU.
    cpu: AtomicU64,
    pending: AtomicU64,
    /// Its children open (the low half) and the rows that name it (the high half, a
    /// [`NAMED_ONE`] each: a child written whole, an event, a sample, a pool job it gave):
    /// a span with either is written whole as it closes, never folded ([`fold_below`]).
    kids: AtomicU64,
    /// Its children folded ([`fold_below`]) and their ticks summed: the time of its own the
    /// rows under it leave out, which its row says (`spans.folded_spans`, `folded_ns`) so its
    /// self time stays exact.
    folded: AtomicU64,
    folded_ticks: AtomicU64,
}

/// A row naming a span, in [`Slot::kids`].
const NAMED_ONE: u64 = 1 << 32;

impl Slot {
    const FREE: Slot = Slot {
        id: AtomicU64::new(0),
        refs: AtomicU64::new(0),
        start: AtomicU64::new(0),
        parent: AtomicU64::new(0),
        what: AtomicU64::new(0),
        krate: AtomicU64::new(0),
        function: AtomicU64::new(0),
        unit: AtomicU64::new(0),
        alloc_bytes: AtomicU64::new(0),
        allocs: AtomicU64::new(0),
        frame_copies: AtomicU64::new(0),
        frame_bytes: AtomicU64::new(0),
        lease: AtomicU64::new(0),
        thread: AtomicU64::new(0),
        typed: [const { AtomicU64::new(0) }; TYPED],
        cpu: AtomicU64::new(UNCOUNTED_CPU),
        pending: AtomicU64::new(0),
        kids: AtomicU64::new(0),
        folded: AtomicU64::new(0),
        folded_ticks: AtomicU64::new(0),
    };
}

/// What a slot held, read whole.
#[derive(Clone, Copy)]
struct Opened {
    start: u64,
    parent: u64,
    site: u16,
    depth: u16,
    build: u32,
    krate: Token,
    function: Token,
    unit: Token,
    alloc: Allocated,
    copied: FrameCopied,
    lease: u64,
    thread: u64,
    typed: [u64; TYPED],
    /// [`Slot::cpu`].
    cpu: u64,
    held: bool,
    /// [`Slot::kids`] as it closed.
    kids: u64,
    /// [`Slot::folded`] and [`Slot::folded_ticks`] as it closed.
    folded: u64,
    folded_ticks: u64,
}

impl Opened {
    /// Its columns, as a span kind reads them.
    fn said(&self) -> Said {
        Said { krate: self.krate, function: self.function, unit: self.unit, typed: self.typed }
    }
}

/// Owners of a ring a [`Store`] names, the latest: as many as a postmortem reads back.
const OWNERS_KEPT: usize = 64;
/// Bytes of a [`Store`] before its rows.
const STORE_HEAD: usize = 4096;
/// The slots of the history's marks a [`Store`] keeps ([`Store::marks`]): one the last commit
/// marked, one a flush may be marking as the process dies, and the one the next flush marks,
/// so a crash in the middle of marking never leaves a slot a postmortem reads half written.
pub(crate) const MARK_SLOTS: usize = 3;

/// A ring's rows and what a reader needs to read them: its head, the rows, and the threads
/// that wrote them. On the heap until the process's history persists the recorder; then in a
/// file the history's directory holds ([`postmortem`]), so the rows outlive a crash. Never
/// freed either way: a reader that loaded a ring's store before it moved reads the old one
/// whole.
#[repr(C)]
pub(crate) struct Store {
    /// Rows ever written.
    head: AtomicU64,
    /// The ring's number.
    number: AtomicU64,
    /// Of `owners`, how many are said.
    owners_len: AtomicU64,
    /// The history's marks in this ring, a pair a slot of the mapping's ([`MARK_SLOTS`],
    /// [`flush_pending`]): the index of the first row the flush did not take (every row before
    /// it is in its files, but the DEBUG rows it held), and the index of the first DEBUG row it
    /// held (`logs::Hold`), at most the first. A postmortem reads from the marks of the slot
    /// the manifest says committed. Each only grows, slot by slot.
    marks: [[AtomicU64; 2]; MARK_SLOTS],
    /// The last [`OWNERS_KEPT`] of the ring's owners ([`Owner`]): the row each wrote from, and
    /// its tid << 32 | its name. Written by [`claim`] beside the ring's `owners`, for a
    /// postmortem, which has no other way to say whose rows they are.
    owners: [[AtomicU64; 2]; OWNERS_KEPT],
    _pad: [u64; (STORE_HEAD - 24 - MARK_SLOTS * 16 - OWNERS_KEPT * 16) / 8],
    /// [`WORDS`] words a row.
    words: [AtomicU64; WORDS * CAP],
}

const _: () =
    assert!(std::mem::offset_of!(Store, words) == STORE_HEAD, "a store's rows start at STORE_HEAD");
const _: () = assert!(
    std::mem::align_of::<Store>() <= 8,
    "a store is placed at any 8-byte boundary of a mapping"
);

impl Store {
    /// A store on the heap, all zero: a page of its rows is committed only once rows reach
    /// it, so a thread that records little holds little.
    fn heap() -> &'static Store {
        // alloc: once per ring, its rows for the process's life (a ring that moves to the
        // mapping leaves it behind, for a reader that loaded it)
        let store = Box::<Store>::new_zeroed();
        // SAFETY: all-zero bytes are a valid `Store`: atomics and integers.
        Box::leak(unsafe { store.assume_init() })
    }

    /// The rows from index `from` that are whole now, each with its index, into `into`; the
    /// index to read from next.
    fn read(&self, from: u64, into: &mut Vec<(u64, [u64; WORDS])>) -> u64 {
        let head = self.head.load(Ordering::Acquire);
        let first = into.len();
        for i in from.max(head.saturating_sub(CAP as u64))..head {
            let at = (i as usize & (CAP - 1)) * WORDS;
            let mut row = [0; WORDS];
            for (value, word) in row.iter_mut().zip(&self.words[at..at + WORDS]) {
                *value = word.load(Ordering::Relaxed);
            }
            into.push((i, row));
        }
        fence(Ordering::Acquire);
        let whole_from = (self.head.load(Ordering::Relaxed) + 1).saturating_sub(CAP as u64);
        let mut kept = first;
        for i in first..into.len() {
            if into[i].0 >= whole_from {
                into.swap(kept, i);
                kept += 1;
            }
        }
        into.truncate(kept);
        head
    }

    /// The names rows ([`names`]) from index `from` that are whole now, each with its index,
    /// into `into`; the first index read and the one after the last, every row between them
    /// read whole ([`names::Source`]).
    fn read_names(&self, from: u64, into: &mut Vec<(u64, [u64; WORDS])>) -> (u64, u64) {
        let head = self.head.load(Ordering::Acquire);
        let first = from.max(head.saturating_sub(CAP as u64));
        let start = into.len();
        for i in first..head {
            let at = (i as usize & (CAP - 1)) * WORDS;
            let tag = self.words[at + 4].load(Ordering::Relaxed);
            if tag >> 62 != KIND_PLOT || !(names::LINK_ROW..=names::GEN_ROW).contains(&(tag & 0xff))
            {
                continue;
            }
            let mut row = [0; WORDS];
            for (value, word) in row.iter_mut().zip(&self.words[at..at + WORDS]) {
                *value = word.load(Ordering::Relaxed);
            }
            into.push((i, row));
        }
        fence(Ordering::Acquire);
        // Rows the writer was writing over as they were read are dropped, and so is the claim
        // that they were read: the first row read is the first one whole.
        let whole_from = (self.head.load(Ordering::Relaxed) + 1).saturating_sub(CAP as u64);
        let mut kept = start;
        for i in start..into.len() {
            if into[i].0 >= whole_from {
                into.swap(kept, i);
                kept += 1;
            }
        }
        into.truncate(kept);
        (first.max(whole_from.min(head)), head)
    }

    /// Copies `self` into `to`, which no one reads yet: rows, owners and the history's marks,
    /// then the head last, with release.
    fn copy_to(&self, to: &Store) {
        let head = self.head.load(Ordering::Acquire);
        let rows = usize::try_from(head).unwrap_or(usize::MAX).min(CAP);
        for (from, into) in self.words[..rows * WORDS].iter().zip(&to.words[..rows * WORDS]) {
            into.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        for (from, into) in self.owners.iter().zip(&to.owners) {
            into[0].store(from[0].load(Ordering::Relaxed), Ordering::Relaxed);
            into[1].store(from[1].load(Ordering::Relaxed), Ordering::Relaxed);
        }
        to.owners_len.store(self.owners_len.load(Ordering::Relaxed), Ordering::Relaxed);
        to.number.store(self.number.load(Ordering::Relaxed), Ordering::Relaxed);
        self.marks_to(to);
        to.head.store(head, Ordering::Release);
    }

    /// Raises each of `to`'s marks to `self`'s: what a mark written to `self` after a copy
    /// carries over ([`Ring::move_store`]).
    fn marks_to(&self, to: &Store) {
        for (from, into) in self.marks.iter().flatten().zip(to.marks.iter().flatten()) {
            into.fetch_max(from.load(Ordering::SeqCst), Ordering::SeqCst);
        }
    }

    /// Raises the marks of slot `slot` to `from` and `held_from`.
    fn mark(&self, slot: usize, from: u64, held_from: u64) {
        self.marks[slot][0].fetch_max(from, Ordering::SeqCst);
        self.marks[slot][1].fetch_max(held_from, Ordering::SeqCst);
    }

    /// Writes the last [`OWNERS_KEPT`] of `owners` for a postmortem.
    fn mirror(&self, owners: &[Owner]) {
        let kept = &owners[owners.len().saturating_sub(OWNERS_KEPT)..];
        for (owner, into) in kept.iter().zip(&self.owners) {
            into[0].store(owner.from, Ordering::Relaxed);
            into[1].store(u64::from(owner.tid) << 32 | u64::from(owner.name), Ordering::Relaxed);
        }
        self.owners_len.store(kept.len() as u64, Ordering::Release);
    }
}

/// Entries a ring's [`ContextLog`] keeps: a power of two. A thread changes what it is inside
/// a few hundred thousand times a second at the most, and the sampler reads every 20 ms, so
/// the log covers a perf sample's delay whenever the thread is not that busy; an older sample
/// reads span 0 and is counted ([`crate::sampler`]).
pub(crate) const CONTEXT_LOG: usize = 4096;
/// Words an entry holds: its ticks, the innermost span, the span under it on the enter stack,
/// the job and the fiber.
const CONTEXT_WORDS: usize = 5;

/// A ring's context log: a fixed ring of what its thread was inside, an entry at every change
/// of its innermost span (a span entered or left, a fiber's spans moved off the thread as it
/// parks and back as it resumes) and of its job or fiber, so a sample whose time the kernel
/// gives later (Linux's perf) is told the span, job and fiber in force at that time.
///
/// Written by the ring's thread alone, atomics only, as [`Ring::put`] writes rows: a release
/// fence, the entry's words, then `head` with release. A reader loads `head` (acquire), reads
/// the entries it needs, then an acquire fence and `head` again: an entry `j` it read is whole
/// when `j + CONTEXT_LOG > head` then, since the writer stores the head that names the write
/// over `j` before its fence and that write's words, so a reader that saw any of them sees
/// that head too. An entry written over as it was read is dropped, never misread.
pub(crate) struct ContextLog {
    head: AtomicU64,
    words: Box<[AtomicU64]>,
}

impl ContextLog {
    pub(crate) fn new() -> ContextLog {
        // alloc: once per ring, 160 KiB, kept for the process's life as the ring is
        let words = std::iter::repeat_with(|| AtomicU64::new(0))
            .take(CONTEXT_LOG * CONTEXT_WORDS)
            // alloc: as above
            .collect();
        ContextLog { head: AtomicU64::new(0), words }
    }

    /// Writes `entry` as the log's next, over the oldest once [`CONTEXT_LOG`] are written.
    #[inline]
    pub(crate) fn put(&self, entry: [u64; CONTEXT_WORDS]) {
        let i = self.head.load(Ordering::Relaxed);
        let at = (i as usize & (CONTEXT_LOG - 1)) * CONTEXT_WORDS;
        fence(Ordering::Release);
        for (word, value) in self.words[at..at + CONTEXT_WORDS].iter().zip(entry) {
            word.store(value, Ordering::Relaxed);
        }
        self.head.store(i + 1, Ordering::Release);
    }

    /// Entry number `i`, as it reads now: whole only if [`ContextLog::whole`] says so after.
    fn entry(&self, i: u64) -> [u64; CONTEXT_WORDS] {
        let at = (i as usize & (CONTEXT_LOG - 1)) * CONTEXT_WORDS;
        std::array::from_fn(|w| self.words[at + w].load(Ordering::Relaxed))
    }

    /// Whether entry `i` and every later one read before this call were whole: none was
    /// written over while it was read.
    fn whole(&self, i: u64) -> bool {
        fence(Ordering::Acquire);
        i + CONTEXT_LOG as u64 > self.head.load(Ordering::Relaxed)
    }

    /// The span, job and fiber in force at `ticks`: those of the last entry at or before it.
    /// A span `is_open` says closed gives way to the span under it, and so on down, each
    /// one's own parent read from the latest entry before that names it innermost; 0 when
    /// none is open or the log no longer holds the entry. `None` for a moment older than the
    /// oldest entry, or an entry written over as it was read. Bounded: every step down reads
    /// only entries older than the step before, at most [`CONTEXT_LOG`] in all.
    pub(crate) fn at(&self, ticks: u64, is_open: impl Fn(u64) -> bool) -> Option<(u64, u64, u32)> {
        let end = self.head.load(Ordering::Acquire);
        // One short of a full log: the oldest slot is the next one written.
        let first = end.saturating_sub(CONTEXT_LOG as u64 - 1);
        // The entries' ticks go up (one thread writes them, on a clock that only goes
        // forward): the first entry past `ticks`, by halves.
        let (mut lo, mut hi) = (first, end);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.entry(mid)[0] <= ticks {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let matched = lo.checked_sub(1).filter(|it| *it >= first)?;
        let [at, span, parent, job, fiber] = self.entry(matched);
        if at > ticks {
            return None;
        }
        let (mut span, mut parent, mut low) = (span, parent, matched);
        while span != 0 && !is_open(span) {
            if parent == 0 || is_open(parent) {
                span = parent;
                break;
            }
            // The parent closed too: its own parent is in the latest entry naming it
            // innermost, older than this one.
            match (first..low).rev().find(|&i| self.entry(i)[1] == parent) {
                Some(i) => {
                    (span, parent, low) = (parent, self.entry(i)[2], i);
                }
                None => {
                    span = 0;
                    low = first;
                }
            }
        }
        self.whole(low).then_some((span, job, fiber as u32))
    }
}

pub(crate) struct Ring {
    /// Its number, in span ids and in [`BY_NUMBER`].
    number: u16,
    /// Its rows: written by its thread alone, read by any ([`Store`]).
    store: AtomicPtr<Store>,
    /// A store to move to, set by [`persist`] while a thread holds the ring: the ring's thread
    /// moves its rows there as it next writes one ([`Ring::move_store`]), so only the writer
    /// writes a ring. Null for none.
    moving: AtomicPtr<Store>,
    /// Spans ever closed and work noted ([`moved`]): what [`progress`] sums.
    spans: AtomicU64,
    /// Spans its thread made and could not record: past [`SLOTS_OPEN`] open at once.
    lost: AtomicU64,
    /// Spans its threads made, for their ids.
    made: AtomicU64,
    /// Spans the thread is inside, the first [`OPEN_MAX`] of them in `entered`.
    depth: AtomicU32,
    entered: [AtomicU64; OPEN_MAX],
    /// The slots of the spans its thread made and has not closed, [`CHUNK`] a chunk: null
    /// until the thread first needs one there, then never freed (rings live as long as the
    /// process), so a published chunk may be read with no lock.
    open: [AtomicPtr<Slot>; SLOTS_OPEN / CHUNK],
    /// Which slots hold a span, a bit each: set by the ring's thread as it takes one, cleared
    /// by the thread that closes the span, after it freed the slot.
    taken: [AtomicU64; SLOTS_OPEN / 64],
    /// Whether a live thread holds it.
    live: AtomicBool,
    /// Threads that have held it: a span closed by the thread that began it closes on the
    /// same ring with the same lease. A ring an ended thread left passes to the next thread,
    /// so the number alone does not say which thread.
    lease: AtomicU64,
    /// The thread that holds it now or held it last: its tid and name, as [`Owner`] has them,
    /// read with no lock by a reader holding another ring's `owners`.
    current: AtomicU64,
    /// What takes longest on this ring's threads: its sketches ([`crate::hot`]), written as
    /// each span closes here.
    hot: Box<[AtomicU64]>,
    owners: Lock<Vec<Owner>>,
    /// What the sampler needs of the thread, under the lock the thread takes to leave.
    pub(crate) sampled: Lock<crate::sampler::Thread>,
    /// What its thread was inside, moment by moment: the sampler's way from a sample's time
    /// to its span, job and fiber ([`Ring::context_at`]).
    context: ContextLog,
    /// The job and fiber running on its thread now, 0 for none: written by its thread at each
    /// switch, read by the macOS sampler while the thread is stopped ([`Ring::running_now`]).
    job: AtomicU64,
    fiber: AtomicU64,
    /// Its thread's CPU accounting ([`crate::time::cpu`]), written by that thread alone: when
    /// its innermost span began counting ticks (0: none counts); the reading its stretch began
    /// with (CPU [`NO_CPU`]: none); the rate of the stretch before (ticks 0: none); when it last
    /// left its last span; and the spans it left with ticks of the stretch, not closed yet.
    ///
    /// Here and not in the [`Store`]: only the writer reads them. Every boundary writes its
    /// readings as a [`CPU_ROW`] in the rows, which is all the decoder needs to give a span its
    /// CPU ([`decode`]), so a postmortem reads a dead thread's spans without them; a span whose
    /// stretch the crash cut before its boundary row reads as not counted.
    top_since: AtomicU64,
    stretch_ticks: AtomicU64,
    stretch_cpu: AtomicU64,
    rate_ticks: AtomicU64,
    rate_cpu: AtomicU64,
    idle_since: AtomicU64,
    left: [AtomicU64; LEFT_MAX],
    left_len: AtomicU32,
    /// What recording cost its threads ([`cost`]): ticks, CPU reads, and the state of the draw
    /// of the calls timed ([`cost_from`]).
    cost: AtomicU64,
    cpu_reads: AtomicU64,
    sample: AtomicU64,
    /// What its threads' names wrote ([`names`]), each count by its thread alone.
    names: RingNames,
}

impl Ring {
    fn new(number: u16) -> Ring {
        Ring::with_store(number, Store::heap())
    }

    /// A ring whose rows go to `store`, which no other ring has.
    fn with_store(number: u16, store: &'static Store) -> Ring {
        store.number.store(u64::from(number), Ordering::Relaxed);
        Ring {
            number,
            store: AtomicPtr::new(std::ptr::from_ref(store).cast_mut()),
            moving: AtomicPtr::new(std::ptr::null_mut()),
            spans: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            made: AtomicU64::new(0),
            depth: AtomicU32::new(0),
            entered: [const { AtomicU64::new(0) }; OPEN_MAX],
            open: [const { AtomicPtr::new(std::ptr::null_mut()) }; SLOTS_OPEN / CHUNK],
            taken: [const { AtomicU64::new(0) }; SLOTS_OPEN / 64],
            live: AtomicBool::new(true),
            lease: AtomicU64::new(0),
            current: AtomicU64::new(0),
            hot: crate::hot::record::cells(),
            // alloc: once per thread, as it first records
            owners: Lock::unrecorded("profile.flight.owners", Vec::new()),
            sampled: Lock::unrecorded("profile.flight.sampled", crate::sampler::Thread::default()),
            context: ContextLog::new(),
            job: AtomicU64::new(0),
            fiber: AtomicU64::new(0),
            top_since: AtomicU64::new(0),
            stretch_ticks: AtomicU64::new(0),
            stretch_cpu: AtomicU64::new(NO_CPU),
            rate_ticks: AtomicU64::new(0),
            rate_cpu: AtomicU64::new(0),
            idle_since: AtomicU64::new(0),
            left: [const { AtomicU64::new(0) }; LEFT_MAX],
            left_len: AtomicU32::new(0),
            cost: AtomicU64::new(0),
            cpu_reads: AtomicU64::new(0),
            // Any odd seed: xorshift never leaves a nonzero state.
            sample: AtomicU64::new(0x9E37_79B9_7F4A_7C15 ^ u64::from(number) << 1 | 1),
            names: RingNames::default(),
        }
    }

    /// Its store, loaded sequentially consistent: as the history marks it ([`flush_pending`]),
    /// against a move ([`Ring::move_store`]).
    fn store_seq(&self) -> &'static Store {
        // SAFETY: as in `Ring::store`.
        unsafe { &*self.store.load(Ordering::SeqCst) }
    }

    /// Its store, as a reader loads it.
    #[inline]
    fn store(&self) -> &'static Store {
        // SAFETY: `store` always holds a store made by `Store::heap` or by the mapping
        // (`postmortem::Mapping::store`), published with release; neither is ever freed or
        // unmapped (both live as long as the process), and a store is only written through
        // atomics.
        unsafe { &*self.store.load(Ordering::Acquire) }
    }

    /// Its store, as its writer loads it: the thread that holds the ring set it itself
    /// ([`Ring::move_store`]) or took the ring after it was set, under [`RINGS`]'s lock.
    #[inline]
    fn store_mine(&self) -> &'static Store {
        // SAFETY: as in `Ring::store`; the writer's own earlier store, or the lock it took
        // the ring under, orders the pointer's write before this load.
        unsafe { &*self.store.load(Ordering::Relaxed) }
    }

    /// Writes `row` as the ring's next: its place in the ring, counted from the ring's making
    /// (a move to the mapping keeps the head, so the place stays the same across it).
    #[inline]
    fn put(&self, row: [u64; WORDS]) -> u64 {
        if !self.moving.load(Ordering::Relaxed).is_null() {
            self.move_store();
        }
        let store = self.store_mine();
        let i = store.head.load(Ordering::Relaxed);
        let at = (i as usize & (CAP - 1)) * WORDS;
        fence(Ordering::Release);
        for (word, value) in store.words[at..at + WORDS].iter().zip(row) {
            word.store(value, Ordering::Relaxed);
        }
        store.head.store(i + 1, Ordering::Release);
        i
    }

    /// Moves the ring's rows to the store [`persist`] left in `moving`: by the ring's writer,
    /// once, as it next writes a row. A reader that loaded the old store reads it whole; the
    /// next read loads the new one, at the same head.
    #[cold]
    #[inline(never)]
    fn move_store(&self) {
        let to = self.moving.swap(std::ptr::null_mut(), Ordering::Acquire);
        if to.is_null() {
            return;
        }
        // SAFETY: `to` is a mapped store `persist` made for this ring alone and published
        // with release (the acquire swap above pairs with it); it is never unmapped.
        let to: &'static Store = unsafe { &*to };
        let from = self.store_mine();
        from.copy_to(to);
        self.store.store(std::ptr::from_ref(to).cast_mut(), Ordering::SeqCst);
        // A flush that loaded the old store before the swap may have marked it after the copy:
        // its marks are raised here once more. Both sides are sequentially consistent, a store
        // then a load each ([`mark`]): the flush sees the new store and marks it too, or this
        // sees its mark, or both. Until 2026-11-04 the mark was copied once, and a flush racing
        // the move left it in the abandoned store, so a postmortem wrote its rows again.
        from.marks_to(to);
    }

    /// The rows from index `from` that are whole now, each with its index, into `into`; the
    /// index to read from next.
    fn read(&self, from: u64, into: &mut Vec<(u64, [u64; WORDS])>) -> u64 {
        self.store().read(from, into)
    }

    /// Takes a free slot for a span its thread makes, the lowest free one, allocating its
    /// chunk when it has none yet; `None` once all [`SLOTS_OPEN`] hold a span. Only the ring's
    /// thread takes slots; any thread frees them ([`Ring::free`]).
    #[inline]
    fn take_slot(&self) -> Option<usize> {
        let (word_at, bits) = self
            .taken
            .iter()
            .enumerate()
            .map(|(at, it)| (at, it.load(Ordering::Acquire)))
            .find(|(_, it)| *it != u64::MAX)?;
        let bit = (!bits).trailing_zeros() as usize;
        let at = word_at * 64 + bit;
        let chunk = &self.open[at / CHUNK];
        if chunk.load(Ordering::Acquire).is_null() {
            // alloc: once per CHUNK slots a thread first holds open at once, for the process's life
            let slots: Box<[Slot]> = std::iter::repeat_with(|| Slot::FREE).take(CHUNK).collect();
            chunk.store(Box::leak(slots).as_mut_ptr(), Ordering::Release);
        }
        self.taken[word_at].fetch_or(1 << bit, Ordering::Relaxed);
        Some(at)
    }

    /// Slot `at`, once its chunk is allocated.
    #[inline]
    fn slot(&self, at: usize) -> Option<&Slot> {
        let chunk = self.open.get(at / CHUNK)?.load(Ordering::Acquire);
        // SAFETY: a non-null chunk was published with release by `take_slot` after it was
        // made whole: `CHUNK` slots, leaked, so it lives as long as the ring and is never
        // written but through atomics; `at % CHUNK` is in it.
        (!chunk.is_null()).then(|| unsafe { &*chunk.add(at % CHUNK) })
    }

    /// The slot of span `id`, whether or not `id` is still in it.
    #[inline]
    fn slot_of(&self, id: u64) -> Option<&Slot> {
        self.slot(slot_of(id))
    }

    /// Frees the slot of span `id`, which held it: any thread may.
    #[inline]
    fn free(&self, slot: &Slot, id: u64) {
        slot.id.store(0, Ordering::Release);
        let at = slot_of(id);
        self.taken[at / 64].fetch_and(!(1 << (at % 64)), Ordering::Release);
    }

    /// The ring names (not kept ones) the spans open in its slots hold now, into `into`: what
    /// a reader keeps the text of while they do ([`names::Streams::hold`]). A word of a typed
    /// column that only looks like a token keeps the text of the name it would be, harmless.
    fn held_tokens(&self, into: &mut Vec<Token>) {
        for (word, bits) in self.taken.iter().enumerate() {
            let mut bits = bits.load(Ordering::Acquire);
            while bits != 0 {
                let at = word * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let Some(slot) = self.slot(at) else { continue };
                for it in [&slot.krate, &slot.function, &slot.unit].into_iter().chain(&slot.typed) {
                    let token = it.load(Ordering::Relaxed);
                    if names::parts(token).is_some() {
                        into.push(token);
                    }
                }
            }
        }
    }

    /// What the slot of span `id` holds, while `id` is in it.
    fn opened(&self, id: u64) -> Option<Opened> {
        let slot = self.slot_of(id)?;
        if slot.id.load(Ordering::Acquire) != id {
            return None;
        }
        let what = slot.what.load(Ordering::Relaxed);
        let opened = Opened {
            start: slot.start.load(Ordering::Relaxed),
            parent: slot.parent.load(Ordering::Relaxed),
            site: (what & ((1 << SITE_BITS) - 1)) as u16,
            depth: (what >> 16 & 0xffff) as u16,
            build: (what >> 32) as u32,
            krate: slot.krate.load(Ordering::Relaxed),
            function: slot.function.load(Ordering::Relaxed),
            unit: slot.unit.load(Ordering::Relaxed),
            alloc: Allocated {
                bytes: slot.alloc_bytes.load(Ordering::Relaxed),
                count: slot.allocs.load(Ordering::Relaxed),
            },
            copied: FrameCopied {
                copies: slot.frame_copies.load(Ordering::Relaxed),
                bytes: slot.frame_bytes.load(Ordering::Relaxed),
            },
            lease: slot.lease.load(Ordering::Relaxed),
            thread: slot.thread.load(Ordering::Relaxed),
            typed: std::array::from_fn(|it| slot.typed[it].load(Ordering::Relaxed)),
            cpu: slot.cpu.load(Ordering::Relaxed),
            held: what & HELD_BIT != 0,
            kids: slot.kids.load(Ordering::Relaxed),
            folded: slot.folded.load(Ordering::Relaxed),
            folded_ticks: slot.folded_ticks.load(Ordering::Relaxed),
        };
        fence(Ordering::Acquire);
        (slot.id.load(Ordering::Relaxed) == id).then_some(opened)
    }

    /// The span ids the thread is inside, outermost first, and how many there are.
    fn entered_now(&self) -> (Vec<u64>, u32) {
        let depth = self.depth.load(Ordering::Acquire);
        let named = (depth as usize).min(OPEN_MAX);
        // alloc: a dump, on demand
        (self.entered[..named].iter().map(|it| it.load(Ordering::Relaxed)).collect(), depth)
    }

    /// The innermost span its thread is inside now, 0 for none: atomics only, so the sampler
    /// may read it while the thread is stopped.
    pub(crate) fn innermost(&self) -> u64 {
        let depth = self.depth.load(Ordering::Acquire) as usize;
        depth
            .checked_sub(1)
            .and_then(|top| self.entered.get(top))
            .map_or(0, |it| it.load(Ordering::Relaxed))
    }

    /// The job and fiber running on its thread now (0: none): atomics only, so the sampler
    /// may read them while the thread is stopped.
    #[cfg(target_vendor = "apple")]
    pub(crate) fn running_now(&self) -> (u64, u32) {
        (self.job.load(Ordering::Relaxed), self.fiber.load(Ordering::Relaxed) as u32)
    }

    /// Notes in the context log what its thread is inside now: its innermost span (and that
    /// span's place under the one below it), its job and its fiber. By the ring's thread alone,
    /// at every change of any of them.
    #[inline]
    fn note_context(&self) {
        let depth = self.depth.load(Ordering::Relaxed) as usize;
        let at = |i: Option<usize>| {
            i.and_then(|it| self.entered.get(it)).map_or(0, |it| it.load(Ordering::Relaxed))
        };
        self.context.put([
            clock::ticks(),
            at(depth.checked_sub(1)),
            at(depth.checked_sub(2)),
            self.job.load(Ordering::Relaxed),
            self.fiber.load(Ordering::Relaxed),
        ]);
    }

    /// Sets the job and fiber running on its thread, and notes them: by the ring's thread.
    #[inline]
    fn set_running(&self, job: u64, fiber: u32) {
        self.job.store(job, Ordering::Relaxed);
        self.fiber.store(u64::from(fiber), Ordering::Relaxed);
        self.note_context();
    }

    /// The span, job and fiber its thread was inside at `ticks` ([`clock::ticks`]), from its
    /// context log: `None` for a moment older than the log's oldest entry, or one whose
    /// entries the thread wrote over as they were read. A span that has closed since (and may
    /// have been folded) gives way to its innermost ancestor still open; the span given is
    /// kept, so it is written whole as it closes, never folded ([`keep`]).
    pub(crate) fn context_at(&self, ticks: u64) -> Option<(u64, u64, u32)> {
        let (span, job, fiber) = self.context.at(ticks, |id| open_slot(id).is_some())?;
        keep(span);
        Some((span, job, fiber))
    }

    /// The tid and name of [`Ring::current`].
    fn current(&self) -> (u32, Sym) {
        let current = self.current.load(Ordering::Acquire);
        ((current >> 32) as u32, current as u32)
    }

    /// The thread that wrote row `i`, of those in `owners`.
    fn owner(owners: &[Owner], i: u64) -> Option<&Owner> {
        let at = owners.partition_point(|it| it.from <= i);
        owners.get(at.checked_sub(1)?)
    }
}

fn ring_of(id: u64) -> usize {
    (id >> (SLOT_BITS + SEQ_BITS)) as usize
}

fn slot_of(id: u64) -> usize {
    (id >> SEQ_BITS) as usize & ((1 << SLOT_BITS) - 1)
}

/// Every ring, in the order made: a ring's number is its place here plus one.
// alloc: once a process
static RINGS: Lock<Vec<&'static Ring>> = Lock::unrecorded("profile.flight.rings", Vec::new());
/// Every ring by its number, read with no lock by a span closed on another thread.
static BY_NUMBER: [OnceLock<&'static Ring>; RINGS_MAX] = [const { OnceLock::new() }; RINGS_MAX];
/// Spans made on threads past [`RINGS_MAX`] alive at once, so with no ring: not recorded.
static RINGLESS: AtomicU64 = AtomicU64::new(0);

fn by_number(number: usize) -> Option<&'static Ring> {
    BY_NUMBER.get(number.checked_sub(1)?)?.get().copied()
}

/// Gives the ring back when its thread ends.
struct Held(Cell<Option<&'static Ring>>);

impl Drop for Held {
    fn drop(&mut self) {
        _ = MINE.try_with(|it| it.set(None));
        if let Some(ring) = self.0.take() {
            // Its last stretch's rate, for the spans it closed and the sums it folded.
            boundary(ring, CpuWhy::End);
            put_folds(ring);
            let mut sampled = lock(&ring.sampled);
            sampled.leave();
            ring.depth.store(0, Ordering::Relaxed);
            ring.set_running(0, 0);
            ring.live.store(false, Ordering::Release);
            drop(sampled);
        }
    }
}

thread_local! {
    /// This thread's ring, read by every span: no destructor, so it is there to the end.
    static MINE: Cell<Option<&'static Ring>> = const { Cell::new(None) };
    static HELD: Held = const { Held(Cell::new(None)) };
    /// Where a field's value that is not a string is written before it is interned.
    // alloc: once per thread, grown to its longest field
    static TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// The innermost span this thread is inside now, `None` for none or a thread with no ring yet:
/// atomics only, it neither allocates nor claims a ring, so an allocator hook may call it.
#[inline]
pub fn innermost_span_id() -> Option<u64> {
    let ring = MINE.try_with(Cell::get).ok().flatten()?;
    Some(ring.innermost()).filter(|&id| id != 0)
}

#[inline]
pub(crate) fn ring() -> Option<&'static Ring> {
    match MINE.with(Cell::get) {
        Some(ring) => Some(ring),
        None => claim(),
    }
}

/// Takes a ring a thread left, or makes one, for this thread. `None` once the thread is
/// ending past the point its ring was given back, or past [`RINGS_MAX`] threads at once.
#[cold]
fn claim() -> Option<&'static Ring> {
    HELD.try_with(|held| {
        let tid = crate::sampler::os_tid() as u32;
        let current = std::thread::current();
        let name = match current.name() {
            Some(name) => intern_kept(name),
            None => {
                // alloc: a thread's name, once as it first records
                let mut text = String::new();
                _ = write!(text, "thread {tid}");
                intern_kept(&text)
            }
        };
        let mut rings = lock(&RINGS);
        let left = rings.iter().copied().find(|it| {
            it.live.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        });
        let ring: &'static Ring = match left {
            Some(ring) => ring,
            None if rings.len() < RINGS_MAX => {
                let number = rings.len() as u16 + 1;
                // In the mapping once the history persisted the recorder ([`persist`]), and
                // never on the heap after that: past the mapping's rings, no ring.
                let ring = match postmortem::store_for(number) {
                    postmortem::StoreFor::Heap => Ring::new(number),
                    postmortem::StoreFor::Mapped(store) => Ring::with_store(number, store),
                    postmortem::StoreFor::Full => return None,
                };
                // alloc: once per thread past those that ended, kept for the process's life
                let ring: &'static Ring = Box::leak(Box::new(ring));
                rings.push(ring);
                _ = BY_NUMBER[usize::from(number) - 1].set(ring);
                ring
            }
            None => return None,
        };
        drop(rings);
        // A store `persist` left for the ring's last thread, which ended before it wrote again.
        ring.move_store();
        let store = ring.store_mine();
        let head = store.head.load(Ordering::Relaxed);
        let mut owners = lock(&ring.owners);
        owners.push(Owner { from: head, tid, name });
        // An owner whose rows were all written over names none.
        let oldest = head.saturating_sub(CAP as u64);
        while owners.len() > 1 && owners[1].from <= oldest {
            owners.remove(0);
        }
        store.mirror(&owners);
        drop(owners);
        ring.lease.fetch_add(1, Ordering::Relaxed);
        ring.current.store(u64::from(tid) << 32 | u64::from(name), Ordering::Release);
        // The lock module's wait-for graph names the thread as its rows do, so a stall's threads
        // are the profile's (`locks::Recorder::thread`).
        blaze_core::lock::set_thread_mark(u64::from(tid));
        ring.depth.store(0, Ordering::Relaxed);
        // The thread before left its own stretch, ended by its last boundary.
        ring.top_since.store(0, Ordering::Relaxed);
        ring.stretch_cpu.store(NO_CPU, Ordering::Relaxed);
        ring.rate_ticks.store(0, Ordering::Relaxed);
        ring.idle_since.store(0, Ordering::Relaxed);
        ring.left_len.store(0, Ordering::Relaxed);
        // A new thread, inside nothing and running no job: the log's entries from here are
        // its own.
        ring.set_running(0, 0);
        lock(&ring.sampled).enter(u64::from(tid));
        held.0.set(Some(ring));
        MINE.with(|it| it.set(Some(ring)));
        // The thread's first stretch begins.
        boundary(ring, CpuWhy::Claim);
        Some(ring)
    })
    .ok()
    .flatten()
}

// --- What spans do ----------------------------------------------------------------------

/// The generic columns of an ad hoc span, from its fields, as it is made: those of
/// [`Site::fields`].
struct Fields {
    wanted: u8,
    said: Said,
}

impl Fields {
    /// `field` is one bit of [`Site::fields`].
    fn set(&mut self, field: u8, value: &str) {
        self.said.set(field.trailing_zeros() as usize, intern(value));
    }
}

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        let field = field_of(field.name()) & self.wanted;
        if field != 0 {
            self.set(field, value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let field = field_of(field.name()) & self.wanted;
        if field == 0 {
            return;
        }
        _ = TEXT.try_with(|text| {
            let mut text = text.borrow_mut();
            text.clear();
            _ = write!(text, "{value:?}");
            self.set(field, &text);
        });
    }
}

/// Makes the ad hoc span `span` of `site` beginning now on this thread: its id, or
/// [`UNRECORDED`]. Its fields fill the generic columns its site names.
#[inline]
pub(crate) fn opened(span: &tracing::span::Attributes<'_>, site: u16) -> u64 {
    let wanted = SITES[usize::from(site) - 1].get().map_or(0, |it| it.fields);
    let mut fields = Fields { wanted, said: Said::EMPTY };
    if wanted != 0 {
        span.record(&mut fields);
    }
    let parent =
        if span.is_root() { Some(0) } else { span.parent().map(tracing::span::Id::into_u64) };
    began(site, parent, &fields.said)
}

/// Makes a span of a kind ([`crate::span`]) of `site` beginning now on this thread, its columns
/// `said`, and enters it: its id, or [`UNRECORDED`].
#[inline]
pub(crate) fn open_said(site: u16, said: &Said) -> u64 {
    let id = began(site, None, said);
    entered(id);
    id
}

/// Makes a span of `site` beginning now, its columns `said`, under no span and entered on no
/// thread's stack ([`crate::span::Detached`]): its id, for [`closed`] on whichever thread its
/// wait ends on, which writes its row there.
pub(crate) fn open_detached(site: u16, said: &Said) -> u64 {
    began(site, Some(0), said)
}

/// Makes a span of `site` beginning now on this thread, its columns `said`, under `parent`
/// (`Some(0)`: a root), or under the innermost span the thread is inside when `None`.
#[inline]
fn began(site: u16, parent: Option<u64>, said: &Said) -> u64 {
    let Some(ring) = ring() else {
        RINGLESS.fetch_add(1, Ordering::Relaxed);
        return UNRECORDED;
    };
    let cost = cost_from(ring);
    // A wait opening cuts the thread's CPU stretch: what it waits through (blocked or parked)
    // is a stretch of its own, never the work before it's.
    let counts = counting();
    if counts
        && Some(site) == <span::Wait as span::Span>::site()
        && stretch_age(ring) >= CPU_SLIVER_TICKS.load(Ordering::Relaxed)
    {
        boundary(ring, CpuWhy::Wait);
    }
    let depth = ring.depth.load(Ordering::Relaxed);
    let Some(at) = ring.take_slot() else {
        ring.lost.store(ring.lost.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        return UNRECORDED;
    };
    let parent = parent.unwrap_or_else(|| {
        depth
            .checked_sub(1)
            .and_then(|top| ring.entered.get(top as usize))
            .map_or(0, |it| it.load(Ordering::Relaxed))
    });
    let held = SITES[usize::from(site) - 1].get().is_some_and(|it| it.held);
    let made = ring.made.load(Ordering::Relaxed) + 1;
    ring.made.store(made, Ordering::Relaxed);
    let id = u64::from(ring.number) << (SLOT_BITS + SEQ_BITS)
        | (at as u64) << SEQ_BITS
        | made & SEQ_MASK;
    let Some(slot) = ring.slot(at) else {
        // `take_slot` made its chunk: unreachable, but a lost span is not a panic.
        ring.lost.store(ring.lost.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        return UNRECORDED;
    };
    let build = BUILD.with(Cell::get);
    let alloc = allocated().unwrap_or_default();
    let copied = frame_copied();
    slot.start.store(clock::ticks(), Ordering::Relaxed);
    slot.parent.store(parent, Ordering::Relaxed);
    let held_bit = if held { HELD_BIT } else { 0 };
    slot.what.store(
        u64::from(build) << 32 | u64::from(depth.min(0xffff)) << 16 | held_bit | u64::from(site),
        Ordering::Relaxed,
    );
    store_said(slot, said);
    // It counts CPU only while it is the innermost span entered ([`entered`]).
    slot.cpu.store(if counts { 0 } else { UNCOUNTED_CPU }, Ordering::Relaxed);
    slot.pending.store(0, Ordering::Relaxed);
    slot.kids.store(0, Ordering::Relaxed);
    slot.folded.store(0, Ordering::Relaxed);
    slot.folded_ticks.store(0, Ordering::Relaxed);
    slot.alloc_bytes.store(alloc.bytes, Ordering::Relaxed);
    slot.allocs.store(alloc.count, Ordering::Relaxed);
    slot.frame_copies.store(copied.copies, Ordering::Relaxed);
    slot.frame_bytes.store(copied.bytes, Ordering::Relaxed);
    slot.lease.store(ring.lease.load(Ordering::Relaxed), Ordering::Relaxed);
    // Written by `claim` on this thread before it made any span.
    slot.thread.store(ring.current.load(Ordering::Relaxed), Ordering::Relaxed);
    slot.refs.store(1, Ordering::Relaxed);
    slot.id.store(id, Ordering::Release);
    if let Some(parent) = open_slot(parent) {
        parent.kids.fetch_add(1, Ordering::Relaxed);
    }
    cost_to(ring, cost);
    id
}

/// The slot of the span `id` while it is open there.
#[inline]
fn open_slot(id: u64) -> Option<&'static Slot> {
    if id == 0 || id == UNRECORDED {
        return None;
    }
    by_number(ring_of(id))
        .and_then(|it| it.slot_of(id))
        .filter(|it| it.id.load(Ordering::Acquire) == id)
}

/// Notes that a row names the open span `id` (an event, a sample, a pool job it gave): it is
/// written whole as it closes, never folded ([`fold_below`]). Nothing for a span closed
/// already, which was written whole or folded by then.
#[inline]
pub(crate) fn keep(id: u64) {
    if let Some(slot) = open_slot(id) {
        slot.kids.fetch_add(NAMED_ONE, Ordering::Relaxed);
    }
}

#[inline]
fn store_said(slot: &Slot, said: &Said) {
    slot.krate.store(said.krate, Ordering::Relaxed);
    slot.function.store(said.function, Ordering::Relaxed);
    slot.unit.store(said.unit, Ordering::Relaxed);
    for (word, value) in slot.typed.iter().zip(said.typed) {
        word.store(value, Ordering::Relaxed);
    }
}

/// Another handle of the open span `id` was made.
pub(crate) fn cloned(id: u64) {
    if id == UNRECORDED {
        return;
    }
    if let Some(slot) = by_number(ring_of(id)).and_then(|ring| ring.slot_of(id)) {
        if slot.id.load(Ordering::Acquire) == id {
            slot.refs.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The ad hoc span `id`, still open, was given values after it was made: those of its
/// generic columns replace the ones it began with. A value given after the span closed is not
/// kept.
pub(crate) fn recorded(id: u64, values: &tracing::span::Record<'_>) {
    if id == UNRECORDED {
        return;
    }
    let Some(ring) = by_number(ring_of(id)) else { return };
    let Some(opened) = ring.opened(id) else { return };
    let wanted = SITES[usize::from(opened.site) - 1].get().map_or(0, |it| it.fields);
    if wanted == 0 {
        return;
    }
    let mut fields = Fields { wanted, said: opened.said() };
    values.record(&mut fields);
    if let Some(slot) = ring.slot_of(id) {
        store_said(slot, &fields.said);
    }
}

/// The span `id` of a kind, still open, says more as it ends ([`span::Ending::ended`]):
/// `say` changes its columns.
pub(crate) fn said_after(id: u64, say: impl FnOnce(&mut Said)) {
    if id == UNRECORDED {
        return;
    }
    let Some(ring) = by_number(ring_of(id)) else { return };
    let Some(opened) = ring.opened(id) else { return };
    let mut said = opened.said();
    say(&mut said);
    if let Some(slot) = ring.slot_of(id) {
        store_said(slot, &said);
    }
}

/// The build of the open span `id`, 0 for none or a span closed already: atomics only, so the
/// sampler may read it while the span's thread is stopped.
pub(crate) fn build_of(id: u64) -> u32 {
    if id == 0 || id == UNRECORDED {
        return 0;
    }
    by_number(ring_of(id)).and_then(|ring| ring.opened(id)).map_or(0, |it| it.build)
}

/// The spans the live thread with the operating system's id `tid` is inside now, outermost
/// first, each given to `each` with its name (the first [`OPEN_MAX`]); the innermost one's id,
/// 0 for none or a thread with no ring. Atomics and the callsite table only (no lock, no
/// allocation, no thread-local), so a fatal signal's handler may call it on the thread that
/// faulted (`crate::crash`), as the panic hook does (`crate::panics`).
pub(crate) fn open_of_thread(tid: u32, each: &mut dyn FnMut(u64, &'static str)) -> u64 {
    let ring = BY_NUMBER
        .iter()
        .map_while(OnceLock::get)
        .copied()
        .find(|ring| ring.live.load(Ordering::Acquire) && ring.current().0 == tid);
    let Some(ring) = ring else { return 0 };
    let depth = ring.depth.load(Ordering::Acquire) as usize;
    let mut innermost = 0;
    for slot in &ring.entered[..depth.min(OPEN_MAX)] {
        let id = slot.load(Ordering::Relaxed);
        innermost = id;
        let name = site_of(id).and_then(meta).map_or("?", |it| it.name());
        each(id, name);
    }
    innermost
}

/// The site of the open span `id`.
pub(crate) fn site_of(id: u64) -> Option<u16> {
    if id == UNRECORDED {
        return None;
    }
    Some(by_number(ring_of(id))?.opened(id)?.site)
}

/// The span `id` is entered on this thread: the innermost it is inside, counting this thread's
/// ticks as its CPU ([`crate::time::cpu`]) until it is left or another is entered.
#[inline]
pub(crate) fn entered(id: u64) {
    if id == UNRECORDED {
        return;
    }
    let Some(ring) = ring() else { return };
    let cost = cost_from(ring);
    let depth = ring.depth.load(Ordering::Relaxed);
    let counts = counting();
    let mut now = 0;
    if counts {
        now = clock::ticks();
        // The first span after a gap in none: the gap (blocked or not) is a stretch of its own.
        if depth == 0
            && now.wrapping_sub(ring.idle_since.load(Ordering::Relaxed))
                >= CPU_GAP_TICKS.load(Ordering::Relaxed)
        {
            boundary(ring, CpuWhy::Gap);
            now = clock::ticks();
        }
        accrue(ring, now);
    }
    if let Some(slot) = ring.entered.get(depth as usize) {
        slot.store(id, Ordering::Relaxed);
    }
    ring.depth.store(depth + 1, Ordering::Release);
    ring.note_context();
    if counts {
        counted_from(ring, now);
    }
    cost_to(ring, cost);
}

/// The span `id` is left on this thread: the span under it counts the thread's ticks again, and
/// `id`'s ticks of the stretch wait for its rate ([`left_open`]) unless it closes first.
#[inline]
pub(crate) fn exited(id: u64) {
    if id == UNRECORDED {
        return;
    }
    let Some(ring) = ring() else { return };
    let cost = cost_from(ring);
    let depth = ring.depth.load(Ordering::Relaxed);
    let Some(top) = depth.checked_sub(1) else { return };
    let named = (depth as usize).min(OPEN_MAX);
    let counts = counting();
    let now = if counts { clock::ticks() } else { 0 };
    // Spans leave in the order they entered but for a guard dropped out of turn: then the
    // ones above it move down.
    if (top as usize) < OPEN_MAX && ring.entered[top as usize].load(Ordering::Relaxed) != id {
        let Some(at) =
            ring.entered[..named].iter().rposition(|it| it.load(Ordering::Relaxed) == id)
        else {
            return;
        };
        if counts {
            accrue(ring, now);
            left_open(ring, id);
        }
        for i in at..named - 1 {
            ring.entered[i].store(ring.entered[i + 1].load(Ordering::Relaxed), Ordering::Relaxed);
        }
    } else if counts && (top as usize) < OPEN_MAX {
        accrue(ring, now);
        left_open(ring, id);
    }
    ring.depth.store(top, Ordering::Release);
    ring.note_context();
    if counts {
        counted_from(ring, now);
        if top == 0 {
            ring.idle_since.store(now, Ordering::Relaxed);
        }
    }
    cost_to(ring, cost);
}

/// A handle of the span `id` was dropped, on this thread. With the last, the span closed now:
/// its row is written to this thread's ring, and `Some` the ticks it lasted.
#[inline]
pub(crate) fn closed(id: u64) -> Option<u64> {
    if id == UNRECORDED {
        moved();
        return None;
    }
    let made_by = by_number(ring_of(id))?;
    let slot = made_by.slot_of(id)?;
    if slot.id.load(Ordering::Acquire) != id || slot.refs.fetch_sub(1, Ordering::AcqRel) > 1 {
        return None;
    }
    let opened = made_by.opened(id)?;
    // Its ticks of the stretch now, taken before the slot is free for another span.
    let pending = slot.pending.swap(0, Ordering::Relaxed);
    made_by.free(slot, id);
    let now = clock::ticks();
    let ring = ring()?;
    // Closed by another thread than made it: its own ring, but leased again since, is another
    // thread's too.
    let moved = ring.number != made_by.number || ring.lease.load(Ordering::Relaxed) != opened.lease;
    // The allocations as their count plus one, so 0 is "not counted". This thread's counters
    // say nothing of a span opened on another, so a moved span records none.
    let (allocs, bytes) = allocated().filter(|_| !moved).map_or((0, 0), |now| {
        let count = now.count.saturating_sub(opened.alloc.count).min(ALLOCS_MAX - 1);
        (count + 1, now.bytes.saturating_sub(opened.alloc.bytes))
    });
    let what = KIND_SPAN << 62
        | u64::from(opened.site) << (62 - SITE_BITS)
        | u64::from(opened.depth.min((1 << DEPTH_BITS) - 1)) << (62 - SITE_BITS - DEPTH_BITS)
        | (ring_of(id) as u64) << ALLOCS_BITS
        | allocs;
    let cost = cost_from(ring);
    let ticks = now.wrapping_sub(opened.start);
    // Its self CPU: what the stretches it was open across gave it, and the ticks of this one,
    // which its row holds as a share of its duration for the reader to give this stretch's
    // rate at the thread's next boundary ([`crate::time::cpu`]).
    let counted = counting() && opened.cpu != UNCOUNTED_CPU;
    let known = counted.then_some(opened.cpu);
    let share = if counted { cpu::share_word(pending, ticks) } else { 0 };
    let left = (ring.left_len.load(Ordering::Relaxed) as usize).min(LEFT_MAX);
    if left > 0 && ring.left[left - 1].load(Ordering::Relaxed) == id {
        ring.left_len.store(left as u32 - 1, Ordering::Relaxed);
    }
    let copied = FRAME_COPIED.get().is_some().then(frame_copied);
    // Noise, folded into its sum under its parent's site on this thread: short, named by no
    // row (no child written whole, no event, sample or job), on the thread it began on, copying
    // no frame, and saying no typed column (a `job`, `hold` and every other kind that says one
    // is the model's), or a wait too short to have parked ([`WAIT_FOLD_BELOW`]).
    let wait = Some(opened.site) == <span::Wait as span::Span>::site();
    let below = if opened.typed == [0; TYPED] {
        fold_ticks()
    } else if wait {
        wait_fold_ticks()
    } else {
        0
    };
    let fold = ticks < below
        && opened.kids == 0
        && !opened.held
        && !moved
        && copied.is_none_or(|it| it.copies == opened.copied.copies);
    // Its parent is named by this row when it is written whole, and has one child less open;
    // a folded child's ticks are its parent's to say ([`Slot::folded`]).
    let parent = open_slot(opened.parent);
    let mut parent_site = 0;
    if let Some(parent) = parent {
        parent.kids.fetch_add(if fold { u64::MAX } else { NAMED_ONE - 1 }, Ordering::Relaxed);
        if fold {
            parent.folded.fetch_add(1, Ordering::Relaxed);
            parent.folded_ticks.fetch_add(ticks, Ordering::Relaxed);
            parent_site = (parent.what.load(Ordering::Relaxed) & ((1 << SITE_BITS) - 1)) as u16;
        }
    }
    let key = crate::hot::record::key(opened.site, hot_key(opened.function));
    let waits_on = opened.typed[<span::col::WaitsOn as span::Column>::AT - span::GENERIC];
    crate::hot::record::closed(&ring.hot, key, ticks, waits_on);
    if fold {
        folded(ring, &opened, parent_site, now, known, pending, allocs, bytes);
    } else {
        // Every name its rows hold, as ids of this ring's, given before its first row: a
        // definition or a link (a span another thread made, a `Name` passed along) goes in
        // before the rows that name it, never between them.
        let mut words = [0; 3 + TYPED];
        words[..3].copy_from_slice(&[opened.krate, opened.function, opened.unit]);
        let mut typed = opened.typed;
        for (at, word) in typed.iter().enumerate() {
            if span::SYM_COLUMNS[span::GENERIC + at] {
                words[3 + at] = *word;
            }
        }
        row_syms(&mut words);
        for (at, word) in typed.iter_mut().enumerate() {
            if span::SYM_COLUMNS[span::GENERIC + at] {
                *word = words[3 + at];
            }
        }
        let of = words[0] << 32 | words[1];
        let build_unit = u64::from(opened.build) << 32 | words[2];
        let bytes_share = bytes.min(BYTES_MAX) | share << SHARE_SHIFT;
        ring.put([now, opened.start, id, opened.parent, what, of, build_unit, bytes_share]);
        let folded = (opened.folded != 0).then_some((opened.folded, opened.folded_ticks));
        // The CPU of the stretches it outlived, a row more only for a span that did.
        let cpu = known.filter(|it| *it > 0).map_or(0, |it| it + 1);
        put_cols(ring, now, id, cpu, moved.then_some(opened.thread), folded, &typed);
        if let Some(copied) = copied {
            let row = if moved {
                Some([0, 0, UNCOUNTED])
            } else if copied.copies != opened.copied.copies {
                Some([copied.copies - opened.copied.copies, copied.bytes - opened.copied.bytes, 0])
            } else {
                None
            };
            if let Some([copies, bytes, flags]) = row {
                ring.put([now, copies, bytes, id, KIND_PLOT << 62 | SPAN_COPIED_ROW, flags, 0, 0]);
            }
        }
    }
    // A wait's close ends the stretch it waited through; a root's, once the stretch is
    // `ROOT_AGE` old, ends its work's before the thread may block in no span; any close once it
    // is `AGE` old bounds how long a row waits for its rate.
    if counting() {
        let age = now.wrapping_sub(ring.stretch_ticks.load(Ordering::Relaxed));
        let root = opened.depth == 0 && ring.depth.load(Ordering::Relaxed) == 0;
        let why = if wait && age >= CPU_SLIVER_TICKS.load(Ordering::Relaxed) {
            Some(CpuWhy::Wait)
        } else if root && age >= CPU_ROOT_AGE_TICKS.load(Ordering::Relaxed) {
            Some(CpuWhy::Root)
        } else if age >= CPU_AGE_TICKS.load(Ordering::Relaxed) {
            Some(CpuWhy::Age)
        } else {
            None
        };
        if let Some(why) = why {
            boundary(ring, why);
        }
    }
    fold_aged(ring, now);
    ring.spans.store(ring.spans.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    cost_to(ring, cost);
    Some(ticks)
}

// --- Folding short spans ------------------------------------------------------------------

/// Spans shorter than this, of a process that folds them ([`fold_below`]), are summed under
/// their parent's site instead of written a row each; the parent's row says how many and how
/// long ([`Slot::folded`]), so its self time stays exact, and the sum keeps their count, time,
/// longest and self CPU.
///
/// Why 20 us (build 24 of a 52-minute cold self-build, 2026-11-04, 153.9 M spans tagged with
/// the build): 132.8 M spans (86%) were under 20 us and 98.5 M under 5 us; the 5 to 20 us band
/// is mostly `codegen describe` (24.6 M of its 35 M). Of those under 20 us, the ones saying no
/// typed column (129.9 M) sum to about 1.1 M rows a thread, parent and name each
/// [`FOLD_AGE`], so the spans table goes from 153.9 M rows to about 25 M (6x fewer). Their
/// self CPU is small beside the build's: 88 s under 5 us and 823 s under 100 us, of 16,407 s.
/// No reader looks at one span this short: `blaze slow` reads that build's spans of 200 us and
/// longer, and the shorter ones only on its critical path's lanes
/// ([`crate::time::load::SPAN_BUDGET`]). Waits fold only below
/// [`WAIT_FOLD_BELOW`].
pub const FOLD_BELOW: Duration = Duration::from_micros(20);

/// Waits shorter than this fold ([`fold_below`]); longer ones are written a row each with what
/// they waited on (rule every-wait-says-what). A wait this short never parked: on build 24 none
/// of the 164,892 waits under 1 us held a `parked` switch, while 42,532 of the 96,076 from 1
/// to 5 us did. Its sum keeps the count and time under its parent's site, not what it was on.
pub const WAIT_FOLD_BELOW: Duration = Duration::from_micros(1);

/// How long a thread holds a site's folded spans before it writes their row: about a drain
/// ([`crate::history::DRAIN`]), so a flush finds what the threads that keep working folded.
/// A pool worker writes its own as it goes idle, any thread as it ends.
pub const FOLD_AGE: Duration = Duration::from_millis(50);

/// [`FOLD_BELOW`], [`WAIT_FOLD_BELOW`] and [`FOLD_AGE`] in ticks, once [`fold_below`] set
/// them: 0 folds nothing.
static FOLD_TICKS: AtomicU64 = AtomicU64::new(0);
static WAIT_FOLD_TICKS: AtomicU64 = AtomicU64::new(0);
static FOLD_AGE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Has this process fold each span shorter than `below`, and each wait shorter than
/// [`WAIT_FOLD_BELOW`] (or `below`, if shorter), into a sum a parent's site, site, thread and
/// build (the `folded` table's rows) instead of writing its row, unless a row names it or it
/// says a typed column ([`closed`]); a sum is written at least every [`FOLD_AGE`]. blaze's
/// CLI sets it once, to [`FOLD_BELOW`], for the client and the daemon; a process that never
/// does records every span.
pub fn fold_below(below: Duration) {
    let scale = clock::Scale::now();
    FOLD_AGE_TICKS.store(scale.ticks_in(FOLD_AGE.as_nanos() as u64), Ordering::Relaxed);
    WAIT_FOLD_TICKS
        .store(scale.ticks_in(below.min(WAIT_FOLD_BELOW).as_nanos() as u64), Ordering::Relaxed);
    FOLD_TICKS.store(scale.ticks_in(below.as_nanos() as u64), Ordering::Relaxed);
}

#[cfg(test)]
thread_local! {
    /// A test's own [`fold_below`] (spans, waits) and [`FOLD_AGE`] in ticks, on its thread
    /// alone, so the tests beside it running in the process record every span.
    static FOLD_HERE: Cell<Option<(u64, u64, u64)>> = const { Cell::new(None) };
}

/// Has this thread alone fold spans shorter than `below` and waits shorter than `waits_below`,
/// writing their sums after `age`.
#[cfg(test)]
pub(crate) fn fold_here(below: Duration, waits_below: Duration, age: Duration) {
    let scale = clock::Scale::now();
    let ticks = |it: Duration| scale.ticks_in(it.as_nanos() as u64);
    FOLD_HERE.with(|it| it.set(Some((ticks(below), ticks(waits_below), ticks(age)))));
}

#[inline]
fn fold_ticks() -> u64 {
    #[cfg(test)]
    if let Some((below, _, _)) = FOLD_HERE.with(Cell::get) {
        return below;
    }
    FOLD_TICKS.load(Ordering::Relaxed)
}

#[inline]
fn wait_fold_ticks() -> u64 {
    #[cfg(test)]
    if let Some((_, waits, _)) = FOLD_HERE.with(Cell::get) {
        return waits;
    }
    WAIT_FOLD_TICKS.load(Ordering::Relaxed)
}

#[inline]
fn fold_age_ticks() -> u64 {
    #[cfg(test)]
    if let Some((_, _, age)) = FOLD_HERE.with(Cell::get) {
        return age;
    }
    FOLD_AGE_TICKS.load(Ordering::Relaxed)
}

/// The plot byte of a [`Fold`]'s row. Word 4 above it: the site (bits 8..20), its parent's site
/// (20..32, 0 for none open), how many spans (32..56), whether they were roots (depth 0, as
/// `uncovered_per_worker` counts them; 56), and whether their CPU (57) and allocations (58)
/// were counted.
const FOLD_ROW: u64 = 0xf9;
const FOLD_PARENT_SHIFT: u32 = 20;
const FOLD_SPANS_SHIFT: u32 = 32;
const FOLD_SPANS_MAX: u64 = (1 << 24) - 1;
const FOLD_ROOT: u64 = 1 << 56;
const FOLD_CPU: u64 = 1 << 57;
const FOLD_ALLOCS: u64 = 1 << 58;
const _: () = assert!(SITE_BITS == 12, "a fold row's site is bits 8..20 and its parent's 20..32");

/// A thread's sums of short spans a site, parent's site, root or not, and build: written as
/// one row.
#[derive(Clone, Copy)]
struct Fold {
    /// [`fold_key`]; 0 for an empty slot.
    key: u64,
    first: u64,
    last: u64,
    spans: u64,
    ticks: u64,
    max: u64,
    /// Self CPU (ns) and allocations summed over the spans that counted them.
    cpu: Option<u64>,
    allocs: Option<(u64, u64)>,
    /// Its spans' ticks of the thread's CPU stretch now, given the stretch's rate at its end
    /// ([`settle_folds`]); and whether a stretch's rate was not read, so its CPU is not
    /// counted.
    pending: u64,
    uncounted: bool,
}

const NO_FOLD: Fold = Fold {
    key: 0,
    first: 0,
    last: 0,
    spans: 0,
    ticks: 0,
    max: 0,
    cpu: None,
    allocs: None,
    pending: 0,
    uncounted: false,
};

/// Slots a thread folds into, by key. On build 24 a thread held at most 55 keys (a parent's
/// name and a name) in a [`FOLD_AGE`], 27 at the 99th percentile; a key looks in
/// [`FOLD_PROBES`] slots from its home, and one that finds them all taken writes its home's
/// row early.
const FOLD_SLOTS: usize = 128;
const FOLD_PROBES: usize = 4;

struct Folds {
    slots: [Fold; FOLD_SLOTS],
    /// The earliest start any slot holds, in ticks; `u64::MAX` for none.
    oldest: u64,
    /// The slots with ticks waiting for a rate ([`Fold::pending`]), a bit each.
    dirty: u128,
}

const _: () = assert!(FOLD_SLOTS <= 128, "a fold slot is a bit of `Folds::dirty`");

thread_local! {
    /// No destructor, so a span closing as the thread ends still folds; the ring's
    /// [`Held`] writes what is left as it gives the ring back.
    static FOLDS: RefCell<Folds> = const { RefCell::new(Folds { slots: [NO_FOLD; FOLD_SLOTS], oldest: u64::MAX, dirty: 0 }) };
}

/// A sum's key: its build, its parent's site, whether its spans are roots and its site, with a
/// bit set so no key is 0.
fn fold_key(site: u16, parent_site: u16, root: bool, build: u32) -> u64 {
    u64::from(build) << 32
        | u64::from(parent_site) << 16
        | u64::from(root) << 13
        | 1 << 12
        | u64::from(site)
}

/// Adds the span `opened`, closing now under a parent of `parent_site` (0: none open), to its
/// sum on this thread: `cpu` its self CPU so far (`None`: not counted) and `pending` its ticks
/// of the stretch now, `allocs` as its row would have it, plus one (0: not counted).
#[allow(clippy::too_many_arguments)]
fn folded(
    ring: &Ring,
    opened: &Opened,
    parent_site: u16,
    now: u64,
    cpu: Option<u64>,
    pending: u64,
    allocs: u64,
    bytes: u64,
) {
    let key = fold_key(opened.site, parent_site, opened.depth == 0, opened.build);
    let home = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 57) as usize % FOLD_SLOTS;
    _ = FOLDS.try_with(|folds| {
        let Ok(mut folds) = folds.try_borrow_mut() else { return };
        let at = (0..FOLD_PROBES)
            .map(|probe| (home + probe) % FOLD_SLOTS)
            .find(|&at| folds.slots[at].key == key || folds.slots[at].key == 0)
            .unwrap_or(home);
        if folds.slots[at].key != key && folds.slots[at].key != 0 {
            put_fold_settled(ring, &mut folds, at);
        }
        let slot = &mut folds.slots[at];
        if slot.key != key {
            *slot = Fold { key, first: opened.start, last: now, ..NO_FOLD };
        }
        let ticks = now.wrapping_sub(opened.start);
        slot.first = slot.first.min(opened.start);
        slot.last = slot.last.max(now);
        slot.spans += 1;
        slot.ticks += ticks;
        slot.max = slot.max.max(ticks);
        if let Some(cpu) = cpu {
            slot.cpu = Some(slot.cpu.unwrap_or(0) + cpu);
            slot.pending += pending;
        }
        if let Some(count) = allocs.checked_sub(1) {
            let (had, had_bytes) = slot.allocs.unwrap_or((0, 0));
            slot.allocs = Some((had + count, had_bytes + bytes));
        }
        let (first, full) = (slot.first, slot.spans >= FOLD_SPANS_MAX);
        if cpu.is_some() && pending > 0 {
            folds.dirty |= 1 << at;
        }
        if full {
            put_fold_settled(ring, &mut folds, at);
        }
        folds.oldest = folds.oldest.min(first);
    });
}

/// Gives every sum of `folds` with ticks waiting the stretch's `rate` (`None`: not counted).
fn settle_folds(folds: &mut Folds, rate: Option<Rate>) {
    let mut dirty = std::mem::take(&mut folds.dirty);
    while dirty != 0 {
        let at = dirty.trailing_zeros() as usize;
        dirty &= dirty - 1;
        let Some(fold) = folds.slots.get_mut(at) else { continue };
        let pending = std::mem::take(&mut fold.pending);
        match rate {
            Some(rate) => fold.cpu = fold.cpu.map(|it| it + rate.cpu_of(pending)),
            None => fold.uncounted = true,
        }
    }
}

/// Writes the sum in slot `at` of `folds` to `ring` and empties it, cutting the CPU stretch
/// first when it has ticks waiting.
fn put_fold_settled(ring: &Ring, folds: &mut Folds, at: usize) {
    if folds.dirty & 1 << at != 0 {
        boundary_in(ring, CpuWhy::Fold, Some(&mut *folds));
    }
    put_fold(ring, &mut folds.slots[at]);
}

/// Writes `fold`'s row to `ring` and empties it. Ticks still waiting for a rate (a boundary
/// that could not reach the sums) leave its CPU not counted.
fn put_fold(ring: &Ring, fold: &mut Fold) {
    let Fold { key, first, last, spans, ticks, max, cpu, allocs, pending, uncounted } =
        std::mem::replace(fold, NO_FOLD);
    if key == 0 {
        return;
    }
    let cpu = cpu.filter(|_| pending == 0 && !uncounted);
    let site = key & ((1 << SITE_BITS) - 1);
    let parent_site = key >> 16 & ((1 << SITE_BITS) - 1);
    let root = if key >> 13 & 1 == 1 { FOLD_ROOT } else { 0 };
    let counted =
        if cpu.is_some() { FOLD_CPU } else { 0 } | if allocs.is_some() { FOLD_ALLOCS } else { 0 };
    let what = KIND_PLOT << 62
        | counted
        | root
        | spans.min(FOLD_SPANS_MAX) << FOLD_SPANS_SHIFT
        | parent_site << FOLD_PARENT_SHIFT
        | site << 8
        | FOLD_ROW;
    let (count, bytes) = allocs.unwrap_or((0, 0));
    let build = key >> 32;
    ring.put([
        last,
        first,
        ticks,
        max,
        what,
        cpu.unwrap_or(0),
        build << 32 | count.min(u64::from(u32::MAX)),
        bytes,
    ]);
}

/// Writes this thread's folded sums once the oldest is [`FOLD_AGE`] old.
#[inline]
fn fold_aged(ring: &Ring, now: u64) {
    let oldest =
        FOLDS.try_with(|it| it.try_borrow().map_or(u64::MAX, |it| it.oldest)).unwrap_or(u64::MAX);
    if oldest != u64::MAX && now.wrapping_sub(oldest) >= fold_age_ticks() {
        put_folds(ring);
    }
}

/// Writes every sum this thread folded into `ring`, those with ticks waiting given their rate
/// first.
fn put_folds(ring: &Ring) {
    _ = FOLDS.try_with(|folds| {
        let Ok(mut folds) = folds.try_borrow_mut() else { return };
        if folds.dirty != 0 {
            boundary_in(ring, CpuWhy::Fold, Some(&mut *folds));
        }
        for slot in &mut folds.slots {
            put_fold(ring, slot);
        }
        folds.dirty = 0;
        folds.oldest = u64::MAX;
    });
}

/// Writes the sums of the spans this thread folded ([`fold_below`]): as a pool worker goes
/// idle and a build ends, so a flush has them.
pub fn flush_folds() {
    if let Some(ring) = MINE.with(Cell::get) {
        put_folds(ring);
    }
}

/// Writes the [`SPAN_COLS_ROW`]s of the span `id`, closing now: its self CPU (`cpu`, plus one;
/// 0: not counted), the thread that made it when another closes it (`made_by`), the children it
/// folded and their ticks (`folded`) and each typed column said, [`COL_WORDS`] a row.
#[inline]
fn put_cols(
    ring: &Ring,
    now: u64,
    id: u64,
    cpu: u64,
    made_by: Option<u64>,
    folded: Option<(u64, u64)>,
    typed: &[u64; TYPED],
) {
    let said = (cpu != 0).then_some((CPU_COL, cpu));
    let made_by = made_by.map(|it| (THREAD_COL, it));
    let folded = folded
        .into_iter()
        .flat_map(|(spans, ticks)| [(FOLDED_COL, spans), (FOLDED_TICKS_COL, ticks)]);
    let mut values = said
        .into_iter()
        .chain(made_by)
        .chain(folded)
        .chain((0..TYPED).filter(|&it| typed[it] != 0).map(|it| (it as u64, typed[it])));
    loop {
        let mut row = [now, 0, 0, id, 0, 0, 0, 0];
        let mut whose = 0;
        let mut any = false;
        for (place, &word) in COL_WORDS.iter().enumerate() {
            let (col, value) = match values.next() {
                Some(it) => {
                    any = true;
                    it
                }
                None => (NO_COL, 0),
            };
            row[word] = value;
            whose |= col << (8 * (place + 1));
        }
        if !any {
            return;
        }
        row[4] = KIND_PLOT << 62 | whose | SPAN_COLS_ROW;
        ring.put(row);
    }
}

/// Notes that work moved on this thread with no span closing: what a long piece of work
/// with no span of its own (a query's run) says to the stall watchdog. One store.
#[inline]
pub fn moved() {
    if let Some(ring) = ring() {
        ring.spans.store(ring.spans.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    }
}

/// Every ring now, into `into`.
#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn rings(into: &mut Vec<&'static Ring>) {
    into.clear();
    into.extend(lock(&RINGS).iter().copied());
}

/// Each ring's number and its sketches' counters ([`crate::hot`]), read as they are now.
pub(crate) fn each_hot(mut each: impl FnMut(u16, &[AtomicU64])) {
    // alloc: a read of the sketches, once a read
    let rings: Vec<&'static Ring> = lock(&RINGS).clone();
    for ring in rings {
        each(ring.number, &ring.hot);
    }
}

/// Spans closed in this process so far, on every thread: it grows while any work moves.
pub fn progress() -> u64 {
    lock(&RINGS).iter().map(|it| it.spans.load(Ordering::Relaxed)).sum()
}

// --- Reading rows -----------------------------------------------------------------------

/// A row of the schema's `spans` table, as the ring holds it: names as sites and [`Sym`]s,
/// resolved by the writer of a table ([`meta`], [`name`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanRow {
    pub build: u32,
    pub thread: u32,
    pub thread_name: Sym,
    pub span_id: u64,
    pub parent: u64,
    pub depth: u16,
    pub site: u16,
    pub start_ns: i64,
    pub dur_ns: i64,
    /// `None` when allocations are not counted ([`count_allocations`]).
    pub alloc: Option<Allocated>,
    /// What its thread's task frames copied growing blocks while it was open: `None` when not
    /// counted ([`count_frame_copies`]) or when it closed on another thread than it began on.
    pub copied: Option<FrameCopied>,
    pub krate: Sym,
    pub function: Sym,
    pub unit: Sym,
    /// Its CPU in nanoseconds while it was the innermost span its thread was inside, from the
    /// thread's CPU clock read at boundaries ([`crate::time::cpu`], with its error bound):
    /// `None` when not counted ([`count_cpu`]), a read failed, or its thread's next boundary
    /// was not in the read (a [`window`], a ring silent past [`HOLD_MAX`]).
    pub self_cpu_ns: Option<u64>,
    /// The words of its typed columns, as its span kind said them (0: not said): read with
    /// [`SpanRow::get`] or as its kind ([`span::read`]).
    pub typed: [u64; TYPED],
    /// Its children folded into `folded` rows ([`fold_below`]) and their durations summed: 0
    /// when none was. Its self time is its duration less its children's rows and this.
    pub folded_spans: u64,
    pub folded_ns: i64,
}

impl SpanRow {
    /// Its columns, as a span kind reads them.
    pub fn said(&self) -> Said {
        Said {
            krate: u64::from(self.krate),
            function: u64::from(self.function),
            unit: u64::from(self.unit),
            typed: self.typed,
        }
    }

    /// Its column `C`: `None` when not said.
    pub fn get<C: span::Column>(&self) -> Option<C::Value> {
        self.said().value::<C>()
    }
}

/// A row of the schema's `counters` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CounterRow {
    pub build: u32,
    pub plot: Plot,
    pub t_ns: i64,
    pub value: u64,
    /// The fiber and job a [`fiber_stack`] counter is of; `None` for every other plot.
    pub fiber: Option<(u32, u64)>,
    /// The name of a [`Plot::Named`] counter ([`plot_named`]); 0 for every other plot.
    pub named: Sym,
}

/// A row of the schema's `switches` table: a [`switched`] row, on the thread it happened on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchRow {
    pub build: u32,
    pub thread: u32,
    pub thread_name: Sym,
    pub t_ns: i64,
    pub event: Switched,
    pub job: u64,
    pub fiber: u32,
    pub core: u32,
    pub waited: Option<(WaitedOn, u64)>,
    /// For `started` and `ran`, the job that gave it to the pool; for `ready`, the job that
    /// woke it. `None`: no job (a thread on no fiber), or another event.
    pub by: Option<u64>,
    /// For `started` and `ran`, when the job began on its fiber.
    pub since_ns: Option<i64>,
    /// For `started` after a `Began`, the number the job had before it joined the graph, which
    /// what it did before then (its spans, the jobs it gave the pool) names.
    pub began_as: Option<u64>,
    /// For `started` and `ran`, how long the job waited for a core: from its pool's submit to
    /// `since_ns`. `None` when the pool did not say when it was given.
    pub queued_ns: Option<i64>,
    /// The thread's CPU time in nanoseconds read at the switch (its boundary,
    /// [`crate::time::cpu`]): two of a thread's give the CPU it had between them. `None` when
    /// spans count no CPU ([`count_cpu`]), the read failed, or for `ready`.
    pub cpu_ns: Option<u64>,
}

/// A row of the schema's `events` table: an [`evented`] `tracing` event, on the thread it
/// happened on; its level, target and name are its site's ([`meta`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRow {
    pub build: u32,
    pub thread: u32,
    pub thread_name: Sym,
    pub t_ns: i64,
    /// The job running on the thread, as `switches` names it: 0 for none.
    pub job: u64,
    /// The span it happened in: 0 for none.
    pub span: u64,
    pub site: u16,
    /// Its message alone ([`name`]): its other fields are its [`FieldRow`]s.
    pub message: Sym,
    /// Its id, which no other event of the process has ([`event_id`]): what its fields name.
    pub event_id: u64,
}

/// A row of the schema's `event_fields` table: one field, other than its message, of an event
/// ([`evented`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldRow {
    pub build: u32,
    /// When its event was, as [`EventRow::t_ns`].
    pub t_ns: i64,
    /// Its event's [`EventRow::event_id`].
    pub event_id: u64,
    /// Its event's site: with `index`, its key.
    pub site: u16,
    /// Its place in its callsite's fields ([`tracing::field::Field::index`]).
    pub index: u8,
    /// Its place among its event's fields as the event recorded them.
    pub ordinal: u16,
    pub value: FieldValue,
}

/// A row of the schema's `folded` table: the spans of a site shorter than [`fold_below`]'s
/// threshold that a thread closed in a stretch under spans of one site, summed ([`closed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRow {
    pub build: u32,
    pub thread: u32,
    pub thread_name: Sym,
    pub site: u16,
    /// The site of the span they were inside, whose row says their count and time
    /// ([`SpanRow::folded_spans`]): 0 for none open as they closed.
    pub parent_site: u16,
    /// Its spans were roots (depth 0): a worker's own work between jobs, which the gate counts
    /// as covered.
    pub root: bool,
    /// The first one's start and the last one's end.
    pub from_ns: i64,
    pub to_ns: i64,
    pub spans: u64,
    /// Their durations summed, and the longest.
    pub dur_ns: i64,
    pub max_dur_ns: i64,
    /// Their self CPU summed: `None` when not counted.
    pub self_cpu_ns: Option<u64>,
    pub alloc: Option<Allocated>,
}

/// A build's end, as [`crate::built`] noted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildEnd {
    pub build: u64,
    pub t_ns: i64,
    pub thread: u32,
}

/// What a read took from the rings.
#[derive(Default, Debug)]
pub struct Read {
    pub spans: Vec<SpanRow>,
    pub counters: Vec<CounterRow>,
    pub switches: Vec<SwitchRow>,
    pub events: Vec<EventRow>,
    /// The fields of `events`, after them in the order they were read.
    pub fields: Vec<FieldRow>,
    pub folds: Vec<FoldRow>,
    pub ends: Vec<BuildEnd>,
    /// Rows written over before this reader read them: it fell behind by that many.
    pub skipped: u64,
    /// Spans made and not recorded since the process began: past the callsite table, the
    /// open slots of a thread, or the rings of a process.
    pub lost: u64,
    /// Names a row held that were read after their generation was let go, since the process
    /// began: a reader that fell behind by a generation of names.
    pub names_forgotten: u64,
    /// When the read was, on the schema's clock.
    pub at_ns: i64,
    scratch: Vec<(u64, [u64; WORDS])>,
    /// The spans of the ring being read with ticks waiting for its next boundary's rate: their
    /// place in `spans` and the ticks ([`Read::ring_begins`], [`decode`]).
    waiting: Vec<(usize, u64)>,
    /// The ring being read's last switch: its row's [`event_id`] and its place in `switches`,
    /// for a boundary's row right after it to give it its reading.
    last_switch: Option<(u64, usize)>,
}

impl Read {
    pub fn clear(&mut self) {
        self.spans.clear();
        self.counters.clear();
        self.switches.clear();
        self.events.clear();
        self.fields.clear();
        self.folds.clear();
        self.ends.clear();
        self.skipped = 0;
    }

    /// The rows it holds, of every table.
    pub fn rows(&self) -> usize {
        self.spans.len()
            + self.counters.len()
            + self.switches.len()
            + self.events.len()
            + self.fields.len()
            + self.folds.len()
            + self.ends.len()
    }

    /// The highest build any of its rows is of; 0 when none is of a build.
    pub fn highest_build(&self) -> u64 {
        let rows = [
            self.spans.iter().map(|it| it.build).max(),
            self.counters.iter().map(|it| it.build).max(),
            self.switches.iter().map(|it| it.build).max(),
            self.events.iter().map(|it| it.build).max(),
            self.folds.iter().map(|it| it.build).max(),
        ];
        let ends = self.ends.iter().map(|it| it.build).max().unwrap_or(0);
        rows.into_iter().flatten().map(u64::from).max().unwrap_or(0).max(ends)
    }
}

/// Where a reader is in each ring: the next row it takes. A new cursor takes everything the
/// rings hold.
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    next: Vec<u64>,
}

/// A span row whose CPU waits for its thread's next boundary is held back from a [`read`] at
/// most this long ([`take`]); past it, a silent thread's spans are read with their CPU not
/// counted.
pub const HOLD_MAX: Duration = Duration::from_secs(1);

/// How [`take`] reads the rings: from a cursor, holding back each ring's rows from its first
/// span still waiting for a boundary's rate, or the whole of each.
enum Taking<'a> {
    Cursor(&'a mut Cursor),
    Window,
}

/// Marks in the recorder's mapping, before the history commits flush `seq`, what that flush
/// took: the rows before `cursor` in each ring, but the DEBUG rows the history still holds
/// (`held`, their event ids, of the builds `held_builds`). A postmortem of this process reads
/// past these marks once the manifest says flush `seq` committed, past the last committed
/// flush's until then ([`flush_committed`]), so no row is written twice and none held is lost,
/// whether the process dies before the commit or after. Nothing until the recorder is
/// persisted ([`persist`]).
pub fn flush_pending(
    cursor: &Cursor,
    held: impl IntoIterator<Item = u64>,
    held_builds: &[u32],
    seq: u64,
) {
    if !postmortem::persisted() {
        return;
    }
    // The first held row of each ring, by its number less one.
    // alloc: once a flush of a persisted recorder, a word a ring
    let mut lowest = vec![u64::MAX; cursor.next.len()];
    for id in held {
        let (ring, index) = event_place(id);
        if let Some(it) = usize::from(ring).checked_sub(1).and_then(|at| lowest.get_mut(at)) {
            *it = (*it).min(index);
        }
    }
    postmortem::mark_flush(seq, held_builds, |slot| {
        for ring in lock(&RINGS).iter() {
            let at = usize::from(ring.number) - 1;
            let Some(&next) = cursor.next.get(at) else { continue };
            let held_from = lowest.get(at).copied().unwrap_or(u64::MAX).min(next);
            let store = ring.store_seq();
            store.mark(slot, next, held_from);
            // The ring moved to the mapping meanwhile ([`Ring::move_store`]): its new store is
            // marked too, so the mark is in the store a postmortem reads.
            let now = ring.store_seq();
            if !std::ptr::eq(now, store) {
                now.mark(slot, next, held_from);
            }
        }
    });
}

/// Notes that flush `seq`, marked by [`flush_pending`], committed, `at_ns` the read that took
/// its rows: a postmortem reads past its marks even with no manifest saying so.
pub fn flush_committed(seq: u64, at_ns: i64) {
    postmortem::flush_committed(seq, at_ns);
}

/// The ring and the index in it of the row the event `id` names ([`event_id`]).
const fn event_place(id: u64) -> (u16, u64) {
    ((id >> EVENT_INDEX_BITS) as u16, id & ((1 << EVENT_INDEX_BITS) - 1))
}

/// Backs the rings, and all a reader needs to decode them (callsites, names, threads' names,
/// the clock), with a file of this process in the history's directory `dir`, so the rows the
/// history has not written yet outlive a crash: the next history to open there writes them
/// ([`postmortem`]). `blaze` and `process` are what the history names this process's builds
/// and rows with. Once a process; a ring held by a thread moves to the file as it next writes.
pub fn persist(
    dir: &std::path::Path,
    blaze: &str,
    process: &str,
) -> Result<(), postmortem::PostmortemError> {
    postmortem::persist(dir, blaze, process)
}

/// Takes into `into` every row written since `cursor`, and moves `cursor` past them. A reader
/// slower than the writers loses what was written over before it read, counted in
/// [`Read::skipped`]; its memory stays what one read holds. A ring's rows from its first span
/// whose CPU waits for the thread's next boundary are left for the next read ([`HOLD_MAX`]).
pub fn read(cursor: &mut Cursor, into: &mut Read) {
    take(into, Taking::Cursor(cursor));
}

/// Takes into `into` every row of the last `window` the rings still hold. A span whose CPU
/// waits for its thread's next boundary reads as not counted.
pub fn window(window: Duration, into: &mut Read) {
    let since = clock::mono_ns() - window.as_nanos() as i64;
    take(into, Taking::Window);
    into.spans.retain(|it| it.start_ns + it.dur_ns >= since);
    into.counters.retain(|it| it.t_ns >= since);
    into.switches.retain(|it| it.t_ns >= since);
    into.events.retain(|it| it.t_ns >= since);
    into.fields.retain(|it| it.t_ns >= since);
    into.folds.retain(|it| it.to_ns >= since);
    into.ends.retain(|it| it.t_ns >= since);
}

/// Whether `row` is a boundary's ([`CPU_ROW`]).
fn is_cpu_row(row: &[u64; WORDS]) -> bool {
    row[4] >> 62 == KIND_PLOT && row[4] & 0xff == CPU_ROW
}

/// Whether `row` is a span's whose CPU waits for its thread's next boundary's rate.
fn waits_for_rate(row: &[u64; WORDS]) -> bool {
    row[4] >> 62 == KIND_SPAN && row[7] >> SHARE_SHIFT > cpu::SHARE_COUNTED
}

fn take(into: &mut Read, mut taking: Taking<'_>) {
    // The reading thread's own stretch ends here, so its spans have their rate in this read.
    if let Ok(Some(ring)) = MINE.try_with(Cell::get) {
        boundary(ring, CpuWhy::Read);
    }
    let scale = clock::Scale::now();
    let now = clock::ticks();
    into.at_ns = scale.mono(now);
    let hold = scale.ticks_in(HOLD_MAX.as_nanos() as u64);
    // clone: the list of rings, so none is read under the lock threads take to start
    let rings = lock(&RINGS).clone();
    let mut scratch = std::mem::take(&mut into.scratch);
    let mut lost = RINGLESS.load(Ordering::Relaxed);
    let counting_copies = FRAME_COPIED.get().is_some();
    // The readers' names, for the whole read: no thread that records takes it.
    let mut reader = reader();
    // The names open spans hold, kept while they do: a span that closes in a later read
    // writes them as links, maybe past RETAIN generations of their ring.
    // alloc: a read's held names, once a read
    let mut held = Vec::new();
    for ring in &rings {
        ring.held_tokens(&mut held);
    }
    reader.streams.hold(&held, &mut Rings);
    for ring in rings {
        lost += ring.lost.load(Ordering::Relaxed);
        scratch.clear();
        let mut end = None;
        match &mut taking {
            Taking::Cursor(cursor) => {
                let at = usize::from(ring.number) - 1;
                if cursor.next.len() <= at {
                    cursor.next.resize(at + 1, 0);
                }
                let from = cursor.next[at];
                let head = ring.read(from, &mut scratch);
                cursor.next[at] = head;
                into.skipped += head.saturating_sub(CAP as u64).saturating_sub(from);
                // The rows from the first span waiting for a rate that no boundary read gives
                // are the next read's, unless its thread has been silent past `HOLD_MAX`.
                let after =
                    scratch.iter().rposition(|(_, row)| is_cpu_row(row)).map_or(0, |it| it + 1);
                if let Some(first) = scratch[after..]
                    .iter()
                    .position(|(_, row)| waits_for_rate(row))
                    .map(|it| after + it)
                    && now.wrapping_sub(scratch[first].1[0]) < hold
                {
                    cursor.next[at] = scratch[first].0;
                    end = Some(first);
                }
            }
            Taking::Window => {
                ring.read(0, &mut scratch);
            }
        }
        // Its definitions up to (at least) the rows just read, so every id they hold resolves.
        reader.streams.catch_up(ring.number, &mut Rings);
        let owners = lock(&ring.owners);
        into.ring_begins();
        let namer = &mut LiveNamer(&mut reader);
        for &(i, row) in &scratch[..end.unwrap_or(scratch.len())] {
            let Some(by) = Ring::owner(&owners, i) else { continue };
            decode(
                into,
                (by.tid, by.name),
                event_id(ring.number, i),
                &row,
                &scale,
                counting_copies,
                namer,
            );
        }
        into.ring_ends();
    }
    into.lost = lost;
    into.names_forgotten = NAMES_FORGOTTEN.load(Ordering::Relaxed) + reader.streams.forgotten;
    into.scratch = scratch;
}

/// Whether the row of [`event_id`] `id` is the one right after `prev`'s, on the same ring.
fn follows(prev: u64, id: u64) -> bool {
    let index = (1 << EVENT_INDEX_BITS) - 1;
    prev & !index == id & !index && prev.wrapping_add(1) & index == id & index
}

impl Read {
    /// Starts decoding a ring's rows ([`decode`]), in order: no span of it waits for a rate
    /// yet, and no switch was read last.
    fn ring_begins(&mut self) {
        self.waiting.clear();
        self.last_switch = None;
    }

    /// Ends decoding a ring's rows: a span still waiting for its thread's next boundary's rate
    /// (a window, a silent thread, a dead process's last stretch) reads as not counted.
    fn ring_ends(&mut self) {
        for &(at, _) in &self.waiting {
            if let Some(span) = self.spans.get_mut(at) {
                span.self_cpu_ns = None;
            }
        }
        self.waiting.clear();
        self.last_switch = None;
    }
}

/// One row of a ring into the tables of `into`: `by` the tid and name of the thread that
/// wrote it, `id` the row's [`event_id`] (its ring's number and its place there), `scale` the
/// clock of the process that did, `counting_copies` whether it counted frame copies
/// ([`count_frame_copies`]). A row that follows a span's (its copies, its typed columns) or an
/// event's (its fields) fills in the span or event `into` read last. The one decoder of rows:
/// [`take`] reads this process's rings with it, the postmortem a dead process's
/// ([`postmortem`]). A ring's rows are decoded in order between [`Read::ring_begins`] and
/// [`Read::ring_ends`]: a span's CPU share waits for the ring's next boundary row
/// ([`CPU_ROW`]), and a span none follows reads as not counted.
fn decode(
    into: &mut Read,
    by: (u32, Sym),
    id: u64,
    row: &[u64; WORDS],
    scale: &clock::Scale,
    counting_copies: bool,
    namer: &mut impl Namer,
) {
    let (ring, at) = event_place(id);
    let mut sym = |word: u64| namer.sym(ring, at, word & 0xffff_ffff);
    let tid = by.0;
    if names::is_names_row(row) {
        // A definition, a link or a generation's start: the readers' ([`names::Streams`]).
        return;
    }
    let thread_name = sym(u64::from(by.1));
    match row[4] >> 62 {
        KIND_SPAN => {
            let allocs = row[4] & ALLOCS_MAX;
            // Its CPU: what its last stretch's share waits for is resolved at the ring's next
            // boundary row; a `CPU_COL` adds the stretches it outlived.
            let (self_cpu_ns, pending) =
                match cpu::share_of(row[7] >> SHARE_SHIFT, row[0].wrapping_sub(row[1])) {
                    cpu::Share::Uncounted => (None, 0),
                    cpu::Share::Counted { pending } => (Some(0), pending),
                };
            if pending > 0 {
                into.waiting.push((into.spans.len(), pending));
            }
            into.spans.push(SpanRow {
                build: (row[6] >> 32) as u32,
                // The thread that closed it, until a `THREAD_COL` names the one that
                // made it.
                thread: tid,
                thread_name,
                span_id: row[2],
                parent: row[3],
                depth: (row[4] >> (62 - SITE_BITS - DEPTH_BITS) & ((1 << DEPTH_BITS) - 1)) as u16,
                site: (row[4] >> (62 - SITE_BITS) & ((1 << SITE_BITS) - 1)) as u16,
                start_ns: scale.mono(row[1]),
                dur_ns: scale.ns(row[0].wrapping_sub(row[1])) as i64,
                alloc: (allocs != 0)
                    .then(|| Allocated { bytes: row[7] & BYTES_MAX, count: allocs - 1 }),
                copied: counting_copies.then_some(FrameCopied::default()),
                krate: sym(row[5] >> 32),
                function: sym(row[5]),
                unit: sym(row[6]),
                self_cpu_ns,
                typed: [0; TYPED],
                folded_spans: 0,
                folded_ns: 0,
            });
        }
        KIND_PLOT if row[4] & 0xff == SWITCH_ROW => {
            let Some(event) = Switched::of(row[4] >> 8 & 0xff) else { return };
            let queued = (row[4] >> QUEUED_LOW_SHIFT & ((1 << QUEUED_LOW_BITS) - 1))
                | (row[6] >> 32) << QUEUED_LOW_BITS;
            into.switches.push(SwitchRow {
                build: row[6] as u32,
                thread: tid,
                thread_name,
                t_ns: scale.mono(row[0]),
                event,
                job: row[1],
                fiber: row[3] as u32,
                core: (row[3] >> 32) as u32,
                waited: WaitedOn::of(row[4] >> 16 & 0xff).map(|on| (on, row[2])),
                by: (row[5] != 0).then_some(row[5]),
                since_ns: (row[7] != 0).then(|| scale.mono(row[7])),
                began_as: (event == Switched::Started && row[2] != 0).then_some(row[2]),
                queued_ns: queued.checked_sub(1).map(|it| scale.ns(it) as i64),
                cpu_ns: None,
            });
            into.last_switch = Some((id, into.switches.len() - 1));
        }
        KIND_PLOT if row[4] & 0xff == CPU_ROW => {
            let rate = (row[4] & CPU_VALID != 0)
                .then(|| {
                    Rate::between(
                        Reading { ticks: row[2], cpu_ns: row[3] },
                        Reading { ticks: row[0], cpu_ns: row[1] },
                    )
                })
                .flatten();
            for &(at, pending) in &into.waiting {
                if let Some(span) = into.spans.get_mut(at) {
                    span.self_cpu_ns = span
                        .self_cpu_ns
                        .zip(rate)
                        .map(|(known, rate)| known + rate.cpu_of(pending));
                }
            }
            into.waiting.clear();
            if row[4] >> 8 & 0xff == CpuWhy::Switch as u64
                && row[4] & CPU_READ != 0
                && let Some((prev, switch)) = into.last_switch
                && follows(prev, id)
                && let Some(switch) = into.switches.get_mut(switch)
            {
                switch.cpu_ns = Some(row[1]);
            }
        }
        KIND_PLOT if row[4] & 0xff == EVENT_ROW => into.events.push(EventRow {
            build: row[6] as u32,
            thread: tid,
            thread_name,
            t_ns: scale.mono(row[0]),
            job: row[1],
            span: row[2],
            site: (row[4] >> 8 & ((1 << SITE_BITS) - 1)) as u16,
            message: sym(row[5]),
            event_id: id,
        }),
        KIND_PLOT if row[4] & 0xff == EVENT_FIELDS_ROW => {
            // Written right after its event's row, by the same thread: the event read
            // last, unless the ring wrote over that row first.
            let Some(event) = into.events.last().filter(|it| it.event_id == row[3]) else { return };
            let (build, t_ns, site) = (event.build, event.t_ns, event.site);
            let mut ordinal =
                into.fields.iter().rev().take_while(|it| it.event_id == row[3]).count() as u16;
            for (place, &word) in COL_WORDS.iter().enumerate() {
                let slot = row[4] >> (FIELD_SLOT_SHIFT + FIELD_SLOT_BITS * place as u32)
                    & ((1 << FIELD_SLOT_BITS) - 1);
                let Some(value) = FieldValue::of(slot & 0b111, row[word]) else { continue };
                let value = match value {
                    FieldValue::Str(word) => FieldValue::Str(sym(u64::from(word))),
                    value => value,
                };
                into.fields.push(FieldRow {
                    build,
                    t_ns,
                    event_id: row[3],
                    site,
                    index: (slot >> 3) as u8,
                    ordinal,
                    value,
                });
                ordinal += 1;
            }
        }
        KIND_PLOT if row[4] & 0xff == SPAN_COPIED_ROW => {
            // Written right after its span's row, by the same thread: the span read
            // last, unless the ring wrote over that row first.
            if let Some(span) = into.spans.last_mut()
                && span.span_id == row[3]
            {
                span.copied = (row[5] & UNCOUNTED == 0)
                    .then_some(FrameCopied { copies: row[1], bytes: row[2] });
            }
        }
        KIND_PLOT if row[4] & 0xff == SPAN_COLS_ROW => {
            // As a copied row: right after its span's, or after that one's other rows.
            if let Some(span) = into.spans.last_mut()
                && span.span_id == row[3]
            {
                for (place, &word) in COL_WORDS.iter().enumerate() {
                    match row[4] >> (8 * (place + 1)) & 0xff {
                        // Its CPU from the stretches it outlived, beside its last stretch's
                        // share in its row.
                        CPU_COL => {
                            if let Some(cpu) = row[word].checked_sub(1) {
                                span.self_cpu_ns = span.self_cpu_ns.map(|it| it + cpu);
                            }
                        }
                        FOLDED_COL => span.folded_spans = row[word],
                        FOLDED_TICKS_COL => span.folded_ns = scale.ns(row[word]) as i64,
                        THREAD_COL => {
                            span.thread = (row[word] >> 32) as u32;
                            span.thread_name = sym(row[word]);
                        }
                        col => {
                            if let Some(at) = usize::try_from(col).ok().filter(|it| *it < TYPED) {
                                span.typed[at] = if span::SYM_COLUMNS[span::GENERIC + at] {
                                    u64::from(sym(row[word]))
                                } else {
                                    row[word]
                                };
                            }
                        }
                    }
                }
            }
        }
        KIND_PLOT if row[4] & 0xff == FOLD_ROW => into.folds.push(FoldRow {
            build: (row[6] >> 32) as u32,
            thread: tid,
            thread_name,
            site: (row[4] >> 8 & ((1 << SITE_BITS) - 1)) as u16,
            parent_site: (row[4] >> FOLD_PARENT_SHIFT & ((1 << SITE_BITS) - 1)) as u16,
            root: row[4] & FOLD_ROOT != 0,
            from_ns: scale.mono(row[1]),
            to_ns: scale.mono(row[0]),
            spans: row[4] >> FOLD_SPANS_SHIFT & FOLD_SPANS_MAX,
            dur_ns: scale.ns(row[2]) as i64,
            max_dur_ns: scale.ns(row[3]) as i64,
            self_cpu_ns: (row[4] & FOLD_CPU != 0).then_some(row[5]),
            alloc: (row[4] & FOLD_ALLOCS != 0)
                .then(|| Allocated { bytes: row[7], count: row[6] & 0xffff_ffff }),
        }),
        KIND_PLOT if row[4] & 0xff == FIBER_STACK_ROW => {
            let fiber = Some(((row[4] >> 8) as u32, row[7]));
            let (build, t_ns) = (row[6] as u32, scale.mono(row[0]));
            into.counters.extend(
                [
                    (Plot::FiberStackReserved, row[1]),
                    (Plot::FiberStackDeepest, row[2]),
                    (Plot::FiberStackTouched, row[3]),
                    (Plot::FiberStackGrown, row[5]),
                ]
                .map(|(plot, value)| CounterRow {
                    build,
                    plot,
                    t_ns,
                    value,
                    fiber,
                    named: 0,
                }),
            );
        }
        KIND_PLOT => {
            let Some(plot) = Plot::of(row[4] & 0xff) else { return };
            into.counters.push(CounterRow {
                build: row[6] as u32,
                plot,
                t_ns: scale.mono(row[0]),
                value: row[1],
                fiber: None,
                named: if plot == Plot::Named { sym(row[7]) } else { 0 },
            });
        }
        KIND_MARK => {
            into.ends.push(BuildEnd { build: row[1], t_ns: scale.mono(row[0]), thread: tid })
        }
        _ => {}
    }
}

// --- Threads now ------------------------------------------------------------------------

/// Writes into `out` what this thread is working on: the innermost span it is inside that
/// names a `function`, `unit` or `crate` (the first it has), as `<span name> <that>`, else the
/// innermost span's name. False, writing nothing, when it is inside no span. A scope's caller
/// waits in the span it opened the scope in, so before its wait this names the scope's creator.
pub(crate) fn write_working_on(out: &mut String) -> bool {
    let Some(ring) = ring() else { return false };
    let depth = (ring.depth.load(Ordering::Relaxed) as usize).min(OPEN_MAX);
    let mut innermost = None;
    for entered in ring.entered[..depth].iter().rev() {
        let id = entered.load(Ordering::Relaxed);
        let Some(opened) = by_number(ring_of(id)).and_then(|it| it.opened(id)) else { continue };
        let Some(meta) = meta(opened.site) else { continue };
        let of = [opened.function, opened.unit, opened.krate].into_iter().find(|it| *it != 0);
        match of.and_then(token_text) {
            Some(of) => {
                // Writing to a `String` does not fail.
                _ = write!(out, "{} {of}", meta.name());
                return true;
            }
            None if innermost.is_none() => innermost = Some(meta.name()),
            None => {}
        }
    }
    let Some(innermost) = innermost else { return false };
    out.push_str(innermost);
    true
}

/// A span a thread is inside now, what it is of, and for how long it has been.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct OpenSpan {
    pub name: &'static str,
    pub target: &'static str,
    pub for_ns: u64,
    /// Its fields `crate`, `function` and `unit`: a query's key, a unit's crate.
    pub krate: Option<std::sync::Arc<str>>,
    pub function: Option<std::sync::Arc<str>>,
    pub unit: Option<std::sync::Arc<str>>,
    /// A wait's `awaits` and `owner`, as in `spans`.
    pub awaits: Option<std::sync::Arc<str>>,
    pub owner: Option<u64>,
    /// The build its thread worked for as it opened ([`building`]: the daemon plane's request
    /// number); 0 for none.
    pub build: u32,
    /// A daemon `task` span's plane node ([`crate::span::col::Node`]); `None` for any other.
    pub node: Option<u64>,
}

/// A live thread and the spans it is inside now, outermost first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadNow {
    pub tid: u64,
    pub name: String,
    pub open: Vec<OpenSpan>,
    /// Spans it is inside past the [`OPEN_MAX`] named.
    pub deeper: u32,
}

/// Every live thread that is inside a span, and those spans. A thread moving while it is read
/// may show a span it just left; the stall watchdog reads threads that do not move.
pub fn threads_now() -> Vec<ThreadNow> {
    let scale = clock::Scale::now();
    // clone: the list of rings, so none is read under the lock threads take to start
    let rings = lock(&RINGS).clone();
    // alloc: a dump, on demand
    let mut threads = Vec::new();
    for ring in rings {
        if !ring.live.load(Ordering::Acquire) {
            continue;
        }
        let (ids, depth) = ring.entered_now();
        if depth == 0 {
            continue;
        }
        let open = ids
            .iter()
            .filter_map(|&id| {
                let opened = by_number(ring_of(id))?.opened(id)?;
                let meta = meta(opened.site)?;
                // Read after the span's start, not once before the walk: a span its thread
                // opened while the walk read the rings before it started after such a reading,
                // and `now - start` wrapped to 2^64 ticks, said as 9223372036.9 s. A start past
                // the reading (another core's counter a tick ahead) is a span just begun.
                let now = clock::ticks();
                Some(OpenSpan {
                    name: meta.name(),
                    target: meta.target(),
                    for_ns: scale.ns(now.saturating_sub(opened.start)),
                    krate: token_text(opened.krate),
                    function: token_text(opened.function),
                    unit: token_text(opened.unit),
                    awaits: opened.said().value::<span::col::Awaits>().and_then(span::Name::text),
                    owner: opened.said().value::<span::col::Owner>(),
                    build: opened.build,
                    node: opened.said().value::<span::col::Node>(),
                })
            })
            // alloc: a dump, on demand
            .collect();
        let (tid, name) = ring.current();
        // alloc: a dump, on demand
        let name = self::name(name).as_deref().unwrap_or_default().to_owned();
        threads.push(ThreadNow {
            tid: u64::from(tid),
            name,
            open,
            deeper: depth.saturating_sub(OPEN_MAX as u32),
        });
    }
    threads
}

// --- The Chrome trace of a window ---------------------------------------------------------

/// Microseconds since the Unix epoch of instants, from one reading of both clocks.
pub(crate) struct Epoch {
    now: Instant,
    now_us: u64,
    mono_ns: i64,
}

impl Epoch {
    pub(crate) fn now() -> Epoch {
        let now_us =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
        Epoch { now: Instant::now(), now_us, mono_ns: clock::mono_ns() }
    }

    pub(crate) fn us(&self, at: Instant) -> u64 {
        self.now_us.saturating_sub(self.now.saturating_duration_since(at).as_micros() as u64)
    }

    fn us_of_mono(&self, ns: i64) -> u64 {
        self.now_us.saturating_sub((self.mono_ns - ns).max(0) as u64 / 1000)
    }
}

/// What every ring holds of the last `window`, as one process's part of a profile.
pub(crate) fn part(window: Duration) -> Part {
    let epoch = Epoch::now();
    let since = epoch.now.checked_sub(window).unwrap_or(epoch.now);
    let mut read = Read::default();
    self::window(window, &mut read);
    let mut part = Part { pid: std::process::id(), command: crate::command(), ..Part::default() };
    let mut named: HashSet<u32, Global> = HashSet::new_in(Global);
    for span in &read.spans {
        if named.insert(span.thread) {
            // alloc: a dump, on demand
            let thread = name(span.thread_name).as_deref().unwrap_or_default().to_owned();
            part.threads.push((u64::from(span.thread), thread));
        }
        let Some(meta) = meta(span.site) else { continue };
        part.events.push(Event {
            // alloc: a dump, on demand
            name: meta.name().to_owned(),
            // alloc: a dump, on demand
            subject: name(span.function).as_deref().map(str::to_owned),
            tid: u64::from(span.thread),
            start_us: epoch.us_of_mono(span.start_ns),
            dur_us: span.dur_ns.max(0) as u64 / 1000,
            cpu_us: None,
        });
    }
    for counter in &read.counters {
        part.counters.push(Counter {
            // alloc: a dump, on demand
            name: counter.plot.name().to_owned(),
            tid: 0,
            at_us: epoch.us_of_mono(counter.t_ns),
            value: counter.value,
        });
    }
    for end in &read.ends {
        part.events.push(Event {
            // alloc: a dump, on demand
            name: format!("build {} ended", end.build),
            subject: None,
            tid: u64::from(end.thread),
            start_us: epoch.us_of_mono(end.t_ns),
            dur_us: 0,
            cpu_us: None,
        });
    }
    let (samples, sampler) = crate::sampler::taken(since, &epoch);
    part.samples = samples;
    part.sampler = sampler;
    part.fibers = crate::time::trace::of_read(&read, part.pid, |ns| epoch.us_of_mono(ns) as f64);
    part
}

/// The last `window` of every thread's ring and of the CPU samples, as the Chrome trace
/// (ui.perfetto.dev, [`crate::dump`]): what a stall writes beside its error.
pub fn dump(window: Duration) -> Vec<u8> {
    let part = part(window);
    let end = Epoch::now().now_us;
    let start = end.saturating_sub(window.as_micros() as u64);
    // alloc: a dump, on demand
    let mut out = Vec::new();
    match crate::dump::chrome(&mut out, std::slice::from_ref(&part), start, end) {
        Ok(()) => out,
        Err(why) => unreachable!("a trace written to memory fails only when out of memory: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plot_row(i: u64) -> [u64; WORDS] {
        [i, i, i, i, KIND_PLOT << 62, i, i, i]
    }

    #[test]
    fn a_ring_keeps_the_last_rows_whole_and_in_order() {
        let ring = Ring::new(1);
        for i in 0..(CAP as u64 + 100) {
            ring.put(plot_row(i));
        }
        let mut rows = Vec::new();
        let next = ring.read(0, &mut rows);
        assert_eq!(next, CAP as u64 + 100);
        // The row after the head may be half written by the time a reader looks: dropped.
        assert_eq!(rows.len(), CAP - 1);
        assert!(rows.windows(2).all(|it| it[1].0 == it[0].0 + 1));
        assert!(rows.iter().all(|(i, row)| row[0] == *i && row[7] == *i));
        rows.clear();
        assert_eq!(ring.read(next, &mut rows), next);
        assert!(rows.is_empty(), "a cursor at the head takes nothing");
    }

    #[test]
    fn a_ring_read_while_written_has_no_torn_row() {
        let ring: &'static Ring = Box::leak(Box::new(Ring::new(1)));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    ring.put(plot_row(i));
                    i += 1;
                }
            })
        };
        let mut rows = Vec::new();
        for _ in 0..200 {
            rows.clear();
            ring.read(0, &mut rows);
            for (_, row) in &rows {
                assert!(
                    row.iter().all(|it| *it == row[0] || *it == KIND_PLOT << 62),
                    "a row's words are of one write"
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    /// A thread holds the open spans of every fiber parked with spans it made: far past one
    /// chunk, each slot its own, and a freed slot is the next taken.
    #[test]
    fn a_ring_holds_thousands_of_open_spans_and_reuses_a_freed_slot() {
        let ring = Ring::new(1);
        let taken: Vec<usize> = (0..3000).map(|_| ring.take_slot().expect("a free slot")).collect();
        assert!(taken.iter().enumerate().all(|(i, at)| *at == i), "the lowest free slot first");
        assert!(
            taken.iter().all(|&at| ring.slot(at).is_some()),
            "every taken slot's chunk is made"
        );
        assert!(ring.slot(3000 / CHUNK * CHUNK + CHUNK).is_none(), "no chunk before it is needed");
        let id = u64::from(ring.number) << (SLOT_BITS + SEQ_BITS) | 700 << SEQ_BITS | 1;
        let slot = ring.slot_of(id).unwrap();
        slot.id.store(id, Ordering::Release);
        ring.free(slot, id);
        assert_eq!(ring.take_slot(), Some(700));
        while ring.take_slot().is_some() {}
        assert!(
            ring.taken.iter().all(|it| it.load(Ordering::Relaxed) == u64::MAX),
            "full at {SLOTS_OPEN}"
        );
    }

    #[test]
    fn spans_entered_and_left_out_of_turn_leave_the_right_ones_open() {
        let (a, b, c) = (11, 22, 33);
        std::thread::spawn(move || {
            entered(a);
            entered(b);
            entered(c);
            exited(b);
            let ring = ring().unwrap();
            assert_eq!(ring.entered_now(), (vec![a, c], 2));
            exited(c);
            exited(a);
            assert_eq!(ring.entered_now(), (vec![], 0));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_thread_that_ended_lets_its_ring_go_to_the_next() {
        let (first, lease) = std::thread::spawn(|| {
            let ring = ring().unwrap();
            (ring.number, ring.lease.load(Ordering::Relaxed))
        })
        .join()
        .unwrap();
        let ring = by_number(usize::from(first)).unwrap();
        // Other tests' threads take and leave rings meanwhile, the lowest free first, so the
        // ring is free or has passed to another thread, with a new lease. A thread taking it
        // marks it live before it bumps the lease, so a ring seen live under the old lease is
        // one being taken, or one the ended thread still holds: the bound says which.
        blaze_graph::spin::SpinWait::within(
            "the ended thread to have let its ring go",
            Duration::from_secs(10),
            || !ring.live.load(Ordering::Acquire) || ring.lease.load(Ordering::Relaxed) != lease,
        );
    }

    #[test]
    fn a_name_is_one_token_on_its_thread_and_reads_back() {
        let a = intern("blaze_profile::flight::tests");
        assert_ne!(a, 0);
        assert_eq!(intern("blaze_profile::flight::tests"), a);
        assert_eq!(token_text(a).as_deref(), Some("blaze_profile::flight::tests"));
        assert_ne!(intern("another name"), a);
        assert_eq!(token_text(0), None);
        assert_eq!(name(0), None);
        let sym = read_sym("blaze_profile::flight::tests");
        assert_eq!(name(sym).as_deref(), Some("blaze_profile::flight::tests"));
        assert_eq!(read_sym("blaze_profile::flight::tests"), sym, "a reader's sym is one a text");
    }

    /// Threads naming the same texts and their own at once, each in its own table: every token
    /// reads back as its own text, from any thread, with no lock or atomic any of them shares.
    #[test]
    fn threads_naming_at_once_read_back_their_own_names() {
        const THREADS: usize = 16;
        const SHARED: usize = 512;
        const OWN: usize = 2048;
        // alloc: a test's tokens
        let seen: Vec<Vec<(String, Token)>> = std::thread::scope(|scope| {
            // alloc: a test's threads
            let threads: Vec<_> = (0..THREADS)
                .map(|t| {
                    scope.spawn(move || {
                        // alloc: a test's tokens
                        let mut named = Vec::new();
                        for i in 0..OWN {
                            let own = format!("thread {t} name {i}");
                            let token = intern(&own);
                            assert_eq!(token_text(token).as_deref(), Some(own.as_str()));
                            named.push((own, token));
                            let shared = format!("shared {}", (i + t * 31) % SHARED);
                            let token = intern(&shared);
                            named.push((shared, token));
                        }
                        named
                    })
                })
                .collect();
            threads.into_iter().map(|it| it.join().unwrap()).collect()
        });
        for (text, token) in seen.iter().flatten() {
            assert_ne!(*token, 0, "{text} was named");
            assert_eq!(token_text(*token).as_deref(), Some(text.as_str()), "{text} reads back");
        }
    }

    #[test]
    fn a_span_is_a_row_with_its_parent_depth_fields_and_build() {
        crate::install();
        let thread = std::thread::spawn(|| {
            let _build = building(7);
            let outer =
                tracing::info_span!(target: "blaze", "flight outer", krate = "app").entered();
            let outer_id = outer.id().unwrap().into_u64();
            {
                let _inner =
                    tracing::info_span!(target: "blaze", "flight inner", function = "app::main", unit = 3).entered();
            }
            drop(outer);
            (crate::sampler::os_tid() as u32, outer_id)
        });
        let mut cursor = Cursor::default();
        let mut read = Read::default();
        let (tid, outer_id) = thread.join().unwrap();
        self::read(&mut cursor, &mut read);
        let ours: Vec<&SpanRow> = read.spans.iter().filter(|it| it.thread == tid).collect();
        let name_of = |row: &SpanRow| meta(row.site).map(|it| it.name());
        let inner = ours
            .iter()
            .find(|it| name_of(it) == Some("flight inner"))
            .expect("the inner span is a row");
        let outer = ours
            .iter()
            .find(|it| name_of(it) == Some("flight outer"))
            .expect("the outer span is a row");
        assert_eq!(outer.span_id, outer_id);
        assert_eq!((inner.parent, inner.depth, outer.depth), (outer_id, 1, 0));
        assert_eq!((inner.build, outer.build), (7, 7));
        assert_eq!(name(outer.krate).as_deref(), Some("app"));
        assert_eq!(
            (name(inner.function).as_deref(), name(inner.unit).as_deref()),
            (Some("app::main"), Some("3"))
        );
        assert!(
            inner.start_ns >= outer.start_ns
                && inner.start_ns + inner.dur_ns <= outer.start_ns + outer.dur_ns
        );
        read.clear();
        self::read(&mut cursor, &mut read);
        assert!(read.spans.iter().all(|it| it.thread != tid), "a cursor takes a row once");
    }

    #[test]
    fn a_span_given_its_crate_and_function_after_it_began_has_them_in_its_row() {
        crate::install();
        let thread = std::thread::spawn(|| {
            let span = tracing::info_span!(
                target: "blaze",
                "flight named late",
                krate = tracing::field::Empty,
                function = tracing::field::Empty,
                unit = 5
            );
            span.record("krate", "shapes");
            span.record("function", "shapes::area::of_square");
            drop(span.entered());
            let unnamed = tracing::info_span!(target: "blaze", "flight never named", function = tracing::field::Empty);
            drop(unnamed);
            crate::sampler::os_tid() as u32
        });
        let mut cursor = Cursor::default();
        let mut read = Read::default();
        let tid = thread.join().unwrap();
        self::read(&mut cursor, &mut read);
        let row = |name: &str| {
            read.spans
                .iter()
                .find(|it| it.thread == tid && meta(it.site).map(|it| it.name()) == Some(name))
                .unwrap_or_else(|| panic!("`{name}` is a row"))
        };
        let named = row("flight named late");
        assert_eq!(
            (
                self::name(named.krate).as_deref(),
                self::name(named.function).as_deref(),
                self::name(named.unit).as_deref()
            ),
            (Some("shapes"), Some("shapes::area::of_square"), Some("5"))
        );
        assert_eq!(row("flight never named").function, 0);
    }

    /// The waits of `thread` in `read` for a key, refusing any other. A test's threads are in a
    /// scheduler of their own, which no other test's pools or drivers lock, and the tests order
    /// their threads' ends so that neither waits on the other's lock: a wait for the scheduler's
    /// lock here would be one the test made.
    fn key_waits(read: &Read, thread: u32) -> Vec<&SpanRow> {
        use blaze_graph::wait::WaitKind;
        // alloc: a test's rows
        let mut keys = Vec::new();
        for row in read.spans.iter().filter(|it| it.thread == thread) {
            let Some(wait) = span::read::<span::Wait>(row).unwrap() else { continue };
            let awaits = wait.fields.awaits.text();
            match wait.fields.on {
                WaitKind::Key => keys.push(row),
                other => panic!("thread {thread} waited on {other:?} {awaits:?}, not a key"),
            }
        }
        keys
    }

    #[test]
    fn a_thread_waiting_on_a_query_another_runs_spans_the_wait_naming_it() {
        use std::time::Duration;

        use blaze_graph::{Key, pump::Flights};

        crate::install();
        let flights: &'static Flights<u32> = Box::leak(Box::new(Flights::new()));
        let key = Key([0x5a; 32]);
        let named = |it: &mut String| it.push_str("slow_query(of_square)");
        let (started, has_started) = crossbeam_channel::unbounded();
        // The two threads' ends each lock the scheduler their wait joined: the asker ends once
        // the run's end let go of it, and the runner once the asker ended, so neither waits on
        // the other's lock.
        let (ran_out, has_ran_out) = crossbeam_channel::unbounded();
        let (asked_out, has_asked_out) = crossbeam_channel::unbounded::<()>();
        let runner = std::thread::spawn(move || {
            let got = flights.get_or_run_named(key, named, || {
                started.send(()).unwrap();
                blaze_graph::spin::busy(Duration::from_millis(300));
                Ok(5)
            });
            ran_out.send(()).unwrap();
            _ = has_asked_out.recv();
            (crate::sampler::os_tid() as u32, got)
        });
        has_started.recv().unwrap();
        let asker = std::thread::spawn(move || {
            let asking = tracing::info_span!(target: "blaze", "flight asking").entered();
            let asking_id = asking.id().unwrap().into_u64();
            let got = flights.get_or_run_named(key, named, || Ok(0));
            drop(asking);
            has_ran_out.recv().unwrap();
            (crate::sampler::os_tid() as u32, asking_id, got)
        });
        let (asker, asking_id, asked) = asker.join().unwrap();
        drop(asked_out);
        let (runner, ran) = runner.join().unwrap();
        assert_eq!((ran, asked), (Ok(5), Ok(5)), "the second thread waited for the first's run");
        let mut cursor = Cursor::default();
        let mut read = Read::default();
        self::read(&mut cursor, &mut read);
        let named_on = |thread: u32, name: &str| {
            read.spans
                .iter()
                .filter(|it| it.thread == thread && meta(it.site).map(|it| it.name()) == Some(name))
                .collect::<Vec<_>>()
        };
        let [wait] = key_waits(&read, asker)[..] else { panic!("the asker waited once for a key") };
        let [asking] = named_on(asker, "flight asking")[..] else {
            panic!("the asking span is a row")
        };
        assert_eq!(asking.span_id, asking_id);
        assert_eq!(self::name(wait.function).as_deref(), Some("slow_query(of_square)"));
        let typed = span::read::<span::Wait>(wait).unwrap().expect("the wait is a `wait`").fields;
        assert_eq!(
            typed.awaits.text().as_deref(),
            Some("slow_query(of_square)"),
            "a wait says what it awaits"
        );
        assert_eq!(typed.on, blaze_graph::wait::WaitKind::Key);
        assert!(typed.owner.is_some(), "a wait for a run says whose run it is");
        assert_eq!(
            asking.get::<span::col::Awaits>(),
            None,
            "a span that waits on nothing awaits nothing"
        );
        assert_eq!((wait.parent, wait.depth), (asking_id, 1));
        assert!(
            wait.dur_ns >= 100_000_000,
            "the wait lasted while the run slept: {} ns",
            wait.dur_ns
        );
        let waited: i64 = named_on(asker, "wait")
            .iter()
            .filter(|it| it.parent == asking_id)
            .map(|it| it.dur_ns)
            .sum();
        let asking_self = asking.dur_ns - waited;
        assert!(
            asking_self < wait.dur_ns / 4,
            "the asker's own time leaves out its waits: {asking_self} ns"
        );
        assert!(key_waits(&read, runner).is_empty(), "the run's own thread waited for no key");
    }

    #[test]
    fn a_wait_behind_a_hold_names_its_holder_and_the_hold_is_a_span() {
        use std::time::Duration;

        use blaze_graph::{Key, pump::Flights};

        crate::install();
        let flights: &'static Flights<u32> = Box::leak(Box::new(Flights::new()));
        let key = Key([0x6b; 32]);
        let (held, has_held) = crossbeam_channel::unbounded();
        // As in the test above: the waiter ends once the hold's end let go of the scheduler
        // their wait joined, and the holder once the waiter ended.
        let (ended, has_ended) = crossbeam_channel::unbounded();
        let (waited, has_waited) = crossbeam_channel::unbounded::<()>();
        let holder = std::thread::spawn(move || {
            let hold = flights
                .hold_named(key, |it| it.push_str("codegen round 3 of app"))
                .expect("no one holds it");
            held.send(()).unwrap();
            blaze_graph::spin::busy(Duration::from_millis(200));
            hold.end(&9);
            ended.send(()).unwrap();
            _ = has_waited.recv();
            crate::sampler::os_tid() as u32
        });
        has_held.recv().unwrap();
        let waiter = std::thread::spawn(move || {
            let got = flights.get_or_run_named(key, |it| it.push_str("codegen round 3"), || Ok(0));
            has_ended.recv().unwrap();
            (crate::sampler::os_tid() as u32, got)
        });
        let (waiter, got) = waiter.join().unwrap();
        drop(waited);
        let holder = holder.join().unwrap();
        assert_eq!(got, Ok(9), "the waiter took the hold's value");
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let named_on = |thread: u32, name: &str| {
            read.spans
                .iter()
                .filter(|it| it.thread == thread && meta(it.site).map(|it| it.name()) == Some(name))
                .collect::<Vec<_>>()
        };
        let [hold] = named_on(holder, "hold")[..] else { panic!("the holder's hold is one span") };
        let [wait] = key_waits(&read, waiter)[..] else {
            panic!("the waiter waited once for a key")
        };
        assert_eq!(self::name(hold.function).as_deref(), Some("codegen round 3 of app"));
        let holder_of =
            span::read::<span::Hold>(hold).unwrap().expect("the hold is a `hold`").fields.holder;
        assert_eq!(
            wait.get::<span::col::Owner>(),
            Some(holder_of),
            "the wait names the hold's holder as whose it waits on"
        );
        assert!(hold.dur_ns >= 150_000_000, "the hold lasted until it ended: {} ns", hold.dur_ns);
        assert!(
            hold.start_ns <= wait.start_ns
                && wait.start_ns + wait.dur_ns <= hold.start_ns + hold.dur_ns + 50_000_000
        );
    }

    #[test]
    fn a_span_closed_on_another_thread_keeps_where_it_began() {
        crate::install();
        let (span, tid) = std::thread::spawn(|| {
            (tracing::info_span!(target: "blaze", "flight moved"), crate::sampler::os_tid() as u32)
        })
        .join()
        .unwrap();
        let id = span.id().unwrap().into_u64();
        drop(span);
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let row = read.spans.iter().find(|it| it.span_id == id).expect("the span is a row");
        assert_eq!(meta(row.site).map(|it| it.name()), Some("flight moved"));
        assert!(row.dur_ns >= 0);
        assert_eq!(row.thread, tid, "a span another thread closed names the thread that made it");
    }

    /// Spans made on a thread that ended, closed after its ring passed to the next thread (one
    /// by that thread, one by another), name the thread that made them, never the one holding
    /// the ring when they are read: a wait a pool worker began and another thread ended once
    /// read as a wait of whichever test's thread took the worker's ring next.
    #[test]
    fn spans_whose_thread_ended_name_it_after_its_ring_passed_on() {
        crate::install();
        // Named, so a row names the maker by its name as well as its tid.
        const MAKER: &str = "flight maker of spans another closes";
        let (by_taker, by_other, maker, ring_of_maker, lease) = std::thread::Builder::new()
            .name(MAKER.to_owned())
            .spawn(|| {
                let by_taker =
                    tracing::info_span!(target: "blaze", "flight outlived, closed by the taker");
                let by_other =
                    tracing::info_span!(target: "blaze", "flight outlived, closed by another");
                let ring = ring().unwrap();
                (
                    by_taker,
                    by_other,
                    crate::sampler::os_tid() as u32,
                    ring.number,
                    ring.lease.load(Ordering::Relaxed),
                )
            })
            .unwrap()
            .join()
            .unwrap();
        let ids = [&by_taker, &by_other].map(|it| it.id().unwrap().into_u64());
        let left = by_number(usize::from(ring_of_maker)).unwrap();
        let passed = || left.lease.load(Ordering::Relaxed) != lease;
        // Threads take the lowest free ring: start them, each holding its ring, until the ring
        // the maker left is held by another thread (one of these, or another test's).
        let (took, taken) = crossbeam_channel::unbounded::<u16>();
        // alloc: a test's threads
        let mut holders = Vec::new();
        let mut taker = None;
        for _ in 0..256 {
            if passed() {
                break;
            }
            let took = took.clone();
            let (to_close, close) = crossbeam_channel::bounded::<Option<tracing::Span>>(1);
            holders.push((
                to_close,
                std::thread::spawn(move || {
                    took.send(ring().unwrap().number).unwrap();
                    // The span to close here, or none.
                    drop(close.recv().unwrap());
                }),
            ));
            if taken.recv().unwrap() == ring_of_maker {
                taker = Some(holders.len() - 1);
            }
        }
        assert!(passed(), "another thread took the ring the maker left, within 256 threads");
        let holder = left.current().0;
        // Closed by another thread while the ring is another's, then by the thread holding the
        // ring under a new lease when it is one of these.
        std::thread::spawn(move || drop(by_other)).join().unwrap();
        let mut by_taker = Some(by_taker);
        for (i, (to_close, thread)) in holders.into_iter().enumerate() {
            to_close.send(if taker == Some(i) { by_taker.take() } else { None }).unwrap();
            thread.join().unwrap();
        }
        drop(by_taker);
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        for id in ids {
            let row = read.spans.iter().find(|it| it.span_id == id).expect("the span is a row");
            assert_eq!(
                (row.thread, name(row.thread_name).as_deref()),
                (maker, Some(MAKER)),
                "`{:?}` names the thread that made it, not {holder}, which took its ring",
                meta(row.site).map(|it| it.name())
            );
        }
    }

    thread_local! {
        static TEST_COPIED: Cell<FrameCopied> = const { Cell::new(FrameCopied { copies: 0, bytes: 0 }) };
    }

    fn test_copied() -> FrameCopied {
        TEST_COPIED.with(Cell::get)
    }

    fn copy(copies: u64, bytes: u64) {
        TEST_COPIED.with(|it| {
            let was = it.get();
            it.set(FrameCopied { copies: was.copies + copies, bytes: was.bytes + bytes });
        });
    }

    #[test]
    fn a_span_has_the_frame_copies_made_while_it_was_open_on_its_thread() {
        crate::install();
        // The one test that counts copies: a process takes one reader.
        count_frame_copies(test_copied).expect("no other test gives a reader");
        let (copying, quiet, outer, moved) = std::thread::spawn(|| {
            copy(1, 10);
            let outer = tracing::info_span!(target: "blaze", "copies outer").entered();
            let outer_id = outer.id().unwrap().into_u64();
            let copying = {
                let it = tracing::info_span!(target: "blaze", "copies inner").entered();
                copy(2, 300);
                it.id().unwrap().into_u64()
            };
            let quiet = {
                let it = tracing::info_span!(target: "blaze", "copies none").entered();
                it.id().unwrap().into_u64()
            };
            copy(1, 7);
            drop(outer);
            let moved = tracing::info_span!(target: "blaze", "copies moved");
            (copying, quiet, outer_id, moved)
        })
        .join()
        .unwrap();
        let moved_id = moved.id().unwrap().into_u64();
        copy(5, 5);
        drop(moved);
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let copied = |id: u64| {
            read.spans.iter().find(|it| it.span_id == id).expect("the span is a row").copied
        };
        assert_eq!(copied(copying), Some(FrameCopied { copies: 2, bytes: 300 }));
        assert_eq!(copied(quiet), Some(FrameCopied::default()), "counted, and none were made");
        assert_eq!(
            copied(outer),
            Some(FrameCopied { copies: 3, bytes: 307 }),
            "a span counts those of the spans in it"
        );
        assert_eq!(copied(moved_id), None, "two threads' counts don't subtract");
    }

    /// What [`Carry`]'s jobs saw: their span ids and the threads they ran on.
    #[derive(Default)]
    struct CarrySeen {
        outer: u64,
        inner: u64,
        after: u64,
        other: u64,
        left: u32,
        reached: u32,
        other_on: u32,
    }

    /// A job inside two spans, on the slow core, that moves to the fast core at a query miss
    /// once the fast core frees, opens a span there, then waits for a job on the core it left.
    struct Carry {
        started: AtomicBool,
        seen: Lock<CarrySeen>,
    }

    enum CarryJob {
        Block,
        Moving,
        Other,
    }

    /// Read in a function of its own: a thread-local read must not span a wait in one function.
    #[inline(never)]
    fn tid() -> u32 {
        crate::sampler::os_tid() as u32
    }

    fn id(span: &tracing::Span) -> u64 {
        span.id().unwrap().into_u64()
    }

    impl blaze_graph::sched::fiber::Work for &'static Carry {
        type Job = CarryJob;

        fn run(&self, cx: &blaze_graph::sched::fiber::Cx<'_, Self>, job: CarryJob) {
            use blaze_graph::sched::{Affinity, CoreId, Spawn};
            match job {
                CarryJob::Block => {
                    blaze_graph::spin::SpinWait::within(
                        "the moving job to start",
                        Duration::from_secs(60),
                        || self.started.load(Ordering::Acquire),
                    );
                }
                CarryJob::Moving => {
                    let _build = building(41);
                    let outer = tracing::info_span!(target: "blaze", "carry outer").entered();
                    let inner = tracing::info_span!(target: "blaze", "carry inner").entered();
                    let left = tid();
                    self.started.store(true, Ordering::Release);
                    let deadline = std::time::Instant::now() + Duration::from_secs(60);
                    while tid() == left {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the job never moved to the fast core"
                        );
                        cx.miss();
                    }
                    let reached = tid();
                    let after = tracing::info_span!(target: "blaze", "carry after");
                    let after_id = id(&after);
                    drop(after);
                    let other =
                        cx.spawn(CarryJob::Other, Spawn::root().hint(Affinity::Core(CoreId(1))));
                    cx.wait_job(other).expect("waiting for a child is no cycle");
                    let mut seen = lock(&self.seen);
                    (seen.outer, seen.inner, seen.after) = (id(&outer), id(&inner), after_id);
                    (seen.left, seen.reached) = (left, reached);
                    drop(seen);
                    drop(inner);
                    drop(outer);
                }
                CarryJob::Other => {
                    let other = tracing::info_span!(target: "blaze", "carry other");
                    let mut seen = lock(&self.seen);
                    (seen.other, seen.other_on) = (id(&other), tid());
                }
            }
        }
    }

    #[test]
    fn a_span_opened_before_a_wait_closes_after_its_job_resumes_on_another_worker() {
        use blaze_graph::sched::{Affinity, CoreId, Cores, Spawn, fiber};
        crate::install();
        static CARRY: OnceLock<Carry> = OnceLock::new();
        let work = CARRY.get_or_init(|| Carry {
            started: AtomicBool::new(false),
            seen: Lock::unrecorded("profile.test.flight.carry_seen", CarrySeen::default()),
        });
        let cores = Cores::classes(&[(1024, 1), (512, 1)]).unwrap();
        let config = fiber::Config { fibers: 4, stack: 256 << 10, spin: 64 };
        let driver =
            fiber::Driver::start(cores, work, config, |_| Ok(())).expect("the driver starts");
        driver.spawn(CarryJob::Block, Spawn::root().hint(Affinity::Core(CoreId(0))));
        driver.spawn(CarryJob::Moving, Spawn::root().critical(true));
        driver.join().unwrap();
        let seen = lock(&work.seen);
        assert_ne!(seen.left, seen.reached, "the job resumed on another worker");

        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let span =
            |id: u64| *read.spans.iter().find(|it| it.span_id == id).expect("the span is a row");
        let (outer, inner, after, other) =
            (span(seen.outer), span(seen.inner), span(seen.after), span(seen.other));
        assert_eq!((inner.parent, inner.depth), (seen.outer, 1));
        assert_eq!(
            (after.parent, after.depth),
            (seen.inner, 2),
            "a span made after the move nests under the job's"
        );
        assert_eq!(after.thread, seen.reached);
        assert_eq!(
            (outer.thread, inner.thread),
            (seen.left, seen.left),
            "a span's thread is the one it began on"
        );
        assert!(
            inner.start_ns + inner.dur_ns >= after.start_ns + after.dur_ns,
            "the inner span closed after the move"
        );
        assert_eq!(
            (outer.build, inner.build, after.build),
            (41, 41, 41),
            "the job's build moved with it"
        );
        assert_eq!(other.thread, seen.left, "the other job ran on the thread the moving one left");
        assert_eq!(
            (other.parent, other.depth),
            (0, 0),
            "the other thread's next span is not inside the moved job's"
        );
        assert_eq!(other.build, 0, "nor of its build");

        let job = read
            .switches
            .iter()
            .find(|it| it.build == 41 && it.waited == Some((WaitedOn::Moved, 0)))
            .expect("the move is a parked switch of the job's build")
            .job;
        let mut moving: Vec<&SwitchRow> = read.switches.iter().filter(|it| it.job == job).collect();
        moving.sort_by_key(|it| it.t_ns);
        assert_eq!(moving.first().map(|it| it.event), Some(Switched::Started));
        assert_eq!(moving.last().map(|it| it.event), Some(Switched::Ended));
        let moved = moving.iter().position(|it| it.waited == Some((WaitedOn::Moved, 0))).unwrap();
        let (parked, resumed) = (moving[moved], moving[moved + 1]);
        assert_eq!(
            (parked.thread, resumed.thread),
            (seen.left, seen.reached),
            "{parked:?} then {resumed:?}"
        );
        assert_eq!(resumed.event, Switched::Resumed);
        assert_eq!((parked.job, parked.fiber), (resumed.job, resumed.fiber), "one job, one fiber");
        assert!(
            moving
                .iter()
                .any(|it| it.event == Switched::Parked
                    && matches!(it.waited, Some((WaitedOn::Job, _)))),
            "the wait for the child is a parked switch too"
        );
    }

    /// Every stretch a pool's worker runs a job (from the job's start or resume to its next
    /// switch) is inside a `job` span of that job's kind on that worker: a job that parks on a
    /// key closes its span as it parks and opens another where it resumes.
    #[test]
    fn a_pool_job_is_inside_its_job_span_on_every_worker_it_runs_on() {
        use blaze_graph::{Key, pool::Pool, pump::Flights, topology::Topology};

        crate::install();
        let pool = Pool::new(&Topology::flat(2), false).expect("the pool starts");
        let flights: &'static Flights<u32> = Box::leak(Box::new(Flights::new()));
        let key = Key([0x7c; 32]);
        let named = |it: &mut String| it.push_str("job span test key");
        let since = clock::mono_ns();
        let (started, has_started) = crossbeam_channel::unbounded();
        let (first, first_done) = crossbeam_channel::unbounded();
        let (second, second_done) = crossbeam_channel::unbounded();
        pool.submit(
            None,
            move || {
                let on = tid();
                let got = flights.get_or_run_named(key, named, || {
                    started.send(()).unwrap();
                    blaze_graph::spin::busy(Duration::from_millis(200));
                    Ok(5)
                });
                (got, on)
            },
            move |got| first.send(got.unwrap()).unwrap(),
        );
        has_started.recv().unwrap();
        pool.submit(
            None,
            move || {
                let before = tid();
                let got = flights.get_or_run_named(key, named, || Ok(0));
                (got, before, tid())
            },
            move |got| second.send(got.unwrap()).unwrap(),
        );
        let (got, ran_on) = first_done.recv().unwrap();
        let (asked, asked_on, resumed_on) = second_done.recv().unwrap();
        assert_eq!((got, asked), (Ok(5), Ok(5)), "the second job waited for the first's run");
        drop(pool);

        let workers = [ran_on, asked_on, resumed_on];
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let jobs: Vec<&SpanRow> = read
            .spans
            .iter()
            .filter(|it| workers.contains(&it.thread) && it.start_ns >= since)
            .filter(|it| span::read::<span::Job>(it).ok().flatten().is_some())
            .collect();
        assert!(
            jobs.iter().all(|it| it.depth == 0 && it.parent == 0),
            "a job's span is a root: {jobs:?}"
        );
        assert!(
            jobs.len() >= 3,
            "the first job's, and the second's before its park and after its resume: {jobs:?}"
        );
        let mut switches: Vec<&SwitchRow> = read
            .switches
            .iter()
            .filter(|it| workers.contains(&it.thread) && it.t_ns >= since)
            .collect();
        switches.sort_by_key(|it| (it.thread, it.t_ns));
        let parked = switches.iter().any(|it| {
            it.event == Switched::Parked && matches!(it.waited, Some((WaitedOn::Key, _)))
        });
        assert!(parked, "the second job parked on the key: {switches:?}");
        for pair in switches.windows(2) {
            let [from, to] = [pair[0], pair[1]];
            if from.thread != to.thread
                || !matches!(from.event, Switched::Started | Switched::Resumed)
            {
                continue;
            }
            let covers = |it: &&SpanRow| {
                it.thread == from.thread
                    && it.start_ns < to.t_ns
                    && it.start_ns + it.dur_ns > from.t_ns
            };
            assert!(
                jobs.iter().any(covers),
                "the run from {from:?} to {to:?} is inside a job span: {jobs:?}"
            );
            let crossing = |it: &&SpanRow| {
                it.thread == from.thread
                    && it.start_ns < from.t_ns
                    && it.start_ns + it.dur_ns > to.t_ns
            };
            assert!(
                !(from.event == Switched::Resumed && jobs.iter().any(crossing)),
                "no job span stays open across a park: {jobs:?}"
            );
        }
    }

    /// The folded rows and span rows of `tid` from `cursor` on, by name.
    fn mine(cursor: &mut Cursor, tid: u32) -> (Vec<FoldRow>, Vec<String>) {
        let mut read = Read::default();
        self::read(cursor, &mut read);
        let folds = read.folds.iter().filter(|it| it.thread == tid).cloned().collect();
        let spans = read
            .spans
            .iter()
            .filter(|it| it.thread == tid)
            .filter_map(|it| meta(it.site))
            .map(|it| it.name().to_owned())
            .collect();
        (folds, spans)
    }

    #[test]
    fn short_spans_no_row_names_are_one_folded_row_and_a_span_a_row_names_stays_a_row() {
        crate::install();
        let thread = std::thread::spawn(|| {
            let tid = crate::sampler::os_tid() as u32;
            let mut cursor = Cursor::default();
            _ = mine(&mut cursor, tid);
            fold_here(Duration::from_secs(5), Duration::from_secs(5), Duration::from_secs(600));
            let _build = building(23);
            for _ in 0..100 {
                let _short = tracing::info_span!(target: "blaze", "fold short").entered();
            }
            {
                let _parent = tracing::info_span!(target: "blaze", "fold parent").entered();
                for _ in 0..3 {
                    let _child = tracing::info_span!(target: "blaze", "fold child").entered();
                }
                let _named = tracing::info_span!(target: "blaze", "fold named").entered();
                tracing::info!(target: "blaze", "fold names its span");
            }
            let typed = span::open_raw(span::Job {
                kind: span::Name::kept("fold typed"),
                job: 9,
                creator: None,
            });
            span::close_raw(typed);
            let wait = span::open_raw(span::Wait {
                awaits: span::Name::kept("fold wait"),
                function: span::Name::kept("fold wait"),
                owner: None,
                on: blaze_graph::wait::WaitKind::Channel,
            });
            span::close_raw(wait);
            let mut read = Read::default();
            self::read(&mut cursor, &mut read);
            let spans: Vec<&SpanRow> = read.spans.iter().filter(|it| it.thread == tid).collect();
            let named = |it: &SpanRow| meta(it.site).map(|it| it.name());
            assert!(
                read.folds.iter().all(|it| it.thread != tid),
                "a sum younger than its age is held: {:?}",
                read.folds
            );
            for gone in ["fold short", "fold child", <span::Wait as span::Span>::NAME] {
                assert!(
                    !spans.iter().any(|it| named(it) == Some(gone)),
                    "{gone}, short and named by no row, is no row: {spans:?}"
                );
            }
            for kept in ["fold parent", "fold named", <span::Job as span::Span>::NAME] {
                assert!(
                    spans.iter().any(|it| named(it) == Some(kept)),
                    "{kept} is a row: one an event names, its parent, a typed span: {spans:?}"
                );
            }
            let parent = spans
                .iter()
                .find(|it| named(it) == Some("fold parent"))
                .expect("the parent is a row");
            assert_eq!(parent.folded_spans, 3, "a parent says the children it folded: {parent:?}");
            assert!(parent.folded_ns > 0 && parent.folded_ns <= parent.dur_ns, "{parent:?}");
            flush_folds();
            let (folds, _) = mine(&mut cursor, tid);
            let sum = |name: &str| {
                folds
                    .iter()
                    .find(|it| meta(it.site).map(|it| it.name()) == Some(name))
                    .unwrap_or_else(|| panic!("a sum of {name}: {folds:?}"))
            };
            assert_eq!(folds.len(), 3, "one sum a site and parent: {folds:?}");
            let short = sum("fold short");
            assert_eq!(
                (short.spans, short.root, short.build, short.parent_site),
                (100, true, 23, 0)
            );
            assert!(
                short.dur_ns >= short.max_dur_ns
                    && short.to_ns >= short.from_ns
                    && short.max_dur_ns > 0,
                "{short:?}"
            );
            let child = sum("fold child");
            assert_eq!((child.spans, child.root, child.parent_site), (3, false, parent.site));
            assert_eq!(
                child.dur_ns, parent.folded_ns,
                "the parent's folded time is its children's sum"
            );
            assert_eq!(
                sum(<span::Wait as span::Span>::NAME).spans,
                1,
                "a wait shorter than its bound folds, counted"
            );
            for _ in 0..10 {
                let _short = tracing::info_span!(target: "blaze", "fold at the end").entered();
            }
            (tid, cursor)
        });
        let (tid, mut cursor) = thread.join().unwrap();
        let (folds, _) = mine(&mut cursor, tid);
        let ended: Vec<_> = folds
            .iter()
            .filter(|it| meta(it.site).map(|it| it.name()) == Some("fold at the end"))
            .collect();
        assert!(
            matches!(ended.as_slice(), [it] if it.spans == 10),
            "a thread that ends writes its sums as its ring passes on: {folds:?}"
        );
    }

    #[test]
    fn a_job_given_to_the_pool_inside_a_span_names_it_as_its_creator() {
        use blaze_graph::{pool::Pool, topology::Topology};

        crate::install();
        let pool = Pool::new(&Topology::flat(1), false).expect("the pool starts");
        let (done, ran) = crossbeam_channel::bounded(1);
        let giver = tracing::info_span!(target: "blaze", "creator test giver").entered();
        let creator = giver.id().unwrap().into_u64();
        pool.submit(None, tid, move |on| done.send(on.unwrap()).unwrap());
        drop(giver);
        let on = ran.recv().unwrap();
        drop(pool);
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let named: Vec<Option<u64>> = read
            .spans
            .iter()
            .filter(|it| it.thread == on)
            .filter_map(|it| span::read::<span::Job>(it).ok().flatten())
            .map(|it| it.fields.creator)
            .collect();
        assert!(
            named.contains(&Some(creator)),
            "the job's span names the span it was given in, {creator}: {named:?}"
        );
    }

    /// A job tree a pool runs: job 0 is given to the pool from the test's thread, each other
    /// job by its parent from inside the parent's run. A job asks one of two keys or none, so
    /// one job parks on another's run of a key.
    #[derive(Debug)]
    struct Tree {
        parent: Vec<usize>,
        key: Vec<Option<u8>>,
        spin_us: Vec<u64>,
    }

    fn tree(tc: &hegel::TestCase) -> Tree {
        use hegel::generators as gs;
        let jobs = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let mut it = Tree { parent: vec![0], key: Vec::new(), spin_us: Vec::new() };
        for i in 1..jobs {
            it.parent.push(tc.draw(gs::integers::<usize>().max_value(i - 1)));
        }
        for _ in 0..jobs {
            let key = tc.draw(gs::integers::<u8>().max_value(2));
            it.key.push((key < 2).then_some(key));
            it.spin_us.push(tc.draw(gs::integers::<u64>().max_value(300)));
        }
        it
    }

    /// A case's run. Its jobs hold the pool weakly: the test's thread holds the one strong
    /// handle, so the pool is dropped (and its workers joined) there and never on a worker.
    struct Case {
        tree: Tree,
        build: u32,
        flights: blaze_graph::pump::Flights<u32>,
        pool: std::sync::Weak<blaze_graph::pool::Pool>,
        done: crossbeam_channel::Sender<(usize, bool)>,
    }

    /// Gives `job` to `pool`: called with the test's handle for the root, and with a parent
    /// job's upgrade, which lives only for the parent's run, for the rest.
    fn give(pool: &blaze_graph::pool::Pool, case: std::sync::Arc<Case>, job: usize) {
        let done = case.done.clone();
        pool.submit(
            None,
            move || run_job(&case, job),
            move |it| done.send((job, it.is_ok())).unwrap(),
        );
    }

    fn run_job(case: &std::sync::Arc<Case>, job: usize) {
        let _build = building(case.build);
        let spin = Duration::from_micros(case.tree.spin_us[job]);
        match case.tree.key[job] {
            Some(key) => {
                let mut bytes = [key; 32];
                bytes[..4].copy_from_slice(&case.build.to_le_bytes());
                let got = case.flights.get_or_run_named(
                    blaze_graph::Key(bytes),
                    |it| it.push_str("pool run key"),
                    || {
                        blaze_graph::spin::busy(spin);
                        Ok(0)
                    },
                );
                assert!(got.is_ok(), "nothing cancels a key's run here");
            }
            None => blaze_graph::spin::busy(spin),
        }
        let children: Vec<usize> =
            (1..case.tree.parent.len()).filter(|&it| case.tree.parent[it] == job).collect();
        if children.is_empty() {
            return;
        }
        // The test's thread drops its handle only once every job's `done` came, and this job's
        // comes after this run returns.
        let pool = case.pool.upgrade().expect("the test holds the pool until every job is done");
        for child in children {
            give(&pool, std::sync::Arc::clone(case), child);
        }
    }

    /// The rows of `build`'s jobs: their switches, by the jobs whose `started` or `ran` row is
    /// of it, and the spans under a `job` span naming one of them.
    fn of_build(read: &Read, build: u32) -> Read {
        let mut jobs: std::collections::HashSet<u64> = read
            .switches
            .iter()
            .filter(|it| it.build == build && matches!(it.event, Switched::Started | Switched::Ran))
            .map(|it| it.job)
            .collect();
        let began: Vec<u64> = read
            .switches
            .iter()
            .filter(|it| jobs.contains(&it.job))
            .filter_map(|it| it.began_as)
            .collect();
        jobs.extend(began);
        let spans: std::collections::HashMap<u64, &SpanRow> =
            read.spans.iter().map(|it| (it.span_id, it)).collect();
        // A fn, not a closure: an annotated closure argument is higher-ranked, and walking up to a
        // parent from the map needs the argument and the map's rows to share one lifetime.
        fn root<'a>(
            spans: &std::collections::HashMap<u64, &'a SpanRow>,
            mut it: &'a SpanRow,
        ) -> &'a SpanRow {
            while let Some(parent) =
                spans.get(&it.parent).copied().filter(|_| it.parent != 0 && it.depth > 0)
            {
                it = parent;
            }
            it
        }
        let root_job = |it| root(&spans, it).get::<span::col::OfJob>();
        Read {
            switches: read.switches.iter().filter(|it| jobs.contains(&it.job)).copied().collect(),
            spans: read
                .spans
                .iter()
                .filter(|it| root_job(it).is_some_and(|job| jobs.contains(&job)))
                .copied()
                .collect(),
            ..Read::default()
        }
    }

    /// On a recorded pool run, the rows the pool writes account for each job's whole life:
    /// every job lane runs from its `started` (or `ran`) row's `since` to its `ended` (or `ran`)
    /// row with no unrecorded time, and the critical path crosses from each job's start to its
    /// spawner and from each wait to its waker without a gap, from the build's first row to its
    /// last.
    #[hegel::test(test_cases = 40)]
    fn a_recorded_pool_run_accounts_for_every_jobs_wall_time_and_its_critical_path_spans_the_build(
        tc: hegel::TestCase,
    ) {
        use blaze_graph::{pool::Pool, pump::Flights, topology::Topology};

        use crate::time::{Begin, End, Input, Model, path};
        // The rings outlive each case's pool, so each case's rows are told apart by build.
        static BUILDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(7000);
        crate::install();
        let pool =
            std::sync::Arc::new(Pool::new(&Topology::flat(2), false).expect("the pool starts"));
        let tree = tree(&tc);
        let jobs = tree.parent.len();
        let build = BUILDS.fetch_add(1, Ordering::Relaxed);
        let (done, finished) = crossbeam_channel::unbounded();
        let case = Case {
            tree,
            build,
            flights: Flights::new(),
            pool: std::sync::Arc::downgrade(&pool),
            done,
        };
        give(&pool, std::sync::Arc::new(case), 0);
        for _ in 0..jobs {
            let (job, ok) = finished
                .recv_timeout(Duration::from_secs(60))
                .expect("every job finishes within a minute");
            assert!(ok, "job {job} of build {build} panicked");
        }
        // A worker writes a job's `ended` (or `ran`) row after its `done`; joining the workers
        // is what makes every one of them written before the read.
        let pool =
            std::sync::Arc::into_inner(pool).expect("no job holds the pool once every job is done");
        drop(pool);
        let mut all = Read::default();
        window(Duration::from_secs(600), &mut all);
        let read = of_build(&all, build);
        let ended = read
            .switches
            .iter()
            .filter(|it| matches!(it.event, Switched::Ended | Switched::Ran))
            .count();
        assert_eq!(
            ended, jobs,
            "build {build}: one end row a job, all written once the pool's workers are joined"
        );
        let input = Input {
            build: u64::from(build),
            ..Input::of_read(&read, 1).expect("the recorder's rows read")
        };
        let m = Model::build(input).expect("the build has rows");

        let lanes: Vec<_> = m.lanes.iter().filter(|it| it.id.job().is_some()).collect();
        assert_eq!(lanes.len(), jobs, "one lane a job: {:?}", read.switches);
        for lane in lanes {
            assert_eq!(
                (lane.begin, lane.end),
                (Begin::Started, End::Ended),
                "{}: {:?}",
                lane.id,
                lane.intervals
            );
            let totals = lane.totals();
            assert_eq!(totals.unrecorded_ns, 0, "{}: {:?}", lane.id, lane.intervals);
            assert_eq!(totals.total(), lane.end_ns() - lane.begin_ns(), "{}", lane.id);
        }
        let path = path::critical(&m);
        assert_eq!(path.reached, path::Reached::BuildStart, "{:?}", path.links);
        for pair in path.links.windows(2) {
            assert_eq!(pair[0].to_ns, pair[1].from_ns, "links meet: {:?}", path.links);
        }
        assert_eq!(
            (path.start_ns(), path.end_ns()),
            (Some(m.bounds.start_ns), Some(m.bounds.end_ns)),
            "{:?}",
            path.links
        );
    }

    #[test]
    fn a_fiber_stack_row_reads_as_four_counters_of_its_fiber_and_job() {
        crate::install();
        let job = u64::MAX - 5;
        std::thread::spawn(move || {
            let _build = building(9);
            fiber_stack(FiberStack {
                fiber: u32::MAX - 1,
                job,
                reserved: 16 << 20,
                deepest: 300 << 10,
                touched: 320 << 10,
                grown: 1 << 20,
            });
            plot(Plot::Queued, 1);
        })
        .join()
        .unwrap();
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        let ours: Vec<(Plot, u64)> = read
            .counters
            .iter()
            .filter(|it| it.fiber == Some((u32::MAX - 1, job)))
            .inspect(|it| assert_eq!(it.build, 9))
            .map(|it| (it.plot, it.value))
            .collect();
        assert_eq!(
            ours,
            [
                (Plot::FiberStackReserved, 16 << 20),
                (Plot::FiberStackDeepest, 300 << 10),
                (Plot::FiberStackTouched, 320 << 10),
                (Plot::FiberStackGrown, 1 << 20),
            ]
        );
        assert!(
            read.counters
                .iter()
                .filter(|it| it.plot == Plot::Queued && it.build == 9)
                .all(|it| it.fiber.is_none()),
            "a plot is of no fiber"
        );
    }

    thread_local! {
        static TEST_CPU: Cell<u64> = const { Cell::new(0) };
    }

    fn test_cpu() -> Option<u64> {
        Some(TEST_CPU.with(Cell::get))
    }

    /// Spends `ms` milliseconds on this thread: the wall's and, whatever the machine does, the
    /// test's CPU clock's, so a stretch's rate is its CPU over its wall to the recorder's own
    /// overhead.
    fn spend(ms: u64) {
        blaze_graph::spin::busy(Duration::from_millis(ms));
        TEST_CPU.with(|it| it.set(it.get() + ms * 1_000_000));
    }

    fn row_named<'a>(read: &'a Read, name: &str) -> &'a SpanRow {
        read.spans
            .iter()
            .find(|it| meta(it.site).map(|it| it.name()) == Some(name))
            .unwrap_or_else(|| panic!("`{name}` is a row"))
    }

    /// `got` is `ms` milliseconds to the error bound of [`crate::time::cpu`] on the test's clock:
    /// its stretches are on CPU but for the recorder's and the test's own time between spends,
    /// well under 5% and 300 µs.
    fn near(got: Option<u64>, ms: u64, what: &str) {
        let want = ms * 1_000_000;
        let got = got.unwrap_or_else(|| panic!("{what}: its CPU is counted"));
        assert!(got.abs_diff(want) <= want / 20 + 300_000, "{what}: {got} ns, not {ms} ms");
    }

    #[test]
    fn a_spans_self_cpu_is_its_own_while_innermost_less_the_spans_under_it_and_what_ran_while_it_parked()
     {
        crate::install();
        // Both tests that count CPU give the same reader; a process takes the first.
        _ = count_cpu(test_cpu);
        let (parked, taken) = std::thread::spawn(|| {
            {
                let _outer = tracing::info_span!(target: "blaze", "cpu outer").entered();
                spend(10);
                let _inner = tracing::info_span!(target: "blaze", "cpu inner").entered();
                spend(20);
            }
            {
                // Held across two steps of a state machine, with the loop's other work between.
                let held = tracing::info_span!(target: "blaze", "cpu held", held = true);
                spend(40);
                held.in_scope(|| {
                    spend(7);
                    let _under = tracing::info_span!(target: "blaze", "cpu held under").entered();
                    spend(2);
                });
                spend(40);
                held.in_scope(|| spend(4));
            }
            let _build = building(3);
            // Entered with no guard, so it moves with the job's enter stack.
            let parked = span::open_raw(span::Worker { doing: span::Name::kept("cpu parked") });
            spend(5);
            let taken = take_open().expect("a job inside a span has state to carry");
            // Another job runs on the thread while the first is parked.
            {
                let _other = tracing::info_span!(target: "blaze", "cpu other job").entered();
                spend(100);
            }
            (parked, taken)
        })
        .join()
        .unwrap();
        std::thread::spawn(move || {
            spend(50);
            put_open(taken);
            spend(3);
            span::close_raw(parked);
        })
        .join()
        .unwrap();
        let mut read = Read::default();
        window(Duration::from_secs(60), &mut read);
        near(row_named(&read, "cpu inner").self_cpu_ns, 20, "cpu inner");
        near(
            row_named(&read, "cpu outer").self_cpu_ns,
            10,
            "cpu outer, less the 20 of the span under it",
        );
        near(row_named(&read, "cpu other job").self_cpu_ns, 100, "cpu other job");
        near(row_named(&read, "cpu held under").self_cpu_ns, 2, "cpu held under");
        near(
            row_named(&read, "cpu held").self_cpu_ns,
            11,
            "a held span counts its two entered steps' 7 and 4, less the 2 under it, never the 80 between",
        );
        let parked =
            read.spans.iter().find(|it| it.span_id == parked).expect("the parked span is a row");
        near(
            parked.self_cpu_ns,
            8,
            "5 before it parked and 3 after it resumed on another thread, never the other job's 100",
        );
    }

    /// A span whose CPU waits for its thread's next boundary is not read from a cursor before
    /// it: it is read once, after the boundary, with its CPU counted.
    #[test]
    fn a_span_read_before_its_threads_next_boundary_waits_for_it_and_is_never_read_uncounted() {
        crate::install();
        _ = count_cpu(test_cpu);
        let (ready, is_ready) = crossbeam_channel::bounded(0);
        let (read_once, has_read) = crossbeam_channel::bounded(0);
        let mut cursor = Cursor::default();
        let mut first = Read::default();
        self::read(&mut cursor, &mut first);
        let thread = std::thread::spawn(move || {
            let outer = tracing::info_span!(target: "blaze", "cpu waits outer").entered();
            // Closed well within `cpu::AGE` of the boundary the root's enter made: its ticks
            // wait in its row.
            drop(tracing::info_span!(target: "blaze", "cpu waits inner").entered());
            ready.send(()).unwrap();
            has_read.recv().unwrap();
            spend(1);
            // A root closing past `cpu::ROOT_AGE`: the boundary the inner span waits for.
            drop(outer);
        });
        is_ready.recv().unwrap();
        let mut before = Read::default();
        self::read(&mut cursor, &mut before);
        read_once.send(()).unwrap();
        thread.join().unwrap();
        let mut after = Read::default();
        self::read(&mut cursor, &mut after);
        let named = |read: &Read| -> Vec<SpanRow> {
            read.spans
                .iter()
                .filter(|it| meta(it.site).map(|it| it.name()) == Some("cpu waits inner"))
                .copied()
                .collect()
        };
        let (before, after) = (named(&before), named(&after));
        assert_eq!(before.len() + after.len(), 1, "read once: {before:?} then {after:?}");
        for span in before.iter().chain(&after) {
            assert!(
                span.self_cpu_ns.is_some(),
                "a cursor never reads a counted span before its rate: {span:?}"
            );
        }
    }
}

#[cfg(test)]
mod context_log_tests {
    use super::{CONTEXT_LOG, ContextLog};

    /// The log answers with the entry in force at a moment; a moment before its oldest entry
    /// is `None`, as is any once the log wrapped past it; a closed span gives way to its
    /// innermost ancestor still open, through spans closed in between.
    #[test]
    fn the_context_log_tells_the_entry_in_force_and_an_open_ancestor() {
        let log = ContextLog::new();
        let open = |id: u64| id != 0 && id != 20 && id != 30;
        assert_eq!(log.at(5, open), None, "an empty log tells nothing");
        // ticks, innermost, under it, job, fiber: 10 entered at 100, 20 inside it at 200, 30
        // inside 20 at 300, back to 10 at 400 (20 and 30 since closed), out of all at 500.
        log.put([100, 10, 0, 7, 1]);
        log.put([200, 20, 10, 7, 1]);
        log.put([300, 30, 20, 7, 1]);
        log.put([400, 10, 0, 8, 2]);
        log.put([500, 0, 0, 0, 0]);
        assert_eq!(log.at(99, open), None, "older than the oldest entry");
        assert_eq!(log.at(100, open), Some((10, 7, 1)));
        assert_eq!(log.at(250, open), Some((10, 7, 1)), "20 closed: its parent 10 is open");
        assert_eq!(
            log.at(350, open),
            Some((10, 7, 1)),
            "30 and 20 closed: 30's parent's parent, from 20's own entry"
        );
        assert_eq!(log.at(450, open), Some((10, 8, 2)), "the job and fiber of the entry");
        assert_eq!(log.at(10_000, open), Some((0, 0, 0)), "the last entry holds until the next");
        assert_eq!(log.at(350, |id| id == 10 || id == 20), Some((20, 7, 1)), "20 still open");
        // Wrapped: the first entries are gone, so their moments are too old.
        for i in 0..CONTEXT_LOG as u64 {
            log.put([1_000 + i, 40, 10, 9, 3]);
        }
        assert_eq!(log.at(450, open), None, "written over: older than the oldest");
        assert_eq!(log.at(1_000 + CONTEXT_LOG as u64, open), Some((40, 9, 3)));
        assert_eq!(
            log.at(2_000, |id| id == 10),
            Some((10, 9, 3)),
            "a closed span after a wrap gives way to its parent"
        );
        // A closed span whose parent closed too and has no entry left reads span 0.
        assert_eq!(log.at(2_000, |_| false), Some((0, 9, 3)));
    }
}
