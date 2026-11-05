---
status: plan
summary: lane blake3-servil's speed of light: the bound of each of blaze's BLAKE3 shapes, its arithmetic, and where blake3-servil and official BLAKE3 stand against it until the first compile measures them.
date: 2026-10-29
---
# blake3-servil: speed of light

BLAKE3 is compute-bound on every shape blaze has: a core hashes at most a few GB/s, a tenth of
what one core can stream from DRAM, so the bound is the compression function's operations over
the vector ports. One compression is 7 rounds of 8 G steps, about 14 operations each (4 adds,
4 xors, 4 rotates, of which the rotates by 12 and 7 cost two operations without a rotate
instruction), so about 840 operations per 64-byte block per lane, plus 8 xors at the end.

| shape | bound | arithmetic | how it is reached |
|---|---|---|---|
| whole file, 64 KiB and up, AVX-512 (EPYC Turin) | about 0.35 cycles/byte, 11 GB/s at 4 GHz | 840 operations per block over 16 lanes of a 512-bit register, 2 vector ports: 26 cycles per 16 blocks of 64 bytes | the fork's 16- and 32-input AVX-512 kernels (its Linux-only `blake3_avx512_32.S`), official's 16-input one |
| whole file, NEON (Graviton 4, Neoverse V2) | about 0.8 cycles/byte, 3.5 GB/s at 2.8 GHz | 840 operations over 4 lanes of a 128-bit register, 4 vector ports: 210 cycles per 4 blocks | the fork's integer + NEON hybrids (integer lanes fill the idle issue slots, `xar` for rotates); official's 4-lane C intrinsics |
| whole file, SME2 (Apple M4) | the fork's 6 GB/s from 1 MiB, as its documentation gives | the fork's own measurement; official has no SME2 kernel | the fork's SME2 kernels, from 16 KiB |
| `ContentId::of`, 64 bytes or less | about 50 ns | one compression's dependent chain: 7 rounds of 2 half-rounds of about 14 dependent operations, plus the row shuffles, about 200 cycles | one call of the one-chunk kernel, no hasher: `b3::derive_key_with` (official needs its 1.9 KiB `Hasher`, the fork its `hash_with`) |
| `ContentId::of_many`, one-chunk inputs | about 7 ns per block per input on AVX-512, 15 on NEON | the whole-file bound per block, every lane filled | last blocks by `compress_lanes` for both backends, 16 lanes on AVX-512 and 8 on AVX2 by runtime dispatch, the baseline's 8 elsewhere; leading blocks by official's `hash_many` (16 lanes on AVX-512), or by `compress_lanes` under the fork. Messages and outputs are transposed in registers (see [format's section](../format/speed-of-light.md#contentidof_many-many-short-inputs-item-hashes), speed-sol2-format) |
| `blaze_ids::Hasher` and blaze-diagnostics' writes | the one-shot bound of the total length | the same compression count; the pieces add only copies | both stage short pieces in a buffer and compress whole blocks |

Where the fork stands short, until the bench says otherwise:

- **`of_many`'s leading blocks under the fork on x86-64.** The fork exports no kernel that stops
  short of a chunk's root (its batches take messages of one length and give their roots), so a
  one-chunk input's blocks before its last go through `many::leading_lanes`, blaze's 8-lane
  safe Rust that LLVM vectorizes for the baseline target (SSE2: two 128-bit registers). On
  AVX-512 that was about 2.5 times the bound for inputs of 65 bytes to 1 KiB; on NEON it is
  about the bound. The second of the two fixes is in (sol-all, 2026-10-30): `compress_lanes`
  and `leading_lanes_of` are generic over the lane count and always inlined, and on a CPU with
  AVX-512F `leading_lanes` runs them 16 lanes wide inside a `#[target_feature(enable =
  "avx512f")]` function (`many::x86`), so each word of the transposed state is one 512-bit
  register and the rotates are `vprord`. That is the whole-file bound's 16 lanes over 2 ports
  (840 vector operations, 420 cycles a block of each of 16 inputs), plus the scalar transpose of
  the messages (256 words, about 128 cycles at two stores a cycle): **about 1.3× the bound** by
  count, not measured, against 2.5× before. (speed-sol2-format, 2026-10-30) The transpose is now
  in registers too: `many::columns` loads each block's words and `transpose` interleaves them
  (`std::simd`, `vpermt2d` on AVX-512), 32 interleaves over 16 inputs in place of 256 scalar
  stores. That makes it **about 1.15×** by count. AVX2 CPUs take 8 lanes through the same
  dispatch.
  Last blocks had the same baseline problem, at about 4× the bound for inputs of one block. They
  now dispatch the same way, start group 0 from a splat of the key, and read and write in
  registers. Format's section gives the count, about 1.2×.
  `leading_blocks_are_alike_at_every_width` holds 8 lanes, 16 lanes and the AVX-512 copy to the
  same chaining values; `of_many_is_alike_on_both_backends` holds them to official BLAKE3. The
  other fix, the fork exporting its `hash_many` with flags, would need no transpose.
- **The fork's 18 KiB `Hasher`** (official's is 1.9 KiB): a hasher made and moved per ID costs a
  copy of it where the compiler doesn't build it in place. `ContentId::of` and `of_many` make
  none; `blaze_ids::Hasher` and blaze-diagnostics' key make one each. The bench's two `Hasher`
  rows measure it.
- **The fork's SME2 turn** (`Sme2Turn`, M4 only): every hash of 16 KiB or more loads and stores
  one process-wide flag, so threads hashing files at once share one cache line. At 6 GB/s a
  64 KiB hash takes about 11 µs, against about 100 ns for the line to move: under 1%.

The bench is `crates/blaze-ids/benches/blake3.rs`; its verdict line decides the backend
(docs/v2/plan/process/decisions.md).
