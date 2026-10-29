---
status: record
summary: Results: every number with the harness, commit and report it came from.
date: 2026-10-27
---
# Results

Every number here names the harness that printed it, the commit it was measured at, and
the report it was copied from. Nothing is averaged or rounded beyond what the source
printed. Most numbers are from a single run on one machine.

How to read the commit column. The work was done on separate branches that were merged
into one, so the commits differ from section to section. Where a report names the commit
it measured, that commit is given. Where it only says "the head of this branch", the commit
given is the one that last changed the report, marked "report at". Commits are of the
development branch (`blaze-collate` and the branches merged into it), not of the published
tree; `vendor/source.rev` says which development commit a published tree was made from.

Reports are named by file. Those under [reports/](reports/) are published with this
tree. BREAKERS.md and the other `harness/*.md` files are in `harness/`. WINDOWS.md,
MACOS.md, MACOS-ABI2.md, AARCH64.md and BLAZE.md are earlier reports that
[platforms.md](platforms.md) and these documents replace; they are on the development
branch only.

Where two reports give different numbers for the same thing, the later report is used and
the difference is listed under [Disagreements](#disagreements).

To update this file: rerun the harness named in the row, replace the row, and change the
commit. Do not append a second row for the same measurement.

## 1. Checked for these documents

Run on Linux x86-64 (8 CPUs, 31 GB) with Rust 1.99.0 at f191c6e42a, the commit these
documents were written against. blaze was built with `cargo build -p blaze-engine --release`, and
for the LLVM column with `--features llvm` against LLVM 20.1.8.
Source trees and scratch directories were on tmpfs.

The gate, from `harness/` with `BLAZE_BIN` set and `TMPDIR` on tmpfs:

<!-- LOCAL-GATE: replace this table when the gate is rerun. -->

| Harness | Cranelift | LLVM (`BLAZE_OPT=2 BLAZE_EMIT=fast`) |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 822 of 822 functions from blaze, 0 of 893 layouts differ | MATCH, 822 of 822, 0 of 893 layouts differ |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 58 of 58, 7 open, 0 functions skipped | 58 of 58, 7 open, 0 functions skipped |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `worktrees.py` | 62 of 62 | 62 of 62 |
| `keys.py` | 46 of 46, 0 open | 46 of 46, 0 open |
| `fuzz/fuzz.py smoke` | 2 programs, 505 functions compiled, 0 differences, 0 crashes, 0 left to rustc | 2 programs, 505 functions compiled, 0 differences, 0 crashes, 0 left to rustc |
| `debuginfo.py` | all right | all right |
| `backends.py corpus runner cases --bin runner --levels 2f` | does not apply | 11,801 of 11,801 functions compiled by LLVM, none left to Cranelift; the program prints the same as cargo's |

The seven "open" breakers are the five known differences and the two known refusals
listed in [support.md](support.md). Not run here: `buildmodel.py`, `farsplices.py`, the
four store harnesses, `remote.py` and `blaze_cli.py`.

By hand, with `blaze` from that build:

| What | Result |
|---|---|
| New `cargo new` project: `blaze build`, `run`, edit, `run` | first build cargo's; edit served by the daemon; `Finished` in 0.01 s |
| ripgrep 15.1.0: `blaze build` | cargo's, 5.6 s; daemon ready within 10 s |
| ripgrep: one-line edit to `crates/core/main.rs`, `blaze build` | by the daemon, 0.26 s wall; the binary prints the edit |
| ripgrep: `blaze test` | by the daemon; 114 and 329 tests pass, the same as cargo's |
| ripgrep: second git worktree | first build cargo's, 16 s; joined the same worker; edit by the daemon, 0.44 s wall |
| `BLAZE_CACHE` set when the daemon starts | store filled: 1 pack, 26,377 records, 57,153,509 bytes; `blaze-engine cache verify` exit 0 |
| `blaze-engine cache push` and `pull` to a directory remote | 1 pack, 7,744,971 bytes compressed, sent and fetched |
| Published layout: recursive clone, `cargo build -p blaze-engine --release --locked` | builds |

## 2. Corpus and layouts, per platform

`difftest.py corpus --bin runner --crate cases --layouts`. "Functions" is how many
functions of the corpus rustc emitted a symbol for and blaze compiled; the rest would be
left to rustc. "Layouts" is how many types blaze laid out differently from rustc's
`-Zprint-type-sizes`. The LLVM columns are from
`backends.py corpus runner cases --bin runner --levels 2f` and count instances.

| Platform | Functions from blaze, both backends | Layouts that differ | LLVM compiled | Left to Cranelift | Commit | Source |
|---|---|---|---|---|---|---|
| Linux x86-64 | 822 of 822 | 0 of 893 | 11,787 of 11,787 | 0 | 888e87df74, CI run 39057174862 | PLAT-ABI2.md |
| Linux AArch64, native | 825 of 825 | 0 of 903 | 11,813 of 11,813 | 0 | same | PLAT-ABI2.md |
| Windows x86-64 MSVC | 830 of 830 | 0 of 898 | 11,801 of 11,801 | 0 | same | PLAT-ABI2.md |
| macOS arm64 | 828 of 828 | 0 of 914 | 11,879 of 11,879 | 0 | same | PLAT-ABI2.md |
| wasm32-wasip1 (wasmtime) | 957 of 959 | not run | no LLVM | | fdc1c53f15, nightly-2026-10-25, arm64 ix VM | platforms.md |

The rest of the gate on that CI run, on every leg: programs, stress, breakers (43 of 43, 5
open), unwinds, splices, asmedits, includes, worktrees, keys (36 of 36, 0 open),
storekills, storecorrupts, storeshared. The Linux legs add `debuginfo.py`, `storefull.py`
and `buildmodel.py`; AArch64 adds `farsplices.py`. The report does not print the counts of
each harness per leg, only that the legs passed.

The wasm row was measured by hand on 2026-10-27 with wasmtime 49.0.2, along with stress 100 of
100, splices 71 of 71 and breakers 176 of 177 (`procmacro`'s `spelled`, which fails the same way
on the aarch64 host without blaze's proc-macro server). The train runs the corpus, stress and
`allblaze_wasm.py` for wasm32 on every push to main (`harness/wasm32.py`, Linux x86-64). Of
the two functions left to rustc there, `undecided_alias_drop`'s `release_state` (a layout with
a type error) is left on the aarch64 host as well, and `fma_nans::vectors` calls `simd_fma`,
which blaze refuses on wasm. Builds with nothing of rustc's (wasm-host guests, Cloudflare
Workers, blaze-manifest and blaze-resolve) are in [platforms.md](platforms.md), wasm32.

