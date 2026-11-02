//! Compute the binary representation of structs, unions and enums

use std::cmp;

use base_db::Db;
use hir_def::{AdtId, VariantId, attrs::AttrFlags, signatures::StructFlags};
use rustc_abi::{
    BackendRepr, Float, Integer, NicheOptimizations, Primitive, ReprOptions, Scalar,
    TargetDataLayout, WrappingRange,
};
use rustc_index::IndexVec;
use blaze_hashcons::Interned;
use blaze_mem::Frame;

use crate::{
    consteval::discriminants::enum_discriminants,
    db::HirDatabase,
    layout::{
        InternedLayout, Layout, LayoutCx, LayoutError, RustcEnumVariantIdx, RustcFieldIdx,
        enum_variant_ids, intern_layout, struct_flags, struct_tail_may_be_unsized,
        variant_field_ids,
    },
    next_solver::{DbInterner, GenericArgs, StoredGenericArgs},
    representability::{Representability, representability},
    traits::StoredParamEnvAndCrate,
};

#[blaze_query::tracked(cycle_result = layout_of_adt_cycle_result, returns(clone), mem_tag = Infer)]
pub fn layout_of_adt_query(
    db: &Db,
    def: AdtId,
    args: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    blaze_mem::frame::current(|frame| layout_of_adt_in(db, def, args, trait_env, frame))
}

fn layout_of_adt_in(
    db: &Db,
    def: AdtId,
    args: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
    frame: &Frame,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let krate = trait_env.krate;
    let Ok(target) = db.target_data_layout(krate) else {
        return Err(LayoutError::TargetLayoutNotAvailable);
    };
    if representability(db, def) == Representability::Infinite {
        return Err(LayoutError::RecursiveTypeWithoutIndirection);
    }
    let dl = target;
    let cx = LayoutCx::new(dl);
    let args = args.as_ref();
    let variant_count = match def {
        AdtId::StructId(_) | AdtId::UnionId(_) => 1,
        AdtId::EnumId(e) => enum_variant_ids(db, e).len(),
    };
    // Every variant's field layouts in one buffer, variant `i` at `fields[ends[i - 1]..ends[i]]`.
    // `ends` is made at its size first, so that `fields`, the frame's last block, grows in place.
    let mut ends = Vec::with_capacity_in(variant_count, frame);
    let mut fields = Vec::new_in(frame);
    let mut push_variant = |variant: VariantId| -> Result<(), LayoutError> {
        variant_field_layouts(db, variant, args, &trait_env, &mut fields)?;
        ends.push(fields.len() as u32);
        Ok(())
    };
    let flags = match def {
        AdtId::StructId(s) => Some(struct_flags(db, s)),
        AdtId::UnionId(_) | AdtId::EnumId(_) => None,
    };
    let (repr, niche_optimizations, discriminants) = match def {
        AdtId::StructId(s) => {
            push_variant(s.into())?;
            // As rustc: `UnsafeCell` and `UnsafePinned` hide their fields' niches.
            let hides_niches = flags.is_some_and(|it| {
                it.intersects(StructFlags::IS_UNSAFE_CELL | StructFlags::IS_UNSAFE_PINNED)
            });
            let niches = if hides_niches {
                NicheOptimizations::Disabled
            } else {
                NicheOptimizations::Enabled
            };
            (AttrFlags::repr(db, s.into()).unwrap_or_default(), niches, None)
        }
        AdtId::UnionId(id) => {
            let repr = AttrFlags::repr(db, id.into());
            push_variant(id.into())?;
            (repr.unwrap_or_default(), NicheOptimizations::Enabled, None)
        }
        AdtId::EnumId(e) => {
            let variants = enum_variant_ids(db, e);
            for &v in variants.iter() {
                push_variant(v.into())?;
            }
            let repr = AttrFlags::repr(db, e.into()).unwrap_or_default();
            let discriminants = enum_discriminants(db, e);
            if let Some(index) = discriminants.iter().position(|it| it.is_err()) {
                return Err(LayoutError::DiscriminantFailed { variant: variants[index] });
            }
            (repr, NicheOptimizations::Enabled, Some(discriminants))
        }
    };
    let maybe_unsized = match def {
        AdtId::StructId(s) => struct_tail_may_be_unsized(db, s, krate),
        AdtId::UnionId(_) | AdtId::EnumId(_) => false,
    };
    let variants = variants_of(&fields, &ends);
    let mut result = if matches!(def, AdtId::UnionId(..)) {
        cx.calc.layout_of_union(&repr, &variants)?
    } else {
        // As rustc has a discriminant: the bits of its type, and no more.
        let discr_size = Integer::from_attr(dl, repr.discr_type()).size();
        cx.calc.layout_of_struct_or_enum(
            &repr,
            &variants,
            matches!(def, AdtId::EnumId(..)),
            niche_optimizations,
            // As rustc's `Integer::repr_discr`, a `#[repr]` too small for the values is a bug:
            // the values are truncated to it above.
            |min, max| {
                repr_discr(dl, &repr, min.start, max.last).unwrap_or_else(|err| {
                    panic!("`#[repr]` of {def:?} too small for its discriminants: {err}")
                })
            },
            discriminants.iter().flat_map(|it| it.iter()).enumerate().filter_map(|(index, d)| {
                Some((RustcEnumVariantIdx(index), discr_size.truncate(d.ok()? as u128)))
            }),
            !matches!(def, AdtId::EnumId(..)) && !maybe_unsized,
        )?
    };
    // As rustc's `layout_of_uncached`: the `f16b` and `ppcf128` lang items are structs over an
    // integer of their size, laid out as the backend's float scalar of that format.
    if let Some(flags) = flags {
        let float = if flags.contains(StructFlags::IS_F16B) {
            Some(Float::F16B)
        } else if flags.contains(StructFlags::IS_PPC_F128) {
            Some(Float::PpcF128)
        } else {
            None
        };
        if let Some(float) = float {
            assert_eq!(result.size, float.size());
            result.align = float.align(dl);
            result.backend_repr = BackendRepr::Scalar(Scalar::Initialized {
                value: Primitive::Float(float),
                valid_range: WrappingRange::full(float.size()),
            });
        }
    }
    Ok(intern_layout(result))
}

