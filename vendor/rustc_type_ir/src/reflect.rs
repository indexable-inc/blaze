//! blaze: the field binders of compile-time reflection (`#![feature(blaze_comptime)]`,
//! docs/v2/plan/design/comptime.md, phase 2).
//!
//! `where for<F: Field<T>> F::Type: Hash` is an ordinary binder whose variable `F` is a
//! [`BoundVariableKind::Reflected`]: it ranges over the fields of `T` (the variable's
//! `container`), not over every type. The container rides in the binder's variable list, not
//! in a side table, so that it folds with the clause: the where clause of `hash_fields::<S>` is
//! the item's clause with `T := S`, and its variable's container must say `S` too.
//! `Binder`'s fold, visit and flag computation include the containers
//! ([`fold_reflected_containers`], [`visit_reflected_containers`]).
//!
//! A container is written inside its binder: it may name the binder's earlier variables at
//! `INNERMOST` (`for<V: Variant<T>, F: Field<V::View>>`), never its own or a later one.
//!
//! The solver proves such a binder by expanding it over the container's members
//! (`rustc_next_trait_solver`'s `compute_reflected_forall_goal`) and uses one in the
//! environment only through its members ([`instantiate_reflected_var`]), never by
//! instantiating `F` with an inference variable: nothing but a member may stand for `F`.

use derive_where::derive_where;
#[cfg(feature = "nightly")]
use rustc_macros::{Decodable_NoContext, Encodable_NoContext, StableHash_NoContext};
use rustc_type_ir_macros::{GenericTypeVisitable, Lift_Generic};
use smallvec::SmallVec;

use crate::fold::{FallibleTypeFolder, FnMutDelegate, TypeFoldable, TypeFolder};
use crate::inherent::*;
use crate::visit::{TypeVisitable, TypeVisitableExt, TypeVisitor, VisitorResult, try_visit};
use crate::{
    self as ty, Binder, BoundTyKind, BoundVar, BoundVariableKind, Const, Interner, Region,
};

/// What a reflected variable ranges over.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(
    feature = "nightly",
    derive(Encodable_NoContext, Decodable_NoContext, StableHash_NoContext)
)]
pub enum ReflectedDomain {
    /// `F: core::reflect::Field<C>`: the fields of the struct, tuple or variant view `C`, in
    /// declaration order, as their field representing types.
    Fields,
    /// `V: core::reflect::Variant<C>`: the variants of the enum `C`, in declaration order, as
    /// their variant representing types.
    Variants,
}

/// What `Interner::reflect_container_of` answers.
#[derive_where(Clone, Copy, PartialEq, Eq, Debug, Hash; I: Interner)]
pub enum ReflectContainer<I: Interner> {
    /// The member's container.
    Of(I::Ty),
    /// The type is no member of any container in this domain.
    Not,
    /// An inference variable (the base or an index) decides it.
    Unknown,
}

/// A variable of a field binder: `F` of `for<F: Field<C>>`.
#[derive_where(Clone, Copy, PartialEq, Eq, Debug, Hash; I: Interner)]
#[derive(Lift_Generic, GenericTypeVisitable)]
#[cfg_attr(
    feature = "nightly",
    derive(Encodable_NoContext, Decodable_NoContext, StableHash_NoContext)
)]
pub struct ReflectedVar<I: Interner> {
    /// The variable as a type variable: what a `ty::Bound` of it carries.
    pub kind: BoundTyKind<I>,
    #[lift(identity)]
    pub domain: ReflectedDomain,
    /// `C`, written inside the binder.
    pub container: I::Ty,
}

/// The first reflected variable of `vars`, with its index.
pub fn first_reflected_var<I: Interner>(
    vars: I::BoundVarKinds,
) -> Option<(usize, ReflectedVar<I>)> {
    vars.iter().enumerate().find_map(|(index, var)| match var {
        BoundVariableKind::Reflected(var) => Some((index, var)),
        _ => None,
    })
}

/// Whether any variable of `vars` is reflected.
#[inline]
pub fn has_reflected_vars<I: Interner>(vars: I::BoundVarKinds) -> bool {
    vars.iter().any(|var| matches!(var, BoundVariableKind::Reflected(_)))
}

/// `vars` with every reflected variable's container folded by `folder`, which is inside the
/// binder (`Binder::super_fold_with`). The list itself when it has none, the common case.
pub fn fold_reflected_containers<I: Interner, F: TypeFolder<I>>(
    vars: I::BoundVarKinds,
    folder: &mut F,
) -> I::BoundVarKinds {
    if !has_reflected_vars::<I>(vars) {
        return vars;
    }
    let folded = vars
        .iter()
        .map(|var| match var {
            BoundVariableKind::Reflected(var) => BoundVariableKind::Reflected(ReflectedVar {
                container: var.container.fold_with(folder),
                ..var
            }),
            var => var,
        })
        .collect::<SmallVec<[_; 4]>>();
    I::BoundVarKinds::from_vars(folder.cx(), folded)
}

