//! Nesting: a step that asks for another step's value, and the lookup that answers a read of
//! one (docs/v2/plan/design/kernel.md, "The kernel as the home", (b)).
//!
//! - **One read in the parent.** [`crate::Reads::step`] looks the child up, or runs it with
//!   reads of its own and keeps it, and records one read: kind [`ReadKind::STEP`], key the
//!   child's step number and its input's stable bytes ([`Ask::key`]), seen the hash of the
//!   child's value bytes. The child's code is not in the key: the registry gives it now, so a
//!   child whose reads or code changed and whose value did not leaves the parent a hit (early
//!   cutoff).
//! - **The kernel answers that read**, not the [`World`]: valuing it at lookup is the child's
//!   own lookup, and where that misses, the child's run, through the runner its registration
//!   left in the registry ([`asked`]).
//! - **A list is a guess.** A read-list holds names, not the hashes an entry saw, so a lookup
//!   cannot stop at a list's first changed read: it values every step read the identity's lists
//!   name, and some of those the step would not ask for now. So a child that cannot be valued
//!   during a lookup (it asks back for a value being looked up, its step refuses its input, a
//!   read of its world fails, its step is no longer registered) makes that list not hold,
//!   never the lookup fail ([`KernelError::unvaluable`]). The same failure in a run is the
//!   run's error: a run asks only for what it needs.
//! - **One walk.** A lookup walks its identity's lists once and values each read a list names
//!   once: a child is looked up inside the walk, through the same store the walk reads
//!   ([`Looker`]), and what it comes to is in the request's session for every later read of it.
//!   What a walk cannot do is run a child, since a run keeps and the walk holds a list of the
//!   store: a child with no entry valid now (or whose hit the request's setting checks by a
//!   run) is run after the walk, and only then are the lists walked a second, last time.
//! - **A read the world cannot value now** ([`crate::WorldError::not_now`]) makes its list
//!   not hold, as a child that cannot be valued does; in a run it is the run's error.
//! - **A session** ([`Session`]) keeps what was found valid under one pinned world, so a value
//!   is verified once however many parents and requests ask for it; a world at another pin is
//!   refused ([`KernelError::Moved`]). Every admission to it writes its postings first
//!   (`session`'s doc), which is what lets [`carry`] take it to the next pin by looking up
//!   only what a changed read reaches.
//! - **What a hit costs** (pinned by `tests::cost`): a step with no reads asked for at the top
//!   is one probe and one get; with reads, the probe of the entry with no reads, its lists,
//!   the probe of the answer and the one get of it. A child with reads is one get that finds
//!   no entry with no reads, its lists and one get of its answer, whose kept hash is what the
//!   parent's read sees ([`Entry::value_hash`]): the child's value is not hashed and not
//!   copied. A child with no reads is its bare value, got once and hashed.
//! - **Cycles are refused.** A value asked for while it is being looked up or computed is a
//!   [`KernelError::Cycle`] naming the identities on the way. The kernel has no fixpoint: steps
//!   that need each other are one step over their whole cycle, whose own business the
//!   iteration is.
//! - **A lookup that keeps nothing computes nothing** ([`Door::Look`], [`crate::Kernel::find`]):
//!   a child with no entry valid now makes the parent's list not hold.

use std::cell::RefCell;

use blaze_cas::{Key, content_of};
use blaze_ids::ContentId;
use blaze_store_check::{Hit, Recompute, RecordedRead, Setting};

use crate::reads::Unfinished;
use crate::session::{By, Known, Popped, Posted, Session};
use crate::{
    Ask, Carried, Changed, Compute, Dropped, Entry, Get, Identity, Kernel, KernelError, Link,
    Lists, Put, Read, ReadKind, Reads, Seen, Step, StepError, StepId, Trace, World, entry,
    find_key, hex, keep, of, said,
};

/// How deep steps may nest: each level is a few frames of this thread's stack, so the bound is
/// what keeps a long chain a typed refusal ([`KernelError::TooDeep`]) and never a stack
/// overflow (rule 18). A chain that needs more is a step that should be one step.
pub const MOST_NESTED: usize = 512;

/// A store lent to the engine as a `dyn Get`, whatever its type.
pub(crate) struct Lent<'a, G: ?Sized>(pub &'a G);

impl<G: Get + ?Sized> Get for Lent<'_, G> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        self.0.get(key)
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        self.0.has(key)
    }
}

/// A store lent to the engine as a `dyn Put`, whatever its type.
pub(crate) struct LentMut<'a, P: ?Sized>(pub &'a mut P);

impl<P: Put + ?Sized> Get for LentMut<'_, P> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        self.0.get(key)
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        self.0.has(key)
    }
}

impl<P: Put + ?Sized> Put for LentMut<'_, P> {
    fn put(
        &mut self,
        key: Key,
        kind: blaze_cas::Kind,
        bytes: Vec<u8>,
    ) -> Result<(), blaze_cas::Mismatch> {
        self.0.put(key, kind, bytes)
    }
}

/// The store of one request: read only, or read and kept in.
pub(crate) enum Door<'e> {
    /// A lookup that keeps nothing, so it computes nothing.
    Look(&'e (dyn Get + 'e)),
    /// A get: what misses is computed and kept.
    Keep(&'e mut (dyn Put + 'e)),
}

