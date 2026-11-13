//! What the daemon gives back when memory is short: the thin driver of
//! `blaze_graph::giveback::Planner` (docs/v2/plan/design/memory.md, "Giving memory back").
//!
//! Once a loop turn ([`Jobs::give_back`], from `Jobs::apply_writes`) the planner is told the
//! limit, the last footprint reading and what a task waits for ([`Memory`]); when it would
//! plan, the sources are listed and its gives done:
//!
//! | rung | the source | the give |
//! |---|---|---|
//! | allocator | what the last reading says the allocators keep freed | a purge on the purger's thread (`Plane::ask_purge`), then a reading marked purged |
//! | pool | the pages the stores' pools hold (the `store` tag's committed bytes), unless their last sweeps freed nothing | asked of the pools' slicer (`blaze_graph::poolgive::Slices`): `Store::evict_pool` of one slice a turn ([`Jobs::sweep_pools`]), never the whole give in a turn |
//! | caches | what codegen keeps between builds (`Steps::kept_caches`), listed only while no task that may walk a program runs (`Plane::walks_in_flight`) | `Steps::give_caches` |
//! | caches ([`CHUNKS`]) | the warm chunks no owner holds (`blaze_mem::chunks::cached`) | `blaze_mem::chunks::trim` |
//! | caches ([`NAMES`]) | the names the flight reader keeps of past generations (`blaze_profile::flight::names_held`) | `blaze_profile::flight::trim_names` |
//! | caches ([`IMAGES`]), idle only | the kept link images no run maps (`Paths::weights`, less the live ones) | `Paths::evict_to(0)`, which never lets a live one go |
//! | caches ([`TABLES`]), idle only | the process tables (`Steps::kept_tables`), listed only once no front end is kept or out, no image is kept and no task runs or walks | `Steps::give_tables` |
//! | values | each kept front end's values kept by bytes (`KeptDb::values`); the front ends of the builds under way, all or nothing | a task on a worker ([`Jobs::give_values`]); `Writer::short` |
//! | front end | each kept front end (`KeptDb::held`), the checkout built last the floor unless a build is under way (then that build's own front end is what the edit loop needs) or the daemon is idle past its own wait ([`idle::front_after`]: 64 times its last remake, 30 s to 30 min; its values are kept with it until then) | dropped: its checkout's next build lowers anew, and its `blaze why` says so |
//! | running tasks | the builds under way, while a thread task runs: what their front ends hold in MIR and inference | `Writer::stop_for_memory`: the readers stopped, each task gives way, the release applied, the tasks posted again whole |
//!
//! The limit the planner is told is the limit's give-back line, a sixteenth under it
//! ([`Lines::give`]): the daemon gives back before it is at its limit, since nothing gates an
//! allocation. Past the last rung, with the footprint past the fail line, a thirty-second
//! under the limit ([`Lines::fail`]), and one task running alone, that task's unit fails with
//! a typed error naming it ([`Jobs::fail_too_big`], `blaze_graph::footprint::past_ceiling`):
//! the daemon keeps serving and the kernel's limit is never what ends the build. Both lines,
//! and what the kernel is told, are `blaze_graph::lines`, which holds their order: the kernel
//! slows the daemon nowhere under the fail line (no `memory.high`), so the band between the
//! give-back line and the limit always has that exit.
//!
//! A kept front end's values are swept on a worker, never the loop: the front end is taken
//! from its checkout as it is for its store, a task of kind `GIVE_BACK` sweeps it in at most
//! [`SLICES`] slices and gives it back in its value ([`Jobs::gave_values`]). A lowering of
//! that checkout asked meanwhile waits for it, as it waits for a store, under a span that
//! names the wait; the wait is bounded: the task stops at the first slice's end after a
//! build asks ([`give_in_slices`]). While a give is out nothing more is planned, and the
//! planner measures it by a reading after its return (`Planner::done`).
//!
//! Sharing the machine. The limit the driver gives back to is the less of the daemon's own
//! and what the machine's ledger last told it to hold for the other daemons' wants
//! (`blaze_graph::ledger::Ledger::yielded`, `admission::yield_to`), while it has no build
//! under way. While it has no job at all it says, every [`GIVES_EVERY`], what it could give
//! back and keep its floor (`Plane::set_gives`), which the ledger records for the others.
//!
//! The idle budget (decision D29). Once the daemon has had no job for [`idle::BUDGET_AFTER`]
//! the planner is told [`idle::BUDGET`] as the limit, exactly (no give-back line under it),
//! and everything goes in rung order: the caches, the link images no run maps, the front ends
//! whole and last, the process tables once nothing that names an interned id is left. The
//! front end of the checkout built last keeps its floor, values and all, for its own wait
//! ([`idle::front_after`], 64 times its last remake within 30 s and 30 minutes): what an edit
//! after the wait pays is that remake, and the wait shortens by itself as a reload from disk
//! gets cheap. The daemon stays up,
//! small, and its next build lowers anew; what it could not give is the `idle over bytes`
//! counter ([`Jobs::idle_over`]). No daemon exits for being idle.
//!
//! Every give is a `memory give-back` span and event (what, whose, how much, why, what it
//! costs to make again), which `blaze explain memory` reads. A front end let go keeps the
//! span and the log line it had before this (`kept front ends let go for memory`, "is let go,
//! and its next build lowers anew"), which the guard counts (`scripts/src/bin/guards.rs`).

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use blaze_core::lock::Lock;
use blaze_graph::{
    NodeId, Pool,
    footprint::{Past, Step as Gate, past_ceiling},
    giveback::{self, Give, Now, Outcome, Planner, Rung, Source, Why},
    lines::Lines,
    poolgive::{self, Asked, Slices},
};
use blaze_protocol::events::NodeKind;

use super::{Does, Jobs, KeptDb, Stage, Step, Steps, World, key_of};
use crate::serve::{
    idle,
    plane::{kinds, ns},
    work::{Front as _, Kept as _, Writer as _},
};

/// The owner of the one source that is every build under way's front end: no checkout's
/// number.
const RUNNING: u32 = u32::MAX;

/// The owner of the kept link images no run maps (a [`Rung::Caches`] source, idle only).
const IMAGES: u32 = u32::MAX - 1;

/// The owner of blaze-mem's warm cached chunks (a [`Rung::Caches`] source).
const CHUNKS: u32 = u32::MAX - 2;

/// The owner of the names the flight reader keeps (a [`Rung::Caches`] source).
const NAMES: u32 = u32::MAX - 3;

/// The owner of the process tables every front end's ids index, the type interner's and the
/// trait solver's (a [`Rung::Caches`] source, idle only, and only once nothing names an id).
const TABLES: u32 = u32::MAX - 4;

/// What a [`Rung::Caches`] give of `owner` is named by, in its span and `blaze explain memory`.
const fn cache_name(owner: u32) -> &'static str {
    match owner {
        IMAGES => "kept link images",
        CHUNKS => "chunks",
        NAMES => "flight names",
        TABLES => "process tables",
        _ => "codegen's kept walks",
    }
}

/// How often, at most, the sources are listed again after a plan that had nothing to give: a
/// listing weighs every kept front end's values.
const PLAN_EVERY: Duration = Duration::from_millis(250);

/// How often, at most, the builds under way are asked for a release
/// (`Writer::short`): whether one comes of it is the release's own rule
/// (`blaze_graph::admission::Squeeze::short`), so the ask's source does not fall at once.
const RUNNING_EVERY: Duration = Duration::from_secs(1);

/// How often, at most, a daemon with no job says what it could give back
/// (`Plane::set_gives`): a listing weighs every kept front end's values.
const GIVES_EVERY: Duration = Duration::from_secs(5);

/// The most slices a give of values is swept in ([`give_in_slices`]): a build that asks for
/// the front end meanwhile waits for one slice's end at most, an eighth of the give.
pub(super) const SLICES: u64 = 8;

/// The least a slice gives: 256 MiB, so a small give is one sweep.
const SLICE_MIN: u64 = 256 << 20;

/// What the driver reads of memory each turn: the planner's `Now` but its clock.
#[derive(Clone, Copy, Debug)]
struct Memory {
    limit: u64,
    /// Whether the planner is told `limit` itself, not its give-back line: the idle budget
    /// ([`Memory::held_to`]), a number the daemon gives down to, not a ceiling it keeps under.
    exact: bool,
    footprint: u64,
    /// What of the footprint the allocators keep freed, an estimate.
    kept: u64,
    readings: u64,
    purged: u64,
    ask: u64,
}

impl Memory {
    /// The same reading, held to the idle budget ([`idle::BUDGET`], exactly) when `idle`: the
    /// daemon has had no job for [`idle::BUDGET_AFTER`].
    fn held_to(self, idle: bool) -> Memory {
        match idle {
            true => Memory { limit: idle::BUDGET, exact: true, ..self },
            false => self,
        }
    }

    /// What the planner is told: the limit as its give-back line ([`Lines::give`]), or the
    /// limit itself when it is exact.
    fn now(&self, at: Duration) -> Now {
        Now {
            at_ns: ns(at),
            limit: if self.exact { self.limit } else { Lines::of(self.limit).give() },
            footprint: self.footprint,
            ask: self.ask,
            reading: self.readings,
            purged: self.purged,
        }
    }
}

/// What the process-wide caches hold ([`Jobs::process_held`]), in bytes.
#[derive(Clone, Copy, Debug)]
struct Process {
    /// blaze-mem's warm cached chunks ([`CHUNKS`]).
    chunks: u64,
    /// The flight reader's names ([`NAMES`]).
    names: u64,
    /// The process tables ([`TABLES`]).
    tables: u64,
}

