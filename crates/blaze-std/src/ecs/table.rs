//! Archetype SoA tables and Flecs-style table-graph transition edges.
//!
//! Each [`Table`] stores all entities sharing an identical sorted canonical archetype (`&[Id]`)
//! along with parallel dense [`AnyColumn`] storage for each data-bearing component and cached
//! `add_edges` / `remove_edges` for O(1) steady-state archetype transitions.

use core::alloc::Allocator;
use core::borrow::Borrow;
use core::cmp::Ordering;

use blaze_own::{Box as OwnBox, Vec as OwnVec};

use crate::data::Value;

use super::column::{
    AnyColumn, ComponentMeta, downcast_slice, downcast_slice_mut, downcast_typed,
    downcast_typed_mut,
};
use super::error::EcsError;
use super::id::{ComponentId, Entity, Id, TableId, u32_to_usize};
use super::map::FlatMap;
use super::reflect::ReflectError;

/// Canonical sorted archetype key usable for zero-allocation `&[Id]` lookups in [`FlatMap`].
pub(crate) struct ArchetypeKey<A: Allocator + Copy>(pub(crate) OwnVec<Id, A>);

impl<A: Allocator + Copy> ArchetypeKey<A> {
    pub(crate) fn from_slice_in(ids: &[Id], alloc: A) -> Result<Self, EcsError> {
        let mut v = OwnVec::try_with_capacity_in(ids.len(), alloc)?;
        for &id in ids {
            v.try_push(id).map_err(|r| EcsError::Alloc(r.why))?;
        }
        Ok(Self(v))
    }
}

impl<A: Allocator + Copy> PartialEq for ArchetypeKey<A> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

impl<A: Allocator + Copy> Eq for ArchetypeKey<A> {}

impl<A: Allocator + Copy> PartialOrd for ArchetypeKey<A> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<A: Allocator + Copy> Ord for ArchetypeKey<A> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_slice().cmp(other.0.as_slice())
    }
}

impl<A: Allocator + Copy> Borrow<[Id]> for ArchetypeKey<A> {
    #[inline]
    fn borrow(&self) -> &[Id] {
        self.0.as_slice()
    }
}

/// One column entry inside a [`Table`], paired with its [`ComponentId`].
pub(crate) struct TableColumn<A: Allocator + Copy> {
    pub(crate) id: ComponentId,
    pub(crate) col: OwnBox<dyn AnyColumn<A>, A>,
}

/// Dense SoA archetype table holding all entities with a specific sorted set of [`Id`]s.
pub struct Table<A: Allocator + Copy + 'static> {
    pub(crate) id: TableId,
    pub(crate) archetype: OwnVec<Id, A>,
    pub(crate) entities: OwnVec<Entity, A>,
    pub(crate) columns: OwnVec<TableColumn<A>, A>,
    pub(crate) add_edges: FlatMap<Id, TableId, A>,
    pub(crate) remove_edges: FlatMap<Id, TableId, A>,
}

