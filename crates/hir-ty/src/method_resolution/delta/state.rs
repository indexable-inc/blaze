//! The maintainer's state and one write's pass (design §7, §8): the impls held per crate with
//! their keys, the arrangements the slices are published from, and the read sets that say
//! which keys a write may have changed.
//!
//! A pass goes over the crates not frozen. For each it applies name resolution's delta (impls
//! and derives added or removed, traits declared or gone), recomputes the keys of the impls the
//! write woke and the body parts of the modules it may have changed the bodies of, publishes
//! the slices whose rows changed, and hands the next maintainer what changed ([`ImplsDelta`]).
//! A key or body part is recomputed through the pure queries, so recomputing one that did not
//! change costs memo validations and changes nothing.
//!
//! The pass is one node per crate (docs/v2/plan/design/scheduler.md §3.2, step 3), run in
//! the maintainer's reads on the database's workers, not a step per batch of reads with a
//! barrier after each:
//!
//! 1. **[`prepare`]** (step 0's apply): the crates gone dropped, and each crate maintained for
//!    the first time given its directory's write-once slot, all registered with one write.
//! 2. **[`run_nodes`]** (step 1's reads): each crate's work ([`work_crate`]) as a node of a
//!    [`Plan`]: started in order of bottom level (its predicted cost and the longest chain of
//!    its dependents', learned from each crate's last runs, [`Costs`]), begun once every crate
//!    under it has begun. A crate first maintained makes its slices and directory and fills
//!    its slot by `&` ([`publish_new`]), so a reader of it, in another crate's node or after,
//!    waits on that node alone, and the cold pass opens no view for it.
//! 3. **[`finish`]** (step 1's apply): in the crates' order, what each crate's work changed of
//!    the state shared across crates (the read indexes, the impls' owners) replayed and the
//!    slices of a crate published before set by `&mut` ([`finish_crate`]), so no result depends
//!    on which worker ran which crate.
//!
//! A crate's work reads only its own state, the memos and what the pass's start fixed (which
//! impls the write woke, and whose they were), each into its own [`CrateState`] and
//! [`CrateScratch`].

use base_db::{
    Crate, Db, Written,
    maintain::nodes::{Begin, Costs, Plan},
};
use blaze_core::lock::Lock;
use blaze_dbsp::{Arrangement, Pos, ReadIndex, Work};
use blaze_hash::{HashMap, HashSet};
use blaze_mem::Tag;
use blaze_query::db::{Cancelled, SlotClaim, SlotError};
use hir_def::{
    BuiltinDeriveImplId, HasModule, ImplId, ModuleDefId, ModuleId, TraitId,
    builtin_derive::BuiltinDeriveImplTrait,
    lang_item::lang_items,
    nameres::{
        crate_def_map,
        delta::{CrateDelta, LastWrite},
        module_impls,
    },
};
use hir_expand::name::Name;
use intern::Symbol;
use smallvec::SmallVec;
use std::{
    alloc::Global,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use super::{
    ImplsDelta,
    head::Head,
    key::{ImplKey, Reads, assoc_names, derive_key, derive_module, impl_key, impl_reads},
    publish::{
        self, CandidateRows, DirectorySlot, InherentRows, Published, SliceDirectory, TraitRows,
    },
    read::{BodyPart, body_part},
};
use crate::method_resolution::{frozen, macros_writing_impls};

/// What the impls maintainer reports its work under ([`Db::report_work`]).
pub(crate) const NAME: &str = "impls";

/// A row of a trait's arrangement: an impl with its head, or a built-in derive impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TraitEntry {
    Impl(ModuleId, ImplId, Option<Head>),
    Derive(ModuleId, BuiltinDeriveImplId, Head),
}

struct ImplRow {
    module: ModuleId,
    pos: Pos,
    key: ImplKey,
    trait_impl: bool,
}

struct DeriveRow {
    module: ModuleId,
    pos: Pos,
    key: Option<(TraitId, Head)>,
}

/// One crate's impls and traits, and its published inputs.
#[derive(Default)]
pub(crate) struct CrateState {
    impls: HashMap<ImplId, ImplRow, Global>,
    derives: HashMap<BuiltinDeriveImplId, DeriveRow, Global>,
    pub(crate) traits: Arrangement<TraitId, Pos, TraitEntry>,
    pub(crate) heads: Arrangement<Head, Pos, (ModuleId, ImplId)>,
    /// Each trait declared in the crate's modules with its item names (`AssocFn`).
    assoc: HashMap<TraitId, SmallVec<[Name; 8]>, Global>,
    /// Traits whose items a macro call writes: their names may change with no edit of theirs.
    wild_traits: HashSet<TraitId, Global>,
    pub(crate) candidates: Arrangement<Name, TraitId, TraitId>,
    /// The traits the crate's lang items give each built-in derive, by
    /// `BuiltinDeriveImplTrait::ALL` index, once a derive impl asked.
    lang: Option<SmallVec<[Option<TraitId>; 16]>>,
    /// The body part of each module whose bodies may hold an impl.
    bodies: HashMap<ModuleId, BodyPart, Global>,
    /// The modules whose bodies hold impls of each trait, of each head, and (under `()`) any
    /// inherent impl.
    body_traits: Arrangement<TraitId, ModuleId, ModuleId>,
    body_heads: Arrangement<Head, ModuleId, ModuleId>,
    body_inherent: Arrangement<(), ModuleId, ModuleId>,
    /// What `macros_writing_impls` gave the last write: which macro calls may write an impl
    /// in a body, so a change of it reads every module's bodies again.
    macros: Option<Box<[(Symbol, bool)]>>,
    published: Published,
    /// The crate's buffers, kept from one write to the next.
    scratch: CrateScratch,
}

/// Every crate's state, and the read sets across crates: an alias edited in a dependency wakes
/// the impls of its dependents that read it.
#[derive(Default)]
pub struct State {
    crates: HashMap<Crate, CrateState, Global>,
    owner: HashMap<ImplId, Crate, Global>,
    by_name: ReadIndex<Name, ImplId>,
    by_def: ReadIndex<ModuleDefId, ImplId>,
    wild: HashSet<ImplId, Global>,
    /// What each crate's node took on its last runs, by [`cost_key`]: the plan's predictions.
    costs: Costs,
    scratch: Scratch,
}

/// The run's buffers across crates, kept from one write to the next.
#[derive(Default)]
struct Scratch {
    deltas: HashMap<Crate, usize, Global>,
    names: HashSet<Name, Global>,
    defs: HashSet<ModuleDefId, Global>,
    woken: HashSet<ImplId, Global>,
    /// The crates' work, from a pass's nodes ([`run_nodes`]) to its end ([`finish`]).
    jobs: Vec<Job>,
    slots: Vec<(Crate, DirectorySlot)>,
    /// The impls of the crates the pass resets ([`reset`]), whose read sets go in one
    /// [`forget_reset`] once every crate's job is out.
    reset: Vec<ImplId>,
}

/// One crate's buffers, kept from one write to the next: what its work in a run uses, and what
/// it hands the run's serial end ([`finish_crate`]).
#[derive(Default)]
struct CrateScratch {
    reads: Reads,
    done: HashSet<ImplId, Global>,
    kinds: HashMap<ImplId, bool, Global>,
    kinds_of: HashSet<ModuleId, Global>,
    woken: Vec<ImplId>,
    trait_keys: Vec<TraitId>,
    head_keys: Vec<Head>,
    name_keys: Vec<Name>,
    redo_traits: Vec<TraitId>,
    assoc: SmallVec<[Name; 8]>,
    body_modules: Vec<ModuleId>,
    units: Vec<()>,
    keys_changed: Vec<ImplId>,
    traits_changed: Vec<TraitId>,
    /// What the work did to the state shared across crates, in order: replayed by
    /// [`finish_crate`]. A `Note`'s names and defs are the next ones of `read_names` and
    /// `read_defs`.
    index: Vec<IndexOp>,
    read_names: Vec<Name>,
    read_defs: Vec<ModuleDefId>,
    /// The slices whose rows the work made, set by [`finish_crate`] unless they hold them.
    trait_rows: Vec<(TraitId, TraitRows)>,
    head_rows: Vec<(Head, InherentRows)>,
    name_rows: Vec<(Name, CandidateRows)>,
    /// The directory's body modules, when the work read them again.
    inherent_bodies: Option<Vec<ModuleId>>,
}

