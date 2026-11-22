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
use blaze_rng::SplitMix64;

use crate::{
    Plant, Stopped,
    args::{ArgList, Args},
    audio::Audio,
    automation::Automation,
    clock::{Born, Clock},
    code::Code,
    cx::Cx,
    device::{Bluetooth, Camera, Clipboard, Location, Notify, Screen, Serial, Usb},
    disk::{Disk, DiskCounts},
    env::{Env, Vars},
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
    rng::Rng,
    roots::Roots,
    streams::Streams,
    term::Term,
    trace::{Record, RecordKind, Trace},
    user::User,
    window::Window,
};

/// What a world reads and does without its lock: the native clock, inline file calls, lent
/// writes (std.md, "End state: efficiency", pieces 6, 7 and 11).
mod fast;
mod native_in;
/// [`Io::sip_key`]: a hash key from the world's random source.
mod sip_key;
/// [`LentDriver`]: the world's backend lent to a state machine over `blaze_core::Driver`
/// (blaze-cas's plain store under the command), fenced to one directory.
mod lend;
pub use lend::LentDriver;
#[cfg(test)]
mod tests;

pub(crate) use blaze_core::threads::inline::Lent;
pub(crate) use fast::ReadAllLent;

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

    /// As [`Limits::USUAL`], with global trace recording off (`trace_most: 0`): `Cx::span` and
    /// `Cx::event` take no world lock on high-core workloads where worker telemetry is used
    /// instead.
    pub const QUIET: Limits = Limits {
        abandon_wait: Duration::from_secs(1),
        poll_fuel: 1 << 16,
        close_most: 1024,
        trace_most: 0,
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
    /// A hosted program's: every reach a call through its host's table ([`crate::guest`]).
    // alloc: once per hosted run.
    Guest(Box<crate::guest::Guest>),
}

impl Backend {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.submit(effects),
            Backend::Sim(sim) => sim.submit(effects),
            Backend::Guest(guest) => guest.submit(effects),
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.poll(out, until),
            Backend::Sim(sim) => sim.poll(out, until),
            Backend::Guest(guest) => guest.poll(out),
        }
    }

    fn now(&self) -> Duration {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.now(),
            Backend::Sim(sim) => sim.now(),
            Backend::Guest(guest) => guest.now(),
        }
    }

    fn wall_ns(&self) -> u64 {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.wall_ns(),
            Backend::Sim(sim) => sim.wall_ns(),
            Backend::Guest(guest) => guest.wall_ns(),
        }
    }

    fn boot(&self) -> Result<blaze_core::Boot, blaze_core::NoBoot> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.boot(),
            Backend::Sim(sim) => Driver::boot(&**sim),
            // A guest's host table carries no boot: the guest says so, never a made-up one.
            Backend::Guest(_) => Err(blaze_core::NoBoot { source: "a guest's host table", why: String::from("it carries no boot") }),
        }
    }

    /// `parts`, in order, on the standard stream `stream`, as one write: the driver's one
    /// reach for output, one call per flush of an [`Out`]'s buffer. Not an [`Effect`]: the
    /// native driver performs one on a thread of its own per stream, which a `hello` cannot
    /// afford, so the write is made here, on the calling thread, by the backend that owns the
    /// stream. A simulated world's streams are its [`SimHost`]'s ([`Io::write_out`]).
    fn write_out(&mut self, stream: Stream, parts: &[&[u8]]) -> Result<(), String> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(_) => crate::rt::write_out(stream, parts),
            // alloc: an error's message.
            Backend::Sim(_) => Err(String::from("bstd: a simulated world's streams are its host's, and it has none")),
            // alloc: an error's message.
            Backend::Guest(guest) => guest.write(stream, parts).map_err(|error| error.to_string()),
        }
    }

    /// What `stream` is drawn on: the kernel's answer for a native world (`rt::tty`), read when
    /// asked; a simulated world's streams are its host's (`SimHost::ttys`, set from
    /// [`SimWorld::ttys`]), so its backend has none; the hosted wire does not carry it yet.
    fn tty(&mut self, stream: Stream) -> crate::term::Tty {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(_) => crate::rt::tty::read(stream),
            Backend::Sim(_) | Backend::Guest(_) => crate::term::Tty::NotTerminal,
        }
    }

    /// A poll brought nothing before `until`: the simulation has nothing that completes by
    /// then, so its clock moves to `until`, as a real wait sleeps to it. A real driver's
    /// clock moves by itself. [`Deadline::NEVER`] is no time to move to: a real wait with no
    /// deadline blocks until something completes, so the clock stays, and the caller's poll
    /// fuel ends the wait as [`Stopped::Stalled`] (moving it to `Duration::MAX` left a clock
    /// that every later delay overflowed).
    fn idle_until(&mut self, until: Deadline) {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(_) => {}
            // Every escaped effect completed inside its call: nothing is left to come.
            Backend::Guest(_) => {}
            Backend::Sim(sim) => {
                let Some(until) = until.finite() else { return };
                let now = sim.now();
                if !until.passed(now) {
                    sim.advance(until.get().saturating_sub(now));
                }
            }
        }
    }

    /// How many effects the driver holds that have not completed: what the end of a native
    /// `main` drains ([`Io::drain`]), and a test's view.
    fn in_flight(&self) -> usize {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(driver) => driver.in_flight(),
            Backend::Sim(sim) => sim.in_flight(),
            Backend::Guest(guest) => guest.in_flight(),
        }
    }

    fn sim(&mut self) -> Option<&mut Sim> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(_) => None,
            Backend::Sim(sim) => Some(sim),
            Backend::Guest(_) => None,
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
    /// A watch's or a journal's: it ends with [`Done::Unwatched`] or [`Done::Failed`].
    Watch,
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
    /// The generator [`Io::rng`] forks from: `None` in a native world until its first fork,
    /// which draws its seed from the host then (std.md, "What a program pays for at start").
    rng: Option<SplitMix64>,
    /// Every file an effect named, and whether the effect changes it, while `record` is on: a
    /// test's view of where its capabilities reached.
    touched: Vec<(FileName, bool)>,
    /// Every effect handed to the driver, in order, while `record` is on: a test's view of
    /// what a call asked of the device.
    asked: Vec<Effect>,
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
    /// What standard output and standard error are drawn on, read through the backend at the
    /// first ask ([`Io::tty`]) and kept.
    ttys: [Option<crate::term::Tty>; 2],
    /// What was said on standard output and standard error and not yet written, in that
    /// order ([`crate::out`], "Buffering"): inline, so the buffer is part of the world's one
    /// allocation, and one a world, so every `Out` of it keeps its lines in order.
    pending: [crate::out::Pending; 2],
    /// How many folders above the root were granted ([`Io::above_most`]).
    above: u8,
    /// The simulated host behind the capabilities blaze-core has no effect for: `None` in a
    /// real world, which has no provider for them.
    // alloc: once per simulated world.
    hosted: Option<Box<SimHost>>,
    /// The simulated disk's ledger ([`crate::disk`]): what was paid. `None` in a real world.
    // alloc: once per simulated world.
    disk: Option<Box<Disk>>,
}

