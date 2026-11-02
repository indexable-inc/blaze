---
status: plan
summary: How blaze-live answers the safety audit for real, not by comment: v1 writes its jumps and statics with every other thread stopped and none inside the bytes (refusing the patch naming the function otherwise), its patch room is claimed before anything is mapped, every address, FDE and function table of a patch is bounds-checked with checked_add before a view or write, slots writes data by atomic stores, unsafe is confined to the platform modules, and what the linker still owes; nothing in it was built or run.
date: 2026-10-29
---
# speed-link-edge-live-safety: blaze-live's safety findings, fixed

Part of [link](../link.md), lane `speed-link-edge-live-safety` (a child of `speed-link-edge`),
after [speed-link-edge-live](speed-link-edge-live.md). It answers the safety audit of
`crates/blaze-live` ("From safety-audit", `### blaze-live`, in `v2/requests/link.md`, each finding
marked there), by [the unsafe design](../../design/unsafe.md). Nothing here was built or run: this
lane may not build; the tests below are written, not yet run.

## Safety

### Where unsafe is

- `lib.rs` has `#![deny(unsafe_code)]`. `#![allow(unsafe_code)]` is only in the modules that call
  the kernel or the loader: `linux.rs`, `macos.rs`, `windows.rs` and their `stop` modules,
  `sample.rs`, `slots/{unix,linux,macos,windows,image}.rs`, and the slots tests' fake memory.
  Two items allow it on their own: `#[unsafe(no_mangle)] blaze_live_counters` and one layout test
  in `patch.rs`.
- `apply.rs` has `#![forbid(unsafe_code)]`. It holds every decision the three v1 platforms share,
  tested on every host: where a patch goes in the room, that each write is in the image, the
  unwind entries given to the unwinder, the writes and the originals they put back, and the
  stop-the-world loop.
- `Cargo.toml` takes `[lints] workspace = true`, so `unsafe_op_in_unsafe_fn`,
  `clippy::undocumented_unsafe_blocks` and `clippy::multiple_unsafe_ops_per_block` apply here.
  Each unsafe block is one operation with a `// SAFETY:` comment naming the check it rests on;
  each `unsafe fn` has a `# Safety` section.

### The twelve MAY BE UNSOUND items

1. **Failed apply.** `apply::claim` moves `process.next` past a patch's room before anything is
   mapped. A failed apply's memory is kept, never undone and never reused, so a jump or a frame
   registered before the failure keeps pointing at the bytes it was made for.
