# The merge gate built and tested with blaze (2026-10-28)

Lane cibl. [#896](https://github.com/indexable-inc/blaze/pull/896) made the merge gate (`ci` of
`blaze.yaml`, linux x64) build and test blaze with blaze, from blaze's R2 store.
[#1264](https://github.com/indexable-inc/blaze/pull/1264) made main push that store and pinned the
gate's blaze in `.github/blaze-version`. [#1253](https://github.com/indexable-inc/blaze/pull/1253)
removed rust-cache from the remaining workflows. This measures what `ci` takes now against what it
took with cargo, sccache and rust-cache.

## Method

GitHub-hosted `ubuntu-24.04` (x86_64, 4 cores, 16 GiB, plus 20 GiB of swap from
`.github/actions/blaze`). blaze v0.20261028.5 (`.github/blaze-version`).

- **Before**: every green `pull_request` run of `blaze.yaml` on 2026-10-28 before #896 merged
  (12:03Z), cargo + sccache + rust-cache. Wall time is from the run's start to its last update.
  Runs under 150 s are docs-only changes that build nothing (12 of 99), and are left out.
  All 99 are in `before-runs.tsv` (run id, branch, start, end).
- **After, warm**: three draft PRs opened at 15:03Z, after main's run
  [40701530720](https://github.com/indexable-inc/blaze/actions/runs/40701530720) on 3e08247fea had
  pushed the linux x64 build store (638 MB, 36.7 s) and tests store (856 MB, 66.7 s):
  - no-op ([#1275](https://github.com/indexable-inc/blaze/pull/1275)): a comment line at the end
    of `crates/blaze-engine/src/lib.rs`;
  - one function ([#1276](https://github.com/indexable-inc/blaze/pull/1276)):
    `std::hint::black_box(..)` around the body of blaze-manifest's `Dependency::is_transitive`;
  - a front-end crate ([#1277](https://github.com/indexable-inc/blaze/pull/1277), what was vendor/):
    the same around one arm of hir-ty's `target_feature_is_safe_in_target`.

  None of the three shifts a line. Main had moved 10 commits past 3e08247fea, including edits in
  blaze-cargo and blaze-ui, and a PR's run builds its merge with main. So each run also rebuilt
  those, as any real PR does when main moves.
- **After, cold**: the gate's own PR runs before main had pushed anything for the release.

Job and step times for every run are in `runs.json` (from `gh run view --json jobs`).

## Numbers

`ci` wall time, s:

| | ci | build job | tests job |
|---|---|---|---|
| before, cargo + sccache + rust-cache: median of 87 | **557** (p25 477, p75 620, min 193, max 931) | | |
| after, warm, no-op | **1916** | 675 | 1894 |
| after, warm, one function (blaze-manifest) | **1848** | 938 | 1830 |
| after, warm, one function (hir-ty) | **1826** | 808 | 1812 |
| after, cold (#896's last run) | 3609 | 2162 | 3544 |
| after, cold (#1264) | 2989 | 2332 | 2944 |

The gate is about 3.3 times slower warm than it was with cargo, and 5 to 6.5 times slower cold.
It misses the 1 to 2 minute target by more than an order of magnitude.

### Where the warm time goes (no-op run)

Build job (675 s): toolchain 35 s, blaze install + swap + pull 55 s (pull: about 2.8 GB in 40-55 s across the three runs),
then `blaze build` 547 s:

| step of `blaze build -p blaze-engine --release --features llvm` | time |
|---|---|
| Loading workspace (a fresh runner: no analysis kept) | 2m08s |
| Resolving names | 17.8 s |
| `blaze` (src/bin/blaze.rs): 362,081 fns compiled, **16,477 kept** | 2m57s |
| checking 1137 files | 44.5 s |
| `blaze_engine`: 68,766 fns compiled, 789,152 kept | 1m51s |
| links | 7.8 s, 13.5 s, ... |

Tests job (1894 s): pull 0.91 GB in 13.5 s, then `blaze test` "Finished" at 1690 s, and about
2 min running the tests (`BLAZE_TESTS=affected`: the 3237 that run every time plus the affected
ones; the rest are taken as passed from main's store). Of the 1690 s:

- Loading workspace twice, 2m48s each;
- the `blaze` bin's tests: 371,041 fns compiled, **0 kept**, in every warm run;
- links of the test programs: `ide_diagnostics` 4m22s, `hir_ide` 2m59s, `blaze_engine` 2m27s,
  `blaze_cargo` 1m18s, `hir_ty_codegen` 1m03s, `hir_def` 58.8 s, while their compile, mostly kept,
  took 7 to 54 s.

The one-function and hir-ty runs look the same; the hir-ty edit compiles more (`hir_ide` 116,312
fns, `ide_diagnostics` 85,443, `blaze_engine` 323,658 in the tests build) but links less.

### What the smoke corpus costs on a blaze-built blaze

The std cases of the smoke corpus, which the gate ran on a rustc-built blaze in 74 s, took 15
(timed out, 17 of 20 cases) and 30 min on the blaze-built one. For example, `threads dev run`
went from 2.7 s to 27.7 s, `hashmap dev test` from 4.2 s to 38.5 s, and a unit-test binary's 533 tests from 5.4 s to 60.1 s.
blaze inlines no function into another
([#899](https://github.com/indexable-inc/blaze/issues/899)). So smoke left the merge gate (main
runs it), and the train's harness and bench artifact stays cargo-built, as release.yaml builds it.

## What would make it fast

These are in blaze, not in the CI wiring:

1. a fresh machine loads blaze's workspace in 2-3 min, even with a store holding everything
   (lane warmstate: persisted analysis, #274; manifest, #41);
2. linking a test program takes minutes when its functions are all kept;
3. the `blaze` bin target's functions are never served from the store (0 of 371,041 kept in the
   tests build, 16,477 of 378,558 in the build);
4. blaze-built code runs 10-20 times slower (#899).

Also, `BLAZE_TESTS=affected` already holds the test running itself to ~2 min.
