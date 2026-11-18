//! The derivation sink (certificates.md, "The emitter"; rule 78): ONE seam, the solver's type
//! parameter `E: Emit`. `NoDerivation` records nothing and compiles to nothing (`E::ON` is a
//! constant false at every branch that tests it); `Derivations` keeps, per canonical input the
//! solver framed, the candidate its merge chose when the answer was `Yes`. blaze-cert's emitter
//! reads those choices back and rebuilds each step with the checker's own rule functions, so a
//! row's shape is what the checker reads by construction.
//!
//! What is recorded is the CHOICE only: `(source, rule, def, n)` of the chosen candidate. No
//! arena handle, pointer or map order is kept, and the table is indexed by the solver's own
//! input ids, which never leave the solver: the certificate's bytes hold canonical goal words.
//!
//! Determinism: where several candidates' responses equal the merged one, the choice is the
//! least by [`rank`], then by the impl's key, then by `n`: a function of the candidates as a
//! set, not of the order they were assembled in (impls declared in another order give the
//! same choice).

use blaze_own::{Alloc, Vec};

use crate::assembly::{BuiltinSource, Cand, Rule, Source};
use crate::error::{Limit, SolveError};
use crate::term::{Certainty, Def, List, Pred, TyKind};
use crate::tys::Tys;

/// The most inputs a `Derivations` table holds a choice for (rule 18): past it a record is a
/// typed refusal (`Limit::Table`), never a dropped choice.
pub const MAX_CHOICES: usize = 1 << 20;

/// The chosen candidate of one goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub source: Source,
    pub rule: Rule,
    /// The candidate's definition field (`Cand::d`): the impl, the bound's alias item.
    pub def: Def,
    /// The candidate's index field (`Cand::n`): a bound's index.
    pub n: u32,
}

impl Choice {
    pub fn of(c: &Cand) -> Choice {
        Choice { source: c.source, rule: c.rule, def: c.d, n: c.n }
    }
}

/// The order ties are broken in: trivial builtins, then env clauses, alias bounds, object
/// bounds, other builtins, impls, the rest.
pub fn rank(c: &Choice) -> u8 {
    match c.source {
        Source::Builtin(BuiltinSource::Trivial) => 0,
        Source::ParamEnv { .. } => 1,
        Source::AliasBound { .. } => 2,
        Source::Builtin(BuiltinSource::Object | BuiltinSource::TraitUpcasting) => 3,
        Source::Builtin(BuiltinSource::Misc) => 4,
        Source::Impl(_) => 5,
        Source::CoherenceUnknowable => 6,
    }
}

/// The sort key of a choice (see the module doc).
fn key(c: &Choice) -> (u8, u128, u32) {
    (rank(c), c.def.0, c.n)
}

/// The sink the solver records its choices in.
pub trait Emit {
    /// Whether choices are recorded at all: `false` turns every recording branch into nothing.
    const ON: bool;
    /// Records `c` as input `input`'s choice; a later record of the same input replaces it (a
    /// cycle head's last rerun is its fixpoint).
    fn record(&mut self, input: u32, c: Choice) -> Result<(), SolveError>;
    /// Input `input`'s choice, if one was recorded.
    fn choice(&self, input: u32) -> Option<Choice>;
}

/// No derivations: the default strategy.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDerivation;

impl Emit for NoDerivation {
    const ON: bool = false;

    fn record(&mut self, _input: u32, _c: Choice) -> Result<(), SolveError> {
        Ok(())
    }

    fn choice(&self, _input: u32) -> Option<Choice> {
        None
    }
}

/// The choices of every framed goal answered `Yes`, by the solver's input id.
pub struct Derivations<'a> {
    rows: Vec<Option<Choice>, Alloc<'a>>,
    recorded: u64,
}

impl<'a> Derivations<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Derivations<'a> {
        Derivations { rows: Vec::new_in(alloc), recorded: 0 }
    }

    /// Choices recorded so far (each merge that chose, replacements counted).
    pub fn recorded(&self) -> u64 {
        self.recorded
    }
}

impl Emit for Derivations<'_> {
    const ON: bool = true;

    fn record(&mut self, input: u32, c: Choice) -> Result<(), SolveError> {
        let i = input as usize;
        if i >= MAX_CHOICES {
            return Err(SolveError::Limit(Limit::Table));
        }
        if self.rows.len() <= i {
            self.rows.resize(i + 1, None);
        }
        self.rows[i] = Some(c);
        self.recorded += 1;
        Ok(())
    }

    fn choice(&self, input: u32) -> Option<Choice> {
        self.rows.get(input as usize).copied().flatten()
    }
}

