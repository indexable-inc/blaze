//! 100% safe contiguous Structure-of-Arrays (SoA) component columns (`#![forbid(unsafe_code)]`).
//!
//! Each column stores values of a single registered component type `T` densely in a
//! `blaze_own::Vec<T, A>` aligned 1:1 with its owning table's entity slice.

use core::alloc::Allocator;
use core::any::{Any, TypeId};

use blaze_own::{AllocFailure, Box as OwnBox, String as OwnString, Vec as OwnVec};

use crate::data::Value;

use super::error::EcsError;
use super::id::{ComponentId, TableId};
use super::reflect::{ReflectComponent, ReflectError};

/// Optional safe reflection vtable stored per reflected [`TypedColumn`].
pub(crate) struct ReflectVtable<T: 'static, A: Allocator + Copy> {
    pub(crate) to_value: fn(&T, A) -> Result<Value<A>, ReflectError>,
    pub(crate) patch_value: fn(&mut T, &Value<A>, A) -> Result<(), ReflectError>,
    pub(crate) from_value: fn(&Value<A>, A) -> Result<T, ReflectError>,
}

impl<T: 'static, A: Allocator + Copy> Clone for ReflectVtable<T, A> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static, A: Allocator + Copy> Copy for ReflectVtable<T, A> {}

/// Construct a [`ReflectVtable`] for any [`ReflectComponent`] type `T`.
pub(crate) fn reflect_vtable<T: ReflectComponent, A: Allocator + Copy>() -> ReflectVtable<T, A> {
    fn to_val<T: ReflectComponent, A: Allocator + Copy>(
        v: &T,
        alloc: A,
    ) -> Result<Value<A>, ReflectError> {
        v.reflect_to_value(alloc)
    }
    fn patch_val<T: ReflectComponent, A: Allocator + Copy>(
        v: &mut T,
        patch: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        v.patch_value(patch, alloc)
    }
    fn from_val<T: ReflectComponent, A: Allocator + Copy>(
        val: &Value<A>,
        alloc: A,
    ) -> Result<T, ReflectError> {
        T::from_patch_value(val, alloc)
    }
    ReflectVtable {
        to_value: to_val::<T, A>,
        patch_value: patch_val::<T, A>,
        from_value: from_val::<T, A>,
    }
}

/// Validate that `val` decodes cleanly as `T` before mutating any world table.
pub(crate) fn validate_reflected_value<T: ReflectComponent, A: Allocator + Copy>(
    val: &Value<A>,
    alloc: A,
) -> Result<(), ReflectError> {
    let _ = T::from_patch_value(val, alloc)?;
    Ok(())
}

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

    /// Whether this column has live [`ReflectComponent`] reflection hooks enabled.
    fn is_reflected(&self) -> bool;

    /// Reflect the value at `row` into a [`Value`], returning `Ok(None)` if this column is opaque.
    fn row_to_value(
        &self,
        table: TableId,
        row: usize,
        cid: ComponentId,
        alloc: A,
    ) -> Result<Option<Value<A>>, ReflectError>;

    /// Patch the value at `row` in-place from `patch` using this column's reflection vtable.
    fn patch_row_from_value(
        &mut self,
        table: TableId,
        row: usize,
        cid: ComponentId,
        patch: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError>;

    /// Decode a new component value from `val` and push it onto the back of this column.
    fn push_from_value(
        &mut self,
        cid: ComponentId,
        val: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError>;
}

/// Concrete contiguous SoA column for component type `T`.
pub struct TypedColumn<T: 'static, A: Allocator + Copy> {
    data: OwnVec<T, A>,
    duplicator: Option<fn(&T) -> T>,
    reflect: Option<ReflectVtable<T, A>>,
}

