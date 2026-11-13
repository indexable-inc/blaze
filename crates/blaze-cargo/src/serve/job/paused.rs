//! What a build's client is told while a task of it is held back for memory by the gate
//! (`blaze_mem::gate`, docs/v2/plan/design/end-state/memory.md 4.3): sans-IO, driven by
//! `Jobs::tell_paused` on the loop's clock. Moved here from blaze-graph's `pressure` when the
//! gate replaced it (2026-11-13): the rule is the client's line alone, so it lives with the one
//! driver that says it; the CLI shows it as the build's wait (`cli::print`, "build paused: ").

/// How long a build is held back for memory before its client is told: 1 s.
pub(super) const PAUSED_AFTER_NS: u64 = 1_000_000_000;

/// What a build's client is told of its wait for memory: a line once it has been held back
/// for [`PAUSED_AFTER_NS`], again only when what it says changes (what the daemon holds moves
/// by more than a 64th of the ceiling, or the tasks it waits on change), and that it resumed
/// once it is held back no more.
#[derive(Debug, Default)]
pub(super) struct Paused {
    /// Since when it has been held back, without a break.
    since_ns: Option<u64>,
    /// What the line last said: what the daemon held, and the tasks it waited on, by id
    /// ascending. `None` while no line is standing.
    said: Option<(u64, Vec<u32>)>,
}

/// What [`Paused::step`] says to tell the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tell {
    Nothing,
    /// The paused line, again or for the first time.
    Paused,
    /// It is held back no more, after a paused line.
    Resumed,
}

impl Paused {
    /// Whether it counts a wait or has a line standing: a build that is not held back and has
    /// neither has nothing to be told.
    pub(super) fn waiting(&self) -> bool {
        self.since_ns.is_some() || self.said.is_some()
    }

    /// The build at `at_ns`: `held_back` whether a task of it is held back for memory alone
    /// now (`Plane::held_back`), the gate counting `held` committed of `ceiling`, the tasks
    /// granted memory running `on`, by id ascending.
    pub(super) fn step(&mut self, at_ns: u64, held_back: bool, held: u64, ceiling: u64, on: &[u32]) -> Tell {
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
            let moved = held.abs_diff(*was) > ceiling / 64;
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
/// `held` of `ceiling` is what the gate counts committed of its ceiling; `room`, the machine's
/// room under the ceiling while the machine's ledger holds the daemon lower; `machine`, the
/// ledger as it says who holds the rest (`machine 96 GiB, 40.1 held by pid 4242 of
/// /Users/a/.blaze`), once it does; `waiting` how many tasks granted memory run, and `names`
/// the first of them (three are said).
pub(super) fn paused_line(
    held: u64,
    ceiling: u64,
    room: Option<u64>,
    machine: Option<&str>,
    waiting: usize,
    names: &[&str],
) -> String {
    use std::fmt::Write as _;
    // alloc: one line, told to a client at most once per change.
    let mut line = format!("build paused: daemon at {} of {} GiB", gib(held), gib(ceiling));
    if let Some(room) = room {
        _ = write!(line, " (held at {} GiB: the machine's memory is taken by others)", gib(room));
    }
    // The machine's ledger: which other daemons hold the rest: `None` while no other daemon
    // holds any (blaze-cargo's `Machine::says`).
    if let Some(machine) = machine {
        _ = write!(line, " ({machine})");
    }
    match waiting {
        // No running task holds a grant: what is left to wait for is a turn of the machine's
        // ledger, which the plane asks at once.
        0 => line.push_str(
            ", waiting on no running task: a task held back with nothing granted starts at the \
             next turn of the machine's memory ledger",
        ),
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

/// The line a build's client is told once it is held back no more after [`paused_line`].
pub(super) const RESUMED: &str = "build resumed: memory is free again";

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
    use super::{Paused, Tell, paused_line};

    const GIB: u64 = 1 << 30;
    const MS: u64 = 1_000_000;

    #[test]
    fn the_paused_line_waits_a_second_then_says_it_once() {
        let mut paused = Paused::default();
        let ceiling = 64 * GIB;
        assert_eq!(paused.step(0, true, 63 * GIB, ceiling, &[1, 2]), Tell::Nothing);
        assert_eq!(paused.step(999 * MS, true, 63 * GIB, ceiling, &[1, 2]), Tell::Nothing);
        assert_eq!(paused.step(1_000 * MS, true, 63 * GIB, ceiling, &[1, 2]), Tell::Paused);
        // Held moves by less than a 64th of the ceiling (1 GiB here): nothing new.
        let less = 63 * GIB + GIB - 1;
        assert_eq!(paused.step(1_500 * MS, true, less, ceiling, &[1, 2]), Tell::Nothing);
    }

    #[test]
    fn the_paused_line_is_said_again_when_it_changes() {
        let mut paused = Paused::default();
        let ceiling = 64 * GIB;
        paused.step(0, true, 60 * GIB, ceiling, &[1, 2]);
        assert_eq!(paused.step(2_000 * MS, true, 60 * GIB, ceiling, &[1, 2]), Tell::Paused);
        // More than a 64th of the ceiling, from what was said, not from the last step.
        assert_eq!(paused.step(2_100 * MS, true, 60 * GIB + GIB / 2, ceiling, &[1, 2]), Tell::Nothing);
        assert_eq!(paused.step(2_200 * MS, true, 61 * GIB + 1, ceiling, &[1, 2]), Tell::Paused);
        // A task it waited on ended.
        assert_eq!(paused.step(2_300 * MS, true, 61 * GIB, ceiling, &[2]), Tell::Paused);
        assert_eq!(paused.step(2_400 * MS, true, 61 * GIB, ceiling, &[2]), Tell::Nothing);
    }

    #[test]
    fn resumed_is_said_once_and_only_after_a_paused_line() {
        let mut paused = Paused::default();
        let ceiling = 64 * GIB;
        // Held back for less than a second: no line, so no resumed.
        paused.step(0, true, 60 * GIB, ceiling, &[1]);
        assert_eq!(paused.step(500 * MS, false, 60 * GIB, ceiling, &[]), Tell::Nothing);
        // The wait starts over.
        paused.step(600 * MS, true, 60 * GIB, ceiling, &[1]);
        assert_eq!(paused.step(1_500 * MS, true, 60 * GIB, ceiling, &[1]), Tell::Nothing);
        assert_eq!(paused.step(1_600 * MS, true, 60 * GIB, ceiling, &[1]), Tell::Paused);
        assert_eq!(paused.step(1_700 * MS, false, 60 * GIB, ceiling, &[]), Tell::Resumed);
        assert_eq!(paused.step(1_800 * MS, false, 60 * GIB, ceiling, &[]), Tell::Nothing);
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
            "build paused: daemon at 30 of 48 GiB (held at 30 GiB: the machine's memory is taken \
             by others), waiting on 1 task: codegen of hir-ty"
        );
        let machine = "machine 96 GiB, 40.1 held by pid 4242 of /Users/a/.blaze";
        assert_eq!(
            paused_line(GIB / 2, 48 * GIB, Some(GIB / 2), Some(machine), 0, &[]),
            "build paused: daemon at 0.5 of 48 GiB (held at 0.5 GiB: the machine's memory is taken \
             by others) (machine 96 GiB, 40.1 held by pid 4242 of /Users/a/.blaze), waiting on no \
             running task: a task held back with nothing granted starts at the next turn of the \
             machine's memory ledger"
        );
    }
}