impl<A: Allocator + Copy + 'static> Table<A> {
    /// Construct a new empty table for the sorted canonical `archetype`.
    pub(crate) fn new_in(
        id: TableId,
        archetype: OwnVec<Id, A>,
        components: &[ComponentMeta<A>],
        alloc: A,
    ) -> Result<Self, EcsError> {
        let mut columns = OwnVec::new_in(alloc);
        for &entry in archetype.as_slice() {
            if let Id::Component(cid) = entry {
                let meta = components
                    .get(u32_to_usize(cid.0))
                    .ok_or(EcsError::UnknownComponent(cid))?;
                let col = meta.create_column(alloc);
                columns
                    .try_push(TableColumn { id: cid, col })
                    .map_err(|r| EcsError::Alloc(r.why))?;
            }
        }
        // `archetype` is sorted by `Id`, and `Id::Component(a) < Id::Component(b)` iff `a < b`,
        // so `columns` is already strictly sorted by `ComponentId`.
        Ok(Self {
            id,
            archetype,
            entities: OwnVec::new_in(alloc),
            columns,
            add_edges: FlatMap::new_in(alloc),
            remove_edges: FlatMap::new_in(alloc),
        })
    }

    /// Table identifier.
    #[inline]
    pub fn id(&self) -> TableId {
        self.id
    }

    /// Sorted canonical archetype slice of all components, tags, and pairs in this table.
    #[inline]
    pub fn archetype(&self) -> &[Id] {
        self.archetype.as_slice()
    }

    /// Dense slice of entities currently stored in this table (aligned 1:1 with every column).
    #[inline]
    pub fn entities(&self) -> &[Entity] {
        self.entities.as_slice()
    }

    /// Number of entities in this table.
    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether this table currently stores zero entities.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Number of data-bearing component columns in this table.
    #[inline]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Number of cached add-transition edges out of this table.
    #[inline]
    pub fn add_edge_count(&self) -> usize {
        self.add_edges.len()
    }

    /// Number of cached remove-transition edges out of this table.
    #[inline]
    pub fn remove_edge_count(&self) -> usize {
        self.remove_edges.len()
    }

    /// O(log K) exact membership check for an [`Id`] in this table's sorted archetype.
    #[inline]
    pub fn has_id(&self, id: Id) -> bool {
        self.archetype.as_slice().binary_search(&id).is_ok()
    }

    /// Check whether this table's archetype contains any ID matching `pattern` (supporting
    /// `wildcard` in either position of an [`Id::Pair`]).
    #[inline]
    pub fn matches_id_pattern(&self, pattern: Id, wildcard: Entity) -> bool {
        match pattern {
            Id::Pair(pr, pt) if pr == wildcard || pt == wildcard => {
                self.archetype.as_slice().iter().any(|&id| id.matches_pattern(pattern, wildcard))
            }
            exact => self.has_id(exact),
        }
    }

    /// Binary search for the column slot of `cid`.
    #[inline]
    pub(crate) fn column_index(&self, cid: ComponentId) -> Option<usize> {
        column_index_in(self.columns.as_slice(), cid)
    }

    /// Simultaneously borrow the immutable entity slice and mutable column slice of this table.
    #[inline]
    pub(crate) fn entities_and_columns_mut(&mut self) -> (&[Entity], &mut [TableColumn<A>]) {
        (self.entities.as_slice(), self.columns.as_mut_slice())
    }

    /// Contiguous read-only slice for component `cid`.
    pub fn column<T: 'static>(&self, cid: ComponentId) -> Result<&[T], EcsError> {
        let idx = self.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
        let entry = self.columns.get(idx).ok_or(EcsError::MissingComponent(cid))?;
        downcast_slice::<T, A>(entry.col.as_ref(), cid)
    }

    /// Contiguous mutable slice for component `cid`.
    pub fn column_mut<T: 'static>(&mut self, cid: ComponentId) -> Result<&mut [T], EcsError> {
        let idx = self.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
        let entry = self.columns.get_mut(idx).ok_or(EcsError::MissingComponent(cid))?;
        downcast_slice_mut::<T, A>(entry.col.as_mut(), cid)
    }

    /// Safe disjoint mutable borrows of two distinct component columns (`&mut [T1]`, `&mut [T2]`)
    /// using `split_at_mut_checked` with zero `unsafe`.
    pub fn columns_2_mut<T1: 'static, T2: 'static>(
        &mut self,
        c1: ComponentId,
        c2: ComponentId,
    ) -> Result<(&mut [T1], &mut [T2]), EcsError> {
        if c1 == c2 {
            return Err(EcsError::AliasedComponentAccess(c1));
        }
        let i1 = self.column_index(c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = self.column_index(c2).ok_or(EcsError::MissingComponent(c2))?;
        let slice = self.columns.as_mut_slice();
        if i1 < i2 {
            let (left, right) =
                slice.split_at_mut_checked(i2).ok_or(EcsError::MissingComponent(c2))?;
            let e1 = left.get_mut(i1).ok_or(EcsError::MissingComponent(c1))?;
            let e2 = right.get_mut(0).ok_or(EcsError::MissingComponent(c2))?;
            let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
            let s2 = downcast_slice_mut::<T2, A>(e2.col.as_mut(), c2)?;
            Ok((s1, s2))
        } else {
            let (left, right) =
                slice.split_at_mut_checked(i1).ok_or(EcsError::MissingComponent(c1))?;
            let e2 = left.get_mut(i2).ok_or(EcsError::MissingComponent(c2))?;
            let e1 = right.get_mut(0).ok_or(EcsError::MissingComponent(c1))?;
            let s1 = downcast_slice_mut::<T1, A>(e1.col.as_mut(), c1)?;
            let s2 = downcast_slice_mut::<T2, A>(e2.col.as_mut(), c2)?;
            Ok((s1, s2))
        }
    }

    /// Safe disjoint borrow of one mutable component column `&mut [W]` and one shared component
    /// column `&[R]` (e.g., `&mut [Pos]` + `&[Vel]`) with zero `unsafe`.
    pub fn columns_1_mut_1_ref<W: 'static, R: 'static>(
        &mut self,
        cw: ComponentId,
        cr: ComponentId,
    ) -> Result<(&mut [W], &[R]), EcsError> {
        if cw == cr {
            return Err(EcsError::AliasedComponentAccess(cw));
        }
        let iw = self.column_index(cw).ok_or(EcsError::MissingComponent(cw))?;
        let ir = self.column_index(cr).ok_or(EcsError::MissingComponent(cr))?;
        let slice = self.columns.as_mut_slice();
        if iw < ir {
            let (left, right) =
                slice.split_at_mut_checked(ir).ok_or(EcsError::MissingComponent(cr))?;
            let ew = left.get_mut(iw).ok_or(EcsError::MissingComponent(cw))?;
            let er = right.get(0).ok_or(EcsError::MissingComponent(cr))?;
            let sw = downcast_slice_mut::<W, A>(ew.col.as_mut(), cw)?;
            let sr = downcast_slice::<R, A>(er.col.as_ref(), cr)?;
            Ok((sw, sr))
        } else {
            let (left, right) =
                slice.split_at_mut_checked(iw).ok_or(EcsError::MissingComponent(cw))?;
            let er = left.get(ir).ok_or(EcsError::MissingComponent(cr))?;
            let ew = right.get_mut(0).ok_or(EcsError::MissingComponent(cw))?;
            let sw = downcast_slice_mut::<W, A>(ew.col.as_mut(), cw)?;
            let sr = downcast_slice::<R, A>(er.col.as_ref(), cr)?;
            Ok((sw, sr))
        }
    }

    /// Shared borrows of two component columns (`&[T1]`, `&[T2]`).
    pub fn columns_2_ref<T1: 'static, T2: 'static>(
        &self,
        c1: ComponentId,
        c2: ComponentId,
    ) -> Result<(&[T1], &[T2]), EcsError> {
        Ok((self.column::<T1>(c1)?, self.column::<T2>(c2)?))
    }

    /// Shared borrows of three component columns (`&[T1]`, `&[T2]`, `&[T3]`).
    pub fn columns_3_ref<T1: 'static, T2: 'static, T3: 'static>(
        &self,
        c1: ComponentId,
        c2: ComponentId,
        c3: ComponentId,
    ) -> Result<(&[T1], &[T2], &[T3]), EcsError> {
        Ok((self.column::<T1>(c1)?, self.column::<T2>(c2)?, self.column::<T3>(c3)?))
    }

    /// Safe disjoint borrow of one mutable column `&mut [W]` and two shared columns (`&[R1]`,
    /// `&[R2]`) with zero `unsafe`.
    pub fn columns_3_1mut_2ref<W: 'static, R1: 'static, R2: 'static>(
        &mut self,
        cw: ComponentId,
        cr1: ComponentId,
        cr2: ComponentId,
    ) -> Result<(&mut [W], &[R1], &[R2]), EcsError> {
        if cw == cr1 || cw == cr2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(cw) });
        }
        let iw = self.column_index(cw).ok_or(EcsError::MissingComponent(cw))?;
        let ir1 = self.column_index(cr1).ok_or(EcsError::MissingComponent(cr1))?;
        let ir2 = self.column_index(cr2).ok_or(EcsError::MissingComponent(cr2))?;
        let (before, ew, after) = split_1mut(self.columns.as_mut_slice(), iw, cw)?;
        let er1 = pick_ref_around_1(before, iw, after, ir1, cr1)?;
        let er2 = pick_ref_around_1(before, iw, after, ir2, cr2)?;
        Ok((
            downcast_slice_mut::<W, A>(ew.col.as_mut(), cw)?,
            downcast_slice::<R1, A>(er1.col.as_ref(), cr1)?,
            downcast_slice::<R2, A>(er2.col.as_ref(), cr2)?,
        ))
    }

    /// Safe disjoint borrow of two mutable columns (`&mut [W1]`, `&mut [W2]`) and one shared
    /// column `&[R]` with zero `unsafe`.
    pub fn columns_3_2mut_1ref<W1: 'static, W2: 'static, R: 'static>(
        &mut self,
        cw1: ComponentId,
        cw2: ComponentId,
        cr: ComponentId,
    ) -> Result<(&mut [W1], &mut [W2], &[R]), EcsError> {
        if cw1 == cw2 || cw1 == cr {
            return Err(EcsError::ConflictingAccess { id: Id::Component(cw1) });
        }
        if cw2 == cr {
            return Err(EcsError::ConflictingAccess { id: Id::Component(cw2) });
        }
        let iw1 = self.column_index(cw1).ok_or(EcsError::MissingComponent(cw1))?;
        let iw2 = self.column_index(cw2).ok_or(EcsError::MissingComponent(cw2))?;
        let ir = self.column_index(cr).ok_or(EcsError::MissingComponent(cr))?;
        let (seg0, ew1, seg1, ew2, seg2, lo, hi) =
            split_2mut(self.columns.as_mut_slice(), iw1, cw1, iw2, cw2)?;
        let er = pick_ref_around_2(seg0, lo, seg1, hi, seg2, ir, cr)?;
        Ok((
            downcast_slice_mut::<W1, A>(ew1.col.as_mut(), cw1)?,
            downcast_slice_mut::<W2, A>(ew2.col.as_mut(), cw2)?,
            downcast_slice::<R, A>(er.col.as_ref(), cr)?,
        ))
    }

    /// Safe disjoint mutable borrows of three distinct component columns with zero `unsafe`.
    pub fn columns_3_mut<W1: 'static, W2: 'static, W3: 'static>(
        &mut self,
        c1: ComponentId,
        c2: ComponentId,
        c3: ComponentId,
    ) -> Result<(&mut [W1], &mut [W2], &mut [W3]), EcsError> {
        if c1 == c2 || c1 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
        }
        if c2 == c3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(c2) });
        }
        let i1 = self.column_index(c1).ok_or(EcsError::MissingComponent(c1))?;
        let i2 = self.column_index(c2).ok_or(EcsError::MissingComponent(c2))?;
        let i3 = self.column_index(c3).ok_or(EcsError::MissingComponent(c3))?;
        let (e1, e2, e3) = split_3mut(self.columns.as_mut_slice(), i1, c1, i2, c2, i3, c3)?;
        Ok((
            downcast_slice_mut::<W1, A>(e1.col.as_mut(), c1)?,
            downcast_slice_mut::<W2, A>(e2.col.as_mut(), c2)?,
            downcast_slice_mut::<W3, A>(e3.col.as_mut(), c3)?,
        ))
    }

    /// Shared borrows of four component columns (`&[T1]`, `&[T2]`, `&[T3]`, `&[T4]`).
    pub fn columns_4_ref<T1: 'static, T2: 'static, T3: 'static, T4: 'static>(
        &self,
        c1: ComponentId,
        c2: ComponentId,
        c3: ComponentId,
        c4: ComponentId,
    ) -> Result<(&[T1], &[T2], &[T3], &[T4]), EcsError> {
        Ok((
            self.column::<T1>(c1)?,
            self.column::<T2>(c2)?,
            self.column::<T3>(c3)?,
            self.column::<T4>(c4)?,
        ))
    }

    /// Safe disjoint borrow of one mutable column `&mut [W]` and three shared columns
    /// (`&[R1]`, `&[R2]`, `&[R3]`) with zero `unsafe`.
    pub fn columns_4_1mut_3ref<W: 'static, R1: 'static, R2: 'static, R3: 'static>(
        &mut self,
        cw: ComponentId,
        cr1: ComponentId,
        cr2: ComponentId,
        cr3: ComponentId,
    ) -> Result<(&mut [W], &[R1], &[R2], &[R3]), EcsError> {
        if cw == cr1 || cw == cr2 || cw == cr3 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(cw) });
        }
        let iw = self.column_index(cw).ok_or(EcsError::MissingComponent(cw))?;
        let ir1 = self.column_index(cr1).ok_or(EcsError::MissingComponent(cr1))?;
        let ir2 = self.column_index(cr2).ok_or(EcsError::MissingComponent(cr2))?;
        let ir3 = self.column_index(cr3).ok_or(EcsError::MissingComponent(cr3))?;
        let (before, ew, after) = split_1mut(self.columns.as_mut_slice(), iw, cw)?;
        let er1 = pick_ref_around_1(before, iw, after, ir1, cr1)?;
        let er2 = pick_ref_around_1(before, iw, after, ir2, cr2)?;
        let er3 = pick_ref_around_1(before, iw, after, ir3, cr3)?;
        Ok((
            downcast_slice_mut::<W, A>(ew.col.as_mut(), cw)?,
            downcast_slice::<R1, A>(er1.col.as_ref(), cr1)?,
            downcast_slice::<R2, A>(er2.col.as_ref(), cr2)?,
            downcast_slice::<R3, A>(er3.col.as_ref(), cr3)?,
        ))
    }

    /// Safe disjoint borrow of two mutable columns (`&mut [W1]`, `&mut [W2]`) and two shared
    /// columns (`&[R1]`, `&[R2]`) with zero `unsafe`.
    pub fn columns_4_2mut_2ref<W1: 'static, W2: 'static, R1: 'static, R2: 'static>(
        &mut self,
        cw1: ComponentId,
        cw2: ComponentId,
        cr1: ComponentId,
        cr2: ComponentId,
    ) -> Result<(&mut [W1], &mut [W2], &[R1], &[R2]), EcsError> {
        if cw1 == cw2 || cw1 == cr1 || cw1 == cr2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(cw1) });
        }
        if cw2 == cr1 || cw2 == cr2 {
            return Err(EcsError::ConflictingAccess { id: Id::Component(cw2) });
        }
        let iw1 = self.column_index(cw1).ok_or(EcsError::MissingComponent(cw1))?;
        let iw2 = self.column_index(cw2).ok_or(EcsError::MissingComponent(cw2))?;
        let ir1 = self.column_index(cr1).ok_or(EcsError::MissingComponent(cr1))?;
        let ir2 = self.column_index(cr2).ok_or(EcsError::MissingComponent(cr2))?;
        let (seg0, ew1, seg1, ew2, seg2, lo, hi) =
            split_2mut(self.columns.as_mut_slice(), iw1, cw1, iw2, cw2)?;
        let er1 = pick_ref_around_2(seg0, lo, seg1, hi, seg2, ir1, cr1)?;
        let er2 = pick_ref_around_2(seg0, lo, seg1, hi, seg2, ir2, cr2)?;
        Ok((
            downcast_slice_mut::<W1, A>(ew1.col.as_mut(), cw1)?,
            downcast_slice_mut::<W2, A>(ew2.col.as_mut(), cw2)?,
            downcast_slice::<R1, A>(er1.col.as_ref(), cr1)?,
            downcast_slice::<R2, A>(er2.col.as_ref(), cr2)?,
        ))
    }

    /// Read a single entity's component value at `row`.
    pub(crate) fn get_row<T: 'static>(&self, cid: ComponentId, row: usize) -> Result<&T, EcsError> {
        let idx = self.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
        let entry = self.columns.get(idx).ok_or(EcsError::MissingComponent(cid))?;
        let typed = downcast_typed::<T, A>(entry.col.as_ref(), cid)?;
        typed.get(row).ok_or(EcsError::RowOutOfBounds { table: self.id, row, len: self.len() })
    }

    /// Mutably access a single entity's component value at `row`.
    pub(crate) fn get_row_mut<T: 'static>(
        &mut self,
        cid: ComponentId,
        row: usize,
    ) -> Result<&mut T, EcsError> {
        let tid = self.id;
        let len = self.len();
        let idx = self.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
        let entry = self.columns.get_mut(idx).ok_or(EcsError::MissingComponent(cid))?;
        let typed = downcast_typed_mut::<T, A>(entry.col.as_mut(), cid)?;
        typed.get_mut(row).ok_or(EcsError::RowOutOfBounds { table: tid, row, len })
    }

    /// Overwrite an existing component value at `row` in-place.
    pub(crate) fn set_row<T: 'static>(
        &mut self,
        cid: ComponentId,
        row: usize,
        value: T,
    ) -> Result<(), EcsError> {
        let tid = self.id;
        let idx = self.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
        let entry = self.columns.get_mut(idx).ok_or(EcsError::MissingComponent(cid))?;
        let typed = downcast_typed_mut::<T, A>(entry.col.as_mut(), cid)?;
        typed.set(tid, row, value)
    }

    /// Push a new value into component column `cid`.
    pub(crate) fn push_component<T: 'static>(
        &mut self,
        cid: ComponentId,
        value: T,
    ) -> Result<(), EcsError> {
        let idx = self.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
        let entry = self.columns.get_mut(idx).ok_or(EcsError::MissingComponent(cid))?;
        let typed = downcast_typed_mut::<T, A>(entry.col.as_mut(), cid)?;
        typed.push(value)
    }

    /// Reflect the component value at `(cid, row)` into a [`Value`], returning `Ok(None)` if opaque.
    pub(crate) fn row_to_value(
        &self,
        cid: ComponentId,
        row: usize,
        alloc: A,
    ) -> Result<Option<Value<A>>, ReflectError> {
        let idx = self.column_index(cid).ok_or(ReflectError::MissingComponent(cid))?;
        let entry = self.columns.get(idx).ok_or(ReflectError::MissingComponent(cid))?;
        entry.col.row_to_value(self.id, row, cid, alloc)
    }

    /// Patch the component value at `(cid, row)` in-place from `patch`.
    pub(crate) fn patch_row_from_value(
        &mut self,
        cid: ComponentId,
        row: usize,
        patch: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let tid = self.id;
        let idx = self.column_index(cid).ok_or(ReflectError::MissingComponent(cid))?;
        let entry = self.columns.get_mut(idx).ok_or(ReflectError::MissingComponent(cid))?;
        entry.col.patch_row_from_value(tid, row, cid, patch, alloc)
    }

    /// Decode a reflected value from `val` and push it into component column `cid`.
    pub(crate) fn push_component_from_value(
        &mut self,
        cid: ComponentId,
        val: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let idx = self.column_index(cid).ok_or(ReflectError::MissingComponent(cid))?;
        let entry = self.columns.get_mut(idx).ok_or(ReflectError::MissingComponent(cid))?;
        entry.col.push_from_value(cid, val, alloc)
    }

    /// Swap-remove `row` from `self.entities` and drop every column value at `row`.
    /// Returns `Some(moved_entity)` if a trailing entity was swapped into `row`.
    pub(crate) fn swap_remove_drop_row(&mut self, row: usize) -> Result<Option<Entity>, EcsError> {
        let len = self.entities.len();
        if row >= len {
            return Err(EcsError::RowOutOfBounds { table: self.id, row, len });
        }
        let tid = self.id;
        for entry in self.columns.as_mut_slice() {
            entry.col.swap_remove_drop(tid, row)?;
        }
        let _ = self.entities.remove_swap(row);
        if row < self.entities.len() {
            Ok(self.entities.get(row).copied())
        } else {
            Ok(None)
        }
    }
}

