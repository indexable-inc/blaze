---
status: plan
summary: lane dedup's speed of light: for each shared primitive, the work, the bound and its arithmetic, today's count, the gap, and what proof measures; the full arithmetic is in the part's notes beside this file.
date: 2026-10-30
---
# dedup: speed of light

Each shared primitive's bound from the hardware, its arithmetic and how the one implementation
comes within 10% of it are in the part's notes beside this file:

| primitive | shared implementation | bound in |
|---|---|---|
| bit sets, bit matrices, ranked and atomic bits | `blaze_ids::bits` | [bits.md](bits.md) (and `bits-db.md`, `bits-edge.md`, `bits-store.md`) |
| CRC32C, SHA-256, hasher forwarders | `blaze_ids::crc32c`, `blaze-sha256`, `blaze_ids::StdHash` | [codec.md](codec.md) |
| LEB128 | `blaze_format::leb` | [codec-leb.md](codec-leb.md) |
| seeded RNG | `blaze_ids::rng::SplitMix64` | [codec-rng.md](codec-rng.md) |
| byte cursor and writer | `blaze_format::cursor` | [cursor.md](cursor.md) |
| counting sort into CSR, timing wheel | `blaze_radix::Csr`, `blaze_core::wheel` | [order.md](order.md) |
| alignment | `blaze_mem::align` | [mem.md](mem.md) |
| read-only file mapping | `blaze_core::fs::Mapped` | [map.md](map.md) |
| SwissTable probe, lock-free `u32` index | `blaze_mem::swiss` | [intern.md](intern.md) |
| proc-macro bridge, lexer, wire | `blaze_expand::bridge` (requested) | [macros.md](macros.md) |
| edit distance, Windows command line, signal names, the rest of the scan | `stdx::edit_distance` (requested), `blaze_core::{create_process, signal}` | [scan.md](scan.md), `scan-*.md` |

## The machine (speed-sol2-small)

As [link/speed-of-light.md](../link/speed-of-light.md)'s "The machine": 3.5 GHz, L1 64 bytes a
cycle, one core from DRAM about 25 GB/s, about 4 instructions a cycle for straight-line integer
code, a system call on an open descriptor 0.15 µs, `mmap` and first touch of a page 0.25 µs.
Reference workloads: a warm `blaze check` of a big workspace (the bit sets of every body's
borrow check and liveness, every query key's BLAKE3, the store's CRC32C of each record read) and
a cold build (the store's and the linker's sorts, LEB128 and cursors over every image).

## Each primitive, counted (speed-sol2-small)

Today's count is read from the shared implementation as of 2026-10-30; the bound and its
arithmetic are the note's.

