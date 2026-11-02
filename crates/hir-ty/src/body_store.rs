//! The item store (docs/v2/plan/design/item-hash.md): a body's inference, MIR and borrow check
//! verdict filed in blaze's store under its identity and the facts the step read, so a later
//! build, branch or machine reads the result instead of running the step.
//!
//! - A body's step is filed under its **base key** (its identity from [`BodyFacts::identity`],
//!   the step, the schema of the step's answers, the records' schema and the solver version).
//!   Under it, in one of [`record::SLOTS`] slots, a **shape** lists the facts the step read; the
//!   **answer** is filed under the shape's key and those facts' values when it ran
//!   ([`record`]). A lookup reads the shapes, hashes their facts as they are now, and asks for
//!   the answer: the goal store's constructive trace (`next_solver::goal_store`), one level up.
//! - A step is stored only when every read it made is covered by the identity or stands for a
//!   fact ([`BodyFacts::classify`]); any other read keeps it out, counted by name in
//!   [`Flushed::refused`]. Only passing borrow checks are stored.
//! - Offers are buffered per thread and written by [`Installed::flush`], which the driver calls
//!   where it flushes the goal store, so no worker waits on the store ([`pending`]).
//! - Under [`Mode::Verify`] a hit runs the step as well, and a result that differs from the
//!   stored one is the build's error, naming the body and the step.
//!
//! With no store installed for a database, every entry point is one load and a branch. A
//! record that does not open under its key, a store that cannot be read, or two answers under
//! one key of fact values is an error naming the body, reported by the next flush; the step
//! is run then, which gives the same result. A miss is a miss.

mod facts;
mod infer;
mod mir;
mod pending;
mod record;
#[cfg(test)]
mod tests;
mod value;

use std::{
    cell::Cell,
    fmt,
    ops::ControlFlow,
    ptr::{self, NonNull},
    sync::{
        Arc, LazyLock, OnceLock,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
};

use base_db::{Db, Nonce};
use blaze_format::{Part, SchemaHash, schema_of};
use blaze_ids::ContentId;
use blaze_reclaim::Limbo;
use hir_def::{DefWithBodyId, deps::ImageStore};
use parking_lot::Mutex;

pub use facts::{BodyFacts, Classified, Step};
pub use value::step_value;
pub(crate) use infer::inference;
#[cfg(test)]
use infer::lookup_inference;
pub(crate) use mir::built as built_mir;
pub use mir::location_literals;

use crate::next_solver::goal_store::{FactRef, solver_version};
use pending::{Counts, Offered, Pending, Tallies};
use record::{AnswerKey, SLOTS, ShapeRef};

/// What a store does on a hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A hit is the step's result.
    Serve,
    /// A hit runs the step as well; a result that differs from the stored one is an error.
    Verify,
}

/// The schema of a borrow check's answer by the checker named `checker`: it has no bytes, its
/// being there is the verdict. Checkers accept different programs, so each has its own key.
fn passed_schema(checker: &'static str) -> SchemaHash {
    schema_of(&[Part::Str("blaze v2 item store borrow check passed 1"), Part::Str(checker)])
}

/// What a step's name is in an error.
fn what(step: Step) -> &'static str {
    match step {
        Step::Infer => "inference",
        Step::Mir => "MIR",
        Step::Borrowck => "borrow check",
    }
}

/// What [`lookup`] found of a body's step.
pub(crate) enum Lookup<T> {
    /// No store for this database, or the body has no identity: run the step, offer nothing.
    Off,
    /// Nothing stored: run the step, then [`offer`] it with what it read.
    Miss,
    /// The stored result, decoded. With `verify`, run the step as well and [`fail`] when the
    /// two differ.
    Hit { value: T, verify: bool },
}

/// The item store of one database.
struct Store {
    nonce: Nonce,
    version: ContentId,
    mode: Mode,
    // dyn: the driver's store over `blaze-cas`, probed once per step of a body, written at a flush
    images: Arc<dyn ImageStore + Send + Sync>,
    // dyn: lane item-hash's identities and facts, above hir-ty; memoized queries on a hit
    facts: Arc<dyn BodyFacts>,
    pending: Pending,
    /// The first error a lookup met (a store that could not be read, a record that does not
    /// open under its key, a stored result that differs from running the step under
    /// [`Mode::Verify`]): [`Installed::flush`] stops with it.
    error: Mutex<Option<String>>,
}

