//! Regions, as rustc's `renumber.rs`, `universal_regions.rs`, `type_check` and
//! `region_infer` in `rustc_borrowck`.
//!
//! Every region in the types of a body becomes a variable. The regions of the signature are
//! the universal ones: they hold on all of the body and past its end. Relating the types each
//! statement puts together says which regions outlive which; liveness says at which points a
//! region must hold; propagating both gives each region the points it holds at, which is
//! where a loan of that region is in scope. A universal region made to outlive another the
//! signature does not say it outlives is rustc's "lifetime may not live long enough".

use hir_ty::db::HirDatabase;
use base_db::Db;
use blaze_ids::radix;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, GenericDefId, ItemContainerId, Lookup as _,
    hir::Pat,
    resolver::HasResolver as _,
    signatures::FunctionSignature,
};
use crate::mem::{self, FMap, FSet, FVec, Frame, filled};
use rustc_type_ir::{
    AliasConstKind, ClauseKind, OutlivesClause, RegionKind, Variance,
    inherent::{
        GenericArgs as _, IntoKind, ParamEnv as _, SliceLike as _, Term as _, Ty as _,
    },
};

use hir_ty::{
    GenericPredicates, InferBodyId,
    mir::{
        AggregateKind, BinOp, BorrowKind, CastKind, LocalId, MirBody, MirSpan, Operand,
        OperandKind, PlaceElem, PlaceTy, ProjectionElem, Rvalue, StatementKind, TerminatorKind,
    },
    next_solver::{
        Clause, ConstKind, FreeConstAliasId, GenericArgKind, GenericArgs, InherentAssocConstId,
        OpaqueTyIdWrapper, Region, RegionExt as _, StoredRegion, StoredTy, TraitAssocConstId, Ty,
        TyKind, infer::region_constraints::Constraint,
    },
};

pub(super) use super::inference::{RegionValues, infer};
use super::{
    Ctx,
    bitset::{BitMatrix, BitSet, bits, matrix},
    cfg::{Cfg, Location},
    constraints::Constraints,
};

/// A region variable.
pub(super) type Rv = usize;

/// A `let` with a type written out, in the block it is a statement of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Annotation {
    pub(super) pat: hir_def::hir::PatId,
    pub(super) type_ref: hir_def::type_ref::TypeRefId,
    pub(super) initializer: Option<hir_def::hir::ExprId>,
    pub(super) block: hir_def::hir::ExprId,
}

/// Why one region outlives another, as rustc's `ConstraintCategory`; the most interesting
/// first, which is the order errors pick the constraint to point at by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Category {
    Return,
    TypeAnnotation,
    /// A path's written generic arguments, `foo::<&'a T>`, rustc's
    /// `TypeAnnotation(AnnotationSource::GenericArg)`: unlikely to be what relates the regions
    /// an error is about, so blamed after the other interesting constraints.
    GenericArg,
    Cast,
    CallArgument,
    Assignment,
    Usage,
    Predicate,
    Boring,
    Internal,
    /// What normalizing the signature needs, which rustc points at the signature.
    Signature,
    /// A region of a returned `impl Trait`'s hidden type outliving the lifetime chosen for
    /// it, rustc's `OpaqueType`.
    Opaque,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Outlives {
    pub(super) sup: Rv,
    pub(super) sub: Rv,
    pub(super) location: Option<Location>,
    pub(super) category: Category,
    pub(super) span: MirSpan,
    /// A closure's requirement, proven where the closure is made.
    pub(super) from_closure: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum UniversalKind {
    Static,
    /// The body itself: every universal region outlives it.
    FnBody,
    /// A lifetime parameter.
    Named,
    /// An elided lifetime of a parameter.
    AnonParam,
    /// Any other region of the signature, as an elided lifetime of the return type that
    /// elision gives no lifetime to.
    Anon,
    /// In a closure, a region of the closure's type: one of the body that creates it, which
    /// rustc's `UniversalRegions` calls external. The closure's own, the late-bound lifetimes
    /// of its signature and the borrow of its environment, are `AnonParam`.
    Creator,
    /// A lifetime of a `for<'a>` the trait solver proves something for, as rustc's
    /// placeholders: any lifetime at all, so it outlives only itself and is outlived by
    /// `'static`.
    Placeholder,
}

/// The position among a closure's creator's regions of the anonymous lifetime 0 of the impl
/// its enclosing method is in (`impl Foo<'_>`), past those of the closure's type.
pub(super) const IMPL_ANON: usize = 1 << 20;

/// The position among a closure's creator's regions of the enclosing item's late-bound
/// lifetime 0.
pub(super) const LATE_PARENT: usize = 1 << 24;

/// In a `const { .. }` block, the position of its function's lifetime parameter at `index`:
/// [`EARLY_PARENT`] + `index`.
pub(super) const EARLY_PARENT: usize = 1 << 28;

/// What a closure needs of its creator's regions that it cannot prove itself, as rustc's
/// `ClosureOutlivesRequirement`: that the region at position `longer` among the regions of
/// the closure's type outlives the one at `shorter`, or `'static` for `None`. With a `subject`,
/// that type parameter outlives it instead, a type test the creator proves.
#[derive(Clone, Copy, Debug)]
pub(super) struct Requirement<'db> {
    pub(super) subject: Option<Ty<'db>>,
    pub(super) longer: usize,
    pub(super) shorter: Option<usize>,
    pub(super) category: Category,
    pub(super) span: MirSpan,
}

/// That the type parameter `ty` must outlive `lower`, as rustc's `TypeTest`.
pub(super) struct TypeTest<'db> {
    pub(super) ty: Ty<'db>,
    pub(super) lower: Rv,
    pub(super) span: MirSpan,
    /// From a `for<'a> T: 'a` bound, which rustc reports without a code.
    pub(super) forall: bool,
    /// Needed by the signature, which rustc reports at it.
    pub(super) signature: bool,
}

pub(super) struct Regions<'f, 'db> {
    pub(super) universals: FVec<'f, (Rv, UniversalKind)>,
    /// The position in `universals` of each region variable that is universal, by the
    /// variable, `u32::MAX` for one that is not; as long as the largest universal variable.
    universal_index: FVec<'f, u32>,
    pub(super) static_rv: Rv,
    /// The universal region of each early-bound lifetime parameter, by its index.
    pub(super) early: FMap<'f, u32, Rv>,
    /// Per universal region, the universal regions it is known to outlive.
    known: BitMatrix<'f>,
    /// The type parameters known to outlive a region.
    pub(super) type_bounds: FVec<'f, (Ty<'db>, Rv)>,
    /// The type of each local, its regions variables.
    pub(super) local_tys: FVec<'f, Ty<'db>>,
    /// The region of the loan each borrow makes.
    pub(super) loan_regions: FMap<'f, Location, Rv>,
    pub(super) outlives: Constraints<'f>,
    pub(super) type_tests: FVec<'f, TypeTest<'db>>,
    /// Whether universal regions are checked against each other: not in a closure whose
    /// signature could not be split into its own regions and its creator's.
    pub(super) check_universals: bool,
    /// The universal regions of the placeholders of relating two types' `for<'a>`, whose
    /// errors are rustc's `UniverseInfo::RelateTys`: E0308, one type more general than the other.
    pub(super) related_placeholders: FSet<'f, usize>,
    /// Of those, the ones what normalizing a projection needs of constrains: rustc's
    /// `UniverseInfo::TypeOp`, "not general enough", which its type check cannot see.
    pub(super) normalized_placeholders: FSet<'f, usize>,
    /// The body is a method of a trait impl, whose signature the trait fixes: rustc's
    /// `is_bound_region_in_impl_item`, which keeps E0621 from suggesting a change to it.
    pub(super) is_impl_item: bool,
    /// The universal regions of the return type (an `async fn`'s declared one) and of `self`:
    /// rustc's `is_return_type_anon` and `is_self_anon`, for which E0621 is not given either.
    pub(super) unsuggested: FSet<'f, usize>,
    /// In a closure called by reference, the universal region of that reference.
    pub(super) closure_env: Option<usize>,
    /// In a closure, the position among the regions of its type of each of its creator's
    /// universal regions, by universal region.
    pub(super) creator_positions: FMap<'f, usize, usize>,
    /// The regions of a returned `impl Trait`'s hidden type, each of which must be one of
    /// `choices`, `'static` and the lifetimes the `impl Trait` captures: rustc's member
    /// constraints. Empty when what it captures is not known exactly.
    pub(super) members: FVec<'f, Rv>,
    pub(super) choices: FVec<'f, Rv>,
    /// The body returns an `impl Trait` that captures no lifetime, of a hidden type alias
    /// liveness keeps some region of live: what it returns must be `'static`, rustc's
    /// `OpaqueType` constraint, and a borrow of a local in it is used later, E0597.
    pub(super) returns_static_opaque: bool,
    /// A variable a function item or constructor is called through, read at the call: rust-
    /// analyzer lowers `f(x)` with `let f = Foo::<&'a u32>` as a call of the item itself.
    pub(super) callee_reads: FVec<'f, (LocalId, Location)>,
}

impl<'f, 'db> Regions<'f, 'db> {
    pub(super) fn universal(&self, rv: Rv) -> Option<usize> {
        match self.universal_index.get(rv) {
            Some(&at) if at != u32::MAX => Some(at as usize),
            _ => None,
        }
    }

    pub(super) fn knows(&self, longer: usize, shorter: usize) -> bool {
        self.known.contains(longer, shorter)
    }

    /// The regions the where clauses say the type parameter `ty` outlives.
    pub(super) fn type_bounds_of(&self, ty: Ty<'db>) -> impl Iterator<Item = Rv> + '_ {
        self.type_bounds.iter().filter(move |&&(bounded, _)| bounded == ty).map(|&(_, region)| region)
    }

    /// Whether the type parameter `ty` is known to outlive the universal region `universal`.
    pub(super) fn type_outlives_known(&self, ty: Ty<'_>, universal: usize) -> bool {
        self.type_bounds.iter().any(|&(bounded, region)| {
            bounded == ty && self.universal(region).is_some_and(|it| self.knows(it, universal))
        })
    }
}

/// The item an alias of any kind names, whose bounds rustc's `declared_bounds_from_definition`
/// reads.
pub(super) fn alias_def_id<'db>(
    kind: rustc_type_ir::AliasTyKind<hir_ty::next_solver::DbInterner<'db>>,
) -> hir_ty::next_solver::SolverDefId<'db> {
    use rustc_type_ir::AliasTyKind;
    match kind {
        AliasTyKind::Projection { def_id } => def_id.into(),
        AliasTyKind::Inherent { def_id } => def_id.into(),
        AliasTyKind::Opaque { def_id } => def_id.into(),
        AliasTyKind::Free { def_id } => def_id.into(),
    }
}

pub(super) fn rv_of(region: Region<'_>) -> Option<Rv> {
    match region.kind() {
        RegionKind::ReVar(vid) => Some(vid.as_usize()),
        _ => None,
    }
}

/// Each region in `ty`, in order, those under a binder aside.
/// The associated types `ty` names, with their arguments.
fn projections<'db>(
    ty: Ty<'db>,
    out: &mut FVec<'_, (hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>,
) {
    struct Find<'o, 'f, 'db>(&'o mut FVec<'f, (hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>);
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find<'_, '_, 'db> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Alias(_, alias) = ty.kind()
                && let rustc_type_ir::AliasTyKind::Projection { def_id } = alias.kind
            {
                self.0.push((def_id.into(), alias.args));
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Find(out));
}

/// Which arguments of the `impl Trait` `opaque`, of `len` of them, its hidden type may name:
/// those not bivariant in rustc's `variances_of` the opaque type (hir-ty's
/// `next_solver::opaque_variances`), which its liveness skips. `None` when the opaque has another
/// number of arguments, which counts every argument captured.
pub(super) fn opaque_captures<'f, 'db>(
    frame: &'f Frame,
    db: &'db Db,
    opaque: OpaqueTyIdWrapper<'db>,
    len: usize,
) -> Option<FVec<'f, bool>> {
    use rustc_type_ir::Interner as _;
    let variances = hir_ty::next_solver::DbInterner::new_no_crate(db).variances_of(opaque.into());
    if variances.len() != len {
        return None;
    }
    Some(mem::collect(frame, variances.iter().map(|variance| variance != Variance::Bivariant)))
}

/// Whether `ty` has an `impl Trait` in it.
fn has_opaque(ty: Ty<'_>) -> bool {
    struct Find;
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find {
        type Result = std::ops::ControlFlow<()>;
        fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
            if let TyKind::Alias(_, alias) = ty.kind()
                && let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind
            {
                return std::ops::ControlFlow::Break(());
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Find).is_break()
}

/// Each type in `ty`, itself first, in `ty.walk()`'s order until `f` breaks. Unlike `walk`, it
/// keeps no set of the types seen, which allocates once a type has more than eight: a type
/// met twice is visited twice.
fn each_ty<'db>(
    ty: Ty<'db>,
    f: impl FnMut(Ty<'db>) -> std::ops::ControlFlow<()>,
) -> std::ops::ControlFlow<()> {
    struct Each<F>(F);
    impl<'db, F: FnMut(Ty<'db>) -> std::ops::ControlFlow<()>>
        rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Each<F>
    {
        type Result = std::ops::ControlFlow<()>;
        fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
            (self.0)(ty)?;
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Each(f))
}

/// Whether `f` holds of a type in `ty`, as [`each_ty`] visits them.
fn any_ty<'db>(ty: Ty<'db>, mut f: impl FnMut(Ty<'db>) -> bool) -> bool {
    each_ty(ty, |it| if f(it) { std::ops::ControlFlow::Break(()) } else { std::ops::ControlFlow::Continue(()) })
        .is_break()
}

/// The parts of `actual` where `declared`, the same type but for its `impl Trait`s, has an
/// `impl Trait`: the types those hide.
fn hidden_types<'db>(declared: Ty<'db>, actual: Ty<'db>, out: &mut FVec<'_, Ty<'db>>) {
    use rustc_type_ir::inherent::GenericArg as _;
    let args = |declared: hir_ty::next_solver::GenericArgs<'db>,
                actual: hir_ty::next_solver::GenericArgs<'db>,
                out: &mut FVec<'_, Ty<'db>>| {
        if declared.len() == actual.len() {
            for (declared, actual) in declared.iter().zip(actual.iter()) {
                if let (Some(declared), Some(actual)) = (declared.as_type(), actual.as_type()) {
                    hidden_types(declared, actual, out);
                }
            }
        }
    };
    match (declared.kind(), actual.kind()) {
        (TyKind::Alias(_, alias), _)
            if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind =>
        {
            out.push(actual)
        }
        (TyKind::Adt(a, declared), TyKind::Adt(b, actual)) if a == b => args(declared, actual, out),
        (TyKind::Tuple(declared), TyKind::Tuple(actual)) if declared.len() == actual.len() => {
            for (declared, actual) in declared.iter().zip(actual.iter()) {
                hidden_types(declared, actual, out);
            }
        }
        (TyKind::Ref(_, declared, _), TyKind::Ref(_, actual, _))
        | (TyKind::RawPtr(declared, _), TyKind::RawPtr(actual, _))
        | (TyKind::Slice(declared), TyKind::Slice(actual))
        | (TyKind::Array(declared, _), TyKind::Array(actual, _)) => {
            hidden_types(declared, actual, out)
        }
        _ => (),
    }
}

/// Whether `ty` has a closure or a coroutine in it, whose signature RA does not keep
/// higher-ranked: `|line: &String|` is `fn(&'1 String)` for one `'1`, not `for<'a>`.
fn names_closure(ty: Ty<'_>) -> bool {
    any_ty(ty, |it| {
        matches!(
            it.kind(),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) | TyKind::Coroutine(..)
        )
    })
}

/// Whether `ty` names a lifetime other than in the arguments an `impl Trait` in it does not
/// capture: `impl Sized` in edition 2021 names none of its function's.
fn names_captured_lifetime<'db>(frame: &Frame, db: &Db, ty: Ty<'db>) -> bool {
    struct Find<'d> {
        db: &'d Db,
        frame: &'d Frame,
        found: bool,
    }
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find<'_> {
        type Result = ();
        fn visit_region(&mut self, region: Region<'db>) {
            self.found |= !matches!(region.kind(), RegionKind::ReBound(..));
        }
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Alias(_, alias)
                    if let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind
                        && let Some(captured) =
                            opaque_captures(self.frame, self.db, def_id, alias.args.len()) =>
                {
                    for (arg, captured) in alias.args.iter().zip(captured) {
                        if captured {
                            rustc_type_ir::TypeVisitable::visit_with(&arg, self);
                        }
                    }
                }
                _ => rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self),
            }
        }
    }
    let mut find = Find { db, frame, found: false };
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut find);
    find.found
}

/// Whether `ty` is an `impl Trait` whose bounds say nothing of its hidden type's lifetimes, as
/// `impl Cap<'a> + Cap<'b>`: no `+ 'a`, and no associated type that names one. Nested in a
/// returned one's `Item = ..`, it adds nothing the returned one's member constraints need.
fn unbounded_opaque<'db>(
    frame: &Frame,
    db: &Db,
    interner: hir_ty::next_solver::DbInterner<'db>,
    ty: Ty<'db>,
) -> bool {
    let TyKind::Alias(_, alias) = ty.kind() else { return false };
    let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind else { return false };
    rustc_type_ir::Interner::item_bounds(interner, def_id.into())
        .iter_instantiated(interner, alias.args.as_slice())
        .all(|clause| match clause.kind().skip_binder() {
            ClauseKind::TypeOutlives(_) | ClauseKind::RegionOutlives(_) => false,
            ClauseKind::Projection(projection) => match projection.term.as_type() {
                Some(ty) => {
                    !names_captured_lifetime(frame, db, ty) || unbounded_opaque(frame, db, interner, ty)
                }
                None => !rustc_type_ir::TypeVisitableExt::has_free_regions(&projection.term),
            },
            _ => true,
        })
}

/// The regions of a hidden type that rustc's `CollectMemberConstraintsVisitor` gives member
/// constraints: not those of a closure's parent arguments, nor an `impl Trait`'s arguments
/// it does not capture.
fn member_regions<'db>(
    frame: &Frame,
    db: &Db,
    ty: Ty<'db>,
    aliases: bool,
    out: &mut FVec<'_, Rv>,
) {
    struct Collect<'o, 'f, 'd> {
        db: &'d Db,
        frame: &'d Frame,
        aliases: bool,
        out: &'o mut FVec<'f, Rv>,
    }
    impl Collect<'_, '_, '_> {
        fn args<'db>(&mut self, args: GenericArgs<'db>, skip: usize) {
            for arg in args.iter().skip(skip) {
                rustc_type_ir::TypeVisitable::visit_with(&arg, self);
            }
        }
    }
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Collect<'_, '_, '_> {
        type Result = ();
        fn visit_region(&mut self, region: Region<'db>) {
            if let Some(rv) = rv_of(region) {
                self.out.push(rv);
            }
        }
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Closure(_, args) => self.args(args, args.as_closure().parent_args().len()),
                TyKind::Coroutine(_, args) => {
                    self.args(args, args.as_coroutine().parent_args().len())
                }
                TyKind::CoroutineClosure(_, args) => {
                    self.args(args, args.as_coroutine_closure().parent_args().len())
                }
                TyKind::Alias(_, _) if !self.aliases => (),
                TyKind::Alias(_, alias)
                    if let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind
                        && let Some(captured) =
                            opaque_captures(self.frame, self.db, def_id, alias.args.len()) =>
                {
                    for (arg, captured) in alias.args.iter().zip(captured) {
                        if captured {
                            rustc_type_ir::TypeVisitable::visit_with(&arg, self);
                        }
                    }
                }
                _ => rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self),
            }
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Collect { db, frame, aliases, out });
}

pub(super) fn regions_of(ty: Ty<'_>, out: &mut FVec<'_, Rv>) {
    struct Collect<'o, 'f>(&'o mut FVec<'f, Rv>);
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Collect<'_, '_> {
        type Result = ();
        fn visit_region(&mut self, region: Region<'db>) {
            if let Some(rv) = rv_of(region) {
                self.0.push(rv);
            }
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Collect(out));
}

pub(super) fn build<'a, 'db>(
    ctx: &Ctx<'a, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg<'_>,
    requirements: &FMap<'_, hir_def::hir::ExprId, (usize, FVec<'_, Requirement<'db>>)>,
) -> Regions<'a, 'db> {
    let frame = ctx.frame;
    let mut builder = Builder {
        ctx,
        body,
        regions: Regions {
            universals: Vec::new_in(frame),
            universal_index: Vec::new_in(frame),
            static_rv: 0,
            known: BitMatrix::default(),
            type_bounds: Vec::new_in(frame),
            early: mem::map(frame),
            local_tys: Vec::new_in(frame),
            loan_regions: mem::map(frame),
            outlives: Constraints::new(frame),
            type_tests: Vec::new_in(frame),
            check_universals: ctx.closure_local.is_none(),
            related_placeholders: mem::set(frame),
            normalized_placeholders: mem::set(frame),
            is_impl_item: false,
            unsuggested: mem::set(frame),
            closure_env: None,
            creator_positions: mem::map(frame),
            members: Vec::new_in(frame),
            returns_static_opaque: false,
            callee_reads: Vec::new_in(frame),
            choices: Vec::new_in(frame),
        },
        requirements,
        early: mem::map(frame),
        late: mem::map(frame),
        parent_late: mem::map(frame),
        impl_self: None,
        returns_opaque: false,
        opaque_bounds: Vec::new_in(frame),
        opaque_hidden: None,
        elided_output: None,
        error_region: false,
        inferred: Default::default(),
        location: None,
        env_aliases: Vec::new_in(frame),
        annotated: mem::map(frame),
        const_positions: 0,
        aggregate_temps: aggregate_temps(frame, body),
        synthesized: synthesized(ctx),
        placeholders: mem::map(frame),
        relate_universes: mem::set(frame),
        elided: std::cell::RefCell::new(mem::set(frame)),
        scratch: Scratch::new(frame),
    };
    builder.universal_regions();
    builder.env_aliases();
    builder.location = None;
    builder.signature_projections();
    builder.signature_higher_ranked();
    builder.annotations();
    for &block in &cfg.order {
        let data = &body.basic_blocks[block];
        for (statement, it) in data.statements.iter().enumerate() {
            builder.location = Some(Location { block, statement });
            builder.statement(&it.kind, it.span);
        }
        if let Some(terminator) = &data.terminator {
            builder.location = Some(Location { block, statement: data.statements.len() });
            builder.terminator(&terminator.kind, terminator.span);
        }
    }
    // Last, as annotations and turbofishes can name lifetimes the signature does not.
    builder.known_relations();
    if ctx.closure_local.is_none()
        && !ctx.inline_const
        && let GenericDefId::FunctionId(function) = ctx.owner.generic_def(ctx.db)
    {
        builder.error_region |= signature_lifetime_error(ctx.db, function);
    }
    // rustc's `type_check`: a body that meets an error region has its outlives constraints
    // and type tests dropped, so the E0106 is the one error, no lifetime error after it.
    if builder.error_region {
        builder.regions.outlives = Constraints::new(frame);
        builder.regions.type_tests.clear();
    }
    builder.regions
}

/// Whether the signature of `function` leaves out a lifetime with none to elide to (E0106,
/// E0637), which rustc resolves to `LifetimeRes::Error` and lowers as an error region.
fn signature_lifetime_error(db: &Db, function: hir_def::FunctionId) -> bool {
    db.fn_sig_for_fn_with_diagnostics(function)
        .diagnostics()
        .iter()
        .any(|it| matches!(it, hir_ty::TyLoweringDiagnostic::MissingLifetimes { .. }))
}

struct Builder<'c, 'a, 'db> {
    ctx: &'c Ctx<'a, 'db>,
    /// The associated types, regions erased, that a where clause or a parameter's type says
    /// outlive some region: rustc's declared bounds from the environment, known before
    /// `known_relations` says which regions.
    env_aliases: FVec<'a, Ty<'db>>,
    /// The variables a `let` writes the type of, with that type where it is theirs.
    annotated: FMap<'a, LocalId, Option<Ty<'db>>>,
    /// In a `const { .. }` block, how many regions of its type have a position.
    const_positions: usize,
    body: &'c MirBody<'db>,
    regions: Regions<'a, 'db>,
    /// What each closure checked so far needs of the body that creates it, with how many
    /// regions the closure's type has.
    requirements: &'c FMap<'c, hir_def::hir::ExprId, (usize, FVec<'c, Requirement<'db>>)>,
    /// The universal region of each lifetime parameter, by its index.
    early: FMap<'a, u32, Region<'db>>,
    /// The universal region of each of the signature's late-bound lifetimes.
    late: FMap<'a, u32, Region<'db>>,
    /// In a closure, the universal region of each late-bound lifetime of the enclosing item
    /// it names, one of its creator's.
    parent_late: FMap<'a, u32, Region<'db>>,
    /// The self type of the impl the body is in, as rust-analyzer gives it with `'_` an error,
    /// and with universal regions there: rustc's anonymous lifetime parameters of the impl.
    impl_self: Option<(Ty<'db>, Ty<'db>)>,
    /// The return type is an `impl Trait`, so the return slot has the type it hides, which
    /// implies nothing to the body.
    returns_opaque: bool,
    /// The trait and projection bounds of the returned `impl Trait`, of the type it hides,
    /// proven where the body first assigns the return slot.
    opaque_bounds: FVec<'a, Clause<'db>>,
    /// The hidden type of the returned `impl Trait`, which must be well-formed where the body
    /// defines it, as rustc's `add_item_bounds_for_hidden_type` requires (#114728).
    opaque_hidden: Option<Ty<'db>>,
    /// The region lifetime elision gives the elided lifetimes of the return type.
    elided_output: Option<Region<'db>>,
    /// The body's signature, or that of a function it calls or names, leaves out a lifetime
    /// with none to elide to (E0106): an error region in rustc, its `encountered_re_error`.
    error_region: bool,
    /// rustc's `inferred_outlives_of` the ADTs the body uses.
    inferred: hir_ty_wfcheck::Inferred<'db>,
    location: Option<Location>,
    /// rustc's `AggregateTemp`s: the temporaries a struct or variant expression, or a call of
    /// a constructor, builds its fields in, by local.
    aggregate_temps: BitSet<'a>,
    /// The expressions lowering makes that the source does not write, as the `let x = x;` of
    /// each parameter of an `async fn`: rustc's desugarings, `Boring` to blame. By expression.
    synthesized: BitSet<'a>,
    /// The universal region of each placeholder the trait solver's answers name.
    placeholders: FMap<'a, Region<'db>, Region<'db>>,
    /// The universes made to relate two types' `for<'a>`.
    relate_universes: FSet<'a, rustc_type_ir::UniverseIndex>,
    /// The region variables given the error regions rust-analyzer leaves for elided lifetimes,
    /// each a late-bound lifetime of its own in rustc: the fn pointer two fn items or closures
    /// coerce to, `if r { up } else { down }`, is `for<'a> fn(&'a T)` there.
    elided: std::cell::RefCell<FSet<'a, rustc_type_ir::RegionVid>>,
    /// The walk's per-statement and per-call buffers, cleared at each use.
    scratch: Scratch<'a, 'db>,
}

fn synthesized<'a>(ctx: &Ctx<'a, '_>) -> BitSet<'a> {
    let exprs = ctx.store.expr_ids();
    let mut out = bits(ctx.frame, exprs.len());
    let InferBodyId::DefWithBodyId(def) = ctx.owner else { return out };
    let source_map = hir_def::expr_store::Body::unmoved_source_map(ctx.db, def);
    for expr in exprs.filter(|&expr| source_map.expr_syntax(expr).is_err()) {
        out.insert(expr.into_raw().into_u32() as usize);
    }
    out
}

fn aggregate_temps<'f>(frame: &'f Frame, body: &MirBody<'_>) -> BitSet<'f> {
    let mut out = bits(frame, body.locals.len());
    let mut add = |operand: &Operand| {
        if let OperandKind::Move(place) = &operand.kind
            && place.projection.as_slice().is_empty()
        {
            out.insert(place.local.into_raw().into_u32() as usize);
        }
    };
    for (_, data) in body.basic_blocks.iter() {
        for it in &data.statements {
            if let StatementKind::Assign(
                _,
                Rvalue::Aggregate(AggregateKind::Adt(..) | AggregateKind::Union(..), operands),
            ) = &it.kind
            {
                operands.iter().for_each(&mut add);
            }
        }
        if let Some(terminator) = &data.terminator
            && let TerminatorKind::Call { func, args, .. } = &terminator.kind
            && let OperandKind::Constant { ty, .. } = &func.kind
            && let TyKind::FnDef(def, _) = ty.as_ref().kind()
            && !matches!(def.0, CallableDefId::FunctionId(_))
        {
            args.iter().for_each(&mut add);
        }
    }
    out
}

/// Where a constraint comes from.
#[derive(Clone, Copy)]
struct At {
    category: Category,
    span: MirSpan,
}

/// Frame vectors to reuse: [`Shelf::take`] one for a call, [`Shelf::give`] it back cleared, so a
/// nested call takes another. There are as many as the deepest nesting, each as long as its
/// longest use.
struct Shelf<'a, T> {
    frame: &'a Frame,
    free: FVec<'a, FVec<'a, T>>,
}

impl<'a, T> Shelf<'a, T> {
    fn new(frame: &'a Frame) -> Self {
        Shelf { frame, free: Vec::new_in(frame) }
    }

    fn take(&mut self) -> FVec<'a, T> {
        let frame = self.frame;
        self.free.pop().unwrap_or_else(|| Vec::new_in(frame))
    }

    fn give(&mut self, mut it: FVec<'a, T>) {
        it.clear();
        self.free.push(it);
    }
}

/// The walk's buffers, by what they hold.
struct Scratch<'a, 'db> {
    rvs: Shelf<'a, Rv>,
    tys: Shelf<'a, Ty<'db>>,
    regions: Shelf<'a, Region<'db>>,
    clauses: Shelf<'a, Clause<'db>>,
    pairs: Shelf<'a, (Region<'db>, Region<'db>)>,
    obligations: Shelf<'a, (Ty<'db>, Region<'db>)>,
    constraints: Shelf<'a, Constraint<'db>>,
    projections: Shelf<'a, (hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>,
}

impl<'a, 'db> Scratch<'a, 'db> {
    fn new(frame: &'a Frame) -> Self {
        Scratch {
            rvs: Shelf::new(frame),
            tys: Shelf::new(frame),
            regions: Shelf::new(frame),
            clauses: Shelf::new(frame),
            pairs: Shelf::new(frame),
            obligations: Shelf::new(frame),
            constraints: Shelf::new(frame),
            projections: Shelf::new(frame),
        }
    }
}

