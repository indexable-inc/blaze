//! Where generic code may drop a linear value unseen.
//!
//! A function names its callee with type arguments; the callee's body is checked once, generic
//! over its parameters, where a parameter is linear only when it is marked. So a type argument
//! that holds a linear value ([`Types::reaches`]) is refused for a parameter that is not marked
//! `#[blaze::linear]`: its code may drop a value of it. A trait method is checked as the impl's
//! method it resolves to, whose own parameters are the ones that matter; a call of a closure,
//! a function item or a function pointer through `Fn*` goes into a body checked itself; a
//! constructor of a type with no `Drop` impl only builds the value, which is checked where it is
//! dropped. An impl that binds an associated type to a linear type is refused when the trait's
//! associated type is not marked: code generic over the trait drops values of it.

use hir_def::{
    AssocItemId, CallableDefId, DefWithBodyId, GenericDefId, GenericParamId, ItemContainerId,
    Lookup, attrs::AttrFlags, hir::generics::GenericParams,
};
use hir_ty::{
    ParamEnvAndCrate, TyDefId,
    db::HirDatabase,
    drop::destructor,
    next_solver::{GenericArg, GenericArgs, Ty, TyKind},
};
use hir_ty_mir::mir::{MirBody, MirSpan, Operand, OperandKind};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};

use crate::{LinearDiagnostic, names, ty::Types};

/// Errors for each function `body` names with a type argument that holds a linear value for a
/// parameter that does not admit one.
pub(crate) fn check_calls<'db>(
    types: &Types<'_, 'db>,
    body: &MirBody<'db>,
    out: &mut Vec<LinearDiagnostic>,
) {
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            statement.kind.for_each_operand(&mut |operand| {
                check_operand(types, body, operand, statement.span, out)
            });
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut |operand| {
                check_operand(types, body, operand, terminator.span, out)
            });
        }
    }
}

fn check_operand<'db>(
    types: &Types<'_, 'db>,
    body: &MirBody<'db>,
    operand: &Operand,
    at: MirSpan,
    out: &mut Vec<LinearDiagnostic>,
) {
    let Some((callable, args)) = operand.fn_def() else { return };
    if !args.types().any(|it| types.reaches(it)) {
        return;
    }
    let callee = match &operand.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        _ => return,
    };
    let span = operand.span.unwrap_or(at);
    let db = types.db;
    let (def, args) = match callable {
        CallableDefId::FunctionId(func) => match resolve(types, func, args) {
            Some(it) => it,
            None => return,
        },
        CallableDefId::StructId(id) => {
            if destructor(db, id.into()).is_none() {
                return;
            }
            (GenericDefId::AdtId(id.into()), args)
        }
        CallableDefId::EnumVariantId(id) => {
            let parent = id.loc(db).parent;
            if destructor(db, parent.into()).is_none() {
                return;
            }
            (GenericDefId::AdtId(parent.into()), args)
        }
    };
    let interner = types.interner;
    let mut params = Vec::new_in(types.frame);
    GenericArgs::for_item(interner, def.into(), |_, id, _, _| {
        params.push(id);
        GenericArg::error_from_id(interner, id)
    });
    for (&param, arg) in params.iter().zip(args.iter()) {
        let GenericParamId::TypeParamId(param) = param else { continue };
        let Some(ty) = arg.ty() else { continue };
        if !types.reaches(ty)
            || AttrFlags::query_type_or_const_param(db, param.into()).contains(AttrFlags::LINEAR)
        {
            continue;
        }
        let name = GenericParams::of(db, param.parent())[param.local_id()]
            .name()
            .map_or("Self", |it| it.as_str())
            .to_owned(); // alloc: error path
        let shown = names::ty(db, body, ty);
        let callee = names::ty(db, body, callee);
        out.push(LinearDiagnostic {
            span,
            message: format!(
                "`{shown}` holds a `#[blaze::linear]` value and is given to `{callee}` as its type \
                 parameter `{name}`, which does not admit linear types: its code may drop a \
                 value of it"
            ),
            labels: Vec::new(), // alloc: error path, empty
            notes: vec![
                // alloc: error path
                format!(
                    "a parameter admits linear types when it is marked `#[blaze::linear]` \
                     (`<#[blaze::linear] {name}>`), and its code is then checked to consume \
                     every value of it"
                ),
            ],
        });
    }
}

