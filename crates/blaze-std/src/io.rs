//! `Io`: one handle to the process's driver (docs/v2/plan/design/std.md, "Io"). One concrete
//! type, not a trait and not a generic parameter: inside is the target's
//! [`blaze_core::Native`] driver or [`blaze_core::sim::Sim`], picked when the `Io` is made.
//!
//! A call pushes an [`Effect`] under a fresh [`Tag`] and waits for the [`Completion`] of that
//! tag under its task's [`Cx`]: it ends with the completion, or with [`Stopped::Deadline`] once
//! the deadline passes on the driver's clock, or [`Stopped::Cancelled`] once the task's scope is
//! cancelled. The driver's own [`Driver::poll`] takes the deadline, absolute, so no layer here
//! restarts a relative timeout (rule deadline-waits).
//!
//! **How a call waits, in S1.** Every call waits through the one wait, [`Cx::wait`]. The
//! spec parks the calling task on the scheduler; S1's layout is `Inline` (a task runs to
//! completion at its spawn, docs/v2/plan/design/std.md "Layouts"), so the wait polls the driver
//! on the calling thread through [`Io::poll`], the seam a fiber park replaces (it would hand the
//! tag to the scheduler and park instead of polling). A completion of another tag that a poll brings is kept for its waiter; one of a
//! wait that already stopped is dropped as it arrives. In the simulation a poll that brings
//! nothing before the deadline means nothing will complete by then, so the virtual clock is
//! moved to the deadline, as a real wait would sleep to it.

use std::{fmt, sync::Arc, time::Duration};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, Tag,
    lock::Lock,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_mem::Frame;
use blaze_rng::SplitMix64;

use crate::{
    Plant, Stopped,
    args::Args,
    clock::Clock,
    cx::Cx,
    env::Env,
    fs::Fs,
    host::{Exit, Host, HostFacts},
    mem::{Mem, TASK_FRAME},
    net::Net,
    rng::Rng,
    trace::{Record, RecordKind, Trace},
};

/// The most polls in a row that bring nothing while the deadline has not passed: a driver
/// whose poll returns early this often does not keep its deadline (rule bounded-progress).
pub(crate) const POLL_FUEL: u32 = 1 << 16;

/// What the simulation's generator of [`Io::rng`] starts from, besides the seed: so it draws
/// other numbers than the simulation's own scheduler.
const RNG_SALT: u64 = 0x6273_7464_2e72_6e67;

/// The driver inside an [`Io`].
enum Backend {
    #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
    // alloc: once per process, the driver.
    Native(Box<blaze_core::Native>),
    // alloc: once per simulated world.
    Sim(Box<Sim>),
}

impl Backend {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.submit(effects),
            Backend::Sim(sim) => sim.submit(effects),
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.poll(out, until),
            Backend::Sim(sim) => sim.poll(out, until),
        }
    }

    fn now(&self) -> Duration {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.now(),
            Backend::Sim(sim) => sim.now(),
        }
    }

    /// A poll brought nothing before `until`: the simulation has nothing that completes by
    /// then, so its clock moves to `until`, as a real wait sleeps to it. A real driver's
    /// clock moves by itself.
    fn idle_until(&mut self, until: Deadline) {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(_) => {}
            Backend::Sim(sim) => {
                let now = sim.now();
                if !until.passed(now) {
                    sim.advance(until.get().saturating_sub(now));
                }
            }
        }
    }

    fn sim(&mut self) -> Option<&mut Sim> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(_) => None,
            Backend::Sim(sim) => Some(sim),
        }
    }
}

/// Whether a stopped wait's tag still has completions to come.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ends {
    /// One completion: the next of its tag is its last.
    Once,
    /// A listen's: it ends with [`Done::Unlistened`] or [`Done::Failed`].
    Stream,
}

/// Everything behind one [`Io`].
struct World {
    driver: Backend,
    next: u64,
    /// Effects made and not yet handed to the driver.
    effects: Vec<Effect>,
    /// What the last poll moved, before it is sorted into `got`.
    polled: Vec<Completion>,
    /// Completions polled and not yet taken by their waiter.
    got: Vec<Completion>,
    /// Tags whose waiter stopped: their completions are dropped as they arrive.
    abandoned: Vec<(Tag, Ends)>,
    /// The generator [`Io::rng`] forks from.
    rng: SplitMix64,
    /// Every file an effect named, and whether the effect changes it, while `record` is on: a
    /// test's view of where its capabilities reached.
    touched: Vec<(FileName, bool)>,
    record: bool,
    /// What the world's tasks recorded through their contexts.
    trace: Trace,
    host: HostFacts,
    /// The status the simulated process exited with.
    exited: Option<i32>,
    plant: Plant,
}

