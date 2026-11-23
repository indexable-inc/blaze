//! 100% safe contiguous Structure-of-Arrays (SoA) component columns (`#![forbid(unsafe_code)]`).
//!
//! Each column stores values of a single registered component type `T` densely in a
//! `blaze_own::Vec<T, A>` aligned 1:1 with its owning table's entity slice.

use core::alloc::Allocator;
use core::any::{Any, TypeId};

use blaze_own::{Box as OwnBox, String as OwnString, Vec as OwnVec};

use super::error::EcsError;
use super::id::{ComponentId, TableId};

/// Type-erased contiguous SoA column stored inside an archetype [`super::table::Table`].
pub trait AnyColumn<A: Allocator + Copy> {
    /// Number of stored component values (matches the owning table's entity count).
    fn len(&self) -> usize;

    /// Whether the column contains zero values.
    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `TypeId` of the stored component type `T`.
    fn type_id(&self) -> TypeId;

    /// Downcast handle for shared typed slice access.
    fn as_any(&self) -> &dyn Any;

    /// Downcast handle for exclusive typed slice access.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Remove and drop the value at `row` using O(1) swap-remove.
    fn swap_remove_drop(&mut self, table: TableId, row: usize) -> Result<(), EcsError>;

    /// Move the value at `row` out of `self` (via O(1) swap-remove) and push it into `dst`,
    /// preserving the exact value without cloning.
    fn move_row_to(
        &mut self,
        table: TableId,
        row: usize,
        cid: ComponentId,
        dst: &mut dyn AnyColumn<A>,
    ) -> Result<(), EcsError>;

    /// Duplicate the value at `row` into `dst` if this column's component registered a duplicator
    /// (`Copy` or explicit copy function). Returns `Ok(true)` if copied, `Ok(false)` otherwise.
    fn copy_row_to(
        &self,
        table: TableId,
        row: usize,
        cid: ComponentId,
        dst: &mut dyn AnyColumn<A>,
    ) -> Result<bool, EcsError>;

    /// Construct a fresh empty column of the same component type in `alloc`.
    fn make_empty(&self, alloc: A) -> OwnBox<dyn AnyColumn<A>, A>;
}

/// Concrete contiguous SoA column for component type `T`.
pub struct TypedColumn<T: 'static, A: Allocator + Copy> {
    data: OwnVec<T, A>,
    duplicator: Option<fn(&T) -> T>,
}

impl<T: 'static, A: Allocator + Copy> TypedColumn<T, A> {
    /// Create an empty column in `alloc` with an optional value duplicator for prefab instantiation.
    #[inline]
    pub fn new_in(alloc: A, duplicator: Option<fn(&T) -> T>) -> Self {
        Self { data: OwnVec::new_in(alloc), duplicator }
    }

    /// Push a new value onto the back of the column.
    #[inline]
    pub fn push(&mut self, value: T) -> Result<(), EcsError> {
        self.data.try_push(value).map_err(|refused| EcsError::Alloc(refused.why))
    }

    /// Replace an existing value at `row`.
    #[inline]
    pub fn set(&mut self, table: TableId, row: usize, value: T) -> Result<(), EcsError> {
        let len = self.data.len();
        let slot = self.data.get_mut(row).ok_or(EcsError::RowOutOfBounds { table, row, len })?;
        *slot = value;
        Ok(())
    }

    /// Shared reference to the value at `row`.
    #[inline]
    pub fn get(&self, row: usize) -> Option<&T> {
        self.data.get(row)
    }

    /// Mutable reference to the value at `row`.
    #[inline]
    pub fn get_mut(&mut self, row: usize) -> Option<&mut T> {
        self.data.get_mut(row)
    }

    /// Contiguous read-only slice over all values in this column.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        self.data.as_slice()
    }

    /// Contiguous mutable slice over all values in this column.
    #[inline]
    pub fn as_slice_mut(&mut self) -> &mut [T] {
        self.data.as_mut_slice()
    }

    /// Remove and return the value at `row` using O(1) swap-remove.
    pub fn swap_remove(&mut self, table: TableId, row: usize) -> Result<T, EcsError> {
        let len = self.data.len();
        if row >= len {
            return Err(EcsError::RowOutOfBounds { table, row, len });
        }
        Ok(self.data.remove_swap(row))
    }
}

impl<T: 'static, A: Allocator + Copy + 'static> AnyColumn<A> for TypedColumn<T, A> {
    #[inline]
    fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    fn type_id(&self) -> TypeId {
        TypeId::of::<T>()
    }

    #[inline]
    fn as_any(&self) -> &dyn Any {
        self
    }

    #[inline]
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn swap_remove_drop(&mut self, table: TableId, row: usize) -> Result<(), EcsError> {
        let _ = self.swap_remove(table, row)?;
        Ok(())
    }

