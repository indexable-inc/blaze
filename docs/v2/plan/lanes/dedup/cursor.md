---
status: plan
summary: dedup-cursor (2026-10-29): one checked little-endian byte cursor, `blaze_format::cursor` (`Reader` over `&[u8]` with fixed-width, LEB128, length-prefixed, NUL-terminated and counted reads, an `Error` naming the byte and what was wanted; `Put` on `Vec<u8>` for the writers), what it replaced, what stays and why, and its speed of light.
date: 2026-10-29
---
# dedup-cursor: the checked little-endian byte cursor

Part of [dedup](../dedup.md). The cursor lives in `blaze-format` beside [LEB128](codec-leb.md):
`crates/blaze-format/src/cursor.rs`, tests in `cursor/tests.rs`.

## What it is

`Reader<'a>` holds the whole input and what is left of it (two slices). Every read moves past
what it read, or fails, leaves the reader where it was, and returns an `Error`.

| method | what |
|---|---|
| `new(bytes)`, `starting_at(bytes, at)` | a reader; `starting_at` refuses an `at` past the end |
| `at`, `left`, `is_done`, `bytes`, `remaining` | the position from the start, bytes left, none left, all the bytes, the bytes left (not moving) |
| `take_rest`, `seek(at)`, `finish` | the rest (moving to the end); move to `at`; `Wanted::End` unless all was read |
| `take(len)`, `skip(len)`, `array::<N>()` | the next bytes, as a slice or `&[u8; N]` |
| `u8` `u16` `u32` `u64` `u128` `i8` `i16` `i32` `i64` | little-endian integers |
| `uleb`, `sleb`, `uleb32`, `sleb32`, `skip_leb` | LEB128 through `blaze_format::leb`: strict past 64 bits, wasm's 5 bytes and 32 bits |
| `bytes_u32`, `bytes_u64`, `bytes_uleb` | a length (`u32`, `u64`, ULEB128), then that many bytes |
| `str_u32`, `str_u64`, `str_uleb` | the same, checked UTF-8 |
| `cstr` | the bytes to the next NUL, moving past it |
| `count_u32(each)`, `count_u64(each)`, `count_uleb(each)` | a count of items of at least `each` bytes (at least 1), refused when the bytes left can't hold them, so nothing is allocated for bytes that aren't there |

A length-prefixed or counted read that fails goes back before its length, so a failed read never
moves the reader.

`Error { at, wanted }`: `at` is the byte (from the start of the reader's bytes) where the
failing part was wanted, and `wanted` is one of `Bytes { len, left }`, `Leb(LebError)`,
`Utf8 { len }`, `Nul { left }`, `Count { count, left }`, `End { left }`. It is `Copy`, at most 32
bytes (a `const` assertion), `Display`s as `at byte 12: 4 bytes wanted, 2 left`, and is a
`std::error::Error`. Mapping it:

- `String` errors: `impl From<cursor::Error> for String` is in `blaze-format`, so `?` works.
- `&'static str` errors: `.map_err(cursor::Error::as_str)`.
- An error type of the caller's: one `impl From<cursor::Error> for ItsError` (often
  `ItsError::Short { at: error.at }`) and `?`, or one `map_err` where a field wants its own
  words (`.map_err(|_| "a truncated call frame entry")`).

