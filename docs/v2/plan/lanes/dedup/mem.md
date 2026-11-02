---
status: plan
summary: dedup-mem (2026-10-29): one alignment module (blaze_mem::align), the arena and pool copies against blaze-mem's allocators, and the thread and process pools compared with their own kind.
date: 2026-10-29
---
# dedup-mem: alignment, arenas, pools

## Alignment: `blaze_mem::align`

**Picked:** a new module, `crates/blaze-mem/src/align.rs`: `const fn`s `align_up`,
`checked_align_up`, `align_down`, `is_aligned`, `padding_for` and `align_range` for `usize`, with
`_u32` and `_u64` forms, all for a power-of-two alignment (checked in debug builds).

**Why not std alone.** Most of the tree already rounds up with `next_multiple_of` (over 200
sites), which is right where the alignment is a constant: the compiler makes it the mask. Where
the alignment is known only at run time (a `Layout`'s in the frame and arena bump paths, a
section's or a page's in the linker), `next_multiple_of` and `is_multiple_of` compile to a
`div`: 20 to 40 cycles on x86-64 for 64-bit operands, 7 to 9 on Apple's cores. The mask is an
add and an `andn` (or `bic`), one cycle each with no dependency on a divider. std also has no
integer rounding down, and that is the form copied most (`x & !(PAGE - 1)` in `blaze-live`,
`blaze-inproc`, `blaze-link`, `blaze-mem`, and the cache-line loops).

**Speed of light.** `align_up` is two dependent single-cycle ALU ops (add, and-not); `align_down`
and `is_aligned` one; `padding_for` two (neg, and); `checked_align_up` adds a predictable
carry branch. All are `#[inline(always)]` and `const`, so a constant argument folds away. That is
the bound: no instruction sequence rounds a value to a runtime power of two in fewer.

**Copies and where they go** (callers not free, so each is a request):

| copy | goes to | request |
|---|---|---|
| `blaze-mem/src/os.rs` `round_up` | `checked_align_up` | `v2/requests/mem.md` |
| `blaze-mem` `frame.rs`, `arena.rs`, `region.rs` inline masks | `align_up`, `align_down` | `v2/requests/mem.md` |
| `blaze-link/src/reloc.rs` `align_up` (9 calls in `place.rs`) | `align_up_u64` | `v2/requests/link.md` |
| `blaze-link/src/pe/image.rs` `page_of` | `align_down_u64` | `v2/requests/link.md` |
| `blaze-cas/src/disk.rs` `aligned`, `blocks` (constant alignments) | std `next_multiple_of` | `v2/requests/mem.md` (mem-backend-cas) |
| `blaze-inproc/src/alloc.rs:95` mask | `align_down_u64` | `v2/requests/fastcompile.md` |
| `blaze-stencil/src/buf.rs` `(a - len % a) % a` | `padding_for_u32` | `v2/requests/fastcompile.md` |

**Kept on purpose:**
- `blaze-live`'s page arithmetic (`pages`, `page` loops in `slots/*.rs`, `linux.rs`, `macos.rs`):
  `blaze-live` is a published crate with no dependencies, linked into the user's program; it
  cannot take `blaze-mem`. Its forms are constants (`PAGE`), already the mask.
- `blaze-format/src/lib.rs` `align_up`: std's `next_multiple_of` plus the format's own check
  (no record aligned past `ALIGN`); not a copy of the arithmetic.
- `hir-ty-codegen/mod.rs` `align_up`: emits Cranelift instructions, not a number; renamed
  `emit_align_up` (request in `v2/requests/codegen.md`).
- `rustc_abi`'s `Size::align_to`/`is_aligned`: methods of its own types, as the brief says.
- `hir-ty-mir`'s `Address::aligned`: sets an address's alignment, not rounding.

