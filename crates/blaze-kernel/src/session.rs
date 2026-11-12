//! A session: what one process verified under one pinned world, the index that says who read
//! what, and what carries both to the next pin (docs/v2/plan/design/kernel.md, "Carrying a
//! session").
//!
//! - **Rows.** For an identity, the key of the entry valid at the session's pin, the hash of
//!   its value once a parent asked for it, and its height: one more than the highest step it
//!   reads, 0 where it reads no step.
//! - **Postings.** For a read's name (its kind and key, as a read-list keeps it), the rows whose
//!   entry's list names it. A read of a step's value is posted under its name as any other, and
//!   that name also says which row is the step's own ([`Session::link`]), so the readers of a
//!   row are the postings of its name.
//! - **Admission is two calls that cannot be taken apart.** [`Session::post`] writes the
//!   postings of a list and gives a [`Posted`]; [`Session::admit`] makes a row live and takes a
//!   [`Posted`], which nothing else makes. So no row is live whose postings are not written
//!   (invariant I1), and nothing outside this module inserts a row. A posting with no live row
//!   (a lookup that failed after it posted, a row dropped since) costs a check at a carry and
//!   changes no answer.
//! - **The memory is the session's**: rows, postings and names live as long as whoever holds
//!   the [`Session`] keeps it, and are freed with it. Postings are never taken out while it
//!   lives: a row that is dropped and admitted again finds its postings written
//!   (`Row::posted`), so they do not grow with the number of carries.

use std::alloc::Global;
use std::collections::BTreeSet;
use std::fmt;

use blaze_cas::Key;
use blaze_ids::ContentId;

#[cfg(test)]
use crate::Seen;
#[cfg(test)]
use crate::nest::planted::{self, Bug};
use crate::{KernelError, Pin, Read, ReadKind, StepId, World, entry, hex};

/// An entry found valid or made under a session's pin, as its row keeps it.
#[derive(Clone)]
pub(crate) struct Known {
    pub key: Key,
    /// The hash of its value's bytes: none while only its key was needed (a value asked for at
    /// the top, which no step has read).
    pub hash: Option<ContentId>,
    /// Its reads' names and what each saw: read only by a planted bug's control.
    #[cfg(test)]
    pub reads: Vec<(Vec<u8>, Seen)>,
}

/// The live part of a row.
#[cfg_attr(test, derive(Clone))]
struct Live {
    known: Known,
    height: u32,
}

/// One identity the session admitted, now or before.
#[cfg_attr(test, derive(Clone))]
struct Row {
    /// None once a carry dropped it: it is looked up again when asked for.
    live: Option<Live>,
    /// The slots of its identity's chain whose lists' postings are written, by bit, for the
    /// first 64 slots: a list past those is checked reader by reader when it is posted.
    posted: u64,
    /// The name of the read of its value, by its place in `posts`, once a step read it.
    name: Option<u32>,
}

/// The readers of one read.
#[cfg_attr(test, derive(Clone))]
struct Post {
    /// The rows whose entry's list names the read, each once.
    readers: Vec<u32>,
    /// For a read of a step's value: that step's own row.
    row: Option<u32>,
}

impl Post {
    fn new() -> Post {
        // alloc: none until a reader is posted
        Post { readers: Vec::new(), row: None }
    }
}

/// Which transition admits an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum By {
    /// A lookup found it in the store: kept by an earlier request, another process or an
    /// earlier daemon.
    Hit,
    /// It was computed now and kept.
    Keep,
}

/// The postings of one list of one identity are written: what [`Session::admit`] takes. Only
/// [`Session::post`] makes one.
pub(crate) struct Posted {
    row: u32,
    height: u32,
}

