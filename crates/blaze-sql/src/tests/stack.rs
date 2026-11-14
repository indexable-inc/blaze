//! A test-only measure of the stack a call uses. Every recursive function of the checker calls
//! [`touch`] on entry (under `#[cfg(test)]`), and [`used`] says how far below its own frame the
//! deepest touch was. A bound on nesting is a bound on stack only if it is measured: a test that
//! finds out by overflowing aborts the process and hides every other test's result.

use std::cell::Cell;

std::thread_local! {
    static LOW: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Records the address of this frame if it is the lowest seen on this thread since [`used`]
/// began. The stack grows down on every target blaze builds for.
#[inline(never)]
pub(crate) fn touch() {
    let here = 0u8;
    let at = core::ptr::addr_of!(here) as usize;
    LOW.with(|low| {
        if at < low.get() {
            low.set(at);
        }
    });
}

/// Runs `f`, and returns what it returned with the bytes of stack between this call and the
/// deepest [`touch`] under it (0 when nothing touched).
#[inline(never)]
pub(crate) fn used<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let here = 0u8;
    let top = core::ptr::addr_of!(here) as usize;
    LOW.with(|low| low.set(usize::MAX));
    let result = f();
    let low = LOW.with(Cell::get);
    (result, if low == usize::MAX { 0 } else { top.saturating_sub(low) })
}
