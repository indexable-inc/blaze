//! What the general correctness lints (`blocking_in_async`, `relaxed_publish`,
//! `unwind_across_ffi`, `unbounded_from_input`, `dos_hasher_on_input`, `panic_in_drop`) ask of a
//! body: which std item a callee or a type is, found by its crate, the first module under the
//! crate root and its name (as `seqcst_unexplained` finds `Ordering`), never by the text written;
//! whether an expression runs inside an `async` body; what a path names; the orderings of
//! atomics; and which expressions of a body can panic.

use base_db::Db;
use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId, EnumVariantId, FunctionId, Lookup as _, VariantId,
    expr_store::{Body, scope::ExprScopes},
    hir::{ArithOp, BinaryOp, ClosureKind, CoroutineKind, CoroutineSource, Expr, ExprId, Literal},
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
};
use hir_ty::next_solver::GenericArgs;
use rustc_type_ir::inherent::GenericArgs as _;

use super::{alloc::written_callee, facts};

/// How many expressions a walk of a subtree visits at most; past it the walk reports it ran out
/// (rule 18), and the lints asking take that as "not shown", never as a finding.
pub(super) const FUEL: usize = 8192;

/// How many parents a walk up from an expression follows.
pub(super) const UP: usize = 256;

/// Whether `krate` is `core`, `alloc` or `std`.
pub(super) fn is_std_crate(db: &Db, krate: hir::Crate) -> bool {
    ["core", "alloc", "std"].iter().any(|it| facts::crate_is(db, krate, it))
}

/// The first named module under the crate root of `module`'s path (`fs` for `std::fs::File`'s),
/// when its crate is `core`, `alloc` or `std`.
pub(super) fn std_top(db: &Db, module: hir::Module) -> Option<hir::Name> {
    if !is_std_crate(db, module.krate(db)) {
        return None;
    }
    module.path_to_root(db).into_iter().rev().find_map(|it| it.name(db))
}

/// Whether `adt` is one of `names` in the module `top` of core, alloc or std.
pub(super) fn std_adt(db: &Db, adt: hir::Adt, top: &str, names: &[&str]) -> bool {
    names.contains(&adt.name(db).as_str())
        && std_top(db, adt.module(db)).is_some_and(|it| it.as_str() == top)
}

/// Whether `adt` is one of `names` of the crate named `krate`.
pub(super) fn crate_adt(db: &Db, adt: hir::Adt, krate: &str, names: &[&str]) -> bool {
    names.contains(&adt.name(db).as_str()) && facts::crate_is(db, adt.module(db).krate(db), krate)
}

/// A call's callee, as the lints match it: the function, its name, and the ADT its `self` is
/// (the impl's type for an inherent or trait impl's method, the `Self` inferred for a trait's
/// own method), references peeled.
#[derive(Clone, Copy)]
pub(super) struct Callee<'db> {
    pub(super) f: FunctionId,
    pub(super) name: &'db str,
    pub(super) self_adt: Option<hir::Adt>,
    /// The trait the method is of, when it is a trait's or a trait impl's.
    pub(super) trait_: Option<hir::Trait>,
}

/// What the call expression `call` of `analysis`'s body calls.
pub(super) fn callee<'db>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    call: ExprId,
) -> Option<Callee<'db>> {
    let (f, args) = written_callee(analysis.infer(), analysis.body(), call)?;
    let name = FunctionSignature::of(db, f).name.as_str();
    let (self_adt, trait_) =
        match hir::Function::from(f).as_assoc_item(db).map(|it| it.container(db)) {
            None => (None, None),
            Some(hir::AssocItemContainer::Trait(it)) => (self_of(analysis, args), Some(it)),
            Some(hir::AssocItemContainer::Impl(it)) => {
                (it.self_ty(db).strip_references().as_adt(), it.trait_(db))
            }
        };
    Some(Callee { f, name, self_adt, trait_ })
}

/// The ADT a trait method's `Self` is, from the call's generic arguments (a trait's own method
/// has `Self` first, as `blocking_on_pool` reads it).
fn self_of<'db>(analysis: &hir::BodyAnalysis<'db>, args: GenericArgs<'db>) -> Option<hir::Adt> {
    analysis.ty(args.type_at(0)).strip_references().as_adt()
}

