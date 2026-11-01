//! The daemon's side of a lazy run, on threads: [`crate::fetch::Fetcher`] driven against the
//! compiler, the placer and the channel the daemon gives it.
//!
//! - [`Compiler`] compiles one instance: codegen's memo and the store first, the
//!   copy-and-patch backend (blaze-stencil) on a miss in dev and test runs.
//! - [`Placer`] places one compiled function into the run process's code memory with its
//!   relocations resolved (a callee not compiled yet is reached through its slot, whose stub
//!   is placed), then publishes its slot: one release store of the slot's word after the code
//!   is written and visible to the process's instruction fetch (blaze-link into
//!   blaze-inproc's code memory, whose publish does the cache maintenance).
//! - [`Answers`] sends a frame to a run process and stops one; [`Runs`] is blaze-inproc's, and
//!   [`Runner::serve`] reads one run process's asks.
//! - With a [`TierUp`], a run whose functions start at tier 0 (blaze-stencil) is tiered up:
//!   blaze-stencil's tier [`tier::Runner`] reads each function's counter cells through a
//!   [`Cells`] view, and recompiles a hot one with [`Compiler::tier_up`], which
//!   [`Placer::replace`] publishes in the function's slot, retiring the code it replaced
//!   behind blaze-inproc's fence; the fetcher is retargeted, so a late ask gets the new code.
//!   A function is bound to the tier runner once its first code is published, so a
//!   promotion never races its first compile; [`Runner::close`] stops the tier runner before
//!   the workers whose placer it uses.
//!
//! Workers take compiles from the fetcher, compile with no lock held, place one at a time,
//! and send the answers after the fetcher's lock is let go, before queuing the callees or
//! binding the tier-up. A worker with nothing to do spins a while before it sleeps (one at a
//! time), and a queued compile wakes one sleeper, not all. A lone ask whose code is compiled
//! already ([`Compiler::hit`]) skips the workers: the thread that read it places and answers
//! it, saving the hand-off, and what the compiler keeps of the hit for the store comes after the
//! answer ([`Compiler::hit_answered`]). A compile that panics or fails
//! answers `Failed` naming the function; a run that called it is stopped with that error,
//! never left to run something else. The fetcher's lock ([`Shared::state`]) is held only to
//! take or record a compile, never across one; the placer's only across one placement.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use blaze_codegen::{CompiledFunction, InstanceKey};
use blaze_hash::{HashMap, SharedMap};
use blaze_inproc::CellView;
use blaze_inproc::ask::{self, Reader, Sender};
use blaze_inproc::frame::Kind;
use blaze_items::{CodeHash, ItemKey};
use blaze_stencil::tier::{self, Tier};
use crossbeam_queue::SegQueue;
use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::{
    LazyId,
    fetch::{Action, Fetcher, Outcome, Priority, Waiter},
    session::Session,
    wire::{self, Frame, WireError},
};

/// Compiles one instance of the build.
pub trait Compiler: Send + Sync + 'static {
    fn compile(&self, key: InstanceKey) -> Result<CompiledFunction, String>;
    /// `key`'s code when it is compiled already (codegen's memo, the store's mapped index),
    /// found by a probe: no compile, no I/O, no lock held past the lookup. `None` when it is
    /// not, and [`Compiler::compile`] compiles it; an `Err` is the function's failure, as a
    /// compile's would be. Called on the thread that read the ask, which answers a hit itself.
    fn hit(&self, key: InstanceKey) -> Option<Result<CompiledFunction, String>>;
    /// After a [`Compiler::hit`] of `key` was placed and its askers answered, on the same
    /// thread: what the compiler keeps of it, off the askers' path. A panic stops the run.
    fn hit_answered(&self, _key: InstanceKey) {}
    /// The instance whose first code `key` is, compiled again at `to` (its backend,
    /// [`Tier::backend`], counting calls and back edges in cells when [`Tier::counts`]).
    fn tier_up(&self, key: InstanceKey, to: Tier) -> Result<CompiledFunction, String>;
}

