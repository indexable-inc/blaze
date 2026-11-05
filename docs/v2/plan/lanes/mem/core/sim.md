---
status: plan
summary: mem-core-sim - the simulation (sim.rs), the portable Threads driver (threads.rs) and the code every Unix driver shares (net.rs, process.rs, process/spawn.rs, fs.rs, journal.rs) onto blaze-mem - spawns, watches, listens and connections in Slabs tagged Tag::Io named by generational handles, a stale completion detected and dropped or told empty, one mailbox in place of a channel and boxed senders, an errand pool that keeps its threads, per-call paths and C strings in thread scratch buffers, each warm cycle held to its payload by a cross-driver test, and each path's cost against its bound.
date: 2026-10-29
---

# mem-core-sim: the simulation, `Threads` and the shared Unix code onto blaze-mem

Child of [mem-core](../core.md). Paths: `crates/blaze-core/src/{sim.rs, sim/, threads.rs,
net.rs, process.rs, process/, fs.rs, journal.rs, tests.rs}` (not `fs/mapped*`, which is
dedup-map's). Nothing was built or run; the tests are written beside the code for the
coordinator's run.

## What moved

| Structure | Before | After |
|---|---|---|
| `Sim` effects in flight | `Wheel<Pending>` | unchanged (mem-core's slab-backed wheel) |
| `Sim` spawns, watches, listens | `FxHashMap<Tag, _>` each | `threads::tagged::Tagged<_>`: a `Slab` tagged `Io`, an `FxHashMap<Tag, Handle>` reserved at setup |
| `Sim` journals | a map of their own | folded into the watch (`Watching::journal`) |
| `Sim` connection ends | `FxHashMap<Conn, End>` keyed by a counter | `Slab<End>`; `Conn` is the handle's bits |
| `Sim` told completions | a completion alone | a completion and what it is of (`Of`): checked against the slab when due |
| `Threads` watches, spawns, listens | `FxHashMap<Tag, _>` and `Mutex<FxHashMap<Tag, Process>>` | `Tagged<_>` on the driver's thread; a spawn's state an `Arc<Life>` shared with its errand only |
| `Threads` connections | `Mutex<FxHashMap<Conn, Arc<Socket>>>` | `Slab<Connection>` on the driver's thread; `Conn` is the handle's bits |
| `Threads` completions | an mpsc channel and a `Box<dyn Fn>` sender per op | one `Mailbox<Told>` (`Mutex<VecDeque>` + `Condvar`), swapped whole at a poll |
| `Threads` file work | a channel of effects | a `Mailbox<Effect>` |
| `Threads` blocking work | a thread per spawn, two per spawn's pipes, one per connect, receive, send, listen | an errand pool that grows to the most at once and keeps its threads; a spawn's pipes one `poll` loop (`process::communicate`) |
| Linux watch controls | made per watch | `linux::Control` (eventfds, flush list, the tree's `Spare`) kept for the next watch |
| macOS watches | `fsevents::Stream` per watch | `fsevents::Watch` in the slab entry, stopped in place and kept (`spare`) |
| file paths for a syscall | `root.join(name)` per op | `blaze_mem::scratch::with_path` |
| `fs.rs` C paths, temporary names, child names | `CString::new`, `format!` | `with_c_path` on `scratch::with_bytes`; `with_string`; `with_path` |
| `net.rs` socket paths | `root.join` per connect | `scratch::with_path` |
| a spawn's strings | a `CString` each, a `BTreeMap` of the environment | one `scratch::lend_bytes` buffer; the environment ordered in `scratch::with_u32s`; `PATH` candidates in `with_path` |
| a spawn's argument arrays | two `Vec`s | still two `Vec`s, `// alloc: once per spawn`: they are made in the parent before `clone`/`posix_spawn` by design, and are at most a few hundred pointers |
| a Linux spawn's child stack | `vec![0; STACK]` per spawn | the thread's `scratch::with_bytes`, reserved once |

A completion for an op that ended first names a stale handle and is detected, never
dereferenced: in `Sim` a change of a watch whose end was delivered first is dropped, and bytes a
receive got on an end hung up before they were due are told empty, each counted
(`Sim::stale`); the scheduler tells a change made while a watch is ending once in
`RACE_ONE_IN`, so both races happen on some seeds. In `Threads` what a thread tells carries the
entry's handle (`Told::Watch`, `Exited`, `Accepted`, `Listened`), and one of an entry replaced
or ended is dropped; a `Conn` of a hung-up connection fails, even once its slot is another's.

## Tests

- `threads::tagged::tests`: `Tagged` against the `FxHashMap<Tag, _>` it replaces over 50
  seeds, stale handles included; a warm insert, lookup and remove allocates nothing.
- `sim::tests::ends_on_a_slab_answer_as_the_map_of_conns_did`: the end slab against the
  `FxHashMap<Conn, End>` model over seeds, stale conns included.
- `sim::tests::a_change_racing_an_unwatch_is_stale_and_dropped` and
  `a_hang_up_racing_a_receive_tells_it_empty`: the two races, over 500 and 300 seeds, each
  checked to happen on some seeds and not others, `Sim::stale` matching what was dropped.
- `tests.rs`, each for `Sim` and `Threads` after a warm-up:
  `a_warm_file_cycle_allocates_only_the_reads_bytes` (a Sync, a Stat and a Read),
  `a_warm_timer_arm_and_fire_allocates_nothing`,
  `a_warm_watch_add_change_and_remove_allocates_only_the_changes` (`Threads` on Linux and
  macOS), `a_warm_send_and_receive_allocates_only_the_bytes_received` (Unix), which then checks
  that hung-up `Conn`s fail once their slots are reused. `Threads::off_thread` counts what its
  file threads, errands and (on macOS) FSEvents callbacks allocate, with what completions carry
  counted apart, so the polling thread and the other threads are each held to a budget.

## Speed of light

The work of an effect on a real driver is its system calls; everything else is overhead.

- **`Threads`, a file effect.** The calls are the effect's own: a Stat is one `stat`, a Read
  `open` + `pread` + `close`, a Sync `open` + `fsync`/`F_FULLFSYNC` + `close` (the API names
  files by path, so no descriptor is cached across effects). On top: two hand-offs, driver to
  file thread and back, each a lock and unlock of an uncontended mutex (about 20 ns, two cache
  lines moving between cores) plus a futex wake only when the other side sleeps (about 2 to 5
  µs with the switch). The smallest call above, `stat` of a cached inode, is about 0.5 to 1
  µs; a batch of effects submitted together pays one wake for all of them, since the file
  threads drain the queue before sleeping and the poll swaps the whole mailbox at once
  (`take_all`). The bound is the calls plus one wake each way per batch; the code adds two
  lock pairs per effect (about 40 ns) and no allocation, under 10% of the cheapest call.
- **`Threads`, an errand** (spawn, connect, receive, send, listen, Linux watch). The bound is
  the calls plus one wake each way. A warm pool has an idle thread for each errand at once, so
  no thread is started; a spawn's output is one `poll` loop of 16 KiB reads, the fewest calls
  for pipes that don't say how much is coming.
- **A spawn.** The bound is `clone`/`posix_spawn` and `execve`, whose kernel cost (copying page
  tables or `vfork` semantics, loading the program: hundreds of µs) dwarfs the parent's work:
  one pass over the arguments and environment into one byte buffer the thread keeps, one sort
  of the environment's indices, and two pointer arrays, a few µs at most.
- **A timer.** The wheel's insert and fire are O(1), a few cache lines; the poll waits exactly
  until the next deadline.
- **`Sim`.** No system calls: the bound is the state each op touches. An effect is a wheel
  insert and fire, a file lookup by `FxHash`, and for a read one copy of its bytes; a
  connection end is a slab index. `after_told` scans the completions told and not yet due to
  keep a tag's order, which is short (what is in flight between two polls); the simulation is
  a test driver, so its bound is seeds per second, which this keeps at memory speed.

What is fastest known for each: the mailbox swap is the batch hand-off the LMAX disruptor and
io_uring's completion queue use (one synchronisation per batch, not per item); generational
slabs for in-flight ops are what tokio's and io_uring drivers use for `user_data`; spawning
with `clone(CLONE_VM | CLONE_VFORK | CLONE_PIDFD)` from arrays prepared in the parent is what
`posix_spawn` in glibc and std's spawn do.