/// Where the pages of a checkout's members are, of the front end it keeps now
/// ([`Jobs::store_members`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum Members {
    /// Not written by this front end: its build's end stored the sealed crates alone.
    #[default]
    Unwritten,
    /// Its second store is under way, the front end out with it.
    Writing,
    /// Written, the front end back; durable once nothing is left to settle under
    /// [`members_job`].
    Written,
}

/// What the store of checkout `root`'s members is named by among what is left to settle
/// (`Settling`): it is no job's, and no job has this slot, so nothing else is under it.
fn members_job(root: u32) -> blaze_api::build::JobId {
    blaze_api::build::JobId { slot: u32::MAX, generation: root }
}

/// What the give-back rule knows of one checkout.
#[derive(Clone, Copy, Default)]
struct Root {
    /// When its lowering from nothing began, until its front end is kept.
    began: Option<Duration>,
    /// What its front end took to make from nothing, lowering to kept, the last time; 0 until
    /// one was.
    cost_ns: u64,
    /// Whether `cost_ns` was measured: a front end never made from nothing in this daemon
    /// waits the longest before it loses its floor ([`idle::FRONT_MOST`]).
    measured: bool,
    /// Whether its kept front end's members are in the store ([`Jobs::store_members`]).
    members: Members,
}

/// A kept front end's values being given on a worker ([`Jobs::give_values`]).
struct Out<S: Steps> {
    root: u32,
    node: NodeId,
    /// The front end, shared with the task: there until the task takes it, so a task that
    /// settles without running gives it back ([`Jobs::give_settled`]).
    kept: Arc<Lock<Option<S::KeptFront>>>,
    /// Set once a build a client waits on is held for the front end: the task stops at its
    /// slice's end.
    wanted: Arc<AtomicBool>,
    /// The give's span, open until the front end is back.
    span: tracing::Span,
    /// The wait of a lowering held for it, named (every-wait-says-what), once one is.
    wait: Option<tracing::Span>,
}

/// The give-back rule's state, the daemon's one ([`Jobs::give_back`]).
pub(super) struct Giving<S: Steps> {
    planner: Planner,
    /// Kept scratch.
    sources: Vec<Source>,
    gives: Vec<Give>,
    /// By root number.
    roots: Vec<Root>,
    /// The checkout whose front end was kept last: the floor.
    last_built: Option<u32>,
    /// A plan had nothing to give: no listing before this.
    idle_until: Option<Duration>,
    /// When the builds under way were last asked for a release.
    running_asked: Option<Duration>,
    /// The floor was said not to fit, in this stretch past the limit.
    floor_said: bool,
    /// The system said memory is short since the last turn ([`Jobs::pressed`]).
    pressed: bool,
    /// What is wanted of the stores' pools, given a slice a turn ([`Jobs::sweep_pools`]).
    pool: Slices,
    /// The gives of values under way, a checkout each.
    out: Vec<Out<S>>,
    /// Since when a build has been under way, as the loop's turns saw it; restarted when its
    /// tasks are stopped for memory: the work a stop loses.
    building_since: Option<Duration>,
    /// Since when the daemon has had no job, and when it last said what it could give.
    idle_since: Option<Duration>,
    gives_at: Option<Duration>,
    /// What the footprint was past [`idle::BUDGET`] at the last plan of an idle daemon that
    /// found nothing more it could give; 0 otherwise ([`Jobs::idle_over`]).
    idle_over: u64,
    /// What is left past the budget was said, in this idle stretch.
    idle_said: bool,
}

impl<S: Steps> Giving<S> {
    pub(super) fn new() -> Giving<S> {
        Giving {
            planner: Planner::new(),
            sources: Vec::new(), // alloc: kept scratch, none until memory is short
            gives: Vec::new(),   // alloc: kept scratch, none until memory is short
            roots: Vec::new(),   // alloc: one entry a checkout
            last_built: None,
            idle_until: None,
            running_asked: None,
            floor_said: false,
            pressed: false,
            pool: Slices::new(),
            out: Vec::new(), // alloc: none until values are given
            building_since: None,
            idle_since: None,
            gives_at: None,
            idle_over: 0,
            idle_said: false,
        }
    }

    /// Whether the daemon has had no job for [`idle::BUDGET_AFTER`] at `now`, as the loop's
    /// last turn saw it: held to [`idle::BUDGET`].
    fn idle_at(&self, now: Duration) -> bool {
        self.idle_since.is_some_and(|since| now.saturating_sub(since) >= budget_after())
    }

    /// Whether checkout `owner`'s kept front end is past its own wait at `now`, the daemon
    /// idle that long ([`idle::front_after`] of its last remake): no floor then.
    fn front_waited(&self, owner: u32, now: Duration) -> bool {
        let Some(since) = self.idle_since else { return false };
        let remake = match self.roots.get(owner as usize) {
            Some(root) if root.measured => Some(Duration::from_nanos(root.cost_ns)),
            _ => None,
        };
        now.saturating_sub(since) >= front_wait(remake)
    }

    fn root(&mut self, root: u32) -> &mut Root {
        let at = root as usize;
        if self.roots.len() <= at {
            self.roots.resize(at + 1, Root::default()); // alloc: one entry a checkout
        }
        &mut self.roots[at]
    }

    /// A job of checkout `root` lowers at `now`, into its kept front end (`kept`) or from
    /// nothing: the time a front end made from nothing takes is what giving it back costs.
    pub(super) fn lowers(&mut self, root: u32, kept: bool, now: Duration) {
        let it = self.root(root);
        it.began = (!kept).then_some(now);
        // The build writes into the front end: its members' pages are of the next build's end.
        it.members = Members::Unwritten;
    }

    /// Checkout `root`'s front end is kept again at `now`: it is the floor, and one made from
    /// nothing is held for a time that grows with what it took
    /// (`blaze_graph::giveback::hold_ns`).
    pub(super) fn kept(&mut self, root: u32, now: Duration) {
        // The store of its members, back: no build kept it, so it is not the floor for that.
        if self.root(root).members == Members::Writing {
            self.root(root).members = Members::Written;
            return;
        }
        self.last_built = Some(root);
        if let Some(began) = self.root(root).began.take() {
            let cost_ns = ns(now.saturating_sub(began));
            // A test says what the remake took: the simulation's lowering takes no time.
            #[cfg(test)]
            let cost_ns = sim::REMAKE.get().map_or(cost_ns, ns);
            *self.root(root) = Root { began: None, cost_ns, measured: true, members: Members::Unwritten };
            self.planner.remade(Rung::FrontEnd, root, ns(now), cost_ns);
        }
    }

    /// The system says memory is short: at the loop's next turn every kept front end gives
    /// its values ([`Jobs::give_back`]).
    pub(super) fn press(&mut self) {
        self.pressed = true;
    }

    /// A lowering of checkout `root` is held while its front end's values are given: the node
    /// of the give it waits on, if one is under way. A build a client waits on (`idle` not
    /// set) makes the give stop at its slice's end, and its wait is a span from now.
    pub(super) fn wanted(&mut self, root: u32, idle: bool) -> Option<NodeId> {
        let out = self.out.iter_mut().find(|it| it.root == root)?;
        if !idle {
            out.wanted.store(true, Ordering::Release);
            let node = out.node.0;
            _ = out.wait.get_or_insert_with(|| {
                tracing::info_span!(
                    target: "blaze",
                    "lowering waits for a memory give-back of its front end's values",
                    root,
                    node,
                )
            });
        }
        Some(out.node)
    }

    /// What the planner did so far, counted (`blaze explain memory`).
    pub(super) fn counts(&self) -> giveback::Counts {
        self.planner.counts()
    }
}

/// Lets go of about `bytes` of `front`'s values in at most [`SLICES`] sweeps, each at least
/// [`SLICE_MIN`], and stops early once `wanted` is set (a build waits for the front end) or
/// a sweep gives nothing; what went. On a worker, the front end the task's alone. Bounded:
/// each turn of the loop gives something or ends it, and there are at most [`SLICES`] and one.
pub(super) fn give_in_slices<K: KeptDb>(front: &mut K, bytes: u64, wanted: &AtomicBool) -> u64 {
    let slice = (bytes / SLICES).max(SLICE_MIN);
    let mut gave = 0u64;
    for _ in 0..=SLICES {
        if gave >= bytes || wanted.load(Ordering::Acquire) {
            break;
        }
        let _span = tracing::info_span!(
            target: "blaze",
            "memory give-back: one slice of a front end's values",
            slice,
            gave,
        )
        .entered();
        let went = front.give_values(slice.min(bytes - gave));
        if went == 0 {
            break;
        }
        gave = gave.saturating_add(went);
    }
    gave
}

/// How long the daemon has no job before it is held to the idle budget:
/// [`idle::BUDGET_AFTER`], or none under the simulation's planted bug that sets the waits to
/// zero ([`sim::Plant::NoWait`]).
fn budget_after() -> Duration {
    #[cfg(test)]
    if sim::PLANT.get() == sim::Plant::NoWait {
        return Duration::ZERO;
    }
    idle::BUDGET_AFTER
}