    fn move_row_to(
        &mut self,
        table: TableId,
        row: usize,
        cid: ComponentId,
        dst: &mut dyn AnyColumn<A>,
    ) -> Result<(), EcsError> {
        let dst_typed = dst
            .as_any_mut()
            .downcast_mut::<TypedColumn<T, A>>()
            .ok_or(EcsError::ComponentTypeMismatch { component: cid })?;
        let value = self.swap_remove(table, row)?;
        dst_typed.push(value)
    }

    fn copy_row_to(
        &self,
        table: TableId,
        row: usize,
        cid: ComponentId,
        dst: &mut dyn AnyColumn<A>,
    ) -> Result<bool, EcsError> {
        let Some(dup) = self.duplicator else {
            return Ok(false);
        };
        let len = self.data.len();
        let src_val = self.data.get(row).ok_or(EcsError::RowOutOfBounds { table, row, len })?;
        let dst_typed = dst
            .as_any_mut()
            .downcast_mut::<TypedColumn<T, A>>()
            .ok_or(EcsError::ComponentTypeMismatch { component: cid })?;
        dst_typed.push(dup(src_val))?;
        Ok(true)
    }

    fn make_empty(&self, alloc: A) -> OwnBox<dyn AnyColumn<A>, A> {
        OwnBox::new_in(TypedColumn::<T, A>::new_in(alloc, self.duplicator), alloc)
    }
}

/// Downcast an [`AnyColumn`] to a shared slice `&[T]`.
#[inline]
pub(crate) fn downcast_slice<T: 'static, A: Allocator + Copy + 'static>(
    col: &dyn AnyColumn<A>,
    cid: ComponentId,
) -> Result<&[T], EcsError> {
    let typed = col
        .as_any()
        .downcast_ref::<TypedColumn<T, A>>()
        .ok_or(EcsError::ComponentTypeMismatch { component: cid })?;
    Ok(typed.as_slice())
}

/// Downcast an [`AnyColumn`] to a mutable slice `&mut [T]`.
#[inline]
pub(crate) fn downcast_slice_mut<T: 'static, A: Allocator + Copy + 'static>(
    col: &mut dyn AnyColumn<A>,
    cid: ComponentId,
) -> Result<&mut [T], EcsError> {
    let typed = col
        .as_any_mut()
        .downcast_mut::<TypedColumn<T, A>>()
        .ok_or(EcsError::ComponentTypeMismatch { component: cid })?;
    Ok(typed.as_slice_mut())
}

/// Downcast an [`AnyColumn`] to a mutable `&mut TypedColumn<T, A>`.
#[inline]
pub(crate) fn downcast_typed_mut<T: 'static, A: Allocator + Copy + 'static>(
    col: &mut dyn AnyColumn<A>,
    cid: ComponentId,
) -> Result<&mut TypedColumn<T, A>, EcsError> {
    col.as_any_mut()
        .downcast_mut::<TypedColumn<T, A>>()
        .ok_or(EcsError::ComponentTypeMismatch { component: cid })
}

/// Downcast an [`AnyColumn`] to a shared `&TypedColumn<T, A>`.
#[inline]
pub(crate) fn downcast_typed<T: 'static, A: Allocator + Copy + 'static>(
    col: &dyn AnyColumn<A>,
    cid: ComponentId,
) -> Result<&TypedColumn<T, A>, EcsError> {
    col.as_any()
        .downcast_ref::<TypedColumn<T, A>>()
        .ok_or(EcsError::ComponentTypeMismatch { component: cid })
}

/// Metadata stored per registered component type inside a `World`.
pub struct ComponentMeta<A: Allocator + Copy> {
    pub id: ComponentId,
    pub name: OwnString<A>,
    pub type_id: TypeId,
    /// Optional Blaze record schema ID hook (e.g. `blaze_record_schema_id::<T>()`).
    pub schema_id: Option<u64>,
    pub(crate) make_column: fn(A) -> OwnBox<dyn AnyColumn<A>, A>,
    pub can_copy: bool,
}

impl<A: Allocator + Copy> ComponentMeta<A> {
    /// Human-readable component name.
    #[inline]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }
}

/// Helper column factory for non-copyable components.
pub(crate) fn make_column_plain<T: 'static, A: Allocator + Copy + 'static>(
    alloc: A,
) -> OwnBox<dyn AnyColumn<A>, A> {
    OwnBox::new_in(TypedColumn::<T, A>::new_in(alloc, None), alloc)
}

/// Helper column factory for `Copy` components (enables prefab instantiation).
pub(crate) fn make_column_copy<T: Copy + 'static, A: Allocator + Copy + 'static>(
    alloc: A,
) -> OwnBox<dyn AnyColumn<A>, A> {
    fn copy_val<T: Copy>(v: &T) -> T {
        *v
    }
    OwnBox::new_in(TypedColumn::<T, A>::new_in(alloc, Some(copy_val::<T>)), alloc)
}