/// Places one function in the running image and publishes its slot; the address it runs at.
pub trait Placer: Send + 'static {
    fn place(&mut self, function: &CompiledFunction) -> Result<u64, String>;
    /// Where the placed `key`'s counter cells are, as an offset of the code memory's cells
    /// part ([`CellView::load`]); none for a function that doesn't count.
    fn cells(&self, key: &InstanceKey) -> Option<u64>;
    /// Places `function`, a tier-up of the function whose first code is `slot`, and publishes
    /// it in `slot`'s slot: the address it runs at. The code it replaces is retired behind
    /// blaze-inproc's fence, freed only once every thread of the run was seen past it.
    fn replace(&mut self, slot: InstanceKey, function: &CompiledFunction) -> Result<u64, String>;
}

/// The run processes' asks.
pub trait Answers: Send + Sync + 'static {
    /// Sends `body` to `client` as a blaze-inproc frame of `kind`, answering the frame of
    /// sequence number `seq`.
    fn send(&self, client: u32, seq: u32, kind: Kind, body: &[u8]);
    /// The run called `function` (its instance and item; none for an id the session doesn't
    /// have), which can't be compiled: stop it with `why`.
    fn stop(&self, function: Option<(InstanceKey, ItemKey)>, why: &str);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Compiles at once.
    pub workers: u32,
    /// Whether every lazy function nothing asked for is compiled at [`Priority::Idle`] once
    /// nothing else is queued: idle cores' work, and how a failing function nobody called is
    /// found ([`Runner::close`]).
    pub idle: bool,
}

/// A tier-up of a run whose functions start at tier 0: the code memory's cells and how hot
/// a function must be for each tier.
pub struct TierUp {
    pub cells: CellView,
    pub policy: tier::Policy,
    pub config: tier::Config,
}

/// The run's counter cells as the tier runner reads them.
pub struct Cells(pub CellView);

impl tier::Cells for Cells {
    fn load(&self, offset: u64) -> Result<u64, String> {
        self.0.load(offset).map_err(|it| it.to_string()) // alloc: only for a load that failed
    }
}

type TierRunner<C, P, A> = tier::Runner<Cells, Recompiler<C, P, A>>;

/// What the tier runner recompiles through: the lazy runner's compiler, placer and fetcher,
/// weakly, so the runner owning the tier runner is not kept by it.
struct Recompiler<C: Compiler, P: Placer, A: Answers>(Weak<Shared<C, P, A>>);

impl<C: Compiler, P: Placer, A: Answers> tier::Recompile for Recompiler<C, P, A> {
    fn recompile(&self, function: u32, to: Tier) -> Result<Option<u64>, String> {
        let shared = self
            .0
            .upgrade() // alloc: below, only once the runner closed
            .ok_or_else(|| format!("tier-up of lazy function {function}: the lazy runner closed"))?;
        shared.recompile(LazyId(function), to)
    }
}

struct Tiered<C: Compiler, P: Placer, A: Answers> {
    runner: TierRunner<C, P, A>,
    /// Per function: bound to the runner, once its first code was published.
    bound: Vec<bool>,
    /// The tiers the daemon's history remembered for functions not bound yet.
    remembered: HashMap<u32, Tier>,
}

struct State {
    fetcher: Fetcher,
    closing: bool,
    idle_queued: bool,
    /// Workers asleep on [`Shared::wake`].
    parked: u32,
    /// When the last ask for code not ready yet came.
    last_ask: Option<Instant>,
}

/// A worker's, or an asking thread's, reusable buffers for one compile's end.
struct Buffers {
    actions: Vec<Action>,
    callees: Vec<LazyId>,
    frame: Vec<u8>,
    /// [`crate::plan::Plan::callees`]'s scratch.
    seen: Vec<u64>,
}

/// How long after the last ask an ask is taken to be alone, not one of a burst of first calls
/// from several threads: past the time a reader takes for one ask, below the time from one
/// answer to the same thread's next first call. A lone ask whose code is compiled already is
/// answered by the thread that read it; one in a burst is a worker's, so a burst places in
/// parallel.
const ALONE: Duration = Duration::from_micros(1);

