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
    // `Range` and `RangeInclusive` as values, and a slice indexed by a range.
    ("Blaze.Range", include_str!("../../../lean/Blaze/Range.lean")),
    ("Blaze.ScalarOps", include_str!("../../../lean/Blaze/ScalarOps.lean")),
    ("Blaze.Ordering", include_str!("../../../lean/Blaze/Ordering.lean")),
    ("Blaze.Duration", include_str!("../../../lean/Blaze/Duration.lean")),
    // The order a derive of `PartialOrd` or `Ord` gives, and those it compares fields by.
    ("Blaze.Order", include_str!("../../../lean/Blaze/Order.lean")),
    ("Blaze.Str", include_str!("../../../lean/Blaze/Str.lean")),
    ("Blaze.Machine", include_str!("../../../lean/Blaze/Machine.lean")),
    ("Blaze.Opaque", include_str!("../../../lean/Blaze/Opaque.lean")),
    // Threads that share atomic words, and `core::sync::atomic` over them.
    ("Blaze.Threads", include_str!("../../../lean/Blaze/Threads.lean")),
    ("Blaze.Atomic", include_str!("../../../lean/Blaze/Atomic.lean")),
    ("Blaze", include_str!("../../../lean/Blaze.lean")),
    // What a proof file imports and no model does: the tactic a statement is tried with first.
    ("Blaze.Auto", include_str!("../../../lean/Blaze/Auto.lean")),
    // The `@[spec]`s of `Blaze.Vec`'s operations, which `Blaze.Auto` must come before.
    ("Blaze.VecAuto", include_str!("../../../lean/Blaze/VecAuto.lean")),
    // What a proof about two threads is written with: the induction over a schedule and the
    // world's step on each operation.
    ("Blaze.ThreadsAuto", include_str!("../../../lean/Blaze/ThreadsAuto.lean")),
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
    // What the borrow checker said, first: an error names the thing it is about.
    let why = match (checked.diagnostics.first(), checked.not_checked, checked.skipped.as_ref()) {
        (Some(first), _, _) => Some(format!("{}: {}", first.code, first.message)),
        (None, Some(not), _) => Some(format!("not checked: {not:?}")),
        (None, None, Some(skipped)) => Some(format!("skipped: {skipped}")),
        (None, None, None) => None,
    };
    if let Some(why) = why {
        return Err(refuse(&format!("a function the borrow checker did not accept ({why})")));
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
    // Then the order of each whose derived `PartialOrd` or `Ord` it calls, each after those its
    // fields' orders are; its functions are pure.
    for adt in body.adts.iter().filter(|it| lowered.orders.contains(&it.name)) {
        imports.push(ord_module(adt).map_err(|what| refuse(what))?);
        for function in ORDER_FUNCTIONS {
            let pure = Shape { fuel: false, monadic: false, shared: false, mut_params: Vec::new(), returns_unit: false };
            shapes.insert(format!("{}.impl.{function}", adt.name), pure);
        }
    }
    // What a callee imports comes first; a module two callees share is listed once.
    let admit = |imports: &mut Vec<Emitted>, import: &Emitted| -> Result<(), NotTranslated> {
        match imports.iter().find(|it| it.module == import.module) {
            None => {
                // Two modules whose files differ only by letter case are one file where the
                // file system folds case (macOS's, by default).
                if let Some(it) =
                    imports.iter().find(|it| it.module.eq_ignore_ascii_case(&import.module))
                {
                    return Err(refuse(format!(
                        "two modules whose files differ only by letter case, `{}` and `{}`",
                        it.module, import.module
                    )));
                }
                imports.push(import.clone());
                Ok(())
            }
            // One module, two texts: two items with one Lean name (methods of two traits of one
            // name from two modules, on one type; two instances whose arguments are written
            // alike). Neither may stand for the other.
            Some(it) if it.text != import.text => Err(refuse(format!(
                "two functions with one Lean name, `{}` (`{}` and `{}`)",
                import.name, it.function, import.function
            ))),
            Some(_) => Ok(()),
        }
    };
    for (callee_name, callee) in &lowered.callees {
        let callee = emit_in(db, *callee, stack)?;
        shapes.insert(callee_name.clone(), callee.shape.clone());
        for import in callee.imports.iter().chain([&callee]) {
            admit(&mut imports, import)?;
        }
    }
    // An opaque function's module imports only the library and the modules of the types its
    // signature names, which are among this body's, so it comes after them.
    for opaque in &lowered.opaques {
        let module = opaque_module(opaque);
        shapes.insert(opaque.lean.clone(), module.shape.clone());
        admit(&mut imports, &module)?;
    }
    stack.pop();
    let func = instance.func;
    let model = translate::translate(body, &shapes)?;
    // A closure's body states nothing: `requires`, `ensures` and `#[blaze::theorem]` are its
    // function's.
    if instance.closure.is_some() {
        return Ok(module_of(body, model, None, imports));
    }
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
        (false, Some(_)) if model.shape.shared => {
            return Err(NotTranslated {
                function: body.display.clone(),
                what: "`requires` or `ensures` clauses on a shared function (its model is a \
                       `Threads.Prog`, a program of operations on the memory threads share, \
                       whose result depends on the other threads; state it as a \
                       `#[blaze::theorem]` over a schedule)"
                    .to_owned(),
                at: format!("fn {}", body.display),
            });
        }
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
        Ty::PhantomData => "core::marker::PhantomData".to_owned(),
        Ty::Duration => "core::time::Duration".to_owned(),
        Ty::Ordering => "core::cmp::Ordering".to_owned(),
        Ty::Range { elem, inclusive: false } => format!("core::ops::Range<{}>", rust_ty(elem)),
        Ty::Range { elem, inclusive: true } => format!("core::ops::RangeInclusive<{}>", rust_ty(elem)),
        Ty::Str => "str".to_owned(),
        Ty::Opaque(opaque) => opaque.rust().to_owned(),
        Ty::Shared(shared) => shared.rust(),
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
    // A shared function's model is a program of operations on the memory threads share: it is
    // run alone, from an empty memory (`Threads.Prog.value`), and gives `true`.
    let holds = match (model.shape.shared, model.shape.monadic) {
        (true, _) => infix(Infix::Eq, Term::Proj(Box::new(call), "value".to_owned()), app("Result.ok", vec![atom("true")])),
        (false, true) => infix(Infix::Eq, call, app("Result.ok", vec![atom("true")])),
        (false, false) => infix(Infix::Eq, call, atom("true")),
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
        shape: Shape { fuel: false, monadic: false, shared: false, mut_params: Vec::new(), returns_unit: false },
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
        shape: Shape { fuel: false, monadic: false, shared: false, mut_params: Vec::new(), returns_unit: false },
        imports: Vec::new(),
    }
}

