//! The native libraries a program links, read from their own bytes (NATIVE.md section 3; piece
//! W1): the C, C++ and assembly that build scripts compile (aws-lc, blst, c-kzg, rocksdb, mdbx,
//! ring, secp256k1, zstd, zlib, jemalloc) arrive as archives of relocatable objects, and blaze
//! links them itself into one shared-library record per program. This crate is the readers.
//!
//! | module | what |
//! |---|---|
//! | [`ar`] | `ar` archives, BSD (`#1/len` names) and GNU (`//` long-name table) forms; symbol indexes skipped, thin archives refused |
//! | [`object`] | the one object model both formats are read into, and [`object::read`], which dispatches by the platform and the magic |
//! | [`macho`] | Mach-O `MH_OBJECT` for `CPU_TYPE_ARM64`: sections, `nlist_64` symbols, `ARM64_RELOC_*` (ADDEND and SUBTRACTOR pairs folded), `__LD,__compact_unwind` rows |
//! | [`elf`] | ELF64 little-endian `ET_REL` for `EM_AARCH64`: sections, symbols, RELA relocations, `SHT_GROUP` COMDAT groups |
//!
//! Sans-IO: bytes in, tables out, every name and section's bytes borrowed from the input in place.
//! Every offset is bounds-checked before a read; a form the readers do not handle is refused by
//! name, never guessed. The output is a function of the bytes alone: tables in file order,
//! relocations sorted by (section, offset).

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

pub mod ar;
pub mod descriptor;
pub mod elf;
pub mod image;
pub mod layout;
pub mod libraries;
pub mod macho;
pub mod object;
pub mod resolve;
pub mod synthetic;
pub mod tbd;
pub mod unwind;

/// The platform a native object is built for: it fixes the one object format accepted
/// (Mach-O on macOS, ELF on Linux) and the architecture (arm64 on both).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    MacosArm64,
    LinuxArm64,
}
