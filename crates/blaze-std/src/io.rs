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
//!
//! **What the host offers** ([`Offers`]) is a table fixed when the `Io` is made
//! (docs/v2/plan/design/std.md, "When a capability is missing"): every root accessor
//! ([`Io::fs`], [`Io::net`], ...) is an `Option`, `None` for a capability the host withholds,
//! and each has a `need_` form ([`Io::need_fs`], ...) for the common case where `main` requires
//! it, whose [`Missing`] names it. A native `Io` offers what its target can
//! ([`crate::host::OFFERS`]); [`Io::sim`] offers everything, and [`Io::sim_with`] lets a test
//! withhold some.
//!
//! **Capabilities with no effect yet.** A capability is a type whenever it is authority; its
//! real provider exists only where a blaze-core effect supports it. The terminal, the
//! keychain, trust anchors, the locale, the user, other processes, executable memory, windows,
//! automation, the opener, power, sound, the devices, and requests and streams made by the
//! host have none, so a native `Io` withholds
//! them ([`Offers::NATIVE`]) and a simulated one answers them from its
//! [`SimHost`] ([`Io::script`]). Nothing stands in for a missing provider.

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{fmt, time::Duration};

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
    audio::Audio,
    automation::Automation,
    clock::Clock,
    code::Code,
    cx::Cx,
    device::{Bluetooth, Camera, Clipboard, Location, Notify, Screen, Serial, Usb},
    disk::{Disk, DiskCounts},
    env::Env,
    fs::Fs,
    gpu::Gpu,
    host::{Exit, Host, HostFacts},
    hosted::SimHost,
    keys::Keys,
    locale::Locale,
    mem::Mem,
    net::Net,
    opener::Opener,
    out::{Out, Stream},
    power::Power,
    requests::Http,
    rng::Rng,
    roots::Roots,
    streams::Streams,
    term::Term,
    trace::{Record, RecordKind, Trace},
    user::User,
    window::Window,
};

/// The bounds bstd would otherwise pick for its caller, said once where the [`Io`] is made
/// (docs/v2/plan/design/std.md, "The second rule": a bound is an argument, never a default).
/// A world is made with one ([`SimWorld::limits`], [`NativeWorld::limits`]); there is no
/// `Default`. [`Limits::USUAL`] is a named value, written at the call like any other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// How long an abandoned value waits, on the driver's clock, for what it had in flight to
    /// end (a [`crate::pending::Pending`]'s op, a [`crate::task::Scope`]'s tasks) when a panic
    /// drops it: its abandonment path's bound.
    #[blaze::policy]
    pub abandon_wait: Duration,
    /// The most polls in a row that bring nothing while the deadline has not passed, before
    /// the wait ends as [`Stopped::Stalled`]: a driver whose poll returns early this often
    /// does not keep its deadline.
    #[blaze::policy]
    pub poll_fuel: u32,
    /// The most connections [`crate::net::Listener::close`] hangs up while it waits for the
    /// listen's end, before it stops waiting.
    #[blaze::policy]
    pub close_most: usize,
    /// The most records the world's trace keeps; past it each further one is counted in
    /// [`Trace::dropped`].
    #[blaze::policy]
    pub trace_most: usize,
}

impl Limits {
    /// One second for an abandoned value, 65,536 idle polls, 1,024 connections hung up by a
    /// close, 65,536 trace records: what blaze's own programs and the tests say.
    pub const USUAL: Limits = Limits {
        abandon_wait: Duration::from_secs(1),
        poll_fuel: 1 << 16,
        close_most: 1024,
        trace_most: 1 << 16,
    };
}

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

    fn in_flight(&self) -> usize {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.in_flight(),
            Backend::Sim(sim) => sim.in_flight(),
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
    /// For each file whose bytes or name an effect of this world changed, the number of the
    /// last effect that did (its tag, plus one): the file's generation, which a
    /// [`crate::fs::Flushed`] carries and its clone compares. A file removed or renamed away
    /// has no entry, so the table holds one entry a file that is there and was changed here.
    // alloc: one entry a live file changed through this world.
    changed: BTreeMap<FileName, u64>,
    /// What the world's tasks recorded through their contexts.
    trace: Trace,
    host: HostFacts,
    /// The status the simulated process exited with.
    exited: Option<i32>,
    /// The simulated host behind the capabilities blaze-core has no effect for: `None` in a
    /// real world, which has no provider for them.
    // alloc: once per simulated world.
    hosted: Option<Box<SimHost>>,
    /// The simulated disk's ledger ([`crate::disk`]): what was paid, and which names a crash
    /// may undo. `None` in a real world.
    // alloc: once per simulated world.
    disk: Option<Box<Disk>>,
    plant: Plant,
}

