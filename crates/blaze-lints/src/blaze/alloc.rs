//! `hot_alloc`: a call that reaches the global allocator.
//!
//! [`reaches_alloc`] answers for one instance of a function, its generic arguments given: a
//! root is a function with `#[rustc_allocator]`, `#[rustc_allocator_zeroed]` or
//! `#[rustc_reallocator]` (the `__rust_alloc` family), the `exchange_malloc` lang item or the
//! `box_new` intrinsic; any other function reaches when a call in its body does, the callee's
//! arguments instantiated with the instance's and resolved to the impl that runs
//! (`lookup_impl_method`). So std's `Vec::with_capacity`, which allocates only through
//! `A: Allocator` three calls down, and `collect`, which allocates only through
//! `B: FromIterator`, are seen to allocate from the call that names their arguments. A call that
//! resolves to no body (a trait's required method, through a type parameter the hot function
//! itself has, or through `dyn`) does not count. Cycles start as not reaching and are iterated to
//! their fixpoint.
//!
//! The walk goes into a function of a workspace member only for `hot_alloc_call`, the
//! item-level contract that nothing a hot function calls allocates; for `hot_alloc` such a
//! function's allocations are found in its own body, and a call of it is none.

use base_db::Db;
use hir_def::{
    AdtId, BuiltinDeriveImplId, DefWithBodyId, FunctionId, HasModule as _, ItemContainerId,
    VariantId,
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::Body,
    hir::{Expr, ExprId, UnaryOp},
    lang_item::lang_items,
    signatures::FunctionSignature,
    type_ref::Mutability,
};
use hir_ty::{
    InferenceResult, ParamEnvAndCrate,
    db::HirDatabase as _,
    next_solver::{DbInterner, EarlyBinder, GenericArg, GenericArgs, StoredGenericArgs, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use intern::sym;
use rustc_type_ir::inherent::IntoKind as _;

/// The most type nodes an instance's arguments may have: past it a function instantiates
/// itself with ever larger arguments (polymorphic recursion), which rustc refuses at its
/// `type_length_limit`; such an instance is not followed.
const LONGEST: usize = 256;

/// Whether the instance `f` with `args`, in the environment `env`, reaches the global
/// allocator: through functions of workspace members too when `through_workspace`.
#[blaze_query::tracked(returns(copy), cycle_initial = |_, _, _, _, _, _| false)]
pub(crate) fn reaches_alloc(
    db: &Db,
    env: StoredParamEnvAndCrate,
    f: FunctionId,
    args: StoredGenericArgs,
    through_workspace: bool,
) -> bool {
    if is_root(db, f) {
        return true;
    }
    if !FunctionSignature::of(db, f).has_body() {
        return false;
    }
    let def = DefWithBodyId::FunctionId(f);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    let interner = DbInterner::new_no_crate(db);
    let env = env.as_ref(db);
    let args = args.as_ref();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((callee, callee_args)) = written_callee(infer, body, id) else { continue };
        let callee_args =
            EarlyBinder::bind(interner, callee_args).instantiate(interner, args).skip_norm_wip();
        if instance_reaches(db, env, resolve(db, env, callee, callee_args), through_workspace) {
            return true;
        }
    }
    false
}

/// A function of the allocator itself.
fn is_root(db: &Db, f: FunctionId) -> bool {
    let flags = AttrFlags::query(db, f.into());
    if flags.intersects(
        AttrFlags::RUSTC_ALLOCATOR | AttrFlags::RUSTC_ALLOCATOR_ZEROED | AttrFlags::RUSTC_REALLOCATOR,
    ) {
        return true;
    }
    if lang_items(db, f.krate(db)).ExchangeMalloc == Some(f) {
        return true;
    }
    FunctionSignature::is_intrinsic(db, f) && FunctionSignature::of(db, f).name == sym::box_new
}

/// The function a call or method call `id` of `body` names, with its arguments in the
/// generics of the body's owner, as inference recorded them: a method's resolution, the
/// `Fn*` method of an overloaded call, or the function item called.
pub(super) fn written_callee<'db>(
    infer: &InferenceResult<'db>,
    body: &Body,
    id: ExprId,
) -> Option<(FunctionId, GenericArgs<'db>)> {
    if let Some(it) = infer.method_resolution(id) {
        return Some(it);
    }
    let Expr::Call { callee, .. } = body.expr(id) else { return None };
    let TyKind::FnDef(def, args) = infer.expr_ty(callee).kind() else { return None };
    match def.0 {
        hir_def::CallableDefId::FunctionId(f) => Some((f, args.skip_binder())),
        _ => None,
    }
}

