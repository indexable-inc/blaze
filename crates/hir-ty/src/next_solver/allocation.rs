use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    num::NonZeroU32,
};

use macros::GenericTypeVisitable;
use rustc_type_ir::GenericTypeVisitable;

use crate::{
    MemoryMap,
    next_solver::{Ty, impl_stored_interned, interner::table},
};

/// Every interned [`Allocation`], kept whole: it derefs to its data.
static ALLOCATIONS: table::Values<AllocationData<'static>> = table::Values::new("allocations");
table::erase!(AllocationData);

/// An allocation: its row in [`ALLOCATIONS`], which is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Allocation<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<Allocation<'static>>>() == 4);

impl<'db> Allocation<'db> {
    pub fn new(data: AllocationData<'db>) -> Self {
        Self::at(ALLOCATIONS.intern(data))
    }

    /// The allocation of `memory` with no memory map (a scalar constant), found without copying
    /// `memory` when it is interned already, so lowering a literal it has met allocates nothing.
    pub fn of_bytes(ty: Ty<'db>, memory: &[u8]) -> Self {
        /// [`AllocationData`] with no memory map, borrowed: hashes as `AllocationData::hash`
        /// does, which `find_by` needs to land on the same row.
        struct Key<'a, 'db> {
            ty: Ty<'db>,
            memory: &'a [u8],
        }
        impl Hash for Key<'_, '_> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.ty.hash(state);
                self.memory.hash(state);
                MemoryMap::Empty.hash(state);
            }
        }
        let same = |data: &AllocationData<'db>| {
            data.ty == ty && *data.memory == *memory && matches!(data.memory_map, MemoryMap::Empty)
        };
        match ALLOCATIONS.find_by(&Key { ty, memory }, same) {
            Some(row) => Self::at(row),
            None => Self::new(AllocationData {
                ty,
                memory: memory.into(), // alloc: a new constant's bytes, which the table keeps
                memory_map: MemoryMap::Empty,
            }),
        }
    }

    #[inline]
    fn at(index: NonZeroU32) -> Self {
        Allocation { index, _db: PhantomData }
    }
}

impl<'db> std::ops::Deref for Allocation<'db> {
    type Target = AllocationData<'db>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        ALLOCATIONS.get(self.index)
    }
}

// SAFETY: the handle holds one value, its row's data, which this visits whenever the visitor
// asks to go into the row.
unsafe impl<'db, V: super::WorldExposer> GenericTypeVisitable<V> for Allocation<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(ALLOCATIONS.name(), self.index.get()).is_continue() {
            (**self).generic_visit_with(visitor);
        }
    }
}

impl fmt::Debug for Allocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let AllocationData { ty, memory, memory_map } = &**self;
        f.debug_struct("Allocation")
            .field("ty", ty)
            .field("memory", memory)
            .field("memory_map", memory_map)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq, GenericTypeVisitable)]
pub struct AllocationData<'db> {
    pub ty: Ty<'db>,
    pub memory: Box<[u8]>,
    pub memory_map: MemoryMap<'db>,
}

// `Allocation::of_bytes` hashes a borrowed key the same way: change both together.
impl<'db> Hash for AllocationData<'db> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let Self { ty, memory, memory_map } = self;
        ty.hash(state);
        memory.hash(state);
        memory_map.hash(state);
    }
}

impl_stored_interned!(Allocation, StoredAllocation);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::next_solver::TyKind;

    #[test]
    fn allocations_that_differ_in_their_memory_map_are_two() {
        let ty = Ty::new_without_interner(TyKind::Str);
        let data = |map: MemoryMap<'static>| AllocationData {
            ty,
            memory: Box::new([1, 2, 3]),
            memory_map: map,
        };
        let empty = Allocation::new(data(MemoryMap::Empty));
        assert_eq!(Allocation::new(data(MemoryMap::Empty)), empty);
        let simple = Allocation::new(data(MemoryMap::Simple(Box::new([4]))));
        assert_ne!(simple, empty);
        assert_eq!(simple.store().as_ref(), simple);
        assert_eq!(*simple.memory, [1, 2, 3]);
    }
}