/// The functions of a derived order's module ([`ord_module`]), each `<type>.impl.<it>`, named
/// as the trait methods they model.
const ORDER_FUNCTIONS: [&str; 8] = ["cmp", "partial_cmp", "lt", "le", "gt", "ge", "max", "min"];

/// The module of the struct or enum `adt`'s derived `PartialOrd` and `Ord`,
/// `BlazeModel.<its path>.ord`: `<type>.impl.cmp`, its order, and the trait methods by it
/// ([`ORDER_FUNCTIONS`]). `impl` is a Rust keyword, so no method or trait of the crate has
/// that name. The rule is the derive's (crates/hir-expand/src/builtin/derive_macro.rs,
/// `write_ordering`, as rustc's): the fields in declaration order, the first not `Equal`
/// deciding (`Ordering.then`); of an enum of several variants, the variants' discriminants
/// first, by their type's order, so two values of different variants are ordered by them and
/// two of one variant by its fields. Each field is compared by its type's order in
/// `lean/Blaze/Order.lean` or by the derived one of the crate's type it is, whose module this
/// imports: the translator gives a call this model only for such fields ([`body`]'s
/// `orderable`). Every such order is total, so `partial_cmp` is `Some` of `cmp`, and `lt`..
/// are the trait's defaults on it (cmp.rs:1502-1556), `max` and `min` `Ord`'s (cmp.rs:1101,
/// 1140).
pub fn ord_module(adt: &body::AdtDecl) -> Result<Emitted, String> {
    let module = format!("BlazeModel.{}.ord", adt.name);
    let display = adt.display.clone();
    let about = vec![
        format!("Generated by blaze ({TRANSLATOR}) from `{display}`: its derived `PartialOrd` and `Ord`."),
        "Do not edit: the functions that compare it import this module.".to_owned(),
    ];
    let mut imports = vec!["Blaze".to_owned(), lean::ident(&type_module_name(&adt.name))];
    imports.extend(
        adt.uses()
            .iter()
            .filter(|it| **it != adt.name)
            .map(|it| lean::ident(&format!("BlazeModel.{it}.ord"))),
    );
    // Variables named apart from every root of a global name the module uses (the library's
    // roots are capitalized or `core`, none of these).
    let mut roots: std::collections::BTreeSet<String> = adt.uses();
    roots.insert(adt.name.clone());
    let roots: std::collections::BTreeSet<String> =
        roots.iter().filter_map(|it| it.split('.').next().map(str::to_owned)).collect();
    let var = |base: String| -> String {
        let mut it = base;
        while roots.contains(&it) {
            it.push('_');
        }
        it
    };
    let (l, r) = (var("l".to_owned()), var("r".to_owned()));
    let own = |function: &str| format!("{}.impl.{function}", adt.name);
    let then = |orders: Vec<Term>| -> Term {
        let mut orders = orders.into_iter().rev();
        let Some(last) = orders.next() else { return name("core.cmp.Ordering.Equal") };
        orders.fold(last, |rest, first| app("core.cmp.Ordering.then", vec![first, rest]))
    };
    let ty = name(adt.name.clone());
    let pair = |l: &str, r: &str| vec![Binder { name: l.to_owned(), ty: ty.clone() }, Binder { name: r.to_owned(), ty: ty.clone() }];
    let mut decls = Vec::new();
    let cmp = match &adt.kind {
        body::AdtKind::Struct(fields) => {
            let mut orders = Vec::with_capacity(fields.len());
            for field in fields {
                let by = order_of(&field.ty)?;
                let at = |of: &str| Term::Proj(Box::new(name(of.to_owned())), field.name.clone());
                orders.push(Term::App(Box::new(by), vec![at(&l), at(&r)]));
            }
            then(orders)
        }
        body::AdtKind::Enum(variants) => {
            if variants.is_empty() {
                return Err(format!("the derived order of `{display}`, an enum of no variant"));
            }
            let ctor = |variant: &body::Variant| format!("{}.{}", adt.name, variant.name);
            let mut arms = Vec::with_capacity(variants.len() + 1);
            for variant in variants {
                let ls: Vec<String> = (0..variant.fields.len()).map(|i| var(format!("{l}{i}"))).collect();
                let rs: Vec<String> = (0..variant.fields.len()).map(|i| var(format!("{r}{i}"))).collect();
                let mut orders = Vec::with_capacity(variant.fields.len());
                for (i, field) in variant.fields.iter().enumerate() {
                    let by = order_of(&field.ty)?;
                    orders.push(Term::App(Box::new(by), vec![name(ls[i].clone()), name(rs[i].clone())]));
                }
                let pat = |names: &[String]| Pat::Ctor(ctor(variant), names.iter().map(|it| Pat::Name(it.clone())).collect());
                arms.push((Pat::Tuple(vec![pat(&ls), pat(&rs)]), then(orders)));
            }
            if variants.len() > 1 {
                // The discriminant of each variant, as an `Int`: `discriminant_value`'s, as its
                // type's value (an unsigned one above `i128::MAX` is refused before this).
                let x = var("v".to_owned());
                let discriminants = variants
                    .iter()
                    .map(|variant| {
                        let wild = variant.fields.iter().map(|_| Pat::Wild).collect();
                        (Pat::Ctor(ctor(variant), wild), atom(format!("({} : Int)", variant.discriminant as i128)))
                    })
                    .collect();
                decls.push(Decl::Def {
                    name: own("discriminant"),
                    params: vec![Binder { name: x.clone(), ty: ty.clone() }],
                    ty: name("Int"),
                    body: Term::Match(Box::new(name(x)), discriminants),
                });
                let of = |v: &str| app(&own("discriminant"), vec![name(v.to_owned())]);
                arms.push((Pat::Wild, app("core.cmp.Order.int", vec![of(&l), of(&r)])));
            }
            Term::Match(Box::new(Term::Tuple(vec![name(l.clone()), name(r.clone())])), arms)
        }
    };
    let ordering = name("core.cmp.Ordering");
    let by_cmp = |l: &str, r: &str| app(&own("cmp"), vec![name(l.to_owned()), name(r.to_owned())]);
    decls.push(Decl::Def { name: own("cmp"), params: pair(&l, &r), ty: ordering.clone(), body: cmp });
    decls.push(Decl::Def {
        name: own("partial_cmp"),
        params: pair(&l, &r),
        ty: app("Option", vec![ordering]),
        body: app("Option.some", vec![by_cmp(&l, &r)]),
    });
    for (function, test) in [("lt", "is_lt"), ("le", "is_le"), ("gt", "is_gt"), ("ge", "is_ge")] {
        decls.push(Decl::Def {
            name: own(function),
            params: pair(&l, &r),
            ty: name("Bool"),
            body: app(&format!("core.cmp.Ordering.{test}"), vec![by_cmp(&l, &r)]),
        });
    }
    // `max`: `if other < self { self } else { other }`; `min`: `if other < self { other } else
    // { self }` (cmp.rs:1101, 1140).
    let lt = |a: &str, b: &str| app(&own("lt"), vec![name(a.to_owned()), name(b.to_owned())]);
    for (function, (first, second)) in [("max", (&l, &r)), ("min", (&r, &l))] {
        decls.push(Decl::Def {
            name: own(function),
            params: pair(&l, &r),
            ty: ty.clone(),
            body: Term::If(Box::new(lt(&r, &l)), Box::new(name(first.clone())), Box::new(name(second.clone()))),
        });
    }
    let text = Module { about, imports, decls }.print();
    Ok(Emitted {
        function: display,
        name: adt.name.clone(),
        module,
        text,
        statement: None,
        clauses: Vec::new(),
        shape: Shape { fuel: false, monadic: false, shared: false, mut_params: Vec::new(), returns_unit: false },
        imports: Vec::new(),
    })
}