/// Pushes the layout of each field of `variant` onto `out`, in order, reading the variant's
/// field types once.
fn variant_field_layouts<'db>(
    db: &'db Db,
    variant: VariantId,
    args: GenericArgs<'db>,
    trait_env: &StoredParamEnvAndCrate,
    out: &mut Vec<&'static Layout, &Frame>,
) -> Result<(), LayoutError> {
    let interner = DbInterner::new_no_crate(db);
    let types = db.field_types(variant);
    let ids = variant_field_ids(db, variant);
    out.reserve(ids.len());
    for &fd in ids {
        let ty = types[fd].ty().instantiate(interner, args).skip_norm_wip();
        // clone: the env is two `u32` handles
        out.push(super::field_layout(db, ty.store(), trait_env.clone())?);
    }
    Ok(())
}

/// The fields of each variant as the layout calculator takes them: variant `i` is
/// `refs[ends[i - 1]..ends[i]]`. The calculator wants an owned `IndexVec` per variant; each is
/// one exact allocation, none for a variant without fields.
fn variants_of<'a, 'b>(
    refs: &'a [&'b Layout],
    ends: &[u32],
) -> IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, &'a &'b Layout>> {
    let mut variants = IndexVec::with_capacity(ends.len());
    let mut start = 0;
    for &end in ends {
        let end = end as usize;
        // alloc: the calculator takes an owned `IndexVec` per variant (R-abi lifts this)
        variants.push(refs[start..end].iter().collect());
        start = end;
    }
    variants
}

fn layout_of_adt_cycle_result(
    _: &Db,
    _: blaze_query::db::Id,
    _def: AdtId,
    _args: StoredGenericArgs,
    _trait_env: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    Err(LayoutError::RecursiveTypeWithoutIndirection)
}

