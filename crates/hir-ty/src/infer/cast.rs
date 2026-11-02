//! Type cast logic. Basically coercion + additional casts.

use base_db::Db;
use hir_def::{
    AdtId, TraitId, TypeParamId,
    hir::ExprId,
    signatures::{TraitFlags, TraitSignature},
};
use rustc_ast_ir::Mutability;
use rustc_type_ir::{
    InferTy, RegionKind, TypeVisitableExt, UintTy,
    error::TypeError,
    fold_regions,
    inherent::{BoundExistentialPredicates as _, IntoKind, Ty as _},
};
use stdx::never;

use crate::{
    InferenceDiagnostic,
    db::HirDatabase,
    drop::destructor,
    infer::{AllowTwoPhase, InferenceContext, expr::ExprIsRead},
    next_solver::{
        AliasTy, BoundExistentialPredicates, ExistentialPredicate, ParamTy, Region, RegionExt,
        TraitIdWrapper, Ty, TyKind, infer::traits::ObligationCause,
    },
};

#[derive(Debug)]
pub enum Int {
    I,
    U(UintTy),
    Bool,
    Char,
    CEnum,
    InferenceVar,
}

#[derive(Debug)]
pub enum CastTy<'db> {
    Int(Int),
    Float,
    FnPtr,
    Ptr(Ty<'db>, Mutability),
    // `DynStar` is Not supported yet in r-a
}

impl<'db> CastTy<'db> {
    pub fn from_ty(db: &Db, t: Ty<'db>) -> Option<Self> {
        match t.kind() {
            TyKind::Bool => Some(Self::Int(Int::Bool)),
            TyKind::Char => Some(Self::Int(Int::Char)),
            TyKind::Int(_) => Some(Self::Int(Int::I)),
            TyKind::Uint(it) => Some(Self::Int(Int::U(it))),
            TyKind::Infer(InferTy::IntVar(_)) => Some(Self::Int(Int::InferenceVar)),
            TyKind::Infer(InferTy::FloatVar(_)) => Some(Self::Float),
            TyKind::Float(_) => Some(Self::Float),
            TyKind::Adt(..) => {
                let (AdtId::EnumId(id), _) = t.as_adt()? else {
                    return None;
                };
                let enum_data = id.enum_variants(db);
                if enum_data.is_payload_free(db) { Some(Self::Int(Int::CEnum)) } else { None }
            }
            TyKind::RawPtr(ty, m) => Some(Self::Ptr(ty, m)),
            TyKind::FnPtr(..) => Some(Self::FnPtr),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum CastError {
    Unknown,
    CastToBool,
    CastToChar,
    DifferingKinds,
    SizedUnsizedCast,
    IllegalCast,
    IntToWideCast,
    NeedDeref,
    NeedViaPtr,
    NeedViaThinPtr,
    NeedViaInt,
    NonScalar,
    PtrPtrAddingAutoTraits,
    /// E0641: the pointee of the type cast to is not known, so neither is its metadata.
    UnknownCastPtrKind,
    /// E0641: the pointee of the expression's type is not known.
    UnknownExprPtrKind,
    /// A field-less enum with a `Drop` impl cast to an integer.
    CastEnumDrop,
    /// E0606 of an enum from another crate with a `#[non_exhaustive]` variant.
    ForeignNonExhaustiveAdt,
}

/// rustc's `PointerKind` of a pointee, as the report of a refused cast reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastPointerKind {
    Thin,
    /// A trait object, with its principal trait if it has one.
    VTable(Option<TraitId>),
    Length,
    /// The metadata of a projection or opaque type.
    OfAlias,
    /// The metadata of a type parameter.
    OfParam(TypeParamId),
}

/// The pointer kinds of a pointer cast rustc refuses, where its check found them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CastPointers {
    /// The kind of the expression's pointee; `None` when it is not known or was not looked at.
    pub expr: Option<CastPointerKind>,
    /// The kind of the pointee of the type cast to, likewise.
    pub cast: Option<CastPointerKind>,
    /// Of `PtrPtrAddingAutoTraits`: the auto traits the target `dyn` adds, in its order.
    pub added_auto_traits: Box<[TraitId]>,
}

/// A cast rustc refuses: the error and the pointer kinds its report reads.
#[derive(Debug)]
struct CastFailure {
    error: CastError,
    pointers: Option<Box<CastPointers>>,
}

impl From<CastError> for CastFailure {
    fn from(error: CastError) -> Self {
        Self { error, pointers: None }
    }
}

impl CastFailure {
    fn with_kinds(
        error: CastError,
        expr: Option<&PointerKind<'_>>,
        cast: Option<&PointerKind<'_>>,
    ) -> Self {
        // alloc: once per refused pointer cast
        let pointers = Box::new(CastPointers {
            expr: expr.and_then(PointerKind::stored),
            cast: cast.and_then(PointerKind::stored),
            added_auto_traits: Box::new([]), // alloc: empty
        });
        Self { error, pointers: Some(pointers) }
    }