## 3. The gate on Linux x86-64

Counts printed by each harness, Cranelift, on a254756b77 (PLAT-ABI2.md, "Local"). LLVM was
not rerun locally on that commit; the CI legs ran it.

| Harness | Result |
|---|---|
| `difftest.py corpus --layouts` | MATCH, 822 of 822, 0 of 893 layouts |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99 |
| `breakers.py` | 43 of 43, 5 open |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `farsplices.py` | 6 of 6 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 58 of 58 |
| `keys.py` | 36 of 36 |
| `debuginfo.py` | all right |
| `buildmodel.py` | 208 of 208 (CI on `51339e5`) |

`buildmodel.py` on native AArch64: 204 of 204, with four rows that need an x86-64 host not
run (the same CI run).

## 4. rustc's own test suites

Linux x86-64, Rust 1.99.0, the rust-lang/rust checkout at the `1.99.0` tag. Single runs.
Report at b46bc7b8ec, branch `blaze-rustc-suites` off 6d12f21c86. Source: RUSTCSUITES.md.
Harnesses: `uitest.py` and `libsuite.py`.

| Suite | Backend | Tests run | Matching | Differing | Left out | Functions from blaze | Left to rustc |
|---|---|---|---|---|---|---|---|
| `tests/ui` run-pass | Cranelift | 2690 | 2686 | 4 | 1114 | 5558 of 5615 | 57 |
| `tests/ui` run-pass | LLVM | 2689 | 2685 | 4 | 1115 | 5554 of 5611 | 57 |
| `coretests` | Cranelift | 1197 | 1197 | 0 | | 1491 of 1499 | 8 |
| `coretests` | LLVM | 1197 | 1197 | 0 | | 1491 of 1499 | 8 |
| `alloctests` | Cranelift | 703 | 703 | 0 | | 964 of 966 | 2 |
| `alloctests` | LLVM | 703 | 703 | 0 | | 964 of 966 | 2 |
| std's tests (28 binaries) | Cranelift | 252 | 252 | 0 | | 380 of 381 | 1 |
| std's tests (28 binaries) | LLVM | 252 | 252 | 0 | | 380 of 381 | 1 |

What these do not cover. Of 3804 run-pass tests (counting revisions), 1114 are left out:
703 need a nightly feature, 76 a `-Z` flag, 73 have no top-level `main`, 48 have an
auxiliary crate that needs nightly, 47 set an optimisation level, and the rest are smaller
groups listed in the report. The library suites are cut down to what builds on stable:
937 of 1443 `#[test]` functions of `coretests`, 643 of 951 of `alloctests`, 259 of 421 of
std's. The unit tests inside `core`, `alloc` and `std` are not run at all. Only Linux
x86-64 was run.

The four differing tests are three causes, all listed as open in
[support.md](support.md): the size of a future (two tests), a value rustc leaks and blaze
drops, and a macro fragment read by a different edition.

## 5. The crate sweep (partial)

This is a sweep of 93 crates chosen by hand, not of crates.io. Seven of the 93 gave no
result (tests that do not build from the published crate, a missing system library, a
timeout). Each crate's own integration tests are built by cargo and again with blaze's
objects linked in front, and every test must report the same.

Harness: `cratesweep.sh` and `sweepreport.py`. Source: harness/BREAKERS.md, "On stable
1.99". Linux x86-64, Cranelift, Rust 1.99.0, one run, at f296858ee3 plus three fixes.

| What | Result |
|---|---|
| Crates with a result | 86 of 93 |
| Crates whose tests behave as rustc's | 86 of 86 |
| Tests run and compared | 10,745 |
| Functions compiled by blaze, as the sweep ran | 41,958 of 41,987 (29 left to rustc) |
| After the fixes, rerun on the four crates they touch | 15 left, all in generic-array |
| tokio with `--features full` (`cratetest.py`) | 155 test binaries, 1018 tests, all matching, 296,088 of 296,088 functions |

The sweep has not been run under LLVM, or on any platform but Linux x86-64.

## 6. Real applications

### Command lines compared (`bintest.py`)

Each program is built by cargo and with blaze's code for its workspace crates, and a list
of command lines is run with both. Source: harness/BREAKERS.md. Linux x86-64, Cranelift,
`nightly-2026-05-10`, at 1c293327c1. Not rerun on 1.99.0.

| Program | Command lines the same | Functions from blaze |
|---|---|---|
| fd | 10 of 10 | 148 of 148 |
| just | 12 of 12 | 822 of 822 |
| hyperfine | 7 of 7 | 161 of 161 |
| zoxide | 9 of 9 | 131 of 131 |
| ripgrep | 12 of 12 | 1820 of 1820 |
| bat | 10 of 10 | 321 of 321 |
| tokei | 8 of 8 | 141 of 141 |

### `blaze` against cargo (`blaze_cli.py`)

Linux x86-64, 8 CPUs, 31 GB, Rust 1.99.0, Cranelift. Report at 13054e50d6 (the head of
the cargo branch with `blaze-collate` b43304222b merged). Source: harness/BLAZE-CLI.md.
Seconds, `blaze` / plain cargo, one run each. `(c)` marks a step cargo built in the
daemon's place. "Differences" counts steps whose output, exit status or test results were
not cargo's.