/// Move all shared columns of `row` from `src` to `dst`, dropping any columns unique to `src`,
/// and swap-removing the entity from `src` into `dst`.
///
/// Returns `(dst_row, moved_in_src)` where `moved_in_src` is `Some(e)` if another entity was
/// swapped into `src`'s vacated `row`.
pub(crate) fn move_row_between_tables<A: Allocator + Copy + 'static>(
    src: &mut Table<A>,
    src_row: usize,
    dst: &mut Table<A>,
) -> Result<(usize, Option<Entity>), EcsError> {
    let src_len = src.entities.len();
    let entity = *src.entities.get(src_row).ok_or(EcsError::RowOutOfBounds {
        table: src.id,
        row: src_row,
        len: src_len,
    })?;
    let src_tid = src.id;
    let src_cols = src.columns.as_mut_slice();
    let dst_cols = dst.columns.as_mut_slice();
    let mut si = 0usize;
    let mut di = 0usize;
    while let Some(sc) = src_cols.get_mut(si) {
        while let Some(dc) = dst_cols.get(di) {
            if dc.id < sc.id {
                di += 1;
            } else {
                break;
            }
        }
        if let Some(dc) = dst_cols.get_mut(di) {
            if dc.id == sc.id {
                sc.col.move_row_to(src_tid, src_row, sc.id, dc.col.as_mut())?;
                si += 1;
                di += 1;
                continue;
            }
        }
        // Component exists in `src` but not in `dst`: drop its value at `src_row`.
        sc.col.swap_remove_drop(src_tid, src_row)?;
        si += 1;
    }
    let _ = src.entities.remove_swap(src_row);
    let moved_in_src = if src_row < src.entities.len() {
        src.entities.get(src_row).copied()
    } else {
        None
    };
    let dst_row = dst.entities.len();
    dst.entities.try_push(entity).map_err(|r| EcsError::Alloc(r.why))?;
    Ok((dst_row, moved_in_src))
}

