//! What the daemon gives back of its core: the thin driver of `blaze_graph::giveback::Planner`
//! (docs/v2/plan/design/end-state/memory.md 4.1, 5 and 9.3).
//!
//! The memory gate (`blaze_mem::gate`) bounds every commit: a task that does not fit is wounded
//! by age or fails by name, so nothing here stops a task, sweeps a pool or answers a task held
//! back for memory. What is left is the core, the fixed owner that holds the query engine's
//! heap: until memos are records, a kept front end's values, a whole kept front end, codegen's
//! kept walks and the flight reader's names are heap values in it, and a core that fills
//! refuses its next commit. Once a loop turn ([`Jobs::give_back`], from `Jobs::apply_writes`)
//! the planner is told the gate's count of core against the core's line; when it would plan,
//! the sources are listed and its gives done:
//!
//! | rung | the source | the give |
//! |---|---|---|
//! | allocator | what mimalloc keeps committed past its live blocks (`Snapshot::heap_committed` less `arena::held`), an estimate | a purge on the purger's thread (`raw::memory::purger`), done once it serves a ticket asked after |
//! | caches | codegen's kept walks (`Steps::kept_caches`), listed only while no task that may walk a program runs (`Plane::walks_in_flight`) | `Steps::give_caches` |
//! | caches ([`CHUNKS`]) | the warm chunks no owner holds (`blaze_mem::chunks::cached`) | `blaze_mem::chunks::trim` |
//! | caches ([`NAMES`]) | the names the flight reader keeps of past generations (`blaze_profile::flight::names_held`) | `blaze_profile::flight::trim_names` |
//! | values | each kept front end's values kept by bytes (`KeptDb::values`); the builds under way's MIR and inference, all or nothing ([`RUNNING`]) | a task on a worker ([`Jobs::give_values`]); a release made due (`Writer::pressed`) |
//! | front end | each kept front end (`KeptDb::held`), the checkout built last the floor unless a build is under way (then that build's own front end is what the edit loop needs) | dropped: its checkout's next build lowers anew, and its `blaze why` says so |
//!
//! Past the last rung with core past its line, the floor is said by name once a stretch; past
//! the core's size the gate refuses the next commit, naming the region's tag: the typed end,
//! with no fallback.
//!
//! A kept front end's values are swept on a worker, never the loop: the front end is taken
//! from its checkout as it is for its store, a task of kind `GIVE_BACK` sweeps it in at most
//! [`SLICES`] slices and gives it back in its value ([`Jobs::gave_values`]). A lowering of
//! that checkout asked meanwhile waits for it, as it waits for a store, under a span that
//! names the wait; the wait is bounded: the task stops at the first slice's end after a
//! build asks ([`give_in_slices`]). While a give is out nothing more is planned, and the
//! planner is told it is done once it is back (`Planner::done`).
//!
//! Idleness (`serve::idle`'s rule, 2026-11-13). Nothing here is given for idleness but what
//! costs nothing to make again: once the daemon has had no job for [`idle::AFTER`], blaze-mem's
//! warm chunks and the flight reader's past names, once a stretch ([`Jobs::trim_idle`]); the
//! purge is the loop's own (`Daemon::plot`). No front end, value, kept walk or link image is let
//! go for idleness: the daemon exits instead, once it has been idle 64 times the dearest kept
//! front end's last remake (30 s to 30 minutes, [`Jobs::exit_wait`], `idle::exit_after`),
//! through `Daemon::stop`, which writes each kept front end's members first. Until 2026-11-13
//! an idle daemon was held to a 48 MiB budget by this ladder (D29): it dropped the hello-world
//! front end, its kept walks, its link images and the process tables at 30 s, so the next body
//! edit took 150.2 ms where one with no idle took 42.3, and the footprint fell only from 3,140
//! to 2,764 MB, since the bytes those drops freed stayed in the workers' heaps and in what no
//! drop reaches (`serve::idle`'s doc).
//!
//! Deleted with the gate (2026-11-13): the store pools' rung and its slicer, the running
//! tasks' rung (`Writer::stop_for_memory`) and the typed failure of a task alone past the
//! footprint's fail line (the gate's `TooBig` names it), the footprint reading and the
//! machine's yield as inputs, the daemon's word of what it could give (`Plane::set_gives`),
//! and the forecasts of codegen walks (`Plane::walked`, `forecast_instances`: the plane's one
//! predictor is its peak record).
//!
//! Every give is a `memory give-back` span and event (what, whose, how much, what core holds
//! against its line, what it costs to make again), which `blaze explain memory` reads. A front
//! end let go keeps the span and the log line it had before this (`kept front ends let go for
//! memory`, "is let go, and its next build lowers anew"), which the guard counts
//! (`scripts/src/bin/guards.rs`).

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
    giveback::{self, Give, Now, Outcome, Planner, Rung, Source},
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

