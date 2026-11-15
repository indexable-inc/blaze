//! The read recorder: what a piece of work read, at item grain, for its read list.
//!
//! Off unless the snapshot was made with [`Reads::Kept`]. On, each run gathers in its own
//! frame what it read DIRECTLY: [`Read::Input`] for an input getter (and [`read`]), and for
//! each tracked query it asked, hit or run, one row: a [`Read::Query`] when the query's kind
//! is a fact boundary ([`crate::Builder::mark_boundary`]: its own list stays behind, the
//! firewall), else a reference to the asked entry (its callee list). At the run's end both
//! are sorted and deduplicated and kept with the entry, in its arena ([`Lists`]).
//!
//! The FLAT list (every input and boundary row the value depends on, through every callee
//! down to the boundaries: what [`record`] gives and an image record files) is made on demand,
//! once per entry that something asks it of ([`flat_of`]), by a walk of the callee graph that
//! visits each entry once and stops at an entry whose flat list is made, and kept with the
//! entry. It depends on the entry alone, never on who asked first. Port decision 2
//! (2026-11-13); lists made lazy 2026-11-14 (the eager form appended each callee's whole flat
//! list at every ask, and sorted it at every run's end: name resolution of `core` spent most
//! of its 80 s there).
//!
//! Cost (rule 67): an ask, hit or run, appends one row to its asker's list (the floor: the
//! asker must name what it asked); a run sorts only its own direct rows; a flat list costs a
//! walk of its entry's closure (each entry once) and a sort, at most once per entry asked for
//! one, and its rows in the entry's arena. Off: one branch per fetch and a null word per entry.

use std::{
    alloc::{Allocator, Global},
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ptr,
    sync::atomic::{AtomicPtr, Ordering as Atomic},
};

use blaze_ids::ContentId;

use crate::{
    CycleKind, Fact, Gid, Kind, Memo, Plant, Reads,
    arena::{Slot, with_local},
    task,
    trie::Head,
};

/// A query's or an input's kind, as a list row names it: copied, compared and ordered by the
/// kind's name hash then its name, so a sorted list is in the same order in every process.
#[derive(Clone, Copy)]
pub struct KindId(&'static Kind);

impl KindId {
    pub const fn of(kind: &'static Kind) -> KindId {
        KindId(kind)
    }

    /// The kind's full name: `module::f`, `module::Type::method`, or a reserved name
    /// ([`AROUND_INPUTS`], [`UNTRACKED`], [`IMAGE_FACT`]).
    pub const fn name(self) -> &'static str {
        self.0.name()
    }

    pub const fn kind(self) -> &'static Kind {
        self.0
    }
}

impl PartialEq for KindId {
    /// By name, as [`Ord`] orders (a kind's name is its full path, one per kind): the pointer
    /// first, the common case.
    fn eq(&self, other: &KindId) -> bool {
        ptr::eq(self.0, other.0) || (self.0.id() == other.0.id() && self.0.name() == other.0.name())
    }
}

impl Eq for KindId {}

impl Ord for KindId {
    fn cmp(&self, other: &KindId) -> Ordering {
        (self.0.id(), self.0.name()).cmp(&(other.0.id(), other.0.name()))
    }
}

impl PartialOrd for KindId {
    fn partial_cmp(&self, other: &KindId) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for KindId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id().hash(state);
    }
}

impl fmt::Debug for KindId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name())
    }
}

/// One row of a read list.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Read {
    /// A fact read directly: an input's (its kind the input's), one read around the inputs
    /// ([`read`], kind [`AROUND_INPUTS`]), an untracked read ([`read_untracked`]), or a fact a
    /// served image entry read ([`IMAGE_FACT`]).
    Input { kind: KindId, fact: Fact },
    /// A boundary query asked: its kind and its entry's id in this snapshot (the query's key,
    /// [`crate::Memo::boundary_key`] in the generated `key_of`). Valid in this snapshot only:
    /// a persisted list holds stable facts, translated before anything is filed.
    Query { kind: KindId, key: Gid },
}

