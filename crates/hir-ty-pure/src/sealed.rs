//! Summaries of instances no edit can change, held as no row (track 5b, T3).
//!
//! An instance whose home ([`home_env`]) is a sealed crate (`hir_def::deps::sealed`: no
//! workspace member, nor any crate below it, its root in a library root) is first summarized in
//! a task scope (`blaze_query::db::task_scope`, the executor's memo: cycles, sharing within the
//! task) under a **witness** frame (`blaze_query::db::witnessed`), which hands back the keys of
//! what the run read below `Durability::HIGH` and whether it could name every read. Which route
//! the summary takes is that property of the run, read by the engine, not a guess:
//!
//! - It read nothing below `HIGH` and is complete: what it read is what cannot change while the
//!   crate's **cover** holds, so its value, and every value the run made on the way, is shared
//!   across roots and tasks through one table a sealed crate ([`Sealed`]), the value of one
//!   tracked row keyed by the cover: the image keys of the crate and every crate below it. A
//!   persistent caller's whole read of sealed code is that one row; no summary of it is a row, a
//!   node or an edge.
//! - It read something below `HIGH` (a process variable through `env!`, a file outside every
//!   source root through `include_bytes!`, a `#[path]` module outside its package): it is not
//!   sealed. The instance is summarized as a row ([`summary_in`]), as a workspace instance is,
//!   so an edit of what it read verifies it again; this is said by a span `sealed summary reads
//!   below HIGH` naming the crate, the instance, the first such read (by key order) and its
//!   cause (`Witness::cause_text`: the chain of reads down to the input below `HIGH`), and
//!   counted per crate ([`below_high`]), so the profile answers how many. The instance, and
//!   every instance its run asked on the way ([`Attempt::asked`]), is kept in its crate's
//!   [`Sealed::below_high`] and goes straight to its row from then on: every read before its
//!   first below `HIGH` is one the cover holds, so while the cover holds, a run reaches that
//!   read again.
//!
//! **Nothing is shared before the whole run is known.** A run's callees settle inside it, and
//! a callee's own reads cannot be told from its caller's in the witness: so each value a run
//! settles is held in its attempt's pending list ([`Attempt::settled`]) and written to its
//! crate's table only once the outer run is found to have read nothing below `HIGH`, which then
//! holds of every callee too (each one's reads, a scope's hit's replayed, are in the outer
//! witness). A run that read below `HIGH` leaves its pending values unshared, and routes every
//! instance it asked to its row: which callee made the run low the witness cannot say, and a
//! callee asked again on a witness of its own would run its whole subgraph again, each depth
//! of a chain once more than the one above it (quadratic in the depth, the cost of track 5b's
//! first cut). A row is right for any instance (it is what a workspace instance is); the cost
//! is that a callee of a low run reading only `HIGH` is a row, not shared. So each instance is
//! summarized at most twice: once in the discarded run, once as its row. Once more is
//! [`Reattempted`], a typed error naming the instance and the read.
//!
//! An instance whose home is a workspace crate stays a row ([`summary_in`]): an edit must verify
//! it again, and the row graph is linear where forwarded reads would be quadratic.
//!
//! **No read of [`sealed`] under the witness frame.** The cover is made from image keys, which
//! read the crate's source root, and the row is asked once, before the frame. A run under the
//! frame is handed the [`Sealed`] of its home in its key and finds its callees' tables in
//! [`Sealed::below`]: every callee of a sealed crate's instance has its home in that crate or
//! below it (`hir_ty_mir::mir::home::home` picks a crate the asker loads).
//!
//! What is not built here, said plainly: the values stay on the heap while a build runs, in the
//! tables, freed whole when the row's value goes; a store record of them (key: cover and ported
//! instance; value: a `Summary` codec) needs a front-end record hook in hir-ty's body store.

