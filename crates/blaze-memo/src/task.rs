//! A task's stack of runs: which keys it is running now (for cycles), the read lists being
//! gathered (for the recorder), and the arena each run's allocations and borrows go to.
//!
//! A [`Run`] lives on the stack of the fetch that runs it; the innermost one is a thread-local
//! pointer, carried with the task across fiber waits (`blaze_graph::sched::carry`, registered
//! once by [`register`]). Nothing here is shared between tasks: a run is touched only by the
//! task whose stack it is on.

use std::{
    alloc::Global,
    cell::{Cell, RefCell},
    fmt,
    num::NonZeroUsize,
    ptr,
    sync::Once,
};

use crate::{Gid, Kind, KindId, Memo, Plant, Read, Reads, arena::Slot, record::Lists, trie::Head};

thread_local! {
    static INNERMOST: Cell<*const Run> = const { Cell::new(ptr::null()) };
}

/// The recorder's frame kind: it runs nothing and is never a cycle's member.
pub(crate) static RECORDER: Kind = Kind::new("blaze_memo::record", crate::CycleKind::Panic);

/// Registers the task's stack of runs to be carried with each job across its waits. Called by
/// the first run; a driver may call it before its pool starts.
pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        blaze_graph::sched::carry(blaze_graph::sched::Carried::word(
            || {
                let it = INNERMOST.with(|cell| cell.replace(ptr::null()));
                NonZeroUsize::new(it as usize)
            },
            |it| INNERMOST.with(|cell| cell.set(it.get() as *const Run)),
        ));
    });
}

/// Threshold past which [`Book`] switches from linear scans to hash-indexed bucket chains.
const BOOK_INDEX_THRESHOLD: usize = 16;

/// A fixpoint member's value in its head's round, or an inner head's last value.
struct Prov {
    kind: &'static Kind,
    hash: u64,
    /// A copy of the key in the member's arena.
    key: *const (),
    same_key: unsafe fn(*const (), *const ()) -> bool,
    /// The value of round `round`, in the member's arena.
    value: *const (),
    round: u32,
    /// Intrusive link to the earlier `Prov` in `Book::provs` sharing `(kind, hash)`, or
    /// `usize::MAX` if none (populated once `provs_index` is built).
    prev_same: usize,
    /// What publishes the member's entry when its head settles: its value in the last round is
    /// final then (the head's value and every inner head's stood still, and a member is a
    /// function of them).
    settle: Settle,
}

