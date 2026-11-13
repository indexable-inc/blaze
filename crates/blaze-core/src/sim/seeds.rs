//! A program run on the simulation under many seeds, its faults on unless it leaves one out by
//! name: the one runner of a deterministic simulation test whose every choice is its seed's
//! (docs/v2/plan/design/bstd-sim-test.md). `#[bstd::test]` is this runner over a bstd program;
//! a sans-IO core driven on [`Sim`] directly runs under it the same way (`seeds/tests.rs`).
//!
//! - [`Seeds`] says which seeds a test runs and which [`Fault`] kinds it leaves out;
//!   [`Seeds::DEFAULT`] is 256 seeds from 0 with every kind on.
//! - [`Plan::of`] is what one seed injects, drawn from the seed as a swarm: each kind is off on
//!   some seeds, so a rare one is not drowned by a common one. Leaving a kind out zeroes it and
//!   changes no other draw of the seed.
//! - [`Sim::arm`] arms a simulation with a plan. An armed simulation counts the effects it is
//!   handed, draws the power cuts, and writes the trace: one line an effect, a completion, a
//!   cut and a start. [`Sim::cut_due`] tells the driver's user that power is lost now: it
//!   crashes its world ([`Sim::crash`]) and the machine is off, taking no effect and telling no
//!   completion, until [`Sim::power_on`]. What still runs between the two reaches nothing.
//! - [`one`] runs one seed: the program is started, started again after each power cut and
//!   each error it gives up with (at most [`Seeds::restarts_most`] times), and failed by a
//!   panic. When it ends well on a seed that cuts power, power is cut once more at its end,
//!   the faults heal and it runs a last time: its success has to survive a crash. With the
//!   faults healed it has to end well: an error then is what an earlier fault left behind.
//! - [`run`] runs every seed. The first that breaks is run a second time and the two traces
//!   compared, byte for byte (a second run that is the check, said by name either way), and
//!   reported with its plan, the end of its trace and the one line that replays it. A run in
//!   which no seed's program asked the simulation for anything fails, by name: it tested
//!   nothing here.
//! - [`Given`] is the seed and the count the person running the test gave from outside
//!   ([`SEED_VAR`], [`SEEDS_VAR`]): a given seed runs alone, and is the same run.
//!
//! Nothing here reads the clock, a thread or a table's order: a run is a function of its seed
//! and of the program (rule deterministic-output).

#[cfg(test)]
mod tests;

use std::{
    borrow::Cow,
    fmt::{self, Write as _},
    hash::Hasher as _,
    time::Duration,
};

use blaze_hash::FixedHasher;

use super::{Rng, Sim};
use crate::{Completion, Effect};

/// The variable a seed is given in: `BLAZE_SIM_SEED=41` runs seed 41 alone (decimal, or hex
/// after `0x`).
pub const SEED_VAR: &str = "BLAZE_SIM_SEED";

/// The variable a count of seeds is given in: `BLAZE_SIM_SEEDS=10000` runs that many from the
/// test's first, for a long run.
pub const SEEDS_VAR: &str = "BLAZE_SIM_SEEDS";

/// What a plan's draws start from, besides the seed: so they are other numbers than the
/// simulation's own, which start from the seed itself.
const PLAN_SALT: u64 = 0x7365_6564_732e_706c;

/// What an armed simulation's power cuts are drawn from, besides the seed.
const CUT_SALT: u64 = 0x7365_6564_732e_6375;

/// The most bytes of a trace line after its time; what is past them is stood for by its
/// length and a hash, so a megabyte written is a line and not a megabyte.
const LINE_MOST: usize = 240;

/// How many of a trace's last lines a failure prints.
const TAIL_LINES: usize = 24;