impl Callee<'_> {
    /// Whether it is a free function (no `self` type) of the module `top` of core, alloc or std,
    /// named one of `names` (any name when `names` is empty).
    pub(super) fn is_std_free(&self, db: &Db, top: &str, names: &[&str]) -> bool {
        self.self_adt.is_none()
            && self.trait_.is_none()
            && hir::Function::from(self.f).as_assoc_item(db).is_none()
            && (names.is_empty() || names.contains(&self.name))
            && std_top(db, hir::Function::from(self.f).module(db))
                .is_some_and(|it| it.as_str() == top)
    }

    /// Whether the function is defined in the module `top` of core, alloc or std (a trait's
    /// method included: `Read::read` is of `io`).
    pub(super) fn in_std_module(&self, db: &Db, top: &str) -> bool {
        std_top(db, hir::Function::from(self.f).module(db)).is_some_and(|it| it.as_str() == top)
    }
}

/// Where an `async` body comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AsyncIn {
    Fn,
    Block,
    Closure,
}

impl AsyncIn {
    pub(super) fn say(self) -> &'static str {
        match self {
            AsyncIn::Fn => "`async fn`",
            AsyncIn::Block => "`async` block",
            AsyncIn::Closure => "`async` closure",
        }
    }
}

/// The `async` body `expr` runs in, if it runs in one: the innermost closure or coroutine above
/// it is an `async` one. A plain closure in between ends the walk: what it runs is not polled
/// by the executor (`spawn_blocking(|| ..)`'s closure runs on a blocking thread).
pub(super) fn async_context(
    body: &Body,
    parents: &facts::Parents,
    expr: ExprId,
) -> Option<AsyncIn> {
    for parent in parents.up(expr, UP) {
        let Expr::Closure { closure_kind, .. } = body.expr(parent) else { continue };
        return match closure_kind {
            ClosureKind::Coroutine {
                kind: CoroutineKind::Async | CoroutineKind::AsyncGen,
                source,
            } => Some(match source {
                CoroutineSource::Fn => AsyncIn::Fn,
                CoroutineSource::Block => AsyncIn::Block,
                CoroutineSource::Closure => AsyncIn::Closure,
            }),
            ClosureKind::CoroutineClosure(CoroutineKind::Async | CoroutineKind::AsyncGen) => {
                Some(AsyncIn::Closure)
            }
            ClosureKind::Closure
            | ClosureKind::OldCoroutine(_)
            | ClosureKind::Coroutine { .. }
            | ClosureKind::CoroutineClosure(_) => None,
        };
    }
    None
}

/// How a walk of a subtree ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Walked {
    /// Every expression of the subtree was visited.
    Done,
    /// [`FUEL`] expressions were visited and the rest were not.
    OutOfFuel,
}

/// Calls `f` on `root` and every expression under it (patterns' expressions included), at most
/// [`FUEL`] of them.
pub(super) fn walk(body: &Body, root: ExprId, mut f: impl FnMut(ExprId)) -> Walked {
    // alloc: a walk's stack, a few dozen entries
    let mut stack = vec![root];
    let mut fuel = FUEL;
    while let Some(at) = stack.pop() {
        if fuel == 0 {
            return Walked::OutOfFuel;
        }
        fuel -= 1;
        f(at);
        body.walk_child_exprs(at, |child| stack.push(child));
    }
    Walked::Done
}

/// The value the path expression `expr` of the body of `def` names, through the scope it is in.
pub(super) fn value_of(db: &Db, def: DefWithBodyId, body: &Body, expr: ExprId) -> Option<ValueNs> {
    let Expr::Path(path) = body.expr(expr) else { return None };
    let scope = ExprScopes::of(db, def).scope_for(expr);
    resolver_for_scope(db, def, scope).resolve_path_in_value_ns_fully(
        db,
        path,
        body.expr_path_hygiene(expr),
    )
}

/// The variant of `core::sync::atomic::Ordering` the expression `expr` names (`"Relaxed"`,
/// `"Release"`, ..), through renames and globs.
pub(super) fn ordering<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'db>,
    expr: ExprId,
) -> Option<&'static str> {
    let body = analysis.body();
    if !matches!(body.expr(expr), Expr::Path(_)) {
        return None;
    }
    let variant: EnumVariantId = match analysis.infer().variant_resolution_for_expr(expr) {
        Some(VariantId::EnumVariantId(it)) => it,
        _ => match value_of(db, def, body, expr)? {
            ValueNs::EnumVariantId(it) => it,
            _ => return None,
        },
    };
    let enum_ = hir::Enum::from(variant.lookup(db).parent);
    let module = enum_.module(db);
    if enum_.name(db).as_str() != "Ordering"
        || !module.name(db).is_some_and(|it| it.as_str() == "atomic")
    {
        return None;
    }
    if !is_std_crate(db, module.krate(db)) {
        return None;
    }
    let name = hir::EnumVariant::from(variant).name(db);
    ["Relaxed", "Release", "Acquire", "AcqRel", "SeqCst"]
        .into_iter()
        .find(|it| *it == name.as_str())
}

