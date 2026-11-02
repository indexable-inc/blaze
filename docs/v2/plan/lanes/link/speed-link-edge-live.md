---
status: plan
summary: How blaze-live applies a patch fast (one read of the message into a reused buffer, viewed in place, a pool of four tables brought between generations by their cells, one release store, page runs written once, a watcher thread instead of 5 ms polling), the safety audit's answers (memory a patch never writes again, image checks), before and after per patch, what it measures against, and what is left; nothing in it was built or run.
date: 2026-10-29
---
# speed-link-edge-live: the patch apply path of blaze-live

Part of [link](../link.md), lane `speed-link-edge-live` (a child of `speed-link-edge`). Code:
`crates/blaze-live/src/patch.rs` and `crates/blaze-live/src/slots*`. The v1 runtime (`Live` in
`lib.rs`, `format.rs`, `linux.rs`, `macos.rs`, `windows.rs`, `sample.rs`) and the message bytes
are unchanged: `MESSAGE_VERSION` is still 1, and a test compares the new encoder against the old
one, field by field. Nothing here was compiled or run: the numbers below are counted from the
code, not measured.

## What it is measured against

- **Live++ and Hot-Reload (C++)**: patch functions in place by writing a jump at the old entry,
  one `VirtualProtect` pair and `FlushInstructionCache` per patched function, with the threads
  suspended while it writes. blaze never writes an old function and never suspends a thread: a
  patch changes table slots, and one store of the cell switches every call at once.
- **MSVC `/hotpatch`**: a 2-byte `mov edi, edi` at each entry, overwritten with a short jump into
  padding before the function. That costs nothing per call until a function is patched, then
  two jumps per call. blaze costs two loads and an indirect jump per call, patched or not
  (below).
- **Linux livepatch and ftrace (`text_poke_bp`)**: writes text through a second mapping instead
  of changing the protection of the real one. Batched, it pays one IPI sync per batch, not one
  per site. blaze-live on Linux now does the same for patch code: an RW alias of the memfd behind
  the RX region, 0 syscalls per write, and the cell store is the one point of switching.
- **subsecond (dioxus)**: a jump table of its own, swapped by the runtime after `dlopen` of the
  patch. That means one table per patch, a whole copy each time. blaze-live keeps at most four
  tables in all, and moves one to another generation by its changed cells only.

## Before and after, per patch

Counts are for a patch of W writes (of code and data), C cells changed, a table of S slots, on
Linux unless named. "Before" is the tree at `c1c126714e~1`.