/// The buffers an offer fills, kept for the thread's next one. Taken out for the offer and put
/// back after, so an offer nested in one (a fact's value that infers another body) finds none
/// and allocates.
#[derive(Default)]
struct Scratch {
    facts: Vec<FactRef>,
    values: Vec<ContentId>,
    shape: Vec<u8>,
    answer: Vec<u8>,
}

thread_local! {
    static SCRATCH: Cell<Scratch> = Cell::new(Scratch::default());
}

/// Why an offered step is not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// A read of this query or input is neither covered by the identity nor a fact.
    Read(&'static str),
    /// A fact it read cannot be hashed now.
    Unhashable,
    /// The body has no identity.
    NoIdentity,
    /// What it read could not be captured shareably.
    Unshareable,
    /// Its result has no stored form.
    Unencodable,
}

impl Store {
    fn lookup<T>(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        // dyn: the store's facts, given to the step's decoder once per hit
        decode: impl FnOnce(&dyn BodyFacts, &[u8]) -> Result<T, String>,
    ) -> Lookup<T> {
        let Some(identity) = self.facts.identity(db, def) else {
            self.pending.refused(step, Refusal::NoIdentity);
            self.pending.looked_up(step, false);
            return Lookup::Off;
        };
        let base = record::base_key(&identity, step, &schema, &self.version);
        let found = match self.find(db, def, step, &base) {
            None => Lookup::Miss,
            Some((found, answer_key)) => {
                match record::open_answer(found.bytes(), &answer_key).and_then(|body| decode(&*self.facts, body)) {
                    Ok(value) => Lookup::Hit { value, verify: self.mode == Mode::Verify },
                    Err(why) => {
                        self.fail(format_args!("the stored {} of {def:?} (answer {answer_key:x}): {why}", what(step)));
                        Lookup::Miss
                    }
                }
            }
        };
        self.pending.looked_up(step, matches!(found, Lookup::Hit { .. }));
        found
    }

    /// The answer record of the first slot of `base` whose facts hash the same now, and its
    /// answer key.
    fn find(&self, db: &Db, def: DefWithBodyId, step: Step, base: &ContentId) -> Option<(hir_def::deps::ImageBuf, ContentId)> {
        for n in 0..SLOTS {
            let shape_key = record::shape_key(base, n);
            let shape = match self.images.get(&record::shape_at(step, &shape_key)) {
                Ok(Some(shape)) => shape,
                Ok(None) => return None,
                Err(why) => {
                    self.fail(format_args!("reading the shape of the {} of {def:?} in slot {n}: {why}", what(step)));
                    return None;
                }
            };
            let shape = match ShapeRef::open(shape.bytes(), base) {
                Ok(shape) => shape,
                Err(why) => {
                    self.fail(format_args!("the shape of the {} of {def:?} in slot {n}: {why}", what(step)));
                    return None;
                }
            };
            let Some(answer_key) = self.answer_key_now(db, &shape_key, shape) else { continue };
            match self.images.get(&record::answer_at(step, &answer_key)) {
                Ok(Some(found)) => return Some((found, answer_key)),
                Ok(None) => {}
                Err(why) => {
                    self.fail(format_args!("reading the answer of the {} of {def:?} in slot {n}: {why}", what(step)));
                    return None;
                }
            }
        }
        None
    }

    /// The key the answer of the shape at `shape_key` has under its facts' values now; `None`
    /// when one of them cannot be hashed now.
    fn answer_key_now(&self, db: &Db, shape_key: &ContentId, shape: ShapeRef<'_>) -> Option<ContentId> {
        let mut key = AnswerKey::new(shape_key);
        for fact in shape.facts() {
            key.value(&self.facts.value(db, fact)?);
        }
        Some(key.finish())
    }

    fn offer(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        reads: Option<blaze_query::db::Reads>,
        // dyn: the store's facts, given to the step's encoder once per offer
        write: impl FnOnce(&dyn BodyFacts, &mut Vec<u8>) -> Result<(), String>,
    ) {
        let mut scratch = SCRATCH.take();
        scratch.facts.clear();
        scratch.values.clear();
        scratch.answer.clear();
        if let Err(refusal) = self.offered(db, def, step, schema, reads, write, &mut scratch) {
            self.pending.refused(step, refusal);
        }
        SCRATCH.set(scratch);
    }

