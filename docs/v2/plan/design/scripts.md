---
status: plan
summary: Python leaves the repository: what was deleted, and Rust scripts run by blaze (`blaze run script.rs`, bstd, typed input, stage 0) replacing the rest.
date: 2026-11-11
---
# Scripts in Rust, run by blaze

Python leaves the repository (Andrew, 2026-11-10). First what no longer runs is deleted; then
every remaining script becomes a single-file Rust script that blaze builds and runs.

## The audit (2026-11-11, at 7bda937633)

200 tracked `.py` files, 72,402 lines. A file is LIVE when something that runs reaches it: a
workflow or composite action step, a ledger check (`docs/v2/plan/objectives.json`), a `current`
doc or a skill that names it, Rust or another tracked file that names it (a generated file's
header, a test, a manifest), or a live Python file that imports or starts it. DEAD is everything
else, including a file only `record` or `superseded` docs or other dead files name: deleted.
UNSURE is kept: named only by a plan, a unit test of a live script that no workflow runs, or
upstream's file.

| directory | live | dead (deleted) | unsure (kept) |
|---|---|---|---|
| .github/scripts | 15 files, 4,088 lines | 0 | 0 |
| crates | 5 files, 880 lines | 0 | 0 |
| demo | 0 | 1 files, 16 lines | 0 |
| docs | 2 files, 355 lines | 0 | 0 |
| harness | 104 files, 33,102 lines | 3 files, 1,846 lines | 5 files, 770 lines |
| harness/v2 | 32 files, 21,946 lines | 0 | 2 files, 1,369 lines |
| library | 0 | 0 | 2 files, 822 lines |
| site | 3 files, 263 lines | 0 | 0 |
| tools | 19 files, 5,025 lines | 4 files, 982 lines | 3 files, 938 lines |
| total | 180 files, 65,659 lines | 8 files, 2,844 lines | 12 files, 3,899 lines |