/// What publishes a member's `Entry<K, V>` of (key, value) in its slot, with its cycle's lists
/// (the head's entry's, or null): one made from the value (`fetch::publish_member`), or, for an
/// `owns` query, the entry the value was placed in (`fetch::publish_member_placed`).
pub(crate) type Publish = unsafe fn(&Memo, &Slot, &'static Kind, u64, *const (), *const (), *const Lists);

/// A member's entry, made final when its head settles. Its reads are not kept here: a member's
/// rows of each round reached its asker (and so its head) when the round's run returned, and
/// its entry shares its head's lists ([`Run::settle_members`]).
pub(crate) struct Settle {
    pub(crate) slot: *const Slot,
    pub(crate) publish: Publish,
}

/// What a cycle's head keeps across its rounds: its members' values this round. Global heap,
/// made only for a cycle.
struct Book {
    round: u32,
    provs: Vec<Prov, Global>,
    /// Hash index from `(kind_ptr, hash)` to the latest index in `provs` (built lazily once
    /// `provs.len() > BOOK_INDEX_THRESHOLD` to eliminate O(M²×R) cycle scans).
    provs_index: Option<blaze_hash::HashMap<(usize, u64), usize, Global>>,
    /// An inner head's value moved this round.
    moved: bool,
    /// The first [`MOVERS_NAMED`] members whose value moved this round, by name and key: what a
    /// head that does not settle names in its panic ([`Moved::named`]).
    movers: Vec<String, Global>,
    /// Hash index from `(kind_ptr, hash)` to the latest index in `keeps` (built lazily once
    /// `keeps.len() > BOOK_INDEX_THRESHOLD`).
    keeps_index: Option<blaze_hash::HashMap<(usize, u64), usize, Global>>,
    /// The flight of every fixpoint member run so far, held by this task until the head
    /// settles: another task asking a member waits (for the head) instead of running it again,
    /// so no member's function runs on two tasks in one snapshot. Declared last: dropped
    /// after the book's values are published, and on an unwind with the head.
    keeps: Vec<KeptMember, Global>,
}

impl Book {
    fn new() -> Book {
        Book {
            round: 0,
            provs: Vec::new_in(Global),
            provs_index: None,
            moved: false,
            movers: Vec::new_in(Global),
            keeps_index: None,
            keeps: Vec::new_in(Global),
        }
    }

    fn push_prov(&mut self, mut prov: Prov) {
        let idx = self.provs.len();
        if self.provs_index.is_none() && idx >= BOOK_INDEX_THRESHOLD {
            let mut map = blaze_hash::HashMap::new_in(Global);
            for (i, existing) in self.provs.iter_mut().enumerate() {
                let k = (ptr::from_ref(existing.kind) as usize, existing.hash);
                existing.prev_same = map.insert(k, i).unwrap_or(usize::MAX);
            }
            self.provs_index = Some(map);
        }
        if let Some(map) = &mut self.provs_index {
            let k = (ptr::from_ref(prov.kind) as usize, prov.hash);
            prov.prev_same = map.insert(k, idx).unwrap_or(usize::MAX);
        }
        self.provs.push(prov);
    }

    fn push_keep(&mut self, mut kept: KeptMember) {
        let idx = self.keeps.len();
        if self.keeps_index.is_none() && idx >= BOOK_INDEX_THRESHOLD {
            let mut map = blaze_hash::HashMap::new_in(Global);
            for (i, existing) in self.keeps.iter_mut().enumerate() {
                let k = (ptr::from_ref(existing.kind) as usize, existing.hash);
                existing.prev_same = map.insert(k, i).unwrap_or(usize::MAX);
            }
            self.keeps_index = Some(map);
        }
        if let Some(map) = &mut self.keeps_index {
            let k = (ptr::from_ref(kept.kind) as usize, kept.hash);
            kept.prev_same = map.insert(k, idx).unwrap_or(usize::MAX);
        }
        self.keeps.push(kept);
    }

    /// Whether `keeps` holds a flight matching `(kind, hash, key)`.
    ///
    /// # Safety
    ///
    /// `key` must point to a value of `kind`'s key type.
    unsafe fn has_keep(&self, kind: &'static Kind, hash: u64, key: *const ()) -> bool {
        if let Some(map) = &self.keeps_index {
            let Some(&head) = map.get(&(ptr::from_ref(kind) as usize, hash)) else {
                return false;
            };
            let mut cur = head;
            while cur != usize::MAX {
                let it = &self.keeps[cur];
                // SAFETY: `kind` matched and the caller guarantees `key` has `kind`'s key type.
                if ptr::eq(it.kind, kind) && it.hash == hash && unsafe { (it.same_key)(it.key, key) } {
                    return true;
                }
                cur = it.prev_same;
            }
            return false;
        }
        self.keeps.iter().any(|it| {
            // SAFETY: `kind` matched first and the caller guarantees `key` has `kind`'s key type.
            ptr::eq(it.kind, kind) && it.hash == hash && unsafe { (it.same_key)(it.key, key) }
        })
    }

    /// The latest provisional value for `(kind, hash, key)` across any round (or `only_round` when
    /// `Some`), or null if absent.
    ///
    /// # Safety
    ///
    /// `key` must point to a value of `kind`'s key type.
    unsafe fn find_prov(
        &self,
        kind: &'static Kind,
        hash: u64,
        key: *const (),
        only_round: Option<u32>,
    ) -> *const () {
        if let Some(map) = &self.provs_index {
            let Some(&head) = map.get(&(ptr::from_ref(kind) as usize, hash)) else {
                return ptr::null();
            };
            let mut cur = head;
            while cur != usize::MAX {
                let it = &self.provs[cur];
                if let Some(want) = only_round {
                    if it.round < want {
                        // `provs` is pushed in non-decreasing round order, so earlier entries
                        // in this chain cannot match `want`.
                        break;
                    }
                    if it.round != want {
                        cur = it.prev_same;
                        continue;
                    }
                }
                // SAFETY: `kind` matched and the caller guarantees `key` has `kind`'s key type.
                if ptr::eq(it.kind, kind) && it.hash == hash && unsafe { (it.same_key)(it.key, key) } {
                    return it.value;
                }
                cur = it.prev_same;
            }
            return ptr::null();
        }
        self.provs
            .iter()
            .rev()
            .find(|it| {
                only_round.is_none_or(|want| it.round == want)
                    && ptr::eq(it.kind, kind)
                    && it.hash == hash
                    // SAFETY: `kind` matched first and caller guarantees `key` has `kind`'s key type.
                    && unsafe { (it.same_key)(it.key, key) }
            })
            .map_or(ptr::null(), |it| it.value)
    }
}

/// How many moved members a round names ([`Book::movers`]): enough to see which member
/// flips, bounded so a cycle of thousands names a few.
pub(crate) const MOVERS_NAMED: usize = 4;

/// Whether a head's members moved in the round just ended ([`Run::next_round`]), and the first
/// [`MOVERS_NAMED`] that did, each by name and key.
pub(crate) struct Moved {
    pub(crate) any: bool,
    pub(crate) named: Vec<String, Global>,
}

/// A member whose flight the head holds.
struct KeptMember {
    kind: &'static Kind,
    hash: u64,
    key: *const (),
    same_key: unsafe fn(*const (), *const ()) -> bool,
    /// Intrusive link to the earlier `KeptMember` sharing `(kind, hash)`, or `usize::MAX`.
    prev_same: usize,
    /// Borrows the snapshot's flight, erased to `'static`: the book lives on the stack of a
    /// fetch that borrows the snapshot, which holds the flight (`Config::flight`), so the
    /// flight outlives it.
    _held: crate::flight::Kept<'static>,
}

/// Which head keeps a member's flight once its run returns: [`execute`'s] word to its fetch.
///
/// [`execute`'s]: crate::fetch
pub(crate) struct Member {
    head: *const Run,
    kind: &'static Kind,
    hash: u64,
    /// A copy of the key in the member's arena.
    key: *const (),
    same_key: unsafe fn(*const (), *const ()) -> bool,
}

impl Run {
    /// This run as a fixpoint member whose flight its head is to keep; `key` a copy of its key
    /// in its arena.
    pub(crate) fn member(&self, key: *const ()) -> Member {
        Member {
            head: self.head_of.get(),
            kind: self.kind,
            hash: self.hash,
            key,
            same_key: self.same_key,
        }
    }
}

/// Gives `member`'s head the flight `kept` of its run, held until the head settles.
pub(crate) fn keep_flight(member: Member, kept: crate::flight::Kept<'_>) {
    // SAFETY: a member's head is on this task's stack below the fetch that ran the member
    // (it reentered through it), and outlives this call.
    let head = unsafe { &*member.head };
    // SAFETY: lifetime erasure only; `KeptMember::_held` says why the flight outlives it.
    let held = unsafe {
        std::mem::transmute::<crate::flight::Kept<'_>, crate::flight::Kept<'static>>(kept)
    };
    let mut book = head.book.borrow_mut();
    book.get_or_insert_with(Book::new).push_keep(KeptMember {
        kind: member.kind,
        hash: member.hash,
        key: member.key,
        same_key: member.same_key,
        prev_same: usize::MAX,
        _held: held,
    });
}

/// Where this task stands with a key it is about to ask the flight for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Here {
    /// Nothing of it here: ask the flight.
    No,
    /// A run of it is on this task's stack: the flight answers `Mine`, a cycle.
    Running,
    /// A head on this task's stack keeps its flight from an earlier round: run it again here,
    /// without the flight (this task holds it).
    Kept,
}

