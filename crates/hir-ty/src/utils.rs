//! Helper functions for working with def, which don't need to be a separate
//! query, but can't be computed directly from `*Data` (ie, which need a `db`).

use std::{borrow::Cow, iter::Enumerate};

use base_db::Db;
use base_db::target::{self, TargetData};
use hir_def::{
    EnumId, EnumVariantId, FunctionId, Lookup, TraitId, expr_store::ExpressionStore,
    hir::PatId, lang_item::LangItems, signatures::FunctionSignature,
};
use rustc_abi::TargetDataLayout;
use rustc_ast_ir::Mutability;
use span::Edition;

use crate::{
    BindingMode, ByRef, TargetFeatures,
    layout::{Layout, RustcEnumVariantIdx, TagEncoding},
    lower::SupertraitsInfo,
    mir::{IsSigned, pad16},
};

pub fn fn_traits(lang_items: &LangItems) -> impl Iterator<Item = TraitId> + '_ {
    [lang_items.Fn, lang_items.FnMut, lang_items.FnOnce].into_iter().flatten()
}

/// Returns an iterator over the direct super traits (including the trait itself).
pub fn direct_super_traits(db: &Db, trait_: TraitId) -> &[TraitId] {
    SupertraitsInfo::query(db, trait_).direct_supertraits()
}

/// Returns the whole super trait hierarchy (including the trait itself).
pub fn all_super_traits(db: &Db, trait_: TraitId) -> &[TraitId] {
    SupertraitsInfo::query(db, trait_).all_supertraits()
}

