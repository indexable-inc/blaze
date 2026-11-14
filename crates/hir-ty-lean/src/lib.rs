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
//! 4. [`plan`]: the check of the proofs, a sans-IO core. From a crate's theorems and the text
//!    of its proof files, the files to write and the runs of the pinned Lean
//!    ([`LEAN_VERSION`]) to make: the library, the models, the proof files, and for each
//!    theorem a check module that states `theorem <fn>.spec.checked : <fn>.spec.statement :=
//!    <fn>.spec.proof` and prints its axioms. The proof file names the statement and never
//!    restates it, so a statement that changed is an elaboration error naming the theorem.
//!    Proved means: Lean accepts it, and its axioms are among [`ALLOWED_AXIOMS`] (so no
//!    `sorry`, no native evaluation). Who runs Lean is the plan's driver (the daemon, for
//!    `blaze prove`), in blaze's sandbox; nothing in this crate starts a process.
//!
//! Beside the functional translation, [`embed_mir`] writes a function's MIR itself as a term
//! of the deep embedding in `lean/Blaze/Mir` ([`body::embed`]): the executable semantics the
//! models, the passes and the evaluator are held to.
//!
//! What v0 does not translate is a [`NotTranslated`] naming the construct and where it is:
//! never a `sorry`, an axiom or an approximation.
//!
//! The translator runs once per `blaze check`, off every hot path, and its trees are small:
//! it allocates on the global heap throughout.

pub mod body;
pub mod lean;
pub mod plan;
mod spec;
pub mod structured;
#[cfg(test)]
mod tests;
pub mod translate;

use std::{collections::BTreeMap, fmt};

use base_db::Db;
use hir_def::{AttrDefId, DefWithBodyId, FunctionId, attrs::AttrFlags};
use hir_ty::InferenceResult;

pub use plan::{ALLOWED_AXIOMS, Clause, LEAN_VERSION, NotProved, ProofKey, Prover};
pub use body::embed::Embedded as EmbeddedMir;
pub use spec::Spec;

use lean::{Binder, Decl, Infix, Module, Pat, Term, app, atom, infix, name};
use body::Instance;
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
    ("Blaze.Vec", include_str!("../../../lean/Blaze/Vec.lean")),
    ("Blaze.ScalarOps", include_str!("../../../lean/Blaze/ScalarOps.lean")),
    ("Blaze.Ordering", include_str!("../../../lean/Blaze/Ordering.lean")),
    ("Blaze.Duration", include_str!("../../../lean/Blaze/Duration.lean")),
    ("Blaze.Str", include_str!("../../../lean/Blaze/Str.lean")),
    ("Blaze.Machine", include_str!("../../../lean/Blaze/Machine.lean")),
    ("Blaze", include_str!("../../../lean/Blaze.lean")),
    // What a proof file imports and no model does: the tactic a statement is tried with first.
    ("Blaze.Auto", include_str!("../../../lean/Blaze/Auto.lean")),
    // The `@[spec]`s of `Blaze.Vec`'s operations, which `Blaze.Auto` must come before.
    ("Blaze.VecAuto", include_str!("../../../lean/Blaze/VecAuto.lean")),
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
    blaze_query::db::attach(db, || lowered(db, Instance::of(db, func))).map(|it| it.body)
}

fn lowered<'db>(db: &'db Db, instance: Instance<'db>) -> Result<body::Lowered<'db>, NotTranslated> {
    accepted(db, instance.func)?;
    body::from_mir(db, instance)
}

/// Refused unless `func` type-checks and borrow-checks: every reading of its MIR rests on
/// both.
fn accepted(db: &Db, func: FunctionId) -> Result<(), NotTranslated> {
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
    Ok(())
}

/// `func`'s built MIR, embedded: a Lean module holding the `Blaze.Mir.Program` of the function
/// (index 0) and the functions it calls, which `lean/Blaze/Mir` gives a meaning. The semantics'
/// side of every check that a model, a pass or the evaluator agrees with what blaze compiles.
pub fn embed_mir(db: &Db, func: FunctionId) -> Result<EmbeddedMir, NotTranslated> {
    blaze_query::db::attach(db, || {
        accepted(db, func)?;
        let term = body::embed::program(db, func, None, None)?;
        Ok(body::embed::module(db, func, term))
    })
}