/// What this task holds of `key`.
pub(crate) fn here(kind: &'static Kind, hash: u64, key: *const ()) -> Here {
    let Some(first) = innermost() else { return Here::No };
    if !first.in_cycle() {
        return Here::No;
    }
    let mut kept = false;
    let mut at = Some(first);
    while let Some(run) = at {
        if !run.is_recorder() && run.matches(kind, hash, key) {
            return Here::Running;
        }
        if !kept && let Some(book) = run.book.borrow().as_ref() {
            // SAFETY: the keys are of `kind`'s key type, matched first.
            kept = unsafe { book.has_keep(kind, hash, key) };
        }
        // SAFETY: an outer run is below on the same stack.
        at = unsafe { run.outer.as_ref() };
    }
    if kept { Here::Kept } else { Here::No }
}

pub(crate) struct Run {
    outer: *const Run,
    depth: u32,
    pub(crate) kind: &'static Kind,
    /// The arena this run's allocations and borrows go to (a recorder's: its run's, or null).
    pub(crate) slot: *const Slot,
    pub(crate) epoch: u64,
    hash: u64,
    key: *const (),
    same_key: unsafe fn(*const (), *const ()) -> bool,
    describe: unsafe fn(&'static Kind, *const (), &mut dyn fmt::Write),
    /// Whether a run below was a cycle's head or member when this one was pushed.
    inside: bool,
    /// The outermost head of a cycle this run is a member of; null when none.
    head_of: Cell<*const Run>,
    /// Whether this run was asked again inside itself: a head (or an inner head).
    reentered: Cell<bool>,
    /// The value handed to an ask of this run's key inside it, in its arena.
    provisional: Cell<*const ()>,
    book: RefCell<Option<Book>>,
    keeping: bool,
    /// The input and boundary rows this run read itself ([`note_fact`], [`pass_on`]).
    reads: RefCell<Vec<Read, Global>>,
    /// The entries (not boundaries) this run asked, hit or run: one row each ([`pass_on`]);
    /// their own reads stay with them until a flat list is asked ([`crate::record`]).
    callees: RefCell<Vec<*const Head, Global>>,
    /// A run of an imaged query: it reads only its home and its home's declared closure.
    imaged: bool,
}

/// What a run read directly ([`Run::take_reads`]): its rows and the entries it asked, each
/// sorted and deduplicated, on the global heap until its entry keeps them in its arena
/// ([`Lists::in_arena`]) or its asker takes them ([`replay_taken`]).
pub(crate) struct Taken {
    pub(crate) reads: Vec<Read, Global>,
    pub(crate) callees: Vec<*const Head, Global>,
}

impl Taken {
    fn len(&self) -> usize {
        self.reads.len() + self.callees.len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// # Safety
///
/// Both point at `K`s.
pub(crate) unsafe fn same_key<K: Eq>(a: *const (), b: *const ()) -> bool {
    // SAFETY: the caller's.
    unsafe { *a.cast::<K>() == *b.cast::<K>() }
}

/// # Safety
///
/// `key` points at a `K`.
pub(crate) unsafe fn describe<K: std::fmt::Debug>(
    kind: &'static Kind,
    key: *const (),
    out: &mut dyn fmt::Write,
) {
    // SAFETY: the caller's.
    let key = unsafe { &*key.cast::<K>() };
    write_key(out, kind, key);
}

/// The one way a message names a query's key: `kind(key)`, the key cut at
/// [`crate::KEY_CAP`] bytes ([`crate::Bounded::key`]) and each id in it printed one level
/// deep (`attach`'s `shallow`). Called only once a refusal is certain: a message is never
/// built ahead of its check.
pub(crate) fn write_key<K: std::fmt::Debug + ?Sized>(out: &mut dyn fmt::Write, kind: &'static Kind, key: &K) {
    let _ = write!(out, "{}({:?})", kind.name(), crate::Bounded::key(key));
}

impl Run {
    /// A run of `kind` for `key` homed in `slot`, its outer run the task's innermost now;
    /// `imaged` for an imaged query's.
    pub(crate) fn new<K: Eq + std::fmt::Debug>(
        memo: &Memo,
        kind: &'static Kind,
        slot: &Slot,
        hash: u64,
        key: &K,
        imaged: bool,
    ) -> Run {
        let outer = innermost_ptr();
        // SAFETY: the innermost run is on this task's stack below us.
        let outer_ref = unsafe { outer.as_ref() };
        Run {
            outer,
            depth: outer_ref.map_or(0, |it| it.depth + 1),
            kind,
            slot,
            epoch: memo.epoch(),
            hash,
            key: ptr::from_ref(key).cast(),
            same_key: same_key::<K>,
            describe: describe::<K>,
            inside: outer_ref.is_some_and(Run::in_cycle),
            head_of: Cell::new(ptr::null()),
            reentered: Cell::new(false),
            provisional: Cell::new(ptr::null()),
            book: RefCell::new(None),
            keeping: memo.reads() == Reads::Kept,
            reads: RefCell::new(Vec::new_in(Global)),
            callees: RefCell::new(Vec::new_in(Global)),
            imaged,
        }
    }

    /// The recorder's frame: it gathers reads and lends its outer run's arena.
    pub(crate) fn recorder(memo: &Memo) -> Run {
        Run::frame(memo, true)
    }

    /// [`crate::unrecorded`]'s frame: it lends its outer run's arena (borrows are still
    /// noted, as soundness needs) and keeps no read, so nothing below it reaches any list.
    pub(crate) fn unrecorded(memo: &Memo) -> Run {
        Run::frame(memo, false)
    }

    fn frame(memo: &Memo, keeping: bool) -> Run {
        let outer = innermost_ptr();
        // SAFETY: as in `new`.
        let outer_ref = unsafe { outer.as_ref() };
        Run {
            outer,
            depth: outer_ref.map_or(0, |it| it.depth + 1),
            kind: &RECORDER,
            slot: outer_ref.map_or(ptr::null(), |it| it.slot),
            epoch: memo.epoch(),
            hash: 0,
            key: ptr::null(),
            same_key: same_key::<()>,
            describe: describe::<()>,
            inside: outer_ref.is_some_and(Run::in_cycle),
            head_of: Cell::new(ptr::null()),
            reentered: Cell::new(false),
            provisional: Cell::new(ptr::null()),
            book: RefCell::new(None),
            keeping,
            reads: RefCell::new(Vec::new_in(Global)),
            callees: RefCell::new(Vec::new_in(Global)),
            // A recorder inside an imaged run reads for that run.
            imaged: outer_ref.is_some_and(|it| it.imaged),
        }
    }

    fn in_cycle(&self) -> bool {
        self.inside || !self.head_of.get().is_null() || self.book.borrow().is_some()
    }

    fn is_recorder(&self) -> bool {
        ptr::eq(self.kind, &RECORDER)
    }

    /// The run as a message names it, `kind(key)` ([`write_key`]), written when printed.
    fn name(&self) -> RunName<'_> {
        RunName(self)
    }

    fn matches(&self, kind: &'static Kind, hash: u64, key: *const ()) -> bool {
        // SAFETY: both keys are of `kind`'s key type: kinds match first.
        ptr::eq(self.kind, kind) && self.hash == hash && unsafe { (self.same_key)(self.key, key) }
    }

    /// This run's role in a cycle now.
    pub(crate) fn role(&self) -> Role {
        if !self.head_of.get().is_null() {
            Role::Member
        } else if self.reentered.get() {
            Role::Head
        } else {
            Role::Plain
        }
    }

    pub(crate) fn provisional(&self) -> *const () {
        self.provisional.get()
    }

    pub(crate) fn set_provisional(&self, value: *const ()) {
        self.provisional.set(value);
    }

    /// What this run read directly, each list sorted and deduplicated: the run's own cost for
    /// its list, O(r log r) in the r rows it appended (one per input read and per query asked),
    /// whatever its callees read.
    ///
    /// # Panics
    ///
    /// Where one fact was read with two digests directly: inputs are fixed for a snapshot, so
    /// that is two snapshots' reads mixed. (Through callees, the flat list's making refuses
    /// the same: `record::flatten_in`.)
    pub(crate) fn take_reads(&self) -> Taken {
        let mut reads = std::mem::replace(&mut *self.reads.borrow_mut(), Vec::new_in(Global));
        reads.sort_unstable();
        reads.dedup();
        crate::record::one_digest_each(&reads, &|out: &mut dyn fmt::Write| {
            // SAFETY: `key` points at the key `describe` was made for, on this task's stack.
            unsafe { (self.describe)(self.kind, self.key, out) };
        });
        let mut callees = std::mem::replace(&mut *self.callees.borrow_mut(), Vec::new_in(Global));
        callees.sort_unstable();
        callees.dedup();
        Taken { reads, callees }
    }

    /// Whether the head's members moved this round, and starts the next.
    pub(crate) fn next_round(&self) -> Moved {
        let mut book = self.book.borrow_mut();
        let Some(book) = book.as_mut() else { return Moved { any: false, named: Vec::new_in(Global) } };
        let moved = Moved { any: book.moved, named: std::mem::replace(&mut book.movers, Vec::new_in(Global)) };
        book.moved = false;
        book.round += 1;
        moved
    }

    /// Ends the cycle this run headed: after [`Run::settle_members`], so the members' flights
    /// it kept are let go only once their entries are published.
    pub(crate) fn end_cycle(&self) {
        *self.book.borrow_mut() = None;
        self.reentered.set(false);
        self.provisional.set(ptr::null());
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    Plain,
    Head,
    Member,
}

/// The task's innermost run while `run` is pushed.
pub(crate) struct Pushed<'a> {
    run: &'a Run,
}

impl Drop for Pushed<'_> {
    fn drop(&mut self) {
        set_innermost(self.run.outer);
    }
}

pub(crate) fn push(run: &Run) -> Pushed<'_> {
    register();
    debug_assert!(ptr::eq(innermost_ptr(), run.outer), "a run pushed onto another task's stack");
    set_innermost(run);
    Pushed { run }
}

