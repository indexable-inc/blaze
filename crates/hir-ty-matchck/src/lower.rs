//! rustc's pattern lowering (`rustc_mir_build/src/thir/pattern/mod.rs`, `PatCtxt::lower_pattern`,
//! and `const_to_pat.rs`) folded into `RustcPatCtxt::lower_pat`: a pattern of the body goes
//! straight to the analysis' `DeconstructedPat`, with no THIR pattern between. A constant is
//! evaluated and its value taken apart by its type's layout, as `const_to_pat` takes apart a
//! valtree.

use hir_ty::db::HirDatabase;
use std::{cell::Cell, ops::ControlFlow};

use base_db::Db;

use hir_def::{
    AdtId, BuiltinDeriveImplId, ConstId, DefWithBodyId, HasModule, TraitId, VariantId,
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplTrait,
    expr_store::{Body, HygieneId, path::Path, scope::ExprScopes},
    hir::{Expr, ExprId, ExprOrPatId, Literal, Pat as HirPat, PatId, RangeOp, UnaryOp},
    resolver::{Resolver, ValueNs, resolver_for_scope},
    signatures::ImplSignature,
};
use intern::Symbol;
use rustc_apfloat::{
    Float,
    ieee::{Double, Half, Quad, Single},
};
use rustc_pattern_analysis::{
    constructor::{Constructor, IntRange, MaybeInfiniteInt, OpaqueId, RangeEnd, Slice, SliceKind},
    pat::{DeconstructedPat, IndexedPat},
};
use rustc_type_ir::{
    FloatTy, Interner as _, TraitRef, TypeSuperVisitable, TypeVisitable, TypeVisitor,
    inherent::IntoKind,
};
use smallvec::{SmallVec, smallvec};

use hir_ty::{
    BindingMode, ByRef, InferenceResult, MemoryMap, ParamEnvAndCrate, Span,
    consteval::try_const_usize,
    infer::PatAdjust,
    method_resolution::CandidateId,
    mir::{IsSigned, pad16},
    next_solver::{
        Allocation, AnyImplId, DbInterner, GenericArgs, StoredTy, Ty, TyKind, Unnormalized,
        infer::traits::{Obligation, ObligationCause},
    },
    traits::{implements_trait_unique, implements_trait_unique_with_args},
    utils::detect_variant_from_bytes,
};

use crate::{
    ConstPatternError, ManualPartialEq,
    cx::{Cx, VariantIdx},
};

pub(crate) type Pat<'a, 'db> = DeconstructedPat<Cx<'a, 'db>>;

/// Why a pattern was not lowered. rustc reports each and the match is not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LowerError {
    /// An error type checking or const evaluation reports: a pattern of an erroneous type, a
    /// path that resolves to no pattern, a constant that does not evaluate.
    Failed,
    /// E0030 or E0579.
    EmptyRange { pat: PatId, inclusive: bool },
    /// `const_to_pat` refused the constant `pat` names.
    ConstPattern { pat: PatId, errors: SmallVec<[ConstPatternError; 1]> },
}

/// What `const_to_pat` refuses in one constant's value: the one error of making its valtree,
/// which is reported alone, else each error of `valtree_to_pat`.
#[derive(Default)]
struct Refusals {
    valtree: Option<ConstPatternError>,
    pat: SmallVec<[ConstPatternError; 1]>,
}

impl Refusals {
    fn valtree(&mut self, error: ConstPatternError) {
        self.valtree.get_or_insert(error);
    }

    /// `refused`: a part of the value around this one was refused, and `valtree_to_pat` does
    /// not look inside it.
    fn pat(&mut self, refused: bool, error: ConstPatternError) {
        if !refused {
            self.pat.push(error);
        }
    }
}

/// rustc's `PartialEqImplStatus` of an ADT, from `type_has_partial_eq_impl`.
struct PartialEqStatus {
    /// A `PartialEq` impl applies to the type.
    has_impl: bool,
    /// The last impl for the type, `non_blanket_impl`, is derived.
    is_derived: bool,
    non_blanket_impl: Option<AnyImplId>,
    /// A `StructuralPartialEq` impl matches the type, its bounds aside.
    possibly_inapplicable_structural: bool,
    /// A derived `PartialEq` impl matches the type, its bounds aside.
    possibly_inapplicable_derived: bool,
}

/// `extend_type_not_partial_eq`'s visitor: the ADTs in a type, by why they keep it from being
/// `PartialEq`.
struct PartialEqFacts<'l, 'c, 'a, 'db> {
    lowerer: &'l Lowerer<'c, 'a, 'db>,
    local_manual: Vec<ManualPartialEq>,
    local_without: Vec<AdtId>,
    manual: Vec<Ty<'db>>,
    without: Vec<Ty<'db>>,
}

