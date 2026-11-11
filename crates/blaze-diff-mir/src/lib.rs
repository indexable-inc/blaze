//! Symbolic derivatives of pure functions over MIR: layer 2 of docs/future/autodiff.md.
//!
//! `blaze-diff` has the expression graph, the derivative rules, the simplifier and the
//! printer, and knows nothing of the compiler. This crate is the compiler's side:
//!
//! - [`lift`]: a function's monomorphized runtime MIR as an expression graph, or a typed
//!   refusal naming the loop, branch, call, memory or type that did not lift.
//! - [`lower`]: an expression graph as a runtime MIR body, which the normal pipeline compiles.
//! - [`surface`]: `#[blaze::differentiable(symbolic)]` on a `#[blaze::pure]` function, its
//!   derivative body ([`derivative_body`]) and its explanation ([`explain`]).
//!
//! The refinement the tests hold: for a body that lifts, the graph evaluates to what the body
//! computes, bit for bit, at every input; and the body lowered from the graph does too.
//!
//! It is a crate of its own, not a pass in hir-ty-mir's `mir/optimize`: it is not one of
//! rustc's passes and no body goes through it unless a derivative is asked (an extension
//! changes no program that does not use it), and it reads `#[blaze::pure]`'s proof, which is
//! `hir-ty-pure`'s, a crate above hir-ty-mir.

#![forbid(unsafe_code)]
#![feature(allocator_api)]

pub mod lift;
pub mod lower;
pub mod surface;

#[cfg(test)]
mod tests;

pub use lift::{Callees, Lifted, NotLifted, Signature, lift};
pub use lower::{NotLowered, lower};
pub use surface::{Derived, DiffError, derivative_body, derived, explain, symbolic_mode};