/// This thread's innermost run. Never inlined, as every read of a thread-local in this crate
/// is (`blaze_graph::sched::fiber`'s rule, AGENTS.md rule 27): a run's function may park its
/// fiber and resume on another worker, and a reader inlined into a function that spans that
/// wait (a fetch pushes its run, runs the function and pops it) may keep the thread-local's
/// address from before the wait, so the run would be popped off the worker it left and looked
/// up there. The value, not the address, is what this returns: a function returning the
/// address reads no memory, so two calls of it may be merged across the wait all the same.
#[inline(never)]
fn innermost_ptr() -> *const Run {
    INNERMOST.with(Cell::get)
}

/// Sets this thread's innermost run. Never inlined ([`innermost_ptr`]).
#[inline(never)]
fn set_innermost(run: *const Run) {
    INNERMOST.with(|cell| cell.set(run));
}

/// The runs of this task from its root down to `run`, each by kind and key as a cycle's path
/// names them ([`reenter`]), joined by " -> ": who asked the run a refusal is about. A
/// recorder's frame runs nothing and is left out; "(the task's root)" where none is below.
/// For a panic's message alone: written when printed, its list of runs on the global heap.
#[cold]
fn asked_through(run: &Run) -> AskedThrough<'_> {
    AskedThrough(run)
}