impl<'a, 'db> Builder<'_, 'a, 'db> {
    fn interner(&self) -> hir_ty::next_solver::DbInterner<'db> {
        self.ctx.infcx.interner
    }

    fn fresh(&self) -> Region<'db> {
        self.ctx.infcx.next_region_var(hir_ty::Span::Dummy)
    }

    /// A variable for the error region rust-analyzer leaves for an elided lifetime.
    fn fresh_elided(&self) -> Region<'db> {
        let var = self.fresh();
        if let RegionKind::ReVar(vid) = var.kind() {
            self.elided.borrow_mut().insert(vid);
        }
        var
    }

    fn renumber<T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
        &self,
        value: T,
    ) -> T {
        rustc_type_ir::fold_regions(self.interner(), value, |region, _| match region.kind() {
            RegionKind::ReBound(..) => region,
            RegionKind::ReError(_) => self.fresh_elided(),
            _ => self.fresh(),
        })
    }

    /// Whether this is the body of a closure or a `const { .. }` block, whose type's regions
    /// and enclosing item's lifetimes are its creator's.
    fn nested(&self) -> bool {
        self.ctx.closure_local.is_some() || self.ctx.inline_const
    }

    fn new_universal(&mut self, kind: UniversalKind) -> Region<'db> {
        let region = self.fresh();
        let rv = rv_of(region).expect("a fresh region is a variable");
        let index = &mut self.regions.universal_index;
        if index.len() <= rv {
            index.resize(rv + 1, u32::MAX);
        }
        index[rv] = self.regions.universals.len() as u32;
        self.regions.universals.push((rv, kind));
        region
    }

    fn placeholder(&mut self, placeholder: Region<'db>) -> Region<'db> {
        if let Some(&it) = self.placeholders.get(&placeholder) {
            return it;
        }
        let region = self.new_universal(UniversalKind::Placeholder);
        if self.relate_universes.contains(&self.ctx.infcx.universe_of_region(placeholder)) {
            self.regions.related_placeholders.insert(self.regions.universals.len() - 1);
        }
        self.placeholders.insert(placeholder, region);
        region
    }

    fn static_region(&self) -> Region<'db> {
        Region::new_var(
            self.interner(),
            rustc_type_ir::RegionVid::from_usize(self.regions.static_rv),
        )
    }

    fn early_region(&mut self, index: u32) -> Region<'db> {
        if let Some(&it) = self.early.get(&index) {
            return it;
        }
        // A closure's or `const { .. }` block's enclosing item's lifetime parameters are its
        // creator's.
        let kind = match self.nested() {
            true => UniversalKind::Creator,
            false => UniversalKind::Named,
        };
        let region = self.new_universal(kind);
        if self.ctx.inline_const {
            self.regions
                .creator_positions
                .insert(self.regions.universals.len() - 1, EARLY_PARENT + index as usize);
        }
        self.early.insert(index, region);
        self.regions.early.insert(index, rv_of(region).unwrap());
        region
    }

    /// The universal region of the enclosing item's late-bound lifetime `var`: the signature's
    /// in the item, a creator's region of a closure's, at the position [`LATE_PARENT`] + `var`,
    /// as rustc's `for_each_late_bound_region_in_recursive_scope` makes them external.
    fn parent_late_region(&mut self, var: u32) -> Region<'db> {
        if !self.nested() {
            if let Some(&it) = self.late.get(&var) {
                return it;
            }
            let region = self.new_universal(UniversalKind::Named);
            self.late.insert(var, region);
            return region;
        }
        if let Some(&it) = self.parent_late.get(&var) {
            return it;
        }
        let region = self.new_universal(UniversalKind::Creator);
        self.regions
            .creator_positions
            .insert(self.regions.universals.len() - 1, LATE_PARENT + var as usize);
        self.parent_late.insert(var, region);
        region
    }

    /// `region` of the signature as a universal region; `None` for an elided one. `depth` is
    /// how many binders in the region is, those of the signature's own late-bound lifetimes
    /// being outside it.
    fn signature_region(
        &mut self,
        region: Region<'db>,
        depth: rustc_type_ir::DebruijnIndex,
    ) -> Option<Region<'db>> {
        match region.kind() {
            RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(debruijn), bound)
                if debruijn == depth =>
            {
                let var = bound.var.as_u32();
                if let Some(&it) = self.late.get(&var) {
                    return Some(it);
                }
                let kind = match bound.kind {
                    rustc_type_ir::BoundRegionKind::Named(_) => UniversalKind::Named,
                    _ => UniversalKind::AnonParam,
                };
                let region = self.new_universal(kind);
                self.late.insert(var, region);
                Some(region)
            }
            RegionKind::ReBound(..) => Some(region),
            // In a closure, a late-bound lifetime of the enclosing item is its creator's.
            RegionKind::ReLateParam(late) if self.nested() => {
                Some(self.parent_late_region(late.kind.var.as_u32()))
            }
            // A late-bound lifetime the body names, the same as in the signature.
            RegionKind::ReLateParam(late) => {
                let var = late.kind.var.as_u32();
                if let Some(&it) = self.late.get(&var) {
                    return Some(it);
                }
                let region = self.new_universal(UniversalKind::Named);
                self.late.insert(var, region);
                Some(region)
            }
            RegionKind::ReStatic => Some(self.static_region()),
            RegionKind::ReEarlyParam(early) => Some(self.early_region(early.index)),
            RegionKind::ReVar(_) => Some(region),
            _ => None,
        }
    }

    fn universal_regions(&mut self) {
        let static_region = self.new_universal(UniversalKind::Static);
        self.regions.static_rv = rv_of(static_region).unwrap();
        self.new_universal(UniversalKind::FnBody);
        let body = self.body;
        let mut local_tys: FVec<'_, Option<Ty<'db>>> = filled(self.ctx.frame, body.locals.len(), None);
        let return_local = super::return_slot();
        let owner = body.owner;
        if self.ctx.closure_local.is_some() {
            self.regions.check_universals = self.closure_signature(&mut local_tys);
        }
        if self.ctx.closure_local.is_some() && !self.regions.check_universals {
            for &local in body.param_locals.iter().chain([&return_local]) {
                let ty = body.locals[local].ty.as_ref();
                let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, _| match region
                    .kind()
                {
                    RegionKind::ReBound(..) => region,
                    _ => self.new_universal(UniversalKind::AnonParam),
                });
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
        } else if self.ctx.closure_local.is_none() {
            // The declared signature, as the parameters' local types have their lifetimes
            // erased.
            let mut self_free: FVec<'_, Option<Ty<'db>>> = Vec::new_in(self.ctx.frame);
            let mut elided_input = None;
            let signature = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) => {
                    let sig = self
                        .ctx
                        .db
                        .callable_item_signature(CallableDefId::FunctionId(function))
                        .instantiate_identity()
                        .skip_norm_wip()
                        .skip_binder();
                    let mut inputs: FVec<'_, Ty<'db>> = mem::collect(self.ctx.frame, sig.inputs().iter().copied());
                    let mut output = sig.output();
                    // rustc's writeback taints a body whose signature, normalized, has an
                    // error in it, and MIR building then makes no body to check: a projection
                    // of an item an impl with a parameter it does not constrain (E0207)
                    // defines, `-> Self::Output<'a>` in that impl.
                    if normalizes_to_error(self.ctx, inputs.iter().copied().chain([output])) {
                        self.ctx.tainted.set(true);
                    }
                    let written = hir_def::signatures::FunctionSignature::of(self.ctx.db, function);
                    // A C variadic's `ap: ...` is a `VaList` the signature does not list.
                    if written.is_varargs()
                        && inputs.len() + 1 == body.param_locals.len()
                        && let Some(&last) = body.param_locals.last()
                    {
                        inputs.push(body.locals[last].ty.as_ref());
                    }
                    if written.params.len() == inputs.len()
                        && let Some(impl_self) = self.impl_self_of(function)
                    {
                        // Only where `Self` is written: an elided `&str` in `impl Trait for &str`
                        // is the same type to rust-analyzer, but a lifetime of its own.
                        let store = &written.store;
                        let mut folder = Replace::new(self.interner(), impl_self);
                        for (input, &param) in inputs.iter_mut().zip(&written.params) {
                            if writes_self(store, param) {
                                *input =
                                    rustc_type_ir::TypeFoldable::fold_with(*input, &mut folder);
                            }
                        }
                        if written.ret_type.is_some_and(|it| writes_self(store, it)) {
                            output = rustc_type_ir::TypeFoldable::fold_with(output, &mut folder);
                        }
                    }
                    // Each elided input lifetime is its own region before `&dyn Trait` copies
                    // it into the object's default, so that the two are the same one. A
                    // parameter that elides one lifetime has it once however often its type
                    // does (`S2` of `type S2<'a> = S<'a, 'a>`).
                    let resolver = {
                        use hir_def::resolver::HasResolver;
                        function.resolver(self.ctx.db)
                    };
                    let aligned = written.params.len() == inputs.len();
                    let mut errors =
                        filled(self.ctx.frame, inputs.len(), Vec::new_in(self.ctx.frame));
                    for (index, input) in inputs.iter_mut().enumerate() {
                        let single = aligned
                            && written.params.get(index).is_some_and(|&param| {
                                elided_lifetimes(self.ctx.db, &resolver, &written.store, param)
                                    == Some(1)
                            });
                        // Which lifetime left out each error is, where one is there more than
                        // once: `type B<'f, 's> = Box<dyn FnOnce(&dyn T<'s>) + 'f>` has `'s`
                        // in the trait's arguments and in its `Output`'s.
                        let groups = match (single, aligned) {
                            (false, true) => {
                                self.elided_groups(function, written.params[index], *input)
                            }
                            _ => None,
                        };
                        let mut only = None;
                        let mut of_group: FVec<'_, Option<Region<'db>>> = Vec::new_in(self.ctx.frame);
                        let mut position = 0;
                        let errors = &mut errors[index];
                        *input =
                            rustc_type_ir::fold_regions(self.interner(), *input, |region, _| {
                                let region = match region.kind() {
                                    RegionKind::ReError(_) if single => {
                                        *only.get_or_insert_with(|| {
                                            self.new_universal(UniversalKind::AnonParam)
                                        })
                                    }
                                    RegionKind::ReError(_) if let Some(groups) = &groups => {
                                        let group = groups[position];
                                        position += 1;
                                        if of_group.len() <= group {
                                            of_group.resize(group + 1, None);
                                        }
                                        *of_group[group].get_or_insert_with(|| {
                                            self.new_universal(UniversalKind::AnonParam)
                                        })
                                    }
                                    RegionKind::ReError(_) => {
                                        self.new_universal(UniversalKind::AnonParam)
                                    }
                                    _ => return region,
                                };
                                errors.push(region);
                                region
                            });
                    }
                    // What an `impl Trait` it returns has for the lifetimes its parameters
                    // leave out.
                    if aligned {
                        let elided = hir_ty::elided_lifetimes(self.ctx.db, function);
                        let mut regions = Vec::new_in(self.ctx.frame);
                        elided.regions_into(&errors, &mut regions);
                        if let Some(hir_ty::ElidedOutput::Elided(index)) = elided.output {
                            elided_input = regions.get(index as usize).copied().flatten();
                        }
                        output =
                            hir_ty::with_elided_regions(self.ctx.db, output, function, &regions);
                    }
                    let mut defaults = ObjectDefaults {
                        db: self.ctx.db,
                        interner: self.interner(),
                        keep_static: writes_static_object(&written.store),
                        erase: false,
                    };
                    for input in &mut inputs {
                        *input = rustc_type_ir::TypeFoldable::fold_with(*input, &mut defaults);
                    }
                    output = rustc_type_ir::TypeFoldable::fold_with(output, &mut defaults);
                    // What lifetime elision counts of a parameter that writes `Self`: rustc
                    // looks only at the lifetimes a parameter's type writes or elides, so
                    // the impl's (`self` of `impl Foo<'_>`, `other: Self`) are none of them.
                    if aligned
                        && let ItemContainerId::ImplId(impl_) =
                            function.lookup(self.ctx.db).container
                    {
                        let impl_self = match self.impl_self {
                            Some((_, named)) => named,
                            None => self
                                .ctx
                                .db
                                .impl_self_ty(impl_)
                                .instantiate_identity()
                                .skip_norm_wip(),
                        };
                        let impl_self =
                            rustc_type_ir::TypeFoldable::fold_with(impl_self, &mut defaults);
                        let unit = Ty::new_unit(self.interner());
                        let mut folder = Replace::new(self.interner(), (impl_self, unit));
                        self_free = mem::collect(
                            self.ctx.frame,
                            inputs.iter().zip(&written.params).map(|(&input, &param)| {
                                writes_self(&written.store, param).then(|| {
                                    rustc_type_ir::TypeFoldable::fold_with(input, &mut folder)
                                })
                            }),
                        );
                    }
                    (inputs.len() == body.param_locals.len()).then_some((inputs, output))
                }
                _ => None,
            };
            let mut inputs = Vec::new_in(self.ctx.frame);
            for (index, &local) in body.param_locals.iter().enumerate() {
                let ty = match &signature {
                    Some((tys, _)) => tys[index],
                    None => body.locals[local].ty.as_ref(),
                };
                let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                    // Inference variables are ours only in a signature, from its impl's self type.
                    let region = match region.kind() {
                        RegionKind::ReVar(_) if signature.is_none() => None,
                        _ => self.signature_region(region, depth),
                    }
                    .unwrap_or_else(|| self.new_universal(UniversalKind::AnonParam));
                    if self_free.get(index).is_none_or(Option::is_none)
                        && !matches!(region.kind(), RegionKind::ReBound(..))
                    {
                        inputs.push(region);
                    }
                    region
                });
                if let Some(&Some(free)) = self_free.get(index) {
                    rustc_type_ir::fold_regions(self.interner(), free, |region, depth| {
                        if let Some(region) = self.signature_region(region, depth)
                            && !matches!(region.kind(), RegionKind::ReBound(..))
                        {
                            inputs.push(region);
                        }
                        region
                    });
                }
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
            // The lifetime elision gives the elided lifetimes of the return type.
            let self_region = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function))
                    if FunctionSignature::of(self.ctx.db, function).has_self_param() =>
                {
                    body.param_locals.first().and_then(|&it| {
                        let ty = local_tys[it.into_raw().into_u32() as usize]?;
                        self.self_reference(function, ty).or(match ty.kind() {
                            TyKind::Ref(region, ..) => Some(region),
                            _ => None,
                        })
                    })
                }
                _ => None,
            };
            // The lifetime elision counts no trait object's default lifetime, `'static` in
            // `&dyn Any` or the query database's `&dyn Database`, only what a parameter writes or
            // elides.
            if let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = owner
                && signature.is_some()
                && !writes_object_lifetime(&FunctionSignature::of(self.ctx.db, function).store)
            {
                for &local in &body.param_locals {
                    let Some(ty) = local_tys[local.into_raw().into_u32() as usize] else {
                        continue;
                    };
                    // Each object written, as `inputs` has each region written.
                    _ = each_ty(ty, |ty| {
                        if let TyKind::Dynamic(_, object) = ty.kind()
                            && let Some(at) = inputs.iter().position(|&it| it == object)
                        {
                            inputs.remove(at);
                        }
                        std::ops::ControlFlow::Continue(())
                    });
                }
            }
            inputs.dedup();
            // Where the written signature has the one lifetime the parameters leave out, that
            // is it: rustc counts the lifetimes the parameters' types write or elide, not those
            // of a type alias's expansion (`module: ModuleId` of
            // `type ModuleId = ModuleIdLt<'static>`).
            let elided = match (inputs.as_slice(), self_region) {
                (_, Some(region)) => Some(region),
                _ if signature.is_some() && elided_input.is_some() => elided_input,
                ([only], None) => Some(*only),
                _ => None,
            };
            self.elided_output = elided;
            // rustc's E0106, which rust-analyzer does not report: a return type that elides a
            // lifetime where no parameter has one, `fn f<R>(r: R) -> Box<dyn Foo + '_>`.
            if inputs.is_empty()
                && self_region.is_none()
                && self.ctx.closure_local.is_none()
                && let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = owner
                && let written = FunctionSignature::of(self.ctx.db, function)
                && written.ret_type.is_some_and(|it| writes_elided(&written.store, it))
            {
                self.ctx.tainted.set(true);
            }
            let is_fn = matches!(owner, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(_)));
            // A returned `impl Trait` is checked as the type it hides; its lifetimes are
            // `'static` when nothing it could capture is, and left free otherwise.
            let opaque = signature.as_ref().is_some_and(|(_, output)| has_opaque(*output));
            self.returns_opaque = opaque;
            // A return type with `_` in it is rustc's E0121, which takes the type the body
            // infers (`recover_infer_ret_ty`), and an error where it cannot name it (a closure):
            // the error relates to nothing, and the return slot is checked as any other local,
            // its regions inferred. rust-analyzer recovers none, so it is the body's type here.
            let recovered = signature
                .as_ref()
                .is_some_and(|(_, output)| any_ty(*output, |ty| ty.is_ty_error()));
            let ty = match &signature {
                Some((_, output)) if !opaque && !recovered => *output,
                // The type an `impl Trait` hides is inferred, the lifetimes of its trait objects
                // too; a trait object the signature writes next to it keeps its default.
                Some((_, output))
                    if let TyKind::Alias(_, alias) = output.kind()
                        && let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind =>
                {
                    rustc_type_ir::TypeFoldable::fold_with(
                        body.locals[return_local].ty.as_ref(),
                        &mut ObjectDefaults {
                            db: self.ctx.db,
                            interner: self.interner(),
                            keep_static: writes_static_object(self.ctx.store),
                            erase: true,
                        },
                    )
                }
                _ => body.locals[return_local].ty.as_ref(),
            };
            let mut names_parameter = false;
            rustc_type_ir::fold_regions(self.interner(), ty, |region, _| {
                names_parameter |= matches!(
                    region.kind(),
                    RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_)
                );
                region
            });
            // A lifetime parameter it captures is not a universal region until something names
            // it.
            let captures_parameter = signature.as_ref().is_some_and(|(_, output)| {
                let TyKind::Alias(_, alias) = output.kind() else { return false };
                let Some(captured) = alias
                    .kind
                    .try_to_opaque()
                    .and_then(|opaque| opaque_captures(self.ctx.frame, self.ctx.db, opaque, alias.args.len()))
                else {
                    return false;
                };
                alias.args.iter().zip(captured).any(|(arg, captured)| {
                    captured
                        && matches!(arg.kind(), GenericArgKind::Lifetime(region)
                            if !matches!(region.kind(), RegionKind::ReStatic))
                })
            });
            let hidden_static = opaque
                && self.regions.universals.len() == 2
                && !names_parameter
                && !captures_parameter;
            // Of each `impl Trait` in it, `Box<impl Future>` too.
            let captures_lifetime = signature.as_ref().is_some_and(|(_, output)| {
                any_ty(*output, |ty| {
                    let TyKind::Alias(_, alias) = ty.kind() else { return false };
                    let rustc_type_ir::AliasTyKind::Opaque { def_id: opaque } = alias.kind else {
                        return false;
                    };
                    let Some(captured) =
                        opaque_captures(self.ctx.frame, self.ctx.db, opaque, alias.args.len())
                    else {
                        return true;
                    };
                    alias.args.iter().zip(captured).any(|(arg, captured)| {
                        captured && matches!(arg.kind(), GenericArgKind::Lifetime(_))
                    })
                })
            });
            let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                if (opaque || recovered || signature.is_none())
                    && matches!(region.kind(), RegionKind::ReVar(_))
                {
                    return self.fresh();
                }
                self.signature_region(region, depth).unwrap_or_else(|| match (elided, is_fn) {
                    // A `const { .. }` block's type is its creator's, by position.
                    _ if self.ctx.inline_const => {
                        let region = self.new_universal(UniversalKind::Creator);
                        self.regions
                            .creator_positions
                            .insert(self.regions.universals.len() - 1, self.const_positions);
                        self.const_positions += 1;
                        region
                    }
                    // What a fn pointer in the hidden type elides is its own, as in the one
                    // `match k { a => mode, b => name }` coerces fn items to. An argument of
                    // the hidden type itself is not, and is 'static as the rest.
                    _ if opaque
                        && depth != rustc_type_ir::INNERMOST
                        && matches!(region.kind(), RegionKind::ReError(_)) =>
                    {
                        self.fresh_elided()
                    }
                    _ if hidden_static => self.static_region(),
                    _ if opaque || recovered => self.fresh(),
                    (Some(it), _) => it,
                    (None, true) => self.new_universal(UniversalKind::Anon),
                    // The elided lifetimes of the type of a constant or a static are `'static`.
                    (None, false) => self.static_region(),
                })
            });
            // rustc's MIR has the return slot's type normalized: a projection whose impl
            // gives it a lifetime of its own is that lifetime an existential region, live
            // where the slot is.
            let ty = match ty.kind() {
                TyKind::Alias(_, alias)
                    if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. })
                        && self.normalize(ty).is_none() =>
                {
                    normalize_with_fresh(self.ctx, ty).unwrap_or(ty)
                }
                _ => ty,
            };
            // What it returns is borrowed for `'static` through rustc's `OpaqueType` constraint,
            // but rustc explains the borrow by the return value it is live in first, E0597: only
            // where alias liveness keeps no region of the hidden type live, an alias whose bounds
            // say it outlives `'static`, is it the return that is blamed, E0515.
            self.regions.returns_static_opaque = hidden_static
                && !captures_lifetime
                && !super::liveness::live_regions(self.ctx, &self.regions, ty).is_empty();
            local_tys[return_local.into_raw().into_u32() as usize] = Some(ty);
            // rustc proves an `impl Trait`'s bounds of the type it hides where the body
            // defines it: `impl Iterator<Item = Box<dyn Foo>>` hiding `Iter<'a, T>` needs the
            // where clauses of `Iter<'a, T>`'s `Iterator` impl and its `Item` the bound's. An
            // `async fn`'s output is related as it declares it, below.
            let is_async = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) => {
                    FunctionSignature::of(self.ctx.db, function).is_async()
                }
                _ => false,
            };
            if opaque
                && !is_async
                && let Some((_, output)) = &signature
                && let TyKind::Alias(_, alias) = output.kind()
                && let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind
            {
                let interner = self.interner();
                let bounds = rustc_type_ir::Interner::item_bounds(interner, def_id.into());
                let mut hidden = Replace::new(interner, (*output, ty));
                for clause in bounds.iter_instantiated(interner, alias.args.as_slice()) {
                    let clause = clause.skip_norm_wip();
                    if !matches!(
                        clause.kind().skip_binder(),
                        ClauseKind::Trait(_) | ClauseKind::Projection(_)
                    ) {
                        continue;
                    }
                    let clause = rustc_type_ir::TypeFoldable::fold_with(clause, &mut hidden);
                    let clause =
                        rustc_type_ir::fold_regions(interner, clause, |region, depth| match region
                            .kind()
                        {
                            RegionKind::ReVar(_) => region,
                            _ => {
                                self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
                            }
                        });
                    self.opaque_bounds.push(clause);
                }
                self.opaque_hidden = Some(ty);
            }
            let mut unsuggested = Vec::new_in(self.ctx.frame);
            regions_of(ty, &mut unsuggested);
            if let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = owner
                && FunctionSignature::of(self.ctx.db, function).has_self_param()
                && let Some(&local) = body.param_locals.first()
                && let Some(ty) = local_tys[local.into_raw().into_u32() as usize]
            {
                regions_of(ty, &mut unsuggested);
            }
            for rv in unsuggested {
                self.regions.unsuggested.extend(self.regions.universal(rv));
            }
            // Nor for an anonymous lifetime no parameter's written type has, the impl's in
            // `impl MyStruct<'_>` or a C variadic's `ap: ...`: rustc's `find_param_with_region`
            // finds no parameter.
            let variadic = match owner {
                InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) => {
                    FunctionSignature::of(self.ctx.db, function).is_varargs()
                }
                _ => false,
            };
            let written_params = body.param_locals.len().saturating_sub(usize::from(variadic));
            let mut in_params = Vec::new_in(self.ctx.frame);
            for &local in body.param_locals.iter().take(written_params) {
                if let Some(ty) = local_tys[local.into_raw().into_u32() as usize] {
                    regions_of(ty, &mut in_params);
                }
            }
            let mut universal_in_params =
                bits(self.ctx.frame, self.regions.universals.len());
            for rv in in_params {
                if let Some(index) = self.regions.universal(rv) {
                    universal_in_params.insert(index);
                }
            }
            for index in 0..self.regions.universals.len() {
                if self.regions.universals[index].1 == UniversalKind::AnonParam
                    && !universal_in_params.contains(index)
                {
                    self.regions.unsuggested.insert(index);
                }
            }
            // A C variadic's `VaList` lifetime is none of the function's generics, which are
            // all an `impl Trait` it returns could capture; a lifetime parameter no parameter
            // names is no choice of a region of the body.
            if opaque
                && !hidden_static
                && let Some((_, output)) = &signature
                && let Some((alias, hidden)) = returned_opaque(*output, ty)
                && let rustc_type_ir::AliasTyKind::Opaque { def_id: opaque_id } = alias.kind
                && let Some(captured) =
                    opaque_captures(self.ctx.frame, self.ctx.db, opaque_id, alias.args.len())
                        .or_else(|| variadic.then(|| filled(self.ctx.frame, alias.args.len(), true)))
            {
                let mut choices = Vec::new_in(self.ctx.frame);
                choices.push(self.regions.static_rv);
                let mut known = true;
                for (arg, captured) in alias.args.iter().zip(captured) {
                    if let GenericArgKind::Lifetime(region) = arg.kind()
                        && captured
                    {
                        match self
                            .signature_region(region, rustc_type_ir::INNERMOST)
                            .and_then(rv_of)
                        {
                            Some(rv) => choices.push(rv),
                            None if variadic => (),
                            None => known = false,
                        }
                    }
                }
                // What the bounds say the `impl Trait` outlives its hidden type must: rustc
                // proves them of it. A projection bound that names a lifetime constrains it in
                // ways not followed here, so its member constraints are left out.
                let mut outlived = Vec::new_in(self.ctx.frame);
                let bounds =
                    rustc_type_ir::Interner::item_bounds(self.interner(), opaque_id.into());
                for clause in bounds.iter_instantiated(self.interner(), alias.args.as_slice()) {
                    match clause.kind().skip_binder() {
                        ClauseKind::TypeOutlives(OutlivesClause(bounded, region))
                            if let TyKind::Alias(_, bounded) = bounded.kind()
                                && bounded.kind == alias.kind =>
                        {
                            // Under the clause's own binder, the signature's is one further out.
                            let binder = rustc_type_ir::INNERMOST.shifted_in(1);
                            // A lifetime the bound elides, `+ '_`, rustc captures too.
                            let elided = matches!(region.kind(), RegionKind::ReError(_));
                            let rv = match elided {
                                true => self.elided_output.and_then(rv_of),
                                false => self.signature_region(region, binder).and_then(rv_of),
                            };
                            match rv {
                                Some(rv) => {
                                    outlived.push(rv);
                                    if elided && !choices.contains(&rv) {
                                        choices.push(rv);
                                    }
                                }
                                None => known = false,
                            }
                        }
                        ClauseKind::Projection(projection)
                            if match projection.term.as_type() {
                                Some(ty) => {
                                    names_captured_lifetime(self.ctx.frame, self.ctx.db, ty)
                                        && !unbounded_opaque(self.ctx.frame, self.ctx.db, self.interner(), ty)
                                }
                                None => rustc_type_ir::TypeVisitableExt::has_free_regions(
                                    &projection.term,
                                ),
                            } =>
                        {
                            known = false;
                        }
                        _ => (),
                    }
                }
                let mut members = Vec::new_in(self.ctx.frame);
                member_regions(self.ctx.frame, self.ctx.db, hidden, true, &mut members);
                members.retain(|&it| it != self.regions.static_rv);
                radix::sort(&mut members);
                members.dedup();
                // An alias's own bounds may already say it outlives them, so the regions within
                // one are left to the member constraints.
                let mut bounded = Vec::new_in(self.ctx.frame);
                member_regions(self.ctx.frame, self.ctx.db, hidden, false, &mut bounded);
                bounded.retain(|&it| it != self.regions.static_rv);
                radix::sort(&mut bounded);
                bounded.dedup();
                for &member in &bounded {
                    for &sub in &outlived {
                        self.regions.outlives.push(Outlives {
                            sup: member,
                            sub,
                            location: None,
                            category: Category::Boring,
                            span: MirSpan::Unknown,
                            from_closure: false,
                        });
                    }
                }
                if known {
                    self.regions.members = members;
                    self.regions.choices = choices;
                }
            }
            if let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = owner {
                self.regions.is_impl_item = matches!(
                    function.lookup(self.ctx.db).container,
                    ItemContainerId::ImplId(it) if self.ctx.db.impl_trait(it).is_some()
                );
            }
        }
        // A variable with a type written out is related to it, lifetimes and all, as rustc
        // relates it to the user type annotation.
        let annotated = self.annotated_locals();
        for (local, data) in body.locals.iter() {
            let index = local.into_raw().into_u32() as usize;
            if local_tys[index].is_some() {
                continue;
            }
            let ty = data.ty.as_ref();
            local_tys[index] = Some(match annotated.get(&local) {
                Some(&annotation) => {
                    let written = self.annotation_ty(annotation).filter(|it| {
                        let interner = self.interner();
                        let erase = |ty: Ty<'db>| {
                            rustc_type_ir::fold_regions(interner, ty, |_, _| {
                                Region::new_static(interner)
                            })
                        };
                        erase(*it) == erase(ty)
                    });
                    self.annotated.insert(local, written);
                    match written {
                        Some(_) => self.renumber(ty),
                        None => self.written(ty),
                    }
                }
                None => self.renumber(ty),
            });
        }
        self.regions.local_tys = mem::collect(self.ctx.frame, local_tys.into_iter().map(|it| it.unwrap()));
    }

    /// The arguments of a closure's or coroutine's type with each region a universal region of
    /// its creator's, by position. They start with the `parents` generic arguments of the
    /// enclosing item, which rustc makes the item's own parameters where the closure is made.
    fn creator_args(&mut self, args: GenericArgs<'db>, parents: usize) -> GenericArgs<'db> {
        let args =
            rustc_type_ir::fold_regions(self.interner(), args, |region, _| match region.kind() {
                RegionKind::ReBound(..) => region,
                _ => {
                    let region = self.new_universal(UniversalKind::Creator);
                    let position = self.regions.creator_positions.len();
                    self.regions
                        .creator_positions
                        .insert(self.regions.universals.len() - 1, position);
                    region
                }
            });
        for (index, arg) in args.iter().take(parents).enumerate() {
            if let GenericArgKind::Lifetime(region) = arg.kind()
                && let Some(rv) = rv_of(region)
            {
                self.early.insert(index as u32, region);
                self.regions.early.insert(index as u32, rv);
            }
        }
        args
    }

    /// A closure's parameters, return slot and environment typed as rustc's `UniversalRegions`
    /// types them: each region of the closure's type a universal region of its creator's, and
    /// the late-bound lifetimes of its signature and the borrow of its environment its own.
    /// `false`, with nothing set, when the closure's type does not have the shape for it.
    fn closure_signature(&mut self, local_tys: &mut [Option<Ty<'db>>]) -> bool {
        let body = self.body;
        let Some((&env_local, params)) = body.param_locals.split_first() else { return false };
        let env = body.locals[env_local].ty.as_ref();
        let (closure, by_ref) = match env.kind() {
            TyKind::Ref(_, inner, mutability) => (inner, Some(mutability)),
            _ => (env, None),
        };
        let interner = self.interner();
        if let TyKind::Coroutine(def, args) = closure.kind() {
            // An `async` body is polled through `&mut` its future, with a `&mut Context` of the
            // caller's; it returns what its type says the future's output is.
            let args = self.creator_args(args, args.as_coroutine().parent_args().len());
            let mut coroutine = Ty::new_coroutine(interner, def, args);
            if let Some(mutability) = by_ref {
                let region = self.new_universal(UniversalKind::AnonParam);
                self.regions.closure_env = Some(self.regions.universals.len() - 1);
                coroutine = Ty::new_ref(interner, region, coroutine, mutability);
            }
            local_tys[env_local.into_raw().into_u32() as usize] = Some(coroutine);
            for &local in params {
                let ty = rustc_type_ir::fold_regions(
                    interner,
                    body.locals[local].ty.as_ref(),
                    |region, _| match region.kind() {
                        RegionKind::ReBound(..) => region,
                        _ => self.new_universal(UniversalKind::AnonParam),
                    },
                );
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
            local_tys[super::return_slot().into_raw().into_u32() as usize] =
                Some(args.as_coroutine().return_ty());
            return true;
        }
        if let TyKind::CoroutineClosure(def, args) = closure.kind() {
            // An `async` closure called by reference returns a future whose captures are
            // borrowed for as long as that reference: rustc's `AsyncFn` lending.
            let TyKind::Coroutine(coroutine, _) =
                body.locals[super::return_slot()].ty.as_ref().kind()
            else {
                return false;
            };
            let args = self.creator_args(args, args.as_coroutine_closure().parent_args().len());
            let mut env_ty = Ty::new_coroutine_closure(interner, def, args);
            let (kind, env_region) = match by_ref {
                Some(mutability) => {
                    let region = self.new_universal(UniversalKind::AnonParam);
                    self.regions.closure_env = Some(self.regions.universals.len() - 1);
                    env_ty = Ty::new_ref(interner, region, env_ty, mutability);
                    let kind = if mutability.is_mut() {
                        rustc_type_ir::ClosureKind::FnMut
                    } else {
                        rustc_type_ir::ClosureKind::Fn
                    };
                    (kind, region)
                }
                None => (rustc_type_ir::ClosureKind::FnOnce, Region::new_static(interner)),
            };
            let parts = args.as_coroutine_closure();
            let mut late = mem::map(self.ctx.frame);
            let sig = rustc_type_ir::fold_regions(
                interner,
                parts.coroutine_closure_sig().skip_binder(),
                |region, depth| match region.kind() {
                    RegionKind::ReBound(
                        rustc_type_ir::BoundVarIndexKind::Bound(debruijn),
                        bound,
                    ) if debruijn == depth => *late
                        .entry(bound.var.as_u32())
                        .or_insert_with(|| self.new_universal(UniversalKind::AnonParam)),
                    _ => region,
                },
            );
            let TyKind::Tuple(inputs) = sig.tupled_inputs_ty.kind() else { return false };
            if inputs.len() != params.len() {
                return false;
            }
            local_tys[env_local.into_raw().into_u32() as usize] = Some(env_ty);
            for (&local, ty) in params.iter().zip(inputs.iter()) {
                local_tys[local.into_raw().into_u32() as usize] = Some(ty);
            }
            local_tys[super::return_slot().into_raw().into_u32() as usize] =
                Some(sig.to_coroutine_given_kind_and_upvars(
                    interner,
                    parts.parent_args(),
                    coroutine,
                    kind,
                    env_region,
                    parts.tupled_upvars_ty(),
                    parts.coroutine_captures_by_ref_ty(),
                ));
            return true;
        }
        let TyKind::Closure(def, args) = closure.kind() else { return false };
        let sig = args.as_closure().sig();
        let &[tupled] = sig.skip_binder().inputs().as_slice() else { return false };
        let TyKind::Tuple(inputs) = tupled.kind() else { return false };
        if inputs.len() != params.len() {
            return false;
        }
        let args = self.creator_args(args, args.as_closure().parent_args().len());
        let mut closure = Ty::new_closure(interner, def, args);
        if let Some(mutability) = by_ref {
            let region = self.new_universal(UniversalKind::AnonParam);
            self.regions.closure_env = Some(self.regions.universals.len() - 1);
            closure = Ty::new_ref(interner, region, closure, mutability);
        }
        let mut late = mem::map(self.ctx.frame);
        let sig = rustc_type_ir::fold_regions(
            interner,
            args.as_closure().sig().skip_binder(),
            |region, depth| match region.kind() {
                RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(debruijn), bound)
                    if debruijn == depth =>
                {
                    *late
                        .entry(bound.var.as_u32())
                        .or_insert_with(|| self.new_universal(UniversalKind::AnonParam))
                }
                _ => region,
            },
        );
        let &[tupled] = sig.inputs().as_slice() else { return false };
        let TyKind::Tuple(inputs) = tupled.kind() else { return false };
        local_tys[env_local.into_raw().into_u32() as usize] = Some(closure);
        for (index, (&local, ty)) in params.iter().zip(inputs.iter()).enumerate() {
            let ty = self.elided_own(index, ty).unwrap_or(ty);
            let ty = self.written_object_default(index, ty).unwrap_or(ty);
            local_tys[local.into_raw().into_u32() as usize] = Some(ty);
        }
        local_tys[super::return_slot().into_raw().into_u32() as usize] = Some(sig.output());
        true
    }

    /// The closure's parameter `index` written `&dyn Trait`, as `&'r (dyn Trait + 'r)` with
    /// its reference's lifetime, as rustc's object lifetime default, where inference gave the
    /// object a region of its own. A trait bounded by `'static` (`trait Db: 'static`, as
    /// `blaze_query::db::Database` is) defaults its objects to `'static` instead, as rustc's
    /// `object_region_bounds` does before it looks at the reference.
    fn written_object_default(&mut self, index: usize, ty: Ty<'db>) -> Option<Ty<'db>> {
        let closure = self.ctx.closure_expr?;
        let hir_def::hir::Expr::Closure { arg_types, .. } = self.ctx.store.expr(closure) else {
            return None;
        };
        let written = self.lower_written((*arg_types.get(index)?)?, MirSpan::ExprId(closure));
        let TyKind::Ref(_, written, _) = written.kind() else { return None };
        let TyKind::Dynamic(_, object) = written.kind() else { return None };
        if !matches!(object.kind(), RegionKind::ReStatic) || writes_static_object(self.ctx.store) {
            return None;
        }
        let TyKind::Ref(region, inner, mutability) = ty.kind() else { return None };
        let TyKind::Dynamic(predicates, _) = inner.kind() else { return None };
        let interner = self.interner();
        use rustc_type_ir::inherent::BoundExistentialPredicates as _;
        let object = match predicates.principal_def_id() {
            Some(it) if static_trait(self.ctx.db, it.0) => Region::new_static(interner),
            _ => region,
        };
        Some(Ty::new_ref(
            interner,
            region,
            Ty::new_dynamic(interner, predicates, object),
            mutability,
        ))
    }

    /// The type of the closure's parameter `index`, where the closure is typed without an
    /// expected signature, with each lifetime elided in its written type a late-bound one of
    /// the closure's own, as rustc's closure signature binds them: `|x: &i32| -> &i32 { x }`
    /// returns what lives as long as the call, not as long as the closure's type says.
    fn elided_own(&mut self, index: usize, ty: Ty<'db>) -> Option<Ty<'db>> {
        let closure = self.ctx.closure_expr?;
        if !self.ctx.infer.closures_without_expectation.contains(&closure) {
            return None;
        }
        let hir_def::hir::Expr::Closure { arg_types, .. } = self.ctx.store.expr(closure) else {
            return None;
        };
        let written = self.lower_written((*arg_types.get(index)?)?, MirSpan::ExprId(closure));
        let frame = self.ctx.frame;
        let regions = |ty: Ty<'db>| {
            let mut found = Vec::new_in(frame);
            rustc_type_ir::fold_regions(self.interner(), ty, |region, _| {
                found.push(region);
                region
            });
            found
        };
        let elided: FVec<'_, bool> = mem::collect(
            frame,
            regions(written).into_iter().map(|region| {
                !matches!(
                    region.kind(),
                    RegionKind::ReEarlyParam(_)
                        | RegionKind::ReLateParam(_)
                        | RegionKind::ReStatic
                        | RegionKind::ReBound(..)
                )
            }),
        );
        if elided.len() != regions(ty).len() || !elided.contains(&true) {
            return None;
        }
        let mut position = 0;
        Some(rustc_type_ir::fold_regions(self.interner(), ty, |region, _| {
            position += 1;
            match elided[position - 1] {
                true => self.new_universal(UniversalKind::AnonParam),
                false => region,
            }
        }))
    }

    /// What a `let` with a type written out requires, as rustc's user type annotations: that
    /// the type is well-formed, and for a `_` pattern that the value has the type.
    fn annotations(&mut self) {
        let scan = self.ctx.scan;
        self.pattern_paths();
        self.path_arguments();
        self.closure_annotations();
        for &annotation in &scan.lets {
            let (pat, initializer) = (annotation.pat, annotation.initializer);
            // A variable with a type written out has it, lifetimes and all.
            let local = match self.ctx.store.pat(pat) {
                Pat::Bind { id, subpat: None } => self.body.binding_locals.get(id).copied(),
                _ => None,
            };
            let at = At { category: Category::TypeAnnotation, span: MirSpan::PatId(pat) };
            let ty = match local.and_then(|it| Some((it, *self.annotated.get(&it)?))) {
                Some((local, written)) => {
                    let local_ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
                    if let Some(written) = written {
                        self.relate(local_ty, written, Variance::Invariant, at);
                    }
                    Some(local_ty)
                }
                None => self.annotation_ty(annotation),
            };
            let Some(ty) = ty else { continue };
            self.well_formed(ty, at, 0);
            if !matches!(self.ctx.store.pat(pat), Pat::Bind { subpat: None, .. }) {
                self.annotated_bindings(pat, ty, at, true);
            }
            if !matches!(self.ctx.store.pat(pat), Pat::Bind { .. })
                && let Some(initializer) = initializer
            {
                let value = match self.path_local(initializer) {
                    Some(local) => {
                        Some(self.regions.local_tys[local.into_raw().into_u32() as usize])
                    }
                    None => self.ascribed_ty(pat),
                };
                if let Some(value) = value {
                    // `let _: <(u8, _) as Tr>::Ty = &temp();`: the value has the annotation
                    // normalized, as rustc's user type annotation.
                    let ty = match rustc_type_ir::TypeVisitableExt::has_aliases(&ty) {
                        true => self.normalize_at(ty, at).unwrap_or(ty),
                        false => ty,
                    };
                    self.relate(value, ty, Variance::Covariant, at);
                }
            }
        }
    }

    /// A closure's parameters and return slot have the types written for them, as rustc's
    /// `check_signature_annotation`: in `|x: &'a i32| -> &'static i32` that `x` is a
    /// `&'a i32` and the closure returns a `&'static i32`.
    fn closure_annotations(&mut self) {
        let Some(closure) = self.ctx.closure_expr else { return };
        let hir_def::hir::Expr::Closure {
            arg_types,
            ret_type,
            closure_kind: hir_def::hir::ClosureKind::Closure,
            ..
        } = self.ctx.store.expr(closure)
        else {
            return;
        };
        let params = self.body.param_locals.get(1..).unwrap_or_default();
        if !self.regions.check_universals || params.len() != arg_types.len() {
            return;
        }
        let span = MirSpan::ExprId(closure);
        let at = At { category: Category::Boring, span };
        let written = mem::collect(
            self.ctx.frame,
            params
                .iter()
                .copied()
                .zip(arg_types.iter().copied())
                .chain([(super::return_slot(), ret_type)]),
        );
        for (local, type_ref) in written {
            let Some(type_ref) = type_ref else { continue };
            let local_ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
            // `|d: <(_,) as Tr>::Ty|`: the `_`s are what the parameter's type makes them, and
            // the annotation is well-formed, the impl it selects' where clauses holding.
            if let Some(solved) = self.solved_annotation(type_ref, local_ty, span) {
                let ty = self.written(solved);
                self.relate(local_ty, ty, Variance::Invariant, at);
                let at = At { category: Category::TypeAnnotation, span };
                self.well_formed(ty, at, 0);
                continue;
            }
            let ty = self.written(self.lower_written(type_ref, span));
            self.relate(local_ty, ty, Variance::Invariant, at);
        }
    }

    /// The variables a pattern `pat` with the type `ty` written binds have the parts of `ty`
    /// they bind, as rustc's user type projections: in `let (y, _): (&'static u32, u32)`,
    /// `y` is a `&'static u32`. Not through a binding by reference or a pattern that derefs
    /// what it matches by default binding modes.
    fn annotated_bindings(&mut self, pat: hir_def::hir::PatId, ty: Ty<'db>, at: At, top: bool) {
        if self.ctx.infer.pat_adjustments.contains_key(&pat) {
            return;
        }
        let interner = self.interner();
        match self.ctx.store.pat(pat) {
            Pat::Bind { id, subpat } => {
                if !top
                    && let Some(hir_ty::BindingMode(by_ref, _)) = self.ctx.infer.binding_mode(pat)
                    && let Some(&local) = self.body.binding_locals.get(id)
                {
                    let local_ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
                    // `ref y` borrows the part, which has the type.
                    match (by_ref, local_ty.kind()) {
                        (hir_ty::ByRef::No, _) => {
                            self.relate(local_ty, ty, Variance::Invariant, at)
                        }
                        (hir_ty::ByRef::Yes(_), TyKind::Ref(_, inner, _)) => {
                            self.relate(inner, ty, Variance::Invariant, at)
                        }
                        _ => (),
                    }
                }
                if let Some(subpat) = subpat {
                    self.annotated_bindings(subpat, ty, at, false);
                }
            }
            Pat::Tuple { args, ellipsis: None } => {
                if let TyKind::Tuple(tys) = ty.kind()
                    && tys.len() == args.len()
                {
                    for (pat, ty) in args.iter().zip(tys.iter()) {
                        self.annotated_bindings(*pat, ty, at, false);
                    }
                }
            }
            Pat::Ref { pat, .. } => {
                if let TyKind::Ref(_, inner, _) = ty.kind() {
                    self.annotated_bindings(pat, inner, at, false);
                }
            }
            Pat::Record { args, ellipsis: _, .. } => {
                let TyKind::Adt(_, adt_args) = ty.kind() else { return };
                let Some(variant) = self.ctx.infer.variant_resolution_for_pat(pat) else { return };
                let fields = hir_def::signatures::VariantFields::of(self.ctx.db, variant);
                let types = self.ctx.db.field_types(variant);
                let parts = mem::collect(
                    self.ctx.frame,
                    args.iter().filter_map(|it| {
                        let field = fields.field(&it.name)?;
                        Some((it.pat, types[field].ty().instantiate(interner, adt_args)))
                    }),
                );
                for (pat, field) in parts {
                    let mut field = field.skip_norm_wip();
                    if rustc_type_ir::TypeVisitableExt::has_aliases(&field) {
                        field = self.normalize(field).unwrap_or(field);
                    }
                    self.annotated_bindings(pat, field, at, false);
                }
            }
            Pat::TupleStruct { args, ellipsis: None, .. } => {
                let TyKind::Adt(_, adt_args) = ty.kind() else { return };
                let Some(variant) = self.ctx.infer.variant_resolution_for_pat(pat) else { return };
                let types = self.ctx.db.field_types(variant);
                let parts = mem::collect(
                    self.ctx.frame,
                    args.iter()
                        .zip(types.iter())
                        .map(|(pat, (_, field))| (*pat, field.ty().instantiate(interner, adt_args))),
                );
                for (pat, field) in parts {
                    let mut field = field.skip_norm_wip();
                    if rustc_type_ir::TypeVisitableExt::has_aliases(&field) {
                        field = self.normalize(field).unwrap_or(field);
                    }
                    self.annotated_bindings(pat, field, at, false);
                }
            }
            _ => (),
        }
    }

    /// The generic arguments a path in the body spells out are well-formed, reached or not,
    /// as rustc's user type annotations: `None::<<&'a () as Trait>::Ty>` that `&'a ()`
    /// implements `Trait`.
    fn path_arguments(&mut self) {
        use rustc_type_ir::inherent::GenericArg as _;
        let store = self.ctx.store;
        let mut found = Vec::new_in(self.ctx.frame);
        let mut items = Vec::new_in(self.ctx.frame);
        // A path in a closure in this body is the closure's, which proves it.
        for &(expr, closure) in &self.ctx.scan.spelled_paths {
            if closure != self.ctx.closure_expr {
                continue;
            }
            let hir_def::hir::Expr::Path(path) = store.expr(expr) else { continue };
            if spells_generics(path)
                && let Some(ty) = self.ctx.infer.type_of_expr.get(expr)
                && let TyKind::FnDef(def, args) = ty.as_ref().kind()
            {
                items.push((expr, def.0, args.no_bound_vars().unwrap()));
            }
            let positions = self.spelled_positions(expr, path);
            let fn_args = match self.ctx.infer.type_of_expr.get(expr).map(|it| it.as_ref().kind()) {
                Some(TyKind::FnDef(_, args)) => Some(args.no_bound_vars().unwrap()),
                _ => None,
            };
            for args in path.generic_args().into_iter().flatten().flatten() {
                for arg in args.args.iter() {
                    if let hir_def::expr_store::path::GenericArg::Type(type_ref) = arg {
                        let inferred = positions
                            .iter()
                            .flatten()
                            .find(|(it, _)| it == type_ref)
                            .and_then(|&(_, index)| fn_args?.get(index)?.as_type());
                        found.push((expr, *type_ref, inferred));
                    }
                }
            }
        }
        for (expr, type_ref, inferred) in found {
            let span = MirSpan::ExprId(expr);
            // `Some::<<(_,) as Tr>::Ty>(b)`: the `_`s are what the argument inferred makes them.
            let solved =
                inferred.and_then(|inferred| self.solved_annotation(type_ref, inferred, span));
            let ty = solved.unwrap_or_else(|| self.lower_written(type_ref, span));
            let ty = self.written(ty);
            self.well_formed(ty, At { category: Category::TypeAnnotation, span }, 0);
        }
        // A closure proves its calls' where clauses at them, which it can propagate.
        if self.ctx.closure_local.is_some() {
            return;
        }
        for (expr, def, args) in items {
            let args = self.written(args);
            self.callee_predicates(def, args, MirSpan::ExprId(expr));
        }
    }

    /// What a struct, variant or constant pattern whose path spells out generic arguments
    /// requires of the value it matches, `let Foo::<'static> { .. } = foo;` that `foo` is a
    /// `Foo<'static>`, as rustc's user type annotation on the pattern. Only for the whole
    /// pattern of a `let` or a `match` arm, matching a variable or what the `let` reads.
    fn pattern_paths(&mut self) {
        let scan = self.ctx.scan;
        for &(pat, value) in &scan.spelled_patterns {
            let Some(ty) = self.ctx.infer.type_of_pat.get(pat).map(|it| it.as_ref()) else {
                continue;
            };
            let value = match value.and_then(|it| self.path_local(it)) {
                Some(local) => Some(self.regions.local_tys[local.into_raw().into_u32() as usize]),
                None => self.ascribed_ty(pat),
            };
            let Some(value) = value else { continue };
            let ty = self.written(ty);
            let at = At { category: Category::TypeAnnotation, span: MirSpan::PatId(pat) };
            if let Pat::Path(_) = self.ctx.store.pat(pat)
                && let Some(constant) = self.pattern_constant(pat)
            {
                // rustc ascribes a constant's type to what it matches contravariantly:
                // `x: &'x str` matches a `&'static str`, a `&'a str` only matches a
                // `&'static str` if `'a: 'static`.
                let constant = self.regions.local_tys[constant.into_raw().into_u32() as usize];
                self.relate(constant, value, Variance::Covariant, at);
            } else if let Pat::Path(_) = self.ctx.store.pat(pat) {
                let common = self.renumber(value);
                self.relate(value, common, Variance::Covariant, at);
                self.relate(ty, common, Variance::Covariant, at);
            } else {
                self.relate(value, ty, Variance::Covariant, at);
            }
        }
    }

    /// The local a constant pattern's value is put in to be compared.
    fn pattern_constant(&self, pat: hir_def::hir::PatId) -> Option<LocalId> {
        self.body.basic_blocks.iter().find_map(|(_, block)| {
            block.statements.iter().find_map(|statement| match &statement.kind {
                StatementKind::Assign(place, Rvalue::Use(operand))
                    if statement.span == MirSpan::PatId(pat)
                        && matches!(operand.kind, OperandKind::Constant { .. })
                        && place.as_ref().projection.as_slice().is_empty() =>
                {
                    Some(place.as_ref().local)
                }
                _ => None,
            })
        })
    }

    /// The type of the place the `let` of the pattern `pat` reads its value from: the fake
    /// read lowered at the pattern.
    fn ascribed_ty(&self, pat: hir_def::hir::PatId) -> Option<Ty<'db>> {
        self.body.basic_blocks.iter().find_map(|(_, block)| {
            block.statements.iter().find_map(|statement| match &statement.kind {
                StatementKind::FakeRead(place) if statement.span == MirSpan::PatId(pat) => {
                    let place = place.as_ref();
                    Some(self.place_ty(place.local, place.projection.as_slice()).ty)
                }
                _ => None,
            })
        })
    }

    /// The local of the variable `expr` names, if it is a path to one of this body's.
    fn path_local(&self, expr: hir_def::hir::ExprId) -> Option<LocalId> {
        self.body.binding_locals.get(self.path_binding(expr)?).copied()
    }

    /// The variable a call's callee names, a capture of this closure's too.
    fn callee_local(&self, expr: hir_def::hir::ExprId) -> Option<LocalId> {
        let binding = self.path_binding(expr)?;
        self.body
            .binding_locals
            .get(binding)
            .copied()
            .or_else(|| self.ctx.upvars.iter().find(|it| it.binding == binding).map(|it| it.local))
    }

    fn path_binding(&self, expr: hir_def::hir::ExprId) -> Option<hir_def::hir::BindingId> {
        path_binding(self.ctx, self.body, expr)
    }

    /// The constraints the type `ty` holds being well-formed: in `&'a T` that `T: 'a`, and
    /// the where clauses of the structs and enums it names.
    fn well_formed(&mut self, ty: Ty<'db>, at: At, depth: usize) {
        if depth > 8 {
            return;
        }
        match ty.kind() {
            TyKind::Ref(region, inner, _) => {
                self.type_outlives(inner, region, at);
                self.well_formed(inner, at, depth + 1);
            }
            TyKind::Adt(def, args) => {
                self.predicates(GenericDefId::AdtId(def.def_id()), args, at, true);
                for arg in args.iter() {
                    if let GenericArgKind::Type(it) = arg.kind() {
                        self.well_formed(it, at, depth + 1);
                    }
                }
            }
            TyKind::Tuple(tys) => {
                for it in tys.iter() {
                    self.well_formed(it, at, depth + 1);
                }
            }
            TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
                self.well_formed(inner, at, depth + 1)
            }
            // An associated type is well-formed where its trait is implemented, `<&'a () as
            // Trait>::Ty` with only `impl Trait for &'static ()` that `'a: 'static`.
            TyKind::Alias(_, alias) => {
                for it in alias.args.types() {
                    self.well_formed(it, at, depth + 1);
                }
                if let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind {
                    self.prove(alias.trait_ref(self.interner()), at);
                    // And what it normalizes to, as rustc checks the normalized signature of a
                    // call: `FnOnce(&'s str) -> [&'static &'s (); 0]` that `'s: 'static`. Not
                    // one with a trait object, whose lifetime rust-analyzer can make `'static`.
                    if rustc_type_ir::TypeVisitableExt::has_free_regions(&ty)
                        && let Some(normalized) = self.normalize(ty)
                        && !matches!(normalized.kind(), TyKind::Alias(..) | TyKind::Infer(_))
                        && !normalized.walk().any(|it| {
                            rustc_type_ir::inherent::GenericArg::as_type(&it).is_some_and(|it| {
                                matches!(it.kind(), TyKind::Dynamic(..) | TyKind::Infer(_))
                            })
                        })
                    {
                        self.well_formed(normalized, at, depth + 1);
                    }
                }
            }
            // As rustc, a function item is well-formed with its where clauses and its return
            // type, but what names its late-bound lifetimes is not checked.
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                let generic_def: GenericDefId = match def.0 {
                    CallableDefId::FunctionId(it) => it.into(),
                    CallableDefId::StructId(it) => it.into(),
                    CallableDefId::EnumVariantId(it) => it.lookup(self.ctx.db).parent.into(),
                };
                self.predicates(generic_def, args, at, true);
                let output = self
                    .ctx
                    .db
                    .callable_item_signature(def.0)
                    .instantiate(self.interner(), args)
                    .skip_norm_wip()
                    .output()
                    .skip_binder();
                self.well_formed_unbound(output, at, depth + 1);
            }
            _ => (),
        }
    }

    /// What of `ty`'s well-formedness names no region bound outside it, as rustc's `wf`
    /// leaves out a requirement with an escaping bound region: of `(&'c (), &'b &'a ())` with
    /// `'c` late-bound, that `'a: 'b`.
    fn well_formed_unbound(&mut self, ty: Ty<'db>, at: At, depth: usize) {
        use rustc_type_ir::inherent::GenericArgs as _;
        if !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) {
            return self.well_formed(ty, at, depth);
        }
        if depth > 16 {
            return;
        }
        match ty.kind() {
            TyKind::Ref(_, inner, _)
            | TyKind::RawPtr(inner, _)
            | TyKind::Slice(inner)
            | TyKind::Array(inner, _) => self.well_formed_unbound(inner, at, depth + 1),
            TyKind::Tuple(tys) => {
                for ty in tys {
                    self.well_formed_unbound(ty, at, depth + 1);
                }
            }
            TyKind::Adt(_, args) => {
                for ty in args.types() {
                    self.well_formed_unbound(ty, at, depth + 1);
                }
            }
            _ => (),
        }
    }

    /// What a path naming the associated item of `container` through a type needs, as
    /// rustc's user type annotation on it: its generic arguments well-formed, and the impl's
    /// self type and trait arguments, `<Evil>::f` with `struct Evil<'a, 'b: 'a>` that `'b: 'a`.
    fn path_well_formed(
        &mut self,
        expr: hir_def::hir::ExprId,
        container: ItemContainerId,
        args: GenericArgs<'db>,
        span: MirSpan,
    ) {
        let hir_def::hir::Expr::Path(path) = self.ctx.store.expr(expr) else { return };
        if !names_through_type(self.ctx.db, path, container) {
            return;
        }
        let at = At { category: Category::TypeAnnotation, span };
        let mut tys = self.scratch.tys.take();
        tys.extend(args.types());
        if let ItemContainerId::ImplId(impl_) = container {
            let db = self.ctx.db;
            tys.push(db.impl_self_ty(impl_).instantiate(self.interner(), args).skip_norm_wip());
            if let Some(trait_ref) = db.impl_trait(impl_) {
                let trait_ref = trait_ref.instantiate(self.interner(), args).skip_norm_wip();
                tys.extend(trait_ref.args.types().skip(1));
            }
        }
        for &ty in tys.iter() {
            self.well_formed(ty, at, 0);
        }
        self.scratch.tys.give(tys);
    }

    fn annotated_locals(&self) -> FMap<'a, LocalId, Annotation> {
        let mut found = mem::map(self.ctx.frame);
        for &annotation in &self.ctx.scan.lets {
            if let Pat::Bind { id, subpat: None } = self.ctx.store.pat(annotation.pat)
                && let Some(&local) = self.body.binding_locals.get(id)
            {
                found.insert(local, annotation);
            }
        }
        found
    }

    fn env_aliases(&mut self) {
        fn behind_refs<'db>(ty: Ty<'db>, out: &mut FVec<'_, Ty<'db>>, depth: usize) {
            if depth > 4 {
                return;
            }
            match ty.kind() {
                TyKind::Ref(_, inner, _) => {
                    alias_components(inner, out);
                    behind_refs(inner, out, depth + 1);
                }
                TyKind::Adt(_, args) => {
                    for it in args.types() {
                        behind_refs(it, out, depth + 1);
                    }
                }
                TyKind::Tuple(tys) => {
                    for it in tys.iter() {
                        behind_refs(it, out, depth + 1);
                    }
                }
                TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
                    behind_refs(inner, out, depth + 1)
                }
                _ => (),
            }
        }
        let interner = self.interner();
        let mut aliases = Vec::new_in(self.ctx.frame);
        for clause in self.ctx.env.caller_bounds() {
            if let ClauseKind::TypeOutlives(OutlivesClause(ty, _)) = clause.kind().skip_binder()
                && let TyKind::Alias(_, _) = ty.kind()
            {
                aliases.push(ty);
            }
        }
        if self.ctx.closure_local.is_none() {
            let signature = mem::collect(
                self.ctx.frame,
                self.body
                    .param_locals
                    .iter()
                    .chain((!self.returns_opaque).then_some(&super::return_slot()))
                    .map(|&it| self.regions.local_tys[it.into_raw().into_u32() as usize]),
            );
            let mut found = Vec::new_in(self.ctx.frame);
            for ty in signature {
                behind_refs(ty, &mut found, 0);
            }
            for ty in found {
                aliases.push(ty);
                if let Some(normalized) = self.normalize(ty) {
                    alias_components(normalized, &mut aliases);
                }
            }
        }
        for ty in aliases.iter_mut() {
            *ty = rustc_type_ir::fold_regions(interner, *ty, |_, _| Region::new_static(interner));
        }
        self.env_aliases = aliases;
    }

    /// What proving a signature type's higher-ranked where clauses needs, as rustc's
    /// `add_implied_bounds` applies the region constraints of proving the well-formedness of
    /// each parameter's type: `Foo<&'lt u8>` with `for<'x> T::Ty<'x>: Sized` normalizes
    /// `<&'lt u8 as Trait>::Ty<'x>` for a placeholder `'x`, whose `Self: 'x` is `'lt: 'static`
    /// (`implied-bounds/normalization-placeholder-leak.rs`). The others are what the type
    /// implies, assumed rather than proven.
    fn signature_higher_ranked(&mut self) {
        if self.ctx.closure_local.is_some() || self.ctx.inline_const {
            return;
        }
        let interner = self.interner();
        let mut clauses = self.scratch.clauses.take();
        for &local in self
            .body
            .param_locals
            .iter()
            .chain((!self.returns_opaque).then_some(&super::return_slot()))
        {
            let ty = self.regions.local_tys[local.into_raw().into_u32() as usize];
            let db = self.ctx.db;
            _ = each_ty(ty, |it| {
                let TyKind::Adt(def, args) = it.kind() else {
                    return std::ops::ControlFlow::Continue(());
                };
                for clause in GenericPredicates::query_all(db, def.def_id().into())
                    .iter_instantiated(interner, args.as_slice())
                {
                    let clause = clause.skip_norm_wip();
                    if matches!(clause.kind().skip_binder(), ClauseKind::Trait(_))
                        && !clause.kind().bound_vars().is_empty()
                        && !clauses.contains(&clause)
                    {
                        clauses.push(clause);
                    }
                }
                std::ops::ControlFlow::Continue(())
            });
        }
        self.clauses(&clauses, At { category: Category::Signature, span: MirSpan::Unknown }, true);
        self.scratch.clauses.give(clauses);
    }

    /// What normalizing the signature needs, as rustc normalizes the type of each parameter
    /// and the return type: the where clauses of the impl that a projection there is of,
    /// `<&'a () as Visitor<'d>>::Value` with only `impl<'a, 'd: 'a> Visitor<'d> for &'a ()`
    /// that `'d: 'a`. And what computing its implied bounds needs, as rustc's
    /// `compute_implied_outlives_bounds_inner` normalizes what the well-formedness of each
    /// type asks, the where clauses of a struct or enum it names: `Foo<(&'a str,)>` of
    /// `struct Foo<T: Trait> where T::Assoc: Clone` with only `impl<X: 'static> Trait for (X,)`
    /// needs `'a: 'static`. Their trait bounds themselves it does not prove.
    fn signature_projections(&mut self) {
        if self.ctx.closure_local.is_some() || self.ctx.inline_const {
            return;
        }
        let interner = self.interner();
        let mut trait_refs = Vec::new_in(self.ctx.frame);
        let mut tys = mem::collect(
            self.ctx.frame,
            self.body
                .param_locals
                .iter()
                .chain((!self.returns_opaque).then_some(&super::return_slot()))
                .map(|local| self.regions.local_tys[local.into_raw().into_u32() as usize]),
        );
        use rustc_type_ir::inherent::GenericArg as _;
        let mut seen = mem::set(self.ctx.frame);
        while let Some(ty) = tys.pop() {
            // A where clause can name a bigger type than its ADT, with no end.
            if seen.len() > 256 || !seen.insert(ty) {
                continue;
            }
            let db = self.ctx.db;
            _ = each_ty(ty, |it| {
                match it.kind() {
                    TyKind::Alias(_, alias)
                        if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. }) =>
                    {
                        let trait_ref = alias.trait_ref(interner);
                        if !trait_refs.contains(&trait_ref) {
                            trait_refs.push(trait_ref);
                        }
                    }
                    TyKind::Adt(def, args) => {
                        let clauses = GenericPredicates::query_all(db, def.def_id().into())
                            .iter_instantiated(interner, args.as_slice())
                            .map(|it| it.skip_norm_wip());
                        for clause in clauses {
                            match clause.kind().skip_binder() {
                                ClauseKind::Trait(predicate) => tys.extend(
                                    predicate.trait_ref.args.iter().filter_map(|arg| arg.as_type()),
                                ),
                                ClauseKind::Projection(predicate) => {
                                    tys.extend(
                                        predicate
                                            .projection_term
                                            .args
                                            .iter()
                                            .filter_map(|arg| arg.as_type()),
                                    );
                                    tys.extend(predicate.term.as_type());
                                }
                                ClauseKind::TypeOutlives(OutlivesClause(ty, _)) => tys.push(ty),
                                _ => (),
                            }
                        }
                    }
                    _ => (),
                }
                std::ops::ControlFlow::Continue(())
            });
        }
        for trait_ref in trait_refs {
            self.prove(trait_ref, At { category: Category::Signature, span: MirSpan::Unknown });
        }
        // Computing the implied bounds of a returned `impl Trait` reveals, in its defining
        // body, the type it hides, whose well-formedness the solver then proves, at the
        // signature: `-> impl Sized` hiding `Outlives<'static, <T as Trait>::Assoc>` needs
        // `<T as Trait>::Assoc: 'static`. What the type around it asks (`&'t impl Trait` that
        // it outlives `'t`) is an implied bound, assumed.
        if self.returns_opaque
            && let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = self.ctx.owner
        {
            let slot = self.regions.local_tys[super::return_slot().into_raw().into_u32() as usize];
            let declared = self
                .ctx
                .db
                .callable_item_signature(CallableDefId::FunctionId(function))
                .instantiate_identity()
                .skip_norm_wip()
                .skip_binder()
                .output();
            let mut hidden = Vec::new_in(self.ctx.frame);
            hidden_types(declared, slot, &mut hidden);
            for ty in hidden {
                use rustc_type_ir::TypeVisitableExt as _;
                if ty.references_error() || ty.has_non_region_infer() {
                    continue;
                }
                let at = At { category: Category::Signature, span: MirSpan::Unknown };
                self.well_formed(ty, at, 0);
            }
        }
    }

    /// What the signature says of which universal regions outlive which: its where clauses
    /// and what its parameters' types imply, as `&'a T` that `T: 'a`.
    fn known_relations(&mut self) {
        let generic_def: Option<GenericDefId> = match self.body.owner {
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => Some(it.into()),
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => Some(it.into()),
            InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(it)) => Some(it.into()),
            _ => None,
        };
        let mut pairs = Vec::new_in(self.ctx.frame);
        if let Some(def) = generic_def
            && self.ctx.closure_local.is_none()
        {
            for clause in GenericPredicates::query_all(self.ctx.db, def).skip_binder() {
                match clause.kind().skip_binder() {
                    ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {
                        if let (Some(longer), Some(shorter)) = (
                            self.signature_region(longer, rustc_type_ir::INNERMOST),
                            self.signature_region(shorter, rustc_type_ir::INNERMOST),
                        ) && let (Some(longer), Some(shorter)) = (rv_of(longer), rv_of(shorter))
                        {
                            pairs.push((longer, shorter));
                        }
                    }
                    ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {
                        if let Some(region) =
                            self.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
                        {
                            let ty = self.signature_ty(ty);
                            self.regions.type_bounds.push((ty, region));
                        }
                    }
                    _ => (),
                }
            }
        }
        // The bounds the where clauses imply too, as of supertraits: `T: Trait` where
        // `trait Trait: 'static`.
        if self.ctx.closure_local.is_none() {
            let clauses = mem::collect(self.ctx.frame, self.ctx.env.caller_bounds());
            for clause in clauses {
                match clause.kind().skip_binder() {
                    ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {
                        if let Some(region) =
                            self.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
                        {
                            let ty = self.signature_ty(ty);
                            self.regions.type_bounds.push((ty, region));
                        }
                    }
                    ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {
                        if let (Some(longer), Some(shorter)) = (
                            self.signature_region(longer, rustc_type_ir::INNERMOST),
                            self.signature_region(shorter, rustc_type_ir::INNERMOST),
                        ) && let (Some(longer), Some(shorter)) = (rv_of(longer), rv_of(shorter))
                        {
                            pairs.push((longer, shorter));
                        }
                    }
                    _ => (),
                }
            }
        }
        let signature = mem::collect(
            self.ctx.frame,
            self.body
                .param_locals
                .iter()
                .chain((!self.returns_opaque).then_some(&super::return_slot()))
                .map(|&it| self.regions.local_tys[it.into_raw().into_u32() as usize]),
        );
        let mut signature = signature;
        self.impl_header_types(&mut signature);
        let mut seen = mem::set(self.ctx.frame);
        for ty in signature {
            self.implied_bounds(ty, &mut pairs, &mut seen, 0, false);
        }
        let count = self.regions.universals.len();
        let mut known = matrix(self.ctx.frame, count, count);
        let static_index = self.regions.universal(self.regions.static_rv).unwrap();
        for index in 0..count {
            known.insert(index, index);
            // Every universal region outlives the body.
            known.insert(index, 1);
        }
        for shorter in 0..count {
            known.insert(static_index, shorter);
        }
        for (longer, shorter) in pairs {
            if let (Some(longer), Some(shorter)) =
                (self.regions.universal(longer), self.regions.universal(shorter))
            {
                known.insert(longer, shorter);
            }
        }
        // What is known is closed under transitivity: Warshall's, a row union a word at a
        // time, `count³ / 64` word operations.
        for middle in 0..count {
            for longer in 0..count {
                if known.contains(longer, middle) {
                    known.union_rows(longer, middle);
                }
            }
        }
        self.regions.known = known;
    }

    /// The self type of the impl `function` is in with its anonymous lifetimes universal
    /// regions, when it has any.
    /// The region of the one `&Self` in the type of a method's `self` (`self: Box<&Self>`,
    /// `Pin<&mut Self>`), which rustc's lifetime elision gives the elided lifetimes of the
    /// return type.
    fn self_reference(&self, function: hir_def::FunctionId, ty: Ty<'db>) -> Option<Region<'db>> {
        let ItemContainerId::ImplId(impl_) = function.lookup(self.ctx.db).container else {
            return None;
        };
        let interner = self.interner();
        let erase = |ty: Ty<'db>| {
            let ty = rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner));
            super::liveness::normalize_erased(self.ctx, ty).unwrap_or(ty)
        };
        let self_ty = erase(self.ctx.db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip());
        let mut found = Vec::new_in(self.ctx.frame);
        let mut stack = Vec::new_in(self.ctx.frame);
        stack.push(ty);
        while let Some(ty) = stack.pop() {
            match ty.kind() {
                TyKind::Ref(region, inner, _) if erase(inner) == self_ty => found.push(region),
                TyKind::Ref(_, inner, _) => stack.push(inner),
                TyKind::Adt(_, args) => {
                    stack.extend(args.iter().filter_map(|it| match it.kind() {
                        GenericArgKind::Type(ty) => Some(ty),
                        _ => None,
                    }))
                }
                _ => (),
            }
        }
        found.dedup();
        match found.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    fn impl_self_of(&mut self, function: hir_def::FunctionId) -> Option<(Ty<'db>, Ty<'db>)> {
        let ItemContainerId::ImplId(impl_) = function.lookup(self.ctx.db).container else {
            return None;
        };
        let raw = self.ctx.db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let mut anonymous = 0;
        let named =
            rustc_type_ir::fold_regions(self.interner(), raw, |region, _| match region.kind() {
                // In a closure, the impl's anonymous lifetimes are its creator's.
                RegionKind::ReError(_) if self.nested() => {
                    let region = self.new_universal(UniversalKind::Creator);
                    self.regions
                        .creator_positions
                        .insert(self.regions.universals.len() - 1, IMPL_ANON + anonymous);
                    anonymous += 1;
                    region
                }
                RegionKind::ReError(_) => {
                    anonymous += 1;
                    self.new_universal(UniversalKind::AnonParam)
                }
                _ => region,
            });
        if anonymous == 0 {
            return None;
        }
        self.impl_self = Some((raw, named));
        self.impl_self
    }

    /// The universal region of the anonymous lifetime `index` of the impl the body's method
    /// is in, `impl Foo<'_>`.
    fn impl_anon_region(&mut self, index: usize) -> Option<Region<'db>> {
        if self.impl_self.is_none()
            && let GenericDefId::FunctionId(function) = self.ctx.owner.generic_def(self.ctx.db)
        {
            self.impl_self_of(function);
        }
        let (raw, named) = self.impl_self?;
        let mut anonymous = Vec::new_in(self.ctx.frame);
        let mut errors = Vec::new_in(self.ctx.frame);
        rustc_type_ir::fold_regions(self.interner(), raw, |region, _| {
            errors.push(matches!(region.kind(), RegionKind::ReError(_)));
            region
        });
        let mut position = 0;
        rustc_type_ir::fold_regions(self.interner(), named, |region, _| {
            if errors.get(position).copied().unwrap_or(false) {
                anonymous.push(region);
            }
            position += 1;
            region
        });
        anonymous.get(index).copied()
    }

    /// The self type and trait arguments of the impl a method is in, whose well-formedness the
    /// method assumes as it does its parameters'.
    fn impl_header_types(&mut self, out: &mut FVec<'a, Ty<'db>>) {
        if self.ctx.closure_local.is_some() {
            return;
        }
        let container = match self.body.owner {
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => {
                it.lookup(self.ctx.db).container
            }
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => {
                it.lookup(self.ctx.db).container
            }
            _ => return,
        };
        let ItemContainerId::ImplId(impl_) = container else {
            return;
        };
        let db = self.ctx.db;
        let raw = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let start = out.len();
        out.push(match self.impl_self {
            Some((from, to)) if from == raw => to,
            _ => raw,
        });
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            out.extend(trait_ref.args.iter().skip(1).filter_map(|arg| match arg.kind() {
                GenericArgKind::Type(it) => Some(it),
                _ => None,
            }));
        }
        for at in start..out.len() {
            let ty = out[at];
            out[at] = rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
            });
        }
    }

    fn implied_bounds(
        &mut self,
        ty: Ty<'db>,
        pairs: &mut FVec<'a, (Rv, Rv)>,
        seen: &mut FSet<'a, (Ty<'db>, bool)>,
        depth: usize,
        in_field: bool,
    ) {
        if depth > 4 || !seen.insert((ty, in_field)) {
            return;
        }
        match ty.kind() {
            TyKind::Ref(region, inner, _) => {
                if let Some(shorter) = self.region_rv(region) {
                    let referent = match inner.kind() {
                        TyKind::Alias(_, _) => self.normalize(inner).unwrap_or(inner),
                        _ => inner,
                    };
                    let mut components = self.scratch.rvs.take();
                    let mut params = self.scratch.tys.take();
                    self.implied_components(referent, &mut components, &mut params);
                    pairs.extend(components.iter().map(|&longer| (longer, shorter)));
                    self.regions.type_bounds.extend(params.iter().map(|&it| (it, shorter)));
                    self.scratch.rvs.give(components);
                    self.scratch.tys.give(params);
                }
                self.implied_bounds(inner, pairs, seen, depth + 1, in_field);
            }
            TyKind::Adt(def, args) => {
                for arg in args.iter() {
                    if let GenericArgKind::Type(arg) = arg.kind() {
                        self.implied_bounds(arg, pairs, seen, depth + 1, in_field);
                    }
                }
                // What the fields imply is what rustc infers of the type's parameters.
                let adt = def.def_id();
                for variant in adt.variants(self.ctx.db) {
                    for (_, field) in self.ctx.db.field_types(variant).iter() {
                        let field = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                        self.implied_bounds(field, pairs, seen, depth + 1, true);
                    }
                }
                // And the bounds the type declares, which its well-formedness needs, with what
                // rustc infers its fields need of its lifetimes however deep they are.
                let mut clauses = self.scratch.clauses.take();
                clauses.extend(
                    GenericPredicates::query_all(self.ctx.db, adt.into())
                        .iter_instantiated(self.interner(), args.as_slice())
                        .map(|it| it.skip_norm_wip()),
                );
                if args.regions().next().is_some() {
                    use rustc_type_ir::Upcast as _;
                    let interner = self.interner();
                    for predicate in self.inferred.of(self.ctx.db, adt) {
                        let OutlivesClause(arg, region) =
                            rustc_type_ir::EarlyBinder::bind(interner, predicate)
                                .instantiate(interner, args)
                                .skip_norm_wip();
                        clauses.push(match arg.kind() {
                            GenericArgKind::Type(ty) => {
                                OutlivesClause(ty, region).upcast(interner)
                            }
                            GenericArgKind::Lifetime(longer) => {
                                OutlivesClause(longer, region).upcast(interner)
                            }
                            GenericArgKind::Const(_) => continue,
                        });
                    }
                }
                self.implied_by_clauses(&clauses, pairs);
                self.scratch.clauses.give(clauses);
            }
            TyKind::Tuple(tys) => {
                for it in tys.iter() {
                    self.implied_bounds(it, pairs, seen, depth + 1, in_field);
                }
            }
            TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
                self.implied_bounds(inner, pairs, seen, depth + 1, in_field)
            }
            // `dyn Any + 'r` is well-formed only where `'r` outlives what the trait or one of
            // its supertraits requires of `Self`: `trait Any: 'static`. Not in a field, of which
            // rustc's inferred outlives leave out what mentions `Self`.
            // And what the types in its arguments imply, as rustc's well-formedness walks into
            // them: `&dyn Fn() -> &'a G<'db>` that `'db: 'a`.
            TyKind::Dynamic(predicates, region) => {
                for predicate in predicates.iter() {
                    let predicate = predicate.skip_binder();
                    let (args, term) = match predicate {
                        rustc_type_ir::ExistentialPredicate::Trait(it) => (it.args, None),
                        rustc_type_ir::ExistentialPredicate::Projection(it) => {
                            (it.args, it.term.as_type())
                        }
                        rustc_type_ir::ExistentialPredicate::AutoTrait(_) => continue,
                    };
                    for it in args.types().chain(term) {
                        if !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&it) {
                            self.implied_bounds(it, pairs, seen, depth + 1, in_field);
                        }
                    }
                }
                if in_field {
                    return;
                }
                let Some(shorter) = rv_of(region) else { return };
                for predicate in predicates.iter() {
                    let trait_ = match predicate.skip_binder() {
                        rustc_type_ir::ExistentialPredicate::Trait(it) => it.def_id.0,
                        rustc_type_ir::ExistentialPredicate::AutoTrait(it) => it.0,
                        rustc_type_ir::ExistentialPredicate::Projection(_) => continue,
                    };
                    // Its supertraits' too, as rustc elaborates them: a `dyn Db` through a
                    // supertrait bounded by `'static`.
                    if static_trait(self.ctx.db, trait_) {
                        pairs.push((shorter, self.regions.static_rv));
                    }
                }
            }
            // A projection implies what its arguments' well-formedness does as well as what its
            // normalized type does: `<&'a &'b () as Trait>::Type` that `'b: 'a`.
            // And the bounds of its trait, which its trait reference's well-formedness needs:
            // `<T as Trait<A>>::Assoc` that `A: 'static` where `trait Trait<A: 'static>`.
            TyKind::Alias(_, alias) => {
                if let rustc_type_ir::AliasTyKind::Projection { def_id } = alias.kind {
                    for it in alias.args.types() {
                        self.implied_bounds(it, pairs, seen, depth + 1, in_field);
                    }
                    let mut clauses = self.scratch.clauses.take();
                    clauses.extend(
                        GenericPredicates::query_all(self.ctx.db, def_id.0.into())
                            .iter_instantiated(self.interner(), alias.args.as_slice())
                            .map(|it| it.skip_norm_wip())
                            .filter(|it| {
                                !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(it)
                            }),
                    );
                    self.implied_by_clauses(&clauses, pairs);
                    self.scratch.clauses.give(clauses);
                }
                if let Some(normalized) = self.normalize(ty)
                    && normalized != ty
                {
                    self.implied_bounds(normalized, pairs, seen, depth + 1, in_field);
                }
            }
            _ => (),
        }
    }

    fn normalize(&mut self, ty: Ty<'db>) -> Option<Ty<'db>> {
        normalize(self.ctx, ty)
    }

    /// `ty` normalized, with what the solver needs of the regions already there for it as
    /// constraints at `at`: `<T as Fun>::F<'x>` with `T: Fun<F<'a> = [u8]>` is `[u8]` only
    /// where `'x` is `'a`.
    fn normalize_at(&mut self, ty: Ty<'db>, at: At) -> Option<Ty<'db>> {
        use hir_ty::next_solver::infer::region_constraints::Constraint;
        let first_new = self.ctx.infcx.num_region_vars();
        let mut constraints = self.scratch.constraints.take();
        let Some(normalized) = normalize_full(self.ctx, ty, false, &mut constraints) else {
            self.scratch.constraints.give(constraints);
            return None;
        };
        let rv = |this: &mut Self, region: Region<'db>| match region.kind() {
            RegionKind::ReVar(vid) if vid.index() < first_new => Some(vid.as_usize()),
            RegionKind::ReStatic => Some(this.regions.static_rv),
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) => {
                this.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
            }
            // Of a `for<'a>` instantiated to relate types: `<&'a () as Trait>::Ty` with only
            // `impl Trait for &'static ()` needs the placeholder to be `'static`, an error of
            // rustc's type op normalizing it, "not general enough", not of the relating.
            RegionKind::RePlaceholder(_) => {
                let rv = rv_of(this.placeholder(region))?;
                let universal = this.regions.universal(rv)?;
                this.regions.normalized_placeholders.insert(universal);
                Some(rv)
            }
            _ => None,
        };
        let interner = self.interner();
        // Only what a where clause or `'static` gives: the solver relates the variables it was
        // given among themselves more tightly than rustc's relating of the normalized types.
        for constraint in constraints.drain(..) {
            // `a <= b`: `b` outlives `a`.
            let (shorter, longer) = match constraint {
                Constraint::VarSubVar(..) => continue,
                Constraint::RegSubVar(a, b) => (a, Region::new_var(interner, b)),
                Constraint::VarSubReg(a, b) => (Region::new_var(interner, a), b),
                Constraint::RegSubReg(a, b) => (a, b),
            };
            if let (Some(shorter), Some(longer)) = (rv(self, shorter), rv(self, longer)) {
                self.outlives(longer, shorter, at);
            }
        }
        self.scratch.constraints.give(constraints);
        Some(normalized)
    }

    /// The regions and the parameters and projections `ty` outlives what it outlives, with
    /// those of what a projection in it normalizes to: `&S<Self::C>` in an impl with
    /// `type C = C` implies that `C` outlives the reference.
    fn implied_components(
        &mut self,
        ty: Ty<'db>,
        components: &mut FVec<'a, Rv>,
        params: &mut FVec<'a, Ty<'db>>,
    ) {
        outlives_components(ty, components, params);
        alias_components(ty, params);
        let mut aliases = self.scratch.tys.take();
        aliases.extend(params.iter().filter(|it| matches!(it.kind(), TyKind::Alias(_, _))).copied());
        for &alias in aliases.iter() {
            if let Some(normalized) = self.normalize(alias)
                && normalized != alias
            {
                outlives_components(normalized, components, params);
                alias_components(normalized, params);
            }
        }
        self.scratch.tys.give(aliases);
    }

    /// What outlives clauses a well-formed type needs imply.
    fn implied_by_clauses(&mut self, clauses: &[Clause<'db>], pairs: &mut FVec<'a, (Rv, Rv)>) {
        for &clause in clauses {
            match clause.kind().skip_binder() {
                ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {
                    if let (Some(longer), Some(shorter)) =
                        (self.region_rv(longer), self.region_rv(shorter))
                    {
                        pairs.push((longer, shorter));
                    }
                }
                ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {
                    if let Some(shorter) = self.region_rv(region) {
                        let mut components = self.scratch.rvs.take();
                        let mut params = self.scratch.tys.take();
                        self.implied_components(ty, &mut components, &mut params);
                        pairs.extend(components.iter().map(|&it| (it, shorter)));
                        self.regions
                            .type_bounds
                            .extend(params.iter().copied().chain([ty]).map(|it| (it, shorter)));
                        self.scratch.rvs.give(components);
                        self.scratch.tys.give(params);
                    }
                }
                _ => (),
            }
        }
    }

    fn outlives(&mut self, sup: Rv, sub: Rv, at: At) {
        if sup != sub {
            let location = self.location;
            let category = match at.span {
                MirSpan::ExprId(expr)
                    if at.category != Category::Return && self.synthesized.contains(expr.into_raw().into_u32() as usize) =>
                {
                    Category::Boring
                }
                _ => at.category,
            };
            self.regions.outlives.push(Outlives {
                sup,
                sub,
                location,
                category,
                span: at.span,
                from_closure: false,
            });
        }
    }

    /// Where an `async fn` makes the future of its body (`coroutine`), that the future's
    /// output is the declared return type: rustc proves the `Future<Output = ..>` bound of the
    /// `impl Future` the function returns of it. `typed` is the output as type inference
    /// gives it, with an elided lifetime an error, `output` the same renumbered.
    fn async_fn_output(
        &mut self,
        coroutine: hir_def::hir::ExprId,
        typed: Ty<'db>,
        output: Ty<'db>,
        span: MirSpan,
    ) {
        use hir_def::hir::{ClosureKind, CoroutineSource, Expr};
        if self.ctx.closure_local.is_some()
            || !matches!(
                self.ctx.store.expr(coroutine),
                Expr::Closure {
                    closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Fn, .. },
                    ..
                }
            )
        {
            return;
        }
        let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = self.body.owner
        else {
            return;
        };
        let written = FunctionSignature::of(self.ctx.db, function);
        let mut defaults = ObjectDefaults {
            db: self.ctx.db,
            interner: self.interner(),
            keep_static: writes_static_object(&written.store),
            erase: false,
        };
        let typed = rustc_type_ir::TypeFoldable::fold_with(typed, &mut defaults);
        let typed = match self.async_declared_output(function) {
            Some(declared) => self.unnamed_hidden(typed, declared),
            None => typed,
        };
        let elided = self.elided_output;
        let declared = rustc_type_ir::fold_regions(self.interner(), typed, |region, depth| {
            match region.kind() {
                RegionKind::ReError(_) => elided.unwrap_or_else(|| self.fresh_elided()),
                RegionKind::ReVar(_) => self.fresh(),
                _ => self.signature_region(region, depth).unwrap_or_else(|| self.fresh()),
            }
        });
        let mut unsuggested = Vec::new_in(self.ctx.frame);
        regions_of(declared, &mut unsuggested);
        for rv in unsuggested {
            self.regions.unsuggested.extend(self.regions.universal(rv));
        }
        let at = At { category: Category::Boring, span };
        self.relate(output, declared, Variance::Invariant, at);
        self.async_output_members(function, output, span);
    }

    /// The `Output` of the `impl Future` an `async fn` returns, as its signature declares it.
    fn async_declared_output(&self, function: hir_def::FunctionId) -> Option<Ty<'db>> {
        use rustc_type_ir::Interner as _;
        let interner = self.interner();
        let sig = self
            .ctx
            .db
            .callable_item_signature(CallableDefId::FunctionId(function))
            .instantiate_identity()
            .skip_norm_wip()
            .skip_binder();
        let TyKind::Alias(_, future) = sig.output().kind() else { return None };
        let rustc_type_ir::AliasTyKind::Opaque { def_id } = future.kind else { return None };
        interner
            .item_bounds(def_id.into())
            .iter_instantiated(interner, future.args.as_slice())
            .find_map(|clause| match clause.kind().skip_binder() {
                ClauseKind::Projection(projection) => projection.term.as_type(),
                _ => None,
            })
    }

    /// `typed` with fresh regions where `declared` has an `impl Trait`: the lifetimes of a
    /// hidden type are rustc's to choose by member constraints, not the ones type inference
    /// unified, so `I { index: self, req }` of `-> Result<impl Iterator + 'b>` with `'a: 'b`
    /// can be `I<'b>`.
    fn unnamed_hidden(&self, typed: Ty<'db>, declared: Ty<'db>) -> Ty<'db> {
        let interner = self.interner();
        let frame = self.ctx.frame;
        let zip = |typed: GenericArgs<'db>, declared: GenericArgs<'db>| {
            GenericArgs::new_from_slice(&mem::collect(
                frame,
                typed.iter().zip(declared.iter()).map(|(it, other)| match (it.ty(), other.ty()) {
                    (Some(it), Some(other)) => self.unnamed_hidden(it, other).into(),
                    _ => it,
                }),
            ))
        };
        match (typed.kind(), declared.kind()) {
            (kind, TyKind::Alias(_, alias))
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. })
                    && !matches!(kind, TyKind::Alias(_, _)) =>
            {
                rustc_type_ir::fold_regions(interner, typed, |_, _| self.fresh())
            }
            (TyKind::Adt(def, args), TyKind::Adt(other, declared))
                if def == other && args.len() == declared.len() =>
            {
                Ty::new(interner, TyKind::Adt(def, zip(args, declared)))
            }
            (TyKind::Tuple(tys), TyKind::Tuple(declared)) if tys.len() == declared.len() => {
                let tys = mem::collect(
                    frame,
                    tys.iter().zip(declared.iter()).map(|(it, other)| self.unnamed_hidden(it, other)),
                );
                Ty::new_tup(interner, &tys)
            }
            (TyKind::Ref(region, ty, mutability), TyKind::Ref(_, other, _)) => {
                Ty::new_ref(interner, region, self.unnamed_hidden(ty, other), mutability)
            }
            _ => typed,
        }
    }

    /// rustc's member constraints for an `impl Trait` an `async fn` declares it returns: the
    /// regions of the future's `output` are of its hidden type.
    fn async_output_members(
        &mut self,
        function: hir_def::FunctionId,
        output: Ty<'db>,
        span: MirSpan,
    ) {
        use rustc_type_ir::Interner as _;
        let interner = self.interner();
        let Some(inner) = self.async_declared_output(function) else { return };
        let TyKind::Alias(_, alias) = inner.kind() else { return };
        let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind else { return };
        let Some(captured) = opaque_captures(self.ctx.frame, self.ctx.db, def_id, alias.args.len()) else {
            return;
        };
        // What its bounds say it outlives, its hidden type must, as for the `impl Trait` a
        // function returns.
        let binder = rustc_type_ir::INNERMOST.shifted_in(1);
        let mut outlived = Vec::new_in(self.ctx.frame);
        for clause in interner
            .item_bounds(def_id.into())
            .iter_instantiated(interner, alias.args.as_slice())
        {
            match clause.kind().skip_binder() {
                ClauseKind::TypeOutlives(OutlivesClause(bounded, region))
                    if let TyKind::Alias(_, bounded) = bounded.kind()
                        && bounded.kind == alias.kind =>
                {
                    // Under its clause's binder too.
                    match self.signature_region(region, binder.shifted_in(1)).and_then(rv_of) {
                        Some(rv) => outlived.push(rv),
                        None => return,
                    }
                }
                ClauseKind::Projection(projection)
                    if match projection.term.as_type() {
                        Some(ty) => names_captured_lifetime(self.ctx.frame, self.ctx.db, ty),
                        None => rustc_type_ir::TypeVisitableExt::has_free_regions(&projection.term),
                    } =>
                {
                    return;
                }
                _ => (),
            }
        }
        let mut choices = Vec::new_in(self.ctx.frame);
        choices.push(self.regions.static_rv);
        for (arg, captured) in alias.args.iter().zip(captured) {
            if let GenericArgKind::Lifetime(region) = arg.kind()
                && captured
            {
                let Some(rv) = self.signature_region(region, binder).and_then(rv_of) else {
                    return;
                };
                choices.push(rv);
            }
        }
        let mut bounded = Vec::new_in(self.ctx.frame);
        member_regions(self.ctx.frame, self.ctx.db, output, false, &mut bounded);
        bounded.retain(|&it| it != self.regions.static_rv);
        radix::sort(&mut bounded);
        bounded.dedup();
        for &member in &bounded {
            for &sub in &outlived {
                self.regions.outlives.push(Outlives {
                    sup: member,
                    sub,
                    location: None,
                    category: Category::Return,
                    span,
                    from_closure: false,
                });
            }
        }
        let mut members = Vec::new_in(self.ctx.frame);
        member_regions(self.ctx.frame, self.ctx.db, output, true, &mut members);
        members.retain(|&it| it != self.regions.static_rv);
        radix::sort(&mut members);
        members.dedup();
        self.regions.members = members;
        self.regions.choices = choices;
    }

    /// rustc's `prove_closure_bounds` where the closure `ty` is made: its enclosing item's
    /// arguments are this body's parameters, and what the closure needs of this body's
    /// regions holds, blamed on what in the closure needs it.
    fn closure_requirements(
        &mut self,
        closure: hir_def::hir::ExprId,
        ty: Ty<'db>,
        parents: FVec<'_, (u32, Region<'db>)>,
        span: MirSpan,
    ) {
        let at = At { category: Category::Boring, span };
        for (index, region) in parents {
            let own = self.early_region(index);
            self.relate_regions(region, own, Variance::Invariant, at);
        }
        let mut regions = self.scratch.rvs.take();
        regions_of(ty, &mut regions);
        self.apply_requirements(closure, &regions);
        self.scratch.rvs.give(regions);
    }

    /// What the closure or `const { .. }` block `nested` needs of this body's regions, the
    /// regions of its type at their positions being `regions`.
    fn apply_requirements(&mut self, nested: hir_def::hir::ExprId, regions: &[Rv]) {
        let Some((count, requirements)) = self.requirements.get(&nested) else { return };
        if regions.len() != *count {
            return;
        }
        let at_position = |this: &mut Self, position: usize| match position {
            _ if position >= EARLY_PARENT => {
                rv_of(this.early_region((position - EARLY_PARENT) as u32))
            }
            _ if position >= LATE_PARENT => {
                rv_of(this.parent_late_region((position - LATE_PARENT) as u32))
            }
            _ if position >= IMPL_ANON => {
                this.impl_anon_region(position - IMPL_ANON).and_then(rv_of)
            }
            _ => regions.get(position).copied(),
        };
        for requirement in requirements {
            let sub = match requirement.shorter {
                Some(it) => match at_position(self, it) {
                    Some(it) => it,
                    None => continue,
                },
                None => self.regions.static_rv,
            };
            if let Some(ty) = requirement.subject {
                self.regions.type_tests.push(TypeTest {
                    ty,
                    lower: sub,
                    span: requirement.span,
                    forall: false,
                    signature: false,
                });
                continue;
            }
            let Some(sup) = at_position(self, requirement.longer) else { continue };
            if sup != sub {
                self.regions.outlives.push(Outlives {
                    sup,
                    sub,
                    location: self.location,
                    category: requirement.category,
                    span: requirement.span,
                    from_closure: true,
                });
            }
        }
    }

    fn relate_regions(&mut self, a: Region<'db>, b: Region<'db>, variance: Variance, at: At) {
        match variance {
            Variance::Covariant => self.region_outlives(a, b, at),
            Variance::Contravariant => self.region_outlives(b, a, at),
            Variance::Invariant => {
                self.region_outlives(a, b, at);
                self.region_outlives(b, a, at);
            }
            Variance::Bivariant => (),
        }
    }

    /// That `longer` outlives `shorter`. A region of a universe that cannot name the
    /// placeholder `shorter` outlives it only by outliving `'static`, as rustc's
    /// `add_incompatible_universe`.
    fn region_outlives(&mut self, longer: Region<'db>, shorter: Region<'db>, at: At) {
        let shorter = match shorter.is_placeholder()
            && longer != shorter
            && !matches!(longer.kind(), RegionKind::ReBound(..) | RegionKind::ReError(_))
            && self.ctx.infcx.universe_of_region(longer)
                < self.ctx.infcx.universe_of_region(shorter)
        {
            true => Region::new_static(self.interner()),
            false => shorter,
        };
        let static_rv = self.regions.static_rv;
        let mut rv = |this: &mut Self, region: Region<'db>| match region.kind() {
            RegionKind::ReStatic => Some(static_rv),
            RegionKind::RePlaceholder(_) => rv_of(this.placeholder(region)),
            _ => rv_of(region),
        };
        let (Some(longer), Some(shorter)) = (rv(self, longer), rv(self, shorter)) else { return };
        self.outlives(longer, shorter, at);
    }

    /// The inputs and output of a signature's `for<'a>` instantiated, as rustc's
    /// `TypeRelating::binders`: for placeholders of a new universe on the side that must be
    /// the more general, for variables of that universe, which can name them, on the other.
    fn instantiate_sig(
        &mut self,
        sig: rustc_type_ir::Binder<
            hir_ty::next_solver::DbInterner<'db>,
            rustc_type_ir::FnSigTys<hir_ty::next_solver::DbInterner<'db>>,
        >,
        universe: rustc_type_ir::UniverseIndex,
        placeholders: bool,
    ) -> hir_ty::next_solver::Tys<'db> {
        let interner = self.interner();
        let infcx = &self.ctx.infcx;
        let mut existentials: FVec<'_, (rustc_type_ir::BoundVar, Region<'db>)> =
            Vec::new_in(self.ctx.frame);
        rustc_type_ir::fold_regions(
            interner,
            sig.skip_binder().inputs_and_output,
            |region, depth| match region.kind() {
                RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(binder), bound)
                    if binder == depth =>
                {
                    if placeholders {
                        return Region::new_placeholder(
                            interner,
                            hir_ty::next_solver::PlaceholderRegion::new(universe, bound),
                        );
                    }
                    if let Some(&(_, it)) = existentials.iter().find(|it| it.0 == bound.var) {
                        return it;
                    }
                    let it = infcx.next_region_var_in_universe(universe, hir_ty::Span::Dummy);
                    existentials.push((bound.var, it));
                    it
                }
                _ => region,
            },
        )
    }

    /// Two signatures' inputs and output, related as the types of `fn` pointers are.
    fn relate_sig_tys(
        &mut self,
        xs: hir_ty::next_solver::Tys<'db>,
        ys: hir_ty::next_solver::Tys<'db>,
        variance: Variance,
        at: At,
    ) {
        let count = xs.len().min(ys.len());
        for (index, (x, y)) in xs.iter().zip(ys.iter()).enumerate() {
            let inner =
                if index + 1 == count { variance } else { variance.xform(Variance::Contravariant) };
            self.relate(x, y, inner, at);
        }
    }

    /// That `a` is a subtype of `b` under `variance`.
    fn relate(&mut self, a: Ty<'db>, b: Ty<'db>, variance: Variance, at: At) {
        if a == b || variance == Variance::Bivariant {
            return;
        }
        match (a.kind(), b.kind()) {
            (TyKind::Ref(ra, ta, ma), TyKind::Ref(rb, tb, _)) => {
                self.relate_regions(ra, rb, variance, at);
                let inner = match ma {
                    rustc_ast_ir::Mutability::Not => variance,
                    rustc_ast_ir::Mutability::Mut => variance.xform(Variance::Invariant),
                };
                self.relate(ta, tb, inner, at);
            }
            // rust-analyzer lowers coercing `&T` to `*const T` as a borrow, where rustc's is
            // `&raw const *x` of the raw pointer's type.
            (TyKind::Ref(_, ta, _), TyKind::RawPtr(tb, ma))
            | (TyKind::RawPtr(ta, ma), TyKind::RawPtr(tb, _)) => {
                let inner = match ma {
                    rustc_ast_ir::Mutability::Not => variance,
                    rustc_ast_ir::Mutability::Mut => variance.xform(Variance::Invariant),
                };
                self.relate(ta, tb, inner, at);
            }
            (TyKind::Adt(da, aa), TyKind::Adt(db, ab)) if da == db => {
                let variances = self.ctx.db.variances_of(GenericDefId::AdtId(da.def_id()));
                self.relate_args(aa, ab, Some(variances.as_slice()), variance, at);
            }
            (TyKind::Tuple(xs), TyKind::Tuple(ys)) => {
                for (x, y) in xs.iter().zip(ys.iter()) {
                    self.relate(x, y, variance, at);
                }
            }
            (TyKind::Array(x, _), TyKind::Array(y, _)) | (TyKind::Slice(x), TyKind::Slice(y)) => {
                self.relate(x, y, variance, at)
            }
            // Higher-ranked subtyping, as rustc's `TypeRelating::binders`: `a <: b` holds
            // when `b`'s `for<'a>` instantiated with placeholders is a supertype of `a`'s
            // instantiated with variables; invariant, both ways, each still invariant. A region
            // that cannot name a placeholder outlives it by outliving `'static`, so a closure
            // parameter `x: fn(&u32)` of a type `fn(&'1 u32)` needs `'1: 'static`.
            (TyKind::FnPtr(sa, _), TyKind::FnPtr(sb, _)) => {
                if sa.bound_vars().is_empty() && sb.bound_vars().is_empty() {
                    let (xs, ys) =
                        (sa.skip_binder().inputs_and_output, sb.skip_binder().inputs_and_output);
                    self.relate_sig_tys(xs, ys, variance, at);
                    return;
                }
                let mut pass = |this: &mut Self, general_is_b: bool| {
                    let universe = this.ctx.infcx.create_next_universe();
                    this.relate_universes.insert(universe);
                    let xs = this.instantiate_sig(sa, universe, !general_is_b);
                    let ys = this.instantiate_sig(sb, universe, general_is_b);
                    this.relate_sig_tys(xs, ys, variance, at);
                };
                match variance {
                    Variance::Covariant => pass(self, true),
                    Variance::Contravariant => pass(self, false),
                    Variance::Invariant => {
                        pass(self, true);
                        pass(self, false);
                    }
                    Variance::Bivariant => (),
                }
            }
            (TyKind::Dynamic(pa, ra), TyKind::Dynamic(pb, rb)) => {
                self.relate_regions(ra, rb, variance, at);
                let unless_auto = |it: &rustc_type_ir::Binder<_, _>| {
                    !matches!(it.skip_binder(), rustc_type_ir::ExistentialPredicate::AutoTrait(_))
                };
                for (bound, y) in pa.iter().filter(unless_auto).zip(pb.iter().filter(unless_auto)) {
                    let (x, y) = (bound.skip_binder(), y.skip_binder());
                    match (x, y) {
                        (
                            rustc_type_ir::ExistentialPredicate::Trait(x),
                            rustc_type_ir::ExistentialPredicate::Trait(y),
                        ) if x.def_id == y.def_id => {
                            self.relate_args(x.args, y.args, None, Variance::Invariant, at)
                        }
                        // An upcast, `dyn Foo<'static>` to `dyn Bar<'static, 'a>` with
                        // `trait Foo<'a>: Bar<'a, 'a>`: the supertrait as the source's
                        // arguments instantiate it is the target's. A supertrait whose
                        // arguments the source's own `for<'a>` binds is left out: it
                        // constrains no region of the body.
                        (
                            rustc_type_ir::ExistentialPredicate::Trait(x),
                            rustc_type_ir::ExistentialPredicate::Trait(y),
                        ) => {
                            let interner = self.interner();
                            let source =
                                bound.rebind(x.with_self_ty(interner, Ty::new_unit(interner)));
                            let found = rustc_type_ir::elaborate::supertraits(interner, source)
                                .find(|it| it.def_id() == y.def_id)
                                .and_then(|it| it.no_bound_vars());
                            if let Some(found) = found {
                                let args = GenericArgs::new_from_slice(
                                    found.args.as_slice().get(1..).unwrap_or(&[]),
                                );
                                self.relate_args(args, y.args, None, Variance::Invariant, at);
                            }
                        }
                        // `dyn Foo<Bar = &'a u32>`: the binding is invariant.
                        (
                            rustc_type_ir::ExistentialPredicate::Projection(x),
                            rustc_type_ir::ExistentialPredicate::Projection(y),
                        ) if x.def_id == y.def_id => {
                            self.relate_args(x.args, y.args, None, Variance::Invariant, at);
                            if let (Some(x), Some(y)) = (
                                rustc_type_ir::inherent::Term::as_type(&x.term),
                                rustc_type_ir::inherent::Term::as_type(&y.term),
                            ) {
                                self.relate(x, y, Variance::Invariant, at);
                            }
                        }
                        _ => (),
                    }
                }
            }
            (TyKind::Alias(_, x), TyKind::Alias(_, y)) if x.kind == y.kind => {
                // Two projections are what they normalize to, as rustc relates them
                // normalized: `<S as Q>::Item<'w, 's>` and `<S as Q>::Item<'w, 'static>` are
                // both `&'w S` when the impl's `Item<'w, 's>` is.
                if matches!(x.kind, rustc_type_ir::AliasTyKind::Projection { .. }) {
                    let normalized_a = self.normalize_at(a, at).filter(|&it| it != a);
                    let normalized_b = self.normalize_at(b, at).filter(|&it| it != b);
                    if normalized_a.is_some() || normalized_b.is_some() {
                        let (a, b) = (normalized_a.unwrap_or(a), normalized_b.unwrap_or(b));
                        if !matches!(a.kind(), TyKind::Infer(_))
                            && !matches!(b.kind(), TyKind::Infer(_))
                        {
                            self.relate(a, b, variance, at);
                        }
                        return;
                    }
                }
                // rustc relates a projection's arguments invariantly, and an opaque type's by
                // its variances under the ambient one: an argument it does not capture is
                // bivariant, so a covariant `impl Sized` of `'a` is one of `'b`.
                let (variances, ambient) = match x.kind {
                    rustc_type_ir::AliasTyKind::Opaque { def_id } => {
                        let frame = self.ctx.frame;
                        (
                            opaque_captures(frame, self.ctx.db, def_id, x.args.len()).map(|it| {
                                mem::collect(
                                    frame,
                                    it.into_iter().map(|captured| match captured {
                                        true => Variance::Invariant,
                                        false => Variance::Bivariant,
                                    }),
                                )
                            }),
                            variance,
                        )
                    }
                    _ => (None, Variance::Invariant),
                };
                self.relate_args(x.args, y.args, variances.as_deref(), ambient, at)
            }
            // A signature's associated type against what it is,
            // `<&'a mut Vec<T> as IntoIterator>::IntoIter` and `IterMut<'_, T>`.
            (TyKind::Alias(_, x), _) | (_, TyKind::Alias(_, x))
                if matches!(x.kind, rustc_type_ir::AliasTyKind::Projection { .. }) =>
            {
                let normalize = |this: &mut Self, ty: Ty<'db>| match ty.kind() {
                    TyKind::Alias(_, _) => this.normalize_at(ty, at).filter(|&it| it != ty),
                    _ => Some(ty),
                };
                if let Some(a) = normalize(self, a)
                    && let Some(b) = normalize(self, b)
                    && !matches!((a.kind(), b.kind()), (TyKind::Alias(_, _), TyKind::Alias(_, _)))
                {
                    self.relate(a, b, variance, at);
                }
            }
            (TyKind::Closure(_, aa), TyKind::Closure(_, ab))
            | (TyKind::Coroutine(_, aa), TyKind::Coroutine(_, ab))
            | (TyKind::CoroutineClosure(_, aa), TyKind::CoroutineClosure(_, ab)) => {
                self.relate_args(aa, ab, None, Variance::Invariant, at)
            }
            // rustc relates a function item's arguments with the item's variances
            // (`relate_item_args`), which its signature gives: `foo<'a>() -> PhantomData<&'a ()>`
            // is covariant in `'a`, and a constructor `fn(T) -> S<T>` is invariant in `T`.
            (TyKind::FnDef(def, aa), TyKind::FnDef(_, ab)) => {
                let (aa, ab) = (aa.no_bound_vars().unwrap(), ab.no_bound_vars().unwrap());
                let variances = match def.0 {
                    hir_def::CallableDefId::FunctionId(it) => {
                        self.ctx.db.variances_of(GenericDefId::FunctionId(it))
                    }
                    hir_def::CallableDefId::StructId(it) => {
                        hir_ty::ctor_variances(self.ctx.db, it.into())
                    }
                    hir_def::CallableDefId::EnumVariantId(it) => {
                        hir_ty::ctor_variances(self.ctx.db, it.into())
                    }
                };
                self.relate_args(aa, ab, Some(variances.as_slice()), variance, at)
            }
            _ => (),
        }
    }

    fn relate_args(
        &mut self,
        a: GenericArgs<'db>,
        b: GenericArgs<'db>,
        variances: Option<&[Variance]>,
        variance: Variance,
        at: At,
    ) {
        for (index, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            let param =
                variances.and_then(|it| it.get(index).copied()).unwrap_or(Variance::Invariant);
            let inner = variance.xform(param);
            match (x.kind(), y.kind()) {
                (GenericArgKind::Lifetime(x), GenericArgKind::Lifetime(y)) => {
                    self.relate_regions(x, y, inner, at)
                }
                (GenericArgKind::Type(x), GenericArgKind::Type(y)) => self.relate(x, y, inner, at),
                _ => (),
            }
        }
    }

    /// That `ty` outlives `region`: each region in it does, and each type parameter in it
    /// must, which is a type test.
    fn type_outlives(&mut self, ty: Ty<'db>, region: Region<'db>, at: At) {
        self.type_outlives_at(ty, region, at, 0);
    }

    fn type_outlives_at(&mut self, ty: Ty<'db>, region: Region<'db>, at: At, depth: usize) {
        let Some(rv) = self.region_rv(region) else { return };
        let mut components = self.scratch.rvs.take();
        let mut params = self.scratch.tys.take();
        outlives_components(ty, &mut components, &mut params);
        for &longer in components.iter() {
            self.outlives(longer, rv, at);
        }
        for &param in params.iter() {
            self.regions.type_tests.push(TypeTest {
                ty: param,
                lower: rv,
                span: at.span,
                forall: false,
                signature: at.category == Category::Signature,
            });
        }
        self.scratch.rvs.give(components);
        self.scratch.tys.give(params);
        if depth > 4 {
            return;
        }
        let mut aliases = self.scratch.tys.take();
        alias_components(ty, &mut aliases);
        for &alias in aliases.iter() {
            self.alias_outlives(alias, region, at, depth);
        }
        self.scratch.tys.give(aliases);
    }

    /// `<T as Trait>::Item: 'r` as rustc's `alias_ty_must_outlive`: what it normalizes to
    /// outlives `'r`; else with no bound from the where clauses or the trait, and lifetimes in
    /// its arguments, each argument does; else the trait's one bound does, or a type test
    /// that some bound or every argument does.
    fn alias_outlives(&mut self, ty: Ty<'db>, region: Region<'db>, at: At, depth: usize) {
        let TyKind::Alias(_, alias) = ty.kind() else { return };
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty)
            || ty.references_non_lt_error()
        {
            return;
        }
        let opaque = alias.kind.try_to_opaque();
        if opaque.is_none()
            && let Some(normalized) = self.normalize(ty)
            && !matches!(normalized.kind(), TyKind::Alias(_, _) | TyKind::Infer(_))
        {
            self.type_outlives_at(normalized, region, at, depth + 1);
            return;
        }
        let Some(rv) = self.region_rv(region) else { return };
        let interner = self.interner();
        let erase = |ty: Ty<'db>| {
            rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
        };
        let erased = erase(ty);
        let in_env = self.env_aliases.contains(&erased);
        let mut from_trait = self.scratch.regions.take();
        from_trait.extend(
            rustc_type_ir::Interner::item_bounds(interner, alias_def_id(alias.kind))
                .iter_instantiated(interner, alias.args)
                .filter_map(|it| match it.skip_norm_wip().kind().skip_binder() {
                    ClauseKind::TypeOutlives(OutlivesClause(bounded, bound))
                        if erase(bounded) == erased =>
                    {
                        Some(bound)
                    }
                    _ => None,
                }),
        );
        let mut has_regions = false;
        rustc_type_ir::fold_regions(interner, alias.args, |region, _| {
            has_regions |= !matches!(region.kind(), RegionKind::ReBound(..));
            region
        });
        if !in_env && from_trait.is_empty() && (has_regions || opaque.is_some()) {
            let captured = opaque
                .and_then(|opaque| opaque_captures(self.ctx.frame, self.ctx.db, opaque, alias.args.len()));
            for (index, arg) in alias.args.iter().enumerate() {
                if captured.as_ref().is_some_and(|it| !it[index]) {
                    continue;
                }
                match arg.kind() {
                    GenericArgKind::Lifetime(it) => {
                        if let Some(longer) = self.region_rv(it) {
                            self.outlives(longer, rv, at);
                        }
                    }
                    GenericArgKind::Type(it) => self.type_outlives_at(it, region, at, depth + 1),
                    GenericArgKind::Const(_) => (),
                }
            }
            self.scratch.regions.give(from_trait);
            return;
        }
        if !in_env
            && let Some(&bound) = from_trait.first()
            && from_trait.iter().all(|&it| it == bound)
        {
            if let Some(bound) = self
                .signature_region(bound, rustc_type_ir::INNERMOST)
                .and_then(rv_of)
                .or_else(|| self.region_rv(bound))
            {
                self.outlives(bound, rv, at);
            }
            self.scratch.regions.give(from_trait);
            return;
        }
        self.regions.type_tests.push(TypeTest {
            ty,
            lower: rv,
            span: at.span,
            forall: false,
            signature: at.category == Category::Signature,
        });
        self.scratch.regions.give(from_trait);
    }

    fn place_ty(&self, local: LocalId, projection: &[PlaceElem]) -> PlaceTy<'db> {
        PlaceTy::from_ty(self.regions.local_tys[local.into_raw().into_u32() as usize])
            .multi_projection_ty(&self.ctx.infcx, self.ctx.env, projection)
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                Some(self.place_ty(place.local, place.projection.as_slice()).ty)
            }
            OperandKind::Constant { ty, .. } => Some(self.renumber(ty.as_ref())),
            OperandKind::Allocation { allocation } => Some(self.renumber(allocation.as_ref().ty)),
            // A reference to the static, every lifetime of its type `'static`, elided ones
            // too; mutable for a `static mut`, rustc's `*mut T`, so that what is written
            // through it is `'static`.
            OperandKind::Static(id) => {
                let db = self.ctx.db;
                let ty = db.value_ty((*id).into())?.instantiate_identity().skip_norm_wip();
                let ty = rustc_type_ir::fold_regions(self.interner(), ty, |region, _| match region
                    .kind()
                {
                    RegionKind::ReBound(..) => region,
                    _ => self.static_region(),
                });
                let mutable = hir_def::signatures::StaticSignature::of(db, *id)
                    .flags
                    .contains(hir_def::signatures::StaticFlags::MUTABLE);
                let mutability = match mutable {
                    true => rustc_ast_ir::Mutability::Mut,
                    false => rustc_ast_ir::Mutability::Not,
                };
                Some(Ty::new_ref(self.interner(), self.static_region(), ty, mutability))
            }
        }
    }

    /// `value` with the lifetimes a user wrote, the signature's, kept and the others fresh.
    fn written<T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
        &mut self,
        value: T,
    ) -> T {
        let keep_static = writes_static_object(self.ctx.store);
        let value = value.fold_with(&mut ObjectDefaults {
            db: self.ctx.db,
            interner: self.interner(),
            keep_static,
            erase: true,
        });
        let mut vars = mem::map(self.ctx.frame);
        rustc_type_ir::fold_regions(self.interner(), value, |region, depth| match region.kind() {
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic => {
                self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
            }
            RegionKind::ReBound(..) => region,
            RegionKind::ReVar(_) => *vars.entry(region).or_insert_with(|| self.fresh()),
            _ => self.fresh(),
        })
    }

    /// The expression a call's callee is: its path, or the method call.
    fn callee_expr(&self, func: &Operand, call: MirSpan) -> Option<hir_def::hir::ExprId> {
        match (func.span(), call) {
            (Some(MirSpan::ExprId(expr)), _) => Some(expr),
            (_, MirSpan::ExprId(call)) => match self.ctx.store.expr(call) {
                hir_def::hir::Expr::Call { callee, .. } => Some(callee),
                _ => Some(call),
            },
            _ => None,
        }
    }

    /// The type of a callee whose path spells out generic arguments, `f::<&'a T>`, with the
    /// lifetimes written there: rustc's user type annotation on the path.
    ///
    /// As rustc, the callee's type has regions of its own, equal to the written ones by a
    /// `GenericArg` constraint at the path, so an error blames a use of the callee over it.
    fn written_callee(&mut self, func: &Operand, call: MirSpan) -> Option<Ty<'db>> {
        let written = self.written_callee_ty(func, call)?;
        let ty = self.renumber(written);
        let span = self.callee_expr(func, call).map_or(call, MirSpan::ExprId);
        self.relate(ty, written, Variance::Invariant, At { category: Category::GenericArg, span });
        Some(ty)
    }

    fn written_callee_ty(&mut self, func: &Operand, call: MirSpan) -> Option<Ty<'db>> {
        let ty = match &func.kind {
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            _ => return None,
        };
        let expr = self.callee_expr(func, call)?;
        match self.ctx.store.expr(expr) {
            hir_def::hir::Expr::Path(path) if spells_generics(path) => {
                let ty = self.written(ty);
                if let TyKind::FnDef(_, args) = ty.kind() {
                    self.spelled_projections(expr, path, args.no_bound_vars().unwrap());
                }
                if let TyKind::FnDef(def, args) = ty.kind()
                    && let CallableDefId::FunctionId(function) = def.0
                {
                    let args = args.no_bound_vars().unwrap();
                    let container = function.lookup(self.ctx.db).container;
                    let parent =
                        hir_ty::generics::generics(self.ctx.db, function.into()).len_parent();
                    let args = GenericArgs::new_from_slice(
                        args.as_slice().get(..parent).unwrap_or(args.as_slice()),
                    );
                    self.written_self(path, container, args, MirSpan::ExprId(expr));
                }
                Some(ty)
            }
            // `x.f::<&'a T>()`: the method's own arguments as written, the impl's or trait's
            // inferred.
            hir_def::hir::Expr::MethodCall { generic_args: Some(_), .. } => {
                let TyKind::FnDef(def, args) = ty.kind() else { return None };
                let CallableDefId::FunctionId(function) = def.0 else { return None };
                let args = args.no_bound_vars().unwrap();
                let parent = hir_ty::generics::generics(self.ctx.db, function.into()).len_parent();
                let frame = self.ctx.frame;
                let args = mem::collect(
                    frame,
                    args.iter().enumerate().map(|(index, arg)| match index < parent {
                        true => self.renumber(arg),
                        false => self.written(arg),
                    }),
                );
                let args = GenericArgs::new_from_slice(&args);
                Some(Ty::new_fn_def(self.interner(), def, args))
            }
            _ => None,
        }
    }

    fn category_of_assignment(&self, local: LocalId, projection: &[PlaceElem]) -> Category {
        if !projection.is_empty() {
            return Category::Assignment;
        }
        if local == super::return_slot() {
            Category::Return
        } else if self.ctx.bindings.contains_key(&local) {
            Category::Assignment
        } else {
            Category::Boring
        }
    }

    /// rustc's `CopyBound`: a copied value's type is `Copy`, which `impl Copy for Foo<'static>`
    /// proves only for `'static`. References, pointers and scalars are `Copy` for any lifetime.
    fn copy_bound(&mut self, operand: &Operand, span: MirSpan) {
        let OperandKind::Copy(place) = &operand.kind else { return };
        let ty = self.place_ty(place.local, place.projection.as_slice()).ty;
        if !matches!(ty.kind(), TyKind::Adt(..) | TyKind::Tuple(..) | TyKind::Array(..)) {
            return;
        }
        let Some(copy) = self.interner().lang_items().Copy else { return };
        let trait_ref = rustc_type_ir::TraitRef::new(self.interner(), copy.into(), [ty]);
        let span = operand.span().unwrap_or(span);
        self.prove(trait_ref, At { category: Category::Predicate, span });
    }

    /// rustc's `check_local`: a variable's or temporary's type is `Sized`, which a where clause
    /// on an associated type, `<&'static T as A>::X: Sized`, proves for those lifetimes only.
    fn sized_local(&mut self, local: LocalId, ty: Ty<'db>, span: MirSpan) {
        if local == super::return_slot()
            || self.body.param_locals.contains(&local)
            || !matches!(ty.kind(), TyKind::Alias(_, alias) if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. }))
        {
            return;
        }
        let Some(sized) = self.interner().lang_items().Sized else { return };
        let trait_ref = rustc_type_ir::TraitRef::new(self.interner(), sized.into(), [ty]);
        self.prove(trait_ref, At { category: Category::Predicate, span });
    }

    fn statement(&mut self, kind: &StatementKind, span: MirSpan) {
        kind.for_each_operand(&mut |operand| self.copy_bound(operand, span));
        if let StatementKind::InlineAsm(asm) = kind {
            for operand in asm.operands.iter() {
                if let hir_ty_mir::mir::AsmOperand::Sym(operand) = operand {
                    self.asm_sym(operand, span);
                }
            }
            return;
        }
        let StatementKind::Assign(place, rvalue) = kind else { return };
        let projection = place.projection.as_slice();
        let dest = self.place_ty(place.local, projection).ty;
        let mut at = At { category: self.category_of_assignment(place.local, projection), span };
        // A field that is a place is the operand itself in rustc, with no temporary.
        if at.category == Category::Boring
            && projection.is_empty()
            && self.aggregate_temps.contains(place.local.into_raw().into_u32() as usize)
            && !matches!(
                rvalue,
                Rvalue::Use(Operand { kind: OperandKind::Copy(_) | OperandKind::Move(_), .. })
            )
        {
            at.category = Category::Usage;
        }
        if projection.is_empty() {
            self.sized_local(place.local, dest, span);
            if place.local == super::return_slot() {
                let bounds = std::mem::replace(&mut self.opaque_bounds, Vec::new_in(self.ctx.frame));
                let at = At { category: Category::Signature, span };
                self.clauses(&bounds, at, true);
                if let Some(hidden) = self.opaque_hidden.take() {
                    self.well_formed(hidden, at, 0);
                }
            }
        }
        let first = self.regions.outlives.len();
        self.assign(place, rvalue, dest, at, span);
        // rustc relates a variable with a type written out to its annotation, so what flows
        // into a lifetime the annotation names, `let p: &'static T = ..`, is blamed on that.
        if projection.is_empty() && self.annotated.contains_key(&place.local) {
            for index in first..self.regions.outlives.len() {
                if self.regions.outlives.category(index) == Category::Assignment
                    && self.regions.universal(self.regions.outlives.sub(index)).is_some()
                {
                    self.regions.outlives.set_category(index, Category::TypeAnnotation);
                }
            }
        }
    }

    fn assign(
        &mut self,
        place: &hir_ty_mir::mir::StoredPlace,
        rvalue: &Rvalue,
        dest: Ty<'db>,
        at: At,
        span: MirSpan,
    ) {
        let projection = place.projection.as_slice();
        // rustc's user type annotation on a cast is the type written, its `_`s inference
        // variables: `ptr as _` relates no lifetime, though `_` is inferred `'static` from
        // where the value goes.
        if let MirSpan::ExprId(expr) = span
            && let hir_def::hir::Expr::Cast { type_ref, .. } = self.ctx.store.expr(expr)
            && matches!(rvalue, Rvalue::Use(_) | Rvalue::Cast(..))
            && let Some(inferred) = self.ctx.infer.type_of_expr.get(expr)
        {
            let inferred = inferred.as_ref();
            let lowered = self.lower_annotation(type_ref, span);
            let ty = self.zip_annotation(lowered, inferred, &mut mem::map(self.ctx.frame), 0);
            let written = self.written(ty.unwrap_or(inferred));
            let at = At { category: Category::TypeAnnotation, span };
            self.relate(dest, written, Variance::Invariant, at);
        }
        // A range pattern's bounds, `A::<'a>::X..=Y`, are named constants compared.
        if let Rvalue::BinaryOp(_, left, right) | Rvalue::CheckedBinaryOp(_, left, right) = rvalue {
            self.constant_ty(left, span);
            self.constant_ty(right, span);
        }
        match rvalue {
            // Raw and function pointers compare when they have a common supertype.
            Rvalue::BinaryOp(op, left, right) | Rvalue::CheckedBinaryOp(op, left, right)
                if matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                ) =>
            {
                if let (Some(left), Some(right)) = (self.operand_ty(left), self.operand_ty(right))
                    && matches!(left.kind(), TyKind::RawPtr(..) | TyKind::FnPtr(..))
                {
                    let common = self.renumber(left);
                    let at = At { category: Category::CallArgument, span };
                    self.relate(left, common, Variance::Covariant, at);
                    self.relate(right, common, Variance::Covariant, at);
                }
            }
            Rvalue::Repeat(operand, len) => {
                if let (Some(ty), TyKind::Array(element, _)) =
                    (self.operand_ty(operand), dest.kind())
                {
                    self.relate(ty, element, Variance::Covariant, at);
                    // More than one copy of what is not a constant needs it `Copy`, which
                    // `impl Copy for Foo<'static>` proves only for `'static`.
                    let copies = hir_ty::consteval::try_const_usize(self.ctx.db, len.as_ref());
                    if !matches!(
                        operand.kind,
                        OperandKind::Constant { .. } | OperandKind::Allocation { .. }
                    ) && copies.is_none_or(|it| it > 1)
                        && let Some(copy) = self.interner().lang_items().Copy
                    {
                        let trait_ref =
                            rustc_type_ir::TraitRef::new(self.interner(), copy.into(), [ty]);
                        self.prove(trait_ref, At { category: Category::Predicate, span });
                    }
                }
            }
            Rvalue::Aggregate(AggregateKind::Array(_), operands) => {
                if let TyKind::Array(element, _) = dest.kind() {
                    for operand in operands.iter() {
                        if let Some(ty) = self.operand_ty(operand) {
                            let at = At { span: operand.span().unwrap_or(span), ..at };
                            self.relate(ty, element, Variance::Covariant, at);
                        }
                    }
                }
            }
            Rvalue::Aggregate(AggregateKind::Union(_, field), operands) => {
                if let (TyKind::Adt(_, args), Some(operand)) = (dest.kind(), operands.first())
                    && let Some(ty) = self.operand_ty(operand)
                {
                    let field_ty = self.ctx.db.field_types(field.parent)[field.local_id]
                        .ty()
                        .instantiate(self.interner(), args)
                        .skip_norm_wip();
                    self.relate(ty, field_ty, Variance::Covariant, at);
                }
            }
            _ => {
                let constant = match rvalue {
                    Rvalue::Use(operand) => {
                        self.constant_ty(operand, span).or_else(|| self.fn_item(operand, span))
                    }
                    _ => None,
                };
                if let Some(ty) = constant.or_else(|| self.rvalue_ty(rvalue, span)) {
                    self.relate(ty, dest, Variance::Covariant, at);
                }
            }
        }
    }

    /// The type of a named constant as rustc's type check gives it: its generic arguments
    /// renumbered, or kept as written where its path spells them out (rustc's user type
    /// annotation), with its where clauses proven; for a constant of an inherent impl named
    /// through a written self type, `<Foo<'a>>::C`, that type equal to the impl's.
    fn constant_ty(&mut self, operand: &Operand, span: MirSpan) -> Option<Ty<'db>> {
        let OperandKind::Constant { konst, .. } = &operand.kind else { return None };
        let ConstKind::Alias(_, unevaluated) = konst.as_ref().kind() else { return None };
        let id = match unevaluated.kind {
            AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
            | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
            | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => id,
            AliasConstKind::Anon { def_id } => return self.inline_const_ty(def_id.0, operand),
            AliasConstKind::InherentSelf { def_id } => {
                panic!("{def_id:?}: a MIR constant names an inherent const in its `Self` form")
            }
        };
        let span = operand.span().unwrap_or(span);
        let path = match span {
            MirSpan::ExprId(expr) => match self.ctx.store.expr(expr) {
                hir_def::hir::Expr::Path(path) => Some(path),
                _ => None,
            },
            MirSpan::PatId(pat) => match self.ctx.store.pat(pat) {
                Pat::Path(path) => Some(path),
                _ => None,
            },
            _ => None,
        };
        let args = match path {
            Some(path) if spells_generics(path) => self.written(unevaluated.args),
            _ => self.renumber(unevaluated.args),
        };
        let db = self.ctx.db;
        let ty = self
            .declared(db.value_ty(id.into())?.instantiate(self.interner(), args).skip_norm_wip());
        self.predicates(id.into(), args, At { category: Category::Predicate, span }, true);
        if let MirSpan::ExprId(expr) = span {
            self.path_well_formed(expr, id.lookup(db).container, args, span);
        }
        if let Some(path) = path {
            self.written_self(path, id.lookup(db).container, args, span);
        }
        Some(ty)
    }

    /// The type of a `const { .. }` block where it is evaluated, its function's lifetimes this
    /// body's, with what the block needs of the regions of its type, as rustc relates an
    /// inline const's arguments.
    fn inline_const_ty(
        &mut self,
        konst: hir_ty::db::AnonConstId<'db>,
        operand: &Operand,
    ) -> Option<Ty<'db>> {
        let db = self.ctx.db;
        if !konst.is_inline_const(db) {
            return None;
        }
        let OperandKind::Constant { ty, .. } = &operand.kind else { return None };
        let mut positions = self.scratch.rvs.take();
        let ty =
            rustc_type_ir::fold_regions(self.interner(), ty.as_ref(), |region, depth| match region
                .kind()
            {
                RegionKind::ReBound(..) => region,
                RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic => {
                    self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
                }
                _ => {
                    let region = self.fresh();
                    positions.extend(rv_of(region));
                    region
                }
            });
        self.apply_requirements(konst.loc(db).expr, &positions);
        self.scratch.rvs.give(positions);
        Some(ty)
    }

    /// For an associated item named through a written self type, `<Foo<'a>>::C` or
    /// `Foo::<'a>::new`, that type equal to the impl's or the trait's self type.
    fn written_self(
        &mut self,
        path: hir_def::expr_store::path::Path<'_>,
        container: ItemContainerId,
        args: GenericArgs<'db>,
        span: MirSpan,
    ) {
        let written = match path.type_anchor() {
            Some(anchor) => Some(self.lower_written(anchor, span)),
            None => self.lower_written_qualifier(path, span),
        };
        let self_ty = match container {
            ItemContainerId::ImplId(impl_) => Some(self.declared(
                self.ctx.db.impl_self_ty(impl_).instantiate(self.interner(), args).skip_norm_wip(),
            )),
            ItemContainerId::TraitId(_) => args.types().next(),
            _ => None,
        };
        if let Some(written) = written
            && let Some(self_ty) = self_ty
        {
            let written = self.written(written);
            let at = At { category: Category::TypeAnnotation, span };
            self.relate(self_ty, written, Variance::Invariant, at);
        }
    }

    /// A function an inline `asm!` names by `sym`, with the generic arguments its path spells
    /// out: rustc type checks the path as a value, with its user type annotation, so its where
    /// clauses hold of the lifetimes written there (`sym dep::<'a, T>` needs `T: 'a`).
    fn asm_sym(&mut self, operand: &Operand, span: MirSpan) {
        let ty = match &operand.kind {
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            _ => return,
        };
        let ty = self.written(ty);
        if let TyKind::FnDef(def, args) = ty.kind() {
            let args = args.no_bound_vars().unwrap();
            self.callee_predicates(def.0, args, span);
            for ty in args.types() {
                self.well_formed(ty, At { category: Category::Boring, span }, 0);
            }
        }
    }

    /// A function item used as a value, with its where clauses proven, as rustc's type check
    /// proves those of every function item it names (`let f: fn(&'a str) -> _ = extend;` or
    /// passing `extend` to a call); where its path spells out generic arguments,
    /// `let f = <&'static u32 as Trait>::method;`, with the lifetimes written there, as
    /// rustc's user type annotation on the path.
    fn fn_item(&mut self, operand: &Operand, span: MirSpan) -> Option<Ty<'db>> {
        let ty = match self.written_callee(operand, span) {
            Some(ty) => ty,
            None => match &operand.kind {
                OperandKind::Constant { .. } | OperandKind::Allocation { .. } => {
                    self.operand_ty(operand).filter(|it| matches!(it.kind(), TyKind::FnDef(..)))?
                }
                _ => return None,
            },
        };
        if let TyKind::FnDef(def, args) = ty.kind() {
            if let CallableDefId::FunctionId(function) = def.0 {
                self.error_region |= signature_lifetime_error(self.ctx.db, function);
            }
            let args = args.no_bound_vars().unwrap();
            let span = operand.span().unwrap_or(span);
            self.callee_predicates(def.0, args, span);
            for ty in args.types() {
                self.well_formed(ty, At { category: Category::Boring, span }, 0);
            }
        }
        Some(ty)
    }

    /// A declared type instantiated with region variables: its `'static` the static region,
    /// any other region it names a fresh variable.
    fn declared(&self, ty: Ty<'db>) -> Ty<'db> {
        rustc_type_ir::fold_regions(self.interner(), ty, |region, _| match region.kind() {
            RegionKind::ReVar(_) | RegionKind::ReBound(..) => region,
            RegionKind::ReStatic => self.static_region(),
            _ => self.fresh(),
        })
    }

    /// The arguments of the self type of the impl this body is in, with the signature's
    /// lifetimes, where it is `adt`.
    fn impl_self_args(&mut self, adt: AdtId) -> Option<GenericArgs<'db>> {
        let db = self.ctx.db;
        let GenericDefId::FunctionId(function) = self.ctx.owner.generic_def(db) else {
            return None;
        };
        let ItemContainerId::ImplId(impl_) = function.lookup(db).container else { return None };
        let ty = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let TyKind::Adt(def, args) = ty.kind() else { return None };
        if def.def_id() != adt {
            return None;
        }
        // The impl's anonymous lifetimes (`impl MyStruct<'_>`) are universal regions.
        if self.nested() && self.impl_self.is_none() {
            self.impl_self_of(function);
        }
        if let Some((raw, named)) = self.impl_self
            && raw == ty
            && let TyKind::Adt(_, named) = named.kind()
        {
            return Some(rustc_type_ir::fold_regions(self.interner(), named, |region, depth| {
                match region.kind() {
                    RegionKind::ReVar(_) | RegionKind::ReBound(..) => region,
                    _ => self.signature_region(region, depth).unwrap_or_else(|| self.fresh()),
                }
            }));
        }
        Some(self.written(args))
    }

    /// A type written in the body, lowered as inference lowers it, its lifetimes the
    /// signature's where it names them.
    /// For each error in `ty`, the type of `function`'s parameter written `param`, which of the
    /// lifetimes the parameter leaves out it is, by the order they are first met; `None` where
    /// the written type does not line up with `ty`.
    fn elided_groups(
        &self,
        function: hir_def::FunctionId,
        param: hir_def::type_ref::TypeRefId,
        ty: Ty<'db>,
    ) -> Option<FVec<'a, usize>> {
        use hir_def::resolver::HasResolver;
        let db = self.ctx.db;
        let resolver = function.resolver(db);
        let signature = FunctionSignature::of(db, function);
        let generics = std::cell::OnceCell::new();
        let lowered = hir_ty::TyLoweringContext::new(
            db,
            &resolver,
            &signature.store,
            hir_def::ExpressionStoreOwnerId::Signature(function.into()),
            function.into(),
            &generics,
            hir_ty::LifetimeElisionKind::Infer,
            hir_ty::LifetimeLoweringMode::Bound,
            self.ctx.frame,
        )
        .with_infer_ctxt_vars(Some(&self.ctx.infcx))
        .with_elided_region_vars()
        .lower_ty(param);
        let frame = self.ctx.frame;
        let regions = |ty: Ty<'db>| {
            let mut found = Vec::new_in(frame);
            rustc_type_ir::fold_regions(self.interner(), ty, |region, _| {
                found.push(region);
                region
            });
            found
        };
        let (theirs, ours) = (regions(ty), regions(lowered));
        if theirs.len() != ours.len() {
            return None;
        }
        let mut distinct = Vec::new_in(frame);
        let mut groups = Vec::new_in(frame);
        for (theirs, ours) in theirs.into_iter().zip(ours) {
            if !matches!(theirs.kind(), RegionKind::ReError(_)) {
                continue;
            }
            if !matches!(ours.kind(), RegionKind::ReVar(_)) {
                return None;
            }
            groups.push(distinct.iter().position(|&it| it == ours).unwrap_or_else(|| {
                distinct.push(ours);
                distinct.len() - 1
            }));
        }
        Some(groups)
    }

    fn lower_written(&self, type_ref: hir_def::type_ref::TypeRefId, span: MirSpan) -> Ty<'db> {
        self.lowering(span, |ctx| ctx.lower_ty(type_ref))
    }

    /// A `let`'s written type with a region variable for each lifetime it leaves out, so an
    /// alias that uses its lifetime twice (`type P<'a> = (&'a u32, &'a u32)`) shares it, as
    /// rustc's user type annotation does. Its `_`s are type variables.
    fn lower_annotation(&self, type_ref: hir_def::type_ref::TypeRefId, span: MirSpan) -> Ty<'db> {
        self.lowering_with(span, true, |ctx| ctx.lower_ty(type_ref))
    }

    /// The type of a `let` with a type written out, with the lifetimes it names: its written
    /// type where that is the type inferred for it, else the inferred type.
    fn annotation_ty(&mut self, annotation: Annotation) -> Option<Ty<'db>> {
        let inferred = self.ctx.infer.type_of_pat.get(annotation.pat)?.as_ref();
        let lowered = self.lower_annotation(annotation.type_ref, MirSpan::ExprId(annotation.block));
        let mut holes = self.projection_holes(lowered, inferred);
        let ty = self.zip_annotation(lowered, inferred, &mut holes, 0);
        Some(self.written(ty.unwrap_or(inferred)))
    }

    /// The written type `type_ref` with the `_`s in its associated types solved against the
    /// type `inferred` for it, if it has any; see `projection_holes`.
    fn solved_annotation(
        &self,
        type_ref: hir_def::type_ref::TypeRefId,
        inferred: Ty<'db>,
        span: MirSpan,
    ) -> Option<Ty<'db>> {
        let lowered = self.lower_annotation(type_ref, span);
        let mut holes = self.projection_holes(lowered, inferred);
        if holes.is_empty() {
            return None;
        }
        self.zip_annotation(lowered, inferred, &mut holes, 0)
    }

    /// The type arguments a path to a function or constructor spells out on one segment, each
    /// with its index in the arguments of the item's type: in `Some::<T>` or
    /// `Option::<T>::None`, the variant's.
    fn spelled_positions(
        &self,
        expr: hir_def::hir::ExprId,
        path: hir_def::expr_store::path::Path<'_>,
    ) -> Option<FVec<'a, (hir_def::type_ref::TypeRefId, usize)>> {
        use hir_def::expr_store::path::GenericArg;
        use rustc_type_ir::inherent::GenericArg as _;
        let generic_args = path.generic_args()?;
        let mut spelled = generic_args.iter().enumerate().filter(|(_, it)| it.is_some());
        let (at, args) = spelled.next()?;
        if spelled.next().is_some() {
            return None;
        }
        let ty = self.ctx.infer.type_of_expr.get(expr)?.as_ref();
        let TyKind::FnDef(def, inferred) = ty.kind() else { return None };
        let inferred = inferred.no_bound_vars().unwrap();
        let last = at + 1 == generic_args.len();
        let skip = match def.0 {
            CallableDefId::FunctionId(function) if last => {
                hir_ty::generics::generics(self.ctx.db, function.into()).len_parent()
            }
            CallableDefId::StructId(_) if last => 0,
            CallableDefId::EnumVariantId(_) if last || at + 2 == generic_args.len() => 0,
            _ => return None,
        };
        let frame = self.ctx.frame;
        let written = mem::collect(
            frame,
            args?.args.iter().filter_map(|it| match it {
                GenericArg::Type(type_ref) => Some(*type_ref),
                _ => None,
            }),
        );
        let positions = mem::collect(
            frame,
            inferred
                .iter()
                .enumerate()
                .skip(skip)
                .filter(|(_, it)| it.as_type().is_some())
                .map(|(index, _)| index),
        );
        (written.len() == positions.len())
            .then(|| mem::collect(frame, written.into_iter().zip(positions)))
    }

    /// What a call whose callee path spells out `<(_,) as Tr>::Ty` as a type argument
    /// requires, as rustc's user type annotation on the callee: the `_`s solved against the
    /// argument, the annotation normalized is the argument, so `Some::<<(u8, _) as
    /// Tr>::Ty>(&temp())` needs the borrow to live as long as the impl selected needs.
    fn spelled_projections(
        &mut self,
        expr: hir_def::hir::ExprId,
        path: hir_def::expr_store::path::Path<'_>,
        args: GenericArgs<'db>,
    ) {
        use rustc_type_ir::inherent::GenericArg as _;
        let span = MirSpan::ExprId(expr);
        let at = At { category: Category::TypeAnnotation, span };
        for (type_ref, index) in self.spelled_positions(expr, path).into_iter().flatten() {
            let Some(arg) = args.get(index).and_then(|it| it.as_type()) else { continue };
            let Some(solved) = self.solved_annotation(type_ref, arg, span) else { continue };
            let solved = self.written(solved);
            self.well_formed(solved, at, 0);
            let normalized = self.normalize_at(solved, at).unwrap_or(solved);
            self.relate(arg, normalized, Variance::Invariant, at);
        }
    }

    /// The `_`s of a written type that stand in an associated type, `<(_,) as Tr>::Ty`, as
    /// rustc infers them for its user type annotation: what equating the written type with
    /// the one inferred makes them, so normalizing the associated type selects the impl the
    /// inferred type does, whose where clauses then must hold (`impl<T: 'static> Tr for
    /// (T,)` needs what the `_` is to be `'static`). Each with its lifetimes fresh.
    fn projection_holes(&self, lowered: Ty<'db>, inferred: Ty<'db>) -> FMap<'a, Ty<'db>, Ty<'db>> {
        use hir_ty::next_solver::{
            infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt,
        };
        use rustc_type_ir::{TypeVisitableExt as _, inherent::GenericArg as _};
        let frame = self.ctx.frame;
        let mut holes = mem::map(frame);
        let mut in_projection = Vec::new_in(frame);
        for arg in lowered.walk() {
            let Some(ty) = arg.as_type() else { continue };
            if let TyKind::Alias(_, alias) = ty.kind()
                && let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind
            {
                for arg in ty.walk() {
                    if let Some(hole) = arg.as_type()
                        && let TyKind::Infer(rustc_type_ir::InferTy::TyVar(_)) = hole.kind()
                        && !in_projection.contains(&hole)
                    {
                        in_projection.push(hole);
                    }
                }
            }
        }
        if in_projection.is_empty() {
            return holes;
        }
        let interner = self.interner();
        // Regions do not select an impl; rust-analyzer infers some as errors.
        let statik = |ty: Ty<'db>| {
            rustc_type_ir::fold_regions(interner, ty, |region, _| match region.kind() {
                RegionKind::ReBound(..) => region,
                _ => Region::new_static(interner),
            })
        };
        if statik(inferred).references_error() {
            return holes;
        }
        let infcx = &self.ctx.infcx;
        let found: FVec<'_, (Ty<'db>, Ty<'db>)> = infcx.probe(|_| {
            let mut ocx = ObligationCtxt::new(infcx);
            let cause = ObligationCause::dummy();
            if ocx.eq(&cause, self.ctx.env, statik(lowered), statik(inferred)).is_err() {
                return Vec::new_in(frame);
            }
            let _ = ocx.try_evaluate_obligations();
            mem::collect(
                frame,
                in_projection
                    .iter()
                    .map(|&hole| (hole, infcx.deeply_resolve_ignoring_regions(hole)))
                    .filter(|(_, it)| !it.has_non_region_infer() && !it.references_error()),
            )
        });
        for (hole, value) in found {
            holes.insert(hole, self.renumber(value));
        }
        holes
    }

    /// The written type `lowered` with each `_` in it the type inferred there, the same
    /// regions wherever the same `_` stands; `None` where the two differ otherwise.
    fn zip_annotation(
        &self,
        lowered: Ty<'db>,
        inferred: Ty<'db>,
        holes: &mut FMap<'_, Ty<'db>, Ty<'db>>,
        depth: usize,
    ) -> Option<Ty<'db>> {
        let interner = self.interner();
        let frame = self.ctx.frame;
        if depth > 16 {
            return None;
        }
        let mut zip = |a: Ty<'db>, b: Ty<'db>| self.zip_annotation(a, b, holes, depth + 1);
        Some(match (lowered.kind(), inferred.kind()) {
            (TyKind::Infer(rustc_type_ir::InferTy::TyVar(_)), _) => {
                *holes.entry(lowered).or_insert_with(|| self.renumber(inferred))
            }
            (TyKind::Ref(region, a, m), TyKind::Ref(_, b, n)) if m == n => {
                Ty::new_ref(interner, region, zip(a, b)?, m)
            }
            (TyKind::RawPtr(a, m), TyKind::RawPtr(b, n)) if m == n => {
                Ty::new(interner, TyKind::RawPtr(zip(a, b)?, m))
            }
            (TyKind::Slice(a), TyKind::Slice(b)) => Ty::new(interner, TyKind::Slice(zip(a, b)?)),
            (TyKind::Array(a, len), TyKind::Array(b, inferred_len)) => {
                let len =
                    if matches!(len.kind(), ConstKind::Infer(_)) { inferred_len } else { len };
                Ty::new(interner, TyKind::Array(zip(a, b)?, len))
            }
            (TyKind::Tuple(a), TyKind::Tuple(b)) if a.len() == b.len() => {
                let mut tys = Vec::with_capacity_in(a.len(), frame);
                for (a, b) in a.iter().zip(b.iter()) {
                    tys.push(zip(a, b)?);
                }
                Ty::new_tup(interner, &tys)
            }
            (TyKind::Adt(def, a), TyKind::Adt(inferred_def, b))
                if def == inferred_def && a.len() == b.len() =>
            {
                let mut args = Vec::with_capacity_in(a.len(), frame);
                for (a, b) in a.iter().zip(b.iter()) {
                    args.push(match (a.kind(), b.kind()) {
                        (GenericArgKind::Type(a), GenericArgKind::Type(b)) => zip(a, b)?.into(),
                        (GenericArgKind::Const(a), GenericArgKind::Const(_))
                            if matches!(a.kind(), ConstKind::Infer(_)) =>
                        {
                            b
                        }
                        _ => a,
                    });
                }
                Ty::new(interner, TyKind::Adt(def, GenericArgs::new_from_slice(&args)))
            }
            // An associated type with `_`s in it has them as `projection_holes` inferred.
            (TyKind::Alias(_, alias), _)
                if let rustc_type_ir::AliasTyKind::Projection { .. } = alias.kind
                    && rustc_type_ir::TypeVisitableExt::has_non_region_infer(&lowered) =>
            {
                let mut missing = false;
                let ty = rustc_type_ir::TypeFoldable::fold_with(
                    lowered,
                    &mut hir_ty::next_solver::util::BottomUpFolder {
                        interner,
                        ty_op: |ty: Ty<'db>| match ty.kind() {
                            TyKind::Infer(rustc_type_ir::InferTy::TyVar(_)) => {
                                holes.get(&ty).copied().unwrap_or_else(|| {
                                    missing = true;
                                    ty
                                })
                            }
                            _ => ty,
                        },
                        lt_op: |region| region,
                        ct_op: |ct| ct,
                    },
                );
                if missing {
                    return None;
                }
                ty
            }
            (TyKind::FnPtr(a, header), TyKind::FnPtr(b, inferred_header))
                if header == inferred_header
                    && a.skip_binder().inputs_and_output.len()
                        == b.skip_binder().inputs_and_output.len() =>
            {
                let mut tys = Vec::with_capacity_in(a.skip_binder().inputs_and_output.len(), frame);
                for (a, b) in a
                    .skip_binder()
                    .inputs_and_output
                    .iter()
                    .zip(b.skip_binder().inputs_and_output.iter())
                {
                    tys.push(zip(a, b)?);
                }
                let tys = hir_ty::next_solver::Tys::new_from_slice(&tys);
                Ty::new(
                    interner,
                    TyKind::FnPtr(
                        a.rebind(rustc_type_ir::FnSigTys { inputs_and_output: tys }),
                        header,
                    ),
                )
            }
            _ => {
                let erase = |ty: Ty<'db>| {
                    rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
                };
                // A projection rust-analyzer normalized where rustc keeps it as written, as
                // under a `for<'a>`: `<&'a () as Trait>::Ty` inferred as `()`.
                let normalizes = || {
                    matches!(lowered.kind(), TyKind::Alias(..))
                        && normalize(self.ctx, erase(lowered)).map(erase) == Some(erase(inferred))
                };
                if erase(lowered) != erase(inferred) && !normalizes() {
                    return None;
                }
                lowered
            }
        })
    }

    /// The type a value path's qualifier spells out with generic arguments, `A<'a>` of
    /// `A::<'a>::C`, lowered as `lower_written` lowers.
    fn lower_written_qualifier(
        &self,
        path: hir_def::expr_store::path::Path<'_>,
        span: MirSpan,
    ) -> Option<Ty<'db>> {
        let segments = path.segments();
        let qualifier = segments.get(segments.len().checked_sub(2)?)?;
        qualifier.args_and_bindings?;
        self.lowering(span, |ctx| ctx.lower_value_path_qualifier(path))
    }

    fn lowering<T>(
        &self,
        span: MirSpan,
        f: impl FnOnce(&mut hir_ty::TyLoweringContext<'db, '_>) -> T,
    ) -> T {
        self.lowering_with(span, false, f)
    }

    fn lowering_with<T>(
        &self,
        span: MirSpan,
        vars: bool,
        f: impl FnOnce(&mut hir_ty::TyLoweringContext<'db, '_>) -> T,
    ) -> T {
        let db = self.ctx.db;
        let store_owner = self.ctx.owner.expression_store_owner(db);
        let mut resolver = store_owner.resolver(db);
        let _guard = match span {
            MirSpan::ExprId(expr) => Some(resolver.update_to_inner_scope(db, store_owner, expr)),
            _ => None,
        };
        let generics = std::cell::OnceCell::new();
        let mut ctx = hir_ty::TyLoweringContext::new(
            db,
            &resolver,
            hir_def::expr_store::ExpressionStore::of(db, store_owner),
            store_owner,
            self.ctx.owner.generic_def(db),
            &generics,
            hir_ty::LifetimeElisionKind::Infer,
            hir_ty::LifetimeLoweringMode::LateParam,
            self.ctx.frame,
        )
        .with_infer_ctxt_vars(vars.then_some(&self.ctx.infcx));
        if vars {
            ctx = ctx.with_elided_region_vars();
        }
        f(&mut ctx)
    }

    fn rvalue_ty(&mut self, rvalue: &Rvalue, span: MirSpan) -> Option<Ty<'db>> {
        let boring = At { category: Category::Boring, span };
        match rvalue {
            Rvalue::Use(operand) => self.operand_ty(operand),
            Rvalue::CopyForDeref(place) => {
                Some(self.place_ty(place.local, place.projection.as_slice()).ty)
            }
            Rvalue::Ref(kind, place) => {
                let region = self.fresh();
                let rv = rv_of(region).unwrap();
                if let Some(location) = self.location {
                    self.regions.loan_regions.insert(location, rv);
                }
                let projection = place.projection.as_slice();
                self.reborrow_constraints(place.local, projection, rv, boring);
                let ty = self.place_ty(place.local, projection).ty;
                let mutability = match kind {
                    BorrowKind::Mut { .. } => rustc_ast_ir::Mutability::Mut,
                    _ => rustc_ast_ir::Mutability::Not,
                };
                Some(Ty::new_ref(self.interner(), region, ty, mutability))
            }
            Rvalue::Cast(kind, operand, target) => {
                let target = self.renumber(target.as_ref());
                let source = match kind {
                    CastKind::PointerCoercion(hir_ty::PointerCast::ReifyFnPointer) => {
                        self.fn_item(operand, span).or_else(|| self.operand_ty(operand))?
                    }
                    _ => self.operand_ty(operand)?,
                };
                let at = At { category: Category::Cast, span };
                match kind {
                    // rustc's casts between function types: what is cast is a subtype of the
                    // function pointer it becomes, by signature.
                    CastKind::PointerCoercion(hir_ty::PointerCast::UnsafeFnPointer) => {
                        self.relate(source, target, Variance::Covariant, at)
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::ReifyFnPointer) => {
                        if let TyKind::FnDef(def, args) = source.kind() {
                            let args = args.no_bound_vars().unwrap();
                            let sig = self
                                .ctx
                                .db
                                .callable_item_signature(def.0)
                                .instantiate(self.interner(), args)
                                .skip_norm_wip();
                            // An elided `&dyn Trait` of a function's is `&'r (dyn Trait + 'r)`,
                            // once `'r` is a lifetime of the signature's and not an error.
                            let mut defaults = match def.0 {
                                CallableDefId::FunctionId(function) => Some(ObjectDefaults {
                                    db: self.ctx.db,
                                    interner: self.interner(),
                                    keep_static: writes_static_object(
                                        &FunctionSignature::of(self.ctx.db, function).store,
                                    ),
                                    erase: false,
                                }),
                                _ => None,
                            };
                            // To a `for<'a> fn`, the item's signature keeps its own `for<'a>`,
                            // related to the target's as higher-ranked types are.
                            if let TyKind::FnPtr(to, _) = target.kind()
                                && !to.bound_vars().is_empty()
                            {
                                // An elided lifetime rust-analyzer leaves as an error region
                                // (`f: &mut Formatter<'_>`) is late-bound in rustc, so it is a
                                // lifetime of that `for<'a>` too, not one of the caller's: a
                                // variable of the caller could name no placeholder of the
                                // target and would have to outlive `'static`.
                                let interner = self.interner();
                                let bound_vars = sig.bound_vars();
                                let mut elided = 0;
                                let inner = rustc_type_ir::fold_regions(
                                    interner,
                                    sig.skip_binder(),
                                    |r, depth| match r.kind() {
                                        RegionKind::ReError(_) => {
                                            let var = rustc_type_ir::BoundVar::from_usize(
                                                bound_vars.len() + elided,
                                            );
                                            elided += 1;
                                            Region::new_bound(
                                                interner,
                                                depth,
                                                hir_ty::next_solver::BoundRegion {
                                                    var,
                                                    kind: rustc_type_ir::BoundRegionKind::Anon,
                                                },
                                            )
                                        }
                                        _ => r,
                                    },
                                );
                                let inner = match &mut defaults {
                                    Some(defaults) => {
                                        rustc_type_ir::TypeFoldable::fold_with(inner, defaults)
                                    }
                                    None => inner,
                                };
                                let bound_vars = hir_ty::next_solver::BoundVarKinds::new_from_slice(
                                    &mem::collect(
                                        self.ctx.frame,
                                        bound_vars.iter().chain(std::iter::repeat_n(
                                            rustc_type_ir::BoundVariableKind::Region(
                                                rustc_type_ir::BoundRegionKind::Anon,
                                            ),
                                            elided,
                                        )),
                                    ),
                                );
                                let sig = rustc_type_ir::Binder::bind_with_vars(inner, bound_vars);
                                let source = Ty::new_fn_ptr(interner, sig);
                                self.relate(source, target, Variance::Covariant, at);
                                return Some(target);
                            }
                            // rustc instantiates the item's late-bound lifetimes and proves
                            // its signature well-formed with them: `&'a &'b ()` that `'b: 'a`,
                            // when the target has none of its own.
                            let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                                hir_ty::Span::Dummy,
                                hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                                sig,
                            );
                            let sig = rustc_type_ir::fold_regions(self.interner(), sig, |r, _| {
                                match r.kind() {
                                    RegionKind::ReError(_) => self.fresh_elided(),
                                    _ => r,
                                }
                            });
                            let sig = match &mut defaults {
                                Some(defaults) => {
                                    rustc_type_ir::TypeFoldable::fold_with(sig, defaults)
                                }
                                None => sig,
                            };
                            let wf = At { category: Category::Boring, span };
                            for ty in sig.inputs_and_output.iter() {
                                self.well_formed(ty, wf, 0);
                            }
                            let source =
                                Ty::new_fn_ptr(self.interner(), rustc_type_ir::Binder::dummy(sig));
                            self.relate(source, target, Variance::Covariant, at);
                        }
                    }
                    // rustc's: the closure's signature as a `fn` pointer's, a subtype of the
                    // target, higher-ranked as both are: `for<'a> fn(<&'a () as Trait>::Ty)`
                    // of `|_| {}` normalizes the projection for a placeholder.
                    CastKind::PointerCoercion(hir_ty::PointerCast::ClosureFnPointer(_))
                        if let TyKind::Closure(_, args) = source.kind()
                            && let TyKind::FnPtr(to, _) = target.kind()
                            && !to.bound_vars().is_empty()
                            && let sig = args.as_closure().sig()
                            && let &[tupled] = sig.skip_binder().inputs().as_slice()
                            && let TyKind::Tuple(_) = tupled.kind() =>
                    {
                        let interner = self.interner();
                        let sig = interner
                            .signature_unclosure(sig, hir_ty::next_solver::abi::Safety::Safe);
                        self.relate(Ty::new_fn_ptr(interner, sig), target, Variance::Covariant, at);
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::ClosureFnPointer(_)) => {
                        if let TyKind::Closure(_, args) = source.kind()
                            && let TyKind::FnPtr(to, _) = target.kind()
                            && let sig = args.as_closure().sig().skip_binder()
                            && let &[tupled] = sig.inputs().as_slice()
                            && let TyKind::Tuple(inputs) = tupled.kind()
                            && let Some((&output, to_inputs)) =
                                to.skip_binder().inputs_and_output.as_slice().split_last()
                        {
                            for (input, &to) in inputs.iter().zip(to_inputs) {
                                self.relate(input, to, Variance::Contravariant, at);
                            }
                            self.relate(sig.output(), output, Variance::Covariant, at);
                        }
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::Unsize) => {
                        self.unsize(source, target, at)
                    }
                    // rust-analyzer's kind for any reference cast to a raw pointer: rustc's
                    // `&raw const *x` of the pointer's type, then for an array (`&[T; N] as
                    // *const T`) its element a subtype of what the pointer points to.
                    CastKind::PointerCoercion(hir_ty::PointerCast::ArrayToPointer) => {
                        if let TyKind::Ref(_, from, _) | TyKind::RawPtr(from, _) = source.kind()
                            && let TyKind::RawPtr(to, mutability) = target.kind()
                        {
                            let interner = self.interner();
                            let erase = |ty| {
                                rustc_type_ir::fold_regions(interner, ty, |_, _| {
                                    Region::new_erased(interner)
                                })
                            };
                            match from.kind() {
                                TyKind::Array(element, _) if erase(element) == erase(to) => {
                                    self.relate(element, to, Variance::Covariant, at)
                                }
                                _ => {
                                    let variance = match mutability {
                                        rustc_ast_ir::Mutability::Not => Variance::Covariant,
                                        rustc_ast_ir::Mutability::Mut => Variance::Invariant,
                                    };
                                    self.relate(from, to, variance, at)
                                }
                            }
                        }
                    }
                    // `*const [T; N]` to `*const T`, rustc's `ArrayToPointer`; and between
                    // pointers to trait objects, the object's lifetime may only shorten and the
                    // principal trait's arguments stay, else a vtable method could be called
                    // with what it cannot take (rust-lang/rust#136702).
                    CastKind::PtrToPtr => {
                        if let (TyKind::RawPtr(from, _), TyKind::RawPtr(to, _)) =
                            (source.kind(), target.kind())
                        {
                            let (from_tail, to_tail) = (self.tail(from), self.tail(to));
                            let interner = self.interner();
                            let erase = |ty| {
                                rustc_type_ir::fold_regions(interner, ty, |_, _| {
                                    Region::new_erased(interner)
                                })
                            };
                            if let TyKind::Array(element, _) = from.kind()
                                && erase(element) == erase(to)
                            {
                                self.relate(element, to, Variance::Covariant, at)
                            } else if let (
                                TyKind::Dynamic(from_predicates, from_region),
                                TyKind::Dynamic(to_predicates, to_region),
                            ) = (from_tail.kind(), to_tail.kind())
                            {
                                use rustc_type_ir::inherent::BoundExistentialPredicates as _;
                                match (from_predicates.principal(), to_predicates.principal()) {
                                    (Some(_), Some(_)) => {
                                        self.relate(from_tail, to_tail, Variance::Covariant, at)
                                    }
                                    (None, None) => self.relate_regions(
                                        from_region,
                                        to_region,
                                        Variance::Covariant,
                                        at,
                                    ),
                                    _ => (),
                                }
                            }
                        }
                    }
                    CastKind::PointerCoercion(hir_ty::PointerCast::MutToConstPointer) => {
                        if let (TyKind::RawPtr(x, _), TyKind::RawPtr(y, _)) =
                            (source.kind(), target.kind())
                        {
                            self.relate(x, y, Variance::Covariant, at);
                        }
                    }
                    _ => (),
                }
                Some(target)
            }
            Rvalue::Aggregate(AggregateKind::Adt(variant, args), operands) => {
                let store = self.ctx.store;
                let spelled = |expr| match store.expr(expr) {
                    hir_def::hir::Expr::RecordLit { path, .. } | hir_def::hir::Expr::Path(path) => {
                        spells_generics(path)
                    }
                    _ => false,
                };
                // A constructor called through a variable, `let f = Foo::<&'a u32>; f(x)`, has
                // the variable's arguments.
                let through_local = match span {
                    MirSpan::ExprId(expr) => match store.expr(expr) {
                        hir_def::hir::Expr::Call { callee, .. } => {
                            self.callee_local(callee).and_then(|local| {
                                let ty =
                                    self.regions.local_tys[local.into_raw().into_u32() as usize];
                                let ty = match ty.kind() {
                                    TyKind::Ref(_, inner, _) => inner,
                                    _ => ty,
                                };
                                match ty.kind() {
                                    TyKind::FnDef(def, args)
                                        if !matches!(def.0, CallableDefId::FunctionId(_)) =>
                                    {
                                        if let Some(location) = self.location {
                                            self.regions.callee_reads.push((local, location));
                                        }
                                        Some(args.no_bound_vars().unwrap())
                                    }
                                    _ => None,
                                }
                            })
                        }
                        _ => None,
                    },
                    _ => None,
                };
                let db = self.ctx.db;
                let adt_of = |variant: hir_def::VariantId| match variant {
                    hir_def::VariantId::EnumVariantId(it) => AdtId::EnumId(it.lookup(db).parent),
                    hir_def::VariantId::StructId(it) => AdtId::StructId(it),
                    hir_def::VariantId::UnionId(it) => AdtId::UnionId(it),
                };
                let names_self = |expr| match store.expr(expr) {
                    hir_def::hir::Expr::RecordLit { path, .. } | hir_def::hir::Expr::Path(path) => {
                        path.is_self_type()
                    }
                    _ => false,
                };
                // `Self { .. }` and `Self(..)` in an impl are the impl's self type, with its
                // lifetimes: in `impl MyStruct<'_>`, not a `MyStruct<'a>`.
                let self_args = match span {
                    MirSpan::ExprId(expr)
                        if names_self(expr)
                            || matches!(store.expr(expr),
                                hir_def::hir::Expr::Call { callee, .. } if names_self(callee)) =>
                    {
                        self.impl_self_args(adt_of(*variant))
                    }
                    _ => None,
                };
                let mut annotated = false;
                let args = match span {
                    _ if let Some(args) = self_args => args,
                    _ if let Some(args) = through_local => args,
                    MirSpan::ExprId(expr)
                        if spelled(expr)
                            || matches!(store.expr(expr),
                                hir_def::hir::Expr::Call { callee, .. } if spelled(callee)) =>
                    {
                        annotated = true;
                        self.written(args.as_ref())
                    }
                    _ => self.renumber(args.as_ref()),
                };
                // The arguments a path spells out are well-formed, as rustc's user type
                // annotation: `Outer::<T, Inner<T>> { .. }` where `struct Inner<T: 'static>`.
                if annotated {
                    for ty in args.types() {
                        self.well_formed(ty, boring, 0);
                    }
                    // `Some::<<(u8, _) as Tr>::Ty>(&temp())`: its argument is the annotation
                    // normalized.
                    let callee = match span {
                        MirSpan::ExprId(expr) => match store.expr(expr) {
                            hir_def::hir::Expr::Call { callee, .. } => Some(callee),
                            hir_def::hir::Expr::Path(_) => Some(expr),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(callee) = callee
                        && let hir_def::hir::Expr::Path(path) = store.expr(callee)
                    {
                        self.spelled_projections(callee, path, args);
                    }
                }
                let fields = self.ctx.db.field_types(*variant);
                for ((_, field), operand) in fields.iter().zip(operands.iter()) {
                    let field = field.ty().instantiate(self.interner(), args).skip_norm_wip();
                    if let Some(ty) = self.operand_ty(operand) {
                        let at = At { span: operand.span().unwrap_or(span), ..boring };
                        self.relate(ty, field, Variance::Covariant, at);
                    }
                }
                let adt = adt_of(*variant);
                self.predicates(adt.into(), args, boring, true);
                Some(Ty::new_adt(self.interner(), adt, args))
            }
            Rvalue::Aggregate(AggregateKind::Tuple(ty), operands) => {
                let ty = self.renumber(ty.as_ref());
                if let TyKind::Tuple(fields) = ty.kind() {
                    for (field, operand) in fields.iter().zip(operands.iter()) {
                        if let Some(it) = self.operand_ty(operand) {
                            self.relate(it, field, Variance::Covariant, boring);
                        }
                    }
                }
                Some(ty)
            }
            Rvalue::Aggregate(AggregateKind::Closure(ty), operands) => {
                let written = ty.as_ref();
                let ty = self.renumber(written);
                let frame = self.ctx.frame;
                let lifetimes = |parents: &[hir_ty::next_solver::GenericArg<'db>]| {
                    mem::collect(
                        frame,
                        parents.iter().enumerate().filter_map(|(index, arg)| match arg.kind() {
                            GenericArgKind::Lifetime(region) => Some((index as u32, region)),
                            _ => None,
                        }),
                    )
                };
                // rustc's `prove_aggregate_predicates`: a closure's where clauses are its
                // creator's, which hold where it is made, with its parent args: so
                // `for<'a> &'a S: 'a` is proven for a placeholder `'a`, needing `S: 'static`.
                let parent_args = match ty.kind() {
                    TyKind::Closure(_, args) => Some(args.as_closure().parent_args()),
                    TyKind::Coroutine(_, args) => Some(args.as_coroutine().parent_args()),
                    _ => None,
                };
                if let Some(parent_args) = parent_args {
                    let def = self.ctx.owner.generic_def(self.ctx.db);
                    let at = At { category: Category::Boring, span };
                    self.predicates(def, GenericArgs::new_from_slice(parent_args.as_slice()), at, true);
                }
                let made = match ty.kind() {
                    TyKind::Closure(def, args) => Some((
                        def.0.loc(self.ctx.db).expr,
                        args.as_closure().tupled_upvars_ty(),
                        lifetimes(args.as_closure().parent_args().as_slice()),
                    )),
                    // An `async` closure holds its captures as any closure does: `async || x`
                    // borrows `x` into its upvar, rustc's operand of `CoroutineClosure`.
                    TyKind::CoroutineClosure(def, args) => Some((
                        def.0.loc(self.ctx.db).expr,
                        args.as_coroutine_closure().tupled_upvars_ty(),
                        lifetimes(args.as_coroutine_closure().parent_args().as_slice()),
                    )),
                    TyKind::Coroutine(def, args) => {
                        let expr = def.0.loc(self.ctx.db).expr;
                        if let TyKind::Coroutine(_, typed) = written.kind() {
                            self.async_fn_output(
                                expr,
                                typed.as_coroutine().return_ty(),
                                args.as_coroutine().return_ty(),
                                span,
                            );
                        }
                        Some((
                            expr,
                            args.as_coroutine().tupled_upvars_ty(),
                            lifetimes(args.as_coroutine().parent_args().as_slice()),
                        ))
                    }
                    _ => None,
                };
                if let Some((expr, upvars, parents)) = made {
                    if let TyKind::Tuple(upvars) = upvars.kind() {
                        for (upvar, operand) in upvars.iter().zip(operands.iter()) {
                            if let Some(it) = self.operand_ty(operand) {
                                let at = At {
                                    category: Category::Boring,
                                    span: operand.span().unwrap_or(span),
                                };
                                self.relate(it, upvar, Variance::Covariant, at);
                            }
                        }
                    }
                    self.closure_requirements(expr, ty, parents, span);
                }
                Some(ty)
            }
            _ => None,
        }
    }

    /// rustc's `add_reborrow_constraint`: the loan of `*p` outlives nothing longer than the
    /// reference `p` is, up to the first shared reference.
    fn reborrow_constraints(&mut self, local: LocalId, projection: &[PlaceElem], loan: Rv, at: At) {
        for index in (0..projection.len()).rev() {
            if projection[index] != ProjectionElem::Deref {
                continue;
            }
            let base = self.place_ty(local, &projection[..index]).ty;
            match base.kind() {
                TyKind::Ref(region, _, mutability) => {
                    if let Some(region) = rv_of(region) {
                        self.outlives(region, loan, at);
                    }
                    if mutability == rustc_ast_ir::Mutability::Not {
                        return;
                    }
                }
                TyKind::RawPtr(..) => return,
                _ => (),
            }
        }
        // A capture by reference is rustc's `*(*env).i`, reborrowed through the closure's
        // reference to itself too: `|| &mut x` returns what lives only as long as the call.
        // A coroutine's body has itself by value, as rustc checks it.
        if projection.first() == Some(&ProjectionElem::Deref)
            && self.ctx.upvars.iter().any(|it| it.local == local && it.by_ref)
            && let Some(env) = self.ctx.closure_local
            && let TyKind::Ref(region, env, _) =
                self.regions.local_tys[env.into_raw().into_u32() as usize].kind()
            && !matches!(env.kind(), TyKind::Coroutine(..))
            && let Some(region) = rv_of(region)
        {
            self.outlives(region, loan, at);
        }
    }

    fn unsize(&mut self, source: Ty<'db>, target: Ty<'db>, at: At) {
        match (source.kind(), target.kind()) {
            (TyKind::Ref(ra, ta, _), TyKind::Ref(rb, tb, _)) => {
                self.relate_regions(ra, rb, Variance::Covariant, at);
                self.unsize_pointee(ta, tb, at);
            }
            (TyKind::RawPtr(ta, _), TyKind::RawPtr(tb, _)) => self.unsize_pointee(ta, tb, at),
            (TyKind::Adt(da, aa), TyKind::Adt(db, ab)) if da == db => {
                for (x, y) in aa.iter().zip(ab.iter()) {
                    match (x.kind(), y.kind()) {
                        // `Cell<T>: CoerceUnsized<Cell<U>>` where `T: CoerceUnsized<U>`: a
                        // pointer in it coerces as one, `&'a mut (dyn Trait + 'static)` to
                        // `&'a mut (dyn Trait + 'a)`.
                        (GenericArgKind::Type(x), GenericArgKind::Type(y)) => {
                            let pointer = |ty: Ty<'db>| {
                                matches!(
                                    ty.kind(),
                                    TyKind::Adt(..) | TyKind::Ref(..) | TyKind::RawPtr(..)
                                )
                            };
                            if pointer(x) && pointer(y) {
                                self.unsize(x, y, at);
                            } else {
                                self.unsize_pointee(x, y, at);
                            }
                        }
                        // The impls keep a lifetime as it is: `Ref<'b, T>` to `Ref<'b, U>`.
                        (GenericArgKind::Lifetime(x), GenericArgKind::Lifetime(y)) => {
                            self.relate_regions(x, y, Variance::Invariant, at)
                        }
                        _ => (),
                    }
                }
            }
            _ => (),
        }
    }

    /// What a pointer to `ty` is wide for, with its regions: its struct's last field, and so on.
    fn tail(&mut self, ty: Ty<'db>) -> Ty<'db> {
        let db = self.ctx.db;
        hir_ty::layout::struct_tail_erasing_lifetimes(db, ty, |field| {
            match rustc_type_ir::TypeVisitableExt::has_aliases(&field) {
                true => self.normalize(field).unwrap_or(field),
                false => field,
            }
        })
    }

    fn unsize_pointee(&mut self, source: Ty<'db>, target: Ty<'db>, at: At) {
        match (source.kind(), target.kind()) {
            // rustc's `[T; N]: Unsize<[T]>` is of one `T`: the slice's elements are the array's.
            (TyKind::Array(x, _), TyKind::Slice(y)) => self.relate(x, y, Variance::Invariant, at),
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) => {
                self.relate(source, target, Variance::Covariant, at)
            }
            // rustc's struct unsizing, `RefCell<T>: Unsize<RefCell<dyn Trait>>`: the argument
            // its tail unsizes does so, and the others are the same.
            (TyKind::Adt(source_def, from), TyKind::Adt(target_def, to))
                if source_def == target_def =>
            {
                let interner = self.interner();
                let erase = |ty| {
                    rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
                };
                for (x, y) in from.iter().zip(to.iter()) {
                    match (x.kind(), y.kind()) {
                        (GenericArgKind::Type(x), GenericArgKind::Type(y))
                            if erase(x) == erase(y) =>
                        {
                            self.relate(x, y, Variance::Invariant, at)
                        }
                        (GenericArgKind::Type(x), GenericArgKind::Type(y)) => {
                            self.unsize_pointee(x, y, at)
                        }
                        (GenericArgKind::Lifetime(x), GenericArgKind::Lifetime(y)) => {
                            self.relate_regions(x, y, Variance::Invariant, at)
                        }
                        _ => (),
                    }
                }
            }
            // The value implements the object's trait, with what the impl proving it needs:
            // `impl<T: 'static> Gettable<T> for S<T>` makes `&S<&'a u8> as &dyn Gettable<_>`
            // need `'a: 'static`; and what the trait says its types outlive, `T: 'static` of a
            // `T` as `dyn Any + 'a`.
            (_, TyKind::Dynamic(predicates, region)) => {
                self.type_outlives(source, region, at);
                for predicate in predicates.iter() {
                    if let rustc_type_ir::ExistentialPredicate::Trait(principal) =
                        predicate.skip_binder()
                    {
                        let trait_ref = principal.with_self_ty(self.interner(), source);
                        let mut regions = self.scratch.regions.take();
                        self.trait_outlives(trait_ref, &mut regions);
                        for &region in regions.iter() {
                            self.type_outlives(source, region, at);
                        }
                        self.scratch.regions.give(regions);
                        self.prove(trait_ref, at);
                    }
                }
            }
            _ => self.relate(source, target, Variance::Covariant, at),
        }
    }

    fn terminator(&mut self, kind: &TerminatorKind, span: MirSpan) {
        kind.for_each_operand(&mut |operand| self.copy_bound(operand, span));
        match kind {
            TerminatorKind::Call { func, args, .. } | TerminatorKind::TailCall { func, args, .. } => {
                // A tail call's callee writes this body's return place (rustc's
                // `check_call_dest`).
                let (dest_local, dest_projection) = match kind {
                    TerminatorKind::Call { destination, .. } => {
                        (destination.local, destination.projection.as_slice())
                    }
                    _ => (crate::return_slot(), &[][..]),
                };
                // A function item called through a variable, `let f = foo::<'a>; f()`, which
                // rust-analyzer lowers as a call of the constant, has the variable's type.
                // Through a reference to it too (`let f = &foo::<'a>; f()`).
                let peel = |mut ty: Ty<'db>| {
                    while let TyKind::Ref(_, inner, _) = ty.kind() {
                        ty = inner;
                    }
                    ty
                };
                let local = match (&func.kind, span) {
                    (
                        OperandKind::Constant { .. } | OperandKind::Allocation { .. },
                        MirSpan::ExprId(call),
                    ) => match self.ctx.store.expr(call) {
                        hir_def::hir::Expr::Call { callee, .. } => self
                            .path_local(callee)
                            .filter(|&it| {
                                matches!(
                                    (peel(self.regions.local_tys[it.into_raw().into_u32() as usize]).kind(),
                                        self.operand_ty(func).map(|it| it.kind())),
                                    (TyKind::FnDef(a, _), Some(TyKind::FnDef(b, _))) if a == b
                                )
                            }),
                        _ => None,
                    },
                    _ => None,
                };
                if let (Some(local), Some(location)) = (local, self.location) {
                    self.regions.callee_reads.push((local, location));
                }
                // rustc's MIR copies the variable into a temporary it calls, `_6 = copy _4;
                // move _6(..)`, whose type has regions of its own the variable's flow into.
                let local_callee = local.map(|it| {
                    let ty = peel(self.regions.local_tys[it.into_raw().into_u32() as usize]);
                    let copy = self.renumber(ty);
                    let callee = match span {
                        MirSpan::ExprId(call) => match self.ctx.store.expr(call) {
                            hir_def::hir::Expr::Call { callee, .. } => MirSpan::ExprId(callee),
                            _ => span,
                        },
                        _ => span,
                    };
                    let at = At { category: Category::Boring, span: callee };
                    self.relate(ty, copy, Variance::Covariant, at);
                    copy
                });
                let Some(func_ty) = local_callee
                    .or_else(|| self.written_callee(func, span))
                    .or_else(|| self.constant_ty(func, span))
                    .or_else(|| self.operand_ty(func))
                else {
                    return;
                };
                let (mut inputs, output) = match func_ty.kind() {
                    TyKind::FnDef(def, fn_args) => {
                        let fn_args = fn_args.no_bound_vars().unwrap();
                        let def: CallableDefId = def.0;
                        let sig = self
                            .ctx
                            .db
                            .callable_item_signature(def)
                            .instantiate(self.interner(), fn_args)
                            .skip_norm_wip();
                        let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                            hir_ty::Span::Dummy,
                            hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                            sig,
                        );
                        self.callee_predicates(def, fn_args, span);
                        let wf = At { category: Category::Boring, span };
                        for ty in fn_args.types() {
                            self.well_formed(ty, wf, 0);
                        }
                        if let CallableDefId::FunctionId(function) = def
                            && let Some(expr) = self.callee_expr(func, span)
                        {
                            let container = function.lookup(self.ctx.db).container;
                            self.path_well_formed(expr, container, fn_args, span);
                        }
                        let mut impl_self = None;
                        let (self_param, keep_static, returns_self, static_self) = match def {
                            CallableDefId::FunctionId(it) => {
                                self.error_region |= signature_lifetime_error(self.ctx.db, it);
                                let written = FunctionSignature::of(self.ctx.db, it);
                                if let ItemContainerId::ImplId(impl_) =
                                    it.lookup(self.ctx.db).container
                                    && written.params.len() == sig.inputs().len()
                                {
                                    let ty = self
                                        .ctx
                                        .db
                                        .impl_self_ty(impl_)
                                        .instantiate(self.interner(), fn_args)
                                        .skip_norm_wip();
                                    let writes: FVec<'_, bool> = mem::collect(
                                        self.ctx.frame,
                                        written.params.iter().map(|&param| writes_self(&written.store, param)),
                                    );
                                    let ret = written
                                        .ret_type
                                        .is_some_and(|it| writes_self(&written.store, it));
                                    impl_self = Some((ty, writes, ret));
                                }
                                let returns_self = written.ret_type.is_some_and(|it| {
                                    matches!(written.store.type_ref(it),
                                        hir_def::hir::type_ref::TypeRef::Path(path)
                                            if path.is_self_type())
                                });
                                (
                                    written.has_self_param(),
                                    writes_static_object(&written.store),
                                    returns_self,
                                    written.has_self_param() && self.impl_of_static_object(it),
                                )
                            }
                            _ => (false, true, false, false),
                        };
                        let function = match def {
                            CallableDefId::FunctionId(it) => Some(it),
                            _ => None,
                        };
                        let mut inputs = self.scratch.tys.take();
                        let output = self.elide(
                            function,
                            sig.inputs().as_slice(),
                            sig.output(),
                            self_param,
                            keep_static,
                            returns_self,
                            static_self,
                            impl_self,
                            &mut inputs,
                        );
                        // The signature as written is well-formed, `&'a &'b ()` that `'b: 'a`
                        // even where it normalizes to `()`; and the where clauses of its
                        // associated types hold.
                        let wf = At { category: Category::Boring, span };
                        for ty in inputs.iter().copied().chain([output]) {
                            self.well_formed(ty, wf, 0);
                        }
                        let at = At { category: Category::Predicate, span };
                        let mut found = self.scratch.projections.take();
                        for ty in inputs.iter().copied().chain([output]) {
                            projections(ty, &mut found);
                            for (def, args) in found.drain(..) {
                                if let Ok(def) = GenericDefId::try_from(def) {
                                    self.predicates(def, args, at, false);
                                }
                            }
                        }
                        self.scratch.projections.give(found);
                        (inputs, output)
                    }
                    TyKind::FnPtr(sig_tys, header) => {
                        let sig = sig_tys.with(header);
                        let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                            hir_ty::Span::Dummy,
                            hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                            sig,
                        );
                        let mut inputs = self.scratch.tys.take();
                        inputs.extend(sig.inputs().iter().copied());
                        (inputs, sig.output())
                    }
                    _ => return,
                };
                // rustc builds a struct or a variant with a constructor's arguments as an
                // aggregate, whose fields are `Boring` to blame.
                let constructor = matches!(
                    func_ty.kind(),
                    TyKind::FnDef(def, _) if !matches!(def.0, CallableDefId::FunctionId(_))
                );
                let category = if constructor { Category::Boring } else { Category::CallArgument };
                // `Fn::call(&f, (x,))` that lowering passes as `Fn::call(&f, x)`: each argument
                // is its element of the signature's tuple.
                let lang = self.interner().lang_items();
                let calls = [lang.Fn_call, lang.FnMut_call_mut, lang.FnOnce_call_once];
                let untupled = match (func_ty.kind(), inputs.as_slice()) {
                    (TyKind::FnDef(def, _), &[receiver, tupled])
                        if let CallableDefId::FunctionId(function) = def.0
                            && calls.contains(&Some(function))
                            && let TyKind::Tuple(elements) = tupled.kind()
                            && args.len() == 1 + elements.len()
                            && !(args.len() == 2
                                && self
                                    .operand_ty(&args[1])
                                    .is_some_and(|it| matches!(it.kind(), TyKind::Tuple(_)))) =>
                    {
                        Some((receiver, elements))
                    }
                    _ => None,
                };
                if let Some((receiver, elements)) = untupled {
                    inputs.clear();
                    inputs.push(receiver);
                    inputs.extend(elements.iter());
                }
                for (arg, input) in args.iter().zip(inputs.iter().copied()) {
                    let fn_item = arg.span().and_then(|it| self.fn_item(arg, it));
                    if let Some(ty) = fn_item.or_else(|| self.operand_ty(arg)) {
                        let at = At { category, span };
                        self.relate(ty, input, Variance::Covariant, at);
                    }
                }
                self.scratch.tys.give(inputs);
                // A closure called through `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once`,
                // which lowering passes the arguments of one by one: proving it implements the
                // trait equates the arguments with its signature's inputs, whose regions are in
                // its type (`let f = |_| {}; f(&x)`).
                if let TyKind::FnDef(def, _) = func_ty.kind()
                    && let CallableDefId::FunctionId(function) = def.0
                    && calls.contains(&Some(function))
                    && let [receiver, given @ ..] = &args[..]
                    && let Some(receiver) = self.operand_ty(receiver)
                    && let TyKind::Closure(id, closure) = peel(receiver).kind()
                    && self.infers_signature(id.0)
                {
                    let sig = self.ctx.infcx.instantiate_binder_with_fresh_vars(
                        hir_ty::Span::Dummy,
                        hir_ty::next_solver::infer::BoundRegionConversionTime::FnCall,
                        closure.as_closure().sig(),
                    );
                    if let &[tupled] = sig.inputs().as_slice()
                        && let TyKind::Tuple(inputs) = tupled.kind()
                    {
                        for (arg, input) in given.iter().zip(inputs.iter()) {
                            if let Some(ty) = self.operand_ty(arg) {
                                let at = At {
                                    category: Category::CallArgument,
                                    span: arg.span().unwrap_or(span),
                                };
                                self.relate(ty, input, Variance::Covariant, at);
                            }
                        }
                    }
                    let dest = self.place_ty(dest_local, dest_projection).ty;
                    let at = At {
                        category: self.category_of_assignment(dest_local, dest_projection),
                        span,
                    };
                    self.relate(sig.output(), dest, Variance::Covariant, at);
                }
                let dest = self.place_ty(dest_local, dest_projection).ty;
                let at = At {
                    category: self.category_of_assignment(dest_local, dest_projection),
                    span,
                };
                self.relate(output, dest, Variance::Covariant, at);
            }
            TerminatorKind::DropAndReplace { place, value, .. } => {
                let projection = place.projection.as_slice();
                let dest = self.place_ty(place.local, projection).ty;
                if let Some(ty) = self.operand_ty(value) {
                    let at =
                        At { category: self.category_of_assignment(place.local, projection), span };
                    self.relate(ty, dest, Variance::Covariant, at);
                }
            }
            _ => (),
        }
    }

    /// Whether `function` is of an impl for a `dyn Trait + 'static`.
    fn impl_of_static_object(&self, function: hir_def::FunctionId) -> bool {
        let ItemContainerId::ImplId(impl_) = function.lookup(self.ctx.db).container else {
            return false;
        };
        let ty = self.ctx.db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        matches!(ty.kind(), TyKind::Dynamic(_, region) if matches!(region.kind(), RegionKind::ReStatic))
    }

    /// Whether the closure `id` is of this body and writes no type of a parameter or of its
    /// return: the lifetimes rustc infers for its signature are in its type, the same for every
    /// call, where those a written `&T` elides are the call's own.
    fn infers_signature(&self, id: hir_ty::db::InternedClosureId<'db>) -> bool {
        let loc = id.loc(self.ctx.db);
        loc.owner == self.ctx.owner
            && matches!(
                self.ctx.store.expr(loc.expr),
                hir_def::hir::Expr::Closure { arg_types, ret_type: None, .. }
                    if arg_types.iter().all(Option::is_none)
            )
    }

    /// A callee's signature with the lifetimes it elides, which RA leaves as errors, made
    /// region variables by the elision rules: each elided input lifetime its own, an elided
    /// output lifetime that of `self` when `self` is a reference, else that of the only
    /// lifetime of the inputs. The default lifetime of a trait object is not one of them.
    /// `static_self`: `self` is of an impl for `dyn Trait`, whose lifetime the impl's header
    /// defaults to `'static` (`impl dyn Trait { fn f(&self) }`), not the reference's.
    /// An `impl Trait` `function` returns has, for each lifetime its parameters leave out, that
    /// lifetime's region.
    /// `impl_self`: the self type of the impl `function` is in, which parameters write `Self`
    /// and whether the return type does. The impl's anonymous lifetimes (`impl Foo<'_>`) are
    /// one region for each call, and lifetime elision counts none of the impl's lifetimes in
    /// what writes `Self`, as rustc looks only at those a parameter's type writes or elides.
    #[allow(clippy::too_many_arguments)]
    fn elide(
        &mut self,
        function: Option<hir_def::FunctionId>,
        inputs: &[Ty<'db>],
        output: Ty<'db>,
        self_param: bool,
        keep_static: bool,
        returns_self: bool,
        static_self: bool,
        impl_self: Option<(Ty<'db>, FVec<'_, bool>, bool)>,
        out: &mut FVec<'a, Ty<'db>>,
    ) -> Ty<'db> {
        let interner = self.interner();
        let mut defaults = ObjectDefaults { db: self.ctx.db, interner, keep_static, erase: false };
        let mut errors = filled(self.ctx.frame, inputs.len(), Vec::new_in(self.ctx.frame));
        let (impl_self, writes_self, ret_self) = match impl_self {
            Some((raw, writes, ret)) => {
                let named =
                    rustc_type_ir::fold_regions(interner, raw, |region, _| match region.kind() {
                        RegionKind::ReError(_) => self.fresh(),
                        _ => region,
                    });
                (Some((raw, named)), writes, ret)
            }
            None => (None, Vec::new_in(self.ctx.frame), false),
        };
        let writes_self = |index: usize| writes_self.get(index).copied().unwrap_or(false);
        let output = match impl_self {
            Some(replace) if ret_self => {
                rustc_type_ir::TypeFoldable::fold_with(output, &mut Replace::new(interner, replace))
            }
            _ => output,
        };
        out.extend(inputs.iter().copied().enumerate().map(|(index, ty)| {
            let ty = match impl_self {
                Some(replace) if writes_self(index) => rustc_type_ir::TypeFoldable::fold_with(
                    ty,
                    &mut Replace::new(interner, replace),
                ),
                _ => ty,
            };
            let ty = rustc_type_ir::fold_regions(interner, ty, |region, _| match region.kind() {
                RegionKind::ReError(_) => {
                    let region = self.fresh_elided();
                    errors[index].push(region);
                    region
                }
                _ => region,
            });
            if static_self && index == 0 {
                return ty;
            }
            rustc_type_ir::TypeFoldable::fold_with(ty, &mut defaults)
        }));
        let inputs: &[Ty<'db>] = out;
        let self_region = match inputs.first().map(|it| it.kind()) {
            Some(TyKind::Ref(region, ..)) if self_param => Some(region),
            _ => None,
        };
        let mut distinct = self.scratch.regions.take();
        let elided = self_region.or_else(|| {
            for (index, ty) in inputs.iter().enumerate() {
                let ty = match impl_self {
                    Some((_, named)) if writes_self(index) => {
                        let named = match static_self && index == 0 {
                            true => named,
                            false => rustc_type_ir::TypeFoldable::fold_with(named, &mut defaults),
                        };
                        let unit = Ty::new_unit(interner);
                        rustc_type_ir::TypeFoldable::fold_with(
                            *ty,
                            &mut Replace::new(interner, (named, unit)),
                        )
                    }
                    _ => *ty,
                };
                rustc_type_ir::fold_regions(interner, ty, |region, _| {
                    if !matches!(region.kind(), RegionKind::ReBound(..))
                        && !distinct.contains(&region)
                    {
                        distinct.push(region);
                    }
                    region
                });
            }
            match distinct.as_slice() {
                [only] => Some(*only),
                _ => None,
            }
        });
        self.scratch.regions.give(distinct);
        let output = match function {
            Some(function) => {
                let mut regions = Vec::new_in(self.ctx.frame);
                hir_ty::elided_lifetimes(self.ctx.db, function).regions_into(&errors, &mut regions);
                hir_ty::with_elided_regions(self.ctx.db, output, function, &regions)
            }
            None => output,
        };
        // A return of `Self` elides nothing: its anonymous lifetimes are the impl's,
        // `impl Foo<'_>`, which rust-analyzer leaves as errors too.
        let output =
            rustc_type_ir::fold_regions(interner, output, |region, _| match region.kind() {
                RegionKind::ReError(_) if returns_self => self.fresh(),
                RegionKind::ReError(_) => elided.unwrap_or_else(|| self.fresh_elided()),
                _ => region,
            });
        rustc_type_ir::TypeFoldable::fold_with(output, &mut defaults)
    }

    /// The where clauses of what a call calls, which the caller must prove.
    fn callee_predicates(&mut self, def: CallableDefId, args: GenericArgs<'db>, span: MirSpan) {
        let generic_def: GenericDefId = match def {
            CallableDefId::FunctionId(it) => it.into(),
            CallableDefId::StructId(it) => it.into(),
            CallableDefId::EnumVariantId(it) => it.lookup(self.ctx.db).parent.into(),
        };
        let at = At { category: Category::Predicate, span };
        self.predicates(generic_def, args, at, true);
    }

    /// The where clauses of `def` with `args`, which hold where it is used. Without
    /// `type_outlives`, only those between regions.
    fn predicates(
        &mut self,
        def: GenericDefId,
        args: GenericArgs<'db>,
        at: At,
        type_outlives: bool,
    ) {
        let interner = self.interner();
        let mut clauses = self.scratch.clauses.take();
        clauses.extend(
            GenericPredicates::query_all(self.ctx.db, def)
                .iter_instantiated(interner, args.as_slice())
                .map(|it| it.skip_norm_wip()),
        );
        // What an ADT's fields need of its lifetimes, rustc's `inferred_outlives_of`, holds
        // where it is used as its where clauses do.
        if let GenericDefId::AdtId(adt) = def
            && args.regions().next().is_some()
        {
            for predicate in self.inferred.of(self.ctx.db, adt) {
                let OutlivesClause(arg, region) =
                    rustc_type_ir::EarlyBinder::bind(interner, predicate)
                        .instantiate(interner, args)
                        .skip_norm_wip();
                use rustc_type_ir::Upcast as _;
                clauses.push(match arg.kind() {
                    GenericArgKind::Type(ty) => OutlivesClause(ty, region).upcast(interner),
                    GenericArgKind::Lifetime(longer) => {
                        OutlivesClause(longer, region).upcast(interner)
                    }
                    GenericArgKind::Const(_) => continue,
                });
            }
        }
        self.clauses(&clauses, at, type_outlives);
        self.scratch.clauses.give(clauses);
    }

    /// `clauses`, which hold where `at` is.
    fn clauses(&mut self, clauses: &[Clause<'db>], at: At, type_outlives: bool) {
        // Outliving every `'a` of the clause's own `for<'a>` is outliving `'static`.
        let forall = |region: Region<'db>| match region.kind() {
            RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(binder), _) => {
                binder == rustc_type_ir::INNERMOST
            }
            _ => false,
        };
        for &clause in clauses {
            match clause.kind().skip_binder() {
                ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {
                    let shorter = match forall(shorter) {
                        true => self.static_region(),
                        false => shorter,
                    };
                    if let (Some(longer), Some(shorter)) =
                        (self.region_rv(longer), self.region_rv(shorter))
                    {
                        self.outlives(longer, shorter, at);
                    }
                }
                ClauseKind::TypeOutlives(OutlivesClause(ty, region)) if type_outlives => {
                    // A region of the type bound by the clause's `for<'a>` is a placeholder,
                    // taken as `'static` as a placeholder in a type is where it is proven:
                    // `for<'a> &'a S: 'a` needs `S: 'static`.
                    let interner = self.interner();
                    let ty =
                        rustc_type_ir::fold_regions(interner, ty, |it, depth| match it.kind() {
                            RegionKind::ReBound(
                                rustc_type_ir::BoundVarIndexKind::Bound(binder),
                                _,
                            ) if binder == depth => Region::new_static(interner),
                            _ => it,
                        });
                    if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) {
                        continue;
                    }
                    let original = region;
                    let region = match region.kind() {
                        RegionKind::ReStatic => self.static_region(),
                        _ if forall(region) => self.static_region(),
                        _ => region,
                    };
                    let tests = self.regions.type_tests.len();
                    self.type_outlives(ty, region, at);
                    if forall(original) {
                        for test in &mut self.regions.type_tests[tests..] {
                            test.forall = true;
                        }
                    }
                }
                // `I: IntoIterator<Item = T>`: what the projection normalizes to is `T`, so
                // `v.extend(d.iter())` makes the borrow of `d` the vector's element lifetime.
                ClauseKind::Projection(projection)
                    if type_outlives
                        && !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(
                            &projection,
                        ) =>
                {
                    let interner = self.interner();
                    if let Some(term) = projection.term.as_type()
                        && let Some(alias) = projection
                            .projection_term
                            .to_term(interner, rustc_type_ir::IsRigid::No)
                            .as_type()
                        && rustc_type_ir::TypeVisitableExt::has_free_regions(&alias)
                        && let Some(normalized) = self.normalize(alias)
                        && !matches!(normalized.kind(), TyKind::Alias(..) | TyKind::Infer(_))
                    {
                        self.relate(normalized, term, Variance::Invariant, at);
                    }
                }
                ClauseKind::Trait(predicate) if type_outlives => {
                    let trait_ref = predicate.trait_ref;
                    let mut regions = self.scratch.regions.take();
                    self.trait_outlives(trait_ref, &mut regions);
                    for &region in regions.iter() {
                        self.type_outlives(trait_ref.self_ty(), region, at);
                    }
                    self.scratch.regions.give(regions);
                    self.caller_bound(trait_ref, at);
                    self.closure_bound(trait_ref, at);
                    let trait_ref = self.normalize_closure_args(trait_ref);
                    // `for<'a> G: Outlives<'a>` is proven for a placeholder `'a`, which a
                    // region outlives only by outliving `'static`.
                    let forall =
                        rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref);
                    let interner = self.interner();
                    fn instantiate<'db>(
                        interner: hir_ty::next_solver::DbInterner<'db>,
                        trait_ref: hir_ty::next_solver::TraitRef<'db>,
                        mut with: impl FnMut(hir_ty::next_solver::BoundRegion<'db>) -> Region<'db>,
                    ) -> hir_ty::next_solver::TraitRef<'db> {
                        rustc_type_ir::fold_regions(interner, trait_ref, |region, depth| {
                            match region.kind() {
                                RegionKind::ReBound(
                                    rustc_type_ir::BoundVarIndexKind::Bound(binder),
                                    bound,
                                ) if binder == depth => with(bound),
                                _ => region,
                            }
                        })
                    }
                    let placeholders = forall.then(|| {
                        let universe = self.ctx.infcx.create_next_universe();
                        instantiate(interner, trait_ref, |bound| {
                            Region::new_placeholder(
                                interner,
                                hir_ty::next_solver::PlaceholderRegion::new(universe, bound),
                            )
                        })
                    });
                    let trait_ref = match forall {
                        true => instantiate(interner, trait_ref, |_| Region::new_static(interner)),
                        false => trait_ref,
                    };
                    self.trait_args_outlive(trait_ref, at);
                    self.object_assoc_bounds(trait_ref, at);
                    // Proven for placeholders, what the trait's impls need of them is an error
                    // where it is more than they can be, `for<'a> Foo<&'a isize>` needing
                    // `'a: 'b`.
                    if let Some(placeholders) = placeholders {
                        self.prove(placeholders, at);
                    }
                    // Of a call written as one: `map[i] = v` calls `IndexMut::index_mut`, which
                    // is E0594 where only `Index` is implemented.
                    if self.prove(trait_ref, at)
                        && !forall
                        && at.category == Category::Predicate
                        && let MirSpan::ExprId(expr) = at.span
                        && matches!(
                            self.ctx.store.expr(expr),
                            hir_def::hir::Expr::Call { .. } | hir_def::hir::Expr::MethodCall { .. }
                        )
                    {
                        self.ctx.tainted.set(true);
                    }
                }
                _ => (),
            }
        }
    }

    /// The regions a type implementing `trait_ref` outlives, by the trait's `Self: 'r` bounds:
    /// its own, instantiated, and `'static` from its supertraits.
    fn trait_outlives(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        regions: &mut FVec<'a, Region<'db>>,
    ) {
        let db = self.ctx.db;
        if static_trait(db, trait_ref.def_id.0) {
            regions.push(self.static_region());
        }
        for clause in GenericPredicates::query_all(db, trait_ref.def_id.0.into())
            .iter_instantiated(self.interner(), trait_ref.args.as_slice())
        {
            // `trait Trait<'a>: 'a` of `Trait<'static>` too.
            if let ClauseKind::TypeOutlives(OutlivesClause(ty, region)) =
                clause.skip_norm_wip().kind().skip_binder()
                && ty == trait_ref.self_ty()
                && !matches!(region.kind(), RegionKind::ReBound(..))
            {
                regions.push(match region.kind() {
                    RegionKind::ReStatic => self.static_region(),
                    _ => region,
                });
            }
        }
    }

    /// The trait's where clauses on its arguments, which hold where `trait_ref` is well-formed:
    /// `trait Trait<ARG: 'static>` of `Trait<&'a str>` is `'a: 'static`.
    /// Of a trait object proving its own trait, what the trait's bounds on an associated type
    /// need of the type the object says it is, as rustc's object candidate checks them:
    /// `dyn for<'a> Tr<'a, A = &'b u8>: Tr<'static>` with `type A: 'a` needs `'b: 'static`.
    fn object_assoc_bounds(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        use rustc_type_ir::inherent::Term as _;
        let TyKind::Dynamic(predicates, _) = trait_ref.self_ty().kind() else { return };
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref) {
            return;
        }
        let interner = self.interner();
        for predicate in predicates.iter() {
            let rustc_type_ir::ExistentialPredicate::Projection(projection) =
                predicate.skip_binder()
            else {
                continue;
            };
            let hir_ty::next_solver::TermId::TypeAliasId(assoc) = projection.def_id.0 else {
                continue;
            };
            if assoc.lookup(self.ctx.db).container != ItemContainerId::TraitId(trait_ref.def_id.0) {
                continue;
            }
            let Some(term) = projection.term.as_type() else { continue };
            if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&term) {
                continue;
            }
            let kind = rustc_type_ir::AliasTyKind::Projection {
                def_id: hir_ty::next_solver::TraitAssocTyId(assoc),
            };
            let bounds = rustc_type_ir::Interner::item_bounds(interner, alias_def_id(kind));
            for clause in bounds.iter_instantiated(interner, trait_ref.args.as_slice()) {
                if let ClauseKind::TypeOutlives(OutlivesClause(bounded, region)) =
                    clause.kind().skip_binder()
                    && let TyKind::Alias(_, alias) = bounded.kind()
                    && alias.kind == kind
                    && !matches!(region.kind(), RegionKind::ReBound(..))
                {
                    let region = match region.kind() {
                        RegionKind::ReStatic => self.static_region(),
                        _ => region,
                    };
                    self.type_outlives(term, region, at);
                }
            }
        }
    }

    fn trait_args_outlive(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref) {
            return;
        }
        let db = self.ctx.db;
        for clause in GenericPredicates::query_all(db, trait_ref.def_id.0.into())
            .iter_instantiated(self.interner(), trait_ref.args.as_slice())
        {
            let region = |this: &mut Self, region: Region<'db>| match region.kind() {
                RegionKind::ReStatic => Some(this.static_region()),
                RegionKind::ReBound(..) => None,
                _ => Some(region),
            };
            match clause.skip_norm_wip().kind().skip_binder() {
                ClauseKind::TypeOutlives(OutlivesClause(ty, sub))
                    if ty != trait_ref.self_ty()
                        && !rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) =>
                {
                    if let Some(sub) = region(self, sub) {
                        self.type_outlives(ty, sub, at);
                    }
                }
                ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {
                    if let Some(longer) = region(self, longer)
                        && let Some(shorter) = region(self, shorter)
                        && let (Some(longer), Some(shorter)) =
                            (self.region_rv(longer), self.region_rv(shorter))
                    {
                        self.outlives(longer, shorter, at);
                    }
                }
                _ => (),
            }
        }
    }

    /// `F: FnOnce(S, T)` of a closure of this body typed against an expected signature: rustc
    /// gives the closure that signature, so its parameters' types are the bound's arguments
    /// (`|z: &[i32]|` passed as `F` takes the `S` it is called with). A closure typed without
    /// one has a higher-ranked signature in rustc, which this closure's one signature cannot
    /// stand for. Of a bound with `for<'r>`, rustc's closure signature binds `'r` too, so
    /// only the bound's other regions are related: `impl FnOnce(&mut F<'a, 'b>)` makes the
    /// closure's `F<'1, '2>` that `F<'a, 'b>`, and leaves its `&'r` its own.
    fn closure_bound(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        use rustc_type_ir::inherent::{GenericArg as _, GenericArgs as _};
        let TyKind::Closure(def, args) = trait_ref.self_ty().kind() else { return };
        let loc = def.0.loc(self.ctx.db);
        let lang = self.interner().lang_items();
        if loc.owner != self.ctx.owner
            || self.ctx.infer.closures_without_expectation.contains(&loc.expr)
            || ![lang.Fn, lang.FnMut, lang.FnOnce].contains(&Some(trait_ref.def_id.0))
        {
            return;
        }
        let sig = args.as_closure().sig();
        let &[inputs] = sig.skip_binder().inputs().as_slice() else { return };
        let Some(expected) = trait_ref.args.as_slice().get(1).and_then(|it| it.as_type()) else {
            return;
        };
        let higher_ranked = !sig.bound_vars().is_empty();
        if (higher_ranked || rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&expected))
            && !self.binds_alike(inputs, expected)
        {
            return;
        }
        // A region either binds is no region of the body, which relating leaves out.
        self.relate(inputs, expected, Variance::Invariant, at);
    }

    /// Whether `a` and `b`, each under a binder of its own, differ only in regions, and bind
    /// a region in the same places.
    fn binds_alike(&self, a: Ty<'db>, b: Ty<'db>) -> bool {
        let interner = self.interner();
        let frame = self.ctx.frame;
        let shape = |ty: Ty<'db>| {
            let mut bound = Vec::new_in(frame);
            let erased = rustc_type_ir::fold_regions(interner, ty, |region, depth| {
                bound.push(matches!(
                    region.kind(),
                    RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(binder), _)
                        if binder == depth
                ));
                Region::new_erased(interner)
            });
            (erased, bound)
        };
        shape(a) == shape(b)
    }

    /// `trait_ref` with an associated type of a closure's type normalized, to what does not
    /// name the closure: `Vec<&'a str>: FromIterator<<Map<I, F> as Iterator>::Item>` is about
    /// the `&'r str` the closure returns.
    fn normalize_closure_args(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
    ) -> rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>> {
        use rustc_type_ir::inherent::GenericArg as _;
        if !trait_ref.args.iter().any(|arg| {
            arg.as_type()
                .is_some_and(|ty| matches!(ty.kind(), TyKind::Alias(..)) && names_closure(ty))
        }) {
            return trait_ref;
        }
        let frame = self.ctx.frame;
        let args: FVec<'_, hir_ty::next_solver::GenericArg<'db>> =
            mem::collect(frame, trait_ref.args.iter().map(|arg| match arg.as_type() {
                Some(ty) if matches!(ty.kind(), TyKind::Alias(..)) && names_closure(ty) => {
                    match self.normalize(ty) {
                        // Not one with a trait object, whose lifetime an impl may leave out,
                        // `impl Params for &[&dyn ToSql]`, which rust-analyzer makes `'static`.
                        Some(normalized)
                            if !names_closure(normalized)
                                && !normalized.walk().any(|it| {
                                    it.as_type()
                                        .is_some_and(|it| matches!(it.kind(), TyKind::Dynamic(..)))
                                })
                                && !matches!(
                                    normalized.kind(),
                                    TyKind::Alias(..) | TyKind::Infer(_)
                                ) =>
                        {
                            normalized.into()
                        }
                        _ => arg,
                    }
                }
                _ => arg,
            }));
        rustc_type_ir::TraitRef::new(self.interner(), trait_ref.def_id, args)
    }

    /// A trait bound on a type parameter, proven by the one where clause of this body that
    /// names the same trait for it, whose arguments it then equals.
    fn caller_bound(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) {
        let TyKind::Param(param) = trait_ref.self_ty().kind() else { return };
        let mut found = None;
        for clause in self.ctx.env.caller_bounds() {
            let kind = clause.kind();
            let ClauseKind::Trait(bound) = kind.skip_binder() else { continue };
            if bound.trait_ref.def_id != trait_ref.def_id
                || !matches!(bound.trait_ref.self_ty().kind(), TyKind::Param(it) if it.index == param.index)
            {
                continue;
            }
            if found.is_some() || !kind.bound_vars().is_empty() {
                return;
            }
            found = Some(bound.trait_ref);
        }
        let Some(bound) = found else { return };
        let args = rustc_type_ir::fold_regions(self.interner(), bound.args, |region, depth| {
            self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
        });
        self.relate_args(trait_ref.args, args, None, Variance::Invariant, at);
    }

    /// Proves `trait_ref` with the trait solver, as rustc's `prove_predicate`, and takes the
    /// region constraints proving it gives: a where clause or an impl it matches relates the
    /// regions of the two (`&'min G: Get` proven by `&'max G: Get` equals them), and the
    /// impl's where clauses (`T: 'a`) hold. Nothing when it is not proven, and `true` when
    /// it does not hold.
    fn prove(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        at: At,
    ) -> bool {
        use hir_ty::next_solver::{
            infer::traits::{Obligation, ObligationCause},
            obligation_ctxt::ObligationCtxt,
        };
        if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&trait_ref)
            || rustc_type_ir::TypeVisitableExt::references_error(&trait_ref)
            || trait_ref.args.iter().any(|arg| {
                rustc_type_ir::inherent::GenericArg::as_type(&arg).is_some_and(names_closure)
            })
        {
            return false;
        }
        // Without regions of its own it can still need what a `for<'a>` in an impl's where
        // clauses says of the placeholders it is proven for: `&'static str: DeserializeOwned`.
        let regionless = !rustc_type_ir::TypeVisitableExt::has_free_regions(&trait_ref);
        let interner = self.interner();
        let mut pairs = self.scratch.pairs.take();
        let mut obligations = self.scratch.obligations.take();
        let proven = self.prove_canonical(trait_ref, &mut pairs, &mut obligations);
        let refused = match proven {
            Some(Ok(())) => None,
            Some(Err(true_error)) => Some(!regionless && true_error),
            None => {
                let (constraints_before, obligations_before) = {
                    let mut inner = self.ctx.infcx.inner.borrow_mut();
                    let obligations = inner.region_obligations().len();
                    let constraints = inner
                        .unwrap_region_constraints()
                        .region_constraint_data()
                        .constraints
                        .len();
                    (constraints, obligations)
                };
                let mut ocx = ObligationCtxt::new(&self.ctx.infcx);
                let predicate = rustc_type_ir::TraitClause {
                    trait_ref,
                    polarity: rustc_type_ir::ClausePolarity::Positive,
                };
                ocx.register_obligation(Obligation::new(
                    interner,
                    ObligationCause::dummy(),
                    self.ctx.env,
                    predicate,
                ));
                let errors = ocx.evaluate_obligations_error_on_ambiguity();
                if !errors.is_empty() {
                    Some(!regionless && errors.iter().any(|it| it.is_true_error()))
                } else {
                    let mut inner = self.ctx.infcx.inner.borrow_mut();
                    obligations.extend(
                        inner.region_obligations()[obligations_before..]
                            .iter()
                            .map(|it| (it.sup_type, it.sub_region)),
                    );
                    constraint_pairs(
                        interner,
                        inner
                            .unwrap_region_constraints()
                            .region_constraint_data()
                            .constraints
                            .iter_from(constraints_before),
                        &mut pairs,
                    );
                    None
                }
            }
        };
        if let Some(refused) = refused {
            self.scratch.pairs.give(pairs);
            self.scratch.obligations.give(obligations);
            return refused;
        }
        // Its impl's where clauses can still need a type to outlive a region:
        // `ServiceChain<P, S>: Service` that `P::Error: 'static`.
        if regionless && pairs.is_empty() && obligations.is_empty() {
            self.scratch.pairs.give(pairs);
            self.scratch.obligations.give(obligations);
            return false;
        }
        if crate::debugging(self.ctx.db, self.ctx.owner) {
            eprintln!("prove {trait_ref:?}: {pairs:?} {obligations:?}"); // alloc: debug output
        }
        let rv = |this: &mut Self, region: Region<'db>| match region.kind() {
            RegionKind::ReVar(vid) => Some(vid.as_usize()),
            RegionKind::ReStatic => Some(this.regions.static_rv),
            RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) => {
                this.signature_region(region, rustc_type_ir::INNERMOST).and_then(rv_of)
            }
            RegionKind::RePlaceholder(_) => rv_of(this.placeholder(region)),
            _ => None,
        };
        // A placeholder equated with an error region stands for a lifetime rust-analyzer
        // elided in a signature, whose trait objects it gives `'static` rather than that
        // lifetime (`fn(&mut dyn Write)` for `for<'b> FnMut(&'b mut dyn Write)`), so nothing
        // else said of it holds. Nor does it of one equated with the variable given such a
        // region.
        let elided = self.elided.borrow();
        let mut unknown: FSet<'_, Region<'db>> = mem::set(self.ctx.frame);
        unknown.extend(
            pairs
                .iter()
                .flat_map(|&(a, b)| [(a, b), (b, a)])
                .filter(|(it, other)| {
                    it.is_placeholder()
                        && (other.is_error()
                            || matches!(other.kind(), RegionKind::ReVar(vid) if elided.contains(&vid)))
                })
                .map(|(it, _)| it),
        );
        drop(elided);
        // `a <= b`: `b` outlives `a`.
        for (shorter, longer) in pairs.drain(..) {
            if unknown.contains(&shorter) || unknown.contains(&longer) {
                continue;
            }
            // A region of a universe that cannot name the placeholder outlives it only by
            // outliving every region, as rustc's `add_incompatible_universe`.
            let longer = match shorter.is_placeholder()
                && longer != shorter
                && self.ctx.infcx.universe_of_region(longer)
                    < self.ctx.infcx.universe_of_region(shorter)
            {
                true => Region::new_static(interner),
                false => longer,
            };
            if let (Some(shorter), Some(longer)) = (rv(self, shorter), rv(self, longer)) {
                self.outlives(longer, shorter, at);
            }
        }
        // A type outlives a placeholder, of the predicate being proven or of a `for<'a>` in
        // the where clauses of an impl or of the environment that proves it, only by outliving
        // `'static`: the body's regions are of a universe that cannot name it, as rustc's
        // `add_incompatible_universe`. So `for<'a> F: FnMut(<&'1 mut I as Lend>::Item<'a>)`,
        // whose normalization needs `&'1 mut I: 'a`, needs `'1: 'static` (rustc's "due to a
        // current limitation of the type system"), and a type parameter that must outlive it
        // is rustc's codeless "does not live long enough".
        for (ty, region) in obligations.drain(..) {
            let placeholder = region.is_placeholder();
            let ty = rustc_type_ir::fold_regions(interner, ty, |it, _| match it.is_placeholder() {
                true => Region::new_static(interner),
                false => it,
            });
            let region = match placeholder {
                true => Region::new_static(interner),
                false => region,
            };
            let Some(region) = rv(self, region) else { continue };
            let ty = rustc_type_ir::fold_regions(interner, ty, |it, _| match it.kind() {
                RegionKind::ReEarlyParam(_) | RegionKind::ReLateParam(_) | RegionKind::ReStatic => {
                    rv(self, it).map_or(it, |it| {
                        Region::new_var(interner, rustc_type_ir::RegionVid::from_usize(it))
                    })
                }
                _ => it,
            });
            let tests = self.regions.type_tests.len();
            self.type_outlives(
                ty,
                Region::new_var(interner, rustc_type_ir::RegionVid::from_usize(region)),
                at,
            );
            if placeholder {
                for test in &mut self.regions.type_tests[tests..] {
                    test.forall = true;
                }
            }
        }
        self.scratch.pairs.give(pairs);
        self.scratch.obligations.give(obligations);
        false
    }

    /// What proving `trait_ref` gives, in this body's regions, from [`prove_query`], asked with
    /// its [`canonical_regions`]. `None` where it must be proven here, `Err` where it does not hold.
    fn prove_canonical(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        pairs: &mut FVec<'a, (Region<'db>, Region<'db>)>,
        obligations: &mut FVec<'a, (Ty<'db>, Region<'db>)>,
    ) -> Option<Result<(), bool>> {
        use rustc_type_ir::TypeVisitableExt as _;
        if trait_ref.has_placeholders() || trait_ref.has_non_region_infer() {
            return None;
        }
        let interner = self.interner();
        let mut vars = self.scratch.regions.take();
        let Some(canonical) = canonical_regions(self.ctx, trait_ref, &mut vars) else {
            self.scratch.regions.give(vars);
            return None;
        };
        let env = hir_ty::ParamEnvAndCrate { param_env: self.ctx.env, krate: self.ctx.krate };
        let (answer_pairs, answer_obligations, universes) = match prove_query(
            self.ctx.db,
            env.store(),
            hir_ty::next_solver::StoredTraitRef::new(canonical),
            vars.len() as u32,
        ) {
            Proven::Here => {
                self.scratch.regions.give(vars);
                return None;
            }
            &Proven::Error(true_error) => {
                self.scratch.regions.give(vars);
                return Some(Err(true_error));
            }
            Proven::Gives { pairs, obligations, universes, in_universe } => (
                pairs,
                obligations,
                Universes::new(self.ctx, self.ctx.frame, *universes, in_universe),
            ),
        };
        let mut made = mem::map(self.ctx.frame);
        let mut map = |region| region_from_query(self.ctx, &vars, &universes, &mut made, region);
        pairs.extend(answer_pairs.iter().map(|(a, b)| (map(a.as_ref()), map(b.as_ref()))));
        obligations.extend(answer_obligations.iter().map(|(ty, region)| {
            let ty = rustc_type_ir::fold_regions(interner, ty.as_ref(), |it, _| map(it));
            (ty, map(region.as_ref()))
        }));
        self.scratch.regions.give(vars);
        Some(Ok(()))
    }

    /// `ty` of the signature with its lifetimes as universal regions.
    fn signature_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        let interner = self.interner();
        rustc_type_ir::fold_regions(interner, ty, |region, depth| {
            self.signature_region(region, depth).unwrap_or(region)
        })
    }

    fn region_rv(&self, region: Region<'db>) -> Option<Rv> {
        match region.kind() {
            RegionKind::ReStatic => Some(self.regions.static_rv),
            _ => rv_of(region),
        }
    }
}