| Project | Cold build | Warm-up | Warm build | Cold test | Test warm-up | Warm test | Edit build | Edit test | Broken | Mended | Differences |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| ripgrep 15.1.0 | 6.00 / 6.48 | 6.87 | 0.02 / 0.44 | 2.89 / 6.13 | 3.87 | 0.95 / 1.35 | 0.34 / 0.86 | 2.25 (c) / 1.60 | 0.38 (c) / 0.20 | 0.27 / 0.55 | 0 |
| fd 10.3.0 | 21.91 / 21.98 | 0.02 | 0.03 / 0.55 | 5.90 / 5.52 | 3.02 | 0.64 / 1.05 | 0.16 / 0.51 | 0.90 / 1.03 | 0.57 (c) / 0.23 | 0.17 / 0.41 | 0 |
| bat 0.26.0 | 17.47 / 20.90 | 8.50 | 0.02 / 0.08 | 10.68 / 15.38 | 8.51 | 1.68 (c) / 1.46 | 0.30 / 0.53 | 1.95 / 1.33 | 0.38 (c) / 0.26 | 0.09 / 0.50 | 0 |
| hyperfine 1.19.0 | 5.81 / 6.79 | 5.28 | 0.03 / 0.39 | 4.73 / 4.92 | 2.77 | 0.38 / 0.82 | 0.13 / 0.34 | 0.60 / 0.75 | 0.37 (c) / 0.20 | 0.19 / 0.34 | 0 |
| just 1.43.0 | 8.89 / 10.12 | 2.50 | 0.02 / 0.06 | 8.65 / 7.62 | 8.49 | 1.54 / 1.67 | 0.11 / 0.28 | 1.34 / 1.09 | 0.22 (c) / 0.12 | 0.09 / 0.27 | 0 |
| axum 0.8.6 | 9.06 / 9.20 | 3.86 | 0.02 / 0.13 | 11.44 / 10.70 | 2.23 | 0.03 / 0.13 | 0.07 / 0.40 | 0.12 / 0.97 | 0.33 (c) / 0.19 | 0.06 / 0.44 | 0 |
| nushell 0.108.0 | 44.12 / 54.73 | 42.36 | 0.04 / 0.26 | 3.25 / 2.37 | 1.12 | 0.69 / 0.72 | 0.96 / 1.65 | 0.55 / 0.50 | 1.13 (c) / 0.65 | 1.12 / 1.63 | 0 |

How to read it. The cold columns are cargo on both sides; their differences are noise.
"Warm-up" is the time after the cold build until the daemon has loaded the workspace,
during which builds are cargo's. "Broken" is a type error: always cargo's, and slower than
plain cargo by the time it takes to ask the daemon first. Some "edit test" times are
slower than cargo's.

The same run: 66 of 66 flag steps the same as cargo on a generated workspace, and 11 of 11
daemon lifecycle checks.

Not in the table: in ten runs of just, two had one difference each that is not explained
(see [cargo.md](cargo.md), "Known problems").

### Agent-style edit runs (`agents.py`)

Random edits of the kinds coding agents make, against one warm daemon, each step compared
with cargo's build of the same source. Linux x86-64, Cranelift. The files are generated by
the harness and do not record a commit; they were committed at 47dfd2fbb4 and, for
dioxus-cli, 8411413a56. Times include the link.

| Project | Steps right | Restarts | Median step | Daemon RSS at the end | Warm-up | Source |
|---|---|---|---:|---:|---:|---|
| sapp | 60 of 60 | 0 | 84 ms | 694 MB | 2.4 s | harness/AGENTS-sapp.md |
| ripgrep | 80 of 80 | 0 | 327 ms | 1311 MB | 11.1 s | harness/AGENTS-ripgrep.md |
| hyperfine | 80 of 80 | 0 | 214 ms | 2069 MB | 18.9 s | harness/AGENTS-hyperfine.md |
| just | 80 of 80 | 0 | 484 ms | 2083 MB | 16.6 s | harness/AGENTS-just.md |
| dioxus-cli | 60 of 60 | 0 | 3945 ms | 9370 MB | 183.8 s | harness/AGENTS-dioxus-cli-s2.md |

dioxus-cli is the large case: a step takes about four seconds and the daemon holds 9.4 GB.

## 7. The fuzzer

`fuzz/fuzz.py run`, 30 cases per program, both backends. Linux x86-64, Rust 1.99.0,
LLVM 20.1.8. Source: FUZZ.md, report at 8f21942505.

Twenty-one distinct problems were found, most of them miscompiles. Eighteen are fixed, two
(believed to be LLVM bugs) are worked around, and one is open. The table stops at L10; the
later runs, which found findings 17 to 21, are in FUZZ.md.

| Run | blaze at | Programs | Functions generated | Compiled by blaze, each backend | Differences | Left to rustc |
|---|---|---:|---:|---:|---|---|
| L1 | early fixes | 136 | 39,720 | 41,089 Cranelift, 38,984 LLVM | Cranelift 4, LLVM 7 | Cranelift 12 |
| L3 | fixes 1 to 8 | 360 | 105,869 | 102,426 | 5 per backend | 0 |
| L4 | e04c495c9c | 119 | 35,679 | 33,854 | 0 | 0 |
| L5 | a1f0466b5d | 150 | 44,031 | 43,069 | 0 | 0 |
| L6 | a1f0466b5d | 209 | 62,034 | 59,182 | 0 | 0 |
| L7 | 876f964f06 | 360 | 106,202 | 102,898 | 8 per backend | 0 |
| L8 | 49c4fddd1f | 360 | 105,649 | 102,081 | 1 per backend | 0 |
| L9 | 27a94f2770 | 360 | 106,116 | 101,848 | 0 | 0 |
| L10 | 27920a4f69 (after the merge) | 60 | 17,297 | 16,734 | 0 | 0 |

L2 is left out because L3 repeats its seed. No run crashed blaze. Up to L8, about one
program in a few hundred still found something; L9 is the first full-size run that found
nothing. The fuzzer builds whole programs only and has not been run on another platform.

## 8. The disk store

Linux x86-64. Source: DISKCACHE.md, report at 5ca7a332ea.

| Harness | Backend | Result |
|---|---|---|
| `storekills.py` | Cranelift | 400 + 100 + 100 kills, 0 failures |
| `storekills.py` | LLVM | 200 kills, 0 failures |
| `storecorrupts.py` | Cranelift | 10 rounds of damage, 4 old formats discarded, 1 newer refused: pass |
| `storecorrupts.py` | LLVM | 4 rounds and the same formats: pass |
| `storefull.py` | both | 6 amounts of free space, 12 builds, all right |
| `storeshared.py --rounds 12` | Cranelift | 72 builds right; 360 collections, 117 packs evicted, 8734 verifications found nothing |
| `storeshared.py --rounds 12` | LLVM | 72 builds right; 284 collections, 109 packs evicted, 1863 verifications |
| `keys.py` | both | 36 of 36, 0 open (PLAT-ABI2.md, 888e87df74) |

In 800 kills a torn record was left once and a temporary file once; recovery from both is
also covered by unit tests.

