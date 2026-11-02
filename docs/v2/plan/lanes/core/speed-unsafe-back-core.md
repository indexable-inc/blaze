---
status: plan
summary: speed-unsafe-back-core's pass over blaze-core under docs/v2/plan/design/unsafe.md: a root `deny(unsafe_code)` with unsafe only in the kernel drivers, the mapping and `raw.rs`; every block one operation with its SAFETY; kernel completion records range- and generation-checked before they name memory; mapped files bounded by the length captured at map time; the stamp job's and IOCP's wake handoffs model-checked.
date: 2026-10-30
---
# speed-unsafe-back-core: blaze-core's unsafe, module by module

## The gate

`src/lib.rs` says `#![deny(unsafe_code)]`. `#[allow(unsafe_code, reason = ..)]` sits only on the
`mod` lines of the kernel drivers and the mapping: `fanotify`, `fsevents`, `iocp`, `kqueue`,
`net`, `raw`, `uring`, `watch` (in `lib.rs`), `mapped` (in `fs.rs`) and `spawn` (in
`process.rs`). Everything else is safe Rust: `lib.rs`, `fs.rs`, `process.rs`, `cgroup.rs`,
`footprint.rs`, `threads.rs`, the simulation, the wheel, `handoff.rs` and `sync.rs`. The unsafe
those files had moved into `raw.rs` (one system call per safe function) or into the driver it
belonged to. `examples/drivers.rs` forbids unsafe code; `examples/pingpong.rs` denies it, with
an allow on its one `sched_setaffinity` function.

