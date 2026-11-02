//! Things related to consts in the next-trait-solver.

mod valtree;

use std::{marker::PhantomData, num::NonZeroU32};

use hir_def::ConstParamId;
use macros::{GenericTypeVisitable, TypeFoldable, TypeVisitable};
use rustc_type_ir::{
    DebruijnIndex, FlagComputation, GenericTypeVisitable, TypeFlags, TypeFoldable as _,
    TypeVisitable as _, TypeVisitableExt, WithCachedTypeInfo, inherent::IntoKind,
    intern::Interned, relate::Relate,
};

use crate::{
    ParamEnvAndCrate,
    next_solver::{
        AllocationData, ClauseKind, ParamEnv, impl_foldable_for_interned_slice,
        impl_foldable_for_stored_type, interned_slice, interner::table, persist,
    },
};

use super::{DbInterner, GenericArgs, Ty};

pub use self::valtree::*;

pub type ConstKind<'db> = rustc_type_ir::ConstKind<DbInterner<'db>>;
pub type Const<'db> = rustc_type_ir::Const<DbInterner<'db>>;
pub type AliasConst<'db> = rustc_type_ir::AliasConst<DbInterner<'db>>;
pub type AliasConstKind<'db> = rustc_type_ir::AliasConstKind<DbInterner<'db>>;

/// Every interned [`Const`]. A `GenericArg` holds a row in 30 bits.
static CONSTS: table::Rows<ConstKind<'static>> = table::Rows::new("consts", table::LAST_ROW);
table::erase!(ConstKind);

/// What a [`Const`] holds: its row in [`CONSTS`], which is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct InternedConst<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<InternedConst<'static>>>() == 4);
const _: () = assert!(size_of::<Option<Const<'static>>>() == 4);
// A row costs `table::ROW_BYTES` hot bytes and its kind, at most 48 cold ones.
const _: () = assert!(size_of::<ConstKind<'static>>() <= 48);

impl<'db> InternedConst<'db> {
    #[inline]
    pub(crate) fn intern(kind: ConstKind<'db>) -> Self {
        Self::at(CONSTS.intern(kind, || {
            let computed = FlagComputation::for_const_kind(&kind);
            table::RowInfo {
                tag: persist::tag(&kind),
                flags: computed.flags,
                binder: computed.outer_exclusive_binder,
            }
        }))
    }

    #[inline]
    pub(super) fn at(index: NonZeroU32) -> Self {
        InternedConst { index, _db: PhantomData }
    }

    #[inline]
    pub(super) fn row(self) -> NonZeroU32 {
        self.index
    }

    /// The constant's flags, read from its row's column rather than recomputed from its kind.
    #[inline]
    pub fn type_flags(self) -> TypeFlags {
        CONSTS.flags(self.index)
    }

    #[inline]
    pub fn outer_exclusive_binder(self) -> DebruijnIndex {
        CONSTS.binder(self.index)
    }
}

impl<'db> Interned<DbInterner<'db>> for InternedConst<'db> {
    type Value = WithCachedTypeInfo<ConstKind<'db>>;

    /// The row's kind with the flags and binder its columns hold, computed once when it was
    /// interned.
    #[inline]
    fn get(self) -> WithCachedTypeInfo<ConstKind<'db>> {
        WithCachedTypeInfo {
            internee: CONSTS.kind(self.index),
            flags: CONSTS.flags(self.index),
            outer_exclusive_binder: CONSTS.binder(self.index),
        }
    }
}

impl std::fmt::Debug for InternedConst<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        CONSTS.kind::<ConstKind<'_>>(self.index).fmt(f)
    }
}

// SAFETY: the handle holds one value, its row's kind (the flags and binder beside it are
// computed from the kind), which this visits whenever the visitor asks to go into the row.
unsafe impl<'db, V: super::WorldExposer> GenericTypeVisitable<V> for InternedConst<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(CONSTS.name(), self.index.get()).is_continue() {
            CONSTS.kind::<ConstKind<'db>>(self.index).generic_visit_with(visitor);
        }
    }
}

