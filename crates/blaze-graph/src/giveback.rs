//! What the daemon gives back when memory is short, and in which order
//! (docs/v2/plan/design/memory.md, "Giving memory back"): a sans-IO core on the readings, the
//! sources and the time it is told.
//!
//! Until this, the daemon had one answer to a task held back for memory the process holds
//! besides its tasks (`Plane::short`): every kept front end of every checkout, whole, in the
//! loop turn that saw the wait. On 2026-11-12 a task waited for 0.09 GiB and the daemon let go
//! of reth's kept front end (about 55 GiB held); the next build, a one-line revert, took
//! 195.4 s where such an edit takes 4 to 16 s. The purge the plane had asked for in the same
//! step (`Plane::purge_due`) had not landed yet, so the wait was counted against a footprint
//! that still held what the allocator kept freed.
//!
//! [`Planner`] is the rule that replaces it. Each [`Planner::step`] is told the limit, the
//! footprint, what a task waits for ([`Now`]) and what each owner can give and what that costs
//! to make again ([`Source`]), and answers with the gives of one rung ([`Give`]), cheapest
//! rung first ([`Rung`]):
//!
//! 1. Nothing is given while the footprint is within the limit and no task waits.
//! 2. One rung a step, the lowest that has anything to give; within a rung the least remake
//!    time a byte first, and only until the gives cover what is short and a margin
//!    ([`target`]). The allocator's freed pages are the exception: all of them go, since they
//!    cost nothing to make again.
//! 3. After a rung's gives the planner waits for a reading taken after them
//!    ([`Outcome::Measuring`]) before it plans again, at most [`MEASURE_WITHIN_NS`]
//!    (bounded-progress): the next rung is asked only for what the reading still says is
//!    short.
//! 4. Evicted values and front ends are given for a predicted ask only in proportion: no give
//!    of them is more than [`FACTOR`] times the target. A real deficit (the footprint past the
//!    limit) is not held to that, since nothing else brings the process under its limit. And
//!    none is given while the allocator's freed pages have grown by what is short since
//!    they were last purged: the purge comes first, so a kept front end outlives any wait
//!    smaller than what the allocator holds freed.
//! 5. A front end marked the floor (the checkout built last: what keeps the edit loop alive)
//!    is never given whole. When the footprint is past the limit and only floors are left, the
//!    planner says so by name ([`Outcome::FloorOver`]) and gives nothing.
//! 6. Hysteresis: an owner that was given and made again is not given again until
//!    [`hold_ns`] after its remake, a time that grows with what the remake cost. A give of an
//!    owner that was given and remade before, after that hold, is a thrash: counted
//!    ([`Counts::thrash`]) and marked on the give ([`Give::again`]).

#![blaze::sans_io]

use crate::footprint::PURGE_EVERY_NS;

/// What can be given back, cheapest to make again first. The order is the order of giving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Rung {
    /// Pages the allocators freed and still hold: a purge returns them, and nothing is made
    /// again.
    Allocator = 0,
    /// The stores' buffer pool pages no task pins: a page given back is a read when it is next
    /// asked for.
    Pool = 1,
    /// Caches made again in milliseconds (the walks codegen keeps, an image's warm link state).
    Caches = 2,
    /// Values of a front end the engine makes again from their memos: MIR, bodies' inference,
    /// constant values, solver caches.
    Values = 3,
    /// A checkout's whole kept front end: its next build lowers anew, which takes minutes.
    FrontEnd = 4,
}

impl Rung {
    pub const ALL: [Rung; 5] =
        [Rung::Allocator, Rung::Pool, Rung::Caches, Rung::Values, Rung::FrontEnd];

    pub const fn index(self) -> usize {
        self as usize
    }

