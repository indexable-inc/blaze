---
status: plan
summary: speed of light for blaze's test runner (the fork server per test, the wire, the execution record, the queue, a suite's critical path, test selection), with the arithmetic, what the code does to reach 90% of each bound, and what still stands in the way.
date: 2026-10-29
---
# Speed of light: tests

Part of [usercode](../usercode.md); the design is in [tests.md](tests.md). Code:
`crates/blaze-usercode/src/test.rs` and `crates/blaze-usercode/src/test/`. Nothing here was
run. Costs are counted from the code and the machine, not measured. `proof`'s budgets
measure them once the tree builds, and a component more than 10% off its bound is a bug.

## Machine and unit costs

Tests run on Linux (the fork server waits on pidfds), in an ix VM on the M5 Max class of
machine the other lanes assume:
- 6 performance cores at about 4.5 GHz;
- L1 about 3 cycles, L2 about 16 cycles, DRAM about 100 ns;
- IPC about 3.5 on branchy code;
- one core streams about 100 GB/s from L2 and about 50 GB/s of lines another core dirtied.

The Linux and VM costs assumed:

| step | cost |
|---|---|
| a cheap syscall (`close`, `fcntl`, `read` of a few bytes, `waitpid` with `WNOHANG`), in a VM | about 0.3 µs |
| `fork` of a test binary's server (10 to 30 MB resident: page-table copy, task setup) | about 50 µs (`T_fork`) |
| the child's `_exit` and the parent's reap | about 10 µs |
| a minor page fault on a shared page already in memory | about 0.25 µs |
| a page fault that allocates and zeroes a 4 KiB page | about 1 µs |
| `mmap` or `munmap` of a small region, with the TLB shootdown `munmap` makes | about 1 to 2 µs |
| BLAKE3 on one core: one long update (SIMD across chunks) / 32 bytes an update | about 5 GB/s / about 1 GB/s |

## The fork server, per test

The work that can't be skipped: one `fork`, the child's exit, and one wakeup of the server
that tells it so. The bound for a test that runs for `T` is `T_fork + T + T_exit`: about
`60 µs + T`.

What the server spends on top, on Linux:

| | calls | cost |
|---|---|---|
| server, before the fork | `socketpair` | 0.3 µs |
| child, before the test | `setpgid`, two `close`, two `dup2` | 1.5 µs |
| server, after the fork | `close` of the child's end, `fcntl`, `pidfd_open` | 1 µs |
| server, at the end | `read` until end of file (2), `close` (2) | 1.2 µs |
| shared by every child that ends in one turn | `poll`, `waitpid` until empty, one `write` | about 1 µs |

That is about 5 µs, 8% over the bound of a test that does nothing, and less for any test that
does something. The execution record adds nothing (next section).

**What stands in the way: one server forks serially.** A binary's tests start at most at
`1 / (T_fork + 5 µs)`, about 18,000 a second. With `C` cores and tests shorter than
`C × 55 µs`, the fork, not the cores, is the wall. On 6 cores that is any test under 330 µs.
On 64 cores it is any test under 3.5 ms, the common case for unit tests. Every test of such a
binary is off its bound by up to `C` times.

The removal, done: `K` replicas of the server per binary. After the hello, blaze asks the
server for `K - 1` copies of itself (`Frame::Replicate`). Each copy is initialized (statics,
the slot count, its own empty pool of regions) and connects on a socket of its own. The queue
keeps a connection per replica and hands each test to the first with room, so `K` forks are in
flight at once. How `K` is chosen, from the cores and the fork the server measured, is in
[speed-of-light.md](speed-of-light.md), "Tests", "Replicas of a fork server".

The sandbox needs no change: a connection is allowed by the socket's path, and the copies
inherit the filter. The driver's listen accepts every connection until the queue ends it,
which it now does only once all `K` are in.