/// rustc's `pat_has_ref_mut_binding`: whether `pat` binds anything by `ref mut`, by the binding
/// modes typeck gave (`mode`), so match ergonomics' adjustments count.
pub fn pat_has_ref_mut_binding(
    store: &ExpressionStore,
    pat: PatId,
    mode: impl Fn(PatId) -> Option<BindingMode>,
) -> bool {
    let mut found = false;
    store.walk_pats(pat, &mut |pat| {
        found |= matches!(mode(pat), Some(BindingMode(ByRef::Yes(Mutability::Mut), _)));
    });
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsafety {
    Safe,
    Unsafe,
    /// A lint.
    DeprecatedSafe2024,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFeatureIsSafeInTarget {
    No,
    Yes,
}

pub fn target_feature_is_safe_in_target(target: &TargetData) -> TargetFeatureIsSafeInTarget {
    match target.arch {
        target::Arch::Wasm32 | target::Arch::Wasm64 => TargetFeatureIsSafeInTarget::Yes,
        _ => TargetFeatureIsSafeInTarget::No,
    }
}

pub fn is_fn_unsafe_to_call(
    db: &Db,
    func: FunctionId,
    caller_target_features: &TargetFeatures<'_>,
    call_edition: Edition,
    target_feature_is_safe: TargetFeatureIsSafeInTarget,
) -> Unsafety {
    let data = FunctionSignature::of(db, func);
    // Before `is_unsafe`: a `#[rustc_deprecated_safe_2024]` function is also `unsafe`, yet a call
    // of it before 2024 is only linted.
    if data.is_deprecated_safe_2024() {
        if call_edition.at_least_2024() {
            return Unsafety::Unsafe;
        } else {
            return Unsafety::DeprecatedSafe2024;
        }
    }

    if data.is_unsafe() {
        return Unsafety::Unsafe;
    }

    if data.has_target_feature() && target_feature_is_safe == TargetFeatureIsSafeInTarget::No {
        // RFC 2396 <https://rust-lang.github.io/rfcs/2396-target-feature-1.1.html>.
        let callee_target_features = TargetFeatures::from_fn_no_implications(db, func);
        if !caller_target_features.is_superset(&callee_target_features) {
            return Unsafety::Unsafe;
        }
    }

    let loc = func.lookup(db);
    match loc.container {
        hir_def::ItemContainerId::ExternBlockId(_) => {
            // Function in an `extern` block are always unsafe to call, except when
            // it is marked as `safe`.
            if data.is_safe() { Unsafety::Safe } else { Unsafety::Unsafe }
        }
        _ => Unsafety::Safe,
    }
}

pub fn detect_variant_from_bytes<'a>(
    layout: &'a Layout,
    db: &Db,
    target_data_layout: &TargetDataLayout,
    b: &[u8],
    e: EnumId,
) -> Option<(EnumVariantId, Cow<'a, Layout>)> {
    let (var_id, var_layout) = match &layout.variants {
        hir_def::layout::Variants::Empty => unreachable!(),
        hir_def::layout::Variants::Single { index } => {
            (e.enum_variants(db).variants[index.0].0, Cow::Borrowed(layout))
        }
        hir_def::layout::Variants::Multiple { tag, tag_encoding, variants, .. } => {
            let tag_size = tag.size(target_data_layout);
            let signed = tag.is_signed();
            let size = tag_size.bytes_usize();
            let offset = layout.fields.offset(0).bytes_usize(); // The only field on enum variants is the tag field
            let tag = i128::from_le_bytes(pad16(&b[offset..offset + size], IsSigned::No));
            match tag_encoding {
                TagEncoding::Direct => {
                    // A direct tag is the discriminant, read as signed when its type is (a
                    // `#[repr(i8)]` variant `= -1`), as the evaluator's `Discriminant` reads it.
                    let tag = match signed {
                        true => {
                            i128::from_le_bytes(pad16(&b[offset..offset + size], IsSigned::Yes))
                        }
                        false => tag,
                    };
                    let index =
                        crate::consteval::discriminants::enum_discriminants(db, e).variant_of(tag)?;
                    let var_idx = RustcEnumVariantIdx(index);
                    assert!(var_idx.0 < variants.len());
                    let def = e.enum_variants(db).variants[index].0;
                    (def, Cow::Owned(Layout::for_variant(layout, var_idx)))
                }
                TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                    // Compute the variant this niche value/"tag" corresponds to. With niche layout,
                    // discriminant (encoded in niche/tag) and variant index are the same.
                    let variants_start = niche_variants.start.0;
                    let variants_end = niche_variants.last.0;

                    // Match rustc's Sub, which uses overflowing_sub().
                    // https://github.com/rust-lang/rust/blob/e023590de407f417e0f3da675a372eca7acf60c6/compiler/rustc_const_eval/src/interpret/operator.rs#L256
                    let (variant_index_relative_val, _) =
                        (tag as u128).overflowing_sub(*niche_start);

                    let variant_index_relative = tag_size.truncate(variant_index_relative_val);
                    // Check if this is in the range that indicates an actual discriminant.
                    let variant =
                        if variant_index_relative <= (variants_end - variants_start) as u128 {
                            let variant_index_relative = usize::try_from(variant_index_relative)
                                .expect("we checked that this fits into a usize");
                            // Then computing the absolute variant idx should not overflow any more.
                            let variant_index = RustcEnumVariantIdx(
                                variants_start
                                    .checked_add(variant_index_relative)
                                    .expect("overflow computing absolute variant idx"),
                            );
                            assert!(variant_index.0 < variants.len());
                            if variant_index == *untagged_variant {
                                // The untagged variant can be in the niche range, but even then it
                                // is not a valid encoding.
                                return None;
                            }
                            variant_index
                        } else {
                            *untagged_variant
                        };
                    // Reading the discriminant of an uninhabited variant is UB. This is the basis for the
                    // `uninhabited_enum_branching` MIR pass. It also ensures consistency with
                    // `write_discriminant`.
                    if variants[variant].is_uninhabited() {
                        return None;
                    }
                    (
                        e.enum_variants(db).variants[variant.0].0,
                        Cow::Owned(Layout::for_variant(layout, variant)),
                    )
                }
            }
        }
    };
    Some((var_id, var_layout))
}

pub(crate) struct EnumerateAndAdjust<I> {
    enumerate: Enumerate<I>,
    gap_pos: usize,
    gap_len: usize,
}

impl<I> Iterator for EnumerateAndAdjust<I>
where
    I: Iterator,
{
    type Item = (usize, <I as Iterator>::Item);

    fn next(&mut self) -> Option<(usize, <I as Iterator>::Item)> {
        self.enumerate
            .next()
            .map(|(i, elem)| (if i < self.gap_pos { i } else { i + self.gap_len }, elem))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.enumerate.size_hint()
    }
}

pub(crate) trait EnumerateAndAdjustIterator {
    fn enumerate_and_adjust(
        self,
        expected_len: usize,
        gap_pos: Option<u32>,
    ) -> EnumerateAndAdjust<Self>
    where
        Self: Sized;
}

impl<T: ExactSizeIterator> EnumerateAndAdjustIterator for T {
    fn enumerate_and_adjust(
        self,
        expected_len: usize,
        gap_pos: Option<u32>,
    ) -> EnumerateAndAdjust<Self>
    where
        Self: Sized,
    {
        let actual_len = self.len();
        EnumerateAndAdjust {
            enumerate: self.enumerate(),
            gap_pos: gap_pos.map(|it| it as usize).unwrap_or(expected_len),
            gap_len: expected_len - actual_len,
        }
    }
}
