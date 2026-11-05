---
status: plan
summary: The per-patch allocations of blaze-live's v1 runtime (the plan's bytes, its address sets, /proc/self/maps, FDE lists, the published process text) moved into buffers kept from patch to patch, the v2 switch held to zero allocations warm by a budget test, and every other allocation, clone and dyn in the crate justified as setup, error path or edge; before and after, counted from the code, nothing built or run.
date: 2026-10-29
---
# speed-link-edge-hot-live: the hot paths of blaze-live

Part of [link](../link.md), lane `speed-link-edge-hot-live` (a child of `speed-link-edge`).
Code: `crates/blaze-live/`. It follows [speed-link-edge-live](speed-link-edge-live.md), which
made the v2 patch path (`slots/`) read the message in place and keep its tables in a pool; read
that for the switch itself. Nothing here was compiled or run (v2 rule): the counts below are read
off the code, and the tests are written, not yet run.

## What it is measured against

The hot paths are applying a patch, a call through a slot, and the sampler.

- **A call through a slot** is unchanged: two dependent loads and an indirect jump, against
  Live++'s jump at the patched entry (one jump, after a patch only) and MSVC `/hotpatch` (a
  2-byte `mov edi, edi` until patched, two jumps after). The runtime adds nothing to it.
- **Applying a patch**: the state of the art (Live++, subsecond, Linux `text_poke_bp`) writes in
  batches and allocates per patch: Live++ per patched function, subsecond a whole table per
  patch. The target here is no allocation at all once warm, so a patch costs its syscalls and
  its copies and nothing from the allocator. That keeps the time to new code flat, and safe to
  take while other threads hold the allocator's lock.

## Before and after, per patch

v1 (`apply.rs` and `linux.rs`, `macos.rs`, `windows.rs`) is to be removed (link.md 20). Only
what it runs per patch was changed; its setup and error paths are justified where they are.

| | before | after |
|---|---|---|
| plan (`Overwrites::plan`) | a new `Plan`, a `Vec<u8>` per step, a `HashSet` of the patch's addresses (SipHash) and a `Vec` of the ones undone | one `Plan` kept by the runtime: one byte buffer that steps index as ranges, the reach index rebuilt in place, the kept addresses a sorted `Vec` searched by binary search |
| page protections (Linux) | `/proc/self/maps` read into a new `String`, parsed into a new `Vec`, protections collected into another | the same read, into a `String`, a `Vec` and a `Vec<i32>` the runtime keeps |
| unwind entries (macOS) | two new `Vec`s of CIEs and FDEs, a third returned | two kept `Vec`s, cleared and refilled |
| first values of statics | a new `Vec` per static | one kept buffer |
| statics' addresses and pages | `HashSet<u64>` (SipHash per insert and lookup) | sorted `Vec<u64>`, binary search; the smallest is `first()` |
| publishing the process | a path joined and a text encoded per patch | paths made once at start; the text encoded into one kept `String` |
| v2 switch, warm | 0 (no test held it) | 0, held by `a_warm_patch_allocates_nothing` |
| v2 rollback, warm | 2 | 2, held by `a_warm_roll_back_allocates_nothing`: the reason, once for the program and once for the linker |

What v1 still allocates per patch: its watcher thread reads the message with `std::fs::read`
and decodes it into an owned `Patch`, then sends that over a channel. That owned form is v1's
format, and v2 replaces it with `Message::view` over one kept buffer, so it is left as it is.
The first write at an address keeps that address's original bytes for good (to undo it later),
once per address, not per patch.

The sampler was not changed by this lane. What it allocates is its setup (its thread's name, its
paths, its answer buffer, which grows to the largest answer once).

## Lint

`/tmp/sle_lint.py crates/blaze-live/src` (allocations, clones and `dyn` on paths not marked as
setup, error path or edge). 302 findings when the lane started, 311 after `requests-live` took
`blaze-mem` out of the crate, 263 when this lane's second agent took over, and 230 after the
changes above. The other 226 lines (230 findings) were each justified on the line above,
mostly error paths (`format!` of an error) and setup (thread names, paths, the first read of the
image). That leaves 0. The four `Copy` types wider than 64 bytes are plain data or borrowed
slices with no heap buffer, and each says so: `Descriptor`, `PatchRef`, `PatchView` and
`MessageView`.

## Tests added

- `apply/tests.rs`: `a_kept_plan_gives_what_owned_steps_did` runs 500 seeded patches
  (`blaze_ids::rng::SplitMix64`) through one kept `Plan`. It compares every step's address,
  bytes and jump flag against a reference that builds owned steps the old way.
  `a_warm_plan_allocates_nothing` and `fdes_reuse_their_buffers` check with
  `blaze_alloc_count::assert_at_most(0, ...)`.
- `slots/tests.rs`: `a_warm_patch_allocates_nothing` (64 patches after the pool and the kept
  files have filled) and `a_warm_roll_back_allocates_nothing` (at most 2).

The counting allocator is `blaze_alloc_count::Counting`, a dev-dependency installed as the
global allocator under `#[cfg(test)]` only, so the published crate still depends on nothing.

## What is left

- Run the tests above in an ix VM. They were written under the v2 no-build rule.
- Remove v1 (link.md 20). That takes its per-patch decode and channel with it.
