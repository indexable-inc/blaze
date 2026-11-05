//! The system's memory pressure, as admission counts it (docs/memory-budget.md section 7,
//! "Pressure"): a sans-IO core on the time it is told.
//!
//! The system says memory is short before any budget can: on macOS a kqueue knote of
//! `EVFILT_MEMORYSTATUS`, on Linux a PSI trigger (`blaze_core::Effect::Pressure`). Each event
//! makes the daemon act as at its soft ceiling now: the plane admits nothing new, the front ends
//! are asked for a release and the heaps give back their free pages. For [`HOLD_NS`] after the
//! last event the budget the plane admits against is held at what the process held then
//! ([`Pressured::cap`]), never more, so admission resumes only within what the system already
//! bore; once that long passes with no event it is the allowance again.
//!
//! The one place the cap meets the allowance is [`budget_under`]: the plane's budget now is
//! `min(allowance, cap)` (`Plane::budget_now`).
//!
//! What a build's client is told of a wait for memory, by pressure or by its budget, is
//! [`Paused`]'s to decide and [`paused_line`]'s to say ("What the client is told").

#![blaze::sans_io]

/// How long after the last event of memory pressure the budget stays held at what the process
/// held: 10 s.
pub const HOLD_NS: u64 = 10_000_000_000;

/// The pressure the system last said, of one daemon: `until_ns` the time the budget is held to
/// (the last event's, plus [`HOLD_NS`]), `held_at` the bytes it is held at, `released_ns` when
/// it last asked its front ends for a release for it. The default is a daemon the system never
/// pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pressured {
    pub until_ns: u64,
    pub held_at: u64,
    released_ns: Option<u64>,
}

impl Pressured {
    /// An event of memory pressure at `at_ns`, with the process holding `held` bytes then. Of a
    /// daemon still held, the budget stays at the least it was held at in this run of events:
    /// what the process let go of since the first is not taken back while the system still
    /// says memory is short.
    #[must_use]
    pub fn pressed(self, at_ns: u64, held: u64) -> Pressured {
        let held_at = match self.cap(at_ns) {
            Some(was) => was.min(held),
            None => held,
        };
        Pressured { until_ns: at_ns.saturating_add(HOLD_NS), held_at, ..self }
    }

    /// Whether the event at `at_ns` asks the front ends for a release: the first of a run of
    /// events does, and then one each [`HOLD_NS`], so a run that goes on (a PSI trigger fires
    /// each window it stays over) lets go again as it lasts but never sweeps the front ends
    /// over and over. Counts it as asked when it does.
    pub fn release_due(&mut self, at_ns: u64) -> bool {
        let due = self.released_ns.is_none_or(|was| at_ns.saturating_sub(was) >= HOLD_NS);
        if due {
            self.released_ns = Some(at_ns);
        }
        due
    }

    /// The most the budget may be at `at_ns`: what the process held at the events, until
    /// [`HOLD_NS`] after the last; `None` once that passes, or for a daemon never pressed.
    pub fn cap(self, at_ns: u64) -> Option<u64> {
        (at_ns < self.until_ns).then_some(self.held_at)
    }
}

/// The budget admission counts against: the daemon's `allowance` (its ceiling and the
/// machine's room, `admission::set_budgets`), held to `cap` while the system says memory is
/// short. The one join of the two.
pub fn budget_under(allowance: u64, cap: Option<u64>) -> u64 {
    cap.map_or(allowance, |cap| allowance.min(cap))
}

/// How long a build is held back for memory before its client is told: 1 s.
pub const PAUSED_AFTER_NS: u64 = 1_000_000_000;

/// What a build's client is told of its wait for memory: a line once it has been held back
/// for [`PAUSED_AFTER_NS`], again only when what it says changes (what the daemon holds moves
/// by more than a 64th of the budget, or the tasks it waits on change), and that it resumed
/// once it is held back no more.
#[derive(Debug, Default)]
pub struct Paused {
    /// Since when it has been held back, without a break.
    since_ns: Option<u64>,
    /// What the line last said: what the daemon held, and the tasks it waited on, by id
    /// ascending. `None` while no line is standing.
    said: Option<(u64, Vec<u32>)>,
}

/// What [`Paused::step`] says to tell the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tell {
    Nothing,
    /// The paused line, again or for the first time.
    Paused,
    /// Admission resumed after a paused line.
    Resumed,
}

impl Paused {
    /// Whether it counts a wait or has a line standing: a build that is not held back and has
    /// neither has nothing to be told.
    pub fn waiting(&self) -> bool {
        self.since_ns.is_some() || self.said.is_some()
    }