/// A kind of fault the default run injects. A test leaves one out by name
/// ([`Seeds::without`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// An effect fails, once in a number of the seed's ([`Plan::fail_one_in`]): a failed append
    /// writes a part of its bytes, a failed sync makes nothing durable, a failed write leaves
    /// the file as it was.
    IoFailures,
    /// Completions come late, up to a delay of the seed's ([`Plan::max_latency`]), and so in
    /// another order than they were asked in.
    Delays,
    /// The machine loses power at effects of the seed's ([`Plan::crash_one_in`]) and once more
    /// when the program has ended well: what was not synced is lost or torn, and the program
    /// starts again.
    Crashes,
}

impl Fault {
    /// Every kind.
    pub const ALL: [Fault; 3] = [Fault::IoFailures, Fault::Delays, Fault::Crashes];

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Which seeds a test runs, and which faults it leaves out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seeds {
    /// The first seed.
    pub first: u64,
    /// How many seeds, from the first.
    pub count: u32,
    /// The most times one seed's program is started again, after a power cut or an error it
    /// gave up with, before its faults are healed: the bound of a seed's run.
    pub restarts_most: u32,
    /// The kinds left out, a bit each.
    without: u8,
}

impl Seeds {
    /// 256 seeds from 0, six restarts, every kind of fault on.
    pub const DEFAULT: Seeds = Seeds { first: 0, count: 256, restarts_most: 6, without: 0 };

    /// The same seeds with `fault` never injected.
    #[must_use]
    pub const fn without(mut self, fault: Fault) -> Seeds {
        self.without |= fault.bit();
        self
    }

    /// `count` seeds from `first`.
    #[must_use]
    pub const fn from(mut self, first: u64, count: u32) -> Seeds {
        self.first = first;
        self.count = count;
        self
    }

    /// Whether these seeds may inject `fault`.
    pub const fn injects(&self, fault: Fault) -> bool {
        self.without & fault.bit() == 0
    }
}

/// The seed and the count the person running a test gave, outside the test's own code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Given {
    /// Run this seed alone.
    pub seed: Option<u64>,
    /// Run this many seeds from the test's first.
    pub count: Option<u32>,
}

impl Given {
    /// Nothing given: the test's own seeds.
    pub const NOTHING: Given = Given { seed: None, count: None };

    /// What [`SEED_VAR`] and [`SEEDS_VAR`] hold, typed. A value that is no number is refused,
    /// naming the variable: a seed misread would be another run that passes.
    pub fn parse(seed: Option<&str>, count: Option<&str>) -> Result<Given, String> {
        let seed = match seed {
            None => None,
            Some(raw) => {
                let parsed = match raw.strip_prefix("0x") {
                    Some(hex) => u64::from_str_radix(hex, 16),
                    None => raw.parse(),
                };
                // alloc: an error's message.
                Some(parsed.map_err(|error| format!("{SEED_VAR}={raw:?} is no seed: {error}"))?)
            }
        };
        let count = match count {
            None => None,
            Some(raw) => match raw.parse::<u32>() {
                Ok(0) | Err(_) => {
                    // alloc: an error's message.
                    return Err(format!("{SEEDS_VAR}={raw:?} is no count of seeds (1 or more)"));
                }
                Ok(count) => Some(count),
            },
        };
        Ok(Given { seed, count })
    }

    /// What the process's environment gives: the read a test's entry does once, before
    /// [`run`]. A variable that is not set gives nothing; one that is not text, or no number,
    /// is refused.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's entry reads its two settings here, once and typed: the test harness \
                  hands a test nothing else"
    )]
    pub fn from_env() -> Result<Given, String> {
        let read = |name: &str| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            // alloc: an error's message.
            Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is set and is not text")),
        };
        let (seed, count) = (read(SEED_VAR)?, read(SEEDS_VAR)?);
        Given::parse(seed.as_deref(), count.as_deref())
    }
}

/// What one seed injects.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Plan {
    pub seed: u64,
    /// An effect fails once in this many; 0: this seed fails none.
    pub fail_one_in: u32,
    /// Each completion is ready after a delay drawn up to this.
    pub max_latency: Duration,
    /// Power is lost at an effect once in this many; 0: this seed cuts none.
    pub crash_one_in: u32,
    /// The most power cuts drawn at effects. The cut at the program's end is one more.
    pub cuts_most: u32,
}

