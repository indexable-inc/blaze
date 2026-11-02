//! The memo store (docs/v2/plan/design/data.md, "query database"): every memo a `u32` row of
//! a hot column and a cold one, its value a row of a value column, and the [`Slot`]s that list
//! a key's memos.
//!
//! - A memo's hot row is what verification and a hit read: the fingerprint, where the value is,
//!   and the memo's edges as one range of a per-worker edge segment (or of the image). Its cold
//!   row is what only writers read: the key and the segment its edges hold.
//! - A value's location is one word in the hot row: the value's row and that row's generation.
//!   Evicting, refilling and moving a value to disk are each a compare-and-swap of the word, so
//!   a writer that saw a value swaps it only if the word still names that very value, however
//!   often its row was reused since.
//! - Edges and values are copied once into this thread's segments ([`crate::segment`]); a
//!   memo or value let go of lets go of its hold, and a segment is freed whole at collection.
//! - Memos are freed only by [`Database::collect`](crate::Database::collect): unlinked from
//!   their slots, then (once a grace has passed) checked against the open views, then freed
//!   once a grace begun after that check has passed. A view's results name memos by ID, so a
//!   memo a view took from a slot just before it was unlinked stays while the view is open.

use std::{
    alloc::Allocator,
    cell::UnsafeCell,
    ptr::{self, NonNull},
};

use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Fingerprint};
use blaze_reclaim::{Grace, Guard};

use crate::{
    QueryKey, QueryKind, Stored,
    segment::{self, Lanes, Segment, Segments, Span, lane},
    sync::{
        atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence},
        read_bytes,
    },
    table::{COLLECT_AT, EDGE_RECORD, Edit, EdgeRecord, Free, FxMap, Generations, Ids, Map, Rows, ShortLock},
};

/// A memo: its row in the store's hot and cold columns.
pub(crate) type MemoId = u32;

/// A value's location: `generation << 32 | row + 1`, and 0 for none (evicted).
fn word(generation: u32, row: u32) -> u64 {
    u64::from(generation) << 32 | u64::from(row + 1)
}

fn value_row(word: u64) -> Option<u32> {
    (word as u32).checked_sub(1)
}

fn split(fingerprint: Fingerprint) -> [u64; 2] {
    [fingerprint.0 as u64, (fingerprint.0 >> 64) as u64]
}

/// [`MemoHot::seen`] while a writer rewrites the inline copy.
const LOCKED: u64 = u64::MAX;

/// What a hit and verification read of a memo: one cache line, so a hit on a value in memory
/// reads this line and the value's bytes, never the value's row.
///
/// `bytes` and `len` are a copy of the value row's, kept as a seqlock: `seen` is the value
/// word they were copied for ([`LOCKED`] while being rewritten). A reader that sees `seen`,
/// the copy, `seen` again and then `value` all agree uses the copy: the word still names the
/// row the copy came from, so its bytes stay for the reader's task as the row's do.
#[repr(C, align(64))]
pub(crate) struct MemoHot {
    /// [`Fingerprint`], as two words so the row is 8-byte aligned.
    fingerprint: [u64; 2],
    /// See [`word`]: written after the memo is published only by a compare-and-swap.
    value: AtomicU64,
    seen: AtomicU64,
    bytes: AtomicPtr<u8>,
    len: AtomicUsize,
    /// `edge_len` records, in the order they were read.
    edges: NonNull<EdgeRecord>,
    edge_len: u32,
    /// The schema and producer it was made under ([`Store::code`]).
    code: u32,
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<MemoHot>() == 64);

// SAFETY: `edges` names records that never change once copied in and stay until the memo is
// freed (its cold row holds their segment, or they are the image's); the rest is atomic.
unsafe impl Send for MemoHot {}
// SAFETY: as above.
unsafe impl Sync for MemoHot {}

impl MemoHot {
    pub(crate) fn fingerprint(&self) -> Fingerprint {
        Fingerprint(u128::from(self.fingerprint[0]) | u128::from(self.fingerprint[1]) << 64)
    }

    pub(crate) fn code(&self) -> u32 {
        self.code
    }

    /// The inline copy of the value's bytes, if it names the value the word names now.
    fn inline(&self) -> Option<(*const u8, usize)> {
        let seen = self.seen.load(Ordering::Acquire);
        let bytes = self.bytes.load(Ordering::Relaxed);
        let len = self.len.load(Ordering::Relaxed);
        // Acquire: the copy's loads happen before `seen` is read again, so a rewrite that
        // began before them shows as a changed `seen`.
        fence(Ordering::Acquire);
        let again = self.seen.load(Ordering::Relaxed);
        let word = self.value.load(Ordering::Acquire);
        (seen == again && seen == word && word != 0 && seen != LOCKED).then_some((bytes.cast_const(), len))
    }
}

/// What only writers and collection read of a memo. 48 bytes.
struct MemoCold {
    key: QueryKey,
    /// The segment `MemoHot::edges` is in, with one hold on it; null for the image's.
    edges: *const Segment,
    /// The collection generation its key last resolved to it in ([`Store::born`]): set before
    /// it is published, then only raised.
    born: AtomicU32,
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<MemoCold>() == 48);