    fn into_diagnostic<'db>(
        self,
        expr: ExprId,
        expr_ty: Ty<'db>,
        cast_ty: Ty<'db>,
    ) -> InferenceDiagnostic {
        InferenceDiagnostic::InvalidCast {
            expr,
            error: self.error,
            expr_ty: expr_ty.store(),
            cast_ty: cast_ty.store(),
            pointers: self.pointers,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct CastCheck<'db> {
    expr: ExprId,
    source_expr: ExprId,
    expr_ty: Ty<'db>,
    cast_ty: Ty<'db>,
}

impl<'db> CastCheck<'db> {
    pub(super) fn new(
        expr: ExprId,
        source_expr: ExprId,
        expr_ty: Ty<'db>,
        cast_ty: Ty<'db>,
    ) -> Self {
        Self { expr, source_expr, expr_ty, cast_ty }
    }

    pub(super) fn check(
        &mut self,
        ctx: &mut InferenceContext<'db, '_>,
    ) -> Result<(), InferenceDiagnostic> {
        self.expr_ty = ctx.structurally_resolve_type(self.source_expr.into(), self.expr_ty);
        self.cast_ty = ctx.table.try_structurally_resolve_type(self.expr.into(), self.cast_ty);
        if self.cast_ty.is_ty_var() {
            self.cast_ty = ctx.type_must_be_known_at_this_point(self.expr.into(), self.cast_ty);
        }

        if !ctx.table.type_is_sized_modulo_regions(self.cast_ty) && !self.cast_ty.has_infer_types()
        {
            return Err(InferenceDiagnostic::CastToUnsized {
                expr: self.expr,
                cast_ty: self.cast_ty.store(),
            });
        }

        if self.expr_ty.references_non_lt_error() || self.cast_ty.references_non_lt_error() {
            return Ok(());
        }

        if ctx
            .coerce(
                self.source_expr,
                self.expr_ty,
                self.cast_ty,
                AllowTwoPhase::No,
                ExprIsRead::Yes,
            )
            .is_ok()
        {
            // Raw pointers whose pointees differ only in lifetimes, which inference cannot
            // tell apart, stay a cast, as rustc's `check_cast` keeps them (rust-lang/rust#113257).
            if !(self.expr_ty.is_raw_ptr() && self.cast_ty.is_raw_ptr()) {
                ctx.result.coercion_casts.insert(self.source_expr);
            }
            return Ok(());
        }

        self.do_check(ctx).map_err(|e| e.into_diagnostic(self.expr, self.expr_ty, self.cast_ty))
    }

    fn do_check(&self, ctx: &mut InferenceContext<'db, '_>) -> Result<(), CastFailure> {
        let (t_from, t_cast) =
            match (CastTy::from_ty(ctx.db, self.expr_ty), CastTy::from_ty(ctx.db, self.cast_ty)) {
                (Some(t_from), Some(t_cast)) => (t_from, t_cast),
                (None, Some(t_cast)) => match self.expr_ty.kind() {
                    TyKind::FnDef(..) => {
                        // rustc calls `FnCtxt::normalize` on this but it's a no-op in next-solver
                        let sig = self.expr_ty.fn_sig(ctx.interner());
                        let fn_ptr = Ty::new_fn_ptr(ctx.interner(), sig);
                        match ctx.coerce(
                            self.source_expr,
                            self.expr_ty,
                            fn_ptr,
                            AllowTwoPhase::No,
                            ExprIsRead::Yes,
                        ) {
                            Ok(_) => {}
                            Err(TypeError::IntrinsicCast) => {
                                return Err(CastError::IllegalCast.into());
                            }
                            Err(_) => {
                                return Err(CastError::NonScalar.into());
                            }
                        }

                        (CastTy::FnPtr, t_cast)
                    }
                    TyKind::Ref(_, inner_ty, mutbl) => {
                        return match t_cast {
                            CastTy::Int(_) | CastTy::Float => match inner_ty.kind() {
                                TyKind::Int(_)
                                | TyKind::Uint(_)
                                | TyKind::Float(_)
                                | TyKind::Infer(InferTy::IntVar(_) | InferTy::FloatVar(_)) => {
                                    Err(CastError::NeedDeref.into())
                                }

                                _ => Err(CastError::NeedViaPtr.into()),
                            },
                            // array-ptr-cast
                            CastTy::Ptr(t, m) => {
                                let t =
                                    ctx.table.try_structurally_resolve_type(self.expr.into(), t);
                                if !ctx.table.type_is_sized_modulo_regions(t) {
                                    return Err(CastError::IllegalCast.into());
                                }
                                self.check_ref_cast(ctx, inner_ty, mutbl, t, m)
                                    .map_err(CastFailure::from)
                            }
                            _ => Err(CastError::NonScalar.into()),
                        };
                    }
                    _ => return Err(CastError::NonScalar.into()),
                },
                _ => return Err(CastError::NonScalar.into()),
            };

        if let Some((AdtId::EnumId(id), _)) = self.expr_ty.as_adt()
            && id
                .enum_variants(ctx.db)
                .variants
                .values()
                .any(|&(variant, _)| ctx.has_applicable_non_exhaustive(variant.into()))
        {
            return Err(CastError::ForeignNonExhaustiveAdt.into());
        }

        match (t_from, t_cast) {
            // These types have invariants! can't cast into them.
            (_, CastTy::Int(Int::CEnum) | CastTy::FnPtr) => Err(CastError::NonScalar.into()),

            // * -> Bool
            (_, CastTy::Int(Int::Bool)) => Err(CastError::CastToBool.into()),

            // * -> Char
            (CastTy::Int(Int::U(UintTy::U8)), CastTy::Int(Int::Char)) => Ok(()), // u8-char-cast
            (_, CastTy::Int(Int::Char)) => Err(CastError::CastToChar.into()),

            // prim -> float,ptr
            (CastTy::Int(Int::Bool | Int::CEnum | Int::Char), CastTy::Float) => {
                Err(CastError::NeedViaInt.into())
            }

            (CastTy::Int(Int::Bool | Int::CEnum | Int::Char) | CastTy::Float, CastTy::Ptr(..))
            | (CastTy::Ptr(..) | CastTy::FnPtr, CastTy::Float) => {
                Err(CastError::IllegalCast.into())
            }

            // ptr -> ptr
            (CastTy::Ptr(src, _), CastTy::Ptr(dst, _)) => self.check_ptr_ptr_cast(ctx, src, dst), // ptr-ptr-cast

            // // ptr-addr-cast
            (CastTy::Ptr(src, _), CastTy::Int(_)) => self.check_ptr_addr_cast(ctx, src),
            (CastTy::FnPtr, CastTy::Int(_)) => Ok(()),

            // addr-ptr-cast
            (CastTy::Int(_), CastTy::Ptr(dst, _)) => self.check_addr_ptr_cast(ctx, dst),

            // fn-ptr-cast
            (CastTy::FnPtr, CastTy::Ptr(dst, _)) => self.check_fptr_ptr_cast(ctx, dst),

            // enum -> int
            (CastTy::Int(Int::CEnum), CastTy::Int(_)) => match self.expr_ty.as_adt() {
                Some((adt, _)) if destructor(ctx.db, adt).is_some() => {
                    Err(CastError::CastEnumDrop.into())
                }
                _ => Ok(()),
            },

            // prim -> prim
            (CastTy::Int(Int::Char | Int::Bool), CastTy::Int(_)) => Ok(()),
            (CastTy::Int(_) | CastTy::Float, CastTy::Int(_) | CastTy::Float) => Ok(()),
        }
    }

    fn check_ref_cast(
        &self,
        ctx: &mut InferenceContext<'db, '_>,
        t_expr: Ty<'db>,
        m_expr: Mutability,
        t_cast: Ty<'db>,
        m_cast: Mutability,
    ) -> Result<(), CastError> {
        let t_expr = ctx.table.try_structurally_resolve_type(self.expr.into(), t_expr);
        let t_cast = ctx.table.try_structurally_resolve_type(self.expr.into(), t_cast);

        if m_expr >= m_cast
            && let TyKind::Array(ety, _) = t_expr.kind()
            && ctx.infcx().can_eq(ctx.table.param_env, ety, t_cast)
        {
            // Due to historical reasons we allow directly casting references of
            // arrays into raw pointers of their element type.

            // Coerce to a raw pointer so that we generate RawPtr in MIR.
            let array_ptr_type = Ty::new_ptr(ctx.interner(), t_expr, m_expr);
            if ctx
                .coerce(
                    self.source_expr,
                    self.expr_ty,
                    array_ptr_type,
                    AllowTwoPhase::No,
                    ExprIsRead::Yes,
                )
                .is_ok()
            {
            } else {
                unreachable!(
                    "could not cast from reference to array to pointer to array ({:?} to {:?})",
                    self.expr_ty,
                    array_ptr_type
                );
            }

            // this will report a type mismatch if needed
            let _ = ctx.demand_eqtype(self.expr.into(), ety, t_cast);
            return Ok(());
        }

        Err(CastError::IllegalCast)
    }

    fn check_ptr_ptr_cast(
        &self,
        ctx: &mut InferenceContext<'db, '_>,
        src: Ty<'db>,
        dst: Ty<'db>,
    ) -> Result<(), CastFailure> {
        let src_kind = pointer_kind(self.expr, src, ctx).map_err(|_| CastError::Unknown)?;
        let dst_kind = pointer_kind(self.expr, dst, ctx).map_err(|_| CastError::Unknown)?;
        if matches!(src_kind, Some(PointerKind::Error))
            || matches!(dst_kind, Some(PointerKind::Error))
        {
            return Ok(());
        }

        let Some(dst_kind) = dst_kind else {
            return Err(CastFailure::with_kinds(
                CastError::UnknownCastPtrKind,
                src_kind.as_ref(),
                None,
            ));
        };

        // Cast to thin pointer is OK
        if dst_kind == PointerKind::Thin {
            return Ok(());
        }

        // Not `UnknownExprPtrKind`: rustc's E0641 names the cast's side here too.
        let Some(src_kind) = src_kind else {
            return Err(CastFailure::with_kinds(
                CastError::UnknownCastPtrKind,
                None,
                Some(&dst_kind),
            ));
        };
        let failure =
            |error| CastFailure::with_kinds(error, Some(&src_kind), Some(&dst_kind));

        match (&src_kind, &dst_kind) {
            // thin -> fat? report invalid cast (don't complain about vtable kinds)
            (PointerKind::Thin, _) => Err(failure(CastError::SizedUnsizedCast)),

            // trait object -> trait object? need to do additional checks
            (PointerKind::VTable(src_tty), PointerKind::VTable(dst_tty)) => {
                match (src_tty.principal_def_id(), dst_tty.principal_def_id()) {
                    // A<dyn Src<...> + SrcAuto> -> B<dyn Dst<...> + DstAuto>. need to make sure
                    // - `Src` and `Dst` traits are the same
                    // - traits have the same generic arguments
                    // - projections are the same
                    // - `SrcAuto` (+auto traits implied by `Src`) is a superset of `DstAuto`
                    //
                    // Note that trait upcasting goes through a different mechanism (`coerce_unsized`)
                    // and is unaffected by this check.
                    (Some(src_principal), Some(_)) => {
                        // We need to reconstruct trait object types.
                        // `m_src` and `m_dst` won't work for us here because they will potentially
                        // contain wrappers, which we do not care about.
                        //
                        // e.g. we want to allow `dyn T -> (dyn T,)`, etc.
                        //
                        // We also need to skip auto traits to emit an FCW and not an error.
                        let src_obj = Ty::new_dynamic(
                            ctx.interner(),
                            BoundExistentialPredicates::new_from_iter(src_tty.iter().filter(
                                |pred| {
                                    !matches!(
                                        pred.skip_binder(),
                                        ExistentialPredicate::AutoTrait(_)
                                    )
                                },
                            )),
                            Region::new_erased(ctx.interner()),
                        );
                        let dst_obj = Ty::new_dynamic(
                            ctx.interner(),
                            BoundExistentialPredicates::new_from_iter(dst_tty.iter().filter(
                                |pred| {
                                    !matches!(
                                        pred.skip_binder(),
                                        ExistentialPredicate::AutoTrait(_)
                                    )
                                },
                            )),
                            Region::new_erased(ctx.interner()),
                        );

                        // `dyn Src = dyn Dst`, this checks for matching traits/generics/projections
                        // This is `fcx.demand_eqtype`, but inlined to give a better error.
                        if ctx
                            .table
                            .at(&ObligationCause::new(self.expr))
                            .eq(src_obj, dst_obj)
                            .map(|infer_ok| ctx.table.register_infer_ok(infer_ok))
                            .is_err()
                        {
                            return Err(failure(CastError::DifferingKinds));
                        }

                        // Check that `SrcAuto` (+auto traits implied by `Src`) is a superset of `DstAuto`.
                        // Emit an FCW otherwise.
                        let mut src_auto = Vec::new_in(ctx.frame);
                        src_auto.extend(src_tty.auto_traits().into_iter().chain(
                            crate::utils::all_super_traits(ctx.db, src_principal.0)
                                .iter()
                                .filter(|&&trait_| {
                                    TraitSignature::of(ctx.db, trait_)
                                        .flags
                                        .contains(TraitFlags::AUTO)
                                })
                                .map(|&trait_| TraitIdWrapper(trait_)),
                        ));

                        let mut added = dst_tty
                            .auto_traits()
                            .into_iter()
                            .filter(|trait_| !src_auto.contains(trait_))
                            .map(|trait_| trait_.0)
                            .peekable();

                        if added.peek().is_some() {
                            let mut failure = failure(CastError::PtrPtrAddingAutoTraits);
                            if let Some(pointers) = &mut failure.pointers {
                                // alloc: error path, a refused cast's diagnostic
                                pointers.added_auto_traits = added.collect();
                            }
                            return Err(failure);
                        }

                        Ok(())
                    }

                    // dyn Auto -> dyn Auto'? ok.
                    (None, None) => Ok(()),

                    // dyn Trait -> dyn Auto? not ok (for now).
                    //
                    // Although dropping the principal is already allowed for unsizing coercions
                    // (e.g. `*const (dyn Trait + Auto)` to `*const dyn Auto`), dropping it is
                    // currently **NOT** allowed for (non-coercion) ptr-to-ptr casts (e.g
                    // `*const Foo` to `*const Bar` where `Foo` has a `dyn Trait + Auto` tail
                    // and `Bar` has a `dyn Auto` tail), because the underlying MIR operations
                    // currently work very differently:
                    //
                    // * A MIR unsizing coercion on raw pointers to trait objects (`*const dyn Src`
                    //   to `*const dyn Dst`) is currently equivalent to downcasting the source to
                    //   the concrete sized type that it was originally unsized from first (via a
                    //   ptr-to-ptr cast from `*const Src` to `*const T` with `T: Sized`) and then
                    //   unsizing this thin pointer to the target type (unsizing `*const T` to
                    //   `*const Dst`). In particular, this means that the pointer's metadata
                    //   (vtable) will semantically change, e.g. for const eval and miri, even
                    //   though the vtables will always be merged for codegen.
                    //
                    // * A MIR ptr-to-ptr cast is currently equivalent to a transmute and does not
                    //   change the pointer metadata (vtable) at all.
                    //
                    // In addition to this potentially surprising difference between coercion and
                    // non-coercion casts, casting away the principal with a MIR ptr-to-ptr cast
                    // is currently considered undefined behavior:
                    //
                    // As a validity invariant of pointers to trait objects, we currently require
                    // that the principal of the vtable in the pointer metadata exactly matches
                    // the principal of the pointee type, where "no principal" is also considered
                    // a kind of principal.
                    (Some(_), None) => Err(failure(CastError::DifferingKinds)),

                    // dyn Auto -> dyn Trait? not ok.
                    (None, Some(_)) => Err(failure(CastError::DifferingKinds)),
                }
            }

            // fat -> fat? metadata kinds must match
            (src_kind, dst_kind) if src_kind == dst_kind => Ok(()),
            (_, _) => Err(failure(CastError::DifferingKinds)),
        }
    }

    fn check_ptr_addr_cast(
        &self,
        ctx: &mut InferenceContext<'db, '_>,
        expr_ty: Ty<'db>,
    ) -> Result<(), CastFailure> {
        match pointer_kind(self.expr, expr_ty, ctx).map_err(|_| CastError::Unknown)? {
            None => Err(CastError::UnknownExprPtrKind.into()),
            Some(PointerKind::Error) => Ok(()),
            Some(PointerKind::Thin) => Ok(()),
            Some(kind) => {
                Err(CastFailure::with_kinds(CastError::NeedViaThinPtr, Some(&kind), None))
            }
        }
    }

    fn check_addr_ptr_cast(
        &self,
        ctx: &mut InferenceContext<'db, '_>,
        cast_ty: Ty<'db>,
    ) -> Result<(), CastFailure> {
        match pointer_kind(self.expr, cast_ty, ctx).map_err(|_| CastError::Unknown)? {
            None => Err(CastError::UnknownCastPtrKind.into()),
            Some(PointerKind::Error) => Ok(()),
            Some(PointerKind::Thin) => Ok(()),
            Some(
                kind @ (PointerKind::VTable(_)
                | PointerKind::Length
                | PointerKind::OfAlias(_)
                | PointerKind::OfParam(_)),
            ) => Err(CastFailure::with_kinds(CastError::IntToWideCast, None, Some(&kind))),
        }
    }

    fn check_fptr_ptr_cast(
        &self,
        ctx: &mut InferenceContext<'db, '_>,
        cast_ty: Ty<'db>,
    ) -> Result<(), CastFailure> {
        match pointer_kind(self.expr, cast_ty, ctx).map_err(|_| CastError::Unknown)? {
            None => Err(CastError::UnknownCastPtrKind.into()),
            Some(PointerKind::Error) => Ok(()),
            Some(PointerKind::Thin) => Ok(()),
            Some(kind) => Err(CastFailure::with_kinds(CastError::IllegalCast, None, Some(&kind))),
        }
    }
}

/// The kind of pointer and associated metadata (thin, length or vtable) - we
/// only allow casts between wide pointers if their metadata have the same
/// kind.
#[derive(Debug, PartialEq, Eq)]
enum PointerKind<'db> {
    /// No metadata attached, ie pointer to sized type or foreign type
    Thin,
    /// A trait object
    VTable(BoundExistentialPredicates<'db>),
    /// Slice
    Length,
    /// The unsize info of this projection or opaque type, its free regions erased: two
    /// pointers to the same alias have the same metadata.
    OfAlias(AliasTy<'db>),
    /// The unsize info of this parameter
    OfParam(ParamTy),
    /// A type with an error in it: rustc reports nothing more.
    Error,
}

impl PointerKind<'_> {
    fn stored(&self) -> Option<CastPointerKind> {
        Some(match self {
            PointerKind::Thin => CastPointerKind::Thin,
            PointerKind::VTable(tty) => {
                CastPointerKind::VTable(tty.principal_def_id().map(|it| it.0))
            }
            PointerKind::Length => CastPointerKind::Length,
            PointerKind::OfAlias(_) => CastPointerKind::OfAlias,
            PointerKind::OfParam(param) => CastPointerKind::OfParam(param.id),
            PointerKind::Error => return None,
        })
    }
}

fn pointer_kind<'db>(
    expr: ExprId,
    ty: Ty<'db>,
    ctx: &mut InferenceContext<'db, '_>,
) -> Result<Option<PointerKind<'db>>, ()> {
    let ty = ctx.table.try_structurally_resolve_type(expr.into(), ty);
    if ty.references_non_lt_error() {
        return Ok(Some(PointerKind::Error));
    }

