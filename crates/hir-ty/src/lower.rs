//! Methods for lowering the HIR to types. There are two main cases here:
//!
//!  - Lowering a type reference like `&usize` or `Option<foo::bar::Baz>` to a
//!    type: The entry point for this is `TyLoweringContext::lower_ty`.
//!  - Building the type for an item: This happens through the `ty` query.
//!
//! This usually involves resolving names, collecting generic arguments etc.
pub(crate) mod batch;
pub(crate) mod diagnostics;
pub(crate) mod path;
pub mod persist;

use std::{alloc::Allocator, cell::OnceCell, iter, mem, sync::OnceLock};

use base_db::Db;
use blaze_mem::Frame;
use either::Either;
use hir_def::{
    AdtId, AssocItemId, CallableDefId, ConstId, ConstParamId, EnumId, EnumVariantId,
    ExpressionStoreOwnerId, FunctionId, GenericDefId, GenericParamId, HasModule, ImplId,
    ItemContainerId, LifetimeParamId, LocalFieldId, Lookup, StaticId, StructId, TraitId,
    TypeAliasId, TypeOrConstParamId, TypeParamId, UnionId, VariantId,
    builtin_type::BuiltinType,
    expr_store::{ExpressionStore, path::Path},
    hir::{
        ExprId, PatId,
        generics::{
            GenericParamDataRef, GenericParams, LocalTypeOrConstParamId, TypeOrConstParamData,
            TypeParamProvenance, WherePredicate,
        },
    },
    item_tree::FieldsShape,
    lang_item::LangItems,
    resolver::{HasResolver, LifetimeNs, Resolver, TypeNs},
    signatures::{
        ConstSignature, FunctionSignature, ImplSignature, StaticFlags, StaticSignature,
        StructSignature, TraitFlags, TraitSignature, TypeAliasFlags, TypeAliasSignature,
    },
    type_ref::{
        ConstRef, FnType, LifetimeRef, LifetimeRefId, PathId, TraitBoundModifier,
        TraitRef as HirTraitRef, TypeBound, TypeRef, TypeRefId,
    },
};
use hir_expand::name::Name;
use path::{PathDiagnosticCallback, PathLoweringContext};
use rustc_ast_ir::Mutability;
use rustc_type_ir::{
    AliasTyKind, BoundRegion, BoundRegionKind, BoundTyKind, BoundVar, BoundVarIndexKind,
    DebruijnIndex, ExistentialPredicate, ExistentialProjection, ExistentialTraitRef, FnSig,
    Interner, OutlivesClause, PredicateProxy, TermKind, TyKind, TypeFoldable, TypeFolder,
    TypeSuperFoldable, TypeVisitableExt, Upcast, UpcastFrom, elaborate,
    inherent::{Clause as _, GenericArg as _, GenericArgs as _, IntoKind as _, Ty as _},
    shift_vars,
};
use smallvec::SmallVec;
use stdx::impl_from;
use thin_vec::ThinVec;
use tracing::debug;

pub use hir_def::LoweringMode;
pub(crate) use hir_def::TrackedStructToken;

use crate::{
    ElidedLifetimeSite, ImplTraitId, InferenceResult, MissingLifetimeKind,
    PathLoweringDiagnostic, PatternTypeUnsupported, Span, TyLoweringDiagnostic,
    consteval::{CreateConstError, create_anon_const, path_to_const},
    db::{AnonConstId, HirDatabase, InternedOpaqueTyId},
    generics::{Generics, SingleGenerics, generics},
    infer::unify::InferenceTable,
    next_solver::{
        AliasConstKind, AliasTy, AnonConstIdWrapper, Binder, BoundConst, BoundExistentialPredicates,
        BoundTy, BoundVarKinds, BoundVariableKind, Clause, ClauseKind, Clauses, Const, ConstExt,
        ConstKind, DbInterner, DefaultAny, EarlyBinder, EarlyParamRegion, ErrorGuaranteed,
        FnSigKind, GenericArg, GenericArgKind, GenericArgs, IsRigid, ParamConst,
        ParamEnv, PatList, Pattern, PolyFnSig, Predicate, Region, RegionExt, RegionKind,
        StoredClauses, StoredConst, StoredEarlyBinder, StoredGenericArg, StoredGenericArgs,
        StoredPolyFnSig, StoredTraitRef, StoredTy, Symbol, TermId, TraitClause, TraitRef, Ty, Tys,
        Unnormalized, abi::Safety, default_types, infer::InferCtxt, mk_param,
        util::BottomUpFolder,
    },
};
use crate::frame_map;
pub(crate) use crate::FrameMap;

/// A memo's list of `items`, allocated once at its length: nothing when it is empty (R4).
pub(crate) fn exact_thin<T>(
    items: impl IntoIterator<Item = T, IntoIter: ExactSizeIterator>,
) -> ThinVec<T> {
    let items = items.into_iter();
    let mut list = ThinVec::with_capacity(items.len()); // alloc: a memo's own list, exact size
    list.extend(items);
    list
}

pub(crate) struct PathDiagnosticCallbackData(pub(crate) TypeRefId);

#[derive(PartialEq, Eq, Debug, Hash, blaze_query::Value)]
pub struct WithDefinedOpaques<T> {
    value: T,
    impl_traits: ThinVec<SelfAndAssocBounds>,
}

pub enum ImplTrait {}

pub type ImplTraitIdx = la_arena::Idx<ImplTrait>;

#[derive(PartialEq, Eq, Debug, Hash, blaze_query::Value)]
pub struct SelfAndAssocBounds {
    /// The self bounds, then the associated type bounds from `assoc_ty_bounds_start`, then from
    /// `const_bounds_start` each written `[const] Trait` bound again as `HostEffect(Maybe)`.
    bounds: StoredEarlyBinder<StoredClauses>,
    assoc_ty_bounds_start: u32,
    const_bounds_start: u32,
}

impl SelfAndAssocBounds {
    #[inline]
    pub fn all_bounds(&self) -> EarlyBinder<'_, &[Clause<'_>]> {
        self.bounds.get().map_bound(|it| &it.as_slice()[..self.const_bounds_start as usize])
    }

    /// rustc's `explicit_implied_const_bounds` of an associated type or opaque: its written
    /// `[const] Trait` item bounds, as `HostEffect(Maybe)` clauses on the alias.
    #[inline]
    pub fn const_bounds(&self) -> EarlyBinder<'_, &[Clause<'_>]> {
        self.bounds.get().map_bound(|it| &it.as_slice()[self.const_bounds_start as usize..])
    }

    #[inline]
    pub fn self_bounds(&self) -> EarlyBinder<'_, &[Clause<'_>]> {
        self.bounds.get().map_bound(|it| &it.as_slice()[..self.assoc_ty_bounds_start as usize])
    }
}

/// The sizedness bounds written on a type: `Sized`, `?Sized`, `MetaSized` and `PointeeSized`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WrittenSizedness {
    sized: bool,
    relaxed: bool,
    meta_sized: bool,
    pointee_sized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplicitSizedness {
    Sized,
    MetaSized,
}

impl WrittenSizedness {
    /// rustc's `add_implicit_sizedness_bounds`: `?Sized` alone is `MetaSized`, and a type with
    /// no sizedness bound is `Sized`, or `MetaSized` for a trait's `Self`.
    fn implicit(self, of_trait_self: bool) -> Option<ImplicitSizedness> {
        let WrittenSizedness { sized, relaxed, meta_sized, pointee_sized } = self;
        if relaxed && !sized && !meta_sized && !pointee_sized {
            Some(ImplicitSizedness::MetaSized)
        } else if !(sized || relaxed || meta_sized || pointee_sized) {
            Some(if of_trait_self { ImplicitSizedness::MetaSized } else { ImplicitSizedness::Sized })
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct ImplTraitLoweringState<'a> {
    /// When turning `impl Trait` into opaque types, we have to collect the
    /// bounds at the same time to get the IDs correct (without becoming too
    /// complicated).
    mode: ImplTraitLoweringMode,
    // This is structured as a struct with fields and not as an enum because it helps with the borrow checker.
    opaque_type_data: Vec<SelfAndAssocBounds, &'a Frame>,
}

impl<'a> ImplTraitLoweringState<'a> {
    fn new(mode: ImplTraitLoweringMode, frame: &'a Frame) -> ImplTraitLoweringState<'a> {
        Self { mode, opaque_type_data: Vec::new_in(frame) }
    }
}

/// A signature whose binder binds the lifetimes left out of it: that of a function pointer, or
/// of a `Fn(&u8) -> &u8` bound.
struct Elision<'db, 'a> {
    /// The binder's place in `bound_vars`.
    binder: usize,
    /// How many variables the binder had before the lifetimes left out.
    named: usize,
    /// How many lifetimes left out it has bound.
    anonymous: usize,
    /// The lifetimes the arguments name, as the binder sees them. A lifetime left out of the
    /// return type is the one of them, when there is one.
    of_arguments: Vec<Region<'db>, &'a Frame>,
    /// A bound is a signature only once its path turns out to be written as one.
    is_signature: bool,
    in_return_type: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum LifetimeElisionKind<'db> {
    /// Create a new anonymous lifetime parameter and reference it.
    ///
    /// If `report_in_path`, report an error when encountering lifetime elision in a path:
    /// ```compile_fail
    /// struct Foo<'a> { x: &'a () }
    /// async fn foo(x: Foo) {}
    /// ```
    ///
    /// Note: the error should not trigger when the elided lifetime is in a pattern or
    /// expression-position path:
    /// ```
    /// struct Foo<'a> { x: &'a () }
    /// async fn foo(Foo { x: _ }: Foo<'_>) {}
    /// ```
    AnonymousCreateParameter { report_in_path: bool },

    /// Replace all anonymous lifetimes by provided lifetime.
    Elided(Region<'db>),

    /// Give a hard error when either `&` or `'_` is written. Used to
    /// rule out things like `where T: Foo<'_>`. Does not imply an
    /// error on default object bounds (e.g., `Box<dyn Foo>`).
    AnonymousReportError,

    /// No lifetime to elide to, where rustc's lookup meets the item's own rib: a field or a
    /// free type alias, where `&` and `'_` are errors as a path without its lifetimes is.
    Item,

    /// Resolves elided lifetimes to `'static` if there are no other lifetimes in scope,
    /// otherwise give a warning that the previous behavior of introducing a new early-bound
    /// lifetime is a bug and will be removed (if `only_lint` is enabled).
    StaticIfNoLifetimeInScope { only_lint: bool },

    /// Signal we cannot find which should be the anonymous lifetime.
    ElisionFailure,

    /// Infer all elided lifetimes.
    Infer,
}

impl<'db> LifetimeElisionKind<'db> {
    #[inline]
    pub fn for_const(
        interner: DbInterner<'db>,
        const_parent: ItemContainerId,
    ) -> LifetimeElisionKind<'db> {
        match const_parent {
            ItemContainerId::ExternBlockId(_) | ItemContainerId::ModuleId(_) => {
                LifetimeElisionKind::Elided(Region::new_static(interner))
            }
            ItemContainerId::ImplId(_) => {
                LifetimeElisionKind::StaticIfNoLifetimeInScope { only_lint: true }
            }
            ItemContainerId::TraitId(_) => {
                LifetimeElisionKind::StaticIfNoLifetimeInScope { only_lint: false }
            }
        }
    }

    #[inline]
    pub(crate) fn for_fn_params(data: &FunctionSignature) -> LifetimeElisionKind<'db> {
        LifetimeElisionKind::AnonymousCreateParameter { report_in_path: data.is_async() }
    }

    /// A return type, `elides` when its parameters give a lifetime to elide to. That lifetime
    /// is an error here, which `bind_elided_lifetimes` replaces in a function's signature and
    /// [`TyLoweringContext::elided_lifetime`] gives first in a fn pointer's or `Fn()`'s.
    #[inline]
    pub(crate) fn for_fn_ret(interner: DbInterner<'db>, elides: bool) -> LifetimeElisionKind<'db> {
        if elides {
            LifetimeElisionKind::Elided(Region::error(interner))
        } else {
            LifetimeElisionKind::ElisionFailure
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum GenericPredicateSource {
    SelfOnly,
    AssocTyBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForbidParamsAfterReason {
    /// When lowering generic param defaults, you cannot refer to any param after
    /// the currently lowered param, including the current param.
    LoweringParamDefault,
    /// Most anon const (except array repeat expressions) cannot refer to any generic
    /// param.
    AnonConst,
    /// The type of a const param cannot refer to a type param.
    ConstParamTy,
}

/// Where lowering a type written in a body gets a variable for a `_`, a lifetime left out or a
/// const `_`: type inference's table, a body's inference context (the borrow checker), or a
/// finished inference's answers (the IDE: `_` is what inference gave it, all else an error).
pub(crate) enum InferVars<'a, 'db> {
    Inference { table: &'a mut InferenceTable<'db> },
    Context(&'a InferCtxt<'db>),
    Answered(Option<&'a InferenceResult<'db>>),
}

impl<'db> InferVars<'_, 'db> {
    /// A variable for `span`, made for `param` where it stands for a parameter's argument left
    /// out (rustc's `var_for_def`, whose origin names it in "cannot infer type").
    fn next_ty_var(
        &mut self,
        span: Span,
        param: Option<TypeParamId>,
        types: &DefaultAny<'db>,
        type_placeholders: &mut Vec<(TypeRefId, Ty<'db>), &Frame>,
    ) -> Ty<'db> {
        match self {
            InferVars::Inference { table } => {
                let ty = table.infer_ctxt.next_ty_var_for_param(span, param);
                if let Span::TypeRefId(type_ref) = span {
                    type_placeholders.push((type_ref, ty));
                }
                ty
            }
            InferVars::Context(infcx) => infcx.next_ty_var_for_param(span, param),
            InferVars::Answered(infer) => match span {
                Span::TypeRefId(type_ref) => infer
                    .and_then(|infer| infer.type_of_type_placeholder(type_ref))
                    .unwrap_or(types.types.error),
                _ => types.types.error,
            },
        }
    }

    fn next_const_var(
        &mut self,
        span: Span,
        param: Option<ConstParamId>,
        types: &DefaultAny<'db>,
    ) -> Const<'db> {
        match self {
            InferVars::Inference { table, .. } => {
                table.infer_ctxt.next_const_var_for_param(span, param)
            }
            InferVars::Context(infcx) => infcx.next_const_var_for_param(span, param),
            InferVars::Answered(_) => types.consts.error,
        }
    }

    fn next_region_var(&mut self, span: Span, types: &DefaultAny<'db>) -> Region<'db> {
        match self {
            InferVars::Inference { table, .. } => table.infer_ctxt.next_region_var(span),
            InferVars::Context(infcx) => infcx.next_region_var(span),
            InferVars::Answered(_) => types.regions.error,
        }
    }
}

pub struct TyLoweringContext<'db, 'a> {
    pub db: &'db Db,
    pub(crate) interner: DbInterner<'db>,
    types: &'db crate::next_solver::DefaultAny<'db>,
    lang_items: &'db LangItems,
    resolver: &'a Resolver<'db>,
    store: &'a ExpressionStore,
    def: ExpressionStoreOwnerId,
    generic_def: GenericDefId,
    generics: &'a OnceCell<Generics<'db>>,
    in_binders: DebruijnIndex,
    impl_trait_mode: ImplTraitLoweringState<'a>,
    interning_mode: LoweringMode,
    /// The sizedness bounds written on each type, for its implicit one
    /// ([`WrittenSizedness::implicit`]).
    pub(crate) unsized_types: FrameMap<'a, Ty<'db>, WrittenSizedness>,
    pub(crate) diagnostics: Vec<TyLoweringDiagnostic, &'a Frame>,
    lifetime_elision: LifetimeElisionKind<'db>,
    forbid_params_after: Option<u32>,
    forbid_params_after_reason: ForbidParamsAfterReason,
    pub(crate) defined_anon_consts: Vec<AnonConstId<'db>, &'a Frame>,
    infer_vars: Option<InferVars<'a, 'db>>,
    /// The variables `InferVars::Inference` made for `_` placeholders, for the caller to record.
    pub(crate) type_placeholders: Vec<(TypeRefId, Ty<'db>), &'a Frame>,
    /// Lifetimes left out (`&u8`, `'_`) that no signature binds are region variables, not
    /// errors, as the borrow checker lowers a type written in a body.
    elided_region_vars: bool,
    /// Lowering an impl's self type or trait, where rustc makes an elided lifetime a fresh
    /// parameter of the impl: the trait object behind such a reference has its lifetime, an
    /// error as the reference's is, not `'static`, so `impl Params for &[&dyn ToSql]` holds
    /// for `&[&name as &dyn ToSql]`.
    impl_header: bool,
    is_lowering_impl_trait_bounds: bool,
    /// The names the binders in scope bind, outermost first: binder `i`'s are
    /// `bound_names[bound_vars[i].0..]`, up to the next binder's start.
    bound_names: Vec<Name, &'a Frame>,
    bound_vars: Vec<(u32, BoundVarKinds<'db>), &'a Frame>,
    elisions: Vec<Elision<'db, 'a>, &'a Frame>,
    /// The index in `diagnostics` of the elision failures of the return type being lowered.
    elision_failures: Option<usize>,
    /// Lowering a type its body infers (a return type, a const's or a static's), whose `_`
    /// placeholders are one diagnostic, at `placeholders` once there is one.
    group_placeholders: bool,
    placeholders: Option<usize>,
    /// The next bound lowered is one on an associated type, `X: for<'b> B<'a, 'b>` in
    /// `for<'a> T: A<'a, X: for<'b> B<'a, 'b>>`, or one of a `for<'a>` predicate, whose `for`
    /// adds to the enclosing binder.
    bound_of_assoc_ty: bool,
    lifetime_lowering_mode: LifetimeLoweringMode,
    /// The frame of the run lowering: its query's, or that of the inference that made it.
    pub(crate) frame: &'a Frame,
}

impl<'db, 'a> TyLoweringContext<'db, 'a> {
    pub fn new(
        db: &'db Db,
        resolver: &'a Resolver<'db>,
        store: &'a ExpressionStore,
        def: ExpressionStoreOwnerId,
        generic_def: GenericDefId,
        generics: &'a OnceCell<Generics<'db>>,
        lifetime_elision: LifetimeElisionKind<'db>,
        lifetime_lowering_mode: LifetimeLoweringMode,
        frame: &'a Frame,
    ) -> Self {
        let impl_trait_mode =
            ImplTraitLoweringState::new(ImplTraitLoweringMode::Disallowed, frame);
        let in_binders = DebruijnIndex::ZERO;
        let interner = DbInterner::new_with(db, resolver.krate());
        let mut bound_vars = Vec::with_capacity_in(1, frame);
        bound_vars.push((0, TyLoweringContext::bound_vars(db, generic_def, generics)));
        Self {
            db,
            // Can provide no block since we don't use it for trait solving.
            interner,
            types: crate::next_solver::default_types(),
            lang_items: interner.lang_items(),
            resolver,
            def,
            generic_def,
            generics,
            store,
            in_binders,
            impl_trait_mode,
            interning_mode: LoweringMode::Analysis,
            unsized_types: frame_map(frame),
            diagnostics: Vec::new_in(frame),
            lifetime_elision,
            forbid_params_after: None,
            forbid_params_after_reason: ForbidParamsAfterReason::AnonConst,
            defined_anon_consts: Vec::new_in(frame),
            infer_vars: None,
            type_placeholders: Vec::new_in(frame),
            elided_region_vars: false,
            impl_header: false,
            is_lowering_impl_trait_bounds: false,
            bound_names: Vec::new_in(frame),
            bound_vars,
            elisions: Vec::new_in(frame),
            elision_failures: None,
            group_placeholders: false,
            placeholders: None,
            bound_of_assoc_ty: false,
            lifetime_lowering_mode,
            frame,
        }
    }

    pub(crate) fn set_lifetime_elision(&mut self, lifetime_elision: LifetimeElisionKind<'db>) {
        self.lifetime_elision = lifetime_elision;
    }

    pub(crate) fn set_owner(&mut self, owner: &'a SingleGenerics<'db>) {
        self.store = owner.store();
        self.def = ExpressionStoreOwnerId::Signature(owner.def());
    }

    pub fn with_interning_mode(mut self, interning_mode: LoweringMode) -> Self {
        self.interning_mode = interning_mode;
        self
    }

    pub(crate) fn with_debruijn<T>(
        &mut self,
        debruijn: DebruijnIndex,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let old_debruijn = mem::replace(&mut self.in_binders, debruijn);
        let result = f(self);
        self.in_binders = old_debruijn;
        result
    }

    pub(crate) fn with_shifted_in<T>(
        &mut self,
        binder: &[Name],
        f: impl FnOnce(&mut Self) -> T,
    ) -> (T, BoundVarKinds<'db>) {
        self.push_bound_vars(binder);
        let res = self.with_debruijn(self.in_binders.shifted_in(1), f);
        let bound_vars = self.pop_bound_vars();
        (res, bound_vars)
    }

    pub(crate) fn with_impl_trait_mode(self, impl_trait_mode: ImplTraitLoweringMode) -> Self {
        Self { impl_trait_mode: ImplTraitLoweringState::new(impl_trait_mode, self.frame), ..self }
    }

    pub(crate) fn forbid_params_after(&mut self, index: u32, reason: ForbidParamsAfterReason) {
        self.forbid_params_after = Some(index);
        self.forbid_params_after_reason = reason;
    }

    pub(crate) fn with_inference_vars(mut self, table: &'a mut InferenceTable<'db>) -> Self {
        self.infer_vars = Some(InferVars::Inference { table });
        self
    }

    /// `_` and lifetimes left out become variables of `infcx`, as the borrow checker lowers a
    /// type written in a body; with `None` they are refused.
    pub fn with_infer_ctxt_vars(mut self, infcx: Option<&'a InferCtxt<'db>>) -> Self {
        self.infer_vars = infcx.map(InferVars::Context);
        self
    }

    /// `_` is the type `infer` gave it and every other variable an error, as the IDE lowers a
    /// type written in a finished body.
    pub fn with_answered_infer_vars(mut self, infer: Option<&'a InferenceResult<'db>>) -> Self {
        self.infer_vars = Some(InferVars::Answered(infer));
        self
    }

    pub(crate) fn push_diagnostic(&mut self, diagnostic: TyLoweringDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub fn with_elided_region_vars(mut self) -> Self {
        self.elided_region_vars = true;
        self
    }

    /// The lifetime of a trait object written without one where `region` would be its
    /// default. In a body a lifetime left to inference (`&dyn Trait`, `Ref<'_, dyn Trait>`)
    /// resolves to nothing, so rustc's `resolve_object_lifetime_default` gives the object a
    /// lifetime inferred on its own, not the reference's.
    fn object_default(&mut self, region: Region<'db>) -> Region<'db> {
        if self.infers_object_lifetimes() && matches!(region.kind(), RegionKind::ReVar(_)) {
            return self.next_region_var(Span::Dummy);
        }
        region
    }

    /// Lowering a type written in a body with region variables, where a trait object no
    /// enclosing type gives a lifetime (`Box<dyn Trait>`, `*const dyn Trait`) has an inferred
    /// one, not `'static`.
    fn infers_object_lifetimes(&self) -> bool {
        self.elided_region_vars && matches!(self.def, ExpressionStoreOwnerId::Body(_))
    }

    fn push_infer_vars_not_allowed(&mut self, span: Span) {
        if span.is_dummy() {
            return;
        }
        if self.group_placeholders
            && let Some(at) = self.placeholders
            && let Some(TyLoweringDiagnostic::InferVarsNotAllowed { sources }) =
                self.diagnostics.get_mut(at)
        {
            // alloc: an error's sites.
            let mut all = mem::take(sources).into_vec();
            all.push(span);
            *sources = all.into_boxed_slice();
            return;
        }
        if self.group_placeholders {
            self.placeholders = Some(self.diagnostics.len());
        }
        // alloc: an error's sites.
        let sources = Box::new([span]);
        self.push_diagnostic(TyLoweringDiagnostic::InferVarsNotAllowed { sources });
    }

    /// The `_` placeholders of the type this lowers are one diagnostic, as rustc reports those
    /// of a type the body infers (`recover_infer_ret_ty`, `infer_placeholder_type`), when
    /// `inferred_from_body`.
    fn with_grouped_placeholders(mut self, inferred_from_body: bool) -> Self {
        self.group_placeholders = inferred_from_body;
        self
    }

    /// The region of a lifetime left out at `site` (`&T` or `'_`): rustc's
    /// `resolve_anonymous_lifetime` under the rib `lifetime_elision` stands for.
    fn anonymous_lifetime(&mut self, site: ElidedLifetimeSite) -> Region<'db> {
        if let Some(region) = self.elided_lifetime() {
            return region;
        }
        let kind = match self.lifetime_elision {
            LifetimeElisionKind::ElisionFailure => {
                self.push_elision_failure(site);
                return self.types.regions.error;
            }
            _ if self.elided_region_vars => return self.next_region_var(Span::Dummy),
            LifetimeElisionKind::Elided(region) => return region,
            LifetimeElisionKind::StaticIfNoLifetimeInScope { only_lint: _ } => {
                return Region::new_static(self.interner);
            }
            LifetimeElisionKind::AnonymousCreateParameter { report_in_path: _ }
            | LifetimeElisionKind::Infer => return self.types.regions.error,
            LifetimeElisionKind::Item => MissingLifetimeKind::Missing,
            LifetimeElisionKind::AnonymousReportError => MissingLifetimeKind::NotAllowed,
        };
        self.push_diagnostic(TyLoweringDiagnostic::MissingLifetimes {
            // alloc: an error's sites.
            sites: Box::new([site]),
            kind,
        });
        self.types.regions.error
    }

    /// A lifetime left out of the return type being lowered where its parameters give none to
    /// elide to: one diagnostic for all of the return type's, as rustc's `resolve_fn_signature`
    /// reports them.
    #[cold]
    fn push_elision_failure(&mut self, site: ElidedLifetimeSite) {
        if let Some(at) = self.elision_failures
            && let Some(TyLoweringDiagnostic::MissingLifetimes {
                sites,
                kind: MissingLifetimeKind::ElisionFailure,
            }) = self.diagnostics.get_mut(at)
        {
            // alloc: an error's sites.
            let mut all = mem::take(sites).into_vec();
            all.push(site);
            *sites = all.into_boxed_slice();
            return;
        }
        self.elision_failures = Some(self.diagnostics.len());
        self.push_diagnostic(TyLoweringDiagnostic::MissingLifetimes {
            // alloc: an error's sites.
            sites: Box::new([site]),
            kind: MissingLifetimeKind::ElisionFailure,
        });
    }

    /// Lowers a return type with `lifetime_elision`, its elision failures one diagnostic.
    fn with_return_type_elision<T>(
        &mut self,
        lifetime_elision: LifetimeElisionKind<'db>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let old_lifetime_elision = mem::replace(&mut self.lifetime_elision, lifetime_elision);
        let old_failures = self.elision_failures.take();
        let result = f(self);
        self.elision_failures = old_failures;
        self.lifetime_elision = old_lifetime_elision;
        result
    }

    /// Whether the return type of the signature being lowered has a lifetime to elide to: one
    /// its parameters have, as rustc's `resolve_fn_params` finds (a fn pointer's or `Fn()`'s,
    /// which have no `self`).
    pub(crate) fn return_type_elides(&self) -> bool {
        let innermost = self.bound_vars.len() - 1;
        self.elisions
            .iter()
            .rfind(|it| it.is_signature && it.binder == innermost)
            .is_none_or(|it| it.of_arguments.len() == 1)
    }

    #[track_caller]
    pub(crate) fn expect_table(&mut self) -> &mut InferenceTable<'db> {
        match &mut self.infer_vars {
            Some(InferVars::Inference { table, .. }) => &mut **table,
            _ => panic!("only type inference lowers a type with an inference table"),
        }
    }

    fn next_ty_var(&mut self, span: Span) -> Ty<'db> {
        self.next_ty_var_for_param(span, None)
    }

    fn next_ty_var_for_param(&mut self, span: Span, param: Option<TypeParamId>) -> Ty<'db> {
        match &mut self.infer_vars {
            Some(infer_vars) => {
                infer_vars.next_ty_var(span, param, self.types, &mut self.type_placeholders)
            }
            None => {
                self.push_infer_vars_not_allowed(span);
                self.types.types.error
            }
        }
    }

    fn next_const_var(&mut self, span: Span) -> Const<'db> {
        self.next_const_var_for_param(span, None)
    }

    fn next_const_var_for_param(&mut self, span: Span, param: Option<ConstParamId>) -> Const<'db> {
        match &mut self.infer_vars {
            Some(infer_vars) => infer_vars.next_const_var(span, param, self.types),
            None => {
                self.push_infer_vars_not_allowed(span);
                self.types.consts.error
            }
        }
    }

    fn next_region_var(&mut self, span: Span) -> Region<'db> {
        match &mut self.infer_vars {
            Some(infer_vars) => infer_vars.next_region_var(span, self.types),
            None => {
                self.push_infer_vars_not_allowed(span);
                self.types.regions.error
            }
        }
    }

    fn push_bound_vars(&mut self, binder: &[Name]) {
        let bound_vars = BoundVarKinds::new_from_iter(binder.iter().map(for_lifetime_kind));
        self.bound_vars.push((self.bound_names.len() as u32, bound_vars));
        self.bound_names.extend(binder.iter().cloned());
    }

    fn pop_bound_vars(&mut self) -> BoundVarKinds<'db> {
        let (start, bound_vars) = self.bound_vars.pop().unwrap();
        self.bound_names.truncate(start as usize);
        bound_vars
    }

    fn peek_bound_vars(&self) -> BoundVarKinds<'db> {
        self.bound_vars.last().unwrap().1
    }

    /// The variables of the innermost binder, with the lifetimes left out that it binds.
    fn bound_vars_with_elided(&self) -> BoundVarKinds<'db> {
        let named = self.peek_bound_vars();
        let anonymous = match self.elisions.last() {
            Some(elision) if elision.binder == self.bound_vars.len() - 1 => elision.anonymous,
            _ => 0,
        };
        if anonymous == 0 {
            return named;
        }
        let anonymous = iter::repeat_n(BoundVariableKind::Region(BoundRegionKind::Anon), anonymous);
        BoundVarKinds::new_from_iter(named.iter().chain(anonymous))
    }