    /// The name spans, events and `blaze explain memory` say it by.
    pub const fn name(self) -> &'static str {
        match self {
            Rung::Allocator => "allocator freed pages",
            Rung::Pool => "store pool pages",
            Rung::Caches => "caches",
            Rung::Values => "front end values",
            Rung::FrontEnd => "front end",
        }
    }

    /// Whether what it gives is of value: time to make again that a user waits for. The
    /// proportion rule (the module's rule 4) holds these alone.
    pub const fn valued(self) -> bool {
        matches!(self, Rung::Values | Rung::FrontEnd)
    }

    /// The least time between two asks of one source of this rung that an ask leaves as it
    /// is. The cheap rungs' `gives` are estimates that may never fall (the allocator's kept
    /// bytes hold the code and the stacks too), so an ask that gave nothing is not repeated at
    /// every step; one that grew since by what is short is asked at once all the same
    /// (`Planner::step`). A valued source's `gives` is what its owner counts, and falls as it
    /// gives.
    const fn retry_ns(self) -> u64 {
        match self {
            Rung::Allocator | Rung::Pool | Rung::Caches => PURGE_EVERY_NS,
            Rung::Values | Rung::FrontEnd => 0,
        }
    }
}

/// How many times the target a give of a valued rung may be, for a predicted ask: 4.
pub const FACTOR: u64 = 4;

/// The least margin given past what is short: 64 MiB, so a task that waits for a few megabytes
/// does not wait again for the next few.
pub const MARGIN_MIN: u64 = 64 << 20;

/// The longest the planner waits for a reading after a rung's gives before it plans on with
/// the reading it has: 2 s. A purge waits a quarter of a second for the workers' heaps and
/// walks every heap; a reading that has not come by then is counted ([`Counts::unmeasured`]).
pub const MEASURE_WITHIN_NS: u64 = 2_000_000_000;

/// The least time an owner that was given and made again is held before it may be given again:
/// 10 s.
pub const HOLD_MIN_NS: u64 = 10_000_000_000;

/// How many times its remake's time an owner made again is held: 16. A front end that took
/// 195 s to lower is held 52 minutes.
pub const HOLD_FACTOR: u64 = 16;

/// What is given for `need` bytes short: `need` and a margin of an eighth of it, at least
/// [`MARGIN_MIN`].
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
    /// values, 0 where a rung has one owner.
    pub owner: u32,
    /// The bytes it can give now.
    pub gives: u64,
    /// The step it gives in: 0 or 1 for any amount, `gives` or more for all or nothing.
    pub grain: u64,
    /// What making all of `gives` again costs, as its spans last said; 0 where nothing is
    /// made again or it is not known.
    pub remake_ns: u64,
    /// The floor that keeps the edit loop alive: never given whole (the module's rule 5).
    /// Read of [`Rung::FrontEnd`] sources alone.
    pub floor: bool,
}

/// What the planner decides on, read at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Now {
    pub at_ns: u64,
    /// The limit the process is held to; `u64::MAX` for none.
    pub limit: u64,
    /// The process's footprint at the last reading: purged pages are not in it.
    pub footprint: u64,
    /// The bytes a task waits for of what the process holds besides its tasks
    /// (`Plane::short`); 0 when none waits.
    pub ask: u64,
    /// How many footprint readings were taken so far.
    pub reading: u64,
    /// How many of them were taken right after a purge.
    pub purged: u64,
}

/// Why a give is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// A task waits for this many bytes the process holds besides its tasks.
    Ask(u64),
    /// The footprint is this many bytes past the limit.
    Over(u64),
}

impl Why {
    /// The bytes short.
    pub const fn need(self) -> u64 {
        match self {
            Why::Ask(bytes) | Why::Over(bytes) => bytes,
        }
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
    pub why: Why,
    /// The limit and the footprint it was decided on.
    pub limit: u64,
    pub footprint: u64,
    /// This owner was given before and made again since: a thrash (the module's rule 6).
    pub again: bool,
    /// The planner made it.
    pub by: Planned,
}

/// What a step decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The footprint is within the limit and no task waits: nothing to give.
    Enough,
    /// The gives of `rung` are not measured yet: nothing more until a reading after them.
    Measuring { rung: Rung },
    /// The gives of `rung` were added to `out`: the driver does them, then reads the
    /// footprint.
    Gave { rung: Rung },
    /// A task waits for `need` bytes and nothing in proportion is left to give: it waits for
    /// a task to end, or starts alone (`Plane::pick`). No front end is dropped for it.
    Waits { need: u64 },
    /// The footprint is `over` bytes past the limit and what is left is the floor (and what
    /// hysteresis holds): `kept` bytes of front ends, the first of them `owner`'s. The driver
    /// says so to the user by name; nothing is given.
    FloorOver { over: u64, limit: u64, footprint: u64, owner: u32, kept: u64 },
}