impl Door<'_> {
    fn get(&self) -> &dyn Get {
        match self {
            Door::Look(store) => *store,
            Door::Keep(store) => &**store,
        }
    }

    fn keeps(&self) -> bool {
        matches!(self, Door::Keep(_))
    }
}

/// What a read is now, to a lookup walking an identity's read-lists.
pub(crate) enum Now {
    Seen(Seen),
    /// A step's value that must be run before it is known (it has no entry valid now, or its
    /// hit is one the request's setting checks by a run): the list waits for the walk after
    /// that run.
    Pending,
    /// It cannot be valued now: the list does not hold.
    Not,
}

/// What a lookup brings of the entry it finds: each is one read of the store less than the
/// next, so a caller asks for the least it uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Need {
    /// Its key: the answer is only probed, and the caller reads the entry itself (the one get
    /// of a hit, borrowed from the caller's store for as long as the caller wants it).
    Key,
    /// The hash of its value: the answer is read once, the hash is the one the entry keeps
    /// ([`Entry::value_hash`]), and nothing is copied.
    Hash,
    /// The hash, and the value's bytes copied for the step that asked.
    Value,
}

/// The entry a lookup found, with as much of it as was needed ([`Need`]).
pub(crate) struct Found {
    pub key: Key,
    /// The hash of its value's bytes; none only where the key alone was needed.
    pub hash: Option<ContentId>,
    /// Its value, copied, where it was needed.
    pub value: Option<Vec<u8>>,
    /// That its list's postings are written in the session: set by the walk that found it in
    /// the store, taken by its admission. None for a value the session knew already.
    posted: Option<Posted>,
    /// Its reads' names and what each saw: read only by a planted bug's control.
    #[cfg(test)]
    reads: Vec<(Vec<u8>, Seen)>,
}

/// What looking a step up inside another's walk came to.
pub(crate) enum Looked {
    /// Its entry, and its row in the session.
    Found(Found, u32),
    /// No entry of it is valid now.
    Missing,
    /// It must be run before it is known, which a walk cannot do: only where the request keeps.
    Later,
}

/// One value on the way: being looked up or computed.
struct Frame {
    step: &'static str,
    identity: ContentId,
}

/// What one request has on the way, and its session.
struct State<'a> {
    /// The values being looked up or computed, outermost first.
    stack: &'a mut Vec<Frame>,
    /// The values found valid or made, by identity, and who read what.
    session: &'a mut Session,
}

impl State<'_> {
    fn enter<S: Step>(&mut self, input: &S::Input, identity: &Identity) -> Result<(), KernelError> {
        let id = identity.id();
        if let Some(at) = self.stack.iter().position(|it| it.identity == id) {
            return Err(KernelError::Cycle {
                of: of::<S>(input, identity),
                // alloc: an error
                cycle: self.stack[at..]
                    .iter()
                    .map(|it| Link { step: it.step, identity: it.identity })
                    .collect(),
            });
        }
        if self.stack.len() >= MOST_NESTED {
            return Err(KernelError::TooDeep { of: of::<S>(input, identity) });
        }
        // alloc: one request's stack, as deep as its steps nest
        self.stack.push(Frame { step: S::NAME, identity: id });
        Ok(())
    }

    /// Admits what a lookup found in the store: its row. The postings of its list were
    /// written by the walk that found it ([`Lists::held`]).
    fn admit(&mut self, found: &mut Found) -> u32 {
        let posted = found.posted.take().expect("a walk posts the list of the entry it found");
        self.session.admit(posted, known_of(found))
    }
}

/// A value the engine found or made.
pub(crate) struct Valued {
    pub key: Key,
    /// The hash of its value's bytes, what a parent's read of it saw: none only where the key
    /// alone was needed.
    pub hash: Option<ContentId>,
    pub how: How,
    /// Its row in the session.
    row: u32,
    /// Its reads' names and what each saw: read only by a planted bug's control.
    #[cfg(test)]
    reads: Vec<(Vec<u8>, Seen)>,
}

pub(crate) enum How {
    /// The store's entry; its value, copied, when the caller wanted it.
    Hit { value: Option<Vec<u8>> },
    /// Computed now and kept.
    Computed { value: Vec<u8>, reads: Trace },
}

/// What a step that asked for another's value is given.
pub(crate) struct Asked {
    pub hash: ContentId,
    /// The value's bytes, when they were wanted.
    pub value: Option<Vec<u8>>,
    #[cfg(test)]
    pub reads: Vec<(Vec<u8>, Seen)>,
}

/// What [`Reads`] asks for another step's value through: the engine, without its world's type
/// (the context's own world is all a step sees).
pub(crate) trait Asks {
    /// The value now of the step a [`ReadKind::STEP`] read names by `key`.
    fn asked(&mut self, key: &[u8]) -> Result<Asked, KernelError>;
}

/// A step that may be asked for, as the registry holds it: its typed lookup, run and keep
/// behind the bytes of its input.
pub(crate) struct Runner<W: ?Sized> {
    /// Looked up only, inside another step's walk.
    look: for<'a, 'l> fn(&'a mut Looker<'l, W>, &[u8], Need) -> Result<Looked, KernelError>,
    /// Looked up, else run and kept.
    value: for<'a, 'e> fn(&'a mut Engine<'e, W>, &[u8], Need) -> Result<Option<Asked>, KernelError>,
    pub label: fn(&[u8]) -> String,
}