/// How long the daemon has no job before a kept front end whose last remake took `remake`
/// (`None`: never measured, the longest wait) is no floor: [`idle::front_after`]. Under the
/// simulation's planted bugs, none ([`sim::Plant::NoWait`]) or the one constant wait this
/// replaced ([`sim::Plant::ConstantWait`]).
fn front_wait(remake: Option<Duration>) -> Duration {
    #[cfg(test)]
    match sim::PLANT.get() {
        sim::Plant::NoWait => return Duration::ZERO,
        sim::Plant::ConstantWait => return idle::BUDGET_AFTER,
        _ => {}
    }
    remake.map_or(idle::FRONT_MOST, idle::front_after)
}

/// What the kept link images of `paths` weigh less the live ones (a run maps them, and its
/// patches go into that memory): what `Paths::evict_to(0)` gives.
fn unmapped_images<S: Steps>(paths: &super::Paths<S>) -> u64 {
    let live: u64 = (paths.images.iter().enumerate())
        .filter(|(_, it)| it.as_ref().is_some_and(|it| it.kept.live()))
        .map(|(id, _)| paths.weights.row(id as u32, true).bytes)
        .sum();
    paths.weights.held().saturating_sub(live)
}

/// The last component of a checkout's path: the project, as a user names it.
fn project(checkout: &str) -> &str {
    checkout.trim_end_matches('/').rsplit('/').next().filter(|it| !it.is_empty()).unwrap_or(checkout)
}

impl<S: Steps> Jobs<S> {
    /// The checkout numbered `root`, by its path.
    fn checkout_of(&self, root: u32) -> &str {
        self.roots.ids.iter().find(|&(_, &id)| id == root).map_or("no checkout", |(name, _)| &**name)
    }

    /// How many kept front ends give back values on a worker now ([`Jobs::give_values`]):
    /// each is out of its checkout until its task's value is back.
    pub fn giving_values(&self) -> usize {
        self.giving.out.len()
    }

    /// What the footprint is past [`idle::BUDGET`] while the daemon is idle past
    /// [`idle::BUDGET_AFTER`] and the planner's last plan found nothing more to give (the
    /// floor-less front ends held by their remake, live link images, or what no source
    /// names); 0 otherwise. The daemon's `idle over bytes` counter: 0 is the budget held.
    pub fn idle_over(&self) -> u64 {
        self.giving.idle_over
    }

    /// What the driver decides on this turn: the limit the daemon is held to, the plane's last
    /// footprint reading, and `ask`; with `idle`, the limit is the idle budget
    /// ([`Memory::held_to`]). Under the daemon's simulation a test says them
    /// ([`sim::TOLD`]): the process's own footprint is no part of a simulated daemon.
    fn memory(&self, w: &World<'_, S>, ask: u64, building: bool, idle: bool) -> Memory {
        #[cfg(test)]
        if let Some(memory) = sim::memory() {
            return memory.held_to(idle);
        }
        let reading = w.plane.reading();
        let limit = blaze_graph::admission::limit();
        Memory {
            // With no build under way, the less of its own limit and what the machine's
            // ledger told it to hold for the other daemons' wants.
            limit: match building {
                true => limit,
                false => limit.min(blaze_graph::admission::yield_to()),
            },
            exact: false,
            footprint: reading.footprint,
            kept: reading.kept,
            readings: reading.readings,
            purged: reading.purged,
            ask,
        }
        .held_to(idle)
    }

    /// One turn of giving back: one step of the give-back rule ([`Jobs::plan_gives`]), then at
    /// most one slice of the stores' pool pages ([`Jobs::sweep_pools`]), after the plan so
    /// that a pool give planned this turn has its first slice in it. Called once a loop turn.
    pub(super) fn give_back(&mut self, w: &mut World<'_, S>, ask: u64) {
        self.plan_gives(w, ask);
        self.sweep_pools(w);
    }

    /// The system wants the stores' pools to give back every page no task pins, for `stands`
    /// from `now`: memory pressure (`Daemon::pressure`), or a footprint past its release line
    /// (`Daemon::plot`). Nothing is swept here: the pools give a slice a turn while it stands
    /// ([`Jobs::sweep_pools`]), and the asker says it again while its reason lasts.
    pub fn ask_pools(&mut self, now: Duration, stands: Duration) {
        self.giving.pool.stand(ns(now.saturating_add(stands)));
    }

    /// At most one slice of the stores' pool pages given back to the kernel
    /// (`blaze_graph::poolgive::Slices::turn`): the one place the loop evicts pool pages while
    /// a build may run, bounded by the slice so the loop turns between two of them
    /// (bounded-progress). Each slice is a span saying the bytes asked and freed and whose
    /// want it answers; pools whose sweeps free nothing twice in a row are said dry, and
    /// neither swept nor listed as a source until that passes, so the ladder goes on to its
    /// next rung.
    fn sweep_pools(&mut self, w: &mut World<'_, S>) {
        let now = ns(w.now);
        let poolgive::Turn::Sweep { bytes, why } = self.giving.pool.turn(now) else { return };
        let span = tracing::info_span!(
            target: "blaze",
            "memory give-back: pool slice",
            asked = bytes,
            why = why.name(),
            freed = tracing::field::Empty,
            left = tracing::field::Empty,
            outcome = tracing::field::Empty,
        )
        .entered();
        let store = w.store.evict_pool(bytes);
        let std = w.std.evict_pool(bytes.saturating_sub(store));
        let freed = store.saturating_add(std);
        span.record("freed", freed);
        match self.giving.pool.swept(freed, now) {
            poolgive::Swept::Freed { left } => {
                span.record("left", left);
                span.record("outcome", "freed");
            }
            poolgive::Swept::Empty => {
                span.record("outcome", "freed nothing: asked once more");
            }
            poolgive::Swept::Dry { left, until_ns } => {
                span.record("left", left);
                span.record("outcome", "dry");
                tracing::info!(
                    target: "blaze",
                    left,
                    until_ns,
                    sweeps_in_a_row = poolgive::DRY_AFTER,
                    "memory give-back: the stores' pools are dry (every page left is pinned by a running task or was just read): not swept again for 10 s, and the ladder goes on to its next rung"
                );
            }
        }
    }

