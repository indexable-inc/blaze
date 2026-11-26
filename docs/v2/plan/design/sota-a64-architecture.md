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
   - **`mfunc.rs`**: Flat bump-allocated SoA SSA `MFunc<'a>` with typed AArch64 `MOp` (including `CarryAddSub` for `adc`/`adcs`/`sbc`/`sbcs`), `VReg(u32)` (`Gpr32`/`Gpr64`/`Fp32`/`Fp64`/`Vec128`/`Nzcv`), explicit `VFlag`, distinct `PReg::Sp` vs `PReg::Xzr` (excluding `X18` on macOS, enabling `X29`+`X30` in 28-GPR zero-call leaf frames), `MNote` reloc/trap/unwind/jump-table attachments on `MInst` IDs, fallthrough-safe backward-branch-target 16-byte loop alignment, structural verifier `mverify(&MFunc)`, and 1-pass `mencode` with `Branch14`/`Branch19` range relaxation for >32 KiB functions.
   - **`misel.rs`**: Direct LIR-to-SSA `MFunc` selector achieving **100% V3 SSA coverage on `blaze-zstd`** (`0` legacy `isel.rs` fallbacks; `99.81%` binary-wide) — supporting scalars, native `V128` NEON ALU (`Add`/`Sub`/`Mul`/`And`/`Or`/`Xor`/`Not`/`Min`/`Max`/saturating/`Fadd`/`Fsub`/`Fmul`), native `Ty::I128` scalar-pair lowering (`Iconst`/`IconstWide`/`Add`/`Sub`/`Neg`/`Mul`/`And`/`Or`/`Xor`/`Not`/`Shl`/`Lshr`/`Ashr`/`Icmp`/`Select`/`Switch`/`Zext`/`Sext`/`Trunc`/`Store`/`Param` phis, with Ext-TSP non-RPO forward pre-allocation) alongside `Q128` bulk loads/stores/splats, `Sret` and `ParamKind::Byval` (incoming + caller-side private copy), `Sat`/`Overflow`/`Extract`, `DataAddr`/`FuncAddr`, `Switch`, `Prfm`, shift-mask elision, `live_cmp` reuse, `Br(Icmp)` / `TstImm+BCond` fusion, multi-use `ScaledIdx`/`BaseOff` load+store folding, `CallIndirect` (`Blr`), `Invoke`/`InvokeIndirect` + `Cleanup`/`Catch`/`Terminate`/`Resume` (`MNote::Unwind`), `3..=8` direct return values (`X0..X7`/`V0..V7`), and `InternalFast` (`X0..X7, X9..X15` / `V0..V7, V16..V23`, keeping `X8` for `Sret`).
   - **`mopt.rs`**: Pre-RA and post-RA SSA/machine optimizations — global entry-hoisted multi-inst `MovImm` CSE, whole-function SSA cross-block wide-`Mov` copy elimination, dead narrow-32 `mov w` elision, 32-bit copy propagation, shift-into-ALU fusion (including narrow 32-bit `ZeroUpper32` `LSR` into 64-bit ALU), `uxt` into `AddExt`/`SubExt`, `neg+add -> sub`, `lsr+mvn -> mvn lsr`, `cset+add -> cinc`, `CmpImm #0 + BCond -> Cbz/Cbnz`, `MovImm(0|1) + Csel -> Csel/Csinc(..., Xzr)`, post-RA exact 31-GPR liveness + const/copy/uxtb folding + cross-block `MovImm` elision, `B8`/`H16`/`W32`/`X64`/`Q128` post-index addressing + widened `Stp`/`Ldp` pairing (`POST_IDX_WINDOW = 64`), post-RA `add/sub+cbz -> subs/adds+b.eq/ne` (Apple Silicon 1-uop macro-op fusion), post-RA `[SP, #off]` spill reload forwarding, 12-op tail merging, hot loop-header relocation, back-edge trampoline inversion, post-`Ret` copy-trampoline hoisting, unwind-safe unreachable pruning, and short edge-split co-location.
   - **`msched.rs`**: Pre-RA and post-RA scheduler with `WINDOW_MAX = 128` (`u128` bitmasks), exact loop-carried recurrence height from CFG backedges, pre-RA pointer-step deferral until unscheduled stores issue (unlocking in-place post-index `[base], #step`), Apple M5 / Graviton 5 issue pipes, two-level SSA register-pressure spill-prevention guard (`threshold = 18`, `critical = 21`), and bounded Beam-4 optimal hot-loop scheduling.
   - **`mra.rs`**: SSA register allocator with 28-GPR leaf pool (`X29`+`X30`), `ParallelCopy`/phi coalescing, hot-vs-cold eviction tiering, remat cost gating (keeping multi-inst constants out of hot loops), and cycle-safe `ParallelCopy` lowering.
   - **`mfmt.rs`**: Clean textual `MFunc` printer and instruction-mix summary for fast codegen inspection alongside local Mach-O `N_SECT` function symbols (`blaze-export`).
   - **Legacy deletion**: the legacy u32-disassembling `mach_a64` pass (`-9,523` lines) was deleted cleanly.