    fn offered(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        reads: Option<blaze_query::db::Reads>,
        // dyn: as in `offer`
        write: impl FnOnce(&dyn BodyFacts, &mut Vec<u8>) -> Result<(), String>,
        scratch: &mut Scratch,
    ) -> Result<(), Refusal> {
        let identity = self.facts.identity(db, def).ok_or(Refusal::NoIdentity)?;
        let reads = reads.ok_or(Refusal::Unshareable)?;
        let mut refused = None;
        let facts = &mut scratch.facts;
        let storage = blaze_query::db::Database::storage(db);
        let _ = reads.for_each_read(storage, |blaze_query::db::Read { key: read, kind }| {
            if kind == blaze_query::db::ReadKind::Interned {
                return ControlFlow::Continue(());
            }
            let name = storage.read_name(read, kind);
            match self.facts.classify(db, def, step, read, kind, name) {
                Classified::Covered => ControlFlow::Continue(()),
                Classified::Fact(fact) => {
                    facts.push(fact);
                    ControlFlow::Continue(())
                }
                Classified::Refused => {
                    refused = Some(name);
                    ControlFlow::Break(())
                }
            }
        });
        if let Some(name) = refused {
            return Err(Refusal::Read(name));
        }
        scratch.facts.sort_unstable();
        scratch.facts.dedup();
        for &fact in &scratch.facts {
            scratch.values.push(self.facts.value(db, fact).ok_or(Refusal::Unhashable)?);
        }
        if let Err(why) = write(&*self.facts, &mut scratch.answer) {
            tracing::debug!(?def, step = what(step), %why, "a step's result with no stored form");
            return Err(Refusal::Unencodable);
        }
        let base = record::base_key(&identity, step, &schema, &self.version);
        record::write_shape(&mut scratch.shape, &base, &scratch.facts);
        self.pending.push(Offered {
            step,
            def,
            base: &base,
            shape: &scratch.shape,
            values: &scratch.values,
            answer: &scratch.answer,
        });
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn fail(&self, why: fmt::Arguments<'_>) {
        let mut error = self.error.lock();
        if error.is_none() {
            // alloc: error text, once per store until the flush that reports it
            *error = Some(format!("the item store: {why}"));
        }
    }

    /// Writes one buffered step: its shape in the first slot that holds it or is free, then its
    /// answer under the shape's key and the values its facts had.
    fn write(&self, entry: &pending::Entry<'_>) -> Result<Wrote, String> {
        let (step, def) = (entry.step, entry.def);
        // alloc: error text
        let error = |doing: &str, why: String| format!("the item store: {doing} the {} of {def:?}: {why}", what(step));
        let mut wrote = Wrote::Kept;
        let mut slot = None;
        for n in 0..SLOTS {
            let shape_key = record::shape_key(&entry.base, n);
            let at = record::shape_at(step, &shape_key);
            match self.images.get(&at).map_err(|why| error("reading the shape of", why))? {
                Some(kept) if kept.bytes() == entry.shape => {
                    slot = Some(shape_key);
                    break;
                }
                Some(_) => continue,
                // alloc: the shape's bytes, once per step written
                None => match self.images.put(at, entry.shape.to_vec()) {
                    Ok(()) => {
                        wrote = Wrote::Written;
                        slot = Some(shape_key);
                        break;
                    }
                    Err(why) => match self.images.get(&at).map_err(|why| error("reading the shape of", why))? {
                        Some(kept) if kept.bytes() == entry.shape => {
                            slot = Some(shape_key);
                            break;
                        }
                        Some(_) => continue,
                        None => return Err(error("keeping the shape of", why)),
                    },
                },
            }
        }
        let Some(shape_key) = slot else { return Ok(Wrote::SlotsFull) };
        let mut key = AnswerKey::new(&shape_key);
        for value in entry.values() {
            key.value(&value);
        }
        let answer_key = key.finish();
        let at = record::answer_at(step, &answer_key);
        // alloc: the answer's bytes, once per step written
        let answer = record::write_answer(&answer_key, entry.answer);
        match self.images.get(&at).map_err(|why| error("reading the answer of", why))? {
            Some(kept) if kept.bytes() == answer.as_slice() => Ok(wrote),
            Some(_) => Err(error(
                "two answers under one key of fact values for",
                // alloc: error text
                String::from("the step read something its identity and facts do not cover"),
            )),
            None => {
                self.images.put(at, answer).map_err(|why| error("keeping the answer of", why))?;
                Ok(Wrote::Written)
            }
        }
    }
}

/// What writing one buffered step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wrote {
    /// Its shape or its answer was new.
    Written,
    /// The store had both already.
    Kept,
    /// Every slot of its base key holds another shape.
    SlotsFull,
}