impl<W: ?Sized> Clone for Runner<W> {
    fn clone(&self) -> Runner<W> {
        *self
    }
}

impl<W: ?Sized> Copy for Runner<W> {}

impl<W: World + ?Sized> Runner<W> {
    pub(crate) fn of<S: Ask<World = W>>() -> Runner<W> {
        Runner { look: looked::<S, W>, value: asked::<S, W>, label: label::<S> }
    }
}

fn input_of<S: Ask>(key: &[u8]) -> Result<S::Input, KernelError> {
    S::input(key).map_err(|why| KernelError::Input { step: S::NAME, key: hex(key), why })
}

/// `S` of the input `key` holds, looked up: the erased form of [`Looker::look`].
fn looked<S: Ask<World = W>, W: World + ?Sized>(
    looker: &mut Looker<'_, W>,
    key: &[u8],
    need: Need,
) -> Result<Looked, KernelError> {
    let input = input_of::<S>(key)?;
    let identity = looker.cx.kernel.identity::<S>(&input)?;
    let looked = looker.look::<S>(&input, &identity, need)?;
    if let Looked::Found(_, row) = &looked {
        // Before whoever asked is posted: the row's readers are the postings of this name.
        looker.state.session.link(*row, S::ID, key);
    }
    Ok(looked)
}

/// `S` of the input `key` holds: the erased form of [`Engine::value`].
fn asked<S: Ask<World = W>, W: World + ?Sized>(
    engine: &mut Engine<'_, W>,
    key: &[u8],
    need: Need,
) -> Result<Option<Asked>, KernelError> {
    let input = input_of::<S>(key)?;
    let identity = engine.cx.kernel.identity::<S>(&input)?;
    let Some(valued) = engine.value::<S>(&input, &identity, need)? else {
        return Ok(None);
    };
    // Before whoever asked is posted: the row's readers are the postings of this name.
    engine.session.link(valued.row, S::ID, key);
    let value = match valued.how {
        How::Hit { value } => value,
        How::Computed { value, .. } => Some(value),
    };
    Ok(Some(Asked {
        hash: valued.hash.expect("a value asked for by a step is valued with its hash"),
        value,
        #[cfg(test)]
        reads: valued.reads,
    }))
}

fn label<S: Ask>(key: &[u8]) -> String {
    match S::input(key) {
        Ok(input) => S::label(&input),
        // alloc: an error's or a check's text
        Err(why) => format!("input {} ({why})", hex(key)),
    }
}

/// The step's number and its input's bytes in the key of a read of a step's value.
fn step_of(key: &[u8]) -> Result<(StepId, &[u8]), KernelError> {
    match key.split_first_chunk::<2>() {
        Some((step, input)) => Ok((StepId(u16::from_be_bytes(*step)), input)),
        None => Err(KernelError::Input {
            step: "a step",
            key: hex(key),
            // alloc: an error
            why: "its key is too short to hold a step's number".to_owned(),
        }),
    }
}

/// What a read of a step's value saw: the hash of the value's bytes.
pub(crate) fn step_seen(key: &[u8], hash: ContentId) -> Seen {
    #[cfg(test)]
    {
        if planted::is(planted::Bug::IdentityOnly) {
            return Seen::Is(ContentId::of("blaze v2 kernel planted identity only 1", key));
        }
    }
    let _ = key;
    Seen::Is(hash)
}

/// The entry under `key`, as much of it as `need` says: probed for its key alone, else read
/// once. None where the store has nothing under `key`.
fn answer<S: Step>(
    store: &dyn Get,
    input: &S::Input,
    identity: &Identity,
    key: Key,
    need: Need,
) -> Result<Option<Found>, KernelError> {
    let failed = |why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
    if need == Need::Key {
        let has = store.has(&key).map_err(failed)?;
        return Ok(has.then(|| Found {
            key,
            hash: None,
            value: None,
            posted: None,
            #[cfg(test)]
            reads: Vec::new(),
        }));
    }
    let Some(bytes) = store.get(&key).map_err(failed)? else {
        return Ok(None);
    };
    let entry = Entry::read(bytes, key, identity).map_err(failed)?;
    Ok(Some(Found {
        key,
        hash: Some(entry.value_hash()),
        // alloc: a child's value, handed to the step that asked for it
        value: (need == Need::Value).then(|| entry.value.to_vec()),
        posted: None,
        #[cfg(test)]
        reads: planted::reads(entry.reads()),
    }))
}

/// The kernel, the world and the setting of one request.
struct Cx<'e, W: ?Sized> {
    kernel: &'e Kernel<W>,
    world: &'e W,
    verify: &'e Setting,
}

impl<W: ?Sized> Clone for Cx<'_, W> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<W: ?Sized> Copy for Cx<'_, W> {}

/// A request's lookups through a store it only reads: what a walk of an identity's read-lists
/// can do while it holds a list of that store. It looks children up; it never runs one.
pub(crate) struct Looker<'l, W: ?Sized> {
    cx: Cx<'l, W>,
    store: &'l (dyn Get + 'l),
    /// Whether the request keeps: a child with no entry valid now is then run after the walk,
    /// where otherwise it makes its parent's list not hold.
    keeps: bool,
    state: State<'l>,
}