/// What proving a trait reference gives a body, as the query database keeps it: its region variables are
/// the trait reference's, numbered in order, and after them the ones proving it made. The
/// universes it names past the root, of the placeholders of a `for<'a>` it proved and of the
/// variables made in them, are the query's own, `1..=universes`, which the body makes anew in
/// the same order, as rustc's canonical query response does.
#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
enum Proven {
    /// What it gives names a variable the query resolved, which the body would have to see
    /// resolved too.
    Here,
    /// It does not hold; whether that is a true error.
    Error(bool),
    Gives {
        pairs: Vec<(StoredRegion, StoredRegion)>,
        obligations: Vec<(StoredTy, StoredRegion)>,
        universes: u32,
        /// Each variable it made outside the root universe, and that universe.
        in_universe: Vec<(u32, u32)>,
    },
}

/// The universes of a query's answer past the root as the body has them: the body's for each
/// of the query's, and the universe of each variable the query made outside the root.
struct Universes<'f> {
    fresh: &'f [rustc_type_ir::UniverseIndex],
    /// Each variable the answer made outside the root universe and that universe, sorted by
    /// the variable: the memo's own.
    of_var: &'f [(u32, u32)],
}

impl<'f> Universes<'f> {
    fn new(
        ctx: &Ctx<'_, '_>,
        frame: &'f Frame,
        universes: u32,
        in_universe: &'f [(u32, u32)],
    ) -> Self {
        Universes {
            fresh: frame
                .alloc_slice_fill_with(universes as usize, |_| ctx.infcx.create_next_universe()),
            of_var: in_universe,
        }
    }

