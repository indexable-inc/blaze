//! Thread-locals (`#[thread_local] static`): by INDEX ([`index`]; THREADS.md D2), one copy per
//! thread-local per thread, made on the thread's first touch from the thread-local's row, found
//! through a per-thread table under one POSIX thread-specific key. What follows down to the
//! list's end describes the block form D2 replaced and is kept as the record of why a
//! thread-specific key is the mechanism; the block itself is gone.
//!
//! # The scheme, and why
//!
//! - The program's thread-locals are laid out into one block ([`template`]): each at its
//!   alignment, in item-key order. The template is a static of the program (placed with the
//!   other statics, [`crate::data::StaticImage`]); its relocations are filled once at load, so
//!   every thread's copy starts with the same absolute addresses. A thread-local's initialiser
//!   cannot point at a thread-local (Rust refuses to take one's address in a constant), so
//!   nothing in a block points into a block.
//! - A descriptor static ([`descriptor`]) holds the key, the block's size and alignment and the
//!   template's address: `{ key: u64, size: u64, align: u64, template: *const u8 }`. Load
//!   creates the key (`pthread_key_create(&desc.key, free)`) before any code runs.
//! - An access is LIR's `tls_addr` of the thread-local, which the backend lowers (blaze-emit's
//!   `pass/tls.rs`). On macOS it reads the key's slot directly: `mrs tpidrro_el0`, clear the low
//!   bits, and one load at `key * 8`, with no call. The getter runs only when that slot is empty.
//!   Elsewhere every access calls the getter ([`getter`], a LIR function built here). The getter
//!   does `pthread_getspecific`; on a thread's first access it also does `aligned_alloc` of the
//!   block, `memcpy` of the template and `pthread_setspecific`. A thread's exit frees its block
//!   (the key's destructor).
//!
//! Why: code blaze places is not a dyld image or an ELF module, so neither Mach-O thread-local
//! variables (`__thread_vars`, set up by dyld per image) nor ELF TLS (`tpidr_el0` offsets the
//! static linker fixes) can reach it; a thread-specific key is the one mechanism both systems
//! offer to code loaded at run time. It is also what Mach-O's own thread-locals cost: every
//! access there is a call through the variable's descriptor to `tlv_get_addr`, which reads
//! the thread's specific-data slots. The macOS form above is that floor with no call. It relies
//! on the private layout `_pthread_getspecific_direct` reads (rule 45), and blaze-emit's
//! `tls_tests.rs` pins that the direct read equals `pthread_getspecific`.
//! - The block is allocated lazily, so a thread that touches no thread-local allocates nothing,
//!   and a program with no thread-locals has no key ([`template`] gives `None`).

use blaze_typeck::ItemKey;

use crate::data::DataError;

/// The trap code of a failed allocation of a thread's table or copy ([`index`]'s getter).
pub const NO_MEMORY_TRAP: u32 = blaze_code::trap::NO_MEMORY;

/// A thread-local's initialiser that points into its own value: one thread's block would
/// point into another's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelfPointer {
    pub static_: ItemKey,
}

/// Why no template.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsError {
    Data(DataError),
    SelfPointer(SelfPointer),
}

impl core::fmt::Display for TlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TlsError::Data(e) => write!(f, "a thread-local's initialiser: {e}"),
            TlsError::SelfPointer(s) => write!(f, "thread-local {:#x} points into its own value", s.static_.0),
        }
    }
}

impl core::error::Error for TlsError {}

impl From<DataError> for TlsError {
    fn from(e: DataError) -> TlsError {
        TlsError::Data(e)
    }
}

pub mod index;

#[cfg(test)]
mod index_tests;