impl<W: World + ?Sized> Looker<'_, W> {
    /// `S` of `input`, looked up: from the session, else by one walk of its lists.
    fn look<S: Step>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
        need: Need,
    ) -> Result<Looked, KernelError> {
        let id = identity.id();
        if let Some((row, known)) = self.state.session.known(self.store, &id)? {
            let fill = known.hash.is_none();
            let found = known_now::<S>(self.store, input, identity, known, need)?;
            if fill && found.hash.is_some() {
                self.state.session.fill(row, known_of(&found));
            }
            return Ok(Looked::Found(found, row));
        }
        if self.keeps && self.cx.verify.sampled(&id) {
            // Its hit is checked by a run, which asks for its children as any run does.
            return Ok(Looked::Later);
        }
        self.state.enter::<S>(input, identity)?;
        // alloc: one lookup's children to run and children that cannot be valued; both empty
        // where every child its lists name has an entry valid now
        let (mut pending, mut nots) = (Vec::new(), Vec::new());
        let found = self.walk::<S>(input, identity, need, &mut pending, &mut nots, false);
        self.state.stack.pop();
        match found? {
            Some(mut found) => {
                let row = self.state.admit(&mut found);
                Ok(Looked::Found(found, row))
            }
            None if pending.is_empty() => Ok(Looked::Missing),
            None => Ok(Looked::Later),
        }
    }

    /// One walk of the read-lists of `identity`: the entry whose reads all hash the same now.
    /// Each read a list names is valued once: a world's read by the world, a step's value by
    /// that step's own lookup through this same store. A child that must be run first goes to
    /// `pending` (its list is passed over) unless this is the request's `last` walk, where it
    /// makes its list not hold; one that cannot be valued goes to `nots`. The postings of the
    /// list that held are written in the session before the walk ends ([`Lists::held`]).
    fn walk<S: Step>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
        need: Need,
        pending: &mut Vec<Vec<u8>>,
        nots: &mut Vec<Vec<u8>>,
        last: bool,
    ) -> Result<Option<Found>, KernelError> {
        let store = self.store;
        let mut walk =
            Walk::<S, W> { looker: self, input, identity, need, pending, nots, last };
        find_key::<S, dyn Get, Found>(store, input, identity, &mut walk)
    }

    /// The step a read names by `key`, looked up for its value's hash.
    fn child(&mut self, key: &[u8]) -> Result<Looked, KernelError> {
        let (step, input) = step_of(key)?;
        let runner = self.cx.kernel.runner(step)?;
        (runner.look)(self, input, Need::Hash)
    }
}

/// One walk of one identity's read-lists, as [`find_key`] asks it.
struct Walk<'a, 'l, S: Step, W: ?Sized> {
    looker: &'a mut Looker<'l, W>,
    input: &'a S::Input,
    identity: &'a Identity,
    need: Need,
    pending: &'a mut Vec<Vec<u8>>,
    nots: &'a mut Vec<Vec<u8>>,
    last: bool,
}

impl<S: Step, W: World + ?Sized> Lists<Found> for Walk<'_, '_, S, W> {
    fn now(&mut self, read: Read<'_>) -> Result<Now, KernelError> {
        let Cx { kernel, world, .. } = self.looker.cx;
        if read.kind != ReadKind::STEP {
            return match world.hash(read) {
                Ok(seen) => Ok(Now::Seen(seen)),
                // The world cannot value it now: no entry whose run made this read is the
                // one valid now, so the list is passed over and nothing fails.
                Err(error) if error.not_now => Ok(Now::Not),
                Err(error) => Err(KernelError::World {
                    of: of::<S>(self.input, self.identity),
                    read: kernel.describe(world, read),
                    why: error.why,
                }),
            };
        }
        if self.nots.iter().any(|it| it.as_slice() == read.key) {
            return Ok(Now::Not);
        }
        let looked = match self.looker.child(read.key) {
            Ok(looked) => looked,
            // What cannot be valued in a lookup cannot be by a run after it either.
            Err(error) if error.unvaluable() => {
                // alloc: a child that cannot be valued, once a lookup
                self.nots.push(read.key.to_vec());
                return Ok(Now::Not);
            }
            Err(error) => return Err(error),
        };
        match looked {
            Looked::Found(found, _) => {
                let hash = found.hash.expect("a child is looked up for its value's hash");
                Ok(Now::Seen(step_seen(read.key, hash)))
            }
            Looked::Later if !self.last => {
                // alloc: a child to run, once a lookup
                self.pending.push(read.key.to_vec());
                Ok(Now::Pending)
            }
            Looked::Missing if self.looker.keeps && !self.last => {
                // alloc: a child to run, once a lookup
                self.pending.push(read.key.to_vec());
                Ok(Now::Pending)
            }
            Looked::Missing | Looked::Later => {
                // alloc: a child that cannot be valued, once a lookup
                self.nots.push(read.key.to_vec());
                Ok(Now::Not)
            }
        }
    }

    fn answer(&mut self, key: Key) -> Result<Option<Found>, KernelError> {
        answer::<S>(self.looker.store, self.input, self.identity, key, self.need)
    }

