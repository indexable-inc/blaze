//! The test language (solve.md, "Tests"): `.solve` files of declarations and cases, each case
//! naming the reference rule it tests; its parser, an arena modelling blaze-ty's, an in-memory
//! world answering `Facts`, and the runner that checks every goal's answer.

pub mod parse;
pub mod run;
pub mod stagger;
pub mod tys;
pub mod world;
