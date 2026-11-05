---
status: plan
summary: dedup-codec-leb (2026-10-29): one LEB128, `blaze_format::leb` (write into a Vec or a slice, length, strict readers with a bounds error, unsigned and signed, the 5-byte and any-length padded forms), with an 8-bytes-at-a-time decode and a one-store encode; what it replaced, what stays and why, and its speed of light.
date: 2026-10-29
---
# dedup-codec-leb: LEB128

Part of [dedup-codec](codec.md). LEB128 lives in `blaze-format` (table in [../dedup.md](../dedup.md)):
`crates/blaze-format/src/leb.rs`, tests in `leb/tests.rs`.

## What it is

| function | what |
|---|---|
| `write_u64`, `write_i64` | append to a `Vec<u8>` |
| `put_u64`, `put_i64` | write at the start of a slice, touching only the bytes taken; return the length |
| `len_u64`, `len_i64` | the length, `const`, from `leading_zeros` |
| `read_u64`, `read_i64` | `(bytes, &mut at)`, `Err(LebError::{Truncated, Overflow})`, `at` unchanged on an error |
| `read_u32`, `read_i32` | wasm's: at most 5 bytes and 32 bits |
| `skip` | past a signed or unsigned value of at most 10 bytes |
| `padded_u32`, `padded_i32` | the 5-byte `[u8; PADDED]` a wasm object leaves for a relocated index |
| `put_padded_u64`, `put_padded_i64` | fill a slice of 1 to 10 bytes exactly; `Overflow`, and nothing written, when the value doesn't fit |

The readers are strict: past 64 bits (an 11th byte, or a 10th carrying more than bit 63, or for
signed anything but `0x00`/`0x7f`) is `Overflow`, as `blaze-link`'s ELF and Mach-O readers were.
The looser copies (the cold columns' reader, `blaze-splice`'s, `stage.rs`'s) silently dropped the
top bits of a 10th byte. Every input they accepted that a blaze writer can produce, this accepts.
A caller whose error type differs keeps a one-line wrapper (`map_err(LebError::as_str)`, or a
`format!` on the error path).

## How it is fast

- **Decode.** A first byte under `0x80` returns at once (most values in these streams: opcodes,
  small counts, lengths). Otherwise, with 8 bytes left: one unaligned `u64` load, the terminator
  is `trailing_zeros(!word & 0x8080_8080_8080_8080)`, the value is the word masked to it, then
  packed 7 bits a byte by three shift-and-mask steps (16, 32, 64-bit lanes). Bytes 9 and 10 are
  two scalar steps. This is protobuf's unrolled `ReadVarint64` / `varint-simd`'s scalar
  form without BMI2 `pext` (microcoded and slow on AMD before Zen 3). Fewer than 8 bytes left
  (only at a buffer's end) is a byte loop of at most 7.
- **Encode.** Under `0x80` is one `push`. Otherwise the inverse three steps spread the value over
  a word, the continuation bits are one mask, and with 8 bytes of spare capacity it is one 8-byte
  `extend_from_slice` and a `truncate`: one store. A vector sized exactly (as `debuginfo.rs` sizes
  its expressions) takes the byte-copy path, so it never grows.
- No allocation anywhere; the tests hold reads and writes into room to 0.

## Speed of light

The serial work in reading a stream of LEB128s is the position: each read's start is the last
one's end, and that depends on the bytes. Per multi-byte value, on a current x86 or Apple core,
the chain cannot be shorter than an L1 load-to-use (4 to 5 cycles), finding the terminator (one
`andn`, 1 cycle), `tzcnt` (3), turning a bit index into a length (1) and the add (1): about 11
cycles. Ours is that chain plus one more ALU op (`stop / 8 + 1` is a shift and an add), 12
cycles, 9% over. The value's packing (about 10 independent ops) is off the chain and overlaps the
next load. A one-byte value whose length is predicted takes no data-dependent step at all: about
5 µops, 1.25 cycles at 4 per cycle, which is the decode width's bound.

Batch decoders (Masked VByte, Stream VByte, `varint-simd`'s 4- and 8-value forms) beat this only
when many values sit back to back and are wanted at once. None of the callers has that: DWARF,
`.eh_frame`, LSDAs, dyld opcodes, export tries and wasm sections interleave LEB128s with opcodes
and other fields, read one at a time. So the single-value form is the bound for them.

Encoding: the work is a length (`lzcnt`, a multiply-shift for `/ 7`), the spread (9 independent
ops) and one store, about 20 µops or 5 cycles a value at 4 per cycle, bounded below by the store
port and `Vec`'s length bookkeeping at 3 to 4 cycles. Within 10% once the length is known, which
callers that size a buffer first already have.

## What moved and what stays

Moved (done): `blaze-format/src/tiers.rs` (`varint_len`, `put_varint`, `Bytes::varint`: plain
ULEB128), `blaze-wasm/src/link.rs` (`write_sleb`, `write_uleb`, which now refuse a value that
doesn't fit its 5 bytes instead of truncating it).

Requested of their owners (the paths are not free; the report lists each request): every copy
in `blaze-link` (`wasm/{leb,reader}.rs`, `stage.rs`, `unwind.rs`, `macho/{linkedit,input,unwind,dylib,tests}.rs`,
`elf/dynamic.rs`), `blaze-splice` (`macho.rs`, `wasm.rs`), `blaze-macro-wasm/src/lib.rs`,
`hir-ty-codegen` (`debuginfo.rs`, `unwind/{lsda,gcc_except_table}.rs`, `v2/unwind.rs`,
`wasm_object.rs`), `hir-def/src/nameres/frozen.rs`, and the tests of `blaze-inproc/src/load`.
The seeded RNG copies in the same directories go onto `blaze_ids::rng::SplitMix64` in the
same requests.

Stays, on purpose:

- `blaze-inproc/src/load/macho/fixup.rs`'s reader: the run process links `blaze-inproc` with no
  dependency, and `blaze-format` would bring zstd and serde into it.
- `blaze-splice/src/lib.rs`'s CIE skip: one line through a closure that already borrows the
  bytes; nothing to share.
- gimli's `Writer::write_uleb128` (`gcc_except_table.rs`, `debuginfo/split.rs`) and its readers
  (`llvm/emitted.rs`, `debuginfo/split.rs`): calls into gimli's writer and reader types, not copies.
- `tt/src/storage/varint.rs`: named for varints, but its fields are fixed-width.