/// hir-ty's methods on [`Const`] that `rustc_type_ir`'s lacks.
pub trait ConstExt<'db>: Sized {
    /// You should avoid using this if you can: it skips the interner, which a constant
    /// otherwise always goes through.
    fn new_without_interner(kind: ConstKind<'db>) -> Self;

    fn error(interner: DbInterner<'db>) -> Self;

    fn is_error(self) -> bool;

    fn new_valtree(interner: DbInterner<'db>, ty: Ty<'db>, kind: ValTreeKind<'db>) -> Self;

    /// The value tree of an evaluated constant, or why it has none, which the caller
    /// reports (rustc's `ValTreeCreationError`).
    fn new_from_allocation(
        interner: DbInterner<'db>,
        allocation: &AllocationData<'db>,
        param_env: ParamEnvAndCrate<'db>,
    ) -> Result<Self, ValTreeCreationError<'db>>;

    /// This constant's row: the same number for the same constant, on every thread, for the
    /// life of the process.
    fn index(self) -> u32;

    /// The constant at `index`, which [`Self::index`] gave. Panics on any other number.
    fn from_index(interner: DbInterner<'db>, index: u32) -> Self;

    fn store(self) -> StoredConst;
}

impl<'db> ConstExt<'db> for Const<'db> {
    #[inline]
    fn new_without_interner(kind: ConstKind<'db>) -> Self {
        rustc_type_ir::Const(InternedConst::intern(kind))
    }

    #[inline]
    fn error(interner: DbInterner<'db>) -> Self {
        interner.default_types().consts.error
    }

    #[inline]
    fn is_error(self) -> bool {
        matches!(self.kind(), ConstKind::Error(_))
    }

    #[inline]
    fn new_valtree(interner: DbInterner<'db>, ty: Ty<'db>, kind: ValTreeKind<'db>) -> Self {
        Const::new_value(interner, ValTree::new(kind), ty)
    }

    fn new_from_allocation(
        interner: DbInterner<'db>,
        allocation: &AllocationData<'db>,
        param_env: ParamEnvAndCrate<'db>,
    ) -> Result<Self, ValTreeCreationError<'db>> {
        allocation_to_const(
            interner,
            allocation.ty,
            &allocation.memory,
            &allocation.memory_map,
            param_env,
        )
    }

    #[inline]
    fn index(self) -> u32 {
        self.0.index.get()
    }

    fn from_index(_interner: DbInterner<'db>, index: u32) -> Self {
        match NonZeroU32::new(index) {
            Some(index) if CONSTS.published(index) => rustc_type_ir::Const(InternedConst::at(index)),
            _ => panic!("{index} is not the index of an interned constant"),
        }
    }

    #[inline]
    fn store(self) -> StoredConst {
        StoredConst { index: self.0.index }
    }
}

/// A constant as kept in a query result: its row, which stays valid in every revision, as an
/// interned value is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct StoredConst {
    index: NonZeroU32,
}

const _: () = assert!(size_of::<Option<StoredConst>>() == 4);

impl StoredConst {
    #[inline]
    pub fn as_ref<'db>(&self) -> Const<'db> {
        rustc_type_ir::Const(InternedConst::at(self.index))
    }
}

impl std::fmt::Debug for StoredConst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_ref().fmt(f)
    }
}

impl_foldable_for_stored_type!(StoredConst);

pub type PlaceholderConst<'db> = rustc_type_ir::PlaceholderConst<DbInterner<'db>>;

#[derive(Copy, Clone, Hash, Eq, PartialEq)]
pub struct ParamConst {
    // FIXME: See `ParamTy`.
    pub id: ConstParamId,
    pub index: u32,
}

impl std::fmt::Debug for ParamConst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.index)
    }
}

impl ParamConst {
    pub fn find_const_ty_from_env<'db>(self, env: ParamEnv<'db>) -> Ty<'db> {
        let mut candidates = env.clauses.iter().filter_map(|clause| {
            // `ConstArgHasType` are never desugared to be higher ranked.
            match clause.kind().skip_binder() {
                ClauseKind::ConstArgHasType(param_ct, ty) => {
                    assert!(!(param_ct, ty).has_escaping_bound_vars());

                    match param_ct.kind() {
                        ConstKind::Param(param_ct) if param_ct.index == self.index => Some(ty),
                        _ => None,
                    }
                }
                _ => None,
            }
        });

        // N.B. it may be tempting to fix ICEs by making this function return
        // `Option<Ty<'db>>` instead of `Ty<'db>`; however, this is generally
        // considered to be a bandaid solution, since it hides more important
        // underlying issues with how we construct generics and predicates of
        // items. It's advised to fix the underlying issue rather than trying
        // to modify this function.
        let ty = candidates.next().unwrap_or_else(|| {
            panic!("cannot find `{self:?}` in param-env: {env:#?}");
        });
        assert!(
            candidates.next().is_none(),
            "did not expect duplicate `ConstParamHasTy` for `{self:?}` in param-env: {env:#?}"
        );
        ty
    }
}

#[derive(
    Copy, Clone, Debug, Hash, PartialEq, Eq, TypeVisitable, TypeFoldable, GenericTypeVisitable,
)]
pub struct ExprConst;

impl rustc_type_ir::inherent::ParamLike for ParamConst {
    fn index(self) -> u32 {
        self.index
    }
}