use std::{
    alloc::Global,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base_db::{Crate, Db};
use blaze_hash::{HashMap, SharedMap, SharedSet};
use blaze_ids::{ContentId, Hasher};
use blaze_query::db::Witness;
use hir_ty::traits::StoredParamEnvAndCrate;

use crate::walk::{Def, Instance, Summary, home_env, name_of, step_key, summarized};

/// The domain of a sealed crate's cover, and of the `build` its image keys are asked by.
pub(crate) const COVER_DOMAIN: &str = "blaze v2 pure sealed cover 1";

/// The summaries of one sealed crate's instances, by the environment and instance asked.
pub(crate) type Table = SharedMap<(StoredParamEnvAndCrate, Instance), Arc<Summary>>;

/// One witnessed run of a sealed crate's instance ([`sealed_or_row`]).
#[derive(Default)]
pub(crate) struct Attempt {
    /// The values the run settled, by the environment and instance, each with the table of its
    /// home's crate: written to those tables only once the run read nothing below `HIGH`.
    settled: SharedMap<(StoredParamEnvAndCrate, Instance), (Arc<Table>, Arc<Summary>)>,
    /// Every instance the run asked on the scoped path ([`from_table`]), the outer one among
    /// them, with its home crate's [`Sealed`] (no attempt in it: no cycle of counts): routed
    /// to their rows when the run read below `HIGH`.
    asked: SharedMap<(StoredParamEnvAndCrate, Instance), Sealed>,
}

/// The epoch of the routes to rows: each run that read below `HIGH` takes the next once it has
/// routed what it asked ([`route_to_rows`]). Process-wide and only rising: a run that started at
/// or after the epoch an instance was routed at found the route, so it never asks the instance
/// on the scoped path. Not an output: a run's value never reads it.
static ROUTED: AtomicU64 = AtomicU64::new(0);

/// The summaries of instances whose home is `krate`, shared by every task: a function of
/// `cover` alone, so two of one cover are equal whatever their tables hold.
#[derive(Clone)]
pub(crate) struct Sealed {
    cover: ContentId,
    krate: Crate,
    table: Arc<Table>,
    /// The instances of `krate` asked by a run that read below `HIGH`, by the environment
    /// asked: each is a row ([`summary_in`]), and how many there are is the crate's count
    /// ([`below_high`]).
    below_high: Arc<SharedSet<(StoredParamEnvAndCrate, Instance)>>,
    /// Each instance of `below_high` by the epoch ([`ROUTED`]) of the first run that routed it:
    /// a run that started at or after it and summarizes it on the scoped path is
    /// [`Reattempted`].
    routed: Arc<SharedMap<(StoredParamEnvAndCrate, Instance), u64>>,
    /// Each crate below `krate` (all sealed) by its own [`Sealed`]: where a run under the
    /// witness frame finds its callees' tables without a read.
    below: Arc<HashMap<Crate, Sealed, Global>>,
    /// The witnessed run this one is a part of, whose settled values wait in it: `None` for
    /// the value of the [`sealed`] row, `Some` for every [`summary_sealed`] key.
    attempt: Option<Arc<Attempt>>,
}

impl Sealed {
    /// The [`Sealed`] of `krate`, which is this crate or one below it, as a part of this one's
    /// attempt.
    ///
    /// # Panics
    ///
    /// Naming both crates, when `krate` is neither: a sealed instance's callee whose home is not
    /// below its caller's breaks what [`hir_ty_mir::mir::home::home`] promises, and its summary
    /// would be read under a cover that does not hold its inputs.
    fn of(&self, krate: Crate, db: &Db, instance: &Instance) -> Sealed {
        let found = if krate == self.krate { self } else { self.below_of(krate, db, instance) };
        Sealed { attempt: self.attempt.clone(), ..found.clone() } // clone: reference counts
    }

    /// The [`Sealed`] of `krate`, a crate below this one: see [`Sealed::of`].
    fn below_of(&self, krate: Crate, db: &Db, instance: &Instance) -> &Sealed {
        self.below.get(&krate).unwrap_or_else(|| {
            panic!(
                "the summary of {} has its home in crate {krate:?}, which is not crate {:?} nor \
                 below it: a sealed crate's instance reached a callee its cover does not hold \
                 (rule keys-cover-every-input)",
                name_of(db, instance),
                self.krate,
            )
        })
    }
}

impl PartialEq for Sealed {
    fn eq(&self, other: &Sealed) -> bool {
        self.cover == other.cover
    }
}

impl Eq for Sealed {}

impl std::hash::Hash for Sealed {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.cover.hash(state);
    }
}

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sealed({:?}, cover {})", self.krate, self.cover)
    }
}

