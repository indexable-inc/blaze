//! Things related to regions.

use std::{marker::PhantomData, num::NonZeroU32};

use hir_def::LifetimeParamId;
use rustc_type_ir::{
    BoundVarIndexKind, DebruijnIndex, GenericTypeVisitable, INNERMOST, TypeFlags,
    inherent::{DefIdGetter, RegionName},
    intern::Interned,
};

use crate::next_solver::{
    GenericArg, OutlivesClause, Symbol, impl_foldable_for_interned_slice, interned_slice,
    interner::table, persist,
};

use super::{SolverDefId, interner::DbInterner};

pub type RegionKind<'db> = rustc_type_ir::RegionKind<DbInterner<'db>>;
pub type RegionConstraint<'db> = rustc_type_ir::RegionConstraint<DbInterner<'db>>;
pub type Region<'db> = rustc_type_ir::Region<DbInterner<'db>>;

/// Every interned [`Region`]. A `GenericArg` holds a row in 30 bits.
static REGIONS: table::Rows<RegionKind<'static>> = table::Rows::new("regions", table::LAST_ROW);
table::erase!(RegionKind);

/// What a [`Region`] holds: its row in [`REGIONS`], which is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct InternedRegion<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<InternedRegion<'static>>>() == 4);
const _: () = assert!(size_of::<Option<Region<'static>>>() == 4);
// A row costs `table::ROW_BYTES` hot bytes and its kind, at most 48 cold ones.
const _: () = assert!(size_of::<RegionKind<'static>>() <= 48);

impl<'db> InternedRegion<'db> {
    #[inline]
    pub(crate) fn intern(kind: RegionKind<'db>) -> Self {
        Self::at(REGIONS.intern(kind, || table::RowInfo {
            tag: persist::tag(&kind),
            flags: region_flags(kind),
            binder: region_binder(kind),
        }))
    }

    #[inline]
    pub(super) fn at(index: NonZeroU32) -> Self {
        InternedRegion { index, _db: PhantomData }
    }

    #[inline]
    pub(super) fn row(self) -> NonZeroU32 {
        self.index
    }

    /// The region's flags, read from its row's column rather than recomputed from its kind.
    #[inline]
    pub fn type_flags(self) -> TypeFlags {
        REGIONS.flags(self.index)
    }

    #[inline]
    pub fn outer_exclusive_binder(self) -> DebruijnIndex {
        REGIONS.binder(self.index)
    }
}

impl<'db> Interned<DbInterner<'db>> for InternedRegion<'db> {
    type Value = RegionKind<'db>;

    #[inline]
    fn get(self) -> RegionKind<'db> {
        REGIONS.kind(self.index)
    }
}

impl std::fmt::Debug for InternedRegion<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

// SAFETY: the handle holds one value, its row's kind, which this visits whenever the visitor
// asks to go into the row.
unsafe impl<'db, V: super::WorldExposer> GenericTypeVisitable<V> for InternedRegion<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(REGIONS.name(), self.index.get()).is_continue() {
            self.get().generic_visit_with(visitor);
        }
    }
}