fn push_new<T: PartialEq>(list: &mut Vec<T>, item: T) {
    if !list.contains(&item) {
        list.push(item); // alloc: a refused constant's notes, made once the error fires.
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for PartialEqFacts<'_, '_, '_, 'db> {
    type Result = ControlFlow<()>;

    fn visit_ty(&mut self, ty: Ty<'db>) -> ControlFlow<()> {
        match ty.kind() {
            TyKind::Dynamic(..) | TyKind::UnsafeBinder(..) => return ControlFlow::Break(()),
            TyKind::FnPtr(..) => return ControlFlow::Continue(()),
            TyKind::Adt(def, _) => {
                let cx = self.lowerer.cx;
                let adt = def.def_id();
                let local_adt = adt.krate(cx.db) == cx.krate;
                let status = self.lowerer.partial_eq_status(ty, adt);
                let manual = status.has_impl && !status.is_derived;
                match status.non_blanket_impl {
                    _ if status.possibly_inapplicable_structural => {}
                    Some(AnyImplId::ImplId(impl_)) if manual && impl_.krate(cx.db) == cx.krate => {
                        push_new(&mut self.local_manual, ManualPartialEq::Impl(impl_))
                    }
                    _ if manual && local_adt => {
                        push_new(&mut self.local_manual, ManualPartialEq::Adt(adt))
                    }
                    _ if !status.has_impl && local_adt => push_new(&mut self.local_without, adt),
                    _ if manual => push_new(&mut self.manual, ty),
                    _ if !status.has_impl => push_new(&mut self.without, ty),
                    _ => {}
                }
            }
            _ => {}
        }
        ty.super_visit_with(self)
    }
}

/// The longest tuple core implements `PartialEq` for (`tuple_impls!`).
const MAX_TUPLE_PARTIAL_EQ: usize = 12;

/// How deep a constant's value is taken apart before it is called erroneous: a value has no
/// cycles, so only a broken memory map gets this deep.
const MAX_VALUE_DEPTH: u32 = 256;

pub(crate) struct Lowerer<'c, 'a, 'db> {
    pub(crate) cx: &'c Cx<'a, 'db>,
    pub(crate) body: &'c Body,
    pub(crate) infer: &'db InferenceResult<'db>,
    pub(crate) owner: DefWithBodyId,
    /// An expression the patterns being lowered are in the scope of, to resolve a path to a
    /// constant from: the `match`, the `let`'s initializer or block.
    pub(crate) scope: Cell<ExprId>,
}

/// A bound of a range pattern, before it is checked against the other.
#[derive(Clone, Copy)]
enum RangeBound {
    Int(MaybeInfiniteInt),
    Float(u128),
}

fn fields_of<'a, 'db>(pats: impl IntoIterator<Item = Pat<'a, 'db>>) -> Vec<IndexedPat<Cx<'a, 'db>>> {
    // alloc: `DeconstructedPat::new` takes its fields as a `Vec`; one per lowered pattern.
    pats.into_iter().enumerate().map(|(i, p)| p.at_index(i)).collect()
}

fn read_uint(bytes: &[u8]) -> Option<u128> {
    (bytes.len() <= 16).then(|| u128::from_le_bytes(pad16(bytes, IsSigned::No)))
}

fn truncate(bits: u128, size: u64) -> u128 {
    if size >= 128 { bits } else { bits & ((1u128 << size) - 1) }
}

impl<'c, 'a, 'db> Lowerer<'c, 'a, 'db> {
    pub(crate) fn new(
        cx: &'c Cx<'a, 'db>,
        body: &'c Body,
        infer: &'db InferenceResult<'db>,
        owner: DefWithBodyId,
    ) -> Self {
        Lowerer { cx, body, infer, owner, scope: Cell::new(body.root_expr()) }
    }

    fn new_pat(
        ctor: Constructor<Cx<'a, 'db>>,
        fields: Vec<IndexedPat<Cx<'a, 'db>>>,
        arity: usize,
        ty: Ty<'db>,
        src: PatId,
    ) -> Pat<'a, 'db> {
        DeconstructedPat::new(ctor, fields, arity, ty, src)
    }

    fn leaf(ctor: Constructor<Cx<'a, 'db>>, ty: Ty<'db>, src: PatId) -> Pat<'a, 'db> {
        Self::new_pat(ctor, Vec::new(), 0, ty, src) // alloc: empty, so none.
    }

    fn opaque(ty: Ty<'db>, src: PatId) -> Pat<'a, 'db> {
        Self::leaf(Constructor::Opaque(OpaqueId::new()), ty, src)
    }

    /// `lower_pattern`: the pattern with the dereferences match ergonomics put around it,
    /// outermost first.
    pub(crate) fn lower(&self, pat: PatId) -> Result<Pat<'a, 'db>, LowerError> {
        let mut lowered = self.lower_unadjusted(pat)?;
        if let Some(adjustments) = self.infer.pat_adjustment(pat) {
            for adjustment in adjustments.iter().rev() {
                let ty = adjustment.source.as_ref();
                let ctor = match adjustment.kind {
                    PatAdjust::BuiltinDeref => Constructor::Ref,
                    PatAdjust::OverloadedDeref => {
                        self.cx.has_deref_pat.set(true);
                        Constructor::DerefPattern(*lowered.ty())
                    }
                };
                // alloc: the fields `DeconstructedPat::new` takes.
                lowered = Self::new_pat(ctor, vec![lowered.at_index(0)], 1, ty, pat);
            }
        }
        Ok(lowered)
    }

    fn pat_ty(&self, pat: PatId) -> Result<Ty<'db>, LowerError> {
        let ty = self.infer.pat_ty(pat);
        if ty.references_non_lt_error() { Err(LowerError::Failed) } else { Ok(ty) }
    }

    fn lower_unadjusted(&self, pat: PatId) -> Result<Pat<'a, 'db>, LowerError> {
        let db = self.cx.db;
        let mut ty = self.pat_ty(pat)?;
        Ok(match self.body.pat(pat) {
            HirPat::Wild | HirPat::Rest | HirPat::Expr(_) | HirPat::NotNull => {
                Self::leaf(Constructor::Wildcard, ty, pat)
            }
            // Lowering leaves a pattern it could not read (an inline `const` block among them)
            // missing.
            HirPat::Missing => return Err(LowerError::Failed),
            HirPat::Bind { subpat, .. } => {
                // A `ref x` is of type `&T`; the place it binds, which is what it matches, is `T`.
                if let Some(BindingMode(ByRef::Yes(_), _)) = self.infer.binding_mode(pat)
                    && let TyKind::Ref(_, pointee, _) = ty.kind()
                {
                    ty = pointee;
                }
                match subpat {
                    Some(subpat) => return self.lower(subpat),
                    None => Self::leaf(Constructor::Wildcard, ty, pat),
                }
            }
            HirPat::Tuple { args, ellipsis } => {
                let TyKind::Tuple(tys) = ty.kind() else { return Err(LowerError::Failed) };
                let arity = tys.len();
                let fields = self.lower_tuple_fields(args, arity, ellipsis)?;
                Self::new_pat(Constructor::Struct, fields, arity, ty, pat)
            }
            HirPat::TupleStruct { args, ellipsis, .. } => {
                let variant =
                    self.infer.variant_resolution_for_pat(pat).ok_or(LowerError::Failed)?;
                let arity = variant.fields(db).fields().len();
                let fields = self.lower_tuple_fields(args, arity, ellipsis)?;
                self.variant_pat(pat, ty, variant, fields, arity)?
            }
            HirPat::Record { args, .. } => {
                let variant =
                    self.infer.variant_resolution_for_pat(pat).ok_or(LowerError::Failed)?;
                let variant_fields = variant.fields(db);
                let arity = variant_fields.fields().len();
                let mut fields = Vec::with_capacity(args.len());
                for field in args {
                    let fid = variant_fields.field(&field.name).ok_or(LowerError::Failed)?;
                    let idx = u32::from(fid.into_raw()) as usize;
                    fields.push(self.lower(field.pat)?.at_index(idx));
                }
                self.variant_pat(pat, ty, variant, fields, arity)?
            }
            HirPat::Path(path) => match self.infer.variant_resolution_for_pat(pat) {
                Some(variant) => {
                    let arity = variant.fields(db).fields().len();
                    self.variant_pat(pat, ty, variant, Vec::new(), arity)? // alloc: empty.
                }
                None => {
                    let (konst, args) = self
                        .path_const(pat.into(), path, self.body.pat_path_hygiene(pat))
                        .ok_or(LowerError::Failed)?;
                    self.const_pat(pat, ty, konst, args)?
                }
            },
            HirPat::Lit(expr) => self.lit_expr_pat(pat, ty, expr)?,
            HirPat::Range { start, end, range_type } => {
                self.range_pat(pat, ty, start, end, range_type)?
            }
            HirPat::Slice { prefix, slice, suffix } => {
                let array_len = match ty.kind() {
                    TyKind::Array(_, len) => Some(
                        try_const_usize(db, len).ok_or(LowerError::Failed)? as usize,
                    ),
                    TyKind::Slice(_) => None,
                    _ => return Err(LowerError::Failed),
                };
                let kind = if slice.is_some() {
                    SliceKind::VarLen(prefix.len(), suffix.len())
                } else {
                    SliceKind::FixedLen(prefix.len() + suffix.len())
                };
                let mut lowered = Vec::with_capacity(prefix.len() + suffix.len());
                for &p in prefix.iter().chain(suffix) {
                    lowered.push(self.lower(p)?);
                }
                let fields = fields_of(lowered);
                Self::new_pat(Constructor::Slice(Slice::new(array_len, kind)), fields, kind.arity(), ty, pat)
            }
            HirPat::Ref { pat: inner, .. } => {
                // A `&` that edition 2024's match ergonomics ate against an inherited reference.
                if self.infer.is_skipped_ref_pat(pat) {
                    return self.lower(inner);
                }
                if !matches!(ty.kind(), TyKind::Ref(..)) {
                    return Err(LowerError::Failed);
                }
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![self.lower(inner)?.at_index(0)], 1, ty, pat)
            }
            HirPat::Box { inner } | HirPat::Deref { inner } => {
                self.cx.has_deref_pat.set(true);
                let field = self.lower(inner)?;
                let inner_ty = *field.ty();
                let field = field.at_index(0);
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::DerefPattern(inner_ty), vec![field], 1, ty, pat)
            }
            HirPat::Or(_) => {
                // alloc: only for an or-pattern, which becomes this many fields.
                let mut leaves = Vec::new();
                self.or_leaves(pat, &mut leaves);
                let mut lowered = Vec::with_capacity(leaves.len());
                for leaf in leaves {
                    lowered.push(self.lower(leaf)?);
                }
                let arity = lowered.len();
                Self::new_pat(Constructor::Or, fields_of(lowered), arity, ty, pat)
            }
        })
    }

    /// `expand_or_pat`: the alternatives of nested or-patterns, flattened.
    fn or_leaves(&self, pat: PatId, out: &mut Vec<PatId>) {
        match self.body.pat(pat) {
            HirPat::Or(alts) if self.infer.pat_adjustment(pat).is_none() => {
                for &alt in alts {
                    self.or_leaves(alt, out);
                }
            }
            _ => out.push(pat),
        }
    }

    /// `lower_tuple_subpats` with `enumerate_and_adjust`: the fields after `..` are counted
    /// from the end.
    fn lower_tuple_fields(
        &self,
        args: &[PatId],
        arity: usize,
        ellipsis: Option<u32>,
    ) -> Result<Vec<IndexedPat<Cx<'a, 'db>>>, LowerError> {
        if args.len() > arity {
            return Err(LowerError::Failed);
        }
        let gap = arity - args.len();
        let ellipsis = ellipsis.map_or(usize::MAX, |at| at as usize);
        let mut fields = Vec::with_capacity(args.len());
        for (i, &arg) in args.iter().enumerate() {
            let idx = if i < ellipsis { i } else { i + gap };
            fields.push(self.lower(arg)?.at_index(idx));
        }
        Ok(fields)
    }

    fn variant_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        variant: VariantId,
        fields: Vec<IndexedPat<Cx<'a, 'db>>>,
        arity: usize,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        let ctor = match (ty.kind(), variant) {
            (TyKind::Adt(..), VariantId::EnumVariantId(v)) => {
                Constructor::Variant(VariantIdx::of(self.cx.db, v))
            }
            (TyKind::Adt(def, _), VariantId::UnionId(_))
                if matches!(def.def_id(), AdtId::UnionId(_)) =>
            {
                Constructor::UnionField
            }
            (TyKind::Adt(..), VariantId::StructId(_)) => Constructor::Struct,
            _ => return Err(LowerError::Failed),
        };
        Ok(Self::new_pat(ctor, fields, arity, ty, pat))
    }

    /// The resolver at [`Self::scope`]. Made for each path to a constant, which few patterns
    /// have; inference keeps no resolution of a path to a free constant.
    fn resolver(&self) -> Resolver<'db> {
        let db = self.cx.db;
        let scope = ExprScopes::of(db, self.owner).scope_for(self.scope.get());
        resolver_for_scope(db, self.owner, scope)
    }

    /// What a path expression at [`Self::scope`] names in the value namespace.
    pub(crate) fn resolve_value(&self, path: Path<'_>, hygiene: HygieneId) -> Option<ValueNs> {
        self.resolver().resolve_path_in_value_ns_fully(self.cx.db, path, hygiene)
    }

    /// The constant a path names, with the generic arguments inference gave it.
    pub(crate) fn path_const(
        &self,
        node: ExprOrPatId,
        path: Path<'_>,
        hygiene: HygieneId,
    ) -> Option<(ConstId, GenericArgs<'db>)> {
        if let Some((CandidateId::ConstId(konst), args)) =
            self.infer.assoc_resolutions_for_expr_or_pat(node)
        {
            return Some((konst, args));
        }
        match self.resolver().resolve_path_in_value_ns_fully(self.cx.db, path, hygiene)? {
            ValueNs::ConstId(konst) => Some((konst, GenericArgs::empty())),
            _ => None,
        }
    }

    /// `is_const_pat_that_looks_like_binding`: a path pattern of one identifier that names a
    /// constant.
    pub(crate) fn const_that_looks_like_binding(&self, pat: PatId) -> Option<ConstId> {
        let HirPat::Path(path) = self.body.pat(pat) else { return None };
        path.mod_path()?.as_ident()?;
        if self.infer.variant_resolution_for_pat(pat).is_some() {
            return None;
        }
        self.path_const(pat.into(), path, self.body.pat_path_hygiene(pat)).map(|(konst, _)| konst)
    }

    fn eval(&self, konst: ConstId, args: GenericArgs<'db>) -> Result<Allocation<'db>, LowerError> {
        let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate };
        self.cx.db.const_eval(konst, args, Some(env)).map_err(|_| LowerError::Failed)
    }

    /// A path to a constant: its value, as `const_to_pat` makes a pattern of it, refused as
    /// `const_to_pat` refuses one.
    fn const_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        konst: ConstId,
        args: GenericArgs<'db>,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        let value = self.eval(konst, args)?;
        let mut refusals = Refusals::default();
        let lowered =
            self.value_pat(pat, ty, &value.memory, &value.memory_map, 0, &mut refusals, false)?;
        let errors = match refusals {
            // alloc: a `SmallVec` of one inline, for a constant that is refused.
            Refusals { valtree: Some(error), .. } => smallvec![error],
            Refusals { pat: errors, .. } if !errors.is_empty() => errors,
            // alloc: as above.
            _ if !self.has_partial_eq(ty, 0) => smallvec![self.not_partial_eq(ty)],
            _ => return Ok(lowered),
        };
        Err(LowerError::ConstPattern { pat, errors })
    }

    /// `type_has_partial_eq_impl(ty).has_impl`. Core's impls for the built-in type
    /// constructors are read off the type, as they hold exactly when their parts implement
    /// `PartialEq`; an ADT's is asked of the trait solver.
    fn has_partial_eq(&self, ty: Ty<'db>, depth: u32) -> bool {
        if depth > MAX_VALUE_DEPTH {
            return false;
        }
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Never
            | TyKind::RawPtr(..) => true,
            TyKind::Tuple(tys) => {
                tys.len() <= MAX_TUPLE_PARTIAL_EQ
                    && tys.iter().all(|ty| self.has_partial_eq(ty, depth + 1))
            }
            TyKind::Array(elem, _) | TyKind::Slice(elem) | TyKind::Ref(_, elem, _) => {
                self.has_partial_eq(elem, depth + 1)
            }
            TyKind::Adt(..) => {
                // A `no_core` crate without the lang item has no `PartialEq` to require.
                let Some(partial_eq) = self.cx.infcx.interner.lang_items().PartialEq else {
                    return true;
                };
                let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate };
                let args = GenericArgs::new_from_slice(&[ty.into(), ty.into()]);
                implements_trait_unique_with_args(self.cx.db, env, partial_eq, args)
            }
            _ => false,
        }
    }

    /// `type_marked_structural`: `ty`, an instance of `adt`, implements `StructuralPartialEq`.
    /// `#[derive(PartialEq)]`'s impl of it has the bounds of its `PartialEq` impl (rustc's
    /// `additional_bounds`, rust-lang/rust#147714), so it holds of `ty` when those do, whatever
    /// manual `PartialEq` impl `ty` has.
    fn is_structural(&self, ty: Ty<'db>, adt: AdtId) -> bool {
        self.builtin_partial_eq_derive(adt).is_some_and(|derive| self.derive_holds(derive, ty))
            || self.cx.infcx.interner.lang_items().StructuralPeq.is_some_and(|structural| {
                let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate };
                implements_trait_unique(ty, self.cx.db, env, structural)
            })
    }

    /// Whether the bounds of the builtin derive `derive` hold of `ty`, an instance of its ADT.
    fn derive_holds(&self, derive: BuiltinDeriveImplId, ty: Ty<'db>) -> bool {
        let TyKind::Adt(_, args) = ty.kind() else { return false };
        let infcx = self.cx.infcx;
        hir_ty::builtin_derive::predicates(self.cx.db, derive)
            .all_predicates()
            .iter_instantiated(infcx.interner, args.as_slice())
            .map(Unnormalized::skip_norm_wip)
            .all(|clause| {
                let cause = ObligationCause::dummy();
                let obligation =
                    Obligation::new(infcx.interner, cause, self.cx.env, clause.as_predicate());
                infcx.predicate_must_hold_modulo_regions(&obligation)
            })
    }

    /// The front end records a builtin derive as the derive, not as the impls it expands to,
    /// so its `StructuralPartialEq` impl is the recorded derive.
    fn builtin_derives_partial_eq(&self, adt: AdtId) -> bool {
        self.builtin_partial_eq_derive(adt).is_some()
    }

    /// The builtin `#[derive(PartialEq)]` of `adt`, if it has one.
    fn builtin_partial_eq_derive(&self, adt: AdtId) -> Option<BuiltinDeriveImplId> {
        let db = self.cx.db;
        let module = adt.module(db);
        module.def_map(db).module(db, module).scope(db).builtin_derive_impls().find(|derive| {
            let loc = derive.loc(db);
            loc.adt == adt && loc.trait_ == BuiltinDeriveImplTrait::PartialEq
        })
    }

    /// `type_has_partial_eq_impl` of `ty`, an instance of `adt`.
    fn partial_eq_status(&self, ty: Ty<'db>, adt: AdtId) -> PartialEqStatus {
        let db = self.cx.db;
        let lang = self.cx.infcx.interner.lang_items();
        let mut status = PartialEqStatus {
            has_impl: self.has_partial_eq(ty, 0),
            is_derived: false,
            non_blanket_impl: None,
            possibly_inapplicable_structural: self.builtin_derives_partial_eq(adt)
                || lang.StructuralPeq.is_some_and(|structural| {
                    self.any_impl_matches(structural, GenericArgs::new_from_slice(&[ty.into()]))
                }),
            possibly_inapplicable_derived: false,
        };
        let Some(partial_eq) = lang.PartialEq else { return status };
        let args = GenericArgs::new_from_slice(&[ty.into(), ty.into()]);
        let interner = self.cx.infcx.interner;
        let goal = TraitRef::new_from_args(interner, partial_eq.into(), args);
        interner.for_each_relevant_impl(goal, |impl_| {
            let derived = match impl_ {
                AnyImplId::BuiltinDeriveImplId(_) => true,
                AnyImplId::ImplId(id) => {
                    AttrFlags::query(db, id.into()).contains(AttrFlags::AUTOMATICALLY_DERIVED)
                }
            };
            if derived && !status.possibly_inapplicable_derived {
                status.possibly_inapplicable_derived = self.impl_matches(impl_, goal);
            }
            if !is_blanket(db, impl_) {
                status.is_derived = derived;
                status.non_blanket_impl = Some(impl_);
            }
        });
        status
    }

    /// Whether an impl of `trait_` matches `trait_(args)`, its bounds aside.
    fn any_impl_matches(&self, trait_: TraitId, args: GenericArgs<'db>) -> bool {
        let interner = self.cx.infcx.interner;
        let goal = TraitRef::new_from_args(interner, trait_.into(), args);
        let mut found = false;
        interner.for_each_relevant_impl(goal, |impl_| {
            found = found || self.impl_matches(impl_, goal);
        });
        found
    }

    /// Whether `impl_` matches `goal` as `compute_applicable_impls_for_diagnostics` asks with
    /// `ignore_predicates_of_impls`: its trait ref unifies with `goal` and it is positive.
    fn impl_matches(&self, impl_: AnyImplId, goal: TraitRef<DbInterner<'db>>) -> bool {
        let db = self.cx.db;
        let infcx = self.cx.infcx;
        match impl_ {
            // A derive is generic over every parameter of its ADT, with `Self` as `Rhs`.
            AnyImplId::BuiltinDeriveImplId(_) => true,
            AnyImplId::ImplId(id) => {
                !ImplSignature::of(db, id).is_negative()
                    && db.impl_trait(id).is_some_and(|trait_ref| {
                        infcx.probe(|_| {
                            let args = infcx.fresh_args_for_item(Span::Dummy, id.into());
                            let trait_ref =
                                trait_ref.instantiate(infcx.interner, args).skip_norm_wip();
                            infcx
                                .at(&ObligationCause::dummy(), self.cx.env)
                                .eq(goal, trait_ref)
                                .is_ok()
                        })
                    })
            }
        }
    }

    /// `TypeNotPartialEq` of `ty`, with what `extend_type_not_partial_eq` finds in it.
    fn not_partial_eq(&self, ty: Ty<'db>) -> ConstPatternError {
        let mut facts = PartialEqFacts {
            lowerer: self,
            local_manual: Vec::new(), // alloc: none until `push_new`.
            local_without: Vec::new(), // alloc: as above.
            manual: Vec::new(),        // alloc: as above.
            without: Vec::new(),       // alloc: as above.
        };
        let store = |tys: Vec<Ty<'db>>| -> Box<[StoredTy]> {
            tys.into_iter().map(|ty| ty.store()).collect() // alloc: as `push_new`.
        };
        if ty.visit_with(&mut facts).is_break() {
            return ConstPatternError::NotPartialEq {
                ty: ty.store(),
                local_manual: Box::default(),
                local_without: Box::default(),
                manual: Box::default(),
                without: Box::default(),
            };
        }
        ConstPatternError::NotPartialEq {
            ty: ty.store(),
            local_manual: facts.local_manual.into_boxed_slice(),
            local_without: facts.local_without.into_boxed_slice(),
            manual: store(facts.manual),
            without: store(facts.without),
        }
    }

    fn layout_size(&self, ty: Ty<'db>) -> Result<usize, LowerError> {
        let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate }.store();
        let layout = self.cx.db.layout_of_ty(ty.store(), env).map_err(|_| LowerError::Failed)?;
        Ok(layout.size.bytes_usize())
    }

    /// `const_to_pat`'s valtree construction and `valtree_to_pat` over a value's bytes, in one
    /// walk: what either refuses goes to `refusals`, and the part refused becomes opaque.
    fn value_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        bytes: &[u8],
        map: &MemoryMap<'db>,
        depth: u32,
        refusals: &mut Refusals,
        refused: bool,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        if depth > MAX_VALUE_DEPTH {
            return Err(LowerError::Failed);
        }
        let db = self.cx.db;
        let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate }.store();
        let field = |offset: usize, size: usize| {
            bytes.get(offset..offset + size).ok_or(LowerError::Failed)
        };
        Ok(match ty.kind() {
            TyKind::Bool => Self::leaf(Constructor::Bool(bytes.first().is_some_and(|&b| b != 0)), ty, pat),
            TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) => {
                let info = self.cx.int_info(ty).ok_or(LowerError::Failed)?;
                let bits = truncate(read_uint(bytes).ok_or(LowerError::Failed)?, info.bits);
                let x = Cx::int_of_bits(info, bits);
                Self::leaf(Constructor::IntRange(IntRange::from_singleton(x)), ty, pat)
            }
            TyKind::Float(fty) => {
                let bits = read_uint(bytes).ok_or(LowerError::Failed)?;
                if is_nan(fty, bits) {
                    refusals.pat(refused, ConstPatternError::NaN);
                }
                Self::leaf(float_ctor(fty, bits, bits, RangeEnd::Included), ty, pat)
            }
            TyKind::Adt(def, _)
                if !matches!(def.def_id(), AdtId::UnionId(_))
                    && !refused
                    && !self.is_structural(ty, def.def_id()) =>
            {
                let status = self.partial_eq_status(ty, def.def_id());
                // A derived `PartialEq` that doesn't apply is the error, not the missing derive.
                let error = if status.possibly_inapplicable_derived && !status.has_impl {
                    self.not_partial_eq(ty)
                } else {
                    ConstPatternError::NotStructural {
                        ty: ty.store(),
                        inapplicable_structural: status.possibly_inapplicable_structural,
                    }
                };
                refusals.pat(refused, error);
                return self.value_pat(pat, ty, bytes, map, depth, refusals, true);
            }
            TyKind::Tuple(tys) => {
                let layout = db.layout_of_ty(ty.store(), env).map_err(|_| LowerError::Failed)?;
                let mut lowered = Vec::with_capacity(tys.len());
                for (i, field_ty) in tys.iter().enumerate() {
                    let offset = layout.fields.offset(i).bytes_usize();
                    let size = self.layout_size(field_ty)?;
                    lowered.push(self.value_pat(pat, field_ty, field(offset, size)?, map, depth + 1, refusals, refused)?);
                }
                let arity = lowered.len();
                Self::new_pat(Constructor::Struct, fields_of(lowered), arity, ty, pat)
            }
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::StructId(id) => {
                    let layout = db
                        .layout_of_adt(id.into(), args.store(), env)
                        .map_err(|_| LowerError::Failed)?;
                    // alloc: the fields `DeconstructedPat::new` takes, for a const's value only.
                    let mut lowered = Vec::new();
                    for (fid, field_ty) in self.cx.variant_fields(args, id.into()) {
                        let i = u32::from(fid.into_raw()) as usize;
                        let offset = layout.fields.offset(i).bytes_usize();
                        let size = self.layout_size(field_ty)?;
                        lowered.push(self.value_pat(pat, field_ty, field(offset, size)?, map, depth + 1, refusals, refused)?);
                    }
                    let arity = lowered.len();
                    Self::new_pat(Constructor::Struct, fields_of(lowered), arity, ty, pat)
                }
                AdtId::EnumId(id) => {
                    let layout = db
                        .layout_of_adt(id.into(), args.store(), env)
                        .map_err(|_| LowerError::Failed)?;
                    let target = db.target_data_layout(self.cx.krate).map_err(|_| LowerError::Failed)?;
                    let (variant, variant_layout) =
                        detect_variant_from_bytes(&layout, db, target, bytes, id)
                            .ok_or(LowerError::Failed)?;
                    // alloc: the fields `DeconstructedPat::new` takes, for a const's value only.
                    let mut lowered = Vec::new();
                    for (fid, field_ty) in self.cx.variant_fields(args, variant.into()) {
                        let i = u32::from(fid.into_raw()) as usize;
                        let offset = variant_layout.fields.offset(i).bytes_usize();
                        let size = self.layout_size(field_ty)?;
                        lowered.push(self.value_pat(pat, field_ty, field(offset, size)?, map, depth + 1, refusals, refused)?);
                    }
                    let arity = lowered.len();
                    let ctor = Constructor::Variant(VariantIdx::of(db, variant));
                    Self::new_pat(ctor, fields_of(lowered), arity, ty, pat)
                }
                AdtId::UnionId(_) => {
                    refusals.valtree(ConstPatternError::Union);
                    Self::opaque(ty, pat)
                }
            },
            TyKind::Array(elem, len) => {
                let len = try_const_usize(db, len).ok_or(LowerError::Failed)? as usize;
                let size = self.layout_size(elem)?;
                let mut lowered = Vec::with_capacity(len);
                for i in 0..len {
                    lowered.push(self.value_pat(pat, elem, field(i * size, size)?, map, depth + 1, refusals, refused)?);
                }
                let kind = SliceKind::FixedLen(len);
                Self::new_pat(Constructor::Slice(Slice::new(Some(len), kind)), fields_of(lowered), len, ty, pat)
            }
            TyKind::Ref(_, pointee, _) => {
                let half = bytes.len() / 2;
                let inner = match pointee.kind() {
                    TyKind::Str => {
                        let addr = read_uint(&bytes[..half]).ok_or(LowerError::Failed)? as usize;
                        let len = read_uint(&bytes[half..]).ok_or(LowerError::Failed)? as usize;
                        let text = map.get(addr, len).ok_or(LowerError::Failed)?;
                        let text = std::str::from_utf8(text).map_err(|_| LowerError::Failed)?;
                        Self::leaf(Constructor::Str(Symbol::intern(text)), pointee, pat)
                    }
                    TyKind::Slice(elem) => {
                        let addr = read_uint(&bytes[..half]).ok_or(LowerError::Failed)? as usize;
                        let count = read_uint(&bytes[half..]).ok_or(LowerError::Failed)? as usize;
                        let size = self.layout_size(elem)?;
                        let data = map.get(addr, size * count).ok_or(LowerError::Failed)?;
                        let mut lowered = Vec::with_capacity(count);
                        for i in 0..count {
                            let elem_bytes = data.get(i * size..(i + 1) * size).ok_or(LowerError::Failed)?;
                            lowered.push(self.value_pat(pat, elem, elem_bytes, map, depth + 1, refusals, refused)?);
                        }
                        let kind = SliceKind::FixedLen(count);
                        Self::new_pat(Constructor::Slice(Slice::new(None, kind)), fields_of(lowered), count, pointee, pat)
                    }
                    TyKind::Dynamic(..) | TyKind::Foreign(_) => {
                        refusals.valtree(ConstPatternError::Invalid(pointee.store()));
                        return Ok(Self::opaque(ty, pat));
                    }
                    _ if bytes.len() != (self.cx.pointer_bits / 8) as usize => {
                        return Ok(Self::opaque(ty, pat));
                    }
                    _ => {
                        let addr = read_uint(bytes).ok_or(LowerError::Failed)? as usize;
                        let size = self.layout_size(pointee)?;
                        let data = map.get(addr, size).ok_or(LowerError::Failed)?;
                        self.value_pat(pat, pointee, data, map, depth + 1, refusals, refused)?
                    }
                };
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![inner.at_index(0)], 1, ty, pat)
            }
            TyKind::FnPtr(..) => {
                refusals.valtree(ConstPatternError::Pointer);
                Self::opaque(ty, pat)
            }
            // Valtree construction takes a raw pointer only as an integer: one into an
            // allocation is refused.
            TyKind::RawPtr(..) => {
                let addr = read_uint(bytes).ok_or(LowerError::Failed)? as usize;
                if addr != 0 && (map.memory_at(addr).is_some() || map.static_at(addr).is_some()) {
                    refusals.valtree(ConstPatternError::Pointer);
                }
                Self::opaque(ty, pat)
            }
            // A zero-sized valtree that `valtree_to_pat` makes no pattern of.
            TyKind::FnDef(..) => {
                refusals.pat(refused, ConstPatternError::Invalid(ty.store()));
                Self::opaque(ty, pat)
            }
            _ => {
                refusals.valtree(ConstPatternError::Invalid(ty.store()));
                Self::opaque(ty, pat)
            }
        })
    }

    /// A literal pattern, or `-` of one.
    fn lit_expr_pat(&self, pat: PatId, ty: Ty<'db>, expr: ExprId) -> Result<Pat<'a, 'db>, LowerError> {
        let (lit, negated) = self.literal_of(expr).ok_or(LowerError::Failed)?;
        self.lit_pat(pat, ty, lit, negated)
    }

    fn literal_of(&self, expr: ExprId) -> Option<(&'c Literal, bool)> {
        match self.body.expr(expr) {
            Expr::Literal(lit) => Some((lit, false)),
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => match self.body.expr(expr) {
                Expr::Literal(lit) => Some((lit, true)),
                _ => None,
            },
            _ => None,
        }
    }

    fn lit_pat(&self, pat: PatId, ty: Ty<'db>, lit: &Literal, negated: bool) -> Result<Pat<'a, 'db>, LowerError> {
        Ok(match (ty.kind(), lit) {
            (TyKind::Bool, Literal::Bool(b)) => Self::leaf(Constructor::Bool(*b), ty, pat),
            (TyKind::Char | TyKind::Int(_) | TyKind::Uint(_), _) => {
                let x = self.int_lit(ty, lit, negated).ok_or(LowerError::Failed)?;
                Self::leaf(Constructor::IntRange(IntRange::from_singleton(x)), ty, pat)
            }
            (TyKind::Float(fty), Literal::Float(..) | Literal::Int(..) | Literal::Uint(..)) => {
                let bits = float_lit_bits(fty, lit, negated).ok_or(LowerError::Failed)?;
                Self::leaf(float_ctor(fty, bits, bits, RangeEnd::Included), ty, pat)
            }
            // clone: a `Symbol`, which is interned.
            (TyKind::Str, Literal::String(s)) => Self::leaf(Constructor::Str(s.clone()), ty, pat),
            (TyKind::Ref(_, pointee, _), Literal::String(s)) if pointee.is_str() => {
                // clone: as above.
                let inner = Self::leaf(Constructor::Str(s.clone()), pointee, pat);
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![inner.at_index(0)], 1, ty, pat)
            }
            (TyKind::Ref(_, pointee, _), Literal::ByteString(bytes)) => {
                let inner = self.byte_string(pat, pointee, bytes)?;
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![inner.at_index(0)], 1, ty, pat)
            }
            (TyKind::Array(..) | TyKind::Slice(_), Literal::ByteString(bytes)) => {
                self.byte_string(pat, ty, bytes)?
            }
            _ => Self::opaque(ty, pat),
        })
    }

    /// `b"..."` against `[u8; N]` or `[u8]`: its bytes, one `u8` pattern each.
    fn byte_string(&self, pat: PatId, ty: Ty<'db>, bytes: &[u8]) -> Result<Pat<'a, 'db>, LowerError> {
        let (elem, array_len) = match ty.kind() {
            TyKind::Array(elem, _) => (elem, Some(bytes.len())),
            TyKind::Slice(elem) => (elem, None),
            _ => return Ok(Self::opaque(ty, pat)),
        };
        let lowered = bytes.iter().map(|&b| {
            let x = MaybeInfiniteInt::new_finite_uint(b as u128);
            Self::leaf(Constructor::IntRange(IntRange::from_singleton(x)), elem, pat)
        });
        let fields: Vec<_> = fields_of(lowered);
        let kind = SliceKind::FixedLen(bytes.len());
        Ok(Self::new_pat(Constructor::Slice(Slice::new(array_len, kind)), fields, bytes.len(), ty, pat))
    }

    fn int_lit(&self, ty: Ty<'db>, lit: &Literal, negated: bool) -> Option<MaybeInfiniteInt> {
        let info = self.cx.int_info(ty)?;
        let bits = match *lit {
            Literal::Int(v, _) => (if negated { v.wrapping_neg() } else { v }) as u128,
            Literal::Uint(v, _) => if negated { v.wrapping_neg() } else { v },
            Literal::Char(c) => c as u128,
            _ => return None,
        };
        Some(Cx::int_of_bits(info, truncate(bits, info.bits)))
    }

    /// The value of a range pattern's bound: a literal, `-` of one, or a constant.
    fn range_bound(&self, expr: ExprId, ty: Ty<'db>) -> Result<RangeBound, LowerError> {
        if let Some((lit, negated)) = self.literal_of(expr) {
            return match ty.kind() {
                TyKind::Float(fty) => {
                    float_lit_bits(fty, lit, negated).map(RangeBound::Float).ok_or(LowerError::Failed)
                }
                _ => self.int_lit(ty, lit, negated).map(RangeBound::Int).ok_or(LowerError::Failed),
            };
        }
        let Expr::Path(path) = self.body.expr(expr) else { return Err(LowerError::Failed) };
        let (konst, args) = self
            .path_const(expr.into(), path, self.body.expr_path_hygiene(expr))
            .ok_or(LowerError::Failed)?;
        let value = self.eval(konst, args)?;
        let bits = read_uint(&value.memory).ok_or(LowerError::Failed)?;
        match ty.kind() {
            TyKind::Float(_) => Ok(RangeBound::Float(bits)),
            _ => {
                let info = self.cx.int_info(ty).ok_or(LowerError::Failed)?;
                Ok(RangeBound::Int(Cx::int_of_bits(info, truncate(bits, info.bits))))
            }
        }
    }

    /// `lower_pattern_range`, with its E0030 and E0579.
    fn range_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        start: Option<ExprId>,
        end: Option<ExprId>,
        range_type: RangeOp,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        let lo = start.map(|e| self.range_bound(e, ty)).transpose()?;
        let hi = end.map(|e| self.range_bound(e, ty)).transpose()?;
        // `lo..` has no end to leave out: it is `lo..=MAX`.
        let range_end = match (range_type, hi) {
            (RangeOp::Exclusive, Some(_)) => RangeEnd::Excluded,
            _ => RangeEnd::Included,
        };
        let inclusive = range_end == RangeEnd::Included;
        let empty = LowerError::EmptyRange { pat, inclusive };
        let ctor = match ty.kind() {
            TyKind::Float(fty) => {
                let (lo, hi) = (
                    lo.map(|b| match b { RangeBound::Float(bits) => bits, RangeBound::Int(_) => 0 }),
                    hi.map(|b| match b { RangeBound::Float(bits) => bits, RangeBound::Int(_) => 0 }),
                );
                let ctor = float_range_ctor(fty, lo, hi, range_end);
                if !float_range_is_valid(&ctor) {
                    return Err(empty);
                }
                ctor
            }
            TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) => {
                let int = |b: RangeBound| match b {
                    RangeBound::Int(x) => Ok(x),
                    RangeBound::Float(_) => Err(LowerError::Failed),
                };
                let lo = lo.map(int).transpose()?.unwrap_or(MaybeInfiniteInt::NegInfinity);
                let hi = hi.map(int).transpose()?.unwrap_or(MaybeInfiniteInt::PosInfinity);
                let valid = match range_end {
                    RangeEnd::Included => lo <= hi,
                    RangeEnd::Excluded => lo < hi,
                };
                if !valid {
                    return Err(empty);
                }
                Constructor::IntRange(IntRange::from_range(lo, hi, range_end))
            }
            _ => return Err(LowerError::Failed),
        };
        Ok(Self::leaf(ctor, ty, pat))
    }
}

