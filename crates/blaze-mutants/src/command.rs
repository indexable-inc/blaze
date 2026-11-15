//! The entry points the CLI calls: `blaze mutants` (the change since the base),
//! `blaze mutants <path>` (one function) and `blaze mutants --crate <name> --budget <secs>`.
//!
//! Each makes a plan and returns the session; the driver runs it and hands the outcome to
//! [`summarize`]. A session never fails quietly: a plan error names the item, a mutant whose
//! effect failed is listed in [`Summary::errors`], and what the budget did not reach is counted
//! in [`Summary::not_run`] so the person knows the run was partial.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_keyed::machine::Machine;
use blaze_program::key::ItemId;

use crate::Plant;
use crate::report::{Diagnostic, ReportError, Score, diagnostics, scores};
use crate::run::{MutantError, Outcome, Session};
use crate::select::{Change, Config, MutantRef, Plan, PlanError, Scope, Skipped, Universe, plan};
use crate::verdict::Verdict;

/// What a person or an agent asked for.
#[derive(Clone, Copy, Debug)]
pub enum Request<'a> {
    /// The functions whose body key moved since the base.
    Change(&'a Change),
    /// One function, every mutant of it.
    Function(ItemId),
    /// Every function of a crate, within `wall_ns`.
    Crate { items: &'a [ItemId], wall_ns: u64 },
}

/// The plan of `request`, and a session of it started at `start`.
pub fn mutants(
    request: &Request<'_>,
    universe: &Universe<'_>,
    machine: &Machine,
    start: u64,
    plant: Plant,
) -> Result<(Session, PlanSummary), PlanError> {
    let (scope, config) = match *request {
        Request::Change(change) => (Scope::Change(change), Config::change(machine)),
        Request::Function(item) => (Scope::Function(item), Config::function(machine)),
        Request::Crate { items, wall_ns } => (Scope::Items(items), Config::whole(machine, wall_ns)),
    };
    let plan: Plan = plan(&scope, universe, &config, plant)?;
    let summary = PlanSummary {
        candidates: plan.candidates.len() as u32,
        decided: plan.decided.len() as u32,
        arid: plan.arid.clone(),
    };
    Ok((Session::new(plan, start), summary))
}

/// What the plan decided before anything ran.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlanSummary {
    pub candidates: u32,
    pub decided: u32,
    pub arid: Vec<Skipped, Global>,
}

/// What a session found, for printing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Summary {
    pub diagnostics: Vec<Diagnostic, Global>,
    pub scores: Vec<(ItemId, Score), Global>,
    pub total: Score,
    /// Mutants the budget did not reach: the run was partial when this is not zero.
    pub not_run: Vec<MutantRef, Global>,
    pub errors: Vec<(MutantRef, MutantError), Global>,
}

/// The diagnostics and scores of a finished session.
pub fn summarize(outcome: &Outcome, plan: &PlanSummary, universe: &Universe<'_>) -> Result<Summary, ReportError> {
    let diagnostics = diagnostics(outcome, universe)?;
    let (scores, total) = scores(outcome, &plan.arid);
    let mut not_run = Vec::new_in(Global);
    for (at, verdict, _) in &outcome.verdicts {
        if matches!(verdict, Verdict::NotRun(_)) {
            not_run.push(*at);
        }
    }
    Ok(Summary { diagnostics, scores, total, not_run, errors: outcome.errors.clone() })
}