/// [`Run::name`]: the run's `kind(key)`, written by its `describe` into the formatter.
struct RunName<'a>(&'a Run);

impl fmt::Display for RunName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let run = self.0;
        // SAFETY: `key` points at the key `describe` was made for, on this task's stack.
        unsafe { (run.describe)(run.kind, run.key, f) };
        Ok(())
    }
}

/// Runs named in order ([`Run::name`]), joined by " -> ": a cycle's path, or who asked a run.
struct Path<'a>(&'a [&'a Run]);

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, run) in self.0.iter().enumerate() {
            if at > 0 {
                f.write_str(" -> ")?;
            }
            write!(f, "{}", run.name())?;
        }
        Ok(())
    }
}

/// [`asked_through`]'s text.
struct AskedThrough<'a>(&'a Run);

impl fmt::Display for AskedThrough<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut runs: Vec<&Run, Global> = Vec::new_in(Global);
        // SAFETY: an outer run is below on the same stack.
        let mut at = unsafe { self.0.outer.as_ref() };
        while let Some(outer) = at {
            if !outer.is_recorder() {
                runs.push(outer);
            }
            // SAFETY: as above.
            at = unsafe { outer.outer.as_ref() };
        }
        if runs.is_empty() {
            return f.write_str("(the task's root)");
        }
        runs.reverse();
        write!(f, "{}", Path(&runs))
    }
}