fn is_nan(fty: FloatTy, bits: u128) -> bool {
    match fty {
        FloatTy::F16 => Half::from_bits(bits).is_nan(),
        FloatTy::F32 => Single::from_bits(bits).is_nan(),
        FloatTy::F64 => Double::from_bits(bits).is_nan(),
        FloatTy::F128 => Quad::from_bits(bits).is_nan(),
    }
}

/// A float literal's bits, read as rustc reads one: by `rustc_apfloat` from its text.
fn float_lit_bits(fty: FloatTy, lit: &Literal, negated: bool) -> Option<u128> {
    // An integer literal at a float type is a type error, and the body is not checked.
    let Literal::Float(value, _) = lit else { return None };
    let text = value.as_str();
    fn parse<F: Float + std::str::FromStr>(text: &str, negated: bool) -> Option<u128> {
        let value: F = text.parse().ok()?;
        Some(if negated { (-value).to_bits() } else { value.to_bits() })
    }
    match fty {
        FloatTy::F16 => parse::<Half>(text, negated),
        FloatTy::F32 => parse::<Single>(text, negated),
        FloatTy::F64 => parse::<Double>(text, negated),
        FloatTy::F128 => parse::<Quad>(text, negated),
    }
}

fn float_ctor<'a, 'db>(fty: FloatTy, lo: u128, hi: u128, end: RangeEnd) -> Constructor<Cx<'a, 'db>> {
    float_range_ctor(fty, Some(lo), Some(hi), end)
}