/// Whether `adt` is one of core's atomics (`AtomicBool`, `AtomicPtr`, `AtomicU64`, ..).
pub(super) fn is_atomic(db: &Db, adt: hir::Adt) -> bool {
    adt.name(db).as_str().starts_with("Atomic")
        && std_top(db, adt.module(db)).is_some_and(|it| it.as_str() == "sync")
}

/// One place of a body that can panic.
pub(super) struct PanicSite {
    pub(super) expr: ExprId,
    /// What it is, as a message names it: "this `unwrap`", "this index".
    pub(super) what: String,
}

/// The expressions of `analysis`'s body that can panic, from what is written there only (no
/// callee's body is read): `unwrap`, `expect`, `unwrap_err` and `expect_err` of `Option` and
/// `Result`; a call into core's or std's panic machinery (what `panic!`, `assert!`,
/// `unreachable!`, `todo!` and `unimplemented!` expand to, `panic_any`, `resume_unwind`); an
/// index (`a[i]`, through `Index`); and an integer `/` or `%` whose divisor is not a nonzero
/// literal. Overflow of `+`, `-` and `*` (a panic only with overflow checks) is not counted.
pub(super) fn panic_sites<'db>(db: &'db Db, analysis: &hir::BodyAnalysis<'db>) -> Vec<PanicSite> {
    // alloc: the few panic sites of one body
    let mut found = Vec::new();
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        let what = match expr {
            Expr::Index { .. } => Some("this index".to_owned()), // alloc: a message's part
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(BinaryOp::ArithOp(op @ (ArithOp::Div | ArithOp::Rem))),
            }
            | Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(BinaryOp::Assignment { op: Some(op @ (ArithOp::Div | ArithOp::Rem)) }),
            } => {
                let int = analysis.ty(infer.expr_ty(lhs)).strip_references().is_int_or_uint();
                let nonzero = matches!(
                    body.expr(rhs),
                    Expr::Literal(Literal::Int(v, _)) if *v != 0
                ) || matches!(body.expr(rhs), Expr::Literal(Literal::Uint(v, _)) if *v != 0);
                let sign = if op == ArithOp::Div { "/" } else { "%" };
                (int && !nonzero).then(|| format!("this `{sign}`, whose divisor may be zero"))
            }
            Expr::Call { .. } | Expr::MethodCall { .. } => {
                callee(db, analysis, id).and_then(|it| panics(db, &it))
            }
            _ => None,
        };
        if let Some(what) = what {
            found.push(PanicSite { expr: id, what });
        }
    }
    found
}

/// Whether calling `callee` panics by what it is: what a message calls it.
fn panics(db: &Db, callee: &Callee<'_>) -> Option<String> {
    if let Some(adt) = callee.self_adt
        && (std_adt(db, adt, "option", &["Option"]) || std_adt(db, adt, "result", &["Result"]))
        && matches!(callee.name, "unwrap" | "expect" | "unwrap_err" | "expect_err")
    {
        return Some(format!("this `{}`", callee.name));
    }
    let module = hir::Function::from(callee.f).module(db);
    let top = std_top(db, module)?;
    let machinery = match top.as_str() {
        "panicking" => true,
        "rt" => callee.name.contains("panic"),
        "panic" => matches!(callee.name, "panic_any" | "resume_unwind"),
        _ => false,
    };
    // alloc: a message's part
    machinery.then(|| "this panic".to_owned())
}

/// Whether the body calls the free function `top::name` of std anywhere (`panic::catch_unwind`,
/// `thread::panicking`).
pub(super) fn calls_std<'db>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    top: &str,
    name: &str,
) -> bool {
    analysis.body().exprs().any(|(id, expr)| {
        matches!(expr, Expr::Call { .. })
            && callee(db, analysis, id).is_some_and(|it| it.is_std_free(db, top, &[name]))
    })
}
