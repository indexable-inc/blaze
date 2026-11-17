//! Global interning in sharded, append-only arenas (docs/v2/plan/design/data.md section 5, "IDs, two
//! kinds"): an interned value is stored once, never moves and is never freed while the process
//! runs. An [`Interned`]'s handle is a `Copy` pointer compared by address. A [`Symbol`]'s is a
//! `u32` ID into the process's symbol table.
//!
//! Unsafe code lives in two modules with safe APIs (docs/v2/plan/design/unsafe.md): `space`, the
//! raw region memory (claims, zeroed tables, placed values, published pointers, the ID column's
//! words), and `typed`, the typed values of a store (a `T`, a header and items, a table's
//! record). The symbol table's one view of its bytes as text is `symbol`'s `text` module.

#![deny(unsafe_code)]
#![feature(allocator_api)]

mod arena;
mod concat;
mod intern;
mod intern_slice;
#[allow(unsafe_code)]
mod space;
mod symbol;
mod sync;
mod table;
#[allow(unsafe_code)]
mod typed;

/// Counts allocations so the read paths' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub use blaze_mem::swiss;

pub use self::intern::{InternStorage, Internable, Interned, InternedRef, impl_internable};
pub use self::intern_slice::{
    HeaderSlice, HeaderWithLength, InternSliceStorage, InternedSlice, InternedSliceRef,
    SliceInternable, impl_slice_internable,
};
pub use self::symbol::{KnownNames, Symbol, order_key, symbols as sym};
