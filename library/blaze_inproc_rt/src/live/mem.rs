//! Views of the running image's memory at addresses the linker placed things: the descriptor's
//! bytes, the jump table's words and the execution log's bytes. blaze-live's switch reads and
//! writes the image through these too.

use std::sync::atomic::{AtomicU8, AtomicU64};

/// `len` bytes of the image at `at`.
///
/// # Safety
///
/// `at..at + len` is mapped readable for the life of the program (the platform checked it
/// is in the image), and nothing writes it while the slice is used.
pub unsafe fn view(at: u64, len: usize) -> &'static [u8] {
    // SAFETY: the caller's; any bytes are valid `u8`s.
    unsafe { std::slice::from_raw_parts(at as *const u8, len) }
}

/// The byte at `at`, which log stubs and other processes sharing its pages may store to.
///
/// # Safety
///
/// `at` is mapped readable and writable for the life of the program, and accessed only
/// atomically or by the log stubs' single-byte stores.
pub unsafe fn byte(at: u64) -> &'static AtomicU8 {
    // SAFETY: the caller's; an `AtomicU8` has a `u8`'s size and alignment.
    unsafe { AtomicU8::from_ptr(at as *mut u8) }
}

/// The 8-byte word at `at`: a table cell, a word of the jump table's header or of a table.
///
/// # Safety
///
/// `at` is 8-aligned, in the image's data, mapped for the life of the program, and accessed
/// only atomically: the switch's stores and the stubs' loads.
pub unsafe fn cell(at: u64) -> &'static AtomicU64 {
    // SAFETY: the caller's; an `AtomicU64` has a `u64`'s size and alignment.
    unsafe { AtomicU64::from_ptr(at as *mut u64) }
}