Debug info cost, from DEBUGINFO.md (report at efeea650a2), corpus, best of three, compile
and link: Cranelift 5.47 s without and 6.67 s with debug info; LLVM 6.55 s and 7.08 s.

## 9. The remote

Linux x86-64, 8 cores, Cranelift, MinIO over loopback behind a counting proxy. Each
"machine" is its own home directory, checkout, target directory and store on the same
host. One run each. Real AWS was not run. Source: REMOTE.md, report at 90017bf2d5.
Harness: `remote.py --measure`.

ripgrep 14.1.1, 9 crates by blaze, 22,340 functions compiled cold:

| Fresh machine | To a running program | cargo | blaze | Functions compiled | Requests | Bytes down |
|---|---:|---:|---:|---:|---:|---:|
| no remote | 16.8 s | 4.6 s | 12.2 s | 22,340 | 0 | 0 |
| blaze's packs | 14.3 s | 4.8 s | 9.5 s | 0 | 3 | 7,355,315 |
| packs and cargo's units | 9.6 s | 2.0 s | 7.3 s | 0 | 45 | 59,161,263 |
| the same, lazy | 10.0 s | 2.1 s | 7.6 s | 0 | 46 | 59,946,863 |

cargo 0.92.0, 7 crates by blaze, 226,180 functions compiled cold:

| Fresh machine | To a running program | cargo | blaze | Functions compiled | Requests | Bytes down |
|---|---:|---:|---:|---:|---:|---:|
| no remote | 152.4 s | 42.0 s | 110.3 s | 226,180 | 0 | 0 |
| blaze's packs | 117.1 s | 43.4 s | 73.6 s | 0 | 18 | 95,048,041 |
| packs and cargo's units | 94.4 s | 25.2 s | 67.4 s | 0 | 406 | 525,028,848 |
| the same, lazy | 100.2 s | 25.5 s | 73.1 s | 0 | 422 | 532,207,920 |

With every function served from the remote, blaze still takes 7.3 s on ripgrep and 67 s on
cargo. That is loading the workspace and walking every instance to find its key.

The robustness part of `remote.py` (broken transfers, spoiled objects, forged manifests,
collection) passes on MinIO and on SeaweedFS; collection is 12 of 12 on both.

## 10. Edit and warm-up timings

The warm-up and edit times of `blaze` are in section 6. The numbers below are of the
daemon driven directly, which leaves out the client.

### Full link against splice

| Platform | Program | Full link | Splice | What is timed | Source |
|---|---|---:|---:|---|---|
| Windows x86-64 | toy, 7 functions | 98 ms | 9.4 ms | daemon's edit to binary, median of 10 | WINDOWS.md |
| Windows x86-64 | regex-automata | 548 ms | 18 ms | same | WINDOWS.md |
| macOS arm64 | small program | 50 ms | 23 ms | whole request, median of 100 | MACOS.md |
| macOS arm64 | ripgrep 14.1.1 | 104.8 ms | 58.5 ms | daemon's edit to binary, median of 50 | MACOS.md |
| Linux x86-64, AArch64 target | stress app | 131.3 ms | 6.1 ms | `blaze sync`, median of 10 | AARCH64.md |
| Linux x86-64, AArch64 target | ripgrep 14.1.1 | 2018.5 ms | 18.9 ms | same; the full link is GNU ld | AARCH64.md |
| Linux x86-64, wasm32-wasip1 | small std program | 57 ms | 11 ms | edit to a program that can be run | WASM32.md |
| Linux x86-64, wasm32-unknown-unknown | Dioxus hello | 193 ms | 35 ms | same, before 0.27 to 0.36 s of wasm-bindgen | WASM32.md |

These come from the first port to each platform, measured on `nightly-2026-05-10` on
different machines. They are not comparable across rows. There is no Linux x86-64 native
row in the reports for the same measurement.

### What an edit costs the database (`invalidation.py`)

Linux x86-64, the vendored regex-syntax and regex-automata (`harness/bench2`), 17,260
functions and instances. Source: harness/INVALIDATION-regex_syntax+regex_automata+bench2.md,
committed at a1d1676eba. Over its 30 kinds of edit, edit to binary was 498 to 1200 ms
against 250 to 3276 ms for `cargo build`, with 0 to 60 functions recompiled and the
program's output the same as cargo's in every row.

### Generated early and stale

`harness/SCOREBOARD.md` and `harness/BENCH.md` were generated on 12 and 13 October at
58b42c3491 and 923fc86e76, before most of the work above, when the corpus had 152
functions. Their numbers are not repeated here. They need regenerating before they are
quoted.

## 11. Real projects

The newest measurement is in [benchmarks.md](benchmarks.md): `harness/bench.py` on ten
projects (ripgrep to bevy, and zed), cold builds with an empty and a warm store, six kinds of
edit, the test suites, the fastest cargo setups (mold, wild, `-Zthreads=8`, cg_clif, `cargo
check`) and subsecond's hot-patching, every sample kept, and the history the site charts at
/bench. What follows is the first run, with `harness/realbench.py`.