/// What this process verified under one pinned world, through one store and one registry of
/// steps: for an identity, the key of the entry valid at that pin and the hash of its value. A
/// value in it is not looked up again: a child asked for by many parents, in one request or in
/// many, is verified once, and a value asked for again at the top is one get of its entry,
/// with no probe, no read-list read and no read valued.
///
/// It is of one pin ([`World::pin`]). A request whose world is at another pin is refused
/// ([`KernelError::Moved`]), never answered from rows of the session's pin; a world that
/// moved while a request ran is refused the same way, the request's answer is not given, and
/// the rows are dropped. Whoever holds the session carries it to the world as it is now
/// ([`crate::Kernel::carry`]) or makes a new one.
///
/// The invariant: a session changes no answer. Dropped or cleared between any two requests,
/// every request gives the bytes it gives with it kept (`exhaustive/nested.rs`: every sequence
/// with a drop at every place; control `a_session_that_outlives_its_pin_is_caught`).
#[cfg_attr(test, derive(Clone))]
pub struct Session {
    pin: Pin,
    /// The registry of steps its rows were verified under ([`crate::Kernel::registry`]): none
    /// until its first request.
    registry: Option<ContentId>,
    rows: blaze_hash::IndexMap<ContentId, Row, Global>,
    /// How many rows are live.
    live: usize,
    posts: blaze_hash::IndexMap<Box<[u8]>, Post, Global>,
    /// For a kind of read the world answers: the names of that kind, by place in `posts`.
    kinds: blaze_hash::HashMap<u16, Vec<u32>, Global>,
    /// The name being looked for by [`Session::link`]: one buffer, reused.
    scratch: Vec<u8>,
    /// A planted bug's postings, written a carry late.
    #[cfg(test)]
    late: Vec<(u32, u32)>,
}

impl Session {
    /// A session of `world` as it is now, with nothing verified.
    pub fn new<W: World + ?Sized>(world: &W) -> Session {
        Session {
            pin: world.pin(),
            registry: None,
            // alloc: the session's rows; empty until a value is verified
            rows: blaze_hash::IndexMap::new_in(Global),
            live: 0,
            // alloc: the session's postings; empty until a value with reads is verified
            posts: blaze_hash::IndexMap::new_in(Global),
            // alloc: the session's names by kind; empty until a value with reads is verified
            kinds: blaze_hash::HashMap::new_in(Global),
            // alloc: none until a step's value is read
            scratch: Vec::new(),
            #[cfg(test)]
            late: Vec::new(),
        }
    }

    /// The pin it is of.
    pub fn pin(&self) -> Pin {
        self.pin
    }