/// The order of a field of type `ty` as a Lean function of two values
/// (`lean/Blaze/Order.lean`, or a derived order's `<type>.impl.cmp`).
fn order_of(ty: &body::Ty) -> Result<Term, String> {
    use body::Ty;
    Ok(match ty {
        Ty::Int(_) => name("Scalar.cmp"),
        Ty::Bool => name("core.cmp.Order.bool"),
        Ty::Unit => name("core.cmp.Order.unit"),
        Ty::PhantomData => name("core.cmp.Order.phantom"),
        Ty::Duration => name("core.time.Duration.cmp"),
        Ty::Ordering => name("core.cmp.Order.ordering"),
        Ty::Ref(inner) => order_of(inner)?,
        Ty::Option(inner) => app("core.cmp.Order.option", vec![order_of(inner)?]),
        Ty::Array(inner, _) => app("core.cmp.Order.array", vec![order_of(inner)?]),
        Ty::Tuple(fields) if fields.is_empty() => name("core.cmp.Order.unit"),
        Ty::Tuple(fields) => {
            let mut orders: Vec<Term> = fields.iter().map(order_of).collect::<Result<_, _>>()?;
            let last = orders.pop().expect("a tuple of at least one field");
            orders.into_iter().rev().fold(last, |rest, first| app("core.cmp.Order.pair", vec![first, rest]))
        }
        Ty::Adt(adt) => name(format!("{adt}.impl.cmp")),
        other => return Err(format!("the order of `{}`, a field of a derived order, which has no model (rule NoModel)", rust_ty(other))),
    })
}