`blaze build` against `cargo build` on three real projects, each at a pinned tag
(issue #7). Harness: `harness/realbench.py`. blaze at 6a7d245e47 (main), Cranelift backend,
release build. Measured on 2026-10-20. Seconds, wall clock of the command; median of 5
(min-max).

Machine: dev-aws-6, AWS c6i.16xlarge, Intel Xeon Platinum 8375C at 2.90 GHz, 64 vCPU,
123 GB, NixOS, Linux 6.18.53, claimed for this run alone. The one-minute load was 7.24 at the
start (blaze's own release build had just finished) and 3.19 at the end; per project,
ripgrep 7.24 to 6.56, just 6.56 to 3.87, nushell 3.87 to 3.19.

Toolchain: rustc 1.99.0 (b940084d7 2026-09-28), dev profile, incremental on, no
`RUSTFLAGS`. Linker on both sides: Rust's default for x86_64-unknown-linux-gnu, `cc` with
rust-lld; `readelf -p .comment` names LLD 23.1.1 in both binaries.

| Project | Step | cargo | blaze | cargo / blaze | blaze built by |
|---|---|---:|---:|---:|---|
| ripgrep 15.1.0 | cold, empty store: command returns | 6.16 (6.13-6.21) | 6.96 (6.90-7.07) | 0.9x | cargo (wrapper) |
| | cold, empty store: daemon ready | | 12.96 (12.92-13.23) | 0.5x | |
| | cold, warm store: command returns | | 6.94 (6.90-7.00) | 0.9x | cargo (wrapper) |
| | cold, warm store: daemon ready | | 12.55 (12.50-12.60) | 0.5x | |
| | no-op | 0.04 (0.04-0.04) | 0.03 (0.03-0.03) | 1.4x | daemon |
| | edit: body of `main` | 0.57 (0.51-0.69) | 0.10 (0.10-1.23) | 5.6x | daemon; output same 5/5 |
| | edit: body in a library | 0.79 (0.77-1.07) | 0.08 (0.07-0.09) | 10.2x | daemon; output same 5/5 |
| | edit: new function | 0.54 (0.51-0.61) | 0.11 (0.11-0.15) | 5.0x | daemon; output same 5/5 |
| just 1.43.0 | cold, empty store: command returns | 8.14 (8.10-8.19) | 8.40 (8.24-8.41) | 1.0x | cargo (wrapper) |
| | cold, empty store: daemon ready | | 16.84 (16.78-16.97) | 0.5x | |
| | cold, warm store: command returns | | 8.32 (8.29-8.38) | 1.0x | cargo (wrapper) |
| | cold, warm store: daemon ready | | 16.10 (16.04-16.19) | 0.5x | |
| | no-op | 0.07 (0.07-0.08) | 0.03 (0.03-0.03) | 2.4x | daemon |
| | edit: body of `main` | 0.29 (0.28-0.32) | 0.05 (0.05-1.44) | 5.7x | daemon; output same 5/5 |
| | edit: body in a library | 0.92 (0.84-1.01) | 0.07 (0.06-0.07) | 14.1x | daemon; output same 5/5 |
| | edit: new function | 0.28 (0.28-0.29) | 0.06 (0.05-0.08) | 5.1x | daemon; output same 5/5 |
| nushell 0.108.0 | cold, empty store: command returns | 35.51 (35.41-35.68) | 37.19 (36.91-37.46) | 1.0x | cargo (wrapper) |
| | cold, empty store: daemon ready | | 75.93 (75.55-79.91) | 0.5x | |
| | cold, warm store: command returns | | 36.56 (36.47-36.84) | 1.0x | cargo (wrapper) |
| | cold, warm store: daemon ready | | 69.93 (69.79-71.24) | 0.5x | |
| | no-op | 0.28 (0.28-0.29) | 0.04 (0.04-0.04) | 8.1x | daemon |
| | edit: body of `main` | 1.55 (1.51-1.68) | 0.42 (0.41-10.41) | 3.7x | daemon; output same 5/5 |
| | edit: body in a library | 6.19 (6.10-7.46) | 0.24 (0.23-0.38) | 26.2x | daemon; output same 5/5 |
| | edit: new function | 1.55 (1.53-1.59) | 0.42 (0.42-0.44) | 3.7x | daemon; output same 5/5 |

All three projects built under blaze; nothing was refused or fell back to cargo after the
first build.

What each row is:

- **Cold.** `rm -rf` of the target directory, then the build. The cargo side is
  `CARGO_TARGET_DIR=t-cargo cargo build`. The blaze side is
  `CARGO_TARGET_DIR=t-blaze BLAZE_HOME=<new> BLAZE_CACHE=<store> blaze build`
  with a new daemon home each run. A first `blaze build` is cargo's own build with
  blaze as the workspace wrapper, so "command returns" is that build. The daemon then loads
  the workspace and compiles every program once; "daemon ready" is when
  `blaze status` says the checkout is ready and the background `cargo check` has
  finished. Edits are fast only after that. "Empty store" is a new, empty `BLAZE_CACHE`
  each run. "Warm store" is one store filled beforehand by `blaze build` of the same
  commit in a second checkout. The three cold kinds were interleaved run by run.
- **No-op.** The build again with nothing changed.
- **Edits.** Each run writes different text, so no run repeats an earlier one, and the
  pristine source is put back at the end. Body of `main`: `let _ = std::hint::black_box(N);`
  at the top of `main` (ripgrep `crates/core/main.rs`, just and nushell `src/main.rs`).
  Body in a library: the same line at the top of a function in a crate the main crate
  depends on: ripgrep `is_suffix` in `crates/matcher/src/lib.rs` (grep-matcher); just
  `Lexer::lex` in `src/lexer.rs` (just is one package, so this is its library target, which
  the binary depends on); nushell `Value::get_type` in `nu-protocol`, which most of the
  workspace depends on. New function: a
  call from `main` to a new `#[inline(never)] fn` added to the same file.
- **Output same.** After every edit both binaries were run with `--version` and one real
  command line, and exit status, stdout and stderr compared: ripgrep
  `rg -n --no-heading --sort path "fn main" crates/core/main.rs build.rs`, just
  `just --list --unsorted`, nushell `nu -n -c "[3 1 2] | sort | str join ','"`.

Why the maxima of "body of `main`" are high: that row's first run is the first edit after
the warm-up. On nushell it recompiled 71,964 of 159,037 functions and wrote the whole object
again ("not spliced" in the worker log), 10.3 s; the second edit spliced with a 1.9 s link,
and from the third on it took 0.41 s. ripgrep's and just's first edits (1.23 s, 1.44 s)
have the same cause at a smaller size.

Why the warm store saves little: most of the warm-up is not code generation. From the
worker logs, empty store against warm store:

| Project | Load workspace | Compile, empty store | Compile, warm store | Functions reused | Link |
|---|---:|---:|---:|---:|---:|
| ripgrep | 0.9 s | 1.8 s | 1.3 s | 13,459 of 17,103 | 0.18 s |
| just | 2.3 s | 2.3 s | 1.6 s | 19,675 of 21,008 | |
| nushell | 4.6 s | 26.1 s | 21.4 s | 124,846 of 159,036 | 1.0 s |

The store holds compiled functions, not rust-analyzer's analysis, so a warm store still
pays for loading and type-checking every function (see [cache.md](cache.md)). The rest of
"ready" minus "command returns" (about 3 s on ripgrep and 7 s on nushell) is starting the
daemon and worker and waiting for the background check.

Commands, on the host:

    RUSTUP_TOOLCHAIN=1.99.0 cargo build -p blaze-engine --release
    BLAZE_PROJECTS=~/blaze-realproj/projects python3 -u harness/realbench.py \
        ripgrep just nushell --runs 5 --work ~/blaze-realproj/work
    python3 harness/realbench.py --table ~/blaze-realproj/work/realbench.json

Dependencies were fetched with `cargo fetch` first and every timed build ran with
`CARGO_NET_OFFLINE=true`.

### Warm edits, before and after (2026-10-20)

The cold rows above are still from 6a7d245e47. The warm rows were measured again on the same
host, dev-aws-6, to compare two blaze builds. Before is 46aff2550e. After is the same code
plus the two commits "where the time of a build goes" and "warm edits". The commits between
46aff2550e and their base on main change only docs and the bench harness.

Method: for each project, before ran and then after. Each got a new work directory, so no
run could hit a store entry left by the other. Each started only when the one-minute load
had been under 2.5 for 30 s. Another lane shared the host at low CPU weight. The load at the
starts was 0.8 to 1.7. Median of 5 (min-max), seconds.

| Project | Step | cargo | before | after |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | edit: body of `main` | 0.53 (0.51-0.61) | 0.11 (0.11-0.22) | 0.11 (0.10-0.22) |
| | edit: body in a library | 0.79 (0.77-1.03) | 0.07 (0.06-0.10) | 0.07 (0.06-0.09) |
| | edit: new function | 0.53 (0.51-0.61) | 0.12 (0.12-0.17) | 0.11 (0.11-0.13) |
| just 1.43.0 | edit: body of `main` | 0.28 (0.28-0.29) | 0.04 (0.04-0.21) | 0.04 (0.04-0.20) |
| | edit: body in a library | 0.83 (0.82-1.00) | 0.06 (0.06-0.10) | 0.06 (0.06-0.09) |
| | edit: new function | 0.28 (0.28-0.29) | 0.05 (0.05-0.09) | 0.06 (0.05-0.08) |
| nushell 0.108.0 | edit: body of `main` | 1.59 (1.52-1.85) | 0.40 (0.40-2.39) | 0.30 (0.30-2.40) |
| | edit: body in a library | 6.33 (6.21-7.85) | 0.21 (0.20-0.44) | 0.18 (0.16-0.21) |
| | edit: new function | 1.64 (1.60-1.65) | 0.41 (0.41-0.52) | 0.34 (0.32-0.38) |

The cargo column is from the after run. Output was the same in 5 of 5 runs on every row, on
both sides.

Where the time goes, after, from `realbench.py --table` (the median run). The steps sum to
the client's total, and "outside the client" is cargo and rustup starting up.

| Project | Step | blaze | Where the time went (ms) |
|---|---|---:|---|
| ripgrep | edit: main | 0.107 | worker wait 38.4, check 17.8, apply 7.1, client checks 2.4, object 2.2, link 1.9, refuse 1.1, 20 more 8.9, outside the client 27.1 |
| | edit: lib | 0.066 | check 13.8, apply 6.8, reach 2.6, client checks 2.5, compile-rest 1.7, link 1.6, refuse 1.4, 20 more 8.1, outside the client 27.0 |
| just | edit: main | 0.043 | check 8.8, apply 1.4, client↔daemon 0.8, link 0.8, build-rest 0.7, 22 more 3.5, outside the client 26.9 |
| | edit: lib | 0.062 | check 17.6, apply 7.2, reach 1.6, link 1.3, compile-rest 1.2, refuse 0.9, 21 more 5.9, outside the client 26.7 |
| nushell | edit: main | 0.305 | check 209.6, apply 14.7, compile-rest 6.8, build-rest 6.8, link 6.4, object 4.9, refuse 4.9, 20 more 23.5, outside the client 27.0 |
| | edit: lib | 0.178 | check 55.4, apply 29.4, compile-rest 9.2, refuse 8.4, reach 8.3, link 7.3, build-rest 6.8, 20 more 25.2, outside the client 27.8 |

The before binary has no spans. The nushell steps before and after come from `BLAZE_STATS=1`
on two builds of the same tree, one without the fixes and one with them. They are medians
of edits 2 to 6, in ms of the client's total. The library edit followed the main edits in the
same daemon.

| Step | main, before | main, after | lib, before | lib, after |
|---|---:|---:|---:|---:|
| check (type-check, translate, Cranelift of the edited functions) | 310 | 207 | 58.5 | 61.2 |
| - of which translate (of which debuginfo types) | 72 (22) | 36 (8) | | |
| - of which Cranelift | 75 | 47 | | |
| compile-rest | 20.7 | 6.7 | 18.7 | 8.6 |
| resolve | | | 22.7 | 2.0 |
| reach | 3.1 | 3.3 | 10.7 | 2.3 |
| apply | 15.9 | 15.0 | 25.6 | 25.8 |
| build-rest, link, refuse, object | 22.1 | 21.2 | 26.4 | 25.0 |
| total | 392.7 | 271.2 | 182.6 | 141.4 |

The lines don't sum to the total. The other steps are under 5 ms each.

What changed:

- Shared paths are cached by the generation of the symbol table, not rebuilt on every edit
  (resolve and reach on a library edit).
- MIR lowering memoizes `is_copy`.
- rust-analyzer's `lookup_impl_method` for trait methods is a salsa query, so an edit
  doesn't solve it again for every call site in functions that did not change.
- What the debugger is told of a type is a salsa query keyed by the environment and the type,
  not built again per thread on every build.
- A function of over 20,000 instructions gets Cranelift's single-pass register allocator
  (`SINGLE_PASS`). nushell's `main`, of 35,780, compiles in 47 ms instead of 75 ms. Its code
  grows from 148 KB to 311 KB.

What is left:

- On nushell's main edit, most of check is rust-analyzer type-checking the edited function
  (about 100 ms). The time is spread over trait solving, and the solver's cache starts empty
  in every revision.
- Cranelift takes 47 ms on `main`, about 35,000 instructions. Translate takes 36 ms, which is
  salsa verification and SSA building.
- Apply takes 13 to 29 ms. Most of it is the outline of each changed file: types, the item
  list, lowering, hashing.
- ripgrep's worker wait (38 ms) is the rerun of build scripts through
  `cargo check --compile-time-deps`. A build script with no `rerun-if-changed` runs again on
  any change in its package, as cargo does. That check could run alongside type-checking,
  and the build could be redone if a build script's output changed.

Commands, on the host. A loop waited for a quiet host and ran this for each project and side:

    BLAZE_BIN=<before or after>/blaze BLAZE_PROJECTS=~/blaze-warm/projects \
        python3 -u harness/realbench.py <project> --runs 5 --parts noop,edits \
        --work ~/blaze-warm/work-<side>
    python3 harness/realbench.py --table ~/blaze-warm/work-<side>/realbench.json

### Edits on macOS, where the time goes (2026-10-20)

Measured on an M5 Max (18 cores, macOS 27), shared with other builds, so single runs spread
widely; the tables are medians of runs 2 to 6 of `blaze build` after realbench's
edits, each step from the client's `BLAZE_TRACE` breakdown. Before is 1dbad5b048, from
before the merges of `fn-incremental` and `tiers-speed`. After is the header pad (d611d65b66,
since merged with the fix of #13, which keeps the size beside the executable and adds a quarter
rather than a half: the same links once it is in place) and then the signature hashes
(f79b93069d) on that same code; the last column is from 12
edits of each kind taken in turn with 12 of before.

| nushell 0.108.0 | before | header pad | and signature hashes | where the time went, at the end (ms) |
|---|---:|---:|---:|---|
| edit: main | 878 ms | 276 ms | 265 ms (before: 897) | check 135 to 168, outside the client 32 to 35, link 21 to 31, apply 9, build-rest 6 |
| edit: lib | 693 ms | 166 ms | 155 ms (before: 747) | link 38 to 42, outside the client 27 to 33, check 27 to 35, apply 12 to 15, refuse 5 to 7 |
| nothing changed | 35 ms | 52 ms | | outside the client 42 (noise: there is no link either way) |

Before, link was 686 and 576 ms of them: every splice was refused with "the header has no
room for the unwind table" (nushell's `__unwind_info` is 3.2 MiB, every Apple link left
1 MiB) and the program was linked in full. Spliced, a link is place 5 to 8 ms, copy 0.2,
overwrite 1.5 (14 to 25 on the library edit, which redirects 189 functions), sign 20 to 23,
append 4 and rename 1 to 3 (`splice-us`). Signing now reuses the hash of each page that the
last splice appended the same: 4.5 to 6 ms. ripgrep's table fits in 1 MiB: its links were
spliced before (8 ms).

Inside check, nushell's edit of `main` (the detail spans of the trace): type inference of
`main` 42 ms, its MIR 3.5, translation to Cranelift IR 14.5, Cranelift 7.3, one after
another on one thread. Inside apply: the outline of the edited file, 5 to 13 ms (listing the
crate's functions by file 2.5 to 3, lowering the file's bodies 2 to 6, the file's types 0.6
for `src/main.rs` and 4.3 for `value/mod.rs`).

ripgrep 15.1.0, before: edit of main 250 ms, of which "worker wait" is 143: the daemon runs
the build scripts again through `cargo check --compile-time-deps`, since ripgrep's `build.rs`
names no file to watch except on Windows, and cargo reruns such a script for any change to
its package. The client has by then run the script itself and compared what it printed and
wrote with cargo's last run.

What is left, by the milliseconds of the Linux numbers above (nushell main 305 ms, ripgrep
main 107 ms):

1. Type inference of the edited function: about 100 ms of nushell's edit of `main` on Linux,
   42 on macOS. The trait solver's cache starts empty in every revision; keeping it across
   revisions is work in rust-analyzer. Lowering only the edited body (`fn-incremental`,
   merged since these were measured) leaves inference of the edited function as it is.
2. The daemon's second run of the build scripts on ripgrep-like projects: 38 ms of 107 on
   Linux. The client already reran the scripts and found them the same as cargo's last run;
   if it sends a digest of what cargo's last run printed and wrote, and the daemon compares
   that with what it loaded, the daemon's run is needed only when they differ. Running the
   daemon's check beside the build instead was tried and dropped: it hides only the build's
   20 to 30 ms, and a script that writes generated code into `OUT_DIR` races the read of it.
3. Translation and Cranelift of the edited function: 36 and 47 ms on Linux, 14.5 and 7.3 on
   macOS. Cranelift is 6.4 times slower on the Linux host where inference is about 2.4 times:
   worth a profile there.
4. Outside the client, 27 ms: rustup's proxy (15 to 20 ms here) and cargo starting. Not
   blaze's code; running the toolchain's cargo directly saves the proxy.
5. The outline in apply, 5 to 13 ms: the functions of a crate by file are listed again on
   every edit, which an edit to bodies alone cannot change (2.5 to 3 ms).
6. refuse, 4 to 9 ms: the list of tests is read again from every function's attributes after
   each build, which an edit to bodies alone cannot change.
7. The macOS splice writes all 57 MB it appends though most is as the last splice wrote it
   (append 4 ms), and placing the patch takes 5 to 8.

### Warm edits on Linux x86-64: where the time goes, and build scripts (2026-10-20)

On dev-aws-11, the machine of the README's run (AWS c8a.metal-24xl, AMD EPYC 9R45, the run
pinned to cores 0-15), with `harness/bench.py` in `nix develop .#bench`, Cranelift, 5 runs.
Before is main cc22006337, after is c8f58ac7d9 (the two commits on build scripts); both are
built as released, without the `llvm` feature. Median ms; the multiplier is cargo's time of the
same run over blaze's.