    /// Lowers a signature of the innermost binder, which then binds the lifetimes left out.
    fn with_elision<T>(&mut self, is_signature: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        self.elisions.push(Elision {
            binder: self.bound_vars.len() - 1,
            named: self.bound_vars_with_elided().len(),
            anonymous: 0,
            of_arguments: Vec::new_in(self.frame),
            is_signature,
            in_return_type: false,
        });
        let result = f(self);
        self.elisions.pop();
        result
    }

    fn signature_mut(&mut self) -> Option<&mut Elision<'db, 'a>> {
        self.elisions.iter_mut().rfind(|it| it.is_signature)
    }

    /// The path of the bound being lowered is written as a signature: `Fn(&u8) -> &u8`.
    pub(crate) fn bound_is_signature(&mut self) {
        let innermost = self.bound_vars.len() - 1;
        if let Some(elision) = self.elisions.last_mut()
            && elision.binder == innermost
        {
            elision.is_signature = true;
        }
    }

    /// Lowers the return type of the signature being lowered.
    pub(crate) fn in_return_type<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let innermost = self.bound_vars.len() - 1;
        let Some(at) =
            self.elisions.iter().rposition(|it| it.is_signature && it.binder == innermost)
        else {
            return f(self);
        };
        self.elisions[at].in_return_type = true;
        let result = f(self);
        self.elisions[at].in_return_type = false;
        result
    }

    /// A lifetime that is left out (`&u8`, `'_`, `Ref<u8>`), when a signature binds it.
    pub(crate) fn elided_lifetime(&mut self) -> Option<Region<'db>> {
        let binders = self.bound_vars.len();
        let interner = self.interner;
        let Some(elision) = self.signature_mut() else {
            return self.elided_in_impl_trait();
        };
        let depth = binders - 1 - elision.binder;
        let region = if elision.in_return_type {
            let &[region] = elision.of_arguments.as_slice() else { return None };
            region
        } else {
            let var = BoundVar::from_usize(elision.named + elision.anonymous);
            elision.anonymous += 1;
            let bound = BoundRegion { var, kind: BoundRegionKind::Anon };
            let region = Region::new_bound(interner, DebruijnIndex::ZERO, bound);
            elision.of_arguments.push(region);
            region
        };
        Some(match region.kind() {
            RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), bound) => {
                Region::new_bound(interner, debruijn.shifted_in(depth as u32), bound)
            }
            _ => region,
        })
    }

    /// A lifetime left out of the bounds of an `impl Trait` a function returns: the function's
    /// elided output lifetime, as a parameter of the `impl Trait`.
    fn elided_in_impl_trait(&mut self) -> Option<Region<'db>> {
        if !self.is_lowering_impl_trait_bounds {
            return None;
        }
        let Some(GenericDefId::FunctionId(function)) = self.resolver.generic_def() else {
            return None;
        };
        match elided_lifetimes(self.db, function).output? {
            ElidedOutput::Elided(index) => {
                let declared = self.generics().len(true) as u32;
                let id = elided_lifetime_param(self.db, function, index);
                Some(Region::new_early_param(
                    self.interner,
                    EarlyParamRegion { id, index: declared + index },
                ))
            }
            ElidedOutput::Written(lifetime) => Some(
                self.named_region(LifetimeRefId::from_raw(la_arena::RawIdx::from_u32(lifetime))),
            ),
        }
    }

    /// Notes a lifetime that an argument of the signature being lowered names.
    fn named_lifetime(&mut self, region: Region<'db>) {
        let binders = self.bound_vars.len();
        let interner = self.interner;
        let Some(elision) = self.signature_mut() else { return };
        if elision.in_return_type {
            return;
        }
        let depth = binders - 1 - elision.binder;
        let region = match region.kind() {
            RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), bound) => {
                // What a binder inside the signature binds is not a lifetime of the signature.
                let Some(outside) = debruijn.as_usize().checked_sub(depth) else { return };
                Region::new_bound(interner, DebruijnIndex::from_usize(outside), bound)
            }
            _ => region,
        };
        if !elision.of_arguments.contains(&region) {
            elision.of_arguments.push(region);
        }
    }

    fn bound_vars(
        db: &'db Db,
        def: GenericDefId,
        generic: &'a OnceCell<Generics<'db>>,
    ) -> BoundVarKinds<'db> {
        let def_id = def.into();

        let generics = generic.get_or_init(|| generics(db, def));
        let args = generics.iter_self_late_bound().map(|(_, data)| match data {
            GenericParamDataRef::TypeParamData(..) => {
                BoundVariableKind::Ty(BoundTyKind::Param(def_id))
            }
            GenericParamDataRef::ConstParamData(..) => BoundVariableKind::Const,
            GenericParamDataRef::LifetimeParamData(..) => {
                BoundVariableKind::Region(BoundRegionKind::Named(def_id))
            }
        });

        BoundVarKinds::new_from_iter(args)
    }

    fn take_defined_opaques(&mut self) -> ThinVec<SelfAndAssocBounds> {
        exact_thin(self.impl_trait_mode.opaque_type_data.drain(..))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum ImplTraitLoweringMode {
    /// `impl Trait` gets lowered into an opaque type that doesn't unify with
    /// anything except itself. This is used in places where values flow 'out',
    /// i.e. for arguments of the function we're currently checking, and return
    /// types of functions we're calling.
    Opaque,
    /// `impl Trait` is disallowed and will be an error.
    #[default]
    Disallowed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifetimeLoweringMode {
    /// Lowers the late bound lifetimes to `ReBound`, used in cases when lowering
    /// from outside of function.
    Bound,
    /// Lowers the late bound lifetimes to `ReLateParam`, used in cases when lowering
    /// inside the function itself
    LateParam,
}

impl<'db, 'a> TyLoweringContext<'db, 'a> {
    pub fn lower_ty(&mut self, type_ref: TypeRefId) -> Ty<'db> {
        self.lower_ty_ext(type_ref).0
    }

    pub(crate) fn lower_const(
        &mut self,
        const_ref: ConstRef,
        const_type: Ty<'db>,
        allow_anon_const_using_generic_params: bool,
    ) -> Const<'db> {
        self.lower_expr_as_const(const_ref.expr, const_type, allow_anon_const_using_generic_params)
    }

    pub(crate) fn lower_expr_as_const(
        &mut self,
        expr_id: ExprId,
        const_type: Ty<'db>,
        allow_anon_const_using_generic_params: bool,
    ) -> Const<'db> {
        let types = self.types;
        #[expect(clippy::manual_map, reason = "a `map()` here generates a borrowck error")]
        let create_var = match &mut self.infer_vars {
            Some(infer_vars) => Some(&mut |span| infer_vars.next_const_var(span, None, types)),
            None => None,
        };
        let konst = create_anon_const(
            self.interner,
            self.def,
            self.store,
            expr_id,
            self.resolver,
            const_type,
            &|| self.generics.get_or_init(|| generics(self.db, self.generic_def)),
            create_var,
            self.interning_mode,
            self.forbid_params_after,
            allow_anon_const_using_generic_params,
            true,
        );

        if let Ok(konst) = konst
            && let ConstKind::Alias(_, konst) = konst.kind()
            && let AliasConstKind::Anon { def_id: AnonConstIdWrapper(konst) } = konst.kind
        {
            self.defined_anon_consts.push(konst);
        }

        konst.unwrap_or_else(|reason| self.invalid_const_arg(expr_id.into(), reason))
    }

    /// `path`, written at `site` where a type or a const could stand, as the const argument it
    /// is.
    pub(crate) fn lower_path_as_const(&mut self, path: Path<'_>, site: TypeRefId) -> Const<'db> {
        path_to_const(self.db, self.resolver, &|| self.generics(), self.forbid_params_after, path)
            .unwrap_or_else(|reason| self.invalid_const_arg(site.into(), reason))
    }

    /// The error const of a const argument at `site` that is none, reported for `reason`, as
    /// rustc's `lower_const_arg` gives `ty::Const::new_error` after its error. A `_` where no
    /// inference is, as each other placeholder there, is E0121.
    fn invalid_const_arg(&mut self, site: Span, reason: CreateConstError) -> Const<'db> {
        match reason {
            CreateConstError::UnderscoreExpr => self.push_infer_vars_not_allowed(site),
            reason => self.push_diagnostic(TyLoweringDiagnostic::InvalidConstArg { site, reason }),
        }
        self.types.consts.error
    }

    fn generics(&self) -> &Generics<'db> {
        self.generics.get_or_init(|| generics(self.db, self.generic_def))
    }

    fn param_index_is_disallowed(&self, index: u32) -> bool {
        self.forbid_params_after.is_some_and(|disallow_params_after| index >= disallow_params_after)
    }

    fn type_param(&mut self, id: TypeParamId, index: u32) -> Ty<'db> {
        if self.param_index_is_disallowed(index) {
            // FIXME: Report an error.
            self.types.types.error
        } else {
            Ty::new_param(self.interner, id, index)
        }
    }

    fn region_param(
        &mut self,
        id: LifetimeParamId,
        index: u32,
        is_late_bound: bool,
    ) -> Region<'db> {
        if self.param_index_is_disallowed(index) {
            // FIXME: Report an error.
            self.types.regions.error
        } else {
            if is_late_bound {
                self.hrtb_region_param(
                    index,
                    DebruijnIndex::from_usize(self.in_binders.as_usize()),
                    id.parent,
                )
            } else {
                Region::new_early_param(self.interner, EarlyParamRegion { id, index })
            }
        }
    }

    fn hrtb_region_param(
        &self,
        index: u32,
        debruijn: DebruijnIndex,
        parent: GenericDefId,
    ) -> Region<'db> {
        if self.param_index_is_disallowed(index) {
            // FIXME: Report an error.
            self.types.regions.error
        } else {
            if self.lifetime_lowering_mode == LifetimeLoweringMode::Bound {
                Region::new_bound(
                    self.interner,
                    debruijn,
                    BoundRegion {
                        var: BoundVar::from_u32(index),
                        kind: BoundRegionKind::Named(parent.into()),
                    },
                )
            } else {
                let solver_def_id = parent.into();
                Region::new_late_param(
                    self.interner,
                    solver_def_id,
                    BoundRegion {
                        var: BoundVar::from_u32(index),
                        kind: BoundRegionKind::Named(solver_def_id),
                    },
                )
            }
        }
    }

    pub fn lower_ty_ext(&mut self, type_ref_id: TypeRefId) -> (Ty<'db>, Option<TypeNs>) {
        let interner = self.interner;
        let mut res = None;
        let type_ref = self.store.type_ref(type_ref_id);
        let ty = match type_ref {
            TypeRef::Never => self.types.types.never,
            TypeRef::Tuple(inner) => {
                let inner_tys = inner.iter().map(|&tr| self.lower_ty(tr));
                Ty::new_tup_from_iter(interner, inner_tys)
            }
            TypeRef::Path(path) => {
                let (ty, res_) =
                    self.lower_path(path, PathId::from_type_ref_unchecked(type_ref_id));
                res = res_;
                ty
            }
            TypeRef::TypeParam(type_param_id) => {
                res = Some(TypeNs::GenericParam(type_param_id));

                let generics = self.generics();
                let idx = generics.type_or_const_param_idx(type_param_id.into());
                self.type_param(type_param_id, idx)
            }
            TypeRef::RawPtr(inner, mutability) => {
                let inner_ty = self.lower_ty(inner);
                Ty::new(interner, TyKind::RawPtr(inner_ty, lower_mutability(mutability)))
            }
            TypeRef::Array(array) => {
                let inner_ty = self.lower_ty(array.ty);
                let const_len = self.lower_const(array.len, self.types.types.usize, false);
                Ty::new_array_with_const_len(interner, inner_ty, const_len)
            }
            TypeRef::Slice(inner) => {
                let inner_ty = self.lower_ty(inner);
                Ty::new_slice(interner, inner_ty)
            }
            TypeRef::Reference(ref_) => {
                let lifetime = match ref_.lifetime {
                    Some(lifetime) => self.lower_lifetime(lifetime),
                    None => self.anonymous_lifetime(ElidedLifetimeSite::Reference(type_ref_id)),
                };
                let inner_ty = match self.store.type_ref(ref_.ty) {
                    // `&'a dyn Trait` is `&'a (dyn Trait + 'a)`.
                    TypeRef::DynTrait(bounds) if !lifetime.is_error() || self.impl_header => {
                        let object = self.object_default(lifetime);
                        self.lower_dyn_trait(bounds, object)
                    }
                    _ => self.lower_ty(ref_.ty),
                };
                Ty::new_ref(interner, lifetime, inner_ty, lower_mutability(ref_.mutability))
            }
            TypeRef::Placeholder => self.next_ty_var(type_ref_id.into()),
            TypeRef::Fn(fn_) => self.lower_fn_ptr(fn_),
            TypeRef::DynTrait(bounds) => {
                let object = if self.infers_object_lifetimes() {
                    self.next_region_var(Span::Dummy)
                } else {
                    Region::new_static(interner)
                };
                self.lower_dyn_trait(bounds, object)
            }
            TypeRef::ImplTrait(bounds) => {
                match self.impl_trait_mode.mode {
                    ImplTraitLoweringMode::Opaque => {
                        let origin = match self.resolver.generic_def() {
                            Some(GenericDefId::FunctionId(it)) => Either::Left(it),
                            Some(GenericDefId::TypeAliasId(it)) => Either::Right(it),
                            _ => panic!(
                                "opaque impl trait lowering must be in function or type alias"
                            ),
                        };

                        // this dance is to make sure the data is in the right
                        // place even if we encounter more opaque types while
                        // lowering the bounds
                        let idx = ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(
                            self.impl_trait_mode.opaque_type_data.len() as u32,
                        ));
                        self.impl_trait_mode.opaque_type_data.push(SelfAndAssocBounds {
                            bounds: StoredEarlyBinder::bind_no_rigid_aliases(
                                Clauses::empty().store(),
                            ),
                            assoc_ty_bounds_start: 0,
                            const_bounds_start: 0,
                        });

                        let impl_trait_id = origin.either(
                            |f| ImplTraitId::ReturnTypeImplTrait(f, idx),
                            |a| ImplTraitId::TypeAliasImplTrait(a, idx),
                        );
                        let opaque_ty_id = InternedOpaqueTyId::new(self.db, impl_trait_id);

                        // We don't want to lower the bounds inside the binders
                        // we're currently in, because they don't end up inside
                        // those binders. E.g. when we have `impl Trait<impl
                        // OtherTrait<T>>`, the `impl OtherTrait<T>` can't refer
                        // to the self parameter from `impl Trait`, and the
                        // bounds aren't actually stored nested within each
                        // other, but separately. So if the `T` refers to a type
                        // parameter of the outer function, it's just one binder
                        // away instead of two.
                        let actual_opaque_type_data = self
                            .with_debruijn(DebruijnIndex::ZERO, |ctx| {
                                ctx.lower_impl_trait(opaque_ty_id, bounds)
                            });
                        self.impl_trait_mode.opaque_type_data[idx.into_raw().into_u32() as usize] =
                            actual_opaque_type_data;

                        let mut late_bound_index = 0;
                        let args = GenericArgs::for_item(
                            self.interner,
                            opaque_ty_id.into(),
                            |index, param_id, lt_param, _| {
                                // A lifetime the function's parameters leave out, an error as
                                // theirs are.
                                if lt_param.is_none()
                                    && matches!(param_id, GenericParamId::LifetimeParamId(_))
                                    && !self.is_lowering_impl_trait_bounds
                                {
                                    return self.types.regions.error.into();
                                }
                                if let Some(lt) = lt_param
                                    && lt.is_late_bound()
                                    && !self.is_lowering_impl_trait_bounds
                                {
                                    let GenericParamId::LifetimeParamId(id) = param_id else {
                                        unreachable!()
                                    };
                                    let bound_region_kind =
                                        BoundRegionKind::Named(id.parent.into());
                                    let region = match self.lifetime_lowering_mode {
                                        LifetimeLoweringMode::Bound => Region::new_bound(
                                            interner,
                                            self.in_binders,
                                            BoundRegion {
                                                var: BoundVar::from_u32(late_bound_index),
                                                kind: bound_region_kind,
                                            },
                                        ),
                                        LifetimeLoweringMode::LateParam => Region::new_late_param(
                                            interner,
                                            self.generic_def.into(),
                                            BoundRegion {
                                                var: BoundVar::from_u32(late_bound_index),
                                                kind: bound_region_kind,
                                            },
                                        ),
                                    };
                                    late_bound_index += 1;
                                    return region.into();
                                }

                                mk_param(interner, index - late_bound_index, param_id)
                            },
                        );
                        Ty::new_alias(
                            self.interner,
                            IsRigid::No,
                            AliasTy::new_from_args(
                                self.interner,
                                AliasTyKind::Opaque { def_id: opaque_ty_id.into() },
                                args,
                            ),
                        )
                    }
                    ImplTraitLoweringMode::Disallowed => {
                        self.push_diagnostic(TyLoweringDiagnostic::ImplTraitNotAllowed {
                            source: type_ref_id,
                        });
                        self.types.types.error
                    }
                }
            }
            TypeRef::PatternType(ty, pat) => {
                let ty = self.lower_ty(ty);
                let Some(pat) = self.lower_pattern_type(type_ref_id, pat, ty) else {
                    return (self.types.types.error, res);
                };
                Ty::new_pat(self.interner, ty, pat)
            }
            TypeRef::UnsafeBinder { lifetimes, ty } => {
                // rustc's resolver: the binder's lifetimes are late-bound, and an elided one
                // inside is refused (`AnonymousReportError`).
                let old_lifetime_elision = mem::replace(
                    &mut self.lifetime_elision,
                    LifetimeElisionKind::AnonymousReportError,
                );
                let (inner, bound_vars) =
                    self.with_shifted_in(self.store.names(lifetimes), |ctx| ctx.lower_ty(ty));
                self.lifetime_elision = old_lifetime_elision;
                Ty::new_unsafe_binder(interner, Binder::bind_with_vars(inner, bound_vars))
            }
            TypeRef::Error => self.types.types.error,
        };
        (ty, res)
    }

    /// `None` when the pattern is missing (a parse error) or is not lowered, which is reported.
    fn lower_pattern_type(
        &mut self,
        source: TypeRefId,
        pat: PatId,
        ty: Ty<'db>,
    ) -> Option<Pattern<'db>> {
        let unsupported = |reason| TyLoweringDiagnostic::PatternTypeNotLowered { source, reason };
        let pat_kind = match self.store.pat(pat) {
            hir_def::hir::Pat::Range {
                start: Some(start),
                end: Some(end),
                range_type: hir_def::hir::RangeOp::Inclusive,
            } => {
                // rustc allows using generic params in pattern types.
                // In practice this does not matter since this feature is internal and I didn't find a way to actually use generic params,
                // but it doesn't matter so...
                rustc_type_ir::PatternKind::Range {
                    start: self.lower_expr_as_const(start, ty, true),
                    end: self.lower_expr_as_const(end, ty, true),
                }
            }
            hir_def::hir::Pat::NotNull => rustc_type_ir::PatternKind::NotNull,
            hir_def::hir::Pat::Or(pats) => rustc_type_ir::PatternKind::Or(
                PatList::new_from_iter(
                    pats.iter().map(|&pat| self.lower_pattern_type(source, pat, ty).ok_or(())),
                )
                .ok()?,
            ),
            hir_def::hir::Pat::Missing => return None,
            _ => {
                self.push_diagnostic(unsupported(PatternTypeUnsupported::Pattern));
                return None;
            }
        };
        Some(Pattern::new(self.interner, pat_kind))
    }

    /// The types a closure's parameters are written with, pushed to `lowered`, and what their
    /// signature binds: as rustc, a lifetime they leave out is a late-bound one of it,
    /// `|h: &H|` takes a `&H` of any lifetime.
    pub(crate) fn lower_closure_params(
        &mut self,
        params: &[Option<TypeRefId>],
        lowered: &mut impl Extend<Option<Ty<'db>>>,
    ) -> BoundVarKinds<'db> {
        let (bound_vars, _) = self.with_shifted_in(&[], |ctx| {
            ctx.with_elision(true, |ctx| {
                lowered.extend(params.iter().map(|&it| it.map(|it| ctx.lower_ty(it))));
                ctx.bound_vars_with_elided()
            })
        });
        bound_vars
    }

    fn lower_fn_ptr(&mut self, fn_: &FnType) -> Ty<'db> {
        let interner = self.interner;
        let (params, ret_ty) = fn_.split_params_and_ret();
        let old_lifetime_elision = self.lifetime_elision;
        let mut args = Vec::with_capacity_in(fn_.params.len(), self.frame);
        let binder = fn_.binder.as_ref().map(|b| b.as_ref()).unwrap_or_default();
        let (binder, _) = self.with_shifted_in(binder, |ctx: &mut TyLoweringContext<'_, '_>| {
            ctx.with_elision(true, |ctx| {
                ctx.lifetime_elision =
                    LifetimeElisionKind::AnonymousCreateParameter { report_in_path: false };
                args.extend(params.iter().map(|&(_, tr)| ctx.lower_ty(tr)));
                let elides = ctx.return_type_elides();
                let ret_elision = LifetimeElisionKind::for_fn_ret(interner, elides);
                args.push(ctx.with_return_type_elision(ret_elision, |ctx| {
                    ctx.in_return_type(|ctx| ctx.lower_ty(ret_ty))
                }));
                ctx.bound_vars_with_elided()
            })
        });
        self.lifetime_elision = old_lifetime_elision;

        Ty::new_fn_ptr(
            interner,
            Binder::bind_with_vars(
                FnSig {
                    fn_sig_kind: FnSigKind::dummy()
                        .set_abi(fn_.abi)
                        .set_safety(if fn_.is_unsafe { Safety::Unsafe } else { Safety::Safe })
                        .set_c_variadic(fn_.is_varargs),
                    inputs_and_output: Tys::new_from_slice(&args),
                },
                binder,
            ),
        )
    }

    /// This is only for [`resolve_type_param_assoc_type_shorthand`], where we can't just
    /// lower the self types of the predicates since that could lead to cycles.
    /// So we just check here if the `type_ref` resolves to a generic param, and which.
    fn lower_ty_only_param(&self, type_ref: TypeRefId) -> Option<TypeOrConstParamId> {
        let type_ref = self.store.type_ref(type_ref);
        let path = match type_ref {
            TypeRef::Path(path) => path,
            TypeRef::TypeParam(idx) => return Some(idx.into()),
            _ => return None,
        };
        if path.type_anchor().is_some() {
            return None;
        }
        if path.segments().len() > 1 {
            return None;
        }
        let resolution = match self.resolver.resolve_path_in_type_ns(self.db, path) {
            Some((it, None, _)) => it,
            _ => return None,
        };
        match resolution {
            TypeNs::GenericParam(param_id) => Some(param_id.into()),
            _ => None,
        }
    }

    #[inline]
    fn on_path_diagnostic_callback<'b>(type_ref: TypeRefId) -> PathDiagnosticCallback<'b, 'db> {
        PathDiagnosticCallback {
            data: Either::Left(PathDiagnosticCallbackData(type_ref)),
            callback: |data, this, diag| {
                let type_ref = data.as_ref().left().unwrap().0;
                if let PathLoweringDiagnostic::ElisionFailure {
                    generics_source,
                    def,
                    expected_count,
                } = diag
                {
                    this.push_elision_failure(ElidedLifetimeSite::Path {
                        source: type_ref,
                        generics_source,
                        def,
                        expected_count,
                    });
                    return;
                }
                this.push_diagnostic(TyLoweringDiagnostic::PathDiagnostic {
                    source: type_ref,
                    diag,
                })
            },
        }
    }

    #[inline]
    fn at_path(&mut self, path_id: PathId) -> PathLoweringContext<'_, 'a, 'db> {
        let hygiene = self.store.type_path_hygiene(path_id.type_ref());
        PathLoweringContext::new(
            self,
            Self::on_path_diagnostic_callback(path_id.type_ref()),
            self.store.path(path_id),
        )
        .with_hygiene(hygiene)
    }

    pub(crate) fn lower_path(&mut self, path: Path<'_>, path_id: PathId) -> (Ty<'db>, Option<TypeNs>) {
        // Resolve the path (in type namespace)
        if let Some(type_ref) = path.type_anchor() {
            let (ty, res) = self.lower_ty_ext(type_ref);
            let mut ctx = self.at_path(path_id);
            return ctx.lower_ty_relative_path(ty, res, false, path_id.type_ref().into());
        }

        let mut ctx = self.at_path(path_id);
        let (resolution, remaining_index) = match ctx.resolve_path_in_type_ns() {
            Some(it) => it,
            None => return (self.types.types.error, None),
        };

        if matches!(resolution, TypeNs::TraitId(_)) && remaining_index.is_none() {
            // trait object type without dyn
            let bound = TypeBound::Path(path_id, TraitBoundModifier::None);
            let ty = self.lower_dyn_trait(&[bound], Region::new_static(self.interner));
            return (ty, None);
        }

        ctx.lower_partly_resolved_path(resolution, false, path_id.type_ref().into())
    }

    /// The type the qualifier of the value path `path` names, `A<'a>` of `A::<'a>::C`, where
    /// it is a struct, enum, union or type alias.
    pub fn lower_value_path_qualifier(&mut self, path: Path<'_>) -> Option<Ty<'db>> {
        let qualifier = path.qualifier()?;
        let on_diagnostic = PathDiagnosticCallback {
            data: Either::Left(PathDiagnosticCallbackData(TypeRefId::from_raw(
                la_arena::RawIdx::from_u32(0),
            ))),
            callback: |_, _, _| {},
        };
        let mut ctx = PathLoweringContext::new(self, on_diagnostic, qualifier.path());
        match ctx.resolve_path_in_type_ns()? {
            (resolution @ (TypeNs::AdtId(_) | TypeNs::TypeAliasId(_)), None) => {
                Some(ctx.lower_partly_resolved_path(resolution, false, Span::Dummy).0)
            }
            _ => None,
        }
    }

    fn lower_trait_ref_from_path(
        &mut self,
        path_id: PathId,
        explicit_self_ty: Ty<'db>,
    ) -> Option<(TraitRef<'db>, PathLoweringContext<'_, 'a, 'db>)> {
        let mut ctx = self.at_path(path_id);
        let resolved = match ctx.resolve_path_in_type_ns_fully()? {
            // FIXME(trait_alias): We need to handle trait alias here.
            TypeNs::TraitId(tr) => tr,
            _ => return None,
        };
        Some((
            ctx.lower_trait_ref_from_resolved_path(
                resolved,
                explicit_self_ty,
                false,
                path_id.type_ref().into(),
            ),
            ctx,
        ))
    }

    fn lower_trait_ref(
        &mut self,
        trait_ref: HirTraitRef,
        explicit_self_ty: Ty<'db>,
    ) -> Option<TraitRef<'db>> {
        self.lower_trait_ref_from_path(trait_ref.path, explicit_self_ty).map(|it| it.0)
    }

    pub(crate) fn lower_where_predicate<'b>(
        &'b mut self,
        where_predicate: &'b WherePredicate,
        ignore_bindings: bool,
    ) -> impl Iterator<Item = (Clause<'db>, GenericPredicateSource)> + use<'a, 'b, 'db> {
        let lower_type_outlives = |ctx: &mut TyLoweringContext<'db, 'a>,
                                   target: &TypeRefId,
                                   bound,
                                   under_for: bool| {
            let self_ty = ctx.lower_ty(*target);
            // As rustc's `poly_trait_ref_binder_info`, a `for<'b>` bound of `for<'a> T: ..`
            // adds to the predicate's binder (after E0316): `for<'a, 'b>`.
            ctx.bound_of_assoc_ty = under_for;
            Either::Left(ctx.lower_type_bound(bound, self_ty, ignore_bindings))
        };

        match where_predicate {
            WherePredicate::TypeBound { lifetimes, target, bound } => match lifetimes {
                Some(lifetimes) => {
                    self.with_shifted_in(self.store.names(*lifetimes), |ctx| {
                        lower_type_outlives(ctx, target, bound, true)
                    })
                    .0
                }
                None => lower_type_outlives(self, target, bound, false),
            },
            &WherePredicate::Lifetime { bound, target } => Either::Right(iter::once((
                Clause(Predicate::new(
                    self.interner,
                    Binder::dummy(rustc_type_ir::PredicateKind::Clause(
                        rustc_type_ir::ClauseKind::RegionOutlives(OutlivesClause(
                            self.lower_lifetime(target),
                            self.lower_lifetime(bound),
                        )),
                    )),
                )),
                GenericPredicateSource::SelfOnly,
            ))),
        }
        .into_iter()
    }

    /// The trait of `ty`'s implicit sizedness bound, for the bounds lowered on it so far.
    fn implicit_sizedness_trait(&self, ty: Ty<'db>, of_trait_self: bool) -> Option<TraitId> {
        let written = self.unsized_types.get(&ty).copied().unwrap_or_default();
        match written.implicit(of_trait_self)? {
            ImplicitSizedness::Sized => self.lang_items.Sized,
            ImplicitSizedness::MetaSized => self.lang_items.MetaSized,
        }
    }

    pub(crate) fn lower_type_bound<'b>(
        &'b mut self,
        bound: &'b TypeBound,
        self_ty: Ty<'db>,
        ignore_bindings: bool,
    ) -> impl Iterator<Item = (Clause<'db>, GenericPredicateSource)> + use<'a, 'db> {
        let interner = self.interner;
        let frame = self.frame;
        let sized = self.lang_items.Sized;
        let meta_sized = self.lang_items.MetaSized;
        let pointee_sized = self.lang_items.PointeeSized;

        let mut assoc_bounds = None;
        let mut clause = None;

        let mut lower_path_bound = |ctx: &mut TyLoweringContext<'db, '_>, path| {
            let Some((trait_ref, mut ctx)) = ctx.lower_trait_ref_from_path(path, self_ty) else {
                return;
            };
            // Lowering the path bound the lifetimes left out of it.
            let binder = ctx.ty_ctx().bound_vars_with_elided();
            {
                let trait_id = Some(trait_ref.def_id.0);
                if trait_id == sized {
                    ctx.ty_ctx().unsized_types.entry(self_ty).or_default().sized = true;
                } else if trait_id == meta_sized {
                    ctx.ty_ctx().unsized_types.entry(self_ty).or_default().meta_sized = true;
                }
                // As rustc's lowering, a `PointeeSized` bound is no clause: every type is one.
                if trait_id == pointee_sized {
                    ctx.ty_ctx().unsized_types.entry(self_ty).or_default().pointee_sized = true;
                } else {
                    if !ignore_bindings {
                        assoc_bounds = ctx
                            .assoc_type_bindings_from_type_bound(trait_ref, path.type_ref().into())
                            .map(|iter| {
                                let mut bounds = Vec::new_in(frame);
                                bounds.extend(iter);
                                bounds
                            });
                    }
                    clause = Some(Clause(Predicate::new(
                        interner,
                        Binder::bind_with_vars(
                            rustc_type_ir::PredicateKind::Clause(rustc_type_ir::ClauseKind::Trait(
                                TraitClause {
                                    trait_ref,
                                    polarity: rustc_type_ir::ClausePolarity::Positive,
                                },
                            )),
                            binder,
                        ),
                    )));
                }
            }
        };

        let of_assoc_ty = mem::take(&mut self.bound_of_assoc_ty);
        match bound {
            // As rustc, `for<'a> T: A<'a, X: for<'b> B<'a, 'b>>` bounds `X` with
            // `for<'a, 'b> <T as A<'a>>::X: B<'a, 'b>`: a binder of its own would leave `'a`
            // escaping. The variables a level binds come before those left out of it, so a
            // level that already has one of those keeps the binder of its own.
            &TypeBound::ForLifetime(binder, path)
                if of_assoc_ty
                    && self.bound_vars_with_elided().len() == self.peek_bound_vars().len() =>
            {
                let binder = self.store.names(binder);
                let (names, kinds) = (self.bound_names.len(), self.peek_bound_vars());
                self.bound_names.extend(binder.iter().cloned());
                self.bound_vars.last_mut().unwrap().1 = BoundVarKinds::new_from_iter(
                    kinds.iter().chain(binder.iter().map(for_lifetime_kind)),
                );
                self.with_elision(false, |ctx| lower_path_bound(ctx, path));
                self.bound_names.truncate(names);
                self.bound_vars.last_mut().unwrap().1 = kinds;
            }
            &TypeBound::ForLifetime(binder, path) => {
                self.with_shifted_in(self.store.names(binder), |ctx| {
                    ctx.with_elision(false, |ctx| lower_path_bound(ctx, path))
                })
                .0
            }
            &TypeBound::Path(
                path,
                TraitBoundModifier::None
                | TraitBoundModifier::Const
                | TraitBoundModifier::MaybeConst,
            ) => self.with_elision(false, |ctx| lower_path_bound(ctx, path)),
            &TypeBound::Path(path, TraitBoundModifier::Maybe) => {
                let sized_trait = self.lang_items.Sized;
                // Don't lower associated type bindings as the only possible relaxed trait bound
                // `?Sized` has no of them.
                // If we got another trait here ignore the bound completely.
                let trait_id = self
                    .lower_trait_ref_from_path(path, self_ty)
                    .map(|(trait_ref, _)| trait_ref.def_id.0);
                if trait_id == sized_trait {
                    self.unsized_types.entry(self_ty).or_default().relaxed = true;
                }
            }
            &TypeBound::Lifetime(l) => {
                let lifetime = self.lower_lifetime(l);
                let binder = self.peek_bound_vars();
                clause = Some(Clause(Predicate::new(
                    self.interner,
                    Binder::bind_with_vars(
                        rustc_type_ir::PredicateKind::Clause(
                            rustc_type_ir::ClauseKind::TypeOutlives(OutlivesClause(
                                self_ty, lifetime,
                            )),
                        ),
                        binder,
                    ),
                )));
            }
            TypeBound::Use(_) | TypeBound::Error => {}
        }
        clause
            .into_iter()
            .map(|pred| (pred, GenericPredicateSource::SelfOnly))
            .chain(assoc_bounds.into_iter().flatten())
    }

    /// `default_region` is the lifetime of the type when the bounds name none.
    fn lower_dyn_trait(&mut self, bounds: &[TypeBound], default_region: Region<'db>) -> Ty<'db> {
        let interner = self.interner;
        let dummy_self_ty = self.types.types.dyn_trait_dummy_self;
        let mut region = None;
        // `trait Any: 'static` makes `dyn Any` be `dyn Any + 'static` wherever it is written.
        let mut region_of_trait = None;
        // INVARIANT: The principal trait bound, if present, must come first. Others may be in any
        // order but should be in the same order for the same set but possibly different order of
        // bounds in the input.
        // INVARIANT: If this function returns `DynTy`, there should be at least one trait bound.
        // These invariants are utilized by `TyExt::dyn_trait()` and chalk.
        let bounds = 'bounds: {
            let frame = self.frame;
            let mut principal = None;
            let mut auto_traits = SmallVec::<[_; 3]>::new();
            let mut projections = Vec::new_in(frame);
            let mut had_error = false;
            let mut principal_source = None;
            // alloc: an error's sites, empty but for one
            let mut additional = Vec::new();

            for b in bounds {
                let db = self.db;
                let source = match b {
                    TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path) => {
                        Some(path.type_ref())
                    }
                    TypeBound::Lifetime(_) | TypeBound::Use(_) | TypeBound::Error => None,
                };
                match b {
                    TypeBound::Path(
                        _,
                        TraitBoundModifier::None
                        | TraitBoundModifier::Const
                        | TraitBoundModifier::MaybeConst,
                    ) => {
                        // `dyn Trait<'a>` is an existential predicate that introduces a binder.
                        self.with_shifted_in(&[], |ctx| {
                            ctx.lower_type_bound(b, dummy_self_ty, false)
                        })
                        .0
                    }
                    _ => self.lower_type_bound(b, dummy_self_ty, false),
                }
                .for_each(|(b, _)| {
                    match b.kind().skip_binder() {
                        rustc_type_ir::ClauseKind::Trait(t) => {
                            let id = t.def_id();
                            let is_auto =
                                TraitSignature::of(db, id.0).flags.contains(TraitFlags::AUTO);
                            if is_auto {
                                auto_traits.push(t.def_id().0);
                            } else {
                                let source = source.expect("a trait bound has a path");
                                if principal.is_some() {
                                    additional.push(source);
                                    had_error = true;
                                } else {
                                    principal_source = Some(source);
                                }
                                principal = Some(b.kind().rebind(t.trait_ref));
                            }
                        }
                        rustc_type_ir::ClauseKind::Projection(p) => {
                            let source = source.expect("a projection bound has a path");
                            projections.push((b.kind().rebind(p), source));
                        }
                        rustc_type_ir::ClauseKind::TypeOutlives(outlives_predicate) => {
                            // E0226, which AST validation reports.
                            if region.is_some() {
                                had_error = true;
                            }
                            region = Some(outlives_predicate.1);
                        }
                        rustc_type_ir::ClauseKind::RegionOutlives(_)
                        | rustc_type_ir::ClauseKind::ConstArgHasType(_, _)
                        | rustc_type_ir::ClauseKind::WellFormed(_)
                        | rustc_type_ir::ClauseKind::ConstEvaluatable(_)
                        | rustc_type_ir::ClauseKind::HostEffect(_)
                        | rustc_type_ir::ClauseKind::UnstableFeature(_) => unreachable!(),
                    }
                })
            }

            if !additional.is_empty() {
                let first = principal_source.expect("a second non-auto trait has a first");
                // alloc: an error's sites
                let additional = additional.into_boxed_slice();
                self.push_diagnostic(TyLoweringDiagnostic::AdditionalNonAutoTraits { first, additional });
            }

            if had_error {
                break 'bounds None;
            }

            if principal.is_none() && auto_traits.is_empty() {
                // No traits is not allowed. With no trait written the parser reports it, and a
                // path that does not name a trait is reported where it is resolved; bounds
                // that are all relaxed (`dyn ?Sized`) are reported here.
                let relaxed = bounds.iter().find_map(|b| match b {
                    TypeBound::Path(path, TraitBoundModifier::Maybe) => Some(path.type_ref()),
                    _ => None,
                });
                let only_relaxed = bounds.iter().all(|b| {
                    !matches!(b, TypeBound::ForLifetime(..))
                        && !matches!(b, TypeBound::Path(_, modifier) if *modifier != TraitBoundModifier::Maybe)
                });
                if let Some(source) = relaxed
                    && only_relaxed
                {
                    self.push_diagnostic(TyLoweringDiagnostic::ObjectWithoutTrait { source });
                }
                break 'bounds None;
            }

            // `Send + Sync` is the same as `Sync + Send`.
            auto_traits.sort_unstable();
            // Duplicate auto traits are permitted.
            auto_traits.dedup();

            // Map the projection bounds onto a key that makes it easy to remove redundant
            // bounds that are constrained by supertraits of the principal def id.
            //
            // Also make sure we detect conflicting bounds from expanding a trait alias and
            // also specifying it manually, like:
            // ```
            // type Alias = Trait<Assoc = i32>;
            // let _: &dyn Alias<Assoc = u32> = /* ... */;
            // ```
            let mut projection_bounds = frame_map(frame);
            for (proj, source) in projections {
                let term = proj.skip_binder().def_id().0;
                let key = (
                    term,
                    interner.anonymize_bound_vars(
                        proj.map_bound(|proj| proj.projection_term.trait_ref(interner)),
                    ),
                );
                if let Some(old_proj) = projection_bounds.insert(key, proj)
                    && interner.anonymize_bound_vars(proj)
                        != interner.anonymize_bound_vars(old_proj)
                {
                    let assoc = match term {
                        TermId::TypeAliasId(it) => AssocItemId::TypeAliasId(it),
                        TermId::ConstId(it) => AssocItemId::ConstId(it),
                    };
                    self.push_diagnostic(TyLoweringDiagnostic::ConflictingAssocTypeBindings {
                        source,
                        assoc,
                    });
                }
            }

            // A stable ordering of associated types from the principal trait and all its
            // supertraits. We use this to ensure that different substitutions of a trait
            // don't result in `dyn Trait` types with different projections lists, which
            // can be unsound: <https://github.com/rust-lang/rust/pull/136458>.
            // We achieve a stable ordering by walking over the unsubstituted principal
            // trait ref.
            let mut ordered_associated_types = Vec::new_in(frame);

            if let Some(principal_trait) = principal {
                // Generally we should not elaborate in lowering as this can lead to cycles, but
                // here rustc cycles as well.
                for clause in elaborate::elaborate(
                    interner,
                    [Clause::upcast_from(
                        TraitRef::identity(interner, principal_trait.def_id()),
                        interner,
                    )],
                )
                .filter_only_self()
                {
                    let clause = clause.instantiate_supertrait(interner, principal_trait);
                    debug!("observing object predicate `{clause:?}`");

                    let bound_predicate = clause.kind();
                    match bound_predicate.skip_binder() {
                        ClauseKind::TypeOutlives(OutlivesClause(ty, outlived))
                            if ty == dummy_self_ty && !outlived.has_escaping_bound_vars() =>
                        {
                            if region_of_trait.is_none() || outlived.is_static() {
                                region_of_trait = Some(outlived);
                            }
                        }
                        ClauseKind::Trait(pred) => {
                            // FIXME(negative_bounds): Handle this correctly...
                            let trait_ref = interner
                                .anonymize_bound_vars(bound_predicate.rebind(pred.trait_ref));
                            ordered_associated_types.extend(
                                pred.trait_ref
                                    .def_id
                                    .0
                                    .trait_items(self.db)
                                    .associated_types()
                                    .map(|item| (item.into(), trait_ref)),
                            );
                        }
                        ClauseKind::Projection(pred) => {
                            let pred = bound_predicate.rebind(pred);
                            // A `Self` within the original bound will be instantiated with a
                            // `trait_object_dummy_self`, so check for that.
                            let references_self = match pred.skip_binder().term.kind() {
                                TermKind::Ty(ty) => {
                                    ty.walk().any(|arg| arg == dummy_self_ty.into())
                                }
                                // FIXME(mgca): We should walk the const instead of not doing anything
                                TermKind::Const(_) => false,
                            };

                            // If the projection output contains `Self`, force the user to
                            // elaborate it explicitly to avoid a lot of complexity.
                            //
                            // The "classically useful" case is the following:
                            // ```
                            //     trait MyTrait: FnMut() -> <Self as MyTrait>::MyOutput {
                            //         type MyOutput;
                            //     }
                            // ```
                            //
                            // Here, the user could theoretically write `dyn MyTrait<MyOutput = X>`,
                            // but actually supporting that would "expand" to an infinitely-long type
                            // `fix $ τ → dyn MyTrait<MyOutput = X, Output = <τ as MyTrait>::MyOutput`.
                            //
                            // Instead, we force the user to write
                            // `dyn MyTrait<MyOutput = X, Output = X>`, which is uglier but works. See
                            // the discussion in #56288 for alternatives.
                            if !references_self {
                                let key = (
                                    pred.skip_binder().def_id().0,
                                    interner.anonymize_bound_vars(pred.map_bound(|proj| {
                                        proj.projection_term.trait_ref(interner)
                                    })),
                                );
                                if !projection_bounds.contains_key(&key) {
                                    projection_bounds.insert(key, pred);
                                }
                            }
                        }
                        _ => (),
                    }
                }
            }

            // We compute the list of projection bounds taking the ordered associated types,
            // and check if there was an entry in the collected `projection_bounds`. Those
            // are computed by first taking the user-written associated types, then elaborating
            // the principal trait ref, and only using those if there was no user-written.
            // See note below about how we handle missing associated types with `Self: Sized`,
            // which are not required to be provided, but are still used if they are provided.
            let mut projection_bounds = {
                let mut bounds = Vec::new_in(frame);
                bounds.extend(
                    ordered_associated_types
                        .iter()
                        .filter_map(|key| projection_bounds.get(key).copied()),
                );
                bounds
            };

            projection_bounds.sort_unstable_by_key(|proj| proj.skip_binder().def_id().0);

            let principal_trait = principal.map(|it| it.skip_binder().def_id.0);
            // alloc: an error's parameters, empty but on an error
            let mut missing = Vec::new();
            let principal = principal.map(|principal| {
                principal.map_bound(|principal| {
                    // Verify that `dummy_self` did not leak inside default type parameters.
                    // Skip `Self`
                    let args = principal.args.iter().enumerate().skip(1).map(|(index, arg)| {
                        if arg.walk().any(|arg| arg == dummy_self_ty.into()) {
                            missing.push(index as u32);
                            self.types.types.error.into()
                        } else {
                            arg
                        }
                    });

                    ExistentialPredicate::Trait(ExistentialTraitRef::new(
                        interner,
                        principal.def_id,
                        args,
                    ))
                })
            });
            if !missing.is_empty() {
                let trait_ = principal_trait.expect("a principal's arguments have its trait");
                let source = principal_source.expect("a principal has its bound");
                let trait_generics = generics(self.db, trait_.into());
                let params = missing
                    .iter()
                    .map(|&index| {
                        trait_generics
                            .iter_id(false)
                            .find_map(|id| match id {
                                GenericParamId::TypeParamId(param)
                                    if trait_generics.type_or_const_param_idx(param.into())
                                        == index =>
                                {
                                    Some(param)
                                }
                                _ => None,
                            })
                            .expect("an argument naming `Self` is of a type parameter")
                    })
                    // alloc: an error's parameters
                    .collect();
                self.push_diagnostic(TyLoweringDiagnostic::MissingSelfDefaultParams {
                    source,
                    trait_,
                    params,
                });
            }

            let projections = projection_bounds.into_iter().map(|proj| {
                proj.map_bound(|mut proj| {
                    // Like for trait refs, verify that `dummy_self` did not leak inside default type
                    // parameters.
                    let references_self = proj.projection_term.args.iter().skip(1).any(|arg| {
                        if arg.walk().any(|arg| arg == dummy_self_ty.into()) {
                            return true;
                        }
                        false
                    });
                    if references_self {
                        proj.projection_term = replace_dummy_self_with_error(
                            interner,
                            self.types,
                            proj.projection_term,
                        );
                    }

                    ExistentialPredicate::Projection(ExistentialProjection::erase_self_ty(
                        interner, proj,
                    ))
                })
            });

            let auto_traits = auto_traits.into_iter().map(|auto_trait| {
                Binder::dummy(ExistentialPredicate::AutoTrait(auto_trait.into()))
            });

            // N.b. principal, projections, auto traits
            Some(BoundExistentialPredicates::new_from_iter(
                principal.into_iter().chain(projections).chain(auto_traits),
            ))
        };

        if let Some(bounds) = bounds {
            Ty::new_dynamic(
                self.interner,
                bounds,
                region.or(region_of_trait).unwrap_or(default_region),
            )
        } else {
            // FIXME: report error
            // (additional non-auto traits, associated type rebound, or no resolved trait)
            self.types.types.error
        }
    }

    fn lower_impl_trait(
        &mut self,
        def_id: InternedOpaqueTyId<'db>,
        bounds: &[TypeBound],
    ) -> SelfAndAssocBounds {
        let interner = self.interner;
        cov_mark::hit!(lower_rpit);
        let args = GenericArgs::identity_for_item(interner, def_id.into());
        let self_ty = Ty::new_alias(
            self.interner,
            IsRigid::No,
            AliasTy::new_from_args(interner, rustc_type_ir::Opaque { def_id: def_id.into() }, args),
        );
        let prev_is_lowering_impl_trait_bounds =
            mem::replace(&mut self.is_lowering_impl_trait_bounds, true);

        let mut predicates = Vec::new_in(self.frame);
        let mut assoc_ty_bounds = Vec::new_in(self.frame);
        let mut const_bounds = Vec::new_in(self.frame);
        for b in bounds {
            for (pred, source) in self.lower_type_bound(b, self_ty, false) {
                const_bounds.extend(maybe_const_bound(interner, b, pred, &source));
                match source {
                    GenericPredicateSource::SelfOnly => predicates.push(pred),
                    GenericPredicateSource::AssocTyBound => assoc_ty_bounds.push(pred),
                }
            }
        }

        if let Some(trait_id) = self.implicit_sizedness_trait(self_ty, false) {
            let trait_ref = TraitRef::new_from_args(
                interner,
                trait_id.into(),
                GenericArgs::new_from_slice(&[self_ty.into()]),
            );
            predicates.push(Clause(Predicate::new(
                interner,
                Binder::dummy(rustc_type_ir::PredicateKind::Clause(
                    rustc_type_ir::ClauseKind::Trait(TraitClause {
                        trait_ref,
                        polarity: rustc_type_ir::ClausePolarity::Positive,
                    }),
                )),
            )));
        }

        let assoc_ty_bounds_start = predicates.len() as u32;
        predicates.extend(assoc_ty_bounds);
        let const_bounds_start = predicates.len() as u32;
        predicates.extend(const_bounds);

        self.is_lowering_impl_trait_bounds = prev_is_lowering_impl_trait_bounds;
        SelfAndAssocBounds {
            bounds: StoredEarlyBinder::bind(interner, Clauses::new_from_slice(&predicates).store()),
            assoc_ty_bounds_start,
            const_bounds_start,
        }
    }

    pub(crate) fn lower_lifetime(&mut self, lifetime: LifetimeRefId) -> Region<'db> {
        if let LifetimeRef::Placeholder = self.store[lifetime] {
            return self.anonymous_lifetime(ElidedLifetimeSite::Placeholder(lifetime));
        }
        let region = self.named_region(lifetime);
        self.named_lifetime(region);
        region
    }

    fn named_region(&mut self, lifetime: LifetimeRefId) -> Region<'db> {
        if let Some(region) = self.find_and_lower_hrtb_lifetime(lifetime) {
            return region;
        };

        match self.resolver.resolve_lifetime(&self.store[lifetime]) {
            Some(resolution) => match resolution {
                LifetimeNs::Static => Region::new_static(self.interner),
                LifetimeNs::LifetimeParam(id) => {
                    let (idx, is_late_bound) =
                        self.generics().lifetime_param_idx(id, self.is_lowering_impl_trait_bounds);
                    self.region_param(id, idx, is_late_bound)
                }
            },
            None => Region::error(self.interner),
        }
    }

    fn find_and_lower_hrtb_lifetime(&mut self, lifetime: LifetimeRefId) -> Option<Region<'db>> {
        if let LifetimeRef::Named(lt_name) = &self.store[lifetime] {
            let mut end = self.bound_names.len();
            for (debruijn, &(start, _)) in self.bound_vars.iter().rev().enumerate() {
                let names = &self.bound_names[start as usize..end];
                // A `for<'a>` binds its lifetime in a body as much as in a signature.
                if let Some(index) = names.iter().position(|l| l == lt_name) {
                    return Some(Region::new_bound(
                        self.interner,
                        DebruijnIndex::from_usize(debruijn),
                        BoundRegion {
                            var: BoundVar::from_u32(index as u32),
                            kind: for_lifetime_region_kind(&names[index]),
                        },
                    ));
                }
                end = start as usize;
            }
            None
        } else {
            None
        }
    }
}