/// One handle to the process's driver. Cloning it copies authority it holds and adds none.
pub struct Io {
    reach: Reach,
    offers: Offers,
    limits: Limits,
    /// The tests' planted bug: [`Plant::None`] but in their controls.
    plant: Plant,
}

// Written out, not derived: a derived `Clone` calls each field's `clone`, three functions
// more on the hosted hello's path (ladder README, "What the hosted hello executes"); the
// `Copy` fields are copied.
impl Clone for Io {
    fn clone(&self) -> Io {
        Io { reach: self.reach.clone(), offers: self.offers, limits: self.limits, plant: self.plant }
    }
}

/// Where an [`Io`]'s world is.
#[derive(Clone)]
enum Reach {
    /// A native or simulated world: shared by the `Io`s cloned from it, on any thread, behind
    /// its lock; with what is read or done without the lock beside it ([`fast::Beside`]: the
    /// native clock and inline file calls).
    ///
    /// `workers` is the world's per-worker state (D171, [`crate::workers`]): the cells its
    /// tasks' `on_worker` holds, made with the world or handed in ([`Io::on_workers`]).
    Shared { world: Arc<Lock<World>>, env: Vars, args: ArgList, beside: fast::Beside, workers: crate::workers::Workers },
    /// A hosted run's ([`crate::guest::Run`]): found through the calling thread's run, never
    /// held, so cloning and dropping an `Io` of it touches nothing. Its hot calls (a write,
    /// the clock, a grant, the exit) go straight to the host's table with no lock; the rest
    /// (the escape's effects) through a world made at its first use ([`Io::world`]).
    Hosted,
}

/// A hosted run's world for what is not on its hot path: made at the first effect sent
/// through the escape ([`crate::guest::Run`] keeps it for the run).
pub(crate) struct Escape(Lock<World>);

impl Escape {
    /// The world over the host's `table`: a [`Backend::Guest`] with no environment, no
    /// trace kept (a hosted run's spans are its host's to see), `limits`; the escape's
    /// encodings made in `alloc`, the run's heap.
    pub(crate) fn new(table: crate::wire::HostTable, limits: Limits, alloc: blaze_own::SharedAlloc<'static>) -> Escape {
        let world = World::new(Start {
            // alloc: once per hosted run that sends an effect, the escape's driver.
            driver: Backend::Guest(Box::new(crate::guest::Guest::new(table, alloc))),
            seed: None,
            // The hosted wire carries no folders above its root.
            above: 0,
            env: Vars::given(Vec::new()),
            args: ArgList::given(Vec::new()),
            host: HostFacts {
                pid: 0,
                cores: core::num::NonZero::<usize>::MIN,
                // alloc: once per hosted run that sends an effect.
                name: Arc::from(""),
                // alloc: once per hosted run that sends an effect.
                born: Born::Unknown(Arc::from("the hosted wire does not carry the process's start yet")),
                build_id: None,
                // The hosted wire does not carry the program's path.
                exe: None,
            },
            offers: crate::guest::OFFERS,
            limits,
            hosted: None,
            disk: None,
            plant: Plant::None,
        });
        Escape(Lock::new("bstd.io.world", world))
    }
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
    /// What standard output and standard error are drawn on, in that order: not terminals
    /// unless a test says so.
    pub ttys: [crate::term::Tty; 2],
    /// The folders above its root, nearest first ([`crate::fs::Fs::ancestors`]): none unless a
    /// test says so.
    pub above: Vec<blaze_core::sim::SimAbove>,
    /// How many of them are granted: all by default.
    pub top: u8,
    /// The bounds its waits, closes and trace end by.
    #[blaze::policy]
    pub limits: Limits,
    /// How many simulated workers its tasks move between (D171, [`crate::workers`]): each has
    /// its own cell. [`SIM_WORKERS`] unless a test says so; 0 seats its roots as a real
    /// world's are, on leased caller cells.
    #[blaze::policy]
    pub models: u32,
}

/// The simulated workers of a [`SimWorld`] unless a test says otherwise: enough that a task
/// that moves at each wait lands on a cell other than its last.
pub const SIM_WORKERS: u32 = 4;

/// What the simulation's workers' streams start from, besides the seed: so they draw apart
/// from the world's own generator.
const WORKERS_SALT: u64 = 0x5745_524B_4552_5331;

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
            ttys: [crate::term::Tty::NotTerminal; 2],
            // alloc: none, no folders above.
            above: Vec::new(),
            top: u8::MAX,
            models: SIM_WORKERS,
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
    pub env: Vars,
    pub args: ArgList,
    pub host_name: Arc<str>,
    /// The seed of [`Io::rng`]'s generator: `None` draws it from the host's secure source at
    /// the first [`Io::rng`], so a program that never asks for randomness never draws.
    pub seed: Option<u64>,
    /// How many folders above `root` it may read ([`crate::fs::Fs::ancestors`]): every one
    /// up to the filesystem's root (`u8::MAX`) for a command, none for a launched program.
    pub above: u8,
    /// The bounds its waits, closes and trace end by.
    pub limits: Limits,
}

