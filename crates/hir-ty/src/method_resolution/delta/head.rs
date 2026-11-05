//! A simplified self type with no database lifetime: the key of an inherent slice and of a trait
//! slice's rows. It is `fast_reject::SimplifiedType` over [`HeadDef`], so it converts both ways
//! with `SimplifiedType<'db>` variant for variant, and the tables built from it hash and compare
//! the converted type exactly as `TraitImpls` and `InherentImpls` do.

use std::fmt;

use hir_def::{AdtId, TraitId, TypeAliasId};
use rustc_type_ir::fast_reject::SimplifiedType as S;

use crate::next_solver::{SimplifiedType, SolverDefId};

/// What a simplified type of an impl header can name: an ADT, a trait (`dyn Trait`) or an extern
/// type. A closure, coroutine or other solver-made def is never in a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HeadDef {
    Adt(AdtId),
    Trait(TraitId),
    Alias(TypeAliasId),
}

/// A simplified self type, lifetime-free.
pub type Head = S<HeadDef>;

const _: () = assert!(size_of::<Head>() <= 24);

fn def_of(def: SolverDefId<'_>) -> Option<HeadDef> {
    match def {
        SolverDefId::AdtId(it) => Some(HeadDef::Adt(it)),
        SolverDefId::TraitId(it) => Some(HeadDef::Trait(it)),
        SolverDefId::TypeAliasId(it) => Some(HeadDef::Alias(it)),
        _ => None,
    }
}

fn solver_def<'db>(def: HeadDef) -> SolverDefId<'db> {
    match def {
        HeadDef::Adt(it) => SolverDefId::AdtId(it),
        HeadDef::Trait(it) => SolverDefId::TraitId(it),
        HeadDef::Alias(it) => SolverDefId::TypeAliasId(it),
    }
}

/// The head of `ty`, or `None` when it names a def no impl header can name (a closure or a
/// coroutine): no slice has such a head.
pub fn try_head(ty: &SimplifiedType<'_>) -> Option<Head> {
    Some(match *ty {
        S::Bool => S::Bool,
        S::Char => S::Char,
        S::Int(it) => S::Int(it),
        S::Uint(it) => S::Uint(it),
        S::Float(it) => S::Float(it),
        S::Adt(def) => S::Adt(def_of(def)?),
        S::Foreign(def) => S::Foreign(def_of(def)?),
        S::Str => S::Str,
        S::Array => S::Array,
        S::Slice => S::Slice,
        S::Ref(it) => S::Ref(it),
        S::Ptr(it) => S::Ptr(it),
        S::Never => S::Never,
        S::Tuple(n) => S::Tuple(n),
        S::MarkerTraitObject => S::MarkerTraitObject,
        S::Trait(def) => S::Trait(def_of(def)?),
        S::Closure(_) | S::Coroutine(_) | S::CoroutineWitness(_) => return None,
        S::Function(n) => S::Function(n),
        S::UnsafeBinder => S::UnsafeBinder,
        S::Placeholder => S::Placeholder,
        S::Error => S::Error,
    })
}

/// The head of an impl header's simplified self type. An impl header names no closure or
/// coroutine, so one here is a bug in lowering, reported with the impl.
pub fn head_of(ty: &SimplifiedType<'_>, of: impl fmt::Debug) -> Head {
    try_head(ty).unwrap_or_else(|| {
        panic!("impls: the self type of {of:?} simplifies to {ty:?}, which no impl header names")
    })
}

/// The `SimplifiedType<'db>` `head` stands for.
pub fn simplified<'db>(head: Head) -> SimplifiedType<'db> {
    match head {
        S::Bool => S::Bool,
        S::Char => S::Char,
        S::Int(it) => S::Int(it),
        S::Uint(it) => S::Uint(it),
        S::Float(it) => S::Float(it),
        S::Adt(def) => S::Adt(solver_def(def)),
        S::Foreign(def) => S::Foreign(solver_def(def)),
        S::Str => S::Str,
        S::Array => S::Array,
        S::Slice => S::Slice,
        S::Ref(it) => S::Ref(it),
        S::Ptr(it) => S::Ptr(it),
        S::Never => S::Never,
        S::Tuple(n) => S::Tuple(n),
        S::MarkerTraitObject => S::MarkerTraitObject,
        S::Trait(def) => S::Trait(solver_def(def)),
        S::Closure(def) => S::Closure(solver_def(def)),
        S::Coroutine(def) => S::Coroutine(solver_def(def)),
        S::CoroutineWitness(def) => S::CoroutineWitness(solver_def(def)),
        S::Function(n) => S::Function(n),
        S::UnsafeBinder => S::UnsafeBinder,
        S::Placeholder => S::Placeholder,
        S::Error => S::Error,
    }
}
