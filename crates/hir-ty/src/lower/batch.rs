//! Signatures lowered a crate and a kind at a time (docs/v2/plan/design/data.md, "batch by
//! kind"): the field types of every variant, the self type, trait and predicates of every impl.
//! A batch is columns: the items in ascending order, their values in a column beside them (the
//! field types as one CSR list), and what lowering found (diagnostics, anonymous constants) in a
//! sparse side table, since nearly every row has none.
//!
//! Only the items of a crate's modules are in its batch. An item declared in a block is in a
//! body, and a signature query reads no body, so such items keep a query of their own.

use std::alloc::Allocator;

use base_db::{Db, Crate};
use blaze_ids::radix;
use blaze_mem::Frame;
use hir_def::{AdtId, ImplId, ModuleDefId, ModuleId, VariantId, nameres::crate_def_map};
use rustc_type_ir::inherent::Ty as _;
use blaze_query::db::AsId;

use crate::{
    TyLoweringDiagnostic,
    db::{AnonConstId, HirDatabase},
    lower::{
        FieldType, FieldTypes, FrameLowered, GenericPredicates, Lowered, generic_predicates,
        lower_field_types, lower_impl_self_ty, lower_impl_trait,
    },
    next_solver::{
        Clauses, DbInterner, ErrorGuaranteed, StoredEarlyBinder, StoredTraitRef, StoredTy, Ty,
    },
};

/// The crate whose batch holds the signatures of the items of `module`, `None` when they have a
/// query each.
///
/// A row of a batch is read through the batch, so its reader depends on every item of that kind
/// in the crate: an edit of one struct's fields recomputes the field types of all the crate's
/// variants (work linear in the crate, where a query per item recomputes one), and every query
/// that read a row of it runs again to find its row unchanged. A dependency crate (origin not
/// `Local`: a library, a lang crate, a rustc crate) is not edited by the user, so its batch is
/// computed once and kept, which costs nothing on an edit and saves a memo, its edges and its
/// allocations per item. A workspace crate is edited, so its items keep a query each. An edited
/// path dependency is a library too: it recomputes its batch, a crate's worth of signatures, on
/// each edit of one of them.
pub(crate) fn batched_crate(db: &Db, module: ModuleId) -> Option<Crate> {
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    (!krate.data(db).origin.is_local()).then_some(krate)
}

/// The diagnostics and anonymous constants of the rows that have any, ascending by row.
#[derive(Debug, Default, PartialEq, Eq, blaze_query::Value)]
pub struct SideTable<'db> {
    pub(super) rows: Box<[u32]>,
    /// Where each of `rows` starts in `diagnostics`; it ends where the next starts.
    pub(super) diagnostics_at: Box<[u32]>,
    /// Where each of `rows` starts in `anon_consts`; it ends where the next starts.
    pub(super) anon_consts_at: Box<[u32]>,
    pub(super) diagnostics: Box<[TyLoweringDiagnostic]>,
    pub(super) anon_consts: Box<[AnonConstId<'db>]>,
}

