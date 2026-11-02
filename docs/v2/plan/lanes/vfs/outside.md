---
status: plan
summary: blaze v2 vfs - what changed outside every view (the toolchain, the C compiler's, /usr/include, each traced path's parent) answered from a lock-free table of watched directories, sound through a flush barrier per request; compared with watchman's clocks and sync cookies and Bazel's --watchfs.
date: 2026-10-29
---
# blaze v2 vfs: what changed outside every view

User code (build scripts, proc macros, the C compiler they run) reads files no view holds: the
toolchain, the C compiler's own headers, `/usr/include`, whatever a trace names.
blaze-usercode's `inputs::Files` stamps each traced path once per check, which means one
`stat` per path, and a C build script reads hundreds of headers. `crates/blaze-vfs/src/outside.rs`
answers the same question with no syscall when nothing has changed: "has anything under this
path changed since generation `g`?" (request 11 of `v2/requests/usercode.md`). The rules it
reads under are in [../../design/concurrency.md](../../design/concurrency.md).

## What it is

- **A process-wide table** (`Dirs`, one `static`): a generation counter, and for each wanted
  directory two atomics, `since` (the generation since which it has been watched without a
  break, `u64::MAX` while it is not) and `changed` (the generation of the last change heard
  under it). Directories are keyed by an `FxHash` of their bytes in an open-addressed index of
  `u64` places that grows by rehashing into a table twice the size, the old tables kept for
  readers still probing them. Directories, their paths' bytes and the index's tables (laid end
  to end) are append-only `blaze_mem::AppendVec` columns over reserved regions tagged
  `Tag::Vfs`: one writer appends under the adding lock, readers read with no lock, and nothing
  moves or is copied as they grow.
- **Reads** (`generation()`, `unchanged_since(path, g)`) are acquire loads only: no lock, no
  shared write, no allocation. `unchanged_since` walks the path's ancestors byte by byte and
  is true when some ancestor has `since <= g` and `changed <= g`, and the last flush round is
  in.
- **`cover(path)`** finds, once per answer, the watched directory above a path. A check kept
  with it (`Cover::unchanged_since`) is three loads on two lines, with no hashing and no walk
  up the path ([speed-of-light-outside.md](speed-of-light-outside.md)).
- **`want(dir)`** adds a directory, once per directory per process, under a lock (the cold
  path). The file system's root is never watched. A directory under one already wanted is
  not added, and one wanted above directories already watched retires their watches, so a
  round flushes one watch per directory not under another.
- **`Outside`**, a sans-IO driver written like `Watcher`, with its own tag range. It turns a
  wanted directory into `Effect::Watch` followed at once by `Effect::Stat`. Every driver sets
  the watch up synchronously as it is submitted, so a stat that finds a directory confirms that
  the watch was in place first: `since` becomes a freshly bumped generation. Any `Done::Changed`
  (lost or not) bumps the generation into `changed`. A change naming the directory itself, an
  `Unwatched` or a `Failed` sets `since` back to `u64::MAX` and watches the directory again
  (a failure backs off, doubling up to 1024 rounds). A directory the driver's root cannot name
  is never confirmed, so each check of a path under it looks at the disk, which is the accurate
  answer.
- **The barrier**: an event is heard after the change that made it, so a request's checks
  could otherwise run while its own last edit is still queued. `Effect::Flush { watch, tag }`
  in blaze-core completes only after every change queued for that watch when it was asked has
  been told. On inotify and fanotify the driver reads the descriptor until it is empty (the
  threads driver on the watch's own thread; io_uring cancels its read in flight first). On
  FSEvents it is a sync cookie: a file written into a per-stream temporary directory the same
  stream watches. Windows fails it, naming why. The simulation tells it after every change of
  the watch it has queued. `Outside::flush` starts a round (one flush per confirmed watch);
  `unchanged_since` is false until the round's last flush is in, and `Outside::flushed` says
  when that is. The daemon runs one round per request, beside its checkout cookie.

## The state of the art, and how this compares

- **Watchman's clock**: a per-root tick counter bumped by each change, with a query "since"
  a clock answered from its in-memory view. `generation` is that clock, process-wide. What is
  better: a watchman query is a round trip over a socket to another process, with JSON or BSER.
  Here a check is two acquire loads per ancestor in the daemon's own memory.
- **Watchman's sync cookies**: before a query, watchman writes a cookie file into the watched
  root and waits until its own watcher hears of it, so the query sees every change made before
  it. `Effect::Flush` is that barrier. It matches watchman on FSEvents, where the cookie lives
  in a directory of the stream's own, so a read-only root such as `/usr/include` can still be
  flushed; watchman needs a writable root or skips the sync there. On Linux it beats watchman:
  inotify and fanotify queue an event inside the syscall that made the change, so draining the
  descriptor until it is empty is a barrier with no file written and no extra event.
- **Bazel's `--watchfs`**: Bazel watches the workspace and output base and, at the start of each
  command, takes the diff the watcher collected instead of stat'ing every file. Files outside
  the workspace (the external repositories' sources, system headers) are not covered: Bazel
  stats them, or trusts them. Here directories outside every view are covered too, and a
  watch that broke, or was never confirmed, never vouches. `--watchfs` has long been
  flagged experimental, with no barrier between a command and the events of its own last
  edit; here a round of flushes is that barrier, and a lost event (an overflowed queue,
  FSEvents' `MustScanSubDirs`) is a change, never a skip.
- **What is better than all three**: soundness does not rest on a timestamp or on the watcher
  keeping up. `since` is set only by a stat that ran after the watch was in place, `changed` is
  raised by every event, lost or not, and a round in flight vouches for nothing. A check that
  is not vouched for falls to `stat`, which is the accurate answer rather than a fallback.

## Measured

Nothing yet: no lane builds until every lane is done. To measure once it builds: the cost of
`unchanged_since` against `stat` on a cache hit of a C build script reading 500 headers (it
should be two loads per ancestor against 500 syscalls), and the latency of a flush round per
driver (inotify drain, io_uring cancel and drain, FSEvents cookie).