    /// One step of the give-back rule (the module's doc), `ask` the bytes a task waits for of
    /// what the process holds besides its tasks (`blaze_graph::Plane::short`). Called once a
    /// loop turn: with nothing short it reads two numbers and returns.
    fn plan_gives(&mut self, w: &mut World<'_, S>, ask: u64) {
        if std::mem::take(&mut self.giving.pressed) {
            self.press_kept(w);
        }
        let building = self.building();
        match (building, self.giving.building_since) {
            (true, None) => self.giving.building_since = Some(w.now),
            (false, Some(_)) => self.giving.building_since = None,
            _ => {}
        }
        // A daemon with no job at all: since when, whether it is held to the idle budget yet,
        // and what it could give back, said for the machine's ledger every `GIVES_EVERY`.
        let jobless = !building && self.order.is_empty();
        match (jobless, self.giving.idle_since) {
            (true, None) => self.giving.idle_since = Some(w.now),
            (false, Some(_)) => self.giving.idle_since = None,
            _ => {}
        }
        let idle = jobless && self.giving.idle_at(w.now);
        if !idle {
            (self.giving.idle_over, self.giving.idle_said) = (0, false);
        }
        let memory = self.memory(w, ask, building, idle);
        if !jobless {
            if self.giving.gives_at.take().is_some() {
                w.plane.set_gives(0);
            }
        } else if self.giving.gives_at.is_none_or(|at| w.now >= at + GIVES_EVERY) {
            self.giving.gives_at = Some(w.now);
            let mut sources = std::mem::take(&mut self.giving.sources);
            sources.clear();
            self.give_sources(w, memory.kept, false, idle, &mut sources);
            #[cfg(test)]
            sim::listed(&sources);
            let gives = sources
                .iter()
                .filter(|it| !(it.rung == Rung::FrontEnd && it.floor))
                .fold(0u64, |sum, it| sum.saturating_add(it.gives));
            w.plane.set_gives(gives);
            self.giving.sources = sources;
        }
        #[cfg(test)]
        if sim::PLANT_NO_PLAN.get() {
            // The planted bug, and the rule before this one: the driver gives by itself, with
            // no plan, every kept front end for any wait.
            if memory.ask > 0 {
                for at in 0..self.roots.fronts.len() {
                    if self.roots.fronts[at].take().is_some() {
                        let checkout = self.checkout_of(at as u32).to_owned();
                        sim::dropped(&checkout);
                    }
                }
            }
            return;
        }
        // A give of values is out on a worker: what it gave is measured once it is back.
        if !self.giving.out.is_empty() {
            return;
        }
        let now = memory.now(w.now);
        let mut gives = std::mem::take(&mut self.giving.gives);
        gives.clear();
        if !self.giving.planner.due(&now) {
            // Enough, or measuring: the planner keeps its own state, with no source listed.
            let outcome = self.giving.planner.step(&now, &[], &mut gives);
            if outcome == Outcome::Enough {
                self.giving.floor_said = false;
                self.giving.idle_until = None;
                self.giving.idle_over = 0;
                // And the pools owe the ladder nothing more.
                self.giving.pool.enough();
                // No task under way is asked to give way any more.
                giveback::set_short(false);
            }
            self.giving.gives = gives;
            return;
        }
        if self.giving.idle_until.is_some_and(|until| w.now < until) {
            self.giving.gives = gives;
            return;
        }
        let span = tracing::info_span!(
            target: "blaze",
            "memory give-back plan",
            ask = memory.ask,
            limit = now.limit,
            footprint = now.footprint,
            allocator_kept = memory.kept,
            outcome = tracing::field::Empty,
        )
        .entered();
        let mut sources = std::mem::take(&mut self.giving.sources);
        sources.clear();
        self.give_sources(w, memory.kept, building, idle, &mut sources);
        #[cfg(test)]
        sim::listed(&sources);
        let outcome = self.giving.planner.step(&now, &sources, &mut gives);
        // Idle with nothing more to give and still past the budget: by how much, and once a
        // stretch, what is left by name (the sources the planner would not take).
        let stuck = idle && matches!(outcome, Outcome::Waits { .. } | Outcome::FloorOver { .. });
        let over = if stuck { now.footprint.saturating_sub(idle::BUDGET) } else { 0 };
        if over != 0 && !std::mem::replace(&mut self.giving.idle_said, true) {
            // alloc: once an idle stretch the budget is not reached, a line naming what is left
            let left: Vec<String> = sources
                .iter()
                .filter(|it| it.gives != 0)
                .map(|it| format!("{} of {}: {} bytes", it.rung.name(), self.source_name(it), it.gives))
                .collect();
            tracing::warn!(
                target: "blaze",
                over,
                budget = idle::BUDGET,
                footprint = now.footprint,
                left = %left.join("; "),
                "memory give-back: the idle daemon is past its budget and nothing more is given now"
            );
        }
        self.giving.idle_over = over;
        // When nothing more is given: planned again after `PLAN_EVERY`, or, idle, after the
        // budget's wait. An idle daemon past its budget with nothing it may give (a floor
        // inside its wait, a live image, what no source names) would otherwise list and purge
        // four times a second for as long as it stays so.
        let replan = if idle { budget_after().max(PLAN_EVERY) } else { PLAN_EVERY };
        match outcome {
            Outcome::Gave { rung } => {
                span.record("outcome", rung.name());
                self.giving.idle_until = None;
                for give in &gives {
                    self.give(w, give);
                }
            }
            Outcome::Waits { need } => {
                span.record("outcome", "waits");
                self.giving.idle_until = Some(w.now + replan);
                tracing::debug!(
                    target: "blaze",
                    need,
                    "a task waits for memory and nothing in proportion is left to give: it waits for a task to end, or starts alone"
                );
            }
            Outcome::FloorOver { over, limit, footprint, owner, kept } => {
                span.record("outcome", "the floor does not fit");
                self.giving.idle_until = Some(w.now + replan);
                // Said only of the daemon's own limit: one that keeps its floor against
                // another daemon's want is within its limit, and says nothing.
                let yielding = memory.limit < blaze_graph::admission::limit();
                if !yielding && !std::mem::replace(&mut self.giving.floor_said, true) {
                    let checkout = self.checkout_of(owner);
                    let line = giveback::floor_line(project(checkout), over, limit, footprint, kept);
                    tracing::warn!(
                        target: "blaze",
                        checkout,
                        over,
                        limit,
                        footprint,
                        kept,
                        "memory give-back: the floor does not fit under the limit"
                    );
                    crate::serve::said::logged!(WARN, "blaze serve: {line}");
                    // The checkout's next build says it too (`blaze why`), unless a loss
                    // waits there already.
                    let lost = &mut self.roots.lost[owner as usize];
                    if lost.is_none() {
                        *lost = Some(line);
                    }
                }
            }
            Outcome::Enough | Outcome::Measuring { .. } => {}
        }
        // Nothing is left to give and the footprint is at the limit: a task that runs alone
        // and alone does not fit fails, by name.
        if building && matches!(outcome, Outcome::Waits { .. } | Outcome::FloorOver { .. }) {
            self.fail_too_big(w, &memory);
        }
        drop(span);
        self.giving.sources = sources;
        self.giving.gives = gives;
    }

    /// What every owner can give now (the module's table), added to `sources`; `idle` whether
    /// the daemon is held to the idle budget ([`idle::BUDGET_AFTER`]): the kept link images
    /// and the process tables are sources too, and a front end past its own wait
    /// ([`idle::front_after`]) is no floor.
    fn give_sources(
        &mut self,
        w: &World<'_, S>,
        allocator_kept: u64,
        building: bool,
        idle: bool,
        sources: &mut Vec<Source>,
    ) {
        let one = |rung, gives, grain| Source { rung, owner: 0, gives, grain, remake_ns: 0, floor: false };
        let cache = |owner, gives| Source { rung: Rung::Caches, owner, gives, grain: gives, remake_ns: 0, floor: false };
        if allocator_kept != 0 {
            // alloc: none past the most sources, the scratch kept (every push below)
            sources.push(one(Rung::Allocator, allocator_kept, allocator_kept));
        }
        let pool = blaze_mem::tag::snapshot().get(blaze_mem::Tag::Store).live;
        // Dry pools (their last sweeps freed nothing) are no source until that passes.
        if pool != 0 && !self.giving.pool.dry(ns(w.now)) {
            sources.push(one(Rung::Pool, pool, 1));
        }
        // Codegen's kept walks, only while no task that may walk runs: one in flight keeps
        // its own table until it ends, so a give then frees nothing and loses its result.
        if w.plane.walks_in_flight() == 0 {
            let caches = w.steps.kept_caches();
            if caches != 0 {
                sources.push(one(Rung::Caches, caches, caches));
            }
        }
        // The process's own caches: blaze-mem's warm chunks and the flight reader's names,
        // made again as they are next used.
        let process = self.process_held(w);
        if process.chunks != 0 {
            sources.push(cache(CHUNKS, process.chunks));
        }
        if process.names != 0 {
            sources.push(cache(NAMES, process.names));
        }
        if idle {
            // The kept link images no run maps: the next link of each lays it out whole, so
            // only an idle daemon lets them go, past the budget's own rule
            // (`Paths::evict_to`, which never lets a live one go).
            let images = unmapped_images(&self.paths);
            if images != 0 {
                sources.push(cache(IMAGES, images));
            }
            // The process tables, last: only once nothing that names an interned id is
            // left, no front end kept or out (stored, swept or under way), no link image
            // kept, and no task running or walking. Freed under any of those, an id read
            // after would name another value.
            let nothing_names_an_id = self.roots.fronts.iter().all(Option::is_none)
                && self.roots.storing.iter().all(|&it| it == 0)
                && self.giving.out.is_empty()
                && self.paths.kept == 0
                && w.plane.walks_in_flight() == 0
                && w.plane.running() == 0;
            #[cfg(test)]
            let nothing_names_an_id = nothing_names_an_id || sim::PLANT.get() == sim::Plant::TablesAlways;
            if nothing_names_an_id && process.tables != 0 {
                sources.push(cache(TABLES, process.tables));
            }
        }
        // The floor is what the edit loop needs next: the front end of the checkout built
        // last, unless a build is under way, whose own front end (out with it) is that.
        // Nor is it a floor in a daemon idle past that front end's own wait
        // (`idle::front_after`): it is in no edit loop, and is held to the idle budget.
        let waited = |owner: u32| idle && self.giving.front_waited(owner, w.now);
        #[cfg(test)]
        let waited = |owner: u32| waited(owner) && sim::PLANT.get() != sim::Plant::IdleKeepsFloor;
        let floor = self.giving.last_built.filter(|&owner| !building && !waited(owner));
        for (at, front) in self.roots.fronts.iter_mut().enumerate() {
            let Some(front) = front else { continue };
            let owner = at as u32;
            let cost_ns = self.giving.roots.get(at).map_or(0, |it| it.cost_ns);
            let held = front.held();
            let values = front.values();
            // Idle and still the floor: its values are the edit loop's too, kept with it
            // until its wait has passed (under the budget's limit they would go at once).
            if values != 0 && !(idle && floor == Some(owner)) {
                // What the values cost to make again is not summed from their queries' spans
                // yet: their part of the front end's own cost, by bytes, which is more than
                // they cost (a value is made again from its memo's inputs, kept).
                let remake_ns = match held {
                    0 => 0,
                    held => {
                        let part = u128::from(cost_ns) * u128::from(values.min(held)) / u128::from(held);
                        u64::try_from(part).unwrap_or(u64::MAX)
                    }
                };
                sources.push(Source { rung: Rung::Values, owner, gives: values, grain: 1, remake_ns, floor: false });
            }
            if held != 0 {
                sources.push(Source {
                    rung: Rung::FrontEnd,
                    owner,
                    gives: held,
                    grain: held,
                    remake_ns: cost_ns,
                    floor: floor == Some(owner),
                });
            }
        }
        // The builds under way: what their front ends hold in MIR and inference, which a
        // release lets go of where nothing still to compile reads it.
        if !building {
            return;
        }
        let snapshot = blaze_mem::tag::snapshot();
        let of = |tag: blaze_mem::Tag| {
            let it = snapshot.get(tag);
            it.live.saturating_add(it.heap)
        };
        let gives = of(blaze_mem::Tag::Mir).saturating_add(of(blaze_mem::Tag::Infer));
        if gives == 0 {
            return;
        }
        // Asked politely first: a release once the tasks under way end by themselves.
        let lately = self.giving.running_asked.is_some_and(|at| w.now < at + RUNNING_EVERY);
        if !lately {
            sources.push(Source { rung: Rung::Values, owner: RUNNING, gives, grain: gives, remake_ns: 0, floor: false });
        }
        // Then, the last rung: the tasks stopped for that release now. What it costs is the
        // work they did since the build began, or since they were last stopped.
        if w.plane.running() > 0 {
            let lost = self.giving.building_since.map_or(0, |since| ns(w.now.saturating_sub(since)));
            sources.push(Source { rung: Rung::Running, owner: RUNNING, gives, grain: gives, remake_ns: lost, floor: false });
        }
    }

