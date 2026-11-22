//! [`Local`] and [`LocalTable`]: an item as a small number inside one task (docs/v2/plan/design/
//! bottom.md, point 3).

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};

use crate::error::SurfaceError;

/// What a [`Local`] of any item is: the marker of a task's table of every item it has asked
/// about. A later layer may number one kind of item apart (a body, an instance) with its own
/// marker.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AnyItem {}

/// An item inside one task: its place in the task's [`LocalTable`], the order the task first
/// asked about it in. It compares in one instruction, as an interned id does, and it is not
/// `blaze_id::Portable`: it means nothing outside the task, so it can never be written to a
/// record or key a table that outlives the task. Such a value is written as its key
/// ([`LocalTable::key`]).
///
/// ```compile_fail,E0277
/// use blaze_surface::{AnyItem, Local};
///
/// fn written_to_a_record<P: blaze_id::Portable>() {}
/// written_to_a_record::<Local<AnyItem>>();
/// ```
///
/// The control: its key is `Portable`.
///
/// ```
/// fn written_to_a_record<P: blaze_id::Portable>() {}
/// written_to_a_record::<blaze_id::ItemKey>();
/// ```
pub struct Local<T> {
    index: u32,
    of: PhantomData<fn() -> T>,
}

impl<T> Local<T> {
    /// Its number in its table.
    pub fn index(self) -> u32 {
        self.index
    }
}

impl<T> Clone for Local<T> {
    fn clone(&self) -> Local<T> {
        *self
    }
}

impl<T> Copy for Local<T> {}

impl<T> PartialEq for Local<T> {
    fn eq(&self, other: &Local<T>) -> bool {
        self.index == other.index
    }
}

impl<T> Eq for Local<T> {}

impl<T> PartialOrd for Local<T> {
    fn partial_cmp(&self, other: &Local<T>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The order a task first asked in: never an order for output or a key (identity.md, point 5).
impl<T> Ord for Local<T> {
    fn cmp(&self, other: &Local<T>) -> Ordering {
        self.index.cmp(&other.index)
    }
}

impl<T> Hash for Local<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<T> fmt::Debug for Local<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Local({})", self.index)
    }
}

/// A task's numbering of the items it asks about: an [`ItemKey`] gets a [`Local`] the first
/// time it is interned, and the same one after; a `Local` maps back to its key. It lives as
/// long as the task, in the allocator the task names (its frame), and is never kept.
pub struct LocalTable<'a, T> {
    keys: AVec<'a, ItemKey>,
    locals: Map<ItemKey, u32, FixedState, Alloc<'a>>,
    of: PhantomData<fn() -> T>,
}

impl<'a, T> LocalTable<'a, T> {
    /// An empty table in `alloc`. It allocates nothing until its first intern.
    pub fn new_in(alloc: Alloc<'a>) -> LocalTable<'a, T> {
        LocalTable { keys: AVec::new_in(alloc), locals: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }), of: PhantomData }
    }

    /// The local of `key`, numbered now if the task has not asked about it before.
    pub fn intern(&mut self, key: ItemKey) -> Result<Local<T>, SurfaceError> {
        if let Some(&index) = self.locals.get(&key) {
            return Ok(Local { index, of: PhantomData });
        }
        let index = u32::try_from(self.keys.len()).map_err(|_| SurfaceError::LocalsFull)?;
        if index == u32::MAX {
            return Err(SurfaceError::LocalsFull);
        }
        self.keys.push(key);
        self.locals.insert(key, index);
        Ok(Local { index, of: PhantomData })
    }

    /// The local of `key`, if the task has asked about it.
    pub fn get(&self, key: ItemKey) -> Option<Local<T>> {
        self.locals.get(&key).map(|&index| Local { index, of: PhantomData })
    }

    /// The key of `local`, refused when it is no number of this table.
    pub fn key(&self, local: Local<T>) -> Result<ItemKey, SurfaceError> {
        usize::try_from(local.index)
            .ok()
            .and_then(|at| self.keys.get(at))
            .copied()
            .ok_or(SurfaceError::NoLocal { local: local.index })
    }

    /// How many items the task has numbered.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}