/// The least of `cands` by the module's order, or `None` when empty.
pub fn least(cands: impl Iterator<Item = Choice>) -> Option<Choice> {
    let mut best: Option<Choice> = None;
    for c in cands {
        best = match best {
            Some(b) if key(&b) <= key(&c) => Some(b),
            _ => Some(c),
        };
    }
    best
}

// THE TRAIT-GOAL TRACE: the other half of what a merge saw. `Derivations` keeps the candidate a
// `Yes` chose; a trace keeps, for the goals of ONE trait it is asked about, EVERY candidate a
// frame ran with what became of it (refused by its rule, a nested goal with no solution, or
// held with a certainty), the merge's verdict over those it kept, and each root evaluation of
// such a goal with its arguments before and after. It is what a test prints to see why a goal
// stayed ambiguous (a `Maybe` has no choice to record). Off unless asked
// (`Solver::trace_on`): the solver's `Option<Trace>` is `None`, and each site tests it once. It
// is a measurement only: no answer reads it, and a row past a cap is counted in `dropped`
// (never an error, so tracing cannot change what is solved).

/// The most distinct goals (canonical inputs) one trace follows, whatever it is asked.
pub const MAX_TRACED_GOALS: u32 = 64;
/// The most candidate rows one traced goal keeps, whatever it is asked.
pub const MAX_TRACED_CANDS: u32 = 256;
/// The most merge rows, and the most root rows, one trace keeps.
pub const MAX_TRACED_ROWS: usize = 1 << 12;
/// The arguments of a traced goal shown (the self type first).
pub const TRACED_ARGS: usize = 3;

/// What a trace follows: the goals of `trait_` (whose self type is of kind `self_kind`, one
/// level and resolved, when given), the first `goals` of them, `cands` candidates each (both
/// clamped to the `MAX_TRACED_*` caps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceAsk {
    pub trait_: Def,
    pub self_kind: Option<TyKind>,
    pub goals: u32,
    pub cands: u32,
}

impl TraceAsk {
    /// The goal's argument kinds when `p` is a goal this trace follows.
    pub fn matches<T: Tys>(&self, tys: &T, p: Pred) -> Option<[Option<TyKind>; TRACED_ARGS]> {
        let Pred::Trait { trait_, args, .. } = p else { return None };
        if trait_ != self.trait_ {
            return None;
        }
        let kinds = kinds_of(tys, args);
        match self.self_kind {
            Some(k) if kinds[0] != Some(k) => None,
            _ => Some(kinds),
        }
    }
}

/// The first [`TRACED_ARGS`] terms of `args`, each one level and resolved.
pub fn kinds_of<T: Tys>(tys: &T, args: List) -> [Option<TyKind>; TRACED_ARGS] {
    let mut out = [None; TRACED_ARGS];
    for (i, slot) in out.iter_mut().enumerate() {
        let i = i as u32;
        if i < args.len {
            *slot = Some(tys.kind(tys.shallow_resolve(tys.list_get(args, i))));
        }
    }
    out
}

/// Why a candidate's rule refused it, by the solver's counters around its step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// An impl's header against the goal's, before any import (prefilter.rs `header_may_match`).
    Prematch,
    /// An impl's header, imported with fresh arguments, did not relate with the goal's.
    Header,
    /// Any other refusal of the rule (an impl's polarity; a builtin's or a bound's rule).
    Rule,
}

/// What became of one candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Refused(Refusal),
    /// A nested goal of the candidate (an impl's where-clause) has no solution: that goal's
    /// trait (`None`: not a trait goal) and its self type's kind.
    NestedNo { trait_: Option<Def>, self_kind: Option<TyKind> },
    /// The candidate holds with `certainty`; `constrains`: its response binds the goal's
    /// variables (or has goals), so it is no identity.
    Held { certainty: Certainty, constrains: bool },
}

/// A goal the trace follows: its canonical input, its trait and argument kinds as its first
/// frame saw them, the frames run for it (a cycle head's reruns count), the candidate rows
/// kept and the rows past the cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracedGoal {
    pub input: u32,
    pub trait_: Def,
    pub args: [Option<TyKind>; TRACED_ARGS],
    pub frames: u32,
    pub cands: u32,
    pub cands_dropped: u32,
}

/// One candidate of a frame of a traced goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracedCand {
    pub input: u32,
    pub choice: Choice,
    pub fate: Fate,
}