/// A lowered signature and what lowering it found: its diagnostics and the anonymous constants it
/// defined. Both lists are empty for most items, and an empty `ThinVec` allocates nothing.
#[derive(Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct TyLoweringResult<'db, T> {
    pub value: T,
    diagnostics: ThinVec<TyLoweringDiagnostic>,
    anon_consts: ThinVec<AnonConstId<'db>>,
}

const _: () = assert!(size_of::<TyLoweringResult<'static, ()>>() == 16);

/// A lowered signature as read: from a query of its own item ([`TyLoweringResult`]), or from a
/// row of a crate's batch (`lower::batch`), whose diagnostics sit in a side table.
pub struct Lowered<'db, T: ?Sized> {
    pub value: &'db T,
    diagnostics: &'db [TyLoweringDiagnostic],
    anon_consts: &'db [AnonConstId<'db>],
}

impl<T: ?Sized> Clone for Lowered<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for Lowered<'_, T> {}

const _: () = assert!(size_of::<Lowered<'static, ()>>() == 8 + 2 * 16);

impl<'db, T: ?Sized> Lowered<'db, T> {
    pub(crate) fn new(
        value: &'db T,
        diagnostics: &'db [TyLoweringDiagnostic],
        anon_consts: &'db [AnonConstId<'db>],
    ) -> Self {
        Lowered { value, diagnostics, anon_consts }
    }

    #[inline]
    pub fn diagnostics(&self) -> &'db [TyLoweringDiagnostic] {
        self.diagnostics
    }

    #[inline]
    pub fn defined_anon_consts(&self) -> &'db [AnonConstId<'db>] {
        self.anon_consts
    }
}