impl Plan {
    /// The plan of `seed` under `seeds`. Every kind is drawn, in one order, and a kind the
    /// seeds leave out is zeroed after: leaving one out changes nothing else of a seed.
    pub fn of(seed: u64, seeds: &Seeds) -> Plan {
        let mut rng = Rng::new(seed ^ PLAN_SALT);
        let fail_one_in = [0, 0, 4, 16, 64][rng.index(5)];
        let latency_us = [0, 50, 200, 2_000, 50_000][rng.index(5)];
        let crash_one_in = [0, 8, 32, 128, 512][rng.index(5)];
        let cuts_most = 1 + rng.below(3) as u32;
        let on = |fault: Fault, drawn: u32| if seeds.injects(fault) { drawn } else { 0 };
        Plan {
            seed,
            fail_one_in: on(Fault::IoFailures, fail_one_in),
            max_latency: Duration::from_micros(u64::from(on(Fault::Delays, latency_us))),
            crash_one_in: on(Fault::Crashes, crash_one_in),
            cuts_most,
        }
    }
}

/// What an armed simulation did so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Effects it was handed while it was on.
    pub effects: u64,
    /// Effects handed to it while it was off, and dropped.
    pub dropped: u64,
    /// Failures it injected.
    pub failures: u64,
    /// Times its power was cut: at an effect, and by its world's own crashes.
    pub cuts: u64,
}

/// The trace: text, a line an event, each starting with the virtual clock in nanoseconds.
#[derive(Default)]
struct Trace {
    // alloc: the trace's text, grown a line an event of an armed simulation.
    text: String,
}

/// A line's text, kept up to [`LINE_MOST`] bytes: what is past them is counted and hashed.
struct Capped<'a> {
    out: &'a mut String,
    room: usize,
    more: usize,
    hash: FixedHasher,
}

impl fmt::Write for Capped<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut keep = self.room.min(text.len());
        while !text.is_char_boundary(keep) {
            keep -= 1;
        }
        self.out.push_str(&text[..keep]);
        // A character that did not fit whole ends the line's kept part.
        self.room = if keep < text.len() { 0 } else { self.room - keep };
        self.more += text.len() - keep;
        self.hash.write(&text.as_bytes()[keep..]);
        Ok(())
    }
}

impl Trace {
    fn line(&mut self, now: Duration, what: fmt::Arguments<'_>) {
        // Writing into a `String` does not fail, and no `Debug` here does.
        _ = write!(self.text, "{:>14} ", now.as_nanos());
        let mut line =
            Capped { out: &mut self.text, room: LINE_MOST, more: 0, hash: FixedHasher::default() };
        _ = line.write_fmt(what);
        let (more, hash) = (line.more, line.hash.finish());
        if more > 0 {
            _ = write!(self.text, " ..{more} more bytes, hash {hash:016x}");
        }
        self.text.push('\n');
    }
}

/// What an armed simulation keeps beside the machine ([`Sim::arm`]).
pub(super) struct Armed {
    crash_one_in: u32,
    /// The generator the power cuts are drawn from: its own, so arming changes none of the
    /// simulation's draws.
    rng: Rng,
    /// Cuts at effects the plan still allows.
    cuts_left: u32,
    /// A cut was drawn and [`Sim::cut_due`] has not been asked since.
    due: bool,
    /// [`Sim::cut_due`] said yes: the next [`Sim::crash`] is that cut, and turns the machine
    /// off.
    cutting: bool,
    /// The machine is off: it takes no effect and tells no completion.
    off: bool,
    trace: Trace,
    counts: Counts,
}