| primitive | unit of work | bound | today | verdict | `proof` measures |
|---|---|---|---|---|---|
| `bits` `contains`/`insert`/`remove` | one member | one shift and mask, one load (and store) in L1, ~1 ns | the same (`locate` then a word op) | at the bound | ns per op over a body-sized set, 0 allocations |
| `bits` unions, `count` | a word | load and store ports: 24 bytes a word, ~100 GB/s from DRAM | vectorized word loops (`cnt`/`vpopcntq`) | within 10% | bytes per second of `union_with` on 1 MiB sets |
| `bits` iteration (`Ones`) | a member, an empty word | `members + words / k` with a SIMD zero skip | `members + words` (`trailing_zeros` and clear lowest) | up to 2× off on sets sparser than 1 in 64 and over ~64 KiB; at the bound for the body-sized sets every caller has (a skip's setup costs more there) | ns per member over the callers' set sizes |
| `Ranked::rank`/`place` | a query | one cache miss (word and count load in parallel) | the same | at the bound | ns per query, cold and warm |
| CRC32C | a byte | 8 bytes a cycle on the CRC unit with three streams | three 512-byte streams and a two-round tail: 5% over on 64 KiB | within 10% | GB/s on 64 KiB and on 40-byte headers |
| SHA-256 | a 64-byte block | 64 serial rounds, SHA-NI or ARMv8 two a cycle: ~2 GB/s a core | the same instructions | within 10% | GB/s per core |
| BLAKE3 key (`blaze_ids::Hasher`) | a key of tens of bytes | one compression, ~20 ns | one compression plus the domain's context-key lookup by FNV-1a of its name (~30 ns) on every `Hasher::new(&str)`, and ~3 to 5 ns per tiny `update` | **~2.5× off for a one-block key** (scan-ident.md: 1.5× for identity spellings) | ns per `Hasher::new` + `finish` of a 32-byte key; ns per 1-byte `update` |
| LEB128 | a value | 1 cycle a single byte, ~12 cycles multi-byte on the position chain; writes ~5 cycles | the same | within 10% | ns per value, 0 allocations |
| byte cursor | a field | 3 µops, 1 cycle on the position chain | 4 µops, 1 cycle chain | within 10% (throughput is the caller's) | ns per field, 0 allocations |
| SplitMix64 | a draw | ~2 to 3 cycles a draw; `below` a multiply-high, 3 cycles | `below` divides (25 to 40 cycles on x86-64) | off on `below`, kept: no hot path draws (tests and simulations only), and `%` is what keeps every stream bit-identical | none: not on a build's path |
| counting sort into CSR | a pair | memory traffic, `2 × pair + item` bytes | two passes, no zeroing pass | within 10% for every caller | ns per pair at 1 M pairs |
| timing wheel | a timer | O(1) insert, cancel and tick | hierarchical wheel, steady state allocates nothing | at the bound | ns per insert and expiry, 0 allocations |
| `align_up`/`align_down` | a value | two single-cycle ALU ops | add and and-not | at the bound | none |
| `Mapped` | an open, a page | `open`, `fstat`, `mmap`, ≤2 `madvise`, `close` (2 to 5 µs); page faults or populate, then bandwidth | the same calls, nothing copied | within 10% | syscalls per open (5 or 6), GB/s of a populated read |
| `swiss` probe | a lookup | 2 dependent lines (group, value) plus the hash; expected 1.1 table lines | one acquire load of the level, the group's control word and slot in one line | within 10% | lines per hit (cache-miss counter), ns per hit |
| hex, signal names, edit distance, command lines | per call | a table lookup per digit; a jump table; O(n·m) with one row buffer | the same | at the bound; all off a hot path but hex | none |

## The gap, and what removes it (speed-sol2-small)

One primitive is more than 10% off its bound on a hot path: a short BLAKE3 key. Every one of the
workspace's 251 `Hasher::new(&'static str)` calls finds the domain's context key by hashing the
domain name with FNV-1a (`crates/blaze-ids/src/lib.rs` `context_key_in`, one dependent multiply
a byte over 20 to 30 bytes, ~100 cycles) and a slot compare, and only 12 calls use `Hasher::of`
with a `Domain` static, whose key is one `OnceLock` load. For a one-block key, the compression
itself is ~20 ns, so a key costs ~50 ns where its bound is ~20. Many tiny `update`s (the identity
spellings of `hir_ty_share`) add ~3 to 5 ns each where a buffered push would cost ~1.

What removes both: in `Hasher::new`, a direct-mapped cache keyed by the name's pointer in front
of the FNV path (an acquire load and a compare, ~5 cycles; same context key, so no hash
changes), and an inline buffer of a few hundred bytes in `Hasher` flushed to BLAKE3 when full
(BLAKE3 is independent of how its input is split, so again no hash changes). `blaze-ids` is lane
format's, held this round by `speed-sol2-format`: queue 00885 carries both with the exact change
and tests.

The note `scan-cargo.md` says `blaze-cargo/src/cargo/directives.rs` formats one string per key
per line it reads. The code no longer does (2026-10-30): its `format!`s are all on refusal and
error paths (lines 121 to 313), so there is nothing to file.