/// How long a worker that ran out of compiles watches [`Shared::jobs`] before it sleeps: past
/// the gap between two first calls of one thread (the asker's wake-up, the resolver, the run up
/// to its next call), so a burst of first calls is taken without a wake-up per ask.
const SPIN: Duration = Duration::from_micros(50);
/// Loads of [`Shared::jobs`] between two looks at the clock.
const SPIN_LOADS: u32 = 64;

struct Shared<C: Compiler, P: Placer, A: Answers> {
    session: Session,
    /// The fetcher's state: held only to take or record a compile, never across one.
    state: Mutex<State>,
    wake: Condvar,
    /// Bumped, under the fetcher's lock, whenever a compile is queued or the runner closes:
    /// what the spinning worker watches.
    jobs: AtomicU64,
    /// Whether a worker is spinning on `jobs`: at most one is.
    spinning: AtomicBool,
    compiler: C,
    /// Held across one placement: the image is placed into one function at a time.
    placer: Mutex<P>,
    answers: A,
    idle: bool,
    /// The tier-up, taken out (and closed) before the workers stop. Held across a bind,
    /// never with the fetcher's or the placer's lock.
    tier: Mutex<Option<Tiered<C, P, A>>>,
    /// The functions whose first code could not be bound to the tier runner, and why.
    unbound: SegQueue<(LazyId, String)>,
    /// The asking threads' buffers, between the hits they answer: popped and pushed once a lone
    /// hit, which allocates nothing, so a lock held for the one pop or push (a `SegQueue` takes a
    /// new block every 31 pushes, and the asking threads are too many to bound an `ArrayQueue`).
    spare: Mutex<Vec<Buffers>>,
}

/// One lazy run's daemon side.
pub struct Runner<C: Compiler, P: Placer, A: Answers> {
    shared: Arc<Shared<C, P, A>>,
    threads: Vec<JoinHandle<()>>,
}

/// What a closed runner found: the functions that failed to compile, asked for or not.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a run's result, once at its close
pub struct Closed {
    pub failures: Vec<(InstanceKey, ItemKey, String)>,
    pub compiles: u64,
    /// What the tier runner did, for a run started with a [`TierUp`].
    pub tier: Option<tier::Closed>,
    /// The functions whose first code could not be bound to the tier runner, and why.
    pub unbound: Vec<(LazyId, String)>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

impl<C: Compiler, P: Placer, A: Answers> Runner<C, P, A> {
    /// Starts `config.workers` threads fetching `session`'s functions, `first` queued, and
    /// with `tier`, the tier runner over its cells.
    pub fn start(
        session: Session,
        first: &[(LazyId, Priority)],
        compiler: C,
        placer: P,
        answers: A,
        config: Config,
        tier: Option<TierUp>,
    ) -> Result<Runner<C, P, A>, String> {
        let workers = config.workers.max(1);
        let functions = session.plan().len();
        let mut fetcher = Fetcher::new(functions, workers)?;
        for &(id, priority) in first {
            fetcher.want(id, priority);
        }
        // alloc: setup, once per run.
        let shared = Arc::new(Shared {
            session,
            state: Mutex::new(State { fetcher, closing: false, idle_queued: false, parked: 0, last_ask: None }),
            wake: Condvar::new(),
            jobs: AtomicU64::new(0),
            spinning: AtomicBool::new(false),
            compiler,
            placer: Mutex::new(placer),
            answers,
            idle: config.idle,
            tier: Mutex::new(None),
            unbound: SegQueue::new(),
            spare: Mutex::new(Vec::new()),   // alloc: setup; a buffer per thread that asks at once
        });
        if let Some(up) = tier {
            let recompiler = Recompiler(Arc::downgrade(&shared));
            let runner = tier::Runner::start(functions, up.policy, up.config, Cells(up.cells), recompiler)?;
            *lock(&shared.tier) = Some(Tiered {
                runner,
                bound: vec![false; functions as usize], // alloc: setup, once per run
                remembered: HashMap::default(),
            });
        }
        let mut threads = Vec::with_capacity(workers as usize);
        for n in 0..workers {
            let worker = Arc::clone(&shared);
            let name = format!("blaze-lazy-{n}"); // alloc: setup, a name per worker
            let spawned = std::thread::Builder::new().name(name).spawn(move || worker.work());
            match spawned {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    drop(Runner { shared, threads });
                    // alloc: the error of a failed start.
                    return Err(format!("starting a lazy compile thread: {error}"));
                }
            }
        }
        Ok(Runner { shared, threads })
    }

