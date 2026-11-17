---
status: plan
summary: bstd programs run with no global heap. The `#[global_allocator]` is a refuser; each of bstd's 94 `// alloc:` sites moves to the owner of its lifetime (the world, a task's frame, a request's frame, a registered span); a test runs every bstd example under the refuser.
date: 2026-11-12
---
# No global heap under bstd

sys.md's runtime table decides the global allocator: none, a refuser that aborts naming the size
and the site, once "effects stop owning their bytes". This is how every remaining use goes, so the
refuser can go in. Asked for by Andrew, 2026-11-12 ("what is best long term? probs do that").

## The end state

- A program on bstd links `alloc` for its types only. Its `#[global_allocator]` is bstd's refuser:
  any call aborts through the panic path with the size and the `#[track_caller]` site. Tests on
  `std` keep std's allocator but count it (`blaze-alloc-count`), and the simulation runner fails a
  test whose bstd code allocated globally, naming the site.
- No `// alloc:` line remains in `crates/blaze-std/src` outside tests. The rule bump-allocate's
  escape hatch is gone for bstd, not just used rarely.
- Every allocation has an owner whose lifetime is the value's, from this table.

| class (sites on 3ca7012626) | owner | why this owner |
|---|---|---|
| once per process or world: the driver, the simulated host, disk ledger, environment, command line, recorder (~35) | the world's region: one `blaze_mem` region made by `rt` at entry from the page source, freed when the world ends | lives exactly as long as the world; one reservation, no per-object free |
| effect bytes (`Effect::Append { bytes: Vec<u8> }`, sends, receives) (~5, plus blaze-core) | a registered span (`Spans::register`) in the caller's region, the same path `ReadInto`/`WriteFrom` already use | zero copy, and io_uring's registered buffers are the fastest path the kernel has |
| names shared by clones: `FileName(Arc<str>)`, `Name`, env names, `Url` parts, header names (~20) | long-lived names (paths, env, capability names): the world's interner, a handle of one word, compared by id. Per-request text (a URL, a header value): the request's frame, borrowed with its lifetime | interning dedups and makes a clone free; per-request text must not grow the world for a server's lifetime |
| errors' messages (~6) | none: an error holds what it is about as data (a handle, a size, an errno), and `Display` writes into the caller's formatter | errors-name-the-thing needs the thing, not a copied string |
| panic payloads crossing a task's end (`task.rs`, `io.rs:1043`) | the joiner's slot: a fixed buffer in the waiting task's frame, the message truncated to it and marked so | the dead task's frame is gone; the joiner is the one owner left |
| driver queues grown to their peak (effects queue, poll buffer, completions, stopped tags) (~5) | fixed rings made once in the world's region, sized from the world's budget; a full ring is a typed `Busy` the state machine waits on | bounded-progress: a bound, not a peak |
| per-scope and per-task owners (`cx.rs` cancellation, frame owners) (~3) | a slab in the world's region, freed on scope end | a scope's lifetime is the slab slot's |
| buffers shared with foreign threads (VideoToolbox's outbox) (~4) | a bounded `crossbeam` `ArrayQueue` made once in the stream's owner | the encoder's threads have no frame of ours; a fixed queue needs no allocator after creation |
| simulation-only records (term, device, hosted, disk undo) (~15) | the simulated world's region | they live as long as the simulated world |

## Floors

- Memory: the floor is the live bytes; a region wastes at most its last chunk. The chunk size and
  the ratio to the floor are not measured yet; the first lane measures them on the examples.
- Time: an allocation becomes a bump and a name clone a word copy, against malloc and an atomic
  increment today. Not measured; the check quotes both from the runner's spans.
- Disk: none.

## Order

1. The world's region and slab in `bstd::rt`, and the owners above, one class per lane, each in its
   own files (they don't overlap: io.rs and the drivers; names; errors; effects in blaze-core;
   simulation records; videotoolbox).
2. The refuser and the counting runner; every `examples/` program and every bstd test runs under
   it. The check: `rg '// alloc:' crates/blaze-std/src` outside tests is empty and the runner's
   global-allocation count is 0 on every test.

Builds after D29 (bstd-fallible-alloc), which changes the same files.