    /// The universe of the answer's variable `var`, if it made it outside the root.
    fn of_var(&self, var: u32) -> Option<u32> {
        let at = self.of_var.binary_search_by_key(&var, |&(it, _)| it).ok()?;
        Some(self.of_var[at].1)
    }

    const NONE: Universes<'static> = Universes { fresh: &[], of_var: &[] };

    fn get(&self, universe: u32) -> Option<rustc_type_ir::UniverseIndex> {
        self.fresh.get((universe as usize).checked_sub(1)?).copied()
    }
}

/// `a <= b` for each of `constraints`: `b` outlives `a`.
fn constraint_pairs<'db>(
    interner: hir_ty::next_solver::DbInterner<'db>,
    constraints: impl IntoIterator<
        Item = hir_ty::next_solver::infer::region_constraints::Constraint<'db>,
    >,
    out: &mut FVec<'_, (Region<'db>, Region<'db>)>,
) {
    out.extend(constraints.into_iter().map(|constraint| constraint.regions(interner)));
}

/// Proves `trait_ref`, whose region variables are the first `vars`, in `env`, as rustc's
/// canonical `type_op_prove_predicate`: a query, so a goal is proven once for every body of the
/// same environment, and an edit proves again only the goals of what it changes.
#[blaze_query::tracked(returns(ref))]
fn prove_query(
    db: &Db,
    env: hir_ty::traits::StoredParamEnvAndCrate,
    trait_ref: hir_ty::next_solver::StoredTraitRef,
    vars: u32,
) -> Proven {
    use hir_ty::next_solver::{
        DbInterner, TypingMode,
        infer::{
            DbInternerInferExt as _,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    };
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let env = env.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate).with_regions();
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        for _ in 0..vars {
            infcx.next_region_var(hir_ty::Span::Dummy);
        }
        let trait_ref = trait_ref.get(interner);
        let mut ocx = ObligationCtxt::new(&infcx);
        ocx.register_obligation(Obligation::new(
            interner,
            ObligationCause::dummy(),
            env.param_env,
            rustc_type_ir::TraitClause {
                trait_ref,
                polarity: rustc_type_ir::ClausePolarity::Positive,
            },
        ));
        let errors = ocx.evaluate_obligations_error_on_ambiguity();
        if !errors.is_empty() {
            return Proven::Error(errors.iter().any(|it| it.is_true_error()));
        }
        let mut inner = infcx.inner.borrow_mut();
        let obligations = mem::collect(
            frame,
            inner.region_obligations().iter().map(|it| (it.sup_type, it.sub_region)),
        );
        let mut constraints = inner.unwrap_region_constraints();
        let mut pairs = Vec::new_in(frame);
        constraint_pairs(
            interner,
            constraints.region_constraint_data().constraints.iter(),
            &mut pairs,
        );
        let mut universes = 0;
        let mut in_universe: FMap<'_, u32, u32> = mem::map(frame);
        let mut resolved = false;
        let mut visit = |region: Region<'_>| match region.kind() {
            RegionKind::RePlaceholder(placeholder) => {
                universes = universes.max(placeholder.universe.as_u32());
            }
            RegionKind::ReVar(vid) if vid.as_u32() >= vars => {
                match constraints.try_resolve_region_var(vid) {
                    Err(universe) if universe != rustc_type_ir::UniverseIndex::ROOT => {
                        universes = universes.max(universe.as_u32());
                        in_universe.insert(vid.as_u32(), universe.as_u32());
                    }
                    Err(_) => {}
                    Ok(_) => resolved = true,
                }
            }
            _ => {}
        };
        for &(a, b) in &pairs {
            visit(a);
            visit(b);
        }
        for &(ty, region) in &obligations {
            visit(region);
            rustc_type_ir::fold_regions(interner, ty, |it, _| {
                visit(it);
                it
            });
        }
        if resolved {
            return Proven::Here;
        }
        let mut in_universe = mem::owned(in_universe.into_iter());
        radix::sort_copy_by_key(&mut in_universe, |&(vid, universe)| radix::pack_32(vid, universe));
        Proven::Gives {
            pairs: mem::owned(pairs.iter().map(|&(a, b)| (a.store(), b.store()))),
            obligations: mem::owned(
                obligations.iter().map(|&(ty, region)| (ty.store(), region.store())),
            ),
            universes,
            in_universe,
        }
    })
}

