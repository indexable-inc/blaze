# rustc's MIR optimization passes

Fetched 2026-10-20 from `https://raw.githubusercontent.com/rust-lang/rust/<rev>/<path>`.

- `1.99.0/`: rev `b940084d7eb6a299eb4bfeb8e34901bc051e7ac4`, the 1.99.0 release, which is the toolchain
  blaze pins (`rust-toolchain.toml`). Paths under `compiler/rustc_mir_transform/src/`, except
  `session-excerpt.rs`: three functions cut from `compiler/rustc_session/src/session.rs`
  (`emit_lifetime_markers`, `mir_opt_level`, `ub_checks`).
- `master/`: `main` on the same day (19833af722), `lib.rs` and `tail_copy_to_move.rs` only.
- Added 2026-10-21 at the same 1.99.0 rev, for drop elaboration
  (`vendor/rust-analyzer/crates/hir-ty/src/mir/transform/{elaborate_drops,move_paths}.rs`):
  `1.99.0/elaborate_drops.rs` and `1.99.0/elaborate_drop.rs` (the pass and the drop ladders it
  builds, `compiler/rustc_mir_transform/src/`), and under `1.99.0/dataflow/`, from
  `compiler/rustc_mir_dataflow/src/`: `initialized.rs` (`impls/initialized.rs`, the maybe-init
  and maybe-uninit analyses with their switch-edge and dead-unwind rules),
  `drop_flag_effects.rs`, `move_paths_builder.rs` (`move_paths/builder.rs`) and
  `move_paths_mod.rs` (`move_paths/mod.rs`).
- Added 2026-10-23 from the `1.99.0` tag, for what the borrow checker takes to be promoted in a
  `const` or `static` item (`vendor/rust-analyzer/crates/hir-ty-borrowck/src/loans.rs`):
  `1.99.0/promote_consts.rs`. `validate_ref` promotes a mutable borrow only of an empty array;
  `validate_call` promotes a `const fn` call in an item only in `promotion_safe_blocks`, the
  blocks reached from the start through gotos, calls, drops and asserts.

What `1.99.0/` holds: `lib.rs` (the pipeline, `run_optimization_passes`), `pass_manager.rs`
(how a pass is turned on, validated and dumped) and the source of every pass blaze ports
(`vendor/rust-analyzer/crates/hir-ty/src/mir/optimize/`), plus `ssa.rs`, which copy propagation
is built on, and `prettify.rs` (block and local reordering, which blaze doesn't port).

## The pipeline at 1.99.0 (read, `lib.rs` `run_optimization_passes`)

The level comes from `-Zmir-opt-level`, by default 1 at `-Copt-level=0` and 2 above it
(`session-excerpt.rs`). Each pass says when it runs in `fn policy`; the pipeline lifts a few to
level 1 or more with `o1(..)`. "Always" means at every level, 0 included. In order:

| Pass | Runs | blaze |
| --- | --- | --- |
| CheckAlignment, CheckNull, CheckEnums | when UB checks are on (`debug_assertions` by default) | not ported |
| LowerSliceLenCalls | 1 and up | ported |
| InstSimplify (BeforeInline) | 1 and up | ported, partly |
| ForceInline | always | not ported |
| Inline | 2 with `-Copt-level` 2 or 3 and no incremental; 3 and up | not ported |
| RemoveStorageMarkers | 1 and up, unless LLVM lifetime markers are emitted (any `-Copt-level` above 0) | ported |
| RemoveZsts | 1 and up | not ported |
| RemoveUnneededDrops | always | ported |
| UnreachableEnumBranching | 1 and up | not ported |
| UnreachablePropagation | 2 and up | ported |
| SimplifyCfg (AfterUnreachableEnumBranching) | 1 and up | ported |
| MultipleReturnTerminators | 4 and up | not needed |
| InstSimplify (AfterSimplifyCfg) | 1 and up | ported, partly |
| SimplifyConstCondition (AfterInstSimplify) | 1 and up | ported |
| ReferencePropagation | 2 and up | not ported |
| ScalarReplacementOfAggregates | 2 and up | not ported |
| SimplifyLocals (BeforeConstProp) | always | ported |
| DeadStoreElimination (Initial) | 2 and up | ported |
| GVN | 2 and up | not ported |
| SimplifyLocals (AfterGVN) | always | ported |
| SsaRangePropagation | 2 and up | not ported |
| MatchBranchSimplification | 2 and up | not ported |
| DataflowConstProp | 3 and up | not needed |
| SingleUseConsts | 1 and up | ported |
| SimplifyConstCondition (AfterConstProp) | 1 and up | ported |
| JumpThreading | 2 and up, not on GPU targets | not ported |
| EarlyOtherwiseBranch | 2 and up | not ported |
| SimplifyComparisonIntegral | 2 and up | not ported |
| SimplifyConstCondition (Final) | 1 and up | ported |
| RemoveNoopLandingPads | 1 and up, when panics unwind | ported (blaze had it) |
| SimplifyCfg (Final) | 1 and up | ported |
| StripDebugInfo | with `-Zmir-strip-debuginfo` | not needed |
| CopyProp | 1 and up | ported |
| DeadStoreElimination (Final) | 2 and up | ported |
| DestinationPropagation | 2 and up | not ported |
| SimplifyLocals (Final) | always | ported |
| MultipleReturnTerminators | 4 and up | not needed |
| EnumSizeOpt | 3 and up with `-Zunsound-mir-opts` | not needed |
| CriticalCallEdges | always (required) | ported |
| ReorderBasicBlocks, ReorderLocals | only when enabled by name | not needed |

"Not needed": off at the levels rustc uses by default (1 and 2).

## What changed on master (read, `master/lib.rs`)

- `TailCopyToMove` added after `SimplifyComparisonIntegral`'s position, on at level 2 only with
  the new `-Zmir-move-elimination` flag. Its doc comment says it prepares for `MoveElimination`,
  the pass the 2026 move-elimination goal proposes; `MoveElimination` itself is not in the
  pipeline yet.
- `LargeEnums` (`EnumSizeOpt`) removed from the pipeline.
- `fn policy` takes a `PassCtx` rather than the `Session`.