impl CrateScratch {
    /// Empties what one run's work fills.
    fn start(&mut self) {
        self.done.clear();
        self.kinds.clear();
        self.kinds_of.clear();
        self.keys_changed.clear();
        self.traits_changed.clear();
        debug_assert!(self.index.is_empty() && self.trait_rows.is_empty());
    }
}

/// A change to the state shared across crates, as a crate's work made it.
#[derive(Clone, Copy)]
enum IndexOp {
    /// `forget_reads`: the impl's read set dropped.
    Forget(ImplId),
    /// The impl is no longer the crate's.
    Disown(ImplId),
    /// The impl is the crate's.
    Own(ImplId),
    /// The impl's read set, `names` names and `defs` defs, and whether it is wild.
    Note { imp: ImplId, names: u32, defs: u32, wild: bool },
}

/// One crate's work in a run: its state out of [`State::crates`] while the run has it.
struct Job {
    krate: Crate,
    /// Its delta, by index into the write's `LastWrite::crates`.
    delta: Option<usize>,
    fresh: bool,
    crate_state: CrateState,
    scratch: CrateScratch,
    work: Work,
}

/// What every crate's work reads that is not its own: fixed at the run's start.
struct Shared<'a> {
    last: &'a LastWrite,
    owner: &'a HashMap<ImplId, Crate, Global>,
    woken: &'a HashSet<ImplId, Global>,
    crates_changed: bool,
    changed: bool,
}

impl State {
    /// Whether `krate` is maintained here.
    #[cfg(test)]
    pub(crate) fn crate_state(&self, krate: Crate) -> Option<&CrateState> {
        self.crates.get(&krate)
    }
}

/// Step 0's apply, before any crate's node: drops the crates gone, and gives each crate
/// maintained for the first time its state and its directory's empty slot, all the slots
/// registered with one write ([`publish::register_slots`]). Reads only inputs.
pub(crate) fn prepare(db: &mut Db, state: &mut State, written: &Written<'_>) {
    let crates = base_db::all_crates(&*db);
    if written.crates {
        drop_gone(state, &crates);
    }
    let mut slots = std::mem::take(&mut state.scratch.slots);
    slots.clear();
    for &krate in crates.iter() {
        if frozen::served(&*db, krate) || state.crates.contains_key(&krate) {
            continue;
        }
        let durability = base_db::crate_durability(&*db, krate);
        let empty = blaze_query::db::Slot::empty(publish::slot_name(&*db, krate));
        let slot = DirectorySlot::new(&*db, durability, krate, empty);
        state.crates.insert(
            krate,
            CrateState { published: Published::unpublished(slot), ..CrateState::default() },
        );
        slots.push((krate, slot));
    }
    {
        let _span =
            tracing::info_span!(target: "blaze", "impls register", count = slots.len()).entered();
        publish::register_slots(db, &slots);
    }
    slots.clear();
    state.scratch.slots = slots;
}

/// The key a crate's cost is learned by: its content ID's first eight bytes, which cover its
/// data and its dependencies', the same from one daemon to the next.
fn cost_key(db: &Db, krate: Crate) -> u64 {
    let id = krate.content(db);
    u64::from_le_bytes(id.0[..8].try_into().expect("eight bytes"))
}

/// Step 1's reads: every crate's work, as one node per crate (see the module), on the database's
/// workers; the jobs kept for [`finish`].
pub(crate) fn run_nodes(db: &Db, state: &mut State, last: &LastWrite, written: &Written<'_>) {
    let crates = base_db::all_crates(db);
    let waking =
        tracing::info_span!(target: "blaze", "impls woken", count = last.crates.len()).entered();
    let scratch = &mut state.scratch;
    scratch.deltas.clear();
    scratch.names.clear();
    scratch.defs.clear();
    scratch.woken.clear();
    for (at, delta) in last.crates.iter().enumerate() {
        scratch.deltas.insert(delta.krate, at);
        scratch.names.extend(delta.bindings.iter().map(|&(_, name)| name));
        scratch.defs.extend(delta.touched.iter().copied());
        scratch.woken.extend(delta.touched_impls.iter().copied());
    }
    for name in &scratch.names {
        scratch.woken.extend(state.by_name.readers(*name));
    }
    for def in &scratch.defs {
        scratch.woken.extend(state.by_def.readers(*def));
    }
    let changed = !scratch.names.is_empty() || !scratch.defs.is_empty();
    if changed {
        scratch.woken.extend(state.wild.iter().copied());
    }
    drop(waking);
    let mut jobs = std::mem::take(&mut state.scratch.jobs);
    jobs.clear();
    {
        let _span =
            tracing::info_span!(target: "blaze", "impls start", count = crates.len()).entered();
        let mut reset_impls = std::mem::take(&mut state.scratch.reset);
        reset_impls.clear();
        for &krate in crates.iter() {
            if frozen::served(db, krate) {
                continue;
            }
            let delta = state.scratch.deltas.get(&krate).copied();
            jobs.push(start_crate(state, last, krate, delta, &mut reset_impls));
        }
        forget_reset(state, &reset_impls);
        reset_impls.clear();
        state.scratch.reset = reset_impls;
    }
    let _span = tracing::info_span!(target: "blaze", "impls nodes", count = jobs.len()).entered();
    // alloc: once per pass, a node's index and cost key per crate maintained
    let index: HashMap<Crate, u32, Global> =
        jobs.iter().enumerate().map(|(at, job)| (job.krate, at as u32)).collect();
    let keys: Vec<u64> = jobs.iter().map(|job| cost_key(db, job.krate)).collect(); // alloc: as above
    let predicted: Vec<u64> = keys.iter().map(|&key| state.costs.predict(key)).collect(); // alloc: as above
    let (by_crate, of) = (&index, &jobs);
    let plan = Plan::new(&predicted, move |node| {
        let krate = of[node as usize].krate;
        krate
            .data(db)
            .dependencies
            .iter()
            .filter_map(move |dep| by_crate.get(&dep.crate_id).copied())
    })
    .unwrap_or_else(|error| {
        let krate = jobs[error.node() as usize].krate;
        panic!("impls: the crates' plan refused crate {krate:?}: {error}")
    });
    let nodes = Nodes {
        // alloc: once per pass, a node's slot, job and run time per crate maintained
        slots: jobs.iter().map(|job| job.crate_state.published.unfilled()).collect(),
        jobs: jobs.drain(..).map(|job| Lock::new("ty.impls_pass.job", Some(job))).collect(),
        ran_ns: predicted.iter().map(|_| AtomicU64::new(0)).collect(),
        predicted: &predicted,
        plan: Lock::new("ty.impls_pass.plan", plan),
        failed: Lock::new("ty.impls_pass.failed", None),
    };
    let ran = {
        let shared = Shared {
            last,
            owner: &state.owner,
            woken: &state.scratch.woken,
            crates_changed: written.crates,
            changed,
        };
        let mut order = nodes.plan.lock().order().to_vec(); // alloc: once per pass, the nodes' order
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.run_each(Tag::Infer, &mut order, |db, &mut node| {
                run_node(db, &shared, &nodes, node)
            });
        }))
    };
    // A node that could not run again is the cause of whatever followed it (its dependents'
    // reads of its slot, abandoned): named first (rule 20). Before the pass fails, each slot it
    // left unfilled is abandoned, so no reader gives way for a rerun that won't come.
    if let Some(failed) = nodes.failed.lock().take() {
        abandon_unfilled(db, &nodes);
        panic!("impls: {}", failed.say(db));
    }
    if let Err(panic) = ran {
        abandon_unfilled(db, &nodes);
        std::panic::resume_unwind(panic);
    }
    let outcome = nodes.plan.lock().outcome();
    if let Err(error) = outcome {
        // The order holds every node, a node given back is begun again by the next task that
        // begins it (a dependent's during the loop, or the loop's rerun of the node after it),
        // and each runs once: one not done is a bug of the driver.
        abandon_unfilled(db, &nodes);
        let krate = nodes.jobs[error.node() as usize].lock().as_ref().map(|job| job.krate);
        panic!("impls: the crates' nodes did not all run, crate {krate:?}: {error}");
    }
    let Nodes { jobs: done, ran_ns, .. } = nodes;
    for (node, slot) in done.into_vec().into_iter().enumerate() {
        jobs.push(slot.into_inner().expect("every node put its job back"));
        state.costs.observe(keys[node], ran_ns[node].load(Ordering::Relaxed));
    }
    state.costs.keep_only(&keys);
    state.scratch.jobs = jobs;
}