/// The lifetime of a trait object written without one, which rust-analyzer gives as
/// `'static`: in `&'r dyn Trait` it is `'r`, and elsewhere in a function body inferred, unless
/// the body writes `dyn Trait + 'static` somewhere, where it may be that, or the trait's
/// bounds say `'static`, as rustc's `compute_object_lifetime_bound`.
struct ObjectDefaults<'db> {
    db: &'db Db,
    interner: hir_ty::next_solver::DbInterner<'db>,
    keep_static: bool,
    /// In a body, where the lifetime is inferred rather than `'static`.
    erase: bool,
}

impl<'db> rustc_type_ir::TypeFolder<hir_ty::next_solver::DbInterner<'db>> for ObjectDefaults<'db> {
    fn cx(&self) -> hir_ty::next_solver::DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        use rustc_type_ir::{
            TypeFoldable, TypeSuperFoldable, inherent::BoundExistentialPredicates as _,
        };
        let is_static = |region: Region<'db>| matches!(region.kind(), RegionKind::ReStatic);
        let defaulted = |this: &Self, predicates: <hir_ty::next_solver::DbInterner<'db> as rustc_type_ir::Interner>::BoundExistentialPredicates, object| {
            is_static(object)
                && !this.keep_static
                && !predicates.principal_def_id().is_some_and(|it| static_trait(this.db, it.0))
        };
        match ty.kind() {
            TyKind::Ref(region, inner, mutability) if !is_static(region) => {
                if let TyKind::Dynamic(predicates, object) = inner.kind()
                    && defaulted(self, predicates, object)
                {
                    let inner = Ty::new_dynamic(self.interner, predicates.fold_with(self), region);
                    return Ty::new_ref(self.interner, region, inner, mutability);
                }
                ty.super_fold_with(self)
            }
            TyKind::Dynamic(predicates, object)
                if self.erase && defaulted(self, predicates, object) =>
            {
                Ty::new_dynamic(
                    self.interner,
                    predicates.fold_with(self),
                    Region::new_erased(self.interner),
                )
            }
            _ => ty.super_fold_with(self),
        }
    }
}

