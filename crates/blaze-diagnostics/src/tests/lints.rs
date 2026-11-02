//! rustc's lints and their levels, and what rustc's MIR passes give (borrow checking, const
//! checking, match checking, unsafety), checked against rustc's UI tests.

pub(crate) mod bodies;
mod levels;
mod mir;
mod names;
mod syntax;