fn innermost<'a>() -> Option<&'a Run> {
    // SAFETY: the innermost run is on this task's stack and outlives this call's use.
    unsafe { innermost_ptr().as_ref() }
}

/// Notes that the innermost run reads arena `slot`: a borrow of it by the run's home.
///
/// # Panics
///
/// Where a run homed at the session arena reads a crate's arena: its key names no crate, so
/// its entry would be dropped with any crate's and drop every arena that read it. Give the
/// query a key that names the crate.
#[inline]
pub(crate) fn note_read_of(memo: &Memo, slot: &Slot) {
    let Some(run) = innermost() else { return };
    // SAFETY: a run's slot is held by the snapshot it runs in.
    let Some(home) = (unsafe { run.slot.as_ref() }) else { return };
    if home.id == slot.id {
        return;
    }
    if home.id.is_session() && !slot.id.is_session() {
        panic!(
            "{} is homed at the session arena (its key names no arena) but reads {:?}: give it \
             a key that names the crate; asked through {}",
            run.name(),
            slot.id,
            asked_through(run)
        );
    }
    // The same mistake one level down: an open arena's entries are keyed by inputs no crate
    // owns (a source root's files), so one that reads a crate's arena is missing the crate in
    // its key, and would drop every arena that named the open one with that crate's edits.
    if home.is_open() && !home.reaches(slot.id) && !slot.is_open() {
        panic!(
            "{} is homed at the open arena {:?} (its key names only inputs no crate owns) but \
             reads {:?}: give it a key that names the crate; asked through {}",
            run.name(),
            home.id,
            slot.id,
            asked_through(run)
        );
    }
    if run.imaged && !home.reaches(slot.id) {
        panic!(
            "{} is an imaged query homed at {:?} but reads {:?}, outside its home's declared \
             borrows (`Inputs::borrows`): an entry served from the image sets no borrow, so an \
             imaged query reads only what its home's declared closure keeps it sound to carry",
            run.name(),
            home.id,
            slot.id
        );
    }
    home.borrow(slot.id, memo.epoch());
}

/// Lists `fact` as read by the innermost run.
pub(crate) fn note_fact(memo: &Memo, fact: Read) {
    if memo.reads() != Reads::Kept {
        return;
    }
    if let Some(run) = innermost()
        && run.keeping
    {
        run.reads.borrow_mut().push(fact);
    }
}