/// hir-ty's methods on [`Region`] that `rustc_type_ir`'s lacks.
pub trait RegionExt<'db>: Sized {
    fn new(interner: DbInterner<'db>, kind: RegionKind<'db>) -> Self;

    /// You should avoid using this if you can: it skips the interner, which a region
    /// otherwise always goes through.
    fn new_without_interner(kind: RegionKind<'db>) -> Self;

    fn new_erased(interner: DbInterner<'db>) -> Self;

    fn error(interner: DbInterner<'db>) -> Self;

    /// This region's row: the same number for the same region, on every thread, for the life
    /// of the process.
    fn index(self) -> u32;

    /// The region at `index`, which [`Self::index`] gave. Panics on any other number.
    fn from_index(interner: DbInterner<'db>, index: u32) -> Self;

    fn store(self) -> StoredRegion;
}

impl<'db> RegionExt<'db> for Region<'db> {
    #[inline]
    fn new(_interner: DbInterner<'db>, kind: RegionKind<'db>) -> Self {
        rustc_type_ir::Region(InternedRegion::intern(kind))
    }

    #[inline]
    fn new_without_interner(kind: RegionKind<'db>) -> Self {
        rustc_type_ir::Region(InternedRegion::intern(kind))
    }

    #[inline]
    fn new_erased(interner: DbInterner<'db>) -> Self {
        interner.default_types().regions.erased
    }

    #[inline]
    fn error(interner: DbInterner<'db>) -> Self {
        interner.default_types().regions.error
    }

    #[inline]
    fn index(self) -> u32 {
        self.0.index.get()
    }

    fn from_index(_interner: DbInterner<'db>, index: u32) -> Self {
        match NonZeroU32::new(index) {
            Some(index) if REGIONS.published(index) => rustc_type_ir::Region(InternedRegion::at(index)),
            _ => panic!("{index} is not the index of an interned region"),
        }
    }

    #[inline]
    fn store(self) -> StoredRegion {
        StoredRegion { index: self.0.index }
    }
}

/// A region as kept in a query result: its row, which stays valid in every revision, as an
/// interned value is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct StoredRegion {
    index: NonZeroU32,
}

const _: () = assert!(size_of::<Option<StoredRegion>>() == 4);

impl StoredRegion {
    #[inline]
    pub fn as_ref<'db>(&self) -> Region<'db> {
        rustc_type_ir::Region(InternedRegion::at(self.index))
    }
}

impl std::fmt::Debug for StoredRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_ref().fmt(f)
    }
}

/// The flags of a region of `kind`, as `rustc_middle` computes them.
fn region_flags(kind: RegionKind<'_>) -> TypeFlags {
    let mut flags = TypeFlags::empty();
    match kind {
        RegionKind::ReVar(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
            flags |= TypeFlags::HAS_RE_INFER;
        }
        RegionKind::RePlaceholder(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
            flags |= TypeFlags::HAS_RE_PLACEHOLDER;
        }
        RegionKind::ReEarlyParam(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
            flags |= TypeFlags::HAS_RE_PARAM;
        }
        RegionKind::ReLateParam(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
        }
        RegionKind::ReStatic => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
        }
        RegionKind::ReBound(BoundVarIndexKind::Canonical, ..) => {
            flags |= TypeFlags::HAS_RE_BOUND;
            flags |= TypeFlags::HAS_CANONICAL_BOUND;
        }
        RegionKind::ReBound(BoundVarIndexKind::Bound(..), ..) => {
            flags |= TypeFlags::HAS_RE_BOUND;
        }
        RegionKind::ReErased => {
            flags |= TypeFlags::HAS_RE_ERASED;
        }
        RegionKind::ReError(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_RE_ERROR;
        }
    }
    flags
}

fn region_binder(kind: RegionKind<'_>) -> DebruijnIndex {
    match kind {
        RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), _) => debruijn.shifted_in(1),
        _ => INNERMOST,
    }
}

pub type PlaceholderRegion<'db> = rustc_type_ir::PlaceholderRegion<DbInterner<'db>>;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct EarlyParamRegion {
    // FIXME: See `ParamTy`.
    pub id: LifetimeParamId,
    pub index: u32,
}

/// What a liberated late-bound region (`ReLateParam`) names within its scope: the bound
/// region it was liberated from, var and kind, so two anonymous ones stay apart.
pub type LateParamRegionKind<'db> = BoundRegion<'db>;
pub type LateParamRegion<'db> = rustc_type_ir::LateParamRegion<DbInterner<'db>>;

pub type BoundRegion<'db> = rustc_type_ir::BoundRegion<DbInterner<'db>>;
pub type BoundRegionKind<'db> = rustc_type_ir::BoundRegionKind<DbInterner<'db>>;

impl rustc_type_ir::inherent::ParamLike for EarlyParamRegion {
    fn index(self) -> u32 {
        self.index
    }
}

impl<'db> RegionName<DbInterner<'db>> for EarlyParamRegion {
    /// The lifetime parameter's declared name, or `None` for an elided one (`'_`), as rustc's.
    fn get_name(&self, interner: DbInterner<'db>) -> Option<Symbol> {
        let params = hir_def::hir::generics::GenericParams::of(interner.db, self.id.parent);
        let param = &params[self.id.local_id];
        let name = *param.name.symbol();
        (name != intern::sym::tick_underscore && !param.is_fresh()).then_some(Symbol(name))
    }