/// Every slot of the pass not filled, marked abandoned ([`blaze_query::db::Slot::abandon`]):
/// the pass is failing, so no rerun will fill one given back and no node will claim one never
/// claimed. A reader reads [`SlotError::Abandoned`] naming it at once, never gives way and is
/// posted again without a bound (rule 18). Called once every task of the loop has ended, so no
/// task holds a claim.
fn abandon_unfilled(db: &Db, nodes: &Nodes<'_>) {
    #[cfg(test)]
    let before = tests_read_slots(db, nodes);
    for slot in nodes.slots.iter().flatten() {
        slot.slot(db).abandon();
    }
    #[cfg(test)]
    tests_record_failed_pass(before, tests_read_slots(db, nodes));
}

/// What a reader of a slot gets: tests only, as a failed pass leaves it ([`FAILED_PASS`]).
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SlotRead {
    Full,
    Error(SlotError),
    /// The read gave way, as for a claim given back.
    GaveWay,
}

#[cfg(test)]
thread_local! {
    /// Tests only: each slot of the last pass that failed on this thread, by crate name, as a
    /// reader read it before and after the pass abandoned the slots it left unfilled.
    pub(super) static FAILED_PASS: std::cell::RefCell<Vec<(Box<str>, SlotRead, SlotRead)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Each slot of the pass as a reader reads it, by its crate's name.
#[cfg(test)]
fn tests_read_slots(db: &Db, nodes: &Nodes<'_>) -> Vec<(Box<str>, SlotRead)> {
    nodes
        .slots
        .iter()
        .flatten()
        .map(|slot| {
            let name = slot.krate(db).named(db).to_string();
            let read = Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                slot.slot(db).get(db).map(|_| ())
            }));
            let read = match read {
                Ok(Ok(())) => SlotRead::Full,
                Ok(Err(error)) => SlotRead::Error(error),
                Err(_) => SlotRead::GaveWay,
            };
            (name.into_boxed_str(), read)
        })
        .collect()
}

/// Ends before any wait (rule 27).
#[cfg(test)]
#[inline(never)]
fn tests_record_failed_pass(before: Vec<(Box<str>, SlotRead)>, after: Vec<(Box<str>, SlotRead)>) {
    let reads = before
        .into_iter()
        .zip(after)
        .map(|((name, before), (_, after))| (name, before, after))
        .collect();
    FAILED_PASS.with(|it| *it.borrow_mut() = reads);
}

/// One pass's nodes, shared by the workers that run them.
struct Nodes<'a> {
    /// What a node may begin, decided under this lock alone, held for the decision and the
    /// claims of the slots of the nodes begun, never across a query.
    plan: Lock<Plan>,
    /// Each node's job, out of its slot while it runs: no lock is held across a query.
    jobs: Box<[Lock<Option<Job>>]>,
    /// Each node's slot to fill, for a crate first maintained.
    slots: Box<[Option<DirectorySlot>]>,
    predicted: &'a [u64],
    /// What each node took, measured by the driver: what the plan learns from.
    ran_ns: Box<[AtomicU64]>,
    /// The first node that could not run again: the pass fails naming it.
    failed: Lock<Option<NodeFailed>>,
}

/// Why a crate's node ended the pass.
enum NodeFailed {
    /// Its wait gave way (`cancelled`) after its work began changing the crate's state in
    /// place: run again, it would start from that half-changed state.
    GaveWayChanging { krate: Crate, cancelled: Cancelled },
    /// Its slot could not be claimed as it began.
    Claim { krate: Crate, error: SlotError },
}

impl NodeFailed {
    /// What the pass's failure says, once.
    fn say(&self, db: &Db) -> String {
        // alloc: a failed pass's message, once
        match self {
            NodeFailed::GaveWayChanging { krate, cancelled } => format!(
                "crate `{}` ({krate:?})'s node gave way ({cancelled}) after its work began changing the \
                 crate's impl tables in place, so it can't run again from where it began: the pass \
                 fails, and the next write maintains from empty",
                krate.named(db),
            ),
            NodeFailed::Claim { krate, error } => format!(
                "crate `{}` ({krate:?})'s node could not claim its directory as it began: {error}",
                krate.named(db),
            ),
        }
    }
}

/// Runs `node` unless another task began it: first, on this stack, every node under it no task
/// began (begun with it, deps first), so that every slot it reads, of any crate under it, is
/// filled or claimed by a running task.
fn run_node<'a>(db: &'a Db, shared: &Shared<'_>, nodes: &'a Nodes<'a>, node: u32) {
    // Declared before the plan's lock is taken: dropped (on an unwind) only after it is let go.
    let gave = blaze_graph::pump::gave_way();
    let mut begun =
        Begun { db, nodes, entries: SmallVec::new(), ran: 0, job: None, changing: false, gave };
    let refused = {
        let mut plan = nodes.plan.lock();
        let Begin::Run { first } = plan.begin(node) else { return };
        // Claimed under the plan's lock: a node begins only once every node under it has, so
        // its read of any of their slots finds the slot claimed, and waits on its task.
        let mut refused = None;
        for at in first.into_iter().chain([node]) {
            let claim = nodes.slots[at as usize].and_then(|slot| match slot.slot(db).claim(db) {
                Ok(claim) => Some(claim),
                Err(error) => {
                    refused.get_or_insert(NodeFailed::Claim { krate: slot.krate(db), error });
                    None
                }
            });
            begun.entries.push((at, claim));
        }
        refused
    };
    if let Some(refused) = refused {
        // No give-way: as this unwinds `begun` abandons every claim it took, and the pass
        // fails naming the crate.
        panic!("impls: {}", refused.say(db));
    }
    begun.run(shared);
}