/// What an ask of the entry `head` (homed at `arena`) adds to the innermost run's list, hit or
/// run alike: ONE row. For a boundary kind a [`Read::Query`] naming the entry (the firewall:
/// its own list stays behind); for any other a callee row naming the entry, whose reads its
/// asker's flat list reaches through it when one is asked ([`crate::record`]); nothing for an
/// entry that read nothing. O(1) whatever the entry read.
/// This is a hit's; [`pass_on`] is a run's own end.
#[inline]
pub(crate) fn replay(memo: &Memo, arena: crate::ArenaId, head: &Head) {
    if memo.plant() == Plant::HitSkipsReplay {
        return;
    }
    pass_on(memo, arena, head);
}

/// [`replay`] at the end of the entry's own run.
#[inline]
pub(crate) fn pass_on(memo: &Memo, arena: crate::ArenaId, head: &Head) {
    if memo.reads() != Reads::Kept {
        return;
    }
    let Some(run) = innermost() else { return };
    if !run.keeping {
        return;
    }
    if memo.is_boundary(head.kind) && memo.plant() != Plant::BoundaryLeaks {
        let row = Read::Query { kind: KindId::of(head.kind), key: Gid::new(arena, head.local) };
        run.reads.borrow_mut().push(row);
    } else if head.lists().is_none() {
        return;
    } else if memo.plant() == Plant::AskCopiesFlatList {
        // The eager lists this replaced: the callee's whole flat list copied into the asker.
        let flat = crate::record::flat_of(memo, head);
        run.reads.borrow_mut().extend_from_slice(flat);
        crate::counts::rows_replayed(flat.len());
        return;
    } else {
        run.callees.borrow_mut().push(ptr::from_ref(head));
    }
    crate::counts::rows_replayed(1);
}

/// Appends what a run (a recorder's frame, or a fixpoint member's round, which has no entry
/// to name yet) read directly to the innermost run's lists: its asker read it too. O(its
/// direct rows), never its callees' reads.
pub(crate) fn replay_taken(taken: &Taken) {
    if taken.is_empty() {
        return;
    }
    if let Some(run) = innermost()
        && run.keeping
    {
        run.reads.borrow_mut().extend_from_slice(&taken.reads);
        run.callees.borrow_mut().extend_from_slice(&taken.callees);
        crate::counts::rows_replayed(taken.len());
    }
}

/// The arena the innermost run allocates in.
///
/// # Panics
///
/// Outside any run: a kept value lives in the arena of the run that keeps it.
pub(crate) fn home_slot<'a>() -> &'a Slot {
    let run = innermost().expect("`blaze_memo::keep*` outside a query run: nothing owns it");
    // SAFETY: a run's slot is held by the snapshot it runs in, which outlives the run.
    unsafe { run.slot.as_ref() }.expect("`blaze_memo::keep*` in a recorder outside any run")
}

/// The innermost run's name, for messages.
pub(crate) fn innermost_name() -> Option<String> {
    innermost().map(|run| run.name().to_string())
}

/// The value an ask of `key` gets when this task is running `key` already: the cycle's
/// provisional value, made by `make` (the fallback or the initial value, allocated in the
/// arena) the first time.
///
/// Marks every run from the innermost down to the one running `key` a member of the cycle's
/// outermost head.
///
/// # Panics
///
/// Where `key` is on no run of this task (the flight said it was), or where a run in the cycle
/// declared no recovery: the cycle's path is the message.
pub(crate) fn reenter(
    kind: &'static Kind,
    hash: u64,
    key: *const (),
    make: impl FnOnce() -> *const (),
) -> *const () {
    let mut path: Vec<&Run, Global> = Vec::new_in(Global);
    let mut at = innermost();
    let found = loop {
        let Some(run) = at else {
            panic!(
                "the flight says this task runs {} already, but no run of it is on the task's \
                 stack",
                kind.name()
            );
        };
        if !run.is_recorder() && run.matches(kind, hash, key) {
            break run;
        }
        if !run.is_recorder() {
            path.push(run);
        }
        // SAFETY: an outer run is below on the same stack.
        at = unsafe { run.outer.as_ref() };
    };
    let refuses = |run: &Run| run.kind.cycle() == crate::CycleKind::Panic;
    if refuses(found) || path.iter().any(|it| refuses(*it)) {
        let mut runs: Vec<&Run, Global> = Vec::new_in(Global);
        runs.push(found);
        runs.extend(path.iter().rev().copied());
        runs.push(found);
        panic!("cycle with no recovery (declare `cycle_result` or `cycle_initial`): {}", Path(&runs));
    }
    let head = match found.head_of.get() {
        it if it.is_null() => ptr::from_ref(found),
        it => it,
    };
    // SAFETY: the head is on this task's stack at or below `found`.
    let head_ref = unsafe { &*head };
    found.reentered.set(true);
    for run in &path {
        join(run, head_ref);
    }
    if head_ref.book.borrow().is_none() {
        *head_ref.book.borrow_mut() = Some(Book::new());
    }
    crate::counts::cycle();
    if found.provisional.get().is_null() {
        let last = last_of(head_ref, kind, hash, key);
        found.provisional.set(if last.is_null() { make() } else { last });
    }
    found.provisional.get()
}