impl<'db, T> From<&'db TyLoweringResult<'db, T>> for Lowered<'db, T> {
    fn from(result: &'db TyLoweringResult<'db, T>) -> Self {
        result.as_lowered()
    }
}

impl<T: std::fmt::Debug + ?Sized> std::fmt::Debug for Lowered<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Lowered");
        debug.field("value", &self.value);
        if !self.diagnostics.is_empty() {
            debug.field("diagnostics", &self.diagnostics);
        }
        if !self.anon_consts.is_empty() {
            debug.field("defined_anon_consts", &self.anon_consts);
        }
        debug.finish()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for TyLoweringResult<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("TyLoweringResult");
        debug.field("value", &self.value);
        let diagnostics = self.diagnostics();
        if !diagnostics.is_empty() {
            debug.field("diagnostics", &diagnostics);
        }
        let defined_anon_consts = self.defined_anon_consts();
        if !defined_anon_consts.is_empty() {
            debug.field("defined_anon_consts", &defined_anon_consts);
        }
        debug.finish()
    }
}

impl<'db, T> TyLoweringResult<'db, T> {
    /// `value` with what lowering it found, each list allocated once at its length (none
    /// when empty, as for most items).
    pub(crate) fn new(
        value: T,
        diagnostics: impl IntoIterator<Item = TyLoweringDiagnostic, IntoIter: ExactSizeIterator>,
        defined_anon_consts: impl IntoIterator<Item = AnonConstId<'db>, IntoIter: ExactSizeIterator>,
    ) -> Self {
        Self {
            value,
            diagnostics: exact_thin(diagnostics),
            anon_consts: exact_thin(defined_anon_consts),
        }
    }

    fn from_ctx(value: T, mut ctx: TyLoweringContext<'db, '_>) -> Self {
        Self::new(value, ctx.diagnostics.drain(..), ctx.defined_anon_consts.drain(..))
    }

    pub(crate) fn empty(value: T) -> Self {
        Self { value, diagnostics: ThinVec::new(), anon_consts: ThinVec::new() }
    }

    #[inline]
    pub fn diagnostics(&self) -> &[TyLoweringDiagnostic] {
        &self.diagnostics
    }

    #[inline]
    pub fn defined_anon_consts(&self) -> &[AnonConstId<'db>] {
        &self.anon_consts
    }

    #[inline]
    pub fn as_lowered(&'db self) -> Lowered<'db, T> {
        Lowered { value: &self.value, diagnostics: &self.diagnostics, anon_consts: &self.anon_consts }
    }
}

/// A signature lowered in a run, with what lowering found still in the run's frame: a per-item
/// query makes its owned result of it ([`FrameLowered::owned`]), a crate's batch copies it
/// into its columns.
#[derive(Debug)]
pub(crate) struct FrameLowered<'f, 'db, T> {
    pub(crate) value: T,
    pub(crate) diagnostics: Vec<TyLoweringDiagnostic, &'f Frame>,
    pub(crate) anon_consts: Vec<AnonConstId<'db>, &'f Frame>,
}

impl<'f, 'db, T> FrameLowered<'f, 'db, T> {
    /// `value` with what `ctx` found, in `frame`: `ctx`'s lists live as long as its resolver's
    /// borrow, shorter than the caller's frame. Both are empty for most items, and an empty
    /// list moves nothing.
    fn from_ctx(value: T, mut ctx: TyLoweringContext<'db, '_>, frame: &'f Frame) -> Self {
        fn moved<'f, T>(from: &mut Vec<T, &Frame>, frame: &'f Frame) -> Vec<T, &'f Frame> {
            let mut to = Vec::with_capacity_in(from.len(), frame);
            to.extend(from.drain(..));
            to
        }
        FrameLowered {
            value,
            diagnostics: moved(&mut ctx.diagnostics, frame),
            anon_consts: moved(&mut ctx.defined_anon_consts, frame),
        }
    }

    pub(crate) fn empty(value: T, frame: &'f Frame) -> Self {
        FrameLowered { value, diagnostics: Vec::new_in(frame), anon_consts: Vec::new_in(frame) }
    }

    pub(crate) fn owned(self) -> TyLoweringResult<'db, T> {
        TyLoweringResult::new(self.value, self.diagnostics, self.anon_consts)
    }

    /// As [`FrameLowered::owned`], the value made owned by `f`.
    pub(crate) fn owned_with<U>(self, f: impl FnOnce(T) -> U) -> TyLoweringResult<'db, U> {
        TyLoweringResult::new(f(self.value), self.diagnostics, self.anon_consts)
    }
}

/// The kind of the region a `for<'a>` binds for `'a`: its name as written, which rustc's
/// `BoundRegionKind::Named` leads to through the parameter it names.
fn for_lifetime_region_kind<'db>(name: &Name) -> BoundRegionKind<DbInterner<'db>> {
    BoundRegionKind::NamedForPrinting(Symbol(*name.symbol()))
}

fn for_lifetime_kind<'db>(name: &Name) -> BoundVariableKind<'db> {
    BoundVariableKind::Region(for_lifetime_region_kind(name))
}

fn replace_dummy_self_with_error<'db, T: TypeFoldable<DbInterner<'db>>>(
    interner: DbInterner<'db>,
    types: &DefaultAny<'db>,
    t: T,
) -> T {
    t.fold_with(&mut BottomUpFolder {
        interner,
        ty_op: |ty| {
            if ty == types.types.dyn_trait_dummy_self { types.types.error } else { ty }
        },
        lt_op: |lt| lt,
        ct_op: |ct| ct,
    })
}

pub fn lower_mutability(m: hir_def::type_ref::Mutability) -> Mutability {
    match m {
        hir_def::type_ref::Mutability::Shared => Mutability::Not,
        hir_def::type_ref::Mutability::Mut => Mutability::Mut,
    }
}

/// [`lower_mutability`] back: the solver's mutability as the HIR writes it.
pub fn hir_mutability(m: Mutability) -> hir_def::type_ref::Mutability {
    match m {
        Mutability::Not => hir_def::type_ref::Mutability::Shared,
        Mutability::Mut => hir_def::type_ref::Mutability::Mut,
    }
}

pub(crate) fn impl_trait_query<'db>(
    db: &'db Db,
    impl_id: ImplId,
) -> Option<EarlyBinder<'db, TraitRef<'db>>> {
    impl_trait_with_diagnostics(db, impl_id).map(|it| it.value.get(DbInterner::new_no_crate(db)))
}

/// The trait an impl implements, `None` for an inherent impl. A dependency crate's module-level
/// impls are read from its batch (`lower::batch::batched_crate`).
pub(crate) fn impl_trait_with_diagnostics<'db>(
    db: &'db Db,
    impl_id: ImplId,
) -> Option<Lowered<'db, StoredEarlyBinder<StoredTraitRef>>> {
    if let Some(krate) = batch::batched_crate(db, impl_id.module(db)) {
        return batch::CrateImplTraits::of(db, krate).get(impl_id).unwrap_or_else(|| {
            panic!("{impl_id:?} is a module-level impl of {krate:?} but not in its batch")
        });
    }
    impl_trait_of_impl(db, impl_id).as_ref().map(TyLoweringResult::as_lowered)
}

#[blaze_query::tracked(
    returns(ref),
    cycle_result = impl_trait_with_diagnostics_cycle_result,
    mem_tag = Infer
)]
fn impl_trait_of_impl<'db>(
    db: &'db Db,
    impl_id: ImplId,
) -> Option<TyLoweringResult<'db, StoredEarlyBinder<StoredTraitRef>>> {
    let self_ty = db.impl_self_ty(impl_id).skip_binder();
    blaze_mem::frame::current(|frame| {
        lower_impl_trait(db, impl_id, self_ty, frame).map(FrameLowered::owned)
    })
}

/// Lowers the trait `impl_id` implements, for the self type `self_ty`, which is the impl's.
pub(crate) fn lower_impl_trait<'db, 'f>(
    db: &'db Db,
    impl_id: ImplId,
    self_ty: Ty<'db>,
    frame: &'f Frame,
) -> Option<FrameLowered<'f, 'db, StoredEarlyBinder<StoredTraitRef>>> {
    let impl_data = ImplSignature::of(db, impl_id);
    let resolver = impl_id.resolver(db);
    let generics = OnceCell::new();
    let mut ctx = TyLoweringContext::new(
        db,
        &resolver,
        &impl_data.store,
        ExpressionStoreOwnerId::Signature(impl_id.into()),
        impl_id.into(),
        &generics,
        LifetimeElisionKind::AnonymousCreateParameter { report_in_path: true },
        LifetimeLoweringMode::Bound,
        frame,
    );
    ctx.impl_header = true;
    let target_trait = impl_data.target_trait?;
    let trait_ref = ctx.lower_trait_ref(target_trait, self_ty)?;
    let value = StoredEarlyBinder::bind(ctx.interner, StoredTraitRef::new(trait_ref));
    Some(FrameLowered::from_ctx(value, ctx, frame))
}

pub(crate) fn impl_trait_with_diagnostics_cycle_result<'db>(
    _db: &'db Db,
    _: blaze_query::db::Id,
    _impl_id: ImplId,
) -> Option<TyLoweringResult<'db, StoredEarlyBinder<StoredTraitRef>>> {
    None
}

impl ImplTraitId {
    #[inline]
    pub fn bounds(self, db: &Db) -> &SelfAndAssocBounds {
        let (impl_traits, idx) = match self {
            ImplTraitId::ReturnTypeImplTrait(owner, idx) => {
                (SelfAndAssocBounds::return_type_impl_traits(db, owner), idx)
            }
            ImplTraitId::TypeAliasImplTrait(owner, idx) => {
                (SelfAndAssocBounds::type_alias_impl_traits(db, owner), idx)
            }
        };
        &impl_traits[idx.into_raw().into_u32() as usize]
    }

    #[inline]
    pub fn all_bounds<'db>(self, db: &'db Db) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        self.bounds(db).all_bounds()
    }

    #[inline]
    pub fn self_bounds<'db>(
        self,
        db: &'db Db,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        self.bounds(db).self_bounds()
    }
}

impl InternedOpaqueTyId<'_> {
    #[inline]
    pub fn predicates<'db>(self, db: &'db Db) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        self.loc(db).all_bounds(db)
    }

    #[inline]
    pub fn self_predicates<'db>(
        self,
        db: &'db Db,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        self.loc(db).self_bounds(db)
    }
}

impl SelfAndAssocBounds {
    #[inline]
    pub fn return_type_impl_traits(db: &Db, def: FunctionId) -> &[SelfAndAssocBounds] {
        &fn_sig_for_fn(db, def).value.impl_traits
    }

    #[inline]
    pub(crate) fn type_alias_impl_traits(
        db: &Db,
        def: TypeAliasId,
    ) -> &[SelfAndAssocBounds] {
        &type_for_type_alias_with_diagnostics(db, def).value.impl_traits
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TyDefId {
    BuiltinType(BuiltinType),
    AdtId(AdtId),
    TypeAliasId(TypeAliasId),
}
impl_from!(BuiltinType, AdtId(StructId, EnumId, UnionId), TypeAliasId for TyDefId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum ValueTyDefId {
    FunctionId(FunctionId),
    StructId(StructId),
    UnionId(UnionId),
    EnumVariantId(EnumVariantId),
    ConstId(ConstId),
    StaticId(StaticId),
}
impl_from!(FunctionId, StructId, UnionId, EnumVariantId, ConstId, StaticId for ValueTyDefId);

impl ValueTyDefId {
    pub(crate) fn to_generic_def_id(self, db: &Db) -> GenericDefId {
        match self {
            Self::FunctionId(id) => id.into(),
            Self::StructId(id) => id.into(),
            Self::UnionId(id) => id.into(),
            Self::EnumVariantId(var) => var.lookup(db).parent.into(),
            Self::ConstId(id) => id.into(),
            Self::StaticId(id) => id.into(),
        }
    }
}

/// Build the declared type of an item. This depends on the namespace; e.g. for
/// `struct Foo(usize)`, we have two types: The type of the struct itself, and
/// the constructor function `(usize) -> Foo` which lives in the values
/// namespace.
pub(crate) fn ty_query<'db>(db: &'db Db, def: TyDefId) -> EarlyBinder<'db, Ty<'db>> {
    let interner = DbInterner::new_no_crate(db);
    match def {
        TyDefId::BuiltinType(it) => EarlyBinder::bind(interner, Ty::from_builtin_type(interner, it)),
        TyDefId::AdtId(it) => EarlyBinder::bind(interner, Ty::new_adt(
            interner,
            it,
            GenericArgs::identity_for_item(interner, it.into()),
        )),
        TyDefId::TypeAliasId(it) => db.type_for_type_alias_with_diagnostics(it).value.value.get(),
    }
}

/// Build the declared type of a function. This should not need to look at the
/// function body.
fn type_for_fn<'db>(db: &'db Db, def: FunctionId) -> EarlyBinder<'db, Ty<'db>> {
    let interner = DbInterner::new_no_crate(db);
    EarlyBinder::bind(interner, Ty::new_fn_def(
        interner,
        CallableDefId::FunctionId(def).into(),
        GenericArgs::identity_for_item(interner, def.into()),
    ))
}

pub(crate) fn type_for_const<'db>(
    db: &'db Db,
    def: ConstId,
) -> EarlyBinder<'db, Ty<'db>> {
    type_for_const_with_diagnostics(db, def).value.get()
}