/// What [`Installed::flush`] did, and why offered steps were not kept.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Steps whose shape or answer was written.
    pub written: usize,
    /// Steps the store had already.
    pub kept: usize,
    /// Steps not written as every slot of their base key holds another shape.
    pub slots_full: usize,
    /// For each query or input whose read kept steps out (neither covered nor a fact), how
    /// many, most first: what lane item-hash's classification is asked to cover.
    pub refused: Vec<(&'static str, u64)>,
    /// Steps kept out as a fact they read could not be hashed.
    pub unhashable: u64,
    /// Lookups of bodies with no identity, which are neither looked up nor stored.
    pub no_identity: u64,
    /// Steps kept out as what they read could not be captured shareably.
    pub unshareable: u64,
    /// Steps kept out as their result has no stored form.
    pub unencodable: u64,
    /// Steps dropped as the buffer of their thread was full.
    pub dropped: u64,
    /// What each step's lookups found and its offers met since the last flush, by [`Step`] as an
    /// index.
    pub steps: [StepCounts; 3],
}

/// What one step's lookups found and its offers met.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepCounts {
    /// Results read from the store; under [`Mode::Verify`] each also ran.
    pub hits: u64,
    /// Steps that ran as nothing was stored, or as the body has no identity.
    pub ran: u64,
    /// Steps that ran and were kept out of the store (any reason but no identity).
    pub refused: u64,
}

/// How many databases may have an item store at once.
const STORES: usize = 16;

/// How many item stores are installed: zero is the one load every entry point makes.
static INSTALLED: AtomicUsize = AtomicUsize::new(0);

/// The installed stores, each a `Box` of [`install`], freed through [`LIMBO`] once unlinked.
static REGISTRY: [AtomicPtr<Store>; STORES] = [const { AtomicPtr::new(ptr::null_mut()) }; STORES];

static LIMBO: LazyLock<Mutex<Limbo>> = LazyLock::new(|| Mutex::new(Limbo::new()));

/// Runs `f` on the item store of `db`, if one is installed.
#[inline]
fn with_store<R>(db: &Db, f: impl FnOnce(&Store) -> R) -> Option<R> {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let (nonce, _) = db.nonce_and_revision();
    let _task = blaze_reclaim::enter();
    for slot in &REGISTRY {
        let held = slot.load(Ordering::Acquire);
        // SAFETY: a store is freed only through `LIMBO`, after a grace started once `Installed`'s
        // drop unlinked it; `_task` was entered before `held` was loaded and outlives `f`, so that
        // grace has not passed while the store is used here.
        let Some(store) = (unsafe { held.as_ref() }) else { continue };
        if store.nonce == nonce {
            return Some(f(store));
        }
    }
    None
}

/// Before running `step` of `def`: its stored result, decoded by `decode` from the answer's
/// bytes, if a store is installed for `db` and every fact a shape of it lists hashes as it did.
/// The identity and the fact values are reads of the active query. `schema` is the schema of
/// the step's answers: another schema is another key.
#[inline]
pub(crate) fn lookup<T>(
    db: &Db,
    def: DefWithBodyId,
    step: Step,
    schema: SchemaHash,
    // dyn: as `Store::lookup`
    decode: impl FnOnce(&dyn BodyFacts, &[u8]) -> Result<T, String>,
) -> Lookup<T> {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return Lookup::Off;
    }
    with_store(db, |store| store.lookup(db, def, step, schema, decode)).unwrap_or(Lookup::Off)
}