/// Each occurrence of one type made another: an impl's self type the one with universal
/// regions for its anonymous lifetimes, an `impl Trait` the type it hides. The first may name
/// bound lifetimes, shifted under each binder; the second names none.
struct Replace<'db> {
    interner: hir_ty::next_solver::DbInterner<'db>,
    replace: (Ty<'db>, Ty<'db>),
    binders: u32,
}

impl<'db> Replace<'db> {
    fn new(interner: hir_ty::next_solver::DbInterner<'db>, replace: (Ty<'db>, Ty<'db>)) -> Self {
        Replace { interner, replace, binders: 0 }
    }
}

impl<'db> rustc_type_ir::TypeFolder<hir_ty::next_solver::DbInterner<'db>> for Replace<'db> {
    fn cx(&self) -> hir_ty::next_solver::DbInterner<'db> {
        self.interner
    }

    fn fold_binder<T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
        &mut self,
        binder: rustc_type_ir::Binder<hir_ty::next_solver::DbInterner<'db>, T>,
    ) -> rustc_type_ir::Binder<hir_ty::next_solver::DbInterner<'db>, T> {
        use rustc_type_ir::TypeSuperFoldable;
        self.binders += 1;
        let binder = binder.super_fold_with(self);
        self.binders -= 1;
        binder
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        use rustc_type_ir::TypeSuperFoldable;
        if ty == rustc_type_ir::shift_vars(self.interner, self.replace.0, self.binders) {
            self.replace.1
        } else {
            ty.super_fold_with(self)
        }
    }
}

