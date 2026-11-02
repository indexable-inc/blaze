//! `rustc_pattern_analysis`'s `PatCx` over hir-ty's types: rustc's `RustcPatCtxt`
//! (`rustc_pattern_analysis/src/rustc.rs`) with hir-ty's inhabitedness and field types.

use std::{
    cell::{Cell, RefCell},
    fmt,
};

use base_db::{Db, Crate};
use hir_def::{
    AdtId, EnumId, EnumVariantId, HasModule, LocalFieldId, ModuleId, VariantId, attrs::AttrFlags,
    hir::PatId, signatures::VariantFields,
};
use intern::Symbol;
use rustc_pattern_analysis::{
    IndexVec, PatCx, PrivateUninhabitedField,
    constructor::{
        Constructor, ConstructorSet, IntRange, MaybeInfiniteInt, RangeEnd, VariantVisibility,
    },
    pat::DeconstructedPat,
    usefulness::{PlaceValidity, UsefulnessReport, compute_match_usefulness},
};
use rustc_type_ir::{IntTy, UintTy, inherent::IntoKind};
use smallvec::{SmallVec, smallvec};

use hir_ty::{
    consteval::try_const_usize,
    db::HirDatabase,
    inhabitedness::{is_enum_variant_uninhabited_from, is_ty_uninhabited_from},
    next_solver::{
        GenericArgs, ParamEnv, Ty, TyKind,
        infer::{InferCtxt, traits::ObligationCause},
    },
};

use Constructor::*;

pub(crate) type Pat<'a, 'db> = DeconstructedPat<Cx<'a, 'db>>;
pub(crate) type Witness<'a, 'db> = rustc_pattern_analysis::pat::WitnessPat<Cx<'a, 'db>>;
pub(crate) type Arm<'p, 'a, 'db> = rustc_pattern_analysis::MatchArm<'p, Cx<'a, 'db>>;
pub(crate) type Report<'p, 'a, 'db> = UsefulnessReport<'p, Cx<'a, 'db>>;

/// rustc's `pattern_complexity_limit` default.
const COMPLEXITY_LIMIT: usize = 10_000_000;

/// A variant's index in its enum, as `rustc_pattern_analysis` indexes variants.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct VariantIdx(pub(crate) u32);

impl rustc_pattern_analysis::Idx for VariantIdx {
    fn new(idx: usize) -> Self {
        VariantIdx(idx as u32)
    }

    fn index(self) -> usize {
        self.0 as usize
    }
}

impl VariantIdx {
    pub(crate) fn of(db: &Db, variant: EnumVariantId) -> Self {
        VariantIdx(variant.index(db) as u32)
    }

    pub(crate) fn id(self, db: &Db, enum_: EnumId) -> EnumVariantId {
        enum_.enum_variants(db).variants[self.0 as usize].0
    }
}

/// Why the analysis stopped: rustc's `ErrorGuaranteed` of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CxError {
    /// "reached pattern complexity limit".
    Complexity,
    /// A type the analysis can't take apart: an error type, or a pattern of a type that
    /// doesn't fit it, which type checking has reported.
    Bug,
    /// A deref pattern and a normal constructor matched on one place.
    MixedDerefPatterns { deref_pat: PatId, normal_pat: PatId },
}

/// A range lint the analysis raised while it ran, for the visitor to report once the
/// analysis is kept.
#[derive(Debug, Clone)]
pub(crate) enum RangeLint<'db> {
    /// `lint_overlapping_range_endpoints`.
    Overlap { pat: PatId, ty: Ty<'db>, on: IntRange, with: Box<[PatId]> },
    /// `lint_non_contiguous_range_endpoints`: `pat` is the range `range`.
    Gap { pat: PatId, ty: Ty<'db>, range: IntRange, gap: IntRange, with: Box<[PatId]> },
}

/// The facts of an integer type a range is read with: rustc's `Integer::size` and sign.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IntInfo {
    pub(crate) bits: u64,
    pub(crate) signed: bool,
    pub(crate) ptr_sized: bool,
}

