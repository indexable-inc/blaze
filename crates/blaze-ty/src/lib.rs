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

#![no_std]
#![feature(allocator_api, portable_simd, register_tool, custom_inner_attributes)]
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
mod tys;

#[cfg(test)]
mod tests;

pub use canon::{CanonBuf, CanonKind, CanonVar, Canonical, Mode};
pub use cx::{Outlives, Snapshot, TyCx};
pub use error::{FoldError, ResponseError, TypeError};
pub use flags::{Flags, markers};
pub use fold::{AllInvariant, BoundTo, VarianceOf};
pub use ids::{
    Arg, ArgKind, Const, Def, FloatTy, InferTy, IntTy, List, Mutability, Node, Pred, Region, Ty, UintTy, Universe,
    VarKind, Variance,
};
pub use portable::{ImportError, PTy, PTys};
pub use print::{DefaultNames, Names, Out, decimal, print};
pub use solve::{Ctx, Obligation, Solver};
pub use tys::{AliasKind, ConstKind, FnHeader, PredKind, RegionKind, TyKind, Tys};
