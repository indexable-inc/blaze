//! Zero-boilerplate, 100% safe typed Flecs 4.2 query fetching (`QueryFetch`) and chainable
//! filtered query builder (`TypedQuery`).

use core::alloc::Allocator;
use core::any::TypeId;
use core::marker::PhantomData;

use blaze_own::Vec as OwnVec;

use super::column::{downcast_slice, downcast_slice_mut};
use super::error::EcsError;
use super::id::{ComponentId, Entity, Id};
use super::query::QuerySpec;
use super::table::{
    Table, column_index_in, pick_ref_around_1, pick_ref_around_2, split_1mut, split_2mut,
    split_3mut,
};
use super::typed::Relation;
use super::world::World;

/// Stack-allocated fixed buffer of resolved [`ComponentId`]s for a [`QueryFetch`] (zero heap
/// allocation on the query hot path).
#[derive(Clone, Copy)]
pub struct FetchIds {
    ids: [ComponentId; 4],
    len: usize,
}

impl FetchIds {
    #[inline]
    pub(crate) fn from_1(c0: ComponentId) -> Self {
        Self { ids: [c0, c0, c0, c0], len: 1 }
    }

    #[inline]
    pub(crate) fn from_2(c0: ComponentId, c1: ComponentId) -> Self {
        Self { ids: [c0, c1, c0, c0], len: 2 }
    }

    #[inline]
    pub(crate) fn from_3(c0: ComponentId, c1: ComponentId, c2: ComponentId) -> Self {
        Self { ids: [c0, c1, c2, c0], len: 3 }
    }

    #[inline]
    pub(crate) fn from_4(
        c0: ComponentId,
        c1: ComponentId,
        c2: ComponentId,
        c3: ComponentId,
    ) -> Self {
        Self { ids: [c0, c1, c2, c3], len: 4 }
    }

    /// Slice of resolved component IDs in query tuple order.
    #[inline]
    pub fn as_slice(&self) -> &[ComponentId] {
        self.ids.get(..self.len).unwrap_or(&[])
    }
}

#[inline]
fn conflict_for_type<T: 'static, A: Allocator + Copy + 'static>(world: &World<A>) -> EcsError {
    let cid = world.component_id::<T>().unwrap_or(ComponentId(u32::MAX));
    EcsError::ConflictingAccess { id: Id::Component(cid) }
}

#[inline]
fn id_at(ids: &[ComponentId], idx: usize) -> Result<ComponentId, EcsError> {
    ids.get(idx).copied().ok_or(EcsError::MissingComponent(ComponentId(u32::MAX)))
}

fn push_unique_id<A: Allocator + Copy>(vec: &mut OwnVec<Id, A>, id: Id) -> Result<(), EcsError> {
    if !vec.as_slice().contains(&id) {
        vec.try_push(id).map_err(|r| EcsError::Alloc(r.why))?;
    }
    Ok(())
}

fn add_spec_unique<A: Allocator + Copy>(
    spec: QuerySpec<A>,
    cids: &[ComponentId],
) -> Result<QuerySpec<A>, EcsError> {
    let mut out = spec;
    for (i, &cid) in cids.iter().enumerate() {
        let seen_before = cids.get(..i).is_some_and(|prefix| prefix.contains(&cid));
        if !seen_before {
            out = out.with_component(cid)?;
        }
    }
    Ok(out)
}

/// Typed Flecs 4.2 query fetch specification implemented for single references (`&T`, `&mut T`)
/// and 2-, 3-, and 4-component tuples.
///
/// Rejects aliasing conflicts (`(&mut T, &T)`, `(&mut T, &mut T)`, etc.) with
/// [`EcsError::ConflictingAccess`], extracts read/write sets automatically for
/// [`super::pipeline::Pipeline::add_query`], and iterates SoA table columns with zero `unsafe`.
pub trait QueryFetch {
    /// Per-entity borrowed item yielded by [`World::each`] and [`World::each_with_entity`].
    type Item<'a>;
    /// Per-table contiguous SoA slice tuple yielded by [`World::each_chunk`].
    type Chunk<'a>;

    /// Register all component types in `Self` with `world`, validate disjointness, and append
    /// their [`Id`]s to `reads` and `writes` for automatic [`super::pipeline::Pipeline`] batching.
    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError>;

    /// Build a [`QuerySpec`] requiring all components in `Self` (lazily registering any missing
    /// types and rejecting aliasing conflicts).
    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError>;

    /// Validate that `Self` contains no conflicting mutable aliases without mutating `world`.
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError>;

    /// Look up already-registered [`ComponentId`]s for `Self` without mutating `world`.
    /// Returns `None` if any required component has not been registered yet.
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds>;

    /// Invoke `f` for every entity row in `table`.
    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError>;

    /// Invoke `f` once with the contiguous SoA column slice(s) of `table` (skipped if empty).
    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError>;

