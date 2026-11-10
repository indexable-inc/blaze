//! Symbolic differentiation of scalar float expressions: layers 2 and 3 of
//! docs/future/autodiff.md, without the compiler around them.
//!
//! - [`expr`]: the expression graph, a hash-consed DAG over `f32` or `f64` ([`Graph`]).
//! - [`diff`]: `d(expr)/d(variable)` by one rule per operation ([`diff::RULES`]), with sharing;
//!   gradients, Jacobians and Hessians; a typed choice at points without a derivative.
//! - [`simplify`]: equality saturation on an e-graph, by rewrites that are exact in IEEE 754
//!   and, when asked, algebraic ones ([`simplify::REWRITES`]); bounded by fuel.
//! - [`print`]: an expression as a Rust function and as one line of math.
//! - [`eval`]: an expression's value at a point, and what each operation means.
//!
//! Lifting a function's MIR to a [`Graph`] and lowering a graph back to MIR is `blaze-diff-mir`;
//! nothing here knows the compiler. The crate is a sans-IO core: pure computation, no clock,
//! file, thread or database.

#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

pub mod diff;
pub mod eval;
pub mod expr;
pub mod print;
pub mod simplify;

#[cfg(test)]
mod tests;

pub use diff::{Choice, DiffError, Policy};
pub use expr::{Graph, Id, Node, Op, Sort, Width};
pub use simplify::{CostModel, Fuel, Options, Simplified, Stop};

/// The derivatives of `roots` by variable `var`, simplified: [`diff::Differentiator`] by
/// [`diff::RULES`], then [`simplify::simplify`]. The derivatives are added to `graph`; the
/// result's graph holds the simplified ones alone, in the order of `roots`.
pub fn differentiate(
    graph: &mut Graph,
    roots: &[Id],
    var: u32,
    policy: Policy,
    options: &Options,
) -> Result<Simplified, DiffError> {
    let derivatives = diff::Differentiator::new(policy).derivatives(graph, roots, var)?;
    Ok(simplify::simplify(graph, &derivatives, options))
}
