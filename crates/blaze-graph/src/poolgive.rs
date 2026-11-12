//! How the stores' buffer pools give pages back on the daemon's loop: a slice a turn, never a
//! whole sweep. A sans-IO core on the asks, the sweeps' results and the time it is told; its
//! driver is `blaze-cargo`'s `serve::job::giving` (`Jobs::sweep_pools`), the one place the
//! loop evicts pool pages while a build may run.
//!
//! Until this, three paths evicted in line on the loop: the give-back ladder's pool rung (as
//! many bytes as were short, uncapped), and the pressure handler and the plot's squeezed look,
//! each `Store::evict_pool(u64::MAX)` on both stores once a second. On 2026-11-12 (a cold
//! build of tempo, the daemon's kernel throttling every thread that touched memory) those
//! sweeps were 68 spans of 1,605 s wall for 90 s of CPU on the loop's thread, with turns of up
//! to 542 s, during which the ladder and the typed failure of a task that does not fit, which
//! run once a turn, could not run.
//!
//! [`Slices`] is the rule that replaces them:
//!
//! 1. One sweep a turn at most ([`Slices::turn`]), of at most [`SLICE`] bytes, and no sooner
//!    than [`EVERY_NS`] after the last. What is still wanted is asked again by a later turn:
//!    the loop turns between two slices, so everything else it does once a turn is at most
//!    one slice late.
//! 2. Two wants, neither a sweep by itself. The ladder's ([`Slices::ask`]) is a byte count,
//!    lowered by what each sweep frees and dropped when the ladder says the footprint is
//!    within its limit ([`Slices::enough`]). The system's ([`Slices::stand`]: memory pressure,
//!    or a footprint past the release line) is every unpinned page, for as long as it stands
//!    and no longer: its asker says until when, and says so again while its reason lasts.
//! 3. A sweep that frees nothing [`DRY_AFTER`] times in a row is the typed state
//!    [`Swept::Dry`]: every page left is pinned by a running task or was just read. The
//!    ladder's want is dropped (the answer says how much was left), nothing is swept for
//!    [`DRY_NS`], and an ask meanwhile is answered [`Asked::Dry`], so the ladder goes on to
//!    its next rung and to the fail line. Never a spin: between two dry spells at most
//!    [`DRY_AFTER`] sweeps free nothing.

#![blaze::sans_io]

/// The most one turn of the loop asks of the pools: 64 MiB, 16,384 pages of 4 KiB given back
/// with a few `madvise` calls, some milliseconds of a turn where the kernel is not short.
pub const SLICE: u64 = 64 << 20;

/// The least time between two sweeps: 10 ms, so a loop that turns thousands of times a second
/// spends at most one slice in each 10 ms on the pools (6.4 GiB a second at most), and each
/// sweep's own upkeep (the pins of ended tasks let go) is not paid at every turn.
pub const EVERY_NS: u64 = 10_000_000;

/// How many sweeps in a row free nothing before the pools are said dry: 2. One empty sweep
/// may be the sweep's hand passing pages read since it last came by, which the next one
/// takes; a second is every page pinned.
pub const DRY_AFTER: u32 = 2;

/// How long dry pools are not swept again: 10 s. What pins their pages is running tasks,
/// which end in seconds, and an empty sweep visits every open page twice.
pub const DRY_NS: u64 = 10_000_000_000;

/// What a turn does ([`Slices::turn`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// Nothing is wanted, or the pools are dry.
    Idle,
    /// Sweep the pools for this many bytes, at most [`SLICE`], and say what came of it
    /// ([`Slices::swept`]) before the next turn.
    Sweep { bytes: u64, why: Why },
}

/// Whose want a sweep answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The give-back ladder's pool rung: the footprint is past its limit's line, or a task
    /// waits for memory.
    Ladder,
    /// The system: memory pressure, or a footprint past the release line.
    System,
}

impl Why {
    pub const fn name(self) -> &'static str {
        match self {
            Why::Ladder => "the give-back ladder asked",
            Why::System => "memory is short (pressure, or the footprint is past its release line)",
        }
    }
}

/// What an ask is told ([`Slices::ask`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asked {
    /// Wanted: swept a slice a turn from the next turn on.
    Wanted,
    /// The pools are dry until this time: nothing is wanted of them, and the asker goes on.
    Dry { until_ns: u64 },
}

