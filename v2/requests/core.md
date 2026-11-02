# Requests from lane core

## 1. Workspace dependency `crossbeam-deque`

- What: add `crossbeam-deque = "0.8.6"` to the root `Cargo.toml`'s `[workspace.dependencies]`
  (already in `Cargo.lock` through rayon, so the lock does not change).
- Where: root `Cargo.toml`; used by `crates/blaze-graph/Cargo.toml` (`crossbeam-deque.workspace
  = true`).
- Why: the worker pool (docs/v2/plan/lanes/core.md section 4) steals work between threads; each worker's
  deque is crossbeam's Chase-Lev deque, so the hot path takes no lock.

---
Applied by the coordinator, 16:46.

> applied: `crossbeam-deque = "0.8.6"` is in the root `[workspace.dependencies]` (by the coordinator, 16:46).

## 2. usercode: a `blaze_core::Confine` over the sandbox

- What: implement `blaze_core::Confine` (`crates/blaze-core/src/lib.rs`) over the sandbox in
  `crates/blaze-cargo/src/sandbox*` (Landlock and seccomp on Linux, Seatbelt on macOS):
  `confine(command, scratch)` returns the program, arguments, environment and directory that run
  `command` under `Sandbox::On { read, write, network, trace }`, with its trace written under
  `scratch`; `traced(command, scratch)` reads that trace back as a `Traced`. Hand it to the
  drivers with `Threads::confined`, `Uring::confined` and `Kqueue::confined`.
- Where: `crates/blaze-usercode/` (usercode), and whoever builds the daemon's driver.
- Why: core does not own the sandbox. A driver without a `Confine` fails every
  `Sandbox::On` spawn with an error naming it, so build scripts and proc macros cannot run
  under `Effect::Spawn` until this exists (sandbox-everything: nothing runs outside by default).

> applied: by usercode (`impl Confine for Launcher` in `crates/blaze-usercode/src/confine.rs`).
> forwarded to cargo: handing it to the daemon's driver (`Threads::confined`, `Uring::confined`, `Kqueue::confined`) is the daemon's.

> answered by lane cargo (cargo-inbox): done for the daemon: crates/blaze-cargo/src/serve.rs hands `blaze_usercode::confine::Launcher` to the platform's driver (`driver.confined(confine).scratch(<state>/spawn)`), so every confined spawn goes through the sandbox launcher.

## 3. daemon: count allocations and run work on the scheduler

- What: in the daemon binary, `#[global_allocator] static ALLOC:
  blaze_graph::arena::Counting<std::alloc::System> = blaze_graph::arena::Counting::new(std::alloc::System);`,
  and the daemon's CPU work (queries, codegen, links) posted through
  `blaze_graph::exec::Scheduler` as `Task`s with their `TaskKind`, the plane's budget from
  `blaze_graph::arena::machine_budget`.
- Where: the crate holding the daemon's `main` (cargo or ci), `crates/blaze-cargo/src/memory.rs`.
- Why: memory admission (docs/v2/plan/lanes/core.md section 4) learns each kind's peak from what the
  counting allocator measures per task and holds the daemon's own arenas against the budget;
  without the allocator every measured peak is 0 and admission admits everything.

> forwarded to cargo: the daemon's `main` still uses `tikv_jemallocator::Jemalloc` (`crates/blaze-engine/src/lib.rs`) and `crates/blaze-cargo/src/` uses no `blaze_graph::exec::Scheduler`; lane cargo's daemon owns it (wrap its allocator in `blaze_graph::arena::Counting`).

> answered by lane cargo (cargo-inbox): the scheduler half is done at `crates/blaze-cargo/src/serve/plane.rs:236` `post_work` (every thread task runs on `blaze_graph::exec::shared()` with its `TaskKind`, measured by `arena::measure`) with the plane's budget from `blaze_graph::arena::machine_budget` (`crates/blaze-cargo/src/serve.rs:183`). The allocator half is not lane cargo's: the daemon binary's `#[global_allocator]` is `crates/blaze-engine/src/lib.rs:47-52` (jemalloc on unix, mimalloc on Windows), lane codegen's crate. Asked of codegen in `v2/requests/cargo.md`, "cargo-inbox: count the daemon's allocations".

## 4. Applied: link's request 3 and vfs's request 1

`Effect::WriteAt`, `Effect::Rename`, `Effect::Mode` (`Done::Written`, `Done::Renamed`,
`Done::Moded`) and `Effect::List` (`Done::Listed`) are in `crates/blaze-core/src/lib.rs`, in
the simulation, `Threads`, `Uring` and `Kqueue`. `Sim::mode(file)` reads a file's mode.

> applied: note only.

## 5. Note for vfs, store and db: the simulation's truncate is now undone by a crash

The simulation's files keep a log of unsynced changes (appends, writes at an offset,
truncations); a crash keeps a random prefix of it, the last possibly torn. A truncate not yet
covered by a completed `Sync` can now be lost by a crash, as on a disk (before, the cut always
survived). A journal that cuts a torn tail and appends after it must not count the cut durable
until a sync after it completes; tests that relied on the old rule may now fail on some seeds.

> applied: note only, for vfs, store and db.

## From concurrency: lock audit (2026-10-29)

Audited against `docs/v2/plan/design/concurrency.md`. The plane (`plane.rs`, `machine.rs`,
`admission.rs`) takes no lock. The pool takes none per task: `pool.rs:76` `stray` is locked
only when a job's `done` panics. The per-query locks are all in `pump.rs`, and they are on
every memo miss: `blaze-query`'s `single` (`crates/blaze-query/src/lib.rs:1139`) calls
`Flights::get_or_run` for each key a view's `results` does not have yet, including each green
check on a warm build.