    /// Reads `client`'s asks from `reader` until it ends or sends what is not an ask: why, for
    /// the daemon to stop the run with when it still runs. Blocking: on a thread of the run's.
    pub fn serve(&self, client: u32, reader: &mut Reader) -> String {
        loop {
            let frame = match reader.recv() {
                Ok(frame) => frame,
                Err(error) => return error.to_string(), // alloc: the run's end
            };
            if let Err(error) = self.frame(client, frame.kind, frame.seq, frame.body) {
                // alloc: the run's end.
                return format!("process {client} asked what is not a lazy function's code: {error}");
            }
        }
    }

    /// A blaze-inproc frame of `kind` and sequence number `seq` from `client`'s run process,
    /// with body `bytes`. A frame that is not an ask is an error of the channel, for the
    /// daemon to stop the run with. A lone ask ([`ALONE`]) for code the compiler has already
    /// is placed and answered on the calling thread, before this returns.
    pub fn frame(&self, client: u32, kind: Kind, seq: u32, bytes: &[u8]) -> Result<(), WireError> {
        let frame = Frame::decode(bytes)?;
        if wire::channel_kind(&frame) != kind {
            return Err(WireError::Kind(u32::from(kind.0)));
        }
        match frame {
            Frame::Ask { request, lazy } => {
                self.shared.ask(Waiter { client, seq, request }, lazy);
                Ok(())
            }
            Frame::Ready { .. } | Frame::Failed { .. } => Err(WireError::Unasked),
        }
    }

    /// Queues `ids` at `priority`: the daemon's prefetches, such as an agent's declared intent.
    pub fn want(&self, ids: &[LazyId], priority: Priority) {
        let mut state = lock(&self.shared.state);
        let mut queued = 0;
        for &id in ids {
            queued += u32::from(state.fetcher.want(id, priority));
        }
        let wakes = self.shared.queued(&state, queued);
        drop(state);
        self.shared.rouse(wakes);
    }

    /// Where `key`'s code was published, once it was.
    pub fn target(&self, key: &InstanceKey) -> Option<u64> {
        let id = self.shared.session.plan().id(key)?;
        lock(&self.shared.state).fetcher.target(id)
    }

    /// The daemon's history says `id` reached `tier` in an earlier run: it is recompiled to
    /// it at once, or once its first code is published.
    pub fn remembered(&self, id: LazyId, tier: Tier) -> Result<(), String> {
        let mut tiered = lock(&self.shared.tier);
        let Some(tiered) = tiered.as_mut() else {
            // alloc: the daemon's mistake, reported.
            return Err(format!("remembering {tier:?} for lazy function {}: the run is not tiered", id.0));
        };
        match tiered.bound.get(id.index()) {
            None => {
                // alloc: the daemon's mistake, reported.
                Err(format!("remembering {tier:?} for lazy function {}, of {}", id.0, tiered.bound.len()))
            }
            Some(true) => {
                tiered.runner.remembered(id.0, tier);
                Ok(())
            }
            Some(false) => {
                tiered.remembered.insert(id.0, tier);
                Ok(())
            }
        }
    }

    /// Why tiering stopped before a close, once it has: for the daemon to report at once.
    pub fn tier_stopped(&self) -> Option<String> {
        lock(&self.shared.tier).as_ref()?.runner.stopped()
    }