/// Build the declared type of a const.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(crate) fn type_for_const_with_diagnostics<'db>(
    db: &'db Db,
    def: ConstId,
) -> TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
    blaze_mem::frame::current(|frame| {
        let resolver = def.resolver(db);
        let data = ConstSignature::of(db, def);
        let parent = def.loc(db).container;
        let generics = OnceCell::new();
        let mut ctx = TyLoweringContext::new(
            db,
            &resolver,
            &data.store,
            ExpressionStoreOwnerId::Signature(def.into()),
            def.into(),
            &generics,
            LifetimeElisionKind::AnonymousReportError,
            LifetimeLoweringMode::Bound,
            frame,
        )
        .with_grouped_placeholders(data.has_body());
        ctx.set_lifetime_elision(LifetimeElisionKind::for_const(ctx.interner, parent));
        let result = StoredEarlyBinder::bind(ctx.interner, ctx.lower_ty(data.type_ref).store());
        TyLoweringResult::from_ctx(result, ctx)
    })
}

pub(crate) fn type_for_static<'db>(
    db: &'db Db,
    def: StaticId,
) -> EarlyBinder<'db, Ty<'db>> {
    type_for_static_with_diagnostics(db, def).value.get()
}

/// Build the declared type of a static.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(crate) fn type_for_static_with_diagnostics<'db>(
    db: &'db Db,
    def: StaticId,
) -> TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
    blaze_mem::frame::current(|frame| {
        let resolver = def.resolver(db);
        let data = StaticSignature::of(db, def);
        let generics = OnceCell::new();
        let mut ctx = TyLoweringContext::new(
            db,
            &resolver,
            &data.store,
            ExpressionStoreOwnerId::Signature(def.into()),
            def.into(),
            &generics,
            LifetimeElisionKind::AnonymousReportError,
            LifetimeLoweringMode::Bound,
            frame,
        )
        .with_grouped_placeholders(data.flags.contains(StaticFlags::HAS_BODY));
        ctx.set_lifetime_elision(LifetimeElisionKind::Elided(Region::new_static(ctx.interner)));
        let result = StoredEarlyBinder::bind(ctx.interner, ctx.lower_ty(data.type_ref).store());
        TyLoweringResult::from_ctx(result, ctx)
    })
}

/// Build the type of a tuple struct constructor.
fn type_for_struct_constructor<'db>(
    db: &'db Db,
    def: StructId,
) -> Option<EarlyBinder<'db, Ty<'db>>> {
    let struct_data = StructSignature::of(db, def);
    match struct_data.shape {
        FieldsShape::Record => None,
        FieldsShape::Unit => Some(type_for_adt(db, def.into())),
        FieldsShape::Tuple => {
            let interner = DbInterner::new_no_crate(db);
            let def = CallableDefId::StructId(def);
            Some(EarlyBinder::bind(interner, Ty::new_fn_def(
                interner,
                def.into(),
                GenericArgs::identity_for_item(interner, def.into()),
            )))
        }
    }
}

/// Build the type of a tuple enum variant constructor.
fn type_for_enum_variant_constructor<'db>(
    db: &'db Db,
    def: EnumVariantId,
) -> Option<EarlyBinder<'db, Ty<'db>>> {
    let struct_data = def.fields(db);
    match struct_data.shape {
        FieldsShape::Record => None,
        FieldsShape::Unit => Some(type_for_adt(db, def.loc(db).parent.into())),
        FieldsShape::Tuple => {
            let interner = DbInterner::new_no_crate(db);
            let def = CallableDefId::EnumVariantId(def);
            Some(EarlyBinder::bind(interner, Ty::new_fn_def(
                interner,
                def.into(),
                GenericArgs::identity_for_item(interner, def.into()),
            )))
        }
    }
}

pub(crate) fn value_ty<'db>(
    db: &'db Db,
    def: ValueTyDefId,
) -> Option<EarlyBinder<'db, Ty<'db>>> {
    match def {
        ValueTyDefId::FunctionId(it) => Some(type_for_fn(db, it)),
        ValueTyDefId::StructId(it) => type_for_struct_constructor(db, it),
        ValueTyDefId::UnionId(it) => Some(type_for_adt(db, it.into())),
        ValueTyDefId::EnumVariantId(it) => type_for_enum_variant_constructor(db, it),
        ValueTyDefId::ConstId(it) => Some(type_for_const(db, it)),
        ValueTyDefId::StaticId(it) => Some(type_for_static(db, it)),
    }
}

#[blaze_query::tracked(
    returns(ref),
    cycle_result = type_for_type_alias_with_diagnostics_cycle_result,
    mem_tag = Infer
)]
pub(crate) fn type_for_type_alias_with_diagnostics<'db>(
    db: &'db Db,
    t: TypeAliasId,
) -> TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredTy>>> {
    let type_alias_data = TypeAliasSignature::of(db, t);
    let interner = DbInterner::new_no_crate(db);
    if type_alias_data.flags.contains(TypeAliasFlags::IS_EXTERN) {
        TyLoweringResult::empty(WithDefinedOpaques {
            value: StoredEarlyBinder::bind(interner, Ty::new_foreign(interner, t.into()).store()),
            impl_traits: ThinVec::new(),
        })
    } else {
        blaze_mem::frame::current(|frame| {
            let resolver = t.resolver(db);
            let generics = OnceCell::new();
            let mut ctx = TyLoweringContext::new(
                db,
                &resolver,
                &type_alias_data.store,
                ExpressionStoreOwnerId::Signature(t.into()),
                t.into(),
                &generics,
                match t.loc(db).container {
                    ItemContainerId::TraitId(_) | ItemContainerId::ImplId(_) => {
                        LifetimeElisionKind::AnonymousReportError
                    }
                    ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
                        LifetimeElisionKind::Item
                    }
                },
                LifetimeLoweringMode::Bound,
                frame,
            )
            .with_impl_trait_mode(ImplTraitLoweringMode::Opaque);
            let res = StoredEarlyBinder::bind(
                interner,
                type_alias_data
                    .ty
                    .map(|type_ref| ctx.lower_ty(type_ref))
                    .unwrap_or_else(|| Ty::new_error(interner, ErrorGuaranteed))
                    .store(),
            );
            TyLoweringResult::from_ctx(
                WithDefinedOpaques { value: res, impl_traits: ctx.take_defined_opaques() },
                ctx,
            )
        })
    }
}

pub(crate) fn type_for_type_alias_with_diagnostics_cycle_result<'db>(
    db: &'db Db,
    _: blaze_query::db::Id,
    _adt: TypeAliasId,
) -> TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredTy>>> {
    TyLoweringResult::empty(WithDefinedOpaques {
        value: StoredEarlyBinder::bind_no_rigid_aliases(
            Ty::new_error(DbInterner::new_no_crate(db), ErrorGuaranteed).store(),
        ),
        impl_traits: ThinVec::new(),
    })
}

pub(crate) fn impl_self_ty_query<'db>(
    db: &'db Db,
    impl_id: ImplId,
) -> EarlyBinder<'db, Ty<'db>> {
    impl_self_ty_with_diagnostics(db, impl_id).value.get()
}

/// The self type of an impl. A dependency crate's module-level impls are read from its batch
/// (`lower::batch::batched_crate`).
pub(crate) fn impl_self_ty_with_diagnostics<'db>(
    db: &'db Db,
    impl_id: ImplId,
) -> Lowered<'db, StoredEarlyBinder<StoredTy>> {
    if let Some(krate) = batch::batched_crate(db, impl_id.module(db)) {
        return batch::CrateImplSelfTys::of(db, krate).get(impl_id).unwrap_or_else(|| {
            panic!("{impl_id:?} is a module-level impl of {krate:?} but not in its batch")
        });
    }
    impl_self_ty_of_impl(db, impl_id).as_lowered()
}

#[blaze_query::tracked(
    returns(ref),
    cycle_result = impl_self_ty_with_diagnostics_cycle_result,
    mem_tag = Infer
)]
fn impl_self_ty_of_impl<'db>(
    db: &'db Db,
    impl_id: ImplId,
) -> TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
    blaze_mem::frame::current(|frame| lower_impl_self_ty(db, impl_id, frame).owned())
}

pub(crate) fn lower_impl_self_ty<'db, 'f>(
    db: &'db Db,
    impl_id: ImplId,
    frame: &'f Frame,
) -> FrameLowered<'f, 'db, StoredEarlyBinder<StoredTy>> {
    let resolver = impl_id.resolver(db);
    let generics = OnceCell::new();
    let impl_data = ImplSignature::of(db, impl_id);
    let mut ctx = TyLoweringContext::new(
        db,
        &resolver,
        &impl_data.store,
        ExpressionStoreOwnerId::Signature(impl_id.into()),
        impl_id.into(),
        &generics,
        LifetimeElisionKind::AnonymousCreateParameter { report_in_path: true },
        LifetimeLoweringMode::Bound,
        frame,
    );
    ctx.impl_header = true;
    let ty = ctx.lower_ty(impl_data.self_ty);
    assert!(!ty.has_escaping_bound_vars());
    FrameLowered::from_ctx(StoredEarlyBinder::bind(ctx.interner, ty.store()), ctx, frame)
}

pub(crate) fn impl_self_ty_with_diagnostics_cycle_result<'db>(
    db: &'db Db,
    _: blaze_query::db::Id,
    _impl_id: ImplId,
) -> TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
    TyLoweringResult::empty(StoredEarlyBinder::bind_no_rigid_aliases(
        Ty::new_error(DbInterner::new_no_crate(db), ErrorGuaranteed).store(),
    ))
}

pub(crate) fn const_param_ty<'db>(db: &'db Db, def: ConstParamId) -> Ty<'db> {
    const_param_types(db, def.parent()).get(def.local_id())
}

/// The types of an item's const parameters, by the parameter's `LocalTypeOrConstParamId`: one
/// column, `None` at the type parameters' places.
#[derive(Debug, Default, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct ConstParamTypes {
    tys: ThinVec<Option<StoredTy>>,
}

impl ConstParamTypes {
    #[inline]
    pub fn get(&self, local_id: LocalTypeOrConstParamId) -> Ty<'_> {
        match self.tys.get(local_id.into_raw().into_u32() as usize) {
            Some(Some(ty)) => ty.as_ref(),
            _ => default_types().types.error,
        }
    }
}

pub(crate) fn const_param_types(db: &Db, def: GenericDefId) -> &ConstParamTypes {
    &const_param_types_with_diagnostics(db, def).value
}

#[blaze_query::tracked(
    returns(ref),
    cycle_result = const_param_types_with_diagnostics_cycle_result,
    mem_tag = Infer
)]
pub(crate) fn const_param_types_with_diagnostics<'db>(
    db: &'db Db,
    def: GenericDefId,
) -> TyLoweringResult<'db, ConstParamTypes> {
    let (data, store) = GenericParams::with_store(db, def);
    let consts = || {
        data.iter_type_or_consts().filter_map(|(local_id, it)| match it {
            TypeOrConstParamData::ConstParamData(it) => {
                Some((local_id.into_raw().into_u32() as usize, it))
            }
            TypeOrConstParamData::TypeParamData(_) => None,
        })
    };
    let len = consts().map(|(index, _)| index + 1).max().unwrap_or(0);
    if len == 0 {
        return TyLoweringResult::empty(ConstParamTypes::default());
    }
    blaze_mem::frame::current(|frame| {
        let resolver = def.resolver(db);
        let generics = OnceCell::new();
        let mut ctx = TyLoweringContext::new(
            db,
            &resolver,
            store,
            ExpressionStoreOwnerId::Signature(def),
            def,
            &generics,
            LifetimeElisionKind::AnonymousReportError,
            LifetimeLoweringMode::Bound,
            frame,
        );
        ctx.forbid_params_after(0, ForbidParamsAfterReason::ConstParamTy);
        let mut result = ThinVec::with_capacity(len); // alloc: the memo's column, exact size
        result.extend(iter::repeat_n(None, len));
        for (index, param_data) in consts() {
            result[index] = Some(ctx.lower_ty(param_data.ty).store());
        }
        TyLoweringResult::from_ctx(ConstParamTypes { tys: result }, ctx)
    })
}

fn const_param_types_with_diagnostics_cycle_result<'db>(
    _db: &'db Db,
    _: blaze_query::db::Id,
    _def: GenericDefId,
) -> TyLoweringResult<'db, ConstParamTypes> {
    TyLoweringResult::empty(ConstParamTypes::default())
}

/// The types of a variant's fields, one per field in the order of its `LocalFieldId`s (every
/// field has one): a slice of a per-item query's value, or a range of a crate's column of field
/// types (`lower::batch`).
#[derive(Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct FieldTypes([FieldType]);

impl FieldTypes {
    #[inline]
    pub(crate) fn from_slice(fields: &[FieldType]) -> &FieldTypes {
        // SAFETY: `FieldTypes` is `repr(transparent)` over `[FieldType]`.
        unsafe { &*(fields as *const [FieldType] as *const FieldTypes) }
    }

    #[inline]
    pub fn as_slice(&self) -> &[FieldType] {
        &self.0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[inline]
    pub fn get(&self, field: LocalFieldId) -> Option<&FieldType> {
        self.0.get(field.into_raw().into_u32() as usize)
    }

    #[inline]
    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (LocalFieldId, &FieldType)> + ExactSizeIterator + '_ {
        self.0.iter().enumerate().map(|(index, field)| {
            (LocalFieldId::from_raw(la_arena::RawIdx::from_u32(index as u32)), field)
        })
    }

    #[inline]
    pub fn values(&self) -> impl DoubleEndedIterator<Item = &FieldType> + ExactSizeIterator + '_ {
        self.0.iter()
    }
}

impl std::ops::Index<LocalFieldId> for FieldTypes {
    type Output = FieldType;

    #[inline]
    fn index(&self, field: LocalFieldId) -> &FieldType {
        &self.0[field.into_raw().into_u32() as usize]
    }
}

impl<'a> IntoIterator for &'a FieldTypes {
    type Item = (LocalFieldId, &'a FieldType);
    type IntoIter = std::iter::Map<
        std::iter::Enumerate<std::slice::Iter<'a, FieldType>>,
        fn((usize, &'a FieldType)) -> (LocalFieldId, &'a FieldType),
    >;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        let entry: fn((usize, &'a FieldType)) -> (LocalFieldId, &'a FieldType) =
            |(index, field)| (LocalFieldId::from_raw(la_arena::RawIdx::from_u32(index as u32)), field);
        self.0.iter().enumerate().map(entry)
    }
}

pub(crate) fn field_types_query(db: &Db, variant_id: VariantId) -> &FieldTypes {
    field_types_with_diagnostics(db, variant_id).value
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct FieldType {
    pub(crate) ty: StoredEarlyBinder<StoredTy>,
    pub(crate) default: Option<StoredEarlyBinder<StoredConst>>,
}

const _: () = assert!(size_of::<FieldType>() <= 16);

impl FieldType {
    #[inline]
    pub fn ty<'db>(&self) -> EarlyBinder<'db, Ty<'db>> {
        self.ty.get()
    }

    #[inline]
    pub fn default<'db>(&self) -> Option<EarlyBinder<'db, Const<'db>>> {
        self.default.as_ref().map(|default| default.get_with(|it| it.as_ref()))
    }
}

/// The types of the fields of a struct, union or enum variant. A dependency crate's module-level
/// variants are read from its batch, every other variant from a query of its own
/// (`lower::batch::batched_crate` says which and why).
pub(crate) fn field_types_with_diagnostics<'db>(
    db: &'db Db,
    variant_id: VariantId,
) -> Lowered<'db, FieldTypes> {
    if let Some(krate) = batch::batched_crate(db, variant_id.module(db)) {
        return batch::CrateFieldTypes::of(db, krate).get(variant_id).unwrap_or_else(|| {
            panic!("{variant_id:?} is a module-level variant of {krate:?} but not in its batch")
        });
    }
    let result = field_types_of_variant(db, variant_id);
    Lowered::new(
        FieldTypes::from_slice(&result.value),
        result.diagnostics(),
        result.defined_anon_consts(),
    )
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn field_types_of_variant<'db>(
    db: &'db Db,
    variant_id: VariantId,
) -> TyLoweringResult<'db, ThinVec<FieldType>> {
    blaze_mem::frame::current(|frame| {
        lower_field_types(db, variant_id, frame).owned_with(exact_thin)
    })
}

/// Lowers the types of the fields of `variant_id`, in the order of their `LocalFieldId`s.
pub(crate) fn lower_field_types<'db, 'f>(
    db: &'db Db,
    variant_id: VariantId,
    frame: &'f Frame,
) -> FrameLowered<'f, 'db, Vec<FieldType, &'f Frame>> {
    let var_data = variant_id.fields(db);
    let fields = var_data.fields();
    if fields.is_empty() {
        return FrameLowered::empty(Vec::new_in(frame), frame);
    }

    let (resolver, generic_def): (_, GenericDefId) = match variant_id {
        VariantId::StructId(it) => (it.resolver(db), it.into()),
        VariantId::UnionId(it) => (it.resolver(db), it.into()),
        VariantId::EnumVariantId(it) => (it.resolver(db), it.lookup(db).parent.into()),
    };
    let generics = OnceCell::new();
    let mut res = Vec::with_capacity_in(fields.len(), frame);
    let mut ctx = TyLoweringContext::new(
        db,
        &resolver,
        &var_data.store,
        ExpressionStoreOwnerId::VariantFields(variant_id),
        generic_def,
        &generics,
        LifetimeElisionKind::Item,
        LifetimeLoweringMode::Bound,
        frame,
    );
    for (field_id, field_data) in var_data.fields().iter() {
        debug_assert_eq!(field_id.into_raw().into_u32() as usize, res.len());
        let ty = ctx.lower_ty(field_data.type_ref);
        let default = field_data.default_value.map(|default| ctx.lower_const(default, ty, true));
        res.push(FieldType {
            ty: StoredEarlyBinder::bind(ctx.interner, ty.store()),
            default: default.map(|default| StoredEarlyBinder::bind(ctx.interner, default.store())),
        });
    }
    FrameLowered::from_ctx(res, ctx, frame)
}

#[derive(Debug, PartialEq, Eq, Default, Hash)]
pub(crate) struct SupertraitsInfo {
    /// Every supertrait, the trait itself first and the rest depth first in the order their
    /// bounds are written, then the direct supertraits in the order they are written: one
    /// column, the direct ones from `direct_start`. The order is the source's, never an id's or
    /// a hash's, so a hash over the list is the same in every run.
    supertraits: Box<[TraitId]>,
    direct_start: u32,
    /// The associated types of the trait and its supertraits, the trait's own first.
    pub(crate) defined_assoc_types: Box<[(Name, TypeAliasId)]>,
}

const _: () = assert!(size_of::<SupertraitsInfo>() <= 40);

/// Appends `it` unless `list` has it: a trait has a few supertraits and associated types, so a
/// scan beats hashing.
fn push_new<T: PartialEq, A: Allocator>(list: &mut Vec<T, A>, it: T) {
    if !list.contains(&it) {
        list.push(it);
    }
}

impl SupertraitsInfo {
    /// Every supertrait, the trait itself included.
    #[inline]
    pub(crate) fn all_supertraits(&self) -> &[TraitId] {
        &self.supertraits[..self.direct_start as usize]
    }

    #[inline]
    pub(crate) fn direct_supertraits(&self) -> &[TraitId] {
        &self.supertraits[self.direct_start as usize..]
    }

    fn new(
        all: &[TraitId],
        direct: &[TraitId],
        defined_assoc_types: &[(Name, TypeAliasId)],
    ) -> SupertraitsInfo {
        let direct_start = u32::try_from(all.len()).expect("2^32 supertraits");
        SupertraitsInfo {
            // alloc: the memo's own lists, exact size
            supertraits: all.iter().chain(direct).copied().collect(),
            direct_start,
            defined_assoc_types: Box::from(defined_assoc_types),
        }
    }