    /// How many values it holds verified.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Drops what it verified, postings too. No answer changes: each value is looked up again
    /// when asked for.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.live = 0;
        self.posts.clear();
        self.kinds.clear();
        #[cfg(test)]
        self.late.clear();
    }

    /// Refuses, before a request reads anything, a world that is not at the session's pin and
    /// a kernel whose registry is not the one its rows were verified under.
    pub(crate) fn enter<W: World + ?Sized>(
        &mut self,
        world: &W,
        registry: ContentId,
    ) -> Result<(), KernelError> {
        match self.registry {
            None => self.registry = Some(registry),
            Some(had) if had != registry => {
                return Err(KernelError::Reregistered { session: had, now: registry });
            }
            Some(_) => {}
        }
        let now = world.pin();
        if now == self.pin || Session::planted_stale() {
            return Ok(());
        }
        Err(KernelError::Moved { pinned: self.pin, now })
    }

    /// Refuses a world that moved while a request ran, and drops the rows: some were verified
    /// against a world that is no longer the pinned one.
    pub(crate) fn leave<W: World + ?Sized>(&mut self, world: &W) -> Result<(), KernelError> {
        let now = world.pin();
        if now == self.pin || Session::planted_stale() {
            return Ok(());
        }
        self.clear();
        Err(KernelError::Moved { pinned: self.pin, now })
    }

    fn planted_stale() -> bool {
        #[cfg(test)]
        {
            if planted::is(Bug::StalePin) {
                return true;
            }
        }
        false
    }

    /// The live row of the identity whose ID is `id`: its number and what it keeps.
    pub(crate) fn known(&self, id: &ContentId) -> Option<(u32, Known)> {
        let (row, _, it) = self.rows.get_full(id)?;
        let live = it.live.as_ref()?;
        // clone: a key and a hash; a planted bug's reads in a test
        Some((row as u32, live.known.clone()))
    }

    /// Writes the postings of the entry the identity `id` is about to be admitted with: each
    /// read `names` names (a name as a read-list keeps it) → this identity's row. `slot` is
    /// the slot of the list in the identity's chain, none for an entry with no reads. The
    /// row's height comes with it: one more than the highest step among the reads.
    ///
    /// The first half of an admission (the module's doc): [`Session::admit`] takes what this
    /// gives. Each step among the reads was looked up or run just before, through
    /// [`Session::link`], so its row is live and its name says which row that is.
    pub(crate) fn post<'n>(
        &mut self,
        id: ContentId,
        slot: Option<u32>,
        names: impl Iterator<Item = &'n [u8]>,
        by: By,
    ) -> Posted {
        // alloc: the session's rows, one an identity it ever admitted
        let (row, _) = self.rows.get_or_insert_with(id, || Row { live: None, posted: 0, name: None });
        let bit = slot.filter(|n| *n < 64).map(|n| 1u64 << n);
        let written = bit.is_some_and(|bit| self.rows[row].posted & bit != 0);
        let row = u32::try_from(row).expect("a session's rows are counted in u32");
        let unwritten = Session::planted_unposted(by);
        let mut height = 0;
        for name in names {
            let read = entry::named(name).expect("a read-list's name starts with its kind");
            let at = match self.posts.get_index_of(name) {
                Some(at) => at,
                None => {
                    // alloc: the session's names, one a read it ever posted
                    let at = self.posts.insert_full(Box::from(name), Post::new()).0;
                    if read.kind != ReadKind::STEP {
                        let at = u32::try_from(at).expect("a session's names are counted in u32");
                        // alloc: the session's names by kind
                        self.kinds.entry(read.kind.0).or_insert_with(Vec::new).push(at);
                    }
                    at
                }
            };
            let post = self.posts.get_index_mut(at).expect("a name found or put just now").1;
            if read.kind == ReadKind::STEP {
                let child = post.row.expect(
                    "a step whose value an entry read is looked up or run, and linked, before the entry is posted",
                );
                let child = self.rows[child as usize].live.as_ref().expect(
                    "a step whose value an entry read is live in the session while the entry is posted",
                );
                height = height.max(child.height + 1);
            }
            if written || unwritten {
                continue;
            }
            #[cfg(test)]
            {
                if planted::is(Bug::PostLate) {
                    self.late.push((at as u32, row));
                    continue;
                }
            }
            if bit.is_some() || !post.readers.contains(&row) {
                // alloc: the session's postings, one a read of a list of a row
                post.readers.push(row);
            }
        }
        if let Some(bit) = bit
            && !unwritten
            && !Session::planted_late()
        {
            self.rows.get_index_mut(row as usize).expect("the row made above").1.posted |= bit;
        }
        Posted { row, height }
    }

    fn planted_unposted(by: By) -> bool {
        #[cfg(test)]
        {
            if by == By::Hit && planted::is(Bug::HitUnposted) {
                return true;
            }
        }
        let _ = by;
        false
    }

    fn planted_late() -> bool {
        #[cfg(test)]
        {
            if planted::is(Bug::PostLate) {
                return true;
            }
        }
        false
    }

    /// Makes the row whose postings `posted` says are written live with `known`: its number.
    /// The second half of an admission, and the only place a row becomes live.
    pub(crate) fn admit(&mut self, posted: Posted, known: Known) -> u32 {
        let row = self.rows.get_index_mut(posted.row as usize).expect("a posting is of a row").1;
        if row.live.is_none() {
            self.live += 1;
        }
        row.live = Some(Live { known, height: posted.height });
        posted.row
    }

    /// Gives the live row `row` the hash of its value, read from its entry for the first step
    /// that asked for it.
    pub(crate) fn fill(&mut self, row: u32, known: Known) {
        let Some(live) = self.live_mut(row) else {
            return;
        };
        if known.hash.is_some() {
            live.known = known;
        }
    }

    fn live_mut(&mut self, row: u32) -> Option<&mut Live> {
        self.rows.get_index_mut(row as usize)?.1.live.as_mut()
    }

    fn live_of(&self, row: u32) -> Option<&Live> {
        self.rows.get_index(row as usize)?.1.live.as_ref()
    }

    /// Says that `row` is the value a read of the value of the step numbered `step` of the
    /// input `input` holds names: from then on the postings of that name are the row's
    /// readers, and a carry can look the row up again by the read's key. Called wherever a
    /// step is looked up or run for another (`nest::looked`, `nest::asked`), before its
    /// reader is posted. Once a row: later calls find it said.
    pub(crate) fn link(&mut self, row: u32, step: StepId, input: &[u8]) {
        if self.rows[row as usize].name.is_some() {
            return;
        }
        self.scratch.clear();
        entry::name_kind(ReadKind::STEP, &mut self.scratch);
        // alloc: the session's one buffer for a name, reused
        self.scratch.extend_from_slice(&step.0.to_be_bytes());
        // alloc: the session's one buffer for a name, reused
        self.scratch.extend_from_slice(input);
        let at = match self.posts.get_index_of(self.scratch.as_slice()) {
            Some(at) => at,
            // alloc: the session's names, one a step's value another step read
            None => self.posts.insert_full(Box::from(self.scratch.as_slice()), Post::new()).0,
        };
        self.posts.get_index_mut(at).expect("a name found or put just now").1.row = Some(row);
        let at = u32::try_from(at).expect("a session's names are counted in u32");
        self.rows.get_index_mut(row as usize).expect("a row the caller was given").1.name = Some(at);
    }

    /// The key a read of the value of `row` names it by, once a step read it.
    pub(crate) fn asked(&self, row: u32) -> Option<&[u8]> {
        let name = self.rows.get_index(row as usize)?.1.name?;
        let (name, _) = self.posts.get_index(name as usize)?;
        Some(&name[2..])
    }

    /// The hash of the value of `row`, where it is live and a step read it.
    pub(crate) fn hash_of(&self, row: u32) -> Option<ContentId> {
        self.live_of(row)?.known.hash
    }

    pub(crate) fn is_live(&self, row: u32) -> bool {
        self.live_of(row).is_some()
    }

    /// The `n`th reader of `row`, live or not: none past the last, and none for a row no step
    /// read.
    pub(crate) fn reader(&self, row: u32, n: usize) -> Option<u32> {
        let name = self.rows.get_index(row as usize)?.1.name?;
        self.posts.get_index(name as usize)?.1.readers.get(n).copied()
    }

    /// Drops the live row `row` and queues it for the carry's repair, at its height, with the
    /// hash its readers saw. A row that is not live is left: it is queued already, or dropped.
    pub(crate) fn dirty(&mut self, row: u32, queue: &mut Queue) -> bool {
        let Some(it) = self.rows.get_index_mut(row as usize) else {
            return false;
        };
        let Some(live) = it.1.live.take() else {
            return false;
        };
        self.live -= 1;
        // alloc: one carry's queue
        queue.order.insert((live.height, row));
        // The first hash stays while the row is queued: it is what every reader not yet
        // queued was verified against.
        // alloc: one carry's queue
        queue.was.entry(row).or_insert(live.known.hash);
        true
    }

    /// The start of a carry to `changed.to`: refuses a change set that is not from the
    /// session's pin, not to the world's, or not total; drops the session whole where the
    /// registry is another; else moves the pin and gives the rows `changed` reaches directly,
    /// dropped and queued. None: the session was dropped whole.
    ///
    /// Total (invariant I2): `changed` says, for every kind of read the session has a posting
    /// of, either the exact names that differ or that every one may. A kind it does not
    /// mention is a refusal naming the kind, never read as "nothing changed".
    pub(crate) fn seed(
        &mut self,
        registry: ContentId,
        now: Pin,
        changed: &Changed,
    ) -> Result<Option<Queue>, KernelError> {
        if changed.from != self.pin {
            return Err(KernelError::Carry(CarryError::From {
                session: self.pin,
                from: changed.from,
            }));
        }
        if changed.to != now {
            return Err(KernelError::Moved { pinned: changed.to, now });
        }
        if changed.kinds.contains_key(&ReadKind::STEP.0) {
            return Err(KernelError::Carry(CarryError::Steps));
        }
        if self.registry.is_some_and(|had| had != registry) {
            self.clear();
            self.registry = Some(registry);
            self.pin = changed.to;
            return Ok(None);
        }
        self.registry = Some(registry);
        let unsaid = self
            .kinds
            .iter()
            .filter(|(kind, names)| !names.is_empty() && !changed.kinds.contains_key(*kind))
            .map(|(kind, names)| (*kind, names.len()))
            .min();
        if let Some((kind, names)) = unsaid {
            return Err(KernelError::Carry(CarryError::Unsaid { kind: ReadKind(kind), names }));
        }
        let mut queue = Queue::new(self.rows.len());
        for (kind, difference) in changed.kinds.iter() {
            match difference {
                Difference::Exact(names) => {
                    for name in names {
                        if let Some(at) = self.posts.get_index_of(&**name) {
                            self.dirty_readers(at, &mut queue);
                        }
                    }
                }
                Difference::Everything => {
                    let names = self.kinds.get(kind).map_or(0, Vec::len);
                    for n in 0..names {
                        let at = self.kinds[kind][n] as usize;
                        self.dirty_readers(at, &mut queue);
                    }
                }
            }
        }
        self.pin = changed.to;
        #[cfg(test)]
        {
            // The planted bug's postings, written now: after the seed that needed them.
            for (at, row) in std::mem::take(&mut self.late) {
                let post = self.posts.get_index_mut(at as usize).expect("a name posted late").1;
                if !post.readers.contains(&row) {
                    post.readers.push(row);
                }
            }
        }
        Ok(Some(queue))
    }

    /// Queues every live reader of the read at `at` in `posts`.
    fn dirty_readers(&mut self, at: usize, queue: &mut Queue) {
        let readers = self.posts.get_index(at).map_or(0, |(_, post)| post.readers.len());
        for n in 0..readers {
            let row = self.posts.get_index(at).expect("a name counted just now").1.readers[n];
            self.dirty(row, queue);
        }
    }

    /// Itself, for a test that goes back to an earlier state.
    #[cfg(test)]
    pub(crate) fn saved(&self) -> Session {
        // clone: a test's copy of the session
        self.clone()
    }

    /// The session `saved` gave.
    #[cfg(test)]
    pub(crate) fn restored(saved: &Session) -> Session {
        // clone: a test's copy of the session
        saved.clone()
    }
}