fn float_range_ctor<'a, 'db>(
    fty: FloatTy,
    lo: Option<u128>,
    hi: Option<u128>,
    end: RangeEnd,
) -> Constructor<Cx<'a, 'db>> {
    match fty {
        FloatTy::F16 => Constructor::F16Range(
            lo.map_or(-Half::INFINITY, Half::from_bits),
            hi.map_or(Half::INFINITY, Half::from_bits),
            end,
        ),
        FloatTy::F32 => Constructor::F32Range(
            lo.map_or(-Single::INFINITY, Single::from_bits),
            hi.map_or(Single::INFINITY, Single::from_bits),
            end,
        ),
        FloatTy::F64 => Constructor::F64Range(
            lo.map_or(-Double::INFINITY, Double::from_bits),
            hi.map_or(Double::INFINITY, Double::from_bits),
            end,
        ),
        FloatTy::F128 => Constructor::F128Range(
            lo.map_or(-Quad::INFINITY, Quad::from_bits),
            hi.map_or(Quad::INFINITY, Quad::from_bits),
            end,
        ),
    }
}

/// rustc's check of a float range: `lo <= hi` for `..=`, `lo < hi` for `..`, neither a NaN.
fn float_range_is_valid(ctor: &Constructor<Cx<'_, '_>>) -> bool {
    fn valid<F: Float>(lo: &F, hi: &F, end: RangeEnd) -> bool {
        match (lo.partial_cmp(hi), end) {
            (Some(std::cmp::Ordering::Less), _) => true,
            (Some(std::cmp::Ordering::Equal), RangeEnd::Included) => true,
            _ => false,
        }
    }
    match ctor {
        Constructor::F16Range(lo, hi, end) => valid(lo, hi, *end),
        Constructor::F32Range(lo, hi, end) => valid(lo, hi, *end),
        Constructor::F64Range(lo, hi, end) => valid(lo, hi, *end),
        Constructor::F128Range(lo, hi, end) => valid(lo, hi, *end),
        _ => true,
    }
}