/// Makes `run` a member of `head`'s cycle unless it is one of an outer head's already.
fn join(run: &Run, head: &Run) {
    let now = run.head_of.get();
    // SAFETY: a head is on this task's stack below `run`.
    if now.is_null() || unsafe { (*now).depth } > head.depth {
        run.head_of.set(head);
    }
}

/// The value an inner head had at the end of the last round, null when none.
fn last_of(head: &Run, kind: &'static Kind, hash: u64, key: *const ()) -> *const () {
    let book = head.book.borrow();
    let Some(book) = book.as_ref() else { return ptr::null() };
    // SAFETY: the keys are of `kind`'s key type, matched first.
    unsafe { book.find_prov(kind, hash, key, None) }
}

/// The value a fixpoint member of a cycle on this task's stack had this round, if it ran:
/// what an ask of it gets until the head settles (members are not memoized before then).
pub(crate) fn provisional(kind: &'static Kind, hash: u64, key: *const ()) -> *const () {
    let Some(first) = innermost() else { return ptr::null() };
    if !first.in_cycle() {
        return ptr::null();
    }
    let mut at = Some(first);
    while let Some(run) = at {
        let found = {
            let book = run.book.borrow();
            book.as_ref().and_then(|book| {
                // SAFETY: as in `last_of`.
                let value = unsafe { book.find_prov(kind, hash, key, Some(book.round)) };
                (!value.is_null()).then_some(value)
            })
        };
        if let Some(value) = found {
            // Every run from the innermost to the head read a provisional value.
            let mut member = innermost();
            while let Some(it) = member {
                if ptr::eq(it, run) {
                    break;
                }
                join(it, run);
                // SAFETY: as above.
                member = unsafe { it.outer.as_ref() };
            }
            return value;
        }
        // SAFETY: an outer run is below on the same stack.
        at = unsafe { run.outer.as_ref() };
    }
    ptr::null()
}

/// Keeps a fixpoint member's value of this round in its head's book; `moved` says whether an
/// inner head's value differs from its last.
pub(crate) fn member_done(
    run: &Run,
    key: *const (),
    value: *const (),
    moved: bool,
    settle: Settle,
) {
    // SAFETY: a member's head is on this task's stack below it.
    let head = unsafe { &*run.head_of.get() };
    let mut book = head.book.borrow_mut();
    let book = book.get_or_insert_with(Book::new);
    book.moved |= moved;
    if moved && book.movers.len() < MOVERS_NAMED {
        book.movers.push(run.name().to_string());
    }
    let round = book.round;
    book.push_prov(Prov {
        kind: run.kind,
        hash: run.hash,
        key,
        same_key: run.same_key,
        value,
        round,
        prev_same: usize::MAX,
        settle,
    });
}

impl Run {
    /// Publishes, as final, the value each member of this head's cycle had in the round that
    /// just settled (`round`, before [`Run::next_round`] moved past it), each with `lists`: the
    /// head's entry's, published just before. Members of earlier rounds that the last round
    /// did not reach are not published: nothing asked them.
    ///
    /// One list for the component: a member's value is a function of its head's provisional
    /// value, so it depends on everything any member read in any round; and every member's
    /// rows of every round reached the head's list (a member's round passes its direct rows to
    /// its asker, `replay_taken`, down the cycle's path to the head). So the head's lists are
    /// each member's, and the component's flat list is made once, for whichever is asked first.
    pub(crate) fn settle_members(&self, memo: &Memo, round: u32, lists: *const Lists) {
        let book = self.book.borrow();
        let Some(book) = book.as_ref() else { return };
        for prov in book.provs.iter().filter(|it| it.round == round) {
            // SAFETY: the member's slot is held by `memo`; its key and value were made in that
            // slot's arena for this `publish`'s types; `lists` is the head's entry's, in an
            // arena that outlives the member's (`record::Lists`).
            unsafe {
                (prov.settle.publish)(
                    memo,
                    &*prov.settle.slot,
                    prov.kind,
                    prov.hash,
                    prov.key,
                    prov.value,
                    lists,
                );
            }
        }
    }

    /// The round this head is in.
    pub(crate) fn round(&self) -> u32 {
        self.book.borrow().as_ref().map_or(0, |it| it.round)
    }
}