// SAFETY: `edges` is only let go of, once, when the row is dropped.
unsafe impl Send for MemoCold {}
// SAFETY: as above.
unsafe impl Sync for MemoCold {}

impl Drop for MemoCold {
    fn drop(&mut self) {
        // SAFETY: the row's one hold on its edges' segment; the row is dropped once no reader
        // can reach the memo.
        unsafe { segment::release(self.edges) };
    }
}

/// Where a value's bytes are. 64 bytes.
struct ValueRow {
    bytes: NonNull<u8>,
    len: usize,
    /// The segment `bytes` is in, with one hold on it; null for a mapping's.
    segment: *const Segment,
    /// A mapped value's content ID, which [`Stored::Disk`] names.
    id: ContentId,
    /// In a segment of this process, which [`Store::evict`] may drop.
    memory: bool,
    /// One value of a [`Shared`] kind, named by every memo of an equal value
    /// ([`Store::shared`]); otherwise the row is its one memo's.
    shared: bool,
    /// For a shared row: how many memo words name it (or are about to), and [`DEAD`] once
    /// [`Store::sweep`] found none and retired it.
    names: AtomicU32,
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<ValueRow>() == 64);

/// [`ValueRow::names`] of a shared row being retired: no memo may name it again.
const DEAD: u32 = u32::MAX;

/// A kind whose equal values are kept once ([`crate::Query::SHARE`]): its schema, which with a
/// value's fingerprint finds the row, and its name, for the panic when two values of one
/// fingerprint differ.
#[derive(Clone, Copy)]
pub(crate) struct Shared {
    pub(crate) name: &'static str,
    pub(crate) schema: SchemaHash,
}

const SHARED_DOMAIN: &str = "blaze v2 db shared value 1";

/// The key of the shared row of values of `schema` with `fingerprint`.
fn shared_key(schema: SchemaHash, fingerprint: Fingerprint) -> QueryKey {
    let mut parts = [0u8; 48];
    parts[..32].copy_from_slice(&schema.0.0);
    parts[32..].copy_from_slice(&fingerprint.0.to_le_bytes());
    QueryKey { kind: QueryKind(0), input: ContentId::of(SHARED_DOMAIN, &parts) }
}

// SAFETY: `bytes` never change once copied in or mapped, and stay while the row's hold on its
// segment does, or while the database keeps the mapping.
unsafe impl Send for ValueRow {}
// SAFETY: as above.
unsafe impl Sync for ValueRow {}

impl ValueRow {
    fn new(span: Span, memory: bool, id: ContentId, shared: bool) -> ValueRow {
        let (bytes, len, segment) = span.into_raw();
        ValueRow { bytes, len, segment, id, memory, shared, names: AtomicU32::new(u32::from(shared)) }
    }

    fn bytes(&self) -> &[u8] {
        read_bytes(self.bytes.as_ptr(), self.len);
        // SAFETY: see the `Send` impl: `len` bytes at `bytes`, unchanged while the row lives.
        unsafe { std::slice::from_raw_parts(self.bytes.as_ptr(), self.len) }
    }
}

impl Drop for ValueRow {
    fn drop(&mut self) {
        // SAFETY: the row's one hold on its segment; the row is dropped once no reader can
        // reach it.
        unsafe { segment::release(self.segment) };
    }
}

#[derive(Default)]
struct Codes {
    by: FxMap<(SchemaHash, ContentId), u32>,
    all: Vec<(SchemaHash, ContentId)>,
}

/// One lane's writer state: claiming and readying memo and value rows.
#[derive(Default)]
struct Frees {
    memos: Free,
    values: Free,
}

/// A lane's [`Frees`], touched only by the thread holding the lane, so a write takes no lock.
#[repr(align(128))]
#[derive(Default)]
struct Writers(UnsafeCell<Frees>);

// SAFETY: a lane's `Frees` is reached only through `Store::frees` by the thread holding that
// lane (`lane()`), or through `&mut Store`.
unsafe impl Sync for Writers {}

/// Memos on their way out; see the module's comment.
#[derive(Default)]
struct Dying {
    /// Unlinked from their slots, each with the grace begun after.
    unlinked: Vec<(Grace, MemoId)>,
    /// Held by an open view when last checked.
    held: Vec<MemoId>,
    /// Held by no open view when checked, each with the grace begun after the check.
    checked: Vec<(Grace, MemoId)>,
}

/// How many of `held` (in the order their graces were started) have passed their grace.
fn passed<T>(held: &[(Grace, T)]) -> usize {
    match held.last() {
        None => 0,
        Some((newest, _)) if newest.passed() => held.len(),
        Some(_) => held.iter().take_while(|(grace, _)| grace.passed()).count(),
    }
}

