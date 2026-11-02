---
status: plan
summary: speed-unsafe-back's pass over the back half's crates against unsafe.md: root gates, unsafe kept to small modules with one op per block, untrusted bytes checked before any view, and what each unsafe use buys over its safe version.
date: 2026-10-30
---
# speed-unsafe-back: the back half under unsafe.md

The rules are [unsafe.md](../../design/unsafe.md). The crates are split among this agent and
three children, each with its own notes:

| crates | agent | notes |
|---|---|---|
| `blaze-core` | speed-unsafe-back-core | [speed-unsafe-back-core.md](speed-unsafe-back-core.md) |
| `hir-ty-codegen`, `blaze-codegen`, `vendor/llvm-sys` | speed-unsafe-back-codegen | [../codegen/speed-unsafe-back-codegen.md](../codegen/speed-unsafe-back-codegen.md) |
| `blaze-live`, `blaze-usercode`, `blaze-build-script`, `blaze-cargo/src/sandbox/` | speed-unsafe-back-run | [../usercode/speed-unsafe-back-run.md](../usercode/speed-unsafe-back-run.md) |
| `blaze-graph`, `blaze-vfs`, `blaze-link`, `blaze-api`, `blaze-ci`, `blaze-protocol`, `blaze-ui`, `blaze-web`, `blaze-v2-proof` | speed-unsafe-back | this file |

The benchmark is the standard unsafe.md sets: the Rust standard library's and the Rust
for Linux project's (unsafe only behind a safe API, each block with one operation and a
`// SAFETY:` comment, `unsafe_op_in_unsafe_fn`), checked by clippy's `undocumented_unsafe_blocks` and
`multiple_unsafe_ops_per_block`.

## blaze-graph

Before: no root gate. Unsafe sat in five modules, and kernel calls were scattered through
`arena.rs` (`sysctlbyname`, `getrlimit`) and `topology.rs`. Two blocks held several operations:
`pump.rs`'s unlink walk (two reads and a write through raw pointers in one loop) and its wake loop
(three reads and a write), and `pool.rs`'s job run (two `read`s).

After:
- `lib.rs` says `#![deny(unsafe_code)]`. `exec`, `pool` and `pump` have `#[allow(unsafe_code)]`:
  they are the lock-free structures themselves. Their safe versions would cost an `Arc` and a
  boxed closure per job (pool), a lock per waiter's wake (pump), or a lock per slot read (exec's
  table). pool and pump have model tests under `--cfg blaze_check`. `arena.rs` keeps its allocator
  hook in `mod global`, since `GlobalAlloc` is an unsafe trait and there is no safe way to see
  every allocation. Its two kernel calls are in `mod sys`. `topology.rs`'s `linux` and `macos`
  modules hold the affinity and `sysctl` calls. Each of those inner modules is allowed alone, so
  the rest of `arena.rs` and `topology.rs` is under the deny.
- Every block does one operation. The unlink walk reads `*link`, then the waiter's `next`, then
  writes `*link`, each in its own block. The wake loop reads `next`, clones the thread, writes
  the value and stores the state, in that order, with the reference not used after the store.
  The order of reads and writes is unchanged, so the model tests still cover it.

Untrusted input: none. The plane takes requests from the daemon in the same process.

## blaze-vfs

Already as unsafe.md asks (speed-runtime-vfs, then safety-audit-edge). It has 38 blocks, all in
`store.rs` and `store/tests.rs`, one op each, under `#![deny]` with `#[allow] mod store`. The
safe version would cost a `RwLock` count per column read, or a copy of the tree per commit.
Its untrusted inputs:
- The journal is checksummed per record, read through `Reader` with every length checked, and
  stops at the first bad record without panicking. It has three fuzz targets (`vfs_journal_*`).
- The image base is checked whole by `Base::check` before any view: every span, entry range,
  file range, mode tag and index place, and how full each index is. `within` checks that every
  column lies in the mapping. Files are checked by CRC32C per page group on first read.
- Not fuzzed: `Base::check`. Filed with safety-fuzz below.

## blaze-link

The readers (ELF, Mach-O, COFF, wasm objects and archives) are safe code under
`#![forbid(unsafe_code)]`. Every offset goes through `cursor::Reader` or `.get` before it is
sliced (read for this pass: `elf/read/archive.rs`, `strings.rs`, `macho/input.rs`'s call
frames, `macho/archive.rs`'s index strings, `macho/dylib.rs`'s trie edges), and each format has
fuzz targets (`link_*`). The one unsafe block is `intern::name_at`'s `from_utf8_unchecked`:
the bytes were appended from a `&str`, and checking them again costs a UTF-8 validation per name
read. It has a test against the checked version. The missing root gate, and a test's
`from_raw_parts` that buys nothing (made safe with `zerocopy::IntoBytes`), are requested in
`v2/requests/link.md`, because `lib.rs` and `tests.rs` are held by requests-link-next-lazy and
-sanitize.

## blaze-api, blaze-protocol, blaze-ci, blaze-ui, blaze-web

- blaze-protocol, blaze-ci (library and binary) and blaze-ui say `#![forbid(unsafe_code)]`.
- blaze-api: `#![deny]`, with unsafe only in `activation` (`passed`, `sys`). Taking the
  descriptors systemd or launchd passed is an FFI step with no safe version.
- blaze-web: `#![deny]`, with unsafe only in `flecs.rs` (FFI to Flecs). Its build script says
  `forbid`.
- Network input has hard limits: frames at most `LONGEST_FRAME` (16 MiB); CBOR depth-limited,
  with lengths bounded by the input and checked before decode; HTTP heads at most 8 KiB a line
  and 100 headers, with bodies capped by the caller (`MOST_HELD`); the CI webhook payload at
  most 25 MiB; GitHub answers at most 8 MiB. Each parser is fuzzed (`api_*`).

## blaze-v2-proof

`bytes.rs` is the oracle's first-difference compare: AVX-512BW, AVX2, SSE2 and NEON paths chosen
at run time, each tested against `scalar`. Before, it had 63 blocks, 24 of them `pa.add(at)`
offsets the loop bound already keeps in both slices. They are now the safe `wrapping_add`, the
same instruction, so the loads and the map are the only unsafe left (39 blocks). The module doc
now names the safe version and its cost: `==` over pieces (libc's `bcmp`) and then a byte loop
in the piece that differs, which reads that piece twice and pays a call per difference.

## Filed

- `v2/requests/link.md` "speed-unsafe-back": the blaze-link root gate and the test view
  (applied by requests, pass 45).
- `v2/requests/safety.md` "From speed-unsafe-back": a fuzz target for `blaze-vfs`'s
  `Base::check`, over arbitrary columns and a blobs length.
