//! blaze: refinements of an enum by a set of its variants, `Conn is Conn::Open { .. }`
//! (`#![blaze::pattern_types]`, docs/v2/plan/design/edition.md, section 8).
//!
//! A refinement is `TyKind::Pat(base, pattern)` whose pattern is a set of variants: one
//! `PatternKind::Variant`, or an `Or` of them sorted by index with none twice
//! ([`refine`] is the one place that makes one). The set of every variant is the base type
//! itself, never a pattern. A refinement:
//!
//! - has its base's layout, ABI and code (`layout_of_ty_in` returns the base's layout,
//!   `hir_ty_share::identity` spells it as its base, MIR locals hold the base, [`erase`]);
//! - is a subtype of its base and of a refinement by a larger set, never under invariance
//!   (`SolverRelating::tys`, `LatticeOp::tys`), so `&mut (T is PAT)` is not a `&mut T`;
//! - reads a field of its one variant directly (`infer::expr::lookup_field`), and a `match` on
//!   it needs only its variants (`hir_ty_matchck`'s `VariantVisibility::Excluded`).

use hir_def::{AdtId, EnumId, EnumVariantId};
use rustc_type_ir::{TypeFoldable, TypeFolder, TypeSuperFoldable, inherent::IntoKind};
use smallvec::SmallVec;

use crate::next_solver::{DbInterner, PatList, Pattern, PatternKind, Ty, TyKind};

/// Variant indices, in index order with none twice.
pub type Variants = SmallVec<[u32; 8]>;

/// The base and pattern of `ty` when it is a refinement by variants.
pub fn refinement<'db>(ty: Ty<'db>) -> Option<(Ty<'db>, Pattern<'db>)> {
    match ty.kind() {
        TyKind::Pat(base, pattern) if pattern.kind().is_variants() => Some((base, pattern)),
        _ => None,
    }
}

/// `ty` without its refinement by variants: the enum a `T is PAT` restricts.
pub fn base<'db>(ty: Ty<'db>) -> Ty<'db> {
    refinement(ty).map_or(ty, |(base, _)| base)
}

/// The variants of a set of variants, in index order.
pub fn variants(pattern: Pattern<'_>) -> Variants {
    match pattern.kind() {
        PatternKind::Variant { index } => SmallVec::from_slice(&[index]),
        PatternKind::Or(pats) => pats
            .iter()
            .filter_map(|it| match it.kind() {
                PatternKind::Variant { index } => Some(index),
                _ => None,
            })
            .collect(), // alloc: inline up to eight variants, a set's size past that
        PatternKind::Range { .. } | PatternKind::NotNull => SmallVec::new(),
    }
}

/// The enum `ty` is, through a refinement.
pub fn enum_of(ty: Ty<'_>) -> Option<EnumId> {
    match base(ty).kind() {
        TyKind::Adt(def, _) => match def.def_id() {
            AdtId::EnumId(it) => Some(it),
            _ => None,
        },
        _ => None,
    }
}

/// `base` refined to `variants` (any order, repeats allowed) of its `variant_count`: the
/// canonical form, sorted and deduplicated, and `base` itself when the set is every variant.
/// `None` for the empty set, which no type written with a pattern names.
pub fn refine<'db>(
    interner: DbInterner<'db>,
    base: Ty<'db>,
    mut variants: Variants,
    variant_count: usize,
) -> Option<Ty<'db>> {
    variants.sort_unstable();
    variants.dedup();
    let pattern = match *variants.as_slice() {
        [] => return None,
        _ if variants.len() >= variant_count => return Some(base),
        [index] => Pattern::new(interner, PatternKind::Variant { index }),
        _ => Pattern::new(
            interner,
            PatternKind::Or(PatList::new_from_iter(
                variants
                    .iter()
                    .map(|&index| Pattern::new(interner, PatternKind::Variant { index })),
            )),
        ),
    };
    Some(Ty::new(interner, TyKind::Pat(base, pattern)))
}

/// The one variant of a refinement by a single variant, `Conn is Conn::Open { .. }`, whose
/// fields a place of the type reads directly.
pub fn single_variant(db: &base_db::Db, ty: Ty<'_>) -> Option<EnumVariantId> {
    let (_, pattern) = refinement(ty)?;
    let PatternKind::Variant { index } = pattern.kind() else { return None };
    let enum_ = enum_of(ty)?;
    enum_.enum_variants(db).variants.get_index(index as usize).map(|(_, &(it, _))| it)
}

/// `value` with every refinement by variants replaced by its base: what MIR holds, as a
/// refinement has its base's layout and code.
pub fn erase<'db, T: TypeFoldable<DbInterner<'db>>>(interner: DbInterner<'db>, value: T) -> T {
    return value.fold_with(&mut Erase { interner });

    struct Erase<'db> {
        interner: DbInterner<'db>,
    }

    impl<'db> TypeFolder<DbInterner<'db>> for Erase<'db> {
        fn cx(&self) -> DbInterner<'db> {
            self.interner
        }

        fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
            match refinement(ty) {
                Some((base, _)) => base.fold_with(self),
                None => ty.super_fold_with(self),
            }
        }
    }
}