    /// The ids of the lazy functions whose code is among `reached`.
    pub fn reached(&self, reached: &[CodeHash]) -> Vec<LazyId> {
        let mut out = Vec::with_capacity(reached.len()); // alloc: once per run, from history
        self.shared.session.plan().reached(reached, &mut out);
        out
    }

    /// How many workers sleep until an ask wakes them, past their spin.
    #[cfg(test)]
    pub(crate) fn parked(&self) -> u32 {
        lock(&self.shared.state).parked
    }

    /// Stops the tier runner, then the workers once the compiles running end, and says what
    /// failed.
    pub fn close(mut self) -> Closed {
        let tiered = lock(&self.shared.tier).take();
        let tier = tiered.map(|it| it.runner.close());
        self.shared.shut();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let state = lock(&self.shared.state);
        let plan = self.shared.session.plan();
        Closed {
            failures: state
                .fetcher
                .failures()
                .map(|(id, why)| (plan.key(id), plan.item(id), why.to_owned())) // alloc: the close's report
                .collect(), // alloc: the close's report
            compiles: state.fetcher.compiles(),
            tier,
            // alloc: the close's report
            unbound: std::iter::from_fn(|| self.shared.unbound.pop()).collect(),
        }
    }
}

impl<C: Compiler, P: Placer, A: Answers> Drop for Runner<C, P, A> {
    fn drop(&mut self) {
        let tiered = lock(&self.shared.tier).take();
        drop(tiered);
        if self.threads.is_empty() {
            return;
        }
        self.shared.shut();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl<C: Compiler, P: Placer, A: Answers> Shared<C, P, A> {
    fn ask(&self, to: Waiter, lazy: u32) {
        let mut state = lock(&self.state);
        if let Some(target) = state.fetcher.target(LazyId(lazy)) {
            drop(state);
            self.answers.send(to.client, to.seq, wire::READY_KIND, &wire::ready(to.request, lazy, target));
            return;
        }
        let id = LazyId(lazy);
        let now = Instant::now();
        let alone = state.last_ask.is_none_or(|last| now.duration_since(last) >= ALONE);
        state.last_ask = Some(now);
        if alone && !state.fetcher.started(id) && lazy < state.fetcher.len() {
            drop(state);
            let hit = self.hit(id);
            state = lock(&self.state);
            // Not started by another thread meanwhile: answered here, never a waiter.
            if let Some(compiled) = hit
                && state.fetcher.start(id)
            {
                drop(state);
                let placed = self.place_compiled(id, compiled);
                let answered = placed.is_ok();
                let mut buffers = lock(&self.spare).pop().unwrap_or_else(|| self.buffers());
                self.finish(id, placed, Some(to), &mut buffers);
                lock(&self.spare).push(buffers);
                if answered {
                    self.hit_answered(id);
                }
                return;
            }
        }
        let queued = u32::from(!state.fetcher.started(id) && lazy < state.fetcher.len());
        let mut actions = Vec::new(); // alloc: only for a failure; a wait or a queue pushes nothing
        state.fetcher.ask(to, lazy, &mut actions);
        let wakes = self.queued(&state, queued);
        drop(state);
        self.rouse(wakes);
        self.act(&actions, &mut Vec::new()); // alloc: only to encode a failure
    }

    /// After `queued` compiles were queued, with the fetcher's lock held: tells the spinning
    /// worker, and says how many sleeping workers to wake ([`Shared::rouse`], once the lock is
    /// let go): one per compile the spinner doesn't take. A worker that finishes a compile takes
    /// the next itself, so a wake too few costs parallelism, never a compile.
    fn queued(&self, state: &State, queued: u32) -> u32 {
        if queued == 0 {
            return 0;
        }
        self.jobs.fetch_add(1, Ordering::Release);
        let spinner = u32::from(self.spinning.load(Ordering::Acquire));
        queued.saturating_sub(spinner).min(state.parked)
    }

    fn rouse(&self, wakes: u32) {
        for _ in 0..wakes {
            self.wake.notify_one();
        }
    }

    /// Closes the runner: every worker ends once its compile does.
    fn shut(&self) {
        lock(&self.state).closing = true;
        self.jobs.fetch_add(1, Ordering::Release);
        self.wake.notify_all();
    }

    /// The next compile, waiting for one; none once the runner closes. A worker that finds
    /// none spins on [`Shared::jobs`] for up to [`SPIN`] when no other worker does, then
    /// sleeps: an ask that comes meanwhile is taken without waking a thread.
    fn take(&self) -> Option<LazyId> {
        let mut state = lock(&self.state);
        let mut spun = false;
        loop {
            if state.closing {
                return None;
            }
            if let Some((id, _)) = state.fetcher.next() {
                return Some(id);
            }
            if self.idle && !state.idle_queued && state.fetcher.idle() {
                state.idle_queued = true;
                for id in 0..state.fetcher.len() {
                    state.fetcher.want(LazyId(id), Priority::Idle);
                }
                self.jobs.fetch_add(1, Ordering::Release);
                self.wake.notify_all();
                continue;
            }
            if !spun && !self.spinning.swap(true, Ordering::AcqRel) {
                let seen = self.jobs.load(Ordering::Acquire);
                drop(state);
                spun = !self.spin(seen);
                self.spinning.store(false, Ordering::Release);
                state = lock(&self.state);
                continue;
            }
            state.parked += 1;
            self.wake.wait(&mut state);
            state.parked -= 1;
            spun = false;
        }
    }

    /// Watches [`Shared::jobs`] move from `seen`, for up to [`SPIN`]: whether it did.
    fn spin(&self, seen: u64) -> bool {
        let start = Instant::now();
        let mut spin = blaze_spin::SpinWait::new();
        loop {
            for _ in 0..SPIN_LOADS {
                if self.jobs.load(Ordering::Acquire) != seen {
                    return true;
                }
                spin.relax();
            }
            if start.elapsed() >= SPIN {
                return false;
            }
        }
    }

    fn work(&self) {
        let mut buffers = self.buffers();
        while let Some(id) = self.take() {
            let placed = self.compile_and_place(id);
            self.finish(id, placed, None, &mut buffers);
        }
    }

    fn buffers(&self) -> Buffers {
        // alloc: a thread's buffers, made once and reused for every compile it ends.
        Buffers { actions: Vec::new(), callees: Vec::new(), frame: Vec::new(), seen: self.session.plan().scratch() }
    }

    /// The end of `id`'s compile: recorded, answered (its waiters, and `asker`, who started it
    /// with [`Fetcher::start`]), its callees queued and its code bound.
    fn finish(
        &self,
        id: LazyId,
        placed: Result<(u64, CompiledFunction, Option<u64>), String>,
        asker: Option<Waiter>,
        buffers: &mut Buffers,
    ) {
        let Buffers { actions, callees, frame, seen } = buffers;
        actions.clear();
        let mut state = lock(&self.state);
        let result = placed.as_ref().map(|it| it.0).map_err(String::as_str);
        state.fetcher.done(id, result, actions);
        if let Some(to) = asker {
            state.fetcher.ask(to, id.0, actions);
        }
        drop(state);
        // The askers wait on nothing else: answered before the prefetch and the tier-up.
        self.act(actions, frame);
        let Ok((_, function, cells)) = placed else { return };
        callees.clear();
        self.session.plan().callees(&function, seen, callees);
        if !callees.is_empty() {
            let mut state = lock(&self.state);
            let mut queued = 0;
            for &callee in callees.iter() {
                queued += u32::from(state.fetcher.want(callee, Priority::Callee));
            }
            let wakes = self.queued(&state, queued);
            drop(state);
            self.rouse(wakes);
        }
        self.bind(id, cells);
    }

    /// `id`'s first code was published (the fetcher has it ready), counting in `cells`.
    fn bind(&self, id: LazyId, cells: Option<u64>) {
        let mut tiered = lock(&self.tier);
        let Some(tiered) = tiered.as_mut() else { return };
        if let Err(why) = tiered.runner.bind(id.0, cells) {
            self.unbound.push((id, why));
            return;
        }
        tiered.bound[id.index()] = true;
        if let Some(tier) = tiered.remembered.remove(&id.0) {
            tiered.runner.remembered(id.0, tier);
        }
    }

    /// `id` recompiled at `to` and published in its slot, the fetcher retargeted: where its
    /// new code counts, for the tier runner.
    fn recompile(&self, id: LazyId, to: Tier) -> Result<Option<u64>, String> {
        let plan = self.session.plan();
        if id.0 >= plan.len() {
            // alloc: the tier runner's mistake, reported.
            return Err(format!("tier-up of lazy function {}, of {}", id.0, plan.len()));
        }
        let (key, item) = (plan.key(id), plan.item(id));
        // alloc: called only for a failure.
        let named = |why: &str| format!("recompiling {key:?} (item {item:?}) at {to:?}: {why}");
        let function = self.compiler.tier_up(key, to).map_err(|why| named(&why))?;
        let (target, cells) = {
            let mut placer = lock(&self.placer);
            // alloc: only for a placement that failed.
            let target = placer.replace(key, &function).map_err(|why| named(&format!("placing it: {why}")))?;
            (target, placer.cells(&function.key))
        };
        if target == 0 {
            return Err(named("placed at address 0"));
        }
        if !lock(&self.state).fetcher.retarget(id, target) {
            return Err(named("its first code was never published"));
        }
        Ok(cells)
    }

    /// `id`'s code, compiled, placed and published: where it runs, the function and where it
    /// counts, or why not, naming it.
    fn compile_and_place(&self, id: LazyId) -> Result<(u64, CompiledFunction, Option<u64>), String> {
        let key = self.session.plan().key(id);
        self.place_compiled(id, catch_unwind(AssertUnwindSafe(|| self.compiler.compile(key))))
    }

    /// `id`'s code when the compiler has it already ([`Compiler::hit`]), for
    /// [`Shared::place_compiled`]: a probe that failed or panicked is the compile's failure.
    fn hit(&self, id: LazyId) -> Option<std::thread::Result<Result<CompiledFunction, String>>> {
        let key = self.session.plan().key(id);
        match catch_unwind(AssertUnwindSafe(|| self.compiler.hit(key))) {
            Ok(None) => None,
            Ok(Some(compiled)) => Some(Ok(compiled)),
            Err(payload) => Some(Err(payload)),
        }
    }

    /// [`Compiler::hit_answered`] for `id`, whose hit was placed and answered: a panic in it
    /// stops the run, naming the function.
    fn hit_answered(&self, id: LazyId) {
        let plan = self.session.plan();
        let (key, item) = (plan.key(id), plan.item(id));
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.compiler.hit_answered(key))) {
            // alloc: the run's end.
            let why = format!("keeping {key:?} (item {item:?}) after its first call: the compiler panicked: {}", panic_text(&*payload));
            self.answers.stop(Some((key, item)), &why);
        }
    }

