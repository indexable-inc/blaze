//! Formal verification of `#![feature(blaze_verify)]`'s native specification clauses
//! (docs/v2/plan/design/verus.md).
//!
//! A function's clauses (`requires`, `ensures`, loop `invariant` and `decreases`) are written in
//! Verus's syntax as blaze grammar, and hir-def lowers each clause to a ghost group at the head
//! of the block it is about (`hir_def::expr_store::SpecClauseKind`). This crate replaces Verus's
//! `rust_verify`: it lowers the function's HIR, typed by blaze's inference, into Verus's VIR
//! ([`lower`], a port of `rust_to_vir` rule by rule), runs VIR's own checks (well-formedness,
//! modes, resolution inference) and passes (simplify, prune, SST, poly), and proves the
//! function through AIR with Z3 ([`driver`], a port of `rust_verify`'s verifier loop).
//!
//! The answer is a [`Verdict`]: [`Outcome::Proved`], [`Outcome::Refuted`] naming the clause that
//! failed and a counterexample from the solver's model, [`Outcome::Unknown`] when the solver ran
//! out of its resource limit or gave up (never a pass), or [`Outcome::Unsupported`] naming the
//! construct this slice of the lowering does not take yet. The solver is deterministic: a pinned
//! Z3 ([`Z3_VERSION`], from `BLAZE_Z3`), fixed seeds and a resource limit in place of a timeout.
//!
//! Vendored from Verus (<https://github.com/verus-lang/verus>, MIT, commit [`VERUS_COMMIT`]):
//! `vir`, `air`, `vir_macros`, `builtin` and `vstd` under `vendor/verus` (`vendor/verus/SOURCE.md`).

mod commands;
mod driver;
mod lower;
#[cfg(test)]
mod tests;

use std::{fmt, path::PathBuf};

use base_db::{ContentId, Db};
use hir_def::{FunctionId, expr_store::SpecClauseKind};
use hir_ty::next_solver::StoredGenericArgs;

/// The Verus commit `vendor/verus` is taken from. Part of every [`ProofKey`].
pub const VERUS_COMMIT: &str = "3881d4494e035f1e8a649ede0f1e8fc4d56a19e7";

/// The Z3 version blaze proves with (`flake.nix` pins it and sets `BLAZE_Z3`). air refuses any
/// other: a proof is only as good as the solver it names. Part of every [`ProofKey`].
pub const Z3_VERSION: &str = "4.16.0";

/// Z3's `smt.random_seed` and `sat.random_seed`. Part of every [`ProofKey`].
pub const SEED: u32 = 0;

/// Z3 resource units per second of solving on a typical machine (Verus's
/// `RLIMIT_PER_SECOND`): [`Config::rlimit`] is a count of these, not a time.
pub const RLIMIT_PER_SECOND: u32 = 3_000_000;

/// The resource limit [`proved`] uses: about ten seconds of solving per query (Verus's default).
pub const DEFAULT_RLIMIT: u32 = 10 * RLIMIT_PER_SECOND;

/// How a function is proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The Z3 executable. Its version must be [`Z3_VERSION`].
    pub z3: PathBuf,
    /// Z3's resource limit per query, in Z3's resource units (`rlimit`). A query that needs more
    /// is [`Outcome::Unknown`], on every machine alike: this is not a timeout.
    pub rlimit: u32,
}

impl Config {
    /// Z3 from `BLAZE_Z3`, which `nix develop` sets to the pinned Z3. There is no other place
    /// it is looked for: no `PATH`, no `VERUS_Z3_PATH`.
    pub fn from_env(rlimit: u32) -> Result<Config, VerifyError> {
        match std::env::var_os("BLAZE_Z3") {
            Some(z3) => Ok(Config { z3: z3.into(), rlimit }),
            None => Err(VerifyError {
                function: String::new(),
                stage: "solver",
                message: format!(
                    "`BLAZE_Z3` is not set: blaze proves with Z3 {Z3_VERSION}, which `nix develop` \
                     pins and names there"
                ),
            }),
        }
    }
}

/// One clause of a function, as written.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClauseRef {
    /// The clause's keyword and whether it is on the function or on a loop.
    pub kind: SpecClauseKind,
    /// Which clause of the function, in source order (the group's index in
    /// `ExpressionStore::spec_clauses`).
    pub group: u32,
    /// Which of the clause's comma-separated expressions.
    pub item: u32,
    /// The expression, printed from HIR (no positions, so a key over it does not move with an
    /// edit elsewhere in the file).
    pub text: String,
}

impl fmt::Display for ClauseRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = match self.kind {
            SpecClauseKind::Invariant
            | SpecClauseKind::LoopEnsures
            | SpecClauseKind::LoopDecreases => " of a loop",
            _ => "",
        };
        write!(f, "`{} {}`{place}", self.kind.keyword(), self.text)
    }
}

/// What a failed or unknown query was about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Obligation {
    /// A clause: a function's `ensures`, a loop's `invariant` or `decreases`, a `requires` of a
    /// callee.
    Clause(ClauseRef),
    /// A check Verus puts in the code itself: an arithmetic overflow, a division by zero.
    Code {
        /// Verus's description of the check ("possible arithmetic underflow/overflow").
        what: String,
        /// The expression, printed from HIR.
        text: String,
    },
}

impl fmt::Display for Obligation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Obligation::Clause(clause) => write!(f, "{clause}"),
            Obligation::Code { what, text } => write!(f, "{what} in `{text}`"),
        }
    }
}