/// `None` unless `hir_def::deps::sealed` holds of `krate`. The cover: [`COVER_DOMAIN`] over
/// `hir_def::deps::keys` of the crate and every crate below it (`Crate::transitive_deps`, the
/// crate among them) by `ContentId::of(COVER_DOMAIN, b"build")`, in `blaze_items::key::crate_key`
/// order, each key by its id; a crate with no key makes it `None`.
#[blaze_query::tracked(returns(clone))]
pub(crate) fn sealed(db: &Db, krate: Crate) -> Option<Sealed> {
    if !hir_def::deps::sealed(db, krate) {
        return None;
    }
    let crates = krate.transitive_deps(db);
    let keys = hir_def::deps::keys(db, crates, ContentId::of(COVER_DOMAIN, b"build"));
    let mut ordered: Vec<(ContentId, ContentId)> = Vec::with_capacity(crates.len()); // alloc: a crate's cover, once per row run
    for &it in crates {
        let name = blaze_items::key::crate_key(db, it);
        #[cfg(test)]
        if plant::names_only() {
            ordered.push((name, name));
            continue;
        }
        ordered.push((name, *keys.get(&it)?));
    }
    ordered.sort_unstable();
    let mut hasher = Hasher::new(COVER_DOMAIN);
    hasher.update(&(ordered.len() as u64).to_le_bytes());
    for (_, key) in &ordered {
        hasher.id(key);
    }
    // Every dependency is sealed too (`sealed` holds of none whose dependency it fails for):
    // its own row gives its table, and what is below it.
    let mut below: HashMap<Crate, Sealed, Global> = HashMap::new_in(Global);
    for dep in &krate.data(db).dependencies {
        let it = sealed(db, dep.crate_id)?;
        for (krate, sealed) in it.below.iter() {
            if !below.contains_key(krate) {
                below.insert(*krate, sealed.clone()); // clone: two reference counts
            }
        }
        below.insert(dep.crate_id, it);
    }
    Some(Sealed {
        cover: hasher.finish(),
        krate,
        table: Arc::new(Table::default()), // alloc: the crate's table, once per cover
        below_high: Arc::new(SharedSet::default()), // alloc: the crate's rows, once per cover
        routed: Arc::new(SharedMap::default()),     // alloc: their epochs, once per cover
        below: Arc::new(below), // alloc: the crates below, once per cover
        attempt: None,
    })
}

/// How many instances of `krate` were asked by a run that read below `HIGH` under its present
/// cover (the run's own instance and every one it asked), and so are rows ([`summary_in`])
/// though the crate is sealed: 0 for a crate that is not.
pub(crate) fn below_high(db: &Db, krate: Crate) -> usize {
    sealed(db, krate).map_or(0, |it| it.below_high.len())
}

