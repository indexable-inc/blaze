//! rustc's lints of function bodies (`lints::bodies`), checked against rustc's UI tests with
//! their `.stderr`: what type checking finds unreachable, match checking's
//! `unreachable_patterns` and `irrefutable_let_patterns`, `unused_unsafe`,
//! `unconditional_recursion`, liveness, the known-panics lint and the late lints of calls and
//! types.

mod calls;
mod liveness;
mod panics;
mod patterns;
pub(crate) mod reachable;
mod recursion;
pub(crate) mod types;
mod unsafety;
mod unstable_names;