/// The rows a carry has still to repair: lowest first, each with the hash its readers saw
/// before the carry reached it. One carry's memory, freed when it ends.
pub(crate) struct Queue {
    order: BTreeSet<(u32, u32)>,
    was: blaze_hash::HashMap<u32, Option<ContentId>, Global>,
    /// How many rows may still be taken (rule 18): [`Popped::Spent`] past it.
    left: usize,
    /// A planted bug's rows repaired already, which it does not queue again.
    #[cfg(test)]
    done: Vec<u32>,
}

/// What [`Queue::pop`] gives.
pub(crate) enum Popped {
    /// The lowest row queued, and the hash its readers saw.
    Row { row: u32, was: Option<ContentId> },
    Empty,
    /// More rows were taken than a carry of this session may repair.
    Spent,
}

impl Queue {
    /// The bound: a carry whose heights are right repairs a row once, and one that met a row
    /// below a taller child repairs it again after that child, so a few times the session's
    /// rows is far from any carry that ends; past it the session is dropped whole
    /// ([`Dropped::Repairs`]).
    fn new(rows: usize) -> Queue {
        Queue {
            // alloc: one carry's queue; empty where nothing the session read changed
            order: BTreeSet::new(),
            // alloc: one carry's queue; empty where nothing the session read changed
            was: blaze_hash::HashMap::new_in(Global),
            left: rows.saturating_mul(8).saturating_add(64),
            #[cfg(test)]
            done: Vec::new(),
        }
    }

