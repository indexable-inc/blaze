# Const eval against rustc's tests/ui/consts and const-generics (#862)

`harness/consteval_ui.py` on linux x64 (GitHub's ubuntu-24.04), with rustc's tests at the pinned
nightly's commit (nightly-2026-10-27, 16d1ffac17ad). Each revision of a test is a unit; one is in
scope when rustc's verdict on its constants can be compared (docs/harness.md).

| | evaluation, units the same as rustc | run-pass values |
|---|---|---|
| before (main at 88ceec28eb) | **710 of 767** (92.6%) | 0 of 223 |
| after #929, #933, #936, #938 | **720 of 767** (93.9%) | 0 of 223 |
| main at 0c0ca43feb, with #943 and #961 | **727 of 767** (94.8%) | **222 of 222** (100%) |

1729 units in 1595 tests; 962 are out of scope, each with a reason (`before.md`, `after.md`,
`main.md`, the end). No file made blaze panic.

In the first two runs the run-pass values are 0, because `uitest.py` built no test then:
`blaze has no answer to rustc --print link-args -C save-temps ...`. That was fixed on main by
#927. The train holds both numbers of the last row (`min_evaluation`, `min_run_pass`).

## What the fixes were

The classes of the first run, largest first, and what came of them:

- 392 items, "rustc fails where blaze evaluates", overflow in a constant (`attempt to compute`,
  shifts, negation). Most were the harness: compiletest does not optimize, so its tests run with
  debug assertions and overflow checks, and the first run gave blaze neither (`checks_of` now
  does). Under them, blaze still skipped the checks of a constant's own body in a crate built
  without `-C overflow-checks`, where rustc always checks them: #929 (a wrong value in a
  `--release` build).
- 40 items, `unchecked_shl`/`unchecked_shr` with an amount past the width (const-int-unchecked.rs),
  and `exact_div` with a remainder: the shim wrapped. #933 makes every `unchecked_*` and
  `exact_div` undefined where rustc says so, which also fixes two wrong values
  (`unchecked_div(-128i8, -1)`, `exact_div(7, 2)`).
- 14 items in 11 units, the `gca!` built-in macro (const-generics/gca, mgca): #938.
- 6 items, the `variant_count` intrinsic: #936.
- 8 items in 6 units, a static's initializer writing to a static (another, or itself): #943.
- consts/recursive.rs, endless recursion in a const fn. On main it took the train's runner down
  (one process at 15 GB): calls were not counted against a frame limit, so evaluation ran until
  #912's step budget of 10^9 was spent. #961 limits frames to the crate's `recursion_limit`, as
  rustc does.

## What is left

`main.md` lists every unit that differs. The largest classes are rustc's undefined-behavior
checks that blaze's evaluator does not make: pointer arithmetic out of an allocation
(`offset`, `offset_from`), reading part of a pointer, writing through a shared reference,
invalid values (an enum's tag, a `bool`, an uninhabited type). Then generic const items (`const ADD1<const N: usize>`), which the gca tests use.

## Again

Train runs record it (`consteval` job of `blaze-platform.yaml`, linux x64, artifact
`consteval-linux-x64`), failing under `min_evaluation`. By hand, on a VM with rustc's tests and a
built blaze:

    python3 harness/consteval_ui.py '{"rust": "<rust checkout>", "bin": "target/release/blaze-engine", "out": "tools/bench/results/<date>-consteval"}'

Runs: before https://github.com/indexable-inc/blaze/actions/runs/40665753306, after
https://github.com/indexable-inc/blaze/actions/runs/40665753384 (the harness's branch and that
branch with the four fixes; the after run's unit tests found the gca test's fixture lacking
`len`, fixed in #938). Main: https://github.com/indexable-inc/blaze/actions/runs/40682227369.