    /// Unit node `node` settled (docs/v2/plan/design/memory.md, "Forecasts"). The plane is
    /// told what its unit's codegen walk reached, where it is a codegen that ended well: what
    /// that task held per instance teaches the walks to come. And where it is the unit's
    /// first node (its names), the unit's own codegen, not started yet, is forecast by the
    /// most any unit it depends on reached: a top crate's walk reaches at least what its
    /// largest dependency's did, which no forecast made as the build's nodes were pushed
    /// could know. Only raises a forecast; a dependency whose walk has not ended says nothing.
    pub(super) fn forecast_walk(&mut self, w: &mut World<'_, S>, node: NodeId) {
        let Some(id) = self.batches.job_of(node) else { return };
        let Some(job) = self.jobs.get(key_of(id)) else { return };
        let Stage::Units(build) = &job.stage else { return };
        let Some(unit) = build.unit(node) else { return };
        w.plane.walked(node, build.front.reached(unit));
        let at = node.0.wrapping_sub(build.first.0) as usize;
        let of_unit = |at: usize| build.units.get(at) == Some(&(unit as u32));
        if at > 0 && of_unit(at - 1) {
            return;
        }
        let depends = &build.input.units.graph.units[unit].dependencies;
        let most = depends.iter().filter_map(|dep| build.front.reached(dep.index as usize)).max();
        let Some(instances) = most else { return };
        let mut next = at + 1;
        while of_unit(next) {
            let compile = NodeId(build.first.0 + next as u32);
            if w.plane.kind_of(compile) == Some(kinds::COMPILE) {
                w.plane.forecast_instances(w.now, compile, instances);
                return;
            }
            next += 1;
        }
    }

    /// Whether a build's units are under way: a job at its units' stage.
    fn building(&self) -> bool {
        self.order.iter().any(|&id| {
            self.jobs.get(key_of(id)).is_some_and(|job| matches!(job.stage, Stage::Units(_)))
        })
    }

    /// The ceiling's last step (`blaze_graph::footprint::past_ceiling`), once nothing is left
    /// to give: with the footprint past the limit's fail line ([`Lines::fail`]) and
    /// one thread task running alone, that task's unit is refused with a typed error naming
    /// it, its peak, what the process holds besides and the ceiling, and its build's readers
    /// are stopped so the task gives way and ends with that error where it is posted again
    /// (`Jobs::post`). While another task runs nothing fails: it ends and gives memory back.
    /// A task that makes no query call (inside the back end's own code) is not reached by the
    /// stop; the ceiling's guard is still the bound there (`serve::ceiling`).
    fn fail_too_big(&mut self, w: &mut World<'_, S>, memory: &Memory) {
        if memory.limit == u64::MAX {
            return;
        }
        let Some((node, held, running)) = w.plane.biggest_running() else { return };
        let past = Past {
            ceiling: Lines::of(memory.limit).fail(),
            footprint: memory.footprint,
            // The planner purged, and nothing it could give is left.
            kept: 0,
            bytes: 0,
            asker: node,
            asker_holds: held,
            others_unrealized: 0,
            others_running: running.saturating_sub(1),
            others_waiting: 0,
            newest: node,
            tried: true,
        };
        let Gate::Fail(too_big) = past_ceiling(&past) else { return };
        let Some(id) = self.batches.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let Stage::Units(build) = &mut job.stage else { return };
        let Some(unit) = build.unit(node) else { return };
        let named = {
            let it = &build.input.units.graph.units[unit];
            format!("{} of {}", it.target.name, it.pkg_id) // alloc: an error, once
        };
        // alloc: an error, once
        let why = format!(
            "{named} does not fit in the daemon's memory: {too_big}; everything else the daemon could give back was given first"
        );
        tracing::error!(
            target: "blaze",
            unit = named.as_str(),
            held,
            footprint = memory.footprint,
            limit = memory.limit,
            "memory ceiling: a task that alone does not fit fails"
        );
        crate::serve::said::logged!(ERROR, "blaze serve: {why}");
        build.refuse_unit(unit, why);
        build.writer.stop_for_memory();
    }

    /// Does one give (the module's table), under a span that says what, whose, how much, why
    /// and what it costs to make again.
    fn give(&mut self, w: &mut World<'_, S>, give: &Give) {
        let (why, need) = match give.why {
            Why::Ask(bytes) => ("a task waits for memory the process holds besides its tasks", bytes),
            Why::Over(bytes) => ("the footprint is past the limit", bytes),
        };
        let checkout = match give.rung {
            Rung::Values | Rung::FrontEnd if give.owner != RUNNING => self.checkout_of(give.owner).to_owned(), // alloc: a give's name, once a give
            Rung::Values | Rung::Running => "the builds under way".to_owned(), // alloc: a give's name, once a give
            Rung::Caches => cache_name(give.owner).to_owned(), // alloc: a give's name, once a give
            _ => String::new(),                                // alloc: none, empty
        };
        #[cfg(test)]
        sim::GAVE.with_borrow_mut(|it| it.push((give.rung, checkout.clone(), give.bytes))); // clone: a test's record
        let span = tracing::info_span!(
            target: "blaze",
            "memory give-back",
            what = give.rung.name(),
            owner = checkout.as_str(),
            bytes = give.bytes,
            why,
            need,
            limit = give.limit,
            footprint = give.footprint,
            remake_ns = give.remake_ns,
            again = give.again,
            gave = tracing::field::Empty,
        );
        tracing::info!(
            target: "blaze",
            parent: &span,
            what = give.rung.name(),
            owner = checkout.as_str(),
            bytes = give.bytes,
            why,
            need,
            limit = give.limit,
            footprint = give.footprint,
            remake_ns = give.remake_ns,
            again = give.again,
            "memory give-back"
        );
        if give.again {
            // The named failure (hysteresis): given, made again, and given again.
            crate::serve::said::logged!(WARN,
                "blaze serve: memory thrash: {} of {checkout} was given back, made again and is given back again ({} such so far)",
                give.rung.name(),
                self.giving.counts().thrash
            );
        }
        // A kept front end's values go on a worker: the span is the give's until it is back.
        if give.rung == Rung::Values && give.owner != RUNNING {
            self.give_values(w, give.owner, &checkout, give.bytes, span);
            return;
        }
        let entered = span.enter();
        let gave = match give.rung {
            Rung::Allocator => {
                self.purge(w);
                None
            }
            // Wanted of the pools, not swept here: a slice a turn from this one on
            // ([`Jobs::sweep_pools`], whose spans say what each freed), since a give may be
            // gigabytes and the loop must turn meanwhile.
            Rung::Pool => match self.giving.pool.ask(give.bytes, ns(w.now)) {
                Asked::Wanted => None,
                Asked::Dry { .. } => Some(0),
            },
            Rung::Caches => Some(self.give_cache(w, give.owner)),
            Rung::Values => {
                self.giving.running_asked = Some(w.now);
                for &id in &self.order {
                    let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
                    if let Stage::Units(build) = &mut job.stage {
                        build.writer.short();
                    }
                }
                None
            }
            Rung::FrontEnd => {
                // Let go for idleness: only once its members' pages are durable, so the next
                // process, or the next build of this one, reads them from the store. Until
                // then the give writes them and gives nothing this turn.
                let idle = !self.building() && self.order.is_empty() && self.giving.idle_at(w.now);
                let members = self.giving.roots.get(give.owner as usize).map_or(Members::Unwritten, |it| it.members);
                #[cfg(test)]
                let members = match sim::PLANT.get() {
                    sim::Plant::DropsUnwritten => Members::Written,
                    _ => members,
                };
                let durable = !self.settling.iter().any(|it| it.job == members_job(give.owner));
                match (idle, members) {
                    (true, Members::Unwritten) => {
                        self.store_members(w, give.owner);
                        None
                    }
                    (true, Members::Writing) => None,
                    (true, Members::Written) if !durable => None,
                    (false, Members::Unwritten | Members::Writing) => {
                        crate::serve::said::logged!(WARN,
                            "blaze serve: the kept front end of {checkout} is let go for memory before its workspace members' pages \
                             were written: the next build, or the next daemon, resolves its members from their sources"
                        );
                        self.give_front_end(give, &checkout)
                    }
                    _ => self.give_front_end(give, &checkout),
                }
            }
            Rung::Running => {
                for &id in &self.order {
                    let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
                    if let Stage::Units(build) = &mut job.stage {
                        build.writer.stop_for_memory();
                    }
                }
                // The work begins again from here, and a build stopped is not stopped again
                // until it has had many times what it lost (`giveback::hold_ns`).
                self.giving.planner.remade(Rung::Running, RUNNING, ns(w.now), give.remake_ns);
                self.giving.building_since = Some(w.now);
                // And a long task that makes few query calls gives way at its own safe
                // point (the codegen walk's, between two runs of its worklist), until the
                // footprint is within the limit again.
                giveback::set_short(true);
                crate::serve::said::logged!(WARN,
                    "blaze serve: memory: the tasks of the builds under way are stopped and run again, {:.0} s of their work with them: \
                     the footprint is {:.2} GiB of a limit's line of {:.2} GiB and nothing cheaper is left to give",
                    give.remake_ns as f64 / 1e9,
                    give.footprint as f64 / f64::from(1u32 << 30),
                    give.limit as f64 / f64::from(1u32 << 30)
                );
                None
            }
        };
        drop(entered);
        if let Some(gave) = gave {
            span.record("gave", gave);
        }
    }

