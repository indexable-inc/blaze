---
status: plan
summary: lane hugepages - which of blaze's big, randomly read regions get 2 MiB pages on Linux (transparent huge pages, or the reserved hugetlb pool by BLAZE_HUGE_PAGES=reserved with an error when it has none) and what macOS on Apple silicon offers (nothing for anonymous memory), the region-by-region audit, and how to measure TLB misses on a build.
date: 2026-10-30
---

# Lane hugepages: huge pages for big, randomly read memory

Code: `crates/blaze-mem/src/huge.rs` (the choice), `src/region.rs` (how a region of huge pages
commits), `src/os.rs` (the kernel calls), tests in `src/tests.rs` ("Huge pages"), the bench
`benches/huge.rs`. Speed of light: [speed-of-light.md](speed-of-light.md).

## The choice, once per process

`BLAZE_HUGE_PAGES` is read at the first `Region::reserve_huge`:

| value | pages | notes |
|---|---|---|
| unset, `transparent` | Linux THP (`MADV_HUGEPAGE`) when `/sys/kernel/mm/transparent_hugepage/enabled` is not `[never]`; base pages elsewhere | no setup; the kernel may give base pages when memory is fragmented (that is the kernel's call, not a blaze path) |
| `reserved` | the 2 MiB hugetlb pool (`MAP_HUGETLB`) | Linux only; an error at the first huge reservation when `nr_hugepages` is 0, and an error from the commit when the pool has too few free pages; never base pages instead |
| `off` | base pages | to measure what huge pages save |
| anything else | error naming the value and the choices | |

The daemon reads it once: change it, then restart the daemon.

## How a region of huge pages commits

Every Unix reservation is 2 MiB-aligned (`os::RESERVE_ALIGN`). A region of huge pages is
reserved in whole 2 MiB pages and:

- while it is at most 2 MiB long, commits base-page granules (64 KiB) like any region, so the
  hundreds of small tables (one memo column per tracked function and struct, a column per
  interner) cost a few base pages each instead of a 2 MiB page each;
- past 2 MiB, every commit ends on a 2 MiB boundary, so each huge page is committed whole
  before it is touched: under THP its first touch faults in one huge page; under the pool it
  is mapped from the pool and populated at once (`MADV_POPULATE_WRITE`), so a short pool is an
  error at the commit, never a `SIGBUS` at a later touch;
- under THP, the commit that takes it past 2 MiB collapses the first 2 MiB, made of base pages
  while it was small, into a huge page (`MADV_COLLAPSE`, Linux 6.1; advice, contents kept).
  Under the pool the first 2 MiB stays base pages, so a small region takes nothing from it.

Pool regions also: grow under one process-wide lock (a hugetlb mapping replaces what is under
it, so two growers must never map the same range; growth is at most once per quarter of a
region's size, so the lock is cold); are `MADV_DONTFORK` (a write after `fork` never needs a
pool page for copy-on-write); keep their pool pages through `purge` (a page given back would be
faulted again on the next touch, which kills the process if the pool is empty). A decommit
gives pool pages back whole.

## macOS and Windows

macOS on Apple silicon has no huge pages for anonymous memory. XNU's superpages
(`VM_FLAGS_SUPERPAGE_SIZE_2MB` to `mmap`/`mach_vm_allocate`) exist on x86-64 only;
`vm_map_enter` returns `KERN_INVALID_ARGUMENT` for them on arm64. The test
`apple_silicon_has_no_superpages_for_anonymous_memory` asserts the refusal, so it fails on the
day macOS offers them. What Apple silicon has instead: 16 KiB base pages, four times x86-64's
reach per TLB entry, and a large shared second-level TLB (about 3072 entries on M1's
performance cores, about 48 MiB of reach). So on macOS `transparent` means base pages and
`reserved` is an error. Windows' large pages must be committed whole when reserved
(`MEM_LARGE_PAGES`, with `SeLockMemoryPrivilege`), which commit-on-growth cannot do: base pages.

## The audit

"Before" is the tree as this lane found it, on Linux with THP `madvise` (the default on most
distributions). Every row is a `blaze-mem` region or arena, or a big table outside them.

| memory | read | before | now |
|---|---|---|---|
| `chunks` spaces (frames' and arenas' 2 MiB chunks) | random | huge | huge (each chunk is committed whole) |
| frame spans, arena large blocks | random | huge, at least 2 MiB committed each | base below 2 MiB, huge past it |
| `Column<T>` values (interners, salsa's page list, base-db logs) | random | huge, at least 2 MiB each | base below 2 MiB, huge past it |
| `Column<T>` ready bits | random | base | base (one bit per value) |
| intern `space` (values, probe tables) | random | huge | huge |
| intern `Slots` (ID to record) | random, every lookup | base | huge |
| salsa memo columns: `rows`, `slab` (`table/memo.rs`) | random, every hit | base | huge |
| salsa graph pages: node keys, colors, edges (`graph.rs` `Zeroed`) | random | base | huge |
| salsa slot arenas, the node table (`table.rs` `PageArena`) | random | base | huge |
| salsa node map and interned SwissTables (`graph/nodes.rs`, `interned/table.rs`) | random | base, from the global allocator | unchanged: see below |
| blaze-query hot rows and maps (`Rows::hot`, `Map::hot`) | random | huge | huge |
| blaze-query cold rows, live flags, generations | rare | base | base |
| blaze-cas in-memory index: `Values`, `Words` (`table.rs`, `words.rs`) | random | base | huge |
| blaze-cas segment chunks | appended, read by place | base | base |
| blaze-cas mapped index files (`blaze-core/src/fs/mapped.rs`) | random | `MADV_HUGEPAGE` from 2 MiB | unchanged: page-cache huge pages need a filesystem with large folios (XFS, recent ext4) or `CONFIG_READ_ONLY_THP_FOR_FS`; the kernel already aligns such mappings to 2 MiB (`thp_get_unmapped_area`) |
| blaze-link symbol index (`intern.rs` `Index`) | random | base | huge (past 2 MiB a shard: images of millions of symbols) |
| syntax trees (`syntax-tree/src/flat/block.rs`) | random | base, one global-allocator block per tree | unchanged: see below |
| I/O buffers, watch tables, splice and journal columns | sequential or small | base | base |

Two of the tables the lane brief names are not `blaze-mem` memory and need their owners' design:

- **salsa's node map and interned tables** are per-shard SwissTables from the global allocator
  (mimalloc), one per shard per doubling, the old one freed a revision later. At 16 Mi nodes
  they hold about 120 MiB probed at random, on base pages. Sharding keeps each table under
  2 MiB, so a region per table would stay base pages; they need one huge region per `Nodes`
  that tables are carved from and returned to. Requested of lane db.
- **the syntax tree** has no arena: each tree is one block from the global allocator, freed when
  an edit replaces it. Putting blocks on huge pages means a huge-page slab for them. Requested
  of lane format.

## Measuring on the first build

The policy is per process, so restart the daemon between runs. On an ix VM (system-wide, so the
daemon's threads count):

```sh
# Intel
perf stat -a -e cycles,instructions,dTLB-loads,dTLB-load-misses,dtlb_load_misses.walk_completed,dtlb_load_misses.walk_active,dtlb_store_misses.walk_completed -- blaze build
# AMD Zen
perf stat -a -e cycles,instructions,ls_l1_d_tlb_miss.all,ls_l1_d_tlb_miss.all_l2_miss,ls_tablewalker.dside -- blaze build
# arm64 (Graviton)
perf stat -a -e cycles,instructions,l1d_tlb_refill,l2d_tlb_refill,dtlb_walk -- blaze build
```

Run each with `BLAZE_HUGE_PAGES=off`, unset, and (after `sysctl vm.nr_hugepages=4096`)
`reserved`. With THP `always`, `off` still gets huge pages: set
`/sys/kernel/mm/transparent_hugepage/enabled` to `madvise` first. Confirm what the daemon holds
with `grep -E 'AnonHugePages|Private_Hugetlb' /proc/<daemon pid>/smaps_rollup`. The number to
read is page-walk cycles over all cycles (`walk_active / cycles` on Intel): the share of the
build the TLB costs.

The bench holds one table to the bound: `blaze bench -p blaze-mem --bench huge`.