    #[inline]
    pub(crate) fn query(db: &Db, trait_: TraitId) -> &Self {
        return supertraits_info(db, trait_);

        #[blaze_query::tracked(returns(ref), cycle_result = supertraits_info_cycle, mem_tag = Infer)]
        fn supertraits_info(db: &Db, trait_: TraitId) -> SupertraitsInfo {
            blaze_mem::frame::current(|frame| {
                let mut all_supertraits = Vec::new_in(frame);
                all_supertraits.push(trait_);
                let mut direct_supertraits = Vec::new_in(frame);
                let mut defined_assoc_types = Vec::new_in(frame);
                defined_assoc_types.extend(trait_.trait_items(db).items.iter().filter_map(
                    |(name, id)| match *id {
                        // clone: a 4-byte interned `Name`
                        AssocItemId::TypeAliasId(id) => Some((name.clone(), id)),
                        _ => None,
                    },
                ));

                let resolver = trait_.resolver(db);
                let signature = TraitSignature::of(db, trait_);
                for pred in signature.generic_params.where_predicates() {
                    let WherePredicate::TypeBound { lifetimes: _, target, bound } = pred else {
                        continue;
                    };
                    let (TypeBound::Path(
                        bounded_trait,
                        TraitBoundModifier::None
                        | TraitBoundModifier::Const
                        | TraitBoundModifier::MaybeConst,
                    )
                    | TypeBound::ForLifetime(_, bounded_trait)) = *bound
                    else {
                        continue;
                    };
                    let target = signature.store.type_ref(*target);
                    match target {
                        TypeRef::TypeParam(param)
                            if param.local_id() == GenericParams::SELF_PARAM_ID_IN_SELF => {}
                        TypeRef::Path(path) if path.is_self_type() => {}
                        _ => continue,
                    }
                    let Some(TypeNs::TraitId(bounded_trait)) = resolver
                        .resolve_path_in_type_ns_fully(db, signature.store.path(bounded_trait))
                    else {
                        continue;
                    };
                    let bounded = SupertraitsInfo::query(db, bounded_trait);
                    for &it in bounded.all_supertraits() {
                        push_new(&mut all_supertraits, it);
                    }
                    push_new(&mut direct_supertraits, bounded_trait);
                    for it in &bounded.defined_assoc_types {
                        push_new(&mut defined_assoc_types, it.clone()); // clone: a `Name` and an id
                    }
                }

                SupertraitsInfo::new(&all_supertraits, &direct_supertraits, &defined_assoc_types)
            })
        }

        fn supertraits_info_cycle(
            _db: &Db,
            _: blaze_query::db::Id,
            _trait_: TraitId,
        ) -> SupertraitsInfo {
            SupertraitsInfo::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AssocTypeShorthandResolution {
    Resolved(StoredEarlyBinder<(TypeAliasId, StoredGenericArgs)>),
    Ambiguous {
        /// If one resolution belongs to a sub-trait and one to a supertrait, this contains
        /// the sub-trait's resolution. This can be `None` if there is no trait inheritance
        /// relationship between the resolutions.
        sub_trait_resolution: Option<StoredEarlyBinder<(TypeAliasId, StoredGenericArgs)>>,
    },
    NotFound,
    Cycle,
}

/// Predicates for `param_id` of the form `P: SomeTrait`. If
/// `assoc_name` is provided, only return predicates referencing traits
/// that have an associated type of that name.
///
/// This query exists only to be used when resolving short-hand associated types
/// like `T::Item`.
///
/// See the analogous query in rustc and its comment:
/// <https://github.com/rust-lang/rust/blob/9150f844e2624eb013ec78ca08c1d416e6644026/src/librustc_typeck/astconv.rs#L46>
///
/// This is a query mostly to handle cycles somewhat gracefully; e.g. the
/// following bounds are disallowed: `T: Foo<U::Item>, U: Foo<T::Item>`, but
/// these are fine: `T: Foo<U::Item>, U: Foo<()>`.
#[tracing::instrument(skip(db), ret)]
#[blaze_query::tracked(
    returns(ref),
    cycle_result = resolve_type_param_assoc_type_shorthand_cycle_result,
    mem_tag = Infer
)]
fn resolve_type_param_assoc_type_shorthand(
    db: &Db,
    def: GenericDefId,
    param: TypeParamId,
    assoc_name: Name,
) -> AssocTypeShorthandResolution {
    blaze_mem::frame::current(|frame| {
        resolve_type_param_assoc_type_shorthand_in(db, def, param, assoc_name, frame)
    })
}

fn resolve_type_param_assoc_type_shorthand_in(
    db: &Db,
    def: GenericDefId,
    param: TypeParamId,
    assoc_name: Name,
    frame: &Frame,
) -> AssocTypeShorthandResolution {
    let generics = generics(db, def);
    let store = generics.store();
    let generics = &OnceCell::from(generics);
    let resolver = def.resolver(db);
    let mut ctx = TyLoweringContext::new(
        db,
        &resolver,
        store,
        ExpressionStoreOwnerId::Signature(def),
        def,
        generics,
        LifetimeElisionKind::AnonymousReportError,
        LifetimeLoweringMode::Bound,
        frame,
    );
    let interner = ctx.interner;
    let generics = generics.get().unwrap();
    let param_ty = Ty::new_param(interner, param, generics.type_or_const_param_idx(param.into()));

    let mut this_trait_resolution = None;
    if let GenericDefId::TraitId(containing_trait) = param.parent()
        && param.local_id() == GenericParams::SELF_PARAM_ID_IN_SELF
    {
        // Add the trait's own associated types.
        if let Some(assoc_type) =
            containing_trait.trait_items(db).associated_type_by_name(&assoc_name)
        {
            let args = GenericArgs::identity_for_item(interner, containing_trait.into());
            this_trait_resolution =
                Some(StoredEarlyBinder::bind_assoc_ty(interner, assoc_type, args.store()));
        }
    }

    let mut supertraits_resolution = None;
    for maybe_parent_generics in generics.iter_owners().rev() {
        ctx.set_owner(maybe_parent_generics);
        for pred in maybe_parent_generics.where_predicates() {
            let WherePredicate::TypeBound { lifetimes: _, target, bound } = pred else {
                continue;
            };
            let (TypeBound::Path(
                bounded_trait_path,
                TraitBoundModifier::None
                | TraitBoundModifier::Const
                | TraitBoundModifier::MaybeConst,
            )
            | TypeBound::ForLifetime(_, bounded_trait_path)) = *bound
            else {
                continue;
            };
            let Some(target) = ctx.lower_ty_only_param(*target) else { continue };
            if target != param.into() {
                continue;
            }
            let mut ctx = ctx.at_path(bounded_trait_path);
            let Some(TypeNs::TraitId(bounded_trait)) = ctx.resolve_path_in_type_ns_fully() else {
                continue;
            };
            if !SupertraitsInfo::query(db, bounded_trait)
                .defined_assoc_types
                .iter()
                .any(|(name, _)| *name == assoc_name)
            {
                continue;
            }

            let bounded_trait_ref =
                ctx.lower_trait_ref_from_resolved_path(bounded_trait, param_ty, false, Span::Dummy);
            // Now, search from the start on the *bounded* trait like if we wrote `Self::Assoc`. Eventually, we'll get
            // the correct trait ref (or a cycle).
            let lookup_on_bounded_trait = resolve_type_param_assoc_type_shorthand(
                db,
                bounded_trait.into(),
                TypeParamId::trait_self(bounded_trait),
                assoc_name,
            );
            let assoc_type_and_args = match &lookup_on_bounded_trait {
                AssocTypeShorthandResolution::Resolved(trait_ref) => trait_ref,
                AssocTypeShorthandResolution::Ambiguous {
                    sub_trait_resolution: Some(trait_ref),
                } => trait_ref,
                AssocTypeShorthandResolution::Ambiguous { sub_trait_resolution: None } => {
                    return AssocTypeShorthandResolution::Ambiguous {
                        sub_trait_resolution: this_trait_resolution,
                    };
                }
                AssocTypeShorthandResolution::NotFound => {
                    unreachable!("we checked that the trait defines this assoc type")
                }
                AssocTypeShorthandResolution::Cycle => return AssocTypeShorthandResolution::Cycle,
            };
            let (assoc_type, args) = assoc_type_and_args
                .get_with(|(assoc_type, args)| (*assoc_type, args.as_ref()))
                .skip_binder();
            let args = EarlyBinder::bind(interner, args)
                .instantiate(interner, bounded_trait_ref.args)
                .skip_norm_wip();
            let current_result = StoredEarlyBinder::bind_assoc_ty(interner, assoc_type, args.store());
            if let Some(this_trait_resolution) = &this_trait_resolution {
                if *this_trait_resolution == current_result {
                    continue;
                } else {
                    return AssocTypeShorthandResolution::Ambiguous {
                        sub_trait_resolution: Some(*this_trait_resolution),
                    };
                }
            } else if let Some(prev_resolution) = &supertraits_resolution {
                if let AssocTypeShorthandResolution::Ambiguous {
                    sub_trait_resolution: Some(prev_resolution),
                }
                | AssocTypeShorthandResolution::Resolved(prev_resolution) = prev_resolution
                    && *prev_resolution == current_result
                {
                    continue;
                } else {
                    return AssocTypeShorthandResolution::Ambiguous { sub_trait_resolution: None };
                }
            } else {
                supertraits_resolution = Some(match lookup_on_bounded_trait {
                    AssocTypeShorthandResolution::Resolved(_) => {
                        AssocTypeShorthandResolution::Resolved(current_result)
                    }
                    AssocTypeShorthandResolution::Ambiguous { .. } => {
                        AssocTypeShorthandResolution::Ambiguous {
                            sub_trait_resolution: Some(current_result),
                        }
                    }
                    AssocTypeShorthandResolution::NotFound
                    | AssocTypeShorthandResolution::Cycle => unreachable!(),
                });
            }
        }
    }

    supertraits_resolution
        .or_else(|| this_trait_resolution.map(AssocTypeShorthandResolution::Resolved))
        .unwrap_or(AssocTypeShorthandResolution::NotFound)
}

fn resolve_type_param_assoc_type_shorthand_cycle_result(
    _db: &Db,
    _: blaze_query::db::Id,
    _def: GenericDefId,
    _param: TypeParamId,
    _assoc_name: Name,
) -> AssocTypeShorthandResolution {
    AssocTypeShorthandResolution::Cycle
}

#[inline]
pub(crate) fn type_alias_bounds<'db>(
    db: &'db Db,
    type_alias: TypeAliasId,
) -> EarlyBinder<'db, &'db [Clause<'db>]> {
    type_alias_bounds_with_diagnostics(db, type_alias).value.all_bounds()
}

#[inline]
pub(crate) fn type_alias_self_bounds<'db>(
    db: &'db Db,
    type_alias: TypeAliasId,
) -> EarlyBinder<'db, &'db [Clause<'db>]> {
    type_alias_bounds_with_diagnostics(db, type_alias).value.self_bounds()
}

/// rustc's `explicit_implied_const_bounds` of an associated type or opaque (its
/// `explicit_item_bounds_with_filter(.., ConstIfConst)`): each `[const] Trait` written on the
/// item, as a `HostEffect(Maybe)` clause on the alias. A trait's where clauses on `Self::Assoc`
/// add none, as rustc's `bounds_from_parent` reads `explicit_predicates_of`, which has no
/// host effects.
pub(crate) fn implied_const_bounds<'db>(
    db: &'db Db,
    item: crate::next_solver::util::ItemWithBounds<'db>,
) -> EarlyBinder<'db, &'db [Clause<'db>]> {
    use crate::next_solver::util::ItemWithBounds;
    match item {
        ItemWithBounds::TypeAliasId(alias) => {
            type_alias_bounds_with_diagnostics(db, alias).value.const_bounds()
        }
        ItemWithBounds::InternedOpaqueTyId(opaque) => opaque.loc(db).bounds(db).const_bounds(),
    }
}

/// `pred`, lowered from `bound`, as rustc's `ConstIfConst` filter keeps it: the trait clause of a
/// `[const] Trait` bound on the item itself, as `HostEffect(Maybe)`.
fn maybe_const_bound<'db>(
    interner: DbInterner<'db>,
    bound: &TypeBound,
    pred: Clause<'db>,
    source: &GenericPredicateSource,
) -> Option<Clause<'db>> {
    if !matches!(bound, TypeBound::Path(_, TraitBoundModifier::MaybeConst))
        || !matches!(source, GenericPredicateSource::SelfOnly)
    {
        return None;
    }
    let trait_clause = pred.as_trait_clause()?;
    Some(
        trait_clause
            .map_bound(|it| it.trait_ref)
            .to_host_effect_clause(interner, rustc_type_ir::BoundConstness::Maybe),
    )
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(crate) fn type_alias_bounds_with_diagnostics<'db>(
    db: &'db Db,
    type_alias: TypeAliasId,
) -> TyLoweringResult<'db, SelfAndAssocBounds> {
    blaze_mem::frame::current(|frame| type_alias_bounds_in(db, type_alias, frame))
}

fn type_alias_bounds_in<'db>(
    db: &'db Db,
    type_alias: TypeAliasId,
    frame: &Frame,
) -> TyLoweringResult<'db, SelfAndAssocBounds> {
    let type_alias_data = TypeAliasSignature::of(db, type_alias);
    let resolver = type_alias.resolver(db);
    let generics = OnceCell::new();
    let mut ctx = TyLoweringContext::new(
        db,
        &resolver,
        &type_alias_data.store,
        ExpressionStoreOwnerId::Signature(type_alias.into()),
        type_alias.into(),
        &generics,
        LifetimeElisionKind::AnonymousReportError,
        LifetimeLoweringMode::Bound,
        frame,
    );
    let interner = ctx.interner;

    let item_args = GenericArgs::identity_for_item(interner, type_alias.into());
    let interner_ty =
        Ty::new_projection_from_args(interner, IsRigid::No, type_alias.into(), item_args);

    let mut bounds = Vec::new_in(frame);
    let mut assoc_ty_bounds = Vec::new_in(frame);
    let mut const_bounds = Vec::new_in(frame);
    for bound in &type_alias_data.bounds {
        ctx.lower_type_bound(bound, interner_ty, false).for_each(|(pred, source)| {
            const_bounds.extend(maybe_const_bound(interner, bound, pred, &source));
            match source {
                GenericPredicateSource::SelfOnly => {
                    bounds.push(pred);
                }
                GenericPredicateSource::AssocTyBound => {
                    assoc_ty_bounds.push(pred);
                }
            }
        });
    }

    if let ItemContainerId::TraitId(parent_trait) = type_alias.loc(db).container {
        let item_trait_ref = TraitRef::identity(interner, parent_trait.into());
        let parent_predicates = GenericPredicates::query_explicit(db, parent_trait.into());
        parent_predicates.iter_identity().for_each(|parent_predicate| {
            let parent_predicate = parent_predicate.skip_norm_wip();
            let Some((predicate, source)) = remap_gat_vars_and_recurse_into_nested_projections(
                interner,
                item_trait_ref,
                type_alias,
                &generics,
                parent_predicate,
                frame,
            ) else {
                return;
            };
            match source {
                GenericPredicateSource::SelfOnly => bounds.push(predicate),
                GenericPredicateSource::AssocTyBound => assoc_ty_bounds.push(predicate),
            }
        });
    }

    if let Some(sized_trait) = ctx.implicit_sizedness_trait(interner_ty, false) {
        let trait_ref = TraitRef::new_from_args(
            interner,
            sized_trait.into(),
            GenericArgs::new_from_slice(&[interner_ty.into()]),
        );
        bounds.push(trait_ref.upcast(interner));
    }

    let assoc_ty_bounds_start = bounds.len() as u32;
    bounds.extend(assoc_ty_bounds);
    let const_bounds_start = bounds.len() as u32;
    bounds.extend(const_bounds);

    TyLoweringResult::from_ctx(
        SelfAndAssocBounds {
            bounds: StoredEarlyBinder::bind(interner, Clauses::new_from_slice(&bounds).store()),
            assoc_ty_bounds_start,
            const_bounds_start,
        },
        ctx,
    )
}

/// The code below is quite involved, so let me explain.
///
/// We loop here, because we also want to collect vars for nested associated items as
/// well. For example, given a clause like `Self::A::B`, we want to add that to the
/// item bounds for `A`, so that we may use that bound in the case that `Self::A::B` is
/// rigid.
///
/// Secondly, regarding bound vars, when we see a where clause that mentions a GAT
/// like `for<'a, ...> Self::Assoc<'a, ...>: Bound<'b, ...>`, we want to turn that into
/// an item bound on the GAT, where all of the GAT args are substituted with the GAT's
/// param regions, and then keep all of the other late-bound vars in the bound around.
/// We need to "compress" the binder so that it doesn't mention any of those vars that
/// were mapped to params.
fn remap_gat_vars_and_recurse_into_nested_projections<'db>(
    interner: DbInterner<'db>,
    item_trait_ref: TraitRef<'db>,
    assoc_item_def_id: TypeAliasId,
    generics: &OnceCell<Generics<'db>>,
    clause: Clause<'db>,
    frame: &Frame,
) -> Option<(Clause<'db>, GenericPredicateSource)> {
    let mut clause_ty = match clause.kind().skip_binder() {
        ClauseKind::Trait(tr) => tr.self_ty(),
        ClauseKind::Projection(proj) => proj.projection_term.self_ty(),
        ClauseKind::TypeOutlives(outlives) => outlives.0,
        ClauseKind::HostEffect(host) => host.self_ty(),
        _ => return None,
    };

    let mut predicate_source = GenericPredicateSource::SelfOnly;
    let gat_vars = loop {
        if let TyKind::Alias(
            _,
            alias_ty @ AliasTy { kind: AliasTyKind::Projection { def_id: alias_ty_def_id }, .. },
        ) = clause_ty.kind()
        {
            if alias_ty.trait_ref(interner) == item_trait_ref
                && alias_ty_def_id.0 == assoc_item_def_id
            {
                // We have found the GAT in question...
                // Return the vars, since we may need to remap them.
                break &alias_ty.args.as_slice()[item_trait_ref.args.len()..];
            } else {
                predicate_source = GenericPredicateSource::AssocTyBound;

                clause_ty = alias_ty.self_ty();
                continue;
            }
        }

        return None;
    };

    // Special-case: No GAT vars, no mapping needed.
    if gat_vars.is_empty() {
        return Some((clause, predicate_source));
    }

    // First, check that all of the GAT args are substituted with a unique late-bound arg.
    // If we find a duplicate, then it can't be mapped to the definition's params.
    let mut mapping = frame_map(frame);
    let generics = generics
        .get_or_init(|| crate::generics::generics(interner.db, assoc_item_def_id.into()))
        .owner();
    for ((param_idx, param, _), var) in std::iter::zip(generics.iter_with_idx(), gat_vars) {
        let existing = match var.kind() {
            GenericArgKind::Lifetime(re) => {
                let RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), bv) = re.kind() else {
                    return None;
                };
                if debruijn != rustc_type_ir::INNERMOST {
                    return None;
                }
                mapping.insert(bv.var, mk_param(interner, param_idx, param))
            }
            GenericArgKind::Type(ty) => {
                let TyKind::Bound(BoundVarIndexKind::Bound(debruijn), bv) = ty.kind() else {
                    return None;
                };
                if debruijn != rustc_type_ir::INNERMOST {
                    return None;
                }
                mapping.insert(bv.var, mk_param(interner, param_idx, param))
            }
            GenericArgKind::Const(ct) => {
                let ConstKind::Bound(BoundVarIndexKind::Bound(debruijn), bv) = ct.kind() else {
                    return None;
                };
                if debruijn != rustc_type_ir::INNERMOST {
                    return None;
                }
                mapping.insert(bv.var, mk_param(interner, param_idx, param))
            }
        };

        if existing.is_some() {
            return None;
        }
    }

    // Finally, map all of the args in the GAT to the params we expect, and compress
    // the remaining late-bound vars so that they count up from var 0.
    let mut folder = MapAndCompressBoundVars {
        tcx: interner,
        binder: rustc_type_ir::INNERMOST,
        still_bound_vars: Vec::new_in(frame),
        mapping,
    };
    let pred = clause.kind().skip_binder().fold_with(&mut folder);

    Some((
        Binder::bind_with_vars(pred, BoundVarKinds::new_from_slice(&folder.still_bound_vars))
            .upcast(interner),
        predicate_source,
    ))
}

/// Given some where clause like `for<'b, 'c> <Self as Trait<'a_identity>>::Gat<'b>: Bound<'c>`,
/// the mapping will map `'b` back to the GAT's `'b_identity`. Then we need to compress the
/// remaining bound var `'c` to index 0.
///
/// This folder gives us: `for<'c> <Self as Trait<'a_identity>>::Gat<'b_identity>: Bound<'c>`,
/// which is sufficient for an item bound for `Gat`, since all of the GAT's args are identity.
struct MapAndCompressBoundVars<'db, 'f> {
    tcx: DbInterner<'db>,
    /// How deep are we? Makes sure we don't touch the vars of nested binders.
    binder: DebruijnIndex,
    /// List of bound vars that remain unsubstituted because they were not
    /// mentioned in the GAT's args.
    still_bound_vars: Vec<BoundVariableKind<'db>, &'f Frame>,
    /// Subtle invariant: If the `GenericArg` is bound, then it should be
    /// stored with the debruijn index of `INNERMOST` so it can be shifted
    /// correctly during substitution.
    mapping: FrameMap<'f, BoundVar, GenericArg<'db>>,
}

impl<'db> TypeFolder<DbInterner<'db>> for MapAndCompressBoundVars<'db, '_> {
    fn cx(&self) -> DbInterner<'db> {
        self.tcx
    }

    fn fold_binder<T>(&mut self, t: Binder<'db, T>) -> Binder<'db, T>
    where
        Binder<'db, T>: TypeSuperFoldable<DbInterner<'db>>,
    {
        self.binder.shift_in(1);
        let out = t.super_fold_with(self);
        self.binder.shift_out(1);
        out
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if !ty.has_bound_vars() {
            return ty;
        }

        if let TyKind::Bound(BoundVarIndexKind::Bound(binder), old_bound) = ty.kind()
            && self.binder == binder
        {
            let mapped = if let Some(mapped) = self.mapping.get(&old_bound.var) {
                mapped.expect_ty()
            } else {
                // If we didn't find a mapped generic, then make a new one.
                // Allocate a new var idx, and insert a new bound ty.
                let var = BoundVar::from_usize(self.still_bound_vars.len());
                self.still_bound_vars.push(BoundVariableKind::Ty(old_bound.kind));
                let mapped = Ty::new_bound(
                    self.tcx,
                    rustc_type_ir::INNERMOST,
                    BoundTy { var, kind: old_bound.kind },
                );
                self.mapping.insert(old_bound.var, mapped.into());
                mapped
            };

            shift_vars(self.tcx, mapped, self.binder.as_u32())
        } else {
            ty.super_fold_with(self)
        }
    }

    fn fold_region(&mut self, re: Region<'db>) -> Region<'db> {
        if let RegionKind::ReBound(BoundVarIndexKind::Bound(binder), old_bound) = re.kind()
            && self.binder == binder
        {
            let mapped = if let Some(mapped) = self.mapping.get(&old_bound.var) {
                mapped.expect_region()
            } else {
                let var = BoundVar::from_usize(self.still_bound_vars.len());
                self.still_bound_vars.push(BoundVariableKind::Region(old_bound.kind));
                let mapped = Region::new_bound(
                    self.tcx,
                    rustc_type_ir::INNERMOST,
                    BoundRegion { var, kind: old_bound.kind },
                );
                self.mapping.insert(old_bound.var, mapped.into());
                mapped
            };

            shift_vars(self.tcx, mapped, self.binder.as_u32())
        } else {
            re
        }
    }

    fn fold_const(&mut self, ct: Const<'db>) -> Const<'db> {
        if !ct.has_bound_vars() {
            return ct;
        }

        if let ConstKind::Bound(BoundVarIndexKind::Bound(binder), old_bound) = ct.kind()
            && self.binder == binder
        {
            let mapped = if let Some(mapped) = self.mapping.get(&old_bound.var) {
                mapped.expect_const()
            } else {
                let var = BoundVar::from_usize(self.still_bound_vars.len());
                self.still_bound_vars.push(BoundVariableKind::Const);
                let mapped =
                    Const::new_bound(self.tcx, rustc_type_ir::INNERMOST, BoundConst::new(var));
                self.mapping.insert(old_bound.var, mapped.into());
                mapped
            };

            shift_vars(self.tcx, mapped, self.binder.as_u32())
        } else {
            ct.super_fold_with(self)
        }
    }

    fn fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> P {
        if !p.has_bound_vars() { p } else { p.super_fold_with(self) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct GenericPredicates {
    // The order is the following:
    //
    // 1. If `has_trait_implied_predicate == true`, the implicit trait predicate.
    // 2. The explicit, self-only predicates for the parent.
    // 3. The bounds of the associated types of the parents, coming from `Trait<Assoc: Trait>`.
    //    Note: associated type bounds from `Self::Assoc: Trait` on traits *won't* be included
    //    here, they are in 3.
    // 4. The explicit, self-only trait predicate for the child,
    // 5. The bounds of the associated types of the child.
    pub(crate) predicates: StoredEarlyBinder<StoredClauses>,
    // Keep this ordered according to the above.
    pub(crate) has_trait_implied_predicate: bool,
    pub(crate) parent_assoc_ty_bounds_start: u32,
    pub(crate) own_predicates_start: u32,
    pub(crate) own_assoc_ty_bounds_start: u32,
}

const _: () = assert!(size_of::<GenericPredicates>() <= 24);

impl GenericPredicates {
    /// Resolve the where clause(s) of an item with generics.
    ///
    /// Diagnostics are computed only for this item's predicates, not for parents. The impls of a
    /// dependency crate's modules are read from its batch (`lower::batch::batched_crate`), which
    /// coherence and method resolution read whole.
    pub fn query_with_diagnostics<'db>(
        db: &'db Db,
        def: GenericDefId,
    ) -> Lowered<'db, GenericPredicates> {
        if let GenericDefId::ImplId(impl_id) = def
            && let Some(krate) = batch::batched_crate(db, impl_id.module(db))
        {
            return batch::CrateImplPredicates::of(db, krate).get(impl_id).unwrap_or_else(|| {
                panic!("{impl_id:?} is a module-level impl of {krate:?} but not in its batch")
            });
        }
        generic_predicates_of(db, def).as_lowered()
    }
}

#[blaze_query::tracked(returns(ref), cycle_result = generic_predicates_cycle_result, mem_tag = Infer)]
fn generic_predicates_of<'db>(
    db: &'db Db,
    def: GenericDefId,
) -> TyLoweringResult<'db, GenericPredicates> {
    blaze_mem::frame::current(|frame| generic_predicates(db, def, frame).owned())
}

/// A cycle can occur from malformed code.
fn generic_predicates_cycle_result<'db>(
    _db: &'db Db,
    _: blaze_query::db::Id,
    _def: GenericDefId,
) -> TyLoweringResult<'db, GenericPredicates> {
    TyLoweringResult::empty(GenericPredicates::from_explicit_own_predicates(
        StoredEarlyBinder::bind_no_rigid_aliases(Clauses::empty().store()),
    ))
}

