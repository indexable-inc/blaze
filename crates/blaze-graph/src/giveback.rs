//! What the daemon gives back of its core, and in which order: a sans-IO core on the core's
//! count, the sources and the time it is told (docs/v2/plan/design/end-state/memory.md 4.1, 5
//! and 9.3).
//!
//! What is left of the give-back ladder under the gate. The memory gate (`blaze_mem::gate`)
//! bounds every commit: a task's frame and heap reserve from the Budget, and a task that does
//! not fit is wounded by age or refused by name (`blaze_mem::budget::TooBig`), so no running
//! task, no store pool and no forecast is anything this module decides on any more. The one
//! owner the gate cannot take memory back from is the **core**: a fixed size at start, which
//! holds the query engine's heap (mimalloc's arena commits pass `Gate::core_reserve`), and in
//! it every query memo, every kept front end, codegen's kept walks and the flight reader's
//! names. Until memos are records (memory.md 4.4; 9.3: "release and give-back cannot be
//! deleted while 28 GiB of query memos have no other way out"), a core that fills refuses its
//! next commit, a typed error naming the region's tag, and the only way to keep it from that
//! is to let go of heap values. This is that rule, and nothing else:
//!
//! 1. Nothing is given while what core holds is under its line ([`line`]): the core's size
//!    less an eighth of it.
//! 2. One rung a step, the lowest that has anything to give ([`Rung`]); within a rung the
//!    least remake time a byte first, and only until the gives cover what is past the line and
//!    a margin ([`target`]). The allocator's freed pages are the exception: all of them go,
//!    since they cost nothing to make again.
//! 3. After a rung's gives the planner waits until they are done ([`Planner::done`]; for the
//!    allocator, until a purge asked after them is served) before it plans again, at most
//!    [`MEASURE_WITHIN_NS`] (bounded-progress). The core's count is exact (every commit is
//!    counted as it is made), so a give done is a give measured: no sampled footprint stands
//!    between the give and the count. What a value or a front end frees stays committed in the
//!    allocator's heaps until a purge, so the allocator's rung comes round again whenever
//!    what it keeps grew by what is past the line.
//! 4. A front end marked the floor (the checkout built last: what keeps the edit loop alive)
//!    is never given whole. Past the line with only floors left, the planner says so by name
//!    ([`Outcome::FloorOver`]) and gives nothing; past the core's size the next commit is
//!    refused by the gate, named, which is the typed end (no-fallback).
//! 5. Hysteresis: an owner that was given and made again is not given again until
//!    [`hold_ns`] after its remake, a time that grows with what the remake cost. A give of an
//!    owner given and remade before, after that hold, is a thrash: counted ([`Counts::thrash`])
//!    and marked on the give ([`Give::again`]).
//!
//! Deleted with the gate (2026-11-13): the store pool's rung (there is no page pool; the
//! kernel's page cache is the one cache), the running tasks' rung (the gate wounds a task by
//! age; nothing stops a build's readers for memory), the asks of tasks held back for memory
//! (a task waits in the Budget, not on the core) and the proportion rule that held a valued
//! give to the size of such an ask, the footprint reading as the input, and the idle budget
//! (`serve::idle`: an idle daemon exits rather than give back what the next edit needs).

#![blaze::sans_io]

/// What can be given back of the core, cheapest to make again first. The order is the order
/// of giving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Rung {
    /// Pages the allocators freed and still hold committed: a purge returns them, and nothing
    /// is made again.
    Allocator = 0,
    /// Caches made again in milliseconds (the walks codegen keeps, the flight reader's past
    /// names).
    Caches = 1,
    /// Values of a front end the engine makes again from their memos: MIR, bodies' inference,
    /// constant values, solver caches. Of a kept front end, swept on a worker; of a build
    /// under way, its release made due.
    Values = 2,
    /// A checkout's whole kept front end: its next build lowers anew, which takes minutes.
    FrontEnd = 3,
}

impl Rung {
    pub const ALL: [Rung; 4] = [Rung::Allocator, Rung::Caches, Rung::Values, Rung::FrontEnd];

    pub const fn index(self) -> usize {
        self as usize
    }