/// What the instance does, asked from `env`: worked out in its home ([`home_env`]), so every
/// build script that reaches it shares one summary. A body instance is a step of the walk's
/// bound, if one is set (`blaze_query::db::with_steps`), however often it is asked for.
///
/// The read of [`sealed`] is made before the witness frame opens, so a persistent caller holds
/// it: its one read of the sealed crate's code.
pub(crate) fn summary(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Arc<Summary> {
    summary_within(db, None, env, instance)
}

/// [`summary`], asked from a run under the witness frame of the sealed crate `within` (`None`:
/// from a persistent caller, or none).
pub(crate) fn summary_within(
    db: &Db,
    within: Option<&Sealed>,
    env: StoredParamEnvAndCrate,
    instance: Instance,
) -> Arc<Summary> {
    if let Def::Body(body) = instance.def {
        blaze_query::db::step(|| step_key(body, &instance.args), || name_of(db, &instance));
    }
    let home = home_env(db, &env, &instance);
    match within {
        Some(outer) => {
            let sealed = outer.of(home.krate, db, &instance);
            from_table(db, sealed, home, instance)
        }
        #[cfg(test)]
        None if plant::all_rows() => summary_in(db, home, instance),
        None => match sealed(db, home.krate) {
            None => summary_in(db, home, instance),
            Some(sealed) => sealed_or_row(db, &sealed, home, instance),
        },
    }
}

/// The summary of `instance` in `home`, a sealed crate's environment, asked by a persistent
/// caller: its table's; a row when it is known to read below `HIGH`; else made under a witness
/// frame, and shared with every value the run settled when the run read nothing below `HIGH`,
/// a row when it did (see the module's doc).
fn sealed_or_row(
    db: &Db,
    sealed: &Sealed,
    home: StoredParamEnvAndCrate,
    instance: Instance,
) -> Arc<Summary> {
    // clone: the table's key, reference counts
    let key = (home.clone(), instance.clone());
    if let Some(it) = sealed.table.pin().get(&key) {
        return Arc::clone(it);
    }
    if sealed.below_high.pin().contains(&key) {
        return summary_in(db, home, instance);
    }
    let attempt = Arc::new(Attempt::default()); // alloc: one witnessed run's settled values
    // clone: reference counts
    let part = Sealed { attempt: Some(Arc::clone(&attempt)), ..sealed.clone() };
    // Loaded before the run asks anything: every route of an epoch below it is found.
    let started = ROUTED.load(Ordering::SeqCst);
    let mut reads = Vec::new(); // alloc: the keys of one witnessed run's reads below `HIGH`
    // clone: the scoped key, reference counts
    let (value, witness) = blaze_query::db::witnessed(db, &mut reads, || {
        from_table(db, part, home.clone(), instance.clone())
    });
    #[cfg(test)]
    let witness = match plant::shares_below_high() {
        // The planted bug: the run is shared whatever it read.
        true => {
            reads.clear();
            Witness { complete: true }
        }
        false => witness,
    };
    if witness.complete && reads.is_empty() {
        for (key, (table, settled)) in attempt.settled.pin().iter() {
            // clone: the table's key and value, reference counts
            _ = table.pin().try_insert(key.clone(), Arc::clone(settled));
        }
        return value;
    }
    let krate = crate_name(db, home.krate);
    let name = name_of(db, &instance);
    match reads.first() {
        Some(&first) => {
            let read = Witness::read_text(db, first);
            let cause = Witness::cause_text(db, first);
            let (discarded, epoch) = route_to_rows(db, &key, &attempt, started, &cause);
            let _span = tracing::info_span!(
                target: "blaze",
                "sealed summary reads below HIGH",
                krate = %krate,
                instance = %name,
                read = %read,
                cause = %cause,
                discarded,
                epoch,
                count = sealed.below_high.len(),
            )
            .entered();
            summary_in(db, home, instance)
        }
        None => {
            // Under another witness frame (whose reads this one cannot name) or a run it could
            // not give keys for: a row this time, not kept as one, since it is the asker's
            // frame and not the instance that made it so.
            let _span = tracing::info_span!(
                target: "blaze",
                "sealed summary not complete",
                krate = %krate,
                instance = %name,
            )
            .entered();
            summary_in(db, home, instance)
        }
    }
}

/// Routes every instance `attempt` asked on the scoped path (`root`, its outer instance, among
/// them) to its row ([`Sealed::below_high`] of its home), then takes the next epoch
/// ([`ROUTED`]) and records it as the epoch of each one not routed before. How many it
/// discarded and the epoch, for the span.
///
/// # Panics
///
/// [`Reattempted`], when an instance the run asked was routed at an epoch at or below
/// `started`, the epoch the run began at: it found the route, so it asked the instance on the
/// scoped path only if the routing is broken, and work a discarded run did is done again.
fn route_to_rows(
    db: &Db,
    root: &(StoredParamEnvAndCrate, Instance),
    attempt: &Attempt,
    started: u64,
    cause: &str,
) -> (usize, u64) {
    let asked = attempt.asked.pin();
    for (key, home) in asked.iter() {
        // The planted bug: the outer instance alone is routed, as before the fix, so each
        // callee is asked again on a witness of its own.
        #[cfg(test)]
        if plant::routes_the_root_only() && key != root {
            continue;
        }
        _ = home.below_high.pin().insert(key.clone()); // clone: the routing's key, reference counts
    }
    #[cfg(not(test))]
    let _ = root;
    // After every route above: a run that loads this epoch or a later one finds each of them.
    let epoch = ROUTED.fetch_add(1, Ordering::SeqCst) + 1;
    for (key, home) in asked.iter() {
        // clone: the routing's key, reference counts
        if let Err(it) = home.routed.pin().try_insert(key.clone(), epoch)
            && *it.current <= started
        {
            Reattempted {
                krate: crate_name(db, key.0.krate),
                instance: name_of(db, &key.1),
                cause: cause.to_owned(), // alloc: an error's text, once
                routed: *it.current,
                started,
            }
            .throw();
        }
        // Routed before by a run still going when this one started: both discarded it once.
    }
    (asked.len(), epoch)
}

/// A sealed crate's instance summarized on the scoped path by a witnessed run that started at
/// or after the epoch another run, which read below `HIGH`, routed it to its row: the work of
/// a discarded run done again, which [`route_to_rows`] makes impossible, so a bug in the
/// routing (rule bounded-progress: each instance is summarized at most twice, once in the
/// discarded run and once as its row). It unwinds with this as its payload
/// ([`Reattempted::throw`]), failing the job that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reattempted {
    /// The instance's home crate, by name.
    pub(crate) krate: String,
    /// The instance, by name (`walk::name_of`).
    pub(crate) instance: String,
    /// The first read below `HIGH` of the run that discarded it again, with its chain of
    /// reads down to the input (`Witness::cause_text`).
    pub(crate) cause: String,
    /// The epoch ([`ROUTED`]) it was first routed at, and the one the run started at.
    pub(crate) routed: u64,
    pub(crate) started: u64,
}

