//! `blaze export` (docs/v2/plan/design/bottom.md, section 11, "Commands"; run.md, "Export
//! (writer, 2026-11-16)"): one native executable from the code records of everything a program
//! reaches, which runs with no blaze on the machine. Written once, whole; not incremental; keeps
//! nothing; nothing patches a file in place.
//!
//! | module | what |
//! |---|---|
//! | [`input`] | what an export is given: functions (symbol, record, tier), statics and vtables, the entry's form, the panic strategy, the profile earlier runs left |
//! | [`layout`] | the whole program laid out: equal records folded, ordered by profile then call graph, every call in reach a direct `bl`, C names through one GOT, relro parts and data in their regions, every word the loader fixes |
//! | [`start`] | the start records the loader enters (dyld's `LC_MAIN` wrapper; Linux's `_start` into glibc's `__libc_start_main`) |
//! | [`macho`] | the Mach-O writer: `__PAGEZERO`, `__TEXT`, `__DATA_CONST`, `__DATA`, `__LINKEDIT`, chained fixups, `LC_MAIN`, libSystem, `LC_UUID` of the content |
//! | [`native_dylib`] | the program's native library record as a Mach-O `MH_DYLIB` around a `blaze_native` image: chained fixups by flat lookup, export trie, one `LC_LOAD_DYLIB` per install name, signed (NATIVE.md W4) |
//! | [`native_so`] | the same record as an ELF shared object for arm64 Linux: RELA (RELATIVE, GLOB_DAT/ABS64, TLSDESC, TLS_TPREL64), `.gnu.hash`, one `DT_NEEDED` per soname, `PT_TLS`, `PT_GNU_RELRO` over the Got (NATIVE.md W5) |
//! | [`sign`] | the ad-hoc code signature (one code directory of SHA-256 page hashes) |
//! | [`elf`] | the ELF writer: PIE, `PT_INTERP`, the dynamic section, RELA, `PT_GNU_RELRO`, `PT_GNU_STACK`, a build id of the content |
//! | [`check`] | readers that check a written file against the formats' structures (bounds, alignment, fixups, page hashes) |
//! | [`export`] | the one entry ([`export::export`]) and the command's report ([`export::Exported::report`]) |
//!
//! Sans-IO: records in, bytes out into a vector the caller passes; the command writes the file
//! and marks it executable.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

mod bytes;
pub mod check;
mod commands;
pub mod eh_out;
pub mod elf;
pub mod error;
pub mod export;
pub mod input;
pub mod layout;
pub mod macho;
pub mod native_dylib;
pub mod native_so;
pub mod native_merge;
pub mod optimized;
mod plant;
pub mod sign;
pub mod start;
/// Wasmtime 64 (`memory64`) `.wasm` module writer & validator for Blaze's sans-IO host ring ABI.
pub mod wasm64;

pub use error::ExportError;
pub use export::{Exported, Panics, export};
pub use input::{CExport, Data, DataPlace, DataWord, Entry, Function, Panic, Platform, Profile, ProgramIn, Tier, TlsRuntime};
pub use optimized::{Function02, ProgramIn02, export_optimized, v1_to_v2};
pub use wasm64::{
    Wasm64Check, Wasm64Data, Wasm64Error, Wasm64Exported, Wasm64Func, Wasm64Opts, Wasm64Program,
    check_wasm64, export_wasm64,
};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod optimized_tests;
