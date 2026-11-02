//! The persistent goal cache (docs/v2/plan/design/compile-time.md change 17;
//! docs/v2/plan/lanes/frontend/fast-trait.md section 3; notes in
//! docs/v2/plan/lanes/frontend/fast-trait-store.md): a shareable global goal's answer, kept by
//! content in the driver's store and reused by every later build while every fact its
//! evaluation read hashes the same.
//!
//! - A goal's **stable form** ([`stable`]) is its predicate and param env in `persist`'s codec
//!   with each definition by its item key, then the typing mode, the regions flag and the
//!   crate's recursion limit. Its key hashes that form with the solver version and the crate's
//!   key (its identity and lang items).
//! - Under the goal's key, in one of [`record::SLOTS`] slots, a **shape** record lists the facts
//!   the evaluation read; the **answer** is filed under the shape's key and those facts' values
//!   when it was solved ([`record`]). A lookup reads the shapes, hashes their facts as they are
//!   now, and asks for the answer: a constructive trace, as Bazel's action cache and Shake keep.
//! - Only what the global-goal table offers is offered here: root, shareable, global goals,
//!   never a cycle's provisional answer nor one with inference variables. A goal is stored only
//!   when every read of its evaluation is an interned value or a fact; any other read (a query
//!   or input with no fact) keeps it out, counted by name in [`Flushed::inexpressible`].
//! - Offers are buffered per thread and written by [`Installed::flush`], which the driver calls
//!   where it writes the dependency images, so no worker waits on the store ([`pending`]).
//!
//! With no store installed for a database, [`lookup`] and [`offer`] are one load and a branch:
//! that database has no persistent tier, as one given no store by `hir_def::deps::serve` has no
//! images. Nothing is answered another way: the goal is solved, as it always is.

mod pending;
mod record;
mod stable;
#[cfg(test)]
mod tests;

use std::{
    cell::Cell,
    ops::ControlFlow,
    ptr::{self, NonNull},
    sync::{
        Arc, LazyLock, OnceLock,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
};

use base_db::{Crate, Db, Nonce};
use blaze_ids::{ContentId, Hasher};
use blaze_query::QueryKind;
use blaze_reclaim::Limbo;
use hir_def::deps::ImageStore;
use parking_lot::Mutex;

use super::{
    DbInterner, SolverDefId,
    global_goals::{Answer, GoalKey},
};
use pending::{Pending, Tallies};
use record::{AnswerKey, SLOTS, ShapeRef, shape_key};
use stable::Goal;

/// What a goal's stable form and its facts come from, above hir-ty: lane items' item keys and
/// facts (`blaze_items`), which hir-ty cannot name. Asked only on a global-table miss and after
/// a solve, never on a hit; every method may be called from any worker at once.
pub trait GoalFacts: Send + Sync + 'static {
    /// The key of a definition a goal names (`blaze_items::key::def_key`'s): the same for the
    /// same item in every database and checkout, another one after a rename or a move.
    fn def_key<'db>(&self, db: &'db Db, def: SolverDefId<'db>) -> ContentId;

    /// The key of `krate` and of everything an interner of it reads when it is made, outside
    /// any query a goal's evaluation runs: its identity (`blaze_items::key::crate_key`) and its
    /// lang items, each by item key; for `None` (an interner of no crate, which has no lang
    /// items), one fixed key. A memoized query, as it is asked on every miss.
    fn crate_key(&self, db: &Db, krate: Option<Crate>) -> ContentId;

    /// The fact a read of the query or input `read` (named `name`, its ingredient's debug name)
    /// stands for, if one does: one whose value changes whenever what that read returned does.
    /// `None` keeps the goal out of the store.
    fn fact_of(&self, db: &Db, read: blaze_query::db::DatabaseKeyIndex, name: &'static str)
    -> Option<FactRef>;

    /// The value of `fact` in `db` now (`blaze_items::facts::value`), read by the active query;
    /// `None` when it cannot be hashed now (`blaze_items::Unhashable`).
    fn value(&self, db: &Db, fact: FactRef) -> Option<ContentId>;
}

/// A fact of lane items (`blaze_items::Fact`) as hir-ty keeps it without naming it: its kind
/// (a number of `blaze_query::kinds::FACTS`), its subject and its object, as
/// `blaze_items::FactSet` splits a fact. Its `blaze_items::Fact::key` is a function of these.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FactRef {
    pub kind: QueryKind,
    pub subject: ContentId,
    pub object: FactObject,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum FactObject {
    None,
    /// A field, by position.
    Field(u32),
    Id(ContentId),
}

/// A persisted answer and the reads of checking it (`None` when that check could not be
/// captured shareably: the answer is then good for the asking query alone).
pub(crate) struct Found {
    pub(crate) answer: Answer,
    pub(crate) reads: Option<blaze_query::db::Reads>,
}

/// On a global-table miss, before solving: the stored answer of `key` for `interner`'s crate
/// and typing of regions, if its record's solver version matches and every fact it read
/// hashes the same now. The fact checks are reads of the active query.
#[inline]
pub(crate) fn lookup<'db>(interner: DbInterner<'db>, key: GoalKey) -> Option<Found> {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    lookup_goal(interner, Goal::of(key))
}