Blocks (`unsafe {`, `unsafe fn`, `unsafe impl`): 448 at the start, 426 now. Every block has
`// SAFETY:` directly above it (`/tmp/speed-safety-scan2.py` finds none without), holds one
unsafe operation (the four that held more, in `iocp/rundirs.rs`, `iocp/sys.rs` twice and
`iocp.rs`'s `drop`, are split), and every `unsafe fn` has a `# Safety` section with its
operations in inner blocks.

## What the safe version would cost

Each unsafe module's doc says why it is unsafe and what the safe version would cost:

| module | why unsafe | safe version's cost |
|---|---|---|
| `uring` | rings shared with the kernel, buffers and names lent by address | a system call and a blocked thread per operation, where a ring takes a batch in one `io_uring_enter` or none (`SQPOLL`) |
| `iocp` | overlapped I/O lends an `OVERLAPPED` and a buffer until the packet comes back | a blocked thread per operation |
| `kqueue` | no kqueue in std | a thread per descriptor |
| `fsevents` | CoreFoundation, FSEvents and dispatch are C | a walk and a `stat` per file per poll |
| `fanotify`, `watch` | no fanotify or inotify in std | `read_dir`, `read_link` and `canonicalize` allocate per entry; `read_dir` also `stat`s per entry where `getdents64` gives the kind |
| `net` | descriptors handed between drivers; `recv` into spare room | `Read::read` needs initialized bytes: zeroing the whole buffer before every receive |
| `fs::mapped` | `mmap`, `MapViewOfFile` | reading the whole file: a copy at memory bandwidth and the memory to hold it |
| `process::spawn` | `clone(CLONE_VM \| CLONE_VFORK)` running a child | `Command`'s fork: a page-table copy per spawn of a large daemon |
| `raw` | single calls std lacks; `Aligned`'s byte view | a copy per read of `Aligned`; std has no `poll`, `waitid`, `clonefile`, one-call `utimensat` |

## Untrusted input and its checks

- **Mapped files** (`fs/mapped.rs`). The length comes once, at map time, from `fstat` of the
  descriptor being mapped, and the mapping is exactly that long. `Mapped::bytes` is a slice of
  that length, so every index safe code makes is checked against it. On Windows the section
  and view are now asked for that exact `len`: before, a file cut short between `fstat` and
  `CreateFileMappingW` gave a view shorter than the slice. Windows refuses a read-only section
  longer than the file, so a file cut short now fails to map. `SIGBUS` stance (module doc): the
  file is
  replaced only by rename (`Effect::Map`'s documented contract, named in each SAFETY), and no
  handler is installed. A cut-short file stops the process at the faulting read and nothing
  continues with bytes it did not read. A `sigsetjmp` around each read would put a branch and a
  thread-local write on every read. `Mapped` hands out bytes, never a typed view: readers check
  the content hash and every offset first (`blaze-image`).
- **io_uring completions.** `user_data` is a slab handle (`of_data`), looked up by index and
  generation (`ops.get`). It is never used as a pointer, and a stale completion is dropped. The
  provided-buffer id in a completion's flags is now checked against the group in every build
  (`Provided::buffer`, one compare per 16 KiB received); before, release builds trusted it as an
  offset. `Map::at` and `Registered::at` were safe functions that only `debug_assert!`ed their
  bounds. They now `assert!` and use `wrapping_add`, so they return an in-mapping pointer or
  panic, and safe code cannot misuse them. `Paths::name`, whose `CStr` scan from a wrong offset
  could run into a `statx` room the kernel is writing, is now an `unsafe fn` with a `# Safety`.
  `name_of` states why each caller's index is a name. A stamp job's write index is asserted to
  be in the claimed chunk before the raw write.
- **kqueue events.** `udata` is a handle's bits, looked up by index and generation in
  `dispatch`, never followed. `set_len(got)` is bounded by the room passed to `kevent`.
- **IOCP packets.** A `KEY_IO` packet's `OVERLAPPED` is read through to the head's id, so only
  the kernel's completions may carry that key. Before this pass, safe `Port::post` could post
  `KEY_IO` with any number: UB from safe code. It now refuses `KEY_IO`; a test's late
  completion goes through `unsafe fn post_head`. The id read is then checked by generation
  (`Slab::get`) and by the head's address (`owner`). `Head::id_of` forms the field address with
  `offset_of!` and `wrapping_add`, so its one unsafe operation is the read. The functions that
  lend the kernel memory past their borrow (`Watching::read`, `usn::Journal::read`,
  `Accepting::accept`, `Connecting::start`, `net::recv`, `net::send`) were safe functions
  whose soundness rested on the caller keeping the request in place. They are now
  `unsafe fn`s with that obligation as their `# Safety`. Their two callers (`Request::ask`, the
  connect) say where it is upheld: the request's slab entry, kept in place until the completion
  is taken.
- **FSEvents callbacks.** The callback's three arrays become slices only when `count` is not
  0, because `from_raw_parts` needs non-null pointers even for an empty slice. An empty batch
  told nothing before either.
- **fanotify and inotify records, `getdents64` entries.** These are parsed in safe code. Each
  record's length is checked against what `read` returned before a slice is taken, and a record
  that does not fit ends the batch as lost.
- **Process spawn** (`process/spawn.rs`). Every call the Linux child makes between `clone` and
  `execve` says on its line that it is async-signal-safe: `sigaction`, `sigemptyset`,
  `sigprocmask`, `dup2`, `write`, `chdir`, `execve`, `_exit`, `__errno_location`. `run`'s doc
  now also covers `mem::zeroed` (stores or `memset`, async-signal-safe since POSIX.1-2016). It
  also gives the stack bound: under 400 bytes of frame plus one libc wrapper, no recursion, in
  64 KiB with no guard page. Every signal is blocked across the `clone`, and handlers are reset
  before the mask clears. macOS uses `posix_spawn`, so no blaze code runs in the child.

## Lock-free structures and model tests

Two hand-rolled protocols were in driver code with no model test. They are now in
`src/handoff.rs`, safe code over `crate::sync::atomic` (`std`'s, or `blaze-check`'s under
`--cfg blaze_check`), so they are model-checked on every platform:

- `Claims`: the io_uring stamp job's chunks, claimed with a relaxed `fetch_add` and finished
  with an AcqRel `fetch_add`. The last finisher takes the whole buffer. `Job::run` uses it, and
  its SAFETY comments name it. The test `model_the_last_finisher_sees_every_chunk` checks, with
  two threads beside the model's own over three chunks, that each chunk is claimed once and
  there is one finisher. The finisher's tracked read (`blaze_check::cell::read`) of every
  chunk's bytes is ordered after each write. A threaded test does the same with eight threads
  over 1000 chunks.
- `Wake`: IOCP's coalesced wake flag (`swap(true)` after a hand-back, `swap(false)` before the
  drain). The test `model_no_hand_back_waits_without_a_wake_up` uses relaxed hand-backs, so only
  `Wake` orders them. A lost wake-up leaves the driver waiting, which the model reports.

Not model-tested: the kernel-shared ring words (the kernel is not a model thread) and the
fsevents `stopped` flag (a plain acquire flag with no protocol).

## Reference versions

Mapped bytes against `fs::read` (a proptest over every option), `raw::Aligned` against the
bytes (a proptest), the ring against the threads driver (`as_threads_do`), the stamp sweep
against its reference, the spawn's busy schedule against its reference. New: `Map::at` refusing
an offset past the mapping (`a_ring_offset_past_its_mapping_is_refused`), and `post` refusing
`KEY_IO` while others round-trip as numbers (`a_posted_packet_never_carries_an_overlapped`).

## Speed

Every added check is one compare beside a much larger cost:

| check | where it runs | cost beside it |
|---|---|---|
| `Map::at` bounds | once per ring setup offset, once per buffer given back | a 16 KiB copy |
| `Provided::buffer` | once per received buffer | up to 16 KiB received |
| `Registered::at` | once per 1 MiB direct write piece | the write |
| stamp index assert | once per name | a `statx` system call, about 1 µs |
| IOCP `post` key check | once per post | a system call |
| FSEvents `count == 0` | once per callback | the callback |

None is measurable (all under 0.1%). Nothing that was zero-copy now copies, and no lock was
added.
