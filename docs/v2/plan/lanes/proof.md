---
status: plan
summary: blaze v2: proving it: oracle, differential corpus, determinism, budgets.
date: 2026-10-29
---
# blaze v2: proof

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 11. Proving it

Testing happens **all at once, on the integrated branch**, not per lane. Lanes write their
code and the tests for it, and keep `blaze-v2` building; they don't run suites. The coordinator
and `v2-proof` run everything together once the lanes' work is in:

- **Differential against rustc** (`harness/toolchain` nightly): the existing harnesses, plus a
  corpus of real crates (top crates.io, blaze, nushell, ripgrep, helix) built and run both ways,
  outputs and test results compared.
- **Invalidation oracle**: for random edit sequences (fuzzed: body, signature, impl, rename,
  move, format, cfg, feature), the incremental result must equal a from-scratch build of the same
  sources, bit for bit for the codegen inputs. Over-invalidation is measured and budgeted;
  under-invalidation is a failure.
- **Rejections**: rustc's compile-fail tests must fail under blaze with the same error codes (#1522, #1440).
- **Simulation** of the core and the store over many seeds (crashes, torn writes, reordered
  completions); the daemon image and journal must recover.
- **Memory**: builds under a tight budget must finish, slower, never OOM.
- **Performance budgets** (section 1) as required checks, measured on the VM, including release
  code speed against rustc's.
- **Determinism**: every corpus build twice, on different core counts, byte-identical.
- **Platforms**: Linux (x86-64, aarch64), macOS (aarch64), Windows (x86-64, MSVC), wasm, and
  cross-compilation; debuggers (gdb, lldb) reading the debug-info side tables.

## Issues this lane takes in

- **#906**: a stale sibling closure after a capture change gives silent wrong results. Closure
  captures are a fact of their own (section 6.1), and this repro goes into the oracle's corpus.
  (`v2-items`, `v2-proof`)
- **#1522, #1440**: blaze accepts code rustc rejects. The differential corpus compares
  rejections too: every compile-fail test of rustc's suite must fail under blaze with the same
  error code. (`v2-proof`)
- **#1325, #1326, #805**: deleted `include_str!` inputs not invalidated, literal whitespace
  changing fingerprints, a worker keyed by `PATH`. Each becomes an oracle edit kind, and the
  environment becomes a traced input, not part of a worker's identity. (`v2-proof`, `v2-usercode`)

Since 2026-10-30 nothing here compares with rustc or cargo (Andrew): the oracle holds the daemon
to blaze's own build from nothing, the corpus judges each crate's tests by their own verdicts,
`rejections.py` is deleted and its fixed cases for #1522 and #1440 are front-end cases
(`harness/v2/frontend/cases/rejects/`) that state the errors they expect.

## Where it lives

The scripts are in `harness/v2/`, one entry for all of them (`proof.py`), and what each
checks is in [the harnesses](../../../harness.md#the-v2-proof). `crates/blaze-v2-proof` holds the
part that reads v2's own record: the query trace compared to name the stale query and the read
it missed, reruns for nothing counted, SIMD byte comparison, and the round-trip and in-place
checks of every column encoding.

## Done when

- the oracle, corpus with rejections, determinism, tight memory and budgets run against v1 today through the same entry points v2 will use
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