/// The kind of a fact [`read`] lists without an input of its own: a record read from the
/// store, say.
pub static AROUND_INPUTS: Kind = Kind::new("blaze_memo::read", CycleKind::Panic);
/// The kind of an untracked read ([`read_untracked`]): a classifier refuses to store a list
/// holding one, so the reader is recomputed in each snapshot and never filed.
pub static UNTRACKED: Kind = Kind::new("blaze_memo::untracked", CycleKind::Panic);
/// The kind of the facts of an entry served from the crate's own image: the record's list,
/// each an input fact or a boundary query's stable fact as the image translated it.
pub static IMAGE_FACT: Kind = Kind::new("blaze_memo::image", CycleKind::Panic);

impl From<Fact> for Read {
    fn from(fact: Fact) -> Read {
        Read::Input { kind: KindId(&AROUND_INPUTS), fact }
    }
}

impl Read {
    pub const fn kind(self) -> KindId {
        match self {
            Read::Input { kind, .. } | Read::Query { kind, .. } => kind,
        }
    }

    /// Whether this row is an untracked read.
    pub fn is_untracked(self) -> bool {
        self.kind() == KindId(&UNTRACKED)
    }
}

/// The recorder is off in this snapshot ([`Reads::Off`]): nothing was listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecorderOff;

impl std::fmt::Display for RecorderOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the read recorder is off in this snapshot (`Reads::Off`): make it with `Reads::Kept`")
    }
}

/// Lists `read` as read by the current run (for a read the memo does not see: a record read
/// from the store, say). A bare [`Fact`] is a [`Read::Input`] of kind [`AROUND_INPUTS`].
pub fn read(memo: &Memo, read: impl Into<Read>) {
    task::note_fact(memo, read.into());
}

/// Lists an untracked read, `what` naming it (`"the process environment"`): something read
/// that no input covers. Its digest is the snapshot's own, so it never matches another
/// snapshot's, and a classifier refuses to store a list holding it.
pub fn read_untracked(memo: &Memo, what: &'static str) {
    let fact = Fact {
        id: ContentId::of("blaze_memo untracked read", what.as_bytes()),
        digest: ContentId::of("blaze_memo untracked epoch", &memo.snapshot_epoch().get().to_le_bytes()),
    };
    task::note_fact(memo, Read::Input { kind: KindId(&UNTRACKED), fact });
}

/// Runs `work` with the recorder off for it: nothing it reads, asks or records reaches the
/// running query's list or any [`record`] open around it (a [`record`] inside `work` still
/// gives its own rows). For bookkeeping whose reads are no part of the running query's value:
/// a span's name, a cache filing a goal. That is the caller's promise; the memo cannot check
/// it, and a value that does depend on such a read is listed without it. Borrows of the arenas
/// `work` reads are still noted (they keep values sound, not lists right).
pub fn unrecorded<T>(memo: &Memo, work: impl FnOnce() -> T) -> T {
    if memo.reads() != Reads::Kept {
        return work();
    }
    let frame = task::Run::unrecorded(memo);
    let _pushed = task::push(&frame);
    work()
}

/// Runs `work` and gives the rows it read, sorted and deduplicated, in the global heap: its
/// flat list (every input and boundary row it depends on, directly or through any query it
/// asked, hit or run, down to the boundaries).
///
/// # Errors
///
/// [`RecorderOff`] when the snapshot keeps no read lists.
pub fn record<T>(memo: &Memo, work: impl FnOnce() -> T) -> Result<(T, Vec<Read, Global>), RecorderOff> {
    record_in(memo, Global, work)
}

/// [`record`], the list in `alloc` (the task's frame, typically).
///
/// # Errors
///
/// As [`record`].
///
/// # Panics
///
/// Where `work` read one fact with two digests, directly or through what it asked: two
/// snapshots' reads mixed.
pub fn record_in<T, A: Allocator>(
    memo: &Memo,
    alloc: A,
    work: impl FnOnce() -> T,
) -> Result<(T, Vec<Read, A>), RecorderOff> {
    if memo.reads() != Reads::Kept {
        return Err(RecorderOff);
    }
    let frame = task::Run::recorder(memo);
    let pushed = task::push(&frame);
    let value = work();
    let taken = frame.take_reads();
    drop(pushed);
    // An outer run read what this work read: its direct rows and the entries it asked, as
    // though asked there (the outer run's own flat list reaches the rest through them).
    task::replay_taken(&taken);
    let rows = flatten_in(memo, &taken.reads, &taken.callees, alloc, &|out: &mut dyn fmt::Write| {
        let _ = out.write_str("a piece of work recorded by `blaze_memo::record`");
    });
    Ok((value, rows))
}