/// What a sweep came to ([`Slices::swept`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swept {
    /// It freed bytes; `left` of the ladder's want is still to come, a slice a turn.
    Freed { left: u64 },
    /// It freed nothing, for the first time in a row: asked once more.
    Empty,
    /// It freed nothing [`DRY_AFTER`] times in a row: `left` of the ladder's want was dropped,
    /// and nothing is swept until `until_ns`.
    Dry { left: u64, until_ns: u64 },
}

/// What the sweeps have done, for a daemon's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub sweeps: u64,
    pub freed: u64,
    /// Times the pools were said dry.
    pub dry: u64,
}

/// A removed safeguard the simulation plants, to see that its oracle catches the removal.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// The rule before this module: a turn sweeps for everything wanted at once
    /// (`evict_pool(u64::MAX)` under pressure).
    WholeWant,
    /// An empty sweep is asked again every turn, for as long as its want stands.
    NeverDry,
}

/// The pools' give-back state, the daemon's one.
#[derive(Clone, Debug)]
pub struct Slices {
    /// What the ladder still wants.
    ladder: u64,
    /// Until when the system wants every unpinned page; 0 for no such want.
    system_until_ns: u64,
    /// Sweeps in a row that freed nothing.
    empty: u32,
    /// Until when the pools are dry; 0 while they are not.
    dry_until_ns: u64,
    /// The earliest the next sweep may be: [`EVERY_NS`] after the last.
    next_ns: u64,
    counts: Counts,
    #[cfg(test)]
    plant: Plant,
}

impl Default for Slices {
    fn default() -> Slices {
        Slices::new()
    }
}