/// After solving: `key`'s answer, shareable, with what its evaluation read.
#[inline]
pub(crate) fn offer<'db>(interner: DbInterner<'db>, key: GoalKey, answer: Answer, reads: blaze_query::db::Reads) {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return;
    }
    offer_goal(interner, Goal::of(key), answer, reads);
}

#[inline(never)]
fn lookup_goal<'db>(interner: DbInterner<'db>, goal: Goal<'db>) -> Option<Found> {
    with_store(interner.db, |store| store.lookup(interner, goal)).flatten()
}

#[inline(never)]
fn offer_goal<'db>(
    interner: DbInterner<'db>,
    goal: Goal<'db>,
    answer: Answer,
    reads: blaze_query::db::Reads,
) {
    with_store(interner.db, |store| store.offer(interner, goal, answer, reads));
}

const SOLVER_DOMAIN: &str = "blaze v2 goal 1 solver";

/// What the solver's answers are a function of besides the goal and the facts: the pinned
/// nightly, the vendored solver's patch series (each crate's series file and every patch), and
/// the front end's code ([`install`]'s `build`). A patch added to a series is added here too;
/// `the_solver_version_covers_every_patch` checks it.
const SOLVER_FILES: [(&str, &[u8]); 23] = [
    ("harness/toolchain", include_bytes!("../../../../harness/toolchain")),
    (
        "vendor/rustc_next_trait_solver/PATCHES.md",
        include_bytes!("../../../../vendor/rustc_next_trait_solver/PATCHES.md"),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0001-opaque-of-impl.patch",
        include_bytes!("../../../../vendor/rustc_next_trait_solver/patches/0001-opaque-of-impl.patch"),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0002-unsafe-code-policy.patch",
        include_bytes!(
            "../../../../vendor/rustc_next_trait_solver/patches/0002-unsafe-code-policy.patch"
        ),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0003-rustc-rules-at-unimplemented-sites.patch",
        include_bytes!(
            "../../../../vendor/rustc_next_trait_solver/patches/0003-rustc-rules-at-unimplemented-sites.patch"
        ),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0004-solver-temporaries-recycled.patch",
        include_bytes!(
            "../../../../vendor/rustc_next_trait_solver/patches/0004-solver-temporaries-recycled.patch"
        ),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0005-fast-path-depth.patch",
        include_bytes!("../../../../vendor/rustc_next_trait_solver/patches/0005-fast-path-depth.patch"),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0006-def-id-is-local.patch",
        include_bytes!("../../../../vendor/rustc_next_trait_solver/patches/0006-def-id-is-local.patch"),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0007-solver-temporaries-kept.patch",
        include_bytes!(
            "../../../../vendor/rustc_next_trait_solver/patches/0007-solver-temporaries-kept.patch"
        ),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0008-scratch-buffers-raised.patch",
        include_bytes!(
            "../../../../vendor/rustc_next_trait_solver/patches/0008-scratch-buffers-raised.patch"
        ),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0009-bailed-probes-give-back-their-goals.patch",
        include_bytes!(
            "../../../../vendor/rustc_next_trait_solver/patches/0009-bailed-probes-give-back-their-goals.patch"
        ),
    ),
    (
        "vendor/rustc_next_trait_solver/patches/0010-never-grown-scratch-buffers-kept.patch",
        include_bytes!(
            "../../../../vendor/rustc_next_trait_solver/patches/0010-never-grown-scratch-buffers-kept.patch"
        ),
    ),
    ("vendor/rustc_type_ir/PATCHES.md", include_bytes!("../../../../vendor/rustc_type_ir/PATCHES.md")),
    (
        "vendor/rustc_type_ir/patches/0001-from-checked-parts.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0001-from-checked-parts.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0002-opaque-of-impl.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0002-opaque-of-impl.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0003-shared-solver-cache.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0003-shared-solver-cache.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0004-unsafe-code-policy.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0004-unsafe-code-policy.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0005-simplified-type-ord.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0005-simplified-type-ord.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0006-solver-storage-recycled.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0006-solver-storage-recycled.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0007-fast-path-depth.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0007-fast-path-depth.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0008-def-id-is-local.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0008-def-id-is-local.patch"),
    ),
    (
        "vendor/rustc_type_ir/patches/0009-small-sets-and-stalled-lists-inline.patch",
        include_bytes!(
            "../../../../vendor/rustc_type_ir/patches/0009-small-sets-and-stalled-lists-inline.patch"
        ),
    ),
    (
        "vendor/rustc_type_ir/patches/0010-solver-cache-in-blocks.patch",
        include_bytes!("../../../../vendor/rustc_type_ir/patches/0010-solver-cache-in-blocks.patch"),
    ),
];

