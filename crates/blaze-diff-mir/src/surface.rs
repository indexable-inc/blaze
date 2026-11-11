//! `#[blaze::differentiable(symbolic)]`: a function's symbolic derivative as a body.
//!
//! [`derivative_body`] is what a use of the derivative compiles to (`blaze::grad!(f, wrt =
//! x)`, docs/future/autodiff.md): the function is read as marked symbolic and pure, its
//! monomorphized runtime MIR is lifted ([`crate::lift`]), differentiated by the parameter
//! asked (`blaze_diff::diff`), simplified by the exact rewrites, or the algebraic ones too
//! under `fast_math` (`blaze_diff::simplify`), and lowered back ([`crate::lower`]). Each step
//! that cannot be done is a [`DiffError`] naming the function, the thing and where it is; none
//! switches to another way of differentiating (no-fallback).
//!
//! [`derived`] stops before lowering, for `blaze explain`: the derivative as a graph, how the
//! simplification ended and which rewrites ran. [`explain`] prints it.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_diff::{
    Choice, Options, Policy, Simplified, Stop,
    diff::{DiffError as RuleError, Differentiator},
    print,
    simplify::simplify,
};
use hir_def::{
    FunctionId, HasModule, ModuleDefId, ModuleId,
    attrs::{AttrFlags, DifferentiableAttr},
    nameres::{ModuleData, crate_def_map},
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::HirDatabase,
    intrinsic::intrinsic_of,
    next_solver::{GenericArgs, Ty},
};
use hir_ty_mir::mir::{
    MirBody, MirSpan, Operand,
    checks::source_span,
    location::{source_location_in_file, source_path},
};
use hir_ty_pure::{FnPurity, fn_purity};
use intern::Symbol;

use crate::{
    lift::{Callees, Lifted, NotLifted, Signature, intrinsic_name, lift},
    lower::{NotLowered, lower},
};

/// Why a function has no symbolic derivative body. `function` is the function's name.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffError {
    /// It is not `#[blaze::differentiable]`.
    NotDifferentiable { function: String },
    /// It is `#[blaze::differentiable]` without `symbolic`: its derivative is layer 1's, which
    /// this does not build.
    NotSymbolic { function: String },
    /// `#[blaze::differentiable(..)]` with arguments that are not `symbolic` or `symbolic,
    /// fast_math`.
    Malformed { function: String, arguments: String },
    /// It is not `#[blaze::pure]`: a symbolic derivative is of a function proved pure.
    NotPure { function: String },
    /// `#[blaze::pure]` refuses it: what the proof said.
    Impure { function: String, refusal: String },
    /// Its MIR could not be built.
    NoBody { function: String, error: String },
    /// Its body did not lift to an expression: the thing, and where it is written when the
    /// source says (`path:line:column`).
    NotLifted { function: String, why: NotLifted, at: Option<String> },
    /// It has `params` parameters, and the derivative by parameter `wrt` was asked.
    NoSuchParam { function: String, wrt: u32, params: u32 },
    /// A rule refused: a point without a derivative on the variable's way.
    Rule { function: String, error: RuleError },
    /// The derivative did not lower to a body.
    NotLowered { function: String, why: NotLowered },
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffError::NotDifferentiable { function } => write!(
                f,
                "`{function}` has no derivative to ask for: it is not `#[blaze::differentiable]`"
            ),
            DiffError::NotSymbolic { function } => write!(
                f,
                "`{function}` is `#[blaze::differentiable]` without `symbolic`: its derivative is \
                 by automatic differentiation on MIR (layer 1), which blaze does not build yet"
            ),
            DiffError::Malformed { function, arguments } => write!(
                f,
                "`#[blaze::differentiable{arguments}]` on `{function}`: the arguments are \
                 `symbolic` or `symbolic, fast_math`, or none"
            ),
            DiffError::NotPure { function } => write!(
                f,
                "`{function}` is `#[blaze::differentiable(symbolic)]` and not `#[blaze::pure]`: a \
                 symbolic derivative is of a function proved pure; add `#[blaze::pure]`"
            ),
            DiffError::Impure { function, refusal } => write!(
                f,
                "`{function}` is `#[blaze::differentiable(symbolic)]`, and `#[blaze::pure]` \
                 refuses it: {refusal}"
            ),
            DiffError::NoBody { function, error } => {
                write!(f, "the MIR of `{function}` could not be built: {error}")
            }
            DiffError::NotLifted { function, why, at } => {
                write!(f, "`{function}` has no symbolic derivative: its body has {why}")?;
                if let Some(at) = at {
                    write!(f, ", at {at}")?;
                }
                write!(
                    f,
                    ". Automatic differentiation (`#[blaze::differentiable]` without `symbolic`) \
                     {}; blaze does not switch to it on its own",
                    why.layer_1()
                )
            }
            DiffError::NoSuchParam { function, wrt, params } => write!(
                f,
                "the derivative of `{function}` by parameter {wrt} was asked, and it has {params}"
            ),
            DiffError::Rule { function, error } => {
                write!(f, "`{function}` has no symbolic derivative: {error}")
            }
            DiffError::NotLowered { function, why } => {
                write!(f, "the derivative of `{function}` has no body: {why}")
            }
        }
    }
}