impl GenericPredicates {
    #[inline]
    pub fn empty() -> &'static GenericPredicates {
        static EMPTY: OnceLock<GenericPredicates> = OnceLock::new();
        EMPTY.get_or_init(|| GenericPredicates {
            predicates: StoredEarlyBinder::bind_no_rigid_aliases(Clauses::default().store()),
            has_trait_implied_predicate: false,
            parent_assoc_ty_bounds_start: 0,
            own_predicates_start: 0,
            own_assoc_ty_bounds_start: 0,
        })
    }

    #[inline]
    pub(crate) fn from_explicit_own_predicates(
        predicates: StoredEarlyBinder<StoredClauses>,
    ) -> Self {
        let len = predicates.get().skip_binder().len() as u32;
        Self {
            predicates,
            has_trait_implied_predicate: false,
            parent_assoc_ty_bounds_start: 0,
            own_predicates_start: 0,
            own_assoc_ty_bounds_start: len,
        }
    }

    #[inline]
    pub fn query(db: &Db, def: GenericDefId) -> &GenericPredicates {
        Self::query_with_diagnostics(db, def).value
    }

    #[inline]
    pub fn query_all<'db>(
        db: &'db Db,
        def: GenericDefId,
    ) -> EarlyBinder<'db, impl Iterator<Item = Clause<'db>>> {
        Self::query(db, def).all_predicates()
    }

    #[inline]
    pub fn query_own_explicit<'db>(
        db: &'db Db,
        def: GenericDefId,
    ) -> EarlyBinder<'db, impl Iterator<Item = Clause<'db>>> {
        Self::query(db, def).own_explicit_predicates()
    }

    #[inline]
    pub fn query_explicit<'db>(
        db: &'db Db,
        def: GenericDefId,
    ) -> EarlyBinder<'db, impl Iterator<Item = Clause<'db>>> {
        Self::query(db, def).explicit_predicates()
    }

    #[inline]
    pub fn all_predicates(&self) -> EarlyBinder<'_, impl Iterator<Item = Clause<'_>>> {
        self.predicates.get().map_bound(|it| it.as_slice().iter().copied())
    }

    #[inline]
    pub fn own_explicit_predicates(&self) -> EarlyBinder<'_, impl Iterator<Item = Clause<'_>>> {
        self.predicates
            .get()
            .map_bound(|it| it.as_slice()[self.own_predicates_start as usize..].iter().copied())
    }

    #[inline]
    pub fn explicit_predicates(&self) -> EarlyBinder<'_, impl Iterator<Item = Clause<'_>>> {
        self.predicates.get().map_bound(|it| {
            it.as_slice()[usize::from(self.has_trait_implied_predicate)..].iter().copied()
        })
    }

    #[inline]
    pub fn explicit_non_assoc_types_predicates(
        &self,
    ) -> EarlyBinder<'_, impl Iterator<Item = Clause<'_>>> {
        self.predicates.get().map_bound(|it| {
            std::iter::chain(
                it.as_slice()[usize::from(self.has_trait_implied_predicate)
                    ..self.parent_assoc_ty_bounds_start as usize]
                    .iter()
                    .copied(),
                it.as_slice()
                    [self.own_predicates_start as usize..self.own_assoc_ty_bounds_start as usize]
                    .iter()
                    .copied(),
            )
        })
    }

    #[inline]
    pub fn explicit_assoc_types_predicates(
        &self,
    ) -> EarlyBinder<'_, impl Iterator<Item = Clause<'_>>> {
        self.predicates.get().map_bound(|it| {
            std::iter::chain(
                it.as_slice()[self.parent_assoc_ty_bounds_start as usize
                    ..self.own_predicates_start as usize]
                    .iter()
                    .copied(),
                it.as_slice()[self.own_assoc_ty_bounds_start as usize..].iter().copied(),
            )
        })
    }
}

pub(crate) fn param_env_from_predicates<'db>(
    interner: DbInterner<'db>,
    predicates: &'db GenericPredicates,
) -> ParamEnv<'db> {
    let clauses = rustc_type_ir::elaborate::elaborate(
        interner,
        predicates.all_predicates().iter_identity().map(Unnormalized::skip_norm_wip),
    );
    let clauses = Clauses::new_from_iter(clauses);

    // FIXME: We should normalize projections here, like rustc does.
    ParamEnv { clauses }
}

pub(crate) fn trait_environment<'db>(db: &'db Db, def: GenericDefId) -> ParamEnv<'db> {
    return ParamEnv { clauses: trait_environment_query(db, def).as_ref() };

    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    pub(crate) fn trait_environment_query(
        db: &Db,
        def: GenericDefId,
    ) -> StoredClauses {
        let module = def.module(db);
        let interner = DbInterner::new_with(db, module.krate(db));
        let predicates = GenericPredicates::query(db, def);
        param_env_from_predicates(interner, predicates).clauses.store()
    }
}

/// What a use of `def` in a const context has to prove const: its `[const]` bounds, its parent's,
/// and for a trait `Self: [const] Trait`, as `HostEffect(Maybe)` clauses (rustc's
/// `const_conditions`). Empty unless `def` is conditionally const.
pub(crate) fn const_conditions<'db>(
    db: &'db Db,
    def: GenericDefId,
) -> EarlyBinder<'db, &'db [Clause<'db>]> {
    return const_conditions_query(db, def).get().map_bound(|it| it.as_slice());

    #[blaze_query::tracked(returns(ref), mem_tag = Infer)]
    fn const_conditions_query(
        db: &Db,
        def: GenericDefId,
    ) -> StoredEarlyBinder<StoredClauses> {
        blaze_mem::frame::current(|frame| {
            let interner = DbInterner::new_no_crate(db);
            let mut conditions = Vec::new_in(frame);
            if crate::effects::is_conditionally_const(db, def) {
                let generics = generics(db, def);
                if let Some(parent) = generics.parent() {
                    conditions
                        .extend(const_conditions(db, parent.def()).skip_binder().iter().copied());
                }
                if let GenericDefId::TraitId(trait_) = def {
                    conditions
                        .push(maybe_const(interner, TraitRef::identity(interner, trait_.into())));
                }
                let store = generics.store();
                let generics = &OnceCell::from(generics);
                let resolver = def.resolver(db);
                let mut ctx = TyLoweringContext::new(
                    db,
                    &resolver,
                    store,
                    ExpressionStoreOwnerId::Signature(def),
                    def,
                    generics,
                    LifetimeElisionKind::AnonymousReportError,
                    LifetimeLoweringMode::Bound,
                    frame,
                );
                for pred in generics.get().unwrap().owner().where_predicates() {
                    let WherePredicate::TypeBound {
                        lifetimes: None,
                        target,
                        bound: TypeBound::Path(path, TraitBoundModifier::MaybeConst),
                    } = pred
                    else {
                        continue;
                    };
                    let self_ty = ctx.lower_ty(*target);
                    let mut ctx = ctx.at_path(*path);
                    let Some(TypeNs::TraitId(bounded)) = ctx.resolve_path_in_type_ns_fully()
                    else {
                        continue;
                    };
                    let trait_ref = ctx.lower_trait_ref_from_resolved_path(
                        bounded,
                        self_ty,
                        false,
                        Span::Dummy,
                    );
                    conditions.push(maybe_const(interner, trait_ref));
                }
            }
            StoredEarlyBinder::bind(interner, Clauses::new_from_slice(&conditions).store())
        })
    }

    fn maybe_const<'db>(interner: DbInterner<'db>, trait_ref: TraitRef<'db>) -> Clause<'db> {
        Binder::dummy(trait_ref)
            .to_host_effect_clause(interner, rustc_type_ir::BoundConstness::Maybe)
    }
}

/// Resolve the where clause(s) of an item with generics,
/// with a given filter
#[tracing::instrument(skip(db, frame), ret)]
pub(crate) fn generic_predicates<'db, 'f>(
    db: &'db Db,
    def: GenericDefId,
    frame: &'f Frame,
) -> FrameLowered<'f, 'db, GenericPredicates> {
    lower_generic_predicates(db, def, frame, None)
}

/// Where a predicate of [`GenericPredicates::query_all`] comes from, for rustc's "required by
/// this bound in `f`" and "the implicit `Sized` requirement on this type parameter".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PredicateSource {
    /// A trait's own `Self: Trait`, which no one writes.
    ImpliedTrait(TraitId),
    /// From `GenericParams::of(def).where_predicates()[index]`: a bound written on a parameter,
    /// in a where clause, or an `impl Trait` argument's.
    Written { def: GenericDefId, index: u32 },
    /// A type parameter's implicit sizedness: `Sized`, or `MetaSized` for one bound `?Sized` and
    /// for a trait's `Self`.
    ImplicitSized(TypeParamId),
    /// A const parameter's `ConstArgHasType`.
    ConstParamType(ConstParamId),
}

/// The source of each predicate of [`GenericPredicates::query_all`]`(db, def)`, in its order:
/// the predicates lowered again, so only for an error.
pub fn predicate_sources(db: &Db, def: GenericDefId) -> Vec<PredicateSource> {
    blaze_mem::frame::current(|frame| {
        let mut sources = Vec::new(); // alloc: error path, one per predicate
        let _ = lower_generic_predicates(db, def, frame, Some(&mut sources));
        sources
    })
}

fn lower_generic_predicates<'db, 'f>(
    db: &'db Db,
    def: GenericDefId,
    frame: &'f Frame,
    sources: Option<&mut Vec<PredicateSource>>,
) -> FrameLowered<'f, 'db, GenericPredicates> {
    // alloc: error path, only when `sources` is asked for
    let mut own_sources = sources.is_some().then(Vec::new);
    // alloc: error path, as `own_sources`
    let mut assoc_sources = sources.is_some().then(Vec::new);
    let generics = generics(db, def);
    let store = generics.store();
    let generics = &OnceCell::from(generics);
    let resolver = def.resolver(db);
    let interner = DbInterner::new_no_crate(db);
    let mut ctx = TyLoweringContext::new(
        db,
        &resolver,
        store,
        ExpressionStoreOwnerId::Signature(def),
        def,
        generics,
        LifetimeElisionKind::AnonymousReportError,
        LifetimeLoweringMode::Bound,
        frame,
    );
    let generics = generics.get().unwrap();
    let myself = generics.owner();

    let mut own_predicates = Vec::new_in(frame);
    let mut own_assoc_ty_bounds = Vec::new_in(frame);
    let own_implicit_trait_predicate = implicit_trait_predicate(interner, def);
    let generics_parent = generics.parent().map(|parent| parent.def());
    let parent = if let Some(parent) = generics.parent() {
        GenericPredicates::query_with_diagnostics(db, parent.def()).value
    } else {
        GenericPredicates::empty()
    };

    let generics = myself;

    for (index, pred) in generics.where_predicates().enumerate() {
        tracing::debug!(?pred);
        let written = PredicateSource::Written { def, index: index as u32 };
        for (pred, source) in ctx.lower_where_predicate(pred, false) {
            match source {
                GenericPredicateSource::SelfOnly => {
                    own_predicates.push(pred);
                    own_sources.iter_mut().for_each(|it| it.push(written));
                }
                GenericPredicateSource::AssocTyBound => {
                    own_assoc_ty_bounds.push(pred);
                    assoc_sources.iter_mut().for_each(|it| it.push(written));
                }
            }
        }
    }

    push_const_arg_has_type_predicates(db, &mut own_predicates, generics);
    if let Some(own_sources) = &mut own_sources {
        own_sources.extend(generics.iter_with_idx().filter_map(|(_, id, _)| match id {
            GenericParamId::ConstParamId(id) => Some(PredicateSource::ConstParamType(id)),
            _ => None,
        }));
    }

    // As rustc, a trait's `Self` gets a `MetaSized` supertrait, except an auto trait's (it could
    // not be relaxed) and `PointeeSized`'s own.
    let trait_self_gets_implicit = match def {
        GenericDefId::TraitId(id) => {
            Some(id) != ctx.lang_items.PointeeSized
                && !TraitSignature::of(db, id).flags.contains(TraitFlags::AUTO)
        }
        _ => false,
    };
    let mut add_sized_clause = |param_idx, param_id, param_data| {
        let (GenericParamId::TypeParamId(param_id), GenericParamDataRef::TypeParamData(param_data)) =
            (param_id, param_data)
        else {
            return;
        };

        let of_trait_self = param_data.provenance == TypeParamProvenance::TraitSelf;
        if of_trait_self && !trait_self_gets_implicit {
            return;
        }

        let param_ty = Ty::new_param(interner, param_id, param_idx);
        let Some(sized_trait) = ctx.implicit_sizedness_trait(param_ty, of_trait_self) else {
            return;
        };
        let trait_ref = TraitRef::new_from_args(
            interner,
            sized_trait.into(),
            GenericArgs::new_from_slice(&[param_ty.into()]),
        );
        let clause = Clause(Predicate::new(
            interner,
            Binder::dummy(rustc_type_ir::PredicateKind::Clause(rustc_type_ir::ClauseKind::Trait(
                TraitClause { trait_ref, polarity: rustc_type_ir::ClausePolarity::Positive },
            ))),
        ));
        own_predicates.push(clause);
        own_sources.iter_mut().for_each(|it| it.push(PredicateSource::ImplicitSized(param_id)));
    };
    generics.iter_with_idx().for_each(|(param_idx, param_id, param_data)| {
        add_sized_clause(param_idx, param_id, param_data);
    });

    let parent_predicates = parent.predicates.get().skip_binder().as_slice();
    let mut all = Vec::with_capacity_in(
        parent_predicates.len()
            + usize::from(own_implicit_trait_predicate.is_some())
            + own_predicates.len()
            + own_assoc_ty_bounds.len(),
        frame,
    );
    all.extend_from_slice(parent_predicates);
    all.extend(own_implicit_trait_predicate);
    all.extend_from_slice(&own_predicates);
    all.extend_from_slice(&own_assoc_ty_bounds);
    let has_trait_implied_predicate =
        parent.has_trait_implied_predicate || own_implicit_trait_predicate.is_some();
    let parent_assoc_ty_bounds_start =
        parent.own_assoc_ty_bounds_start + u32::from(own_implicit_trait_predicate.is_some());
    let own_predicates_start =
        parent_predicates.len() as u32 + u32::from(own_implicit_trait_predicate.is_some());
    let own_assoc_ty_bounds_start = own_predicates_start + own_predicates.len() as u32;

    let predicates = GenericPredicates {
        has_trait_implied_predicate,
        parent_assoc_ty_bounds_start,
        own_predicates_start,
        own_assoc_ty_bounds_start,
        predicates: StoredEarlyBinder::bind(interner, Clauses::new_from_slice(&all).store()),
    };
    if let Some(sources) = sources {
        if let Some(parent) = generics_parent {
            sources.extend(predicate_sources(db, parent));
        }
        if let GenericDefId::TraitId(trait_) = def
            && own_implicit_trait_predicate.is_some()
        {
            sources.push(PredicateSource::ImpliedTrait(trait_));
        }
        sources.extend(own_sources.into_iter().flatten());
        sources.extend(assoc_sources.into_iter().flatten());
    }
    return FrameLowered::from_ctx(predicates, ctx, frame);

    fn implicit_trait_predicate<'db>(
        interner: DbInterner<'db>,
        def: GenericDefId,
    ) -> Option<Clause<'db>> {
        // For traits, add `Self: Trait` predicate. This is
        // not part of the predicates that a user writes, but it
        // is something that one must prove in order to invoke a
        // method or project an associated type.
        //
        // In the chalk setup, this predicate is not part of the
        // "predicates" for a trait item. But it is useful in
        // rustc because if you directly (e.g.) invoke a trait
        // method like `Trait::method(...)`, you must naturally
        // prove that the trait applies to the types that were
        // used, and adding the predicate into this list ensures
        // that this is done.
        //
        // A trait alias holds of whatever its bounds hold of, so it has no such
        // predicate: one would make proving the alias wait on itself.
        if let GenericDefId::TraitId(def_id) = def
            && !TraitSignature::of(interner.db(), def_id).flags.contains(TraitFlags::ALIAS)
        {
            Some(TraitRef::identity(interner, def_id.into()).upcast(interner))
        } else {
            None
        }
    }
}

fn push_const_arg_has_type_predicates<'db>(
    db: &'db Db,
    predicates: &mut Vec<Clause<'db>, &Frame>,
    generics: &SingleGenerics<'db>,
) {
    let mut const_param_tys = None;
    let interner = DbInterner::new_no_crate(db);
    generics.iter_with_idx().for_each(|(param_index, param_id, _)| {
        let GenericParamId::ConstParamId(param_id) = param_id else { return };

        let const_param_tys =
            *const_param_tys.get_or_insert_with(|| const_param_types(db, generics.def()));
        predicates.push(Clause(
            ClauseKind::ConstArgHasType(
                Const::new_param(interner, ParamConst { id: param_id, index: param_index }),
                const_param_tys.get(param_id.local_id()),
            )
            .upcast(interner),
        ));
    });
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct GenericDefaults(ThinVec<Option<StoredEarlyBinder<StoredGenericArg>>>);

impl GenericDefaults {
    #[inline]
    pub fn as_ref(&self) -> GenericDefaultsRef<'_> {
        GenericDefaultsRef(&self.0)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GenericDefaultsRef<'db>(&'db [Option<StoredEarlyBinder<StoredGenericArg>>]);

impl<'db> GenericDefaultsRef<'db> {
    #[inline]
    pub fn get(self, idx: usize) -> Option<EarlyBinder<'db, GenericArg<'db>>> {
        Some(self.0.get(idx)?.as_ref()?.get())
    }
}

pub(crate) fn generic_defaults(db: &Db, def: GenericDefId) -> GenericDefaultsRef<'_> {
    generic_defaults_with_diagnostics(db, def).value.as_ref()
}

/// Resolve the default type params from generics.
///
/// Diagnostics are only returned for this `GenericDefId` (returned defaults include parents).
#[blaze_query::tracked(
    returns(ref),
    cycle_result = generic_defaults_with_diagnostics_cycle_result,
    mem_tag = Infer
)]
pub(crate) fn generic_defaults_with_diagnostics<'db>(
    db: &'db Db,
    def: GenericDefId,
) -> TyLoweringResult<'db, GenericDefaults> {
    let generics = generics(db, def);
    if generics.has_no_params() {
        return TyLoweringResult::empty(GenericDefaults(ThinVec::new()));
    }
    return blaze_mem::frame::current(|frame| {
        let resolver = def.resolver(db);

        let store_for_self = generics.store();
        let generics = &OnceCell::from(generics);
        let mut ctx = TyLoweringContext::new(
            db,
            &resolver,
            store_for_self,
            ExpressionStoreOwnerId::Signature(def),
            def,
            generics,
            LifetimeElisionKind::AnonymousReportError,
            LifetimeLoweringMode::Bound,
            frame,
        )
        .with_impl_trait_mode(ImplTraitLoweringMode::Disallowed);
        let generics = generics.get().unwrap();

        let parent: &[_] = match generics.parent() {
            Some(parent) => &generic_defaults_with_diagnostics(db, parent.def()).value.0,
            None => &[],
        };
        let mut own = Vec::new_in(frame);
        for (idx, _id, p) in generics.iter_self_with_idx() {
            if let Some(default) = handle_generic_param(&mut ctx, idx, p) {
                own.push((idx as usize, default));
            }
        }
        let len = own.last().map_or(parent.len(), |&(idx, _)| idx + 1);
        let mut defaults = ThinVec::with_capacity(len); // alloc: the memo's column, exact size
        defaults.extend_from_slice(parent);
        for (idx, default) in own {
            defaults.extend(iter::repeat_n(None, idx - defaults.len()));
            defaults.push(Some(default));
        }
        TyLoweringResult::from_ctx(GenericDefaults(defaults), ctx)
    });

    fn handle_generic_param<'db>(
        ctx: &mut TyLoweringContext<'db, '_>,
        idx: u32,
        p: GenericParamDataRef<'_>,
    ) -> Option<StoredEarlyBinder<StoredGenericArg>> {
        ctx.forbid_params_after(idx, ForbidParamsAfterReason::LoweringParamDefault);
        match p {
            GenericParamDataRef::TypeParamData(p) => {
                let ty = p.default.map(|ty| ctx.lower_ty(ty));
                ty.map(|ty| StoredEarlyBinder::bind(ctx.interner, GenericArg::from(ty).store()))
            }
            GenericParamDataRef::ConstParamData(p) => {
                let val = p.default.map(|c| {
                    let param_ty = ctx.lower_ty(p.ty);
                    let c = ctx.lower_const(c, param_ty, false);
                    GenericArg::from(c).store()
                });
                val.map(|val| StoredEarlyBinder::bind(ctx.interner, val))
            }
            GenericParamDataRef::LifetimeParamData(_) => None,
        }
    }
}

fn generic_defaults_with_diagnostics_cycle_result<'db>(
    _db: &'db Db,
    _: blaze_query::db::Id,
    _def: GenericDefId,
) -> TyLoweringResult<'db, GenericDefaults> {
    TyLoweringResult::empty(GenericDefaults(ThinVec::new()))
}

/// Build the signature of a callable item (function, struct or enum variant).
pub(crate) fn callable_item_signature<'db>(
    db: &'db Db,
    def: CallableDefId,
) -> EarlyBinder<'db, PolyFnSig<'db>> {
    match def {
        CallableDefId::FunctionId(f) => fn_sig_for_fn(db, f).value.value.get(),
        CallableDefId::StructId(s) => fn_sig_for_struct_constructor(db, s).get(),
        CallableDefId::EnumVariantId(e) => fn_sig_for_enum_variant_constructor(db, e).get(),
    }
}

/// The signature the solver takes for the function item `def` (`Interner::fn_sig`): rustc's,
/// each lifetime its parameters leave out a late-bound one of its own
/// ([`bind_elided_lifetimes`]), so `Location::uses_memory` of `fn uses_memory(&self) -> bool`
/// is `for<'a> FnMut(&'a Location) -> bool`. [`callable_item_signature`] keeps them errors
/// for inference, which gives each call's its own region ([`ElidedLifetimes`]).
pub(crate) fn solver_fn_sig<'db>(
    db: &'db Db,
    def: CallableDefId,
) -> EarlyBinder<'db, PolyFnSig<'db>> {
    match def {
        CallableDefId::FunctionId(f) => fn_sig_with_elided_bound(db, f).get(),
        CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
            callable_item_signature(db, def)
        }
    }
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn fn_sig_with_elided_bound(db: &Db, def: FunctionId) -> StoredEarlyBinder<StoredPolyFnSig> {
    let interner = DbInterner::new_no_crate(db);
    let sig = callable_item_signature(db, def.into()).skip_binder();
    let sig = blaze_mem::frame::current(|frame| bind_elided_lifetimes(db, def, sig, frame));
    StoredEarlyBinder::bind(interner, StoredPolyFnSig::new(sig))
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(crate) fn fn_sig_for_fn<'db>(
    db: &'db Db,
    def: FunctionId,
) -> TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredPolyFnSig>>> {
    blaze_mem::frame::current(|frame| fn_sig_for_fn_in(db, def, frame))
}