    fn is_named(&self, interner: DbInterner<'db>) -> bool {
        self.get_name(interner).is_some()
    }
}

impl<'db> RegionName<DbInterner<'db>> for BoundRegion<'db> {
    fn get_name(&self, interner: DbInterner<'db>) -> Option<Symbol> {
        self.kind.get_name(interner)
    }

    fn is_named(&self, interner: DbInterner<'db>) -> bool {
        self.kind.is_named(interner)
    }
}

impl<'db> DefIdGetter<DbInterner<'db>> for BoundRegion<'db> {
    fn get_def_id(self) -> Option<SolverDefId<'db>> {
        self.kind.get_id()
    }
}

impl std::fmt::Debug for EarlyParamRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.index)
        // write!(f, "{}/#{}", self.name, self.index)
    }
}

type GenericArgOutlivesClause<'db> = OutlivesClause<'db, GenericArg<'db>>;

interned_slice!(
    RegionAssumptionsStorage,
    RegionAssumptions,
    StoredRegionAssumptions,
    region_assumptions,
    GenericArgOutlivesClause<'db>,
    GenericArgOutlivesClause<'static>,
);
impl_foldable_for_interned_slice!(RegionAssumptions);

#[cfg(test)]
mod tests {
    use rustc_type_ir::{BoundVar, Flags, RegionVid, TypeVisitableExt, UniverseIndex};

    use super::*;

    fn every_kind() -> Vec<RegionKind<'static>> {
        let bound = BoundRegion { var: BoundVar::from_u32(1), kind: BoundRegionKind::Anon };
        vec![
            RegionKind::ReBound(BoundVarIndexKind::Bound(DebruijnIndex::from_u32(2)), bound),
            RegionKind::ReBound(BoundVarIndexKind::Canonical, bound),
            RegionKind::ReStatic,
            RegionKind::ReVar(RegionVid::from_u32(7)),
            RegionKind::RePlaceholder(PlaceholderRegion::new(UniverseIndex::from_u32(3), bound)),
            RegionKind::ReErased,
            RegionKind::ReError(crate::next_solver::ErrorGuaranteed),
        ]
    }

    #[test]
    fn a_region_reads_back_its_kind_and_its_row_flags() {
        for kind in every_kind() {
            let region = Region::new_without_interner(kind);
            assert_eq!(region.kind(), kind);
            assert_eq!(region.0.get(), kind);
            assert_eq!(Region::new_without_interner(kind), region, "interned once");
            assert_eq!(region.0.type_flags(), region_flags(kind));
            assert_eq!(region.flags(), region.0.type_flags(), "rustc's flags match the column");
            assert_eq!(region.outer_exclusive_binder(), region.0.outer_exclusive_binder());
            assert_eq!(region.store().as_ref(), region);
        }
    }

    #[test]
    fn a_bound_region_escapes_one_binder_past_its_index() {
        let bound = BoundRegion { var: BoundVar::from_u32(0), kind: BoundRegionKind::Anon };
        let region = Region::new_without_interner(RegionKind::ReBound(
            BoundVarIndexKind::Bound(DebruijnIndex::from_u32(1)),
            bound,
        ));
        assert_eq!(region.outer_exclusive_binder(), DebruijnIndex::from_u32(2));
        assert!(region.has_escaping_bound_vars());
    }

    #[test]
    fn a_late_param_keeps_its_bound_var() {
        let a = BoundRegion { var: BoundVar::from_u32(0), kind: BoundRegionKind::Anon };
        let b = BoundRegion { var: BoundVar::from_u32(1), kind: BoundRegionKind::Anon };
        let scope: SolverDefId<'static> = SolverDefId::StaticId(
            blaze_query::db::FromId::from_id(blaze_query::db::Id::from_u32(1)),
        );
        let late = |kind| RegionKind::ReLateParam(LateParamRegion { scope, kind });
        assert_ne!(Region::new_without_interner(late(a)), Region::new_without_interner(late(b)));
        assert_eq!(a.get_def_id(), None);
    }
}