    /// `id`'s code, as its compile (or probe) ended, placed and published: as
    /// [`Shared::compile_and_place`].
    fn place_compiled(
        &self,
        id: LazyId,
        compiled: std::thread::Result<Result<CompiledFunction, String>>,
    ) -> Result<(u64, CompiledFunction, Option<u64>), String> {
        let plan = self.session.plan();
        let (key, item) = (plan.key(id), plan.item(id));
        // alloc: called only for a failure.
        let named = |why: &str| format!("compiling {key:?} (item {item:?}) on its first call: {why}");
        let function = match compiled {
            Ok(Ok(function)) if function.key == key => function,
            // alloc: a failure.
            Ok(Ok(function)) => return Err(named(&format!("the compiler gave {:?}", function.key))),
            Ok(Err(why)) => return Err(named(&why)),
            // alloc: a failure.
            Err(payload) => return Err(named(&format!("the compiler panicked: {}", panic_text(&*payload)))),
        };
        let placed = catch_unwind(AssertUnwindSafe(|| {
            let mut placer = lock(&self.placer);
            placer.place(&function).map(|target| (target, placer.cells(&function.key)))
        }));
        match placed {
            Ok(Ok((0, _))) => Err(named("placed at address 0")),
            Ok(Ok((target, cells))) => Ok((target, function, cells)),
            Ok(Err(why)) => Err(named(&format!("placing it: {why}"))), // alloc: a failure
            // alloc: a failure.
            Err(payload) => Err(named(&format!("placing it panicked: {}", panic_text(&*payload)))),
        }
    }