/// One handle to the process's driver. Cloning it copies authority it holds and adds none.
#[derive(Clone)]
pub struct Io {
    world: Arc<Lock<World>>,
    env: Arc<[(String, String)]>,
    args: Arc<[String]>,
    offers: Offers,
    limits: Limits,
}

/// The table of what a host offers, one flag a root capability, and what is made from it.
macro_rules! offers {
    ($($(#[$doc:meta])* $name:ident,)*) => {
        /// Which capabilities a host offers: a table fixed when the [`Io`] is made, never
        /// discovered by an op failing halfway through (docs/v2/plan/design/std.md, "When a
        /// capability is missing"). One flag a root capability; a small `Copy` value.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub struct Offers {
            $($(#[$doc])* pub $name: bool,)*
        }

        impl Offers {
            /// Every capability.
            pub const ALL: Offers = Offers { $($name: true,)* };

            /// None: a host that hands its program nothing but its context.
            pub const NONE: Offers = Offers { $($name: false,)* };

            /// Each capability's name, as its flag is called, in the order of
            /// [`Offers::bits`].
            pub const NAMES: &'static [&'static str] = &[$(stringify!($name),)*];

            /// The table as bits, the capability at `at` of [`Offers::NAMES`] being bit `at`.
            pub fn bits(self) -> u64 {
                let flags = [$(self.$name,)*];
                flags.iter().enumerate().fold(0, |bits, (at, on)| bits | (u64::from(*on) << at))
            }

            /// The table `bits` spell, as [`Offers::bits`] gives them; bits past the last
            /// capability say nothing.
            pub fn from_bits(bits: u64) -> Offers {
                let mut flags = (0..u64::BITS).map(|at| bits & (1 << at) != 0);
                Offers { $($name: flags.next().unwrap_or(false),)* }
            }

            /// Whether the capability called `name` is offered; `None` for a name that is no
            /// capability's.
            pub fn offers(self, name: &str) -> Option<bool> {
                match name {
                    $(stringify!($name) => Some(self.$name),)*
                    _ => None,
                }
            }
        }
    };
}

offers! {
    fs,
    net,
    env,
    args,
    clock,
    rng,
    mem,
    host,
    /// The terminal as a device ([`Io::term`]).
    term,
    /// The keychain ([`Io::keys`]).
    keys,
    /// The machine's trust anchors ([`Io::roots`]).
    roots,
    locale,
    user,
    /// Other processes ([`crate::host::Host::procs`]).
    procs,
    /// Executable memory and native modules ([`Io::code`]).
    code,
    windows,
    automation,
    opener,
    power,
    /// Sound, and playing it ([`Io::audio`]).
    audio,
    /// Recording sound ([`crate::audio::Audio::input`]).
    audio_in,
    camera,
    screen,
    location,
    bluetooth,
    usb,
    serial,
    clipboard,
    notify,
    /// Requests the host makes for the program ([`Io::http`]).
    http,
    /// Streams the host carries for the program ([`Io::streams`]).
    streams,
    /// Standard output ([`Io::out`]).
    out,
    /// Standard error ([`Io::err`]).
    err,
}

impl Offers {
    /// What a native world can offer today: the capabilities blaze-core has effects for. The
    /// rest (see the module's doc) have a simulated provider only.
    pub const NATIVE: Offers = Offers {
        fs: true,
        net: true,
        env: true,
        args: true,
        clock: true,
        rng: true,
        mem: true,
        host: true,
        out: true,
        err: true,
        ..Offers::NONE
    };
}

/// A capability `main` requires and the host does not offer ([`Offers`]): the error of each
/// `need_` accessor of [`Io`], naming it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Missing {
    /// The capability, as its accessor is named: `fs`, `net`, `env`, ...
    pub capability: &'static str,
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let capability = self.capability;
        write!(
            f,
            "bstd::Io: the host does not offer `{capability}` (io.{capability}() is None): this \
             program needs it, so it stops here rather than run without it"
        )
    }
}

impl core::error::Error for Missing {}

