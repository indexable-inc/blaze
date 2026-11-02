//! `hot_dyn`: a call that goes through a vtable.
//!
//! A method call or path call whose callee, as inference resolved it, is a trait's method with
//! `Self` a `dyn Trait` once references and boxes are peeled (`x.f()` with `x: &dyn Trait`,
//! `Trait::f(&*boxed)`), and a call of a `dyn Fn*` value through any pointer (`f(x)` with
//! `f: &dyn Fn(u32)`), which inference records as the `Fn*` method on the call. `dyn` in a type
//! is not a call and is not flagged.

use hir::HirDisplay as _;
use hir_def::FunctionId;
use hir_ty::next_solver::{GenericArgs, Ty};

use super::{Known, alloc};

/// The finding of a call of `callee` with `args`, if it goes through a vtable.
pub(super) fn finding<'db>(
    k: &Known<'db>,
    callee: FunctionId,
    args: GenericArgs<'db>,
) -> Option<String> {
    if !alloc::in_trait(k.db, callee) {
        return None;
    }
    let receiver = peel(args.types().next()?);
    receiver.dyn_trait()?;
    Some(format!("this call goes through the vtable of `{}`", receiver.display(k.db, k.target)))
}

/// `ty` without the references and boxes in front of it.
fn peel<'db>(mut ty: Ty<'db>) -> Ty<'db> {
    loop {
        let inner = ty.strip_references();
        match inner.boxed_ty() {
            Some(it) => ty = it,
            None => return inner,
        }
    }
}