impl Reattempted {
    /// Unwinds with `self` as the payload. It is logged first: a catcher that reads only a
    /// panic's text finds none in it, and the panic hook prints only where it was thrown.
    pub(crate) fn throw(self) -> ! {
        tracing::error!(target: "blaze", "{self}");
        std::panic::panic_any(self)
    }
}

impl std::fmt::Display for Reattempted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pure summary: {} of crate `{}` was summarized again on the scoped path by a \
             witnessed run that started at epoch {}, after a run that read below `HIGH` routed \
             it to its row at epoch {}; the run read {} (rule bounded-progress: a sealed \
             crate's instance is summarized at most once in a discarded run and once as its row)",
            self.instance, self.krate, self.started, self.routed, self.cause,
        )
    }
}

/// The name of `krate` a span shows: its display name, else its ID.
fn crate_name(db: &Db, krate: Crate) -> String {
    match &krate.extra_data(db).display_name {
        Some(name) => name.crate_name().as_str().to_owned(), // alloc: a span's field, once a row
        None => format!("{krate:?}"),                         // alloc: as above
    }
}

/// The summary of `instance` in `home`, a sealed crate's environment, asked inside a witnessed
/// run that `sealed` is a part of: its table's; a row when it is known to read below `HIGH` (so
/// the run is not shared, as it is not when made again); else made in the task scope (opened
/// here when none is: a scope already open on the task is used as it is).
fn from_table(
    db: &Db,
    sealed: Sealed,
    home: StoredParamEnvAndCrate,
    instance: Instance,
) -> Arc<Summary> {
    // clone: the table's key, reference counts
    let key = (home.clone(), instance.clone());
    if let Some(it) = sealed.table.pin().get(&key) {
        return Arc::clone(it);
    }
    if sealed.below_high.pin().contains(&key) {
        return summary_in(db, home, instance);
    }
    let attempt = sealed.attempt.as_ref().unwrap_or_else(|| {
        panic!(
            "the summary of {} in crate {:?} was asked on the scoped path outside a witnessed \
             run: `from_table` is given a part of one (`sealed_or_row`, `Sealed::of`)",
            name_of(db, &instance),
            sealed.krate,
        )
    });
    // clone: reference counts; the attempt is left out, so the record holds no count of it
    _ = attempt.asked.pin().try_insert(key, Sealed { attempt: None, ..sealed.clone() });
    blaze_query::db::task_scope(db, || summary_sealed(db, sealed, home, instance))
}