/// A simulated world: the seed every choice of it comes from, how unkind it is, its
/// environment variables, its command line and its host.
#[derive(Clone, Debug)] // clone: a test's description of a world
pub struct SimWorld {
    pub seed: u64,
    pub faults: Faults,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
    pub host: HostFacts,
    /// What its program is offered: [`Offers::ALL`] unless a test withholds some.
    pub offers: Offers,
    /// The bounds its waits, closes and trace end by.
    #[blaze::policy]
    pub limits: Limits,
}

impl SimWorld {
    /// The world of [`Io::sim`]: no faults beyond latency, no environment, no arguments, the
    /// simulated host, every capability offered, with the bounds `limits`.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn new(seed: u64, limits: Limits) -> SimWorld {
        SimWorld {
            limits,
            seed,
            faults: Faults::default(),
            // alloc: none, an empty environment.
            env: Vec::new(),
            // alloc: none, an empty command line.
            args: Vec::new(),
            host: HostFacts::simulated(),
            offers: Offers::ALL,
        }
    }
}

/// The process's world, as its entry hands it over until `bstd::rt` (S4) reads it from the
/// process's start block: the directory the driver's names are under, its threads, the
/// environment, the command line, the host's name, and the seed of [`Io::rng`] (blaze-core has
/// no effect that asks the system for randomness or the host's name yet).
#[derive(Debug)]
pub struct NativeWorld<'a> {
    pub root: &'a crate::rt::RootPath,
    pub threads: usize,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
    pub host_name: Arc<str>,
    pub seed: u64,
    /// The bounds its waits, closes and trace end by.
    pub limits: Limits,
}

/// What a world starts from, whichever its driver.
struct Start {
    driver: Backend,
    seed: u64,
    env: Vec<(String, String)>,
    args: Vec<String>,
    host: HostFacts,
    offers: Offers,
    limits: Limits,
    hosted: Option<Box<SimHost>>,
    disk: Option<Box<Disk>>,
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

impl core::error::Error for OpenError {}

impl Io {
    /// A whole simulated world from `seed`: an in-memory file tree, a virtual clock, a seeded
    /// generator, no faults beyond the simulation's latency and no environment, with
    /// [`Limits::USUAL`]: a test's world, whose seed is said. A world with other bounds is
    /// [`Io::simulated`] of a [`SimWorld`].
    pub fn sim(seed: u64) -> Io {
        Io::simulated(SimWorld::new(seed, Limits::USUAL))
    }