const _: () = assert!(size_of::<SideTable<'static>>() == 5 * 16);
const _: () = assert!(size_of::<CrateImplSelfTys<'static>>() == 2 * 16 + size_of::<SideTable<'static>>());
const _: () = assert!(size_of::<CrateFieldTypes<'static>>() == 3 * 16 + size_of::<SideTable<'static>>());

impl<'db> SideTable<'db> {
    /// The diagnostics and anonymous constants of `row`.
    pub(crate) fn get(&self, row: u32) -> (&[TyLoweringDiagnostic], &[AnonConstId<'db>]) {
        let Ok(at) = self.rows.binary_search(&row) else { return (&[], &[]) };
        let range = |starts: &[u32], len: usize| {
            let end = starts.get(at + 1).map_or(len, |&it| it as usize);
            starts[at] as usize..end
        };
        (
            &self.diagnostics[range(&self.diagnostics_at, self.diagnostics.len())],
            &self.anon_consts[range(&self.anon_consts_at, self.anon_consts.len())],
        )
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Builds a [`SideTable`], rows pushed in ascending order, in the frame of the run building it.
pub(crate) struct SideTableBuilder<'f, 'db> {
    rows: Vec<u32, &'f Frame>,
    diagnostics_at: Vec<u32, &'f Frame>,
    anon_consts_at: Vec<u32, &'f Frame>,
    diagnostics: Vec<TyLoweringDiagnostic, &'f Frame>,
    anon_consts: Vec<AnonConstId<'db>, &'f Frame>,
}

impl<'f, 'db> SideTableBuilder<'f, 'db> {
    pub(crate) fn new(frame: &'f Frame) -> Self {
        SideTableBuilder {
            rows: Vec::new_in(frame),
            diagnostics_at: Vec::new_in(frame),
            anon_consts_at: Vec::new_in(frame),
            diagnostics: Vec::new_in(frame),
            anon_consts: Vec::new_in(frame),
        }
    }

    pub(crate) fn push(
        &mut self,
        row: u32,
        diagnostics: impl IntoIterator<Item = TyLoweringDiagnostic>,
        anon_consts: impl IntoIterator<Item = AnonConstId<'db>>,
    ) {
        debug_assert!(self.rows.last().is_none_or(|&last| last < row), "rows out of order");
        let (diagnostics_at, anon_consts_at) =
            (self.diagnostics.len() as u32, self.anon_consts.len() as u32);
        self.diagnostics.extend(diagnostics);
        self.anon_consts.extend(anon_consts);
        if self.diagnostics.len() as u32 == diagnostics_at
            && self.anon_consts.len() as u32 == anon_consts_at
        {
            return;
        }
        self.rows.push(row);
        self.diagnostics_at.push(diagnostics_at);
        self.anon_consts_at.push(anon_consts_at);
    }

    pub(crate) fn finish(mut self) -> SideTable<'db> {
        SideTable {
            rows: exact(&mut self.rows),
            diagnostics_at: exact(&mut self.diagnostics_at),
            anon_consts_at: exact(&mut self.anon_consts_at),
            diagnostics: exact(&mut self.diagnostics),
            anon_consts: exact(&mut self.anon_consts),
        }
    }
}

/// A batch's own column of `items`, allocated once at their length: nothing when empty.
fn exact<T>(items: &mut Vec<T, &Frame>) -> Box<[T]> {
    items.drain(..).collect() // alloc: a batch's own column, exact size
}

/// One kind of signature of a crate's items: the items ascending, a value for each.
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
pub struct Batch<'db, K, V> {
    pub(super) keys: Box<[K]>,
    pub(super) values: Box<[V]>,
    pub(super) side: SideTable<'db>,
}

impl<'db, K: Ord + Copy, V> Batch<'db, K, V> {
    /// `lower` of each of `keys`, which are ascending and distinct, each item in a task of its
    /// own: an item's temporaries are gone before the next, so a crate of 10,000 items peaks at
    /// its largest item's, in warm memory. What an item found is copied to `frame`'s side table.
    fn build(
        keys: &[K],
        frame: &Frame,
        mut lower: impl for<'f> FnMut(K, &'f Frame) -> FrameLowered<'f, 'db, V>,
    ) -> Self {
        let mut values = Vec::with_capacity(keys.len()); // alloc: the batch's own column, exact size
        let mut side = SideTableBuilder::new(frame);
        for (row, &key) in keys.iter().enumerate() {
            blaze_mem::frame::task(crate::db::INFER_TAG, |item| {
                let lowered = lower(key, item);
                side.push(row as u32, lowered.diagnostics, lowered.anon_consts);
                values.push(lowered.value);
            });
        }
        // alloc: the batch's keys, exact size; `values` is at its capacity, so boxing it moves
        Batch { keys: Box::from(keys), values: values.into_boxed_slice(), side: side.finish() }
    }

    /// `value()` for each of `keys`: the batch when computing it reached itself, which only code
    /// rustc rejects does (an impl header naming its own self type or trait). Each row is what
    /// the per-item query's cycle result gives.
    fn filled(keys: &[K], value: impl Fn() -> V) -> Self {
        let values = keys.iter().map(|_| value()).collect(); // alloc: the batch's column, exact size
        Batch { keys: Box::from(keys), values, side: SideTable::default() }
    }

    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    fn row(&self, key: K) -> Option<usize> {
        self.keys.binary_search(&key).ok()
    }

    fn lowered(&'db self, row: usize) -> Lowered<'db, V> {
        let (diagnostics, anon_consts) = self.side.get(row as u32);
        Lowered::new(&self.values[row], diagnostics, anon_consts)
    }
}

/// The self type of each impl of a crate's modules.
pub type CrateImplSelfTys<'db> = Batch<'db, ImplId, StoredEarlyBinder<StoredTy>>;

/// The trait of each impl of a crate's modules, `None` for an inherent impl.
pub type CrateImplTraits<'db> = Batch<'db, ImplId, Option<StoredEarlyBinder<StoredTraitRef>>>;

/// The predicates of each impl of a crate's modules, which coherence and method resolution read
/// for all of a crate's impls together.
pub type CrateImplPredicates<'db> = Batch<'db, ImplId, GenericPredicates>;

impl<'db> CrateImplSelfTys<'db> {
    pub fn of(db: &'db Db, krate: Crate) -> &'db Self {
        crate_impl_self_tys(db, krate)
    }

    /// `None` for an impl not of this crate's modules.
    pub fn get(&'db self, impl_id: ImplId) -> Option<Lowered<'db, StoredEarlyBinder<StoredTy>>> {
        Some(self.lowered(self.row(impl_id)?))
    }
}

impl<'db> CrateImplTraits<'db> {
    pub fn of(db: &'db Db, krate: Crate) -> &'db Self {
        crate_impl_traits(db, krate)
    }

    /// `None` for an impl not of this crate's modules, `Some(None)` for an inherent impl.
    #[allow(clippy::option_option)]
    pub fn get(
        &'db self,
        impl_id: ImplId,
    ) -> Option<Option<Lowered<'db, StoredEarlyBinder<StoredTraitRef>>>> {
        let row = self.row(impl_id)?;
        let (diagnostics, anon_consts) = self.side.get(row as u32);
        Some(self.values[row].as_ref().map(|it| Lowered::new(it, diagnostics, anon_consts)))
    }
}

impl<'db> CrateImplPredicates<'db> {
    pub fn of(db: &'db Db, krate: Crate) -> &'db Self {
        crate_impl_predicates(db, krate)
    }

    /// `None` for an impl not of this crate's modules.
    pub fn get(&'db self, impl_id: ImplId) -> Option<Lowered<'db, GenericPredicates>> {
        Some(self.lowered(self.row(impl_id)?))
    }
}

/// The field types of every variant of a crate's modules: the structs, the unions and each
/// variant of the enums, ascending, their fields one CSR list.
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
pub struct CrateFieldTypes<'db> {
    pub(super) variants: Box<[VariantId]>,
    /// The fields of `variants[i]` are `fields[starts[i]..starts[i + 1]]`: one more than
    /// `variants`.
    pub(super) starts: Box<[u32]>,
    pub(super) fields: Box<[FieldType]>,
    pub(super) side: SideTable<'db>,
}

impl<'db> CrateFieldTypes<'db> {
    pub fn of(db: &'db Db, krate: Crate) -> &'db Self {
        crate_field_types(db, krate)
    }

    /// `None` for a variant not of this crate's modules.
    pub fn get(&'db self, variant: VariantId) -> Option<Lowered<'db, FieldTypes>> {
        let row = self.variants.binary_search(&variant).ok()?;
        let fields = &self.fields[self.starts[row] as usize..self.starts[row + 1] as usize];
        let (diagnostics, anon_consts) = self.side.get(row as u32);
        Some(Lowered::new(FieldTypes::from_slice(fields), diagnostics, anon_consts))
    }

    pub fn len(&self) -> usize {
        self.variants.len()
    }

    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
    }

    /// The batch of `lower` of each of `variants`, which are ascending and distinct.
    /// Each variant is lowered in a task of its own, as [`Batch::build`]'s items are.
    pub(crate) fn build(
        variants: &[VariantId],
        frame: &Frame,
        mut lower: impl for<'f> FnMut(
            VariantId,
            &'f Frame,
        ) -> FrameLowered<'f, 'db, Vec<FieldType, &'f Frame>>,
    ) -> Self {
        let mut starts = Vec::with_capacity(variants.len() + 1); // alloc: the batch's column, exact size
        let mut fields = Vec::new_in(frame);
        let mut side = SideTableBuilder::new(frame);
        starts.push(0);
        for (row, &variant) in variants.iter().enumerate() {
            blaze_mem::frame::task(crate::db::INFER_TAG, |item| {
                let lowered = lower(variant, item);
                fields.extend(lowered.value);
                starts.push(
                    u32::try_from(fields.len()).expect("more than 2^32 fields in one crate"),
                );
                side.push(row as u32, lowered.diagnostics, lowered.anon_consts);
            });
        }
        CrateFieldTypes {
            variants: Box::from(variants), // alloc: the batch's column, exact size
            starts: starts.into_boxed_slice(),
            fields: fields.drain(..).collect(), // alloc: the batch's column, exact size
            side: side.finish(),
        }
    }
}

/// The variants of a crate's modules, ascending, in `alloc`.
fn module_variants<A: Allocator>(db: &Db, krate: Crate, alloc: A) -> Vec<VariantId, A> {
    let mut variants = Vec::new_in(alloc);
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            match def {
                ModuleDefId::AdtId(AdtId::StructId(it)) => variants.push(VariantId::from(it)),
                ModuleDefId::AdtId(AdtId::UnionId(it)) => variants.push(VariantId::from(it)),
                ModuleDefId::AdtId(AdtId::EnumId(it)) => variants.extend(
                    it.enum_variants(db).variants.values().map(|&(it, _)| VariantId::from(it)),
                ),
                _ => {}
            }
        }
    }
    radix::sort_copy_by_key(&mut variants, |&it| {
        let (kind, id) = match it {
            VariantId::EnumVariantId(it) => (0, it.as_id()),
            VariantId::StructId(it) => (1, it.as_id()),
            VariantId::UnionId(it) => (2, it.as_id()),
        };
        radix::pack_32(kind, id.as_u32())
    });
    variants.dedup();
    variants
}

/// The impls of a crate's modules, ascending, in `alloc`.
fn module_impls<A: Allocator>(db: &Db, krate: Crate, alloc: A) -> Vec<ImplId, A> {
    let mut impls = Vec::new_in(alloc);
    for (_, module) in crate_def_map(db, krate).modules(db) {
        impls.extend(module.scope(db).impls());
    }
    radix::sort_copy_by_key(&mut impls, |it| it.as_id().as_u32());
    impls.dedup();
    impls
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn crate_field_types<'db>(db: &'db Db, krate: Crate) -> CrateFieldTypes<'db> {
    blaze_mem::frame::current(|frame| {
        let variants = module_variants(db, krate, frame);
        CrateFieldTypes::build(&variants, frame, |variant, item| {
            lower_field_types(db, variant, item)
        })
    })
}

#[blaze_query::tracked(returns(ref), cycle_result = crate_impl_self_tys_cycle, mem_tag = Infer)]
fn crate_impl_self_tys<'db>(db: &'db Db, krate: Crate) -> CrateImplSelfTys<'db> {
    blaze_mem::frame::current(|frame| {
        let impls = module_impls(db, krate, frame);
        Batch::build(&impls, frame, |impl_id, item| lower_impl_self_ty(db, impl_id, item))
    })
}