/// One handle to the process's driver. Cloning it copies authority it holds and adds none.
#[derive(Clone)]
pub struct Io {
    world: Arc<Lock<World>>,
    env: Arc<[(String, String)]>,
    args: Arc<[String]>,
}

/// A simulated world: the seed every choice of it comes from, how unkind it is, its
/// environment variables, its command line and its host.
#[derive(Clone, Debug)] // clone: a test's description of a world
pub struct SimWorld {
    pub seed: u64,
    pub faults: Faults,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
    pub host: HostFacts,
}

impl SimWorld {
    /// The world of [`Io::sim`]: no faults beyond latency, no environment, no arguments, the
    /// simulated host.
    pub fn new(seed: u64) -> SimWorld {
        // alloc: none, an empty environment and command line.
        SimWorld { seed, faults: Faults::default(), env: Vec::new(), args: Vec::new(), host: HostFacts::simulated() }
    }
}

/// The process's world, as its entry hands it over until `bstd::rt` (S4) reads it from the
/// process's start block: the directory the driver's names are under, its threads, the
/// environment, the command line, the host's name, and the seed of [`Io::rng`] (blaze-core has
/// no effect that asks the system for randomness or the host's name yet).
#[derive(Debug)]
pub struct NativeWorld<'a> {
    pub root: &'a std::path::Path,
    pub threads: usize,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
    pub host_name: Arc<str>,
    pub seed: u64,
}

/// What a world starts from, whichever its driver.
struct Start {
    driver: Backend,
    seed: u64,
    env: Vec<(String, String)>,
    args: Vec<String>,
    host: HostFacts,
    plant: Plant,
}

/// The target's driver could not be made over a root.
#[derive(Debug)]
pub struct OpenError {
    pub root: String,
    pub why: String,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::Io::native over {}: the target's driver: {}", self.root, self.why)
    }
}

impl std::error::Error for OpenError {}

impl Io {
    /// A whole simulated world from `seed`: an in-memory file tree, a virtual clock, a seeded
    /// generator, no faults beyond the simulation's latency and no environment.
    pub fn sim(seed: u64) -> Io {
        Io::simulated(SimWorld::new(seed))
    }

    pub fn simulated(world: SimWorld) -> Io {
        Io::planted(world, Plant::None)
    }

    /// `world` with `plant` planted: [`Plant::None`] but for the tests' controls.
    pub(crate) fn planted(world: SimWorld, plant: Plant) -> Io {
        Io::over(Start {
            // alloc: once per simulated world.
            driver: Backend::Sim(Box::new(Sim::new(world.seed, world.faults))),
            seed: world.seed ^ RNG_SALT,
            env: world.env,
            args: world.args,
            host: world.host,
            plant,
        })
    }

    /// The target's own driver over `world.root`.
    #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
    pub fn native(world: NativeWorld<'_>) -> Result<Io, OpenError> {
        let refused = |why: String| OpenError {
            // alloc: an error's message.
            root: world.root.display().to_string(),
            why,
        };
        // alloc: an error's message.
        let cores = blaze_core::cores::machine().map_err(|error| refused(error.to_string()))?;
        // alloc: an error's message.
        let driver = blaze_core::native(world.root, world.threads).map_err(|error| refused(error.to_string()))?;
        Ok(Io::over(Start {
            // alloc: once per process, the driver.
            driver: Backend::Native(Box::new(driver)),
            seed: world.seed,
            env: world.env,
            args: world.args,
            host: HostFacts { pid: std::process::id(), cores, name: world.host_name },
            plant: Plant::None,
        }))
    }

    fn over(start: Start) -> Io {
        let Start { driver, seed, env, args, host, plant } = start;
        let world = World {
            driver,
            next: 0,
            // alloc: the effects' queue, grown once to the most made between waits.
            effects: Vec::new(),
            // alloc: the poll's buffer, grown once to the most a poll moves.
            polled: Vec::new(),
            // alloc: the completions kept for their waiters.
            got: Vec::new(),
            // alloc: the tags of waits that stopped.
            abandoned: Vec::new(),
            rng: SplitMix64::new(seed),
            // alloc: none until a test records.
            touched: Vec::new(),
            record: false,
            trace: Trace::new(),
            host,
            exited: None,
            plant,
        };
        Io {
            // alloc: once per world.
            world: Arc::new(Lock::new("bstd.io.world", world)),
            // alloc: once per world, its environment.
            env: env.into(),
            // alloc: once per world, its command line.
            args: args.into(),
        }
    }