    /// [`Io::sim`] offering only what `offers` says: a test of a host that withholds some.
    pub fn sim_with(seed: u64, offers: Offers) -> Io {
        let mut world = SimWorld::new(seed, Limits::USUAL);
        world.offers = offers;
        Io::simulated(world)
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
            offers: world.offers,
            limits: world.limits,
            // alloc: once per simulated world, its host.
            hosted: Some(Box::new(SimHost::new())),
            // alloc: once per simulated world, its disk's ledger.
            disk: Some(Box::new(Disk::new(world.seed))),
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
            host: HostFacts { pid: crate::rt::process_id(), cores, name: world.host_name },
            offers: crate::host::OFFERS,
            limits: world.limits,
            hosted: None,
            disk: None,
            plant: Plant::None,
        }))
    }

    fn over(start: Start) -> Io {
        let Start { driver, seed, env, args, host, offers, limits, hosted, disk, plant } = start;
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
            // alloc: none until a file is changed.
            changed: BTreeMap::new(),
            trace: Trace::new(limits.trace_most),
            host,
            exited: None,
            hosted,
            disk,
            plant,
        };
        Io {
            // alloc: once per world.
            world: Arc::new(Lock::new("bstd.io.world", world)),
            // alloc: once per world, its environment.
            env: env.into(),
            // alloc: once per world, its command line.
            args: args.into(),
            offers,
            limits,
        }
    }

    /// The bounds this world was made with.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The root context: named `main`, never cancelled, ending `budget` from now on the
    /// driver's clock, with a frame of its own that hands out at most `scratch` bytes
    /// ([`crate::mem::TASK_FRAME`] is the usual value; it is named at the call, never
    /// assumed). Every wait under it ends by `budget`.
    pub fn cx(&self, budget: Duration, scratch: usize) -> Result<Cx, Stopped> {
        let frame = Frame::with_reserve(scratch, blaze_mem::Tag::Frames)
            .map_err(|_| Stopped::OutOfMemory { arena: "main's frame", asked: scratch })?;
        Ok(Cx::root(Deadline::after(self.now(), budget), frame, self.clone()))
    }

    /// What the host offers: fixed when this `Io` was made.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn offers(&self) -> Offers {
        self.offers
    }

    /// Whether the host offers the capability `of` picks from the table.
    pub(crate) fn offered(&self, of: fn(&Offers) -> bool) -> bool {
        of(&self.offers) || self.plant() == Plant::OffersIgnored
    }

    /// The file tree, if the host offers one.
    pub fn fs(&self) -> Option<Fs> {
        self.offered(|it| it.fs).then(|| Fs::new(self.clone()))
    }

    /// The file tree, which `main` requires: [`Missing`] names it when the host has none.
    pub fn need_fs(&self) -> Result<Fs, Missing> {
        self.fs().ok_or(Missing { capability: "fs" })
    }

    /// The driver's clock, if the host offers one.
    pub fn clock(&self) -> Option<Clock> {
        self.offered(|it| it.clock).then(|| Clock::new(self.clone()))
    }

    pub fn need_clock(&self) -> Result<Clock, Missing> {
        self.clock().ok_or(Missing { capability: "clock" })
    }

    /// A generator of its own, forked from the world's: seeded in the simulation. `None` when
    /// the host offers no randomness.
    pub fn rng(&self) -> Option<Rng> {
        self.offered(|it| it.rng).then(|| Rng::new(self.world.lock().rng.next_u64()))
    }

    pub fn need_rng(&self) -> Result<Rng, Missing> {
        self.rng().ok_or(Missing { capability: "rng" })
    }

    /// Every variable of the world's environment, if the host offers it; narrow it with
    /// [`Env::only`].
    pub fn env(&self) -> Option<Env> {
        self.offered(|it| it.env).then(|| Env::new(Arc::clone(&self.env), self.plant()))
    }

    pub fn need_env(&self) -> Result<Env, Missing> {
        self.env().ok_or(Missing { capability: "env" })
    }

    /// The command line, if the host hands one over.
    pub fn args(&self) -> Option<Args> {
        self.offered(|it| it.args).then(|| Args::new(Arc::clone(&self.args)))
    }

    pub fn need_args(&self) -> Result<Args, Missing> {
        self.args().ok_or(Missing { capability: "args" })
    }

    /// The host's facts and its exit, if it offers them.
    pub fn host(&self) -> Option<Host> {
        self.offered(|it| it.host).then(|| Host::new(self.clone()))
    }

    pub fn need_host(&self) -> Result<Host, Missing> {
        self.host().ok_or(Missing { capability: "host" })
    }

    /// Named arenas against the memory budget, if the host offers them.
    pub fn mem(&self) -> Option<Mem> {
        self.offered(|it| it.mem).then(|| Mem::new(self.clone()))
    }

    pub fn need_mem(&self) -> Result<Mem, Missing> {
        self.mem().ok_or(Missing { capability: "mem" })
    }

    /// Standard output, if the host offers it.
    pub fn out(&self) -> Option<Out> {
        self.offered(|it| it.out).then(|| Out::new(self.clone(), Stream::Out))
    }

    /// Standard output, which `main` requires.
    pub fn need_out(&self) -> Result<Out, Missing> {
        self.out().ok_or(Missing { capability: "out" })
    }

    /// Standard error, if the host offers it: an [`Out`] of the other stream.
    pub fn err(&self) -> Option<Out> {
        self.offered(|it| it.err).then(|| Out::new(self.clone(), Stream::Err))
    }

    /// Standard error, which `main` requires.
    pub fn need_err(&self) -> Result<Out, Missing> {
        self.err().ok_or(Missing { capability: "err" })
    }

    /// `parts`, in order, on this world's `stream`, as one write: kept by the simulated host
    /// ([`SimHost::out`], [`SimHost::err`]), or written to the process's by `rt`.
    pub(crate) fn write_out(&self, stream: Stream, parts: &[&[u8]]) -> Result<(), String> {
        let mut world = self.world.lock();
        match world.hosted.as_deref_mut() {
            Some(host) => {
                let kept = match stream {
                    Stream::Out => &mut host.out,
                    Stream::Err => &mut host.err,
                };
                for part in parts {
                    kept.extend_from_slice(part);
                }
                Ok(())
            }
            None => crate::rt::write_out(stream, parts),
        }
    }

    /// Sockets, if the host offers them (a wasm worker does not).
    pub fn net(&self) -> Option<Net> {
        self.offered(|it| it.net).then(|| Net::new(self.clone()))
    }

    pub fn need_net(&self) -> Result<Net, Missing> {
        self.net().ok_or(Missing { capability: "net" })
    }

    /// The graphics device, if this world has one: a native macOS world does; the simulation
    /// and other targets do not yet ([`Gpu`], a placeholder until the GPU plan's device).
    pub fn gpu(&self) -> Option<Gpu> {
        let native = self.world.lock().driver.sim().is_none();
        Gpu::offered(native)
    }

    /// The terminal as a device, if the host offers one (the simulation does).
    pub fn term(&self) -> Option<Term> {
        self.offered(|it| it.term).then(|| Term::new(self.clone()))
    }

    pub fn need_term(&self) -> Result<Term, Missing> {
        self.term().ok_or(Missing { capability: "term" })
    }

    /// The keychain, if the host offers one (the simulation does).
    pub fn keys(&self) -> Option<Keys> {
        self.offered(|it| it.keys).then(|| Keys::new(self.clone()))
    }

    pub fn need_keys(&self) -> Result<Keys, Missing> {
        self.keys().ok_or(Missing { capability: "keys" })
    }

    /// The machine's trust anchors, if the host offers them (the simulation does); narrow
    /// them with [`Roots::only`]. [`Roots::none`] needs no host.
    pub fn roots(&self) -> Option<Roots> {
        if !self.offered(|it| it.roots) {
            return None;
        }
        // clone: the anchors handed out, refcounts; alloc: once per call, their list.
        let anchors: Arc<[crate::roots::Anchor]> = self.script(|host| host.roots.iter().cloned().collect())?;
        Some(Roots::new(anchors, self.plant()))
    }

    pub fn need_roots(&self) -> Result<Roots, Missing> {
        self.roots().ok_or(Missing { capability: "roots" })
    }

    /// The machine's locale, as data, if the host offers it (the simulation does).
    pub fn locale(&self) -> Option<Locale> {
        if !self.offered(|it| it.locale) {
            return None;
        }
        // clone: the locale handed out, plain data.
        self.script(|host| host.locale.clone())
    }

    pub fn need_locale(&self) -> Result<Locale, Missing> {
        self.locale().ok_or(Missing { capability: "locale" })
    }

    /// Who is running, if the host says (the simulation does).
    pub fn user(&self) -> Option<User> {
        if !self.offered(|it| it.user) {
            return None;
        }
        // clone: the user's facts handed out, plain data.
        let facts = self.script(|host| host.user.clone())?;
        Some(User::new(self.clone(), facts))
    }

    pub fn need_user(&self) -> Result<User, Missing> {
        self.user().ok_or(Missing { capability: "user" })
    }

    /// Making memory executable and loading native code, if the host offers it (the simulation does).
    pub fn code(&self) -> Option<Code> {
        self.offered(|it| it.code).then(|| Code::new(self.clone()))
    }

    pub fn need_code(&self) -> Result<Code, Missing> {
        self.code().ok_or(Missing { capability: "code" })
    }

    /// The program's windows, if the host offers them (the simulation does).
    pub fn windows(&self) -> Option<Window> {
        self.offered(|it| it.windows).then(|| Window::new(self.clone()))
    }

    pub fn need_windows(&self) -> Result<Window, Missing> {
        self.windows().ok_or(Missing { capability: "windows" })
    }

    /// Driving other applications, if the host offers it (the simulation does).
    pub fn automation(&self) -> Option<Automation> {
        self.offered(|it| it.automation).then(|| Automation::new(self.clone()))
    }

    pub fn need_automation(&self) -> Result<Automation, Missing> {
        self.automation().ok_or(Missing { capability: "automation" })
    }

    /// The user's default applications, if the host offers them (the simulation does).
    pub fn opener(&self) -> Option<Opener> {
        self.offered(|it| it.opener).then(|| Opener::new(self.clone()))
    }

    pub fn need_opener(&self) -> Result<Opener, Missing> {
        self.opener().ok_or(Missing { capability: "opener" })
    }

    /// The machine's power, if the host offers it (the simulation does).
    pub fn power(&self) -> Option<Power> {
        self.offered(|it| it.power).then(|| Power::new(self.clone()))
    }

    pub fn need_power(&self) -> Result<Power, Missing> {
        self.power().ok_or(Missing { capability: "power" })
    }

    /// Sound, if the host offers it (the simulation does): playing, and recording where that is offered too.
    pub fn audio(&self) -> Option<Audio> {
        self.offered(|it| it.audio).then(|| Audio::new(self.clone()))
    }

    pub fn need_audio(&self) -> Result<Audio, Missing> {
        self.audio().ok_or(Missing { capability: "audio" })
    }

    /// Cameras, if the host offers them (the simulation does).
    pub fn camera(&self) -> Option<Camera> {
        self.offered(|it| it.camera).then(|| Camera::new(self.clone()))
    }

    pub fn need_camera(&self) -> Result<Camera, Missing> {
        self.camera().ok_or(Missing { capability: "camera" })
    }

    /// The screen's contents, if the host offers them (the simulation does).
    pub fn screen(&self) -> Option<Screen> {
        self.offered(|it| it.screen).then(|| Screen::new(self.clone()))
    }

    pub fn need_screen(&self) -> Result<Screen, Missing> {
        self.screen().ok_or(Missing { capability: "screen" })
    }

    /// Where the machine is, if the host says (the simulation does).
    pub fn location(&self) -> Option<Location> {
        self.offered(|it| it.location).then(|| Location::new(self.clone()))
    }

    pub fn need_location(&self) -> Result<Location, Missing> {
        self.location().ok_or(Missing { capability: "location" })
    }

    /// Bluetooth, if the host offers it (the simulation does).
    pub fn bluetooth(&self) -> Option<Bluetooth> {
        self.offered(|it| it.bluetooth).then(|| Bluetooth::new(self.clone()))
    }

    pub fn need_bluetooth(&self) -> Result<Bluetooth, Missing> {
        self.bluetooth().ok_or(Missing { capability: "bluetooth" })
    }

    /// USB devices, if the host offers them (the simulation does).
    pub fn usb(&self) -> Option<Usb> {
        self.offered(|it| it.usb).then(|| Usb::new(self.clone()))
    }

    pub fn need_usb(&self) -> Result<Usb, Missing> {
        self.usb().ok_or(Missing { capability: "usb" })
    }

    /// Serial ports, if the host offers them (the simulation does).
    pub fn serial(&self) -> Option<Serial> {
        self.offered(|it| it.serial).then(|| Serial::new(self.clone()))
    }

    pub fn need_serial(&self) -> Result<Serial, Missing> {
        self.serial().ok_or(Missing { capability: "serial" })
    }

    /// The clipboard, if the host offers it (the simulation does).
    pub fn clipboard(&self) -> Option<Clipboard> {
        self.offered(|it| it.clipboard).then(|| Clipboard::new(self.clone()))
    }

    pub fn need_clipboard(&self) -> Result<Clipboard, Missing> {
        self.clipboard().ok_or(Missing { capability: "clipboard" })
    }

    /// Notifications, if the host offers them (the simulation does).
    pub fn notify(&self) -> Option<Notify> {
        self.offered(|it| it.notify).then(|| Notify::new(self.clone()))
    }

    pub fn need_notify(&self) -> Result<Notify, Missing> {
        self.notify().ok_or(Missing { capability: "notify" })
    }

    /// Requests the host makes for the program, if it offers that (a wasm worker's `fetch`;
    /// today the simulation's routes alone). A program that has a `Net` makes its own with
    /// [`Http::over`].
    pub fn http(&self) -> Option<Http> {
        self.offered(|it| it.http).then(|| Http::host(self.clone()))
    }

    pub fn need_http(&self) -> Result<Http, Missing> {
        self.http().ok_or(Missing { capability: "http" })
    }

    /// Streams the host carries for the program, if it offers that (a browser's WebTransport;
    /// today the simulation's alone).
    pub fn streams(&self) -> Option<Streams> {
        self.offered(|it| it.streams).then(|| Streams::host(self.clone()))
    }

    pub fn need_streams(&self) -> Result<Streams, Missing> {
        self.streams().ok_or(Missing { capability: "streams" })
    }

    /// `f` on the simulated host, for a test to script what its devices will say and read
    /// what was done to them, and for the capabilities it answers; `None` in a real world,
    /// which has no provider for them.
    pub fn script<R>(&self, f: impl FnOnce(&mut SimHost) -> R) -> Option<R> {
        self.world.lock().hosted.as_deref_mut().map(f)
    }

    /// `f` on the simulation, for a test to change the world (advance the clock, fail syncs,
    /// hold a lock); `None` on a real driver.
    pub fn with_sim<R>(&self, f: impl FnOnce(&mut Sim) -> R) -> Option<R> {
        self.world.lock().driver.sim().map(f)
    }

    /// The simulated machine loses power ([`Sim::crash`]): every effect in flight is gone,
    /// and with it what this `Io` kept for their waiters; each file keeps what was flushed
    /// and a prefix of what was not, and the name changes no name sync covered are kept as a
    /// prefix too, the rest undone ([`crate::disk`]). `false` on a real driver.
    pub fn crash(&self) -> bool {
        let mut world = self.world.lock();
        let World { driver, got, abandoned, polled, effects, disk, .. } = &mut *world;
        let Some(sim) = driver.sim() else { return false };
        sim.crash();
        if let Some(disk) = disk {
            disk.crashed(sim);
        }
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
            crate::rt::exit(status);
        }
        // alloc: once, the unwind's payload.
        crate::rt::resume_unwind(Box::new(Exit { status }))
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
        change(&mut world.changed, &effect, tag.0 + 1);
        let World { driver, effects, disk, .. } = &mut *world;
        if let (Some(disk), Some(sim)) = (disk.as_deref_mut(), driver.sim()) {
            disk.submitted(sim, &effect);
        }
        effects.push(effect);
        driver.submit(effects);
        tag
    }

    /// The generation of `name`: the number of the last effect of this world that changed
    /// its bytes or made it, 0 for a file no effect of this world changed (or one since
    /// removed). Changes by another process are not seen: blaze-core's `Meta` carries no
    /// inode or change time to compare (std.md, "still implicit").
    pub(crate) fn generation(&self, name: &FileName) -> u64 {
        self.world.lock().changed.get(name).copied().unwrap_or(0)
    }

    /// A number no other call of this world was given: a tag's, taken for a name made beside
    /// another (a publish's temporary file).
    pub(crate) fn fresh(&self) -> u64 {
        let mut world = self.world.lock();
        let fresh = world.next;
        world.next += 1;
        fresh
    }

    /// What the simulated disk was asked to do so far ([`DiskCounts`]): bytes written and
    /// read, flushes, waits for the device, clones and the bytes they forced out. `None` on a
    /// real driver, which keeps no ledger.
    pub fn disk(&self) -> Option<DiskCounts> {
        self.world.lock().disk.as_deref().map(Disk::counts)
    }

    /// A caller waited for the device (a flush it did not return before): counted.
    pub(crate) fn waited_for_device(&self) {
        if let Some(disk) = self.world.lock().disk.as_deref_mut() {
            disk.waited();
        }
    }

    /// A sync of the directory `dir`'s names completed and was waited for
    /// ([`Effect::SyncDir`]): the simulated disk's ledger makes them durable and counts the
    /// wait. `true` in a simulated world, which has a ledger; `false` in a real one, where
    /// the disk itself did it.
    pub(crate) fn names_synced(&self, dir: &str) -> bool {
        match self.world.lock().disk.as_deref_mut() {
            Some(disk) => {
                disk.sync_names(dir);
                disk.waited();
                true
            }
            None => false,
        }
    }

    /// Whether this world's rename has synced its directories by the time it completes: a
    /// real driver's does (blaze-core's `Effect::Rename`); the simulation's ledger is told by
    /// a directory sync.
    pub(crate) fn rename_syncs_names(&self) -> bool {
        self.world.lock().disk.is_none()
    }

    /// How many effects the driver holds that have not completed: a test's view of whether
    /// anything a wait gave back is still in flight.
    pub(crate) fn in_flight(&self) -> usize {
        self.world.lock().driver.in_flight()
    }

    /// The next completion of the one-shot `tag`, waited for until `until` heeding no
    /// cancellation: the wait of a cancel and of an abandonment, which must see their op end
    /// so that nothing it does lands after they return. Past `until`, or after
    /// [`Limits::poll_fuel`] polls in a row that bring nothing, the op is given up ([`Io::forget`]) and the stop
    /// says why (rule bounded-progress).
    pub(crate) fn settle(&self, tag: Tag, until: Deadline, span: &'static str) -> Result<Done, Stopped> {
        let mut idle = 0;
        loop {
            if let Some(done) = self.take(tag) {
                return Ok(done);
            }
            let stopped = if until.passed(self.now()) {
                Some(Stopped::Deadline { span, at: until.get() })
            } else if idle == self.limits.poll_fuel {
                Some(Stopped::Stalled { span, polls: idle })
            } else {
                None
            };
            if let Some(stopped) = stopped {
                self.forget(tag, Ends::Once);
                return Err(stopped);
            }
            match self.poll(until) {
                0 => idle += 1,
                _ => idle = 0,
            }
        }
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
        let World { driver, polled, got, abandoned, disk, .. } = &mut *world;
        let moved = driver.poll(polled, until);
        for completion in polled.drain(..) {
            if let Some(disk) = disk.as_deref_mut() {
                disk.completed(&completion);
            }
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
        | Effect::ReadInto { file, .. }
        | Effect::ReadLink { file, .. }
        | Effect::Stat { file, .. }
        | Effect::Lstat { file, .. }
        | Effect::Map { file, .. } => named(file, false),
        Effect::Append { file, .. }
        | Effect::Sync { file, .. }
        | Effect::Barrier { file, .. }
        | Effect::WriteBack { file, .. }
        | Effect::Truncate { file, .. }
        | Effect::Write { file, .. }
        | Effect::WriteUnsynced { file, .. }
        | Effect::WriteAt { file, .. }
        | Effect::WriteFrom { file, .. }
        | Effect::Mode { file, .. }
        | Effect::Seal { file, .. }
        | Effect::Times { file, .. }
        | Effect::Remove { file, .. }
        | Effect::Lock { file, .. } => named(file, true),
        Effect::Rename { from, to, .. }
        | Effect::RenameUnsynced { from, to, .. }
        | Effect::Clone { from, to, .. }
        | Effect::Link { from, to, .. } => {
            named(from, true);
            named(to, true);
        }
        Effect::List { dir, .. } | Effect::Entries { dir, .. } | Effect::Watch { dir, .. } => named(dir, false),
        Effect::Private { dir, .. }
        | Effect::MakeDir { dir, .. }
        | Effect::RemoveDir { dir, .. }
        | Effect::SyncDir { dir, .. } => named(dir, true),
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
        | Effect::Pressure { .. }
        // The host effects name no file: a socket, a standard stream, the clock, the
        // system's random source.
        | Effect::Bind { .. }
        | Effect::SendTo { .. }
        | Effect::RecvFrom { .. }
        | Effect::Unbind { .. }
        | Effect::ReadStdin { .. }
        | Effect::WriteOut { .. }
        | Effect::Now { .. }
        | Effect::Random { .. } => {}
    }
}

