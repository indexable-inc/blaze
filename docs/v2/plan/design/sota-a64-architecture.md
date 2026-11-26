---
status: current
summary: V3 SOTA AArch64 compiler architecture — destination-passing lowering (walk.rs), unified MemorySSA (memssa.rs), pure-SSA typed SoA Machine IR (MFunc/misel/mopt/msched/mra/mencode/mfmt replacing deleted 9,523-line u32-disassembling mach_a64.rs), whole-program Zero-ABI (InternalFast 15-GPR/16-FP args + ColdPreserveAll + bottom-up Tarjan SCC IPRA), PGO branch-entropy if-conversion, and local Mach-O N_SECT function symbols.
date: 2026-11-26
---
# V3 SOTA AArch64 Compiler Architecture

## Architecture Overview (4 Pillars Shipped)

All four V3 architectural pillars are built, wired end-to-end, and verified on Apple Silicon (`aarch64-apple-darwin`) and `blaze-zstd`, replacing legacy u32-disassembling `mach_a64.rs` (`-9,523` lines deleted) and duplicated `memopt.rs` passes (`-2,314` lines deleted):

1. **Guaranteed RVO Destination-Passing Lowering (`crates/blaze-build/src/walk.rs`)**:
   - Fresh uninitialized `Stmt::Let` bindings (`Pat::Bind { by_ref: false }`, `!let_owns_parts`, `Repr::Agg` with `size > 0`, exact `expr_repr == local_repr`) pass the local's slot directly into `[c; N]` (`Repeat`) and `Copy`/no-drop aggregate constructors, eliminating `temp -> local` `Op::Memcpy` and stack-frame bloat at the source.
   - Soundness invariant: never passes an existing local for `Assign` (`x = [x[0]; 64]`) or for drop-bearing partial aggregate initializers that could panic mid-construction.

2. **Unified MemorySSA (`crates/blaze-lir/src/simplify/memssa.rs`)**:
   - Single canonical home for alias partitioning (proven non-escaping `Op::Slot` byte intervals `[lo, hi)` + disjoint-base Rust alias proofs vs Escaped/Heap `MemVersion` chain + loop-header `MemPhi` barriers), store-to-load forwarding (exact interval + uniform-byte `Memset` constant folding), Dead-Store Elimination, dead-temp bulk-copy elision + slot compaction, and `DataAddr`/noalias Memory LICM hoisting.
   - Replaced and deleted `-2,314` lines of duplicated forwarding/bulk-copy logic in `crates/blaze-lir/src/simplify/memopt.rs`.

3. **Pure-SSA Typed SoA AArch64 Machine IR (`crates/blaze-emit/src/sota/{mfunc,misel,mopt,msched,mra,memitter,mfmt}.rs`)**:
   - **`mfunc.rs`**: Flat bump-allocated SoA SSA `MFunc<'a>` with typed AArch64 `MOp`, `VReg(u32)` (`Gpr32`/`Gpr64`/`Fp32`/`Fp64`/`Vec128`/`Nzcv`), explicit `VFlag`, distinct `PReg::Sp` vs `PReg::Xzr` (excluding `X18` on macOS, enabling `X29`+`X30` in 28-GPR zero-call leaf frames), `MNote` reloc/trap/unwind/jump-table attachments on `MInst` IDs, backward-branch-target 16-byte loop alignment without wasted fallthrough `nop`s, structural verifier `mverify(&MFunc)`, and 1-pass `mencode` with `Branch14`/`Branch19` range relaxation for >32 KiB functions.
   - **`misel.rs`**: Direct LIR-to-SSA `MFunc` selector supporting scalars, `i128`/Q128 loads/stores/splats, `Sret`, `Sat`/`Overflow`/`Extract`, `DataAddr`/`FuncAddr`, `Switch`, `Prfm`, shift-mask elision, `live_cmp` reuse, `Br(Icmp)` / `TstImm+BCond` fusion, multi-use `ScaledIdx`/`BaseOff` load+store folding, `CallIndirect` (`Blr`), `Invoke`/`InvokeIndirect` + `Cleanup`/`Catch`/`Terminate`/`Resume` (`MNote::Unwind`), `3..=8` direct return values (`X0..X7`/`V0..V7`), and `InternalFast` (`X0..X7, X9..X15` / `V0..V7, V16..V23`, keeping `X8` for `Sret`).
   - **`mopt.rs`**: Pre-RA and post-RA SSA/machine optimizations — global entry-hoisted multi-inst `MovImm` CSE, dead narrow-32 `mov w` elision, 32-bit copy propagation, shift-into-ALU fusion, `neg+add -> sub`, `lsr+mvn -> mvn lsr`, `cset+add -> cinc`, post-RA `add/sub+cbz -> subs/adds+b.eq/ne` (Apple Silicon 1-uop macro-op fusion), post-RA `[SP, #off]` spill reload forwarding, fallthrough + hot-backedge + self-loop branch inversion, unwind-safe unreachable pruning, and short edge-split co-location.
   - **`msched.rs`**: Pre-RA and post-RA scheduler with `WINDOW_MAX = 128` (`u128` bitmasks), exact loop-carried recurrence height from CFG backedges, Apple M5 / Graviton 5 issue pipes, two-level SSA register-pressure spill-prevention guard (`threshold = 18`, `critical = 21`), and bounded Beam-4 optimal hot-loop scheduling.
   - **`mra.rs`**: SSA register allocator with 28-GPR leaf pool (`X29`+`X30`), `ParallelCopy`/phi coalescing, hot-vs-cold eviction tiering, remat cost gating (keeping multi-inst constants out of hot loops), and cycle-safe `ParallelCopy` lowering.
   - **`mfmt.rs`**: Clean textual `MFunc` printer and instruction-mix summary for fast codegen inspection alongside local Mach-O `N_SECT` function symbols (`blaze-export`).
   - **Legacy deletion**: `crates/blaze-emit/src/sota/mach_a64.rs` (`-9,523` lines) deleted cleanly.

