//! blaze's MIR to Lean 4: v0 of docs/v2/plan/design/formal-verification.md, safe sequential
//! Rust by functional translation, the way Charon and Aeneas do it and without either.
//!
//! The pipeline, one module per step:
//!
//! 1. [`structured`]: a function's built, borrow-checked, unoptimized MIR
//!    (`HirDatabase::mir_body`) as structured MIR, [`body::Body`]: `if`, `match`, `loop`,
//!    `break`, `continue` and `return` reconstructed from the control-flow graph
//!    ([`structured::reconstruct`]), each check folded into the operation it guards.
//! 2. [`translate`]: the model, a Lean function over the library in `lean/Blaze`
//!    ([`translate::translate`]); [`spec`]: the function's `requires` and `ensures` as Lean
//!    propositions over the model.
//! 3. [`emit`]: one Lean module per function ([`lean::Module`]): the types, the model, and
//!    `def <fn>.spec.statement : Prop`, the theorem its clauses state. It imports the library
//!    and the modules of the functions it calls, nothing else. Emitting twice gives the same
//!    bytes.
//! 4. [`check`]: the pinned Lean ([`LEAN_VERSION`], from `BLAZE_LEAN_PROVER`) compiles the
//!    library, the models and the crate's proof file, then a check module that states
//!    `theorem <fn>.spec : <fn>.spec.statement := <fn>.spec.proof` and prints its axioms. The
//!    proof file names the statement and never restates it, so a statement that changed is
//!    an elaboration error naming the theorem. Proved means: Lean accepts it, and its axioms
//!    are among [`ALLOWED_AXIOMS`] (so no `sorry`, no native evaluation).
//!
//! What v0 does not translate is a [`NotTranslated`] naming the construct and where it is:
//! never a `sorry`, an axiom or an approximation.
//!
//! The translator runs once per `blaze check`, off every hot path, and its trees are small:
//! it allocates on the global heap throughout.

pub mod body;
mod driver;
pub mod lean;
mod spec;
pub mod structured;
#[cfg(test)]
mod tests;
pub mod translate;

use std::{collections::BTreeMap, fmt};

use base_db::Db;
use hir_def::{DefWithBodyId, FunctionId};
use hir_ty::InferenceResult;

pub use driver::{
    ALLOWED_AXIOMS, Clause, Config, LEAN_VERSION, LeanError, NotProved, ProofFile, ProofKey,
    Proofs, Proved, Prover, check, check_emitted, proof_key, proved,
};
pub use spec::Spec;

use lean::{Binder, Decl, Infix, Module, Pat, Term, app, atom, infix, name};
use translate::{Model, Shape};

/// The translator's version: part of every proof key. Raised with any change to what is
/// emitted.
pub const TRANSLATOR: &str = "hir-ty-lean v0";

/// The Lean library the models are written against (`lean/Blaze`), as built into blaze: each
/// module and its text, in import order. Part of every proof key.
pub const LIBRARY: &[(&str, &str)] = &[
    ("Blaze.Result", include_str!("../../../lean/Blaze/Result.lean")),
    ("Blaze.Scalar", include_str!("../../../lean/Blaze/Scalar.lean")),
    ("Blaze.Slice", include_str!("../../../lean/Blaze/Slice.lean")),
    ("Blaze.Std", include_str!("../../../lean/Blaze/Std.lean")),
    ("Blaze", include_str!("../../../lean/Blaze.lean")),
];

/// A construct v0 does not translate: the function, the construct and where it is. The plan's
/// rule names (`Unsafe`, `RawPointer`, `NoModel`, ..) are in `what`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotTranslated {
    /// The function, `krate::module::name`.
    pub function: String,
    /// The construct ("a raw pointer (rule RawPointer)", "a closure").
    pub what: String,
    /// Where: the expression, printed from HIR, or `fn <path>` for the function as a whole.
    pub at: String,
}

impl fmt::Display for NotTranslated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not translated to Lean: {} at `{}` (v0 translates safe, sequential Rust; \
             docs/v2/plan/design/formal-verification.md)",
            self.function, self.what, self.at
        )
    }
}

impl std::error::Error for NotTranslated {}

/// A function's Lean module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emitted {
    /// The function, `krate::module::name`.
    pub function: String,
    /// The model's Lean name, `krate.module.name`.
    pub name: String,
    /// The module, `BlazeModel.krate.module.name`; its file is the name with `/` for `.`.
    pub module: String,
    pub text: String,
    /// `<name>.spec.statement`, when the function has clauses.
    pub statement: Option<String>,
    /// The clauses the statement states, as written.
    pub clauses: Vec<String>,
    pub shape: Shape,
    /// The modules of the functions it calls, each after those it calls.
    pub imports: Vec<Emitted>,
}

impl Emitted {
    /// The module's path under the directory generated modules are written in.
    pub fn path(&self) -> String {
        format!("{}.lean", self.module.replace('.', "/"))
    }
}

/// `func`'s structured MIR: pipeline step 1. Refused unless the function type-checks and
/// borrow-checks: the translation's reading of `&mut` rests on both.
///
/// It is computed from memoized queries (`mir_body`, `borrowck`, inference) and is not yet a
/// memoized query of its own.
pub fn structured(db: &Db, func: FunctionId) -> Result<body::Body, NotTranslated> {
    blaze_query::db::attach(db, || lowered(db, func)).map(|it| it.body)
}

