---
status: plan
summary: lane hugepages' speed of light - a random read of a big table costs one DRAM access and no page walk when its pages fit the second-level TLB, the arithmetic for base and 2 MiB pages on the machines blaze targets, and the bench that holds blaze-mem to it.
date: 2026-10-30
---

# Lane hugepages: speed of light

The bar is "Within 10% of the theoretical maximum" (`docs/v2/plan/goals/principles.md`).

## A random read

The least a random read of a cold line of a big table costs is one DRAM access: about 80 to
110 ns load to use. Everything the TLB adds is a page walk on top: on a miss of both TLB
levels, x86-64 walks four levels and arm64 (4 KiB granule) four, the upper ones in the
paging-structure caches, the last one a load of a page-table line that is itself often an L2,
L3 or DRAM miss (20 to 100 ns more).

A walk happens when the table's pages outnumber the second-level TLB:

| core | second-level TLB entries | reach on 4 KiB pages | reach on 2 MiB pages |
|---|---|---|---|
| Intel Skylake to Raptor Lake | 1536 (4 KiB and 2 MiB shared) | 6 MiB | 3 GiB |
| AMD Zen 2 and 3 | 2048 | 8 MiB | 4 GiB |
| AMD Zen 4 | 3072 | 12 MiB | 6 GiB |
| Arm Neoverse N1 (Graviton 2) | 1280 | 5 MiB | 2.5 GiB |
| Arm Neoverse V1 (Graviton 3) | 2048 | 8 MiB | 4 GiB |
| Apple M1 performance core | about 3072, 16 KiB pages | about 48 MiB | none for anonymous memory |

blaze's memo columns, node and edge pages, slot arenas and intern slots run from tens of MiB to
several GiB on a big workspace. On 4 KiB pages almost every random read of them past the first
few MiB walks; on 2 MiB pages a table of up to 3 GiB walks never. So the bound is: **on huge
pages, zero page walks per random read of a table that fits the TLB's reach**, and the read
costs its one DRAM access.

## How blaze-mem reaches it

- Every Unix reservation is 2 MiB-aligned, and a region of huge pages commits whole 2 MiB pages
  past its first, so no huge page is ever cut by a protection boundary (which would leave base
  pages).
- The first 2 MiB is collapsed into a huge page when the region outgrows it (THP), so a big
  table has no base-page head.
- Small regions stay on base pages: a huge page for each of the hundreds of small tables would
  cost hundreds of MiB of resident memory for TLB reach the small tables do not need.
- A commit is one `mprotect` (THP) or one `mmap` and one `madvise` (pool), at most once per
  quarter of growth: `O(log n)` system calls, as before.

The state of the art is the same rule in every allocator that does this well: mimalloc's and
jemalloc's 2 MiB-aligned segments with `MADV_HUGEPAGE`, TCMalloc's hugepage-aware allocator
(Hunter et al., OSDI 2021), and the JVM's and PostgreSQL's `huge_pages` choice between THP and a
reserved pool. blaze goes past them in committing the first 2 MiB small and collapsing it once,
and in populating pool pages at commit, so a short pool is an error and never a `SIGBUS`.

## The machine (speed-sol2-small)

As [link/speed-of-light.md](../link/speed-of-light.md)'s "The machine" (3.5 GHz; `mmap` and
first touch of a 4 KiB page 0.25 µs; `mprotect` 0.5 µs) plus the TLB table above, and DRAM 80
to 110 ns load to use. Reference workloads: a warm `blaze check` of a big workspace (16 Mi salsa
nodes, memo columns and slot arenas of 1 to 4 GiB, an intern space of hundreds of MiB, memo
values in the general allocator of several GiB), and a cold build of the same.

## Each table, counted (speed-sol2-small)

The bound for each is the one above: zero walks per random read while the table fits the
second-level TLB's reach on 2 MiB pages; a table on base pages past ~6 MiB pays about one walk
per random read, 20 to 100 ns on an 80 to 110 ns read, **1.2× to 2× off**.