/// What a world starts from, whichever its driver.
struct Start {
    driver: Backend,
    seed: Option<u64>,
    /// The granted depth above the root ([`Io::above_most`]).
    above: u8,
    env: Vars,
    args: ArgList,
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
        // The world's workers (D171): its `models` simulated workers' cells and bstd's own
        // fields, each worker's stream drawn from the world's seed.
        let setup = crate::workers::WorkersSetup {
            limits: crate::workers::Limits::BARE,
            seed: Some(world.seed ^ WORKERS_SALT),
            models: world.models,
        };
        let workers = match crate::workers::Workers::bare(setup) {
            Ok(workers) => workers,
            Err(error) => panic!("bstd::Io::simulated of seed {}: {error}", world.seed),
        };
        Io::planted_on(world, plant, workers)
    }

    /// [`Io::simulated`] over `workers`, made by a test's main with its slots registered
    /// ([`crate::workers::WorkersOpts`]): its tasks' `on_worker` holds `workers`' cells.
    pub fn simulated_on(world: SimWorld, workers: crate::workers::Workers) -> Io {
        Io::planted_on(world, Plant::None, workers)
    }

    /// This world's handle, its tasks' per-worker state `workers` instead (D171): what main
    /// does once after registering its slots, before it makes the root it runs on. Contexts
    /// made from the old handle keep the old cells; nothing is shared between the two.
    pub fn on_workers(&self, workers: crate::workers::Workers) -> Io {
        let mut io = self.clone();
        match &mut io.reach {
            Reach::Shared { workers: held, .. } => *held = workers,
            Reach::Hosted => crate::guest::Run::current().set_workers(workers),
        }
        io
    }

    /// The world's per-worker state: what `Cx::on_worker` holds a cell of.
    pub(crate) fn workers(&self) -> &crate::workers::Workers {
        match &self.reach {
            Reach::Shared { workers, .. } => workers,
            Reach::Hosted => crate::guest::Run::current().workers(),
        }
    }

    fn planted_on(world: SimWorld, plant: Plant, workers: crate::workers::Workers) -> Io {
        Io::over(Start {
            // alloc: once per simulated world.
            driver: Backend::Sim(Box::new({
                let mut sim = Sim::new(world.seed, world.faults);
                sim.set_above(world.above);
                sim
            })),
            seed: Some(world.seed ^ RNG_SALT),
            above: world.top,
            env: Vars::given(world.env),
            args: ArgList::given(world.args),
            host: world.host,
            offers: world.offers,
            limits: world.limits,
            // alloc: once per simulated world, its host.
            hosted: Some(Box::new(SimHost { ttys: world.ttys, ..SimHost::new() })),
            // alloc: once per simulated world, its disk's ledger.
            disk: Some(Box::new(Disk::new())),
            plant,
        }, workers)
    }

    /// The target's own driver over `world.root`.
    ///
    /// The authority to touch the real system from nowhere: it reaches the process's root
    /// itself. Its one home is [`Io::native_in`], which takes the root (`&blaze_core::Os`);
    /// this form goes in batch B1 (port27/bstd-capabilities.md) with its callers moved to
    /// `native_in` (an entry's `Os::process()`, a test's `bstd::test::native`).
    #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
    pub fn native(world: NativeWorld<'_>) -> Result<Io, OpenError> {
        Io::native_in(blaze_core::Os::process(), world)
    }

    /// The world of the hosted run the calling thread is in ([`crate::guest::Run`]), offering
    /// `offers`. Its state is the run's, found through the thread at each reach, so this
    /// value holds none: an `Io` of it used on another thread, or after the run, stops with a
    /// panic naming that. The wire carries no host facts yet, so `offers` withholds `host`.
    pub(crate) fn hosted(offers: Offers, limits: Limits) -> Io {
        debug_assert!(!offers.host, "a hosted world has no host facts to offer");
        Io { reach: Reach::Hosted, offers, limits, plant: Plant::None }
    }

    /// Whether a hosted world's host grants the parameter `name` of kind `kind`: what a
    /// hosted program's start asks for each of `main`'s parameters ([`crate::guest::grant`]).
    pub(crate) fn guest_grant(&self, kind: &str, name: &str) -> Result<(), crate::guest::WireError> {
        match &self.reach {
            Reach::Hosted => crate::guest::grant_of(&crate::guest::Run::current().table, kind, name),
            Reach::Shared { .. } => Err(crate::guest::WireError { entry: "grant", refusal: crate::wire::Refusal::Unknown }),
        }
    }

    /// The world's lock: a shared world's, or a hosted run's escape, made now if this is its
    /// first use. Everything off a hosted run's hot path reaches its world here.
    /// How many folders above the root this world may read ([`crate::fs::Fs::ancestors`]):
    /// none for a hosted run, whose wire carries none.
    pub(crate) fn above_most(&self) -> u8 {
        match &self.reach {
            Reach::Hosted => 0,
            Reach::Shared { .. } => self.world().lock().above,
        }
    }

    fn world(&self) -> &Lock<World> {
        // The tests' count of reaches of the lock (io/tests.rs, `native_now_takes_no_lock`).
        #[cfg(test)]
        self.counted(|seen| &seen.locks);
        match &self.reach {
            Reach::Shared { world, .. } => world,
            Reach::Hosted => &crate::guest::Run::current().escape().0,
        }
    }

    fn over(start: Start, workers: crate::workers::Workers) -> Io {
        let (offers, limits, plant) = (start.offers, start.limits, start.plant);
        // clone: the world's environment and command line, `Arc`s.
        let (env, args) = (start.env.clone(), start.args.clone());
        // Chosen once, from the driver, before it moves under the lock.
        let beside = fast::Beside::of(&start.driver, limits.trace_most);
        Io {
            // alloc: once per world.
            reach: Reach::Shared { world: Arc::new(Lock::new("bstd.io.world", World::new(start))), env, args, beside, workers },
            offers,
            limits,
            plant,
        }
    }
}

/// A world's standard error as a `core::fmt::Write`, each piece one raw write: what an exit
/// says a refused flush with, allocating nothing.
struct RawErr<'a>(&'a Io);

impl core::fmt::Write for RawErr<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        match self.0.write_out(Stream::Err, &[text.as_bytes()]) {
            Ok(()) => Ok(()),
            Err(_) => Err(core::fmt::Error),
        }
    }
}

/// Where `stream`'s per-stream state sits in a world's pairs (`ttys`, `pending`,
/// `SimHost::writes`).
fn stream_at(stream: Stream) -> usize {
    match stream {
        Stream::Out => 0,
        Stream::Err => 1,
    }
}

/// Whether `stream` is drawn on a terminal, from a world's borrowed parts: what [`Io::tty`]
/// answers, for a caller that holds the lock already.
fn is_terminal(
    driver: &mut Backend,
    hosted: &Option<Box<SimHost>>,
    ttys: &mut [Option<crate::term::Tty>; 2],
    stream: Stream,
) -> bool {
    let at = stream_at(stream);
    let tty = match hosted.as_deref() {
        Some(host) => host.ttys[at],
        None => *ttys[at].get_or_insert_with(|| driver.tty(stream)),
    };
    matches!(tty, crate::term::Tty::Terminal(_))
}

/// `parts` on `stream` as one write, with nothing before it: the simulated host keeps and
/// counts it ([`SimHost::writes`]), a real world's backend makes it.
fn emit(driver: &mut Backend, hosted: &mut Option<Box<SimHost>>, stream: Stream, parts: &[&[u8]]) -> Result<(), String> {
    match hosted.as_deref_mut() {
        Some(host) => {
            host.writes[stream_at(stream)] += 1;
            let kept = match stream {
                Stream::Out => &mut host.out,
                Stream::Err => &mut host.err,
            };
            for part in parts {
                kept.extend_from_slice(part);
            }
            Ok(())
        }
        None => driver.write_out(stream, parts),
    }
}