/// What a call of `func` with `args` runs, as the item whose parameters are checked: the impl's
/// method a trait method resolves to, or `func`. `None` when the call goes into a body checked
/// on its own: a closure's, a function item's or a function pointer's through `Fn*`, or a
/// method a derive writes (which takes `&self` and drops no value of `Self`).
fn resolve<'db>(
    types: &Types<'_, 'db>,
    func: hir_def::FunctionId,
    args: GenericArgs<'db>,
) -> Option<(GenericDefId, GenericArgs<'db>)> {
    let db = types.db;
    let ItemContainerId::TraitId(trait_) = func.lookup(db).container else {
        return Some((func.into(), args));
    };
    let lang = types.interner.lang_items();
    let callable_traits =
        [lang.Fn, lang.FnMut, lang.FnOnce, lang.AsyncFn, lang.AsyncFnMut, lang.AsyncFnOnce];
    if callable_traits.contains(&Some(trait_))
        && matches!(
            args.type_at(0).kind(),
            TyKind::Closure(..)
                | TyKind::CoroutineClosure(..)
                | TyKind::FnDef(..)
                | TyKind::FnPtr(..)
        )
    {
        return None;
    }
    let env = ParamEnvAndCrate { param_env: types.env, krate: types.krate };
    match db.lookup_impl_method(env, func, args) {
        (either::Either::Left(method), args) => Some((method.into(), args)),
        (either::Either::Right(_), _) => None,
    }
}

/// Errors for an impl, reported at the body of its first function, that binds an associated
/// type to a type holding a linear value where the trait's associated type is not marked.
pub(crate) fn check_impl_bindings<'db>(
    types: &Types<'_, 'db>,
    def: DefWithBodyId,
    out: &mut Vec<LinearDiagnostic>,
) {
    let db = types.db;
    let DefWithBodyId::FunctionId(func) = def else { return };
    let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return };
    let items = &impl_.impl_items(db).items;
    let first = items.iter().find_map(|&(_, it)| match it {
        AssocItemId::FunctionId(it) => Some(it),
        _ => None,
    });
    if first != Some(func) {
        return;
    }
    let Some(trait_ref) = db.impl_trait(impl_) else { return };
    let trait_ = trait_ref.skip_binder().def_id.0;
    for (name, item) in items.iter() {
        let &AssocItemId::TypeAliasId(alias) = item else { continue };
        let ty: Ty<'db> = db.ty(TyDefId::TypeAliasId(alias)).instantiate_identity().skip_norm_wip();
        if !types.holds(ty) {
            continue;
        }
        let admits = trait_
            .trait_items(db)
            .associated_type_by_name(name)
            .is_some_and(|it| AttrFlags::query(db, it.into()).contains(AttrFlags::LINEAR));
        if admits {
            continue;
        }
        let trait_name =
            hir_def::signatures::TraitSignature::of(db, trait_).name.as_str().to_owned(); // alloc: error path
        let name = name.as_str();
        let shown = names::ty_in(db, types.krate, ty);
        out.push(LinearDiagnostic {
            span: MirSpan::Unknown,
            message: format!(
                "this impl of `{trait_name}` binds `{name}` to `{shown}`, which holds a \
                 `#[blaze::linear]` value, but `{trait_name}::{name}` does not admit linear \
                 types: code generic over `{trait_name}` may drop a value of it"
            ),
            labels: Vec::new(), // alloc: error path, empty
            notes: vec![format!(
                // alloc: error path
                "an associated type admits linear types when it is marked \
                 `#[blaze::linear] type {name};` in the trait"
            )],
        });
    }
}