fn crate_impl_self_tys_cycle<'db>(
    db: &'db Db,
    _: blaze_query::db::Id,
    krate: Crate,
) -> CrateImplSelfTys<'db> {
    let error = StoredEarlyBinder::bind_no_rigid_aliases(
        Ty::new_error(DbInterner::new_no_crate(db), ErrorGuaranteed).store(),
    );
    // clone: an interned row, on a cycle only code rustc rejects reaches
    Batch::filled(&module_impls(db, krate, std::alloc::Global), || error.clone())
}

/// Reads the crate's self types: a trait's arguments may name `Self`.
#[blaze_query::tracked(returns(ref), cycle_result = crate_impl_traits_cycle, mem_tag = Infer)]
fn crate_impl_traits<'db>(db: &'db Db, krate: Crate) -> CrateImplTraits<'db> {
    let self_tys = CrateImplSelfTys::of(db, krate);
    blaze_mem::frame::current(|frame| {
        Batch::build(&self_tys.keys, frame, |impl_id, item| {
            let self_ty = self_tys.get(impl_id).expect("the self types have every impl");
            match lower_impl_trait(db, impl_id, self_ty.value.get().skip_binder(), item) {
                Some(lowered) => FrameLowered {
                    value: Some(lowered.value),
                    diagnostics: lowered.diagnostics,
                    anon_consts: lowered.anon_consts,
                },
                None => FrameLowered::empty(None, item),
            }
        })
    })
}