| Project | Edit | cargo | before | after |
|---|---|---:|---:|---:|
| ripgrep 15.1.0 | a function | 453 | 76 (5.9x) | 50 (9.1x) |
| | a widely used function | 724 | 36 (20x) | 37 (20x) |
| | a signature | 562 | 70 (8.0x) | 68 (8.3x) |
| | nothing | 386 | 18 (21x) | 19 (20x) |
| fd 10.3.0 | a function | 434 | 88 (5.0x) | 43 (10x) |
| | a widely used function | 400 | 74 (5.4x) | 32 (13x) |
| | a signature | 638 | 119 (5.3x) | 71 (9.0x) |
| hyperfine 1.19.0 | a function | 318 | 81 (3.9x) | 34 (9.4x) |
| | a widely used function | 350 | 81 (4.3x) | 34 (10x) |
| | a signature | 355 | 119 (3.0x) | 71 (5.0x) |
| nushell 0.108.0 | a function | 1436 | 258 (5.7x) | 261 (5.5x) |
| | a widely used function | 4805 | 227 (21x) | 231 (21x) |
| | a signature | 4848 | 308 (16x) | 308 (16x) |

bat, just and axum have no build script that runs again for these edits, and moved by 1 ms or
less. cargo's column has a bench bug in it: its target directory was inside the checkout, which
cargo takes for a source of the package, so ripgrep's, fd's and hyperfine's build scripts (no
`rerun-if-changed`) ran again at every build of cargo's. "Nothing" is 30 ms for cargo on
ripgrep with the target directory outside it, not 386; an edit of another crate of these
workspaces had the same run of the script added to cargo's time (CHANGELOG, Fixed). Every program said what cargo's said, in both runs.