/// Every memo of the database and every value of a memo.
///
/// - A memo costs 64 hot bytes (one line), 48 cold bytes and 56 bytes an edge (one range of
///   an edge segment); a value 64 bytes of row, 4 of generation and its bytes (in a value
///   segment, at a 16-byte boundary). Reading a memo's fingerprint, edges or value's bytes is
///   one line loaded by index; its value's row is read only when the inline copy is stale.
pub(crate) struct Store {
    ids: Ids,
    hot: Rows<MemoHot>,
    cold: Rows<MemoCold>,
    value_ids: Ids,
    values: Rows<ValueRow>,
    generations: Generations,
    /// The word of each shared value row, by [`shared_key`]: how a memo of a [`Shared`] kind
    /// finds the row of an equal value, whichever view or key made it.
    shared: Map<u64>,
    writers: Lanes<Writers>,
    codes: ShortLock<Codes>,
    dying: ShortLock<Dying>,
    /// Memos a cold write in flight will swap the value of, each with how many: not freed.
    pins: ShortLock<FxMap<MemoId, u32>>,
    // The segments last: the rows above hold spans of them, let go of as they drop.
    edges: Segments,
    bytes: Segments,
    /// Keys' bytes and slots' memo lists.
    keys: Segments,
}

impl Store {
    pub(crate) fn new() -> Store {
        Store {
            ids: Ids::new(),
            hot: Rows::hot(),
            cold: Rows::new(),
            value_ids: Ids::new(),
            values: Rows::new(),
            generations: Generations::new(),
            shared: Map::new(),
            writers: Lanes::new(),
            codes: ShortLock::default(),
            dying: ShortLock::default(),
            pins: ShortLock::default(),
            edges: Segments::new(16 << 10, 1 << 20, align_of::<EdgeRecord>()),
            bytes: Segments::new(16 << 10, 4 << 20, blaze_format::ALIGN),
            keys: Segments::new(4 << 10, 256 << 10, align_of::<MemoId>()),
        }
    }

    /// This thread's lane's writer state, for `f`, which must not call back into it.
    fn frees<R>(&self, f: impl FnOnce(&mut Frees) -> R) -> R {
        let cell = self.writers.get(lane());
        // SAFETY: only this thread holds its lane, and no caller's `f` reaches `frees` again
        // (they claim, ready and drop rows, and dropping a row only lets go of segment holds).
        f(unsafe { &mut *cell.0.get() })
    }

    /// Reclaims this lane's retired value rows past their grace, once enough are waiting.
    fn tidy(&self, frees: &mut Frees) {
        if frees.values.retired() >= COLLECT_AT {
            let values = &self.values;
            // SAFETY: a retired value row's grace has passed: no task that could reach it is left.
            frees.values.reclaim(|row| unsafe { values.drop_row(row) });
        }
    }

    /// The number of a schema and producer pair, the same for every memo of it.
    pub(crate) fn code(&self, schema: SchemaHash, producer: ContentId) -> u32 {
        let mut codes = self.codes.lock();
        if let Some(code) = codes.by.get(&(schema, producer)) {
            return *code;
        }
        let code = u32::try_from(codes.all.len()).expect("2^32 schemas and producers");
        codes.all.push((schema, producer));
        codes.by.insert((schema, producer), code);
        code
    }

    pub(crate) fn parts(&self, code: u32) -> (SchemaHash, ContentId) {
        self.codes.lock().all[code as usize]
    }

    /// Every code's schema and producer, indexed by code: for a pass over many memos, which
    /// would otherwise take the codes' lock once a memo.
    pub(crate) fn codes(&self) -> Vec<(SchemaHash, ContentId)> {
        // clone: once per image write, a list of every schema and producer.
        self.codes.lock().all.clone()
    }

    /// Above every memo ID handed out so far.
    pub(crate) fn end(&self) -> u32 {
        self.ids.end()
    }

    fn put_value(&self, frees: &mut Frees, span: Span, memory: bool, id: ContentId, shared: bool) -> u64 {
        let row = self.value_ids.claim(&mut frees.values);
        let generation = self.generations.bump(row);
        // SAFETY: claimed just now and not yet published.
        unsafe { self.values.put(row, ValueRow::new(span, memory, id, shared)) };
        word(generation, row)
    }

    /// `bytes` copied into this thread's value segment, in a row of their own.
    fn copy_value(&self, bytes: &[u8]) -> u64 {
        let span = self.bytes.copy(bytes, blaze_format::ALIGN);
        self.frees(|frees| self.put_value(frees, span, true, ContentId::default(), false))
    }

