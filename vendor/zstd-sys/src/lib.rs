#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![no_std]
//! Low-level bindings to the [zstd] library.
//!
//! [zstd]: https://facebook.github.io/zstd/

// blaze: on wasm the declarations below resolve to libzstd-rs-sys's exported symbols, in place
// of upstream's C library and its `wasm_shim` allocator (SOURCE.md).
#[cfg(target_family = "wasm")]
extern crate libzstd_rs_sys as _;

// If running bindgen, we'll end up with the correct bindings anyway.
#[cfg(feature = "bindgen")]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// The bindings used depend on a few feature flags.
#[cfg(all(not(feature = "experimental"), not(feature = "bindgen")))]
include!("bindings_zstd.rs");

#[cfg(all(
    not(feature = "experimental"),
    feature = "zdict_builder",
    not(feature = "bindgen")
))]
include!("bindings_zdict.rs");

#[cfg(all(feature = "experimental", not(feature = "bindgen")))]
include!("bindings_zstd_experimental.rs");

#[cfg(all(
    feature = "experimental",
    feature = "zdict_builder",
    not(feature = "bindgen")
))]
include!("bindings_zdict_experimental.rs");

#[cfg(all(feature = "seekable", not(feature = "bindgen")))]
include!("bindings_zstd_seekable.rs");