Where the time of one edit went before, from `BLAZE_TRACE` (the median of 5 to 7):

| | ripgrep, a function (70 ms) | nushell, a function (252 ms) |
|---|---:|---:|
| cargo starting and handing over to `blaze` and the engine | 14 | 20 |
| the worker waiting for `cargo check --compile-time-deps` | 26 | 1 |
| type-checking, MIR, translating and Cranelift of the edited function | 18 | 149 |
| rust-analyzer taking the change | 4 | 20 |
| object, debug info, and what is reached from the function | 5 | 15 |
| the splice into the executable | 0.3 | 1.9 |
| the rest of client, daemon and worker | 2 | 46 |

- Cargo's part is the same on both sides: Nix's `cargo` is a bash wrapper (1.3 ms), and cargo
  takes about 8 ms before it runs an external subcommand (`cargo --version` takes 1.5 ms), part
  of it reading the 470 KB certificate bundle. `blaze` takes 0.4 ms and `blaze` 0.9 ms to
  start.
- The splice keeps the executable of the last link and appends the edit, so linking is not
  on the way: 0.3 to 2 ms, also for nushell's 1.1 GB executable. A full link happens on the
  first edit after the daemon loads (14 s for nushell, which writes a 457 MB object); bench.py
  keeps it out of the medians.
