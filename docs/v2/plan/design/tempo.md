---
status: record
summary: How far each of blaze's stages gets on tempo (tempoxyz/tempo at 1200580) and what stops it, from crates/blaze-real's `stages` example and the real `blaze check`; regenerated as stages move.
date: 2026-11-16
---
# blaze on tempo

Input: tempo at commit 1200580, read from an APFS copy; nothing of it is built or run. 52 members,
816 `.rs` files, 409,322 lines, 15.2 MB. Regenerate with
`examplerun.sh blaze-real stages <copy>` (crates/blaze-real/examples/stages.rs); every number
below is from its runs on the working tree over dbae95611d, 18 threads, release, all on
2026-11-16. Rows a to f come from the run before stages g and h existed: 4.23 s, 328 MB max RSS.
The lock row and stages g and h come from the latest run: 17.4 s, 2.5 GB max RSS, most of it the
549 MB of dependency sources held in memory.

| stage | covered | stops it | 1 thread | 18 threads | heap peak |
|---|---|---|---|---|---|
| a. manifests (`Layout`) | 53 manifests, 52 members, 44 libs (1 proc-macro), 12 bins, 124 features, 4 passes | nothing | 4.3 ms | 3.6 ms | 1.6 MB |
| a. Cargo.lock (`blaze_manifest::lock`) | 1348 packages (52 path, 1169 registry, 127 git), 408 KB | nothing (was 33.3 ms: a line count from the file's start per package, fixed by the units lane) | 2.7 ms | — | — |
| b. dependency sources | registry 796 of 1169, git 0 of 127; 22,309 files, 549.4 MB | 373 registry packages not unpacked; git reth 31d05e1 (110), evm2 (11), reth-core (5), invariants-core (1): the fetch lane's | — | 443.6 ms (census) | — |
| c. outline | 37,343 items in 816 files | 0 syntax errors, 0 unhandled forms | 194.7 ms | 16.5 ms | 2.3 MB |
| d. census | 39,109 macro uses; 28 workspace `macro_rules!`; 10 workspace proc macros | see below | 36.2 ms | — | — |
| d. item-position `macro_rules!` | 126 of 126 workspace calls expanded (195 in item position) | 65 calls are dependencies' macros, 4 std's: wait for (b) | 3.0 ms | — | 0.6 MB |
| e. names per crate (blaze-scope's `pipeline` example, each crate alone, no dependencies) | all 52 members walked and filled | 9,103 imports unresolved, every leading segment named in the tallies a dependency (`std` 798, `alloy_primitives` 653, `tempo_contracts` 381, ...; none `crate`, `self` or `super`); 4,770 failures: macro path NotFound 3,209, impl's trait not resolved 754, macro path Undetermined 460, attribute macros not lowered 225 (`#[serde]` 121), no prelude 52. All wait for dependencies' scopes | whole run 0.57 s | — | 36.9 MB RSS |
| f. token lints, all rules | 6,920 findings in 38,159 units | nothing | 70.6 ms | 7.0 ms | 2.3 MB |
| f. one edit | a line added to a fn in tt_2d_pool.rs (360 units) | — | 3 units in 56.7 µs, after 3.8 ms outline + plan | — | — |
| `blaze check` (the binary) | — | panics in 0.27 s: `the allocator refused 192 bytes aligned to 4 for blaze_surface::file_items::Attr` (one 64 MiB request frame holds every outline); sent to the driver lane | — | — | 77 MB RSS |

Row (e) is from `examplerun.sh blaze-scope pipeline <52 member dirs>` on the same copy; the
driver lives only in that example, and a library function (one crate's files and cfg, its
dependencies' finished scopes in; counts and causes out) is asked of main to schedule, so the next
run can give each crate its dependencies.

Floors (rule 67): the lint alone is 70.6 ms against one lex of every byte, 23.5 ms: 3.0x. The pass
with no rule takes 52.5 ms of it, so the overhead is per unit, not in the rules.

## Macros (stage d)

- Built in: 3,696 derives, 21,122 std macro calls, 4,763 attributes rustc reads itself.
- Procedural derives: 830 in 34 kinds. Leading: serde `Deserialize` 201 and `Serialize` 168, plus
  path forms 47 and 46; `thiserror::Error` 59; the workspace's `Storable` 57; clap `Parser` 47+26,
  `Args` 41.
- Attribute macros: 1,524 in 13 kinds. Leading: `tokio::test` 682, `test_case` 532+88,
  `instrument` 103, the workspace's `contract` 59.
- Function-like procedural macros: `sol!` 47 calls, all inline Solidity, none naming a file (26
  through tempo's own `crate::sol!` wrapper). Six workspace calls of `tempo_precompiles_macros`.
- Dependencies' `macro_rules!`: 4,403 calls in 78 kinds, led by eyre's `eyre!`, `ensure!`, `bail!`.
- Not found on disk: 577 attribute uses in 20 kinds (`test_traced` 348) and 381 calls in 45 kinds,
  all in packages that are not unpacked here.

The census went to the procedural-macro lane on 2026-11-16.

## Bodies (stage h): the work list for type-checking tempo

Every fn, const and static body of the 52 crates through the ladder's two calls:
`blaze_lower::lower_body`, then `typeck::to_typeck`. The cfg is this host's (macOS, arm64) with
tests on and features off, and each written type is left to inference, so what is counted is the
body's forms. Of 15,045 bodies, 7,664 are clean (50.9%), 7,334 are stopped, 47 are turned off by
cfg, and 5,362 are stopped only by macro calls. Time: 512.1 ms on 1 thread with the tally, 82.9 ms
on 18. These are the numbers after the expansion lane's rest-pattern fix.

| cause (a body counted under each of its causes) | bodies | first place |
|---|---|---|
| a macro call, not expanded (23,526 calls; `assert_eq!` 8,885, `assert!` 5,324, `vec!` 2,129, `format!` 1,081, `println!` 932; then eyre's `eyre!` 479 and `ensure!` 469) | 6,582 | bin/prover/enclave/src/main.rs:41 |
| `yield` from an `.await`'s desugaring | 1,742 | bin/prover/enclave/src/main.rs:38 |
| a coroutine (`async fn`, async block) | 91 | crates/alloy/src/accounts/p256.rs:211 |
| a slice pattern with `..` (the 89 struct-pattern `..` refusals were a bug, now fixed) | 3 | crates/primitives/src/transaction/tt_signature.rs:614 |
| a const argument (`first_chunk::<4>()`) | 74 | crates/contracts/src/precompiles/tip20.rs:239 |
| a path anchor (`<T as Tr>::f`) | 51 | bin/tempo/src/payload_bench.rs:323 |
| format arguments, destructuring assignment, a lifetime argument, `_ =` | 6, 2, 2, 1 | crates/consensus/src/consensus/block.rs:150 |

Macro calls: std's and dependencies' `macro_rules!` need definitions resolved across crates (the
expansion lane's next piece). The type-checker forms (`.await`, coroutines, const, lifetime and
anchored paths, destructuring assignment, `_ =`) belong to the checker lane.

## Dependencies on disk (stage g)

These are the 796 registry packages on this disk (of 1,169), read in place, each in its own
edition. The manifests are 796 of 796 read by `Layout` (editions: 2015 55, 2018 229, 2021 428,
2024 84) in 125.7 ms.

| step | covered | stops it | time |
|---|---|---|---|
| outline | 22,309 files (18,005 under src/ or build.rs), 11.9 M lines, 549.4 MB; 3,609,161 items | 0 syntax errors in src/ (was 292, all a `\` before CR LF in a string, fixed by one line-break rule in crates/parser/src/line_break.rs; the expander's lexer refuses 0 files, was 8). 37 more are in docs and ui-test fixtures (derivative's doc/, pin-project's tests/ui), which are not Rust or not meant to be. 0 unhandled forms | 8.0 s on 1 thread, 1.0 s on 18; heap 42 MB |
| `macro_rules!` | 5,543 definitions, all prepared; 512,336 outermost calls, 149,492 of the package's own | 140,986 expanded (94.3%), 0 refused. The other 8,506 name two same-named definitions (cfg variants), which needs names. 340 of 474 packages expand every own call. The expander's lexer refused 8 files of the same CRLF class; it now refuses 0 (fixed in `blaze_expand::lex`) | 729.6 ms on 18 |

The outline's CRLF class is in `crates/parser`'s `lexed_str`; it goes to the CRLF lane.

## Lint findings by adoption step (stage f)

| step | findings |
|---|---|
| `rust` (default) | 0 |
| `suggest` | 85 |
| `edition` | 3,513 |
| `strict` | 6,920 |

Per rule under `strict`: format-macro 1,705, clones 1,624, stops 1,509, ambient 1,022, allocs 680,
casts 295, unsafe-comment 37, untyped 28, recursion 20. Why the default is `rust`:
docs/v2/plan/design/lint.md, "Which rules a project gets".