/// As [`embed_mir`], with `mir` as `func`'s own body: the body before or after a pass, for
/// the check that the pass did what its Lean specification does. The term alone.
pub fn embed_mir_body<'db>(
    db: &'db Db,
    func: FunctionId,
    mir: &hir_ty::mir::MirBody<'db>,
) -> Result<String, NotTranslated> {
    blaze_query::db::attach(db, || {
        accepted(db, func)?;
        body::embed::program(db, func, Some(mir), None)
    })
}

/// As [`embed_mir_body`], with the blocks `layout` reaches, in `layout`'s numbering: the body
/// after a pass written over the blocks its input reached. The encoder writes the blocks a
/// body reaches and numbers them as it meets them, so a pass that leaves a block unreachable
/// would otherwise be compared with its specification over two different sets of blocks.
pub fn embed_mir_body_as<'db>(
    db: &'db Db,
    func: FunctionId,
    mir: &hir_ty::mir::MirBody<'db>,
    layout: &hir_ty::mir::MirBody<'db>,
) -> Result<String, NotTranslated> {
    blaze_query::db::attach(db, || {
        accepted(db, func)?;
        body::embed::program(db, func, Some(mir), Some(layout))
    })
}

/// `func`'s Lean module, and those of the functions it calls: pipeline steps 1 to 3. A
/// function with type or const parameters is translated at each instance a function calls it
/// at, never alone: asked for alone, it is a [`NotTranslated`] that says so
/// ([`body::GENERIC_UNASKED`]), and a generic theorem function is refused.
pub fn emit(db: &Db, func: FunctionId) -> Result<Emitted, NotTranslated> {
    blaze_query::db::attach(db, || emit_in(db, Instance::of(db, func), &mut Vec::new()))
}

fn emit_in<'db>(
    db: &'db Db,
    instance: Instance<'db>,
    stack: &mut Vec<Instance<'db>>,
) -> Result<Emitted, NotTranslated> {
    let lowered = lowered(db, instance)?;
    let body = &lowered.body;
    let refuse = |what: String| NotTranslated {
        function: body.display.clone(),
        what,
        at: format!("fn {}", body.display),
    };
    if stack.contains(&instance) {
        return Err(refuse("recursion".to_owned()));
    }
    // A chain of instances is bounded: deeper is a function that calls itself at an ever larger
    // type, which no finite set of modules translates.
    let depth = stack.iter().chain([&instance]).filter(|it| it.args.is_some()).count();
    if depth > body::INSTANCE_DEPTH {
        return Err(refuse(format!(
            "a chain of instances deeper than {} (a generic function that calls itself, directly \
             or not, at an ever larger type)",
            body::INSTANCE_DEPTH
        )));
    }
    stack.push(instance);
    // The types it names first, each after those its fields name (`Body::adts`' order), then
    // the `DecidableEq` of each whose derived `==` it calls, in the same order.
    let mut imports: Vec<Emitted> = body.adts.iter().map(type_module).collect();
    imports.extend(body.adts.iter().filter(|it| lowered.derives.contains(&it.name)).map(eq_module));
    let mut shapes = BTreeMap::new();
    for (callee_name, callee) in &lowered.callees {
        let callee = emit_in(db, *callee, stack)?;
        shapes.insert(callee_name.clone(), callee.shape.clone());
        // What the callee imports comes first; a module two callees share is listed once.
        for import in callee.imports.iter().chain([&callee]) {
            match imports.iter().find(|it| it.module == import.module) {
                None => {
                    // Two modules whose files differ only by letter case are one file where
                    // the file system folds case (macOS's, by default).
                    if let Some(it) = imports
                        .iter()
                        .find(|it| it.module.eq_ignore_ascii_case(&import.module))
                    {
                        return Err(refuse(format!(
                            "two modules whose files differ only by letter case, `{}` and `{}`",
                            it.module, import.module
                        )));
                    }
                    imports.push(import.clone())
                }
                // One module, two texts: two items with one Lean name (methods of two traits
                // of one name from two modules, on one type; two instances whose arguments are
                // written alike). Neither may stand for the other.
                Some(it) if it.text != import.text => {
                    return Err(refuse(format!(
                        "two functions with one Lean name, `{}` (`{}` and `{}`)",
                        import.name, it.function, import.function
                    )));
                }
                Some(_) => {}
            }
        }
    }
    stack.pop();
    let func = instance.func;
    let model = translate::translate(body, &shapes)?;
    let params: Vec<String> = model.params.iter().map(|it| it.name.clone()).collect();
    let spec = spec::lower(db, func, &params)?;
    if instance.args.is_some() && spec.is_some() {
        return Err(refuse(
            "a generic function with `requires` or `ensures` clauses (its statement would be one \
             per instance, which is not made yet)"
                .to_owned(),
        ));
    }
    let theorem = AttrFlags::query(db, AttrDefId::FunctionId(func)).contains(AttrFlags::THEOREM);
    let stated = match (theorem, spec.as_ref()) {
        (false, None) => None,
        (false, Some(spec)) => Some(Stated::Clauses(spec)),
        (true, None) => {
            theorem_shape(body, &model)?;
            Some(Stated::Theorem)
        }
        (true, Some(_)) => {
            return Err(NotTranslated {
                function: body.display.clone(),
                what: "a `#[blaze::theorem]` function with `requires` or `ensures` clauses (a \
                       theorem states that it returns `true`; what the clauses say is another \
                       theorem)"
                    .to_owned(),
                at: format!("fn {}", body.display),
            });
        }
    };
    Ok(module_of(body, model, stated, imports))
}