/// `effects` handed to `sim`, before it takes them: traced, and a power cut drawn at each.
/// `true` when the machine is off, and the effects were dropped.
pub(super) fn submitted(sim: &mut Sim, effects: &mut Vec<Effect>) -> bool {
    let now = sim.now;
    let Some(armed) = sim.armed.as_deref_mut() else { return false };
    if armed.off {
        for effect in effects.drain(..) {
            armed.counts.dropped += 1;
            armed.trace.line(now, format_args!("off, dropped {effect:?}"));
        }
        return true;
    }
    for effect in effects.iter() {
        armed.counts.effects += 1;
        armed.trace.line(now, format_args!("> {effect:?}"));
        if !armed.due && armed.cuts_left > 0 && armed.rng.one_in(u64::from(armed.crash_one_in)) {
            armed.cuts_left -= 1;
            armed.due = true;
        }
    }
    false
}

/// `completions`, which a poll of `sim` just told: traced.
pub(super) fn polled(sim: &mut Sim, completions: &[Completion]) {
    let now = sim.now;
    let Some(armed) = sim.armed.as_deref_mut() else { return };
    for Completion { tag, done } in completions {
        armed.trace.line(now, format_args!("< {} {done:?}", tag.0));
    }
}

/// Whether `sim` is armed and off.
pub(super) fn off(sim: &Sim) -> bool {
    sim.armed.as_deref().is_some_and(|armed| armed.off)
}

/// `sim` is losing power ([`Sim::crash`], before it loses anything): traced, and the machine
/// turned off when it is the cut [`Sim::cut_due`] announced.
pub(super) fn crashed(sim: &mut Sim) {
    let (now, in_flight) = (sim.now, sim.pending.len());
    let Some(armed) = sim.armed.as_deref_mut() else { return };
    armed.counts.cuts += 1;
    let which = match armed.cutting {
        true => "at an effect: the machine is off",
        false => "by its world",
    };
    armed.trace.line(now, format_args!("power lost {which}, {in_flight} effects in flight"));
    armed.off |= armed.cutting;
    armed.cutting = false;
}

/// `sim` injected a failure.
pub(super) fn injected(sim: &mut Sim) {
    if let Some(armed) = sim.armed.as_deref_mut() {
        armed.counts.failures += 1;
    }
}

impl Sim {
    /// Arms the simulation with `plan`: its failure rate and latency become the plan's, and
    /// from here it counts its effects, draws the plan's power cuts and writes the trace
    /// ([`Sim::trace`]). Arming again starts a new trace and a new count.
    pub fn arm(&mut self, plan: &Plan) {
        self.faults.fail_one_in = plan.fail_one_in;
        self.faults.max_latency = plan.max_latency;
        let mut trace = Trace::default();
        trace.line(self.now, format_args!("armed: {plan:?}"));
        // alloc: once per armed simulation.
        self.armed = Some(Box::new(Armed {
            crash_one_in: plan.crash_one_in,
            rng: Rng::new(plan.seed ^ CUT_SALT),
            cuts_left: plan.cuts_most,
            due: false,
            cutting: false,
            off: false,
            trace,
            counts: Counts::default(),
        }));
    }

    /// Whether power is lost now: asked by the driver's user after each submit. `true` once a
    /// cut: the asker then crashes its world, [`Sim::crash`] and whatever it keeps beside the
    /// simulation, which turns the machine off, and ends its program's run. Never `true` of a
    /// simulation that is not armed.
    pub fn cut_due(&mut self) -> bool {
        let Some(armed) = self.armed.as_deref_mut() else { return false };
        let due = std::mem::take(&mut armed.due);
        armed.cutting |= due;
        due
    }

    /// Whether the machine is off: its power was cut at an effect and not turned on since.
    pub fn is_off(&self) -> bool {
        off(self)
    }

    /// The machine comes back on, for its program's next start.
    pub fn power_on(&mut self) {
        let now = self.now;
        if let Some(armed) = self.armed.as_deref_mut() {
            armed.off = false;
            armed.trace.line(now, format_args!("power on"));
        }
    }