impl std::error::Error for DiffError {}

/// How `function` asks to be differentiated: `Ok(fast_math)` when it is
/// `#[blaze::differentiable(symbolic)]` and `#[blaze::pure]` proves it pure.
pub fn symbolic_mode(db: &Db, function: FunctionId) -> Result<bool, DiffError> {
    // alloc: an error names the function; one short string per derivative asked
    let name = || FunctionSignature::of(db, function).name.as_str().to_owned();
    let fast_math = match AttrFlags::differentiable_attr(db, function.into()) {
        None => return Err(DiffError::NotDifferentiable { function: name() }),
        Some(DifferentiableAttr::Auto) => return Err(DiffError::NotSymbolic { function: name() }),
        Some(DifferentiableAttr::Malformed(arguments)) => {
            // alloc: an error's text
            let arguments = arguments.as_str().to_owned();
            return Err(DiffError::Malformed { function: name(), arguments });
        }
        Some(DifferentiableAttr::Symbolic { fast_math }) => fast_math,
    };
    if AttrFlags::pure_attr(db, function.into()).is_none() {
        return Err(DiffError::NotPure { function: name() });
    }
    let args = GenericArgs::new_from_slice(&[]).store();
    match fn_purity(db, function.krate(db), function, args) {
        FnPurity::Pure(_) => Ok(fast_math),
        FnPurity::Impure(refusal) => {
            Err(DiffError::Impure { function: name(), refusal: refusal.said() })
        }
    }
}

/// Where `span` of `body` is written: `path:line:column`.
fn written_at(db: &Db, body: &MirBody<'_>, span: MirSpan) -> Option<String> {
    let (span, _) = source_span(body, span);
    let InferBodyId::DefWithBodyId(owner) = body.owner else { return None };
    let (file, line, column) = source_location_in_file(db, owner, span)?;
    let path = source_path(db, HasModule::krate(&owner, db), file).unwrap_or_default();
    // alloc: an error's text
    Some(format!("{path}:{line}:{column}"))
}

/// The monomorphized runtime MIR of `function`, which takes no generic arguments.
pub(crate) fn body_of<'db>(
    db: &'db Db,
    function: FunctionId,
) -> Result<&'db MirBody<'db>, DiffError> {
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(function.into()),
        krate: function.krate(db),
    };
    let args = GenericArgs::new_from_slice(&[]).store();
    db.monomorphized_runtime_mir_body(function.into(), args, env.store()).map_err(|error| {
        DiffError::NoBody {
            // alloc: an error's text
            function: FunctionSignature::of(db, function).name.as_str().to_owned(),
            error: format!("{error:?}"),
        }
    })
}

fn module_data(db: &Db, module: ModuleId) -> &ModuleData {
    module.to_db(db).def_map(db).module(db, module)
}

/// `krate` itself when it is `core`, else its dependency that is.
fn core_of(db: &Db, krate: Crate) -> Option<Crate> {
    let is =
        |krate: Crate| matches!(krate.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Core));
    if is(krate) {
        return Some(krate);
    }
    krate.data(db).dependencies.iter().map(|dep| dep.crate_id).find(|&dep| is(dep))
}

/// `core::intrinsics::<name>` instantiated at `float`, as a call's callee.
fn core_intrinsic<'db>(db: &'db Db, krate: Crate, name: &str, float: Ty<'db>) -> Option<Operand> {
    let core = core_of(db, krate)?;
    let root = crate_def_map(db, core).root_module_id();
    let in_scope = |module: ModuleId, name: &str| {
        module_data(db, module).name_in_scope(db, &Name::new_symbol_root(Symbol::intern(name)))
    };
    let ModuleDefId::ModuleId(intrinsics) = in_scope(root, "intrinsics").take_types()? else {
        return None;
    };
    let ModuleDefId::FunctionId(func) = in_scope(intrinsics, name).take_values()? else {
        return None;
    };
    Some(Operand::from_fn(db, func, GenericArgs::new_from_slice(&[float.into()])))
}