/// What the planner did, counted: `blaze explain memory`'s rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Gives asked and their bytes, by [`Rung::index`].
    pub gives: [u64; 5],
    pub bytes: [u64; 5],
    /// Waits for a reading that ended at [`MEASURE_WITHIN_NS`] with none.
    pub unmeasured: u64,
    /// Steps at which an owner made again was kept out of a plan by its hold.
    pub held: u64,
    /// Gives of an owner that was given and made again before: the named failure.
    pub thrash: u64,
    /// Steps that said [`Outcome::FloorOver`].
    pub floor_over: u64,
    /// Steps at which a valued source was passed over as out of proportion to the ask.
    pub out_of_proportion: u64,
}

/// The gives of one rung the planner waits to see measured.
#[derive(Clone, Copy, Debug)]
struct Awaiting {
    rung: Rung,
    reading: u64,
    purged: u64,
    since_ns: u64,
}

impl Awaiting {
    /// Whether a reading after the gives came: for the allocator's, one taken after a purge.
    const fn measured(&self, now: &Now) -> bool {
        match self.rung {
            Rung::Allocator => now.purged > self.purged,
            _ => now.reading > self.reading,
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
    /// step after that ask was measured: one that gave nothing (`left` no less than `at_ask`)
    /// says an estimate that does not fall.
    at_ask: u64,
    left: u64,
    /// Its last ask is not measured yet: `left` is still to be read.
    unread: bool,
    /// Whether it was asked since it was last made again (or ever, if never remade).
    given: bool,
    /// Whether it was given and made again at least once.
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
    /// Today's behaviour, the control: any ask lets go of every front end, whole, at once.
    WholeFrontEnd,
    /// The next rung is asked before the last one's gives are measured.
    NoMeasure,
    /// Every source of a rung is taken, not only what covers the target.
    TakeAll,
    /// An owner made again is given again at once.
    NoHold,
    /// The floor is given like any front end.
    NoFloor,
}

/// The give-back rule (the module's doc). One a daemon.
#[derive(Debug)]
pub struct Planner {
    awaiting: Option<Awaiting>,
    /// One entry a source ever asked or remade: a handful (the rungs' single owners and a
    /// checkout a front end), so a scan is cheap.
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

    /// The rung whose gives are not measured yet, if any.
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

    /// Whether a step at `now` would plan: something is short, and no give waits for a
    /// reading that may still come. A driver whose sources cost something to list lists them
    /// only then; a step with none listed is right otherwise (it says [`Outcome::Enough`] or
    /// [`Outcome::Measuring`]).
    pub fn due(&self, now: &Now) -> bool {
        if now.ask == 0 && now.footprint <= now.limit {
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

    /// What each source whose last ask is not read yet can still give, now that its give is
    /// in a reading (or the wait for one ended): one no longer listed has nothing left.
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
    /// which the driver does before it reads the footprint and steps again. `sources` is what
    /// every owner can give now, in any order.
    pub fn step(&mut self, now: &Now, sources: &[Source], out: &mut Vec<Give>) -> Outcome {
        let over = now.footprint.saturating_sub(now.limit);
        let need = now.ask.max(over);
        if need == 0 {
            // A give still to be measured stays so: what its sources have left is read only
            // from a reading taken after it.
            if self.awaiting.is_some_and(|it| it.measured(now)) {
                self.awaiting = None;
                self.read_left(sources);
            }
            return Outcome::Enough;
        }
        let why = if over >= now.ask { Why::Over(over) } else { Why::Ask(now.ask) };
        #[cfg(test)]
        if self.plant == Plant::WholeFrontEnd {
            return self.whole_front_ends(now, why, sources, out);
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
                // What was asked and never measured is not asked again at once: its retry
                // counts from now, so the next rung has its turn (bounded-progress).
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
        // What a give of a valued rung may be, for a predicted ask (rule 4).
        let most = match why {
            Why::Ask(_) => target.saturating_mul(FACTOR),
            Why::Over(_) => u64::MAX,
        };
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
                    // what is short: asking again now would give nothing. A source that grew
                    // by the need is asked at once, before any dearer rung: a purge that
                    // alone covers a wait always comes before a value is dropped for it.
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
                if rung.valued() && !all && bytes > most.saturating_sub(sum) {
                    self.counts.out_of_proportion += 1;
                    continue;
                }
                sum = sum.saturating_add(bytes);
                let seen = self.asked_mut(rung, source.owner);
                let again = seen.remade;
                seen.at_ns = now.at_ns;
                seen.ever = true;
                seen.at_ask = source.gives;
                seen.unread = true;
                seen.given = true;
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
                    why,
                    limit: now.limit,
                    footprint: now.footprint,
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
            self.awaiting = Some(Awaiting {
                rung,
                reading: now.reading,
                purged: now.purged,
                since_ns: now.at_ns,
            });
            return Outcome::Gave { rung };
        }
        if over == 0 {
            return Outcome::Waits { need };
        }
        // Past the limit with nothing left to give but what is never given: the floor, and
        // what a remake holds.
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
                Outcome::FloorOver {
                    over,
                    limit: now.limit,
                    footprint: now.footprint,
                    owner,
                    kept,
                }
            }
            // No front end is kept at all: what is past the limit is the tasks' own, which
            // admission and the ceiling's guard answer.
            None => Outcome::Waits { need },
        }
    }

    /// [`Plant::WholeFrontEnd`]: the rule before this module, kept as the control. Every front
    /// end goes, whole, for any ask, with no purge first and no measure between.
    #[cfg(test)]
    fn whole_front_ends(
        &mut self,
        now: &Now,
        why: Why,
        sources: &[Source],
        out: &mut Vec<Give>,
    ) -> Outcome {
        let before = out.len();
        for source in sources {
            if source.rung == Rung::FrontEnd && source.gives > 0 {
                out.push(Give {
                    rung: Rung::FrontEnd,
                    owner: source.owner,
                    bytes: source.gives,
                    remake_ns: source.remake_ns,
                    why,
                    limit: now.limit,
                    footprint: now.footprint,
                    again: false,
                    by: Planned(()),
                });
            }
        }
        match out.len() > before {
            true => Outcome::Gave { rung: Rung::FrontEnd },
            false => Outcome::Waits { need: why.need() },
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
    use std::fmt::Write as _;
    let gb = |bytes: u64| bytes as f64 / 1e9;
    // alloc: a note, once a front end let go
    let mut line = format!("analysis of {project} was dropped to free {:.1} GB", gb(give.bytes));
    let _ = match give.limit {
        u64::MAX => write!(line, " (no limit, held {:.1} GB)", gb(give.footprint)),
        limit => write!(line, " (limit {:.1} GB, held {:.1} GB)", gb(limit), gb(give.footprint)),
    };
    line.push_str("; the next build redoes it");
    line
}

/// The note a user reads when the floor does not fit under the limit
/// ([`Outcome::FloorOver`]): one line, the checkout by name.
pub fn floor_line(project: &str, over: u64, limit: u64, footprint: u64, kept: u64) -> String {
    let gb = |bytes: u64| bytes as f64 / 1e9;
    // alloc: a note, once a stretch past the limit
    format!(
        "memory: blaze holds {:.1} GB, {:.1} GB past its limit of {:.1} GB, and what is left to give is the analysis of {project} ({:.1} GB), which the next edit needs: it is kept; free memory or raise the limit",
        gb(footprint),
        gb(over),
        gb(limit),
        gb(kept),
    )
}

#[cfg(test)]
mod tests;
