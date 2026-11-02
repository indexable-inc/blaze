//! The backend's tests (stencil.md, "Tests"): that the stencil backend means what Cranelift
//! means. Each stencil against `cranelift-interpreter` and Cranelift's code of the same
//! one-instruction function ([`stencil`]), whole functions of both backends calling each
//! other ([`func`]), the CLIF corpus ([`clif`]), and seeded random IR ([`fuzz`]).
//!
//! What runs code ([`exec`], the crate's one unsafe module) runs only on the host's own
//! variant: x86-64 Linux, AArch64 Linux, or AArch64 macOS (`apple-m1`), each the variant its
//! CI host covers. Compiling for every target the IR builder makes code for, and the
//! interpreter's side, run everywhere.

pub(crate) mod host;
mod interp;
mod rng;

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod exec;

mod clif;
mod func;
mod fuzz;
mod stencil;