**Smaller gaps, each under 2%:**
- `clone3` with `CLONE_PIDFD` would fold `pidfd_open` into the fork.
- `SOCK_NONBLOCK` on the `socketpair` would drop the `fcntl`.
- `poll` rebuilds its fd list each turn, O(open children): about 20 ns a child, 6% of a turn at
  16 children and 1% of a test. An epoll set made once per child removes it, at one `epoll_ctl`
  per child, which is the same cost. They stay as they are.

## The execution record

The work: the child sets one byte per slot it first calls. After the child ends, the server
reads `S` bytes (the binary's slot count, rounded up to words) and sends `H` hits of 4 bytes.
The bound is the read at the bandwidth of lines another core dirtied: `S / 50 GB/s`. For
`S = 100,000` that is 2 µs.

**Before:** one volatile `u64` a step, with a compare and a branch each, about 2.5 cycles a word:
`12,500 × 2.5 / 4.5 GHz` = 7 µs, 3.5 times the bound.

**Now (`wire::read_region`):**
- Four independent loads a step, a group of zeros skipped in one OR and compare: about 1 cycle
  a word, 2.8 µs, or 36 GB/s.
- Two loads a cycle would reach the bound. The loads are volatile, which keeps them one word
  each and in order.
- A test that executed anything spends far longer than 3 µs, so on the per-test bound the read
  is under 5%.

**The region, before:** an `mmap` and a `munmap` per test, plus a page fault that allocated and
zeroed every page the server or the child touched. For `S = 100,000` that is 25 pages, about
`2 × 1.5 + 25 × 1 = 28 µs`, half of `T_fork`.

**Now (`Exec::give`):** a region read is cleared and kept for the next test. The clear is the `H`
bytes the read found, or every word after a record that was not whole, and then the header.
A steady stream makes no `mmap` or `munmap` and allocates no page. The child still takes a
minor fault on each shared page it touches, because `fork` copies no page-table entries for a
shared mapping: about `25 × 0.25 = 6 µs` at most, and only for pages it writes.

Removing those faults would mean premapping the region in the child before the test, which
costs the same faults sooner. That is the bound.

The pool holds as many regions as tests ran at once. A process a test left running may still
set a byte in a region another test now has. The runtime only sets bytes, so that test's
record may gain a slot, never lose one: selection reruns more, never less.

## The wire

The work: a `Run` frame of about 30 bytes to the server, and an `Ended` frame of `O` output
bytes and `4H` hit bytes back. For tests whose output is under 64 KiB the bytes are noise
(`O / 10 GB/s` is under 7 µs). The bound is the syscalls: about one `read` and one `write` per
turn on each side, shared by every frame in it.

**Now:**
- Frames are read in place (`View`), and every frame of a turn is encoded into one buffer and
  sent in one `write_all`.
- `Frames` grows to the longest frame and then never allocates.
- The decoding is one pass over each byte, about 1 ns for 10 bytes.

This is at the bound.

**What stands in the way for large outputs:** a test's output crosses five copies:
1. the child's write;
2. the server's read;
3. the server's write;
4. the driver's read;
5. the queue's copy into the result.

Above about 256 KiB the copies pass 10% of a test's cost. Two removals:
- **The driver lending its read buffer** (`blaze_core::Done::Read`) saves the queue's copy.
  That is core's API.
- **Passing the child's output descriptor to the driver with `SCM_RIGHTS`** saves the server's
  read and write. That is a wire change I make when output size shows up in `proof`'s budgets.

## The queue, per test

The work for each test:
- choose it: the first set bit of `ready`, one word scan per 64 tests;
- send its `Run`;
- arm its deadline: a wheel insert, O(1);
- on its end, remove the deadline (O(1)) and build its result.

That is about 300 instructions and 4 allocations (output, own lines, hits, name), about 0.5 µs,
1% of `T_fork`. The record lines a test read are parsed at about 50 ns a line, dispatched on
their first byte and interned per server.

**Before:** each result copied every line of every scope (shared libraries, the runtime's reads)
into its own `Lines`. A binary that read 2,000 such lines of about 40 bytes copied 80 KB and
made two allocations per test: about 5 to 10 µs, 8 to 16% of `T_fork`.

**Now:** the lines of every scope are one `Arc<Run>` shared by every result of the server until
a new one arrives (`Lines::common`, `Lines::common_is`). A result costs an `Arc` clone and its
own lines. This is at the bound.

**Allocation budget** (`the_steady_state_reuses_its_slabs`): once the columns have grown, a
test costs the queue at most 8 allocations, every one owned by something past the queue:
- the result's output, execution record and name;
- its own line, kept and then its result's two buffers;
- the record file name of the driver's read;
- the bytes of the driver's send of the next run.

None is per line every test read, per timer or per attempt. The wire, the region read, an
interner hit and the wheel in steady state allocate nothing, and a test's identity, sorted in the
worker's scratch (the radix sort's buffer included, through `sort_copy_by_key_in`), allocates
nothing warm below `blaze_radix::PARALLEL` hashes. Each has a test with `blaze-alloc-count`.

**What stands in the way, outside these paths:** cargo's `Trace::traced` turns each result into
a `Traced` and rehashes every common line for every test, about 20 µs for 2,000 lines. With
`common_is` it can build the common part once per server and merge each test's own lines into
it. That is asked for in `v2/requests/usercode-fast-tests.md`.

## A suite's critical path

The bound for `N` tests on `C` cores is the larger of:
- the total work spread over the cores, `Σ T / C`;
- the longest test, `max T`;
- the startup of the slowest binary: exec, dynamic linking, its statics and the hello, about 1
  to 5 ms.

**Now:**
- Tests run longest first by expected time: the LPT rule, within 4/3 of optimal at worst and
  within a few percent when `N ≫ C`.
- Every binary's server starts at once.
- A deadline costs a wheel slot, not a thread.

**What stands in the way: a new test has no expected time, so it runs last.** If it is long,
the makespan grows by up to that test's time. The removal is a prior for tests never seen:
the binary's median, or a cost estimated from the size of the reached set. This is in
`queue.rs` and is the next change to the order.

## Selection: a test's identity

The work: sort and dedup the test's `n` reached code hashes (32 bytes each), then hash them.
For `n = 10,000`:

| step | bound |
|---|---|
| hash `32n` = 320 KB at one long update | 64 µs |
| radix sort of `n` 8-byte pairs: 4 passes, reading and writing | about 13 µs |
| gather `n` hashes in sorted order | about 6 µs |
| total | about 83 µs |

**Before:**
- `sort_unstable` of 32-byte keys: `n log n` is 130,000 compares at about 5 ns each, 650 µs.
- One `Hasher::id` of 32 bytes per hash, which BLAKE3 cannot spread across SIMD lanes: 320 µs.
- Total about 1 ms, 12 times the bound.

**Now (`select::sorted`, `Test::identity`):**
- A radix sort of each hash's first four bytes and its index. Hashes are uniform, so runs of
  one prefix are rare, and each run is sorted by the whole hash.
- The sorted hashes go into one buffer hashed in one update. The bytes hashed are the same as
  one at a time, so every stored key stays; a test checks this.
- About 90 µs, within 10% of the bound.

**What stands in the way: identity is computed twice per test.** `select` computes it for the
lookup, and `record` computes it again for the key. Caching it needs a field on `Test`, whose
fields are public and built by the parent's code. The removal is for the caller to compute
`Test::identity` once and pass it to both, or for codegen to hand over `reached` already sorted
and deduped. Then the sort goes too and identity is 64 µs. Both are asked for in
`v2/requests/usercode-fast-tests.md`.

Rejected: an additive set hash (the sum of the hashes modulo 2^256) needs no sort and costs
about 10 µs. It is weak against chosen inputs (Wagner's generalized birthday attack), and a
store key must not be.