    /// The name spans, events and `blaze explain memory` say it by.
    pub const fn name(self) -> &'static str {
        match self {
            Rung::Allocator => "allocator freed pages",
            Rung::Caches => "caches",
            Rung::Values => "front end values",
            Rung::FrontEnd => "front end",
        }
    }

    /// Whether what it gives is of value: time to make again that a user waits for.
    pub const fn valued(self) -> bool {
        matches!(self, Rung::Values | Rung::FrontEnd)
    }

    /// The least time between two asks of one source of this rung that an ask leaves as it
    /// is. The cheap rungs' `gives` are estimates that may never fall (what the allocator
    /// keeps holds code and stacks too), so an ask that gave nothing is not repeated at every
    /// step; one that grew since by what is past the line is asked at once all the same
    /// ([`Planner::step`]). A valued source's `gives` is what its owner counts, and falls as
    /// it gives.
    const fn retry_ns(self) -> u64 {
        match self {
            Rung::Allocator | Rung::Caches => RETRY_NS,
            Rung::Values | Rung::FrontEnd => 0,
        }
    }
}

/// How long a cheap source that gave nothing is not asked again: 1 s, four times the purge's
/// own wait for the workers' heaps.
pub const RETRY_NS: u64 = 1_000_000_000;

/// The line is the core's size less this part of it: an eighth, so the give-back has an
/// eighth of the core to work in before a commit is refused.
pub const LINE_PART: u64 = 8;

/// The least margin given past what is over the line: 64 MiB, so a core just over is not just
/// over again at the next commit.
pub const MARGIN_MIN: u64 = 64 << 20;

/// The longest the planner waits for a rung's gives to be done before it plans on: 2 s. A
/// purge waits a quarter of a second for the workers' heaps and walks every heap; a give that
/// is not done by then is counted ([`Counts::unmeasured`]).
pub const MEASURE_WITHIN_NS: u64 = 2_000_000_000;

/// The least time an owner that was given and made again is held before it may be given again:
/// 10 s.
pub const HOLD_MIN_NS: u64 = 10_000_000_000;

/// How many times its remake's time an owner made again is held: 16. A front end that took
/// 195 s to lower is held 52 minutes.
pub const HOLD_FACTOR: u64 = 16;

/// The core's give-back line for a core of `core` bytes: `core` less [`LINE_PART`] of it.
/// `u64::MAX` (no core size) has no line.
pub const fn line(core: u64) -> u64 {
    match core {
        u64::MAX => u64::MAX,
        core => core - core / LINE_PART,
    }
}

/// What is given for `need` bytes over the line: `need` and a margin of an eighth of it, at
/// least [`MARGIN_MIN`].
pub const fn target(need: u64) -> u64 {
    let eighth = need / 8;
    need.saturating_add(if eighth > MARGIN_MIN { eighth } else { MARGIN_MIN })
}

/// How long an owner whose remake took `remake_ns` is held after it: [`HOLD_FACTOR`] times
/// the remake, at least [`HOLD_MIN_NS`].
pub const fn hold_ns(remake_ns: u64) -> u64 {
    let held = remake_ns.saturating_mul(HOLD_FACTOR);
    if held > HOLD_MIN_NS { held } else { HOLD_MIN_NS }
}

/// One thing an owner can give now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub rung: Rung,
    /// Who holds it, as the driver names it: a checkout's number for a front end and its
    /// values, the driver's own numbers for a cache or the builds under way.
    pub owner: u32,
    /// The bytes it can give now.
    pub gives: u64,
    /// The step it gives in: 0 or 1 for any amount, `gives` or more for all or nothing.
    pub grain: u64,
    /// What making all of `gives` again costs, as its spans last said; 0 where nothing is
    /// made again or it is not known.
    pub remake_ns: u64,
    /// The floor that keeps the edit loop alive: never given whole (the module's rule 4).
    /// Read of [`Rung::FrontEnd`] sources alone.
    pub floor: bool,
}

/// What the planner decides on, read at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Now {
    pub at_ns: u64,
    /// The core's size: what core regions may commit (`blaze_mem::gate`); `u64::MAX` for
    /// none.
    pub core: u64,
    /// What core regions hold committed now: exact, the gate's count.
    pub used: u64,
    /// The purges served so far (the purger's tickets done).
    pub purged: u64,
}