    /// How many rows are queued.
    pub(crate) fn len(&self) -> usize {
        self.was.len()
    }

    /// The lowest row queued (invariant I3: a row is repaired after every queued row it reads,
    /// which are lower).
    pub(crate) fn pop(&mut self) -> Popped {
        loop {
            let next = if Queue::planted_parent_first() {
                self.order.pop_last()
            } else {
                self.order.pop_first()
            };
            let Some((_, row)) = next else {
                return Popped::Empty;
            };
            // A row queued twice, at two heights, is repaired at the first.
            let Some(was) = self.was.remove(&row) else {
                continue;
            };
            if self.left == 0 {
                return Popped::Spent;
            }
            self.left -= 1;
            #[cfg(test)]
            {
                if Queue::planted_parent_first() {
                    self.done.push(row);
                }
            }
            return Popped::Row { row, was };
        }
    }

    /// Whether a changed row's reader `row` is to be queued: always, but for a planted bug.
    pub(crate) fn takes(&self, row: u32) -> bool {
        #[cfg(test)]
        {
            if Queue::planted_parent_first() && self.done.contains(&row) {
                return false;
            }
        }
        let _ = row;
        true
    }

    fn planted_parent_first() -> bool {
        #[cfg(test)]
        {
            if planted::is(Bug::ParentFirst) {
                return true;
            }
        }
        false
    }
}

/// What differs between two pins of a world, as whoever moved the world says it: the reads a
/// carry starts from ([`crate::Kernel::carry`]).
///
/// It is total by kind of read (invariant I2). For each kind the world answers, it says
/// either the exact reads that differ ([`Changed::exact`], [`Changed::differs`]) or that any
/// read of the kind may ([`Changed::everything`]: a kind with no exact difference, such as a
/// fact valued by something that keeps no history). A read that differs is one whose
/// [`World::hash`] at `to` is not what it was at `from`: a hash for another hash, a thing
/// that appeared where nothing was ([`crate::Seen::Gone`] before) or went, and a read that
/// can or cannot be valued where before it was the other. Postings are by name, so a name
/// nothing was under is covered like any other, as long as it is said here.
///
/// The kernel cannot check that the names are all of them: that is the one thing a carry
/// trusts, and what a driver's own tests are for (`exhaustive/nested.rs` has the control: a
/// fact that appears and is left out ends stale).
pub struct Changed {
    from: Pin,
    to: Pin,
    kinds: blaze_hash::HashMap<u16, Difference, Global>,
}