    /// The word of the shared row of `bytes`, with a name taken on it for the memo about to
    /// name it: the row of an equal value made before (its bytes compared), or a new one.
    fn take_shared(&self, task: &Guard, shared: Shared, fingerprint: Fingerprint, bytes: &[u8]) -> u64 {
        let key = shared_key(shared.schema, fingerprint);
        loop {
            if let Some(&word) = self.shared.get(task, &key) {
                let row = value_row(word).expect("a shared value has a row");
                // SAFETY: a row named in `shared` is retired only after `Store::sweep` found no
                // name on it, and dropped after a grace begun then, which this task (which
                // loaded the word) holds back.
                let value = unsafe { self.values.get(row) };
                if value.bytes() != bytes {
                    panic!(
                        "query {} made two values of schema {:?} and fingerprint {fingerprint:?} that differ \
                         in their bytes: its `Persist` encoding is not deterministic, or the fingerprints \
                         collided",
                        shared.name, shared.schema
                    );
                }
                let mut names = value.names.load(Ordering::Relaxed);
                while names != DEAD {
                    debug_assert!(names < DEAD - 1, "2^32 memos naming one value");
                    match value.names.compare_exchange_weak(names, names + 1, Ordering::Relaxed, Ordering::Relaxed) {
                        Ok(_) => return word,
                        Err(now) => names = now,
                    }
                }
            }
            let span = self.bytes.copy(bytes, blaze_format::ALIGN);
            let made = self.frees(|frees| self.put_value(frees, span, true, ContentId::default(), true));
            // What the published word replaced: none, or a dead row's (dead stays dead).
            let mut replaced = None;
            let mut ours = false;
            self.shared.update(task, key, |now| {
                replaced = now.copied();
                ours = now.is_none_or(|&now| {
                    let row = value_row(now).expect("a shared value has a row");
                    // SAFETY: as above: this task loaded the word from `shared`.
                    unsafe { self.values.get(row) }.names.load(Ordering::Acquire) == DEAD
                });
                ours.then_some(made)
            });
            if ours {
                if let Some(dead) = replaced {
                    // Unlinked by this publish, so this maker retires it, not the sweep.
                    self.retire_shared(dead);
                }
                return made;
            }
            // A racing maker published a live row first; this one was never published.
            let row = value_row(made).expect("a value made has a row");
            self.frees(|frees| {
                // SAFETY: never published, so no reader has it.
                unsafe { self.values.drop_row(row) };
                frees.values.ready(row);
            });
        }
    }

    /// The memo's word, from the row it names: `value` is 0, a row this thread just put, or a
    /// shared row it took a name on.
    fn add(&self, key: QueryKey, fingerprint: Fingerprint, code: u32, edges: Span, value: u64) -> MemoId {
        let (bytes, value_len) = match value_row(value) {
            Some(row) => {
                // SAFETY: as the comment says, the memo about to name the row keeps it.
                let row = unsafe { self.values.get(row) };
                (row.bytes.as_ptr(), row.len)
            }
            None => (ptr::null_mut(), 0),
        };
        self.frees(|frees| {
            let memo = self.ids.claim(&mut frees.memos);
            let (at, len, segment) = edges.into_raw();
            // SAFETY: claimed just now and not yet published.
            unsafe {
                self.hot.put(
                    memo,
                    MemoHot {
                        fingerprint: split(fingerprint),
                        value: AtomicU64::new(value),
                        seen: AtomicU64::new(value),
                        bytes: AtomicPtr::new(bytes),
                        len: AtomicUsize::new(value_len),
                        edges: at.cast(),
                        edge_len: u32::try_from(len / EDGE_RECORD).expect("2^32 edges of one memo"),
                        code,
                    },
                );
                self.cold.put(memo, MemoCold { key, edges: segment, born: AtomicU32::new(0) });
            }
            memo
        })
    }

    /// A memo a run made: its edges and value copied once into this thread's segments.
    pub(crate) fn make(
        &self,
        key: QueryKey,
        fingerprint: Fingerprint,
        code: u32,
        edges: &[EdgeRecord],
        value: &[u8],
    ) -> MemoId {
        let edges = self.edges.copy(EdgeRecord::as_bytes(edges), align_of::<EdgeRecord>());
        let value = self.copy_value(value);
        self.add(key, fingerprint, code, edges, value)
    }

    /// A memo a run of a [`Shared`] kind made: its edges copied as [`Store::make`]'s, its value
    /// the row of an equal value of the kind's schema if there is one (whichever view or key
    /// made it), else copied once into a new shared row.
    pub(crate) fn make_shared(
        &self,
        key: QueryKey,
        fingerprint: Fingerprint,
        code: u32,
        edges: &[EdgeRecord],
        value: &[u8],
        shared: Shared,
    ) -> MemoId {
        let edges = self.edges.copy(EdgeRecord::as_bytes(edges), align_of::<EdgeRecord>());
        let task = blaze_reclaim::enter();
        let value = self.take_shared(&task, shared, fingerprint, value);
        self.add(key, fingerprint, code, edges, value)
    }

    /// A memo of the image: its edges and value stay in the mapping.
    ///
    /// # Safety
    ///
    /// `edges` and the value's bytes are in a mapping kept, unchanged, as long as the store
    /// lives ([`Span::mapped`]).
    pub(crate) unsafe fn make_mapped(
        &self,
        key: QueryKey,
        fingerprint: Fingerprint,
        code: u32,
        edges: &[EdgeRecord],
        value: Option<(&[u8], ContentId)>,
    ) -> MemoId {
        // SAFETY: the caller's contract.
        let edges = unsafe { Span::mapped(EdgeRecord::as_bytes(edges)) };
        let value = value.map_or(0, |(bytes, id)| {
            // SAFETY: as above.
            let span = unsafe { Span::mapped(bytes) };
            self.frees(|frees| self.put_value(frees, span, false, id, false))
        });
        self.add(key, fingerprint, code, edges, value)
    }

    /// A memo's hot row. `memo` was read in the caller's task from a slot or a view's
    /// results, or was made by the caller: memos are freed only once no task that could have
    /// read one is left and no open view holds it (see the module's comment).
    pub(crate) fn hot<'g>(&'g self, _task: &'g Guard, memo: MemoId) -> &'g MemoHot {
        // SAFETY: as the comment says, the memo is not freed while `_task` lives.
        unsafe { self.hot.get(memo) }
    }