/// The nodes one task began together, deps first and the node it asked for last, each with its
/// slot's claim: run in turn on this stack. Dropped before all ran (an unwind), it puts back
/// what lets the loop run them again ([`Plan::give_back`], [`SlotClaim::give_back`]) when the
/// loop will (a give-way before the running node's work began changing its state); on any
/// other unwind it abandons their claims, as the pass fails.
struct Begun<'a> {
    db: &'a Db,
    nodes: &'a Nodes<'a>,
    entries: SmallVec<[(u32, Option<SlotClaim<'a, SliceDirectory>>); 4]>,
    /// How many of `entries` have run.
    ran: usize,
    /// The running node's job, out of its slot.
    job: Option<Job>,
    /// Whether the running node's work has begun changing its job's state, which [`warm`]
    /// does not (it reads memos alone).
    changing: bool,
    /// Its task's give-ways ([`blaze_graph::pump::gave_way`]) as it began. One counted since,
    /// when it unwinds, is what makes its loop run its item again (`blaze_graph::par`'s rule
    /// on every thread, and the test database's): a claim is given back for that alone.
    gave: u64,
}

impl Begun<'_> {
    fn run(&mut self, shared: &Shared<'_>) {
        while self.ran < self.entries.len() {
            let node = self.entries[self.ran].0;
            self.changing = false;
            self.job =
                Some(self.nodes.jobs[node as usize].lock().take().expect("a node's job runs once"));
            let started = Instant::now();
            {
                let db = self.db;
                let job = self.job.as_mut().expect("taken above");
                let _span = hir_def::span_names::with_crate(
                    db,
                    job.krate,
                    tracing::info_span!(
                        target: "blaze",
                        "impls node",
                        krate = tracing::field::Empty,
                        predicted_ns = self.nodes.predicted[node as usize],
                    ),
                )
                .entered();
                #[cfg(test)]
                tests_give_way(db, job.krate, GiveWayIn::Warm);
                warm(db, shared, job);
                self.changing = true;
                let claim = self.entries[self.ran].1.take();
                let changed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(debug_assertions)]
                    let _working = Working::enter();
                    #[cfg(test)]
                    tests_give_way(db, job.krate, GiveWayIn::Work);
                    work_crate(db, shared, job);
                    if let Some(claim) = claim {
                        publish_new(db, job, claim);
                    }
                }));
                if let Err(payload) = changed {
                    // A give-way here can't be run again; the loop reruns the item all the
                    // same (it gave way), finds the node begun and returns, and the pass names
                    // it once the loop is done.
                    if let Some(&cancelled @ Cancelled::WouldWaitOnItself { .. }) =
                        payload.downcast_ref::<Cancelled>()
                    {
                        let krate = job.krate;
                        self.nodes
                            .failed
                            .lock()
                            .get_or_insert(NodeFailed::GaveWayChanging { krate, cancelled });
                    }
                    std::panic::resume_unwind(payload);
                }
            }
            let ran = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.nodes.ran_ns[node as usize].store(ran, Ordering::Relaxed);
            let job = self.job.take().expect("its job, until put back");
            let krate = job.krate;
            *self.nodes.jobs[node as usize].lock() = Some(job);
            self.ran += 1;
            self.changing = false;
            let ended = self.nodes.plan.lock().end(node);
            if let Err(error) = ended {
                panic!("impls: crate {krate:?}'s node: {error}");
            }
        }
    }
}

impl Drop for Begun<'_> {
    fn drop(&mut self) {
        if self.ran == self.entries.len() {
            return;
        }
        // Unwinding, the node running (`ran`) and those after it not run. Its loop runs this
        // task again only when it gave way, and the rerun only helps when the running node's
        // work had not begun changing its job: then each goes back to waiting, its job in its
        // slot and its claim given back, for the next task that begins it (a dependent's,
        // during the loop, or the loop's rerun after it). Any other unwind (a panic, a claim
        // refused, a give-way mid-work, recorded in `failed`) ends the pass: each claim is
        // dropped, so its readers read `Abandoned` rather than give way for a rerun that will
        // never come, and the nodes stay running, so no task begins them again meanwhile. The
        // job of a node whose work began changing it is dropped half changed.
        let rerun = !self.changing && blaze_graph::pump::gave_way() != self.gave;
        if let Some(job) = self.job.take()
            && !self.changing
        {
            *self.nodes.jobs[self.entries[self.ran].0 as usize].lock() = Some(job);
        }
        if !rerun {
            for (_, claim) in self.entries.iter_mut().skip(self.ran) {
                drop(claim.take());
            }
            return;
        }
        let mut plan = self.nodes.plan.lock();
        for (node, claim) in self.entries.iter_mut().skip(self.ran) {
            if let Some(claim) = claim.take() {
                claim.give_back();
            }
            // Each entry was begun with this task and not ended: running, so given back.
            let given = plan.give_back(*node);
            debug_assert!(given.is_ok(), "an entry of the nodes begun is running: {given:?}");
        }
    }
}

/// Debug builds: the jobs whose node's work has begun changing its state, each as its id + 1 (0
/// a free entry), for [`assert_slot_read_in_warm`]. A pass runs at most a worker's count of
/// nodes at once; past this many, a node is not tracked (the check is a debug aid, never a
/// rule a build relies on).
#[cfg(debug_assertions)]
static WORKING: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

/// Debug builds: the running job's entry in [`WORKING`] while its node's work changes its
/// state; let go of as it ends or unwinds. Tied to the job, not the thread, so it holds
/// across a wait that resumes the job elsewhere (rule 27).
#[cfg(debug_assertions)]
struct Working(Option<&'static AtomicU64>);

#[cfg(debug_assertions)]
impl Working {
    fn enter() -> Working {
        let me = u64::from(blaze_graph::sched::job_here().0) + 1;
        Working(
            WORKING.iter().find(|it| {
                it.compare_exchange(0, me, Ordering::Relaxed, Ordering::Relaxed).is_ok()
            }),
        )
    }
}

#[cfg(debug_assertions)]
impl Drop for Working {
    fn drop(&mut self) {
        if let Some(entry) = self.0 {
            entry.store(0, Ordering::Relaxed);
        }
    }
}

/// Debug builds: every read of a directory slot is in a node's [`warm`] or outside the nodes,
/// never in a node's work once it began changing its state. There a read of a slot given back
/// gives way, and the node, half changed, can't run again: the pass fails
/// ([`NodeFailed::GaveWayChanging`]) where in `warm` it would have run again. Panics naming
/// `krate`, whose slot the read was of.
#[cfg(debug_assertions)]
pub(super) fn assert_slot_read_in_warm(db: &Db, krate: Crate) {
    let me = u64::from(blaze_graph::sched::job_here().0) + 1;
    assert!(
        !WORKING.iter().any(|it| it.load(Ordering::Relaxed) == me),
        "impls: crate `{}` ({krate:?})'s directory slot was read by a node whose work had begun \
         changing its crate's state: every slot read of a node is in its warm, where a read that \
         gives way runs the node again",
        krate.named(db),
    );
}

/// Where a test makes a node's wait give way ([`GIVE_WAY`]).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GiveWayIn {
    /// Before it changed anything: in its warm.
    Warm,
    /// Once its work began changing its state.
    Work,
    /// In its warm, as [`GiveWayIn::Warm`], but a panic of its own, not a give-way: the
    /// loop does not run it again, and the pass fails.
    PanicInWarm,
}

