---
status: plan
summary: blaze v2: per-function codegen.
date: 2026-10-29
---
# blaze v2: codegen

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 9. Codegen and linking

- **Per-function codegen units**: the unit of codegen is a function, not a crate. The
  `blaze_engine` tail (one 12,000-line crate compiling alone at the end) disappears.
- Dev: Cranelift per function. Release: LLVM per function with ThinLTO-style summaries, inlining
  recorded as deep-hash edges (section 6.1). Release code must run as fast as rustc's: the
  summaries carry what cross-function passes need (inlining, constant propagation through
  arguments, dead-argument removal, devirtualization), and a release benchmark against rustc's
  output is one of the budgets.
- Code dedupe never merges statics or anything whose address the program can compare as
  distinct.

## Issues this lane takes in

- **#1291 back-end memo**: instances equal up to callee renaming compile once (anim: 7,587 fns
  plus 70,330 instances). Keyed exactly: constants, layout facts and symbols stay in the key.
  **#1292**: functions that differ only in constants fold into one with the constants as
  parameters (dev builds; release only if run time holds). (`v2-codegen`)
- **#1336**: per-function keys across revisions; Astral's version gained within noise on narrow
  function classes. v2's item hashes generalize it; measure on their benchmark. (`v2-codegen`)
- **#1404, #635**: dev code speed. Find where blaze's dev ripgrep loses 5.8x (stack frames,
  #1307's smaller Cranelift frames, overflow checks, missed inlining, calls through patch
  slots) and fix it in codegen; tests built at the profile's opt-level. **#807**: tiering,
  Cranelift first and LLVM in the background, instead of O2 on every dependency function.
  (`v2-codegen`)

## Done when

- one unit per function or batch; instances keyed by code hash and type-argument facts
- the back-end memo and constant folding; dedupe never merges statics
- dev code at least as fast as cargo debug on ripgrep; release with summaries
- statics, unwind and line tables emitted (link's request)
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
