//! What a test run did, counted ([`Tally`]): the one record the terminal's summary line, the
//! `blaze: tests` line off a terminal ([`tests_line`]) and the JSON summary are made from, so
//! none of them can say a run passed without saying how many tests it ran.
//!
//! The class this closes is vacuous success (`agents.json`, A39): a filter that matched no test
//! printed `test result: ok. 0 passed` and was read as a pass. A tally always carries how many
//! tests the selection held ([`Tally::selected`]) and how many ran ([`Tally::ran`]); the rule
//! that a run of none is refused is `blaze_cargo::cli::test::refusal`, which reads this.

use alloc::{format, string::String};

use serde::{Deserialize, Serialize};

use crate::text;

/// The counts of one test executable, or of a whole run (their sum, [`Tally::add`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Tally {
    /// The test executables that were started.
    pub binaries: u32,
    pub passed: u32,
    /// The tests that failed, and each executable that ended badly outside any test.
    pub failed: u32,
    /// Selected and not run: `#[ignore]`.
    pub ignored: u32,
    /// Benchmarks measured (`blaze bench`).
    pub measured: u32,
    /// The tests the filter left out, as libtest counts them.
    pub filtered_out: u32,
    /// Selected and not run because the edit did not reach them (`--reached`).
    pub unreached: u32,
    /// The test executables that never built, so never ran.
    pub unbuilt: u32,
}

impl Tally {
    /// The tests the selection held: every one that ran, and every one it kept and did not run.
    #[must_use]
    pub const fn selected(&self) -> u32 {
        self.ran().saturating_add(self.ignored).saturating_add(self.unreached)
    }

    /// The tests that ran: the only ones a pass says anything about.
    #[must_use]
    pub const fn ran(&self) -> u32 {
        self.passed.saturating_add(self.failed).saturating_add(self.measured)
    }

    /// `other`'s counts added to this one's.
    pub fn add(&mut self, other: &Tally) {
        self.binaries = self.binaries.saturating_add(other.binaries);
        self.passed = self.passed.saturating_add(other.passed);
        self.failed = self.failed.saturating_add(other.failed);
        self.ignored = self.ignored.saturating_add(other.ignored);
        self.measured = self.measured.saturating_add(other.measured);
        self.filtered_out = self.filtered_out.saturating_add(other.filtered_out);
        self.unreached = self.unreached.saturating_add(other.unreached);
        self.unbuilt = self.unbuilt.saturating_add(other.unbuilt);
    }

    /// `8 of 9 ran`: how many of the selected tests ran, the first words of the summary line.
    #[must_use]
    pub fn ran_of(&self) -> String {
        format!("{} of {} ran", self.ran(), self.selected()) // alloc: the summary, once
    }

    /// The counts as `key=value` fields, every one always there and in one order:
    /// `selected=9 ran=8 passed=7 failed=1 ignored=1 measured=0 filtered_out=0 unreached=0 binaries=3 unbuilt=0`.
    #[must_use]
    pub fn fields(&self) -> String {
        format!(
            "selected={} ran={} passed={} failed={} ignored={} measured={} filtered_out={} unreached={} \
             binaries={} unbuilt={}",
            self.selected(),
            self.ran(),
            self.passed,
            self.failed,
            self.ignored,
            self.measured,
            self.filtered_out,
            self.unreached,
            self.binaries,
            self.unbuilt
        ) // alloc: the run's line, once
    }
}

/// Off a terminal, a test run's end, one whole line after the test executables' own output:
/// `blaze: tests ok selected=9 ran=9 passed=8 failed=0 ignored=1 measured=0 filtered_out=0 unreached=0 binaries=3 unbuilt=0 elapsed_ms=31800`,
/// or `blaze: tests failed ... why="no test ran"`. `ok` is the command's verdict (its exit
/// code is 0), never worked out again here; `why` is what refused a run in which no test
/// failed.
#[must_use]
pub fn tests_line(tally: &Tally, ok: bool, why: Option<&str>, elapsed_ms: u64) -> String {
    let word = match ok {
        true => "ok",
        false => "failed",
    };
    let mut line = format!("blaze: tests {word} {} elapsed_ms={elapsed_ms}", tally.fields()); // alloc: the run's line, once
    if let Some(why) = why {
        line.push_str(&format!(" why={}", text::value(why))); // alloc: as above
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: every line a run's end is said by carries how many tests were selected
    /// and how many ran, so a run of nothing cannot read as a pass of something.
    #[test]
    fn the_line_always_says_how_many_were_selected_and_ran() {
        let none = Tally { binaries: 3, filtered_out: 312, ..Tally::default() };
        assert_eq!((none.selected(), none.ran()), (0, 0));
        assert_eq!(
            tests_line(&none, false, Some("no test ran"), 12),
            "blaze: tests failed selected=0 ran=0 passed=0 failed=0 ignored=0 measured=0 filtered_out=312 \
             unreached=0 binaries=3 unbuilt=0 elapsed_ms=12 why=\"no test ran\""
        );
        let some = Tally { binaries: 3, passed: 7, failed: 1, ignored: 1, ..Tally::default() };
        assert_eq!((some.selected(), some.ran()), (9, 8));
        assert_eq!(some.ran_of(), "8 of 9 ran");
        assert_eq!(
            tests_line(&some, false, None, 31_800),
            "blaze: tests failed selected=9 ran=8 passed=7 failed=1 ignored=1 measured=0 filtered_out=0 \
             unreached=0 binaries=3 unbuilt=0 elapsed_ms=31800"
        );
    }

    /// A test that is selected and not run (ignored, unreached) is selected and is not a run.
    #[test]
    fn what_is_kept_and_not_run_is_selected_and_not_ran() {
        let tally = Tally { ignored: 2, unreached: 5, measured: 1, ..Tally::default() };
        assert_eq!((tally.selected(), tally.ran()), (8, 1));
        let mut sum = tally;
        sum.add(&Tally { binaries: 1, passed: 4, filtered_out: 9, unbuilt: 1, ..Tally::default() });
        assert_eq!(
            sum,
            Tally {
                binaries: 1,
                passed: 4,
                failed: 0,
                ignored: 2,
                measured: 1,
                filtered_out: 9,
                unreached: 5,
                unbuilt: 1
            }
        );
    }
}