/// A blanket impl: its self type is one of its parameters.
fn is_blanket(db: &Db, impl_: AnyImplId) -> bool {
    match impl_ {
        AnyImplId::BuiltinDeriveImplId(_) => false,
        AnyImplId::ImplId(id) => db.impl_trait(id).is_some_and(|trait_ref| {
            matches!(trait_ref.skip_binder().self_ty().kind(), TyKind::Param(_))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_the_type_s_bits() {
        assert_eq!(truncate(u128::MAX, 8), 0xff);
        assert_eq!(truncate((-1i128) as u128, 32), 0xffff_ffff);
        assert_eq!(truncate(5, 128), 5);
    }

    #[test]
    fn values_are_read_little_endian() {
        assert_eq!(read_uint(&[1, 2]), Some(0x0201));
        assert_eq!(read_uint(&[0; 17]), None);
    }

    #[test]
    fn float_literals_are_read_by_apfloat() {
        let one = Literal::Float(hir_def::hir::FloatTypeWrapper::new(Symbol::intern("1.0")), None);
        assert_eq!(float_lit_bits(FloatTy::F32, &one, false), Some(1.0f32.to_bits() as u128));
        assert_eq!(float_lit_bits(FloatTy::F64, &one, true), Some((-1.0f64).to_bits() as u128));
        assert_eq!(float_lit_bits(FloatTy::F32, &Literal::Int(1, None), false), None);
    }
}