/// What a function's module states of its model.
#[derive(Clone, Copy, Debug)]
pub enum Stated<'a> {
    /// The function's `requires` and `ensures`.
    Clauses(&'a Spec),
    /// The function is marked `#[blaze::theorem]`: it returns `true`.
    Theorem,
}

/// What a `#[blaze::theorem]` function is written against.
const THEOREM: &str = "#[blaze::theorem]";

/// Refused unless `body` can be a theorem: it returns `bool` and gives nothing else back, so
/// "it returns `true`" is all there is to say of a call.
fn theorem_shape(body: &body::Body, model: &Model) -> Result<(), NotTranslated> {
    let refuse = |what: &str| NotTranslated {
        function: body.display.clone(),
        what: what.to_owned(),
        at: format!("fn {}", body.display),
    };
    if body.locals[0].ty != body::Ty::Bool {
        return Err(refuse(&format!(
            "a `#[blaze::theorem]` function that returns `{}`, not `bool` (a theorem states \
             that its function returns `true`)",
            rust_ty(&body.locals[0].ty)
        )));
    }
    if !model.shape.mut_params.is_empty() {
        return Err(refuse(
            "a `#[blaze::theorem]` function with a `&mut` parameter (a theorem takes what it \
             speaks of by value or by shared reference)",
        ));
    }
    Ok(())
}

/// `ty` as Rust writes it, for a message.
pub(crate) fn rust_ty(ty: &body::Ty) -> String {
    use body::Ty;
    let list = |all: &[Ty]| all.iter().map(rust_ty).collect::<Vec<_>>().join(", ");
    match ty {
        Ty::Bool => "bool".to_owned(),
        Ty::Int(int) => format!("{int:?}").to_lowercase(),
        Ty::Unit => "()".to_owned(),
        Ty::Never => "!".to_owned(),
        Ty::Tuple(all) => format!("({})", list(all)),
        Ty::Adt(name) => name.replace('.', "::"),
        Ty::Option(inner) => format!("Option<{}>", rust_ty(inner)),
        Ty::Result(ok, err) => format!("Result<{}, {}>", rust_ty(ok), rust_ty(err)),
        Ty::Ref(inner) => format!("&{}", rust_ty(inner)),
        Ty::RefMut(inner) => format!("&mut {}", rust_ty(inner)),
        Ty::Slice(inner) => format!("[{}]", rust_ty(inner)),
        Ty::Vec(inner) => format!("Vec<{}>", rust_ty(inner)),
        Ty::Array(inner, len) => format!("[{}; {len}]", rust_ty(inner)),
        Ty::TryFromIntError => "core::num::TryFromIntError".to_owned(),
        Ty::Duration => "core::time::Duration".to_owned(),
        Ty::Ordering => "core::cmp::Ordering".to_owned(),
        Ty::Str => "str".to_owned(),
    }
}

/// `def <statement> : Prop` for a theorem function: for all arguments the model gives `true`.
/// It gives it: a model that fails (a panic) or, for a function with a loop, gives `true` for
/// no amount of fuel (it never returns) does not meet the statement. A test fails a hang by
/// its timeout, and the statement that takes the test's place says no less.
fn theorem_statement(function: &str, model: &Model, statement: &str) -> Decl {
    let mut args = Vec::new();
    if model.shape.fuel {
        args.push(name("fuel"));
    }
    args.extend(model.params.iter().map(|it| name(it.name.clone())));
    let call = app(function, args);
    let holds = match model.shape.monadic {
        true => infix(Infix::Eq, call, app("Result.ok", vec![atom("true")])),
        false => infix(Infix::Eq, call, atom("true")),
    };
    let holds = match model.shape.fuel {
        true => Term::Exists(
            vec![Binder { name: "fuel".to_owned(), ty: name("Nat") }],
            Box::new(holds),
        ),
        false => holds,
    };
    let body = match model.params.is_empty() {
        true => holds,
        false => Term::Forall(model.params.clone(), Box::new(holds)),
    };
    Decl::Def { name: statement.to_owned(), params: Vec::new(), ty: atom("Prop"), body }
}