fn fn_sig_for_fn_in<'db>(
    db: &'db Db,
    def: FunctionId,
    frame: &Frame,
) -> TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredPolyFnSig>>> {
    let data = FunctionSignature::of(db, def);
    let resolver = def.resolver(db);
    let interner = DbInterner::new_no_crate(db);
    let generics = OnceCell::new();
    let mut ctx_params = TyLoweringContext::new(
        db,
        &resolver,
        &data.store,
        ExpressionStoreOwnerId::Signature(def.into()),
        def.into(),
        &generics,
        LifetimeElisionKind::for_fn_params(data),
        LifetimeLoweringMode::Bound,
        frame,
    );
    let params = data.params.iter().map(|&tr| ctx_params.lower_ty(tr));

    let elided = elided_lifetimes(db, def);
    let elides = elided.output.is_some() || elided.params.iter().any(Option::is_none);
    let mut ctx_ret = TyLoweringContext::new(
        db,
        &resolver,
        &data.store,
        ExpressionStoreOwnerId::Signature(def.into()),
        def.into(),
        &generics,
        LifetimeElisionKind::for_fn_ret(interner, elides),
        LifetimeLoweringMode::Bound,
        frame,
    )
    .with_impl_trait_mode(ImplTraitLoweringMode::Opaque)
    .with_grouped_placeholders(data.has_body());
    let ret = match data.ret_type {
        Some(ret_type) => ctx_ret.lower_ty(ret_type),
        None => Ty::new_unit(interner),
    };
    let impl_traits = ctx_ret.take_defined_opaques();

    let inputs_and_output = Tys::new_from_iter(params.chain(Some(ret)));
    ctx_params.diagnostics.extend(ctx_ret.diagnostics);
    ctx_params.defined_anon_consts.extend(ctx_ret.defined_anon_consts);

    let binder = ctx_params.bound_vars[0].1;
    // rustc's `fn_sig` of a foreign item: unsafe unless declared `safe`.
    let foreign_unsafe =
        matches!(def.lookup(db).container, ItemContainerId::ExternBlockId(_)) && !data.is_safe();
    let fn_sig_kind = FnSigKind::new(
        data.abi,
        if data.is_unsafe() || foreign_unsafe { Safety::Unsafe } else { Safety::Safe },
        data.is_varargs(),
        data.splatted,
        data.params.len(),
    )
    .expect("`FunctionSignature::splatted` indexes `params`, below `u8::MAX`");
    let result = StoredEarlyBinder::bind(
        interner,
        StoredPolyFnSig::new(Binder::bind_with_vars(
            FnSig { inputs_and_output, fn_sig_kind },
            binder,
        )),
    );
    TyLoweringResult::from_ctx(WithDefinedOpaques { value: result, impl_traits }, ctx_params)
}

/// The lifetimes the parameters of a function leave out, which rust-analyzer lowers as errors
/// where rustc makes each a late-bound lifetime of its own. An `impl Trait` the function returns
/// has a lifetime parameter for each after the function's own, as rustc's has one for each
/// lifetime it captures, and an error for its argument as the parameters have.
#[derive(PartialEq, Eq, Debug, Hash, blaze_query::Value)]
pub struct ElidedLifetimes {
    /// How many each parameter leaves out, `None` where this does not work it out: then none of
    /// them is the `impl Trait`'s.
    pub params: Box<[Option<u32>]>,
    /// What a lifetime the return type leaves out is, by the elision rules: the one of the
    /// reference to `Self` in `self`'s type, else the only one the parameters have.
    pub output: Option<ElidedOutput>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, blaze_query::Value)]
pub enum ElidedOutput {
    /// The one the parameters leave out at this index.
    Elided(u32),
    /// A lifetime a parameter writes, by its `LifetimeRefId` in the signature's store.
    Written(u32),
}

impl ElidedLifetimes {
    /// How many lifetime parameters the `impl Trait` has after the function's.
    pub fn len(&self) -> u32 {
        self.params.iter().flatten().sum()
    }

    /// The regions of the lifetimes left out, in the order of the `impl Trait`'s parameters for
    /// them, from the regions that replace the errors in each parameter's type, in order;
    /// `None` for one this cannot tell.
    pub fn regions_into<'db, E: AsRef<[Region<'db>]>>(
        &self,
        errors: &[E],
        regions: &mut impl Extend<Option<Region<'db>>>,
    ) {
        for (index, count) in self.params.iter().enumerate() {
            let Some(count) = count.map(|it| it as usize) else { continue };
            match errors.get(index).map(AsRef::as_ref).unwrap_or_default() {
                errors if errors.len() == count => {
                    regions.extend(errors.iter().map(|&it| Some(it)))
                }
                // One lifetime left out more than once in the type, `S2` of
                // `type S2<'a> = S<'a, 'a>`.
                [first, ..] if count == 1 => regions.extend([Some(*first)]),
                _ => regions.extend(iter::repeat_n(None, count)),
            }
        }
    }
}

/// The signature `sig` of `function` with each lifetime its parameters leave out, an error, a
/// late-bound one of its own, and one its return type leaves out the one the elision rules give
/// it: the fn pointer rustc reifies `function` to, `fn(&T) -> &U` as `for<'a> fn(&'a T) -> &'a U`.
/// `sig` as it is where the lifetimes left out are not known.
pub(crate) fn bind_elided_lifetimes<'db>(
    db: &'db Db,
    function: FunctionId,
    sig: PolyFnSig<'db>,
    frame: &Frame,
) -> PolyFnSig<'db> {
    let interner = DbInterner::new_no_crate(db);
    let elided = elided_lifetimes(db, function);
    let inputs = sig.skip_binder().inputs();
    if inputs.len() != elided.params.len() || !sig.skip_binder().references_error() {
        return sig;
    }
    let mut vars = Vec::new_in(frame);
    vars.extend(sig.bound_vars().iter());
    let mut of_params = Vec::new_in(frame);
    let mut tys = Vec::with_capacity_in(inputs.len() + 1, frame);
    for (&input, count) in inputs.iter().zip(&elided.params) {
        let Some(count) = *count else { return sig };
        let (start, mut seen) = (of_params.len(), 0);
        let input = rustc_type_ir::fold_regions(interner, input, |region, depth| {
            if !region.is_error() {
                return region;
            }
            seen += 1;
            let var = match (count, of_params.get(start)) {
                (1, Some(&var)) => var,
                _ => {
                    let var = BoundVar::from_usize(vars.len());
                    vars.push(BoundVariableKind::Region(BoundRegionKind::Anon));
                    of_params.push(var);
                    var
                }
            };
            Region::new_bound(interner, depth, BoundRegion { var, kind: BoundRegionKind::Anon })
        });
        if (of_params.len() - start) as u32 != count.min(seen) {
            return sig;
        }
        tys.push(input);
    }
    let output =
        rustc_type_ir::fold_regions(interner, sig.skip_binder().output(), |region, depth| {
            match elided.output {
                Some(ElidedOutput::Elided(index)) if region.is_error() => {
                    let Some(&var) = of_params.get(index as usize) else { return region };
                    Region::new_bound(
                        interner,
                        depth,
                        BoundRegion { var, kind: BoundRegionKind::Anon },
                    )
                }
                _ => region,
            }
        });
    tys.push(output);
    Binder::bind_with_vars(
        FnSig { inputs_and_output: Tys::new_from_slice(&tys), ..sig.skip_binder() },
        BoundVarKinds::new_from_slice(&vars),
    )
}

/// `ty` with the arguments an `impl Trait` `function` returns has for the lifetimes its
/// parameters leave out, errors, the `regions` [`ElidedLifetimes::regions_into`] gives.
pub fn with_elided_regions<'db>(
    db: &'db Db,
    ty: Ty<'db>,
    function: FunctionId,
    regions: &[Option<Region<'db>>],
) -> Ty<'db> {
    if regions.iter().all(Option::is_none) {
        return ty;
    }
    let interner = DbInterner::new_no_crate(db);
    let mut folder = BottomUpFolder {
        interner,
        ty_op: |ty: Ty<'db>| match ty.kind() {
            TyKind::Alias(is_rigid, alias)
                if let AliasTyKind::Opaque { def_id: opaque } = alias.kind
                    && let ImplTraitId::ReturnTypeImplTrait(owner, _) = opaque.0.loc(db)
                    && owner == function
                    && let Some(own) = alias.args.len().checked_sub(regions.len()) =>
            {
                let args = alias.args.iter().enumerate().map(|(index, arg)| {
                    match (index.checked_sub(own).and_then(|it| regions[it]), arg.kind()) {
                        (Some(region), GenericArgKind::Lifetime(it)) if it.is_error() => {
                            region.into()
                        }
                        _ => arg,
                    }
                });
                Ty::new_alias(
                    interner,
                    is_rigid,
                    AliasTy::new_from_args(interner, alias.kind, GenericArgs::new_from_iter(args)),
                )
            }
            _ => ty,
        },
        lt_op: |region| region,
        ct_op: |ct| ct,
    };
    ty.fold_with(&mut folder)
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn elided_lifetimes(db: &Db, function: FunctionId) -> ElidedLifetimes {
    blaze_mem::frame::current(|frame| elided_lifetimes_in(db, function, frame))
}

fn elided_lifetimes_in(db: &Db, function: FunctionId, frame: &Frame) -> ElidedLifetimes {
    let data = FunctionSignature::of(db, function);
    let resolver = function.resolver(db);
    let store = &data.store;
    let mut params = Vec::with_capacity(data.params.len()); // alloc: the memo's column, exact size
    let mut distinct: Vec<ElidedOutput, _> = Vec::new_in(frame);
    let mut found = Vec::new_in(frame);
    let mut self_references = Vec::new_in(frame);
    let mut lifetimes = Vec::new_in(frame);
    let mut known = true;
    let mut self_reference = None;
    let mut elided = 0;
    for (index, &param) in data.params.iter().enumerate() {
        found.clear();
        self_references.clear();
        lifetimes.clear();
        if input_lifetimes(db, &resolver, store, param, &mut found, &mut self_references).is_none()
        {
            params.push(None);
            known = false;
            continue;
        }
        lifetimes.extend(found.iter().map(|&it| match it {
            None => {
                elided += 1;
                ElidedOutput::Elided(elided - 1)
            }
            Some(lifetime) => ElidedOutput::Written(lifetime.into_raw().into_u32()),
        }));
        if index == 0
            && data.has_self_param()
            && let [only] = self_references[..]
        {
            self_reference = Some(lifetimes[only]);
        }
        let count = lifetimes.iter().filter(|it| matches!(it, ElidedOutput::Elided(_))).count();
        params.push(Some(count as u32));
        for &it in &lifetimes {
            let same = |other: &ElidedOutput| match (it, *other) {
                (ElidedOutput::Written(a), ElidedOutput::Written(b)) => {
                    let lifetime =
                        |it: u32| &store[LifetimeRefId::from_raw(la_arena::RawIdx::from_u32(it))];
                    lifetime(a) == lifetime(b)
                }
                _ => it == *other,
            };
            if !distinct.iter().any(same) {
                distinct.push(it);
            }
        }
    }
    let output = self_reference.or_else(|| match distinct[..] {
        [only] if known => Some(only),
        _ => None,
    });
    ElidedLifetimes { params: params.into_boxed_slice(), output }
}

/// The lifetimes the type `ty` of a parameter writes, `None` for one it leaves out, in the order
/// rustc's lifetime elision meets them, with in `self_references` the indices in `found` of those
/// of references to `Self`. `None` for a type this does not follow.
fn input_lifetimes(
    db: &Db,
    resolver: &Resolver<'_>,
    store: &ExpressionStore,
    ty: TypeRefId,
    found: &mut Vec<Option<LifetimeRefId>, &Frame>,
    self_references: &mut Vec<usize, &Frame>,
) -> Option<()> {
    let lifetime = |it: Option<LifetimeRefId>| match it.map(|it| (it, &store[it])) {
        None | Some((_, LifetimeRef::Placeholder)) => Some(None),
        Some((_, LifetimeRef::Error)) => None,
        Some((it, _)) => Some(Some(it)),
    };
    match store.type_ref(ty) {
        TypeRef::Reference(it) => {
            if matches!(store.type_ref(it.ty), TypeRef::Path(path) if path.is_self_type()) {
                self_references.push(found.len());
            }
            found.push(lifetime(it.lifetime)?);
            input_lifetimes(db, resolver, store, it.ty, found, self_references)
        }
        TypeRef::Tuple(tys) => tys
            .iter()
            .try_for_each(|&it| input_lifetimes(db, resolver, store, it, found, self_references)),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => {
            input_lifetimes(db, resolver, store, it, found, self_references)
        }
        TypeRef::Array(it) => input_lifetimes(db, resolver, store, it.ty, found, self_references),
        TypeRef::Never | TypeRef::TypeParam(_) => Some(()),
        TypeRef::Path(path) => path_lifetimes(db, resolver, store, path, found, self_references),
        // What the bounds write, not the object lifetime `dyn Trait` leaves out: that is its
        // default, which rustc's elision does not count.
        TypeRef::DynTrait(bounds) => {
            for bound in bounds {
                match bound {
                    TypeBound::Lifetime(it) => found.push(lifetime(Some(*it))?),
                    TypeBound::Path(path, _) => {
                        path_lifetimes(db, resolver, store, store.path(*path), found, self_references)?
                    }
                    _ => return None,
                }
            }
            Some(())
        }
        _ => None,
    }
}

/// [`input_lifetimes`] of the type or trait `path` names.
fn path_lifetimes(
    db: &Db,
    resolver: &Resolver<'_>,
    store: &ExpressionStore,
    path: Path<'_>,
    found: &mut Vec<Option<LifetimeRefId>, &Frame>,
    self_references: &mut Vec<usize, &Frame>,
) -> Option<()> {
    let lifetime = |it: Option<LifetimeRefId>| match it.map(|it| (it, &store[it])) {
        None | Some((_, LifetimeRef::Placeholder)) => Some(None),
        Some((_, LifetimeRef::Error)) => None,
        Some((it, _)) => Some(Some(it)),
    };
    if path.type_anchor().is_some() {
        return None;
    }
    let params = match resolver.resolve_path_in_type_ns_fully(db, path)? {
        TypeNs::AdtId(it) => GenericParams::of(db, it.into()).len_lifetimes(),
        TypeNs::TypeAliasId(it) => GenericParams::of(db, it.into()).len_lifetimes(),
        TypeNs::TraitId(it) => GenericParams::of(db, it.into()).len_lifetimes(),
        TypeNs::BuiltinType(_)
        | TypeNs::GenericParam(_)
        | TypeNs::SelfType(_)
        | TypeNs::AdtSelfType(_) => 0,
        _ => return None,
    };
    let segments = || path.generic_args().into_iter().flatten().flatten();
    if segments().any(|it| {
        !it.bindings.is_empty()
            || !matches!(it.parenthesized, hir_def::expr_store::path::GenericArgsParentheses::No)
    }) {
        return None;
    }
    let args = || segments().flat_map(|it| it.args.iter());
    let mut written = args()
        .filter_map(|it| match it {
            hir_def::expr_store::path::GenericArg::Lifetime(it) => Some(*it),
            _ => None,
        })
        .peekable();
    if written.peek().is_none() {
        found.extend(iter::repeat_n(None, params));
    } else {
        for it in written {
            found.push(lifetime(Some(it))?);
        }
    }
    args().try_for_each(|it| match it {
        hir_def::expr_store::path::GenericArg::Type(it) => {
            input_lifetimes(db, resolver, store, *it, found, self_references)
        }
        _ => Some(()),
    })
}

/// The lifetime parameter of the `impl Trait` `function` returns for the `index`th lifetime its
/// parameters leave out, past those the function declares.
pub(crate) fn elided_lifetime_param(
    db: &Db,
    function: FunctionId,
    index: u32,
) -> LifetimeParamId {
    let declared = GenericParams::of(db, function.into()).len_lifetimes() as u32;
    LifetimeParamId {
        parent: function.into(),
        local_id: la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(declared + index)),
    }
}

fn type_for_adt<'db>(db: &'db Db, adt: AdtId) -> EarlyBinder<'db, Ty<'db>> {
    let interner = DbInterner::new_no_crate(db);
    let args = GenericArgs::identity_for_item(interner, adt.into());
    let ty = Ty::new_adt(interner, adt, args);
    EarlyBinder::bind(interner, ty)
}

fn ctor_signature(
    db: &Db,
    variant: VariantId,
    adt: AdtId,
) -> StoredEarlyBinder<StoredPolyFnSig> {
    let field_tys = db.field_types(variant);
    let params = field_tys.iter().map(|(_, field)| field.ty().skip_binder());
    let ret = type_for_adt(db, adt).skip_binder();

    let inputs_and_output = Tys::new_from_iter(params.chain(Some(ret)));
    StoredEarlyBinder::bind_no_rigid_aliases(StoredPolyFnSig::new(Binder::dummy(FnSig {
        fn_sig_kind: FnSigKind::dummy(),
        inputs_and_output,
    })))
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn fn_sig_for_struct_constructor(
    db: &Db,
    def: StructId,
) -> StoredEarlyBinder<StoredPolyFnSig> {
    ctor_signature(db, def.into(), def.into())
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn fn_sig_for_enum_variant_constructor(
    db: &Db,
    def: EnumVariantId,
) -> StoredEarlyBinder<StoredPolyFnSig> {
    ctor_signature(db, def.into(), def.lookup(db).parent.into())
}

pub(crate) fn associated_type_by_name_including_super_traits_allow_ambiguity<'db>(
    db: &'db Db,
    trait_ref: TraitRef<'db>,
    name: Name,
) -> Option<(TypeAliasId, GenericArgs<'db>)> {
    let (AssocTypeShorthandResolution::Resolved(assoc_type)
    | AssocTypeShorthandResolution::Ambiguous { sub_trait_resolution: Some(assoc_type) }) =
        resolve_type_param_assoc_type_shorthand(
            db,
            trait_ref.def_id.0.into(),
            TypeParamId::trait_self(trait_ref.def_id.0),
            name,
        )
    else {
        return None;
    };
    let (assoc_type, trait_args) = assoc_type
        .get_with(|(assoc_type, trait_args)| (*assoc_type, trait_args.as_ref()))
        .skip_binder();
    let interner = DbInterner::new_no_crate(db);
    Some((
        assoc_type,
        EarlyBinder::bind(interner, trait_args).instantiate(interner, trait_ref.args).skip_norm_wip(),
    ))
}

#[cfg(test)]
mod tests {
    use hir_def::type_ref::TypeRefId;
    use blaze_query::db::FromId;

    use super::*;
    use crate::lower::diagnostics::{GenericArgsProhibitedReason, PathLoweringDiagnostic};

    fn trait_id(index: u32) -> TraitId {
        TraitId::from_id(blaze_query::db::Id::from_u32(index))
    }

    #[test]
    fn mutability_lowers_and_comes_back() {
        use hir_def::type_ref::Mutability as Hir;
        for (hir, ty) in [(Hir::Shared, Mutability::Not), (Hir::Mut, Mutability::Mut)] {
            assert_eq!(lower_mutability(hir), ty);
            assert_eq!(hir_mutability(ty), hir);
        }
    }

    #[test]
    fn supertraits_keep_the_order_they_are_written_in() {
        let all = vec![trait_id(9), trait_id(4), trait_id(7), trait_id(2)];
        let direct = vec![trait_id(4), trait_id(2)];
        let info = SupertraitsInfo::new(&all, &direct, &[]);
        assert_eq!(info.all_supertraits(), &all[..]);
        assert_eq!(info.direct_supertraits(), &direct[..]);
    }

    #[test]
    fn a_direct_supertrait_a_cycle_left_out_of_all_stays_out() {
        let info = SupertraitsInfo::new(&[trait_id(1)], &[trait_id(3)], &[]);
        assert_eq!(info.all_supertraits(), &[trait_id(1)]);
        assert_eq!(info.direct_supertraits(), &[trait_id(3)]);
        assert!(SupertraitsInfo::default().all_supertraits().is_empty());
    }

    #[test]
    fn supertraits_info_is_two_exact_boxes() {
        let (info, used) = blaze_alloc_count::measure(|| {
            SupertraitsInfo::new(&[trait_id(1), trait_id(2)], &[trait_id(2)], &[])
        });
        assert_eq!(used.allocations, 1, "the supertraits; no associated types");
        assert_eq!(info.all_supertraits().len() + info.direct_supertraits().len(), 3);
    }

    #[test]
    fn implicit_sizedness_follows_rustc() {
        let written = |sized, relaxed, meta_sized, pointee_sized| WrittenSizedness {
            sized,
            relaxed,
            meta_sized,
            pointee_sized,
        };
        let (s, m) = (Some(ImplicitSizedness::Sized), Some(ImplicitSizedness::MetaSized));
        for (bounds, param, trait_self) in [
            (written(false, false, false, false), s, m),
            (written(false, true, false, false), m, m),
            (written(true, false, false, false), None, None),
            (written(true, true, false, false), None, None),
            (written(false, false, true, false), None, None),
            (written(false, true, true, false), None, None),
            (written(false, false, false, true), None, None),
            (written(false, true, false, true), None, None),
        ] {
            assert_eq!(bounds.implicit(false), param, "{bounds:?} on a parameter");
            assert_eq!(bounds.implicit(true), trait_self, "{bounds:?} on a trait's `Self`");
        }
    }

    #[test]
    fn push_new_keeps_the_first_of_each() {
        let mut list = Vec::new();
        for it in [3, 1, 3, 2, 1] {
            push_new(&mut list, it);
        }
        assert_eq!(list, [3, 1, 2]);
    }

    fn diagnostic(segment: u32) -> TyLoweringDiagnostic {
        TyLoweringDiagnostic::PathDiagnostic {
            source: TypeRefId::from_raw(la_arena::RawIdx::from_u32(segment)),
            diag: PathLoweringDiagnostic::GenericArgsProhibited {
                segments: Box::new([segment]),
                reason: GenericArgsProhibitedReason::Module,
            },
        }
    }

    #[test]
    fn a_lowering_result_without_findings_allocates_nothing() {
        let result = TyLoweringResult::<'static, u32>::new(7, [], []);
        assert_eq!(result.value, 7);
        assert_eq!(result.diagnostics.capacity(), 0);
        assert_eq!(result.anon_consts.capacity(), 0);
        assert_eq!(result, TyLoweringResult::empty(7));
    }

    #[test]
    fn a_lowered_view_reads_what_the_result_holds() {
        let mut diagnostics = Vec::with_capacity(8);
        diagnostics.extend([diagnostic(1), diagnostic(2)]);
        let result = TyLoweringResult::<'static, u32>::new(3, diagnostics, []);
        assert_eq!(result.diagnostics.capacity(), 2, "a result keeps no spare capacity");
        let lowered = result.as_lowered();
        assert_eq!(*lowered.value, 3);
        assert_eq!(lowered.diagnostics(), &[diagnostic(1), diagnostic(2)]);
        assert!(lowered.defined_anon_consts().is_empty());
        let clone = result.clone();
        assert_eq!(
            (clone.value, clone.diagnostics().len(), clone.defined_anon_consts().len()),
            (3, 2, 0)
        );
    }

    #[test]
    fn a_frame_lowering_makes_one_allocation_per_list_with_something() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut lowered = FrameLowered::<'_, 'static, u32>::empty(5, frame);
            lowered.diagnostics.extend([diagnostic(1), diagnostic(2), diagnostic(3)]);
            let (result, used) = blaze_alloc_count::measure(|| lowered.owned());
            assert_eq!(used.allocations, 1, "the diagnostics, once; no anonymous constants");
            assert_eq!(result.diagnostics.capacity(), 3);
            let (_, used) = blaze_alloc_count::measure(|| {
                FrameLowered::<'_, 'static, u32>::empty(1, frame).owned()
            });
            assert_eq!(used.allocations, 0);
        });
    }
}