    /// The root context: named `main`, never cancelled, with a frame of its own, ending
    /// `budget` from now on the driver's clock. Every wait under it ends by then.
    pub fn cx(&self, budget: Duration) -> Result<Cx, Stopped> {
        let frame = Frame::with_reserve(TASK_FRAME, blaze_mem::Tag::Frames)
            .map_err(|_| Stopped::OutOfMemory { arena: "main's frame", asked: TASK_FRAME })?;
        Ok(Cx::root(Deadline::after(self.now(), budget), frame, self.clone()))
    }

    pub fn fs(&self) -> Fs {
        Fs::new(self.clone())
    }

    pub fn clock(&self) -> Clock {
        Clock::new(self.clone())
    }

    /// A generator of its own, forked from the world's: seeded in the simulation.
    pub fn rng(&self) -> Rng {
        Rng::new(self.world.lock().rng.next_u64())
    }

    /// Every variable of the world's environment; narrow it with [`Env::only`].
    pub fn env(&self) -> Env {
        Env::new(Arc::clone(&self.env), self.plant())
    }

    pub fn args(&self) -> Args {
        Args::new(Arc::clone(&self.args))
    }

    pub fn host(&self) -> Host {
        Host::new(self.clone())
    }

    pub fn mem(&self) -> Mem {
        Mem::new(self.clone())
    }

    pub fn net(&self) -> Net {
        Net::new(self.clone())
    }

    /// `f` on the simulation, for a test to change the world (advance the clock, fail syncs,
    /// hold a lock); `None` on a real driver.
    pub fn with_sim<R>(&self, f: impl FnOnce(&mut Sim) -> R) -> Option<R> {
        self.world.lock().driver.sim().map(f)
    }

    /// The simulated machine loses power ([`Sim::crash`]): every effect in flight is gone,
    /// and with it what this `Io` kept for their waiters. `false` on a real driver.
    pub fn crash(&self) -> bool {
        let mut world = self.world.lock();
        let World { driver, got, abandoned, polled, effects, .. } = &mut *world;
        let Some(sim) = driver.sim() else { return false };
        sim.crash();
        got.clear();
        abandoned.clear();
        polled.clear();
        effects.clear();
        true
    }

    /// `f` on what the world's tasks recorded through their contexts ([`Cx::span`],
    /// [`Cx::event`]).
    pub fn trace<R>(&self, f: impl FnOnce(&Trace) -> R) -> R {
        f(&self.world.lock().trace)
    }

    /// The status the simulated process exited with, if it did ([`Host::exit`]).
    pub fn exited(&self) -> Option<i32> {
        self.world.lock().exited
    }

    /// A record of `span`, now on the driver's clock.
    pub(crate) fn record_trace(&self, span: &'static str, kind: RecordKind) {
        let mut world = self.world.lock();
        let at = world.driver.now();
        world.trace.push(Record { at, span, kind });
    }

    pub(crate) fn host_facts<R>(&self, f: impl FnOnce(&HostFacts) -> R) -> R {
        f(&self.world.lock().host)
    }

    /// The process ends with `status`: a real one exits; the simulated one remembers it and
    /// unwinds the calling task with an [`Exit`] payload.
    pub(crate) fn exit(&self, status: i32) -> ! {
        let simulated = {
            let mut world = self.world.lock();
            let simulated = world.driver.sim().is_some();
            if simulated {
                world.exited = Some(status);
            }
            simulated
        };
        if !simulated {
            std::process::exit(status);
        }
        // alloc: once, the unwind's payload.
        std::panic::resume_unwind(Box::new(Exit { status }))
    }

    /// The driver's clock.
    pub(crate) fn now(&self) -> Duration {
        self.world.lock().driver.now()
    }

    pub(crate) fn plant(&self) -> Plant {
        self.world.lock().plant
    }

    /// Start recording the files effects name ([`Io::touched`]).
    #[cfg(test)]
    pub(crate) fn record(&self) {
        self.world.lock().record = true;
    }

    /// The files effects named since [`Io::record`], and whether each effect changed it.
    #[cfg(test)]
    pub(crate) fn touched(&self) -> Vec<(FileName, bool)> {
        // clone: a test's copy of the record.
        self.world.lock().touched.clone()
    }

    /// `effect` of a fresh tag, handed to the driver, then waited for under `cx`: one call.
    /// A `cx` that stopped already asks nothing of the driver.
    pub(crate) fn call(&self, cx: &Cx, effect: impl FnOnce(Tag) -> Effect) -> Result<Done, Stopped> {
        cx.stopped_by(self.now())?;
        let tag = self.submit(effect);
        cx.wait(self, tag, Ends::Once)
    }

