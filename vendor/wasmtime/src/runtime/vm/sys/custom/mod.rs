//! Custom platform support in Wasmtime.
//!
//! This module contains an implementation of defining Wasmtime's platform
//! support in terms of a minimal C API. This API can be found in the `capi`
//! module and all other functionality here is implemented in terms of that
//! module.
//!
//! For more information about this see `./examples/min-platform` as well as
//! `./docs/examples-minimal.md`.

#![warn(dead_code, unused_imports)]

#[cfg(has_virtual_memory)]
use crate::prelude::*;

pub mod capi;
#[cfg(has_virtual_memory)]
pub mod mmap;
/// With `std` but no virtual memory (e.g. `wasm32-wasip1`) files are read, not mapped:
/// `MmapVec::from_file` copies them into the allocator.
// blaze: upstream has no `mmap` here without virtual memory, which `vm.rs` imports with `std`
// (vendor/wasmtime/SOURCE.md).
#[cfg(all(feature = "std", not(has_virtual_memory)))]
pub mod mmap {
    use crate::prelude::*;
    use std::fs::File;
    use std::path::Path;

    pub fn open_file_for_mmap(path: &Path) -> Result<File> {
        File::open(path).context("failed to open file")
    }
}
pub mod traphandlers;
#[cfg(has_host_compiler_backend)]
pub mod unwind;
#[cfg(has_virtual_memory)]
pub mod vm;

#[cfg(has_virtual_memory)]
fn cvt(rc: i32) -> Result<()> {
    match rc {
        0 => Ok(()),
        code => bail!("os error {code}"),
    }
}

#[inline]
pub fn tls_get() -> *mut u8 {
    unsafe { capi::wasmtime_tls_get(0) }
}

#[inline]
pub fn tls_set(ptr: *mut u8) {
    unsafe { capi::wasmtime_tls_set(0, ptr) }
}

#[inline]
#[cfg(feature = "component-model-async")]
pub fn component_async_tls_get() -> *mut u8 {
    unsafe { capi::wasmtime_tls_get(1) }
}

#[inline]
#[cfg(feature = "component-model-async")]
pub fn component_async_tls_set(ptr: *mut u8) {
    unsafe { capi::wasmtime_tls_set(1, ptr) }
}