    if ctx.table.type_is_sized_modulo_regions(ty) {
        return Ok(Some(PointerKind::Thin));
    }

    match ty.kind() {
        TyKind::Slice(_) | TyKind::Str => Ok(Some(PointerKind::Length)),
        TyKind::Dynamic(bounds, _) => Ok(Some(PointerKind::VTable(bounds))),
        TyKind::Adt(adt_def, subst) => {
            let id = adt_def.def_id();
            let AdtId::StructId(id) = id else {
                never!("`{:?}` should be sized but is not?", ty);
                return Err(());
            };

            let struct_data = id.fields(ctx.db);
            if let Some((last_field, _)) = struct_data.fields().iter().last() {
                let last_field_ty = ctx.db.field_types(id.into())[last_field]
                    .ty()
                    .instantiate(ctx.interner(), subst)
                    .skip_norm_wip();
                pointer_kind(expr, last_field_ty, ctx)
            } else {
                Ok(Some(PointerKind::Thin))
            }
        }
        TyKind::Tuple(subst) => match subst.iter().next_back() {
            None => Ok(Some(PointerKind::Thin)),
            Some(ty) => pointer_kind(expr, ty, ctx),
        },
        TyKind::Foreign(_) => Ok(Some(PointerKind::Thin)),
        TyKind::Alias(_, alias) => {
            let interner = ctx.interner();
            let alias = fold_regions(interner, alias, |region, _| match region.kind() {
                RegionKind::ReBound(..) => region,
                _ => Region::new_erased(interner),
            });
            Ok(Some(PointerKind::OfAlias(alias)))
        }
        TyKind::Error(_) => Ok(Some(PointerKind::Error)),
        TyKind::Param(idx) => Ok(Some(PointerKind::OfParam(idx))),
        TyKind::Bound(..) | TyKind::Placeholder(..) | TyKind::Infer(..) => Ok(None),
        TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Array(..)
        | TyKind::CoroutineWitness(..)
        | TyKind::RawPtr(..)
        | TyKind::Ref(..)
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..)
        | TyKind::Closure(..)
        | TyKind::Coroutine(..)
        | TyKind::CoroutineClosure(..)
        | TyKind::Never => {
            never!("`{:?}` should be sized but is not?", ty);
            Err(())
        }
        TyKind::UnsafeBinder(..) | TyKind::Pat(..) => {
            never!("we don't produce these types: {ty:?}");
            Err(())
        }
    }
}