    fn act(&self, actions: &[Action], frame: &mut Vec<u8>) {
        let plan = self.session.plan();
        for action in actions {
            match action {
                Action::Answer { to, lazy, outcome: Outcome::Ready(target) } => {
                    let ready = wire::ready(to.request, *lazy, *target);
                    self.answers.send(to.client, to.seq, wire::READY_KIND, &ready);
                }
                Action::Answer { to, lazy, outcome: Outcome::Failed(why) } => {
                    frame.clear();
                    let answer = Frame::Failed { request: to.request, lazy: *lazy, why: fit(why) };
                    answer.encode(frame);
                    self.answers.send(to.client, to.seq, wire::FAILED_KIND, frame);
                }
                Action::Stop { lazy, why } => {
                    let function = (*lazy < plan.len()).then(|| (plan.key(LazyId(*lazy)), plan.item(LazyId(*lazy))));
                    self.answers.stop(function, why);
                }
            }
        }
    }
}

/// The most of `why` an answer carries: what fits in an ask's slot, cut at a character. The
/// whole of it is the fetcher's ([`Runner::close`]).
pub(crate) fn fit(why: &str) -> &str {
    let most = ask::MOST_BODY - wire::FAILED_LEN;
    if why.len() <= most {
        return why;
    }
    let end = (0..=most).rev().find(|it| why.is_char_boundary(*it)).unwrap_or(0);
    &why[..end]
}