    fn held<'n>(
        &mut self,
        slot: Option<u32>,
        names: &mut dyn Iterator<Item = &'n [u8]>,
        found: &mut Found,
    ) {
        let id = self.identity.id();
        found.posted = Some(self.looker.state.session.post(id, slot, names, By::Hit));
    }

    fn listed(&self) -> bool {
        self.looker.state.session.listed(&self.identity.id())
    }
}

/// The row of what a lookup found.
fn known_of(found: &Found) -> Known {
    Known {
        key: found.key,
        hash: found.hash,
        // clone: a planted bug's reads, in a test
        #[cfg(test)]
        reads: found.reads.clone(),
    }
}

/// A value the session knows already, as much of it as `need` says: its key is the row's, and
/// so is its hash once a parent has asked; its bytes, or a hash the row has not yet, are one
/// read of the entry under that key.
fn known_now<S: Step>(
    store: &dyn Get,
    input: &S::Input,
    identity: &Identity,
    known: Known,
    need: Need,
) -> Result<Found, KernelError> {
    if need == Need::Key || (need == Need::Hash && known.hash.is_some()) {
        return Ok(Found {
            key: known.key,
            hash: known.hash,
            value: None,
            posted: None,
            #[cfg(test)]
            reads: known.reads,
        });
    }
    match answer::<S>(store, input, identity, known.key, need)? {
        Some(found) => Ok(found),
        None => Err(KernelError::Store {
            of: of::<S>(input, identity),
            key: known.key,
            // alloc: an error
            why: "its entry went missing after this session found it: a session is of one store".to_owned(),
        }),
    }
}

/// One request's lookups and runs: the kernel, the world and the store they read, the values
/// on the way and the values found.
pub(crate) struct Engine<'e, W: ?Sized> {
    cx: Cx<'e, W>,
    door: Door<'e>,
    /// The values being looked up or computed, outermost first.
    stack: Vec<Frame>,
    /// The request's session: the values found valid or made, and who read what.
    session: &'e mut Session,
}