2. **Jumps under running threads.** v1 writes every jump, static and put-back with every other
   thread stopped:
   - Linux: a real-time signal whose handler parks on a futex and runs `cpuid` before it
     returns.
   - macOS: `thread_suspend`.
   - Windows: `SuspendThread`.

   Each platform reads every stopped thread's pc. No write is made while a pc is strictly inside
   its bytes: a thread at the first byte runs the whole jump when it resumes, and one anywhere
   else never runs the bytes written. Otherwise the threads are let go, then stopped again,
   `TRIES` (20) times, 500 µs apart. After that the patch is refused, naming the function.

   On AArch64, a jump over a call among the words it covers is refused (`Overwrites::calls`).

   A write that fails partway is `Failed::Restart`; nothing written is `Failed::Declined`. A
   half-written jump is never left. Nothing allocates while the threads are stopped (the
   allocator's lock may be held by one): the plan, room for the originals, and the error values
   are all made before.

   What a pc can't show is asked of the linker (`link.md` item 15):
   - a branch target inside the covered bytes;
   - an x86-64 return address inside them.

   The one-store alternatives the brief names fall short. An 8-byte aligned store covers only
   8-aligned entries, and still leaves a thread inside the bytes, which only a stop fixes. The
   4-byte AArch64 `b` reaches only ±128 MiB, and the patch room is past the image. So the stop is
   the fix, and the store is not needed under it.
3. **Unchecked addresses**, and also 10 and 12:
   - `apply::in_image` (`checked_add`) refuses every write outside the image before anything is
     mapped. Each platform also checks that a write lies in one readable mapping (`protections`,
     `readable`).
   - In `slots`, the descriptor, the cell, the loader's table and each data span are checked to
     lie in the image before any view: `Memory::bytes`, `cell`, `in_image`, `loaders`.
   - The function table is checked by `apply::functions`.
4. **macOS bounds math** uses `checked_add` in `apply::claim`, shared by all three platforms.
5. **FDE checks before registering.**
   - Apple: `apply::fdes` checks every entry inside the text, each FDE's CIE pointer naming an
     earlier CIE, before the first `__register_frame` of one FDE.
6. **Zero-terminated run.**
   - libgcc and ELF libunwind: `apply::frame_run` finds the one run ending in a zero length
     inside the text, registered once.
7. **Deregistering.** `slots` never writes over lasting memory: the code of every published or
   registered generation, every registered FDE, terminator and UNWIND_INFO. A registration so
   never needs taking back. v1 never writes into a patch after mapping it.
8. **Code over a generation a thread may be in** is refused: no thread can be proved out of it.
9. **Data races.**
   - `slots` data goes through `Writes::store`: relaxed atomic stores of naturally aligned 8, 4,
     2 and 1 byte pieces, from a 256-byte stack chunk.
   - v1 statics are written with the threads stopped.
   - A program reading a static wider than 8 bytes during a `slots` write may see it torn (old
     and new halves). That is not UB, and fixing it is the linker's item c of `link.md` item 12.
10. See 3.
11. **Counters** are a `static [AtomicU64; COUNTERS]`, loaded relaxed (done by
    speed-link-edge-live). The daemon's adds must be atomic too: `link.md` item 12 d.
12. See 3.

The other findings (comments, `# Safety` sections, one operation per block) were applied by
speed-link-edge-live. I rechecked them, and rewrote those whose code changed here.

### Tests of what is refused

| Case | Test |
| --- | --- |
| Address outside the image, or wrapping | `apply` `a_write_outside_the_image_or_wrapping_is_refused`; `slots` `a_table_outside_the_image_is_refused` |
| Wrapped `end`, patch past the room | `a_patch_past_the_room_or_memory_is_refused` |
| Failed apply, then another patch | `a_failed_apply_keeps_its_room_from_the_next_patch` |
| FDE past its section, or a bad CIE | `each_fde_is_inside_its_section_and_names_a_cie_before_it`, `a_run_of_entries_ends_in_a_zero_length_inside_the_patch` |
| Function table outside the patch | `runtime_functions_are_whole_and_inside_the_patch` |
| Write over a generation still entered | `slots` `patches_switch_the_table_and_roll_back` ("can be running"), `registered_unwind_entries_are_never_written_again` |
| Thread inside a jump | `a_pc_strictly_inside_a_write_is_found`, `writes_wait_for_a_thread_inside_them_and_then_refuse` |
| Call under an AArch64 jump | `a_call_among_the_words_a_jump_covers_is_found` |
| Partial failure | `a_write_that_breaks_after_others_says_restart` |
| No allocation while threads are stopped | `reading_the_originals_allocates_nothing` |
| Each platform stops every other thread and lets it go | `every_other_thread_stops_and_goes_on` in `linux/stop.rs`, `macos/stop.rs`, `windows/stop.rs` |

There are also safe reference versions, checked to give the same bytes:
- `slots/image.rs` `atomic_stores_give_the_bytes_plain_copies_do`: the atomic data store against
  plain copies, at every misalignment and over rebased fields.
- `pieces_are_naturally_aligned`.

The v1 writes have no safe reference to compare with: they are one `copy_nonoverlapping` under
the stop. What they write is planned in safe `apply` code and tested there.

### Speed

- **slots.** The call path is unchanged: one release store per cell, one `mprotect` pair per run,
  no lock. A data write now copies each 256-byte chunk to the stack and stores it in aligned
  pieces, in place of one `memcpy`. That costs about one more pass over the data, which is small
  beside the message read.
- **v1.** A patch now stops the world once, plus once per retry:
  - Each stop costs some tens of µs per thread: a signal and a futex wake on Linux, or a suspend
    and resume on macOS and Windows.
  - Under a VM it also costs a `cpuid` exit per thread.
  - This is what a correct jump write over running threads costs. The way out is moving v1 onto
    `slots`.
