//! MIR's passes over hir-ty's MIR: lowering a body to it, the passes to runtime MIR and
//! rustc's optimizations, monomorphization, the interpreter, and printing a body.
//!
//! hir-ty has MIR's types and errors (`hir_ty::mir`), and it declares the memos of what it asks
//! of these passes, a body, a constant's value (`hir_ty::providers`); this crate registers
//! their code (`mir::providers`). An edit of a pass compiles this crate again, not hir-ty. The passes see hir-ty's modules at this crate's root, and its `mir` as their
//! parent, as when they were in hir-ty.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]
#![allow(rustdoc::private_intra_doc_links)]

extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
use hir_def::CallableDefId;
#[allow(unused_imports, reason = "hir-ty's root, which the passes name as `crate::`")]
use hir_ty::*;

pub mod const_macro;
pub mod mir;
pub mod mir_pretty;

#[cfg(test)]
mod test_db;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
