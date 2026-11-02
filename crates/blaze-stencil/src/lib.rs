//! Copy-and-patch codegen for dev runs (docs/v2/plan/design/compile-time.md, change 4; the
//! design is docs/v2/plan/lanes/fastcompile/stencil.md).
//!
//! When blaze is built, LLVM compiles one *stencil* per Cranelift IR operation, operand shape
//! and type ([`shape`]): a few instructions in continuation-passing style, each ending in a
//! tail jump to the next, with *holes* where a frame offset, a constant or a jump target goes.
//! The generator (`gen/`, run by `build.rs`) records each hole as the relocation LLVM made for
//! it, and lays the stencils out as one library per architecture variant ([`table`]).
//!
//! Compiling a function ([`compile`]) is then: pick the stencil of each instruction, copy it
//! into one buffer, and patch its holes ([`patch`]); what no stencil can be (the prologue,
//! calls in the target's convention, returns, jump tables, traps, addresses of symbols) is
//! written by the architecture's glue ([`arch`]) exactly as Cranelift's code does it, so code
//! of this backend and of Cranelift call each other freely. What a function needs that neither
//! has is an error naming the instruction, never a quiet compile by Cranelift.
//!
//! The input is the same Cranelift IR the Cranelift backend compiles (`hir-ty-codegen`'s IR
//! builder, after ABI lowering), so every MIR construct lowers once, by one set of rules, and
//! both backends agree on what a program means. The output ([`code::Code`]) is what Cranelift's
//! compile gives the codegen lane: bytes, relocations by Cranelift's names, unwind rows, call
//! sites with landing pads, lines and named slots.
//!
//! Tier 0 code counts its calls and loop back edges ([`tier`]); the daemon recompiles what the
//! counts say is hot with Cranelift or LLVM and patches it into the function's slot.

// Denied, not forbidden: the one unsafe module is the tests' `tests/exec.rs`, which maps code
// and calls it.
#![deny(unsafe_code)]
#![feature(allocator_api, allocator_ext)]

pub mod arch;
pub mod buf;
pub mod code;
pub mod enc;
pub mod patch;
pub mod shape;
pub mod table;
pub mod tier;

#[cfg(test)]
mod tests;

use std::fmt;

use cranelift_codegen::ir;

pub use code::{Code, CodeRef};

/// The platform a function is compiled for, as its glue needs it: everything besides the
/// architecture that changes the bytes of a prologue, a call or an address. Each field is
/// the setting `hir-ty-codegen/target.rs` gives Cranelift for the same target.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Target {
    pub arch: blaze_codegen::Arch,
    pub os: Os,
    /// The CPU baseline Cranelift's ISA is given: which stencil library the code is made of.
    pub cpu: Cpu,
    /// Position-independent code: addresses of what is not colocated go through the GOT.
    pub pic: bool,
    pub tls: TlsModel,
    /// Frames larger than a page touch each page on the way down (Cranelift's inline
    /// `probestack`), so a stack overflow faults in the guard page.
    pub probe_stack: bool,
    /// log2 of the probe interval: Cranelift's `probestack_size_log2`.
    pub probe_log2: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

/// The CPU baseline of a target: `hir-ty-codegen`'s `Target::baseline_settings`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cpu {
    /// The architecture's baseline: x86-64 (with `cmpxchg16b`, which only 128-bit
    /// compare-and-swap uses), or AArch64 generic.
    Baseline,
    /// `apple-m1`: AArch64 with LSE atomics (Cranelift's `has_lse`).
    AppleM1,
}

/// How a thread-local's address is found: Cranelift's `tls_model` setting.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TlsModel {
    /// ELF general dynamic (`__tls_get_addr`, or TLS descriptors on AArch64).
    ElfGd,
    /// Mach-O's thread-local variable descriptors.
    MachO,
    /// COFF's `_tls_index` and the TEB.
    Coff,
}

/// Why a function did not compile: the instruction (when one is to blame) and what of it.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error, once per function that fails
pub struct Error {
    pub inst: Option<ir::Inst>,
    pub what: String,
    /// Set when no stencil writes the instruction: what the library lacks.
    pub gap: Option<Gap>,
}

/// An instruction no stencil and no glue writes, by its opcode and controlling type
/// (`types::INVALID` for one with no value): what the stencil library must grow.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Gap {
    pub opcode: ir::Opcode,
    pub ty: ir::Type,
}

impl Error {
    pub fn new(what: impl Into<String>) -> Error {
        Error { inst: None, what: what.into(), gap: None }
    }

    pub fn at(inst: ir::Inst, what: impl Into<String>) -> Error {
        Error { inst: Some(inst), what: what.into(), gap: None }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inst {
            Some(inst) => write!(f, "copy-and-patch: {inst}: {}", self.what),
            None => write!(f, "copy-and-patch: {}", self.what),
        }
    }
}

impl std::error::Error for Error {}

/// Compiles one function of Cranelift IR, after ABI lowering, for `target`.
pub fn compile(func: &ir::Function, target: &Target) -> Result<Code, Error> {
    compile_to(func, target, |code| code.to_code())
}

/// [`compile`] into `out`, whose vectors are reused: a worker that keeps one `Code` allocates
/// only for a function bigger than any before it.
pub fn compile_into(func: &ir::Function, target: &Target, out: &mut Code) -> Result<(), Error> {
    compile_to(func, target, |code| out.copy_from(code))
}

/// [`compile`], handing the code to `sink` while it is still in the compile's frame
/// (`blaze_mem::frame::task`): the consumer copies what it keeps once, straight to where it
/// goes.
pub fn compile_to<R>(
    func: &ir::Function,
    target: &Target,
    sink: impl FnOnce(&CodeRef<'_>) -> R,
) -> Result<R, Error> {
    let library = table::Library::get(table::Variant::of(target)?)?;
    match target.arch {
        blaze_codegen::Arch::X86_64 => {
            patch::compile::<arch::x64::X64, R>(func, target, library, sink)
        }
        blaze_codegen::Arch::Aarch64 => {
            patch::compile::<arch::aarch64::Aarch64, R>(func, target, library, sink)
        }
        blaze_codegen::Arch::Wasm32 => {
            Err(Error::new("wasm32 has no stencils: its code is written from the IR as a module"))
        }
    }
}

/// What keeps `func` from compiling for `target`: every instruction no stencil or glue
/// writes, each with its [`Gap`], or the one error the compile stops at past selection. Empty
/// when it compiles. The coverage survey's entry: [`compile`] stops at the first refusal.
pub fn survey(func: &ir::Function, target: &Target) -> Result<Vec<Error>, Error> {
    let library = table::Library::get(table::Variant::of(target)?)?;
    match target.arch {
        blaze_codegen::Arch::X86_64 => Ok(patch::survey::<arch::x64::X64>(func, target, library)),
        blaze_codegen::Arch::Aarch64 => Ok(patch::survey::<arch::aarch64::Aarch64>(func, target, library)),
        blaze_codegen::Arch::Wasm32 => {
            Err(Error::new("wasm32 has no stencils: its code is written from the IR as a module"))
        }
    }
}

/// The identity of the stencil library `target`'s code is made of: the BLAKE3 hash of its
/// bytes, which codegen hashes into the `Settings` of every stencil unit
/// (`Toolchain::stencils`), so a change to any stencil is a change to every key.
pub fn library_id(target: &Target) -> Result<[u8; 32], Error> {
    Ok(table::Library::get(table::Variant::of(target)?)?.id())
}
