//! The optimizing tier's LLVM (docs/v2/plan/design/backend-next.md, "The outside critic's
//! findings and the architect's objections, decided", D100 points 7, 8, 9 and 14):
//!
//! - [`compile`] and [`bitcode`] (D1): one module, a fresh LLVM context per call, the named
//!   custom pipeline ([`Pipeline`]), the inliner's remarks read back ([`Remark`]; an
//!   unreadable stream is an error, never a guess), object bytes out and, for [`bitcode`], the
//!   optimized module's bitcode, kept as a record and offered to callers later. Offered callees
//!   in bitcode are linked in `available_externally` ([`Linked`]).
//! - [`lower`] (D2): LIR to LLVM IR text, offered callees `available_externally`, every other
//!   callee a declaration carrying exactly blaze's [`CalleeFacts`].
//! - [`names`]: how a symbol is named in IR and read back from a remark.
//! - [`read_lsda`] (P8b): LLVM's LSDA read back into blaze's unwind rows
//!   (`blaze_code::unwind::Row`); blaze-code eh.rs makes the LSDA again at placement.
//! - [`read`] (D3): LLVM's object (arm64 Mach-O, aarch64 ELF) read back into the new code
//!   record, `blaze_tier::Emitted`: one relocation a site, the frame (from the compact entry or
//!   either format's `__eh_frame`) checked against blaze's one frame form, the LSDA's rows;
//!   anything the record cannot say refused by name (x86-64's relocations among them).
//!
//! The contract with LLVM: its own inliner decides inside the offered bodies; NO pass infers a
//! fact about a callee from an offered body; blaze supplies the facts as attributes; the
//! target CPU is named, never `native`; alone and batched give equal bytes.
//!
//! LLVM is linked by vendor/llvm-sys (the pinned 23.1.4); every function called is declared
//! here ([`ffi`]), the C API's and blaze's three remark-stream wrappers.

#![no_std]

use llvm_sys as _;

pub mod compile;
pub mod ffi;
pub mod lower;
pub mod lower_target;
pub mod names;
pub mod pipeline;
pub mod read;
pub mod read_lsda;
pub mod remarks;
pub mod table_gen;

#[cfg(test)]
mod table_gen_tests;

pub use compile::{Linked, LlvmError, Severity, bitcode, compile, default_pipeline, init, triple};
pub use lower::{CalleeFacts, LowerError, Lowered, Offered, lower};
pub use pipeline::Pipeline;
pub use read::{ObjName, ReadError, read_object};
pub use remarks::{Remark, RemarkError};