/// Binary search for `cid` inside a sorted [`TableColumn`] slice.
#[inline]
pub(crate) fn column_index_in<A: Allocator + Copy>(
    cols: &[TableColumn<A>],
    cid: ComponentId,
) -> Option<usize> {
    cols.binary_search_by_key(&cid, |entry| entry.id).ok()
}

/// Split `cols` around a single mutable column at `iw` into `(before, &mut col_w, after)`.
pub(crate) fn split_1mut<'t, A: Allocator + Copy + 'static>(
    cols: &'t mut [TableColumn<A>],
    iw: usize,
    cw: ComponentId,
) -> Result<(&'t [TableColumn<A>], &'t mut TableColumn<A>, &'t [TableColumn<A>]), EcsError> {
    let (before, from_w) = cols.split_at_mut_checked(iw).ok_or(EcsError::MissingComponent(cw))?;
    let (w_slice, after) = from_w.split_at_mut_checked(1).ok_or(EcsError::MissingComponent(cw))?;
    let w_col = w_slice.get_mut(0).ok_or(EcsError::MissingComponent(cw))?;
    Ok((before, w_col, after))
}

/// Pick an immutable column at `ir` from the `before`/`after` halves produced by [`split_1mut`].
pub(crate) fn pick_ref_around_1<'t, A: Allocator + Copy + 'static>(
    before: &'t [TableColumn<A>],
    iw: usize,
    after: &'t [TableColumn<A>],
    ir: usize,
    cr: ComponentId,
) -> Result<&'t TableColumn<A>, EcsError> {
    if ir < iw {
        before.get(ir).ok_or(EcsError::MissingComponent(cr))
    } else if ir > iw {
        let rel = ir.checked_sub(iw.saturating_add(1)).ok_or(EcsError::MissingComponent(cr))?;
        after.get(rel).ok_or(EcsError::MissingComponent(cr))
    } else {
        Err(EcsError::ConflictingAccess { id: Id::Component(cr) })
    }
}