| | before | after |
|---|---|---|
| finding the message | a thread `std::fs::read`ing `message_file(sequence)` every 5 ms | inotify (Linux), kqueue (macOS) or a change notification (Windows) on the directory; `poll` is one Acquire load while nothing came |
| reading it | the whole file copied into a `Vec`, decoded into W + 5 vectors, the code and data cloned once more | one read into a buffer kept from message to message, viewed in place: 0 allocations once the buffer has grown |
| validating it | `HashSet`s of slots and rebased addresses (SipHash per cell) | sorted scratch vectors kept from patch to patch, binary searches |
| syscalls, code writes | 2 `mprotect` per write | 0 (Linux, the RW alias); one page swap per run of pages, runs within 4 pages merged (macOS); one `VirtualProtect` pair per run (Windows) |
| syscalls, data writes | `/proc/self/maps` read whole per patch, 2 `mprotect` per piece | the maps read once at start, binary search; 2 `mprotect` per run of read-only pages, 0 for writable ones |
| icache | one flush per write | AArch64 Linux: all `dc cvau`, one `dsb ish`, all `ic ivau`, one `dsb ish; isb` per patch; one `FlushInstructionCache` per run (Windows) |
| rebasing | the code and data cloned, then fields patched in the copy | `Writes::put` copies the message's bytes straight to memory and adds the slide to each field there |
| allocations per cell | a `HashSet` insert, plus its share of the table copy | 0 (the log's columns grow by 20 bytes per cell, amortized) |
| tables | a new table of S × 8 bytes per patch, 64 retired ones kept | at most `POOL` = 4, made once; a patch costs C relaxed stores, plus the cells undone and redone to bring the least recently used table over |
| the switch | one release store of the cell | the same |
| a rollback | a store of a kept table | a store of a table that still holds the generation (the previous one is always pooled): 0 slot writes. Otherwise the least recently used table is brought over through the common ancestor. Rollback to the first generation stores the image's own table |
| the answer | encoded into a new `Vec`, then written | encoded into a buffer kept from answer to answer, paths made once, one write and a rename |

Syscalls per patch on Linux, after: `openat`, `fstat`, the reads and `close` for the
message, 0 for the code, 2 per run of read-only data pages, 0 or 2 for the cell, and the answer's
write and rename. Before, that was a read every 5 ms, 2 per code write, a maps read, 2 per data
piece, 0 or 2 for the cell, and the answer.

Latency from message to new code: before, up to 5 ms of sleep before the file was even read.
After, the program's next `poll` sees the watcher's count change and takes the patch. The work
between is the mapping, the checks (linear in the message), the writes, C stores and one release
store. With a few-function patch the syscalls above dominate (about ten), against the 5 ms floor
before.

Per call through a slot, nothing changes on the runtime side: the stub loads the cell, loads the
slot from the table it points at, and jumps. On x86-64 that is `mov r11, [rip+cell]; jmp
[r11+disp32]`, 2 instructions, 16 bytes. On AArch64 it is 6 (`adrp`, `ldr` of the cell, `movz`,
`movk`, `ldr` of the slot, `br`); 4 would do (request 3 below). The two loads are address
dependent, so the cell's release store orders every slot store before it without a barrier on the
call path.

## Why it stays correct

- **Generations by file name** (contract change 8): a program at generation g takes
  `message_file(g)` and nothing else. Before, the runtime read files by sequence number, which no
  longer matched what the linker writes. A file already taken (the same length, modification
  time and inode, or creation time on Windows; the last 64 are remembered) is not taken twice.
- **Tables rewritten only off the cell**: a pooled table is written only while the cell points
  at another. A thread that loaded the cell before the switch reads a whole slot of the old or
  the new generation's code, both mapped. Tables are never freed, so a table the cell ever
  pointed at stays readable.
- **The log is never pruned**: every generation's changed cells (slot, new and old target) and
  code ranges stay, about 20 bytes a cell. A table can then reach any kept generation from any
  other through their common ancestor, so rollback to any kept generation keeps v1's meaning.
- **Every error text the tests check is unchanged**; a patch that fails a check writes nothing.

Tests added: a model test that runs 2000 seeded patches and rollbacks (branching) on 16 slots and
compares the cell's table, slot by slot, against a whole table per generation. Also a test that a
rollback to a pooled generation needs no table writes and that rollback to the first generation
stores the image's table. A `put` test covers every range against a byte-by-byte model, with cuts
through spans and rebased fields. The new wire encoder is checked against the old one byte for
byte, and a view is checked to read in place. Two inotify filter and maps lookup tests run on
Linux. There are `const` size asserts on `Span` (24 bytes).

## Unsafe, by [the unsafe design](../../design/unsafe.md)

- **The message is read, not mapped.** A mapping of the file isn't fixed bytes: on Linux a
  write to the file shows through even a `MAP_PRIVATE` mapping. The switch reads the view
  again after checking it (the cells, to write them), so a changed file would break the
  `&[u8]` it holds, and a patch's bytes could differ from the ones checked. One read into
  the buffer kept from the last message costs a copy of the message, which is small next to
  the writes, and removes the mapping's unsafe on every platform. The bytes are then
  bounds-checked whole by `Message::view` before anything is written. Message files carry no
  content hash: they come from the linker through a 0700 directory of the user's, under a
  name keyed by generation, and nothing in them is transmuted.
- **What stays unsafe, and why**: writes to the running image (`Writes::put`, the page
  protections, the Linux alias, the macOS page swap), the stores to the table cell, the
  cache flushes, the loader and unwinder calls, the watcher's system calls and reading the
  descriptor and the loader's table. Each reaches the kernel or the hardware, or the image
  the program runs, and has no safe form. `Writes::fill` is the safe face of `put` for a
  buffer the caller owns (the macOS swap's copy, the tests).
- **One unsafe operation per block, each with a `// SAFETY:` comment**, in the new code. The
  multi-operation blocks are split: the mprotect-write-mprotect of Linux data, the AArch64
  cache flush (one `asm!` per block), the macOS page swap, and the Windows flush. The
  loader's table is read through one bounds-checked slice (`Switch::image`), not pointer
  arithmetic per slot.
- **Safe reference versions in tests**: `put` against a byte model over every range, the
  pool of tables against a whole table per generation, and the encoder against the old one,
  byte for byte.
- **Not yet bound by the workspace lints**: `blaze-live` has no dependencies and isn't
  `[lints] workspace = true` (the `safety-audit` lane lists it). It can take them once
  `v2/requests/link.md` item 12 is in. The unsafe in `slots/` is spread over the platform files rather
  than one `raw.rs`: each platform's file is the module that reaches its kernel, and the
  switch's unsafe is only `Writes::put`, `Switch::image` and the cell's first load.
- **For the safety lane**: the pool's relaxed slot stores published by one release store,
  read by the stub's address-dependent loads, is the lock-free structure to model-check. The
  writer is one thread, and the readers are any threads calling through a slot, possibly with
  a cell loaded before the switch. The property is that every slot read is the old or the
  new generation's whole value. `Message::view` is the untrusted-bytes parser to fuzz; the
  corruption test in `patch.rs` is a start.

## Safety audit

[speed-link-edge-live-safety](speed-link-edge-live-safety.md) has since fixed what this section
leaves as "NOT upheld". v1 now writes jumps and statics with every other thread stopped, and
`slots` writes data by atomic stores. Read that doc for the current state.

The `safety-audit` lane's findings on blaze-live are in `v2/requests/link.md`, under "## From
safety-audit". The answer, and what the linker owes, is item 12 there. Every proposed comment
and split is applied, in the v1 modules too, since blaze-engine, blaze-cargo, the demos and the
live harnesses still run on them. The checks below decline a patch rather than write it.

- **Lasting memory (`slots/switch.rs` `Lasting`)**: code of a generation once registered or
  published, its registered FDEs, each run's zero terminator and Windows UNWIND_INFO are never
  written again, even after a rollback or eviction, since a thread may still be in that code
  and the unwinder holds pointers into those tables (findings 7 and 8). This needs the linker
  to keep patch placement monotonic across rollbacks and give each EhFrame run its own
  terminator (item 12 a and b). Until then, such patches are declined.
- **Data placement (`slots/`)**: data over the loader's table is refused, and each platform
  checks that data lies in the image and in pages with some access (finding 10).
- **v1**:
  - A failed apply still moves `next` past the patch (finding 1).
  - Writes must lie in the executable's image (3).
  - macOS bounds are checked (4, 5), and so is the Windows function table (12).
  - Linux registers its `.eh_frame` run once, with its terminator found first (6).
  - The counters are atomics, loaded relaxed (11).
- **Stated, not fixed**: v1 writes jumps over function entries and statics under running threads
  (2), and both runtimes write data in place (9). The `lib.rs` and `slots` `poll` docs give the
  program the rule, and the comments mark these as "NOT upheld". The fixes belong to the linker
  (item 12 c and d).
- **Tests**: `registered_unwind_entries_are_never_written_again`, plus bad-patch cases for
  UNWIND_INFO past the writes and for data over the loader's table. The "third" step of
  `patches_switch_the_table_and_roll_back` now places code past the earlier generation's.

## What is left

- **macOS `MAP_JIT`**: XNU refuses `MAP_JIT` with `MAP_FIXED`, so the patch code region, which
  is in the image at a fixed address, can't be a JIT mapping. Toggling
  `pthread_jit_write_protect_np` once per patch needs the region mapped `MAP_JIT` by the loader,
  which the image's own pages never are. The page swap stays, one per merged run. Making it JIT
  means the linker placing the patch code region in a separate `MAP_JIT` mapping that the
  runtime makes at start, and the stubs reaching it. That is a contract change, not asked for
  here.
- **Windows dual mapping**: an image's section can't be remapped from a section object, so the
  RW alias Linux uses doesn't exist for image pages. One `VirtualProtect` pair per run is what's
  left.
- **Requests to lead `link`** (in `v2/requests/link.md`, item 10):
  1. `live.rs`'s `message()` copies each piece (`to_vec`) into an owned `Patch`. A `PatchRef`
     borrowing `self.new.bytes(piece)` encodes the same bytes with no copy, sized exactly
     (`encoded_len`, `encode_into`).
  2. `commit.rs` carries the `Message` and encodes it in the commit step. Carrying the encoded
     bytes, from item 1, makes that one buffer, written once by `Effect::Write` (temp and
     rename).
  3. `table.rs`'s AArch64 stub: `adrp; ldr x16, [x16, :lo12:cell]; ldr x16, [x16, #slot*8];
     br x16` is 4 instructions while slot × 8 < 32768. Adding `add x16, x16, #hi, lsl #12`
     makes 5, which covers up to 2^21 slots. Padded to 24 bytes, it replaces the 6 instructions
     with `movz`/`movk`/register load now.