Deleted with the dead scripts, and beside them what nothing named any more: `harness/soak/` and
`harness/SOAKROWS.md` (soakrows.py's), `tools/similar/` (its own project and lock), and the
orphans of scripts deleted earlier: `harness/CARGO-BLAZE.md`, `harness/CARGO-BLAZE-LLVM.md`,
`harness/INVALIDATION-regex_automata.md`, `harness/benches.json`, `harness/bench2/`,
`harness/rung/`, `harness/sweep1000.txt`. No workflow step was removed: none ran a dead file.

The older harness (`harness/` outside `v2/`) is not dead. Workflows run 30 of its scripts
directly; `harness/ci.py`, which `blaze-platform.yaml` and `focused.yaml` run, reaches 37 more;
14 are imported or started by those. 19 are live only because `docs/harness.md` or another
current doc lists them (coldprof, cpufold, genws, heap, heapfold, hotpatch, leanprof, linkbench,
memsplit, nightly_features, outdirwarn, realbench, sandboxbench, selfrestore, spaces, spare,
testselectbench, tierbench, warmstart): they are the next to delete, each with its row of
`docs/harness.md`, once someone who may run them finds they no longer run.

Other script languages, out of scope and left: TypeScript (`site/` 58 files and `crates/harness`
62, which are products: the site, its worker and the app's UI; `tools/remote-cache` 16, a
Cloudflare Worker; `tools/agents/rules.ts`, run by bun, which moves to a Rust script with the
checkers; `crates/blaze-napi` 2, the Node binding's types and test), JavaScript
(`crates/blaze-doc/static/blaze.js`, `crates/blaze-napi/index.js`,
`tools/bench/remote-cache/index_v1.mjs`) and shell (`site/install.sh`, the installer users pipe
to sh, which stays shell; `harness/benchrunner.sh`, `harness/vendor.sh`, `harness/norustc/wrap.sh`
and three fixtures under `crates/blaze-resolve/tests`, which go with the Python that starts them).
Recommendation: what ships to a browser, a Worker or a user's shell stays in its language; what
is a repository script (`rules.ts`, the harness's `.sh`) follows the Python.

## Appendix: every Python file

| file | lines | verdict | evidence |
|---|---|---|---|
| .github/scripts/bench_commit.py | 186 | LIVE | run by .github/workflows/bench-counts.yaml (+2) |
| .github/scripts/build_release.py | 397 | LIVE | run by .github/workflows/install.yaml (+2) |
| .github/scripts/canary.py | 334 | LIVE | run by .github/workflows/canary.yaml (+1) |
| .github/scripts/checksums.py | 45 | LIVE | run by .github/workflows/release.yaml |
| .github/scripts/downstream.py | 620 | LIVE | run by .github/workflows/downstream.yaml |
| .github/scripts/downstream_build.py | 200 | LIVE | run by .github/workflows/downstream.yaml |
| .github/scripts/explore_page.py | 90 | LIVE | run by .github/workflows/blaze.yaml (+1) |
| .github/scripts/gate_changes.py | 174 | LIVE | run by .github/workflows/blaze.yaml |
| .github/scripts/land.py | 240 | LIVE | named in crates/blaze-ci/src/lib.rs (+1) |
| .github/scripts/llvm.py | 351 | LIVE | run by .github/actions/blaze/action.yml (+3) |
| .github/scripts/promote.py | 489 | LIVE | run by .github/workflows/promote.yaml (+2) |
| .github/scripts/r2.py | 192 | LIVE | run by .github/workflows/llvm.yaml (+3) |
| .github/scripts/toolchain_bump_ci.py | 473 | LIVE | run by .github/workflows/toolchain-bump.yaml |
| .github/scripts/train_issue.py | 144 | LIVE | run by .github/actions/train-issue/action.yml |
| .github/scripts/train_release.py | 153 | LIVE | run by .github/workflows/release-cut.yaml (+2) |
| crates/blaze-diagnostics/src/checks/features/attributes/table.py | 292 | LIVE | named in crates/blaze-diagnostics/src/checks/features/attributes/table.rs |
| crates/blaze-diagnostics/src/lints/syntax/attrs/features/table.py | 140 | LIVE | generates crates/blaze-diagnostics/src/lints/syntax/attrs/features/table/lang.rs (named in its header) |
| crates/harness/harness-native-render/fonts/nerd-symbols/gen_constraints.py | 109 | LIVE | named in crates/harness/harness-native-render/src/text/nerd.rs |
| crates/harness/scripts/cef.py | 260 | LIVE | named in crates/harness/harness-browser/src/location.rs (+4) |
| crates/harness/vendor/hyperion-ecs-sys/gen-bindings.py | 79 | LIVE | named in crates/harness/vendor/hyperion-ecs-sys/src/bindings.rs |
| demo/mklink.py | 16 | DEAD | nothing names it; runs `cargo rustc` |
| docs/brand/fire/gen.py | 280 | LIVE | named in crates/harness/harness-native-blaze/src/mark.rs |
| docs/brand/fire/icon.py | 75 | LIVE | named in site/renderer/core/src/render.rs |
| harness/abbench.py | 194 | LIVE | used by live harness/counters.py |
| harness/anonstatics.py | 161 | LIVE | run by .github/workflows/focused.yaml |
| harness/answers.py | 174 | LIVE | run by .github/workflows/toolchain-bump.yaml |
| harness/appruns.py | 196 | LIVE | used by live harness/bunprep.py |
| harness/artifactedits.py | 265 | LIVE | used by live harness/ci.py |
| harness/bench.py | 1325 | LIVE | run by .github/workflows/bench-bun.yaml (+4) |
| harness/benchcharts.py | 735 | LIVE | run by .github/workflows/bench-counts.yaml (+2) |
| harness/benchhistory.py | 368 | LIVE | run by .github/workflows/bench-counts.yaml (+2) |
| harness/benchplatforms.py | 70 | LIVE | run by .github/workflows/bench-bun.yaml (+2) |
| harness/benchreport.py | 1038 | LIVE | run by .github/workflows/bench-bun.yaml (+1) |
| harness/blaze_cli.py | 1617 | LIVE | run by .github/workflows/blaze-platform.yaml |
| harness/bunprep.py | 376 | LIVE | run by .github/workflows/bench-bun.yaml (+1) |
| harness/ci.py | 468 | LIVE | run by .github/workflows/blaze-platform.yaml (+2) |
| harness/clippy_registry.py | 127 | LIVE | named in crates/blaze-lints/src/registry/clippy.rs (+1) |
| harness/coldbuild.py | 413 | LIVE | used by live harness/ci.py (+1) |
| harness/coldprof.py | 180 | LIVE | current doc docs/harness.md |
| harness/constmacros.py | 92 | UNSURE | no workflow, ledger row or current doc; the plan docs/future/const-macros.md measures with its fixture harness/constmacros |
| harness/counters.py | 491 | LIVE | run by .github/workflows/bench-counts.yaml |
| harness/cpufold.py | 223 | LIVE | current doc docs/harness.md |
| harness/debuginfo.py | 997 | LIVE | run by .github/workflows/blaze-platform.yaml |
| harness/deriveerrors.py | 84 | LIVE | used by live harness/ci.py |
| harness/doctestonce.py | 131 | LIVE | used by live harness/ci.py |
| harness/doctests.py | 128 | LIVE | used by live harness/doctestonce.py |
| harness/dwarf.py | 151 | LIVE | used by live harness/debuginfo.py |
| harness/editmacros.py | 153 | LIVE | named in crates/blaze-engine/tests/repro.rs (+1) |
| harness/experience.py | 588 | DEAD | only docs/CHANGELOG.md (record); times blaze beside cargo |
| harness/failures.py | 1133 | LIVE | run by .github/workflows/blaze.yaml |
| harness/floxprep.py | 87 | LIVE | run by .github/workflows/promote.yaml |
| harness/focused.py | 117 | LIVE | run by .github/workflows/blaze-platform.yaml (+1) |
| harness/frames.py | 177 | DEAD | only docs/tiers.md (superseded) |
| harness/fuzz/constfn.py | 271 | LIVE | used by live harness/fuzz/gen.py |
| harness/fuzz/edits.py | 676 | LIVE | used by live harness/ci.py |
| harness/fuzz/gen.py | 2598 | LIVE | used by live harness/fuzz/edits.py |
| harness/fuzz/scenes.py | 600 | LIVE | used by live harness/fuzz/gen.py |
| harness/fuzz/store.py | 228 | LIVE | used by live harness/ci.py (+1) |
| harness/genws.py | 482 | LIVE | current doc docs/benchmarks.md |
| harness/growth.py | 237 | LIVE | used by live harness/ci.py |
| harness/heap.py | 107 | LIVE | current doc docs/harness.md |
| harness/heapfold.py | 57 | LIVE | current doc docs/harness.md |
| harness/hermetic.py | 260 | LIVE | used by live harness/ci.py |
| harness/hotpatch.py | 182 | LIVE | current doc docs/benchmarks.md (+1) |
| harness/imports.py | 42 | LIVE | run by .github/workflows/blaze.yaml |
| harness/includes.py | 218 | LIVE | used by live harness/ci.py |
| harness/incremental.py | 588 | LIVE | used by live harness/ci.py |
| harness/keys.py | 253 | LIVE | used by live harness/ci.py |
| harness/latelisted.py | 112 | LIVE | used by live harness/ci.py |
| harness/leanprof.py | 111 | LIVE | current doc docs/harness.md |
| harness/library.py | 429 | LIVE | run by .github/workflows/release.yaml |
| harness/libraryfetch.py | 124 | LIVE | used by live harness/ci.py |
| harness/linkbench.py | 90 | LIVE | current doc docs/harness.md |
| harness/live_e2e.py | 331 | LIVE | run by .github/workflows/live-debug.yaml (+1) |
| harness/live_latency.py | 315 | LIVE | run by .github/workflows/live-latency.yaml |
| harness/live_types.py | 226 | LIVE | used by live harness/ci.py |
| harness/loadretry.py | 97 | LIVE | used by live harness/ci.py |
| harness/machoimage.py | 230 | LIVE | used by live harness/ci.py |
| harness/memory.py | 413 | LIVE | used by live harness/bench.py (+4) |
| harness/memsplit.py | 127 | LIVE | current doc docs/harness.md |
| harness/nativespans.py | 83 | LIVE | used by live harness/ci.py |
| harness/nightly_features.py | 896 | LIVE | current doc docs/harness.md (+1) |
| harness/norustc.py | 258 | LIVE | used by live harness/blaze_cli.py (+1) |
| harness/norustc/classify.py | 171 | UNSURE | only count.py and setup.py name it (see setup.py) |
| harness/norustc/count.py | 172 | UNSURE | only setup.py, classify.py and wrap.sh name it (see setup.py) |
| harness/norustc/setup.py | 204 | UNSURE | only count.py, wrap.sh and pyproject.toml's lint list name it; ported to typed input recently; measures rule no-rustc on big projects by hand |
| harness/outdirwarn.py | 119 | LIVE | current doc docs/harness.md |
| harness/own_inputs.py | 218 | LIVE | used by live harness/ci.py |
| harness/pfmdiff.py | 131 | UNSURE | only the plan docs/future/gpu/shaders.md runs it |
| harness/profile_fixtures.py | 269 | LIVE | used by live harness/profile_gate.py |
| harness/profile_gate.py | 282 | LIVE | run by .github/workflows/blaze-platform.yaml |
| harness/profile_trend.py | 221 | LIVE | run by .github/workflows/profile-nightly.yaml |
| harness/programs.py | 97 | LIVE | used by live harness/ci.py |
| harness/reachmacros.py | 178 | LIVE | used by live harness/editmacros.py |
| harness/realbench.py | 403 | LIVE | current doc docs/harness.md |
| harness/realdebug.py | 271 | LIVE | used by live harness/dwarf.py |
| harness/rebuild.py | 193 | LIVE | used by live harness/ci.py (+6) |
| harness/rival.py | 554 | LIVE | run by .github/workflows/rival.yaml |
| harness/rustcsites.py | 130 | LIVE | run by .github/workflows/blaze.yaml |
| harness/rustsrc.py | 53 | LIVE | used by live harness/ci.py |
| harness/sandboxbench.py | 289 | LIVE | current doc docs/cargo.md (+1) |
| harness/scriptenv.py | 126 | LIVE | used by live harness/ci.py |
| harness/selects.py | 119 | LIVE | used by live harness/ci.py |
| harness/selfrestore.py | 179 | LIVE | current doc docs/harness.md |
| harness/servoprep.py | 36 | LIVE | used by live harness/bench.py (+1) |
| harness/smoke.py | 454 | LIVE | run by .github/workflows/blaze-platform.yaml (+2) |
| harness/soakrows.py | 1081 | DEAD | only docs/CHANGELOG.md (record) and its own report harness/SOAKROWS.md; with it harness/soak/ and SOAKROWS.md |
| harness/spaces.py | 125 | LIVE | current doc docs/harness.md |
| harness/spare.py | 138 | LIVE | current doc docs/harness.md |
| harness/staticcallbacks.py | 71 | LIVE | used by live harness/ci.py |
| harness/storecorrupts.py | 220 | LIVE | used by live harness/ci.py |
| harness/storefull.py | 249 | LIVE | used by live harness/ci.py |
| harness/storeguards.py | 157 | LIVE | used by live harness/ci.py |
| harness/storekills.py | 223 | LIVE | used by live harness/ci.py |
| harness/storelib.py | 1089 | LIVE | used by live .github/scripts/gate_changes.py (+29) |
| harness/storerevisions.py | 394 | LIVE | used by live harness/ci.py |
| harness/storetracked.py | 207 | LIVE | used by live harness/ci.py (+1) |
| harness/stress-corpus/gen.py | 305 | LIVE | named in harness/stress-corpus/big-file/src/items.rs (+3) |
| harness/stress.py | 301 | LIVE | used by live harness/ci.py (+1) |
| harness/testselect.py | 535 | LIVE | run by .github/workflows/focused.yaml |
| harness/testselectbench.py | 176 | LIVE | current doc docs/harness.md |
| harness/tierbench.py | 167 | LIVE | current doc docs/harness.md |
| harness/tiers.py | 253 | LIVE | run by .github/workflows/nix-live.yaml |
| harness/toolchain_bump.py | 423 | LIVE | run by .github/workflows/toolchain-bump.yaml |
| harness/toolchain_pins.py | 127 | LIVE | used by live harness/blaze_cli.py (+1) |
| harness/unittests.py | 346 | LIVE | run by .github/workflows/blaze-platform.yaml (+1) |
| harness/unreached.py | 97 | LIVE | run by .github/workflows/focused.yaml |
| harness/unwinds.py | 59 | LIVE | used by live harness/ci.py (+1) |
| harness/unwindsplice.py | 127 | LIVE | used by live harness/ci.py |
| harness/v2/bounds.py | 166 | LIVE | used by live harness/v2/budgets.py |
| harness/v2/budgets.py | 2238 | LIVE | run by .github/workflows/blaze-platform.yaml (+1) |
| harness/v2/changed.py | 316 | LIVE | run by .github/workflows/blaze.yaml |
| harness/v2/clone_out.py | 344 | UNSURE | only the plan docs/v2/plan/design/clone-out.md names it (its check) |
| harness/v2/constspec.py | 540 | LIVE | named in harness/v2/constspec/modes.rs |
| harness/v2/corpus.py | 876 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/dbsp/edits.py | 1774 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/debug_side.py | 357 | LIVE | current doc docs/harness.md |
| harness/v2/determinism.py | 298 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/diff_rules.py | 715 | LIVE | run by .github/workflows/blaze.yaml |
| harness/v2/engine.py | 871 | LIVE | used by live .github/scripts/gate_changes.py (+14) |
| harness/v2/fixture.py | 1053 | LIVE | run by .github/workflows/blaze-platform.yaml |
| harness/v2/frontend/borrowck_flow.py | 348 | LIVE | named in crates/hir-ty-borrowck/src/flow_tests.rs |
| harness/v2/frontend/cases.py | 1199 | LIVE | used by live harness/v2/dbsp/edits.py |
| harness/v2/frontend/item_store.py | 1025 | UNSURE | only the plan docs/v2/plan/design/item-hash.md names it (its check) |
| harness/v2/guards.py | 646 | LIVE | run by .github/workflows/blaze-platform.yaml (+1) |
| harness/v2/incraudit.py | 1171 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/invariants.py | 289 | LIVE | run by .github/workflows/blaze.yaml |
| harness/v2/lean_differential.py | 205 | LIVE | named in lean/Blaze/Tests/Fnv1aMain.lean (+2) |
| harness/v2/lean_gate.py | 478 | LIVE | run by .github/workflows/blaze.yaml |
| harness/v2/ledger.py | 325 | LIVE | run by .github/workflows/blaze.yaml |
| harness/v2/macroeval_corpus.py | 534 | LIVE | used by live harness/v2/proof.py |
| harness/v2/mir_semantics.py | 451 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/oracle.py | 800 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/pins.py | 78 | LIVE | run by .github/workflows/realprojects.yaml |
| harness/v2/proof.py | 468 | LIVE | used by live harness/v2/report.py |
| harness/v2/pure_scripts.py | 756 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/reachable.py | 739 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/realprojects.py | 1687 | LIVE | run by .github/workflows/realprojects.yaml |
| harness/v2/report.py | 119 | LIVE | used by live .github/scripts/gate_changes.py (+16) |
| harness/v2/share.py | 294 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/std_uses.py | 565 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/tightmemory.py | 573 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/v2/worktree_keys.py | 1017 | LIVE | a ledger check in docs/v2/plan/objectives.json |
| harness/warm_ci.py | 299 | LIVE | run by .github/workflows/warm.yaml |
| harness/warmstart.py | 122 | LIVE | current doc docs/harness.md |
| harness/wasmcmacro.py | 150 | LIVE | used by live harness/ci.py |
| harness/wasmpairmacro.py | 213 | LIVE | used by live harness/ci.py |
| harness/wholeprogram.py | 147 | LIVE | used by live harness/ci.py |
| harness/worktrees.py | 148 | LIVE | used by live harness/ci.py (+1) |
| library/compiler-builtins/ci/ci-util.py | 525 | UNSURE | upstream's file in the vendored standard library; nothing here runs it |
| library/compiler-builtins/etc/update-api-list.py | 297 | UNSURE | upstream's file in the vendored standard library; nothing here runs it |
| site/flecs/build.py | 55 | LIVE | skill or root doc .agents/skills/flecs/SKILL.md |
| site/renderer/build.py | 98 | LIVE | run by .github/workflows/site.yaml |
| site/renderer/fetch.py | 110 | LIVE | run by .github/workflows/site.yaml |
| tools/astlog/scan.py | 172 | LIVE | run by .github/workflows/blaze.yaml |
| tools/bench/remote-cache/cas_v1.py | 222 | LIVE | named in tools/remote-cache/README.md |
| tools/bench/remote-cache/gateway_v0.py | 170 | LIVE | named in tools/remote-cache/README.md |
| tools/cpus.py | 241 | LIVE | run by .github/workflows/toolchain-bump.yaml |
| tools/devirt/bench.py | 220 | LIVE | named in tools/devirt/dyn_loop/src/main.rs |
| tools/docs.py | 259 | LIVE | run by .github/workflows/blaze.yaml |
| tools/layout/bench.py | 325 | UNSURE | only the plan docs/v2/plan/design/code-layout.md names it |
| tools/nix/life_launch.py | 132 | LIVE | named in crates/blaze-live/Cargo.toml (+1) |
| tools/nix/unit_build.py | 175 | LIVE | used by live tools/nix/unit_graph.py |
| tools/nix/unit_graph.py | 248 | LIVE | run by .github/workflows/nix-live.yaml |
| tools/pgo/pgo.py | 992 | LIVE | run by .github/workflows/pgo.yaml (+1) |
| tools/pgo/test_pgo.py | 386 | UNSURE | unit tests of the live tools/pgo/pgo.py; no workflow runs them |
| tools/remote-cache/smoke.py | 133 | LIVE | named in tools/remote-cache/README.md |
| tools/rename.py | 252 | DEAD | only docs/rename.md (record); the ra-jit rename of 2026-10-28, done |
| tools/similar/src/similar/__init__.py | 509 | DEAD | nothing names it but pyproject.toml's exclude and lint list; with it tools/similar/'s pyproject.toml and uv.lock |
| tools/typed-input/src/typed_input/__init__.py | 111 | LIVE | used by live .github/scripts/build_release.py (+56) |
| tools/unicode/casefold.py | 51 | LIVE | named in crates/blaze-fetch/src/casefold.rs |
| tools/v2/ast_views.py | 363 | LIVE | named in crates/syntax/src/view/generated.rs (+3) |
| tools/v2/domains.py | 134 | LIVE | run by .github/workflows/blaze.yaml |
| tools/v2/hot_lint.py | 247 | LIVE | named in crates/blaze-lints/src/blaze.rs (+1) |
| tools/v2/manifest_lint.py | 117 | DEAD | only its own unit test |
| tools/v2/repro/repro.py | 369 | LIVE | named in crates/blaze-engine/tests/repro.rs |
| tools/v2/solver_sync.py | 284 | LIVE | used by live harness/toolchain_bump.py |
| tools/v2/test_manifest_lint.py | 104 | DEAD | nothing runs it |
| tools/v2/test_solver_sync.py | 227 | UNSURE | unit tests of the live tools/v2/solver_sync.py; no workflow runs them |
| tools/vendor/sync.py | 502 | LIVE | named in Cargo.toml |