/// What the instance does, in `env`, a workspace crate's: a row an edit verifies. A cycle of
/// instances starts from the summary of a recursion (`Summary::recursive`) and iterates to its
/// fixpoint: each local's value only rises, in a lattice three high, and the recursion's witness
/// keeps one depth.
#[blaze_query::tracked(returns(clone), cycle_initial = |_, _, _, _| Arc::new(Summary::recursive()))]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a tracked query's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
pub(crate) fn summary_in(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Arc<Summary> {
    #[cfg(test)]
    plant::ran(db, &instance);
    summarized(db, None, env, &instance)
}

/// What the instance does, in `env`, the environment of `sealed`'s crate: the task scope's
/// value, no row. A cycle iterates in the scope as [`summary_in`]'s does in the engine; each
/// final value is offered to the crate's table ([`publish`]), never a round's.
#[blaze_query::tracked(
    scope = task,
    returns(clone),
    settled = publish,
    cycle_initial = |_, _, _, _| Arc::new(Summary::recursive())
)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a scoped function's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
fn summary_sealed(
    db: &Db,
    sealed: Sealed,
    env: StoredParamEnvAndCrate,
    instance: Instance,
) -> Arc<Summary> {
    #[cfg(test)]
    plant::ran(db, &instance);
    summarized(db, Some(&sealed), env, &instance)
}

/// Offers a final value of [`summary_sealed`] to its crate's table, through the attempt it is
/// a part of: the table has it once the attempt's run is found to read nothing below `HIGH`
/// ([`sealed_or_row`]). Two tasks may summarize one instance at once: both values are equal (a
/// function of the cover and the key), and the first is kept.
fn publish(_db: &Db, key: &(Sealed, StoredParamEnvAndCrate, Instance), value: &Arc<Summary>) {
    let (sealed, env, instance) = key;
    let attempt = sealed.attempt.as_ref().unwrap_or_else(|| {
        panic!(
            "a summary of {instance:?} in crate {:?} settled outside a witnessed run: a \
             `summary_sealed` key is made by `sealed_or_row` or `Sealed::of`, both a part of one",
            sealed.krate
        )
    });
    #[cfg(test)]
    let value = &match plant::publishes_a_round() {
        // The planted bug: what a cycle starts from, as a `Provisional` entry's round value.
        true => Arc::new(Summary::recursive()), // alloc: a planted bug's value
        false => Arc::clone(value),
    };
    // clone: the attempt's key and value, reference counts
    let settled = (Arc::clone(&sealed.table), Arc::clone(value));
    _ = attempt.settled.pin().try_insert((env.clone(), instance.clone()), settled);
}

#[cfg(test)]
pub(crate) mod plant {
    //! What a test plants in the routing, and the count of runs it reads.
    use std::cell::{Cell, RefCell};

    use base_db::Db;

    use crate::walk::{Instance, name_of};

