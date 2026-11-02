---
status: plan
summary: blaze v2: the parallel in-place linker and live patching.
date: 2026-10-29
---
# blaze v2: link

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## Linking (from section 9)

- **Linker**: parallel and in place. Changed functions are placed in per-thread chunks of
  address space and written independently; relocations are applied in parallel; callers of a
  moved function go through a jump table (one slot changes) or are patched in parallel; the
  symbol table is a sharded map. The only serial step is the commit: one atomic switch that
  makes the new image visible, so nothing runs a half-patched binary. Full links are parallel
  the same way.
- Live patching (blaze-live) uses the same blobs and commit.

## Issues this lane takes in

- **#275 patchable function table** and **#232 rollback of a panicking patch**: the in-place
  linker's jump table is the patch table. (`v2-link`)
- **#276 speculation for agents**: what-if views, best-of-N candidates sharing every unchanged
  result, patching before checks finish with rollback. Views (section 2) make it nearly free.
  **#1302** lazy borrow checking is allowed only on this live-patch path, never for a build's
  output (cargo's rule: a borrowck error means no binary). (`v2-db`, `v2-link`)

## Done when

- full and incremental links of ELF, Mach-O and COFF, in parallel, with one atomic commit
- debug info and panic locations from side tables; live patching with rollback
- C objects, archives and shared libraries of the link line
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