/// After a [`Lookup::Miss`]: `step` of `def` ran, reading `reads` (from [`capture`]), and
/// `write` puts its result's bytes in the buffer it is given, or refuses with why. Nothing is
/// written until the flush.
#[inline]
pub(crate) fn offer(
    db: &Db,
    def: DefWithBodyId,
    step: Step,
    schema: SchemaHash,
    reads: Option<blaze_query::db::Reads>,
    // dyn: as `Store::offer`
    write: impl FnOnce(&dyn BodyFacts, &mut Vec<u8>) -> Result<(), String>,
) {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return;
    }
    with_store(db, |store| store.offer(db, def, step, schema, reads, write));
}

/// Records the build's error for `step` of `def`: `why` it could not be served, or how its
/// stored result differs from running it under [`Mode::Verify`]. The next flush stops with the
/// first one.
#[cold]
pub(crate) fn fail(db: &Db, def: DefWithBodyId, step: Step, why: fmt::Arguments<'_>) {
    with_store(db, |store| store.fail(format_args!("the {} of {def:?}: {why}", what(step))));
}

/// `run`, with what it read captured when an item store is installed for any database: what
/// [`offer`] and [`offer_passed`] are given. Only inside a query is anything captured.
#[inline]
pub fn capture<R>(db: &Db, run: impl FnOnce() -> R) -> (R, Option<blaze_query::db::Reads>) {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return (run(), None);
    }
    blaze_query::db::capture_reads(db, run)
}

/// Before `def`'s borrow check by the checker named `checker` (`hir_ty_borrowck::gate::checked`):
/// whether a passing verdict of it is stored, and so the check need not run. Under
/// [`Mode::Verify`] a stored verdict runs `check` (whether the borrow check passes) and answers
/// `false`, so the check runs as without a store; a stored pass whose check fails is the build's
/// error.
#[inline]
pub fn lookup_passed(db: &Db, def: DefWithBodyId, checker: &'static str, check: impl FnOnce() -> bool) -> bool {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return false;
    }
    lookup_passed_in(db, def, checker, check)
}

#[inline(never)]
fn lookup_passed_in(db: &Db, def: DefWithBodyId, checker: &'static str, check: impl FnOnce() -> bool) -> bool {
    let found = lookup(db, def, Step::Borrowck, passed_schema(checker), |_, bytes| match bytes.is_empty() {
        true => Ok(()),
        // alloc: the reason a record does not read
        false => Err(String::from("a borrow check's answer with bytes")),
    });
    match found {
        Lookup::Off | Lookup::Miss => false,
        Lookup::Hit { value: (), verify: false } => true,
        Lookup::Hit { value: (), verify: true } => {
            if !check() {
                fail(db, def, Step::Borrowck, format_args!("stored as passed, and it fails"));
            }
            false
        }
    }
}

/// After `def`'s borrow check by the checker named `checker` passed, reading `reads` (from
/// [`capture`], around the check): its verdict, for the flush to store. A failing verdict is
/// never offered.
#[inline]
pub fn offer_passed(db: &Db, def: DefWithBodyId, checker: &'static str, reads: Option<blaze_query::db::Reads>) {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return;
    }
    offer(db, def, Step::Borrowck, passed_schema(checker), reads, |_, _| Ok(()));
}

/// Gives `db` an item store: `images`, the driver's store (the local store, or the local store
/// over the global cache through `hir_def::deps::ReadThrough`), `facts`, lane item-hash's
/// identities and facts, `build`, the content ID of the front end's code (what the goal store is
/// given), and what a hit does. Until the guard is dropped, every lookup and offer of `db` goes
/// to it; what was offered and not flushed is dropped with it.
pub fn install(
    db: &Db,
    // dyn: the driver's store, kept by the item store (see `Store::images`)
    images: Arc<dyn ImageStore + Send + Sync>,
    // dyn: lane item-hash's facts, kept by the item store (see `Store::facts`)
    facts: Arc<dyn BodyFacts>,
    build: ContentId,
    mode: Mode,
) -> Result<Installed, String> {
    let (nonce, _) = db.nonce_and_revision();
    {
        let _task = blaze_reclaim::enter();
        for slot in &REGISTRY {
            // SAFETY: as in `with_store`, under `_task`.
            if unsafe { slot.load(Ordering::Acquire).as_ref() }.is_some_and(|it| it.nonce == nonce) {
                // alloc: error text
                return Err(String::from("this database has an item store installed already"));
            }
        }
    }
    // alloc: setup: once per installed store, freed through `LIMBO` when it is uninstalled
    let store = Box::into_raw(Box::new(Store {
        nonce,
        version: solver_version(&build),
        mode,
        images,
        facts,
        pending: Pending::new(),
        error: Mutex::new(None),
    }));
    for (at, slot) in REGISTRY.iter().enumerate() {
        if slot.compare_exchange(ptr::null_mut(), store, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            INSTALLED.fetch_add(1, Ordering::Relaxed);
            // SAFETY: `Box::into_raw` never gives null.
            return Ok(Installed { at, store: unsafe { NonNull::new_unchecked(store) } });
        }
    }
    // SAFETY: never published, so this is its only owner.
    drop(unsafe { Box::from_raw(store) });
    // alloc: error text
    Err(format!("more than {STORES} databases with an item store at once"))
}