/// What runs for a call of `f` with `args`.
#[derive(Clone, Copy)]
pub(super) enum Resolved<'db> {
    /// A function, a trait's own when no impl is known (its default body, or none).
    Function(FunctionId, GenericArgs<'db>),
    /// A method of a built-in derive's impl, which has no body of its own.
    Derived(BuiltinDeriveImplId, BuiltinDeriveImplMethod, GenericArgs<'db>),
}

/// The impl's function a call of the trait's `f` with `args` runs, as far as `env` knows.
pub(super) fn resolve<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    f: FunctionId,
    args: GenericArgs<'db>,
) -> Resolved<'db> {
    let (found, args) = db.lookup_impl_method(env, f, args);
    match found.right() {
        Some((impl_, method)) => Resolved::Derived(impl_, method, args),
        None => Resolved::Function(found.left().unwrap_or(f), args),
    }
}

/// Whether what `resolved` runs reaches the allocator: through functions of workspace members
/// too when `through_workspace`.
pub(super) fn instance_reaches<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    resolved: Resolved<'db>,
    through_workspace: bool,
) -> bool {
    match resolved {
        Resolved::Function(f, args) => {
            (through_workspace || !f.krate(db).data(db).origin.is_local())
                && fits(args.types().map(|it| it.walk()), LONGEST)
                && reaches_alloc(db, env.store(), f, args.store(), through_workspace)
        }
        Resolved::Derived(impl_, method, args) => {
            derived_reaches(db, env, impl_, method, args, through_workspace)
        }
    }
}

/// Whether the walks `types` step through at most `longest` nodes between them: stops at the
/// first node past it, so a call is never charged more than `longest` steps before its memo.
fn fits<I: Iterator>(types: impl Iterator<Item = I>, longest: usize) -> bool {
    let mut left = longest;
    for walk in types {
        for _ in walk {
            let Some(rest) = left.checked_sub(1) else { return false };
            left = rest;
        }
    }
    true
}

/// Whether a built-in derive's `clone` or `default` reaches the allocator: it calls the
/// trait's method on every field. The other derived methods reach nothing through their
/// fields that a hot function would call them for (`fmt` formats through `&dyn Debug`).
fn derived_reaches<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    impl_: BuiltinDeriveImplId,
    method: BuiltinDeriveImplMethod,
    args: GenericArgs<'db>,
    through_workspace: bool,
) -> bool {
    if !matches!(method, BuiltinDeriveImplMethod::clone | BuiltinDeriveImplMethod::default) {
        return false;
    }
    let Some(trait_method) = method.trait_method(db, impl_) else { return false };
    let interner = DbInterner::new_no_crate(db);
    let adt = impl_.loc(db).adt;
    any_variant(db, adt, |variant| {
        db.field_types(variant).iter().any(|(_, field)| {
            let ty = field.ty().instantiate(interner, args).skip_norm_wip();
            let field_args = GenericArgs::new_from_slice(&[GenericArg::from(ty)]);
            instance_reaches(db, env, resolve(db, env, trait_method, field_args), through_workspace)
        })
    })
}

/// Whether `f` holds for some variant of `adt`: its one for a struct or union.
pub(super) fn any_variant(db: &Db, adt: AdtId, mut f: impl FnMut(VariantId) -> bool) -> bool {
    match adt {
        AdtId::StructId(it) => f(VariantId::StructId(it)),
        AdtId::UnionId(it) => f(VariantId::UnionId(it)),
        AdtId::EnumId(it) => {
            it.enum_variants(db).variants.values().any(|&(id, _)| f(VariantId::EnumVariantId(id)))
        }
    }
}