/// The owner of codegen's kept walks (a [`Rung::Caches`] source).
const WALKS: u32 = u32::MAX - 1;

/// The owner of blaze-mem's warm cached chunks (a [`Rung::Caches`] source).
const CHUNKS: u32 = u32::MAX - 2;

/// The owner of the names the flight reader keeps (a [`Rung::Caches`] source).
const NAMES: u32 = u32::MAX - 3;

/// What a [`Rung::Caches`] give of `owner` is named by, in its span and `blaze explain memory`.
const fn cache_name(owner: u32) -> &'static str {
    match owner {
        CHUNKS => "chunks",
        NAMES => "flight names",
        _ => "codegen's kept walks",
    }
}

/// How often, at most, the sources are listed again after a plan that had nothing to give: a
/// listing weighs every kept front end's values.
const PLAN_EVERY: Duration = Duration::from_millis(250);

/// How often, at most, the builds under way are made to release (`Writer::pressed`): what
/// it lets go is the release's own rule, so the source does not fall at once.
const RUNNING_EVERY: Duration = Duration::from_secs(1);

/// The most slices a give of values is swept in ([`give_in_slices`]): a build that asks for
/// the front end meanwhile waits for one slice's end at most, an eighth of the give.
pub(super) const SLICES: u64 = 8;

/// The least a slice gives: 256 MiB, so a small give is one sweep.
const SLICE_MIN: u64 = 256 << 20;

/// What the process-wide caches hold ([`Jobs::process_held`]), in bytes.
#[derive(Clone, Copy, Debug)]
struct Process {
    /// blaze-mem's warm cached chunks ([`CHUNKS`]).
    chunks: u64,
    /// The flight reader's names ([`NAMES`]).
    names: u64,
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
    /// Whether `cost_ns` was measured: a daemon that keeps a front end never made from nothing
    /// in it waits the longest before it exits ([`idle::EXIT_MOST`], [`Jobs::exit_wait`]).
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
    /// When the builds under way were last made to release.
    running_asked: Option<Duration>,
    /// The floor was said not to fit, in this stretch past the line.
    floor_said: bool,
    /// The system said memory is short since the last turn ([`Jobs::pressed`]).
    pressed: bool,
    /// The gives of values under way, a checkout each.
    out: Vec<Out<S>>,
    /// Since when the daemon has had no job.
    idle_since: Option<Duration>,
    /// The caches that cost nothing to make again were trimmed in this idle stretch
    /// ([`Jobs::trim_idle`]).
    trimmed: bool,
}

