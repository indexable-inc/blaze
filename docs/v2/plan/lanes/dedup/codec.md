---
status: plan
summary: dedup-codec (2026-10-29): one CRC32C (blaze_ids::crc32c), one SHA-256 (crate blaze-sha256, a dependency-free build-dependency too), one seeded RNG (blaze_ids::rng::SplitMix64), one fixed-width Hasher half (blaze_ids::fixed_width_writes!, StdHash), and one LEB128 (blaze_format::leb, notes in codec-leb.md); each with its speed of light.
date: 2026-10-29
---
# dedup-codec: checksums, hashes, RNGs, varints

Part of [dedup](../dedup.md). LEB128 is [codec-leb.md](codec-leb.md); the RNG callers'
moves are [codec-rng.md](codec-rng.md). Who moved what, and the requests for callers that
weren't free: `/tmp/blaze-lanes/v2-dedup-codec.md`.

## CRC32C: `blaze_ids::crc32c`

The image's and the store's copies were the same three-stream design (the image's had the
bitwise reference and more tests), so that one moved, with every test of both, into
`crates/blaze-ids/src/crc32c.rs`, the one module of `blaze-ids` allowed `unsafe` (the crate is
`#![deny(unsafe_code)]`). API: `crc32c(bytes)`, `extend(crc, bytes)`. Paths: SSE4.2 `crc32q`,
ARMv8 `crc32cx`, slicing-by-8; which runs is dispatch.

**Speed of light.** On x86-64 (Intel since Nehalem, AMD since Zen) and Apple and Neoverse
aarch64, the 8-byte CRC instruction has a throughput of one a cycle and a latency of three:
8 bytes a cycle, 32 GB/s at 4 GHz, far under L2 bandwidth, so the instruction port is the
bound. One dependent stream reaches a third of it. Three independent streams reach it, joined
by `combine`: two four-lookup `shift`s per round.

A 64 KiB image group, the size checked on first read:

- the bound: 65 536 / 8 = 8192 cycles (2.0 µs at 4 GHz);
- five 12 KiB rounds: 7680 cycles, plus 5 joins of about 20 cycles;
- the 4 KiB left over. On one stream that is 512 words × 3 cycles = 1536 cycles, about 9300
  in all, 13% over the bound. So the tail runs two more rounds of 512-byte streams (2 × 192
  cycles plus 2 joins), and only 1 KiB runs on one stream (128 × 3 = 384 cycles). About 8600 in
  all, **5% over the bound**.

The carry-less-multiply folding of Intel's `crc_pcl` and Corsix's `crc32_fusion` gets past
the one-instruction-a-cycle port by also using the vector unit. It pays only above the
port-bound rate, which the image reaches only when its groups are already hot in cache. At
37 µs per MiB (`intern`'s table restore) the CRC is not what restore waits on.

Short inputs (the store's record headers, tens of bytes) are latency-bound: 3 cycles a word,
the dependency chain itself.

Not copies: `blaze-store`'s `crc32fast` (CRC-32/IEEE, in the old store's pack format, deleted at
integration) and `hir-ty-mir`'s `crc32` shim (it interprets the CPU's `crc32*` instructions for
either polynomial, with no inversion).

## SHA-256: crate `blaze-sha256`

There were four copies: the two build scripts' (`blaze-cargo`, `blaze-engine`, identical,
portable, streaming through a `Vec`), `blaze-link`'s (portable, SHA-NI, ARMv8, one-shot) and
`blaze-splice`'s (ARMv8, else the `sha2` crate at about 250 MB/s on aarch64). The one copy is
`blaze-link`'s compressors under a streaming `Sha256` that holds its partial block inline
(it never allocates). `digest` is that hasher over one slice, compressing whole blocks in place.

**Why a crate.** Build scripts can share code only through a build-dependency or an
`include!`. An `include!` of a sibling crate's file breaks `tools/nix/units.nix`, which cuts
each workspace crate down to its own directory. The `sha2` crate is what those build scripts
avoid: its `cpufeatures` needs `libc` on Apple's aarch64, and `tools/nix/units` holds one graph
per target computed on Linux. A crate with no dependencies adds one small unit to the
build-script graph and works under cargo, nix and blaze alike. A runtime crate's
`[dependencies]` and a build script's `[build-dependencies]` use the same crate.

**Speed of light.** One message is a serial chain of 64 rounds a block. With SHA-NI, two rounds
are one `sha256rnds2`, so a block is 32 of them back to back, plus the message schedule, which
overlaps them. That is about 1.5 to 2 cycles a byte on Ice Lake and Zen (2 to 2.7 GB/s at
4 GHz). ARMv8's `sha256h`/`sha256h2` pair does four rounds, about 1.3 cycles a byte on Apple's
cores. The compressors are the instruction sequences with nothing else on the chain: the loads
and byte swaps are off it, and the state stays in two registers across blocks. Across the many
independent pages of a code signature, the bound is the cores times that. `blaze-link`'s
`sign.rs` and `blaze-splice`'s `macho.rs` split pages over threads (the splice only when
`fast()` is false). The build scripts hash at most the workspace's sources once per build, so
they are bound by reading the files.

## Seeded RNG: `blaze_ids::rng::SplitMix64`

About 45 copies of SplitMix64 (and some xorshift streams) in tests, simulations and jitter. The
one copy has `new`, `next_u64`, `next_u32` (the high half), `below` (`% n`, the form most copies
used, so their streams and seeds keep their meaning), `index`, `range`, `one_in`, `chance`, `coin`,
`unit_f64`, `pick`, `fill`, `shuffle`, and the finalizer `mix` and `GAMMA` for the copies that
hash with it. A draw is one add on the state chain (1 cycle) and two multiplies and three
shift-xors off it: about 2 to 3 cycles a draw, the cost of the mixing itself. `below`'s
division (20 to 40 cycles on x86) is the price of keeping every moved stream's values. No caller
draws in a hot loop. `blaze-radix` keeps its copy because `blaze-ids` depends on it.

## Fixed-width `Hasher`: `blaze_ids::{fixed_width_writes!, StdHash}`

`hir-ty-codegen`'s `Le`, salsa's `Sip`, `blaze-hashcons`'s `Writer` and `hir-ty-share`'s `Feed`
each hand-wrote the twelve `write_u8`..`write_isize` methods: little-endian bytes, `usize` and
`isize` as 64 bits, so that a derived `Hash` gives the same bytes on every target. The macro is
those twelve methods over the impl's own `write`, inlined, the same code as before. `StdHash`
is the whole adapter over `blaze_ids::Hasher` (`Writer` and `Feed`, with `Hasher::std(&value)`).
It costs nothing beyond the BLAKE3 it feeds.
