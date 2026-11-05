---
status: record
summary: The test suites of 1100 crates.io crates through blaze.
date: 2026-10-20
---
# The test suites of 1100 crates.io crates through blaze

Branch `blaze-sweep-2`: `blaze-collate` at `3d47ed59bf` with `blaze-sweep-1000` (`f4fca15318`)
merged in. Measured on one 8-core, 31 GB Linux x86-64 VM with rustc 1.99.0. This file is
updated with each push; a section that says "in progress" has numbers from a sweep that is
still running.

## Where this stands

Work stopped at this head on request. Done: the merges, the gate below, the fix for the one
real miscompile the sweep had found (tracing-opentelemetry), and a harness that no longer
reports tests that differ from run to run as mismatches. Half done: the sweep. Not started:
the LLVM pass of the sweep.

The gate table below is for `bf8c5edbee`, the merge. The head adds the tracing-opentelemetry
fix (`5af7f465bf`) and the harness change. On `5af7f465bf` the whole Cranelift gate passed
again (20 stages, breakers 130 of 130), and so did the tests of `hir-ty`, `hir-def`,
`parser`, `syntax` and `blaze`. The LLVM gate on it was stopped after its first nine stages
(difftest, programs, stress, breakers 130 of 130, unwinds, splices, asmedits, includes,
worktrees), all of which passed; keys, fuzz, backends, debuginfo and the store harnesses
were not run under LLVM on that commit. `blaze-collate-next` is not merged in.

The sweep was started again from the first crate with the binary of `5af7f465bf` and stopped
at 43 of 1100 crates under Cranelift: 37 match, 6 untested, none differ, 2,475 tests,
14,152 of 14,152 functions from blaze. No crate has been run under LLVM. The larger run
under "Totals" is of an older binary.

What to do next, in order: finish the LLVM gate on the head; run the sweep to the end under
Cranelift and then LLVM (the command is at the end; it skips crates that have a status
file); reduce whatever differs; then the open causes: the text of a failed `assert!` that
rustc breaks over lines (num_enum, proc-macro-crate; it needs rustc's pretty printer for
expressions), the 32 functions left to rustc listed below, and the untested crates that
only need features turned on (tokio).

## The list

`harness/sweep1000.txt` holds 1100 `name-version` lines: the most downloaded crates on
crates.io, each at its newest version, as the crates.io API returned them on 2026-10-17. The
session that started this work did not commit its list, so the numbers it reported at 550
crates (402 match, 27 mismatch, 118 untested) were measured on a list that may differ from
this one in versions and in order.

## The gate at the merge

Every harness below was run under Cranelift and then under LLVM (`BLAZE_OPT=2 BLAZE_EMIT=fast`)
on the merge with `blaze-collate` at `3d47ed59bf` (`bf8c5edbee`), one backend at a time, with the binary of
`LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p blaze-engine --release --features llvm`.