    /// Asks a purge of what the allocators keep freed: on the purger's thread, followed by a
    /// reading marked purged (`Plane::ask_purge`). Under the daemon's simulation the model's
    /// kept pages go ([`sim::purged`]): a simulated daemon purges no real heap.
    fn purge(&mut self, w: &mut World<'_, S>) {
        #[cfg(test)]
        if sim::TOLD.get().is_some() {
            sim::purged();
            return;
        }
        w.plane.ask_purge();
    }

    /// What the process-wide caches hold now: blaze-mem's warm chunks, the flight reader's
    /// names and the process tables. Under the daemon's simulation the model's
    /// ([`sim::Told`]): the test process's own are no part of a simulated daemon.
    fn process_held(&self, w: &World<'_, S>) -> Process {
        #[cfg(test)]
        if let Some(told) = sim::TOLD.get() {
            return Process { chunks: told.chunks, names: told.names, tables: told.tables };
        }
        Process {
            chunks: blaze_mem::chunks::cached().warm,
            names: blaze_profile::flight::names_held(),
            tables: w.steps.kept_tables(),
        }
    }

    /// Who holds `source`, as a line names it.
    fn source_name(&self, source: &Source) -> &str {
        match source.rung {
            Rung::Caches => cache_name(source.owner),
            Rung::Values | Rung::FrontEnd if source.owner != RUNNING => self.checkout_of(source.owner),
            Rung::Values | Rung::Running => "the builds under way",
            Rung::Allocator | Rung::Pool | Rung::FrontEnd => "the process",
        }
    }

    /// A [`Rung::Caches`] give of `owner`, under a span named for it with the bytes before and
    /// after: what went.
    fn give_cache(&mut self, w: &mut World<'_, S>, owner: u32) -> u64 {
        #[cfg(test)]
        if sim::TOLD.get().is_some() && matches!(owner, CHUNKS | NAMES | TABLES) {
            let fronts = self.roots.fronts.iter().flatten().count();
            return sim::gave_cache(owner, fronts);
        }
        // The give's own span (`memory give-back`) names the rung and the owner; this one is
        // named for the owner, with what it held before and after.
        let span = match owner {
            IMAGES => tracing::info_span!(target: "blaze", "kept link images", before = tracing::field::Empty, after = tracing::field::Empty),
            CHUNKS => tracing::info_span!(target: "blaze", "chunks", before = tracing::field::Empty, after = tracing::field::Empty),
            NAMES => tracing::info_span!(target: "blaze", "flight names", before = tracing::field::Empty, after = tracing::field::Empty),
            TABLES => tracing::info_span!(target: "blaze", "process tables", before = tracing::field::Empty, after = tracing::field::Empty),
            _ => tracing::info_span!(target: "blaze", "codegen's kept walks", before = tracing::field::Empty, after = tracing::field::Empty),
        };
        let _entered = span.enter();
        let (before, after) = match owner {
            IMAGES => {
                let before = self.paths.weights.held();
                self.paths.evict_to(0);
                (before, self.paths.weights.held())
            }
            CHUNKS => {
                let before = blaze_mem::chunks::cached().warm;
                let chunks = blaze_mem::chunks::trim();
                tracing::debug!(target: "blaze", chunks, "warm chunks trimmed");
                (before, blaze_mem::chunks::cached().warm)
            }
            NAMES => {
                let before = blaze_profile::flight::names_held();
                let trimmed = blaze_profile::flight::trim_names();
                tracing::debug!(target: "blaze", trimmed, "flight names trimmed");
                (before, blaze_profile::flight::names_held())
            }
            TABLES => {
                let before = w.steps.kept_tables();
                let gave = w.steps.give_tables();
                tracing::debug!(target: "blaze", gave, "process tables released");
                (before, w.steps.kept_tables())
            }
            _ => {
                let before = w.steps.kept_caches();
                let gave = w.steps.give_caches();
                (before, before.saturating_sub(gave))
            }
        };
        span.record("before", before);
        span.record("after", after);
        before.saturating_sub(after)
    }

    /// Checkout `root`'s kept front end gives about `bytes` of its values on a worker (the
    /// module's doc): taken from the checkout, swept by a task of a plane request of its own
    /// that asks no memory, and the checkout's again once the task's value is back
    /// ([`Jobs::gave_values`]). `span` is the give's, open until then. A plane that refuses
    /// the task leaves the front end where it was, with nothing given, and says so.
    fn give_values(
        &mut self,
        w: &mut World<'_, S>,
        root: u32,
        checkout: &str,
        bytes: u64,
        span: tracing::Span,
    ) {
        let Some(front) = self.roots.fronts.get_mut(root as usize).and_then(Option::take) else {
            span.record("gave", 0u64);
            return;
        };
        let request = w.plane.open(w.now, false, false);
        let name = super::named("giving back values of the kept front end of ", checkout);
        let mut fresh = w.plane.fresh(1, name.len(), 0);
        let node = w.plane.push_free(
            &mut fresh,
            super::node(NodeKind::Store, &name, &[], Pool::Work, None),
            super::one(kinds::GIVE_BACK),
        );
        let wanted = Arc::new(AtomicBool::new(false)); // alloc: once a give, shared with the task
        // alloc: once a give, shared with the task so a refused task gives the front end back
        let kept = Arc::new(Lock::new("serve.job.give_values.kept", Some(front)));
        let does = Does::GiveValues {
            kept: Arc::clone(&kept), // clone: a refcount, once a give
            checkout: checkout.to_owned(), // alloc: the task's span's name, once a give
            bytes,
            wanted: Arc::clone(&wanted), // clone: a refcount, once a give
        };
        // The task is no job's: nothing cancels it but its own end.
        let never = Arc::new(AtomicBool::new(false)); // alloc: once a give
        let work = Step::of(&never, does);
        // No job looks at this request: marked so its finish is forgotten as a store's is.
        self.store_requests.insert(request);
        match w.plane.add(w.now, request, fresh, Some(work), false) {
            Ok(()) => {
                self.roots.storing[root as usize] += 1;
                // alloc: none past the checkouts given at once
                self.giving.out.push(Out { root, node, kept, wanted, span, wait: None });
            }
            Err(error) => {
                self.roots.fronts[root as usize] = kept.lock().take();
                span.record("gave", 0u64);
                crate::serve::said::logged!(WARN,
                    "blaze serve: the plane refused the task giving back values of the kept front end of {checkout}, \
                     so nothing of it is given: {error}"
                );
            }
        }
        w.plane.close(w.now, request);
    }

    /// The task giving back a kept front end's values ended, its value `front` and what went:
    /// the front end is its checkout's again, the planner measures the give by a reading
    /// after now, and the lowerings held for it are posted. A task that panicked gave no
    /// front end back (it may be half swept): said, and the checkout's next build lowers anew.
    pub(super) fn gave_values(
        &mut self,
        w: &mut World<'_, S>,
        node: NodeId,
        front: Option<S::KeptFront>,
        gave: u64,
    ) {
        let Some(at) = self.giving.out.iter().position(|it| it.node == node) else {
            tracing::error!(
                target: "blaze",
                node = node.0,
                "a give-back of values ended that the loop holds no record of; its front end is dropped"
            );
            return;
        };
        let Out { root, span, wait, .. } = self.giving.out.swap_remove(at);
        span.record("gave", gave);
        drop(wait);
        drop(span);
        self.roots.storing[root as usize] -= 1;
        match front {
            Some(front) => self.roots.fronts[root as usize] = Some(front),
            None => {
                let checkout = self.checkout_of(root).to_owned(); // alloc: a loss, once
                #[cfg(test)]
                sim::dropped(&checkout);
                let line = format!( // alloc: a loss, once
                    "analysis of {} was dropped: the task giving back its values for memory did not give it back (it panicked, or was stopped mid-sweep); the next build redoes it",
                    project(&checkout)
                );
                crate::serve::said::logged!(ERROR, "blaze serve: {line} ({checkout})");
                self.roots.lost[root as usize] = Some(line);
            }
        }
        let building = self.building();
        let idle = !building && self.order.is_empty() && self.giving.idle_at(w.now);
        let now = self.memory(w, 0, building, idle).now(w.now);
        self.giving.planner.done(&now);
        self.lower_waiting(w, root);
    }

    /// Node `node` settled: if it is a give of values whose value never came (its task was
    /// cancelled or failed before it ran, or its run gave none), the give ends here, with the
    /// front end the task left where it was shared, if it did. No lowering waits on a give
    /// that can no longer end.
    pub(super) fn give_settled(&mut self, w: &mut World<'_, S>, node: NodeId) {
        let Some(out) = self.giving.out.iter().find(|it| it.node == node) else { return };
        let front = out.kept.lock().take();
        self.gave_values(w, node, front, 0);
    }