    /// `effect` of a fresh tag, handed to the driver now.
    pub(crate) fn submit(&self, effect: impl FnOnce(Tag) -> Effect) -> Tag {
        let mut world = self.world.lock();
        let tag = Tag(world.next);
        world.next += 1;
        let effect = effect(tag);
        if world.record {
            touch(&mut world.touched, &effect);
        }
        let World { driver, effects, .. } = &mut *world;
        effects.push(effect);
        driver.submit(effects);
        tag
    }

    /// The tag's waiter is gone: its completions are dropped as they arrive.
    pub(crate) fn forget(&self, tag: Tag, ends: Ends) {
        self.world.lock().abandoned.push((tag, ends));
    }

    /// The next completion of `tag`, if one was polled: taken for its waiter.
    pub(crate) fn take(&self, tag: Tag) -> Option<Done> {
        let mut world = self.world.lock();
        let at = world.got.iter().position(|it| it.tag == tag)?;
        Some(world.got.remove(at).done)
    }

    /// One poll of the driver, waiting at most until `until`: what it brought kept for each
    /// waiter (or dropped, a stopped waiter's), and how many completions it moved. A poll that
    /// moved nothing moves the simulation's clock to `until`, as a real wait sleeps to it. The
    /// step of [`Cx::wait`], the one wait.
    pub(crate) fn poll(&self, until: Deadline) -> usize {
        let mut world = self.world.lock();
        let World { driver, polled, got, abandoned, .. } = &mut *world;
        let moved = driver.poll(polled, until);
        for completion in polled.drain(..) {
            keep(got, abandoned, completion);
        }
        if moved == 0 {
            driver.idle_until(until);
        }
        moved
    }
}

/// `completion` kept for its waiter, or dropped when its waiter stopped.
fn keep(got: &mut Vec<Completion>, abandoned: &mut Vec<(Tag, Ends)>, completion: Completion) {
    let Some(at) = abandoned.iter().position(|(tag, _)| *tag == completion.tag) else {
        got.push(completion);
        return;
    };
    let last = match abandoned[at].1 {
        Ends::Once => true,
        Ends::Stream => matches!(completion.done, Done::Unlistened | Done::Failed(_)),
    };
    if last {
        abandoned.swap_remove(at);
    }
}

/// The files `effect` names, recorded for a test.
fn touch(touched: &mut Vec<(FileName, bool)>, effect: &Effect) {
    // clone: a test's record of names, refcounts.
    let mut named = |file: &FileName, changes: bool| touched.push((file.clone(), changes));
    match effect {
        Effect::ReadAll { file, .. }
        | Effect::Read { file, .. }
        | Effect::ReadLink { file, .. }
        | Effect::Stat { file, .. }
        | Effect::Lstat { file, .. }
        | Effect::ReadInto { file, .. }
        | Effect::Map { file, .. } => named(file, false),
        Effect::Append { file, .. }
        | Effect::Sync { file, .. }
        | Effect::Truncate { file, .. }
        | Effect::Write { file, .. }
        | Effect::WriteAt { file, .. }
        | Effect::WriteFrom { file, .. }
        | Effect::Mode { file, .. }
        | Effect::Seal { file, .. }
        | Effect::Times { file, .. }
        | Effect::Remove { file, .. }
        | Effect::Lock { file, .. } => named(file, true),
        Effect::Rename { from, to, .. } | Effect::Clone { from, to, .. } | Effect::Link { from, to, .. } => {
            named(from, true);
            named(to, true);
        }
        Effect::List { dir, .. } | Effect::Entries { dir, .. } | Effect::Watch { dir, .. } => named(dir, false),
        Effect::Private { dir, .. } => named(dir, true),
        Effect::Journal { dir, .. } => named(dir, false),
        Effect::Unlock { .. }
        | Effect::Unwatch { .. }
        | Effect::Stamp { .. }
        | Effect::Flush { .. }
        | Effect::Timer { .. }
        | Effect::Spawn { .. }
        | Effect::Kill { .. }
        | Effect::Listen { .. }
        | Effect::Unlisten { .. }
        | Effect::Connect { .. }
        | Effect::Recv { .. }
        | Effect::Send { .. }
        | Effect::Hangup { .. }
        | Effect::Pressure { .. } => {}
    }
}

impl fmt::Debug for Io {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut world = self.world.lock();
        let kind = match world.driver.sim() {
            Some(_) => "simulated",
            None => "native",
        };
        write!(f, "Io({kind}, at {:?})", world.driver.now())
    }
}