impl<'e, W: World + ?Sized> Engine<'e, W> {
    pub(crate) fn new(
        kernel: &'e Kernel<W>,
        world: &'e W,
        door: Door<'e>,
        verify: &'e Setting,
        session: &'e mut Session,
    ) -> Engine<'e, W> {
        // alloc: one request's stack, as deep as its steps nest; empty until one is entered
        Engine { cx: Cx { kernel, world, verify }, door, stack: Vec::new(), session }
    }

    fn state(&mut self) -> State<'_> {
        State { stack: &mut self.stack, session: &mut *self.session }
    }

    fn looker(&mut self) -> Looker<'_, W> {
        Looker {
            cx: self.cx,
            store: self.door.get(),
            keeps: self.door.keeps(),
            state: State { stack: &mut self.stack, session: &mut *self.session },
        }
    }

    /// The value of `S` of `input` now: the store's entry whose reads all hash the same, else
    /// computed and kept. None only through a door that keeps nothing, where no entry is valid.
    /// `need` says how much of a hit the caller uses.
    pub(crate) fn value<S: Compute<World = W>>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
        need: Need,
    ) -> Result<Option<Valued>, KernelError> {
        let id = identity.id();
        if let Some((row, known)) = self.session.known(self.door.get(), &id)? {
            let fill = known.hash.is_none();
            let found = known_now::<S>(self.door.get(), input, identity, known, need)?;
            if fill && found.hash.is_some() {
                self.session.fill(row, known_of(&found));
            }
            return Ok(Some(Valued {
                key: found.key,
                hash: found.hash,
                how: How::Hit { value: found.value },
                row,
                #[cfg(test)]
                reads: found.reads,
            }));
        }
        self.state().enter::<S>(input, identity)?;
        let valued = self.entered::<S>(input, identity, need);
        self.stack.pop();
        let Some((mut valued, posted)) = valued? else {
            return Ok(None);
        };
        let known = Known {
            key: valued.key,
            hash: valued.hash,
            // clone: a planted bug's reads, in a test
            #[cfg(test)]
            reads: valued.reads.clone(),
        };
        valued.row = self.session.admit(posted, known);
        Ok(Some(valued))
    }

    /// The key of the entry of `identity` whose reads all hash the same now.
    pub(crate) fn found<S: Step>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
    ) -> Result<Option<Key>, KernelError> {
        let id = identity.id();
        if let Some((_, known)) = self.session.known(self.door.get(), &id)? {
            return Ok(Some(known.key));
        }
        self.state().enter::<S>(input, identity)?;
        let found = self.lookup::<S>(input, identity, Need::Key);
        self.stack.pop();
        let Some(mut found) = found? else {
            return Ok(None);
        };
        self.state().admit(&mut found);
        Ok(Some(found.key))
    }

    /// What `read` is now, for whoever explains a miss: a step's value that cannot be had now
    /// is nothing.
    pub(crate) fn now(&mut self, read: Read<'_>) -> Result<Seen, String> {
        if read.kind != ReadKind::STEP {
            return self.cx.world.hash(read).map_err(|error| error.why);
        }
        match self.seen_of(read.key) {
            Ok(Some(hash)) => Ok(step_seen(read.key, hash)),
            Ok(None) => Ok(Seen::Gone),
            // alloc: an error
            Err(error) => Err(error.to_string()),
        }
    }

    /// The value of `S` of `input`, found in the store or computed and kept, with the proof
    /// that its postings are written: what [`Engine::value`] admits it by. Its row is set
    /// there.
    fn entered<S: Compute<World = W>>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
        need: Need,
    ) -> Result<Option<(Valued, Posted)>, KernelError> {
        if let Some(mut found) = self.lookup::<S>(input, identity, need)? {
            self.check::<S>(input, identity, found.key)?;
            let posted = found.posted.take().expect("a walk posts the list of the entry it found");
            let valued = Valued {
                key: found.key,
                hash: found.hash,
                how: How::Hit { value: found.value },
                row: u32::MAX,
                #[cfg(test)]
                reads: found.reads,
            };
            return Ok(Some((valued, posted)));
        }
        if !self.door.keeps() {
            return Ok(None);
        }
        let (value, trace) = self.run::<S>(input, identity)?;
        let Cx { kernel, world, .. } = self.cx;
        let reads = || -> Vec<String> {
            // alloc: an error
            trace.iter().map(|(read, seen)| said(kernel.describe(world, read), seen)).collect()
        };
        let Door::Keep(store) = &mut self.door else {
            return Ok(None);
        };
        let kept = keep::<S, dyn Put>(&mut **store, input, identity, &trace, &value, reads)?;
        // The postings of the trace it was kept with: every step among its reads was asked
        // for by the run, so each is in the session, linked to its name.
        let posted = self.session.post(identity.id(), kept.slot, trace.names(), By::Keep);
        // The entry's own hash where it has one (an entry with reads); a bare value's is made
        // only for a parent that asked.
        let hash = match need {
            Need::Key => kept.hash,
            Need::Hash | Need::Value => Some(kept.hash.unwrap_or_else(|| content_of(&value))),
        };
        let valued = Valued {
            key: kept.key,
            hash,
            row: u32::MAX,
            #[cfg(test)]
            reads: planted::reads(trace.iter()),
            how: How::Computed { value, reads: trace },
        };
        Ok(Some((valued, posted)))
    }

    /// The entry of `identity` whose reads all hash the same now, by one walk of its read-lists
    /// (the module's doc: "One walk"). Only where the walk named children that must be run
    /// (none has an entry valid now, or the request's setting checks its hit) are those run,
    /// with no walk holding the store, and the lists walked a second and last time: bounded at
    /// two, and a child still not known then makes its list not hold.
    fn lookup<S: Step>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
        need: Need,
    ) -> Result<Option<Found>, KernelError> {
        // alloc: one lookup's children to run and children that cannot be valued; both empty
        // where every child its lists name has an entry valid now
        let (mut pending, mut nots): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (Vec::new(), Vec::new());
        let found =
            self.looker().walk::<S>(input, identity, need, &mut pending, &mut nots, false)?;
        if found.is_some() || pending.is_empty() {
            return Ok(found);
        }
        pending.sort();
        pending.dedup();
        for key in pending.drain(..) {
            if self.seen_of(&key)?.is_none() {
                // alloc: a child that cannot be valued, once a lookup
                nots.push(key);
            }
        }
        self.looker().walk::<S>(input, identity, need, &mut pending, &mut nots, true)
    }

    /// The hash of the value now of the step a read names by `key`, found or made; none when
    /// it cannot be valued now (the module's doc says which failures those are).
    fn seen_of(&mut self, key: &[u8]) -> Result<Option<ContentId>, KernelError> {
        match self.ask(key, Need::Hash) {
            Ok(Some(asked)) => Ok(Some(asked.hash)),
            Ok(None) => Ok(None),
            Err(error) if error.unvaluable() => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn ask(&mut self, key: &[u8], need: Need) -> Result<Option<Asked>, KernelError> {
        let (step, input) = step_of(key)?;
        let runner = self.cx.kernel.runner(step)?;
        (runner.value)(self, input, need)
    }

    /// `S` of `input` with fresh reads: its value and its reads, sorted, each once.
    fn run<S: Compute<World = W>>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
    ) -> Result<(Vec<u8>, Trace), KernelError> {
        let Cx { kernel, world, .. } = self.cx;
        let mut ctx = Reads::asking(world, self);
        let computed = S::compute(&mut ctx, input);
        let trace = match ctx.finish() {
            Ok(trace) => trace,
            Err(Unfinished::Failed(error)) => {
                return Err(KernelError::World {
                    of: of::<S>(input, identity),
                    read: kernel.describe(world, error.read()),
                    why: error.why,
                });
            }
            Err(Unfinished::Asked(child)) => {
                return Err(KernelError::Child { of: of::<S>(input, identity), child });
            }
            Err(Unfinished::Unsteady { name, first, then }) => {
                return Err(KernelError::Unsteady {
                    of: of::<S>(input, identity),
                    read: kernel.describe(
                        world,
                        entry::named(&name).expect("a name written by `Reads` starts with its kind"),
                    ),
                    first,
                    then,
                });
            }
        };
        match computed {
            Ok(value) => Ok((value, trace)),
            Err(StepError::World(error)) => Err(KernelError::World {
                of: of::<S>(input, identity),
                read: kernel.describe(world, error.read()),
                why: error.why,
            }),
            Err(StepError::Refused(why)) => {
                Err(KernelError::Compute { of: of::<S>(input, identity), why })
            }
            Err(StepError::Asked(child)) => {
                Err(KernelError::Child { of: of::<S>(input, identity), child })
            }
        }
    }

    /// Checks a hit when the request's setting samples it: `S` is run again with fresh reads,
    /// and it must make the entry's reads and bytes; and the hash the entry keeps of its value
    /// must be the hash of the value under it. Only where the request keeps: the run asks for
    /// its children as any run does.
    fn check<S: Compute<World = W>>(
        &mut self,
        input: &S::Input,
        identity: &Identity,
        key: Key,
    ) -> Result<(), KernelError> {
        let id = identity.id();
        let Cx { kernel, world, verify } = self.cx;
        if !verify.sampled(&id) || !self.door.keeps() {
            return Ok(());
        }
        let failed = |why: String| KernelError::Store { of: of::<S>(input, identity), key, why };
        // alloc: a sampled check holds its entry apart from the store its run keeps in
        let bytes = self
            .door
            .get()
            .get(&key)
            .map_err(failed)?
            // alloc: an error
            .ok_or_else(|| failed("its entry went missing after it was probed".to_owned()))?
            .to_vec();
        let entry = Entry::read(&bytes, key, identity).map_err(failed)?;
        if let Some(kept) = entry.kept_hash() {
            let now = content_of(entry.value);
            if kept != now {
                // alloc: an error
                return Err(failed(format!(
                    "its entry keeps {kept:x} as the hash of its value, whose bytes hash to {now:x}"
                )));
            }
        }
        let label = S::label(input);
        // alloc: a sampled check's text
        let names: Vec<String> = entry
            .reads()
            .map(|(read, seen)| said(kernel.describe(world, read), seen))
            .collect();
        // alloc: a sampled check's reads
        let reads: Vec<RecordedRead<'_>> = names
            .iter()
            .zip(entry.reads())
            .map(|(what, (_, seen))| RecordedRead { what, hash: seen.hash() })
            .collect();
        let hit = Hit { function: &label, identity: id, reads: &reads, stored: entry.value };
        let again = Again::<S> { engine: RefCell::new(self), input, entry: &entry };
        blaze_store_check::check(verify, &hit, &again).map_err(KernelError::Verify)?;
        Ok(())
    }
}

