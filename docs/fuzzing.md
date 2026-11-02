---
status: current
summary: The fuzzers: generated programs and edits, each held to blaze's own build from nothing (a warm daemon, a warm store), what a finding is, and what they do not cover.
---
# The fuzzers

Every other harness runs programs somebody wrote. The fuzzers make programs up (`fuzz/gen.py`)
and edits of them, and hold what blaze builds warm to what blaze builds of the same source from
nothing. None of them compares with rustc or cargo: the differential fuzzer that built each
program with cargo too (`fuzz/fuzz.py`) was deleted on 2026-10-30 with the other comparisons
([harness.md](harness.md)). Its account, with every finding, is in
[reports/FUZZ.md](reports/FUZZ.md), a record.

## Running them

    cd harness
    uv run --frozen python fuzz/store.py '{"blaze": "../target/release/blaze", "profiles": ["dev", "release"]}'   # asm changes, cold and warm store; in CI
    uv run --frozen python fuzz/edits.py smoke '{"profiles": ["dev"]}'                                              # generated edits under one warm daemon; in CI
    uv run --frozen python fuzz/edits.py run '{"seed": "S", "rounds": 10, "steps": 20}'                             # a longer run

Both take the cargo profiles to build, which pick the backend as v2 picks it: opt-level 0 and
1 are Cranelift, the rest LLVM, so `release` needs a blaze built with `--features llvm`
([harness.md](harness.md#under-llvm)). `store.py` takes v2's `blaze` in its input; `edits.py`
the one `BLAZE_BIN` names, through `rebuild.py`. `edits.py` was v1's on v1's daemon (`--socket`,
`sync` and `patch`) and its mixed model; on 2026-10-30 it moved onto `blaze build` in a warm
home, each step held to a build of the same source in an empty home (`rebuild.Project`), and
its `check` and `report` subcommands with it; `fuzz/runs.py` went. Each exits 1 when there is
a finding.

## What a program is

`fuzz/gen.py` writes a program from a seed: `fuzz/prelude.rs` followed by one module per
case. A case is a set of types, traits and helper functions and a `run` function. Everything
a case computes is fed into one 64-bit checksum. `main` runs each case inside
`catch_unwind` and prints one line per case with its name, its checksum and whether it
ended or panicked, and the panic's message.

The source of case N of seed S depends only on S and N, so round N of a seed is the same
program whatever the number of jobs. The generator also combines scenes for async futures,
`dyn Any`, inline assembly and unwinding (`fuzz/scenes.py`), and a few `const fn`s per case
(`fuzz/constfn.py`), each called once as a constant and once through `black_box`, both into
the checksum, so blaze's const evaluator and its compiled code have to agree. The programs
avoid what two correct builds may print differently: arithmetic is wrapping, checked,
saturating or overflowing; no address, NaN bit pattern, sign of a zero or hash order is
observed; every loop is bounded. They are a single crate and use only `std`.

## What counts as a finding

`fuzz/store.py`: one function compiled, then only the text of its `asm!` changed and changed
back, each version built by a fresh v2 daemon whose store (`<home>/v2/store`, with the VFS's
journal and the database's image removed) holds what the versions before it left; and the
smoke programs (`gen.program` on fixed seeds) built in an empty home, into a fresh store and
again from the store that filled. Every build with a store has to run, case by case, as the
same source built in an empty home does. This covers #418, where two `asm!` wrappers with the same Cranelift IR and
different assembly shared one entry of LLVM's store (the assembly is in the key since
`2a3a0a35`).

`fuzz/edits.py`: a generated program built under one warm daemon, then edited a step at a time
(a case's code generated anew, constants, the fields and `repr` of types, one literal, a
function added, re-signed or removed, an earlier step's source again), each step built by
`blaze build` and run case by case against the same source built in an empty home. A finding
is a step whose program prints otherwise, one the daemon refused, or a daemon that died or
panicked; an edit the fresh build turns down is not shown to the daemon. A finding is reduced
to the fewest steps that still show it and written under `found/edits/`, which `check` runs
again.

## From a finding to a fix

1. `store.py` keeps a failure's work directory and its daemons' logs.
2. Reduce it to a test ([harness.md](harness.md#adding-a-test)): a step of the daemon harness
   of that area whose warm build has to give what blaze's build from nothing gives.
3. Fix it and run the gate.

## What they have found

The differential fuzzer found twenty-one problems in whole builds before it was deleted, most
of them miscompiles of drop order; [reports/FUZZ.md](reports/FUZZ.md) lists them and
[results.md](results.md) tabulates the runs. Generated edits against v1's running daemon found
four more problems in the incremental path, all fixed, and a fifth that is open: after some
edits a panic inside `core` names the file by its `/rustc/<commit>/library/...` path instead of
the local one.

## What they do not cover

- A program blaze builds wrong the same way warm and from nothing: since 2026-10-30 that is for
  real crates' own test suites to find, not a fuzzer.
- Generated edits had a few short runs only, all of v1's daemon; `edits.py` on v2's has not
  run yet. It covers rebuilds, not live patches of a running program, which wait on the v2
  API's live-patch request.
- Only Linux x86-64 has been run.
- Single-crate programs using only `std`: nothing about dependencies, features, build
  scripts or proc macros. No threads, macros of their own, SIMD intrinsics, or I/O other than
  the checksum lines.