/// The solver version of a front end whose code is `build`.
pub fn solver_version(build: &ContentId) -> ContentId {
    let mut h = Hasher::new(SOLVER_DOMAIN);
    for (name, bytes) in SOLVER_FILES {
        h.part(name.as_bytes()).part(bytes);
    }
    h.id(build).finish()
}

/// The goal store of one database.
struct Store {
    nonce: Nonce,
    version: ContentId,
    // dyn: the driver's store over `blaze-cas`, probed on a global-table miss, written at a flush
    images: Arc<dyn ImageStore + Send + Sync>,
    // dyn: lane items' keys and facts, above hir-ty; asked on a miss and after a solve, never on a hit
    facts: Arc<dyn GoalFacts>,
    pending: Pending,
    /// The first error a lookup met (a store that could not be read, a record that does not
    /// open under its key): [`Installed::flush`] stops with it. The goal it met it on was
    /// solved instead, which gives the same answer, so the build's error comes at the flush.
    error: Mutex<Option<String>>,
}

/// The buffers an offer fills, kept for the thread's next one. Taken out for the offer and put
/// back after, so an offer nested in one (a fact's value that solves) finds none and allocates.
#[derive(Default)]
struct Scratch {
    facts: Vec<FactRef>,
    values: Vec<ContentId>,
    words: Vec<u32>,
}

thread_local! {
    static SCRATCH: Cell<Scratch> = Cell::new(Scratch::default());
}

/// Why an offered goal is not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// A read of this query or input stands for no fact.
    Read(&'static str),
    /// A fact it read cannot be hashed now.
    Unhashable,
    /// Its stable form is longer than [`stable::MAX_WORDS`].
    TooLong,
}