impl Now {
    /// The line ([`line`]).
    pub const fn line(&self) -> u64 {
        line(self.core)
    }

    /// The bytes core holds past its line.
    pub const fn over(&self) -> u64 {
        self.used.saturating_sub(self.line())
    }
}

/// The mark of a [`Give`] the planner made: no other code can make one, so nothing is given
/// back for memory (a kept front end least of all) but by the rule (guard-the-cause: the drop
/// of every front end for a 0.09 GiB wait was a driver deciding by itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Planned(());

/// One give-back the driver does now. Made by [`Planner::step`] alone ([`Planned`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Give {
    pub rung: Rung,
    pub owner: u32,
    /// The bytes to give: all of the source's for one that gives all or nothing.
    pub bytes: u64,
    /// What making them again costs: the source's remake time, scaled to `bytes`.
    pub remake_ns: u64,
    /// What core held past its line, the line and what core held, as decided on.
    pub over: u64,
    pub line: u64,
    pub used: u64,
    /// This owner was given before and made again since: a thrash (the module's rule 5).
    pub again: bool,
    /// The planner made it.
    pub by: Planned,
}

/// What a step decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Core is under its line: nothing to give.
    Enough,
    /// The gives of `rung` are not done yet: nothing more until they are.
    Measuring { rung: Rung },
    /// The gives of `rung` were added to `out`: the driver does them and says when they are
    /// done ([`Planner::done`]).
    Gave { rung: Rung },
    /// Core is `over` bytes past its line and what is left is the floor (and what hysteresis
    /// holds): `kept` bytes of front ends, the first of them `owner`'s. The driver says so to
    /// the user by name; nothing is given. Past the core's size the gate refuses the next
    /// commit, naming its tag.
    FloorOver { over: u64, line: u64, used: u64, owner: u32, kept: u64 },
    /// Core is past its line and nothing at all is left to give: what holds it is the engine's
    /// memos of the builds under way and what no rung reaches.
    Nothing { over: u64 },
}

/// What the planner did, counted: `blaze explain memory`'s rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Gives asked and their bytes, by [`Rung::index`].
    pub gives: [u64; 4],
    pub bytes: [u64; 4],
    /// Waits for gives to be done that ended at [`MEASURE_WITHIN_NS`].
    pub unmeasured: u64,
    /// Steps at which an owner made again was kept out of a plan by its hold.
    pub held: u64,
    /// Gives of an owner that was given and made again before: the named failure.
    pub thrash: u64,
    /// Steps that said [`Outcome::FloorOver`].
    pub floor_over: u64,
}

/// The gives of one rung the planner waits to see done.
#[derive(Clone, Copy, Debug)]
struct Awaiting {
    rung: Rung,
    /// The purges served when the gives were made: the allocator's are done once one more is.
    purged: u64,
    /// The driver said the gives are done ([`Planner::done`]).
    done: bool,
    since_ns: u64,
}

impl Awaiting {
    /// Whether the gives are done: for the allocator's, a purge served after them.
    const fn measured(&self, now: &Now) -> bool {
        match self.rung {
            Rung::Allocator => now.purged > self.purged,
            _ => self.done,
        }
    }
}

/// What the planner remembers of one source it asked.
#[derive(Clone, Copy, Debug)]
struct Asked {
    rung: Rung,
    owner: u32,
    /// When it was last asked to give, and whether it ever was.
    at_ns: u64,
    ever: bool,
    /// What it could give as it was last asked, and what it could still give at the first
    /// step after that ask was done: one that gave nothing (`left` no less than `at_ask`) says
    /// an estimate that does not fall.
    at_ask: u64,
    left: u64,
    /// Its last ask is not done yet: `left` is still to be read.
    unread: bool,
    /// Whether it was asked since it was last made again (or ever, if never remade).
    given: bool,
    /// Whether it was given and made again since it was last given: its next give is a
    /// thrash.
    remade: bool,
    /// Until when its remake holds it.
    hold_until_ns: u64,
}

/// A planted bug, for the simulation's controls (`giveback/tests.rs`): each breaks one rule of
/// the module's list, and the oracle must catch it.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// The rule before the ladder, the control: past the line, every front end goes, whole,
    /// at once.
    WholeFrontEnd,
    /// The next rung is asked before the last one's gives are done.
    NoMeasure,
    /// Every source of a rung is taken, not only what covers the target.
    TakeAll,
    /// An owner made again is given again at once.
    NoHold,
    /// The floor is given like any front end.
    NoFloor,
}