impl<T: 'static, A: Allocator + Copy> TypedColumn<T, A> {
    /// Create an empty column in `alloc` with an optional value duplicator for prefab instantiation.
    #[inline]
    pub fn new_in(alloc: A, duplicator: Option<fn(&T) -> T>) -> Self {
        Self { data: OwnVec::new_in(alloc), duplicator, reflect: None }
    }

    /// Create an empty column in `alloc` with optional duplicator and reflection vtable.
    #[inline]
    pub(crate) fn new_with_reflect_in(
        alloc: A,
        duplicator: Option<fn(&T) -> T>,
        reflect: Option<ReflectVtable<T, A>>,
    ) -> Self {
        Self { data: OwnVec::new_in(alloc), duplicator, reflect }
    }

    /// Enable value duplication for a `Copy` component column (used when upgrading a lazily
    /// registered component to copyable for prefab instantiation).
    #[inline]
    pub(crate) fn enable_copy(&mut self)
    where
        T: Copy,
    {
        fn copy_val<T: Copy>(v: &T) -> T {
            *v
        }
        self.duplicator = Some(copy_val::<T>);
    }

    /// Enable live `bstd::data` reflection on an existing component column.
    #[inline]
    pub(crate) fn enable_reflect(&mut self)
    where
        T: ReflectComponent,
    {
        self.reflect = Some(reflect_vtable::<T, A>());
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
        OwnBox::new_in(
            TypedColumn::<T, A>::new_with_reflect_in(alloc, self.duplicator, self.reflect),
            alloc,
        )
    }

    #[inline]
    fn is_reflected(&self) -> bool {
        self.reflect.is_some()
    }

    fn row_to_value(
        &self,
        table: TableId,
        row: usize,
        _cid: ComponentId,
        alloc: A,
    ) -> Result<Option<Value<A>>, ReflectError> {
        let Some(vtable) = self.reflect else {
            return Ok(None);
        };
        let len = self.data.len();
        let val = self
            .data
            .get(row)
            .ok_or(ReflectError::Ecs(EcsError::RowOutOfBounds { table, row, len }))?;
        (vtable.to_value)(val, alloc).map(Some)
    }

    fn patch_row_from_value(
        &mut self,
        table: TableId,
        row: usize,
        cid: ComponentId,
        patch: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let Some(vtable) = self.reflect else {
            return Err(ReflectError::NotReflected(cid));
        };
        let len = self.data.len();
        let slot = self
            .data
            .get_mut(row)
            .ok_or(ReflectError::Ecs(EcsError::RowOutOfBounds { table, row, len }))?;
        (vtable.patch_value)(slot, patch, alloc)
    }

    fn push_from_value(
        &mut self,
        cid: ComponentId,
        val: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let Some(vtable) = self.reflect else {
            return Err(ReflectError::NotReflected(cid));
        };
        let item = (vtable.from_value)(val, alloc)?;
        self.push(item).map_err(ReflectError::from)
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
    /// Size in bytes of the registered component type (`core::mem::size_of::<T>()`).
    pub size: usize,
    /// Optional Blaze record schema ID hook (e.g. `blaze_record_schema_id::<T>()`).
    pub schema_id: Option<u64>,
    pub(crate) make_column: fn(A) -> OwnBox<dyn AnyColumn<A>, A>,
    pub(crate) enable_copy_col: Option<fn(&mut dyn AnyColumn<A>)>,
    pub(crate) enable_reflect_col: Option<fn(&mut dyn AnyColumn<A>)>,
    pub can_copy: bool,
    /// Whether live `bstd::data` reflection is enabled for this component.
    pub reflected: bool,
    pub(crate) schema_fn: Option<fn(A) -> Result<Value<A>, AllocFailure>>,
    pub(crate) validate_value_fn: Option<fn(&Value<A>, A) -> Result<(), ReflectError>>,
}

impl<A: Allocator + Copy> ComponentMeta<A> {
    /// Human-readable component name.
    #[inline]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Return this component's reflected schema [`Value`], or `Ok(None)` if not reflected.
    #[inline]
    pub fn schema(&self, alloc: A) -> Result<Option<Value<A>>, AllocFailure> {
        match self.schema_fn {
            Some(f) => f(alloc).map(Some),
            None => Ok(None),
        }
    }

    /// Construct a fresh empty column configured with this component's current copy & reflection hooks.
    pub(crate) fn create_column(&self, alloc: A) -> OwnBox<dyn AnyColumn<A>, A> {
        let mut col = (self.make_column)(alloc);
        if let Some(enable) = self.enable_copy_col {
            enable(col.as_mut());
        }
        if let Some(enable) = self.enable_reflect_col {
            enable(col.as_mut());
        }
        col
    }

    /// Validate that `val` decodes cleanly as this component before mutating any table.
    #[inline]
    pub(crate) fn validate_value(&self, val: &Value<A>, alloc: A) -> Result<(), ReflectError> {
        match self.validate_value_fn {
            Some(f) => f(val, alloc),
            None => Err(ReflectError::NotReflected(self.id)),
        }
    }
}

/// Helper column factory for any component type `T`.
pub(crate) fn make_column_plain<T: 'static, A: Allocator + Copy + 'static>(
    alloc: A,
) -> OwnBox<dyn AnyColumn<A>, A> {
    OwnBox::new_in(TypedColumn::<T, A>::new_in(alloc, None), alloc)
}

/// Type-erased hook that enables `Copy` duplication on a `TypedColumn<T, A>`.
pub(crate) fn enable_copy_any<T: Copy + 'static, A: Allocator + Copy + 'static>(
    col: &mut dyn AnyColumn<A>,
) {
    if let Some(typed) = col.as_any_mut().downcast_mut::<TypedColumn<T, A>>() {
        typed.enable_copy();
    }
}

/// Type-erased hook that enables live reflection on a `TypedColumn<T, A>`.
pub(crate) fn enable_reflect_any<T: ReflectComponent + 'static, A: Allocator + Copy + 'static>(
    col: &mut dyn AnyColumn<A>,
) {
    if let Some(typed) = col.as_any_mut().downcast_mut::<TypedColumn<T, A>>() {
        typed.enable_reflect();
    }
}
