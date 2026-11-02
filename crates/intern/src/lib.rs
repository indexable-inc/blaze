//! Global interning in sharded, append-only arenas (docs/v2/plan/lanes/format.md section 5, "IDs, two
//! kinds"): an interned value is stored once, never moves and is never freed while the process
//! runs. An [`Interned`]'s handle is a `Copy` pointer compared by address. A [`Symbol`]'s and a
//! [`Mapped`] value's is a `u32` ID that means the same in every process restoring one image
//! (docs/v2/plan/design/restore.md): written into it, opened in place, and freed only by its GC
//! ([`Live`]), which never renumbers a value still named.
//!
//! Unsafe code lives in two modules with safe APIs (docs/v2/plan/design/unsafe.md): `space`, the
//! raw region memory (claims, zeroed tables, placed values, published pointers, the ID column's
//! words), and `typed`, the typed values of a store (a `T`, a header and items, a table's
//! record). The symbol table's one view of its bytes as text is `symbol`'s `text` module.

#![deny(unsafe_code)]

mod arena;
mod concat;
#[cfg(feature = "fuzz")]
#[allow(unsafe_code)]
pub mod fuzz;
mod intern;
mod intern_slice;
mod mapped;
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
pub use self::mapped::{Id, Mapped};
pub use self::symbol::{
    KnownNames, Symbol, order_key, restore_symbols, restored_from, symbols as sym, symbols_schema,
    write_symbols,
};
pub use self::table::{Jobs, Live, RestoreError, serial};