enum Difference {
    /// The names (kind and key) of the reads that differ.
    Exact(Vec<Box<[u8]>>),
    Everything,
}

impl Changed {
    /// The difference between the world at `from` and at `to`, with no kind said yet.
    pub fn between(from: Pin, to: Pin) -> Changed {
        // alloc: one carry's change set
        Changed { from, to, kinds: blaze_hash::HashMap::new_in(Global) }
    }

    /// Says the difference of `kind` is exact: only the reads given to [`Changed::differs`].
    pub fn exact(&mut self, kind: ReadKind) {
        // alloc: one carry's change set
        self.kinds.entry(kind.0).or_insert_with(|| Difference::Exact(Vec::new()));
    }

    /// Says `read` differs between the two pins. Its kind's difference is exact, unless
    /// [`Changed::everything`] was said of it.
    pub fn differs(&mut self, read: Read<'_>) {
        // alloc: one carry's change set
        let kind = self.kinds.entry(read.kind.0).or_insert_with(|| Difference::Exact(Vec::new()));
        if let Difference::Exact(names) = kind {
            // alloc: one carry's change set, one name a read that differs
            let mut name = Vec::with_capacity(2 + read.key.len());
            entry::name_kind(read.kind, &mut name);
            name.extend_from_slice(read.key);
            names.push(name.into_boxed_slice());
        }
    }

    /// Says any read of `kind` may differ: every entry with a read of that kind is looked up
    /// again.
    pub fn everything(&mut self, kind: ReadKind) {
        // alloc: one carry's change set
        self.kinds.insert(kind.0, Difference::Everything);
    }

    /// The pin it is from and the pin it is to.
    pub fn pins(&self) -> (Pin, Pin) {
        (self.from, self.to)
    }
}

/// Why a carry was refused before it changed anything: the session is as it was, at its pin.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CarryError {
    /// The change set is from another pin than the session's.
    From { session: Pin, from: Pin },
    /// The change set does not say what differs for a kind of read the session has: `names`
    /// reads of that kind are posted.
    Unsaid { kind: ReadKind, names: usize },
    /// The change set names reads of a step's value, which the kernel answers itself.
    Steps,
}

impl fmt::Display for CarryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CarryError::From { session, from } => write!(
                f,
                "a session of the world pinned at {:x} was given the changes from the pin {:x}: a carry starts from the session's own pin",
                session.0, from.0
            ),
            CarryError::Unsaid { kind, names } => write!(
                f,
                "the changes do not say what differs for reads of kind {} ({}), and the session's entries read {names} of them: say the reads that differ (Changed::exact, Changed::differs) or that any may (Changed::everything)",
                kind.0,
                hex(&kind.0.to_be_bytes())
            ),
            CarryError::Steps => write!(
                f,
                "the changes name reads of kind {}, a step's value: the kernel values those itself, from the reads the world answers",
                ReadKind::STEP.0
            ),
        }
    }
}

/// Why a carry dropped everything the session had verified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dropped {
    /// The kernel's registry of steps is not the one the session was verified under: a step's
    /// code, or which steps there are, is an input of every row.
    Registry,
    /// The repair took more rows than a carry of this session may (rule 18).
    Repairs,
}

/// What a carry did ([`crate::Kernel::carry`]): the counts an edit is reported with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Carried {
    /// Set where the session was dropped whole: it holds nothing, at the new pin.
    pub dropped: Option<Dropped>,
    /// Entries the change set reached directly: one of their own reads differs.
    pub seeded: usize,
    /// Entries looked up again (found in the store, else run).
    pub repaired: usize,
    /// Of those, the entries whose value is other bytes now: their readers were queued.
    pub changed: usize,
    /// Of those, the entries whose value is the same bytes: their readers stayed verified.
    pub cut: usize,
    /// Entries dropped from the session: no step had read them (they are looked up when next
    /// asked for), or they cannot be valued at the new pin.
    pub gone: usize,
}