fn crate_impl_traits_cycle<'db>(
    db: &'db Db,
    _: blaze_query::db::Id,
    krate: Crate,
) -> CrateImplTraits<'db> {
    Batch::filled(&module_impls(db, krate, std::alloc::Global), || None)
}

/// Reads the crate's self types and traits: a where clause may name `Self` and `Self::Assoc`.
#[blaze_query::tracked(returns(ref), cycle_result = crate_impl_predicates_cycle, mem_tag = Infer)]
fn crate_impl_predicates<'db>(db: &'db Db, krate: Crate) -> CrateImplPredicates<'db> {
    let self_tys = CrateImplSelfTys::of(db, krate);
    blaze_mem::frame::current(|frame| {
        Batch::build(&self_tys.keys, frame, |impl_id, item| {
            generic_predicates(db, impl_id.into(), item)
        })
    })
}

fn crate_impl_predicates_cycle<'db>(
    db: &'db Db,
    _: blaze_query::db::Id,
    krate: Crate,
) -> CrateImplPredicates<'db> {
    let empty = StoredEarlyBinder::bind_no_rigid_aliases(Clauses::empty().store());
    Batch::filled(&module_impls(db, krate, std::alloc::Global), || {
        GenericPredicates::from_explicit_own_predicates(empty.clone()) // clone: an interned row, on a cycle
    })
}

