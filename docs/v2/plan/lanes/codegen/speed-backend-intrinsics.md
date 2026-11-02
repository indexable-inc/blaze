---
status: plan
summary: How blaze lowers the SIMD intrinsics of ripgrep's hot loops (memchr, aho-corasick's Teddy, regex-automata) to the vector instructions LLVM selects instead of loops over lanes and calls of assembled stubs, what each costs before and after, what Cranelift still cannot say, and how the assembler cache is read without a lock.
date: 2026-10-29
---
# Intrinsics at LLVM's speed: notes of speed-backend-intrinsics

Notes of the `speed-backend-intrinsics` lane of the speed pass, as of 2026-10-29. Code:
`crates/hir-ty-codegen/{wasm_simd,neon,x86,asm}.rs`. Requests: `v2/requests/speed-backend-lower.md`
(the two calls in `mod.rs`) and `v2/requests/speed.md` under speed-backend-intrinsics (per-function
ISA, Cranelift reductions).

## Where the time went

SIMD values live in memory in blaze's code generator (a `#[repr(simd)]` local is a stack slot),
so an intrinsic is lowered as loads of its operands, the work, and a store of the result. Before
this lane the work was one of:

- a loop over lanes in `mod.rs`: a scalar load, op and store per lane, so 16 to 48 instructions
  for `simd_eq` of `u8x16`, `vqtbl1q_u8` (`tbl1`, about 8 per lane: load the index, compare,
  load the table byte, select, store) or `vpmaxq_u8`;
- a call of an out-of-line function assembled from text (`neon::lower`, `x86::lower`,
  `asm::assemble`), with the operands passed through memory and the result read back;
- for the generic `simd_*` intrinsics on native targets, always the loop: the vector path in
  `wasm_simd.rs` ran only for wasm32 with `simd128`.

memchr's and Teddy's inner loops are made of exactly these: `simd_eq`, `simd_or`/`simd_and`,
`simd_bitmask` (movemask), `simd_reduce_max` (`vmaxvq_u8`), shifts by a splat, `simd_shuffle`
(`vextq_u8`, `_mm_alignr_epi8`), `simd_splat`, `vqtbl1q_u8`, `vpmaxq_u8`, and on x86-64
`_mm_shuffle_epi8`.

## What changed

| Path | Before | After | LLVM |
|---|---|---|---|
| `simd_eq`, `and`, `or`, `add`, `select`, `splat`, ... of 16 bytes, AArch64 | 16 lanes, about 3 to 5 each | 2 loads, 1 op, 1 store | 1 op |
| the same on x86-64 (SSE2 ops only) | same loop | 2 loads, 1 op, 1 store | 1 op |
| `simd_bitmask` of `u8x16`, AArch64 / x86-64 | loop of 16 | Cranelift's `vhigh_bits` (NEON sequence / `pmovmskb`) | same |
| `simd_shuffle` rotation (`vextq_u8`), AArch64 | loop of 16 | `ext` | `ext` |
| `simd_reduce_max` of `u8x16` (`vmaxvq_u8`), AArch64 | loop of 16 | 4 × (`ext` + `umax`) + `umov`, 9 | `umaxv` + `fmov`, 2 |
| `vqtbl1q_u8` (`tbl1`) | loop of 16, about 8 each | 2 loads, `tbl`, store | `tbl` |
| `vqtbx1q_u8` (`tbx1`) | loop of 16 | `tbl`, `cmhi`, `bsl` | `tbx` |
| `vpmaxq_u8` (`umaxp`) and the other pairwise | loop of 16 | `uzp1`, `uzp2`, `umax` | `umaxp` |
| `sqadd`/`uqadd`/`sqsub`/`uqsub`, `urhadd`, `sqrdmulh`, `abs`, `uaddlp` | stub call | 1 instruction | 1 |
| x86 `minps`/`maxps`/`minpd`/`maxpd`, packs, `pmaddwd`, `pavgb`/`pavgw` (SSE2, AVX2 per half) | loop over lanes | 1 instruction per 16 bytes | 1 (AVX2: 1 per 32) |
| `asm::assemble` hit | a global `Mutex` and a SHA-256 of the text per call | a thread-local map, no lock, no hash | n/a |

