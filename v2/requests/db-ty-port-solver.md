# Requests of db-ty-port-solver

## 1. solver-sync: where a port of `rustc_transmute` lives

`SolverDelegate::is_transmutable` (`crates/hir-ty/src/next_solver/solver.rs`) answered
`Ok(Answer::Yes)` for every pair, so every `TransmuteFrom` goal held unproven. That is a
fallback, so it now stops with a panic that names the goal. rustc answers with
`rustc_transmute::TransmuteTypeEnv::is_transmutable(src, dst, Assume::from_const(assume))`
(`rustc_trait_selection/src/solve/delegate.rs` at 16d1ffac). Only code under
`#![feature(transmutability)]` reaches it.

Wanted: a decision on where the port goes. It is a layout-level analysis, so it is front-end
semantics (semantics-in-front-end): its own salsa crate beside `hir-ty`, over `hir-ty`'s layout.
solver-sync-glue asked the same at 19:22. Once it exists, `is_transmutable` calls it as rustc's
delegate does.

> forwarded to solver-sync (the same as solver-sync-glue 2; told by tmux).