    /// The plan's faults end: no effect fails and no power is cut from here. The latency stays.
    pub fn heal(&mut self) {
        self.faults.fail_one_in = 0;
        let now = self.now;
        if let Some(armed) = self.armed.as_deref_mut() {
            armed.cuts_left = 0;
            armed.due = false;
            armed.trace.line(now, format_args!("healed: no failure and no power cut from here"));
        }
    }

    /// A line of the runner's own in the trace: a start, an end.
    pub fn note(&mut self, what: fmt::Arguments<'_>) {
        let now = self.now;
        if let Some(armed) = self.armed.as_deref_mut() {
            armed.trace.line(now, what);
        }
    }

    /// The trace since it was armed; empty when it is not.
    pub fn trace(&self) -> &str {
        self.armed.as_deref().map_or("", |armed| &armed.trace.text)
    }

    /// What it did since it was armed.
    pub fn counts(&self) -> Counts {
        self.armed.as_deref().map(|armed| armed.counts).unwrap_or_default()
    }
}

/// How one start of a program ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ended {
    /// It ran to its end and says all is well.
    Well,
    /// It gave up with an error: under a fault, a program that stops loudly is restarted.
    /// The text is owned, or fixed where its world had no room to keep the error's own.
    GaveUp(Cow<'static, str>),
    /// It panicked: an assertion of the test's, or a bug.
    Panicked(Cow<'static, str>),
    /// Its machine lost power under it ([`Sim::cut_due`]).
    PowerLost,
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ended::Well => f.write_str("ended well"),
            Ended::GaveUp(error) => write!(f, "gave up: {error}"),
            Ended::Panicked(message) => write!(f, "panicked: {message}"),
            Ended::PowerLost => f.write_str("lost power"),
        }
    }
}

/// A simulated world and the program in it: what [`one`] runs a seed on.
pub trait World {
    /// The program run once, from its start, in the world as it is, to its end. A world asks
    /// [`Sim::cut_due`] after each submit; on a cut it crashes itself and ends the run with
    /// [`Ended::PowerLost`], and nothing of the program reaches the machine after.
    fn run(&mut self) -> Ended;

    /// The machine loses power: [`Sim::crash`], and whatever the world keeps beside the
    /// simulation.
    fn crash(&mut self);

    /// `f` on the world's simulation.
    fn with_sim<R>(&mut self, f: impl FnOnce(&mut Sim) -> R) -> R;
}

/// Why a seed's run broke.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Broke {
    /// Which start of the program, from 1.
    pub start: u32,
    pub why: String,
}

/// One seed's run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Run {
    pub plan: Plan,
    pub verdict: Result<(), Broke>,
    /// How many times the program was started.
    pub starts: u32,
    pub counts: Counts,
    /// Every effect, completion, cut and start of the run, a line each.
    pub trace: String,
}