    /// Invoke `f` with `(Entity, Self::Item<'_>)` for every entity row in `table`.
    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError>;
}

// -----------------------------------------------------------------------------
// 1-component impls: &T, &mut T
// -----------------------------------------------------------------------------

impl<'q, T0: 'static> QueryFetch for &'q T0 {
    type Item<'a> = &'a T0;
    type Chunk<'a> = &'a [T0];

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        _writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        push_unique_id(reads, Id::Component(c0))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        QuerySpec::new_in(alloc).with_component(c0)
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(_world: &World<A>) -> Result<(), EcsError> {
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_1(world.component_id::<T0>()?))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let c0 = id_at(ids, 0)?;
        let s0 = table.column::<T0>(c0)?;
        for v0 in s0 {
            f(v0);
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let c0 = id_at(ids, 0)?;
        let s0 = table.column::<T0>(c0)?;
        if !s0.is_empty() {
            f(s0);
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let c0 = id_at(ids, 0)?;
        let entities = table.entities();
        let s0 = table.column::<T0>(c0)?;
        for (&ent, v0) in entities.iter().zip(s0.iter()) {
            f(ent, v0);
        }
        Ok(())
    }
}

impl<'q, T0: 'static> QueryFetch for &'q mut T0 {
    type Item<'a> = &'a mut T0;
    type Chunk<'a> = &'a mut [T0];

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        _reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        push_unique_id(writes, Id::Component(c0))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        QuerySpec::new_in(alloc).with_component(c0)
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(_world: &World<A>) -> Result<(), EcsError> {
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_1(world.component_id::<T0>()?))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let c0 = id_at(ids, 0)?;
        let s0 = table.column_mut::<T0>(c0)?;
        for v0 in s0 {
            f(v0);
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let c0 = id_at(ids, 0)?;
        let s0 = table.column_mut::<T0>(c0)?;
        if !s0.is_empty() {
            f(s0);
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let c0 = id_at(ids, 0)?;
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let e0 = cols.get_mut(i0).ok_or(EcsError::MissingComponent(c0))?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        for (&ent, v0) in entities.iter().zip(s0.iter_mut()) {
            f(ent, v0);
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// 2-component impls: (&A, &B), (&mut A, &B), (&A, &mut B), (&mut A, &mut B)
// -----------------------------------------------------------------------------

impl<'q0, 'q1, T0: 'static, T1: 'static> QueryFetch for (&'q0 T0, &'q1 T1) {
    type Item<'a> = (&'a T0, &'a T1);
    type Chunk<'a> = (&'a [T0], &'a [T1]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        _writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        push_unique_id(reads, Id::Component(c0))?;
        push_unique_id(reads, Id::Component(c1))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(_world: &World<A>) -> Result<(), EcsError> {
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_2(world.component_id::<T0>()?, world.component_id::<T1>()?))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1) = table.columns_2_ref::<T0, T1>(id_at(ids, 0)?, id_at(ids, 1)?)?;
        for pair in s0.iter().zip(s1.iter()) {
            f(pair);
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1) = table.columns_2_ref::<T0, T1>(id_at(ids, 0)?, id_at(ids, 1)?)?;
        if !s0.is_empty() {
            f((s0, s1));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let entities = table.entities();
        let (s0, s1) = table.columns_2_ref::<T0, T1>(id_at(ids, 0)?, id_at(ids, 1)?)?;
        for (&ent, pair) in entities.iter().zip(s0.iter().zip(s1.iter())) {
            f(ent, pair);
        }
        Ok(())
    }
}

impl<'q0, 'q1, T0: 'static, T1: 'static> QueryFetch for (&'q0 mut T0, &'q1 T1) {
    type Item<'a> = (&'a mut T0, &'a T1);
    type Chunk<'a> = (&'a mut [T0], &'a [T1]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        push_unique_id(writes, Id::Component(c0))?;
        push_unique_id(reads, Id::Component(c1))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T0>() == TypeId::of::<T1>() {
            return Err(conflict_for_type::<T0, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_2(world.component_id::<T0>()?, world.component_id::<T1>()?))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (s0, s1) = table.columns_1_mut_1_ref::<T0, T1>(c0, c1)?;
        for pair in s0.iter_mut().zip(s1.iter()) {
            f(pair);
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (s0, s1) = table.columns_1_mut_1_ref::<T0, T1>(c0, c1)?;
        if !s0.is_empty() {
            f((s0, s1));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let (before, e0, after) = split_1mut(cols, i0, c0)?;
        let e1 = pick_ref_around_1(before, i0, after, i1, c1)?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        let s1 = downcast_slice::<T1, A>(e1.col.as_ref(), c1)?;
        for (&ent, pair) in entities.iter().zip(s0.iter_mut().zip(s1.iter())) {
            f(ent, pair);
        }
        Ok(())
    }
}

impl<'q0, 'q1, T0: 'static, T1: 'static> QueryFetch for (&'q0 T0, &'q1 mut T1) {
    type Item<'a> = (&'a T0, &'a mut T1);
    type Chunk<'a> = (&'a [T0], &'a mut [T1]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        push_unique_id(reads, Id::Component(c0))?;
        push_unique_id(writes, Id::Component(c1))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T0>() == TypeId::of::<T1>() {
            return Err(conflict_for_type::<T1, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_2(world.component_id::<T0>()?, world.component_id::<T1>()?))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        let (s1, s0) = table.columns_1_mut_1_ref::<T1, T0>(c1, c0)?;
        for (v0, v1) in s0.iter().zip(s1.iter_mut()) {
            f((v0, v1));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        let (s1, s0) = table.columns_1_mut_1_ref::<T1, T0>(c1, c0)?;
        if !s0.is_empty() {
            f((s0, s1));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let (before, e1, after) = split_1mut(cols, i1, c1)?;
        let e0 = pick_ref_around_1(before, i1, after, i0, c0)?;
        let s0 = downcast_slice::<T0, A>(e0.col.as_ref(), c0)?;
        let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
        for (&ent, pair) in entities.iter().zip(s0.iter().zip(s1.iter_mut())) {
            f(ent, pair);
        }
        Ok(())
    }
}

impl<'q0, 'q1, T0: 'static, T1: 'static> QueryFetch for (&'q0 mut T0, &'q1 mut T1) {
    type Item<'a> = (&'a mut T0, &'a mut T1);
    type Chunk<'a> = (&'a mut [T0], &'a mut [T1]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        _reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        push_unique_id(writes, Id::Component(c0))?;
        push_unique_id(writes, Id::Component(c1))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T0>() == TypeId::of::<T1>() {
            return Err(conflict_for_type::<T0, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_2(world.component_id::<T0>()?, world.component_id::<T1>()?))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (s0, s1) = table.columns_2_mut::<T0, T1>(c0, c1)?;
        for pair in s0.iter_mut().zip(s1.iter_mut()) {
            f(pair);
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (s0, s1) = table.columns_2_mut::<T0, T1>(c0, c1)?;
        if !s0.is_empty() {
            f((s0, s1));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1) = (id_at(ids, 0)?, id_at(ids, 1)?);
        if c0 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let (_, e0, _, e1, _, _, _) = split_2mut(cols, i0, c0, i1, c1)?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
        for (&ent, pair) in entities.iter().zip(s0.iter_mut().zip(s1.iter_mut())) {
            f(ent, pair);
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// 3-component impls:
// (&A, &B, &C), (&mut A, &B, &C), (&A, &mut B, &C), (&A, &B, &mut C),
// (&mut A, &mut B, &C), (&mut A, &mut B, &mut C)
// -----------------------------------------------------------------------------

impl<'q0, 'q1, 'q2, T0: 'static, T1: 'static, T2: 'static> QueryFetch
    for (&'q0 T0, &'q1 T1, &'q2 T2)
{
    type Item<'a> = (&'a T0, &'a T1, &'a T2);
    type Chunk<'a> = (&'a [T0], &'a [T1], &'a [T2]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        _writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        push_unique_id(reads, Id::Component(c0))?;
        push_unique_id(reads, Id::Component(c1))?;
        push_unique_id(reads, Id::Component(c2))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(_world: &World<A>) -> Result<(), EcsError> {
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_3(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) =
            table.columns_3_ref::<T0, T1, T2>(id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?)?;
        for ((v0, v1), v2) in s0.iter().zip(s1.iter()).zip(s2.iter()) {
            f((v0, v1, v2));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) =
            table.columns_3_ref::<T0, T1, T2>(id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?)?;
        if !s0.is_empty() {
            f((s0, s1, s2));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let entities = table.entities();
        let (s0, s1, s2) =
            table.columns_3_ref::<T0, T1, T2>(id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?)?;
        for (&ent, ((v0, v1), v2)) in entities.iter().zip(s0.iter().zip(s1.iter()).zip(s2.iter())) {
            f(ent, (v0, v1, v2));
        }
        Ok(())
    }
}

impl<'q0, 'q1, 'q2, T0: 'static, T1: 'static, T2: 'static> QueryFetch
    for (&'q0 mut T0, &'q1 T1, &'q2 T2)
{
    type Item<'a> = (&'a mut T0, &'a T1, &'a T2);
    type Chunk<'a> = (&'a mut [T0], &'a [T1], &'a [T2]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        push_unique_id(writes, Id::Component(c0))?;
        push_unique_id(reads, Id::Component(c1))?;
        push_unique_id(reads, Id::Component(c2))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T0>() == TypeId::of::<T1>() || TypeId::of::<T0>() == TypeId::of::<T2>() {
            return Err(conflict_for_type::<T0, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_3(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) = table.columns_3_1mut_2ref::<T0, T1, T2>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
        )?;
        for ((v0, v1), v2) in s0.iter_mut().zip(s1.iter()).zip(s2.iter()) {
            f((v0, v1, v2));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) = table.columns_3_1mut_2ref::<T0, T1, T2>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
        )?;
        if !s0.is_empty() {
            f((s0, s1, s2));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1, c2) = (id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?);
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = column_index_in(cols, c2).ok_or(EcsError::MissingComponent(c2))?;
        let (before, e0, after) = split_1mut(cols, i0, c0)?;
        let e1 = pick_ref_around_1(before, i0, after, i1, c1)?;
        let e2 = pick_ref_around_1(before, i0, after, i2, c2)?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        let s1 = downcast_slice::<T1, A>(e1.col.as_ref(), c1)?;
        let s2 = downcast_slice::<T2, A>(e2.col.as_ref(), c2)?;
        for (&ent, ((v0, v1), v2)) in entities.iter().zip(s0.iter_mut().zip(s1.iter()).zip(s2.iter()))
        {
            f(ent, (v0, v1, v2));
        }
        Ok(())
    }
}

impl<'q0, 'q1, 'q2, T0: 'static, T1: 'static, T2: 'static> QueryFetch
    for (&'q0 T0, &'q1 mut T1, &'q2 T2)
{
    type Item<'a> = (&'a T0, &'a mut T1, &'a T2);
    type Chunk<'a> = (&'a [T0], &'a mut [T1], &'a [T2]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c1 == c0 || c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        push_unique_id(reads, Id::Component(c0))?;
        push_unique_id(writes, Id::Component(c1))?;
        push_unique_id(reads, Id::Component(c2))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c1 == c0 || c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T1>() == TypeId::of::<T0>() || TypeId::of::<T1>() == TypeId::of::<T2>() {
            return Err(conflict_for_type::<T1, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_3(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s1, s0, s2) = table.columns_3_1mut_2ref::<T1, T0, T2>(
            id_at(ids, 1)?,
            id_at(ids, 0)?,
            id_at(ids, 2)?,
        )?;
        for ((v0, v1), v2) in s0.iter().zip(s1.iter_mut()).zip(s2.iter()) {
            f((v0, v1, v2));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s1, s0, s2) = table.columns_3_1mut_2ref::<T1, T0, T2>(
            id_at(ids, 1)?,
            id_at(ids, 0)?,
            id_at(ids, 2)?,
        )?;
        if !s0.is_empty() {
            f((s0, s1, s2));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1, c2) = (id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?);
        if c1 == c0 || c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = column_index_in(cols, c2).ok_or(EcsError::MissingComponent(c2))?;
        let (before, e1, after) = split_1mut(cols, i1, c1)?;
        let e0 = pick_ref_around_1(before, i1, after, i0, c0)?;
        let e2 = pick_ref_around_1(before, i1, after, i2, c2)?;
        let s0 = downcast_slice::<T0, A>(e0.col.as_ref(), c0)?;
        let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
        let s2 = downcast_slice::<T2, A>(e2.col.as_ref(), c2)?;
        for (&ent, ((v0, v1), v2)) in entities.iter().zip(s0.iter().zip(s1.iter_mut()).zip(s2.iter()))
        {
            f(ent, (v0, v1, v2));
        }
        Ok(())
    }
}

impl<'q0, 'q1, 'q2, T0: 'static, T1: 'static, T2: 'static> QueryFetch
    for (&'q0 T0, &'q1 T1, &'q2 mut T2)
{
    type Item<'a> = (&'a T0, &'a T1, &'a mut T2);
    type Chunk<'a> = (&'a [T0], &'a [T1], &'a mut [T2]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c2 == c0 || c2 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c2) });
        }
        push_unique_id(reads, Id::Component(c0))?;
        push_unique_id(reads, Id::Component(c1))?;
        push_unique_id(writes, Id::Component(c2))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c2 == c0 || c2 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c2) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T2>() == TypeId::of::<T0>() || TypeId::of::<T2>() == TypeId::of::<T1>() {
            return Err(conflict_for_type::<T2, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_3(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s2, s0, s1) = table.columns_3_1mut_2ref::<T2, T0, T1>(
            id_at(ids, 2)?,
            id_at(ids, 0)?,
            id_at(ids, 1)?,
        )?;
        for ((v0, v1), v2) in s0.iter().zip(s1.iter()).zip(s2.iter_mut()) {
            f((v0, v1, v2));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s2, s0, s1) = table.columns_3_1mut_2ref::<T2, T0, T1>(
            id_at(ids, 2)?,
            id_at(ids, 0)?,
            id_at(ids, 1)?,
        )?;
        if !s0.is_empty() {
            f((s0, s1, s2));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1, c2) = (id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?);
        if c2 == c0 || c2 == c1 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c2) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = column_index_in(cols, c2).ok_or(EcsError::MissingComponent(c2))?;
        let (before, e2, after) = split_1mut(cols, i2, c2)?;
        let e0 = pick_ref_around_1(before, i2, after, i0, c0)?;
        let e1 = pick_ref_around_1(before, i2, after, i1, c1)?;
        let s0 = downcast_slice::<T0, A>(e0.col.as_ref(), c0)?;
        let s1 = downcast_slice::<T1, A>(e1.col.as_ref(), c1)?;
        let s2 = downcast_slice_mut::<T2, A>(e2.col.as_mut(), c2)?;
        for (&ent, ((v0, v1), v2)) in entities.iter().zip(s0.iter().zip(s1.iter()).zip(s2.iter_mut()))
        {
            f(ent, (v0, v1, v2));
        }
        Ok(())
    }
}

impl<'q0, 'q1, 'q2, T0: 'static, T1: 'static, T2: 'static> QueryFetch
    for (&'q0 mut T0, &'q1 mut T1, &'q2 T2)
{
    type Item<'a> = (&'a mut T0, &'a mut T1, &'a T2);
    type Chunk<'a> = (&'a mut [T0], &'a mut [T1], &'a [T2]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        push_unique_id(writes, Id::Component(c0))?;
        push_unique_id(writes, Id::Component(c1))?;
        push_unique_id(reads, Id::Component(c2))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T0>() == TypeId::of::<T1>() || TypeId::of::<T0>() == TypeId::of::<T2>() {
            return Err(conflict_for_type::<T0, A>(world));
        }
        if TypeId::of::<T1>() == TypeId::of::<T2>() {
            return Err(conflict_for_type::<T1, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_3(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) = table.columns_3_2mut_1ref::<T0, T1, T2>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
        )?;
        for ((v0, v1), v2) in s0.iter_mut().zip(s1.iter_mut()).zip(s2.iter()) {
            f((v0, v1, v2));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) = table.columns_3_2mut_1ref::<T0, T1, T2>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
        )?;
        if !s0.is_empty() {
            f((s0, s1, s2));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1, c2) = (id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?);
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = column_index_in(cols, c2).ok_or(EcsError::MissingComponent(c2))?;
        let (seg0, e0, seg1, e1, seg2, lo, hi) = split_2mut(cols, i0, c0, i1, c1)?;
        let e2 = pick_ref_around_2(seg0, lo, seg1, hi, seg2, i2, c2)?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
        let s2 = downcast_slice::<T2, A>(e2.col.as_ref(), c2)?;
        for (&ent, ((v0, v1), v2)) in
            entities.iter().zip(s0.iter_mut().zip(s1.iter_mut()).zip(s2.iter()))
        {
            f(ent, (v0, v1, v2));
        }
        Ok(())
    }
}

impl<'q0, 'q1, 'q2, T0: 'static, T1: 'static, T2: 'static> QueryFetch
    for (&'q0 mut T0, &'q1 mut T1, &'q2 mut T2)
{
    type Item<'a> = (&'a mut T0, &'a mut T1, &'a mut T2);
    type Chunk<'a> = (&'a mut [T0], &'a mut [T1], &'a mut [T2]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        _reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        push_unique_id(writes, Id::Component(c0))?;
        push_unique_id(writes, Id::Component(c1))?;
        push_unique_id(writes, Id::Component(c2))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        if TypeId::of::<T0>() == TypeId::of::<T1>() || TypeId::of::<T0>() == TypeId::of::<T2>() {
            return Err(conflict_for_type::<T0, A>(world));
        }
        if TypeId::of::<T1>() == TypeId::of::<T2>() {
            return Err(conflict_for_type::<T1, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_3(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) =
            table.columns_3_mut::<T0, T1, T2>(id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?)?;
        for ((v0, v1), v2) in s0.iter_mut().zip(s1.iter_mut()).zip(s2.iter_mut()) {
            f((v0, v1, v2));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2) =
            table.columns_3_mut::<T0, T1, T2>(id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?)?;
        if !s0.is_empty() {
            f((s0, s1, s2));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1, c2) = (id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?);
        if c0 == c1 || c0 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = column_index_in(cols, c2).ok_or(EcsError::MissingComponent(c2))?;
        let (e0, e1, e2) = split_3mut(cols, i0, c0, i1, c1, i2, c2)?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
        let s2 = downcast_slice_mut::<T2, A>(e2.col.as_mut(), c2)?;
        for (&ent, ((v0, v1), v2)) in
            entities.iter().zip(s0.iter_mut().zip(s1.iter_mut()).zip(s2.iter_mut()))
        {
            f(ent, (v0, v1, v2));
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// 4-component impls:
// (&A, &B, &C, &D), (&mut A, &B, &C, &D), (&mut A, &mut B, &C, &D)
// -----------------------------------------------------------------------------

impl<'q0, 'q1, 'q2, 'q3, T0: 'static, T1: 'static, T2: 'static, T3: 'static> QueryFetch
    for (&'q0 T0, &'q1 T1, &'q2 T2, &'q3 T3)
{
    type Item<'a> = (&'a T0, &'a T1, &'a T2, &'a T3);
    type Chunk<'a> = (&'a [T0], &'a [T1], &'a [T2], &'a [T3]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        _writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        let c3 = world.component::<T3>()?;
        push_unique_id(reads, Id::Component(c0))?;
        push_unique_id(reads, Id::Component(c1))?;
        push_unique_id(reads, Id::Component(c2))?;
        push_unique_id(reads, Id::Component(c3))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        let c3 = world.component::<T3>()?;
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2, c3])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(_world: &World<A>) -> Result<(), EcsError> {
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_4(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
            world.component_id::<T3>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2, s3) = table.columns_4_ref::<T0, T1, T2, T3>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
            id_at(ids, 3)?,
        )?;
        for (((v0, v1), v2), v3) in s0.iter().zip(s1.iter()).zip(s2.iter()).zip(s3.iter()) {
            f((v0, v1, v2, v3));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2, s3) = table.columns_4_ref::<T0, T1, T2, T3>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
            id_at(ids, 3)?,
        )?;
        if !s0.is_empty() {
            f((s0, s1, s2, s3));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let entities = table.entities();
        let (s0, s1, s2, s3) = table.columns_4_ref::<T0, T1, T2, T3>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
            id_at(ids, 3)?,
        )?;
        for (&ent, (((v0, v1), v2), v3)) in
            entities.iter().zip(s0.iter().zip(s1.iter()).zip(s2.iter()).zip(s3.iter()))
        {
            f(ent, (v0, v1, v2, v3));
        }
        Ok(())
    }
}

impl<'q0, 'q1, 'q2, 'q3, T0: 'static, T1: 'static, T2: 'static, T3: 'static> QueryFetch
    for (&'q0 mut T0, &'q1 T1, &'q2 T2, &'q3 T3)
{
    type Item<'a> = (&'a mut T0, &'a T1, &'a T2, &'a T3);
    type Chunk<'a> = (&'a mut [T0], &'a [T1], &'a [T2], &'a [T3]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        let c3 = world.component::<T3>()?;
        if c0 == c1 || c0 == c2 || c0 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        push_unique_id(writes, Id::Component(c0))?;
        push_unique_id(reads, Id::Component(c1))?;
        push_unique_id(reads, Id::Component(c2))?;
        push_unique_id(reads, Id::Component(c3))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        let c3 = world.component::<T3>()?;
        if c0 == c1 || c0 == c2 || c0 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2, c3])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        let t0 = TypeId::of::<T0>();
        if t0 == TypeId::of::<T1>() || t0 == TypeId::of::<T2>() || t0 == TypeId::of::<T3>() {
            return Err(conflict_for_type::<T0, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_4(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
            world.component_id::<T3>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2, s3) = table.columns_4_1mut_3ref::<T0, T1, T2, T3>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
            id_at(ids, 3)?,
        )?;
        for (((v0, v1), v2), v3) in s0.iter_mut().zip(s1.iter()).zip(s2.iter()).zip(s3.iter()) {
            f((v0, v1, v2, v3));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2, s3) = table.columns_4_1mut_3ref::<T0, T1, T2, T3>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
            id_at(ids, 3)?,
        )?;
        if !s0.is_empty() {
            f((s0, s1, s2, s3));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1, c2, c3) = (id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?, id_at(ids, 3)?);
        if c0 == c1 || c0 == c2 || c0 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = column_index_in(cols, c2).ok_or(EcsError::MissingComponent(c2))?;
        let i3 = column_index_in(cols, c3).ok_or(EcsError::MissingComponent(c3))?;
        let (before, e0, after) = split_1mut(cols, i0, c0)?;
        let e1 = pick_ref_around_1(before, i0, after, i1, c1)?;
        let e2 = pick_ref_around_1(before, i0, after, i2, c2)?;
        let e3 = pick_ref_around_1(before, i0, after, i3, c3)?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        let s1 = downcast_slice::<T1, A>(e1.col.as_ref(), c1)?;
        let s2 = downcast_slice::<T2, A>(e2.col.as_ref(), c2)?;
        let s3 = downcast_slice::<T3, A>(e3.col.as_ref(), c3)?;
        for (&ent, (((v0, v1), v2), v3)) in
            entities.iter().zip(s0.iter_mut().zip(s1.iter()).zip(s2.iter()).zip(s3.iter()))
        {
            f(ent, (v0, v1, v2, v3));
        }
        Ok(())
    }
}

impl<'q0, 'q1, 'q2, 'q3, T0: 'static, T1: 'static, T2: 'static, T3: 'static> QueryFetch
    for (&'q0 mut T0, &'q1 mut T1, &'q2 T2, &'q3 T3)
{
    type Item<'a> = (&'a mut T0, &'a mut T1, &'a T2, &'a T3);
    type Chunk<'a> = (&'a mut [T0], &'a mut [T1], &'a [T2], &'a [T3]);

    fn register_access<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        reads: &mut OwnVec<Id, A>,
        writes: &mut OwnVec<Id, A>,
    ) -> Result<(), EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        let c3 = world.component::<T3>()?;
        if c0 == c1 || c0 == c2 || c0 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 || c1 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        push_unique_id(writes, Id::Component(c0))?;
        push_unique_id(writes, Id::Component(c1))?;
        push_unique_id(reads, Id::Component(c2))?;
        push_unique_id(reads, Id::Component(c3))
    }

    fn build_spec<A: Allocator + Copy + 'static>(
        world: &mut World<A>,
        alloc: A,
    ) -> Result<QuerySpec<A>, EcsError> {
        let c0 = world.component::<T0>()?;
        let c1 = world.component::<T1>()?;
        let c2 = world.component::<T2>()?;
        let c3 = world.component::<T3>()?;
        if c0 == c1 || c0 == c2 || c0 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 || c1 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        add_spec_unique(QuerySpec::new_in(alloc), &[c0, c1, c2, c3])
    }

    #[inline]
    fn check_conflicts<A: Allocator + Copy + 'static>(world: &World<A>) -> Result<(), EcsError> {
        let t0 = TypeId::of::<T0>();
        let t1 = TypeId::of::<T1>();
        if t0 == t1 || t0 == TypeId::of::<T2>() || t0 == TypeId::of::<T3>() {
            return Err(conflict_for_type::<T0, A>(world));
        }
        if t1 == TypeId::of::<T2>() || t1 == TypeId::of::<T3>() {
            return Err(conflict_for_type::<T1, A>(world));
        }
        Ok(())
    }

    #[inline]
    fn lookup_ids<A: Allocator + Copy + 'static>(world: &World<A>) -> Option<FetchIds> {
        Some(FetchIds::from_4(
            world.component_id::<T0>()?,
            world.component_id::<T1>()?,
            world.component_id::<T2>()?,
            world.component_id::<T3>()?,
        ))
    }

    fn for_each_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Item<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2, s3) = table.columns_4_2mut_2ref::<T0, T1, T2, T3>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
            id_at(ids, 3)?,
        )?;
        for (((v0, v1), v2), v3) in s0.iter_mut().zip(s1.iter_mut()).zip(s2.iter()).zip(s3.iter()) {
            f((v0, v1, v2, v3));
        }
        Ok(())
    }

    fn for_each_chunk_in_table<A: Allocator + Copy + 'static, F: FnMut(Self::Chunk<'_>)>(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (s0, s1, s2, s3) = table.columns_4_2mut_2ref::<T0, T1, T2, T3>(
            id_at(ids, 0)?,
            id_at(ids, 1)?,
            id_at(ids, 2)?,
            id_at(ids, 3)?,
        )?;
        if !s0.is_empty() {
            f((s0, s1, s2, s3));
        }
        Ok(())
    }

    fn for_each_with_entity_in_table<
        A: Allocator + Copy + 'static,
        F: FnMut(Entity, Self::Item<'_>),
    >(
        table: &mut Table<A>,
        ids: &[ComponentId],
        f: &mut F,
    ) -> Result<(), EcsError> {
        let (c0, c1, c2, c3) = (id_at(ids, 0)?, id_at(ids, 1)?, id_at(ids, 2)?, id_at(ids, 3)?);
        if c0 == c1 || c0 == c2 || c0 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c0) });
        }
        if c1 == c2 || c1 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        let (entities, cols) = table.entities_and_columns_mut();
        let i0 = column_index_in(cols, c0).ok_or(EcsError::MissingComponent(c0))?;
        let i1 = column_index_in(cols, c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = column_index_in(cols, c2).ok_or(EcsError::MissingComponent(c2))?;
        let i3 = column_index_in(cols, c3).ok_or(EcsError::MissingComponent(c3))?;
        let (seg0, e0, seg1, e1, seg2, lo, hi) = split_2mut(cols, i0, c0, i1, c1)?;
        let e2 = pick_ref_around_2(seg0, lo, seg1, hi, seg2, i2, c2)?;
        let e3 = pick_ref_around_2(seg0, lo, seg1, hi, seg2, i3, c3)?;
        let s0 = downcast_slice_mut::<T0, A>(e0.col.as_mut(), c0)?;
        let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
        let s2 = downcast_slice::<T2, A>(e2.col.as_ref(), c2)?;
        let s3 = downcast_slice::<T3, A>(e3.col.as_ref(), c3)?;
        for (&ent, (((v0, v1), v2), v3)) in
            entities.iter().zip(s0.iter_mut().zip(s1.iter_mut()).zip(s2.iter()).zip(s3.iter()))
        {
            f(ent, (v0, v1, v2, v3));
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Fluent TypedQuery<'w, Q, A> Builder
// -----------------------------------------------------------------------------

/// Chainable filtered typed query builder created by [`World::query`].
pub struct TypedQuery<'w, Q: QueryFetch, A: Allocator + Copy + 'static> {
    world: &'w mut World<A>,
    spec: QuerySpec<A>,
    _marker: PhantomData<fn() -> Q>,
}

impl<'w, Q: QueryFetch, A: Allocator + Copy + 'static> TypedQuery<'w, Q, A> {
    #[inline]
    pub(crate) fn new(world: &'w mut World<A>) -> Self {
        let alloc = world.alloc();
        Self { world, spec: QuerySpec::new_in(alloc), _marker: PhantomData }
    }

    /// Exclude entities carrying component or typed tag `T`.
    pub fn without<T: 'static>(mut self) -> Result<Self, EcsError> {
        if let Some(cid) = self.world.component_id::<T>() {
            self.spec = self.spec.without_component(cid)?;
        }
        if let Some(tag) = self.world.tag_entity_id::<T>() {
            self.spec = self.spec.without_tag(tag)?;
        }
        Ok(self)
    }

    /// Exclude entities carrying component `T`.
    pub fn without_component<T: 'static>(mut self) -> Result<Self, EcsError> {
        if let Some(cid) = self.world.component_id::<T>() {
            self.spec = self.spec.without_component(cid)?;
        }
        Ok(self)
    }

    /// Exclude entities carrying typed tag `T`.
    pub fn without_tag<T: 'static>(mut self) -> Result<Self, EcsError> {
        if let Some(tag) = self.world.tag_entity_id::<T>() {
            self.spec = self.spec.without_tag(tag)?;
        }
        Ok(self)
    }

    /// Require entities carrying typed tag `T`.
    pub fn with_tag<T: 'static>(mut self) -> Result<Self, EcsError> {
        let tag = self.world.tag_entity::<T>()?;
        self.spec = self.spec.with_tag(tag)?;
        Ok(self)
    }

    /// Require entities carrying typed relation pair `(R, target)`.
    pub fn with_relation<R: Relation>(mut self, target: Entity) -> Result<Self, EcsError> {
        let rel = self.world.relation_entity::<R>()?;
        self.spec = self.spec.with_pair(rel, target)?;
        Ok(self)
    }

    /// Exclude entities carrying typed relation pair `(R, target)`.
    pub fn without_relation<R: Relation>(mut self, target: Entity) -> Result<Self, EcsError> {
        if let Some(rel) = self.world.relation_entity_id::<R>() {
            self.spec = self.spec.without_pair(rel, target)?;
        }
        Ok(self)
    }

    /// Require entities carrying any target for typed relation `(R, *)`.
    pub fn with_any_relation<R: Relation>(mut self) -> Result<Self, EcsError> {
        let rel = self.world.relation_entity::<R>()?;
        let wc = self.world.wildcard();
        self.spec = self.spec.with_pair(rel, wc)?;
        Ok(self)
    }

    /// Include `PREFAB`-tagged entities in results (excluded by default).
    #[inline]
    pub fn include_prefabs(mut self) -> Self {
        self.spec = self.spec.include_prefabs(true);
        self
    }

    /// Iterate all matching entities yielding `Q::Item<'_>`.
    #[inline]
    pub fn each(self, f: impl for<'a> FnMut(Q::Item<'a>)) -> Result<(), EcsError> {
        self.world.each_filtered::<Q>(&self.spec, f)
    }

    /// Iterate all matching non-empty archetype tables yielding contiguous SoA `Q::Chunk<'_>`.
    #[inline]
    pub fn each_chunk(self, f: impl for<'a> FnMut(Q::Chunk<'a>)) -> Result<(), EcsError> {
        self.world.each_chunk_filtered::<Q>(&self.spec, f)
    }

    /// Iterate all matching entities yielding `(Entity, Q::Item<'_>)`.
    #[inline]
    pub fn each_with_entity(
        self,
        f: impl for<'a> FnMut(Entity, Q::Item<'a>),
    ) -> Result<(), EcsError> {
        self.world.each_with_entity_filtered::<Q>(&self.spec, f)
    }

    /// Count matching live entities.
    pub fn count(self) -> Result<usize, EcsError> {
        self.world.count_filtered::<Q>(&self.spec)
    }
}