**Tests** (`crates/blaze-mem/src/align/tests.rs`): proptest differentials against the copies
(reloc's `align_up`, os's `round_up`, `buf.rs`'s padding, `x - x % a` for rounding down,
`blaze-live`'s `pages`) and against std's `checked_next_multiple_of`, for every power of two of
each width; compile-time assertions (the functions are `const`); and the edges (0, `MAX`, the last
alignable value, alignment 1 and 2^63). No allocation-budget test: these are register
arithmetic.

## Arenas

`blaze_mem` has the arenas: `Frame` (a task), `Arena`/`ArenaLocal` (a request, or the process
when never reset), `Revisions`. The copies found:

| copy | what it is | verdict |
|---|---|---|
| `blaze-graph/src/arena.rs` `Bump` + `task` + `with` | per-thread task bump arena, spare per depth | copy of `Frame` + `frame::task`; mem-core (owner of the file, migrating it) told, `v2/requests/mem.md`. Its NUMA binding and `Cap` reservation, if kept, belong in `blaze_mem::region` and the tag budgets |
| `intern/src/arena.rs` `Bump` | never-freed bump over chunks, one per shard | copy of `Arena`: a `static` `Arena` (`Arena::new` is `const`) never reset, an `ArenaLocal<'static>` per shard; mem-format and dedup-intern told |
| `blaze-link` `bumpalo::Bump`, reset per stage | request arena | copy of `Arena`; `v2/requests/link.md` (mem-backend-link) |
| `blaze-inproc/src/alloc.rs` `Arena` | TLSF heap over code offsets: free in any order | differs on purpose; renamed `CodeHeap` with a comment (`v2/requests/fastcompile.md`) |
| `mbe/src/expander/matcher.rs` `Arena<'t>` | the reused `Vec`s of one match | not an arena; renamed `Buffers` (mem-format, `v2/requests/mem.md`) |
| `la_arena` (front end) | typed index arena (`Vec<T>` + `Idx<T>`) | a different kind (an ID-indexed table, not memory): out of scope |
| `typed_arena` in `blaze-cargo/src/cargo/package.rs`, `hir-ide` | values with destructors on a cold path (a `.dwp` packing; an IDE crate to be removed) | kept: `blaze_mem::Arena` runs no destructors |

The table copied most is not an arena but a column: "chunk `k` holds `FIRST << k` once-set
slots, never moved" in `base-db`, `blaze-ids`, `blaze-query` (twice), `blaze-vfs`,
`blaze-hashcons`, `blaze-graph`, `proc-macro-api`, `blaze-web`, `span`. Those that append are
`blaze_mem::Column` as it is (the `Column` push is one `fetch_add` and one release `fetch_or`, a
read one acquire load against two in the `OnceLock` form, and no heap chunk per level). The list
is in `v2/requests/mem.md` for the children moving each crate's tables.

## Pools

- **Thread pools.** `stdx::thread::Pool` (rust-analyzer's, a channel and `Box<dyn FnOnce>` per
  job) has no caller left in the tree; `blaze_graph::pool::Pool` (Chase-Lev deques, LIFO slot,
  topology-aware stealing) is the one pool. Deleting stdx's, with its two crossbeam
  dependencies, is in `v2/requests/coordinator.md`.
- **Process pools.** `blaze_usercode::pool::Pool` (sans-I/O warm processes by `WarmKey`) and
  `proc_macro_api::pool::ProcMacroServerPool` (a client's connections, one claimed per expansion)
  are two layers of one design, not copies: `proc-macro-api/src/warm.rs` asks the first for the
  second's servers. Both stay. The second's hand-built connection list is a `Column` (request in
  `v2/requests/usercode.md`).
- **Object pools.** `blaze_mem::Pool`/`Slab` are the pools. Free-list copies found:
  `blaze-graph/src/exec.rs` `Table` (a `Slab<Run>`), `blaze-graph/src/pump.rs` `FREE` and
  `blaze-query/src/segment.rs` `LaneIds` (thread slot indices), `blaze-core/src/uring.rs`
  `Registered::free` (a fixed set of 16-bit registered-buffer indices the kernel names: kept, a
  `Slab` would add a generation the kernel cannot carry). The first three are in mem-core's and
  mem-db's migrations (`v2/requests/mem.md`).

## Seen, for other parts

- The aarch64 cache-maintenance loop (read `CTR_EL0`, `dc cvau` per data line, `dsb ish`,
  `ic ivau` per instruction line, `dsb ish`, `isb`) is copied in `blaze-live/src/slots/linux.rs`,
  `blaze-inproc/src/sys/linux.rs`, `blaze-stencil/src/tests/exec.rs` and
  `blaze-lazy/src/tests/exec.rs`; told lane dedup.
- `blaze-inproc/src/alloc.rs` `OffsetHasher` is a hasher of its own (dedup-codec's).