/// A value of a parameter in a counterexample.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    /// The solver's model gives this value (SMT-LIB syntax: `5`, `(- 3)`, `true`).
    Is(String),
    /// The model does not constrain it: any value fails the same way.
    Any,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Is(value) => write!(f, "{value}"),
            Value::Any => write!(f, "any"),
        }
    }
}

/// Values of the function's parameters under which the obligation fails.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Counterexample {
    /// Each parameter, in order, by its name in the source.
    pub params: Vec<(String, Value)>,
}

impl fmt::Display for Counterexample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (name, value)) in self.params.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{name} = {value}")?;
        }
        Ok(())
    }
}

/// An obligation the solver found a counterexample to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Refutation {
    /// The function, `crate::path::name`.
    pub function: String,
    /// What failed.
    pub obligation: Obligation,
    /// Verus's message ("postcondition not satisfied", "invariant not satisfied at end of loop
    /// body").
    pub message: String,
    /// The parameters' values in the solver's model.
    pub counterexample: Counterexample,
}

/// Why the solver could not decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnknownWhy {
    /// The query used up [`Config::rlimit`].
    Rlimit,
    /// Z3 reported its decision procedure incomplete for the query (a quantifier or a nonlinear
    /// fragment it does not decide); the model it gave is not a counterexample.
    Incomplete,
}

/// A query the solver neither proved nor refuted. Never a pass.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Unknown {
    /// The function, `crate::path::name`.
    pub function: String,
    /// What the query was about, when the solver said (it does not after running out of
    /// resources).
    pub obligation: Option<Obligation>,
    /// Why.
    pub why: UnknownWhy,
    /// The resource limit the query had.
    pub rlimit: u32,
    /// The resources Z3 reports it used, summed over the function's queries.
    pub rlimit_used: Option<u64>,
}

/// A construct this slice of the lowering does not take yet, by name and place. The plan
/// (docs/v2/plan/design/verus.md, "Ported, left, and the order") lists what comes next.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Unsupported {
    /// The function, `crate::path::name`.
    pub function: String,
    /// The construct ("a call", "a `while let` loop", "a type `&u32`").
    pub what: String,
    /// The expression or clause it is in, printed from HIR.
    pub at: String,
}

/// A step of the pipeline refused the function: a VIR check (well-formedness, modes), an AIR
/// type error, the solver failing to start or answering what air does not expect, a panic in
/// vendored code.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VerifyError {
    /// The function, `crate::path::name`.
    pub function: String,
    /// The step: "vir", "modes", "air", "solver", "panic".
    pub stage: &'static str,
    /// The step's message, with the expression it names.
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "verifying `{}` failed in {}: {}", self.function, self.stage, self.message)
    }
}

/// What proving a function found.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Every obligation of the function holds.
    Proved,
    /// At least one obligation fails; each failing one, earliest first.
    Refuted(Vec<Refutation>),
    /// No obligation failed, but one could not be decided.
    Unknown(Unknown),
    /// The lowering does not take a construct of the function yet.
    Unsupported(Unsupported),
    /// A step of the pipeline refused the function.
    Error(VerifyError),
}

/// The verdict on one function.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Verdict {
    /// The function, `crate::path::name`.
    pub function: String,
    /// What proving it found.
    pub outcome: Outcome,
    /// The proof's key, once the function is lowered (see [`ProofKey`]).
    pub key: Option<ProofKey>,
}

/// A clause of a function, for [`proved`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Clause {
    /// The function's `requires`, holding at every call site.
    Requires,
    /// The function's `ensures`, given its `requires`.
    Ensures,
}

/// The identity of a proof: a blake3 over everything the verdict depends on — the vendored
/// Verus commit, the Z3 version, the resource limit, the seed, and the function's VIR as
/// lowered (which takes in its clauses, its body and its types; callees' specs join it as calls
/// are lowered). A caller that optimises on a proved clause puts this in its own code identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProofKey(pub ContentId);

/// Proves `func`'s clauses with `config`.
pub fn verify_function(db: &Db, func: FunctionId, config: &Config) -> Verdict {
    let lowered = match blaze_query::db::attach(db, || lower::lower_function(db, func)) {
        Ok(it) => it,
        Err(lower::Refused::Unsupported(it)) => {
            return Verdict {
                function: it.function.clone(),
                outcome: Outcome::Unsupported(it),
                key: None,
            };
        }
        Err(lower::Refused::Error(it)) => {
            return Verdict {
                function: it.function.clone(),
                outcome: Outcome::Error(it),
                key: None,
            };
        }
    };
    let key = driver::proof_key(&lowered, config);
    let outcome = driver::prove(&lowered, config);
    Verdict { function: lowered.display.clone(), outcome, key: Some(key) }
}

/// Whether `clause` of `func<args>` is proved, and the key of that proof: the interface lane
/// contracts reads (docs/v2/plan/design/contracts.md, part 4). Only [`Outcome::Proved`] answers
/// `Some`; `Unknown` is never a proof.
///
/// This slice proves non-generic functions with Z3 from `BLAZE_Z3` and [`DEFAULT_RLIMIT`].
/// [`Clause::Requires`] needs every call site lowered, which this slice does not do yet, so it
/// is `None`.
pub fn proved(
    db: &Db,
    func: FunctionId,
    args: StoredGenericArgs,
    clause: Clause,
) -> Option<ProofKey> {
    if !args.as_ref().is_empty() {
        return None;
    }
    match clause {
        Clause::Requires => None,
        Clause::Ensures => {
            let config = Config::from_env(DEFAULT_RLIMIT).ok()?;
            let verdict = verify_function(db, func, &config);
            match verdict.outcome {
                Outcome::Proved => verdict.key,
                _ => None,
            }
        }
    }
}