/// Split `cols` around two distinct mutable columns at `i1` and `i2`, returning the three
/// surrounding immutable segments, the two mutable columns in `(c1, c2)` order, and `(lo, hi)`.
pub(crate) fn split_2mut<'t, A: Allocator + Copy + 'static>(
    cols: &'t mut [TableColumn<A>],
    i1: usize,
    c1: ComponentId,
    i2: usize,
    c2: ComponentId,
) -> Result<
    (
        &'t [TableColumn<A>],
        &'t mut TableColumn<A>,
        &'t [TableColumn<A>],
        &'t mut TableColumn<A>,
        &'t [TableColumn<A>],
        usize,
        usize,
    ),
    EcsError,
> {
    if i1 == i2 {
        return Err(EcsError::ConflictingAccess { id: Id::Component(c1) });
    }
    if i1 < i2 {
        let (seg0, from1) = cols.split_at_mut_checked(i1).ok_or(EcsError::MissingComponent(c1))?;
        let (col1_s, after1) =
            from1.split_at_mut_checked(1).ok_or(EcsError::MissingComponent(c1))?;
        let rel2 = i2
            .checked_sub(i1.saturating_add(1))
            .ok_or(EcsError::ConflictingAccess { id: Id::Component(c2) })?;
        let (seg1, from2) =
            after1.split_at_mut_checked(rel2).ok_or(EcsError::MissingComponent(c2))?;
        let (col2_s, seg2) =
            from2.split_at_mut_checked(1).ok_or(EcsError::MissingComponent(c2))?;
        let col1 = col1_s.get_mut(0).ok_or(EcsError::MissingComponent(c1))?;
        let col2 = col2_s.get_mut(0).ok_or(EcsError::MissingComponent(c2))?;
        Ok((seg0, col1, seg1, col2, seg2, i1, i2))
    } else {
        let (seg0, from2) = cols.split_at_mut_checked(i2).ok_or(EcsError::MissingComponent(c2))?;
        let (col2_s, after2) =
            from2.split_at_mut_checked(1).ok_or(EcsError::MissingComponent(c2))?;
        let rel1 = i1
            .checked_sub(i2.saturating_add(1))
            .ok_or(EcsError::ConflictingAccess { id: Id::Component(c1) })?;
        let (seg1, from1) =
            after2.split_at_mut_checked(rel1).ok_or(EcsError::MissingComponent(c1))?;
        let (col1_s, seg2) =
            from1.split_at_mut_checked(1).ok_or(EcsError::MissingComponent(c1))?;
        let col1 = col1_s.get_mut(0).ok_or(EcsError::MissingComponent(c1))?;
        let col2 = col2_s.get_mut(0).ok_or(EcsError::MissingComponent(c2))?;
        Ok((seg0, col1, seg1, col2, seg2, i2, i1))
    }
}

