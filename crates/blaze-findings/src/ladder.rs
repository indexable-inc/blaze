//! The adoption ladder: how a plain Rust project takes blaze's checks one step at a time.
//!
//! One manifest setting picks the step: `step = "suggest"` in `[workspace.metadata.blaze.lints]`
//! (a reserved key there, as `baselines` is in `lints-ratchet`). Absent, the step is `rust`.
//! Group and lint levels written in the same table apply on top of the step's, as they do today.
//!
//! - `rust`: only `blaze::general` at warn: what is wrong in any Rust program.
//! - `suggest`: and `blaze::suggest` at warn: advice, each finding saying what adopting a blaze
//!   feature or a tighter type gains (a cache hit, a proof a caller can rely on, a narrower
//!   type), with an autofix where adding it is mechanical.
//! - `edition`: and `blaze::edition` at warn: blaze's own language and std, used directly.
//! - `strict`: what blaze's own workspace holds itself to (its `lints-ratchet`): every group at
//!   deny, held by the ratchet's baselines.
//!
//! Moving up a step baselines the findings the new step shows, per lint and crate (the ratchet:
//! a crate never gets worse, and each finding fixed lowers its count). A project never has to
//! take a step, and nothing in a lower step changes what the program means: a lint only reports.
//!
//! [`report`] is what `blaze adopt` (and `blaze doctor`) prints for the next step: findings by
//! lint, how many have an automatic fix, and the crates in order of least work by hand.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use crate::view::Level;

/// The manifest key of the step, in `[workspace.metadata.blaze.lints]`.
pub const SETTING: &str = "step";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum Step {
    #[default]
    Rust,
    Suggest,
    Edition,
    Strict,
}

impl Step {
    pub const ALL: [Step; 4] = [Step::Rust, Step::Suggest, Step::Edition, Step::Strict];

    pub fn name(self) -> &'static str {
        match self {
            Step::Rust => "rust",
            Step::Suggest => "suggest",
            Step::Edition => "edition",
            Step::Strict => "strict",
        }
    }

    /// The step of a manifest value; `None` names no step (a typed refusal for the manifest
    /// reader, naming the value and the four steps).
    pub fn named(name: &str) -> Option<Step> {
        Step::ALL.into_iter().find(|step| step.name() == name)
    }

    pub fn next(self) -> Option<Step> {
        match self {
            Step::Rust => Some(Step::Suggest),
            Step::Suggest => Some(Step::Edition),
            Step::Edition => Some(Step::Strict),
            Step::Strict => None,
        }
    }

    /// The group levels of the step, as `blaze::<group>` names, in the order they apply.
    pub fn groups(self) -> &'static [(&'static str, Level)] {
        match self {
            Step::Rust => &[("general", Level::Warn)],
            Step::Suggest => &[("general", Level::Warn), ("suggest", Level::Warn)],
            Step::Edition => &[("general", Level::Warn), ("suggest", Level::Warn), ("edition", Level::Warn)],
            Step::Strict => &[
                ("general", Level::Deny),
                ("suggest", Level::Warn),
                ("internal", Level::Deny),
                ("edition", Level::Deny),
                ("pedantic_perf", Level::Deny),
                ("reach", Level::Deny),
                ("proc_macro", Level::Deny),
            ],
        }
    }
}

/// One finding the next step would show, as the report counts it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shown<'a> {
    pub krate: &'a str,
    /// The lint's full name.
    pub lint: &'a str,
    /// It has a machine-applicable fix.
    pub fixable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ByLint<'a> {
    pub lint: &'a str,
    pub findings: u32,
    pub fixable: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ByCrate<'a> {
    pub krate: &'a str,
    /// Findings with no automatic fix: the work by hand.
    pub by_hand: u32,
    pub findings: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Report<'a> {
    pub next: Step,
    /// By lint name.
    pub by_lint: Vec<ByLint<'a>, Global>,
    /// Least work by hand first, then fewest findings, then by name.
    pub crates: Vec<ByCrate<'a>, Global>,
}

impl Report<'_> {
    pub fn fixable(&self) -> u32 {
        self.by_lint.iter().map(|lint| lint.fixable).sum()
    }

    pub fn findings(&self) -> u32 {
        self.by_lint.iter().map(|lint| lint.findings).sum()
    }
}

pub fn report<'a>(next: Step, shown: &[Shown<'a>]) -> Report<'a> {
    let mut by_lint: Vec<ByLint<'a>, Global> = Vec::new_in(Global);
    let mut crates: Vec<ByCrate<'a>, Global> = Vec::new_in(Global);
    for finding in shown {
        match by_lint.binary_search_by(|row| row.lint.cmp(finding.lint)) {
            Ok(at) => {
                by_lint[at].findings += 1;
                by_lint[at].fixable += u32::from(finding.fixable);
            }
            Err(at) => by_lint.insert(at, ByLint { lint: finding.lint, findings: 1, fixable: u32::from(finding.fixable) }),
        }
        match crates.binary_search_by(|row| row.krate.cmp(finding.krate)) {
            Ok(at) => {
                crates[at].findings += 1;
                crates[at].by_hand += u32::from(!finding.fixable);
            }
            Err(at) => crates.insert(at, ByCrate { krate: finding.krate, by_hand: u32::from(!finding.fixable), findings: 1 }),
        }
    }
    crates.sort_by(|a, b| (a.by_hand, a.findings, a.krate).cmp(&(b.by_hand, b.findings, b.krate)));
    Report { next, by_lint, crates }
}

/// The ratchet's baseline a step-up writes: per (lint, crate), the count, in the layout
/// `blaze_lints::baseline` reads (`<tool>.<lint>.txt`, one `crate<TAB>count` line each). Sorted
/// by lint, then crate.
pub fn baseline<'a>(shown: &[Shown<'a>]) -> Vec<(&'a str, &'a str, u32), Global> {
    let mut rows: Vec<(&'a str, &'a str, u32), Global> = Vec::new_in(Global);
    for finding in shown {
        match rows.binary_search_by(|row| (row.0, row.1).cmp(&(finding.lint, finding.krate))) {
            Ok(at) => rows[at].2 += 1,
            Err(at) => rows.insert(at, (finding.lint, finding.krate, 1)),
        }
    }
    rows
}