| harness | Cranelift | LLVM |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | match, 0 of 893 layouts differ | match, 0 of 893 |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 129 of 129, 7 open or left to rustc, 0 skipped | 129 of 129, 7 open or left to rustc, 0 skipped |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `BLAZE_SPLICE=1 splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `BLAZE_SPLICE=1 worktrees.py` | 62 of 62 | 62 of 62 |
| `keys.py` | 46 of 46 | 46 of 46 |
| `fuzz/fuzz.py smoke` | 2 programs, 505 functions, 0 differences | 2 programs, 505 functions, 0 differences |
| `backends.py corpus runner cases --bin runner --levels 2f` | (LLVM only) | prints the same as cargo's |
| `debuginfo.py` | all right | all right |
| `buildmodel.py` | 194 of 194 | (runs both) |
| `storekills.py --kills 100` | pass | pass |
| `storecorrupts.py --rounds 4` | pass | pass |
| `storefull.py` | pass | pass |
| `storeshared.py` | pass, 180 builds | pass, 180 builds |
| `storeguards.py` | all right | all right |
| `selects.py` | 3 of 3 | 3 of 3 |
| `blaze_cli.py --small` | pass | (Cranelift leg only) |

`cargo fmt --all -- --check` is clean and `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p blaze-engine` passes.

Not run: `remote.py` (no minio on this machine) and `worktrees.py --join --together` and
`--overlay`. `blaze_cli.py --small` did not finish on the previous push (it sat idle at
ripgrep's cold build and was stopped); it passes on this one, in 196 seconds, and the stall
has not been seen again or explained.

Clippy (`cargo clippy -p blaze-engine --all-targets -- --cap-lints warn`) was compared with
`blaze-collate` at `f191c6e42a` on the previous push: the same 97 kinds of warning, three more
instances, two of which are fixed here. It was not compared again after the later merges.

Where `blaze-collate` and this branch had fixed the same thing, one fix was kept:

| what | kept | case |
|---|---|---|
| An alias that names the lifetime of a binder around it, in monomorphization | `blaze-collate`'s: normalize with placeholders and put the bound lifetimes back | `alias_under_binder` (collate's) and `alias_under_binder_projection` (this branch's, renamed) |
| A variant reexported through a private import of its enum | this branch's: the visibility the enum is declared with, kept by the module scope | both `nameres` tests, `variant_alias_reexported` and `variant_reexported_through_private_enum_import` |
| A tuple struct's constructor as a promotable aggregate | identical in both | `promoted_constructor` |

An unset `env!` variable both records the read for the build key (collate's) and is an
expansion error (this branch's). The file name under a stable name goes through collate's
`stored` (a path under cargo's home is named the same on every machine), inside this branch's
`file_name`.

The open breakers are `raw_write_then_match`, `future_unwind_flags`,
`fragment_edition_through_macro`, `assert_long_expression` and `assert_message`; blaze
prints something else than rustc in each. `caller_in_constant` and `empty_array_by_impl` are
left to rustc with a reason. `unreachable_arm_move` was fixed in `blaze-collate`.

## Totals

In progress, and not a result for this head. What follows is the Cranelift sweep on the
binary of an earlier push (`a9b62fbfce`), stopped at 578 of the 1100 crates for this merge.
It is run again from the first crate on this head, and LLVM after it.

| | Cranelift, 578 of 1100 crates |
|---|---|
| tests print the same as under rustc | 443 |
| differ | 8 |
| untested | 126 |
| timed out at 25 minutes | 1 (base64) |
| tests run and compared | 36,880 |
| functions from blaze | 130,580 of 130,612 |

The 32 functions left to rustc:

| reason | functions | crates |
|---|---|---|
| `UnresolvedMethod`, and two layouts of `GenericArray` (the next-solver cause `harness/BREAKERS.md` has as open) | 15 | generic-array |
| `MissingFunctionDefinition` | 6 | ring |
| `NotSupported` | 4 | encode_unicode, ring |
| LLVM intrinsic `x86.aesni.aeskeygenassist` | 3 | aes |
| calling convention `EfiApi` | 3 | r-efi |
| layout of an array | 1 | wit-bindgen |

The 8 that differed, by cause:

| crate | what differed | cause | state |
|---|---|---|---|
| tracing-opentelemetry | `layer::tests::otel_context_propagation` failed under blaze only: a span id that should be zero was not | statics of one name from a macro reached through an import in a `#[test]` function were numbered in another order than rustc's, so a callsite got the metadata of another | fixed in `5af7f465bf`; cases `late_import_in_body`, `static_in_static` and `blanket/tests/late_import_in_test.rs`. All 50 tests print the same and every function is blaze's |
| jobserver | `make-as-a-client` prints the path it runs from | blaze's binary is named `<rustc's>-blaze` | the harness now reads that name as rustc's; matches |
| num_enum, proc-macro-crate | the text of a failed `assert!` over several lines (the tests run cargo on a manifest the published crate does not have, so they fail under both) | blaze prints the expression on one line, rustc as its pretty printer breaks it | open: breakers `assert_message` and `assert_long_expression` |
| sysinfo | `check_refresh_process_update` fails under rustc and passes under blaze | the test counts processes of the machine; it matches when run again | not blaze |
| hyper-timeout | `test_upload_timeout` | a timeout test; it matches when run again | not blaze |
| openssl | a random MIME boundary and port numbers in what four failing tests print | differs between two runs of rustc's binary | not blaze |
| semver-parser | a test that fails under both and prints how long it took | differs between two runs of rustc's binary | not blaze |

`cratetest.py` now runs rustc's binary three more times when a crate differs. If those runs
do not print the same, the crate is reported as `UNSTABLE` rather than as a mismatch, and
`sweepreport.py` shows it as such. openssl and semver-parser are reported that way now.

Of the 126 untested, 83 have no test binary at all (no tests, or the tests do not build under
cargo either) and 42 have one in which no function of the crate is reached. tokio is the last:
171 test binaries, all left out because they need features that are off by default.

## How to rerun

```
LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p blaze-engine --release --features llvm
cd harness
BLAZE_BIN=$PWD/../target/release/blaze-engine CLEAN=1 JOBS=3 BACKENDS="cranelift llvm" \
    ./cratesweep.sh ~/sweep/out $(cat sweep1000.txt)
./sweepreport.py ~/sweep/out/cranelift
```

The sweep runs every crate under the first backend before it starts the second. A crate with a
`<name>.status` in `~/sweep/out/<backend>` is not run again; delete the file to rerun it.
`cratetest.py vendor/<name>` runs one crate and `--bisect` names the functions that differ.