fn lowered(db: &Db, func: FunctionId) -> Result<body::Lowered, NotTranslated> {
    let def = DefWithBodyId::FunctionId(func);
    let display = body::function_path(db, func).join("::");
    let refuse = |what: &str| NotTranslated {
        function: display.clone(),
        what: what.to_owned(),
        at: format!("fn {display}"),
    };
    let infer = InferenceResult::of(db, def);
    if infer.is_erroneous() || infer.has_type_mismatches() {
        return Err(refuse("a function that does not type-check"));
    }
    let checked = hir_ty_borrowck::borrowck(db, def);
    if !checked.diagnostics.is_empty() || checked.not_checked.is_some() || checked.skipped.is_some()
    {
        return Err(refuse("a function the borrow checker did not accept"));
    }
    body::from_mir(db, func)
}

/// `func`'s Lean module, and those of the functions it calls: pipeline steps 1 to 3.
pub fn emit(db: &Db, func: FunctionId) -> Result<Emitted, NotTranslated> {
    blaze_query::db::attach(db, || emit_in(db, func, &mut Vec::new()))
}

fn emit_in(db: &Db, func: FunctionId, stack: &mut Vec<FunctionId>) -> Result<Emitted, NotTranslated> {
    let lowered = lowered(db, func)?;
    let body = &lowered.body;
    if stack.contains(&func) {
        return Err(NotTranslated {
            function: body.display.clone(),
            what: "recursion".to_owned(),
            at: format!("fn {}", body.display),
        });
    }
    stack.push(func);
    let mut imports: Vec<Emitted> = Vec::new();
    let mut shapes = BTreeMap::new();
    for (callee_name, callee) in &lowered.callees {
        let callee = emit_in(db, *callee, stack)?;
        shapes.insert(callee_name.clone(), callee.shape.clone());
        // What the callee imports comes first; a module two callees share is listed once.
        for import in callee.imports.iter().chain([&callee]) {
            if !imports.iter().any(|it| it.module == import.module) {
                imports.push(import.clone());
            }
        }
    }
    stack.pop();
    let model = translate::translate(body, &shapes)?;
    let params: Vec<String> = model.params.iter().map(|it| it.name.clone()).collect();
    let spec = spec::lower(db, func, &params)?;
    Ok(module_of(body, model, spec.as_ref(), imports))
}

/// The module of `body`'s `model` and `spec`.
pub fn module_of(
    body: &body::Body,
    model: Model,
    spec: Option<&Spec>,
    imports: Vec<Emitted>,
) -> Emitted {
    let module = format!("BlazeModel.{}", body.name);
    let mut decls = model.decls.clone();
    let statement = spec.map(|spec| {
        let statement = format!("{}.spec.statement", body.name);
        decls.push(statement_of(&body.name, &model, spec, &statement));
        statement
    });
    let about = vec![
        format!(
            "Generated by blaze ({TRANSLATOR}) from `{}`: its model{}.",
            body.display,
            if spec.is_some() { " and the statement of its clauses" } else { "" }
        ),
        "Do not edit: a proof is written in the crate's `lean/` and names the statement."
            .to_owned(),
    ];
    let mut import_lines = vec!["Blaze".to_owned()];
    import_lines.extend(imports.iter().map(|it| lean::ident(&it.module)));
    let text = Module { about, imports: import_lines, decls }.print();
    Emitted {
        function: body.display.clone(),
        name: body.name.clone(),
        module,
        text,
        statement,
        clauses: spec.map(|it| it.clauses.clone()).unwrap_or_default(),
        shape: model.shape,
        imports,
    }
}

/// `def <statement> : Prop`: for all arguments (and all fuel) under the `requires`, the model
/// does not fail and what it gives meets the `ensures`. Running out of fuel is allowed: the
/// statement of a function with a loop is partial correctness.
fn statement_of(function: &str, model: &Model, spec: &Spec, statement: &str) -> Decl {
    let mut binders = Vec::new();
    let mut args = Vec::new();
    if model.shape.fuel {
        binders.push(Binder { name: "fuel".to_owned(), ty: name("Nat") });
        args.push(name("fuel"));
    }
    binders.extend(model.params.iter().cloned());
    args.extend(model.params.iter().map(|it| name(it.name.clone())));
    let call = app(function, args);
    let ensures = spec
        .ensures
        .iter()
        .cloned()
        .rev()
        .reduce(|rest, clause| infix(Infix::And, clause, rest))
        .unwrap_or_else(|| atom("True"));
    // What the model gives: the result (unless it is `()`), then each `&mut` value.
    let mut gives = Vec::new();
    if !model.shape.returns_unit {
        gives.push(Pat::Name(spec.result.clone()));
    }
    for at in &model.shape.mut_params {
        gives.push(Pat::Name(format!("{}'", model.params[*at].name)));
    }
    let given = match gives.len() {
        0 => Pat::Wild,
        1 => gives.remove(0),
        _ => Pat::Tuple(gives),
    };
    let conclusion = if model.shape.monadic {
        Term::Match(
            Box::new(call),
            vec![
                (Pat::Ctor("Result.ok".to_owned(), vec![given]), ensures),
                (Pat::Ctor("Result.fail".to_owned(), vec![Pat::Wild]), atom("False")),
                (
                    Pat::Ctor("Result.div".to_owned(), Vec::new()),
                    atom(if model.shape.fuel { "True" } else { "False" }),
                ),
            ],
        )
    } else {
        Term::Let(given, Box::new(call), Box::new(ensures))
    };
    let stated = spec
        .requires
        .iter()
        .cloned()
        .rev()
        .fold(conclusion, |rest, clause| infix(Infix::Implies, clause, rest));
    let body = if binders.is_empty() { stated } else { Term::Forall(binders, Box::new(stated)) };
    Decl::Def { name: statement.to_owned(), params: Vec::new(), ty: atom("Prop"), body }
}
