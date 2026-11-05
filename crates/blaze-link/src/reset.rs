//! Kept state behind a lock, started over when a holder panicked: a page may be left without its
//! hash, a table half patched, so the next holder finds the default rather than a half-done
//! change.

use std::ops::{Deref, DerefMut};

use parking_lot::{Mutex, MutexGuard};

/// `lock` held: dropped while its thread unwinds from a panic that started under it, it puts
/// back `T::default()`.
pub(crate) struct Reset<'a, T: Default> {
    guard: MutexGuard<'a, T>,
    panicking: bool,
}

pub(crate) fn lock<T: Default>(lock: &Mutex<T>) -> Reset<'_, T> {
    Reset { guard: lock.lock(), panicking: std::thread::panicking() }
}

impl<T: Default> Deref for Reset<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: Default> DerefMut for Reset<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T: Default> Drop for Reset<'_, T> {
    fn drop(&mut self) {
        if !self.panicking && std::thread::panicking() {
            *self.guard = T::default();
        }
    }
}
