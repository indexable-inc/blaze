//! Mutation testing scoped to the change, selected by reach, budgeted in seconds, reported like a
//! lint.
//!
//! # Why, now
//!
//! Code is now cheap to write and expensive to trust. An LLM writes tests that run the code and
//! assert little: a part of the result, `is_ok()`, a length. Line coverage cannot tell such a test
//! from a good one: both execute every line. Mutation testing measures the one thing that
//! matters, whether the tests would notice if the behaviour changed. And a surviving mutant is a
//! precise, small task for an agent: "no test distinguishes `<` from `<=` at this line: write one
//! that does". It is the automated form of the planted-bug controls this tree's simulations
//! already carry (`blaze_keyed::Plant`, `blaze_program::Plant`): a hand-written mutant of the
//! code under test whose oracle must catch it.
//!
//! What made mutation testing impractical was cost (every mutant times the whole suite) and noise
//! (equivalent mutants, and mutants in code nobody wants pinned). Google's practice answers both:
//! mutants only on changed lines, shown at review, a small number per change, with "arid" nodes
//! such as logging suppressed. Developers there first judged 85% of reported mutants
//! unproductive; the arid-node heuristics, tuned on feedback from more than 20,000 developers,
//! brought that to 11% (productive from 15% to 89%), most of the gain from suppressing logging,
//! time and configuration-flag code. Sources: Petrović and Ivanković, "State of Mutation Testing
//! at Google" (ICSE-SEIP 2018); Petrović, Ivanković, Fraser and Just, "Practical Mutation Testing
//! at Scale: A view from Google" (IEEE TSE 2021, arXiv 2102.11378); and "Does Mutation Testing
//! Improve Testing Practices?" (ICSE 2021), which found that developers exposed to mutants write
//! more and stronger tests over time.
//!
//! blaze's design removes the cost. A mutant is a replaced instance code record: one function's
//! MIR rewritten after type checking (so it compiles by construction) and made at the fast tier
//! in milliseconds. A program is a manifest patched in place, so a mutant program is one
//! placement delta, never a link. A test result is a record keyed by what the test reaches
//! (`blaze_program::reach::test_key`), so only the tests whose reach includes the mutated
//! function run, cheapest-looking first, stopping at the first that fails.
//!
//! # The floor, and the cost against it
//!
//! Per mutant the floor is one fast-tier codegen of the function (and of each instance that
//! inlined it at the tier under test), one placement patch per program that holds it, and the
//! reaching tests until one fails (all of them for a survivor): nothing can do less and still
//! know. This crate adds the verdict lookup (one batched store read for the whole session), a
//! put per verdict, and planning that is linear in the sites of the functions in scope plus a
//! sort. Its cost is the floor plus those, which is within 10% whenever a mutant's tests take a
//! millisecond or more. What it cannot reach: a survivor runs every reaching test, by definition;
//! and test order by reach size is a proxy for cheapest-first, chosen because recorded times are
//! machine-dependent and the verdict record must not be (rule 25).
//!
//! # The modules
//!
//! - [`site`]: what the front end says of one function: its mutation sites, typed, with the role
//!   each plays (from the typed tree, never from text); and the tests with their reach.
//! - [`operator`]: the operators and the concrete [`operator::Mutation`]s a site admits, each
//!   type-correct by construction.
//! - [`arid`]: what is not mutated, by rule, and what is equivalent by construction.
//! - [`select`]: the planner: which mutants for a change, a function or a crate, in which order,
//!   with which tests, seeded by the change so reruns agree.
//! - [`run`]: the sans-IO session that runs a plan within a hard deadline.
//! - [`verdict`]: verdicts as records, keyed by body, site, mutation and the reaching tests.
//! - [`report`]: surviving mutants as diagnostics, and scores.
//! - [`command`]: the entry points the CLI calls.

#![feature(allocator_api, btreemap_alloc)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod arid;
pub mod command;
pub mod operator;
pub mod report;
pub mod run;
pub mod select;
pub mod site;
pub mod verdict;

#[cfg(test)]
mod tests;

/// A bug planted on purpose, so a test shows its oracle catches it (rule 43). `None` in every
/// real use. Each names the place it is planted in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// [`select::plan`] adds a test that does not reach the mutated function to its run list.
    RunsUnreachingTests,
    /// [`verdict::MutantVerdict`]'s key leaves out the reaching tests' keys: a verdict found
    /// again after a test changed.
    VerdictKeyWithoutTests,
    /// [`arid::arid`] calls a comparison in behaviour code logging: a real mutant swallowed.
    AridSwallowsReal,
}