/// The module of the opaque function `opaque` ([`body::OPAQUE_FUNCTIONS`]) at its instance,
/// `BlazeModel.<its Lean name>`: `opaque <name> (a0 : A0) .. : Result R`, a Lean constant with
/// no definition the kernel unfolds, so a proof about a caller holds whatever the function
/// does. `R` is what a translated function of that signature gives: its result (unless `()`),
/// then the value after of each `&mut` parameter; in `Result`, for it may panic or run
/// forever. Lean's `opaque` needs only that the type is inhabited, which `Blaze.Opaque`'s
/// `Inhabited (Result α)` (by `Result.div`) gives without an axiom; nothing is assumed of the
/// function but its type. Being a constant, it gives one result for one set of arguments: why
/// that is true of the Rust function is said by its entry in the list.
pub fn opaque_module(opaque: &body::OpaqueFn) -> Emitted {
    let module = format!("BlazeModel.{}", opaque.lean);
    let about = vec![
        format!(
            "Generated by blaze ({TRANSLATOR}) from `{}`: an opaque function, declared with no \
             definition.",
            opaque.display
        ),
        "Do not edit: the functions that call it import this module.".to_owned(),
    ];
    let mut uses = std::collections::BTreeSet::new();
    for ty in opaque.params.iter().chain([&opaque.result]) {
        ty.adts(&mut uses);
    }
    let mut imports = vec!["Blaze".to_owned()];
    imports.extend(uses.iter().map(|it| lean::ident(&type_module_name(it))));
    let returns_unit = matches!(opaque.result, body::Ty::Unit | body::Ty::Never);
    let mut gives = Vec::new();
    if !returns_unit {
        gives.push(translate::lean_ty(&opaque.result));
    }
    let mut mut_params = Vec::new();
    for (at, ty) in opaque.params.iter().enumerate() {
        if let body::Ty::RefMut(inner) = ty {
            mut_params.push(at);
            gives.push(translate::lean_ty(inner));
        }
    }
    let gives = gives
        .into_iter()
        .rev()
        .reduce(|rest, ty| infix(Infix::Prod, ty, rest))
        .unwrap_or_else(|| name("Unit"));
    let params = opaque
        .params
        .iter()
        .enumerate()
        .map(|(at, ty)| Binder { name: format!("a{at}"), ty: translate::lean_ty(ty) })
        .collect();
    let decl = Decl::Opaque { name: opaque.lean.clone(), params, ty: app("Result", vec![gives]) };
    let text = Module { about, imports, decls: vec![decl] }.print();
    Emitted {
        function: opaque.display.clone(),
        name: opaque.lean.clone(),
        module,
        text,
        statement: None,
        clauses: Vec::new(),
        shape: Shape { fuel: false, monadic: true, shared: false, mut_params, returns_unit },
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