/// Pick an immutable column at `ir` from the three segments produced by [`split_2mut`].
pub(crate) fn pick_ref_around_2<'t, A: Allocator + Copy + 'static>(
    seg0: &'t [TableColumn<A>],
    lo: usize,
    seg1: &'t [TableColumn<A>],
    hi: usize,
    seg2: &'t [TableColumn<A>],
    ir: usize,
    cr: ComponentId,
) -> Result<&'t TableColumn<A>, EcsError> {
    if ir < lo {
        seg0.get(ir).ok_or(EcsError::MissingComponent(cr))
    } else if ir == lo || ir == hi {
        Err(EcsError::ConflictingAccess { id: Id::Component(cr) })
    } else if ir < hi {
        let rel = ir.checked_sub(lo.saturating_add(1)).ok_or(EcsError::MissingComponent(cr))?;
        seg1.get(rel).ok_or(EcsError::MissingComponent(cr))
    } else {
        let rel = ir.checked_sub(hi.saturating_add(1)).ok_or(EcsError::MissingComponent(cr))?;
        seg2.get(rel).ok_or(EcsError::MissingComponent(cr))
    }
}

/// Split `cols` into three distinct mutable columns in `(c0, c1, c2)` order.
pub(crate) fn split_3mut<'t, A: Allocator + Copy + 'static>(
    cols: &'t mut [TableColumn<A>],
    i0: usize,
    c0: ComponentId,
    i1: usize,
    c1: ComponentId,
    i2: usize,
    c2: ComponentId,
) -> Result<(&'t mut TableColumn<A>, &'t mut TableColumn<A>, &'t mut TableColumn<A>), EcsError> {
    let mut order = [(i0, 0usize, c0), (i1, 1usize, c1), (i2, 2usize, c2)];
    order.sort_unstable_by_key(|&(idx, _, _)| idx);
    let mut out0: Option<&'t mut TableColumn<A>> = None;
    let mut out1: Option<&'t mut TableColumn<A>> = None;
    let mut out2: Option<&'t mut TableColumn<A>> = None;
    let mut rest = cols;
    let mut base = 0usize;
    for (idx, slot, cid) in order {
        let rel = idx
            .checked_sub(base)
            .ok_or(EcsError::ConflictingAccess { id: Id::Component(cid) })?;
        let (_, tail) = rest.split_at_mut_checked(rel).ok_or(EcsError::MissingComponent(cid))?;
        let (head, after) = tail.split_at_mut_checked(1).ok_or(EcsError::MissingComponent(cid))?;
        let col = head.get_mut(0).ok_or(EcsError::MissingComponent(cid))?;
        match slot {
            0 => out0 = Some(col),
            1 => out1 = Some(col),
            _ => out2 = Some(col),
        }
        rest = after;
        base = idx.saturating_add(1);
    }
    Ok((
        out0.ok_or(EcsError::MissingComponent(c0))?,
        out1.ok_or(EcsError::MissingComponent(c1))?,
        out2.ok_or(EcsError::MissingComponent(c2))?,
    ))
}