/// The callees of the operations `graph` calls that the lifted body did not: the intrinsic of
/// `core::intrinsics` for each, where there is one. A derivative calls functions its function
/// does not (`cos` for a `sin`).
fn resolve_callees<'db>(
    db: &'db Db,
    krate: Crate,
    simplified: &Simplified,
    signature: &Signature<'db>,
    callees: &mut Callees,
) {
    let reached = simplified.graph.reached(&simplified.roots);
    for (at, node) in simplified.graph.nodes().iter().enumerate() {
        if !reached[at] || callees.of(node.op).is_some() {
            continue;
        }
        let Some(name) = intrinsic_name(node.op) else { continue };
        let Some(func) = core_intrinsic(db, krate, name, signature.float.as_ref()) else {
            continue;
        };
        let intrinsic = func.fn_item().and_then(|(func, _)| intrinsic_of(db, func).ok().flatten());
        callees.note(node.op, &func, intrinsic);
    }
}

/// A function's symbolic derivative, before it is lowered.
#[derive(Debug)]
pub struct Derived<'db> {
    /// The derivative, simplified: one root per scalar of the function's result.
    pub derivative: Simplified,
    /// Whether the algebraic rewrites ran too (`fast_math`).
    pub fast_math: bool,
    pub signature: Signature<'db>,
    /// The functions the derivative's operations call.
    pub callees: Callees,
}

/// The symbolic derivative of `function` by its parameter `wrt` (0 for the first):
/// `function` is `#[blaze::differentiable(symbolic)]` and `#[blaze::pure]`, takes no generic
/// arguments, and its body lifts.
pub fn derived<'db>(
    db: &'db Db,
    function: FunctionId,
    wrt: u32,
) -> Result<Derived<'db>, DiffError> {
    let fast_math = symbolic_mode(db, function)?;
    // alloc: an error names the function
    let name = || FunctionSignature::of(db, function).name.as_str().to_owned();
    let body = body_of(db, function)?;
    let Lifted { mut graph, roots, signature, mut callees } = lift(db, body).map_err(|why| {
        let at = written_at(db, body, why.span());
        DiffError::NotLifted { function: name(), why, at }
    })?;
    if wrt >= signature.params {
        return Err(DiffError::NoSuchParam { function: name(), wrt, params: signature.params });
    }
    // The default policy: a point without a derivative is a typed error, never a silent pick.
    let policy = Policy { kinks: Choice::Error };
    let derivatives = Differentiator::new(policy)
        .derivatives(&mut graph, &roots, wrt)
        .map_err(|error| DiffError::Rule { function: name(), error })?;
    let options = Options { fast_math, ..Options::default() };
    let derivative = simplify(&graph, &derivatives, &options);
    resolve_callees(db, function.krate(db), &derivative, &signature, &mut callees);
    Ok(Derived { derivative, fast_math, signature, callees })
}

/// The body of the derivative of `function` by its parameter `wrt`: the same parameters and
/// return type as `function`, computing `d(function)/d(parameter wrt)`. What a use of the
/// derivative is compiled to; the body goes down the normal pipeline.
pub fn derivative_body<'db>(
    db: &'db Db,
    function: FunctionId,
    wrt: u32,
) -> Result<MirBody<'db>, DiffError> {
    let Derived { derivative, signature, callees, .. } = derived(db, function, wrt)?;
    lower(db, &derivative.graph, &derivative.roots, &signature, &callees).map_err(|why| {
        DiffError::NotLowered {
            // alloc: an error's text
            function: FunctionSignature::of(db, function).name.as_str().to_owned(),
            why,
        }
    })
}

/// `blaze explain d/d<param> <function>`: the derivative as one line of math per result, how
/// the simplification ended and the rewrites that ran. `params` names the parameters (`x0`,
/// `x1`, .. past them).
pub fn explain(
    db: &Db,
    function: FunctionId,
    wrt: u32,
    params: &[&str],
) -> Result<String, DiffError> {
    use std::fmt::Write as _;
    let derived = derived(db, function, wrt)?;
    let name = FunctionSignature::of(db, function).name.as_str();
    // alloc: the explanation's text, the caller's result
    let by = params.get(wrt as usize).map_or_else(|| format!("x{wrt}"), |it| (*it).to_owned());
    let mut out = String::new();
    for &root in &derived.derivative.roots {
        let _ = writeln!(out, "d/d{by} {name}");
        let _ = writeln!(out, "  {}", print::math(&derived.derivative.graph, root, params));
    }
    let rules =
        if derived.fast_math { "exact and fast_math rewrites" } else { "exact rewrites only" };
    let stop = match derived.derivative.stop {
        Stop::Saturated => "ran to the end".to_owned(),
        Stop::Iterations(rounds) => format!("stopped by its bound of {rounds} rounds"),
        Stop::Nodes(nodes) => format!("stopped by its bound on the e-graph, at {nodes} nodes"),
    };
    let _ = writeln!(out, "  simplification ({rules}) {stop}");
    if !derived.derivative.ran.is_empty() {
        let _ = writeln!(out, "  rewrites that ran: {}", derived.derivative.ran.join(", "));
    }
    Ok(out)
}