    /// Writes the pages of checkout `root`'s members, and of the crates above them, from its
    /// kept front end: the front end's second store ([`Steps::store_kept`] on one its build's
    /// end stored), by a task of an idle plane request of its own, as a build's end posts its
    /// first ([`Jobs::store_kept`]). No job's: it settles as what an answered job left does
    /// (`Settling`, named by [`members_job`]), its puts made by `stored_kept`, the front end
    /// the checkout's again once they are, and what is left to settle gone once they are
    /// durable; a lowering of the checkout meanwhile waits for it as for any store. Whether a
    /// task was posted: none when the checkout keeps no front end now, or the plane refused
    /// it (said; the front end is dropped with the task, as a refused store's is).
    pub(super) fn store_members(&mut self, w: &mut World<'_, S>, root: u32) -> bool {
        // A store that broke makes nothing durable again (the daemon stops, failed naming it):
        // pages put now could only be lost, and the loss of what answered builds left is said
        // already. The front end stays the checkout's, and goes with the daemon.
        if w.store.broken().is_some() {
            return false;
        }
        let Some(front) = self.roots.fronts.get_mut(root as usize).and_then(Option::take) else {
            return false;
        };
        let checkout = self.checkout_of(root).to_owned(); // alloc: a store's name, once a store
        let request = w.plane.open(w.now, false, true);
        let name = super::named("storing the members of the kept front end of ", &checkout);
        let fresh = super::lone_learned(
            w.plane,
            NodeKind::Store,
            &name,
            Pool::Work,
            kinds::STORE_KEPT,
            super::store_cost_key(&checkout),
        );
        let node = fresh.first();
        let wanted = Arc::new(AtomicBool::new(false)); // alloc: once a store, shared with the task
        let does = Does::StoreKept {
            steps: Arc::clone(w.steps), // clone: a refcount, once a store
            kept: Lock::new("serve.job.store_members.kept", Some(front)),
            checkout: checkout.clone(), // clone: the task's span's name, once a store
            build: 0,
            logs: false,
            wanted: Arc::clone(&wanted), // clone: a refcount, once a store
            filing: None,
        };
        // The task is no job's: nothing cancels it but its own end.
        let never = Arc::new(AtomicBool::new(false)); // alloc: once a store
        let work = Step::of(&never, does);
        let command = "store the workspace members' pages of a kept front end";
        self.store_requests.insert(request);
        let posted = match w.plane.add(w.now, request, fresh, Some(work), false) {
            Ok(()) => {
                self.roots.storing[root as usize] += 1;
                self.root_members(root, Members::Writing);
                // alloc: once a store
                self.settling.push(super::Settling {
                    job: members_job(root),
                    command: command.to_owned(),
                    checkout: Some(checkout.clone()), // clone: the checkout's path, once a store
                    view: checkout.clone(), // clone: as above
                    storing: Some(super::Storing {
                        job: members_job(root),
                        command: command.to_owned(),
                        request,
                        node,
                        root,
                        checkout,
                        wanted,
                        promoted: false,
                        value: None,
                    }),
                    stored: blaze_cas::Tickets::default(),
                    durable: blaze_cas::Tickets::default(),
                    layer: blaze_cas::Tickets::default(),
                    layer_since: None,
                    layer_wait: None,
                    placing: 0,
                    placing_span: None,
                    leases: None,
                    since: None,
                    wait: None,
                });
                true
            }
            Err(error) => {
                crate::serve::said::logged!(WARN,
                    "blaze serve: the plane refused the task storing the workspace members' pages of the kept front end of \
                     {checkout}, so the front end is dropped with it and its next build lowers anew: {error}"
                );
                false
            }
        };
        w.plane.close(w.now, request);
        posted
    }

    /// Sets what the give-back rule knows of checkout `root`'s members.
    fn root_members(&mut self, root: u32, members: Members) {
        self.giving.root(root).members = members;
    }

    /// At a stop: the members of every kept front end not written yet are written
    /// ([`Jobs::store_members`]), and the stop waits for them as for what answered builds left
    /// to settle (`Daemon::stop`). How many stores were posted.
    pub fn store_all_members(&mut self, w: &mut World<'_, S>) -> usize {
        let mut posted = 0;
        for at in 0..self.roots.fronts.len() {
            let root = at as u32;
            let members = self.giving.roots.get(at).map_or(Members::Unwritten, |it| it.members);
            if self.roots.fronts[at].is_some()
                && members == Members::Unwritten
                && self.store_members(w, root)
            {
                posted += 1;
            }
        }
        posted
    }

    /// Lets go of checkout `checkout`'s kept front end, whole: its next build lowers anew,
    /// and says why (`blaze why`, through `Roots::lost`). What its storage held.
    fn give_front_end(&mut self, give: &Give, checkout: &str) -> Option<u64> {
        let front = self.roots.fronts.get_mut(give.owner as usize)?.take()?;
        #[cfg(test)]
        sim::dropped(checkout);
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        // The span and the line a front end let go has had since before this rule: the guard
        // counts both (`scripts/src/bin/guards.rs`).
        let _span =
            tracing::info_span!(target: "blaze", "kept front ends let go for memory", short = give.why.need(), kept = 1)
                .entered();
        match give.why {
            Why::Ask(short) => crate::serve::said::logged!(WARN,
                "blaze serve: the kept front end of {checkout} is let go, and its next build lowers anew: \
                 a task waits for {:.2} GiB the process holds besides its tasks, and nothing cheaper is left to give \
                 ({:.2} GiB freed, {:.0} s to make again)",
                gib(short),
                gib(give.bytes),
                give.remake_ns as f64 / 1e9
            ),
            Why::Over(over) => crate::serve::said::logged!(WARN,
                "blaze serve: the kept front end of {checkout} is let go, and its next build lowers anew: \
                 blaze holds {:.2} GiB, {:.2} past its limit of {:.2} GiB, and nothing cheaper is left to give \
                 ({:.2} GiB freed, {:.0} s to make again)",
                gib(give.footprint),
                gib(over),
                gib(give.limit),
                gib(give.bytes),
                give.remake_ns as f64 / 1e9
            ),
        }
        let line = giveback::dropped_line(project(checkout), give);
        let lost = &mut self.roots.lost[give.owner as usize];
        *lost = Some(match lost.take() {
            Some(before) => format!("{before}; {line}"), // alloc: a note, once a front end let go
            None => line,
        });
        let held = front.held();
        drop(front);
        Some(held)
    }

    /// The system said memory is short (docs/memory-budget.md section 7, "Pressure"): every
    /// kept front end gives the values it keeps by bytes, all of them, on a worker
    /// ([`Jobs::give_values`]), under a `memory give-back` span each; none is dropped whole.
    /// The daemon purges at the same event and its pools are asked for a slice a turn while the
    /// pressure stands (`Daemon::pressure`, [`Jobs::ask_pools`]), and a
    /// limit that falls under what the daemon holds is [`Jobs::give_back`]'s to answer, in its
    /// order.
    fn press_kept(&mut self, w: &mut World<'_, S>) {
        for at in 0..self.roots.fronts.len() {
            if self.roots.fronts[at].is_none() {
                continue;
            }
            let root = at as u32;
            let checkout = self.checkout_of(root).to_owned(); // alloc: a give's name, once a give
            let span = tracing::info_span!(
                target: "blaze",
                "memory give-back",
                what = Rung::Values.name(),
                owner = checkout.as_str(),
                bytes = u64::MAX,
                why = "the system says memory is short",
                gave = tracing::field::Empty,
            );
            #[cfg(test)]
            sim::GAVE.with_borrow_mut(|it| it.push((Rung::Values, checkout.clone(), u64::MAX))); // clone: a test's record
            self.give_values(w, root, &checkout, u64::MAX, span);
        }
    }
}