1. **`crates/blaze-graph/src/pump.rs:105` `WAITS`, one process-wide `Mutex<Waits>` (a
   `BTreeMap` of wait edges).** It is taken at `:309` in `Ending::end` on *every* run's end,
   even a run with no waiters: the loop over `waiters` is empty, but the lock comes first. That
   contradicts the module doc (`:12-13`, "nothing of a run that waits for nobody takes it").
   It is also taken at `:60` and `:71` around every pumped job, and at `:267` for every wait.
   This breaks two rules: a shared write per query ("Writers shard by hash"), and a single
   global lock on the hot path. Every core serializes on one cache line at each query end.
   Fix, first step: in `end`, take `WAITS` only when `waiters` is non-empty.
   Fix, the real one: store edges per task, not in a global map. Give each task's `on` and
   `above` a home in an atomic word of a slot table indexed by task id (per-worker slots,
   padded to 128 bytes). A waiter publishes its `on` edge with a SeqCst store, then walks the
   chain with acquire loads. If the walk reaches itself, it retracts the edge. When two waits
   close one cycle at once, only the task with the largest id in the cycle gives way, so
   exactly one gives way. Neither a wait nor a pump then takes a lock.
2. **`pump.rs:36`, `:202`, `:213`, `:250`: `Flights` has a fixed 64 shards, indexed by
   `key.0[0] % 64`.** Every `get_or_run` takes a shard lock twice, in `join` (`:254`) and
   `Ending::end` (`:300`). It also allocates an `Arc<Flight>` holding a `Mutex` (`:278-281`)
   for every run, on the global allocator. This breaks "shard count a power of two, at least
   4 times the core count, chosen at startup". The shard lock is held only to claim, never
   while running, which follows the rule.
   Fix: size the shards in `Flights::new` from the topology, as
   `(4 * cpus).next_power_of_two()`, and index by masked key bits. Better still, drop the
   shards: use an open-addressed claim table keyed by the key's hash. A claim is a CAS of the
   slot's state word from empty to running(task id). Waiters push onto a lock-free list in the
   slot. The value is published once (see item 3), and the slot is freed at the plane's task
   boundary. Then no `Arc` or `Mutex` is allocated per run.
3. **`pump.rs:160` `Flight::state: Mutex<State<V>>`, polled by waiters.** `outcome()` (`:166`)
   and `ended()` (`:174`) lock it on every spin of `wait` (`:331-351`, up to `SPINS = 64`
   spins, then on every park check). So readers write the lock word of the runner's flight,
   which breaks "A read writes no shared memory". Fix: keep the state in an `AtomicU8`
   (running, done or cancelled), store the value in a `OnceLock<V>` set before a release store
   of `done`, and have waiters acquire-load it. The waiter list then needs the lock-free list
   from item 2, or a short claim lock taken only to push a waiter and never by a poll.
4. **`crates/blaze-graph/src/arena.rs:30`, `:37`, `:68`: `HELD` is 64 fixed shards of
   `AtomicI64`, assigned round-robin.** `Counting` does a relaxed `fetch_add` on one of them
   for *every* allocation in the daemon. With more than 64 threads, threads share a line, and
   the count does not scale with the machine. This is an atomic, not a lock, but it breaks the
   fixed-64 rule on the hottest write path there is. Fix: keep the count in the thread's own
   slot, in a startup-sized array of 128-byte-padded counters, one per thread slot (the
   thread-local `NET` already holds it). Have `held()` sum the slots; it runs at admission and
   in `machine_budget`, not per allocation.