#[cfg(test)]
thread_local! {
    /// The crate (by name) whose node a test makes give way, once, and where: as a wait that
    /// would close a cycle does. Read only by [`tests_give_way`], which ends before any wait.
    pub(super) static GIVE_WAY: std::cell::Cell<Option<(&'static str, GiveWayIn)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[inline(never)]
fn tests_give_way(db: &Db, krate: Crate, at: GiveWayIn) {
    let wanted = GIVE_WAY.with(|it| it.get());
    let Some((name, here)) = wanted else { return };
    let fires = here == at || (here == GiveWayIn::PanicInWarm && at == GiveWayIn::Warm);
    if !fires || !krate.rust_name(db).is_ok_and(|it| it.as_str() == name) {
        return;
    }
    GIVE_WAY.with(|it| it.set(None));
    if here == GiveWayIn::PanicInWarm {
        panic!("test: crate `{name}`'s node panics in its warm");
    }
    _ = blaze_graph::pump::give_way();
    Cancelled::WouldWaitOnItself { asked: None }.throw();
}

/// A crate's first publication, in its node: its slices and directory made by `&`, and the
/// directory filled into its slot, which lets every reader waiting on it go on.
fn publish_new(db: &Db, job: &mut Job, claim: SlotClaim<'_, SliceDirectory>) {
    let Job { krate, crate_state, scratch, work, .. } = job;
    let _span = tracing::info_span!(target: "blaze", "impls publish new").entered();
    let (made, directory) = crate_state.published.make(
        db,
        *krate,
        &mut scratch.trait_rows,
        &mut scratch.head_rows,
        scratch.inherent_bodies.take(),
        &mut scratch.name_rows,
    );
    work.slices += made;
    if let Err(refused) = claim.fill(directory) {
        panic!("impls: crate {krate:?}: {refused}");
    }
}

/// Step 1's apply: each crate's work ended in the crates' order ([`finish_crate`]); what
/// changed of each, into `out`.
pub(crate) fn finish(db: &mut Db, state: &mut State, out: &mut Vec<ImplsDelta>) {
    let mut jobs = std::mem::take(&mut state.scratch.jobs);
    for job in jobs.drain(..) {
        finish_crate(db, state, job, out);
    }
    state.scratch.jobs = jobs;
}

/// Takes `krate`'s state out for its work, forgetting what it held first when name resolution
/// maintains it from empty (`fresh`): its impls go on `reset_impls`, whose read sets and owners
/// the caller forgets once every crate's job is out ([`forget_reset`]).
fn start_crate(
    state: &mut State,
    last: &LastWrite,
    krate: Crate,
    delta: Option<usize>,
    reset_impls: &mut Vec<ImplId>,
) -> Job {
    let fresh = delta.is_some_and(|at| last.crates[at].fresh);
    let Some(mut crate_state) = state.crates.remove(&krate) else {
        panic!("impls: crate {krate:?} has no state: step 0 ([`prepare`]) gives every crate one");
    };
    if !crate_state.published.is_published() && !fresh {
        panic!(
            "impls: crate {krate:?} has no maintained impl tables and name resolution handed it no \
             fresh delta"
        );
    }
    let mut scratch = std::mem::take(&mut crate_state.scratch);
    scratch.start();
    if fresh {
        crate_state = reset(crate_state, &mut scratch, reset_impls);
        let (traits, heads, names) = crate_state.published.keys();
        scratch.trait_keys.extend(traits);
        scratch.head_keys.extend(heads);
        scratch.name_keys.extend(names);
    }
    Job { krate, delta, fresh, crate_state, scratch, work: Work::default() }
}

/// Drops the state of every crate no longer in the graph, with its impls' read sets.
fn drop_gone(state: &mut State, crates: &[Crate]) {
    // alloc: a crate graph change, once
    let live: HashSet<Crate, Global> = crates.iter().copied().collect();
    let gone: Vec<Crate> = state.crates.keys().copied().filter(|it| !live.contains(it)).collect(); // alloc: as above
    for krate in gone {
        let Some(crate_state) = state.crates.remove(&krate) else { continue };
        for imp in crate_state.impls.keys() {
            forget_reads(state, *imp);
            state.owner.remove(imp);
        }
    }
}

/// Forgets the read sets and owners of the impls of every crate the pass reset, at once: a proc
/// macro or build script reaching most crates resets most impls, and their read sets are most
/// of the index, which a sweep drops in one pass ([`ReadIndex::forget_many`]) on the loop's
/// thread while no crate's node runs yet. Nothing between the resets and here reads the index
/// or the owners: the impls the write woke were read from them first.
fn forget_reset(state: &mut State, impls: &[ImplId]) {
    if impls.is_empty() {
        return;
    }
    let _span = tracing::info_span!(target: "blaze", "impls reset", count = impls.len()).entered();
    state.by_name.forget_many(impls.iter().copied());
    state.by_def.forget_many(impls.iter().copied());
    for imp in impls {
        state.wild.remove(imp);
        state.owner.remove(imp);
    }
}

fn forget_reads(state: &mut State, imp: ImplId) {
    state.by_name.forget(imp);
    state.by_def.forget(imp);
    state.wild.remove(&imp);
}

/// One crate's work: its delta applied, its woken impls rekeyed, its traits' names and its
/// bodies read again, and the rows of the slices that changed made. It reads memos and its own
/// state alone, so the crates' works run at once.
fn work_crate(db: &Db, shared: &Shared<'_>, job: &mut Job) {
    let krate = job.krate;
    let _span = hir_def::span_names::with_crate(
        db,
        krate,
        tracing::info_span!(target: "blaze", "impls crate", krate = tracing::field::Empty),
    )
    .entered();
    macro_rules! phase {
        ($name:literal, $run:expr) => {{
            let _phase = tracing::info_span!(target: "blaze", $name).entered();
            $run
        }};
    }
    let Job { delta, fresh, crate_state, scratch, work, .. } = job;
    let (fresh, crates_changed, changed) = (*fresh, shared.crates_changed, shared.changed);
    let delta = delta.map(|at| &shared.last.crates[at]);
    if let Some(delta) = delta {
        phase!("impls apply_impls", apply_impls(db, scratch, crate_state, delta, work));
        phase!("impls apply_derives", apply_derives(db, crate_state, delta, work));
        phase!("impls apply_traits", apply_traits(db, scratch, crate_state, delta, work));
    }
    phase!("impls rekey", rekey(db, shared, scratch, crate_state, krate, crates_changed, work));
    if crates_changed || changed {
        phase!("impls rename_wild_traits", rename_wild_traits(db, scratch, crate_state, work));
    }
    let lang_check = fresh || crates_changed || delta.is_some_and(touches_traits);
    phase!("impls relang", relang(db, crate_state, krate, lang_check, work));
    let sweep = fresh || crates_changed;
    if sweep || changed || delta.is_some() {
        apply_bodies(db, scratch, crate_state, krate, delta, sweep, work);
    }
    phase!("impls rows", rows(scratch, crate_state, fresh));
}

/// The serial end of one crate's work, in the crates' order: what it changed of the shared
/// state replayed, its slices set (a crate published before; a crate's first publication is
/// its node's), its state put back, its delta handed on and its work reported.
fn finish_crate(db: &mut Db, state: &mut State, job: Job, out: &mut Vec<ImplsDelta>) {
    let Job { krate, mut crate_state, mut scratch, mut work, .. } = job;
    let _span = hir_def::span_names::with_crate(
        &*db,
        krate,
        tracing::info_span!(target: "blaze", "impls publish", krate = tracing::field::Empty),
    )
    .entered();
    replay(state, &mut scratch, krate);
    publish(db, &mut scratch, &mut crate_state, krate, &mut work);
    if !scratch.keys_changed.is_empty() || !scratch.traits_changed.is_empty() {
        scratch.keys_changed.sort_unstable();
        scratch.keys_changed.dedup();
        scratch.traits_changed.sort_unstable();
        scratch.traits_changed.dedup();
        out.push(ImplsDelta {
            krate,
            // alloc: the columns move into the delta; the next write's grow anew
            keys_changed: std::mem::take(&mut scratch.keys_changed),
            traits_changed: std::mem::take(&mut scratch.traits_changed),
        });
    }
    crate_state.scratch = scratch;
    state.crates.insert(krate, crate_state);
    db.report_work(NAME, krate, work);
}

/// Applies a crate's changes to the state shared across crates, in the order its work made
/// them.
fn replay(state: &mut State, scratch: &mut CrateScratch, krate: Crate) {
    let (mut names, mut defs) = (0, 0);
    for op in scratch.index.drain(..) {
        match op {
            IndexOp::Forget(imp) => forget_reads(state, imp),
            IndexOp::Disown(imp) => _ = state.owner.remove(&imp),
            IndexOp::Own(imp) => _ = state.owner.insert(imp, krate),
            IndexOp::Note { imp, names: n, defs: d, wild } => {
                let (n, d) = (n as usize, d as usize);
                for &name in &scratch.read_names[names..names + n] {
                    state.by_name.add(name, imp);
                }
                for &def in &scratch.read_defs[defs..defs + d] {
                    state.by_def.add(def, imp);
                }
                if wild {
                    state.wild.insert(imp);
                }
                names += n;
                defs += d;
            }
        }
    }
    scratch.read_names.clear();
    scratch.read_defs.clear();
}

/// Forgets what a crate held, keeping its published inputs and buffers: a crate maintained
/// from empty sets every slice it had anew, emptied where it has no rows now. Its impls go on
/// `reset_impls`, for [`forget_reset`].
fn reset(old: CrateState, scratch: &mut CrateScratch, reset_impls: &mut Vec<ImplId>) -> CrateState {
    reset_impls.extend(old.impls.keys().copied());
    scratch.keys_changed.extend(old.impls.keys().copied());
    scratch.traits_changed.extend(old.assoc.keys().copied());
    CrateState { published: old.published, ..CrateState::default() }
}

fn touches_traits(delta: &CrateDelta) -> bool {
    !delta.traits.is_empty() || delta.touched.iter().any(|it| matches!(it, ModuleDefId::TraitId(_)))
}

/// Whether `imp` is a trait impl, by its module's scope: the split the tables take.
fn is_trait_impl(db: &Db, scratch: &mut CrateScratch, module: ModuleId, imp: ImplId) -> bool {
    if scratch.kinds_of.insert(module) {
        let impls = module_impls(db, module);
        scratch.kinds.extend(impls.trait_impls.iter().map(|&it| (it, true)));
        scratch.kinds.extend(impls.inherent_impls.iter().map(|&it| (it, false)));
    }
    *scratch
        .kinds
        .get(&imp)
        .unwrap_or_else(|| panic!("impls: {imp:?} is not among the impls of its module {module:?}"))
}

fn apply_impls(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    delta: &CrateDelta,
    work: &mut Work,
) {
    for (imp, pos, _) in delta.impls.iter().filter(|it| it.2 < 0) {
        work.ops += 1;
        if pos_of(crate_state, *imp).is_some_and(|it| it == pos)
            && remove_impl(scratch, crate_state, *imp)
        {
            scratch.keys_changed.push(*imp);
        }
    }
    for (imp, pos, _) in delta.impls.iter().filter(|it| it.2 > 0) {
        work.ops += 1;
        remove_impl(scratch, crate_state, *imp);
        scratch.keys_changed.push(*imp);
        let module = imp.loc(db).container;
        let trait_impl = is_trait_impl(db, scratch, module, *imp);
        let key = impl_key(db, *imp, trait_impl);
        work.rebuilt += 1;
        work.keys += 1;
        // clone: a position, inline to a depth of six
        let row = ImplRow { module, pos: pos.clone(), key, trait_impl };
        arrange(crate_state, *imp, &row, 1);
        crate_state.impls.insert(*imp, row);
        scratch.index.push(IndexOp::Own(*imp));
        note_reads(db, scratch, *imp);
        scratch.done.insert(*imp);
    }
}

fn pos_of(crate_state: &CrateState, imp: ImplId) -> Option<&Pos> {
    crate_state.impls.get(&imp).map(|row| &row.pos)
}

/// Drops `imp`'s row and read set; whether it had one.
fn remove_impl(scratch: &mut CrateScratch, crate_state: &mut CrateState, imp: ImplId) -> bool {
    let Some(row) = crate_state.impls.remove(&imp) else { return false };
    arrange(crate_state, imp, &row, -1);
    scratch.index.push(IndexOp::Forget(imp));
    scratch.index.push(IndexOp::Disown(imp));
    true
}

/// Adds `row` to the arrangements its key is in, with `weight`.
fn arrange(crate_state: &mut CrateState, imp: ImplId, row: &ImplRow, weight: i32) {
    match row.key {
        ImplKey::Skip => {}
        ImplKey::Trait { trait_, head } => crate_state.traits.update(
            trait_,
            // clone: a position, inline to a depth of six
            row.pos.clone(),
            TraitEntry::Impl(row.module, imp, head),
            weight,
        ),
        ImplKey::Inherent(head) => {
            // clone: a position, inline to a depth of six
            crate_state.heads.update(head, row.pos.clone(), (row.module, imp), weight)
        }
    }
}

fn arrange_derive(
    crate_state: &mut CrateState,
    imp: BuiltinDeriveImplId,
    row: &DeriveRow,
    weight: i32,
) {
    if let Some((trait_, head)) = row.key {
        let entry = TraitEntry::Derive(row.module, imp, head);
        // clone: a position, inline to a depth of six
        crate_state.traits.update(trait_, row.pos.clone(), entry, weight);
    }
}

/// Notes what `imp`'s key read, for the read indexes ([`replay`]).
fn note_reads(db: &Db, scratch: &mut CrateScratch, imp: ImplId) {
    let reads = &mut scratch.reads;
    impl_reads(db, imp, reads);
    scratch.read_names.extend_from_slice(&reads.names);
    scratch.read_defs.extend_from_slice(&reads.defs);
    scratch.index.push(IndexOp::Note {
        imp,
        names: reads.names.len() as u32,
        defs: reads.defs.len() as u32,
        wild: reads.wild,
    });
}

/// Recomputes the key of every impl of `krate` the write woke (all of them when the crate
/// graph changed): a changed key moves its row. Which impls are `krate`'s is as the run found
/// them at its start: one this write added is keyed already (`done`), and one it removed has no
/// row.
fn rekey(
    db: &Db,
    shared: &Shared<'_>,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    krate: Crate,
    all: bool,
    work: &mut Work,
) {
    let mut woken = std::mem::take(&mut scratch.woken);
    woken.clear();
    if all {
        woken.extend(crate_state.impls.keys().copied());
    } else {
        let owner = shared.owner;
        woken.extend(shared.woken.iter().copied().filter(|imp| owner.get(imp) == Some(&krate)));
    }
    for &imp in &woken {
        if !scratch.done.insert(imp) {
            continue;
        }
        let Some(row) = crate_state.impls.get(&imp) else { continue };
        work.woken += 1;
        work.rebuilt += 1;
        let trait_impl = is_trait_impl(db, scratch, row.module, imp);
        let key = impl_key(db, imp, trait_impl);
        scratch.index.push(IndexOp::Forget(imp));
        note_reads(db, scratch, imp);
        let row = crate_state.impls.remove(&imp).expect("the row just read");
        if key == row.key && trait_impl == row.trait_impl {
            crate_state.impls.insert(imp, row);
            continue;
        }
        work.keys += 1;
        scratch.keys_changed.push(imp);
        arrange(crate_state, imp, &row, -1);
        let row = ImplRow { key, trait_impl, ..row };
        arrange(crate_state, imp, &row, 1);
        crate_state.impls.insert(imp, row);
    }
    scratch.woken = woken;
}

fn apply_derives(db: &Db, crate_state: &mut CrateState, delta: &CrateDelta, work: &mut Work) {
    for (imp, pos, _) in delta.derive_impls.iter().filter(|it| it.2 < 0) {
        work.ops += 1;
        if crate_state.derives.get(imp).is_some_and(|row| row.pos == *pos)
            && let Some(row) = crate_state.derives.remove(imp)
        {
            arrange_derive(crate_state, *imp, &row, -1);
        }
    }
    for (imp, pos, _) in delta.derive_impls.iter().filter(|it| it.2 > 0) {
        work.ops += 1;
        if let Some(row) = crate_state.derives.remove(imp) {
            arrange_derive(crate_state, *imp, &row, -1);
        }
        let module = derive_module(db, *imp);
        let krate = module.krate(db);
        let key = derive_key(db, *imp, lang_items(db, krate));
        work.rebuilt += 1;
        work.keys += 1;
        // clone: a position, inline to a depth of six
        let row = DeriveRow { module, pos: pos.clone(), key };
        arrange_derive(crate_state, *imp, &row, 1);
        crate_state.derives.insert(*imp, row);
    }
}

/// Re-reads the traits the lang items give the built-in derives when a write may have changed
/// them (a trait declared, gone or re-read, or the crate graph); a changed one rekeys the
/// derive impls of that derive.
fn relang(db: &Db, crate_state: &mut CrateState, krate: Crate, check: bool, work: &mut Work) {
    if crate_state.derives.is_empty() || !(check || crate_state.lang.is_none()) {
        return;
    }
    let items = lang_items(db, krate);
    let now: SmallVec<[Option<TraitId>; 16]> =
        BuiltinDeriveImplTrait::ALL.iter().map(|it| it.get_id(items)).collect(); // alloc: inline
    if crate_state.lang.as_ref() == Some(&now) {
        return;
    }
    crate_state.lang = Some(now);
    // alloc: the derive impls of a crate whose lang items changed, once per such write
    let derives: Vec<BuiltinDeriveImplId> = crate_state.derives.keys().copied().collect();
    for imp in derives {
        let key = derive_key(db, imp, items);
        let row = crate_state.derives.remove(&imp).expect("a derive impl just listed");
        work.rebuilt += 1;
        if key == row.key {
            crate_state.derives.insert(imp, row);
            continue;
        }
        work.keys += 1;
        arrange_derive(crate_state, imp, &row, -1);
        let row = DeriveRow { key, ..row };
        arrange_derive(crate_state, imp, &row, 1);
        crate_state.derives.insert(imp, row);
    }
}

/// `AssocFn` (design §8): the item names of each trait declared, gone or re-read.
fn apply_traits(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    delta: &CrateDelta,
    work: &mut Work,
) {
    for &(trait_, weight) in delta.traits.iter() {
        work.ops += 1;
        if weight < 0 {
            set_names(db, scratch, crate_state, trait_, false, work);
        }
    }
    for &(trait_, weight) in delta.traits.iter() {
        if weight > 0 {
            set_names(db, scratch, crate_state, trait_, true, work);
        }
    }
    scratch.redo_traits.clear();
    scratch.redo_traits.extend(delta.touched.iter().filter_map(|it| match *it {
        ModuleDefId::TraitId(it) => Some(it),
        _ => None,
    }));
    let redo = std::mem::take(&mut scratch.redo_traits);
    for &trait_ in &redo {
        if crate_state.assoc.contains_key(&trait_) {
            set_names(db, scratch, crate_state, trait_, true, work);
        }
    }
    scratch.redo_traits = redo;
}

/// The traits whose items a macro writes, re-read on a write that changed any binding or def.
fn rename_wild_traits(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    work: &mut Work,
) {
    if crate_state.wild_traits.is_empty() {
        return;
    }
    scratch.redo_traits.clear();
    scratch.redo_traits.extend(crate_state.wild_traits.iter().copied());
    let redo = std::mem::take(&mut scratch.redo_traits);
    for &trait_ in &redo {
        work.woken += 1;
        set_names(db, scratch, crate_state, trait_, true, work);
    }
    scratch.redo_traits = redo;
}

/// Sets `trait_`'s names to its items' now (`present`) or to none, moving its candidate rows.
fn set_names(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    trait_: TraitId,
    present: bool,
    work: &mut Work,
) {
    let new = &mut scratch.assoc;
    new.clear();
    if present {
        assoc_names(db, trait_, new);
        if trait_.trait_items(db).macro_calls.is_empty() {
            crate_state.wild_traits.remove(&trait_);
        } else {
            crate_state.wild_traits.insert(trait_);
        }
    } else {
        crate_state.wild_traits.remove(&trait_);
    }
    work.rebuilt += 1;
    let old = crate_state.assoc.remove(&trait_).unwrap_or_default();
    if old == *new {
        if present {
            crate_state.assoc.insert(trait_, old);
        }
        return;
    }
    work.keys += 1;
    scratch.traits_changed.push(trait_);
    for &name in old.iter().filter(|it| new.binary_search(it).is_err()) {
        crate_state.candidates.update(name, trait_, trait_, -1);
    }
    for &name in new.iter().filter(|it| old.binary_search(it).is_err()) {
        crate_state.candidates.update(name, trait_, trait_, 1);
    }
    if present {
        crate_state.assoc.insert(trait_, new.clone()); // clone: a trait's names, inline to eight
    }
}

/// The rows of every slice whose arrangement changed, and the directory's body modules when
/// they may have: what [`publish`] sets.
fn rows(scratch: &mut CrateScratch, crate_state: &mut CrateState, fresh: bool) {
    crate_state.traits.take_changed(&mut scratch.trait_keys);
    crate_state.body_traits.take_changed(&mut scratch.trait_keys);
    scratch.trait_keys.sort_unstable();
    scratch.trait_keys.dedup();
    crate_state.heads.take_changed(&mut scratch.head_keys);
    crate_state.body_heads.take_changed(&mut scratch.head_keys);
    scratch.head_keys.sort_unstable();
    scratch.head_keys.dedup();
    crate_state.candidates.take_changed(&mut scratch.name_keys);
    for trait_ in scratch.trait_keys.drain(..) {
        let rows = trait_rows(&crate_state.traits, &crate_state.body_traits, trait_);
        scratch.trait_rows.push((trait_, rows));
    }
    for head in scratch.head_keys.drain(..) {
        let rows = InherentRows {
            impls: crate_state
                .heads
                .rows(head)
                // clone: each row's position, inline to a depth of six
                .map(|(pos, (module, imp), _)| (module, imp, pos.clone()))
                // alloc: a changed slice's content, once at its length
                .collect(),
            // alloc: as above
            bodies: crate_state.body_heads.get(head).collect(),
        };
        scratch.head_rows.push((head, rows));
    }
    crate_state.body_inherent.take_changed(&mut scratch.units);
    if !scratch.units.is_empty() || fresh {
        scratch.units.clear();
        // alloc: the crate's inherent body modules, once per write that may have changed them
        scratch.inherent_bodies = Some(crate_state.body_inherent.get(()).collect());
    }
    for name in scratch.name_keys.drain(..) {
        let rows = CandidateRows {
            // alloc: a changed slice's content, once at its length
            traits: crate_state.candidates.get(name).collect(),
        };
        scratch.name_rows.push((name, rows));
    }
}

/// Sets every slice whose rows changed, then the directory's new keys and body modules.
fn publish(
    db: &mut Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    krate: Crate,
    work: &mut Work,
) {
    let published = &mut crate_state.published;
    for (trait_, rows) in scratch.trait_rows.drain(..) {
        work.slices += u64::from(published.set_trait(db, krate, trait_, rows));
    }
    for (head, rows) in scratch.head_rows.drain(..) {
        work.slices += u64::from(published.set_head(db, krate, head, rows));
    }
    if let Some(modules) = scratch.inherent_bodies.take() {
        published.set_body_modules(&modules);
    }
    for (name, rows) in scratch.name_rows.drain(..) {
        work.slices += u64::from(published.set_name(db, krate, name, rows));
    }
    work.slices += published.finish(db, krate);
}

/// The rows of `trait_` in position order and its body modules, as a slice holds them.
pub(crate) fn trait_rows(
    traits: &Arrangement<TraitId, Pos, TraitEntry>,
    bodies: &Arrangement<TraitId, ModuleId, ModuleId>,
    trait_: TraitId,
) -> TraitRows {
    let (mut impls, mut derives) = (0, 0);
    for entry in traits.get(trait_) {
        match entry {
            TraitEntry::Impl(..) => impls += 1,
            TraitEntry::Derive(..) => derives += 1,
        }
    }
    // alloc: a changed slice's content, each column once at its length
    let mut impl_rows = Vec::with_capacity(impls);
    // alloc: as above
    let mut derive_rows = Vec::with_capacity(derives);
    for entry in traits.get(trait_) {
        match entry {
            TraitEntry::Impl(module, imp, head) => impl_rows.push((module, imp, head)),
            TraitEntry::Derive(module, imp, head) => derive_rows.push((module, imp, head)),
        }
    }
    TraitRows {
        impls: impl_rows.into_boxed_slice(),
        derives: derive_rows.into_boxed_slice(),
        // alloc: as above
        bodies: bodies.get(trait_).collect(),
    }
}

/// The body parts of the modules a write may have changed the bodies of (design §7): the
/// modules of the defs and impls it touched (a file of theirs written, or their log changed)
/// and of its changed bindings (an item a macro writes), and every module whose bodies may hold
/// an impl, whose impls' paths or macros a change elsewhere may move. A `sweep` (the crate from
/// empty, a crate graph change) or a change of which macros may write impls reads every module.
/// A module no longer in the crate's `DefMap` has an empty part.
fn apply_bodies(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    krate: Crate,
    delta: Option<&CrateDelta>,
    sweep: bool,
    work: &mut Work,
) {
    let _span = tracing::info_span!(target: "blaze", "apply_bodies", sweep).entered();
    let def_map = crate_def_map(db, krate);
    let macros = macros_writing_impls(db, krate);
    let mut modules = std::mem::take(&mut scratch.body_modules);
    bodies_to_read(db, crate_state, krate, delta, sweep, &mut modules);
    if crate_state.macros.as_deref() != Some(macros) {
        // alloc: clone: the crate's macro names, once per write that changed them
        crate_state.macros = Some(macros.into());
    }
    // In module order, as the node's warm read them ([`warm`]).
    let empty = BodyPart::default();
    for &module in modules.iter() {
        let now = match def_map.module_rank(db, module) {
            Some(_) => body_part(db, module),
            None => &empty,
        };
        work.woken += 1;
        let old = crate_state.bodies.remove(&module).unwrap_or_default();
        if old == *now {
            if old.holds {
                crate_state.bodies.insert(module, old);
            }
            continue;
        }
        work.keys += 1;
        for (trait_, weight) in moved(&old.traits, &now.traits) {
            crate_state.body_traits.update(trait_, module, module, weight);
        }
        for (head, weight) in moved(&old.heads, &now.heads) {
            crate_state.body_heads.update(head, module, module, weight);
        }
        match (old.heads.is_empty(), now.heads.is_empty()) {
            (true, false) => crate_state.body_inherent.update((), module, module, 1),
            (false, true) => crate_state.body_inherent.update((), module, module, -1),
            _ => {}
        }
        if now.holds {
            // clone: alloc: a changed part, once per module whose part changed
            crate_state.bodies.insert(module, now.clone());
        }
    }
    scratch.body_modules = modules;
}

/// The modules whose body parts `krate`'s work reads ([`apply_bodies`]), sorted, into
/// `modules`: every module on a `sweep` or a change of which macros may write impls, else
/// those of the defs, impls and bindings the delta touched, and every module whose bodies held
/// an impl.
fn bodies_to_read(
    db: &Db,
    crate_state: &CrateState,
    krate: Crate,
    delta: Option<&CrateDelta>,
    sweep: bool,
    modules: &mut Vec<ModuleId>,
) {
    let macros = macros_writing_impls(db, krate);
    modules.clear();
    if sweep || crate_state.macros.as_deref() != Some(macros) {
        modules.extend(crate_def_map(db, krate).modules(db).map(|(module, _)| module));
    } else if let Some(delta) = delta {
        modules.extend(delta.touched.iter().filter_map(|def| def.module(db)));
        modules.extend(delta.touched_impls.iter().map(|imp| imp.loc(db).container));
        modules.extend(delta.bindings.iter().map(|&(module, _)| module));
    }
    modules.extend(crate_state.bodies.keys().copied());
    modules.sort_unstable();
    modules.dedup();
}

/// A key a crate's node reads before its work, across the database's workers: the work goes
/// over its impls, traits and modules one at a time, and so diffs memos.
enum Warm {
    /// What the work reads of the crate: its def map, the macros that may write an impl in a
    /// body, its lang items.
    Crate(Crate),
    /// An impl's kind, key and read set.
    Impl(ImplId),
    /// A built-in derive impl's key.
    Derive(BuiltinDeriveImplId),
    /// A trait's item names.
    Trait(TraitId),
}

/// Reads what the work will read of `warm`.
fn warm_one(db: &Db, warm: &Warm) {
    match *warm {
        Warm::Crate(krate) => {
            _ = crate_def_map(db, krate);
            _ = macros_writing_impls(db, krate);
            _ = lang_items(db, krate);
        }
        Warm::Impl(imp) => {
            // An impl the write removed, or whose module it removed, is woken by what it read and
            // keyed by no work.
            let module = imp.loc(db).container;
            if module.def_map(db).module_rank(db, module).is_none() {
                return;
            }
            let impls = module_impls(db, module);
            let trait_impl = impls.trait_impls.contains(&imp);
            if !trait_impl && !impls.inherent_impls.contains(&imp) {
                return;
            }
            _ = impl_key(db, imp, trait_impl);
            impl_reads(db, imp, &mut Reads::default()); // alloc: an impl's read set, dropped with it
        }
        Warm::Derive(imp) => {
            // As an impl: one the write removed is keyed by no work.
            let module = derive_module(db, imp);
            if module.def_map(db).module_rank(db, module).is_none()
                || !module_impls(db, module).builtin_derive_impls.contains(&imp)
            {
                return;
            }
            _ = derive_key(db, imp, lang_items(db, module.krate(db)));
        }
        Warm::Trait(trait_) => {
            assoc_names(db, trait_, &mut SmallVec::new());
            _ = trait_.trait_items(db);
        }
    }
}

/// Reads, across the database's workers, what `job`'s work will read: first the crate, the
/// impls the write added or woke, the derive impls and traits it may rekey; then the body
/// parts of the modules its work reads, which the crate's def map names. The work after it is
/// one task's, over memos: a crate the size of `core` is not its node's alone to compute.
fn warm(db: &Db, shared: &Shared<'_>, job: &mut Job) {
    let _span = tracing::info_span!(target: "blaze", "impls warm").entered();
    let krate = job.krate;
    let crate_state = &job.crate_state;
    let delta = job.delta.map(|at| &shared.last.crates[at]);
    let mut keys = Vec::new(); // alloc: a crate's keys, once per pass
    keys.push(Warm::Crate(krate));
    if let Some(delta) = delta {
        keys.extend(delta.impls.iter().filter(|it| it.2 > 0).map(|it| Warm::Impl(it.0)));
        keys.extend(delta.derive_impls.iter().filter(|it| it.2 > 0).map(|it| Warm::Derive(it.0)));
        keys.extend(delta.traits.iter().filter(|it| it.1 > 0).map(|it| Warm::Trait(it.0)));
        keys.extend(delta.touched.iter().filter_map(|it| match *it {
            ModuleDefId::TraitId(it) => Some(Warm::Trait(it)),
            _ => None,
        }));
    }
    if shared.crates_changed {
        keys.extend(crate_state.impls.keys().map(|&it| Warm::Impl(it)));
    } else {
        let owner = shared.owner;
        keys.extend(
            shared
                .woken
                .iter()
                .filter(|imp| owner.get(*imp) == Some(&krate))
                .map(|&it| Warm::Impl(it)),
        );
    }
    if shared.crates_changed || shared.changed {
        // A trait the write removed leaves the wild ones in the work, before they are read again.
        let gone = |it: &TraitId| {
            delta.is_some_and(|delta| {
                delta.traits.iter().any(|&(t, w)| t == *it && w < 0)
                    && !delta.traits.iter().any(|&(t, w)| t == *it && w > 0)
            })
        };
        keys.extend(
            crate_state.wild_traits.iter().filter(|it| !gone(it)).map(|&it| Warm::Trait(it)),
        );
    }
    if job.fresh
        || shared.crates_changed
        || delta.is_some_and(touches_traits)
        || crate_state.lang.is_none()
    {
        keys.extend(crate_state.derives.keys().map(|&it| Warm::Derive(it)));
    }
    db.run_all(Tag::Infer, keys.len(), |db, at| warm_one(db, &keys[at]));
    let sweep = job.fresh || shared.crates_changed;
    if !(sweep || shared.changed || delta.is_some()) {
        return;
    }
    let mut modules = std::mem::take(&mut job.scratch.body_modules);
    bodies_to_read(db, crate_state, krate, delta, sweep, &mut modules);
    let def_map = crate_def_map(db, krate);
    db.run_all(Tag::Infer, modules.len(), |db, at| {
        if def_map.module_rank(db, modules[at]).is_some() {
            _ = body_part(db, modules[at]);
        }
    });
    job.scratch.body_modules = modules;
}

/// The keys of `old` not in `new` (`-1`) and of `new` not in `old` (`+1`), both sorted.
fn moved<'a, K: Ord + Copy>(old: &'a [K], new: &'a [K]) -> impl Iterator<Item = (K, i32)> + 'a {
    let gone = old.iter().filter(|it| new.binary_search(it).is_err()).map(|&it| (it, -1));
    gone.chain(new.iter().filter(|it| old.binary_search(it).is_err()).map(|&it| (it, 1)))
}
