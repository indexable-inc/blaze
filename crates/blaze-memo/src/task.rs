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
    fmt::Write as _,
    num::NonZeroUsize,
    ptr,
    sync::Once,
};

use crate::{Fact, Kind, Memo, Plant, Reads, arena::Slot, trie::Head};

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
    /// What publishes the member's entry when its head settles: its value in the last round is
    /// final then (the head's value and every inner head's stood still, and a member is a
    /// function of them).
    settle: Settle,
}

/// A member's entry, made final when its head settles.
pub(crate) struct Settle {
    pub(crate) slot: *const Slot,
    /// The member's reads in its round, in its arena.
    pub(crate) reads: *const [Fact],
    /// Publishes `Entry<K, V>` of (key, value) in `slot`, the reads its list.
    pub(crate) publish: unsafe fn(&Memo, &Slot, &'static Kind, u64, *const (), *const (), &[Fact]),
}

/// What a cycle's head keeps across its rounds: its members' values this round. Global heap,
/// made only for a cycle.
struct Book {
    round: u32,
    provs: Vec<Prov, Global>,
    /// An inner head's value moved this round.
    moved: bool,
    /// The flight of every fixpoint member run so far, held by this task until the head
    /// settles: another task asking a member waits (for the head) instead of running it again,
    /// so no member's function runs on two tasks in one snapshot. Declared last: dropped
    /// after the book's values are published, and on an unwind with the head.
    keeps: Vec<KeptMember, Global>,
}

impl Book {
    fn new() -> Book {
        Book { round: 0, provs: Vec::new_in(Global), moved: false, keeps: Vec::new_in(Global) }
    }
}

/// A member whose flight the head holds.
struct KeptMember {
    kind: &'static Kind,
    hash: u64,
    key: *const (),
    same_key: unsafe fn(*const (), *const ()) -> bool,
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
    book.get_or_insert_with(Book::new).keeps.push(KeptMember {
        kind: member.kind,
        hash: member.hash,
        key: member.key,
        same_key: member.same_key,
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
        if let Some(book) = run.book.borrow().as_ref() {
            // SAFETY: the keys are of `kind`'s key type, matched first.
            kept |= book.keeps.iter().any(|it| {
                ptr::eq(it.kind, kind) && it.hash == hash && unsafe { (it.same_key)(it.key, key) }
            });
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
    describe: unsafe fn(&'static Kind, *const (), &mut String),
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
    reads: RefCell<Vec<Fact, Global>>,
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
    out: &mut String,
) {
    // SAFETY: the caller's.
    let key = unsafe { &*key.cast::<K>() };
    let _ = write!(out, "{}({key:?})", kind.name());
}

impl Run {
    /// A run of `kind` for `key` homed in `slot`, its outer run the task's innermost now.
    pub(crate) fn new<K: Eq + std::fmt::Debug>(
        memo: &Memo,
        kind: &'static Kind,
        slot: &Slot,
        hash: u64,
        key: &K,
    ) -> Run {
        let outer = INNERMOST.with(Cell::get);
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
        }
    }

    /// The recorder's frame: it gathers reads and lends its outer run's arena.
    pub(crate) fn recorder(memo: &Memo) -> Run {
        let outer = INNERMOST.with(Cell::get);
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
            keeping: true,
            reads: RefCell::new(Vec::new_in(Global)),
        }
    }

    fn in_cycle(&self) -> bool {
        self.inside || !self.head_of.get().is_null() || self.book.borrow().is_some()
    }

    fn is_recorder(&self) -> bool {
        ptr::eq(self.kind, &RECORDER)
    }

    fn name(&self) -> String {
        let mut out = String::new();
        // SAFETY: `key` points at the key `describe` was made for, on this task's stack.
        unsafe { (self.describe)(self.kind, self.key, &mut out) };
        out
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

    /// The facts this run read, sorted and deduplicated.
    ///
    /// # Panics
    ///
    /// Where one fact was read with two digests: inputs are fixed for a snapshot, so that is
    /// two snapshots' reads mixed.
    pub(crate) fn take_reads(&self) -> Vec<Fact, Global> {
        let mut reads = std::mem::replace(&mut *self.reads.borrow_mut(), Vec::new_in(Global));
        reads.sort_unstable();
        reads.dedup();
        for pair in reads.windows(2) {
            assert!(
                pair[0].id != pair[1].id,
                "{} read fact {:?} with two digests ({:?} and {:?}) in one snapshot",
                self.name(),
                pair[0].id,
                pair[0].digest,
                pair[1].digest
            );
        }
        reads
    }

    /// Whether the head's members moved this round, and starts the next.
    pub(crate) fn next_round(&self) -> bool {
        let mut book = self.book.borrow_mut();
        let Some(book) = book.as_mut() else { return false };
        let moved = book.moved;
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
        INNERMOST.with(|cell| cell.set(self.run.outer));
    }
}

pub(crate) fn push(run: &Run) -> Pushed<'_> {
    register();
    INNERMOST.with(|cell| {
        debug_assert!(ptr::eq(cell.get(), run.outer), "a run pushed onto another task's stack");
        cell.set(run);
    });
    Pushed { run }
}

fn innermost<'a>() -> Option<&'a Run> {
    // SAFETY: the innermost run is on this task's stack and outlives this call's use.
    unsafe { INNERMOST.with(Cell::get).as_ref() }
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
    if home.id.index() == 0 && slot.id.index() != 0 {
        panic!(
            "{} is homed at the session arena (its key names no arena) but reads {:?}: give it \
             a key that names the crate",
            run.name(),
            slot.id
        );
    }
    home.borrow(slot.id, memo.epoch());
}

/// Lists `fact` as read by the innermost run.
pub(crate) fn note_fact(memo: &Memo, fact: Fact) {
    if memo.reads() != Reads::Kept {
        return;
    }
    if let Some(run) = innermost()
        && run.keeping
    {
        run.reads.borrow_mut().push(fact);
    }
}

/// Replays an entry's read list into the innermost run: a hit depends on what the entry read.
#[inline]
pub(crate) fn replay(memo: &Memo, head: &Head) {
    if memo.reads() != Reads::Kept || memo.plant() == Plant::HitSkipsReplay {
        return;
    }
    replay_list(head.reads());
}

/// Appends `reads` to the innermost run's list.
pub(crate) fn replay_list(reads: &[Fact]) {
    if reads.is_empty() {
        return;
    }
    if let Some(run) = innermost()
        && run.keeping
    {
        run.reads.borrow_mut().extend_from_slice(reads);
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
    innermost().map(Run::name)
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
        let mut names: Vec<String, Global> = Vec::new_in(Global);
        names.push(found.name());
        names.extend(path.iter().rev().map(|it| it.name()));
        names.push(found.name());
        panic!(
            "cycle with no recovery (declare `cycle_result` or `cycle_initial`): {}",
            names.join(" -> ")
        );
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
    book.provs
        .iter()
        .rev()
        // SAFETY: the keys are of `kind`'s key type, matched first.
        .find(|it| ptr::eq(it.kind, kind) && it.hash == hash && unsafe { (it.same_key)(it.key, key) })
        .map_or(ptr::null(), |it| it.value)
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
                book.provs
                    .iter()
                    .rev()
                    // SAFETY: as in `last_of`.
                    .find(|it| {
                        it.round == book.round
                            && ptr::eq(it.kind, kind)
                            && it.hash == hash
                            && unsafe { (it.same_key)(it.key, key) }
                    })
                    .map(|it| it.value)
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
    book.provs.push(Prov {
        kind: run.kind,
        hash: run.hash,
        key,
        same_key: run.same_key,
        value,
        round: book.round,
        settle,
    });
}

impl Run {
    /// Publishes, as final, the value each member of this head's cycle had in the round that
    /// just settled (`round`, before [`Run::next_round`] moved past it). Members of earlier
    /// rounds that the last round did not reach are not published: nothing asked them.
    pub(crate) fn settle_members(&self, memo: &Memo, round: u32) {
        let book = self.book.borrow();
        let Some(book) = book.as_ref() else { return };
        for prov in book.provs.iter().filter(|it| it.round == round) {
            // SAFETY: the member's slot is held by `memo`; its reads, key and value were made
            // in that slot's arena for this `publish`'s types.
            unsafe {
                (prov.settle.publish)(
                    memo,
                    &*prov.settle.slot,
                    prov.kind,
                    prov.hash,
                    prov.key,
                    prov.value,
                    &*prov.settle.reads,
                );
            }
        }
    }

    /// The round this head is in.
    pub(crate) fn round(&self) -> u32 {
        self.book.borrow().as_ref().map_or(0, |it| it.round)
    }
}