/// A buffer's write ([`crate::out::Pending`]): recorded as the span `bstd.out.write` opened
/// inside `parent`, then made. Recorded here, under the lock the write holds, so an append
/// that writes nothing records nothing and takes the lock once.
fn written(
    driver: &mut Backend,
    hosted: &mut Option<Box<SimHost>>,
    trace: &mut Trace,
    stream: Stream,
    parent: &'static str,
    parts: &[&[u8]],
) -> Result<(), String> {
    trace.push(Record { at: driver.now(), span: "bstd.out.write", kind: RecordKind::Opened { parent } });
    emit(driver, hosted, stream, parts)
}

impl World {
    fn new(start: Start) -> World {
        let Start { driver, seed, above, env: _, args: _, host, offers: _, limits, hosted, disk, plant: _ } = start;
        World {
            driver,
            above,
            next: 0,
            // alloc: the effects' queue, grown once to the most made between waits.
            effects: Vec::new(),
            // alloc: the poll's buffer, grown once to the most a poll moves.
            polled: Vec::new(),
            // alloc: the completions kept for their waiters.
            got: Vec::new(),
            // alloc: the tags of waits that stopped.
            abandoned: Vec::new(),
            rng: seed.map(SplitMix64::new),
            // alloc: none until a test records.
            touched: Vec::new(),
            asked: Vec::new(),
            record: false,
            // alloc: none until a file is changed.
            changed: BTreeMap::new(),
            trace: Trace::new(limits.trace_most),
            host,
            exited: None,
            ttys: [None; 2],
            pending: [crate::out::Pending::new(), crate::out::Pending::new()],
            hosted,
            disk,
        }
    }
}

impl Io {
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
    ///
    /// Built plainly, for what a program that never waits uses: `Duration::MAX` is no deadline
    /// (the clock is not read for it), and the frame is made at its first use
    /// ([`Cx::frame`]), so a root no one allocates in costs one allocation, its task's
    /// placement. A frame reserves nothing until it hands out bytes, so making it later
    /// refuses nothing it would have refused here.
    pub fn cx(&self, budget: Duration, scratch: usize) -> Result<Cx, Stopped> {
        let deadline = match budget {
            Duration::MAX => None,
            budget => Some(Deadline::after(self.now(), budget)),
        };
        Ok(Cx::root(deadline, scratch, self.clone()))
    }

    /// Narrow `Send + Sync + Clone` capability that only spawns root [`Cx`] values (no `Fs`,
    /// `Env`, `Net`, `Clock`, `Out`, `Args`, `Host`, or `Rng`).
    pub fn spawner(&self) -> crate::cx::CxSpawn {
        crate::cx::CxSpawn::new(self.clone())
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
        self.offered(|it| it.rng).then(|| {
            let seeded = self.world().lock().rng.is_some();
            // The seed is one `Effect::Random` through the driver, drawn at the first ask and
            // kept: every later number is the generator's, so a run is replayed by its seed.
            let seed = if seeded { None } else { Some(self.draw_seed()) };
            let mut world = self.world().lock();
            let rng = match (&mut world.rng, seed) {
                (Some(rng), _) => rng,
                (slot @ None, Some(seed)) => slot.insert(SplitMix64::new(seed)),
                (None, None) => unreachable!("the generator was there under the lock, and nothing takes it away"),
            };
            Rng::new(rng.next_u64())
        })
    }

    /// Eight bytes of the driver's secure source, asked as one [`Effect::Random`]. A driver
    /// that does not answer within [`Limits::abandon_wait`], or fails, is a host that cannot
    /// give randomness, which `rng` offered: that panics, naming it.
    fn draw_seed(&self) -> u64 {
        let tag = self.submit(|tag| Effect::Random { len: 8, tag });
        let until = Deadline::after(self.now(), self.limits.abandon_wait);
        match self.settle(tag, until, "bstd.io.rng.seed") {
            Ok(Done::Random(bytes)) if bytes.len() == 8 => {
                let mut seed = [0; 8];
                seed.copy_from_slice(&bytes);
                u64::from_le_bytes(seed)
            }
            Ok(done) => panic!("bstd: Io::rng's seed: the driver answered {done:?}, not 8 random bytes"),
            Err(stopped) => panic!("bstd: Io::rng's seed: the driver did not answer: {stopped}"),
        }
    }

    pub fn need_rng(&self) -> Result<Rng, Missing> {
        self.rng().ok_or(Missing { capability: "rng" })
    }

    /// Every variable of the world's environment, if the host offers it; narrow it with
    /// [`Env::only`].
    pub fn env(&self) -> Option<Env> {
        self.offered(|it| it.env).then(|| {
            let vars = match &self.reach {
                // clone: an `Arc`, or the process's borrowed block.
                Reach::Shared { env, .. } => env.clone(),
                // The wire carries no environment yet.
                Reach::Hosted => Vars::given(Vec::new()),
            };
            Env::new(vars, self.plant())
        })
    }

    pub fn need_env(&self) -> Result<Env, Missing> {
        self.env().ok_or(Missing { capability: "env" })
    }