#[cfg(test)]
mod tests {
    use hir_def::type_ref::TypeRefId;
    use la_arena::RawIdx;

    use super::*;
    use crate::lower::diagnostics::{GenericArgsProhibitedReason, PathLoweringDiagnostic};

    fn diagnostic(segment: u32) -> TyLoweringDiagnostic {
        TyLoweringDiagnostic::PathDiagnostic {
            source: TypeRefId::from_raw(RawIdx::from_u32(segment)),
            diag: PathLoweringDiagnostic::GenericArgsProhibited {
                segments: Box::new([segment]),
                reason: GenericArgsProhibitedReason::Module,
            },
        }
    }

    #[test]
    fn a_side_table_keeps_only_rows_with_something() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut side = SideTableBuilder::<'_, 'static>::new(frame);
            side.push(0, [], []);
            side.push(1, [diagnostic(1), diagnostic(2)], []);
            side.push(2, [], []);
            side.push(5, [diagnostic(5)], []);
            side.push(9, [], []);
            let side = side.finish();
            assert_eq!(side.len(), 2);
            assert_eq!(side.get(0), (&[][..], &[][..]));
            assert_eq!(side.get(1).0, &[diagnostic(1), diagnostic(2)]);
            assert_eq!(side.get(2).0, &[]);
            assert_eq!(side.get(5).0, &[diagnostic(5)]);
            assert_eq!(side.get(9).0, &[]);
            assert_eq!(side.get(100).0, &[]);
        });
    }

    #[test]
    fn an_empty_side_table_allocates_nothing_and_has_nothing() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let (side, used) = blaze_alloc_count::measure(|| {
                SideTableBuilder::<'_, 'static>::new(frame).finish()
            });
            assert_eq!(used.allocations, 0);
            assert_eq!(side.len(), 0);
            assert!(side.get(0).0.is_empty() && side.get(0).1.is_empty());
        });
    }

    #[test]
    fn a_side_table_column_is_its_exact_length() {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            let mut side = SideTableBuilder::<'_, 'static>::new(frame);
            for row in 0..5 {
                side.push(row, [diagnostic(row)], []);
            }
            let (side, used) = blaze_alloc_count::measure(|| side.finish());
            assert_eq!(used.allocations, 4, "rows, two starts, diagnostics; no constants");
            assert_eq!(side.len(), 5);
        });
    }
}