/// Who made a row of [`record_readers`]: the innermost tracked query whose run read it, or the
/// recorded work itself when no tracked query was running.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Reader {
    /// The work given to [`record_readers`], outside any query it asked.
    Work,
    /// The entry of a tracked query: its kind and its key's hash (the entry's trie hash; the
    /// key itself is behind the kind's typed table, which a type-erased entry cannot print).
    /// A fixpoint member reads through its head's lists, so its rows name the member asked.
    /// `at` is the entry's place in its [`Readers`]' walk: [`Readers::chain`] gives the asks
    /// from the work down to it.
    Query { kind: KindId, key_hash: u64, at: u32 },
}

/// What [`record_readers`] gives: each row with its [`Reader`], and the walk's one buffer of
/// entries, each with the entry that asked it, from which [`Readers::chain`] reads a reader's
/// path of asks.
pub struct Readers {
    /// Sorted and deduplicated by row then reader.
    pub rows: Vec<(Read, Reader), Global>,
    /// Each entry the walk reached, once: its kind and the index of the entry it was first
    /// reached from ([`ASKED_BY_WORK`] for one the work asked).
    asked: Vec<(KindId, u32), Global>,
}

/// The asker index of an entry the recorded work asked itself.
const ASKED_BY_WORK: u32 = u32::MAX;

impl Readers {
    /// The kinds on `reader`'s path of asks, innermost (the reader) first, up to the query the
    /// work asked; empty for [`Reader::Work`]. One path where several reach the entry: the one
    /// the walk met first, each step a real ask. Bounded by the walk's entries (each reached
    /// once, its asker reached before it).
    pub fn chain(&self, reader: Reader) -> impl Iterator<Item = KindId> + '_ {
        let mut at = match reader {
            Reader::Work => ASKED_BY_WORK,
            Reader::Query { at, .. } => at,
        };
        std::iter::from_fn(move || {
            let (kind, asker) = *self.asked.get(usize::try_from(at).ok()?)?;
            at = asker;
            Some(kind)
        })
    }
}

impl fmt::Display for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reader::Work => f.write_str("the recorded work"),
            Reader::Query { kind, key_hash, .. } => write!(f, "{}(key hash {key_hash:#018x})", kind.name()),
        }
    }
}

/// [`record`], each row with its [`Reader`]: the innermost tracked query whose run read it
/// (a row merged from a callee keeps the callee, not its asker). Sorted and deduplicated by
/// row then reader, so a row two entries read appears once for each. Its rows, readers
/// dropped and deduplicated, are [`record`]'s for the same run (`tests/reads.rs`).
///
/// A debugging and test surface: [`record`] pays nothing for it. The readers are not kept at
/// each read: every entry's lists already hold the rows its own run read (its direct list),
/// so this walks the callee graph as the flat list's walk does but never takes a made flat list
/// whole (that would lose who read its rows), naming each entry's direct rows by the entry.
/// Cost: a walk of the recorded work's whole closure, each entry once, keeping one
/// `(kind, asker)` pair per entry for [`Readers::chain`].
///
/// # Errors
///
/// [`RecorderOff`] when the snapshot keeps no read lists.
///
/// # Panics
///
/// When the walk reaches `u32::MAX - 1` entries.
pub fn record_readers<T>(memo: &Memo, work: impl FnOnce() -> T) -> Result<(T, Readers), RecorderOff> {
    if memo.reads() != Reads::Kept {
        return Err(RecorderOff);
    }
    let frame = task::Run::recorder(memo);
    let pushed = task::push(&frame);
    let value = work();
    let taken = frame.take_reads();
    drop(pushed);
    // As in `record_in`: an outer run read what this work read.
    task::replay_taken(&taken);
    let mut rows: Vec<(Read, Reader), Global> = Vec::new_in(Global);
    rows.extend(taken.reads.iter().map(|read| (*read, Reader::Work)));
    let mut asked: Vec<(KindId, u32), Global> = Vec::new_in(Global);
    let mut seen: blaze_hash::HashSet<*const Head, Global> = blaze_hash::HashSet::new_in(Global);
    let mut todo: Vec<(*const Head, u32), Global> = Vec::new_in(Global);
    todo.extend(taken.callees.iter().map(|callee| (*callee, ASKED_BY_WORK)));
    while let Some((entry, asker)) = todo.pop() {
        if !seen.insert(entry) {
            continue;
        }
        // SAFETY: a callee outlives every entry and run that names it ([`Lists`]).
        let head = unsafe { &*entry };
        let at = u32::try_from(asked.len())
            .ok()
            .filter(|it| *it != ASKED_BY_WORK)
            .expect("record_readers: more entries than a u32 index names");
        asked.push((KindId(head.kind), asker));
        let Some(lists) = head.lists() else { continue };
        let reader = Reader::Query { kind: KindId(head.kind), key_hash: head.hash, at };
        rows.extend(lists.direct().iter().map(|read| (*read, reader)));
        todo.extend(lists.callees().iter().map(|callee| (*callee, at)));
    }
    rows.sort_unstable();
    rows.dedup();
    Ok((value, Readers { rows, asked }))
}

