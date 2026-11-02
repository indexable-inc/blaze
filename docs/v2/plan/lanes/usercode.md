---
status: plan
summary: blaze v2: build scripts, proc macros and tests.
date: 2026-10-29
---
# blaze v2: usercode

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## Build scripts, proc macros, tests (from section 6)

### 6.5 Build scripts, proc macros, tests

- **Traced inputs**: the sandbox records every file, directory listing and environment
  variable a build script or proc macro reads. Its result is keyed by exactly those (plus its own
  code hash), not by "the package changed".
- **Proc macros**: expansion keyed by (macro's code hash, input tokens, traced inputs);
  wasm proc macros where the crate allows, native in the sandbox otherwise.
- **Tests**: each test's reached functions (by code hash) are recorded; a change reruns only
  tests that reach a changed hash.

## Issues this lane takes in

- **#1450**: ruff's 75 s body edit is one function recompiled plus 35 s of build scripts rerun,
  because a build script with no `rerun-if` directive reruns on any change to its package
  (cargo's rule). Traced inputs (section 6.5) fix it at the root: a script reruns only when
  something it read changed. A script that reads what can't be traced (the clock, randomness,
  the network) keeps cargo's rule. (`v2-usercode`)
- **#1325, #1326, #805**: deleted `include_str!` inputs not invalidated, literal whitespace
  changing fingerprints, a worker keyed by `PATH`. Each becomes an oracle edit kind, and the
  environment becomes a traced input, not part of a worker's identity. (`v2-proof`, `v2-usercode`)
- **#1480**: two store inputs not hashed (unreadable sources skipped, the C compiler of build
  scripts). Traced inputs hash both. (`v2-usercode`, `v2-store`)
- **#1296 build-script elision**: scripts that are provably only constant `println!`s of cargo
  directives are evaluated without compiling or running them, checked against running them.
  **#804**: build scripts are compiled by the daemon, not by fresh processes reloading the
  sysroot (2 s each). (`v2-usercode`)
- *(Superseded by change 20 in `plan/design/compile-time.md`: proc macros run as their own
  code on our compiler, so no hand-written expanders.)* **#1297 native expanders** for the macros every graph uses (serde_derive, thiserror,
  bitflags, tokio): used only when the macro's source matches the verified copy byte for byte,
  otherwise the normal expansion runs (an exact-match cache, not a fallback); a verify mode runs
  both and compares tokens and spans. (`v2-usercode`)
- **#1443 a process per test from a fork server**: each test binary starts once in its sandbox,
  forks a copy-on-write child per test (0.1-0.5 ms on Linux), one global queue over all cores.
  **#227, #1330**: run only the tests an edit reached. (`v2-usercode`)

## Done when

- build scripts and proc macros run sandboxed with every read traced, keyed by those reads
- inert build scripts evaluated; native expanders behind byte-exact source match
- tests one process each from a fork server, selected by reached code hashes
- the Windows sandbox
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
