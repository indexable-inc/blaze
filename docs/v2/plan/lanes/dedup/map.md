---
status: plan
summary: dedup-map (2026-10-29): every read-only file map is one type, `blaze_core::fs::Mapped` (with `MapOptions` for populate, huge pages and madvise), memmap2 leaves the workspace; writable, anonymous and executable maps are sorted to their owners, with the copies that differ on purpose named.
date: 2026-10-29
---
# dedup: file mapping

Part of [dedup](../dedup.md). Every `mmap`, `MapViewOfFile`, `mach_vm_*` and `memmap2` in the
tree, sorted by what it maps.

## Read-only file maps: `blaze_core::fs::Mapped`

`crates/blaze-core/src/fs/mapped.rs`. The copies were `blaze-core`'s own two (`fs.rs` on Unix,
`iocp/files.rs` on Windows, each a private `Mapped` behind `Effect::Map`) and five crates on
memmap2: `blaze-store` (packs sealed and indexed), `blaze-cargo/src/remote.rs` (packs sent),
`blaze-splice` (the executable and the objects it reads), `blaze-engine` (an executable's stand-in
section, rlib metadata) and `blaze-v2-proof` (`bytes::Mapped`, outputs compared).

What it is:

- `MAP_SHARED`, `PROT_READ` of the whole file (`CreateFileMappingW` and `MapViewOfFile` on
  Windows); an empty file maps nothing (`Mapped::EMPTY`), as both blaze-core copies and the
  proof's did. `Deref<Target = [u8]>` and `AsRef<[u8]>`, so memmap2's call sites change only in
  the constructor.
- `Mapped::map(&File)` and `Mapped::open(&Path)` are `unsafe fn`s: the caller promises no one
  writes or truncates the file in place while it is mapped. That is a promise about other
  processes no type can check, so it stays the caller's, with the `// SAFETY:` comment every
  memmap2 call site already had. `Effect::Map` keeps its contract and calls it once, in
  `fs::map`, for every driver (threads, io_uring, kqueue, IOCP).
- `MapOptions`: `populate` (`MAP_POPULATE`; `MADV_WILLNEED` on macOS, `PrefetchVirtualMemory` on
  Windows), `huge_pages` (`MADV_HUGEPAGE` from 2 MiB, on by default, Linux only), `advise`
  (`Normal`, `Sequential`, `Random`, `WillNeed`). That covers every caller: `Effect::Map` (the
  default with huge pages, what `fs.rs` did), the proof (populate when one thread reads it all,
  lazy when cores fault their own pieces), the store's sealing and the remote's zstd of a whole
  pack (`Sequential`).
- Untrusted bytes (docs/v2/plan/design/unsafe.md): the type gives `&[u8]` and nothing typed.
  Every view of more than bytes is its reader's, after checking: `blaze-image` checks page-group
  CRC32Cs before a section is read, the store checks its index and record checksums, `object`
  bounds-checks what the splice and the engine parse.

What it is not: `blaze-vfs`'s `store::Mapped` is not a copy. It holds an `Image<Mapping>` and
checks the blobs section's page groups once each; it is a reader of the mapping, so it keeps its
name inside its crate.

### Speed of light

Opening is syscalls: `open`, `fstat`, `mmap`, at most two `madvise`, `close`, about 2 to 5 µs on
Linux whatever the file's size, the same calls memmap2 made (it took the open file). Nothing is
copied and nothing allocated (`Effect::Map` allocates its one `Arc`, as before).

Reading is page faults, then memory. A 1 GiB file read whole with 4 KiB minor faults, about
0.5 µs each, is 262,144 faults, about 130 ms; Linux's fault-around (64 KiB) makes that 16,384,
about 8 ms; the same bytes at 20 GB/s of memory bandwidth are 50 ms. So:

- a reader of the whole file pays the faults on top of the bandwidth unless it populates: the
  proof's single-threaded compare and the remote's zstd ask for `populate` or `Sequential`, which
  leaves the bandwidth (one `MAP_POPULATE` walks the page tables once in the kernel);
- a reader of a few pages (the image, a pack's index, an executable's headers) pays only the pages
  it touches; huge pages cut its TLB misses from one per 4 KiB to one per 2 MiB where the file
  system makes them (512 times fewer), which is the remaining cost of reading in place;
- `bytes()` is a pointer and a length, inlined: no allocation (tested at zero) and no indirect
  call. `Mapping` keeps its `Arc<dyn AsRef<[u8]>>` (the tests give it owned bytes), and `Image`
  takes its pointer once at open, so the indirect call is off the read path.

That is within 10% of the bound: the work above the hardware is the fixed handful of syscalls
per open, and memory bandwidth for the bytes read.

## Writable and shared maps: stay with their owners

Not files on disk, and each with an invariant of its own:

- `blaze-core/src/uring.rs`: the ring's own `mmap`s (`MAP_SHARED | MAP_POPULATE` of the ring
  file) and its registered buffers (anonymous, populated). The kernel's layout, the ring's alone.
- `blaze-inproc/src/sys/*`: memory objects (`memfd`, Mach memory entries, Windows sections)
  mapped writable in the daemon and executable in the run process, sent across processes.
- `blaze-usercode/src/test/fork_server/recorded.rs`: the execution record (`MAP_SHARED |
  MAP_ANONYMOUS`) is harness source text compiled into the user's test binary, which has no
  dependencies; it declares its own `mmap`. Kept.
- `blaze-stencil/src/tests/exec.rs` line ~839: a page shared with a forked child for its
  results. Kept.

## Anonymous memory: `blaze-mem`'s

`blaze-mem/src/os.rs` (reserve, commit, decommit, release) is the one owner; compared only.
`blaze-graph/src/arena.rs`'s chunk maps are arenas: dedup-mem's part and mem-core's file. The
others stay, on purpose:

- `blaze-check/src/coro.rs`: coroutine stacks with a guard page. blaze-check is a dependency of
  blaze-mem (under `cfg(blaze_check)`), so it cannot use blaze-mem.
- `blaze-cargo/src/sandbox/linux.rs` (a test's two pages, the second unmapped to bound a read)
  and `blaze-inproc/src/load/elf.rs` `anonymous` (`MAP_FIXED` zero pages inside the loader's
  reservation): each maps at a place of its own choosing, which no allocator does.
- `hir-ty-mir/src/mir/eval/shim/thread.rs` and `hir-ty-ub/src/memory.rs`: the evaluator's
  interpreted `mmap` and `munmap`, allocations in its own memory model, not system calls. Not
  copies.

## Executable code memory

Two shapes:

- **Live code replaced under running threads**, in blaze-live (`linux.rs`, `slots/linux.rs`
  `map_twice`, `macos.rs`, `slots/macos.rs` `swap`, `windows.rs`, `slots/windows.rs`):
  `MAP_FIXED` into a reserved room, `mremap` of a memory file over a live region,
  `mach_vm_remap` swaps. blaze-live is the lowest crate of all (no dependencies, linked into the
  user's program), so nothing else can be under it, and no other crate replaces live code. It
  stays, with a comment saying why (`v2/requests/link.md`).
- **Code written once, then run**: `blaze-lazy/src/tests/exec.rs`, `blaze-stencil/src/tests/exec.rs`
  and `blaze-inproc`'s `View::publish`, three copies of the AArch64 `dc cvau`/`ic ivau` loop and
  two of the macOS `MAP_JIT` dance. blaze-lazy already depends on blaze-inproc and blaze-inproc on
  neither test crate, so the one implementation is a `blaze_inproc::exec::Executable` behind its
  `sys` (`v2/requests/fastcompile.md`). Not in blaze-core: blaze-inproc's run process links without
  blaze-core, and the cache maintenance is already blaze-inproc's.