pub type BoundConst<'db> = rustc_type_ir::BoundConst<DbInterner<'db>>;

impl<'db> Relate<DbInterner<'db>> for ExprConst {
    fn relate<R: rustc_type_ir::relate::TypeRelation<DbInterner<'db>>>(
        _relation: &mut R,
        a: Self,
        b: Self,
    ) -> rustc_type_ir::relate::RelateResult<DbInterner<'db>, Self> {
        // Ensure we get back to this when we fill in the fields
        let ExprConst = b;
        Ok(a)
    }
}

impl<'db> rustc_type_ir::inherent::ExprConst<DbInterner<'db>> for ExprConst {
    fn args(self) -> <DbInterner<'db> as rustc_type_ir::Interner>::GenericArgs {
        // Ensure we get back to this when we fill in the fields
        let ExprConst = self;
        GenericArgs::default()
    }
}

interned_slice!(ConstsStorage, Consts, StoredConsts, consts, Const<'db>, Const<'static>);
impl_foldable_for_interned_slice!(Consts);

#[cfg(test)]
mod tests {
    use rustc_type_ir::{
        BoundVar, BoundVarIndexKind, ConstVid, Flags, InferConst, IsRigid, UniverseIndex,
    };

    use super::*;
    use crate::next_solver::{ErrorGuaranteed, FreeConstAliasId, TyKind};

    fn free_const_alias(is_rigid: IsRigid) -> ConstKind<'static> {
        let def_id = FreeConstAliasId(blaze_query::db::FromId::from_id(blaze_query::db::Id::from_u32(1)));
        ConstKind::Alias(
            is_rigid,
            AliasConst::from_checked_parts(AliasConstKind::Free { def_id }, GenericArgs::empty()),
        )
    }

    fn every_kind() -> Vec<ConstKind<'static>> {
        let u8 = Ty::new_without_interner(TyKind::Uint(rustc_ast_ir::UintTy::U8));
        let bound = BoundConst::new(BoundVar::from_u32(1));
        vec![
            ConstKind::Infer(InferConst::Var(ConstVid::from_u32(3))),
            ConstKind::Infer(InferConst::Fresh(4)),
            ConstKind::Bound(BoundVarIndexKind::Bound(DebruijnIndex::from_u32(2)), bound),
            ConstKind::Bound(BoundVarIndexKind::Canonical, bound),
            ConstKind::Placeholder(PlaceholderConst::new(UniverseIndex::from_u32(5), bound)),
            free_const_alias(IsRigid::No),
            free_const_alias(IsRigid::Yes),
            ConstKind::Value(ValueConst::new(u8, ValTree::new(ValTreeKind::Leaf(7u8.into())))),
            ConstKind::Error(ErrorGuaranteed),
            ConstKind::Expr(ExprConst),
        ]
    }

    #[test]
    fn a_const_reads_back_its_kind_and_its_row_flags() {
        for kind in every_kind() {
            let konst = Const::new_without_interner(kind);
            assert!(konst.kind() == kind);
            assert!(konst.0.get().internee == kind);
            assert!(Const::new_without_interner(kind) == konst, "interned once");
            let computed = FlagComputation::for_const_kind(&kind);
            assert_eq!(konst.0.type_flags(), computed.flags);
            assert_eq!(konst.flags(), konst.0.type_flags(), "rustc's flags read the column");
            assert_eq!(konst.outer_exclusive_binder(), computed.outer_exclusive_binder);
            assert_eq!(konst.outer_exclusive_binder(), konst.0.outer_exclusive_binder());
            assert!(konst.store().as_ref() == konst);
        }
    }

    #[test]
    fn an_alias_const_is_rigid_or_not_by_its_flag() {
        let rigid = Const::new_without_interner(free_const_alias(IsRigid::Yes));
        let non_rigid = Const::new_without_interner(free_const_alias(IsRigid::No));
        assert!(rigid != non_rigid, "rigidness is part of the kind");
        assert!(rigid.has_rigid_aliases() && !rigid.has_non_rigid_aliases());
        assert!(non_rigid.has_non_rigid_aliases() && !non_rigid.has_rigid_aliases());
        assert!(rigid.flags().contains(TypeFlags::HAS_CONST_ALIAS));
    }

    #[test]
    fn a_bound_const_escapes_one_binder_past_its_index() {
        let konst = Const::new_without_interner(ConstKind::Bound(
            BoundVarIndexKind::Bound(DebruijnIndex::from_u32(1)),
            BoundConst::new(BoundVar::from_u32(0)),
        ));
        assert_eq!(konst.outer_exclusive_binder(), DebruijnIndex::from_u32(2));
        assert!(konst.has_escaping_bound_vars());
    }
}