/// Whether `trait_` or one of its supertraits is bounded by `'static`, so that its object
/// type's lifetime defaults to that and each type implementing it outlives it.
fn static_trait(db: &Db, trait_: hir_def::TraitId) -> bool {
    hir_ty::utils::all_super_traits(db, trait_).iter().any(|&it| {
        GenericPredicates::query_all(db, it.into()).skip_binder().any(|clause| {
            matches!(clause.kind().skip_binder(),
                ClauseKind::TypeOutlives(OutlivesClause(ty, region))
                    if matches!(ty.kind(), TyKind::Param(param) if param.index == 0)
                        && matches!(region.kind(), RegionKind::ReStatic))
        })
    })
}

/// Whether the type `ty` in `store` writes `Self`, anywhere in it: in a trait object's or
/// `impl Trait`'s bounds and their associated type bindings (`&mut dyn FnMut(&mut Self) ->
/// Self`) and in a function pointer's signature too.
fn writes_self(
    store: &hir_def::expr_store::ExpressionStore,
    ty: hir_def::hir::type_ref::TypeRefId,
) -> bool {
    use hir_def::{
        expr_store::path::{GenericArg, GenericArgs, Path},
        hir::type_ref::{TypeBound, TypeRef},
    };
    fn args(store: &hir_def::expr_store::ExpressionStore, generic: GenericArgs<'_>) -> bool {
        generic
            .args
            .iter()
            .any(|arg| matches!(arg, GenericArg::Type(it) if writes_self(store, *it)))
            || generic.bindings.iter().any(|binding| {
                binding.type_ref.is_some_and(|it| writes_self(store, it))
                    || binding.args.is_some_and(|it| args(store, it))
                    || binding.bounds.iter().any(|it| bound(store, it))
            })
    }
    fn path(store: &hir_def::expr_store::ExpressionStore, path: Path<'_>) -> bool {
        path.is_self_type()
            || path.qualifier().is_some_and(|it| it.is_self_type())
            || path.type_anchor().is_some_and(|it| writes_self(store, it))
            || path.generic_args().into_iter().flatten().flatten().any(|it| args(store, it))
    }
    fn bound(store: &hir_def::expr_store::ExpressionStore, bound: &TypeBound) -> bool {
        bound.as_path(store).is_some_and(|(it, _)| path(store, it))
    }
    match store.type_ref(ty) {
        TypeRef::Path(it) => path(store, it),
        TypeRef::Tuple(tys) => tys.iter().any(|&it| writes_self(store, it)),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_self(store, it),
        TypeRef::Reference(it) => writes_self(store, it.ty),
        TypeRef::Array(it) => writes_self(store, it.ty),
        TypeRef::Fn(it) => it.params.iter().any(|&(_, it)| writes_self(store, it)),
        TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
            bounds.iter().any(|it| bound(store, it))
        }
        _ => false,
    }
}