/// The module of the struct or enum `adt`, `BlazeModel.<its path>.type`: its declaration
/// alone, importing the modules of the types its fields name. A type has one module, so two
/// functions that use it import one declaration of it; `type` is no Rust identifier, so no
/// function's module has that name.
pub fn type_module(adt: &body::AdtDecl) -> Emitted {
    let module = type_module_name(&adt.name);
    let display = adt.display.clone();
    let about = vec![
        format!("Generated by blaze ({TRANSLATOR}) from `{display}`: the type."),
        "Do not edit: the functions that use it import this module.".to_owned(),
    ];
    let mut imports = vec!["Blaze".to_owned()];
    imports.extend(adt.uses().iter().filter(|it| **it != adt.name).map(|it| lean::ident(&type_module_name(it))));
    let text = Module { about, imports, decls: vec![translate::type_decl(adt)] }.print();
    Emitted {
        function: display,
        name: adt.name.clone(),
        module,
        text,
        statement: None,
        clauses: Vec::new(),
        shape: Shape { fuel: false, monadic: false, mut_params: Vec::new(), returns_unit: false },
        imports: Vec::new(),
    }
}

fn type_module_name(adt: &str) -> String {
    format!("BlazeModel.{adt}.type")
}

/// The module of the struct or enum `adt`'s derived `PartialEq`, `BlazeModel.<its path>.impl`:
/// `deriving instance DecidableEq`, which `core.cmp.PartialEq.eq` (`lean/Blaze/Ordering.lean`)
/// decides `==` by. Rust's derive compares field by field with each field's `==`, which the
/// translator has checked is the equality of the field's value ([`body`]'s `structural`), so
/// it is Lean's `=` of the structure. Apart from the type's module, so the type's module is the
/// same whether or not a function compares it; it imports the modules of the types its fields
/// name, each of which a derived `==` compares too. `impl` is no Rust identifier, so no
/// function's module has that name.
pub fn eq_module(adt: &body::AdtDecl) -> Emitted {
    let module = format!("BlazeModel.{}.impl", adt.name);
    let display = adt.display.clone();
    let about = vec![
        format!("Generated by blaze ({TRANSLATOR}) from `{display}`: its derived `PartialEq`."),
        "Do not edit: the functions that compare it import this module.".to_owned(),
    ];
    let mut imports = vec!["Blaze".to_owned(), lean::ident(&type_module_name(&adt.name))];
    imports.extend(
        adt.uses()
            .iter()
            .filter(|it| **it != adt.name)
            .map(|it| lean::ident(&format!("BlazeModel.{it}.impl"))),
    );
    let text = Module { about, imports, decls: vec![Decl::DeriveDecidableEq(adt.name.clone())] }.print();
    Emitted {
        function: display,
        name: adt.name.clone(),
        module,
        text,
        statement: None,
        clauses: Vec::new(),
        shape: Shape { fuel: false, monadic: false, mut_params: Vec::new(), returns_unit: false },
        imports: Vec::new(),
    }
}

/// The module of `body`'s `model` and `spec`.
pub fn module_of(
    body: &body::Body,
    model: Model,
    stated: Option<Stated<'_>>,
    imports: Vec<Emitted>,
) -> Emitted {
    let module = format!("BlazeModel.{}", body.name);
    let mut decls = model.decls.clone();
    let statement = stated.map(|stated| {
        let statement = format!("{}.spec.statement", body.name);
        decls.push(match stated {
            Stated::Clauses(spec) => statement_of(&body.name, &model, spec, &statement),
            Stated::Theorem => theorem_statement(&body.name, &model, &statement),
        });
        statement
    });
    let about = vec![
        format!(
            "Generated by blaze ({TRANSLATOR}) from `{}`: its model{}.",
            body.display,
            match stated {
                None => "",
                Some(Stated::Clauses(_)) => " and the statement of its clauses",
                Some(Stated::Theorem) => " and the statement that it returns `true`",
            }
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
        clauses: match stated {
            None => Vec::new(),
            Some(Stated::Clauses(spec)) => spec.clauses.clone(),
            Some(Stated::Theorem) => vec![THEOREM.to_owned()],
        },
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