    /// The command line, if the host hands one over.
    pub fn args(&self) -> Option<Args> {
        self.offered(|it| it.args).then(|| {
            let args = match &self.reach {
                // clone: an `Arc`, or the process's borrowed block.
                Reach::Shared { args, .. } => args.clone(),
                // Asked of the host at this first ask, and kept for the run.
                Reach::Hosted => crate::guest::Run::current().args().clone(),
            };
            Args::new(args)
        })
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
    ///
    /// Written plainly (no closure), as [`Io::err`]: it is on every hello's path.
    pub fn out(&self) -> Option<Out> {
        match self.offers.out || self.plant == Plant::OffersIgnored {
            true => Some(Out::new(self.clone(), Stream::Out)),
            false => None,
        }
    }

    /// Standard output, which `main` requires.
    pub fn need_out(&self) -> Result<Out, Missing> {
        match self.out() {
            Some(out) => Ok(out),
            None => Err(Missing { capability: "out" }),
        }
    }

    /// Standard error, if the host offers it: an [`Out`] of the other stream.
    pub fn err(&self) -> Option<Out> {
        match self.offers.err || self.plant == Plant::OffersIgnored {
            true => Some(Out::new(self.clone(), Stream::Err)),
            false => None,
        }
    }

    /// Standard error, which `main` requires.
    pub fn need_err(&self) -> Result<Out, Missing> {
        match self.err() {
            Some(err) => Ok(err),
            None => Err(Missing { capability: "err" }),
        }
    }

    /// Standard input, if the host offers the standard streams (the offer `out` is: one for
    /// all three).
    pub fn input(&self) -> Option<crate::input::In> {
        match self.offers.out || self.plant == Plant::OffersIgnored {
            true => Some(crate::input::In::new(self.clone())),
            false => None,
        }
    }

    /// Standard input, which `main` requires.
    pub fn need_input(&self) -> Result<crate::input::In, Missing> {
        self.input().ok_or(Missing { capability: "input" })
    }

    /// Whether standard input is a terminal: the kernel's answer for a native world
    /// (descriptor 0, read when asked), never for a simulated world (its host scripts no
    /// terminal input) or a hosted run (the wire does not carry it).
    pub(crate) fn input_is_terminal(&self) -> bool {
        if let Reach::Hosted = self.reach {
            return false;
        }
        let mut world = self.world().lock();
        let World { driver, .. } = &mut *world;
        match driver {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(_) => matches!(crate::rt::tty::read_fd(0), crate::term::Tty::Terminal(_)),
            Backend::Sim(_) | Backend::Guest(_) => false,
        }
    }

    /// `parts`, in order, on this world's `stream`, now: what the stream's buffer holds is
    /// written first (one write), then `parts` as one write. Kept by the simulated host
    /// ([`SimHost::out`], [`SimHost::err`]), or written to the process's by `rt`.
    /// A hosted run's goes straight to its host's `write` ([`crate::guest::write_to`]).
    pub(crate) fn write_out(&self, stream: Stream, parts: &[&[u8]]) -> Result<(), String> {
        if let Reach::Hosted = self.reach {
            return match crate::guest::write_to(&crate::guest::Run::current().table, stream, parts) {
                Ok(()) => Ok(()),
                // alloc: an error's message.
                Err(error) => Err(error.to_string()),
            };
        }
        let mut world = self.world().lock();
        let World { driver, hosted, pending, trace, .. } = &mut *world;
        pending[stream_at(stream)]
            .flush(&mut |held: &[&[u8]]| written(&mut *driver, &mut *hosted, &mut *trace, stream, "bstd.io.write_out", held))?;
        emit(driver, hosted, stream, parts)
    }

    /// `bytes` appended to `stream`'s buffer ([`crate::out`], "Buffering"), which is written
    /// when full; `end` when they end a call, which writes it when the stream is a terminal.
    /// Each write is recorded as the span `bstd.out.write` inside `parent`. A hosted run holds
    /// no buffer: its bytes go to its host as they come, as one write each.
    pub(crate) fn put_out(&self, stream: Stream, parent: &'static str, bytes: &[u8], end: bool) -> Result<(), String> {
        if let Reach::Hosted = self.reach {
            if bytes.is_empty() {
                return Ok(());
            }
            return self.write_out(stream, &[bytes]);
        }
        let at = stream_at(stream);
        let mut world = self.world().lock();
        let World { driver, hosted, ttys, pending, trace, .. } = &mut *world;
        let terminal = end && is_terminal(&mut *driver, &*hosted, &mut *ttys, stream);
        let mut write =
            |parts: &[&[u8]]| -> Result<(), String> { written(&mut *driver, &mut *hosted, &mut *trace, stream, parent, parts) };
        pending[at].put(bytes, &mut write)?;
        if terminal {
            pending[at].flush(&mut write)?;
        }
        Ok(())
    }

    /// What `stream`'s buffer holds, written now as one write (recorded inside `parent`);
    /// nothing when it is empty or the world is a hosted run's, which holds none.
    pub(crate) fn flush_out(&self, stream: Stream, parent: &'static str) -> Result<(), String> {
        if let Reach::Hosted = self.reach {
            return Ok(());
        }
        let mut world = self.world().lock();
        let World { driver, hosted, pending, trace, .. } = &mut *world;
        pending[stream_at(stream)].flush(&mut |parts: &[&[u8]]| written(&mut *driver, &mut *hosted, &mut *trace, stream, parent, parts))
    }

    /// Both streams' buffers written, standard output first; both are tried, and the first
    /// refusal is the error. What the entry calls when `main` ends, and [`Io::exit`].
    pub(crate) fn flush_streams(&self, parent: &'static str) -> Result<(), crate::out::OutError> {
        let out = self.flush_out(Stream::Out, parent);
        let err = self.flush_out(Stream::Err, parent);
        match (out, err) {
            (Err(said), _) => Err(crate::out::OutError { stream: Stream::Out, why: crate::out::OutWhy::Host(said) }),
            (Ok(()), Err(said)) => Err(crate::out::OutError { stream: Stream::Err, why: crate::out::OutWhy::Host(said) }),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// How many bytes `stream`'s buffer holds: a test's view of what is not written yet.
    #[cfg(test)]
    pub(crate) fn held_out(&self, stream: Stream) -> usize {
        self.world().lock().pending[stream_at(stream)].held()
    }

    /// What `stream` is drawn on, read through the backend once and kept: a simulated world's
    /// from its host, which a test sets ([`SimWorld::ttys`]).
    pub(crate) fn tty(&self, stream: Stream) -> crate::term::Tty {
        let at = match stream {
            Stream::Out => 0,
            Stream::Err => 1,
        };
        if let Reach::Hosted = self.reach {
            // The wire does not carry it yet (`Backend::tty`).
            return crate::term::Tty::NotTerminal;
        }
        let mut world = self.world().lock();
        let World { driver, hosted, ttys, .. } = &mut *world;
        if let Some(host) = hosted.as_deref() {
            return host.ttys[at];
        }
        *ttys[at].get_or_insert_with(|| driver.tty(stream))
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
        let native = self.world().lock().driver.sim().is_none();
        Gpu::offered(native)
    }

    /// The graphics device, which `main` requires (a whole `gpu` grant): refused by name where
    /// the world has none (the simulation, and every target but native macOS for now).
    pub fn need_gpu(&self) -> Result<Gpu, Missing> {
        self.gpu().ok_or(Missing { capability: "gpu" })
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
    /// [`Http::over`]. Behind bstd's `net` feature.
    #[cfg(feature = "requests")]
    pub fn http(&self) -> Option<crate::requests::Http> {
        self.offered(|it| it.http).then(|| crate::requests::Http::host(self.clone()))
    }

    #[cfg(feature = "requests")]
    pub fn need_http(&self) -> Result<crate::requests::Http, Missing> {
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
    /// which has no provider for them. What the streams' buffers hold is handed to the host
    /// first (a write each, as a flush makes), so `f` reads all that was said, in order.
    pub fn script<R>(&self, f: impl FnOnce(&mut SimHost) -> R) -> Option<R> {
        let mut world = self.world().lock();
        let World { driver, hosted, pending, trace, .. } = &mut *world;
        let host = hosted.as_deref_mut()?;
        for stream in [Stream::Out, Stream::Err] {
            let held = &mut pending[stream_at(stream)];
            if held.held() == 0 {
                continue;
            }
            // A simulated host takes every write, so this flush cannot fail: the closure says
            // so in its type.
            let kept: Result<(), core::convert::Infallible> = held.flush(&mut |parts: &[&[u8]]| {
                trace.push(Record { at: driver.now(), span: "bstd.out.write", kind: RecordKind::Opened { parent: "bstd.io.script" } });
                host.writes[stream_at(stream)] += 1;
                let kept = match stream {
                    Stream::Out => &mut host.out,
                    Stream::Err => &mut host.err,
                };
                for part in parts {
                    kept.extend_from_slice(part);
                }
                Ok(())
            });
            let Ok(()) = kept;
        }
        Some(f(host))
    }

    /// `f` on the simulation, for a test to change the world (advance the clock, fail syncs,
    /// hold a lock); `None` on a real driver.
    pub fn with_sim<R>(&self, f: impl FnOnce(&mut Sim) -> R) -> Option<R> {
        self.world().lock().driver.sim().map(f)
    }

    /// The simulated machine loses power ([`Sim::crash`]): every effect in flight is gone,
    /// and with it what this `Io` kept for their waiters; each file keeps what was flushed
    /// and a prefix of what was not, and the name changes no directory sync covered may be
    /// undone (blaze-core's `sim`). `false` on a real driver.
    pub fn crash(&self) -> bool {
        let mut world = self.world().lock();
        let World { driver, got, abandoned, polled, effects, disk, .. } = &mut *world;
        let Some(sim) = driver.sim() else { return false };
        sim.crash();
        if let Some(disk) = disk {
            disk.crashed();
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
        let mut world = self.world().lock();
        if let Reach::Shared { beside, .. } = &self.reach {
            let dropped = beside.trace.take_dropped();
            if dropped > 0 {
                world.trace.add_dropped(dropped);
            }
        }
        f(&world.trace)
    }

    /// The status the simulated process exited with, if it did ([`Host::exit`]).
    pub fn exited(&self) -> Option<i32> {
        self.world().lock().exited
    }

    /// A record of `span`, now on the driver's clock.
    /// A hosted run keeps none: its spans are its host's to see (run.md, "The hosted world").
    /// Once `trace_most` is 0 or full, takes no world lock: `trace_most == 0` returns at once,
    /// and a full trace counts its drops atomically beside the lock.
    pub(crate) fn record_trace(&self, span: &'static str, kind: RecordKind) {
        let Reach::Shared { beside, .. } = &self.reach else {
            return;
        };
        if self.limits.trace_most == 0 {
            return;
        }
        if !beside.trace.is_open() {
            beside.trace.drop_one();
            return;
        }
        let mut world = self.world().lock();
        let at = world.driver.now();
        if !world.trace.push(Record { at, span, kind }) {
            beside.trace.close();
        }
    }

    pub(crate) fn host_facts<R>(&self, f: impl FnOnce(&HostFacts) -> R) -> R {
        f(&self.world().lock().host)
    }

    /// The process ends with `status`: a real one exits; the simulated one remembers it and
    /// unwinds the calling task with an [`Exit`] payload. What the streams' buffers hold is
    /// written first; a refused write turns a status of 0 into 1, said on standard error.
    pub(crate) fn exit(&self, status: i32) -> ! {
        // A hosted program's run is ended by its host: its table's `exit` does not return.
        if let Reach::Hosted = self.reach {
            crate::guest::exit_to(&crate::guest::Run::current().table, status);
        }
        let status = match self.flush_streams("bstd.host.exit") {
            Ok(()) => status,
            Err(error) => {
                // The stream that refused may be standard error itself: nothing is left to
                // say it on then, and the status still says the run failed.
                let _unsaid = core::fmt::write(&mut RawErr(self), format_args!("{error}\n"));
                if status == 0 { 1 } else { status }
            }
        };
        let simulated = {
            let mut world = self.world().lock();
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

    /// The driver's clock: a native world's read beside its lock, with none taken (the
    /// driver's own clock, on the instant it began: [`fast::Time::Native`]); a simulated
    /// world's virtual clock under its lock, which moves only inside the driver's own calls.
    pub(crate) fn now(&self) -> Duration {
        match &self.reach {
            Reach::Shared { beside, .. } => match beside.now() {
                Some(now) => now,
                None => self.world().lock().driver.now(),
            },
            Reach::Hosted => crate::guest::now_of(&crate::guest::Run::current().table),
        }
    }

    /// The driver's wall clock, in ns since the Unix epoch ([`blaze_core::Driver::wall_ns`]).
    pub(crate) fn wall_ns(&self) -> u64 {
        match &self.reach {
            Reach::Shared { world, .. } => world.lock().driver.wall_ns(),
            Reach::Hosted => crate::guest::wall_ns_of(&crate::guest::Run::current().table),
        }
    }

    /// Which boot of the machine this is ([`blaze_core::Driver::boot`]): read once by the
    /// native driver as it was made; in the simulation, drawn anew at every power cut. For what
    /// is trusted within one boot (a pack verified once per boot); never part of a key.
    pub fn boot(&self) -> Result<blaze_core::Boot, blaze_core::NoBoot> {
        match &self.reach {
            Reach::Shared { world, .. } => world.lock().driver.boot(),
            Reach::Hosted => Err(blaze_core::NoBoot { source: "a hosted guest", why: String::from("its host table carries no boot") }),
        }
    }

    /// A native world over `root`, beside this one, for a second tree the program may write
    /// ([`crate::fs::Fs::home`]): this world's limits, no environment, no arguments, nothing
    /// above its root, one file thread. `None` when this world is not native: the simulation
    /// keeps every tree under its one root, and a hosted guest has no second tree.
    #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
    pub(crate) fn beside(&self, root: &crate::rt::RootPath) -> Option<Result<Io, OpenError>> {
        let Reach::Shared { world, .. } = &self.reach else { return None };
        if !matches!(world.lock().driver, Backend::Native(_)) {
            return None;
        }
        Some(Io::native(NativeWorld {
            root,
            above: 0,
            threads: 1,
            // alloc: none, an empty environment and command line.
            env: Vars::given(Vec::new()),
            args: ArgList::given(Vec::new()), // alloc: as above.
            // alloc: once per second tree, the host's name (empty, as the start's).
            host_name: Arc::from(""),
            seed: None,
            limits: self.limits,
        }))
    }

    /// No native driver on this target: no second tree.
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi")))]
    pub(crate) fn beside(&self, _root: &crate::rt::RootPath) -> Option<Result<Io, OpenError>> {
        None
    }

    /// Whether the system keeps a journal of changes across a restart, so an
    /// [`Effect::Journal`] from a saved cursor names what changed while nothing ran: macOS
    /// (FSEvents' per-volume store) and Windows (NTFS's USN journal) do; Linux does not, and
    /// there a journal costs a watch's start (an inotify walk of every folder) to say only
    /// that everything is lost. The simulation's by the journal it plays
    /// ([`blaze_core::sim::Sim::journal`]); a hosted program's world has none it can reach.
    pub(crate) fn keeps_journal(&self) -> bool {
        match &self.reach {
            Reach::Shared { world, .. } => match &world.lock().driver {
                #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
                Backend::Native(_) => cfg!(any(target_os = "macos", windows)),
                Backend::Sim(sim) => sim.journal() != blaze_core::sim::Journal::None,
                Backend::Guest(_) => false,
            },
            Reach::Hosted => false,
        }
    }

    /// Whether this world is the simulation's.
    pub(crate) fn is_sim(&self) -> bool {
        match &self.reach {
            Reach::Shared { world, .. } => matches!(world.lock().driver, Backend::Sim(_)),
            Reach::Hosted => false,
        }
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn plant(&self) -> Plant {
        self.plant
    }

    /// Start recording the files effects name ([`Io::touched`]).
    #[cfg(test)]
    pub(crate) fn record(&self) {
        if let Reach::Shared { beside, .. } = &self.reach {
            beside.recording.store(true, core::sync::atomic::Ordering::Release);
        }
        self.world().lock().record = true;
    }

    /// The files effects named since [`Io::record`], and whether each effect changed it.
    #[cfg(test)]
    pub(crate) fn touched(&self) -> Vec<(FileName, bool)> {
        // clone: a test's copy of the record.
        self.world().lock().touched.clone()
    }

    /// The effects handed to the driver since [`Io::record`], in order, the record emptied.
    #[cfg(test)]
    pub(crate) fn asked(&self) -> Vec<Effect> {
        core::mem::take(&mut self.world().lock().asked)
    }

    /// `effect` of a fresh tag, handed to the driver, then waited for under `cx`: one call.
    /// A `cx` that stopped already asks nothing of the driver. On a native world whose driver
    /// gives inline calls, a blocking file call of blaze-core's table is made on this thread
    /// instead, its `Done` the queue's ([`fast`]); every other effect, and every effect of a
    /// simulated world, is queued and waited for as before.
    pub(crate) fn call(&self, cx: &Cx, effect: impl FnOnce(Tag) -> Effect) -> Result<Done, Stopped> {
        let began = self.now();
        cx.stopped_by(began)?;
        // The meter (D169): the call's time as its asker waited, by its kind (counted at the
        // stamp, `fast::stamp`), on this world's clock.
        let mut kind = blaze_meter::Call::Other;
        let effect = |tag| {
            let effect = effect(tag);
            kind = effect.metered();
            effect
        };
        let done = match self.inline_files() {
            Some(files) => self.call_inline(cx, files, effect),
            None => {
                let tag = self.submit(effect);
                cx.wait(self, tag, Ends::Once)
            }
        };
        let ns = u64::try_from(self.now().saturating_sub(began).as_nanos()).unwrap_or(u64::MAX);
        // The call's time on the worker the asker is on (D171): its meter cell, not the
        // thread's. Its count stays at the stamp (`fast::stamp`), which has no context: both
        // are read together by `Meter::now_with` of the worker's tally.
        // The telemetry's effect row of `kind` too (D178): its count and time; the bytes moved
        // are not known at this seam (0: the report's bytes column counts only what a driver
        // reports, which none does yet).
        cx.on_worker(|w| {
            w.meter().call_took(kind, ns);
            if let Some((_, cell)) = w.telemetry() {
                cell.effect(kind, 0, ns);
            }
        });
        done
    }

    /// `effect` of a fresh tag, handed to the driver now.
    pub(crate) fn submit(&self, effect: impl FnOnce(Tag) -> Effect) -> Tag {
        let mut world = self.world().lock();
        let effect = fast::stamp(&mut world, effect);
        let tag = effect.tag();
        self.hand_over(world, effect);
        tag
    }

    /// The generation of `name`: the number of the last effect of this world that changed
    /// its bytes or made it, 0 for a file no effect of this world changed (or one since
    /// removed). Changes by another process are not seen: blaze-core's `Meta` carries no
    /// inode or change time to compare (std.md, "still implicit").
    pub(crate) fn generation(&self, name: &FileName) -> u64 {
        self.world().lock().changed.get(name).copied().unwrap_or(0)
    }

    /// A number no other call of this world was given: a tag's, taken for a name made beside
    /// another (a publish's temporary file).
    pub(crate) fn fresh(&self) -> u64 {
        let mut world = self.world().lock();
        let fresh = world.next;
        world.next += 1;
        fresh
    }

    /// What the simulated disk was asked to do so far ([`DiskCounts`]): bytes written and
    /// read, flushes, waits for the device, clones and the bytes they forced out. `None` on a
    /// real driver, which keeps no ledger.
    pub fn disk(&self) -> Option<DiskCounts> {
        self.world().lock().disk.as_deref().map(Disk::counts)
    }

    /// A caller waited for the device (a flush it did not return before): counted.
    pub(crate) fn waited_for_device(&self) {
        if !self.has_disk() {
            return;
        }
        if let Some(disk) = self.world().lock().disk.as_deref_mut() {
            disk.waited();
        }
    }

    /// A sync of the directory `dir`'s names completed and was waited for
    /// ([`Effect::SyncDir`]): the simulated disk's ledger counts the sync and the wait; a
    /// real world has no ledger.
    pub(crate) fn names_synced(&self) {
        if !self.has_disk() {
            return;
        }
        if let Some(disk) = self.world().lock().disk.as_deref_mut() {
            disk.names_synced();
            disk.waited();
        }
    }

    /// How many effects the driver holds that have not completed: a test's view of whether
    /// anything a wait gave back is still in flight.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.world().lock().driver.in_flight()
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

    /// Every effect handed to the driver ended, waited for until `until`: what a command does
    /// before it exits without a teardown, so a write-back or a name sync begun and not waited
    /// for (`Unsynced::Started`) has reached the kernel, not died with a thread. Past `until`,
    /// or after [`Limits::poll_fuel`] polls in a row that bring nothing, the stop says how
    /// many were left.
    pub(crate) fn drain(&self, until: Deadline) -> Result<(), Stopped> {
        let span = "bstd::rt: the effects in flight at the end of main";
        let mut idle = 0;
        while self.world().lock().driver.in_flight() > 0 {
            if until.passed(self.now()) {
                return Err(Stopped::Deadline { span, at: until.get() });
            }
            if idle == self.limits.poll_fuel {
                return Err(Stopped::Stalled { span, polls: idle });
            }
            match self.poll(until) {
                0 => idle += 1,
                _ => idle = 0,
            }
        }
        Ok(())
    }

    /// The tag's waiter is gone: its completions are dropped as they arrive.
    pub(crate) fn forget(&self, tag: Tag, ends: Ends) {
        self.world().lock().abandoned.push((tag, ends));
    }

    /// The next completion of `tag`, if one was polled: taken for its waiter.
    pub(crate) fn take(&self, tag: Tag) -> Option<Done> {
        let mut world = self.world().lock();
        let at = world.got.iter().position(|it| it.tag == tag)?;
        Some(world.got.remove(at).done)
    }

    /// The first already-polled completion matching any tag in `tags`, returning `(index, done)`.
    pub(crate) fn take_any(&self, tags: &[Tag]) -> Option<(usize, Done)> {
        let mut world = self.world().lock();
        for (pos, item) in world.got.iter().enumerate() {
            if let Some(idx) = tags.iter().position(|&t| t == item.tag) {
                let done = world.got.remove(pos).done;
                return Some((idx, done));
            }
        }
        None
    }

    /// One poll of the driver, waiting at most until `until`: what it brought kept for each
    /// waiter (or dropped, a stopped waiter's), and how many completions it moved. A poll that
    /// moved nothing moves the simulation's clock to `until`, as a real wait sleeps to it. The
    /// step of [`Cx::wait`], the one wait.
    pub(crate) fn poll(&self, until: Deadline) -> usize {
        let mut world = self.world().lock();
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
        Ends::Watch => matches!(completion.done, Done::Unwatched | Done::Failed(_)),
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
        | Effect::MapWrite { file, .. }
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
        // A copy of a file's data reads `from` and makes `to`.
        Effect::CopyData { from, to, .. } => {
            named(from, false);
            named(to, true);
        }
        // A prewarm reads which pages of `like` are cached and asks for those of `file`: it
        // names both and changes neither.
        Effect::Prewarm { like, file, .. } => {
            named(like, false);
            named(file, false);
        }
        Effect::Watch { dir, .. } => named(dir, false),
        Effect::Private { dir, .. } | Effect::MakeDir { dir, .. } | Effect::RemoveDir { dir, .. } => named(dir, true),
        // The root has no name to record.
        Effect::List { dir, .. } | Effect::Entries { dir, .. } | Effect::Journal { dir, .. } => {
            if let Some(dir) = dir.name() {
                named(dir, false);
            }
        }
        Effect::SyncDir { dir, .. } => {
            if let Some(dir) = dir.name() {
                named(dir, true);
            }
        }
        // A batch of reads names each part's file, and changes none.
        Effect::ReadMany { parts, .. } => {
            for part in parts {
                named(&part.file, false);
            }
        }
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
        | Effect::Resolve { .. }
        | Effect::ReadStdin { .. }
        | Effect::WriteOut { .. }
        | Effect::Now { .. }
        | Effect::Random { .. }
        | Effect::Roots { .. }
        // The folders above the root are not under it: they have no name of the root's.
        | Effect::Ancestors { .. }
        | Effect::Above { .. } => {}
    }
}

/// Records in `changed` what `effect`, numbered `stamp`, does to the files it names: a file
/// whose bytes it changes or that it makes is stamped; one it removes or renames away loses
/// its entry.
fn change(changed: &mut BTreeMap<FileName, u64>, effect: &Effect, stamp: u64) {
    match effect {
        Effect::Append { file, .. }
        | Effect::Truncate { file, .. }
        | Effect::MapWrite { file, .. }
        | Effect::Write { file, .. }
        | Effect::WriteUnsynced { file, .. }
        | Effect::WriteAt { file, .. }
        | Effect::WriteFrom { file, .. } => {
            // clone: the table's key, a refcount.
            changed.insert(file.clone(), stamp);
        }
        Effect::Remove { file, .. } => {
            changed.remove(file);
        }
        // A rename changes the names whether or not it syncs them.
        Effect::Rename { from, to, .. } | Effect::RenameUnsynced { from, to, .. } => {
            changed.remove(from);
            // clone: the table's key, a refcount.
            changed.insert(to.clone(), stamp);
        }
        Effect::Clone { to, .. } | Effect::CopyData { to, .. } | Effect::Link { to, .. } => {
            // clone: the table's key, a refcount.
            changed.insert(to.clone(), stamp);
        }
        // Everything else leaves every file's bytes and names as they were: a barrier orders
        // what was written and a prewarm asks the page cache to read ahead, as a sync and a
        // write-back change no byte.
        Effect::ReadAll { .. } | Effect::Read { .. } | Effect::ReadInto { .. } | Effect::ReadMany { .. } | Effect::ReadLink { .. } | Effect::Sync { .. } | Effect::Barrier { .. } | Effect::Prewarm { .. } | Effect::WriteBack { .. } | Effect::SyncDir { .. } | Effect::List { .. } | Effect::Mode { .. } | Effect::Seal { .. } | Effect::Private { .. } | Effect::Times { .. } | Effect::Stat { .. } | Effect::Lstat { .. } | Effect::Lock { .. } | Effect::Unlock { .. } | Effect::Watch { .. } | Effect::Journal { .. } | Effect::Unwatch { .. } | Effect::Stamp { .. } | Effect::Entries { .. } | Effect::Flush { .. } | Effect::Timer { .. } | Effect::Map { .. } | Effect::Spawn { .. } | Effect::Kill { .. } | Effect::Listen { .. } | Effect::Unlisten { .. } | Effect::Connect { .. } | Effect::Recv { .. } | Effect::Send { .. } | Effect::Hangup { .. } | Effect::Pressure { .. } | Effect::Bind { .. } | Effect::SendTo { .. } | Effect::RecvFrom { .. } | Effect::Unbind { .. } | Effect::Resolve { .. } | Effect::ReadStdin { .. } | Effect::WriteOut { .. } | Effect::Now { .. } | Effect::Random { .. } | Effect::Roots { .. } | Effect::MakeDir { .. } | Effect::RemoveDir { .. } | Effect::Ancestors { .. } | Effect::Above { .. } => {}
    }
}

impl fmt::Debug for Io {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut world = self.world().lock();
        let kind = match world.driver.sim() {
            Some(_) => "simulated",
            None => "native",
        };
        write!(f, "Io({kind}, at {:?})", world.driver.now())
    }
}