/// The [`Answers`] of one runner's run processes, which blaze-inproc started: each one's
/// [`Sender`], by process ID, its client. A run whose answer can't be sent, or that called a
/// function that can't be compiled, is killed, that its error.
#[derive(Default)]
pub struct Runs {
    senders: SharedMap<u32, Sender>,
}

impl Runs {
    /// Answers `sender`'s process from now on: its client.
    pub fn add(&self, sender: Sender) -> u32 {
        let client = sender.pid();
        self.senders.pin().insert(client, sender);
        client
    }

    /// Once `client`'s process ended.
    pub fn remove(&self, client: u32) {
        self.senders.pin().remove(&client);
    }
}

fn kill(sender: &Sender, why: &str) {
    if let Err(error) = sender.kill(why) {
        eprintln!("blaze-lazy: {error}");
    }
}

impl Answers for Runs {
    fn send(&self, client: u32, seq: u32, kind: Kind, body: &[u8]) {
        let senders = self.senders.pin();
        let Some(sender) = senders.get(&client) else { return };
        if let Err(error) = sender.send(kind, seq, body) {
            // alloc: the run's end.
            kill(sender, &format!("answering its ask for a lazy function's code: {error}"));
        }
    }

    /// Kills every run process it answers: they run one program, which called the function.
    fn stop(&self, _: Option<(InstanceKey, ItemKey)>, why: &str) {
        for sender in self.senders.pin().values() {
            kill(sender, why);
        }
    }
}

// dyn: a panic's payload, which is what `catch_unwind` gives.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    match payload.downcast_ref::<&str>() {
        Some(text) => (*text).to_owned(), // alloc: a panic's text
        None => match payload.downcast_ref::<String>() {
            Some(text) => text.clone(), // clone: a panic's text, for its failure
            None => "a payload that is not text".to_owned(), // alloc: a panic's text
        },
    }
}
