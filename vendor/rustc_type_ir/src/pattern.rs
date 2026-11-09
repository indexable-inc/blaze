use derive_where::derive_where;
#[cfg(feature = "nightly")]
use rustc_macros::{Decodable_NoContext, Encodable_NoContext, StableHash_NoContext};
use rustc_type_ir_macros::{
    GenericTypeVisitable, Lift_Generic, TypeFoldable_Generic, TypeVisitable_Generic,
};

use crate::inherent::{IntoKind, SliceLike};
use crate::{Const, Interner};

#[derive_where(Clone, Copy, Hash, PartialEq; I: Interner)]
#[derive(TypeVisitable_Generic, GenericTypeVisitable, TypeFoldable_Generic, Lift_Generic)]
#[cfg_attr(
    feature = "nightly",
    derive(Decodable_NoContext, Encodable_NoContext, StableHash_NoContext)
)]
pub enum PatternKind<I: Interner> {
    Range { start: Const<I>, end: Const<I> },
    Or(I::PatList),
    NotNull,
    /// blaze: the variant of the base enum at `index` (`#![blaze::pattern_types]`,
    /// docs/v2/plan/design/edition.md, section 8). A set of variants is one `Variant`, or an
    /// `Or` of several sorted by index with none twice; the set of every variant is never a
    /// pattern, it is the base type itself. The layout, ABI and code identity of a variant
    /// refinement are its base enum's.
    Variant {
        #[lift(identity)]
        index: u32,
    },
}

impl<I: Interner> Eq for PatternKind<I> {}

impl<I: Interner> PatternKind<I> {
    /// blaze: whether this pattern is a set of enum variants, one `Variant` or an `Or` of them.
    pub fn is_variants(self) -> bool {
        match self {
            PatternKind::Variant { .. } => true,
            PatternKind::Or(pats) => {
                !pats.is_empty()
                    && pats.iter().all(|it| matches!(it.kind(), PatternKind::Variant { .. }))
            }
            PatternKind::Range { .. } | PatternKind::NotNull => false,
        }
    }

    /// blaze: whether the variant at `index` is in this set of variants (false for a pattern
    /// that is not one, [`PatternKind::is_variants`]).
    pub fn admits_variant(self, index: u32) -> bool {
        match self {
            PatternKind::Variant { index: it } => it == index,
            PatternKind::Or(pats) => pats
                .iter()
                .any(|pat| matches!(pat.kind(), PatternKind::Variant { index: it } if it == index)),
            PatternKind::Range { .. } | PatternKind::NotNull => false,
        }
    }

    /// blaze: whether this set of variants is within `other`'s, so a value of this
    /// refinement is one of `other` and widens to it for free.
    pub fn variants_within(self, other: PatternKind<I>) -> bool {
        if !self.is_variants() || !other.is_variants() {
            return false;
        }
        match self {
            PatternKind::Variant { index } => other.admits_variant(index),
            PatternKind::Or(pats) => pats.iter().all(|it| match it.kind() {
                PatternKind::Variant { index } => other.admits_variant(index),
                _ => false,
            }),
            PatternKind::Range { .. } | PatternKind::NotNull => false,
        }
    }
}