/// What one entry read, under [`Reads::Kept`]: its direct rows and the entries it asked, made
/// once with the entry in its arena (`slot`), and its flat list, made at most once, on demand
/// ([`Lists::flat`]). Never written after it is made but for `flat`, set once.
///
/// Why a callee outlives every entry naming it: a callee is an entry its asker's run looked up
/// through a fetch, which noted a borrow of the callee's arena by the run's home
/// (`task::note_read_of`; the homing check there refuses a session-homed run that reads a
/// crate's arena), or it is in the same arena. [`crate::Memo::next`] drops a changed arena and,
/// to a fixpoint, every arena that borrowed from a dropped one, so an arena that holds an entry
/// holds the arenas of everything its entries name, carried or not. An entry a predecessor
/// snapshot made in a carried arena goes to that snapshot's spill table (its borrow is not
/// noted, `Slot::borrow`) and is reachable only while that snapshot, which holds every arena
/// it names, lives. A fixpoint member shares its head's lists (its head is published first,
/// `fetch::execute`): the member read its head through the cycle's path of asks, each a noted
/// borrow, so the head's arena outlives it the same way. Nothing here has a destructor: an
/// arena's drop frees its lists and flat lists with its memory, in whatever order arenas drop.
pub(crate) struct Lists {
    /// The arena these lists are in: where the flat list is kept when made.
    slot: *const Slot,
    /// Input rows and boundary rows, sorted, deduplicated, each fact with one digest.
    direct: *const [Read],
    /// Each entry asked that is not a boundary, once, ordered by address (only to merge
    /// duplicates: nothing is output in this order).
    callees: *const [*const Head],
    /// The flat list ([`Flat`]), null until made.
    flat: AtomicPtr<Flat>,
}

/// A made flat list, in its lists' arena.
struct Flat {
    rows: *const [Read],
}

impl Lists {
    /// Lists of `reads` and `callees` in `slot`'s arena (`local` its allocator), or null when
    /// both are empty.
    pub(crate) fn in_arena(
        slot: &Slot,
        local: &blaze_mem::ArenaLocal<'_>,
        reads: &[Read],
        callees: &[*const Head],
    ) -> *const Lists {
        if reads.is_empty() && callees.is_empty() {
            return ptr::null();
        }
        let direct: *const [Read] = local.alloc_slice_copy(reads);
        let callees: *const [*const Head] = local.alloc_slice_copy(callees);
        local.alloc(Lists { slot: ptr::from_ref(slot), direct, callees, flat: AtomicPtr::new(ptr::null_mut()) })
    }

    pub(crate) fn direct(&self) -> &[Read] {
        // SAFETY: made in `slot`'s arena with these lists, which live as long as it.
        unsafe { &*self.direct }
    }

    pub(crate) fn callees(&self) -> &[*const Head] {
        // SAFETY: as for `direct`.
        unsafe { &*self.callees }
    }

    /// The flat list if made.
    fn made(&self) -> Option<&[Read]> {
        let flat = self.flat.load(Atomic::Acquire);
        // SAFETY: a published `Flat` and its rows are in the lists' arena, never written again.
        (!flat.is_null()).then(|| unsafe { &*(*flat).rows })
    }