5. **`arena.rs:338` `Cap::state: Mutex<State>` plus `Condvar`.** Every `Bump` chunk grow under
   a cap (`:280`, `reserve` at `:427-475`) and every `Grant` drop (`:509-525`) takes one
   process-wide lock. Each drop also calls `notify_all`, which wakes every waiter. A full cap
   blocks the *worker thread* in `Condvar::wait` (`:474`), which breaks "No thread waits on
   another's work". Nothing outside `arena.rs` builds a `Cap` or a `Bump::under` yet, but this
   is the designed per-task arena path. Fix: keep `held` in an `AtomicU64` and reserve with a
   CAS loop while `held + bytes <= limit`, with no lock. Keep `by_holder` per worker slot.
   Only an over-limit reserve takes the slow path, and it should hand the task back to the
   plane to park (return "would wait") instead of blocking the thread.

   Status of 4: done by speed-runtime, another way. `held()` runs after every task's `Done`
   (`exec.rs`, `blaze-cargo/src/serve/plane.rs`), so summing one slot per thread there costs a
   cache line per thread per task. Instead, each thread's count reaches one padded total only
   once it drifts 256 KiB (Linux's `percpu_counter`), and is flushed at thread exit. An
   allocation does no atomic, and `held()` is one load, exact for the calling thread and within
   256 KiB for each other one.

   Status of 5: partly done by speed-runtime. `Bump::reset` now keeps its last chunk, so a
   per-task arena reset between tasks reserves nothing from the cap in steady state: the lock
   is taken once per new chunk (64 KiB or more), not per task. A `Grant` drop no longer wakes,
   which is a futex syscall, unless someone waits. The wait itself stays: core.md section 4
   says "a task that would pass the cap waits", and the cap cancels the newest waiter when
   every holder waits. A lock-free reserve would need per-thread holder counts for that check.
   Not done, because the lock is no longer on a per-task path.
6. **`crates/blaze-core/src/kqueue.rs:202`, `:907-909` (the macOS driver) and
   `crates/blaze-core/src/threads.rs:50`, `:155-157`: the file-effect queue is an
   `Arc<Mutex<mpsc::Receiver<Effect>>>`.** An idle IO thread holds the mutex *while blocked
   in `recv`*. So every file effect (`ReadAll`, `Read`, `Stat`, `Map`, ...) is handed off
   through one lock, and the other IO threads queue on the mutex rather than on the channel.
   This is a single global lock on every file read the macOS driver does. Fix: use
   `crossbeam_channel`'s MPMC `Receiver`, cloned into each thread, which is lock-free. Add
   `crossbeam-channel.workspace = true` to `crates/blaze-core/Cargo.toml` (already a
   workspace dependency, used by `blaze-graph`). Or give each thread its own queue and submit
   round-robin.
7. **For codegen's finding on `hir-ty-codegen/v2/frontend.rs` (in `codegen.md`): expose this
   thread's worker index.** `pool.rs` keeps `Local::index` but exports only `group()`. Add
   `pub fn worker() -> Option<u32>` beside `group()` (`pool.rs:98`) so per-worker tables can
   be indexed without a lock.

> forwarded to core: every finding is in lane core's own `crates/blaze-graph/`; lane core is still running.
> applied (core DONE, so `requests`'): 1-3 and 7 were done in `pump.rs` and `pool.rs` (`Flight::state` an `AtomicU8` with a `OnceLock`, shards sized from the topology, `WAITS` taken only for a wait, `Pool::worker`); 4 and 5 as their status says. 6: `threads.rs` and `kqueue.rs` hand file effects to their IO threads through one `crossbeam_channel` MPMC queue, its `Receiver` cloned into each thread, with no lock; `crates/blaze-core/Cargo.toml` says `crossbeam-channel.workspace = true` (a new edge for the lock).

## From speed-link-edge-remote: two socket effects for the remote tier (2026-10-29)

For `crates/blaze-cas/src/remote.rs` (the laptop/VM read-through tier). The before and after
numbers are in `docs/v2/plan/lanes/store/speed-link-edge-remote.md`, and the rest of that work
is in `v2/requests/concurrency.md`. These are the two copies left there that only
`blaze-core`'s effect API can remove.

K1. **`Effect::SendMapped { conn: Conn, mapping: Mapping, range: std::ops::Range<u64>, tag: Tag }`,
    completing `Done::Sent`.**
    - Sends `mapping.0.as_ref()[range]` on `conn` without copying it into a `Vec`:
      - Linux: io_uring `IORING_OP_SEND_ZC` over the mapped range, or `splice`/`sendfile`
        from the pack's file;
      - macOS: `sendfile(2)` (kqueue driver);
      - threads driver: `write` straight from the slice.
    - The driver holds the `Mapping` clone until the send completes. That clone is the
      lifetime guarantee.
    - Today the server copies every served byte from the pack into a frame `Vec` for
      `Effect::Send`. With this, a `Form::Raw` record is served with no user-space copy: the
      server sends a small header `Vec`, then a `SendMapped` of the value's range, in order.
    - Sim: deliver the bytes as `Send` does.
K2. **`Effect::RecvInto { conn: Conn, buffer: Vec<u8>, most: usize, tag: Tag }`, completing
    `Done::Read(buffer)` with up to `most` bytes appended after `buffer`'s existing contents.**
    - Empty appended means hung up, as for `Recv`.
    - Today each `Recv` returns a fresh `Vec`, and the client copies it into its frame buffer,
      so a value spread over many receives is copied twice.
    - With this, the client passes its buffer (reserved to the frame's length) and gets it
      back: one copy, kernel to final buffer, and no allocation per receive.
    - io_uring: a plain `recv` into `buffer.spare_capacity_mut()`. Multishot recv with provided
      buffers (store's request 6) still suits small asks on the server side.
    - Sim: append the delivered prefix.

> forwarded to core: `Effect::SendMapped` and `Effect::RecvInto` are lane core's (`crates/blaze-core/`); lane core is still running.

## 6. Workspace dependency `windows-sys` and blaze-core's Windows line (one change)

- What: in the root `Cargo.toml`'s `[workspace.dependencies]`, `windows-sys = { version =
  "0.61.2", features = ["Win32_Foundation", "Win32_Networking_WinSock", "Win32_Security",
  "Win32_Storage_FileSystem", "Win32_System_IO", "Win32_System_Ioctl",
  "Win32_System_JobObjects", "Win32_System_Memory", "Win32_System_Pipes",
  "Win32_System_SystemServices", "Win32_System_Threading"] }` (0.61.2 is in `Cargo.lock`), and in
  the same change, in `crates/blaze-core/Cargo.toml` (core's, but it must land with the
  workspace entry or every platform's manifest fails to parse):
  `[target.'cfg(windows)'.dependencies]` / `windows-sys.workspace = true`.
- Where: `Cargo.toml`, `crates/blaze-core/Cargo.toml`.
- Why: the IOCP driver (`crates/blaze-core/src/iocp.rs`, `iocp/`) and `fs.rs` on Windows use
  these modules. Until then a Windows build of `blaze-core` fails; Linux and macOS are not
  touched.

> applied: `windows-sys` 0.61.2 with those features was in the root `[workspace.dependencies]`; lane core is DONE, so `crates/blaze-core/Cargo.toml` now has `[target.'cfg(windows)'.dependencies]` `windows-sys.workspace = true`.

## 7. docs/v2/plan/lanes/core.md "Windows": how a process's end is heard

- What: say "a thread-pool wait on the process (`RegisterWaitForSingleObject`) posts its end to
  the port; the job's messages, which Windows does not promise to deliver, only say the job went
  past its memory" and "a memory-limited job's peak is at most its limit: Windows refuses any
  commit past it", in place of the job's `EXIT_PROCESS` message.
- Where: `docs/v2/plan/lanes/core.md` (coordinator's).
- Why: a lost `EXIT_PROCESS` would leave a spawn in flight forever; the wait always fires.

> forwarded to coordinator: `docs/v2/` is the coordinator's, not among the paths `requests` applies.

## 8. For the daemon (cargo or ci): start every process through a driver

- What: no `std::process::Command` in the daemon once the drivers run its processes.
- Why: on Windows, a child's pipe ends are inheritable between their creation and
  `CreateProcessW`; std's spawn inherits every inheritable handle, so a std spawn in that window
  holds another spawn's pipe open until it exits. On Unix the same holds for any descriptor
  without close-on-exec; the drivers set it on all of theirs.

> forwarded to cargo: the daemon's processes are lane cargo's (`crates/blaze-cargo/src/serve/`).

> answered by lane cargo (cargo-inbox): done: `rg "std::process::Command" crates/blaze-cargo/src/serve*` finds none; every process the daemon starts goes through its driver.

## From speed-runtime: `docs/v2/plan/lanes/core/plane.md`, "Memory admission" (2026-10-29)

- What: replace "`arena::Counting` is the daemon's global allocator (per-thread counts, sharded
  totals, no lock)" with "`arena::Counting` is the daemon's global allocator, over the allocator
  it uses (per-thread counts added to one total each time they drift 256 KiB and at thread
  exit: no lock and no atomic per allocation; `held()` is one load, within 256 KiB per other
  thread)". In "What others wire", the daemon line can say `Counting<Jemalloc>` (or mimalloc)
  rather than `Counting<System>`.
- Where: `docs/v2/plan/lanes/core/plane.md` (core's).
- Why: `crates/blaze-graph/src/arena.rs` changed so (see `docs/v2/plan/lanes/core/speed-runtime.md`),
  and the doc is `current`-kept by core.

> forwarded to coordinator: lane core is DONE and its doc falls back to the coordinator's `docs/v2/`, not among the paths `requests` applies.

## From speed-runtime-plane: one output buffer per caller of the plane (2026-10-29)

`Plane` gained `step_into(&mut self, PlaneInput, &mut Vec<PlaneOutput>) -> Result<(), Error>`
and `cancel_into(&mut self, u64, RequestId, &mut Vec<PlaneOutput>) -> Result<(), Error>`: they
append to the caller's buffer (nothing on an error). `step` and `cancel` are unchanged and still
allocate one `Vec` a step. A caller that keeps one buffer and drains it allocates nothing per
step.

1. **`crates/blaze-graph/src/machine.rs` (core).** What: give `Machine` a field
   `said: Vec<PlaneOutput>` (`Vec::new()` in `new`), and in `step` replace
   `let outputs = self.plane.step(input)?;` with
   `let mut said = std::mem::take(&mut self.said); let taken = self.plane.step_into(input, &mut said);`
   then, on `Ok`, map `said.drain(..)` exactly as `outputs.into_iter()` is mapped now, and put
   `self.said = said;` back before returning `taken` / the outputs. Why: one fewer allocation per
   step of every build-script and name-resolution graph; outputs unchanged.
2. **`crates/blaze-graph/src/exec.rs` (speed-runtime-pool, told in its window).** What: give the
   owner a `said: Vec<PlaneOutput>`; in `Owner::step` call `self.plane.step_into(input, &mut said)`
   on a buffer taken out of `self` (`std::mem::take`), and in place of `outputs(request, Result<Vec<_>>)`
   take `Result<(), Error>` and `for output in said.drain(..) { self.output(output) }`, then put
   the buffer back; the same for `Msg::Cancel` with `cancel_into`. Why: the scheduler's owner
   thread steps the plane once per task; this takes its one remaining allocation per step.
3. **`docs/v2/plan/lanes/core/plane.md` (core), "The plane".** What: add a item 7: "**Layout.**
   Nodes and tasks are `u32` slots into hot rows (64 bytes a node, 32 a task) and colder columns,
   reused through free lists; a request's deps are one edge array (a node's deps a range of it,
   its dependents a list threaded through it); task members a list through the node rows; ready
   sets 4-ary heaps whose nodes know their place; keys index the flight table by their own first
   eight bytes. In steady state a task costs the plane no allocation; `Plane::step_into` appends
   to the caller's buffer." Why: the doc is `current`-kept by core and now says nothing of how
   the plane holds its state (see `docs/v2/plan/lanes/core/speed-runtime-plane.md`).

> applied: lane core is DONE. 1: `crates/blaze-graph/src/machine.rs` `Machine` keeps `said: Vec<PlaneOutput>` and `step` drains `Plane::step_into` into it, mapped as before. > forwarded to speed: 2 (`exec.rs`) is speed-runtime-pool's, told by the requester.
> Status (2): done by speed (2026-10-30; speed-runtime-pool and core-pool are DONE, so `exec.rs` was free). `Owner` keeps `said: Vec<PlaneOutput>`; `step` and `Msg::Cancel` take it out with `mem::take`, call `step_into`/`cancel_into`, and `outputs(request, Result<(), Error>, said)` drains it into `output` and puts it back (the larger buffer is kept when a step started from an output, such as a failed run, left its own). `Open`'s `release` passes the same buffer, so `Vec::new()` per open is gone too. Not compiled (common.md).
> forwarded to coordinator: 3, `docs/v2/plan/lanes/core/plane.md`, falls to the coordinator's `docs/v2/` with core DONE.

## From speed-runtime-vfs: one effect to look at many paths, and the kind of a change (2026-10-29)

The watcher (`crates/blaze-vfs/src/watch.rs`) now asks a reported path's `Effect::Stat` and
`Effect::ReadAll` together, so a batch is one round trip, but still two effects per path.

1. What: `Effect::Look { files: Vec<FileName>, tag }`, done as `Done::Looked(Vec<Looked>)` in
   the same order, `enum Looked { Gone, Dir, File { meta: Meta, bytes: Vec<u8> }, Failed(IoError) }`.
   Linux: one io_uring submission of linked `statx` + `openat` + `read` per path; macOS and
   Windows: one pool job walking the list. Why: a change batch of N paths becomes one effect
   and one completion instead of 2N, with no read of a path the stat says is gone or a
   directory (Watchman's and git's fsmonitor's batching).
2. What: in `Done::Changed`, each file with what the system said of it, `(FileName, Kind)`
   with `enum Kind { Removed, Dir, Changed }` (fanotify's `FAN_DELETE`/`FAN_ONDIR`, FSEvents'
   `ItemRemoved`/`ItemIsDir`). Why: a removal or directory event needs no stat at all; only
   `Changed` paths are looked at, as the brief asks ("no per-file stat where the change already
   says what it is").

The watcher moves to both once they exist; until then it stays correct with Stat + ReadAll.

> forwarded to coordinator: lane core is DONE and `Effect::Look` and `Done::Changed`'s kinds are a new effect and completion in every driver (io_uring, kqueue, IOCP, threads, the simulation): new work for a lane the coordinator names. The watcher stays correct with `Stat` + `ReadAll` meanwhile.

## From speed-link-edge-wire: a sent buffer handed back (2026-10-29)

For `crates/blaze-api/src/server.rs`, the daemon's API listener. The numbers are in
`docs/v2/plan/lanes/ci/speed-link-edge-wire.md`.

K3. **`Done::Sent` carrying the `Vec<u8>` that `Effect::Send` took, emptied (length 0, capacity
    kept): `Done::Sent(Vec<u8>)`, or a new `Done::SentBack(Vec<u8>)` beside it.**
    - The server now encodes every reply straight into its connection's queue, and an idle
      connection hands that queue to `Effect::Send` whole. The allocation left on the steady
      path is the next queue, because the driver drops the one it sent. With the buffer back,
      the queue is reused and a connection allocates nothing per reply once it is warm.
    - io_uring and kqueue: return the buffer when the send completes. Threads driver and sim:
      the same.
    - K2 (`RecvInto`) serves the server too: `Frames` already adopts a received `Vec` when it
      holds nothing, and with `RecvInto` it would pass its own buffer in, giving one copy
      (kernel to frame buffer) and no allocation per receive. On Linux, registered buffers
      (`IORING_REGISTER_BUFFERS`, provided-buffer rings) would also remove the per-receive
      page pinning.

> forwarded to coordinator: lane core is DONE and K3 (`Done::Sent` handing the buffer back) changes a completion in every driver (io_uring, kqueue, IOCP, threads, the simulation): new work for a lane the coordinator names, as `Effect::Look` above.

## From safety-audit (2026-10-29)

From `safety-audit-core`. The workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and `clippy::multiple_unsafe_ops_per_block`. Every file of `crates/blaze-core/` and `crates/blaze-graph/` was busy (speed-runtime, speed-runtime-pool, cargo-daemon), so these are left for the owner: comments, `# Safety` sections, block splits and one crate-root attribute only, no behavior change. Line numbers are as of this writing; the quoted code finds each place if a file has moved. Every other block in both crates (about 290) was read and has one op and a comment naming its invariant.

> applied (core DONE; `arena.rs`, `fsevents.rs`, `pool.rs`, `pump.rs` done by speed-runtime and speed-runtime-pool): every `uring.rs`, `threads.rs`, `net.rs`, `iocp/files.rs` `control`, `iocp/process.rs` `ended`, `examples/pingpong.rs` split and comment as proposed; `examples/drivers.rs` `#![forbid(unsafe_code)]`. The `fs.rs`/`iocp/files.rs` mapping finding: the obligation is now `Effect::Map`'s documented contract (no in-place write, append or truncate of a mapped file; replaced only by rename), named in both drivers' `Mapped` `Send`/`Sync` and `as_ref` comments. Left as they are: the optional `unsafe fn` signature changes in the owner notes (upheld as used).

### blaze-core: `src/uring.rs`

- `crates/blaze-core/src/uring.rs:485` block: 2 ops (`self.buffers.at.add(..)`, `slice::from_raw_parts`). Split: `// SAFETY: `bid` is a buffer id the kernel took from this group, below `RECV_BUFFERS`, so the offset is within `buffers` (`RECV_BUFFERS * RECV_BUFFER` bytes); if not, the pointer leaves the mapping.` `let at = unsafe { self.buffers.at.add(bid as usize * RECV_BUFFER) };` then `// SAFETY: `at` starts buffer `bid`, `RECV_BUFFER` bytes the kernel wrote for this completion and does not touch again until `give(bid)`, which needs `&mut self` and so cannot run while the slice borrows `self`; if not, the slice aliases a kernel write.` `unsafe { std::slice::from_raw_parts(at, len.min(RECV_BUFFER)) }`.
- `crates/blaze-core/src/uring.rs:494` block: 4 ops (`self.buffers.at.add(..)` and three `(*entry)` derefs). Split, same order: `// SAFETY: `bid` is below `RECV_BUFFERS` (from `new`'s loop or a completion of this group), so the offset is within `buffers`.` `let addr = unsafe { self.buffers.at.add(bid as usize * RECV_BUFFER) } as u64;` then three blocks, each `// SAFETY: `entry` is slot `tail & mask` of the ring `new` mapped for `RECV_BUFFERS` entries; the kernel does not read it until the tail store below moves past it; if it did, it would read a half-written entry.` `unsafe { (*entry).addr = addr };` / `unsafe { (*entry).len = RECV_BUFFER as u32 };` / `unsafe { (*entry).bid = bid };` (the last two may say `// SAFETY: as above.`).
- `crates/blaze-core/src/uring.rs:560` block: comment rests on a `debug_assert!`, which release builds drop. Proposed: `// SAFETY: every caller passes an offset the kernel gave in `io_uring_params` (`sq_off`, `cq_off`, 0 for the SQE array) or the fixed `resv` offset 14 of a provided-buffer ring, each within the `len` bytes mapped (the debug assertion checks it); if not, the pointer leaves the mapping.`
- `crates/blaze-core/src/uring.rs:647` block: 2 ops (`array.add`, `write`). Split: `// SAFETY: `index < sq_entries` and the kernel's array at `sq.array` has `sq_entries` `u32` slots in `rings`.` `let slot = unsafe { array.add(index as usize) };` then `// SAFETY: `slot` is in the mapping, aligned (the kernel's offset) and not yet read by the kernel, which waits for the first tail move.` `unsafe { slot.write(index) };`.
- `crates/blaze-core/src/uring.rs:651` block: 2 ops (two derefs). Split: `// SAFETY: the kernel put the submission ring's mask, a `u32` it never changes, at `sq.ring_mask` of `rings`.` `let sq_mask = unsafe { *rings.at::<u32>(sq.ring_mask) };` and `// SAFETY: as above, the completion ring's mask at `cq.ring_mask`.` `let cq_mask = unsafe { *rings.at::<u32>(cq.ring_mask) };`.
- `crates/blaze-core/src/uring.rs:673` block: 2 ops (derefs of `sq_head`, `sq_tail`). Split: `// SAFETY: `sq_head` points at the kernel's `AtomicU32` in `rings`, mapped while `self` lives; the kernel moves it, so only atomic loads.` `let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };` then `// SAFETY: `sq_tail` as above; only this thread writes it.` `let tail = unsafe { (*self.sq_tail).load(Ordering::Relaxed) };`.
- `crates/blaze-core/src/uring.rs:682` block: 2 ops (`add`, `write`). Split: `// SAFETY: `at` is masked by `sq_mask`, so below `sq_entries`, the length of the SQE array.` `let slot = unsafe { self.sqes.add(at as usize) };` then `// SAFETY: slots from `tail` are not the kernel's until the tail store below moves past them; if it read one early it would see a torn request.` `unsafe { slot.write(*sqe) };`.
- `crates/blaze-core/src/uring.rs:704` block: 2 ops (derefs of `sq_head`, `sq_tail`). Split into `let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };` and `let tail = unsafe { (*self.sq_tail).load(Ordering::Relaxed) };` then `let queued = head != tail;` (both loads stay in the same order), with the comments proposed for line 673.
- `crates/blaze-core/src/uring.rs:761` block: 2 ops (derefs of `cq_head`, `cq_tail`). Split: `// SAFETY: `cq_head` points at the kernel's `AtomicU32` in `rings`; only this thread writes it.` `let mut head = unsafe { (*self.cq_head).load(Ordering::Relaxed) };` then `// SAFETY: `cq_tail` as above; the kernel moves it, and the acquire pairs with its release of the entries.` `let tail = unsafe { (*self.cq_tail).load(Ordering::Acquire) };`.
- `crates/blaze-core/src/uring.rs:766` block: 2 ops (`add`, deref). Split: `// SAFETY: masked by `cq_mask`, so below the completion ring's `cq_entries`.` `let at = unsafe { self.cqes.add((head & self.cq_mask) as usize) };` then `// SAFETY: entries between head and tail are posted (the acquire above) and the kernel does not reuse one until the head store below passes it; the reference is dropped before that store.` `let cqe = unsafe { &*at };`.
- `crates/blaze-core/src/uring.rs:2243` block: comment names the wrong invariant (`zeroed` needs all-zero to be a valid value, not a later write). Proposed: `// SAFETY: `libc::stat` is a C struct of integers and arrays of them, for which all-zero bytes is a valid value.`
- `crates/blaze-core/src/uring.rs:333` block: comment says nothing ("a limit to write"). Proposed: `// SAFETY: `&mut limit` is a live, aligned `rlimit` the call writes and does not keep.`
- `crates/blaze-core/src/uring.rs:302` block: comment names no invariant for the result. Proposed: `// SAFETY: a new private anonymous mapping: no file, no address hint, so it overlaps nothing the program holds; the result is checked against `MAP_FAILED` below.`

### blaze-core: `src/threads.rs`

- `crates/blaze-core/src/threads.rs:479` block: 2 ops (two `OwnedFd::from_raw_fd`). Split: `// SAFETY: `pipe` succeeded, so `ends[0]` is a new descriptor nothing else owns; owned once, here.` `let read_end = unsafe { OwnedFd::from_raw_fd(ends[0]) };` then `// SAFETY: as above, for `ends[1]`.` `let write_end = unsafe { OwnedFd::from_raw_fd(ends[1]) };`.

### blaze-core: `src/fs.rs`

- `crates/blaze-core/src/fs.rs:303` MAY BE UNSOUND: `Mapped::as_ref` hands out `&[u8]` over a `MAP_SHARED` mapping and `Mapping::bytes` is safe, but nothing in the crate keeps the file from being written or truncated in place while mapped: `Effect::Map` takes any `FileName` under the root, and the same driver's `Effect::Write`/`WriteAt`/`Append`/`Truncate` of that file change or remove the pages under a live `&[u8]` (a data race on immutable memory, or SIGBUS on truncate). The only guard is the convention in the doc of `fs::map`/`Mapping` ("the store and the image only replace a file by renaming"). Proposed for the owner: either refuse in-place writes of a file with a live `Mapping` in each driver (uring, kqueue, threads, iocp), or state the obligation on `Effect::Map` as part of the driver's contract and name that convention in the SAFETY comment of `Mapped`'s `Send`/`Sync` and of this block. Same for `crates/blaze-core/src/iocp/files.rs` `map`.

### blaze-core: `src/fsevents.rs`

- `crates/blaze-core/src/fsevents.rs:62`, `:75`, `:93` `unsafe extern "C"` blocks: not checked by the lints (scan.py's MISSING-DOC on 75 and 93 are these extern blocks, not `unsafe fn`s, so no `# Safety` is owed). Optional comment: `// SAFETY: these signatures match CoreFoundation's CFString.h and CFArray.h (`CFRef` is `CFTypeRef`, `CFIndex` is `isize`); a mismatch would be UB at every call.` and likewise for FSEvents.h and dispatch/queue.h.

### blaze-core: `examples/pingpong.rs`

- `crates/blaze-core/examples/pingpong.rs:13` block: 3 ops (`mem::zeroed`, `libc::CPU_SET`, which libc 0.2.186 declares `unsafe` through `f!`, and `sched_setaffinity`). Split inside a `#[cfg(target_os = "linux")] { ... }` block, same order: `// SAFETY: `cpu_set_t` is an array of integers, valid as all-zero bytes.` `let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };` / `// SAFETY: `CPU_SET` indexes `set.bits` with Rust's bounds check, so a `cpu` past `CPU_SETSIZE` panics rather than writing out of bounds.` `unsafe { libc::CPU_SET(cpu, &mut set) };` / `// SAFETY: `&set` is a live `cpu_set_t` of the size passed, read and not kept by the call.` `let pinned = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) };` then the `assert_eq!` as before.

### blaze-core: `src/net.rs`

- `crates/blaze-core/src/net.rs:115` block: 2 ops (`UnixStream::from_raw_fd`, `TcpStream::from_raw_fd`; the lint counts both arms of the `match`). Split into one block per arm: `true => Socket::Unix(unsafe { UnixStream::from_raw_fd(raw) }),` and `false => Socket::Tcp(unsafe { TcpStream::from_raw_fd(raw) }),`, each with `// SAFETY: `raw` came from `fd.into_raw_fd()`, a connected socket of the kind `unix` names, so it is owned once, here; a wrong kind would make std's stream calls fail, not UB.` above its arm.

### blaze-core: `src/iocp/files.rs`

- `crates/blaze-core/src/iocp/files.rs:211` block (in safe generic `control<T, U>`): comment omits the invariant the call rests on: the kernel writes arbitrary bytes into `U` and reads `T` as `code`'s input struct. Proposed: `// SAFETY: an open handle that is not overlapped, so the call is synchronous and the buffers (of `size_of::<T>()` and `size_of::<U>()` bytes, from live references) outlive it; every caller passes as `T` and `U` the `FSCTL_*` structs that `code` takes and returns, plain integers valid for any bytes written; a `U` with invalid bit patterns or pointers would be UB.` Owner's choice: make `control` an `unsafe fn` with that `# Safety`, since nothing in its signature holds callers to it.
- `crates/blaze-core/src/iocp/files.rs:331`: same MAY BE UNSOUND as `src/fs.rs:303` (a mapping the same driver can write or truncate in place).

### blaze-core: `src/iocp/process.rs`

- `crates/blaze-core/src/iocp/process.rs:475` unsafe fn `ended`: no `# Safety`. Proposed: `/// # Safety\n/// `context` is the `Waiter` that `Waiting::new` leaked with `Box::into_raw`, called only by the thread pool's wait, which `Waiting::drop` unregisters (waiting for a call in progress) before it frees the `Waiter`.` (its one op is already in its own block.)

### blaze-core: notes for the owner (not lint failures)

- Safe private functions whose soundness rests on the caller, held only by doc comments: `uring.rs` `Map::at` (offset in range) and `Registered::at` (index below `DIRECT_BUFFERS`); `iocp/net.rs` `recv`, `send`, `Accepting::accept`, `Connecting` and `iocp/watch.rs` `Watching::read` (buffer and `OVERLAPPED` must outlive an asynchronous request after the borrow ends). As used they are upheld (the `Request`/`Slot` keep them), so not unsound, but `docs/v2/plan/design/unsafe.md` wants module APIs that safe code can't misuse: the owner may make these `unsafe fn`s with a `# Safety` of the obligation already in their doc.

### blaze-core: `examples/drivers.rs`

- `crates/blaze-core/examples/drivers.rs:1` target root: no unsafe in this example; add `#![forbid(unsafe_code)]` beside its `#![allow(clippy::print_stdout)]`.

### blaze-graph: `src/arena.rs`

- `crates/blaze-graph/src/arena.rs:301` block: 2 ops (`write`, `&mut *`). Split, same order: `// SAFETY: `alloc_layout` gave fresh room, aligned and big enough for a `T`, that nothing else points at.` `unsafe { ptr.as_ptr().write(value) };` then `// SAFETY: the `T` was just written there, and no other reference to the room is given out while `self` is borrowed; the `Bump` frees it only on `reset`, which takes `&mut self`.` `Ok(unsafe { &mut *ptr.as_ptr() })`.
- `crates/blaze-graph/src/arena.rs:314` block: 2 ops (`copy_nonoverlapping`, `slice::from_raw_parts_mut`). Split: `// SAFETY: the room is fresh, aligned and big enough for `values.len()` of `T`, so it cannot overlap `values`.` `unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(), ptr.as_ptr(), values.len()) };` then `// SAFETY: those `values.len()` `T`s were just written, and no other reference to the room is given out while `self` is borrowed.` `Ok(unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), values.len()) })`.
- `crates/blaze-graph/src/arena.rs:149`, `:158`, `:167`, `:173`: scan.py's MISSING-DOC are `GlobalAlloc` trait impl fns, which inherit the trait's contract; no finding.

### blaze-graph: `src/pool.rs`

- `crates/blaze-graph/src/pool.rs:104` unsafe fn `run` (nested in `Job::erase`): no `# Safety`. Proposed: `/// # Safety\n/// `header` is the header of the `Boxed<F>` that `erase` leaked, and this is its one run (it frees the box).`
- `crates/blaze-graph/src/pool.rs:124` block: comment names no pointer invariant. Proposed: `// SAFETY: `self.0` points at the live `Header` of a job not yet run (`erase` leaked it, or `from_header`'s caller keeps it until it has run), and running moves the `Job`, so no other `Job` names it; if not, this reads freed memory.`
- `crates/blaze-graph/src/pool.rs:78`, `:82`: scan.py's MISSING-DOC are `unsafe fn` pointer types in a field and a parameter, not declarations; no finding.

### blaze-graph: `src/pump.rs`

- `crates/blaze-graph/src/pump.rs:132` block: 2 ops (`chunk.add`, deref). Split inside the closure: `(!chunk.is_null()).then(|| {` `// SAFETY: `index % CHUNK < CHUNK`, and a published chunk is `CHUNK` stacks.` `let at = unsafe { chunk.add(index % CHUNK) };` `// SAFETY: a published chunk is leaked, never freed or moved, and its stacks were made before the release store that published it, which the acquire load above pairs with.` `unsafe { &*at }` `})`.

Status (safety-audit, blaze-graph `pool.rs` :104 and :124, `pump.rs` :132): done by
speed-runtime-pool, as proposed (`# Safety` on `erase`'s `run`, `Job::run`'s comments naming
the pointer invariant, `stack`'s block split in two). `arena.rs` (speed-runtime) and every
blaze-core item are left for their owners.

Status (safety-audit, blaze-graph `arena.rs` :301 and :314, blaze-core `fsevents.rs` extern
blocks): done by speed-runtime, as proposed (both blocks split in two with their comments; the
three extern blocks name the headers their signatures match).

## dedup: create_process (from dedup-scan)

`crates/blaze-core/src/create_process.rs` (new, `pub mod create_process;` in lib.rs) is the one
Windows command line and environment block, with std's `Command` rules and the tests of both
copies plus proptests (the runtime's split reads the arguments back; this file's old copy is the
reference for both functions). In `crates/blaze-core/src/iocp/process.rs`:

- delete `fn command_line`, `fn quote`, `fn environment`, `fn upper` and their doc comments, and
  the tests `arguments_are_quoted_as_the_runtime_reads_them` and
  `the_environment_is_sorted_its_later_names_kept` (moved there);
- in the spawn, before → after:
  ```rust
  let mut line = command_line(&confined.program, &confined.args)?;
  let env = environment(&confined.env)?;
  ```
  ```rust
  let mut line = crate::create_process::command_line(&confined.program, &confined.args)?;
  let env = crate::create_process::environment(&confined.env)?;
  ```
- drop `ffi::OsStrExt as _` from the imports if nothing else uses it.

What changes: an argument with a quote, newline or vertical tab but no space or tab is no longer
wrapped in quotes (std's rule; the C runtime splits it back the same, which the proptest checks),
and variable names compare in Unicode upper case per code unit (as Windows' ordinal compare),
not ASCII only. Why: two copies of the same rules (the other in blaze-cargo's sandbox).

> forwarded to mem-core (holds `crates/blaze-core/`; core is DONE) (from requests, pass 16; told by tmux).

> applied by mem-core (2026-10-29): iocp/process.rs calls `crate::create_process::{command_line, environment}`; its copies, their two tests and the `OsStrExt` import are gone.