pub(crate) struct Cx<'a, 'db> {
    pub(crate) db: &'db Db,
    pub(crate) infcx: &'a InferCtxt<'db>,
    pub(crate) env: ParamEnv<'db>,
    pub(crate) module: ModuleId,
    pub(crate) krate: Crate,
    pub(crate) pointer_bits: u64,
    pub(crate) exhaustive_patterns: bool,
    pub(crate) deref_patterns: bool,
    /// An uninhabited witness is printed `!`, not `_`.
    pub(crate) never_patterns: bool,
    /// Whether a deref pattern was lowered in the match being checked.
    pub(crate) has_deref_pat: Cell<bool>,
    /// What the analysis running now linted.
    pub(crate) range_lints: RefCell<Vec<RangeLint<'db>>>,
}

impl fmt::Debug for Cx<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cx").finish_non_exhaustive()
    }
}

impl<'a, 'db> Cx<'a, 'db> {
    /// `analyze_match` without the `non_exhaustive_omitted_patterns` lint, which is allow by
    /// default and unstable.
    pub(crate) fn analyze<'p>(
        &self,
        arms: &[Arm<'p, 'a, 'db>],
        scrut_ty: Ty<'db>,
        known_valid_scrutinee: bool,
    ) -> Result<Report<'p, 'a, 'db>, CxError> {
        let validity = PlaceValidity::from_bool(known_valid_scrutinee);
        compute_match_usefulness(self, arms, scrut_ty, validity, COMPLEXITY_LIMIT)
    }

    pub(crate) fn is_uninhabited(&self, ty: Ty<'db>) -> bool {
        is_ty_uninhabited_from(self.infcx, ty, self.module, self.env)
    }

    /// `variant_list_has_applicable_non_exhaustive`: an enum of another crate marked
    /// `#[non_exhaustive]`.
    pub(crate) fn is_foreign_non_exhaustive_enum(&self, ty: Ty<'db>) -> bool {
        match ty.kind() {
            TyKind::Adt(def, _) => match def.def_id() {
                AdtId::EnumId(id) => {
                    id.krate(self.db) != self.krate
                        && AttrFlags::query(self.db, id.into())
                            .contains(AttrFlags::NON_EXHAUSTIVE)
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub(crate) fn int_info(&self, ty: Ty<'db>) -> Option<IntInfo> {
        Some(match ty.kind() {
            TyKind::Char => IntInfo { bits: 32, signed: false, ptr_sized: false },
            TyKind::Int(int) => match int {
                IntTy::Isize => IntInfo { bits: self.pointer_bits, signed: true, ptr_sized: true },
                _ => IntInfo { bits: int.bit_width()?, signed: true, ptr_sized: false },
            },
            TyKind::Uint(uint) => match uint {
                UintTy::Usize => {
                    IntInfo { bits: self.pointer_bits, signed: false, ptr_sized: true }
                }
                _ => IntInfo { bits: uint.bit_width()?, signed: false, ptr_sized: false },
            },
            _ => return None,
        })
    }

    /// The analysis' integer of `bits`, the value's bits truncated to its size.
    pub(crate) fn int_of_bits(info: IntInfo, bits: u128) -> MaybeInfiniteInt {
        if info.signed {
            MaybeInfiniteInt::new_finite_int(bits, info.bits)
        } else {
            MaybeInfiniteInt::new_finite_uint(bits)
        }
    }

    /// `is_range_beyond_boundaries`: the fictitious values before `isize::MIN` or after
    /// `usize::MAX`/`isize::MAX`.
    pub(crate) fn is_range_beyond_boundaries(&self, range: &IntRange, ty: Ty<'db>) -> bool {
        self.int_info(ty).is_some_and(|info| {
            info.ptr_sized
                && (crate::print::hoist(info, range.lo) == crate::print::Bound::PosInfinity
                    || matches!(range.hi, MaybeInfiniteInt::Finite { 0: 0, .. }))
        })
    }

    /// The fields of `variant` of the ADT `ty`, with their types normalized.
    pub(crate) fn variant_fields(
        &self,
        args: GenericArgs<'db>,
        variant: VariantId,
    ) -> impl Iterator<Item = (LocalFieldId, Ty<'db>)> + use<'a, 'db> {
        let field_tys = self.db.field_types(variant);
        let len = variant.fields(self.db).fields().len() as u32;
        let infcx = self.infcx;
        let env = self.env;
        (0..len).map(move |idx| {
            let fid = LocalFieldId::from_raw(idx.into());
            let ty = field_tys[fid].ty().instantiate(infcx.interner, args).skip_norm_wip();
            // As rustc's `normalize_erasing_regions`: a body checked here has no type error.
            let ty = infcx
                .at(&ObligationCause::dummy(), env)
                .deeply_normalize(ty)
                .unwrap_or_else(|_| panic!("failed to normalize field {fid:?} of {variant:?}: {ty:?}"));
            (fid, ty)
        })
    }

    pub(crate) fn variant_of(&self, ctor: &Constructor<Self>, adt: AdtId) -> Option<VariantId> {
        match (ctor, adt) {
            (Variant(idx), AdtId::EnumId(id)) => Some(idx.id(self.db, id).into()),
            (Struct | UnionField, AdtId::StructId(id)) => Some(id.into()),
            (Struct | UnionField, AdtId::UnionId(id)) => Some(id.into()),
            _ => None,
        }
    }

    fn ctor_sub_tys_inner(
        &self,
        ctor: &Constructor<Self>,
        ty: Ty<'db>,
    ) -> SmallVec<[(Ty<'db>, PrivateUninhabitedField); 2]> {
        let visible = |ty| (ty, PrivateUninhabitedField(false));
        match ctor {
            Struct | Variant(_) | UnionField => match ty.kind() {
                // alloc: a `SmallVec` of two inline, as `ctor_sub_tys` returns.
                TyKind::Tuple(tys) => tys.iter().map(visible).collect(),
                TyKind::Adt(def, args) => {
                    let adt = def.def_id();
                    let Some(variant) = self.variant_of(ctor, adt) else { return SmallVec::new() };
                    let is_enum = matches!(adt, AdtId::EnumId(_));
                    let visibilities =
                        (!is_enum).then(|| VariantFields::field_visibilities(self.db, variant));
                    // alloc: as above.
                    self.variant_fields(args, variant)
                        .map(|(fid, ty)| {
                            let is_visible = visibilities
                                .as_ref()
                                .is_none_or(|vis| vis[fid].is_visible_from(self.db, self.module));
                            let skip = !is_visible && self.is_uninhabited(ty);
                            (ty, PrivateUninhabitedField(skip))
                        })
                        .collect() // alloc: as above.
                }
                _ => SmallVec::new(),
            },
            Ref => match ty.kind() {
                // alloc: as above.
                TyKind::Ref(_, pointee, _) => smallvec![visible(pointee)],
                _ => SmallVec::new(),
            },
            Slice(slice) => match ty.builtin_index() {
                // alloc: as above.
                Some(elem) => (0..slice.arity()).map(|_| visible(elem)).collect(),
                None => SmallVec::new(),
            },
            // alloc: as above.
            DerefPattern(pointee) => smallvec![visible(*pointee)],
            Bool(..) | IntRange(..) | F16Range(..) | F32Range(..) | F64Range(..)
            | F128Range(..) | Str(..) | Opaque(..) | Never | NonExhaustive | Hidden | Missing
            | PrivateUninhabited | Wildcard | Or => SmallVec::new(),
        }
    }
}

impl<'a, 'db> PatCx for Cx<'a, 'db> {
    type Ty = Ty<'db>;
    type Error = CxError;
    type VariantIdx = VariantIdx;
    type StrLit = Symbol;
    /// The arm's index in its match.
    type ArmData = u32;
    /// The pattern the deconstructed one is lowered from: a constant's whole pattern for the
    /// parts of its value, the adjusted pattern for its implicit dereferences.
    type PatData = PatId;

    fn is_exhaustive_patterns_feature_on(&self) -> bool {
        self.exhaustive_patterns
    }

    fn ctor_arity(&self, ctor: &Constructor<Self>, ty: &Self::Ty) -> usize {
        match ctor {
            Struct | Variant(_) | UnionField => match ty.kind() {
                TyKind::Tuple(tys) => tys.len(),
                TyKind::Adt(def, _) => self
                    .variant_of(ctor, def.def_id())
                    .map_or(0, |variant| variant.fields(self.db).fields().len()),
                _ => 0,
            },
            Ref | DerefPattern(_) => 1,
            Slice(slice) => slice.arity(),
            Bool(..) | IntRange(..) | F16Range(..) | F32Range(..) | F64Range(..)
            | F128Range(..) | Str(..) | Opaque(..) | Never | NonExhaustive | Hidden | Missing
            | PrivateUninhabited | Wildcard | Or => 0,
        }
    }

    fn ctor_sub_tys(
        &self,
        ctor: &Constructor<Self>,
        ty: &Self::Ty,
    ) -> impl Iterator<Item = (Self::Ty, PrivateUninhabitedField)> + ExactSizeIterator {
        self.ctor_sub_tys_inner(ctor, *ty).into_iter()
    }

    fn ctors_for_ty(&self, ty: &Self::Ty) -> Result<ConstructorSet<Self>, Self::Error> {
        let ty = *ty;
        let uint_range = |lo: u128, hi: u128| {
            IntRange::from_range(
                MaybeInfiniteInt::new_finite_uint(lo),
                MaybeInfiniteInt::new_finite_uint(hi),
                RangeEnd::Included,
            )
        };
        Ok(match ty.kind() {
            TyKind::Bool => ConstructorSet::Bool,
            TyKind::Char => ConstructorSet::Integers {
                range_1: uint_range('\u{0000}' as u128, '\u{D7FF}' as u128),
                range_2: Some(uint_range('\u{E000}' as u128, '\u{10FFFF}' as u128)),
            },
            TyKind::Int(_) | TyKind::Uint(_) => {
                let info = self.int_info(ty).ok_or(CxError::Bug)?;
                let range = if info.ptr_sized {
                    // The bounds of `isize` and the max of `usize` are not observed.
                    let lo = if info.signed {
                        MaybeInfiniteInt::NegInfinity
                    } else {
                        MaybeInfiniteInt::new_finite_uint(0)
                    };
                    IntRange { lo, hi: MaybeInfiniteInt::PosInfinity }
                } else if info.signed {
                    let min = 1u128 << (info.bits - 1);
                    IntRange::from_range(
                        MaybeInfiniteInt::new_finite_int(min, info.bits),
                        MaybeInfiniteInt::new_finite_int(min - 1, info.bits),
                        RangeEnd::Included,
                    )
                } else {
                    uint_range(0, u128::MAX >> (128 - info.bits))
                };
                ConstructorSet::Integers { range_1: range, range_2: None }
            }
            TyKind::Slice(elem) => {
                ConstructorSet::Slice { array_len: None, subtype_is_empty: self.is_uninhabited(elem) }
            }
            TyKind::Array(elem, len) => ConstructorSet::Slice {
                // An array of a constant but unknown length is matched as a slice.
                array_len: try_const_usize(self.db, len).map(|len| len as usize),
                subtype_is_empty: self.is_uninhabited(elem),
            },
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::EnumId(enum_) => {
                    let variants = &enum_.enum_variants(self.db).variants;
                    let non_exhaustive = self.is_foreign_non_exhaustive_enum(ty);
                    if variants.is_empty() && !non_exhaustive {
                        ConstructorSet::NoConstructors
                    } else {
                        let foreign = enum_.krate(self.db) != self.krate;
                        let mut visibilities = IndexVec::with_capacity(variants.len());
                        for &(variant, _) in variants.values() {
                            let uninhabited = is_enum_variant_uninhabited_from(
                                self.infcx,
                                variant,
                                args,
                                self.module,
                                self.env,
                            );
                            let hidden = foreign && {
                                let flags = AttrFlags::query(self.db, variant.into());
                                flags.contains(AttrFlags::IS_DOC_HIDDEN)
                                    || flags.contains(AttrFlags::IS_UNSTABLE)
                            };
                            visibilities.push(if uninhabited {
                                VariantVisibility::Empty
                            } else if hidden {
                                VariantVisibility::Hidden
                            } else {
                                VariantVisibility::Visible
                            });
                        }
                        ConstructorSet::Variants { variants: visibilities, non_exhaustive }
                    }
                }
                AdtId::UnionId(_) => ConstructorSet::Union,
                AdtId::StructId(_) => ConstructorSet::Struct { empty: self.is_uninhabited(ty) },
            },
            TyKind::Tuple(..) => ConstructorSet::Struct { empty: self.is_uninhabited(ty) },
            TyKind::Ref(..) => ConstructorSet::Ref,
            TyKind::Never => ConstructorSet::NoConstructors,
            TyKind::Error(_) | TyKind::Infer(_) | TyKind::Bound(..) | TyKind::Placeholder(_) => {
                return Err(CxError::Bug);
            }
            _ => ConstructorSet::Unlistable,
        })
    }

    fn write_variant_name(
        f: &mut fmt::Formatter<'_>,
        ctor: &Constructor<Self>,
        _ty: &Self::Ty,
    ) -> fmt::Result {
        // Only `Debug` of a pattern asks this, which has no database to name the variant with.
        match ctor {
            Variant(idx) => write!(f, "#{}", idx.0),
            _ => Ok(()),
        }
    }

    fn bug(&self, _fmt: fmt::Arguments<'_>) -> Self::Error {
        CxError::Bug
    }

    fn complexity_exceeded(&self) -> Result<(), Self::Error> {
        Err(CxError::Complexity)
    }

    fn match_may_contain_deref_pats(&self) -> bool {
        self.has_deref_pat.get()
    }

    fn report_mixed_deref_pat_ctors(
        &self,
        deref_pat: &DeconstructedPat<Self>,
        normal_pat: &DeconstructedPat<Self>,
    ) -> Self::Error {
        CxError::MixedDerefPatterns { deref_pat: *deref_pat.data(), normal_pat: *normal_pat.data() }
    }

    fn lint_overlapping_range_endpoints(
        &self,
        pat: &DeconstructedPat<Self>,
        overlaps_on: IntRange,
        overlaps_with: &[&DeconstructedPat<Self>],
    ) {
        self.range_lints.borrow_mut().push(RangeLint::Overlap {
            pat: *pat.data(),
            ty: *pat.ty(),
            on: overlaps_on,
            // alloc: the patterns of one lint, which the analysis raises only where it fires.
            with: overlaps_with.iter().map(|p| *p.data()).collect(),
        });
    }

    fn lint_non_contiguous_range_endpoints(
        &self,
        pat: &DeconstructedPat<Self>,
        gap: IntRange,
        gapped_with: &[&DeconstructedPat<Self>],
    ) {
        let IntRange(range) = pat.ctor() else { return };
        self.range_lints.borrow_mut().push(RangeLint::Gap {
            pat: *pat.data(),
            ty: *pat.ty(),
            range: *range,
            gap,
            // alloc: as above.
            with: gapped_with.iter().map(|p| *p.data()).collect(),
        });
    }
}