/// Runs `seed`: a world of it from `boot`, armed with its plan under `seeds`, and the program
/// started until it ends well with nothing left to survive, or breaks (the module's doc has
/// the rule). At most `restarts_most + 2` starts. A world `boot` could not make breaks the
/// seed before any start (start 0), naming the seed and what `boot` said.
pub fn one<W: World, E: fmt::Display>(
    seed: u64,
    seeds: &Seeds,
    boot: &mut impl FnMut(u64) -> Result<W, E>,
) -> Run {
    let plan = Plan::of(seed, seeds);
    let mut world = match boot(seed) {
        Ok(world) => world,
        Err(error) => {
            return Run {
                plan,
                // alloc: a failure's message.
                verdict: Err(Broke { start: 0, why: format!("the world of seed {seed} could not be made: {error}") }),
                starts: 0,
                counts: Counts::default(),
                trace: String::new(),
            };
        }
    };
    world.with_sim(|sim| sim.arm(&plan));
    // A seed that injects nothing has nothing to heal: the program's first end is its last.
    let unkind = plan.fail_one_in != 0 || plan.crash_one_in != 0;
    let (mut starts, mut restarts, mut healed) = (0, 0, !unkind);
    let heal = |world: &mut W, healed: &mut bool| {
        world.with_sim(Sim::heal);
        *healed = true;
    };
    let verdict = loop {
        starts += 1;
        world.with_sim(|sim| sim.note(format_args!("start {starts}")));
        let ended = world.run();
        let off = world.with_sim(|sim| {
            sim.note(format_args!("start {starts} {ended}"));
            sim.is_off()
        });
        // alloc: a failure's message.
        let broke = |why: String| Err(Broke { start: starts, why });
        match ended {
            Ended::Panicked(message) => break broke(format!("panicked: {message}")),
            _ if off && healed => {
                break broke(
                    "the simulation cut the power after its faults were healed: a bug of \
                     blaze_core::sim::seeds"
                        .to_owned(),
                );
            }
            // A cut it did not see end its run is a cut all the same: nothing after it landed.
            Ended::Well if off => {}
            Ended::Well if healed || plan.crash_one_in == 0 => break Ok(()),
            Ended::Well => {
                // Its success has to survive a crash: one more cut, and a last start.
                world.with_sim(|sim| sim.note(format_args!("the program ended well: power is cut at its end")));
                world.crash();
                heal(&mut world, &mut healed);
                continue;
            }
            Ended::GaveUp(error) if healed => {
                break broke(format!("with no fault left to blame, the program gave up: {error}"));
            }
            Ended::GaveUp(_) | Ended::PowerLost => {}
        }
        if off {
            world.with_sim(Sim::power_on);
        }
        restarts += 1;
        if restarts >= seeds.restarts_most {
            heal(&mut world, &mut healed);
        }
    };
    let (counts, trace) = world.with_sim(|sim| (sim.counts(), sim.trace().to_owned()));
    Run { plan, verdict, starts, counts, trace }
}

/// A test's name, for what a failure says and the line that replays it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Name<'a> {
    /// The package the test is in, as `-p` takes it.
    pub package: &'a str,
    /// The module the test is in, as `module_path!()` says it: the crate's name first.
    pub module: &'a str,
    /// The test function's name.
    pub test: &'a str,
}

impl Name<'_> {
    /// The test's path as the test harness lists it: its module's path without the crate's
    /// name, then its own.
    pub fn listed(&self) -> String {
        match self.module.split_once("::") {
            // alloc: a failure's name of the test.
            Some((_, path)) => format!("{path}::{}", self.test),
            None => self.test.to_owned(),
        }
    }
}

/// Every seed passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Passed {
    pub seeds: u32,
    /// Starts of the program, over every seed.
    pub starts: u64,
    /// What the simulations did, over every seed.
    pub counts: Counts,
}

/// What the second run of a broken seed showed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Replay {
    /// The same verdict and the same trace, byte for byte.
    Same,
    /// Another run: the program, or its world, takes a choice from outside the seed.
    Diverged {
        /// The first line that differs, from 1.
        line: usize,
        first: String,
        second: String,
    },
}

/// A test under seeds failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failed {
    /// A seed's run broke.
    Seed {
        package: String,
        test: String,
        /// Which of the seeds run it was, from 1, and of how many.
        at: u32,
        of: u32,
        // alloc: once per failure, the run it reports.
        run: Box<Run>,
        replay: Replay,
    },
    /// No seed's program asked the simulation for anything.
    Idle { package: String, test: String, seeds: u32 },
    /// What was given from outside is no seed or no count.
    Given(String),
}