/// What a test of the daemon's simulation says of memory, and what it reads back
/// (`serve::tests::giving`): the simulated daemon's footprint is a model here, since the
/// test process's own is no part of it. Thread-local: the simulation runs on the test's
/// thread, tasks included.
#[cfg(test)]
pub(crate) mod sim {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        time::Duration,
    };

    use blaze_graph::giveback::{Rung, Source};

    use super::{CHUNKS, Memory, NAMES, TABLES};

    /// The planted bugs of the idle budget's tests, each the control of one.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) enum Plant {
        #[default]
        Nothing,
        /// The floor ignores the idle wait: the front end built last is kept however long the
        /// daemon is idle.
        IdleKeepsFloor,
        /// The process tables are listed whatever else is kept.
        TablesAlways,
        /// Every idle wait is zero: the budget holds the moment the daemon has no job.
        NoWait,
        /// A front end's wait is the one 30 s constant, whatever its remake costs.
        ConstantWait,
        /// An idle front end is let go at once, its members' pages unwritten
        /// ([`super::Jobs::store_members`] skipped).
        DropsUnwritten,
    }

    /// The model's memory besides the kept front ends.
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct Told {
        pub limit: u64,
        /// What the tasks and everything not listed below hold.
        pub other: u64,
        /// What the allocators keep freed that a purge returns, and what their estimate shows
        /// that it does not.
        pub kept: u64,
        pub stuck: u64,
        /// The task that waits to start, by its bytes; 0 for none.
        pub task: u64,
        /// What the process-wide caches hold: warm chunks, the flight reader's names, the
        /// process tables (`Jobs::process_held`); each a cache give of its owner returns.
        pub chunks: u64,
        pub names: u64,
        pub tables: u64,
    }

    /// A kept front end of the model: what it holds that only dropping it frees, and its
    /// values.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Front {
        pub core: u64,
        pub values: u64,
    }

    thread_local! {
        /// The model, while a test says one: the driver then reads it, not the process.
        pub(crate) static TOLD: Cell<Option<Told>> = const { Cell::new(None) };
        /// The kept front ends, by checkout: what the fakes' `KeptDb` answers from.
        pub(crate) static FRONTS: RefCell<BTreeMap<String, Front>> = const { RefCell::new(BTreeMap::new()) };
        /// Every give the driver did, in order: the rung, whose, the bytes asked.
        pub(crate) static GAVE: RefCell<Vec<(Rung, String, u64)>> = const { RefCell::new(Vec::new()) };
        /// The planted bug: the driver gives with no plan (every kept front end, for any wait).
        pub(crate) static PLANT_NO_PLAN: Cell<bool> = const { Cell::new(false) };
        /// The idle budget's planted bug, while a test plants one.
        pub(crate) static PLANT: Cell<Plant> = const { Cell::new(Plant::Nothing) };
        /// What a front end's remake took, as the driver records it once kept; `None`: what the
        /// simulation's clock measured.
        pub(crate) static REMAKE: Cell<Option<Duration>> = const { Cell::new(None) };
        /// The sources of the driver's last listing, as it listed them.
        pub(crate) static LISTED: RefCell<Vec<Source>> = const { RefCell::new(Vec::new()) };
        /// At each give of the process tables, how many front ends were kept.
        pub(crate) static TABLES_GIVEN: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
        /// The driver's turns under the model: each is a footprint reading, taken after any
        /// purge.
        static TURNS: Cell<u64> = const { Cell::new(0) };
    }

    /// The model as the driver reads it this turn; `None` while no test says one.
    pub(super) fn memory() -> Option<Memory> {
        let told = TOLD.get()?;
        let turn = TURNS.get() + 1;
        TURNS.set(turn);
        let footprint = footprint_of(&told);
        let ask = match told.task {
            0 => 0,
            task => (footprint + task).saturating_sub(told.limit).min(footprint),
        };
        Some(Memory {
            limit: told.limit,
            exact: false,
            footprint,
            kept: told.kept + told.stuck,
            readings: turn,
            purged: turn,
            ask,
        })
    }

    /// The model's footprint: everything it holds.
    fn footprint_of(told: &Told) -> u64 {
        let fronts: u64 = FRONTS.with_borrow(|it| it.values().map(|it| it.core + it.values).sum());
        told.other + told.kept + told.stuck + told.chunks + told.names + told.tables + fronts
    }

    /// The model's footprint now; 0 while no test says one.
    pub(crate) fn footprint() -> u64 {
        TOLD.get().map_or(0, |told| footprint_of(&told))
    }

    /// The driver listed `sources`.
    pub(super) fn listed(sources: &[Source]) {
        LISTED.with_borrow_mut(|it| {
            it.clear();
            it.extend_from_slice(sources); // alloc: a test's record
        });
    }

    /// A cache give of `owner` with `fronts` front ends kept: what the model held of it goes,
    /// and that much is what went.
    pub(super) fn gave_cache(owner: u32, fronts: usize) -> u64 {
        let Some(told) = TOLD.get() else { return 0 };
        let (gave, told) = match owner {
            CHUNKS => (told.chunks, Told { chunks: 0, ..told }),
            NAMES => (told.names, Told { names: 0, ..told }),
            TABLES => {
                TABLES_GIVEN.with_borrow_mut(|it| it.push(fronts)); // alloc: a test's record
                (told.tables, Told { tables: 0, ..told })
            }
            _ => (0, told),
        };
        TOLD.set(Some(told));
        gave
    }

    /// A purge: what the model's allocators kept freed is returned.
    pub(super) fn purged() {
        if let Some(told) = TOLD.get() {
            TOLD.set(Some(Told { kept: 0, ..told }));
        }
    }

    /// The kept front end of `checkout` was dropped.
    pub(super) fn dropped(checkout: &str) {
        FRONTS.with_borrow_mut(|it| it.remove(checkout));
    }

    /// What `checkout`'s kept front end holds, its values, and a give of `bytes` of them: the
    /// fakes' `KeptDb` (`serve::tests::FakeInputs`). 0 for a checkout the model has none of.
    pub(crate) fn held(checkout: &str) -> u64 {
        FRONTS.with_borrow(|it| it.get(checkout).map_or(0, |it| it.core + it.values))
    }

    pub(crate) fn values(checkout: &str) -> u64 {
        FRONTS.with_borrow(|it| it.get(checkout).map_or(0, |it| it.values))
    }

    pub(crate) fn give(checkout: &str, bytes: u64) -> u64 {
        FRONTS.with_borrow_mut(|it| {
            let Some(front) = it.get_mut(checkout) else { return 0 };
            let went = bytes.min(front.values);
            front.values -= went;
            went
        })
    }

    /// Clears the model: a test's first and last act.
    pub(crate) fn reset() {
        TOLD.set(None);
        FRONTS.with_borrow_mut(BTreeMap::clear);
        GAVE.with_borrow_mut(Vec::clear);
        PLANT_NO_PLAN.set(false);
        PLANT.set(Plant::Nothing);
        REMAKE.set(None);
        LISTED.with_borrow_mut(Vec::clear);
        TABLES_GIVEN.with_borrow_mut(Vec::clear);
        TURNS.set(0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::{SLICE_MIN, SLICES, give_in_slices, project, unmapped_images};
    use crate::serve::{
        job::Paths,
        tests::{Fake, FakeImage},
        work::{Image, KeptDb},
    };

    /// Paths keeping three images, 60, 30 and 40 bytes, the second live as `live` says.
    fn kept_images(live: bool) -> Paths<Fake> {
        let image = |path: &str, held: u64, live: bool| Image::<Fake> { path: path.into(), kept: FakeImage { held, live } };
        let mut paths = Paths::<Fake>::new(u64::MAX);
        paths.keep_one(&[], image("/w/target/debug/a", 60, false), true);
        paths.keep_one(&[], image("/w/target/debug/run", 30, live), true);
        paths.keep_one(&[], image("/w/target/debug/c", 40, false), true);
        paths
    }

    /// The invariant: the idle budget's give of the kept link images (`Paths::evict_to(0)`)
    /// lets go of every image no run maps and never a live one, whose patches go into memory
    /// the running program maps; what the source lists is exactly what goes.
    #[test]
    fn a_live_image_outlasts_the_idle_budget() {
        let mut paths = kept_images(true);
        assert_eq!(unmapped_images(&paths), 100, "the source lists the images no run maps");
        paths.evict_to(0);
        let run = paths.intern("/w/target/debug/run");
        assert!(paths.images[run as usize].is_some(), "the live image outlasts the budget");
        assert_eq!((paths.kept, paths.weights.held(), unmapped_images(&paths)), (1, 30, 0));
    }

    /// The control: the same images with `live` cleared all go, the one the run mapped too.
    #[test]
    fn the_oracle_catches_a_live_image_let_go() {
        let mut paths = kept_images(false);
        paths.evict_to(0);
        let run = paths.intern("/w/target/debug/run");
        assert!(paths.images[run as usize].is_none(), "the image the oracle keeps went, as it should here");
        assert_eq!((paths.kept, paths.weights.held()), (0, 0));
    }

    #[test]
    fn a_project_is_named_by_its_checkouts_last_component() {
        assert_eq!(project("/root/wt/reth-1112/reth"), "reth");
        assert_eq!(project("/root/wt/tempo/"), "tempo");
        assert_eq!(project("reth"), "reth");
    }

    /// A front end of values alone, which counts its sweeps and may set `wanted` after one.
    struct Values {
        values: u64,
        sweeps: u64,
        wants_after: Option<(u64, &'static AtomicBool)>,
    }

    impl KeptDb for Values {
        fn db(&self) -> Option<&base_db::Db> {
            None
        }

        fn give_values(&mut self, bytes: u64) -> u64 {
            self.sweeps += 1;
            if let Some((after, wanted)) = self.wants_after
                && self.sweeps == after
            {
                wanted.store(true, std::sync::atomic::Ordering::Release);
            }
            let went = bytes.min(self.values);
            self.values -= went;
            went
        }
    }

    /// The invariant: a give of values is swept in at most [`SLICES`] sweeps and one, gives
    /// what was asked where the front end has it, and stops at the first sweep's end after a
    /// build asks for the front end (the bound of that build's wait).
    #[test]
    fn a_give_of_values_is_at_most_eight_sweeps_and_stops_for_a_build_that_waits() {
        static NEVER: AtomicBool = AtomicBool::new(false);
        static WANTED: AtomicBool = AtomicBool::new(false);
        let gib = 1u64 << 30;
        let mut front = Values { values: 20 * gib, sweeps: 0, wants_after: None };
        assert_eq!(give_in_slices(&mut front, 16 * gib, &NEVER), 16 * gib);
        assert_eq!((front.sweeps, front.values), (SLICES, 4 * gib));
        // A small give is one sweep.
        let mut front = Values { values: 20 * gib, sweeps: 0, wants_after: None };
        assert_eq!(give_in_slices(&mut front, SLICE_MIN / 2, &NEVER), SLICE_MIN / 2);
        assert_eq!(front.sweeps, 1);
        // A front end with less than asked ends at the sweep that gives nothing.
        let mut front = Values { values: gib, sweeps: 0, wants_after: None };
        assert_eq!(give_in_slices(&mut front, 16 * gib, &NEVER), gib);
        assert_eq!(front.sweeps, 2);
        // A build asks during the third sweep: the give stops at its end.
        let mut front = Values { values: 20 * gib, sweeps: 0, wants_after: Some((3, &WANTED)) };
        assert_eq!(give_in_slices(&mut front, 16 * gib, &WANTED), 6 * gib);
        assert_eq!(front.sweeps, 3);
    }
}