/// [`fold_reflected_containers`] for a fallible folder.
pub fn try_fold_reflected_containers<I: Interner, F: FallibleTypeFolder<I>>(
    vars: I::BoundVarKinds,
    folder: &mut F,
) -> Result<I::BoundVarKinds, F::Error> {
    if !has_reflected_vars::<I>(vars) {
        return Ok(vars);
    }
    let folded = vars
        .iter()
        .map(|var| {
            Ok(match var {
                BoundVariableKind::Reflected(var) => BoundVariableKind::Reflected(ReflectedVar {
                    container: var.container.try_fold_with(folder)?,
                    ..var
                }),
                var => var,
            })
        })
        .collect::<Result<SmallVec<[_; 4]>, F::Error>>()?;
    Ok(I::BoundVarKinds::from_vars(folder.cx(), folded))
}

/// Visits every reflected variable's container of `vars` (`Binder::super_visit_with`).
pub fn visit_reflected_containers<I: Interner, V: TypeVisitor<I>>(
    vars: I::BoundVarKinds,
    visitor: &mut V,
) -> V::Result {
    for var in vars.iter() {
        if let BoundVariableKind::Reflected(var) = var {
            try_visit!(var.container.visit_with(visitor));
        }
    }
    V::Result::output()
}

/// `binder` with its variable `index` (a reflected one) replaced by `member`, which names no
/// variable of `binder`: the binder of the remaining variables, renumbered in order, with their
/// containers instantiated the same way. `for<V: Variant<T>, F: Field<V::View>> P(V, F)` with
/// `V := VariantRepresentingType<T, 1>` is
/// `for<F: Field<<VariantRepresentingType<T, 1> as Variant<T>>::View>> P(.., F)`; a binder with
/// no variable left is `Binder::dummy` of its value.
///
/// Both of the solver's rules over a field binder go through here: proving one expands it over
/// its container's members, and assuming one (a member `!F` of a generic container entering
/// the environment) instantiates it with the member.
pub fn instantiate_reflected_var<I: Interner, T: TypeFoldable<I>>(
    cx: I,
    binder: Binder<I, T>,
    index: usize,
    member: I::Ty,
) -> Binder<I, T> {
    let vars = binder.bound_vars();
    assert!(
        matches!(vars.get(index), Some(BoundVariableKind::Reflected(_))),
        "variable {index} of {vars:?} is not a reflected variable"
    );
    assert!(
        !member.has_escaping_bound_vars(),
        "the member {member:?} instantiating variable {index} of {vars:?} names a bound variable"
    );
    // The remaining variables keep their order: a variable after `index` moves down by one.
    let renumber = |var: BoundVar| {
        let at = var.as_usize();
        BoundVar::from_usize(if at > index { at - 1 } else { at })
    };
    let mut regions = |br: ty::BoundRegion<I>| {
        Region::new_bound(
            cx,
            ty::INNERMOST,
            ty::BoundRegion { var: renumber(br.var), kind: br.kind },
        )
    };
    let mut types = |bt: ty::BoundTy<I>| {
        if bt.var.as_usize() == index {
            member
        } else {
            I::Ty::new_bound(
                cx,
                ty::INNERMOST,
                ty::BoundTy { var: renumber(bt.var), kind: bt.kind },
            )
        }
    };
    let mut consts = |bc: ty::BoundConst<I>| {
        Const::new_bound(cx, ty::INNERMOST, ty::BoundConst::new(renumber(bc.var)))
    };
    let rest = vars
        .iter()
        .enumerate()
        .filter(|&(at, _)| at != index)
        .map(|(at, var)| match var {
            BoundVariableKind::Reflected(var) => {
                assert!(
                    at > index || !var.container.has_escaping_bound_vars(),
                    "the container of variable {at} of {vars:?} names a variable of its binder"
                );
                BoundVariableKind::Reflected(ReflectedVar {
                    container: replace(cx, var.container, &mut regions, &mut types, &mut consts),
                    ..var
                })
            }
            var => var,
        })
        .collect::<SmallVec<[_; 4]>>();
    let value = replace(cx, binder.skip_binder(), &mut regions, &mut types, &mut consts);
    Binder::bind_with_vars(value, I::BoundVarKinds::from_vars(cx, rest))
}

/// `value` with its variables bound at `INNERMOST` replaced as the three functions say.
fn replace<I: Interner, T: TypeFoldable<I>>(
    cx: I,
    value: T,
    regions: &mut dyn FnMut(ty::BoundRegion<I>) -> Region<I>,
    types: &mut dyn FnMut(ty::BoundTy<I>) -> I::Ty,
    consts: &mut dyn FnMut(ty::BoundConst<I>) -> Const<I>,
) -> T {
    crate::fold::replace_escaping_bound_vars_uncached(
        cx,
        value,
        FnMutDelegate { regions, types, consts },
    )
}