| table | memory | pages today | walks per random read | queue |
|---|---|---|---|---|
| blaze-mem chunks, frames, request arenas, `Column` values | regions | huge past 2 MiB | 0 | none |
| salsa memo columns (`table/memo.rs` `Column::new`), graph pages (`graph.rs` `Zeroed::new`), slot arenas (`table.rs` `PageArena::new`) | regions | huge (this lane) | 0 | none |
| intern slots (`intern/src/space.rs` `Slots::new`) | region | huge (this lane) | 0 | none |
| store index (`blaze-cas` `Values`, `Words`), link symbol index | regions | huge (this lane) | 0 | none |
| salsa node map and interned SwissTables (`graph/nodes.rs`, `interned/table.rs`) | general allocator, sharded under 2 MiB each | base | ~1 past 6 MiB in all (~120 MiB at 16 Mi nodes) | 00809 (db) |
| syntax trees (`syntax-tree` `flat/block.rs` `Block::new`) | general allocator, one block a tree | base | ~1 on a big workspace's trees | 00810 (format) |
| memo values (HIR bodies, inference results, MIR) | general allocator (mimalloc) | base, unless the kernel's THP is `always` | ~1 | 00893 (coordinator), 00894 (codegen) |
| mapped store and image files (`blaze-core` `fs/mapped.rs`) | page cache | `MADV_HUGEPAGE` from 2 MiB on Linux; huge only where the filesystem has page-cache huge pages (tmpfs with `huge=`, ext4 and xfs large folios on recent kernels) | 0 to ~1, by filesystem | none: the filesystem decides |

### The general allocator (speed-sol2-small)

What the general allocator serves is a build's largest randomly read memory after salsa's own
tables, and nothing tells mimalloc to use huge pages: `libmimalloc-sys` is built without its
`extended` feature and no code sets `mi_option_large_os_pages`. Under THP `madvise` (the common
distribution default) every mimalloc segment is on 4 KiB pages. The fix is one option set at the
daemon's start when `blaze_mem::huge::chosen()` is `Transparent` (mimalloc 2.3.2 then advises
`MADV_HUGEPAGE` on its 2 MiB-aligned segments): queued to the coordinator (00893, the feature)
and lane codegen (00894, the call in `blaze-engine` beside `static ALLOC`), since neither path is
this lane's and codegen is not done. With it, every table in the table above but the page-cache
row is at the bound on Linux.

### Commits (speed-sol2-small)

The work of a huge region's growth is the kernel zeroing its pages (10 to 20 GB/s a core, 100 to
200 µs per 2 MiB page) plus one `mprotect` (THP) or `mmap` and `madvise` (pool) per commit,
at most one per quarter of growth: for a 1 GiB table, about 20 calls, 10 µs, against 50 to
100 ms of zeroing. The one collapse of the first 2 MiB (`MADV_COLLAPSE`) copies 2 MiB once,
about 0.2 ms per table that outgrows it. **Within 10% of the zeroing bound.**

### macOS (speed-sol2-small)

Apple silicon offers no huge pages for anonymous memory (a test asserts the superpage `mmap` is
refused); its 16 KiB pages and ~3072-entry TLB reach about 48 MiB, so a table past that walks
there and nothing blaze can do removes it. The bound on macOS is that reach; blaze-mem meets it
by never splitting a region's pages.

## What `proof` should measure (speed-sol2-small)

- The bench below: walks per read on huge pages (≤ 0.1) and the time per read on base pages
  against huge (the difference is the walk).
- On a warm `blaze check` of a big workspace: `dtlb_load_misses.walk_active / cycles` (Intel),
  `ls_tablewalker.dside` per instruction (AMD) or `dtlb_walk` per instruction (arm64), unset
  against `BLAZE_HUGE_PAGES=off`, and the share of the daemon's anonymous memory on huge pages
  (`AnonHugePages / Anonymous` in `smaps_rollup`): the target is every table above but the
  page-cache row, so over 90% of anonymous memory once 00809, 00810 and 00894 land.

## The bench

`crates/blaze-mem/benches/huge.rs` chases one random cycle through the 64-byte lines of 1 GiB
(512 huge pages, inside every core above) on base pages (`MADV_NOHUGEPAGE`), THP and the pool,
8 Mi dependent reads each, and counts page walks with `perf_event_open` (Intel `dTLB-load-misses`,
AMD PMCx045 L2-miss units, Arm `DTLB_WALK`). Target: at most 0.1 walks per read on huge pages
(the bound, 0, within 10% of a read). Expected: about 1 walk per read on base pages, and the
huge modes faster per read by the walk's cost. It exits 1 off the bound and 2 where there is no
counter to measure with.