impl Failed {
    /// The run that broke, when one did.
    pub fn broke(&self) -> Option<&Run> {
        match self {
            Failed::Seed { run, .. } => Some(run),
            Failed::Idle { .. } | Failed::Given(_) => None,
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failed::Given(why) => write!(f, "a test under seeds: {why}"),
            Failed::Idle { package, test, seeds } => write!(
                f,
                "`{test}` of {package} asked the simulation for nothing in any of its {seeds} \
                 seeds: it tested nothing under simulation. A test that waits for nothing is a \
                 plain `#[test]`"
            ),
            Failed::Seed { package, test, at, of, run, replay } => {
                let Run { plan, verdict, starts, counts, trace } = &**run;
                let (start, why) = match verdict {
                    Err(Broke { start, why }) => (*start, why.as_str()),
                    Ok(()) => (*starts, "it passed"),
                };
                let seed = plan.seed;
                writeln!(f, "`{test}` failed at seed {seed} (seed {at} of {of}), start {start}: {why}")?;
                writeln!(f, "plan: {plan:?}")?;
                writeln!(
                    f,
                    "effects {}, failures injected {}, power cuts {}, starts {starts}",
                    counts.effects, counts.failures, counts.cuts
                )?;
                match replay {
                    Replay::Same => writeln!(
                        f,
                        "the seed replays: its second run gave the same {} bytes of trace",
                        trace.len()
                    )?,
                    Replay::Diverged { line, first, second } => writeln!(
                        f,
                        "THE SEED DOES NOT REPLAY: its second run differs from line {line} of \
                         the trace, so the program or its world takes a choice from outside \
                         the seed (the clock, a thread, a table's order)\n  first:  {first}\n  \
                         second: {second}"
                    )?,
                }
                let lines = trace.lines().count();
                writeln!(f, "the last {} of the trace's {lines} lines:", lines.min(TAIL_LINES))?;
                for line in trace.lines().skip(lines.saturating_sub(TAIL_LINES)) {
                    writeln!(f, "  {line}")?;
                }
                write!(f, "replay: {SEED_VAR}={seed} blaze test -p {package} {test} -- --exact")
            }
        }
    }
}

impl std::error::Error for Failed {}

/// Where `first` and `second` first differ, as the lines there.
fn diverged(first: &str, second: &str) -> Replay {
    let (mut a, mut b) = (first.lines(), second.lines());
    let mut line = 1;
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Replay::Same,
            (x, y) if x == y => line += 1,
            (x, y) => {
                let said = |it: Option<&str>| it.unwrap_or("(the trace ended)").to_owned();
                return Replay::Diverged { line, first: said(x), second: said(y) };
            }
        }
    }
}

/// Runs the test `name` under `seeds`, or under what was `given` from outside in their place:
/// each seed a world from `boot` ([`one`]). The first seed that breaks is run again and
/// compared, then reported; when all pass, what they did is counted.
pub fn run<W: World, E: fmt::Display>(
    seeds: Seeds,
    given: Given,
    name: Name<'_>,
    mut boot: impl FnMut(u64) -> Result<W, E>,
) -> Result<Passed, Failed> {
    let (first, count) = match given {
        Given { seed: Some(seed), .. } => (seed, 1),
        Given { seed: None, count } => (seeds.first, count.unwrap_or(seeds.count)),
    };
    let (package, test) = (name.package, name.listed());
    let mut passed = Passed { seeds: 0, starts: 0, counts: Counts::default() };
    for at in 0..count {
        let seed = first.wrapping_add(u64::from(at));
        let run = one(seed, &seeds, &mut boot);
        if run.verdict.is_err() {
            // The second run is the check that the seed replays, said either way.
            let again = one(seed, &seeds, &mut boot);
            let replay = match again.verdict == run.verdict {
                true => diverged(&run.trace, &again.trace),
                false => diverged(
                    &format!("{}\nverdict: {:?}", run.trace, run.verdict),
                    &format!("{}\nverdict: {:?}", again.trace, again.verdict),
                ),
            };
            return Err(Failed::Seed {
                package: package.to_owned(),
                test,
                at: at + 1,
                of: count,
                // alloc: once per failure.
                run: Box::new(run),
                replay,
            });
        }
        passed.seeds += 1;
        passed.starts += u64::from(run.starts);
        passed.counts.effects += run.counts.effects;
        passed.counts.dropped += run.counts.dropped;
        passed.counts.failures += run.counts.failures;
        passed.counts.cuts += run.counts.cuts;
    }
    if passed.counts.effects == 0 {
        return Err(Failed::Idle { package: package.to_owned(), test, seeds: count });
    }
    Ok(passed)
}