    fn cold<'g>(&'g self, _task: &'g Guard, memo: MemoId) -> &'g MemoCold {
        // SAFETY: as in `Store::hot`.
        unsafe { self.cold.get(memo) }
    }

    pub(crate) fn fingerprint(&self, task: &Guard, memo: MemoId) -> Fingerprint {
        self.hot(task, memo).fingerprint()
    }

    pub(crate) fn key(&self, task: &Guard, memo: MemoId) -> QueryKey {
        self.cold(task, memo).key
    }

    /// The collection generation the memo's key last resolved to it in: [`crate::Database::collect`]
    /// keeps whole a key with a memo born after it began. Read after the slot that names it,
    /// which was published after the stamp.
    pub(crate) fn born(&self, task: &Guard, memo: MemoId) -> u32 {
        self.cold(task, memo).born.load(Ordering::Acquire)
    }

    /// Stamps `memo`, made by this thread and not yet published, born in `now`: a store into
    /// the cold row it just wrote, with no read-modify-write.
    pub(crate) fn born_in(&self, task: &Guard, memo: MemoId, now: u32) {
        self.cold(task, memo).born.store(now, Ordering::Release);
    }

    /// Raises a published memo's stamp to `now`, as its key resolves to it again: never lowered,
    /// so a giver that loaded the generation before a collection bumped it leaves the stamp of
    /// one that loaded it after.
    pub(crate) fn reborn_in(&self, task: &Guard, memo: MemoId, now: u32) {
        let born = &self.cold(task, memo).born;
        if born.load(Ordering::Relaxed) < now {
            born.fetch_max(now, Ordering::AcqRel);
        }
    }

    /// The memo's edges in the order they were read: one range, read in place.
    pub(crate) fn edges<'g>(&'g self, task: &'g Guard, memo: MemoId) -> &'g [EdgeRecord] {
        let hot = self.hot(task, memo);
        read_bytes(hot.edges.as_ptr().cast::<u8>(), hot.edge_len as usize * EDGE_RECORD);
        // SAFETY: `edge_len` records written at `edges` before the memo was published, held by
        // its cold row's segment hold (or in the image) until the memo is freed.
        unsafe { std::slice::from_raw_parts(hot.edges.as_ptr(), hot.edge_len as usize) }
    }