- The kernel has 8.6% of the processor time of nushell's edit, spread over page faults and
  `stat`; nothing on Linux's side of it is worth more than a millisecond.
- A binary built with `--features llvm` loads libLLVM (198 MB) each time it starts: `blaze
  --version` takes 7.8 ms against 1.4 ms, mostly the dynamic loader binding and relocating it.
  The README's run until now used that binary for the Cranelift column; it now uses the build
  as released (nothing changed in ripgrep: 28 ms with the feature, 19 ms without).

What changed: ripgrep's, fd's and hyperfine's build scripts name no `rerun-if-changed`, so the
worker ran every build script of the workspace through cargo before each edit of their
package. It now runs the one script as cargo ran it and compares it with what it loaded, and
not at all when `blaze` just did; cargo runs them when the script is older than its
source, read a variable while compiled that cargo does not set, or printed or wrote something
else (CHANGELOG, commits 20708cd2da and c8f58ac7d9).

Commands: for each side, `BLAZE_BIN=<side>/blaze taskset -c 0-15 python3 harness/bench.py
<projects> --backend cranelift --runs 5`; the after run measured both tiers (with
`BLAZE_LLVM_BIN` a build with the feature) and is the README's.

||||||| cc22006337
## Disagreements

Where reports disagree, this file follows the later one.

| Subject | Earlier | Later, used here |
|---|---|---|
| Corpus size on Linux x86-64 | 661 of 662 (BREAKERS.md), 679 of 680 (WASM32.md), 803 of 803 (BUILDMODEL.md, CARGO.md, DISKCACHE.md), 808 of 808 (AARCH64.md) | 822 of 822 (PLAT-ABI2.md). The corpus grew, and the one function left to rustc was fixed |
| Corpus on Windows | 827 of 827 (WINDOWS.md) | 830 of 830 (PLAT-ABI2.md) |
| Corpus on macOS | 816 of 816 (MACOS-ABI2.md), 826 of 828 (PLAT-ABI2.md, before a254756b77) | 828 of 828 |
| Corpus on AArch64 | 817 of 817 (AARCH64.md) | 825 of 825 |
| `breakers.py` | 25 of 25, 0 open (BREAKERS.md, BUILDMODEL.md, CARGO.md, DEBUGINFO.md); 43 of 43, 5 open (RUSTCSUITES.md, PLAT-ABI2.md) | 58 of 58, 7 open (section 1). Cases were added from rustc's suites and the fuzzer |
| `keys.py` | 26 of 26, 4 open (AARCH64.md, WINDOWS.md, MACOS-ABI2.md, DISKCACHE.md); 36 of 36, 0 open (CARGO.md, PLAT-ABI2.md) | 46 of 46, 0 open (BUILDMODEL.md, and section 1). The count depends on which branches were merged; none is open at the head |
| `worktrees.py` | 52 of 52 (DEBUGINFO.md), 58 of 58 (PLAT-ABI2.md) | 62 of 62 (BUILDMODEL.md, and section 1) |
| `buildmodel.py` | 97 of 97 per backend (BUILDMODEL.md); 179 of 180 with `cross_aarch64` under LLVM not finishing (CARGO.md) | 180 of 180 (PLAT-ABI2.md) |
| Store platforms | "Linux x86-64 only" (DISKCACHE.md) | store harnesses pass in CI on Windows and macOS too (PLAT-ABI2.md) |
| S3 | "The S3 remote path was not exercised" (DISKCACHE.md) | exercised against MinIO and SeaweedFS (REMOTE.md); real AWS still not run |
| `sym` in `naked_asm!` on Mach-O and COFF | "not read back" (AARCH64.md, WINDOWS.md) | compiled on all four platforms (PLAT-ABI2.md) |
| Aggregates aligned to 16 bytes on the x86-64 stack | open bug (MACOS-ABI2.md) | fixed (PLAT-ABI2.md) |
| Toolchain | `nightly-2026-05-10` (BREAKERS.md first passes, WINDOWS.md, MACOS.md, AARCH64.md first rounds, WASM.md, WASM32.md) | stable 1.99.0 everywhere else |
| Goals | "Hot patching" and optimised code are "not goals"; "What to build next" lists the panic fixtures, the layout check, random programs and CI (BLAZE.md) | all four exist; the splice exists; BLAZE.md is the original plan and is out of date |
| `blaze -q` | listed among the flags handed to cargo (CARGO.md) | not in the list in `cargo/cli.rs`. Checked by hand: the first `-q` build is cargo's because `-q` makes it a different command line, and later ones are the daemon's |