impl Store {
    fn lookup<'db>(&self, interner: DbInterner<'db>, goal: Goal<'db>) -> Option<Found> {
        let (answer, reads) = blaze_query::db::capture_reads(interner.db, || self.find(interner, goal));
        Some(Found { answer: answer?, reads })
    }

    fn find<'db>(&self, interner: DbInterner<'db>, goal: Goal<'db>) -> Option<Answer> {
        let db = interner.db;
        let crate_key = self.facts.crate_key(db, interner.krate());
        let goal_key = stable::goal_key(interner, &*self.facts, &self.version, &crate_key, goal)?;
        for n in 0..SLOTS {
            let at = shape_key(&goal_key, n);
            let shape = match self.images.get(&at) {
                Ok(Some(shape)) => shape,
                Ok(None) => return None,
                Err(why) => {
                    // alloc: error text
                    self.fail(format!("reading the shape of goal {goal_key:x} in slot {n}: {why}"));
                    return None;
                }
            };
            let shape = match ShapeRef::open(shape.bytes(), &goal_key, &self.version) {
                Ok(shape) => shape,
                Err(why) => {
                    // alloc: error text
                    self.fail(format!("the shape of goal {goal_key:x} in slot {n}: {why}"));
                    return None;
                }
            };
            let Some(answer_key) = self.answer_key_now(db, &at, &shape) else { continue };
            match self.images.get(&answer_key) {
                Ok(Some(found)) => match record::open_answer(found.bytes(), &answer_key) {
                    Ok(answer) => return Some(answer),
                    Err(why) => {
                        // alloc: error text
                        self.fail(format!("the answer of goal {goal_key:x} in slot {n}: {why}"));
                        return None;
                    }
                },
                Ok(None) => {}
                Err(why) => {
                    // alloc: error text
                    self.fail(format!("reading the answer of goal {goal_key:x} in slot {n}: {why}"));
                    return None;
                }
            }
        }
        None
    }

    /// The key the answer of the shape at `shape_key` has under its facts' values now; `None`
    /// when one of them cannot be hashed now.
    fn answer_key_now(&self, db: &Db, shape_key: &ContentId, shape: &ShapeRef<'_>) -> Option<ContentId> {
        let mut key = AnswerKey::new(shape_key);
        for fact in shape.facts() {
            key.value(&self.facts.value(db, fact)?);
        }
        Some(key.finish())
    }

    fn offer<'db>(&self, interner: DbInterner<'db>, goal: Goal<'db>, answer: Answer, reads: blaze_query::db::Reads) {
        let mut scratch = SCRATCH.take();
        scratch.facts.clear();
        scratch.values.clear();
        scratch.words.clear();
        match self.offered(interner, goal, answer, reads, &mut scratch) {
            Ok(()) => {}
            Err(refusal) => self.pending.refused(refusal),
        }
        SCRATCH.set(scratch);
    }

    fn offered<'db>(
        &self,
        interner: DbInterner<'db>,
        goal: Goal<'db>,
        answer: Answer,
        reads: blaze_query::db::Reads,
        scratch: &mut Scratch,
    ) -> Result<(), Refusal> {
        let db = interner.db;
        let mut refused = None;
        let facts = &mut scratch.facts;
        let storage = blaze_query::db::Database::storage(db);
        let _ = reads.for_each_read(storage, |blaze_query::db::Read { key: read, kind }| {
            if kind == blaze_query::db::ReadKind::Interned {
                return ControlFlow::Continue(());
            }
            let name = storage.read_name(read, kind);
            match self.facts.fact_of(db, read, name) {
                Some(fact) => {
                    facts.push(fact);
                    ControlFlow::Continue(())
                }
                None => {
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
        let crate_key = self.facts.crate_key(db, interner.krate());
        if !stable::goal_words(interner, &*self.facts, goal, &mut scratch.words) {
            return Err(Refusal::TooLong);
        }
        let goal_key = stable::key_of(&self.version, &crate_key, &scratch.words);
        self.pending.push(&goal_key, &self.version, answer, &scratch.words, &scratch.facts, &scratch.values);
        Ok(())
    }

    fn fail(&self, why: String) {
        let mut error = self.error.lock();
        error.get_or_insert(why);
    }

    /// Writes one buffered goal: its shape in the first slot that holds it or is free, then its
    /// answer under the shape's key and the values its facts had.
    fn write(&self, entry: &pending::Entry<'_>) -> Result<Wrote, String> {
        let goal = entry.goal_key;
        // alloc: error text
        let error = |what: &str, why: String| format!("{what} goal {goal:x}: {why}");
        let mut wrote = Wrote::Kept;
        let mut slot = None;
        for n in 0..SLOTS {
            let at = shape_key(&goal, n);
            match self.images.get(&at).map_err(|why| error("reading the shape of", why))? {
                Some(kept) if kept.bytes() == entry.shape => {
                    slot = Some(at);
                    break;
                }
                Some(_) => continue,
                // alloc: the shape's bytes, once per goal written
                None => match self.images.put(at, entry.shape.to_vec()) {
                    Ok(()) => {
                        wrote = Wrote::Written;
                        slot = Some(at);
                        break;
                    }
                    Err(why) => match self.images.get(&at).map_err(|why| error("reading the shape of", why))? {
                        Some(kept) if kept.bytes() == entry.shape => {
                            slot = Some(at);
                            break;
                        }
                        Some(_) => continue,
                        None => return Err(error("keeping the shape of", why)),
                    },
                },
            }
        }
        let Some(at) = slot else { return Ok(Wrote::SlotsFull) };
        let mut key = AnswerKey::new(&at);
        for value in entry.values() {
            key.value(&value);
        }
        let answer_key = key.finish();
        let answer = record::answer_record(&answer_key, entry.answer);
        match self.images.get(&answer_key).map_err(|why| error("reading the answer of", why))? {
            Some(kept) if kept.bytes() == answer => Ok(wrote),
            Some(_) => Err(error(
                "two answers under the same fact values of",
                // alloc: error text
                String::from("its evaluation read something its facts do not cover"),
            )),
            None => {
                // alloc: the answer's bytes, once per goal written
                self.images.put(answer_key, answer.to_vec()).map_err(|why| error("keeping the answer of", why))?;
                Ok(Wrote::Written)
            }
        }
    }
}

/// What writing one buffered goal did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wrote {
    /// Its shape or its answer was new.
    Written,
    /// The store had both already.
    Kept,
    /// Every slot of its key holds another shape.
    SlotsFull,
}

/// What [`Installed::flush`] did, and why offered goals were not kept.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Goals whose shape or answer was written.
    pub written: usize,
    /// Goals the store had already.
    pub kept: usize,
    /// Goals not written as every slot of their key holds another shape.
    pub slots_full: usize,
    /// For each query or input whose read kept goals out (it stands for no fact), how many,
    /// most first: the facts lane items is asked for.
    pub inexpressible: Vec<(&'static str, u64)>,
    /// Goals kept out as a fact they read could not be hashed.
    pub unhashable: u64,
    /// Goals kept out as their stable form is too long.
    pub too_long: u64,
    /// Goals dropped as the buffer of their thread was full.
    pub dropped: u64,
}

/// How many databases may have a goal store at once.
const STORES: usize = 16;

/// How many goal stores are installed: zero is the one load [`lookup`] and [`offer`] make.
static INSTALLED: AtomicUsize = AtomicUsize::new(0);

/// The installed stores, each a `Box` of [`install`], freed through [`LIMBO`] once unlinked.
static REGISTRY: [AtomicPtr<Store>; STORES] = [const { AtomicPtr::new(ptr::null_mut()) }; STORES];

static LIMBO: LazyLock<Mutex<Limbo>> = LazyLock::new(|| Mutex::new(Limbo::new()));

/// Runs `f` on the goal store of `db`, if one is installed.
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

/// Gives `db` a goal store: `images`, the driver's store (the local store, or the local store
/// over the global cache through `hir_def::deps::ReadThrough`), `facts`, lane items' keys and
/// facts, and `build`, the content ID of the front end's code (what `hir_def::deps::serve` is
/// given). Until the guard is dropped, every lookup and offer of `db` goes to it; what was
/// offered and not flushed is dropped with it.
pub fn install(
    db: &Db,
    // dyn: the driver's store, kept by the goal store (see `Store::images`)
    images: Arc<dyn ImageStore + Send + Sync>,
    // dyn: lane items' keys and facts, kept by the goal store (see `Store::facts`)
    facts: Arc<dyn GoalFacts>,
    build: ContentId,
) -> Result<Installed, String> {
    let (nonce, _) = db.nonce_and_revision();
    {
        let _task = blaze_reclaim::enter();
        for slot in &REGISTRY {
            // SAFETY: as in `with_store`, under `_task`.
            if unsafe { slot.load(Ordering::Acquire).as_ref() }.is_some_and(|it| it.nonce == nonce) {
                // alloc: error text
                return Err(String::from("this database has a goal store installed already"));
            }
        }
    }
    // alloc: setup: once per installed store, freed through `LIMBO` when it is uninstalled
    let store = Box::into_raw(Box::new(Store {
        nonce,
        version: solver_version(&build),
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
    Err(format!("more than {STORES} databases with a goal store at once"))
}

/// A goal store installed for a database ([`install`]); dropping it uninstalls it.
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

    /// Writes every goal offered since the last flush, on the store's threads
    /// (`ImageStore::for_each`). The driver calls it where it writes the dependency images:
    /// after a build's front-end work, with no query running. An error a lookup met since the
    /// last flush, a store that will not keep a record, or a second answer under the same
    /// facts (a read the facts do not cover) is an error naming the goal.
    pub fn flush(&self) -> Result<Flushed, String> {
        let store = self.store();
        let (buffers, tallies) = store.pending.take();
        // alloc: flush, once per build: one entry per goal offered
        let entries: Vec<pending::Entry<'_>> = buffers.iter().flat_map(|it| pending::entries(it)).collect();
        // alloc: flush, as above
        let results: Vec<OnceLock<Result<Wrote, String>>> = entries.iter().map(|_| OnceLock::new()).collect();
        store.images.for_each(entries.len(), &|at| {
            _ = results[at].set(store.write(&entries[at]));
        });
        let Tallies { inexpressible, unhashable, too_long, dropped } = tallies;
        let mut flushed = Flushed { inexpressible, unhashable, too_long, dropped, ..Flushed::default() };
        for result in results {
            let wrote = result.into_inner().unwrap_or_else(|| {
                panic!("`ImageStore::for_each` did not run every task of a goal store flush")
            })?;
            match wrote {
                Wrote::Written => flushed.written += 1,
                Wrote::Kept => flushed.kept += 1,
                Wrote::SlotsFull => flushed.slots_full += 1,
            }
        }
        if let Some(why) = store.error.lock().take() {
            return Err(why);
        }
        Ok(flushed)
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let held = REGISTRY[self.at].swap(ptr::null_mut(), Ordering::AcqRel);
        assert_eq!(held, self.store.as_ptr(), "a goal store's slot held another store");
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