`Put`, implemented for `Vec<u8>`: `put_u8` … `put_i64`, `put_u128`, `put_uleb`, `put_sleb`,
`put_bytes` (no length), `put_bytes_u32` (panics past `u32::MAX` bytes: the format can't say it),
`put_bytes_u64`, `put_bytes_uleb`, `put_cstr` (panics on a NUL inside), `pad_to(multiple)`
(zeros to the next multiple, what `blaze-link`'s three `align`s were), `patch_u32`/`patch_u64`
(write over bytes written before, for a length known only after what follows). Every
`Reader` read has its `Put` write.

Safe code only (`#![forbid(unsafe_code)]` on the module): readers of mapped, untrusted bytes keep
docs/v2/plan/design/unsafe.md's check-then-view rule, and the cursor never makes a view.

## Why this shape

- **Two slices, not a slice and an index.** A read is `split_first_chunk::<N>` on the rest: one
  length compare, one unaligned load, and two register updates (the rest's start and length).
  The index form every copy used (`bytes.get(at..at + n)`) needs the same compare plus an
  overflow check of `at + n` the compiler can't drop, since it doesn't know `at <= len`.
- **The error is built out of line.** `short`, `reset` and `fail` are `#[cold]`: the inlined
  read holds no formatting, no `String`, no error layout work. The copies that returned
  `String` allocated their message inline in the reader, on the error path but in the hot
  function's body, which bloats it and its inlining.
- **No context string per read.** `blaze-cas`'s and `blaze-global`'s readers took `what: &str`
  on every call to name the field in the message. The `Error` names the byte instead, which
  says more for a corrupt input, and a caller that wants the field's name `map_err`s that one
  read.
- **No generic error parameter.** A `Reader<E>` would monomorphise every method per caller's
  error and push its conversion into the hot path; `From` at the `?` costs nothing when it
  isn't taken.

## Speed of light

A field read from an in-memory buffer must at least check that the field is there (one compare
and a predicted branch) and load it (one load, L1-resident for these sequential reads). Moving
the position is one more add, which is the only serial chain from field to field, 1 cycle.
So the bound is about 3 µops and 1 cycle of latency per field. Ours is 4 µops (compare-and-branch
fused, load, the rest's start and length), and the chain is still 1 cycle (the length and start
update in parallel); at 6 to 8 µops a cycle on a current x86 or Apple core both are under a cycle
a field, throughput-bound by the caller's use of the value, not by the read. LEB128 reads are
`leb`'s bound (12 cycles a multi-byte value on the position chain, 1 a single byte;
[codec-leb.md](codec-leb.md)). `cstr` is a byte scan, bound by the string's length; the
strings it reads (CIE augmentations, short names) are under 8 bytes, where a SWAR or SIMD scan's
setup costs more than it saves.

Writes: `extend_from_slice` of a `to_le_bytes` array is a capacity compare, one unaligned store,
and the length update, the same 3 to 4 µops as the read; into a vector with room, nothing is
allocated (the tests hold reads and writes to 0 allocations).

Against the state of the art: this is the shape of `bytes::Buf`'s `get_u32_le` on a `&[u8]` and of
`zerocopy`'s split readers, without their trait dispatch or panics on a short buffer.

## What moved and what stays

The callers and their moves, file by file, are in the report, `/tmp/blaze-lanes/v2-dedup-cursor.md`,
and its children's reports (`v2-dedup-cursor-{link,store,run,front}.md`). Edited directly (free):
`blaze-splice` (`lib.rs`, `macho.rs`, `pe.rs`), `blaze-vfs` (`journal.rs`, `disk.rs`, `tree.rs`),
`blaze-history` (`codec.rs`), `blaze-global` (`wire.rs` and its users). Requested of their
owners: `blaze-link` (every `.eh_frame`, wasm, dylib, span-table and writer copy, and one
`.eh_frame` pointer decoder in place of two), `blaze-cas`, `blaze-graph` (`admission.rs`),
`blaze-cargo`, `blaze-usercode` (`trace.rs`, `index.rs`, `test/select.rs`), `blaze-codegen`
(`blob.rs`, `summary.rs`), `blaze-engine` (`manifest.rs`), `hir-def` (`frozen.rs`), `hir-ty`
(`method_resolution/frozen.rs`), `hir-ty-codegen` (`v2/unwind.rs`, `codeview.rs`, `split.rs`,
`seh.rs`, `asm.rs`, `wasm.rs`, `v2/{shape,hashes,fold}.rs`), `base-db`, and tt's new wire.

Several copies were looser than the cursor, and the move tightens them: LEB loops that dropped
bits past 64 (or 32), `as u32` lengths that wrapped past 4 GiB (one of them a hash input), walks
that stopped quietly at a record running past its section, a LEB skip with no length limit.
Each is listed in the child report that moved it.

Stays, on purpose:

- `blaze-live`'s `patch.rs` and `format.rs` readers and writers: `blaze-live` is the
  dependency-free runtime linked into the program being patched (its only dependency is
  `blaze-mem`); `blaze-format` would bring zstd and serde into every running program. Same
  shape as `Reader`, compared and kept with a comment.
- The proc-macro wires (`blaze-expand/src/native/wire.rs`, `blaze-macro-wasm/src/wire.rs`):
  being replaced by tt's `read_wire`/`write_wire` (lane `usercode-wire-macros`), which was asked
  to read with `Reader`.
- `blaze-inproc/src/frame.rs`'s `Reader` and `Writer`, `blaze-lazy/src/wire.rs`'s field reads,
  and `blaze-usercode/src/test/wire.rs`'s `Cursor` (with `fork_server/recorded.rs`): the first two
  run in the program's process, which links them with nothing but `blaze-mem`; the last is
  `include_str!`'d into every test binary's harness, which links nothing of blaze. Each gets a
  comment saying so (requested).
- `blaze-core`: no `blaze-format` dependency; it sits below `blaze-cas` and `blaze-vfs`, and has
  no cursor (fixed-size records and kernel structs at their offsets).
- `hir-def/src/deps/signatures/key.rs`'s writers: they write into a `SmallVec<[u8; 64]>` so
  building a key never allocates, and `Put` is on `Vec<u8>`.
- `blaze-registry/src/index.rs`'s `Split`: cargo's lenient index rule accepts a last string
  without its NUL, which `cstr` refuses.
- `tt/src/storage/varint.rs`'s `Reader`: native-endian and infallible over bytes it wrote itself
  in the same process; a different invariant.
- `blaze-format/src/schema.rs`'s `Reader`: a `const fn` byte stream feeding BLAKE3 over parts,
  not a field reader.
- Readers of checked, fixed-layout tables (`blaze-stencil/src/table/format.rs`'s `u32_at`, the
  span table's `Spans`): random access after one check of the whole table, not a cursor.
- CBOR decoders (`blaze-protocol/src/cbor`): their `take` is the CBOR item reader, with its own
  limits.