/// A traced goal's merge: the candidates it kept after its filters (`trait_goals::merge`), and
/// its verdict: `None` no solution, else the merged response's certainty and whether it binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracedMerge {
    pub input: u32,
    pub kept: u32,
    pub verdict: Option<(Certainty, bool)>,
}

/// How a root evaluation was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answered {
    /// The ground fast path (a marker trait's kept answer).
    FastPath,
    /// A frame was run for it.
    Framed,
    /// The request's table of shared goals (share.rs).
    Shared,
    /// This solver's global cache.
    Cached,
}

/// One root evaluation (`Solver::evaluate_from`) of a goal the trace follows: its argument
/// kinds before and after (its answer applied), the answer (`None`: it stopped, a fact
/// missing or an error), and how it was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracedRoot {
    pub before: [Option<TyKind>; TRACED_ARGS],
    pub after: [Option<TyKind>; TRACED_ARGS],
    pub answer: Option<crate::eval::Answer>,
    pub how: Answered,
}

/// The rows of one trace (see the section's comment).
pub struct Trace<'a> {
    ask: TraceAsk,
    goals: Vec<TracedGoal, Alloc<'a>>,
    cands: Vec<TracedCand, Alloc<'a>>,
    merges: Vec<TracedMerge, Alloc<'a>>,
    roots: Vec<TracedRoot, Alloc<'a>>,
    dropped: u64,
}

impl<'a> Trace<'a> {
    pub fn new_in(ask: TraceAsk, alloc: Alloc<'a>) -> Trace<'a> {
        let ask = TraceAsk { goals: ask.goals.min(MAX_TRACED_GOALS), cands: ask.cands.min(MAX_TRACED_CANDS), ..ask };
        Trace { ask, goals: Vec::new_in(alloc), cands: Vec::new_in(alloc), merges: Vec::new_in(alloc), roots: Vec::new_in(alloc), dropped: 0 }
    }

    pub fn ask(&self) -> TraceAsk {
        self.ask
    }

    pub fn goals(&self) -> &[TracedGoal] {
        &self.goals
    }

    pub fn cands(&self) -> &[TracedCand] {
        &self.cands
    }

    pub fn merges(&self) -> &[TracedMerge] {
        &self.merges
    }

    pub fn roots(&self) -> &[TracedRoot] {
        &self.roots
    }

    /// Rows not kept: goals past `ask.goals`, merges and roots past [`MAX_TRACED_ROWS`]
    /// (candidates past a goal's cap are its `cands_dropped`).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The row of `input`, when the trace follows it.
    pub fn goal(&self, input: u32) -> Option<&TracedGoal> {
        self.goals.iter().find(|g| g.input == input)
    }

    /// A frame for `input` (a goal of `trait_` with these argument kinds) starts: followed if it
    /// is followed already or the trace has room for one more goal.
    pub(crate) fn admit(&mut self, input: u32, trait_: Def, args: [Option<TyKind>; TRACED_ARGS]) {
        if let Some(g) = self.goals.iter_mut().find(|g| g.input == input) {
            g.frames += 1;
            return;
        }
        if self.goals.len() as u32 >= self.ask.goals {
            self.dropped += 1;
            return;
        }
        self.goals.push(TracedGoal { input, trait_, args, frames: 1, cands: 0, cands_dropped: 0 });
    }

    /// Candidate `choice` of a frame for `input` met `fate` (nothing when `input` is not followed).
    pub(crate) fn cand(&mut self, input: u32, choice: Choice, fate: Fate) {
        let most = self.ask.cands;
        let Some(g) = self.goals.iter_mut().find(|g| g.input == input) else { return };
        if g.cands >= most {
            g.cands_dropped += 1;
            return;
        }
        g.cands += 1;
        self.cands.push(TracedCand { input, choice, fate });
    }

    /// A frame for `input` merged (nothing when `input` is not followed).
    pub(crate) fn merge(&mut self, input: u32, kept: u32, verdict: Option<(Certainty, bool)>) {
        if self.goal(input).is_none() {
            return;
        }
        if self.merges.len() >= MAX_TRACED_ROWS {
            self.dropped += 1;
            return;
        }
        self.merges.push(TracedMerge { input, kept, verdict });
    }

    pub(crate) fn root(&mut self, row: TracedRoot) {
        if self.roots.len() >= MAX_TRACED_ROWS {
            self.dropped += 1;
            return;
        }
        self.roots.push(row);
    }
}