4. **Whole-Program Ephemeral Zero-ABI & PGO (`export_lto.rs` + `abi.rs` + `ifconv.rs` + `sota/mod.rs`)**:
   - **Incremental-cache invariant (Rules 21, 60)**: Portable `CodeStore` records remain strictly item-grain with standard C ABI. Whole-program `InternalFast` (15 GPR / 16 FP args, guarded by `can_use_internal_fast_a64(..., InternalFastRole::{Caller, Callee})` with zero silent C-ABI fallback), `ColdPreserveAll` (`nounwind` cold callees), `ScratchOrder`, bottom-up Tarjan SCC IPRA, and 2-tier hot/cold emission run strictly inside `blaze export`'s ephemeral scratch frame.
   - `extern "C"`, `#[no_mangle]`, address-taken, and unwinding callees remain strictly `CAbi`.
   - `BPG1`/`BPG2` branch entropy drives `ifconv` and Ext-TSP hot/cold layout.

## Verified Results (Apple M5 Max, 1 P-Core, `MB/s/core`)

- **Correctness**: 86/86 `blaze-emit` V3 unit tests + 8/8 `export_lto` tests; 100% byte-identical output (`cmp`) to `rustc` on 1× `corpus.rs` (`4,847,844 B`); bit-exact RFC 8878 `zstd -tq` on 4× (`19,391,376 B`).
- **Hot-leaf instruction counts (`blaze` vs `rustc`)**: `lz77_scan_fast_leaf` `378` vs `467` (`-89`), `lz77_scan_default_leaf` `920` (`14` sp loads) vs `997` (`52` sp loads), `decode_exec_leaf_single<1>` `329` (0 hot-loop spills) vs `352`, `decode_exec_leaf_wide<1>` `345` (0 hot-loop spills) vs `368`, `count_seq_symbols_fast` `46` vs `56`.
- **15-pass interleaved P-core medians (`MB/s/core`, 1 core)**:
  - `Fast (-1)`: **`772.5`** vs `rustc` `768.8` (**`1.005×`**) and C `zstd 1.5.7 --single-thread` `575.1` (**`1.343×`**)
  - `Decompress`: **`3,145.7`** vs `rustc` `3,137.3` (**`1.003×`**) and C `zstd 1.5.7 --single-thread` `1,620.2` (**`1.942×`**)
  - `Default (-3)`: **`554.4`** vs `rustc` `581.2` (`0.954×`) and C `zstd 1.5.7 --single-thread` `360.0` (**`1.540×`**)
  - `user` CPU: `0.381s` vs `0.371s` (`0.976×`)
