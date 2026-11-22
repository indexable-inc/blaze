//! Every view of a command's findings is a query over them (D164): the default view groups the
//! findings of a [`Kind::Grouped`] rule a line a cause key ([`groups`]), `--each` says each
//! ([`each`]), the tally counts them by kind ([`Counts`]), and the exit status is a function of
//! that tally ([`outcome`]). Grouping is written here once; the printers only draw.

use core::fmt;

use crate::finding::{CauseKey, Finding, SayError};
use crate::frame::words::{self, Area, Kind, Policy, WordsError};

/// How many subjects a group's line names before "(and N more)" (blaze-cli not_checked.rs
/// `CRATES_SHOWN` is this).
pub const SHOWN: usize = 5;

/// The rule of a group's line ([`Group::line`]).
pub const GROUP_RULE: u16 = 45;

/// `findings` in the order every view reads them: by cause key (its words first, rule 25), then
/// by subject. Findings of no cause come first, by subject.
pub fn sort(findings: &mut [Finding<'_>]) {
    findings.sort_unstable_by(|a, b| (a.key(), a.subject).cmp(&(b.key(), b.subject)));
}

/// The findings of one cause key, sorted ([`sort`]).
#[derive(Clone, Copy, Debug)]
pub struct Group<'s, 'a> {
    pub key: CauseKey<'a>,
    pub findings: &'s [Finding<'a>],
}

impl Group<'_, '_> {
    /// How many subjects it holds: a subject found twice (a body held by two units) once.
    pub fn count(&self) -> usize {
        self.findings.iter().enumerate().filter(|(i, one)| *i == 0 || self.findings[i - 1].subject != one.subject).count()
    }

    /// Its line of the default view, by [`GROUP_RULE`]'s words: "2 bodies not checked because
    /// <the cause>: `here` (src/lib.rs), `there` (src/lib.rs)", the first [`SHOWN`] subjects
    /// named and ", .. (and N more)" past them.
    pub fn line(&self, out: &mut dyn fmt::Write) -> Result<(), SayError> {
        let row = words::wording(Area::Cli, GROUP_RULE).ok_or(SayError::Words(WordsError::NoWording { area: Area::Cli, number: GROUP_RULE }))?;
        if row.args != 4 {
            return Err(SayError::Words(WordsError::Arguments { name: row.name, given: 4, takes: row.args }));
        }
        let count = self.count();
        let Some(first) = self.findings.first() else {
            return Ok(());
        };
        let noun = first.subject.noun(count);
        let findings = self.findings;
        let of = self.key.of;
        words::fill_with(out, row.headline, 4, &mut |n: usize, out: &mut dyn fmt::Write| match n {
            0 => write!(out, "{count}"),
            1 => out.write_str(noun),
            2 => out.write_str(of),
            _ => {
                let mut named = 0usize;
                for (i, one) in findings.iter().enumerate() {
                    if (i > 0 && findings[i - 1].subject == one.subject) || named == SHOWN {
                        continue;
                    }
                    if named > 0 {
                        out.write_str(", ")?;
                    }
                    one.subject.named(out)?;
                    named += 1;
                }
                if count > named {
                    write!(out, ", .. (and {} more)", count - named)?;
                }
                Ok(())
            }
        })
        .map_err(|_| SayError::Write)
    }
}

/// The groups of a sorted slice ([`sort`]), in order: every finding with a cause, a group a key.
pub fn groups<'s, 'a>(findings: &'s [Finding<'a>]) -> impl Iterator<Item = Group<'s, 'a>> {
    let mut at = 0;
    core::iter::from_fn(move || {
        while at < findings.len() {
            let Some(key) = findings[at].key() else {
                at += 1;
                continue;
            };
            let end = findings[at..].iter().position(|one| one.key() != Some(key)).map_or(findings.len(), |n| at + n);
            let group = Group { key, findings: &findings[at..end] };
            at = end;
            return Some(group);
        }
        None
    })
}

/// `finding` as `--each` says it: its own words.
pub fn each(finding: &Finding<'_>, out: &mut dyn fmt::Write) -> Result<(), SayError> {
    finding.words(out)
}

/// Whether the default view draws `kind` a line a finding (`--each` draws every kind so).
pub fn drawn_alone(kind: Kind) -> bool {
    kind != Kind::Grouped
}

/// The tally of a command's findings by what they make the exit status.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Counts {
    /// Errors of the program.
    pub errors: u64,
    /// Tasks that failed on their own.
    pub failures: u64,
    /// Items not checked, or not checked whole.
    pub unchecked: u64,
}

impl Counts {
    /// One finding of `kind` counted under `policy`.
    pub fn add(&mut self, kind: Kind, policy: Policy) {
        match kind {
            Kind::Error => self.errors += 1,
            Kind::Borrows if policy.borrows_are_errors => self.errors += 1,
            Kind::Failure => self.failures += 1,
            Kind::Unchecked | Kind::Grouped => self.unchecked += 1,
            Kind::Warning | Kind::BorrowsUnchecked | Kind::Borrows | Kind::Waited | Kind::Told => {}
        }
    }
}

/// What a command's findings make of its exit: the worst that holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Every item checked, nothing found that fails.
    Ok,
    /// An error of the program, or a task that failed.
    Failed,
    /// No error; an item was not checked (or not checked whole).
    ItemsNotChecked,
    /// No error and every item reached checked; the answer is not whole (a stage not run, a
    /// line not printed).
    Incomplete,
}

/// The exit's outcome: a function of the tally and of whether the answer is whole.
pub fn outcome(counts: &Counts, incomplete: bool) -> Outcome {
    if counts.errors > 0 || counts.failures > 0 {
        Outcome::Failed
    } else if counts.unchecked > 0 {
        Outcome::ItemsNotChecked
    } else if incomplete {
        Outcome::Incomplete
    } else {
        Outcome::Ok
    }
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