    /// The build at `at_ns`: `held_back` whether a task of it is held back for memory alone
    /// now (`Plane::waits_for_memory`), the daemon holding `held` of `budget`, the tasks granted
    /// memory running `on`, by id ascending.
    pub fn step(
        &mut self,
        at_ns: u64,
        held_back: bool,
        held: u64,
        budget: u64,
        on: &[u32],
    ) -> Tell {
        if !held_back {
            self.since_ns = None;
            return match self.said.take() {
                Some(_) => Tell::Resumed,
                None => Tell::Nothing,
            };
        }
        let since = *self.since_ns.get_or_insert(at_ns);
        if at_ns.saturating_sub(since) < PAUSED_AFTER_NS {
            return Tell::Nothing;
        }
        if let Some((was, waited)) = &mut self.said {
            let moved = held.abs_diff(*was) > budget / 64;
            if !moved && waited.as_slice() == on {
                return Tell::Nothing;
            }
            *was = held;
            waited.clear();
            waited.extend_from_slice(on); // alloc: none once it has held this many.
            return Tell::Paused;
        }
        // alloc: one a build that is held back, the tasks it waits on.
        self.said = Some((held, on.to_vec()));
        Tell::Paused
    }
}

/// The line a build's client is told while it is held back for memory:
/// `build paused: daemon at 47.9 of 48 GiB, waiting on 3 tasks: codegen of hir-ty, ...`.
/// `held` of `budget` is what the daemon holds of its allowance; `cap`, the pressure cap while
/// there is one; `machine`, the machine's ledger as it says who holds the rest (`machine 96
/// GiB, 40.1 held by pid 4242 of /Users/a/.blaze`), once it does; `waiting` how many tasks
/// granted memory run, and `names` the first of them (three are said).
pub fn paused_line(
    held: u64,
    budget: u64,
    cap: Option<u64>,
    machine: Option<&str>,
    waiting: usize,
    names: &[&str],
) -> String {
    use std::fmt::Write as _;
    // alloc: one line, told to a client at most once per change.
    let mut line = format!("build paused: daemon at {} of {} GiB", gib(held), gib(budget));
    if let Some(cap) = cap {
        _ = write!(line, " (held at {} GiB: the system is short of memory)", gib(cap));
    }
    // The machine's ledger: which other daemons hold the rest (docs/memory-budget.md section
    // 7, "The ledger"): `None` while no other daemon holds any (blaze-cargo's `Machine::says`).
    if let Some(machine) = machine {
        _ = write!(line, " ({machine})");
    }
    match waiting {
        0 => line.push_str(", waiting for the system's memory pressure to pass"),
        1 => line.push_str(", waiting on 1 task: "),
        n => _ = write!(line, ", waiting on {n} tasks: "),
    }
    for (at, name) in names.iter().take(3).enumerate() {
        if at > 0 {
            line.push_str(", ");
        }
        line.push_str(name);
    }
    if waiting > names.len().min(3) {
        line.push_str(", ...");
    }
    line
}

/// The line a build's client is told once its admission resumes after [`paused_line`].
pub const RESUMED: &str = "build resumed: memory is free again";

