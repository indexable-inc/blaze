---
status: plan
summary: Speed of light of what changed outside every view (crates/blaze-vfs/src/outside.rs and blaze-core's Effect::Flush) - each component's bound from the hardware and the work, with the arithmetic, how the design comes within 10% of it, and what stands in the way where it does not.
date: 2026-10-29
---
# Speed of light: what changed outside every view

The bounds of `crates/blaze-vfs/src/outside.rs` and of blaze-core's `Effect::Flush`, against
[principles.md](../../goals/principles.md)'s bar of 10%. The design is in
[outside.md](outside.md). Figures are for one core of the reference machines
([targets.md](../../goals/targets.md)): about 3.5 GHz, an L1 hit in 4 to 5 cycles, L2 in about
14, a cache line moved between cores in about 100 cycles. A `stat` costs about 0.5 µs on Linux
in a VM and 1 to 2 µs on macOS. Nothing is measured yet, since nothing builds until every lane
is done; `proof`'s budgets measure these once it does.

## 1. A check on a cache hit: `Cover::unchanged_since`

**Work.** To answer "can this answer be reused?", a check must learn three things: that no flush
round is out, that the directory has been watched since before `g`, and that nothing under it
changed after `g`. Each is state the watcher writes, so a check needs at least one load of the
directory's state and one of the round's state. Both lines are written only when a change is
heard or a round starts or ends, so across a run of checks they sit in L1.

**Bound.** Two independent L1 loads, which overlap: about 5 cycles, about 1.5 ns.

**Design.** A `Cover` holds the table and the directory, found once when the answer is made, so
a check does no hashing and no walk up the path. It does three acquire loads (on x86 and arm64
these are plain loads): `settled` on the table's hot line, and `since` and `changed`, which
share the directory's line. All three are independent, so their latency overlaps. That is
about 5 to 6 cycles plus a predicted branch, within 10% of the bound.

**Against `stat`.** For a C build script that reads 500 headers, the checks cost
500 × ~2 ns ≈ 1 µs, against 500 × 0.5 µs = 250 µs of `stat` calls on Linux (0.5 to 1 ms on
macOS).

**Cold line.** When the directory's line is not in L1, the bound is the same miss (the state has
to be read), so the design still sits at the bound: one L2 or L3 miss, since `since` and
`changed` are on one line.

## 2. A check by path: `Dirs::unchanged_since(path, g)`

**Work.** Read the path's \(L\) bytes once, find a watched directory at or above it, and compare
its bytes, because a hash alone could collide.

**Bound.**
- the separator scan: \(L/32\) cycles with SIMD, \(L\) cycles a byte at a time;
- FxHash: about 4 cycles per 8 bytes (a multiply chain), so about \(L/2\) cycles;
- one probe of the index (an L1 hit) and one compare of \(k \le L\) bytes, about 15 cycles.

For \(L = 40\) that is about 40 cycles, about 11 ns.

**Design.** One forward pass hashes the path part by part and yields the tag of every directory
above it. Before this pass, each ancestor was hashed from scratch: \(O(d \cdot L)\) bytes
hashed, about 360 bytes for a 60-byte path six levels deep, against 60 now. The walk goes
shortest first and stops at the first directory that vouches, and since `want` never adds a
directory under one already wanted, there is at most one hit on the way. The cost is about
\(L\) + \(L/2\) + one probe per part, so about 60 + 20 + 6 × 5 ≈ 110 cycles for \(L = 40\).

**Gap.** This is about 2.5× the bound. The gap is the byte-at-a-time separator scan and a probe
per part. It doesn't matter: this form runs only when an answer is made, beside a `stat`, an
open and a BLAKE3 hash of the file (at least 2 µs), where 30 ns is about 1.5%. The hot path is
the `Cover` above. Removing the gap would take a SIMD separator scan and probing only parts
whose tag the table could hold.

## 3. Asking for a directory: `want`

This is the cold path, once per directory per process. It walks the ancestors without a lock
(as in section 2), then takes the lock once to add the directory, publishes it with two release
stores, and scans the \(n\) directories already wanted to mark the ones it covers. With \(n\) in
the tens after subsumption, that is under a microsecond, once. No bound is set on it beyond
"never on a check".

## 4. A flush round per request: `Outside::flush` and `Effect::Flush`

**Work.** For each kernel queue that holds events of an outside directory, learn that everything
queued before the round has been read. One queue cannot vouch for another, so the work is one
barrier per queue, and the queues are flushed in parallel.

**Bound.** \(W\) barriers, where \(W\) is the number of queues, with the latency of one barrier,
since they run in parallel.