/// An item store installed for a database ([`install`]); dropping it uninstalls it.
pub struct Installed {
    at: usize,
    store: NonNull<Store>,
}

// SAFETY: `Installed` owns its `Store`, which is `Send + Sync` (its fields are), and hands out
// only shared references to it.
unsafe impl Send for Installed {}
// SAFETY: as above.
unsafe impl Sync for Installed {}

impl Installed {
    fn store(&self) -> &Store {
        // SAFETY: the store is freed only by `drop`, which takes `&mut self`.
        unsafe { self.store.as_ref() }
    }

    /// What each step's lookups found and its offers met since the last flush, by [`Step`] as an
    /// index; the next flush's [`Flushed::steps`] has them too.
    pub fn steps(&self) -> [StepCounts; 3] {
        self.store().pending.steps()
    }

    /// The first error a lookup met since the last flush or call (a store that could not be
    /// read, a record that does not open under its key, a stored result that differs from
    /// running the step), taken: the driver calls it once a build has ended, so the build it
    /// happened in stops with it.
    pub fn errors(&self) -> Result<(), String> {
        match self.store().error.lock().take() {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    /// Writes every step offered since the last flush, on the store's threads
    /// (`ImageStore::for_each`). The driver calls it where it flushes the goal store: after a
    /// build's front-end work, with no query running. An error a lookup met since the last
    /// flush, a store that will not keep a record, or a second answer under one key of fact
    /// values (a read the identity and facts do not cover) is an error naming the body.
    pub fn flush(&self) -> Result<Flushed, String> {
        let store = self.store();
        let (taken, tallies) = store.pending.take();
        if let Some(why) = store.error.lock().take() {
            return Err(why);
        }
        // alloc: flush, once per build: one entry per step offered
        let entries: Vec<pending::Entry<'_>> = taken.iter().flat_map(pending::entries).collect();
        // alloc: flush, as above
        let results: Vec<OnceLock<Result<Wrote, String>>> = entries.iter().map(|_| OnceLock::new()).collect();
        store.images.for_each(entries.len(), &|at| {
            _ = results[at].set(store.write(&entries[at]));
        });
        let Tallies { refused, counts } = tallies;
        let Counts { unhashable, no_identity, unshareable, unencodable, dropped, steps } = counts;
        let mut flushed =
            Flushed { refused, unhashable, no_identity, unshareable, unencodable, dropped, steps, ..Flushed::default() };
        for result in results {
            let wrote = result.into_inner().unwrap_or_else(|| {
                panic!("`ImageStore::for_each` did not run every task of an item store flush")
            })?;
            match wrote {
                Wrote::Written => flushed.written += 1,
                Wrote::Kept => flushed.kept += 1,
                Wrote::SlotsFull => flushed.slots_full += 1,
            }
        }
        Ok(flushed)
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let held = REGISTRY[self.at].swap(ptr::null_mut(), Ordering::AcqRel);
        assert_eq!(held, self.store.as_ptr(), "an item store's slot held another store");
        INSTALLED.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: made by `Box::into_raw` in `install`, and unlinked by the swap above, which only
        // this guard makes; readers that loaded it are covered by the limbo's grace.
        let store = unsafe { Box::from_raw(held) };
        let mut limbo = LIMBO.lock();
        limbo.retire(store);
        // alloc: `Limbo::collect` frees; it is not an iterator's.
        limbo.collect();
    }
}
