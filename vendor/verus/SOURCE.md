# verus

- Upstream: https://github.com/verus-lang/verus, commit
  `3881d4494e035f1e8a649ede0f1e8fc4d56a19e7` (`source/`).
- Licence: MIT (LICENSE), copyright the Verus contributors.
- Kept: `air` (the Assertion Intermediate Language and its SMT driver), `vir` (the
  Verification Intermediate Representation: well-formedness, modes, simplification, SST and the
  encoding to AIR), `vir_macros` (their derive macros), `builtin` (the spec-level items vstd
  names) and `vstd` (the standard library of specifications and lemmas).
- Left out: `rust_verify` (the rustc driver; `crates/hir-ty-verus` replaces it, reading blaze's
  HIR, see docs/v2/plan/lanes/verus.md), `builtin_macros` (the `verus!` macro: blaze parses
  Verus's syntax itself in a crate with `#![feature(blaze_verify)]`),
  `state_machines_macros`, `rust_verify_build_macros`, the forked `rustc_hir_analysis`,
  `rustc_hir_typeck` and `rustc_mir_build`, `cargo-verus`, `verusdoc`, `vstd_build`,
  `rust_verify_test*`, `tools` and `docs`; the `air` binary (`air/src/main.rs`) and air's tests
  (`air/src/tests.rs`, which ran Z3 from PATH).

The crates are outside blaze's workspace (the root `Cargo.toml` excludes `vendor/verus/*`) and
`crates/hir-ty-verus` depends on `air` and `vir` by path.

## What blaze changed

`blaze.diff` is the whole diff against upstream for `air`, `vir`, `vir_macros` and `builtin`
(`diff -ru` of upstream's `source/<crate>` against this directory). In short:

- Manifests: versions spelled out instead of Verus's workspace, `publish = false`, and the
  dependencies of what was left out dropped (`getopts`, `smt-scope`, `petgraph`, `serde_json`,
  `yansi`, `win32job`).
- `air::context::Context::start_solver(executable)`: the caller names Z3 (blaze passes
  `BLAZE_Z3`, the flake's pinned 4.16.0); a command before it panics naming the missing call.
  Upstream launched the solver on first use from `PATH` or `VERUS_Z3_PATH`; a launch failure is
  now a `LaunchError` naming the solver and executable, not a panic.
- `Context::last_unknown_incomplete()`: Z3's `unknown` with reason `incomplete` is reported as
  unknown; upstream reported it as `Invalid` with Z3's model, which blaze would read as a
  refutation. Z3's other `unknown` reasons map to `Canceled`, as upstream.
- `Context::set_observers`: a query-result observer reads each refuted query's model (the
  counterexample).
- `air/src/profiler.rs` keeps only the quantifier-name prefixes VIR writes; the trace reader
  went with `smt-scope`. A profile of a proof is a blaze span instead.

## vstd

`vstd` is upstream's, converted mechanically by `tools/v2/verus_strip.py
'{"root": "vendor/verus/vstd"}'`: every column-0 `verus! {` block (and vstd's alias
`verus_skip_verusfmt!`, with its `use` line) is unwrapped, the text inside unchanged. 144 blocks
went. The `verus!` invocations inside `macro_rules!` transcribers (57 in 21 files, most in
`std_specs/ops.rs`; the script lists them) are left, as stripping them changes what those macros expand to; they go when
blaze's macro expansion runs Verus's syntax in a transcriber. Its `Cargo.toml` still names
`verus_builtin_macros` and `verus_state_machines_macros`, which are not vendored: vstd is not
built yet. It parses under blaze once the grammar has the rest of Verus's syntax (`spec`/`proof`
functions, `tracked`/`ghost`, `forall`, `==>`, `&&&`, `#[trigger]`, `@`); the plan lists the
order.

## Notes for whoever ports the rest

- `air/src/smt_verify.rs` and `vir/src/interpreter.rs` read `std::time::Instant::now`: air's
  only for the time it reports, the interpreter (`assert ... by (compute)`) for a time limit,
  which makes such a proof depend on the machine. It becomes fuel before blaze lowers `by
  (compute)`.
- `vir/src/well_formed.rs` and `air/src/singular_manager.rs` read `VERUS_SINGULAR_PATH` for
  `integer_ring` proofs; blaze does not lower those yet, and pins Singular in the flake when it
  does.

## Credit

Verus is by the Verus contributors (https://github.com/verus-lang/verus). blaze's parser for
Verus's syntax follows verus-analyzer's grammar (https://github.com/verus-lang/verus-analyzer,
commit `2ac834c8158e0e0969b0e03afe2f27127d6ea9f8`, MIT OR Apache-2.0).