/// Whether the type `ty` in `store` writes `&T` or `'_`, a lifetime that elision gives the
/// return type, outside a `fn` pointer and `Fn(..)`, which elide on their own.
fn writes_elided(
    store: &hir_def::expr_store::ExpressionStore,
    ty: hir_def::hir::type_ref::TypeRefId,
) -> bool {
    use hir_def::{
        expr_store::path::{GenericArg, GenericArgsParentheses, Path},
        hir::type_ref::{LifetimeRef, LifetimeRefId, TypeBound, TypeRef},
    };
    let lifetime = |it: LifetimeRefId| matches!(store[it], LifetimeRef::Placeholder);
    let path = |path: Path<'_>| {
        path.generic_args().into_iter().flatten().flatten().any(|args| {
            matches!(args.parenthesized, GenericArgsParentheses::No)
                && args.args.iter().any(|arg| match arg {
                    GenericArg::Lifetime(it) => lifetime(*it),
                    GenericArg::Type(it) => writes_elided(store, *it),
                    _ => false,
                })
        })
    };
    match store.type_ref(ty) {
        TypeRef::Reference(it) => it.lifetime.is_none_or(lifetime) || writes_elided(store, it.ty),
        TypeRef::Path(it) => path(it),
        TypeRef::Tuple(tys) => tys.iter().any(|&it| writes_elided(store, it)),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_elided(store, it),
        TypeRef::Array(it) => writes_elided(store, it.ty),
        TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
            bounds.iter().any(|bound| match bound {
                TypeBound::Lifetime(it) => lifetime(*it),
                TypeBound::Path(it, _) => path(store.path(*it)),
                _ => false,
            })
        }
        _ => false,
    }
}

/// How many lifetimes the type `ty` in `store` elides: `&T`, `'_`, and a path that writes
/// none of its lifetimes, one for `S2` of `type S2<'a> = S<'a, 'a>` though its type has it
/// twice. `None` where that is not known (`dyn Trait`, `impl Trait`, `Self`, associated types).
fn elided_lifetimes(
    db: &Db,
    resolver: &hir_def::resolver::Resolver<'_>,
    store: &hir_def::expr_store::ExpressionStore,
    ty: hir_def::hir::type_ref::TypeRefId,
) -> Option<usize> {
    use hir_def::{
        expr_store::path::GenericArg,
        hir::{
            generics::GenericParams,
            type_ref::{LifetimeRef, TypeRef},
        },
        resolver::TypeNs,
    };
    let count = |it| elided_lifetimes(db, resolver, store, it);
    match store.type_ref(ty) {
        TypeRef::Reference(it) => {
            let elided =
                matches!(it.lifetime.map(|it| &store[it]), None | Some(LifetimeRef::Placeholder));
            Some(elided as usize + count(it.ty)?)
        }
        TypeRef::Tuple(tys) => tys.iter().map(|&it| count(it)).sum(),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => count(it),
        TypeRef::Array(it) => count(it.ty),
        TypeRef::Never | TypeRef::TypeParam(_) => Some(0),
        TypeRef::Path(path) => {
            let params = match resolver.resolve_path_in_type_ns_fully(db, path)? {
                TypeNs::AdtId(it) => GenericParams::of(db, it.into()).len_lifetimes(),
                TypeNs::TypeAliasId(it) => GenericParams::of(db, it.into()).len_lifetimes(),
                TypeNs::BuiltinType(_) | TypeNs::GenericParam(_) => 0,
                _ => return None,
            };
            let (mut written, mut placeholders, mut inner) = (0, 0, 0);
            for args in path.generic_args().into_iter().flatten().flatten() {
                if !args.bindings.is_empty() {
                    return None;
                }
                for arg in args.args {
                    match arg {
                        GenericArg::Lifetime(it) => {
                            written += 1;
                            placeholders += matches!(store[*it], LifetimeRef::Placeholder) as usize;
                        }
                        GenericArg::Type(it) => inner += count(*it)?,
                        GenericArg::Const(_) => (),
                    }
                }
            }
            Some(inner + if written == 0 { params } else { placeholders })
        }
        _ => None,
    }
}

/// Whether a type in `store` writes a trait object's lifetime, `dyn Trait + 'a`.
fn writes_object_lifetime(store: &hir_def::expr_store::ExpressionStore) -> bool {
    use hir_def::hir::type_ref::{TypeBound, TypeRef};
    store.type_refs().any(|(_, it)| match it {
        TypeRef::DynTrait(bounds) => bounds.iter().any(|it| matches!(it, TypeBound::Lifetime(_))),
        _ => false,
    })
}

/// Whether a type in `store` writes `dyn Trait + 'static`.
fn writes_static_object(store: &hir_def::expr_store::ExpressionStore) -> bool {
    use hir_def::hir::type_ref::{LifetimeRef, TypeBound, TypeRef};
    store.type_refs().any(|(_, it)| {
        match it {
        TypeRef::DynTrait(bounds) => bounds.iter().any(|bound| {
            matches!(bound, TypeBound::Lifetime(it) if matches!(store[*it], LifetimeRef::Static))
        }),
        _ => false,
    }
    })
}

/// Whether `path` writes generic arguments, `f::<T>` or `<T as Trait>::f`: a user type
/// annotation, whose lifetimes bind.
/// Whether `path` names the associated item `item` through a type, `Foo::f`, `<()>::f` or
/// `T::f`, not through its trait alone, `Default::default`: rustc gives it a user type
/// annotation whose generic arguments, and an inherent impl's self type, must be well-formed.
fn names_through_type(
    db: &Db,
    path: hir_def::expr_store::path::Path<'_>,
    container: ItemContainerId,
) -> bool {
    if path.type_anchor().is_some() {
        return true;
    }
    let segments = path.segments();
    let Some(qualifier) = segments.get(segments.len().wrapping_sub(2)) else { return false };
    match container {
        ItemContainerId::ImplId(_) => true,
        ItemContainerId::TraitId(trait_) => {
            *qualifier.name != hir_def::signatures::TraitSignature::of(db, trait_).name
        }
        _ => false,
    }
}

/// The `impl Trait` a function's return type `output` has, when it has one, and the part of
/// `ty`, the type its body returns, that hides it: `(impl Iterator, &'b str)` hides it in
/// the tuple's first field.
fn returned_opaque<'db>(
    output: Ty<'db>,
    ty: Ty<'db>,
) -> Option<(rustc_type_ir::AliasTy<hir_ty::next_solver::DbInterner<'db>>, Ty<'db>)> {
    fn find<'db>(
        output: Ty<'db>,
        ty: Ty<'db>,
    ) -> Option<(rustc_type_ir::AliasTy<hir_ty::next_solver::DbInterner<'db>>, Ty<'db>)> {
        match (output.kind(), ty.kind()) {
            (TyKind::Alias(_, alias), _)
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. }) =>
            {
                Some((alias, ty))
            }
            (TyKind::Tuple(outputs), TyKind::Tuple(tys)) if outputs.len() == tys.len() => {
                outputs.iter().zip(tys.iter()).find_map(|(output, ty)| find(output, ty))
            }
            (TyKind::Ref(_, output, _), TyKind::Ref(_, ty, _)) => find(output, ty),
            (TyKind::Adt(def, outputs), TyKind::Adt(other, tys))
                if def == other && outputs.len() == tys.len() =>
            {
                outputs.iter().zip(tys.iter()).find_map(|(output, ty)| find(output.ty()?, ty.ty()?))
            }
            _ => None,
        }
    }
    let opaques = output
        .walk()
        .filter_map(|it| rustc_type_ir::inherent::GenericArg::as_type(&it))
        .filter(|ty| {
            matches!(ty.kind(), TyKind::Alias(_, alias)
                if matches!(alias.kind, rustc_type_ir::AliasTyKind::Opaque { .. }))
        })
        .count();
    if opaques != 1 {
        return None;
    }
    find(output, ty)
}

/// The variable the path `expr` names, if it names one.
pub(super) fn path_binding(
    ctx: &Ctx<'_, '_>,
    body: &MirBody<'_>,
    expr: hir_def::hir::ExprId,
) -> Option<hir_def::hir::BindingId> {
    let hir_def::hir::Expr::Path(_) = ctx.store.expr(expr) else { return None };
    let bindings = path_bindings(ctx.db, body.owner);
    let at = bindings.binary_search_by_key(&expr, |&(it, _)| it).ok()?;
    Some(bindings[at].1)
}

/// The variable each path of `owner`'s store names, by expression, of those that name one:
/// resolved with one resolver moved into each path's scope and back, so a deep scope chain
/// grows its stack once, and a query, so checking a body reads it without building one.
#[blaze_query::tracked(returns(ref))]
fn path_bindings<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
) -> Box<[(hir_def::hir::ExprId, hir_def::hir::BindingId)]> {
    use hir_def::resolver::ValueNs;
    let store_owner = owner.expression_store_owner(db);
    let store = hir_def::expr_store::ExpressionStore::of(db, store_owner);
    let mut resolver = owner.resolver(db);
    let mut out = Vec::new();
    for (expr, it) in store.exprs() {
        let hir_def::hir::Expr::Path(path) = it else { continue };
        let guard = resolver.update_to_inner_scope(db, store_owner, expr);
        let resolved =
            resolver.resolve_path_in_value_ns_fully(db, path, store.expr_path_hygiene(expr));
        resolver.reset_to_guard(guard);
        if let Some(ValueNs::LocalBinding(binding)) = resolved {
            out.push((expr, binding)); // alloc: the query's result, once per owner
        }
    }
    out.sort_unstable_by_key(|&(expr, _)| expr);
    out.into_boxed_slice()
}

pub(super) fn spells_generics(path: hir_def::expr_store::path::Path<'_>) -> bool {
    path.type_anchor().is_some()
        || path.generic_args().is_some_and(|args| args.iter().any(|it| it.is_some()))
}

/// The regions and the type parameters in `ty` that must outlive a region for `ty` to: rustc's
/// outlives components.
pub(super) fn outlives_components<'db>(
    ty: Ty<'db>,
    regions: &mut FVec<'_, Rv>,
    params: &mut FVec<'_, Ty<'db>>,
) {
    match ty.kind() {
        TyKind::Param(_) => params.push(ty),
        TyKind::Alias(..) | TyKind::Placeholder(_) | TyKind::Bound(..) => (),
        TyKind::Ref(region, inner, _) => {
            regions.extend(rv_of(region));
            outlives_components(inner, regions, params);
        }
        // Every predicate's arguments, and what an associated type is bound to: in
        // `dyn FnMut() -> Option<B>`, `B` is in `Output`'s.
        TyKind::Dynamic(predicates, region) => {
            regions.extend(rv_of(region));
            for predicate in predicates.iter() {
                match predicate.skip_binder() {
                    rustc_type_ir::ExistentialPredicate::Trait(it) => {
                        args_components(it.args, regions, params)
                    }
                    rustc_type_ir::ExistentialPredicate::Projection(it) => {
                        args_components(it.args, regions, params);
                        if let Some(term) = it.term.as_type() {
                            outlives_components(term, regions, params);
                        }
                    }
                    rustc_type_ir::ExistentialPredicate::AutoTrait(_) => (),
                }
            }
        }
        // The regions the signature binds are not `ReVar`s, so `rv_of` leaves them out.
        TyKind::FnPtr(sig, _) => {
            for it in sig.skip_binder().inputs_and_output.iter() {
                outlives_components(it, regions, params);
            }
        }
        TyKind::Adt(_, args) => args_components(args, regions, params),
        // rustc leaves out the lifetimes among a function item's arguments (rust-lang/rust#70917).
        TyKind::FnDef(_, args) => {
            for it in args.no_bound_vars().unwrap().types() {
                outlives_components(it, regions, params);
            }
        }
        TyKind::Closure(_, args) => {
            outlives_components(args.as_closure().tupled_upvars_ty(), regions, params)
        }
        TyKind::CoroutineClosure(_, args) => {
            outlives_components(args.as_coroutine_closure().tupled_upvars_ty(), regions, params)
        }
        TyKind::Coroutine(_, args) => {
            outlives_components(args.as_coroutine().tupled_upvars_ty(), regions, params);
            outlives_components(args.as_coroutine().resume_ty(), regions, params);
        }
        TyKind::Tuple(tys) => {
            for it in tys.iter() {
                outlives_components(it, regions, params);
            }
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
            outlives_components(inner, regions, params)
        }
        _ => (),
    }
}

/// The associated and opaque types among what `outlives_components` splits `ty` into, which
/// it leaves out.
pub(super) fn alias_components<'db>(ty: Ty<'db>, out: &mut FVec<'_, Ty<'db>>) {
    let args = |args: GenericArgs<'db>, out: &mut FVec<'_, Ty<'db>>| {
        for it in args.types() {
            alias_components(it, out);
        }
    };
    match ty.kind() {
        TyKind::Alias(_, alias)
            if matches!(
                alias.kind,
                rustc_type_ir::AliasTyKind::Projection { .. }
                    | rustc_type_ir::AliasTyKind::Opaque { .. }
            ) =>
        {
            out.push(ty)
        }
        TyKind::Ref(_, inner, _)
        | TyKind::Array(inner, _)
        | TyKind::Slice(inner)
        | TyKind::RawPtr(inner, _) => alias_components(inner, out),
        TyKind::Dynamic(predicates, _) => {
            for predicate in predicates.iter() {
                match predicate.skip_binder() {
                    rustc_type_ir::ExistentialPredicate::Trait(it) => args(it.args, out),
                    rustc_type_ir::ExistentialPredicate::Projection(it) => {
                        args(it.args, out);
                        if let Some(term) = it.term.as_type() {
                            alias_components(term, out);
                        }
                    }
                    rustc_type_ir::ExistentialPredicate::AutoTrait(_) => (),
                }
            }
        }
        TyKind::FnPtr(sig, _) => {
            for it in sig.skip_binder().inputs_and_output.iter() {
                alias_components(it, out);
            }
        }
        TyKind::Adt(_, it) => args(it, out),
        TyKind::FnDef(_, it) => args(it.no_bound_vars().unwrap(), out),
        TyKind::Closure(_, it) => alias_components(it.as_closure().tupled_upvars_ty(), out),
        TyKind::Coroutine(_, it) => alias_components(it.as_coroutine().tupled_upvars_ty(), out),
        TyKind::Tuple(tys) => {
            for it in tys.iter() {
                alias_components(it, out);
            }
        }
        _ => (),
    }
}

fn args_components<'db>(
    args: GenericArgs<'db>,
    regions: &mut FVec<'_, Rv>,
    params: &mut FVec<'_, Ty<'db>>,
) {
    for arg in args.iter() {
        match arg.kind() {
            GenericArgKind::Lifetime(region) => regions.extend(rv_of(region)),
            GenericArgKind::Type(ty) => outlives_components(ty, regions, params),
            GenericArgKind::Const(_) => (),
        }
    }
}

/// The constraints from `from` down to `to`, the shortest way, for the error that blames one
/// of them.
pub(super) fn constraint_path<'f>(
    regions: &Regions<'f, '_>,
    count: usize,
    from: Rv,
    to: Rv,
) -> FVec<'f, Outlives> {
    const NONE: u32 = u32::MAX;
    let frame = *regions.universals.allocator();
    let constraints = &regions.outlives;
    let mut came_from = filled(frame, count, NONE);
    let mut seen = bits(frame, count);
    let mut queue: FVec<'_, Rv> = Vec::with_capacity_in(count, frame);
    queue.push(from);
    seen.insert(from);
    let mut head = 0;
    while let Some(&rv) = queue.get(head) {
        head += 1;
        if rv == to {
            let mut path = Vec::new_in(frame);
            let mut at = to;
            while came_from[at] != NONE {
                let index = came_from[at] as usize;
                path.push(constraints.get(index));
                at = constraints.sup(index);
            }
            path.reverse();
            return path;
        }
        let ids = if rv < count { constraints.of_sup(rv) } else { &[] };
        for &index in ids {
            let sub = constraints.sub(index as usize);
            if seen.insert(sub) {
                came_from[sub] = index;
                queue.push(sub);
            }
        }
    }
    Vec::new_in(frame)
}

/// The constraint an error about `path` points at, as rustc's `best_blame_constraint`.
pub(super) fn blame(path: &[Outlives]) -> Option<Outlives> {
    blame_with(path, false)
}

/// [`blame`], where what is returned is an `impl Trait` that captures no lifetime: rustc's
/// `OpaqueType` constraint, less interesting than an assignment.
pub(super) fn blame_with(path: &[Outlives], opaque: bool) -> Option<Outlives> {
    // rustc's `best_blame_constraint` from a free region: the most interesting constraint,
    // and of equally interesting ones the closest to the region outlived.
    let interest = |it: &Outlives| match it.category {
        Category::Return if opaque => 3,
        Category::Return => 0,
        Category::TypeAnnotation
        | Category::Cast
        | Category::CallArgument
        | Category::Assignment
        | Category::Usage => 2,
        Category::GenericArg => 3,
        Category::Predicate => 4,
        Category::Boring | Category::Signature | Category::Opaque => 5,
        Category::Internal => 7,
    };
    let best = path.iter().map(interest).min()?;
    path.iter()
        .rev()
        .find(|it| interest(it) == best && it.span != MirSpan::Unknown)
        .or_else(|| path.iter().rev().find(|it| interest(it) == best))
        .copied()
}

/// `ty` an associated type normalized, with the lifetimes it takes from its arguments the same
/// regions: the solver gives the result regions of its own, related to those it came from only
/// by constraints it records.
pub(super) fn normalize<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Option<Ty<'db>> {
    normalize_impl(ctx, ty, false)
}

/// `ty` normalized as `normalize` does, a region the result has of its own (an impl's
/// lifetime that nothing constrains, `impl<'a, T: 'a> Tr for T { type A = <T as Mk<'a>>::R }`)
/// left a region variable of its own, as rustc's normalization in borrowck gives it an
/// existential region.
fn normalize_with_fresh<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Option<Ty<'db>> {
    normalize_impl(ctx, ty, true)
}

/// Whether one of `tys`, with no error type in it, has one once normalized. Its regions are
/// `'static`: they select no impl, and rust-analyzer gives an elided one as an error.
fn normalizes_to_error<'db>(ctx: &Ctx<'_, 'db>, tys: impl IntoIterator<Item = Ty<'db>>) -> bool {
    use hir_ty::next_solver::{infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt};
    use rustc_type_ir::TypeVisitableExt as _;
    let interner = ctx.infcx.interner;
    tys.into_iter().any(|ty| {
        if !ty.has_aliases() {
            return false;
        }
        let ty = rustc_type_ir::fold_regions(interner, ty, |region, depth| match region.kind() {
            RegionKind::ReBound(rustc_type_ir::BoundVarIndexKind::Bound(debruijn), _)
                if debruijn < depth =>
            {
                region
            }
            _ => Region::new_static(interner),
        });
        !ty.references_error()
            && ctx.infcx.probe(|_| {
                ObligationCtxt::new(&ctx.infcx)
                    .deeply_normalize(&ObligationCause::dummy(), ctx.env, ty)
                    .is_ok_and(|it| it.references_error())
            })
    })
}

fn normalize_impl<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>, allow_fresh: bool) -> Option<Ty<'db>> {
    normalize_full(ctx, ty, allow_fresh, &mut Vec::new_in(ctx.frame))
}

/// [`normalize_impl`], with the region constraints normalizing records: from
/// [`normalize_query`], or the solver in the body's inference context where that cannot say.
fn normalize_full<'db>(
    ctx: &Ctx<'_, 'db>,
    ty: Ty<'db>,
    allow_fresh: bool,
    constraints: &mut FVec<'_, hir_ty::next_solver::infer::region_constraints::Constraint<'db>>,
) -> Option<Ty<'db>> {
    use hir_ty::next_solver::{
        infer::{region_constraints::Constraint, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    };
    if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) {
        return None;
    }
    let infcx = &ctx.infcx;
    let first_new = infcx.num_region_vars();
    let normalized = match normalize_canonical(ctx, ty, constraints) {
        Some(it) => it?,
        None => {
            let constraints_before = infcx
                .inner
                .borrow_mut()
                .unwrap_region_constraints()
                .region_constraint_data()
                .constraints
                .len();
            let mut ocx = ObligationCtxt::new(infcx);
            let normalized =
                ocx.structurally_normalize_ty(&ObligationCause::dummy(), ctx.env, ty).ok()?;
            constraints.extend(
                infcx
                    .inner
                    .borrow_mut()
                    .unwrap_region_constraints()
                    .region_constraint_data()
                    .constraints
                    .iter_from(constraints_before),
            );
            normalized
        }
    };
    let mut same = mem::map(ctx.frame);
    for constraint in constraints.iter() {
        if let Constraint::VarSubVar(a, b) = *constraint {
            let (new, old) = if a.index() >= first_new { (a, b) } else { (b, a) };
            if new.index() >= first_new && old.index() < first_new {
                same.entry(new).or_insert(old);
            }
        }
    }
    let mut unmapped = false;
    let normalized = rustc_type_ir::fold_regions(ctx.infcx.interner, normalized, |region, _| {
        match region.kind() {
            RegionKind::ReVar(vid) if vid.index() >= first_new => match same.get(&vid) {
                Some(&old) => Region::new_var(ctx.infcx.interner, old),
                None => {
                    unmapped = true;
                    region
                }
            },
            _ => region,
        }
    });
    (allow_fresh || !unmapped).then_some(normalized)
}

/// The region variables of `value`, resolved as the solver resolves them, numbered in order:
/// `value` with the numbers as its variables, and the region each stands for. `None` where one
/// is of a universe other than the root or resolves to a placeholder, which only the body's
/// inference context can say what it is.
fn canonical_regions<'db, T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
    ctx: &Ctx<'_, 'db>,
    value: T,
    vars: &mut FVec<'_, Region<'db>>,
) -> Option<T> {
    let interner = ctx.infcx.interner;
    let mut here = false;
    let canonical = rustc_type_ir::fold_regions(interner, value, |region, _| {
        let RegionKind::ReVar(vid) = region.kind() else { return region };
        let mut inner = ctx.infcx.inner.borrow_mut();
        let mut constraints = inner.unwrap_region_constraints();
        let resolved = constraints.shallow_resolve_region_var(interner, vid);
        let RegionKind::ReVar(root) = resolved.kind() else {
            here |= resolved.is_placeholder();
            return resolved;
        };
        here |= constraints.try_resolve_region_var(root) != Err(rustc_type_ir::UniverseIndex::ROOT);
        let at = vars.iter().position(|&it| it == resolved).unwrap_or_else(|| {
            vars.push(resolved);
            vars.len() - 1
        });
        Region::new_var(interner, rustc_type_ir::RegionVid::from_usize(at))
    });
    (!here).then_some(canonical)
}

/// A region of a query's answer in the body: one of `vars`, which the query was asked with, a
/// fresh variable for each the query made, in its universe, or a placeholder of the universe
/// made for the query's.
fn region_from_query<'db>(
    ctx: &Ctx<'_, 'db>,
    vars: &[Region<'db>],
    universes: &Universes<'_>,
    made: &mut FMap<'_, u32, Region<'db>>,
    region: Region<'db>,
) -> Region<'db> {
    match region.kind() {
        RegionKind::ReVar(vid) => match vars.get(vid.index()) {
            Some(&it) => it,
            None => *made.entry(vid.as_u32()).or_insert_with(|| {
                match universes.of_var(vid.as_u32()).and_then(|it| universes.get(it)) {
                    Some(universe) => {
                        ctx.infcx.next_region_var_in_universe(universe, hir_ty::Span::Dummy)
                    }
                    None => ctx.infcx.next_region_var(hir_ty::Span::Dummy),
                }
            }),
        },
        RegionKind::RePlaceholder(placeholder) => {
            match universes.get(placeholder.universe.as_u32()) {
                Some(universe) => Region::new_placeholder(
                    ctx.infcx.interner,
                    placeholder.with_updated_universe(universe),
                ),
                None => region,
            }
        }
        _ => region,
    }
}

/// What [`normalize_query`] gives `ty`, in the body's regions: `None` where it must be
/// normalized in the body, `Some(None)` where it cannot be.
fn normalize_canonical<'db>(
    ctx: &Ctx<'_, 'db>,
    ty: Ty<'db>,
    constraints: &mut FVec<'_, Constraint<'db>>,
) -> Option<Option<Ty<'db>>> {
    use rustc_type_ir::TypeVisitableExt as _;
    if ty.has_placeholders() || ty.has_non_region_infer() {
        return None;
    }
    let mut vars = Vec::new_in(ctx.frame);
    let canonical = canonical_regions(ctx, ty, &mut vars)?;
    let env = hir_ty::ParamEnvAndCrate { param_env: ctx.env, krate: ctx.krate };
    let (normalized, pairs) =
        match normalize_query(ctx.db, env.store(), canonical.store(), vars.len() as u32) {
            Normalized::Here => return None,
            Normalized::Fails => return Some(None),
            Normalized::Gives { ty, pairs } => (ty, pairs),
        };
    let interner = ctx.infcx.interner;
    let mut made = mem::map(ctx.frame);
    let universes = Universes::NONE;
    let normalized = rustc_type_ir::fold_regions(interner, normalized.as_ref(), |it, _| {
        region_from_query(ctx, &vars, &universes, &mut made, it)
    });
    constraints.extend(pairs.iter().map(|(a, b)| {
        let a = region_from_query(ctx, &vars, &universes, &mut made, a.as_ref());
        let b = region_from_query(ctx, &vars, &universes, &mut made, b.as_ref());
        match (a.kind(), b.kind()) {
            (RegionKind::ReVar(a), RegionKind::ReVar(b)) => Constraint::VarSubVar(a, b),
            (_, RegionKind::ReVar(b)) => Constraint::RegSubVar(a, b),
            (RegionKind::ReVar(a), _) => Constraint::VarSubReg(a, b),
            _ => Constraint::RegSubReg(a, b),
        }
    }));
    Some(Some(normalized))
}

/// What normalizing a type gives, as the query database keeps it, its region variables numbered as
/// [`Proven`]'s.
#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
enum Normalized {
    /// What it gives names a placeholder, a variable of another universe or a type variable.
    Here,
    Fails,
    Gives {
        ty: StoredTy,
        pairs: Vec<(StoredRegion, StoredRegion)>,
    },
}

/// Normalizes `ty`, whose region variables are the first `vars`, in `env`, with the region
/// constraints that records: as [`prove_query`], once per environment and not again after an
/// edit.
#[blaze_query::tracked(returns(ref))]
fn normalize_query(
    db: &Db,
    env: hir_ty::traits::StoredParamEnvAndCrate,
    ty: StoredTy,
    vars: u32,
) -> Normalized {
    use hir_ty::next_solver::{
        DbInterner, TypingMode,
        infer::{DbInternerInferExt as _, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    };
    use rustc_type_ir::TypeVisitableExt as _;
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let env = env.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate).with_regions();
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        for _ in 0..vars {
            infcx.next_region_var(hir_ty::Span::Dummy);
        }
        let mut ocx = ObligationCtxt::new(&infcx);
        let Ok(normalized) =
            ocx.structurally_normalize_ty(&ObligationCause::dummy(), env.param_env, ty.as_ref())
        else {
            return Normalized::Fails;
        };
        let normalized = infcx.deeply_resolve_ignoring_regions(normalized);
        if normalized.has_non_region_infer() || normalized.has_placeholders() {
            return Normalized::Here;
        }
        let mut inner = infcx.inner.borrow_mut();
        let mut constraints = inner.unwrap_region_constraints();
        let mut pairs = Vec::new_in(frame);
        constraint_pairs(
            interner,
            constraints.region_constraint_data().constraints.iter(),
            &mut pairs,
        );
        let mut own = |region: Region<'_>| match region.kind() {
            RegionKind::RePlaceholder(_) => false,
            RegionKind::ReVar(vid) => {
                vid.as_u32() < vars
                    || constraints.try_resolve_region_var(vid)
                        == Err(rustc_type_ir::UniverseIndex::ROOT)
            }
            _ => true,
        };
        let mut ok = pairs.iter().all(|&(a, b)| own(a) && own(b));
        rustc_type_ir::fold_regions(interner, normalized, |region, _| {
            ok &= own(region);
            region
        });
        if !ok {
            return Normalized::Here;
        }
        Normalized::Gives {
            ty: normalized.store(),
            pairs: mem::owned(pairs.iter().map(|&(a, b)| (a.store(), b.store()))),
        }
    })
}

#[cfg(test)]
mod error_region_tests;

#[cfg(test)]
mod tests {
    use super::Shelf;

    #[test]
    fn a_shelf_gives_back_what_it_took() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let mut shelf: Shelf<'_, u32> = Shelf::new(frame);
            let mut outer = shelf.take();
            outer.extend(0..100);
            let ptr = outer.as_ptr();
            let inner = shelf.take();
            assert_ne!(inner.as_ptr(), ptr);
            shelf.give(inner);
            shelf.give(outer);
            let again = shelf.take();
            assert_eq!((again.len(), again.as_ptr()), (0, ptr));
            shelf.give(again);
            blaze_alloc_count::assert_at_most(0, "a shelf's take and give", || {
                let mut it = shelf.take();
                it.extend(0..100);
                shelf.give(it);
            });
        });
    }
}