    /// The flat list: every input and boundary row of these lists and of every callee's,
    /// transitively, sorted and deduplicated; `named` names the entry for a refusal. Made at
    /// the first ask and kept in the lists' arena; with no callees it is the direct list.
    /// Two tasks asking at once may both make it, and the first published is kept (the same
    /// rows: the list is a function of the entry; the other copy is unreachable arena bytes,
    /// as a lost trie race's node is).
    ///
    /// # Panics
    ///
    /// Where the rows hold one fact with two digests: two snapshots' reads mixed.
    pub(crate) fn flat(&self, memo: &Memo, named: &dyn Fn(&mut dyn fmt::Write)) -> &[Read] {
        if let Some(made) = self.made() {
            return made;
        }
        if self.callees().is_empty() {
            return self.direct();
        }
        let rows = flatten_in(memo, self.direct(), self.callees(), Global, named);
        // SAFETY: the lists' own arena, alive while the lists are.
        let slot = unsafe { &*self.slot };
        let made = with_local(slot, |local| -> *mut Flat {
            let kept: *const [Read] = local.alloc_slice_copy(&rows);
            local.alloc(Flat { rows: kept })
        });
        match self.flat.compare_exchange(ptr::null_mut(), made, Atomic::AcqRel, Atomic::Acquire) {
            // SAFETY: just made in the arena, published by release.
            Ok(_) => unsafe { &*(*made).rows },
            // SAFETY: the winner's, published by release.
            Err(theirs) => unsafe { &*(*theirs).rows },
        }
    }
}

/// The flat list of the entry `head` starts ([`Lists::flat`]); empty when it read nothing.
pub(crate) fn flat_of<'a>(memo: &Memo, head: &'a Head) -> &'a [Read] {
    let Some(lists) = head.lists() else { return &[] };
    lists.flat(memo, &|out: &mut dyn fmt::Write| name_entry(head, out))
}

/// An entry as a refusal names it when its key's type is not known here: its kind and its
/// key's hash.
fn name_entry(head: &Head, out: &mut dyn fmt::Write) {
    let _ = write!(out, "an entry of {} (key hash {:#018x})", head.kind.name(), head.hash);
}

/// `direct` and the direct rows of every entry reachable through `callees`, each entry visited
/// once (and none below an entry whose flat list is made: that list is taken whole), sorted and
/// deduplicated, in `alloc`. The walk's own scratch (the visited set and the stack) is on the
/// global heap and freed at its end. Bounded: each entry is pushed once per row naming it and
/// expanded once.
///
/// # Panics
///
/// As [`Lists::flat`]; `named` names whose rows they are.
fn flatten_in<A: Allocator>(
    memo: &Memo,
    direct: &[Read],
    callees: &[*const Head],
    alloc: A,
    named: &dyn Fn(&mut dyn fmt::Write),
) -> Vec<Read, A> {
    let mut rows = Vec::new_in(alloc);
    rows.extend_from_slice(direct);
    if !callees.is_empty() {
        let mut seen: blaze_hash::HashSet<*const Head, Global> = blaze_hash::HashSet::new_in(Global);
        let mut todo: Vec<*const Head, Global> = Vec::new_in(Global);
        todo.extend_from_slice(callees);
        while let Some(at) = todo.pop() {
            if !seen.insert(at) {
                if memo.plant() == Plant::FlattenStopsAtRevisit {
                    break;
                }
                continue;
            }
            // SAFETY: a callee outlives every entry and run that names it ([`Lists`]).
            let Some(lists) = (unsafe { &*at }).lists() else { continue };
            if let Some(made) = lists.made() {
                rows.extend_from_slice(made);
                continue;
            }
            rows.extend_from_slice(lists.direct());
            todo.extend_from_slice(lists.callees());
        }
    }
    rows.sort_unstable();
    rows.dedup();
    one_digest_each(&rows, named);
    rows
}

/// Refuses sorted, deduplicated `rows` that hold one fact (of one input kind) with two digests,
/// `named` naming whose they are: inputs are fixed for a snapshot, so that is two snapshots'
/// reads mixed. Untracked reads are exempt (each is its snapshot's own).
pub(crate) fn one_digest_each(rows: &[Read], named: &dyn Fn(&mut dyn fmt::Write)) {
    for pair in rows.windows(2) {
        if let (Read::Input { kind: a, fact: x }, Read::Input { kind: b, fact: y }) = (pair[0], pair[1])
            && a == b
            && x.id == y.id
            && a.name() != UNTRACKED.name()
        {
            let mut name = String::new();
            named(&mut name);
            panic!(
                "{name} read fact {:?} with two digests ({:?} and {:?}) in one snapshot",
                x.id, x.digest, y.digest
            );
        }
    }
}