/// The core's give-back rule (the module's doc). One a daemon.
#[derive(Debug)]
pub struct Planner {
    awaiting: Option<Awaiting>,
    /// One entry a source ever asked or remade: a handful (the caches' owners and a checkout
    /// a front end), so a scan is cheap.
    asked: Vec<Asked>,
    /// Kept scratch: the candidates of the rung being planned, as indexes into the sources.
    order: Vec<usize>,
    counts: Counts,
    #[cfg(test)]
    pub(crate) plant: Plant,
}

impl Default for Planner {
    fn default() -> Self {
        Planner::new()
    }
}

impl Planner {
    pub fn new() -> Planner {
        Planner {
            awaiting: None,
            asked: Vec::new(), // alloc: one entry a source asked, a handful a daemon
            order: Vec::new(), // alloc: kept scratch, one entry a source of a rung
            counts: Counts::default(),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The rung whose gives are not done yet, if any.
    pub fn measuring(&self) -> Option<Rung> {
        self.awaiting.map(|it| it.rung)
    }

    fn asked_at(&self, rung: Rung, owner: u32) -> Option<usize> {
        self.asked.iter().position(|it| it.rung == rung && it.owner == owner)
    }

    fn asked_mut(&mut self, rung: Rung, owner: u32) -> &mut Asked {
        let at = match self.asked_at(rung, owner) {
            Some(at) => at,
            None => {
                // alloc: none past the sources a daemon ever asked
                self.asked.push(Asked {
                    rung,
                    owner,
                    at_ns: 0,
                    ever: false,
                    at_ask: 0,
                    left: 0,
                    unread: false,
                    given: false,
                    remade: false,
                    hold_until_ns: 0,
                });
                self.asked.len() - 1
            }
        };
        &mut self.asked[at]
    }

    /// What `owner` gave of `rung` was made again by `at_ns`, which took `remake_ns`: it is
    /// held for [`hold_ns`] of that. Told for a source never given too (a checkout's first
    /// build), which holds it the same and counts no cycle.
    pub fn remade(&mut self, rung: Rung, owner: u32, at_ns: u64, remake_ns: u64) {
        let it = self.asked_mut(rung, owner);
        if it.given {
            it.remade = true;
            it.given = false;
        }
        it.hold_until_ns = at_ns.saturating_add(hold_ns(remake_ns));
    }

    /// Whether `owner`'s `rung` is held by its remake at `at_ns`.
    pub fn held(&self, rung: Rung, owner: u32, at_ns: u64) -> bool {
        self.asked_at(rung, owner).is_some_and(|at| at_ns < self.asked[at].hold_until_ns)
    }

    /// Whether a step at `now` would plan: core is past its line, and no give is still out
    /// within its wait. A driver whose sources cost something to list lists them only then; a
    /// step with none listed is right otherwise (it says [`Outcome::Enough`] or
    /// [`Outcome::Measuring`]).
    pub fn due(&self, now: &Now) -> bool {
        if now.over() == 0 {
            return false;
        }
        match self.awaiting {
            Some(awaiting) => {
                awaiting.measured(now)
                    || now.at_ns >= awaiting.since_ns.saturating_add(MEASURE_WITHIN_NS)
            }
            None => true,
        }
    }

    /// The gives of the rung last given are done (synchronous ones as the driver did them, a
    /// give on a worker once it is back): the next step reads what their sources have left.
    /// For the allocator's rung, a served purge says it instead.
    pub fn done(&mut self) {
        if let Some(awaiting) = &mut self.awaiting {
            awaiting.done = true;
        }
    }

    /// What each source whose last ask is not read yet can still give, now that its give is
    /// done (or the wait for it ended): one no longer listed has nothing left.
    fn read_left(&mut self, sources: &[Source]) {
        for seen in self.asked.iter_mut().filter(|it| it.unread) {
            seen.unread = false;
            seen.left = sources
                .iter()
                .find(|it| it.rung == seen.rung && it.owner == seen.owner)
                .map_or(0, |it| it.gives);
        }
    }

    /// One decision (the module's rules): the gives of at most one rung are added to `out`,
    /// which the driver does before it says them done and steps again. `sources` is what
    /// every owner can give now, in any order.
    pub fn step(&mut self, now: &Now, sources: &[Source], out: &mut Vec<Give>) -> Outcome {
        let need = now.over();
        if need == 0 {
            // A give still out stays so: what its sources have left is read only once it is
            // done.
            if self.awaiting.is_some_and(|it| it.measured(now)) {
                self.awaiting = None;
                self.read_left(sources);
            }
            return Outcome::Enough;
        }
        #[cfg(test)]
        if self.plant == Plant::WholeFrontEnd {
            return self.whole_front_ends(now, sources, out);
        }
        if let Some(awaiting) = self.awaiting {
            let measured = awaiting.measured(now);
            let late = now.at_ns >= awaiting.since_ns.saturating_add(MEASURE_WITHIN_NS);
            #[cfg(test)]
            let measured = measured || self.plant == Plant::NoMeasure;
            if !measured && !late {
                return Outcome::Measuring { rung: awaiting.rung };
            }
            if !measured {
                self.counts.unmeasured += 1;
                // What was asked and never done is not asked again at once: its retry counts
                // from now, so the next rung has its turn (bounded-progress).
                for seen in self.asked.iter_mut().filter(|it| it.unread) {
                    seen.at_ns = now.at_ns;
                }
            }
            self.awaiting = None;
            self.read_left(sources);
        }
        #[cfg(test)]
        let (no_floor, no_hold, take_all) = (
            self.plant == Plant::NoFloor,
            self.plant == Plant::NoHold,
            self.plant == Plant::TakeAll,
        );
        #[cfg(not(test))]
        let (no_floor, no_hold, take_all) = (false, false, false);
        let target = target(need);
        let mut order = std::mem::take(&mut self.order);
        let mut gave = None;
        for rung in Rung::ALL {
            order.clear();
            for (at, source) in sources.iter().enumerate() {
                if source.rung != rung || source.gives == 0 {
                    continue;
                }
                if rung == Rung::FrontEnd && source.floor && !no_floor {
                    continue;
                }
                if let Some(seen) = self.asked_at(rung, source.owner) {
                    let seen = self.asked[seen];
                    // Asked lately, and what it shows is what an ask leaves (it gives all or
                    // nothing, or its last ask gave nothing), and it has not grown since by
                    // what is past the line: asking again now would give nothing. A source
                    // that grew by the need is asked at once, before any dearer rung: a purge
                    // that alone covers what is over always comes before a value is dropped.
                    let leaves = rung == Rung::Allocator
                        || source.grain >= source.gives
                        || seen.left >= seen.at_ask;
                    let grew = source.gives >= seen.left.saturating_add(need);
                    let lately = now.at_ns < seen.at_ns.saturating_add(rung.retry_ns());
                    if seen.ever && leaves && !grew && lately {
                        continue;
                    }
                    if now.at_ns < seen.hold_until_ns && !no_hold {
                        self.counts.held += 1;
                        continue;
                    }
                }
                order.push(at); // alloc: none past the most sources of a rung, the scratch kept
            }
            // The least remake time a byte first (a.remake / a.gives < b.remake / b.gives,
            // cross-multiplied), then the owner, so the order hangs on nothing else.
            order.sort_unstable_by(|&a, &b| {
                let (a, b) = (&sources[a], &sources[b]);
                let left = u128::from(a.remake_ns) * u128::from(b.gives);
                let right = u128::from(b.remake_ns) * u128::from(a.gives);
                left.cmp(&right).then(a.owner.cmp(&b.owner))
            });
            let mut sum = 0u64;
            for &at in &order {
                let source = sources[at];
                let all = rung == Rung::Allocator || take_all;
                if !all && sum >= target {
                    break;
                }
                let bytes = if all { source.gives } else { amount(&source, target - sum) };
                sum = sum.saturating_add(bytes);
                let seen = self.asked_mut(rung, source.owner);
                let again = seen.remade;
                seen.at_ns = now.at_ns;
                seen.ever = true;
                seen.at_ask = source.gives;
                seen.unread = true;
                seen.given = true;
                seen.remade = false;
                if again {
                    self.counts.thrash += 1;
                }
                self.counts.gives[rung.index()] += 1;
                self.counts.bytes[rung.index()] += bytes;
                // alloc: none past the most gives of a step, the caller's buffer kept
                out.push(Give {
                    rung,
                    owner: source.owner,
                    bytes,
                    remake_ns: scaled(source.remake_ns, bytes, source.gives),
                    over: need,
                    line: now.line(),
                    used: now.used,
                    again,
                    by: Planned(()),
                });
            }
            if sum > 0 {
                gave = Some(rung);
                break;
            }
        }
        self.order = order;
        if let Some(rung) = gave {
            self.awaiting =
                Some(Awaiting { rung, purged: now.purged, done: false, since_ns: now.at_ns });
            return Outcome::Gave { rung };
        }
        // Past the line with nothing left to give but what is never given: the floor, and what
        // a remake holds.
        let mut kept = 0u64;
        let mut owner = None;
        for source in sources {
            if source.rung == Rung::FrontEnd && source.gives > 0 {
                kept = kept.saturating_add(source.gives);
                if owner.is_none() || source.floor {
                    owner = Some(source.owner);
                }
            }
        }
        match owner {
            Some(owner) => {
                self.counts.floor_over += 1;
                Outcome::FloorOver { over: need, line: now.line(), used: now.used, owner, kept }
            }
            None => Outcome::Nothing { over: need },
        }
    }

    /// [`Plant::WholeFrontEnd`]: the rule before the ladder, kept as the control. Every front
    /// end goes, whole, past the line, with no purge first and nothing awaited between.
    #[cfg(test)]
    fn whole_front_ends(&mut self, now: &Now, sources: &[Source], out: &mut Vec<Give>) -> Outcome {
        let before = out.len();
        for source in sources {
            if source.rung == Rung::FrontEnd && source.gives > 0 {
                out.push(Give {
                    rung: Rung::FrontEnd,
                    owner: source.owner,
                    bytes: source.gives,
                    remake_ns: source.remake_ns,
                    over: now.over(),
                    line: now.line(),
                    used: now.used,
                    again: false,
                    by: Planned(()),
                });
            }
        }
        match out.len() > before {
            true => Outcome::Gave { rung: Rung::FrontEnd },
            false => Outcome::Nothing { over: now.over() },
        }
    }
}

/// What `source` gives toward `short` bytes still to cover: `short` rounded up to its grain,
/// at most all of it; all of it for one that gives all or nothing.
fn amount(source: &Source, short: u64) -> u64 {
    if source.grain >= source.gives {
        return source.gives;
    }
    let grain = source.grain.max(1);
    short.div_ceil(grain).saturating_mul(grain).min(source.gives)
}

/// `remake_ns`, the cost of all `of` bytes, scaled to `bytes` of them.
fn scaled(remake_ns: u64, bytes: u64, of: u64) -> u64 {
    if of == 0 {
        return 0;
    }
    let scaled = u128::from(remake_ns) * u128::from(bytes) / u128::from(of);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// The note a user reads when a front end is let go (the terminal's `Why` reasons carry it):
/// one line, the checkout by name.
pub fn dropped_line(project: &str, give: &Give) -> String {
    let gb = |bytes: u64| bytes as f64 / 1e9;
    // alloc: a note, once a front end let go
    format!(
        "analysis of {project} was dropped to free {:.1} GB (blaze's heap held {:.1} GB, past its line of {:.1} GB); the next build redoes it",
        gb(give.bytes),
        gb(give.used),
        gb(give.line),
    )
}

/// The note a user reads when the floor does not fit under the core's line
/// ([`Outcome::FloorOver`]): one line, the checkout by name.
pub fn floor_line(project: &str, over: u64, line: u64, used: u64, kept: u64) -> String {
    let gb = |bytes: u64| bytes as f64 / 1e9;
    // alloc: a note, once a stretch past the line
    format!(
        "memory: blaze's heap holds {:.1} GB, {:.1} GB past its line of {:.1} GB, and what is left to give is the analysis of {project} ({:.1} GB), which the next edit needs: it is kept; a build that needs more fails, naming what it could not hold; raise the memory ceiling",
        gb(used),
        gb(over),
        gb(line),
        gb(kept),
    )
}

#[cfg(test)]
mod tests;