impl Slices {
    pub const fn new() -> Slices {
        Slices {
            ladder: 0,
            system_until_ns: 0,
            empty: 0,
            dry_until_ns: 0,
            next_ns: 0,
            counts: Counts { sweeps: 0, freed: 0, dry: 0 },
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    pub(crate) fn planted(plant: Plant) -> Slices {
        Slices { plant, ..Slices::new() }
    }

    pub const fn counts(&self) -> Counts {
        self.counts
    }

    /// Whether the pools are dry at `now_ns`: their last sweeps freed nothing, and they are
    /// not asked again yet. The ladder does not list them as a source meanwhile.
    pub const fn dry(&self, now_ns: u64) -> bool {
        now_ns < self.dry_until_ns
    }

    /// The ladder wants `bytes` of the pools' pages at `now_ns`: its want is raised to that,
    /// not added to it, since the ladder asks again for what a later reading still says is
    /// short.
    pub fn ask(&mut self, bytes: u64, now_ns: u64) -> Asked {
        if self.dry(now_ns) {
            return Asked::Dry { until_ns: self.dry_until_ns };
        }
        self.ladder = self.ladder.max(bytes);
        Asked::Wanted
    }

    /// The ladder says the footprint is within its limit and no task waits: its want is
    /// dropped, whatever was left of it.
    pub fn enough(&mut self) {
        self.ladder = 0;
    }

    /// The system wants every unpinned page until `until_ns`: said at each event of memory
    /// pressure and at each look that finds the footprint past its release line, with the
    /// time that reason stands for.
    pub fn stand(&mut self, until_ns: u64) {
        self.system_until_ns = self.system_until_ns.max(until_ns);
    }

    /// What this turn of the loop does, at `now_ns`: one sweep of a slice, or nothing.
    pub fn turn(&self, now_ns: u64) -> Turn {
        #[cfg(test)]
        let (whole, never_dry) = (self.plant == Plant::WholeWant, self.plant == Plant::NeverDry);
        #[cfg(not(test))]
        let (whole, never_dry) = (false, false);
        if (self.dry(now_ns) && !never_dry) || now_ns < self.next_ns {
            return Turn::Idle;
        }
        let (want, why) = match now_ns < self.system_until_ns {
            true => (u64::MAX, Why::System),
            false => (self.ladder, Why::Ladder),
        };
        if want == 0 {
            return Turn::Idle;
        }
        let bytes = if whole || want < SLICE { want } else { SLICE };
        Turn::Sweep { bytes, why }
    }

    /// The sweep [`Slices::turn`] asked for freed `freed` bytes, as of `now_ns`.
    pub fn swept(&mut self, freed: u64, now_ns: u64) -> Swept {
        self.counts.sweeps += 1;
        self.counts.freed = self.counts.freed.saturating_add(freed);
        self.next_ns = now_ns.saturating_add(EVERY_NS);
        if freed != 0 {
            self.empty = 0;
            self.ladder = self.ladder.saturating_sub(freed);
            return Swept::Freed { left: self.ladder };
        }
        self.empty += 1;
        if self.empty < DRY_AFTER {
            return Swept::Empty;
        }
        self.empty = 0;
        self.counts.dry += 1;
        self.dry_until_ns = now_ns.saturating_add(DRY_NS);
        Swept::Dry { left: std::mem::take(&mut self.ladder), until_ns: self.dry_until_ns }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seeded stream (SplitMix64): the simulation's only source of choice.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    const MIB: u64 = 1 << 20;
    /// One loop turn of the simulated daemon.
    const TURN_NS: u64 = 5_000_000;
    const TURNS: u64 = 12_000;

    /// One seeded run of a loop that turns every [`TURN_NS`] over a model pool: pages come in
    /// (tasks read records), are pinned and let go, the ladder asks for what is short, the
    /// system presses, and each turn does what [`Slices::turn`] says. `Err` names the first
    /// turn that broke the invariant.
    fn run(seed: u64, plant: Plant) -> Result<Counts, String> {
        let mut rng = Rng(seed);
        let mut slices = Slices::planted(plant);
        // The model pool: bytes loaded, and how many of them a running task pins.
        let mut loaded = rng.below(8 << 10) * MIB;
        let mut pinned = 0u64;
        // Sweeps that freed nothing since bytes were last freed or the pools last said dry.
        let mut empty_run = 0u32;
        let mut dry_until = 0u64;
        let mut last_sweep = None::<u64>;
        for turn in 0..TURNS {
            let now = turn * TURN_NS;
            match rng.below(40) {
                // A task reads records: the pool grows.
                0..=3 => loaded += rng.below(256) * MIB,
                // Tasks pin all of it, half of it, or let go.
                4 => pinned = loaded,
                5 => pinned = loaded / 2,
                6 | 7 => pinned = 0,
                // The ladder asks: megabytes, or more than the pool holds.
                8 | 9 => {
                    let bytes = match rng.below(3) {
                        0 => rng.below(64) * MIB + 1,
                        1 => rng.below(4 << 10) * MIB + 1,
                        _ => 64 << 30,
                    };
                    let asked = slices.ask(bytes, now);
                    if now < dry_until && asked == Asked::Wanted {
                        return Err(format!("turn {turn}: an ask of dry pools was taken as wanted"));
                    }
                }
                10 => slices.enough(),
                // Memory pressure: it stands for a while.
                11 => slices.stand(now + rng.below(3_000) * 1_000_000),
                _ => {}
            }
            pinned = pinned.min(loaded);
            let Turn::Sweep { bytes, .. } = slices.turn(now) else { continue };
            if bytes == 0 {
                return Err(format!("turn {turn}: a sweep of no bytes"));
            }
            if bytes > SLICE {
                return Err(format!(
                    "turn {turn}: one turn sweeps for {bytes} bytes, past a slice of {SLICE}: the loop does not turn until it is done"
                ));
            }
            if now < dry_until {
                return Err(format!("turn {turn}: dry pools are swept again before {dry_until} ns"));
            }
            if last_sweep.is_some_and(|last| now - last < EVERY_NS) {
                return Err(format!("turn {turn}: two sweeps within {EVERY_NS} ns"));
            }
            last_sweep = Some(now);
            // The pool gives what is asked of what no task pins, as `Store::evict_pool` does.
            let freed = bytes.min(loaded - pinned);
            loaded -= freed;
            let swept = slices.swept(freed, now);
            match (freed, swept) {
                (0, Swept::Dry { until_ns, .. }) => {
                    empty_run = 0;
                    dry_until = until_ns;
                    if until_ns != now + DRY_NS {
                        return Err(format!("turn {turn}: dry until {until_ns} ns, not for {DRY_NS} ns"));
                    }
                }
                (0, Swept::Empty) => {
                    empty_run += 1;
                    if empty_run >= DRY_AFTER {
                        return Err(format!(
                            "turn {turn}: {empty_run} sweeps in a row freed nothing and the pools are not said dry: a spin"
                        ));
                    }
                }
                (0, other) => return Err(format!("turn {turn}: a sweep that freed nothing is {other:?}")),
                (_, Swept::Freed { .. }) => empty_run = 0,
                (_, other) => return Err(format!("turn {turn}: a sweep that freed {freed} bytes is {other:?}")),
            }
        }
        Ok(slices.counts())
    }

    const SEEDS: u64 = 400;

    /// The invariant: no turn of the loop sweeps the pools for more than one slice, whatever
    /// is wanted and by whom; and sweeps that free nothing end in the typed dry state after
    /// [`DRY_AFTER`] in a row, during which nothing is swept and an ask is told so, so the
    /// loop always turns and the ladder always goes on to its next rung.
    #[test]
    fn a_turn_sweeps_one_slice_and_an_empty_pool_is_said_dry() {
        let mut total = Counts::default();
        for seed in 0..SEEDS {
            match run(seed, Plant::None) {
                Ok(counts) => {
                    total.sweeps += counts.sweeps;
                    total.freed += counts.freed;
                    total.dry += counts.dry;
                }
                Err(broke) => panic!("seed {seed}: {broke}"),
            }
        }
        // The runs met what they are about: sweeps that freed, and dry spells.
        assert!(total.sweeps > SEEDS && total.freed > SEEDS * SLICE && total.dry > SEEDS, "{total:?}");
    }

    /// The planted bug is what the loop did: a sweep for everything wanted in one turn.
    #[test]
    fn the_oracle_catches_a_whole_sweep_in_one_turn() {
        let caught = (0..SEEDS).filter(|&seed| {
            run(seed, Plant::WholeWant).is_err_and(|broke| broke.contains("past a slice"))
        });
        assert!(caught.count() as u64 > SEEDS / 2);
    }

    /// And a pool that gives nothing asked again at every turn while its want stands.
    #[test]
    fn the_oracle_catches_an_empty_sweep_asked_for_ever() {
        let caught = (0..SEEDS).filter(|&seed| {
            run(seed, Plant::NeverDry).is_err_and(|broke| broke.contains("before"))
        });
        assert!(caught.count() as u64 > SEEDS / 2);
    }

    #[test]
    fn the_wants_fall_and_stand_as_said() {
        const T: u64 = EVERY_NS;
        let mut slices = Slices::new();
        assert_eq!(slices.turn(0), Turn::Idle);
        // The ladder's want is swept a slice a turn, no sooner than `EVERY_NS` after the
        // last, and falls by what was freed.
        assert_eq!(slices.ask(3 * SLICE / 2, 0), Asked::Wanted);
        assert_eq!(slices.turn(0), Turn::Sweep { bytes: SLICE, why: Why::Ladder });
        assert_eq!(slices.swept(SLICE, 0), Swept::Freed { left: SLICE / 2 });
        assert_eq!(slices.turn(T - 1), Turn::Idle);
        assert_eq!(slices.turn(T), Turn::Sweep { bytes: SLICE / 2, why: Why::Ladder });
        assert_eq!(slices.swept(SLICE / 2, T), Swept::Freed { left: 0 });
        assert_eq!(slices.turn(2 * T), Turn::Idle);
        // The system's want is a slice a turn for as long as it stands, and no longer.
        slices.stand(5 * T);
        assert_eq!(slices.turn(4 * T), Turn::Sweep { bytes: SLICE, why: Why::System });
        assert_eq!(slices.turn(5 * T), Turn::Idle);
        // Two empty sweeps: dry, the ladder's want dropped and said, an ask told so.
        assert_eq!(slices.ask(SLICE, 6 * T), Asked::Wanted);
        assert_eq!(slices.swept(0, 6 * T), Swept::Empty);
        let until_ns = 7 * T + DRY_NS;
        assert_eq!(slices.swept(0, 7 * T), Swept::Dry { left: SLICE, until_ns });
        assert!(slices.dry(8 * T));
        assert_eq!(slices.turn(8 * T), Turn::Idle);
        assert_eq!(slices.ask(SLICE, 8 * T), Asked::Dry { until_ns });
        slices.stand(u64::MAX);
        assert_eq!(slices.turn(until_ns - 1), Turn::Idle);
        assert_eq!(slices.turn(until_ns), Turn::Sweep { bytes: SLICE, why: Why::System });
        assert_eq!(slices.counts(), Counts { sweeps: 4, freed: 3 * SLICE / 2, dry: 1 });
    }
}