    thread_local! {
        static ALL_ROWS: Cell<bool> = const { Cell::new(false) };
        static NAMES_ONLY: Cell<bool> = const { Cell::new(false) };
        static A_ROUND: Cell<bool> = const { Cell::new(false) };
        static BELOW_HIGH: Cell<bool> = const { Cell::new(false) };
        static ROOT_ONLY: Cell<bool> = const { Cell::new(false) };
        /// While [`counting_runs`] runs: each instance's summary runs on this thread, by name.
        static RUNS: RefCell<Option<Vec<(String, u32)>>> = const { RefCell::new(None) };
    }

    /// A summary of `instance` ran (a row's or a scoped run): counted while [`counting_runs`]
    /// runs on this thread.
    pub(super) fn ran(db: &Db, instance: &Instance) {
        if RUNS.with(|it| it.borrow().is_none()) {
            return;
        }
        // Named outside the borrow: a name reads the database.
        let name = name_of(db, instance);
        RUNS.with(|it| {
            let mut runs = it.borrow_mut();
            let runs = runs.as_mut().expect("counting, checked above");
            match runs.iter_mut().find(|it| it.0 == name) {
                Some(it) => it.1 += 1,
                None => runs.push((name, 1)), // alloc: a test's count
            }
        });
    }

    /// `work`, and how many times each instance's summary ran on this thread while it did, by
    /// name ([`name_of`]), sorted by name.
    pub(crate) fn counting_runs<R>(work: impl FnOnce() -> R) -> (R, Vec<(String, u32)>) {
        RUNS.with(|it| *it.borrow_mut() = Some(Vec::new())); // alloc: a test's count
        let made = work();
        let mut runs = RUNS.with(|it| it.borrow_mut().take()).expect("set above");
        runs.sort();
        (made, runs)
    }

    pub(super) fn routes_the_root_only() -> bool {
        ROOT_ONLY.with(Cell::get)
    }

    /// `work` with each run on this thread that read below `HIGH` routing its outer instance
    /// alone to its row, as before every instance it asked was: the planted bug of the chain
    /// test, whose callees are each asked again on a witness of their own.
    pub(crate) fn routing_the_root_only<R>(work: impl FnOnce() -> R) -> R {
        with(&ROOT_ONLY, work)
    }

    /// `work` with `flag` set on this thread, cleared after.
    fn with<R>(flag: &'static std::thread::LocalKey<Cell<bool>>, work: impl FnOnce() -> R) -> R {
        flag.with(|it| it.set(true));
        let made = work();
        flag.with(|it| it.set(false));
        made
    }

    pub(super) fn names_only() -> bool {
        NAMES_ONLY.with(Cell::get)
    }

    pub(super) fn shares_below_high() -> bool {
        BELOW_HIGH.with(Cell::get)
    }

    /// `work` with each witnessed run of a sealed crate's instance on this thread shared as
    /// though it read nothing below `HIGH`, whatever it read: the planted bug of the tests of
    /// summaries that read a process variable or a file outside every source root.
    pub(crate) fn sharing_below_high<R>(work: impl FnOnce() -> R) -> R {
        with(&BELOW_HIGH, work)
    }

    pub(super) fn publishes_a_round() -> bool {
        A_ROUND.with(Cell::get)
    }

    /// `work` with each cover made on this thread of the crates' names (`crate_key`) in place
    /// of their image keys, so no source is in it: the planted bug of the library-edit test.
    pub(crate) fn covering_names_only<R>(work: impl FnOnce() -> R) -> R {
        with(&NAMES_ONLY, work)
    }

    /// `work` with each value offered to a table on this thread a cycle's starting round in
    /// place of the final one: the planted bug of the refinement test.
    pub(crate) fn publishing_a_round<R>(work: impl FnOnce() -> R) -> R {
        with(&A_ROUND, work)
    }

    /// Whether every summary made on this thread is routed to [`super::summary_in`].
    pub(super) fn all_rows() -> bool {
        ALL_ROWS.with(Cell::get)
    }

    /// `work` with every summary made on this thread a row, whatever its home: the planted
    /// bug of the row-count test.
    pub(crate) fn every_home_a_row<R>(work: impl FnOnce() -> R) -> R {
        with(&ALL_ROWS, work)
    }
}