/// std's `Global`, the allocator a collection's `A` defaults to, and `String`, which allocates
/// from it with no parameter naming it.
#[derive(Clone, Copy)]
pub(super) struct Std {
    global: Option<AdtId>,
    string: Option<AdtId>,
}

impl Std {
    pub(super) fn new(cx: &crate::check::Cx<'_, '_>) -> Std {
        let adt = |path: &[&str]| match cx.std_def(path) {
            Some(hir::ModuleDef::Adt(it)) => Some(AdtId::from(it)),
            _ => None,
        };
        Std {
            global: adt(&["alloc", "alloc", "Global"]).or_else(|| adt(&["std", "alloc", "Global"])),
            string: adt(&["alloc", "string", "String"]).or_else(|| adt(&["std", "string", "String"])),
        }
    }
}

/// The type a call of `callee` whose value has type `ty` makes, when it makes a value of the
/// global allocator's that has not allocated yet and will when it grows: one with `Global` as a
/// type argument (`Vec::new()`, `vec![]`, `VecDeque::default()`), `String::new()`, or std's
/// `HashMap` or `HashSet`, which take no allocator (one hashing with `RandomState` is
/// `std_hashmap`'s). A collection that names its allocator (`Vec::new_in(frame)`) is not one.
pub(super) fn makes_global(k: &super::Known<'_>, callee: FunctionId, ty: Ty<'_>) -> Option<AdtId> {
    let (adt, args) = ty.as_adt()?;
    let global = Some(adt) == k.alloc.string
        || (k.std.is_map_or_set(adt) && !k.std.is_random_map(ty))
        || args.types().any(|it| it.as_adt().is_some_and(|(it, _)| Some(it) == k.alloc.global));
    (global && super::hashmap::constructs(k, callee, adt)).then_some(adt)
}

/// Whether the call `id` grows a buffer its caller keeps: `f` takes `&mut self` and the
/// receiver is a place (`buf.push(x)`, `self.out.extend(..)`, `Vec::push(&mut buf, x)`), not
/// a temporary. The buffer then grows only past its capacity, which a reused buffer
/// reaches once.
pub(super) fn grows_kept_buffer(db: &Db, body: &Body, id: ExprId, f: FunctionId) -> bool {
    let takes_mut_self = hir::Function::from(f)
        .self_param(db)
        .is_some_and(|it| it.access(db) == hir::Access::Exclusive);
    if !takes_mut_self {
        return false;
    }
    let receiver = match body.expr(id) {
        Expr::MethodCall { receiver, .. } => receiver,
        Expr::Call { args: [first, ..], .. } => match body.expr(*first) {
            Expr::Ref { expr, mutability: Mutability::Mut, .. } => expr,
            _ => *first,
        },
        _ => return false,
    };
    is_place(body, receiver)
}

/// Whether `expr` names a place: a path, and fields, indices and dereferences of one.
fn is_place(body: &Body, mut expr: ExprId) -> bool {
    loop {
        match body.expr(expr) {
            Expr::Path(_) => return true,
            Expr::Field { expr: inner, .. }
            | Expr::Index { base: inner, .. }
            | Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } => expr = inner,
            _ => return false,
        }
    }
}

/// Whether `f` is a method of a trait, not of an impl.
pub(super) fn in_trait(db: &Db, f: FunctionId) -> bool {
    matches!(f.loc(db).container, ItemContainerId::TraitId(_))
}

#[cfg(test)]
mod tests {
    use super::fits;

    /// Stopping early answers as summing every walk did.
    #[hegel::test]
    fn fits_as_the_sum_of_the_walks_says(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let lengths: Vec<usize> = tc.draw(gs::vecs(gs::integers().max_value(79)).max_size(11));
        let longest: usize = tc.draw(gs::integers().max_value(299));
        let summed = lengths.iter().sum::<usize>() <= longest;
        assert_eq!(fits(lengths.iter().map(|&it| 0..it), longest), summed);
    }

    #[test]
    fn a_long_walk_is_left_at_the_first_node_past_the_limit() {
        let mut stepped = 0;
        let walk = std::iter::repeat(()).inspect(|()| stepped += 1);
        assert!(!fits(std::iter::once(walk), 256));
        assert_eq!(stepped, 257);
    }
}