impl<S: Steps> Giving<S> {
    pub(super) fn new() -> Giving<S> {
        Giving {
            planner: Planner::new(),
            sources: Vec::new(), // alloc: kept scratch, none until core is past its line
            gives: Vec::new(),   // alloc: kept scratch, none until core is past its line
            roots: Vec::new(),   // alloc: one entry a checkout
            last_built: None,
            idle_until: None,
            running_asked: None,
            floor_said: false,
            pressed: false,
            out: Vec::new(), // alloc: none until values are given
            idle_since: None,
            trimmed: false,
        }
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

    /// How long the daemon waits, idle and held by no one, before it exits (`serve::idle`'s
    /// rule 2, `idle::exit_due`): [`idle::EXIT_UNWORKED`] where it built nothing and took no
    /// save since its start (`worked`); else [`idle::exit_after`] of what the next edit would
    /// pay again without it, the dearest kept front end's last lowering from nothing
    /// ([`idle::EXIT_MOST`] for one never measured, [`idle::EXIT_LEAST`] with none kept).
    /// Under the simulation's planted ladders, never within a test ([`idle::EXIT_MOST`]): the
    /// daemon they model stays.
    pub fn exit_wait(&self, worked: bool) -> Duration {
        #[cfg(test)]
        if sim::PLANT.get() != sim::Plant::Nothing {
            return idle::EXIT_MOST;
        }
        if !worked {
            return idle::EXIT_UNWORKED;
        }
        let mut dearest = Duration::ZERO;
        for (at, front) in self.roots.fronts.iter().enumerate() {
            if front.is_none() {
                continue;
            }
            let remake = match self.giving.roots.get(at) {
                Some(root) if root.measured => Duration::from_nanos(root.cost_ns),
                _ => return idle::EXIT_MOST,
            };
            dearest = dearest.max(remake);
        }
        idle::exit_after(dearest)
    }

    /// What the planner decides on at `at`: the gate's core, its size and what core regions
    /// hold committed (exact: every commit is counted as it is made), and the purges served.
    /// Read of the process's gate (`blaze_mem::gate::process`), the one `Daemon::new`
    /// installs.
    /// Under the daemon's simulation a test says them ([`sim::TOLD`]): the test process's own
    /// gate is no part of a simulated daemon.
    fn core_now(&self, at: Duration) -> Now {
        #[cfg(test)]
        if let Some(now) = sim::core(at) {
            return now;
        }
        let gate = blaze_mem::gate::process();
        let purged = crate::raw::memory::purger().done().unwrap_or_else(|why| {
            // No purge is served again: the allocator's rung is then never done, and the
            // planner goes on past it after its bound (`giveback::MEASURE_WITHIN_NS`).
            tracing::error!(target: "blaze", why, "memory give-back: the purger stopped; no purge it asks is served");
            0
        });
        // Two atomic loads: no lock, once a loop turn (`Gate::core`).
        let (core, used) = gate.core();
        Now { at_ns: ns(at), core, used, purged }
    }

    /// What the allocator keeps committed past its live blocks: what a purge can return of
    /// core, an estimate and an upper bound (mimalloc's committed count is not lowered by a
    /// purge, so the estimate may not fall: the planner's retry says so,
    /// `giveback::Rung::retry_ns`). Under the simulation, the model's.
    fn allocator_kept(&self) -> u64 {
        #[cfg(test)]
        if let Some(told) = sim::TOLD.get() {
            return told.kept + told.stuck;
        }
        let committed = blaze_mem::tag::snapshot().heap_committed.unwrap_or(0);
        committed.saturating_sub(blaze_graph::arena::held())
    }

    /// What the builds under way hold in MIR and inference, which a release lets go of where
    /// nothing still to compile reads it. Under the simulation none: a simulated build holds no
    /// real MIR.
    fn running_values(&self) -> u64 {
        #[cfg(test)]
        if sim::TOLD.get().is_some() {
            return 0;
        }
        let snapshot = blaze_mem::tag::snapshot();
        let of = |tag: blaze_mem::Tag| {
            let it = snapshot.get(tag);
            it.live.saturating_add(it.heap)
        };
        of(blaze_mem::Tag::Mir).saturating_add(of(blaze_mem::Tag::Infer))
    }

    /// Once an idle stretch, after [`idle::AFTER`] with no job: the caches that cost nothing to
    /// make again go (`serve::idle`'s rule 1), blaze-mem's warm chunks and the flight reader's
    /// past names, each under its own span with what it held before and after. What the next
    /// edit makes again stays. Under the simulation's planted ladders (the rule before
    /// 2026-11-13, the controls of `serve::tests::giving`), every kept front end is dropped
    /// here too, its bytes freed ([`sim::Plant::DropsFreeing`]) or kept by the allocator
    /// ([`sim::Plant::DropsKeepingPages`]).
    fn trim_idle(&mut self, w: &mut World<'_, S>) {
        let _span = tracing::info_span!(target: "blaze", "idle: caches that cost nothing to make again").entered();
        self.give_cache(w, CHUNKS);
        self.give_cache(w, NAMES);
        #[cfg(test)]
        if sim::PLANT.get() != sim::Plant::Nothing {
            for at in 0..self.roots.fronts.len() {
                if self.roots.fronts[at].take().is_some() {
                    let checkout = self.checkout_of(at as u32).to_owned();
                    sim::GAVE.with_borrow_mut(|it| it.push((Rung::FrontEnd, checkout.clone(), 0))); // clone: a test's record
                    sim::dropped_idle(&checkout, sim::PLANT.get() == sim::Plant::DropsKeepingPages);
                }
            }
        }
    }

    /// One turn of giving back (the module's doc): the idle trim once a stretch, then one step
    /// of the core's rule. Called once a loop turn: with core under its line it reads two
    /// counts and returns.
    pub(super) fn give_back(&mut self, w: &mut World<'_, S>) {
        if std::mem::take(&mut self.giving.pressed) {
            self.press_kept(w);
        }
        let building = self.building();
        // A daemon with no job at all: since when, and the caches that cost nothing to make
        // again trimmed once a stretch past `idle::AFTER`.
        let jobless = !building && self.order.is_empty();
        match (jobless, self.giving.idle_since) {
            (true, None) => self.giving.idle_since = Some(w.now),
            (false, Some(_)) => (self.giving.idle_since, self.giving.trimmed) = (None, false),
            _ => {}
        }
        let trim = self.giving.idle_since.is_some_and(|since| w.now.saturating_sub(since) >= idle::AFTER);
        if trim && !std::mem::replace(&mut self.giving.trimmed, true) {
            self.trim_idle(w);
        }
        let now = self.core_now(w.now);
        #[cfg(test)]
        if sim::PLANT_NO_PLAN.get() {
            // The planted bug, and the rule before the ladder: the driver gives by itself,
            // with no plan, every kept front end once core is past its line.
            if now.over() > 0 {
                for at in 0..self.roots.fronts.len() {
                    if self.roots.fronts[at].take().is_some() {
                        let checkout = self.checkout_of(at as u32).to_owned();
                        sim::dropped(&checkout);
                    }
                }
            }
            return;
        }
        // A give of values is out on a worker: the planner is told once it is back.
        if !self.giving.out.is_empty() {
            return;
        }
        if !self.giving.planner.due(&now) {
            // Under the line, or awaiting a give: nothing is listed, and the planner reads
            // what a give left at its next step that is due.
            if now.over() == 0 {
                self.giving.floor_said = false;
                self.giving.idle_until = None;
            }
            return;
        }
        if self.giving.idle_until.is_some_and(|until| w.now < until) {
            return;
        }
        let mut gives = std::mem::take(&mut self.giving.gives);
        gives.clear();
        let span = tracing::info_span!(
            target: "blaze",
            "memory give-back plan",
            core = now.core,
            line = now.line(),
            used = now.used,
            outcome = tracing::field::Empty,
        )
        .entered();
        let mut sources = std::mem::take(&mut self.giving.sources);
        sources.clear();
        self.give_sources(w, building, &mut sources);
        #[cfg(test)]
        sim::listed(&sources);
        let outcome = self.giving.planner.step(&now, &sources, &mut gives);
        match outcome {
            Outcome::Gave { rung } => {
                span.record("outcome", rung.name());
                self.giving.idle_until = None;
                let mut on_worker = false;
                for give in &gives {
                    on_worker |= self.give(w, give);
                }
                // A give on the loop is done as it returns; the allocator's once a purge is
                // served, which the planner reads itself; a give on a worker once it is back
                // ([`Jobs::gave_values`]).
                if !on_worker {
                    self.giving.planner.done();
                }
            }
            Outcome::FloorOver { over, line, used, owner, kept } => {
                span.record("outcome", "the floor does not fit");
                self.giving.idle_until = Some(w.now + PLAN_EVERY);
                if !std::mem::replace(&mut self.giving.floor_said, true) {
                    let checkout = self.checkout_of(owner);
                    let said = giveback::floor_line(project(checkout), over, line, used, kept);
                    tracing::warn!(
                        target: "blaze",
                        checkout,
                        over,
                        line,
                        used,
                        kept,
                        "memory give-back: the floor does not fit under the core's line"
                    );
                    crate::serve::said::logged!(WARN, "blaze serve: {said}");
                    // The checkout's next build says it too (`blaze why`), unless a loss
                    // waits there already.
                    let lost = &mut self.roots.lost[owner as usize];
                    if lost.is_none() {
                        *lost = Some(said);
                    }
                }
            }
            Outcome::Nothing { over } => {
                span.record("outcome", "nothing to give");
                self.giving.idle_until = Some(w.now + PLAN_EVERY);
                tracing::debug!(
                    target: "blaze",
                    over,
                    "memory give-back: core is past its line and nothing is left to give: the builds under way hold it"
                );
            }
            Outcome::Enough | Outcome::Measuring { .. } => {}
        }
        drop(span);
        self.giving.sources = sources;
        self.giving.gives = gives;
    }

    /// What every owner can give now (the module's table), added to `sources`.
    fn give_sources(&mut self, w: &World<'_, S>, building: bool, sources: &mut Vec<Source>) {
        let cache = |owner, gives| Source { rung: Rung::Caches, owner, gives, grain: gives, remake_ns: 0, floor: false };
        let allocator_kept = self.allocator_kept();
        if allocator_kept != 0 {
            // alloc: none past the most sources, the scratch kept (every push below)
            sources.push(Source {
                rung: Rung::Allocator,
                owner: 0,
                gives: allocator_kept,
                grain: allocator_kept,
                remake_ns: 0,
                floor: false,
            });
        }
        // Codegen's kept walks, only while no task that may walk runs: one in flight keeps
        // its own table until it ends, so a give then frees nothing and loses its result.
        if w.plane.walks_in_flight() == 0 {
            let walks = w.steps.kept_caches();
            if walks != 0 {
                sources.push(cache(WALKS, walks));
            }
        }
        // The process's own caches: blaze-mem's warm chunks and the flight reader's names,
        // made again as they are next used.
        let process = self.process_held();
        if process.chunks != 0 {
            sources.push(cache(CHUNKS, process.chunks));
        }
        if process.names != 0 {
            sources.push(cache(NAMES, process.names));
        }
        // The floor is what the edit loop needs next: the front end of the checkout built
        // last, unless a build is under way, whose own front end (out with it) is that.
        let floor = self.giving.last_built.filter(|_| !building);
        for (at, front) in self.roots.fronts.iter_mut().enumerate() {
            let Some(front) = front else { continue };
            let owner = at as u32;
            let cost_ns = self.giving.roots.get(at).map_or(0, |it| it.cost_ns);
            let held = front.held();
            let values = front.values();
            if values != 0 {
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
        // release lets go of where nothing still to compile reads it. Made due at most once
        // a [`RUNNING_EVERY`]: what goes is the release's own rule.
        if !building || self.giving.running_asked.is_some_and(|at| w.now < at + RUNNING_EVERY) {
            return;
        }
        let gives = self.running_values();
        if gives != 0 {
            sources.push(Source { rung: Rung::Values, owner: RUNNING, gives, grain: gives, remake_ns: 0, floor: false });
        }
    }

    /// Whether a build's units are under way: a job at its units' stage.
    fn building(&self) -> bool {
        self.order.iter().any(|&id| {
            self.jobs.get(key_of(id)).is_some_and(|job| matches!(job.stage, Stage::Units(_)))
        })
    }

    /// Does one give (the module's table), under a span that says what, whose, how much, what
    /// core holds against its line and what it costs to make again. Whether it went to a
    /// worker, to be done once its task is back.
    fn give(&mut self, w: &mut World<'_, S>, give: &Give) -> bool {
        let checkout = match give.rung {
            Rung::Values | Rung::FrontEnd if give.owner != RUNNING => self.checkout_of(give.owner).to_owned(), // alloc: a give's name, once a give
            Rung::Values => "the builds under way".to_owned(), // alloc: a give's name, once a give
            Rung::Caches => cache_name(give.owner).to_owned(), // alloc: a give's name, once a give
            Rung::Allocator => String::new(),                  // alloc: none, empty
            Rung::FrontEnd => self.checkout_of(give.owner).to_owned(), // alloc: a give's name, once a give
        };
        #[cfg(test)]
        sim::GAVE.with_borrow_mut(|it| it.push((give.rung, checkout.clone(), give.bytes))); // clone: a test's record
        let span = tracing::info_span!(
            target: "blaze",
            "memory give-back",
            what = give.rung.name(),
            owner = checkout.as_str(),
            bytes = give.bytes,
            over = give.over,
            line = give.line,
            used = give.used,
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
            over = give.over,
            line = give.line,
            used = give.used,
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
            return self.give_values(w, give.owner, &checkout, give.bytes, span);
        }
        let entered = span.enter();
        let gave = match give.rung {
            Rung::Allocator => {
                self.purge();
                None
            }
            Rung::Caches => Some(self.give_cache(w, give.owner)),
            Rung::Values => {
                self.giving.running_asked = Some(w.now);
                for &id in &self.order {
                    let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
                    if let Stage::Units(build) = &mut job.stage {
                        build.writer.pressed();
                    }
                }
                None
            }
            Rung::FrontEnd => {
                // Let go for memory, never for idleness (`serve::idle`): a front end whose
                // members' pages are not written yet is said, since the next build, or the
                // next daemon, resolves its members from their sources.
                let members = self.giving.roots.get(give.owner as usize).map_or(Members::Unwritten, |it| it.members);
                if members != Members::Written {
                    crate::serve::said::logged!(WARN,
                        "blaze serve: the kept front end of {checkout} is let go for memory before its workspace members' pages \
                         were written: the next build, or the next daemon, resolves its members from their sources"
                    );
                }
                self.give_front_end(give, &checkout)
            }
        };
        drop(entered);
        if let Some(gave) = gave {
            span.record("gave", gave);
        }
        false
    }

    /// Asks a purge of what the allocators keep freed, on the purger's thread: done once it
    /// serves a ticket asked after this (`Now::purged`). Under the daemon's simulation the
    /// model's kept pages go ([`sim::purged`]): a simulated daemon purges no real heap.
    fn purge(&mut self) {
        #[cfg(test)]
        if sim::TOLD.get().is_some() {
            sim::purged();
            return;
        }
        crate::raw::memory::purger().ask();
    }

    /// What the process-wide caches hold now: blaze-mem's warm chunks and the flight reader's
    /// names. Under the daemon's simulation the model's ([`sim::Told`]): the test process's
    /// own are no part of a simulated daemon.
    fn process_held(&self) -> Process {
        #[cfg(test)]
        if let Some(told) = sim::TOLD.get() {
            return Process { chunks: told.chunks, names: told.names };
        }
        Process { chunks: blaze_mem::chunks::cached().warm, names: blaze_profile::flight::names_held() }
    }

    /// A [`Rung::Caches`] give of `owner`, under a span named for it with the bytes before and
    /// after: what went.
    fn give_cache(&mut self, w: &mut World<'_, S>, owner: u32) -> u64 {
        #[cfg(test)]
        if sim::TOLD.get().is_some() && matches!(owner, CHUNKS | NAMES) {
            return sim::gave_cache(owner);
        }
        // The give's own span (`memory give-back`) names the rung and the owner; this one is
        // named for the owner, with what it held before and after.
        let span = match owner {
            CHUNKS => tracing::info_span!(target: "blaze", "chunks", before = tracing::field::Empty, after = tracing::field::Empty),
            NAMES => tracing::info_span!(target: "blaze", "flight names", before = tracing::field::Empty, after = tracing::field::Empty),
            _ => tracing::info_span!(target: "blaze", "codegen's kept walks", before = tracing::field::Empty, after = tracing::field::Empty),
        };
        let _entered = span.enter();
        let (before, after) = match owner {
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
    /// module's doc): taken from the checkout, swept by a task of a plane request of its own,
    /// and the checkout's again once the task's value is back ([`Jobs::gave_values`]). `span`
    /// is the give's, open until then. Whether the task was posted: a plane that refuses it
    /// leaves the front end where it was, with nothing given, and says so.
    fn give_values(
        &mut self,
        w: &mut World<'_, S>,
        root: u32,
        checkout: &str,
        bytes: u64,
        span: tracing::Span,
    ) -> bool {
        let Some(front) = self.roots.fronts.get_mut(root as usize).and_then(Option::take) else {
            span.record("gave", 0u64);
            return false;
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
        let posted = match w.plane.add(w.now, request, fresh, Some(work), false) {
            Ok(()) => {
                self.roots.storing[root as usize] += 1;
                // alloc: none past the checkouts given at once
                self.giving.out.push(Out { root, node, kept, wanted, span, wait: None });
                true
            }
            Err(error) => {
                self.roots.fronts[root as usize] = kept.lock().take();
                span.record("gave", 0u64);
                crate::serve::said::logged!(WARN,
                    "blaze serve: the plane refused the task giving back values of the kept front end of {checkout}, \
                     so nothing of it is given: {error}"
                );
                false
            }
        };
        w.plane.close(w.now, request);
        posted
    }

    /// The task giving back a kept front end's values ended, its value `front` and what went:
    /// the front end is its checkout's again, the planner is told the give is done, and the
    /// lowerings held for it are posted. A task that panicked gave no front end back (it may
    /// be half swept): said, and the checkout's next build lowers anew.
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
        if self.giving.out.is_empty() {
            self.giving.planner.done();
        }
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
            tracing::info_span!(target: "blaze", "kept front ends let go for memory", over = give.over, kept = 1)
                .entered();
        crate::serve::said::logged!(WARN,
            "blaze serve: the kept front end of {checkout} is let go, and its next build lowers anew: \
             blaze's heap holds {:.2} GiB, {:.2} past its line of {:.2} GiB, and nothing cheaper is left to give \
             ({:.2} GiB freed, {:.0} s to make again)",
            gib(give.used),
            gib(give.over),
            gib(give.line),
            gib(give.bytes),
            give.remake_ns as f64 / 1e9
        );
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
    /// The daemon purges at the same event (`Daemon::pressure`), and a machine whose room falls
    /// under what the daemon's tasks hold is the gate's to answer (`Gate::room` wounds the
    /// youngest).
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
/// (`serve::tests::giving`): the simulated daemon's core is a model here, since the test
/// process's own gate is no part of it. Thread-local: the simulation runs on the test's
/// thread, tasks included.
#[cfg(test)]
pub(crate) mod sim {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        time::Duration,
    };

    use blaze_graph::giveback::{Now, Rung, Source};

    use super::{CHUNKS, NAMES};
    use crate::serve::plane::ns;

    /// The planted bugs of the idle rule's tests (`serve::idle`), each the control of one: the
    /// ladder this rule replaced, a daemon that stays and drops what is dear for idleness.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) enum Plant {
        #[default]
        Nothing,
        /// The idle daemon drops its kept front ends and stays, and the allocator keeps their
        /// pages (a drop on the loop's thread into the workers' heaps): a drop that frees no
        /// page counted as freed.
        DropsKeepingPages,
        /// The idle daemon drops its kept front ends and stays, their pages freed: the floor
        /// dropped while the daemon stays.
        DropsFreeing,
    }

    /// The model's core besides the kept front ends.
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct Told {
        /// The core's size.
        pub core: u64,
        /// What the builds under way and everything not listed below hold.
        pub other: u64,
        /// What the allocators keep freed that a purge returns, and what their estimate shows
        /// that it does not.
        pub kept: u64,
        pub stuck: u64,
        /// What the process-wide caches hold: warm chunks and the flight reader's names
        /// (`Jobs::process_held`); each a cache give of its owner returns.
        pub chunks: u64,
        pub names: u64,
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
        /// The planted bug: the driver gives with no plan (every kept front end, past the line).
        pub(crate) static PLANT_NO_PLAN: Cell<bool> = const { Cell::new(false) };
        /// The idle rule's planted bug, while a test plants one.
        pub(crate) static PLANT: Cell<Plant> = const { Cell::new(Plant::Nothing) };
        /// What a front end's remake took, as the driver records it once kept; `None`: what the
        /// simulation's clock measured.
        pub(crate) static REMAKE: Cell<Option<Duration>> = const { Cell::new(None) };
        /// The sources of the driver's last listing, as it listed them.
        pub(crate) static LISTED: RefCell<Vec<Source>> = const { RefCell::new(Vec::new()) };
        /// The purges the model served: each lands at once.
        static PURGES: Cell<u64> = const { Cell::new(0) };
    }

    /// The model as the driver reads it at `at`; `None` while no test says one.
    pub(super) fn core(at: Duration) -> Option<Now> {
        let told = TOLD.get()?;
        Some(Now { at_ns: ns(at), core: told.core, used: used_of(&told), purged: PURGES.get() })
    }

    /// What the model's core holds: everything it holds.
    fn used_of(told: &Told) -> u64 {
        let fronts: u64 = FRONTS.with_borrow(|it| it.values().map(|it| it.core + it.values).sum());
        told.other + told.kept + told.stuck + told.chunks + told.names + fronts
    }

    /// What the model holds now; 0 while no test says one.
    pub(crate) fn footprint() -> u64 {
        TOLD.get().map_or(0, |told| used_of(&told))
    }

    /// The driver listed `sources`.
    pub(super) fn listed(sources: &[Source]) {
        LISTED.with_borrow_mut(|it| {
            it.clear();
            it.extend_from_slice(sources); // alloc: a test's record
        });
    }

    /// A cache give of `owner`: what the model held of it goes, and that much is what went.
    pub(super) fn gave_cache(owner: u32) -> u64 {
        let Some(told) = TOLD.get() else { return 0 };
        let (gave, told) = match owner {
            CHUNKS => (told.chunks, Told { chunks: 0, ..told }),
            NAMES => (told.names, Told { names: 0, ..told }),
            _ => (0, told),
        };
        TOLD.set(Some(told));
        gave
    }

    /// A purge: what the model's allocators kept freed is returned, and the purge is served.
    pub(super) fn purged() {
        if let Some(told) = TOLD.get() {
            TOLD.set(Some(Told { kept: 0, ..told }));
        }
        PURGES.set(PURGES.get() + 1);
    }

    /// The kept front end of `checkout` was dropped.
    pub(super) fn dropped(checkout: &str) {
        FRONTS.with_borrow_mut(|it| it.remove(checkout));
    }

    /// The planted ladder dropped `checkout`'s kept front end for idleness: its bytes leave the
    /// model, or, `keeping`, stay as pages the allocator keeps and no purge returns (`stuck`),
    /// as a drop on the loop's thread into the workers' heaps leaves them.
    pub(super) fn dropped_idle(checkout: &str, keeping: bool) {
        let front = FRONTS.with_borrow_mut(|it| it.remove(checkout)).unwrap_or_default();
        if keeping && let Some(told) = TOLD.get() {
            TOLD.set(Some(Told { stuck: told.stuck + front.core + front.values, ..told }));
        }
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
        PURGES.set(0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::{SLICE_MIN, SLICES, give_in_slices, project};
    use crate::serve::work::KeptDb;

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