/// Finds the appropriate Integer type and signedness for the given
/// signed discriminant range and `#[repr]` attribute.
/// N.B.: `u128` values above `i128::MAX` will be treated as signed, but
/// that shouldn't affect anything, other than maybe debuginfo.
fn repr_discr(
    dl: &TargetDataLayout,
    repr: &ReprOptions,
    min: i128,
    max: u128,
) -> Result<(Integer, bool), LayoutError> {
    // Theoretically, negative values could be larger in unsigned representation
    // than the unsigned representation of the signed minimum. However, if there
    // are any negative values, the only valid unsigned representation is u128
    // which can fit all i128 values, so the result remains unaffected.
    let unsigned_fit = Integer::fit_unsigned(cmp::max(min as u128, max));
    let signed_fit = cmp::max(Integer::fit_signed(min), Integer::fit_signed(max as i128));

    if let Some(ity) = repr.int {
        let discr = Integer::from_attr(dl, ity);
        let fit = if ity.is_signed() { signed_fit } else { unsigned_fit };
        if discr < fit {
            return Err(LayoutError::UserReprTooSmall);
        }
        return Ok((discr, ity.is_signed()));
    }

    let at_least = if repr.c() {
        // This is usually I32, however it can be different on some platforms,
        // notably hexagon and arm-none/thumb-none
        dl.c_enum_min_size
    } else {
        // repr(Rust) enums try to be as small as possible
        Integer::I8
    };

    // `min` and `max` are the ends of a wrapping range, so their sign is not a usable test.
    Ok(if unsigned_fit <= signed_fit {
        (cmp::max(unsigned_fit, at_least), false)
    } else {
        (cmp::max(signed_fit, at_least), true)
    })
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;
    use crate::layout::tests::{sized_fields, unsized_field};

    type Variants<'a, 'b> = IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, &'a &'b Layout>>;

    /// What `layout_of_adt` did before: per variant a `Vec` of layouts, a `Vec` of references
    /// and an `IndexVec`.
    fn nested_with_vecs(
        variants: &[Vec<&Layout>],
        layout: impl FnOnce(&Variants<'_, '_>) -> Result<Layout, LayoutError>,
    ) -> Result<Layout, LayoutError> {
        let variants = variants.iter().map(|it| it.to_vec()).collect::<Vec<_>>();
        let variants = variants.iter().map(|it| it.iter().collect()).collect::<IndexVec<_, _>>();
        layout(&variants)
    }

    fn flat(
        variants: &[Vec<&Layout>],
        layout: impl FnOnce(&Variants<'_, '_>) -> Result<Layout, LayoutError>,
    ) -> Result<Layout, LayoutError> {
        blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let mut ends = Vec::with_capacity_in(variants.len(), frame);
            let mut fields = Vec::new_in(frame);
            for variant in variants {
                fields.extend(variant.iter().copied());
                ends.push(fields.len() as u32);
            }
            layout(&variants_of(&fields, &ends))
        })
    }

    #[test]
    fn one_flat_buffer_matches_a_buffer_per_variant() {
        let dl = TargetDataLayout::default();
        let cx = LayoutCx::new(&dl);
        let pool = sized_fields(&dl);
        let tail = unsized_field(&dl);
        let repr = ReprOptions::default();
        let discr = |min: std::range::RangeFrom<i128>, max: std::range::RangeToInclusive<u128>| {
            repr_discr(&dl, &repr, min.start, max.last).unwrap_or((Integer::I8, false))
        };
        let mut seed = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for case in 0..400 {
            let mut field = || &pool[seed.index(pool.len())];
            let mut seed_len = SplitMix64::new(case as u64);
            let count = match case % 3 {
                // An enum: up to six variants, a variant without fields among them.
                0 => seed_len.index(7),
                // A struct or a union.
                _ => 1,
            };
            let mut variants: Vec<Vec<&Layout>> = (0..count)
                .map(|_| (0..seed_len.index(6)).map(|_| field()).collect())
                .collect();
            let unsized_tail = case % 3 == 2 && seed_len.below(2) == 0;
            if unsized_tail {
                variants[0].push(&tail);
            }
            let is_enum = case % 3 == 0;
            let struct_or_enum = |variants: &Variants<'_, '_>| -> Result<Layout, LayoutError> {
                Ok(cx.calc.layout_of_struct_or_enum(
                    &repr,
                    variants,
                    is_enum,
                    NicheOptimizations::Enabled,
                    discr,
                    (0..variants.len()).map(|it| (RustcEnumVariantIdx(it), it as u128)),
                    !is_enum && !unsized_tail,
                )?)
            };
            assert_eq!(
                flat(&variants, struct_or_enum),
                nested_with_vecs(&variants, struct_or_enum),
                "case {case}: {count} variants",
            );
            if case % 3 == 1 {
                let union = |variants: &Variants<'_, '_>| -> Result<Layout, LayoutError> {
                    Ok(cx.calc.layout_of_union(&repr, variants)?)
                };
                assert_eq!(flat(&variants, union), nested_with_vecs(&variants, union), "case {case}");
            }
        }
    }
}