    fn value<'g>(&'g self, task: &'g Guard, memo: MemoId) -> Option<(u64, &'g ValueRow)> {
        let word = self.hot(task, memo).value.load(Ordering::Acquire);
        let row = value_row(word)?;
        // SAFETY: a value row is retired only after it is swapped out of its memo's word, and
        // dropped once a grace begun then has passed; this task loaded the word before.
        Some((word, unsafe { self.values.get(row) }))
    }

    /// The memo's value's bytes, while it is in memory or mapped: from the hot row's inline
    /// copy when it is current, else from the value's row.
    pub(crate) fn bytes<'g>(&'g self, task: &'g Guard, memo: MemoId) -> Option<&'g [u8]> {
        if let Some((bytes, len)) = self.hot(task, memo).inline() {
            read_bytes(bytes, len);
            // SAFETY: the copy of the row the word named when this task loaded it (see
            // `MemoHot`): its bytes stay unchanged until that row is dropped, which is after a
            // grace begun once the word stops naming it.
            return Some(unsafe { std::slice::from_raw_parts(bytes, len) });
        }
        self.value(task, memo).map(|(_, value)| value.bytes())
    }

    /// Rewrites the hot row's inline copy to the value the word names now, unless another
    /// writer is at it (which rereads the word when it is done).
    ///
    /// SeqCst: this load of `seen` after a swap's compare-and-swap of `value`, and the rewriter's
    /// store of `seen` followed by its reload of `value`, are store buffering. Under acquire and
    /// release both may read the old value (this swap sees `LOCKED`, the rewriter its own word
    /// still in `value`), and the copy stays stale until the next swap.
    fn refresh(&self, task: &Guard, memo: MemoId) {
        let hot = self.hot(task, memo);
        loop {
            let seen = hot.seen.load(Ordering::SeqCst);
            if seen == LOCKED || seen == hot.value.load(Ordering::Acquire) {
                return;
            }
            if hot.seen.compare_exchange(seen, LOCKED, Ordering::Acquire, Ordering::Relaxed).is_err() {
                continue;
            }
            // Release: `LOCKED` is visible before any of the copy's stores.
            fence(Ordering::Release);
            let word = hot.value.load(Ordering::Acquire);
            let (bytes, len) = match value_row(word) {
                Some(row) => {
                    // SAFETY: as in `Store::value`: this task loaded the word naming the row.
                    let value = unsafe { self.values.get(row) };
                    (value.bytes.as_ptr(), value.len)
                }
                None => (ptr::null_mut(), 0),
            };
            hot.bytes.store(bytes, Ordering::Relaxed);
            hot.len.store(len, Ordering::Relaxed);
            hot.seen.store(word, Ordering::SeqCst);
            if hot.value.load(Ordering::SeqCst) == word {
                return;
            }
        }
    }

    /// The value in memory and the word naming it, for a cold write to swap out later.
    pub(crate) fn in_memory<'g>(&'g self, task: &'g Guard, memo: MemoId) -> Option<(u64, &'g [u8])> {
        self.value(task, memo).filter(|(_, value)| value.memory).map(|(word, value)| (word, value.bytes()))
    }

    pub(crate) fn stored(&self, task: &Guard, memo: MemoId) -> Stored {
        match self.value(task, memo) {
            None => Stored::Evicted,
            Some((_, value)) if value.memory => Stored::Memory,
            Some((_, value)) => Stored::Disk(value.id),
        }
    }

    /// Lets go of a memo word's name on `row` if it is a shared row (which [`Store::sweep`]
    /// retires once none is left): `false` for a row that is its one memo's.
    fn let_go_shared(&self, row: u32) -> bool {
        // SAFETY: the caller's word names the row (or is about to and never will), and a row is
        // retired only after its memo's word stops naming it, or, shared, after every name on
        // it was let go of: this is one of them.
        let value = unsafe { self.values.get(row) };
        if !value.shared {
            return false;
        }
        // Release: this memo's reads of the row happen before the sweep that sees no name.
        let was = value.names.fetch_sub(1, Ordering::Release);
        debug_assert!(was != 0 && was != DEAD, "a shared row let go of by a memo that did not name it");
        true
    }

    /// Swaps the memo's word from `was` to `now`: the value let go of goes to the limbo, or,
    /// when the word no longer is `was`, `now`'s row (never published) is freed at once. A shared
    /// row is let go of by its name instead.
    fn swap_value(&self, task: &Guard, memo: MemoId, was: u64, now: u64) -> bool {
        // SeqCst: see `refresh`.
        let swapped = self.hot(task, memo).value.compare_exchange(was, now, Ordering::SeqCst, Ordering::Acquire);
        let (gone, unlinked) = match swapped {
            Ok(_) => (value_row(was), true),
            Err(_) => (value_row(now), false),
        };
        if let Some(row) = gone.filter(|&row| !self.let_go_shared(row)) {
            self.frees(|frees| {
                if unlinked {
                    frees.values.retire(Grace::start(), row);
                    self.tidy(frees);
                } else {
                    // SAFETY: `now` was never published, so no reader has its row.
                    unsafe { self.values.drop_row(row) };
                    frees.values.ready(row);
                }
            });
        }
        if swapped.is_ok() {
            self.refresh(task, memo);
        }
        swapped.is_ok()
    }

    /// Takes the memo's value out of memory; a reader still holding it keeps it until its task
    /// ends. `false` when it was not in memory.
    pub(crate) fn evict(&self, task: &Guard, memo: MemoId) -> bool {
        loop {
            let Some((word, value)) = self.value(task, memo) else { return false };
            if !value.memory {
                return false;
            }
            if self.swap_value(task, memo, word, 0) {
                return true;
            }
        }
    }

    /// An evicted value back in memory, copied once into this thread's segment; a racing
    /// refill's wins.
    pub(crate) fn refill(&self, task: &Guard, memo: MemoId, bytes: &[u8]) {
        let now = self.copy_value(bytes);
        self.swap_value(task, memo, 0, now);
    }

    /// An evicted value of a [`Shared`] kind back in memory: the shared row of an equal value
    /// if there is one, as [`Store::make_shared`]; a racing refill's wins.
    pub(crate) fn refill_shared(&self, task: &Guard, memo: MemoId, bytes: &[u8], shared: Shared) {
        let now = self.take_shared(task, shared, self.fingerprint(task, memo), bytes);
        self.swap_value(task, memo, 0, now);
    }

    /// The value moved to `bytes` of a mapping the database keeps, if the memo's word is still
    /// `was`: the very value the cold write copied. `false` when it changed since.
    ///
    /// # Safety
    ///
    /// `bytes` are in a mapping kept, unchanged, as long as the store lives.
    pub(crate) unsafe fn to_disk(&self, task: &Guard, memo: MemoId, was: u64, bytes: &[u8], id: ContentId) -> bool {
        // SAFETY: the caller's contract.
        let span = unsafe { Span::mapped(bytes) };
        let now = self.frees(|frees| self.put_value(frees, span, false, id, false));
        self.swap_value(task, memo, was, now)
    }

    /// Whether memo `a` of this store is memo `b` of `other`: the same fingerprint, schema,
    /// producer and edges.
    pub(crate) fn same(&self, task: &Guard, a: MemoId, other: &Store, b: MemoId) -> bool {
        let (x, y) = (self.hot(task, a), other.hot(task, b));
        let codes = if ptr::eq(self, other) { x.code == y.code } else { self.parts(x.code) == other.parts(y.code) };
        x.fingerprint == y.fingerprint && codes && self.edges(task, a) == other.edges(task, b)
    }

    /// Frees a memo no reader can reach, with its value.
    fn free(&self, memo: MemoId) {
        // SAFETY: the caller's contract: nothing reaches the memo, so its rows and value row
        // are this caller's.
        let word = unsafe { self.hot.get(memo) }.value.load(Ordering::Acquire);
        let own = value_row(word).filter(|&row| !self.let_go_shared(row));
        self.frees(|frees| {
            if let Some(row) = own {
                // SAFETY: the memo's value, reached only through the memo.
                unsafe { self.values.drop_row(row) };
                frees.values.ready(row);
            }
            // SAFETY: as above.
            unsafe {
                self.hot.drop_row(memo);
                self.cold.drop_row(memo);
            }
            frees.memos.ready(memo);
        });
    }

    /// A memo made but never published (an equal one was there already): freed at once.
    pub(crate) fn discard(&self, memo: MemoId) {
        self.free(memo);
    }

    /// Memos just unlinked from their slots.
    pub(crate) fn unlinked(&self, memos: &[MemoId]) {
        if memos.is_empty() {
            return;
        }
        // Begun under the lock, so `unlinked` stays in the order its graces began when a
        // parallel sweep's workers each add theirs.
        let mut dying = self.dying.lock();
        let grace = Grace::start();
        dying.unlinked.extend(memos.iter().map(|&memo| (grace, memo)));
    }

    /// The memos to check against the open views, before they are walked, added to `out`:
    /// those unlinked whose grace has passed (no task that read them from a slot is left, so
    /// no view can gain them any more), and those a view held at the last check.
    pub(crate) fn to_check<A: Allocator>(&self, out: &mut Vec<MemoId, A>) {
        let mut dying = self.dying.lock();
        out.extend_from_slice(&dying.held);
        dying.held.clear();
        let done = passed(&dying.unlinked);
        out.extend(dying.unlinked.drain(..done).map(|(_, memo)| memo));
    }

    /// After the open views were walked: those `held` says a view holds wait for the next
    /// check, and the rest are freed once a grace begun now has passed (a task in a view closed
    /// before the walk may still hold one).
    pub(crate) fn checked(&self, memos: &[MemoId], held: impl Fn(MemoId) -> bool) {
        if memos.is_empty() {
            return;
        }
        let grace = Grace::start();
        let mut dying = self.dying.lock();
        for &memo in memos {
            if held(memo) {
                dying.held.push(memo);
            } else {
                dying.checked.push((grace, memo));
            }
        }
    }

    /// Keeps `memos` from being freed until [`Store::unpin`]: a cold write will swap their
    /// values.
    pub(crate) fn pin(&self, memos: impl IntoIterator<Item = MemoId>) {
        let mut pins = self.pins.lock();
        for memo in memos {
            *pins.entry(memo).or_default() += 1;
        }
    }

    pub(crate) fn unpin(&self, memo: MemoId) {
        let mut pins = self.pins.lock();
        if let Some(count) = pins.get_mut(&memo) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&memo);
            }
        }
    }

    /// Frees the checked memos whose grace has passed, the values this lane let go of whose
    /// grace has passed (each lane reclaims its own as it writes; `reclaim` frees every
    /// lane's), and the segments left with nothing in them.
    pub(crate) fn sweep(&self) {
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            // Freed outside the lock.
            let mut ready: Vec<MemoId, _> = Vec::new_in(frame);
            {
                let mut dying = self.dying.lock();
                let done = passed(&dying.checked);
                ready.extend(dying.checked.drain(..done).map(|(_, memo)| memo));
            }
            if ready.is_empty() {
                return;
            }
            let (mut pinned, mut free): (Vec<MemoId, _>, Vec<MemoId, _>) = (Vec::new_in(frame), Vec::new_in(frame));
            {
                let pins = self.pins.lock();
                for &memo in &ready {
                    if pins.contains_key(&memo) {
                        pinned.push(memo);
                    } else {
                        free.push(memo);
                    }
                }
            }
            for &memo in &free {
                self.free(memo);
            }
            if !pinned.is_empty() {
                let grace = Grace::start();
                self.dying.lock().checked.extend(pinned.iter().map(|&memo| (grace, memo)));
            }
        });
        self.sweep_shared();
        let values = &self.values;
        self.frees(|frees| {
            // SAFETY: a retired value row's grace has passed: no task that could reach it is left.
            frees.values.reclaim(|row| unsafe { values.drop_row(row) });
        });
        self.shared.sweep();
        self.edges.sweep();
        self.bytes.sweep();
        self.keys.sweep();
    }

    /// Takes each shared row no memo names any more out of [`Store::shared`] and retires it:
    /// once its count is [`DEAD`], a maker of an equal value makes a row of its own.
    fn sweep_shared(&self) {
        let task = blaze_reclaim::enter();
        let values = &self.values;
        self.shared.edit_all_with(
            &task,
            |_, &word| {
                let row = value_row(word).expect("a shared value has a row");
                // SAFETY: as in `Store::take_shared`: this task loaded the word from `shared`.
                let value = unsafe { values.get(row) };
                // Acquire: every memo's reads of the row happen before its retirement. A name
                // taken meanwhile fails this exchange; none can be taken after it.
                let dead = value.names.compare_exchange(0, DEAD, Ordering::Acquire, Ordering::Relaxed).is_ok();
                if dead { Edit::Remove } else { Edit::Keep }
            },
            // Removed: unlinked from `shared` now. A dead word a maker replaced first is
            // retired by that maker (`Store::take_shared`).
            |&word, _| self.retire_shared(word),
        );
    }

    /// Retires a dead shared row just unlinked from [`Store::shared`]: a task that loaded a
    /// memo's word naming it, or the word from `shared`, began before this grace.
    fn retire_shared(&self, word: u64) {
        let row = value_row(word).expect("a shared value has a row");
        self.frees(|frees| frees.values.retire(Grace::start(), row));
    }

    /// How many shared value rows the store keeps, retired ones included until swept.
    #[cfg(test)]
    pub(crate) fn shared_rows(&self) -> usize {
        let task = blaze_reclaim::enter();
        let mut rows = 0;
        self.shared.for_each(&task, |_, _| rows += 1);
        rows
    }

    /// The memo's value word, for a test to compare two memos' rows.
    #[cfg(test)]
    pub(crate) fn value_word(&self, task: &Guard, memo: MemoId) -> u64 {
        self.hot(task, memo).value.load(Ordering::Acquire)
    }

    /// How many values this lane let go of wait for their grace, after freeing those past it.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        let values = &self.values;
        self.frees(|frees| {
            // SAFETY: as in `Store::sweep`.
            frees.values.reclaim(|row| unsafe { values.drop_row(row) });
            frees.values.retired()
        })
    }

    /// How many memos let go of are not freed yet.
    #[cfg(test)]
    pub(crate) fn dying(&self) -> usize {
        let dying = self.dying.lock();
        dying.unlinked.len() + dying.held.len() + dying.checked.len()
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> (usize, usize, usize) {
        (self.edges.count(), self.bytes.count(), self.keys.count())
    }

    /// Frees everything let go of that no open view holds (`held`) and no cold write pins:
    /// with `&mut`, no task reads the store.
    pub(crate) fn reclaim(&mut self, held: impl Fn(MemoId) -> bool) {
        let dying = std::mem::take(self.dying.get_mut());
        // alloc: once a `&mut` reclaim (settle, or a driver's full free).
        let pinned: Vec<MemoId> = self.pins.get_mut().keys().copied().collect();
        let mut keep = Dying::default();
        let all = dying
            .unlinked
            .into_iter()
            .map(|(_, memo)| memo)
            .chain(dying.held)
            .chain(dying.checked.into_iter().map(|(_, memo)| memo));
        for memo in all {
            if held(memo) {
                keep.held.push(memo);
            } else if pinned.contains(&memo) {
                keep.checked.push((Grace::start(), memo));
            } else {
                self.free(memo);
            }
        }
        *self.dying.get_mut() = keep;
        self.sweep_shared();
        self.shared.reclaim();
        let values = &self.values;
        for writers in self.writers.iter_mut() {
            // SAFETY: `&mut` excludes every reader.
            writers.0.get_mut().values.reclaim_all(|row| unsafe { values.drop_row(row) });
        }
        self.edges.sweep();
        self.bytes.sweep();
        self.keys.sweep();
    }

    /// `bytes` as a key's, copied once into this thread's key segment.
    pub(crate) fn key_bytes(&self, bytes: &[u8]) -> Span {
        self.keys.copy(bytes, 1)
    }

    /// A slot of `memos` (the newest last) and `key`: up to [`INLINE`] memos in the slot
    /// itself, more in a list copied into this thread's key segment.
    pub(crate) fn slot(&self, key: Option<Span>, memos: &[MemoId]) -> Slot {
        let len = u32::try_from(memos.len()).expect("2^32 memos of one key");
        if memos.len() <= INLINE {
            let mut inline = [0; INLINE];
            inline[..memos.len()].copy_from_slice(memos);
            return Slot { key, list: None, len, inline };
        }
        // SAFETY: `u32`s have no padding: every byte of the slice is initialized.
        let bytes = unsafe { std::slice::from_raw_parts(memos.as_ptr().cast::<u8>(), size_of_val(memos)) };
        Slot { key, list: Some(self.keys.copy(bytes, align_of::<MemoId>())), len, inline: [0; INLINE] }
    }
}

