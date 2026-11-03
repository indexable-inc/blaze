//! The two thread-local pointers Wasmtime keeps where it has no platform layer of its own: on a
//! target that is neither Unix nor Windows (the wasm seed, `wasm32-wasip1`), Wasmtime's
//! `sys::custom` imports `wasmtime_tls_get` and `wasmtime_tls_set` from its embedder
//! (`wasmtime/src/runtime/vm/sys/custom/capi.rs`). Slot 0 is the runtime's activation pointer;
//! slot 1 is component-model-async's, which blaze does not enable, kept because Wasmtime's
//! contract names it. Each starts null, per thread. The cfg is Wasmtime's own choice of
//! `sys::custom` with `std` (`sys/mod.rs`), so the two are defined exactly where they are
//! imported.

#![allow(unsafe_code, reason = "Wasmtime imports these by their unmangled C names")]

use std::{cell::Cell, ptr};

/// Wasmtime passes only 0 and 1 (`capi::wasmtime_tls_get`).
const SLOTS: usize = 2;

thread_local! {
    static TLS: [Cell<*mut u8>; SLOTS] = const { [Cell::new(ptr::null_mut()), Cell::new(ptr::null_mut())] };
}

/// The pointer in `slot` on this thread, null until set.
#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_tls_get(slot: usize) -> *mut u8 {
    TLS.with(|it| it[slot].get())
}

/// Sets the pointer in `slot` on this thread.
#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_tls_set(slot: usize, ptr: *mut u8) {
    TLS.with(|it| it[slot].set(ptr));
}
