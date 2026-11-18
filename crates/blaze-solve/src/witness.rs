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
use crate::term::Def;

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