/// `bytes` in GiB to a tenth, a whole number without its `.0` (`47.9`, `48`).
fn gib(bytes: u64) -> String {
    let tenths = (bytes as f64 / f64::from(1u32 << 30) * 10.0).round() as u64;
    // alloc: a part of one line.
    if tenths % 10 == 0 {
        format!("{}", tenths / 10)
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn a_daemon_never_pressed_admits_against_its_allowance() {
        let never = Pressured::default();
        assert_eq!(never.cap(0), None);
        assert_eq!(never.cap(u64::MAX), None);
        assert_eq!(budget_under(48 * GIB, never.cap(5)), 48 * GIB);
    }

    #[test]
    fn under_pressure_the_budget_is_held_at_what_the_process_held() {
        let pressed = Pressured::default().pressed(1_000, 30 * GIB);
        assert_eq!(budget_under(48 * GIB, pressed.cap(1_000)), 30 * GIB);
        assert_eq!(budget_under(48 * GIB, pressed.cap(1_000 + HOLD_NS - 1)), 30 * GIB);
        // Never more than the allowance, which may have fallen below it since.
        assert_eq!(budget_under(20 * GIB, pressed.cap(2_000)), 20 * GIB);
    }

    #[test]
    fn the_hold_ends_ten_seconds_after_the_last_event() {
        let first = Pressured::default().pressed(0, 30 * GIB);
        let again = first.pressed(9 * HOLD_NS / 10, 31 * GIB);
        // The second event moves the end on; the first's end alone has passed.
        assert_eq!(again.cap(HOLD_NS), Some(30 * GIB));
        assert_eq!(again.cap(9 * HOLD_NS / 10 + HOLD_NS - 1), Some(30 * GIB));
        assert_eq!(again.cap(9 * HOLD_NS / 10 + HOLD_NS), None);
        assert_eq!(budget_under(48 * GIB, again.cap(2 * HOLD_NS)), 48 * GIB);
    }

    #[test]
    fn within_a_run_of_events_the_cap_only_falls() {
        let first = Pressured::default().pressed(0, 30 * GIB);
        // Released since: held lower.
        let lower = first.pressed(1_000, 28 * GIB);
        assert_eq!(lower.cap(1_000), Some(28 * GIB));
        // Grown since (grants running): not raised.
        let grown = lower.pressed(2_000, 33 * GIB);
        assert_eq!(grown.cap(2_000), Some(28 * GIB));
        // A new run, after the hold ended, starts at what is held then.
        let later = grown.pressed(2_000 + HOLD_NS, 33 * GIB);
        assert_eq!(later.cap(2_000 + HOLD_NS), Some(33 * GIB));
    }

    #[test]
    fn a_release_is_asked_once_per_hold_of_a_run_of_events() {
        let mut pressed = Pressured::default().pressed(0, GIB);
        assert!(pressed.release_due(0));
        assert!(!pressed.release_due(HOLD_NS / 2));
        assert!(!pressed.release_due(HOLD_NS - 1));
        assert!(pressed.release_due(HOLD_NS));
    }

    const MS: u64 = 1_000_000;

    #[test]
    fn the_paused_line_waits_a_second_then_says_it_once() {
        let mut paused = Paused::default();
        let budget = 64 * GIB;
        assert_eq!(paused.step(0, true, 63 * GIB, budget, &[1, 2]), Tell::Nothing);
        assert_eq!(paused.step(999 * MS, true, 63 * GIB, budget, &[1, 2]), Tell::Nothing);
        assert_eq!(paused.step(1_000 * MS, true, 63 * GIB, budget, &[1, 2]), Tell::Paused);
        // Held moves by less than a 64th of the budget (1 GiB here): nothing new.
        let less = 63 * GIB + GIB - 1;
        assert_eq!(paused.step(1_500 * MS, true, less, budget, &[1, 2]), Tell::Nothing);
    }

    #[test]
    fn the_paused_line_is_said_again_when_it_changes() {
        let mut paused = Paused::default();
        let budget = 64 * GIB;
        paused.step(0, true, 60 * GIB, budget, &[1, 2]);
        assert_eq!(paused.step(2_000 * MS, true, 60 * GIB, budget, &[1, 2]), Tell::Paused);
        // More than a 64th of the budget, from what was said, not from the last step.
        assert_eq!(
            paused.step(2_100 * MS, true, 60 * GIB + GIB / 2, budget, &[1, 2]),
            Tell::Nothing
        );
        assert_eq!(paused.step(2_200 * MS, true, 61 * GIB + 1, budget, &[1, 2]), Tell::Paused);
        // A task it waited on ended.
        assert_eq!(paused.step(2_300 * MS, true, 61 * GIB, budget, &[2]), Tell::Paused);
        assert_eq!(paused.step(2_400 * MS, true, 61 * GIB, budget, &[2]), Tell::Nothing);
    }

    #[test]
    fn resumed_is_said_once_and_only_after_a_paused_line() {
        let mut paused = Paused::default();
        let budget = 64 * GIB;
        // Held back for less than a second: no line, so no resumed.
        paused.step(0, true, 60 * GIB, budget, &[1]);
        assert_eq!(paused.step(500 * MS, false, 60 * GIB, budget, &[]), Tell::Nothing);
        // The wait starts over.
        paused.step(600 * MS, true, 60 * GIB, budget, &[1]);
        assert_eq!(paused.step(1_500 * MS, true, 60 * GIB, budget, &[1]), Tell::Nothing);
        assert_eq!(paused.step(1_600 * MS, true, 60 * GIB, budget, &[1]), Tell::Paused);
        assert_eq!(paused.step(1_700 * MS, false, 60 * GIB, budget, &[]), Tell::Resumed);
        assert_eq!(paused.step(1_800 * MS, false, 60 * GIB, budget, &[]), Tell::Nothing);
    }

    #[test]
    fn the_paused_line_names_what_waits_on_what() {
        let names = ["codegen of hir-ty", "link of blaze", "test of blaze-core", "codegen of x"];
        assert_eq!(
            paused_line(479 * GIB / 10, 48 * GIB, None, None, 4, &names),
            "build paused: daemon at 47.9 of 48 GiB, waiting on 4 tasks: codegen of hir-ty, \
             link of blaze, test of blaze-core, ..."
        );
        assert_eq!(
            paused_line(30 * GIB, 48 * GIB, Some(30 * GIB), None, 1, &names[..1]),
            "build paused: daemon at 30 of 48 GiB (held at 30 GiB: the system is short of \
             memory), waiting on 1 task: codegen of hir-ty"
        );
        let machine = "machine 96 GiB, 40.1 held by pid 4242 of /Users/a/.blaze";
        assert_eq!(
            paused_line(GIB / 2, 48 * GIB, Some(GIB / 2), Some(machine), 0, &[]),
            "build paused: daemon at 0.5 of 48 GiB (held at 0.5 GiB: the system is short of \
             memory) (machine 96 GiB, 40.1 held by pid 4242 of /Users/a/.blaze), waiting for the \
             system's memory pressure to pass"
        );
    }
}