**\(W\).** Before this change, \(W\) was one watch per directory `want`ed, and usercode wants each
traced path's parent: for a C build, hundreds (`/usr/include`, `.../bits`, `.../sys`,
`.../c++/v1`, ...). Each was an inotify descriptor, a thread in the threads driver, an FSEvents
stream, and a flush per request. A watch hears its whole tree, so now a directory under one
wanted is never added. One wanted above directories already watched retires their watches,
which `Outside` ends, and checks find the directory above. \(W\) is now the number of
directories not under another, about 3 to 6 (the sysroot, the C compiler's, `/usr/include` or
the SDK).

**Linux, per watch (threads driver).** The minimum to synchronise with the thread that reads
the descriptor is one wake and one check that the queue is empty, so 2 syscalls. The design
takes 3: the `eventfd` write, the `eventfd` read after `poll` returns, and the `poll(0)` that
finds the queue empty (plus a `read` per batch actually queued). The latency is one
cross-thread wake (about 5 µs) plus about 0.2 µs per syscall, so about 5.6 µs against a bound
of about 5.4 µs: within 10%.

**Linux, io_uring.** The cancel of the ring's read rides the next submit, with no syscall of its
own, then one `poll(0)` drain. That is at the bound.

**macOS.** Ordering through fseventsd is only proven by an event passing through it, so one
cookie per stream is the bound per stream. Each cookie is an open, write and close (about
15 µs) plus fseventsd's delivery (about 0.1 to 1 ms at latency 0). The \(W\) cookies are written
in parallel, so the latency is one cookie's, the bound.
- **Remaining cost.** It is not on the latency path: \(W - 1\) extra cookie writes, about
  50 µs of CPU on dispatch queues.
- **What stands in the way of one cookie.** `Effect::Watch` names one directory. Removing it
  takes one FSEvents stream over all of a driver's watched paths (`FSEventStreamCreate` takes
  an array), with flushes answered per watch from the one cookie.

**Against the request.** The round runs beside the checkout's own cookie
([../cargo/notes.md](../cargo/notes.md)), so a request waits for the larger of the two: the
round adds nothing to the critical path when \(W\) is small.

**Windows.** `ReadDirectoryChangesW` has no barrier, and a cookie can't be written into a
read-only directory. So `Flush` fails, nothing outside is confirmed, and every check `stat`s.
That is correct, but about 500 stats per C build script hit instead of 500 loads.
- **What stands in the way.** There is no ordered queue to flush.
- **What would remove it.** The USN journal is per volume and ordered. Watch outside
  directories through `Effect::Journal`, and flush by reading the journal's next USN
  (`FSCTL_QUERY_USN_JOURNAL`) and answering once the journal's reader has passed it: one
  `DeviceIoControl` per round.

## 5. Changes heard

Each event bumps the generation (one atomic add on the hot line) and raises the directory's
`changed` (a `fetch_max`). Each core reading the hot line then pays one line transfer (about
100 cycles) on its next check. Changes under the toolchain or system headers are rare: a few
per toolchain update, not per edit. So this costs nothing per edit, and the hot line is never
written by a check.

## 6. Memory

- **Per directory:** a 32-byte entry (two `u64` generations, the `u32` of the directory that
  covers it, and the `u32` start and length of its path in the paths column, aligned to 16 so
  the generations share a line) plus the path's bytes.
- **Index:** 8 bytes per place, at most half full.
- **Per answer, on usercode's side:** a `Cover` (16 bytes) and a generation (8). With \(W\) in
  single digits and the directories a trace names in the hundreds, this is a few tens of KB per
  process.

**By lifetime** ([../../design/memory.md](../../design/memory.md)):
- **The process's life:** the table lives as long as the process does and never moves: its
  directories (32 bytes each), their paths' bytes and the index's tables are
  `blaze_mem::AppendVec` columns over regions reserved once (128 MB of address space, for
  \(2^{20}\) directories) and committed as they fill, counted under `Tag::Vfs`; readers stay on
  old index tables. A `want` past the high-water mark commits pages and never calls the
  general-purpose allocator (`wants_past_the_high_water_mark_allocate_nothing`). The driver's
  watches are a `RegionVec` beside them.
- **The warm loop** (a request's flush round, its checks, a change heard) makes no
  general-purpose allocation:
  - effects go into the caller's buffer;
  - the threads driver drains its flush lists in place, keeping their room;
  - FSEvents names each cookie in a path buffer kept for the stream's life (its decimal
    digits on the stack) and hears cookies into buffers kept for the stream's life;
  - io_uring's flush list keeps its room.

  The allocation-budget test `checks_wants_and_rounds_allocate_nothing` holds `Outside`'s side
  to zero. The drivers' completion channels are core's.