4. **Whole-Program Ephemeral Zero-ABI & PGO (`export_lto.rs` + `abi.rs` + `ifconv.rs` + `sota/mod.rs`)**:
   - **Incremental-cache invariant (Rules 21, 60)**: Portable `CodeStore` records remain strictly item-grain with standard C ABI. Whole-program `InternalFast` (15 GPR / 16 FP args, guarded by `can_use_internal_fast_a64(..., InternalFastRole::{Caller, Callee})` with zero silent C-ABI fallback), `ColdPreserveAll` (`nounwind` cold callees), `ScratchOrder`, bottom-up Tarjan SCC IPRA, and 2-tier hot/cold emission run strictly inside `blaze export`'s ephemeral scratch frame.
   - `extern "C"`, `#[no_mangle]`, address-taken, and unwinding callees remain strictly `CAbi`.
   - `BPG1`/`BPG2` branch entropy drives `ifconv` and Ext-TSP hot/cold layout.

## Verified Results (Apple M5 Max, 1 P-Core, `MB/s/core`)

- **Correctness**: 256/256 `blaze-emit` `sota::` tests + `blaze-lir` `simplify::` + `blaze-int` + `blaze-zstd` tests; **0** legacy `isel.rs` fallbacks across all of `blaze-zstd` (`99.81%` V3 pure-SSA coverage binary-wide); 100% byte-identical output (`cmp`) to `rustc` on 1× `corpus.rs` (`4,847,844 B`); bit-exact RFC 8878 `zstd -tq` on 4× (`19,391,376 B`).
- **Hot-leaf instruction counts (`blaze` vs `rustc`)**: `lz77_scan_fast_leaf` `384` vs `467` (`-83`), `lz77_scan_default_leaf` `884` (`0` post-`ret` trampolines) vs `997` (`-113`), `fse_pack_sequences_leaf` `180` vs `208` (`-28`), `decode_exec_leaf_single<1>` `338` vs `352` (`-14`), `HuffTable::build_from_weights_inner` `876` (V3 SSA, down from `970` legacy).
- **21-pass interleaved P-core medians (`MB/s/core`, 1 core, unmodified `corpus.rs` vs `rustc_w29`)**:
  - `Fast (-1)`: **`792.60`** (top-5 `805.30`) vs `rustc` `787.40` (top-5 `799.70`) (**`1.0066×`**)
  - `Default (-3)`: **`564.60`** (top-5 `579.10`, max `584.00`, up from `v4base` `557.80` / top-5 `572.10`) vs `rustc` `588.70` (top-5 `600.90`, `0.959×`–`0.964×`)
  - `Decompress`: **`3,121.80`–`3,164.50`** vs `rustc` `3,157.50` (`0.989×`–`1.002×`)
  - `user` CPU: `0.37s` (top-5 `0.36s`) vs `0.36s` (top-5 `0.35s`, `0.973×`)