/// Records in `changed` what `effect`, numbered `stamp`, does to the files it names: a file
/// whose bytes it changes or that it makes is stamped; one it removes or renames away loses
/// its entry.
fn change(changed: &mut BTreeMap<FileName, u64>, effect: &Effect, stamp: u64) {
    match effect {
        Effect::Append { file, .. }
        | Effect::Truncate { file, .. }
        | Effect::Write { file, .. }
        | Effect::WriteAt { file, .. }
        | Effect::WriteFrom { file, .. } => {
            // clone: the table's key, a refcount.
            changed.insert(file.clone(), stamp);
        }
        Effect::Remove { file, .. } => {
            changed.remove(file);
        }
        Effect::Rename { from, to, .. } => {
            changed.remove(from);
            // clone: the table's key, a refcount.
            changed.insert(to.clone(), stamp);
        }
        Effect::Clone { to, .. } | Effect::Link { to, .. } => {
            // clone: the table's key, a refcount.
            changed.insert(to.clone(), stamp);
        }
        // Everything else leaves every file's bytes and names as they were.
        Effect::ReadAll { .. } | Effect::Read { .. } | Effect::ReadInto { .. } | Effect::ReadLink { .. } | Effect::Sync { .. } | Effect::WriteBack { .. } | Effect::SyncDir { .. } | Effect::WriteUnsynced { .. } | Effect::List { .. } | Effect::RenameUnsynced { .. } | Effect::Mode { .. } | Effect::Seal { .. } | Effect::Private { .. } | Effect::Times { .. } | Effect::Stat { .. } | Effect::Lstat { .. } | Effect::Lock { .. } | Effect::Unlock { .. } | Effect::Watch { .. } | Effect::Journal { .. } | Effect::Unwatch { .. } | Effect::Stamp { .. } | Effect::Entries { .. } | Effect::Flush { .. } | Effect::Timer { .. } | Effect::Map { .. } | Effect::Spawn { .. } | Effect::Kill { .. } | Effect::Listen { .. } | Effect::Unlisten { .. } | Effect::Connect { .. } | Effect::Recv { .. } | Effect::Send { .. } | Effect::Hangup { .. } | Effect::Pressure { .. } | Effect::Bind { .. } | Effect::SendTo { .. } | Effect::RecvFrom { .. } | Effect::Unbind { .. } | Effect::ReadStdin { .. } | Effect::WriteOut { .. } | Effect::Now { .. } | Effect::Random { .. } | Effect::MakeDir { .. } | Effect::RemoveDir { .. } => {}
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
