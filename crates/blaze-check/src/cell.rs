//! `UnsafeCell` with loom's `with`/`with_mut`: in a model each is a tracked non-atomic read or
//! write, and one that happens-before does not order with another thread's write (or a write
//! with another thread's read) is a data race, a finding. Accesses are not schedule points:
//! whether two race depends only on the clocks the atomics gave each thread.

use std::{panic::Location, sync::atomic::AtomicU64 as Id};

use crate::rt::{
    self, At, Object,
    memory::{self, Bytes},
};

#[track_caller]
fn bytes(ptr: *const u8, len: usize, kind: Bytes) {
    let at = Location::caller();
    let checked = rt::with(|rt| {
        rt.note(at, "tracked bytes");
        rt.bytes_access(ptr.addr(), len, at, kind)
    });
    if let Some(Err(found)) = checked {
        rt::stop(found);
    }
}

/// A tracked non-atomic read of the `len` raw bytes at `ptr` (a buffer no `UnsafeCell` holds):
/// in a model, the last write of each byte must happen before it, and none of them may be
/// freed. Outside a model, nothing. A crate's `std` twin is an `#[inline(always)]` no-op.
#[track_caller]
pub fn read(ptr: *const u8, len: usize) {
    bytes(ptr, len, Bytes::Read);
}

/// A tracked non-atomic write of the `len` raw bytes at `ptr`: in a model, every earlier read
/// and write of each byte must happen before it. A write over freed bytes starts a new
/// allocation there.
#[track_caller]
pub fn write(ptr: *const u8, len: usize) {
    bytes(ptr, len, Bytes::Write);
}

/// The `len` raw bytes at `ptr` are freed: checked as a write (every read of them must happen
/// before the free: a reader still holding them is a use after reclaim), then a later read is a
/// use after free and a later free a double free. Every tracked buffer's free must say so here,
/// or a new allocation at its address inherits its shadow.
#[track_caller]
pub fn free(ptr: *const u8, len: usize) {
    bytes(ptr, len, Bytes::Free);
}

/// `std::cell::UnsafeCell`, reached through [`UnsafeCell::with`] and [`UnsafeCell::with_mut`]
/// so the model sees each access. A crate's `std` twin of it (its `sync.rs`) has the same two
/// methods, calling the closure on `get()`.
pub struct UnsafeCell<T: ?Sized> {
    id: Id,
    data: std::cell::UnsafeCell<T>,
}

impl<T> UnsafeCell<T> {
    pub const fn new(value: T) -> UnsafeCell<T> {
        UnsafeCell { id: Id::new(0), data: std::cell::UnsafeCell::new(value) }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> UnsafeCell<T> {
    fn access(&self, at: At, write: bool) {
        let checked = rt::with(|rt| {
            let obj = rt.object(&self.id, |_| Object::Cell(memory::Cell::new()));
            if write { rt.cell_write(obj, at) } else { rt.cell_read(obj, at) }
        });
        if let Some(Err(found)) = checked {
            rt::stop(found);
        }
    }

    /// A read through `*const T`, for as long as `f` runs.
    #[track_caller]
    pub fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        self.access(Location::caller(), false);
        f(self.data.get())
    }

    /// A write through `*mut T`, for as long as `f` runs.
    #[track_caller]
    pub fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        self.access(Location::caller(), true);
        f(self.data.get())
    }

    #[track_caller]
    pub fn get_mut(&mut self) -> &mut T {
        self.access(Location::caller(), true);
        self.data.get_mut()
    }
}

impl<T: Default> Default for UnsafeCell<T> {
    fn default() -> UnsafeCell<T> {
        UnsafeCell::new(T::default())
    }
}

impl<T> From<T> for UnsafeCell<T> {
    fn from(value: T) -> UnsafeCell<T> {
        UnsafeCell::new(value)
    }
}