/// How many memo IDs a [`Slot`] holds in itself.
const INLINE: usize = 3;

/// What the database keeps of one key: its memos (the newest last) and its key's bytes, so it
/// can run it again by its [`QueryKey`] alone. A changed slot is a new one swapped in. 64
/// bytes; a key of more than [`INLINE`] memos adds 4 bytes a memo in a list.
pub(crate) struct Slot {
    key: Option<Span>,
    /// All the memo IDs, when there are more than [`INLINE`]: `u32`s at a 4-byte boundary.
    list: Option<Span>,
    len: u32,
    inline: [MemoId; INLINE],
}

const _: () = assert!(size_of::<Slot>() == 64);

impl Slot {
    pub(crate) fn empty() -> Slot {
        Slot { key: None, list: None, len: 0, inline: [0; INLINE] }
    }

    pub(crate) fn memos(&self) -> &[MemoId] {
        match &self.list {
            Some(list) => {
                let bytes = list.bytes();
                // SAFETY: `len` `u32`s copied in at a 4-byte boundary by `Store::slot`.
                unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<MemoId>(), self.len as usize) }
            }
            None => &self.inline[..self.len as usize],
        }
    }

    pub(crate) fn key(&self) -> Option<&[u8]> {
        self.key.as_ref().map(Span::bytes)
    }

    /// The key's bytes, held once more for another slot.
    pub(crate) fn share_key(&self) -> Option<Span> {
        self.key.as_ref().map(Span::share)
    }
}

#[cfg(test)]
mod tests;