impl<W: World + ?Sized> Asks for Engine<'_, W> {
    fn asked(&mut self, key: &[u8]) -> Result<Asked, KernelError> {
        match self.ask(key, Need::Value)? {
            Some(asked) => Ok(asked),
            None => unreachable!("a step runs only where its request keeps, and there a value asked for is found or made"),
        }
    }
}

/// [`Kernel::carry`]: the seed, then the repair, lowest first.
pub(crate) fn carry<W: World + ?Sized>(
    kernel: &Kernel<W>,
    session: &mut Session,
    store: &mut dyn Put,
    world: &W,
    changed: &Changed,
    verify: &Setting,
) -> Result<Carried, KernelError> {
    let mut carried = Carried::default();
    let Some(mut queue) = session.seed(&*store, kernel.registry(), world.pin(), changed)? else {
        carried.dropped = Some(Dropped::Registry);
        return Ok(carried);
    };
    carried.seeded = queue.len();
    // Bounded (rule 18): each turn takes a row out of the queue, and the queue stops giving
    // rows past its count.
    loop {
        let (row, was) = match queue.pop() {
            Popped::Row { row, was } => (row, was),
            Popped::Empty => break,
            Popped::Spent => {
                session.clear();
                carried.dropped = Some(Dropped::Repairs);
                return Ok(carried);
            }
        };
        // Only a value a step read is looked up here: it has the hash its readers saw, and
        // the key they name it by. Any other has no reader to tell: dropped, it is looked up
        // when it is next asked for.
        // alloc: one repair's key, apart from the session its lookup writes in
        let key = session.asked(row).map(<[u8]>::to_vec);
        // A cover is never looked up by a carry (to run one is to ask for every member): it
        // is dropped, and so is every member admitted through it, each then looked up
        // through its own lists as any dropped row is (invariant I5).
        if key.as_deref().is_some_and(|key| kernel.is_cover(key)) {
            if !session.is_live(row) {
                carried.gone += 1;
            }
            if let Err(error) = dirty_readers(session, &*store, row, &mut queue) {
                session.clear();
                return Err(error);
            }
            continue;
        }
        let (Some(key), Some(was)) = (key, was) else {
            if !session.is_live(row) {
                carried.gone += 1;
            }
            continue;
        };
        // A lookup since it was queued may have asked for it, and so repaired it.
        if !session.is_live(row) {
            carried.repaired += 1;
            let asked = {
                let mut engine =
                    Engine::new(kernel, world, Door::Keep(&mut *store), verify, &mut *session);
                engine.ask(&key, Need::Hash)
            };
            match asked {
                Ok(_) => {}
                // It cannot be valued at this pin: it is not in the session, and whoever asks
                // for it next is told why.
                Err(error) if error.unvaluable() => {}
                Err(error) => {
                    session.clear();
                    return Err(error);
                }
            }
        }
        match session.hash_of(row) {
            // The same bytes: its readers stay verified, with no work (the cutoff).
            Some(now) if now == was => {
                carried.cut += 1;
                continue;
            }
            Some(_) => carried.changed += 1,
            None => carried.gone += 1,
        }
        // Its readers, read from the store where they are not in memory. A store that fails
        // here leaves rows dropped whose readers were not reached: the session is dropped.
        if let Err(error) = dirty_readers(session, &*store, row, &mut queue) {
            session.clear();
            return Err(error);
        }
    }
    session.leave(world)?;
    Ok(carried)
}