All rows are in the code generator. `mod.rs` calls `wasm_simd_intrinsic` first on every
target, and `llvm_intrinsic` calls `neon_vector` before `neon_lanes` and `x86_vector` right
after. speed-backend-lower applied that on 2026-10-29.

### Generic `simd_*` on every target (`wasm_simd.rs`)

`wasm_simd_intrinsic` now picks a vector lowering per target: `Vectors::Wasm` (as before),
`Vectors::Neon` on AArch64, `Vectors::Sse2` on x86-64. The name is a `#[repr(u8)] enum Op` parsed
once by `Op::of`, so kinds are tag ranges (`is_binary`, `is_unary`, `is_reduce`) rather than more
string matches. `admits(vectors, op, lane, signed)` says which op and lane Cranelift lowers to
vector instructions on that ISA without a libcall. Anything it refuses goes to the loop in
`mod.rs` as before, so no lowering is lost:

- On x86-64 the ISA has SSE2 alone (`target.rs` sets no SSSE3/SSE4/AVX). Cranelift lowers a
  general `shuffle`, `swizzle` and a byte `splat` there to its `X86Pshufb` libcall, which blaze
  does not link, and lowers `imul` of 64-bit lanes, unsigned compares, `min`/`max`, rounding and
  `pcmpgtq` with SSE4 instructions or a libcall. So `Sse2` admits `add`, `sub`, `and`/`or`/`xor`,
  `==`/`!=` up to 32-bit lanes, signed `<`/`>` up to 32 bits, float compares and arithmetic,
  saturating ops of 8 and 16 bits, shifts of 16 bits and wider, `select`, `splat` (built from one
  scalar times the repeating factor, stored 8 bytes at a time), and `bitmask`.
- On AArch64 everything is admitted but `imul` of 64-bit lanes. `simd_reduce_*` of integers is
  a ladder of rotations by 8, 4, 2, 1 bytes (`ext`) combined with the op, then lane 0.
- f16 lanes and shapes that are not 16 bytes take the loop. `simd_shape` checks the layout before
  `simd_lanes`, which would refuse f16 with an error.

### NEON (`neon.rs`)

`neon_vector(name, args, dst)` lowers by an enum `Vector` (2 bytes), parsed once from the
`aarch64.neon.` name and its LLVM vector suffix:

- `tbl1` is `swizzle` (Cranelift's `tbl`); `tbx1` is `swizzle`, `icmp ugt 15` and `bitselect`;
- the pairwise max and min are `shuffle` by the `uzp1`/`uzp2` masks, then `umax` (Cranelift
  selects `uzp1`/`uzp2` for those masks);
- `uaddlp`/`saddlp` are `iadd_pairwise(widen_low, widen_high)`, which Cranelift folds to one
  `uaddlp`;
- the saturating, rounded-mean, rounding doubling multiply and `abs` ops are one instruction
  each.

### x86 (`x86.rs`)

`x86_vector(name, args, dst)` lowers by an enum `Vector` (1 byte). Each 16 bytes goes through
`vector()`, which builds exactly the tree Cranelift's x64 rules match. For example
`bitselect(bitcast(fcmp lt x y)), x, y)` with that operand order is `minps`. The test
`each_vector_is_the_instruction_of_sse2` compiles each name with an SSE2-only x64 ISA and checks
the instruction and that there is no compare-and-blend or call. `packusdw` is SSE4.1's, so there
Cranelift's SSE2 sequence stands in; it gives the same bits.

### Assembler cache (`asm.rs`)

`assemble` used to lock one global `Mutex` and hash the text with SHA-256 on every call. Now a
repeat on a thread is read from a thread-local `FxHashMap<Box<str>, &'static Assembled>`. A
first lookup goes to a shared `Table` keyed by the unchanged SHA-256 key (domain
`"blaze asm 3"`), then the store, then the assembler. `BLAZE_ASSEMBLER` is read once.

`Table` is the append-only table of [concurrency.md](../../design/concurrency.md), written in
safe code:

- open-addressed `OnceLock` slots, each set once, 16 probes per level;
- a full window leads to the next level, of twice the slots, and nothing is rehashed;
- a lookup is acquire loads and allocates nothing;
- entries are never freed, so readers hold `&'static` references.

`INLINE` (the blocks LLVM gets as inline assembly) uses the same table. When two blocks share a
wrapper, a flag on the entry is set and readers see `None`; the published block is never
changed. Readers (`fragment.rs` `asm_blocks`, `llvm/lower.rs`) take `&'static Inline` from
`inline_block`, with no `Arc` clone.

## Bit-identity

Each new lowering is the definition of the instruction LLVM selects, on integers or by IEEE
compare, so the bits are rustc's on every input:

- the tests in `neon.rs` and `wasm_simd.rs` model `uzp` against Cranelift's constants, rotations
  against a reduction over seeded bytes, and the repeating factor of a splat;
- the test in `x86.rs` checks the instruction Cranelift emits.

Nothing reads `std::env` per function or per instruction. The asm cache key and its hash domain
are unchanged, since the hashed bytes are unchanged.

## Unsafe ([unsafe.md](../../design/unsafe.md))

None of the speed above takes `unsafe` Rust. The vector code is what Cranelift emits, and the
lock-free reads of `asm.rs` are a thread-local map and `&'static` values leaked once. Each of
this lane's modules says `#![forbid(unsafe_code)]`. The crate can't say it, because `llvm*.rs`,
`backend.rs`, `fncache/` and `mod.rs` hold unsafe code.

The one parser of bytes from disk is `Assembled::from_bytes`. It runs on a copy the store
checked with CRC32, is bounds-checked, and allocates no more than its input. A stored value that
doesn't parse is now an error naming the key, not a silent reassembly. Its model is `to_bytes`,
checked by a round-trip test over seeded values. It is listed for safety-fuzz in
`v2/requests/safety.md`.

## What Cranelift cannot say yet (requests)

1. **x86 beyond SSE2.** Cranelift's ISA flags are per ISA, and blaze has one per process. So
   `_mm_shuffle_epi8` (`pshufb`, Teddy's inner loop on x86-64), `palignr`, a general shuffle,
   `pminub`, `roundps` and `pcmpgtq` stay loops. The fix is an ISA per set of
   `#[target_feature]`s (in `v2/requests/speed.md`). Then `admits` takes the set, and the
   `pshufb` arm becomes `x86_pshufb`.
2. **Reductions.** `vmaxvq_u8` is 9 instructions against LLVM's 2. Cranelift has no reduce op.
   Vendoring cranelift-codegen and adding `vreduce_{umax,smax,umin,smin,add}` (`umaxv`, ...) is in
   `v2/requests/speed.md`.

## Benchmarks to run (when the speed pass measures)

- **LLVM's instructions per intrinsic.** For each row above, compile a function of the intrinsic
  with the pinned nightly at `-C opt-level=3` for `aarch64-apple-darwin` (NEON) and
  `x86_64-unknown-linux-gnu -C target-feature=+avx2`. Count the instructions between the loads
  and the store. Compare with blaze's Cranelift output of the same function: the vcode, as the
  x86 test reads it.
- **Cranelift's own SIMD lowering.** Cranelift's `filetests/isa/{aarch64,x64}/simd-*.clif` are
  the reference for what one CLIF op becomes. A row whose count is above LLVM's names the
  Cranelift rule or op that is missing.
- **ripgrep end to end.** `rg` over a large tree, with blaze-built ripgrep against rustc-built:
  memchr's `find` and Teddy's `find` dominate. Run in an ix VM per the lane rules.
- **Dispatch.** `Op::of`, `vector_op` and `x86::lower` are `match`es on `&str`. rustc compiles
  them to a length switch, then a compare of a few bytes, so they are already one lookup with no
  allocation. A perfect hash (`phf`) or a jump table on interned names would need the names
  interned by the caller (`mod.rs`). Measure it with a microbenchmark of the dispatch over every
  intrinsic name in core_arch before changing it; at a few nanoseconds per call, against
  microseconds of codegen per function, it is not expected to show.

## Left as is

- `x86::lower` and `neon::lower` build their asm text per call. Only the ops with no Cranelift
  form reach them (AES, MXCSR conversions, the flag tests, crypto), and `asm::assemble` caches
  the result by text, so a repeat costs a `format!` and a thread-local lookup.
- `sve.rs`, `fma.rs`, `avx512.rs`, `f16_f128.rs`: no ripgrep hot path reaches them. AVX-512 needs
  the per-function ISA first.
- wasm: v1 only, as v2 refuses wasm32. Its lowering is unchanged under `Vectors::Wasm`.
