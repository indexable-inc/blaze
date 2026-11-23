//! blaze's types: one arena per type-checking task, the inference tables, relating, folding,
//! canonicalization and the portable form.
//!
//! Layout: a term is a row of a column set (`kind: u8`, `a: u32`, `b: u32`, `flags: u16`) in
//! [`Tys`], named by a 4-byte id whose low two bits say its class (type, region, const, list).
//! Rows are hash-consed through an open-addressed index probed sixteen tags at a time with
//! `core::simd`, so equal terms are equal ids and comparing types is comparing `u32`s. Every
//! walk (relate, fold, canonicalize, export, print) runs on an explicit stack: no recursion,
//! however deep a type. Everything is allocated in the `Alloc<'a>` the task names.
//!
//! The reference is the vendored rustc at this nightly: `vendor/rustc_type_ir` and
//! `vendor/rustc_next_trait_solver`; each module names the files it follows.
//!
//! What blaze-solve reads: the solver names terms by bare `u32`s and reads them through a
//! shared borrow (its canonicalizer, folds and prefilter walk with `&T`), so blaze-typeck's
//! context bridges it to this crate with four things:
//! 1. [`Tys::node_at`]: a node from its bits, checked against its class's row count (and
//!    [`Node::as_ty`], `as_region`, `as_const`, `as_list`); a handle is meaningful only for
//!    the arena that made it.
//! 2. [`TyCx::probe`] and [`TyCx::probe_const`]: `shallow_resolve` without a write.
//! 3. [`Vars::bindings`]: the binding log, each variable that stopped being an unbound root,
//!    truncated by a rollback; fulfillment wakes the obligations stalled on it.
//! 4. [`Vars::var_count`]: how many variables of each kind exist.

#![no_std]
#![feature(allocator_api, portable_simd, register_tool, custom_inner_attributes)]
// `core::hint::prefetch_read`: `Tys::intern_many` starts every home group's load before the
// first probe, so a batch's cache misses overlap.
#![feature(hint_prefetch)]
#![register_tool(blaze)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod canon;
mod cx;
mod error;
mod flags;
mod fold;
mod hash;
mod ids;
mod index;
mod infer;
mod kind;
mod memo;
mod portable;
mod print;
mod relate;
mod solve;
pub mod effect_ty;
pub mod pat_ty;
pub mod product_row;
pub mod record_ty;
pub mod type_desc;
pub mod type_set;
pub mod union_ty;
pub mod view_ty;
mod tys;

#[cfg(test)]
mod tests;

pub use canon::{CanonBuf, CanonKind, CanonVar, Canonical, Mode};
pub use cx::{Outlives, Snapshot, TyCx};
pub use error::{FoldError, ResponseError, TypeError};
pub use flags::{Flags, markers};
pub use infer::{VarId, VarSnapshot, Vars};
pub use fold::{AllInvariant, BoundTo, VarianceOf};
pub use ids::{
    Arg, ArgKind, Const, Def, FloatTy, InferTy, IntTy, List, Mutability, Node, Pred, Region, Ty, UintTy, Universe,
    VarKind, Variance,
};
pub use portable::{ImportError, PTy, PTys};
pub use print::{DefaultNames, Names, Out, decimal, print};
pub use solve::{Ctx, Obligation, Solver};
pub use tys::{AliasKind, ConstKind, FnHeader, PredKind, RegionKind, TyKind, Tys};