/// Drops and queues every reader of `row` the queue still takes, its readers read from the
/// store where they are not in memory.
fn dirty_readers(
    session: &mut Session,
    store: &dyn Put,
    row: u32,
    queue: &mut crate::session::Queue,
) -> Result<(), KernelError> {
    session.load_readers(store, row)?;
    let mut n = 0;
    // Bounded (rule 18): one turn a reader of one posting.
    while let Some(reader) = session.reader(row, n) {
        n += 1;
        if queue.takes(reader) {
            session.dirty(store, reader, queue)?;
        }
    }
    Ok(())
}

/// A hit's value computed again, for lane verify's check.
struct Again<'a, 'e, S: Compute> {
    engine: RefCell<&'a mut Engine<'e, S::World>>,
    input: &'a S::Input,
    entry: &'a Entry<'a>,
}

impl<S: Compute> Recompute for Again<'_, '_, S> {
    fn recompute(&self, _: &Hit<'_>) -> Result<Vec<u8>, String> {
        // alloc: an error
        let mut engine = self
            .engine
            .try_borrow_mut()
            .map_err(|_| "a hit is computed again while it is being computed again".to_owned())?;
        let (kernel, world) = (engine.cx.kernel, engine.cx.world);
        let (value, trace) =
            // alloc: an error
            engine.run::<S>(self.input, &self.entry.identity).map_err(|it| it.to_string())?;
        let named = |read: Read<'_>, seen: Seen| said(kernel.describe(world, read), seen);
        let mut stored = self.entry.reads();
        for (n, (read, seen)) in trace.iter().enumerate() {
            match stored.next() {
                Some(was) if was == (read, seen) => {}
                Some((was, was_seen)) => {
                    // alloc: an error
                    return Err(format!(
                        "computed again, read {n} is {}, where the entry recorded {}",
                        named(read, seen),
                        named(was, was_seen)
                    ));
                }
                None => {
                    // alloc: an error
                    return Err(format!(
                        "computed again, it made {} reads, where the entry recorded {}",
                        trace.len(),
                        self.entry.reads().len()
                    ));
                }
            }
        }
        if stored.next().is_some() {
            // alloc: an error
            return Err(format!(
                "computed again, it made {} reads, where the entry recorded {}",
                trace.len(),
                self.entry.reads().len()
            ));
        }
        Ok(value)
    }
}

/// Bugs a test plants in the kernel's own rules, one at a time on its thread: the controls of
/// the exhaustive check (`exhaustive/nested.rs`), which must find a stale value for each.
#[cfg(test)]
pub(crate) mod planted {
    use std::cell::Cell;

    use crate::{Read, Seen};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Bug {
        None,
        /// The parent records its child's reads and their hashes in place of the one read of
        /// the child's value, and leaves the last of them out.
        ChildReads,
        /// A read of a step's value sees the child's identity only, never its value's hash.
        IdentityOnly,
        /// A session answers whatever its world's pin is now: its rows outlive their pin.
        StalePin,
        /// An entry found in the store is admitted with no posting: postings are written only
        /// for what this process computed and kept (invariant I1).
        HitUnposted,
        /// An admission's postings are written at the end of the next carry's seed, not with
        /// the admission (invariant I1: "never at the next boundary").
        PostLate,
        /// Whoever says what changed leaves out a fact that appears where nothing was
        /// (invariant I2); the kernel is as it is, and the driver of the check has the bug.
        AbsenceUnsaid,
        /// A carry repairs the highest queued entry first, and one repaired is not queued
        /// again: a parent is looked up before the child it reads (invariant I3).
        ParentFirst,
        /// A member admitted through a cover is not posted as the cover's reader (invariant
        /// I5): a covering read that changes drops the cover and leaves the member verified.
        CoverUnposted,
        /// A session opened from the store reads no posting back: a name's readers are only
        /// those posted since it was opened (rule 26: a restore never silently starts empty).
        OpenUnposted,
    }

    thread_local! {
        static BUG: Cell<Bug> = const { Cell::new(Bug::None) };
    }

    pub(crate) fn plant(bug: Bug) {
        BUG.with(|it| it.set(bug));
    }

    pub(crate) fn is(bug: Bug) -> bool {
        BUG.with(Cell::get) == bug
    }

    /// A value's reads by name, kept only while the bug that reads them is planted.
    pub(crate) fn reads<'a>(reads: impl Iterator<Item = (Read<'a>, Seen)>) -> Vec<(Vec<u8>, Seen)> {
        if !is(Bug::ChildReads) {
            return Vec::new();
        }
        reads
            .map(|(read, seen)| {
                let mut name = read.kind.0.to_be_bytes().to_vec();
                name.extend_from_slice(read.key);
                (name, seen)
            })
            .collect()
    }
}
