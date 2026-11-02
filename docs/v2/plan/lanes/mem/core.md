---
status: plan
summary: mem-core - blaze-core's I/O operations, timers and watch slots and blaze-graph's plane tasks onto blaze-mem, memory admission reading blaze-mem's counters (each task kind a tag, each tag a budget, a tag over budget throttled first), and the counters on the agent API; each cost against its bound.
date: 2026-10-29
---

# mem-core: the core and the plane onto blaze-mem

Part of lane mem ([memory.md](../../design/memory.md)). Paths: `crates/blaze-core/`,
`crates/blaze-graph/`, `crates/blaze-api/` (lanes core and ci, and every speed agent there, DONE).
Nothing was built or run; the tests are written beside the code for the coordinator's run.

Children, each with its own notes under `docs/v2/plan/lanes/mem/core/`:

| child | paths | notes |
|---|---|---|
| `mem-core-linux` | `blaze-core/src/{uring,watch,fanotify,cgroup}.rs` | `core/linux.md` |
| `mem-core-macos` | `blaze-core/src/{kqueue,fsevents,footprint}.rs` | `core/macos.md` |
| `mem-core-windows` | `blaze-core/src/iocp.rs`, `iocp/` | `core/windows.md` |
| `mem-core-warm` | `blaze-core/src/iocp.rs`, `iocp/` (the warm launcher) | "The warm launcher on Windows" below, `core/warm.md` |
| `mem-core-sim` | `blaze-core/src/{sim,threads,net,process,fs,journal,tests}` | `core/sim.md` |
| `mem-core-times` | `blaze-core/` (`Effect::Times` across every driver) | "File times" below |
| `mem-core-plane` | `blaze-graph/src/{plane,plane_tests,machine,pool,pump}` | `core/plane.md` |

Mine: `blaze-core/src/wheel.rs`, `blaze-graph/src/{exec,admission,arena}.rs`, `blaze-api`.

## Timers: the wheel on a slab

`blaze_core::wheel::Wheel<T>` keeps its 704 slot lists (11 levels of 64) and occupancy bitmaps,
and its timers move from two parallel `Vec`s with a free list to one
`blaze_mem::Slab<Timer<T>>` tagged `Io` (at most `MAX_TIMERS` = 2^22). A timer's
`wheel::Handle` is its slab handle's bits, so a cancel or deadline asked with a handle whose timer
fired or was cancelled, and whose slot now holds another timer, is detected by the generation and
answers `None`: never another timer's value. The lists are intrusive (`prev`/`next` as handle
bits), so arm, cancel and fire touch only the timer and its list's ends.

Tests (in `wheel.rs`): the 400-seed model test against a sorted map now also runs the old wheel
(`wheel/reference.rs`, a verbatim copy, test-only) on the same operations and compares every
answer; a stale handle after reuse; a full slab is an error naming `io`; 10,000 warm arm-fire
rounds (a far timer armed and cancelled, a near one fired) under
`blaze_alloc_count::assert_at_most(0, ..)`.

## The scheduler's runs

`exec`'s run table was a list of leaked 64-slot chunks with a `Vec<u32>` free list. Now:

- each run's slot (`Run`: the pool's job header, the context, the `Live` it runs) is a
  `blaze_mem::Pool<Run>` slot tagged `Plane` (`Ctx::slots`, at most `MAX_RUNS` = 2^16): a pool
  because the slot is read by the thread that runs it while the owner keeps taking others, so it
  must never move and must not sit in a slice the owner holds `&mut` to;
- the owner names runs by `Handle<Slot>` in a `Slab<Slot>` (also `Plane`): the `Pooled` slot, its
  tag, pool and place. A `Done` carries the handle's bits, so a `Done` naming a freed run panics
  naming a stale handle before anything reads the slot;
- the owner takes and recycles slots through one `Cache`, so a warm run makes no call to the
  global allocator (test `a_warm_run_and_its_admission_allocate_nothing`, 10,000 take, admit,
  name, free and end cycles, budget 0);
- the mailbox and the IO queue are `crossbeam_channel::bounded` (`MAILBOX` = 4096 and
  `MAX_RUNS` + the IO threads), which allocate nothing a message where `unbounded` allocates a
  block every 31. The owner never sends to its own mailbox (a node it cannot run fails through
  `Owner::later`), so the owner cannot block on itself.

## Memory admission on blaze-mem's counters

**Blaze's own memory** is `arena::own()` = `arena::held()` (the global allocator's) +
`blaze_mem::tag::snapshot().committed` (every region's committed bytes). `machine_budget` takes
what others use as `memory.current` less `own()`, and the scheduler's `Held` input after each
`Done` is `own()`. Test `blaze_holds_what_blaze_mem_committed_too`.

**A task's peak** is what `arena::measure` gave (the global allocator inside the task) plus the
most its frames held at once. The frame half waits on lane mem: asked by tmux on 2026-10-29 for
`blaze_mem::frame::measure<R>(f) -> (R, usize)`, the most bytes the calling thread's frames held
at once while `f` ran (nested tasks on top of their enclosing ones), nesting, allocation-free.
Once it lands, `exec::measured` is
`let ((did, bytes), frames) = blaze_mem::frame::measure(|| arena::measure(|| arena::task(work)))`
with `peak = bytes + frames as u64`. Until then a task's frames are counted in its tag's
`task_peak` (blaze-mem's `task_ended`) and in `committed`, not in its predicted peak.

**Each task kind is a tag** (`admission::tag_of`), by the ranges of `blaze_query::kinds` and the
daemon's own kinds (`admission::kinds`, which `blaze-cargo`'s `serve/plane.rs` is asked to use):

| kinds | tag |
|---|---|
| `FILE`, `DIR`, `CONFIG` (1 to 3); units loads (`0x6000..0x7000`, `PLAN`) | `vfs` |
| `FACTS` (`0x1000..0x2000`); front `0x2200..0x2400` (names) | `nameres` |
| front `0x2000..0x2200` (syntax) | `parse` |
| front `0x2400..0x2c00` (signatures, bodies, inference) | `infer` |
| front `0x2c00..0x3000` (macros) | `expand` |
| front `0x3000..0x4000`; `LOWER`, `UB_TESTS`, `EVALUATE` | `mir` |
| back `0x4000..0x4800`; `LIBRARY`, `COMPILE`, `REACH` | `codegen` |
| back `0x4800..0x5000` | `link` |
| `USER` (`0x5000..0x6000`) | `usercode` |
| `FETCH` | `store` |
| `WALK` | `query` |
| anything else | `other` |

The kinds in use fit the front and back sub-ranges: `hir-def`'s names at `0x2200..0x2240`,
`hir-ty`'s at `0x2400..0x2600`, `blaze-codegen`'s at `0x4000..`, `blaze-units`' loads at
`0x6010..0x6800`. Lane db is asked to write the sub-ranges into `blaze_query::kinds`
(`v2/requests/mem-core.md`) so that a new kind is placed by its subsystem; the test
`each_kind_belongs_to_its_subsystem` pins the map.

**Each tag's budget** is its share of the machine budget in 64ths (`admission::share`,
`set_budgets`, called by `Scheduler::with_pool` when the plane has a budget):

| share | tags |
|---|---|
| 32/64 | `infer`, `codegen`, `link`, `frames` |
| 24/64 | `mir`, `query` |
| 16/64 | `nameres`, `intern` |
| 8/64 | `parse`, `expand`, `store`, `vfs`, `chunks`, `fuzz` |
| 4/64 | `usercode`, `other` |
| 2/64 | `plane`, `io`, `api` |

The shares add to 270/64: a budget is a cap on one subsystem, not a partition. The phases of a
build overlap little (a build is mostly inference, or mostly codegen and linking, at a time), so
each heavy subsystem may take half the machine, and the machine budget still bounds them
together. A subsystem past its share is a runaway (an interner that never frees, a query table
that keeps every revision), and is held back while the rest still fit.

**A tag over budget is throttled first** (`exec::Throttle`): when the plane runs a node, the
scheduler asks `tag::over_budget(tag_of(kind))`. Over budget, with another run of the tag under
way, the run waits in its tag's FIFO; it starts when a run of the tag ends and the tag is under
budget again, or when no run of the tag is left. So a subsystem whose memory sits in tables its
tasks do not free still has its tasks run, one at a time, and nothing waits for an end that never
comes. The plane counts a held run as running, its memory granted, so holding it back never lets
the plane grant more. At stop, held runs start whatever the budgets, so the table empties. The
queues are intrusive lists in a `Slab` (`Plane`), so holding and starting allocate nothing. Tests:
`the_throttle_matches_a_queue_per_subsystem` (200 seeds, against a per-tag `VecDeque` model),
`a_subsystem_over_its_budget_still_runs_one_at_a_time`.

## The counters on the agent API

`Request::Memory` is answered with `Response::Memory(Memory)` (schema 12): one `TagMemory` per
tag in `Tag::ALL`'s order (`tag`, `live`, `rate`, `budget`, `allocated`, `task_peak`,
`high_water`; declared in their keys' CBOR order, so the writer sorts nothing), plus `committed`
and `committed_high_water`. The rate is `tag::Rate` sampled at the session's clock
(`Session::tick`) since the last memory request. The session reads the counters through
`Daemon::memory: fn() -> Snapshot`, which the daemon sets to `blaze_mem::tag::snapshot`, so the
session stays a state machine and its test gives fixed counters. It goes over the socket and HTTP
like every request; there is no MCP. Tests: `memory_gives_each_subsystems_counters_and_rate` and the
codec identity test, which now generates both variants.

## File times

`Effect::Times { file, modified_ns, tag }`, completing with `Done::Timed`, sets a file's (or a
directory's) access and modification times both to `modified_ns`, nanoseconds since the Unix
epoch: cargo dates a build script's `output` with the time its run began
(`v2/requests/cargo.md`, "cargo-inbox-user: to lane core, the time a written file is given").

**A new effect, not a field on `Effect::Write`.** Read across `crates/` on 2026-10-29:

- `Effect::Write` is built in 11 places outside blaze-core (blaze-cargo, blaze-cas, blaze-query,
  blaze-vfs, blaze-link, blaze-usercode) and taken apart field by field in two tests
  (`blaze-query/src/tests.rs`, `blaze-usercode/src/test/queue/tests.rs`). A field breaks all of
  them; a variant breaks no `match` outside blaze-core, since each there ends in a wildcard (the
  only ones naming `Mode`, its nearest kin, are blaze-link's two, both with `other =>`).
- A time is an attribute set on a file that is there, as `Effect::Mode` is, and has `Mode`'s
  shape in every driver. On a `Write` it would sit inside io_uring's write chain (create,
  write, fsync, rename, directory fsync) as a step no ring op does, and it would be an
  `Option` every other writer carries.
- It dates files the daemon did not write in one `Write` (a directory, a spawn's output).

The cost is ordering: the caller asks for it once the write's `Done::Written` is in, since the
write renames a new file over the name. Durable with the next `Effect::Sync` of the file.

**Exactness (no-fallback).** The time is taken as given. Where the system's call cannot say it,
the effect fails naming the file: a `time_t` of 32 bits (past 2038) on Unix. A `u64` of
nanoseconds always fits a 64-bit `timespec` and, from 1970, a `FILETIME`. What the file system
then keeps is its own resolution, the one loss allowed: NTFS keeps 100 ns (a `FILETIME`'s unit,
so the conversion drops what is below 100 ns), HFS+ and FAT keep 1 s and 2 s, ext4, XFS, Btrfs,
tmpfs and APFS keep nanoseconds. A name not there fails naming it (`ENOENT`,
`ERROR_FILE_NOT_FOUND`); nothing is made.

**Per driver.** No `IORING_OP` sets times (`IORING_OP_SETXATTR` sets extended attributes, not
times), and none of kqueue, IOCP or FSEvents has an asynchronous form.

| driver | how | where its state is | allocations, warm |
|---|---|---|---|
| io_uring | one `utimensat(AT_FDCWD, path, [t, t], 0)` on the driver's thread at submit, as `Mode`, `Stat`, `Remove` are; completion through `ready` | none: no op in the pending slab, the path in the thread's scratch | 0 |
| kqueue | `threads::run` on a file thread (`utimensat`), as every file call there | the file queue's bounded channel | 0 (driver thread and file thread) |
| IOCP | on the polling thread: the path from `sys::Paths`' kept buffers, `CreateFileW(FILE_WRITE_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS)`, `SetFileTime(null, &t, &t)`, close | none: no request in the slab | 0 |
| `Threads` | `threads::run` on a file thread | as kqueue | 0 on Unix; on Windows its `sys::path` allocates the path (not the native driver) |
| simulation | the disk's inode: `mtime` set, `ctime` ticked, a journal entry (as a chmod's) | the inode table | the journal entry's name and the log's growth |

io_uring's choice of the driver's thread over `off_ring`: `off_ring` starts a thread per effect
(a spawn, ~20 µs and allocations) for a call of about a microsecond. IOCP's of the polling
thread over its file threads: a file thread builds its path with `sys::path`, which allocates
several times per call; the polling thread already opens the port's files with `Paths`, which does not.

The simulation reports the inode's mtime in `Stat` and `List` (it said 0 before): the count of
changes the disk keeps for stamps, or the time a `Times` gave. Nothing outside blaze-core read
the 0; blaze-cargo's placements compare two stats' `Meta`, which now see a same-length rewrite in
the simulation as on a disk.

**Speed of light.** One set is the kernel's path walk and inode update: on Linux one syscall
(~0.5 to 1.5 µs for a short cached path, the inode marked dirty, the journal written later). blaze
adds the path joined into a reused buffer (~100 bytes copied, a few ns) and one `Completion`
pushed into a reserved `Vec`: under 1% of the call. Windows has no call that sets times by path,
so three (open, set, close: ~10 to 20 µs on NTFS, the open most of it) is the floor; reusing a
kept handle would save the open, but the port's handles are opened to read or append, without
`FILE_WRITE_ATTRIBUTES`. On kqueue the hop to a file thread and back (~2 to 5 µs) is the driver's
design for every file call (`core/macos.md`), not this effect's. Cargo dates one file per build
script run, so there is no batch to amortize.

Tests: `src/tests.rs` `every_driver_sets_a_files_times_as_asked` (a file and a directory read
back as set by a stat and a listing, a rename keeps it, a name never written and one renamed
away fail naming the file: the simulation, `Threads`, and the native driver of the target) and
`a_warm_times_set_allocates_nothing` (the simulation, `Threads`, io_uring, IOCP);
`kqueue.rs` `a_warm_times_set_allocates_nothing` (the driver's thread and the file thread);
`sim.rs` `times_set_the_mtime_a_stat_reads_on_every_seed` (200 seeds with failures injected: a
failed set changes nothing, a crash keeps the time, a later write moves it on).

## Exec of a just-written file

From `fastcompile-inproc-exec 4` (`v2/requests/fastcompile.md`): the dev executable is written
by the daemon to cargo's path and started by the kernel from there, in the run's sandbox, and a
sandboxed run on macOS looks up the daemon's rendezvous service.

**`ETXTBSY`.** Linux refuses to exec a file that any process has open for writing. The daemon
closes its descriptor before it renames the file into place, but a process forked by another
thread while the descriptor was open holds a copy of it until that process's own exec
(microseconds; Go's `os/exec` issue 22315). So an exec of the file can fail with `ETXTBSY` for
a moment after it was written. A spawn that meets it is made again, of the same program at the
same path, by the schedule in `crates/blaze-core/src/busy.rs`: waits doubling from 50 µs to a
cap of 50 ms, the last one cut so that all of them add up to exactly 1 s (about 30 tries).
Past that the spawn fails with `starting <path>: still open for writing after 1000 ms of tries:
Text file busy`. It never runs another path and never copies the file (no-fallback).

**Where the retry runs: in the parent, a new child each try.** The Linux spawn is
`clone(CLONE_VM | CLONE_VFORK)`, and the child gets a copy of the daemon's descriptor table as
it is at the `clone`. If a writer of the daemon still has the file open then, the child holds a
write descriptor of the file itself, so a child that retried its own `execve` would never
succeed. A new `clone` copies the table as it is now. So nothing sleeps between `clone` and
exec, where only system calls are allowed. `process/spawn.rs` `while_busy` wraps the platform's
spawn: a failure whose kind is `ExecutableFileBusy` (only exec says `ETXTBSY`) is reaped by the
spawn as before, and the spawn is made again after the wait. The cgroup, pipes and plan are made
once and reused across tries.

| platform | spawn | race | what blaze-core does |
|---|---|---|---|
| Linux | `clone` + `execve` in the child | yes | parent retry, a new `clone` each try |
| macOS | `posix_spawn` (one call, `POSIX_SPAWN_CLOEXEC_DEFAULT`) | none from blaze-core's own spawns, whose children never hold the daemon's descriptors; a `fork` elsewhere in the process could still hold one | the same parent retry, which does nothing unless `posix_spawn` says `ETXTBSY` |
| other Unix | std's spawn (which reports the child's exec error) | as Linux | the same parent retry |
| Windows | `CreateProcessW` with an explicit handle list (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) | none: no child is given a handle it was not listed, so none holds a writer's copy | nothing |

The calling thread waits for up to 1 s, but only while the file is busy. On io_uring and kqueue
that thread is the driver's, which runs `process::start` at submit; on `Threads` it is the spawn's
own thread. The expected hold is one fork-to-exec window, so the first 50 µs wait is usually the
only one.

**A sandboxed run.** blaze-core starts the sandbox launcher (blaze itself, not a file just
written), and the launcher's own child execs `Command.program`: `blaze-cargo/src/sandbox/linux.rs`
`exec` (`raw::execve`) and `macos.rs`'s forked child. That exec can meet the same `ETXTBSY`, so
the launcher needs the same loop on the same schedule (`blaze_core::busy::next`). That code is
usercode's, so the change is requested in `v2/requests/usercode.md`. There the retry may run in
the child: each launcher process is one thread, it forked its child after its own exec closed
every descriptor it was started with, and so that child holds no copy of the writer's descriptor.

**Exec inside the sandbox.** Read on 2026-10-29: neither sandbox restricts exec of a path it can
see, so `Command.program` under the target directory already runs and nothing is widened.
- Linux, with namespaces: the readable and writable directories are bound `MS_BIND | MS_REC` with
  no `MS_NOEXEC`, and a view's binds get `NOSUID | NODEV` (and `RDONLY`), not `NOEXEC`. Only
  `/dev` and `/proc` are `noexec`.
- Linux, confined with Landlock: the ruleset handles only the write rights (`WRITE_FILE`,
  `MAKE_*`, `REMOVE_*`, `REFER`, `TRUNCATE`), not `EXECUTE`, so exec is not restricted. If
  Landlock ever handles `EXECUTE`, the rule for that path must give it, and nothing wider.
- macOS: the profile is `(allow default)` and denies only writes, the network and Mach lookups.
  `process-exec` is allowed.
- `Program::command` puts the program in `read`, so it is in the view.

None of this is built in blaze-core; the sandboxes are blaze-cargo's (usercode's).

**`services`.** `Sandbox::On` has `services: Vec<String>`: Mach global names a sandboxed process
may look up beyond the profile's fixed list (`dev.blaze.inproc.<daemon pid>` for an in-process
run). They are ignored on Linux and Windows. blaze-core's constructors give `Vec::new()`. The rest
is requested of other lanes:
- usercode (`v2/requests/usercode.md`): `confine::Spec` and `SpecOf` carry the names; the warm
  pool's key hashes them, since a process allowed other lookups is another process; and
  `macos.rs`'s profile adds `(allow mach-lookup (global-name (param "SERVICE<n>")))` per name.
  As a parameter the name needs no quoting. A name that is empty or has a NUL fails the run,
  naming it.
- cargo and fastcompile: their literals get `services: Vec::new()`.

**Speed of light.** When the file is not busy this costs one compare of the spawn's `Result`. A
busy exec costs one `clone` and one failed exec per try (about 30 to 100 µs on a warm daemon).
The floor is the moment the last writer's copy closes. There is no portable event for that, so
the wait overshoots it by at most the last wait: at most one 50 µs wait for the expected hold,
and under twice the hold once the waits have doubled past it. Linux's inotify `IN_CLOSE_WRITE`
would wake exactly at the close, but it costs a watch per busy spawn on a path that is rare and
microseconds long, so it is not used.

Tests (`crates/blaze-core/src/process/spawn/tests.rs`):
- `busy_waits_add_up_to_the_bound`: from the first try, the waits start at 50 µs, never pass the
  cap, never get shorter but for the last, and add up to exactly 1 s in 20 to 40 tries.
- `busy_waits_are_the_reference_schedule`: 64 seeds × 1000 random points (tries past the
  shift's limit, time waited past the bound) against a loop that doubles.
- `a_program_held_open_for_writing_runs_once_it_is_closed`: every Unix. A copy of `/bin/sh` that
  another thread holds open for writing for 30 ms, from before the spawn, exits 7 as told.
- `a_program_held_open_past_the_bound_fails_naming_it`: Linux. Held for 1.5 s, the spawn fails
  after 1 s to 1.4 s, naming the path, `still open for writing`, kind `ExecutableFileBusy`.

## The warm launcher on Windows

By `mem-core-warm`, for section 2 of `v2/requests/usercode-win-launcher.md`; paths
`crates/blaze-core/src/iocp.rs`, `iocp/{warm,rundirs,process,sys}.rs`. Costs in
[core/warm.md](core/warm.md).

Where the confine has a warm launcher (`Confine::warm`, `blaze sandbox --v2-serve` on Windows),
the IOCP driver hands each spawn with `Sandbox::On` to that one long-lived process; it never
starts one itself, and never falls back to `confine()` or a cold start. A spawn with the sandbox
off starts as ever.

- The launcher. Started on the first handed spawn: a control pipe in (the driver's frames), an
  answers pipe out, the daemon's stderr, no window, in no job; its process is
  waited on (`RegisterWaitForSingleObject`, key `KEY_WARM` carrying the launcher's slab handle,
  so a stale post is told and dropped). It lives in `Iocp::warms` (a `Slab<Warm>` tagged `Io`),
  and `warm_now` is the one runs go to.
- A run. The driver makes the run's job (memory limit, `KILL_ON_JOB_CLOSE`), its three pipes and
  its run directory, duplicates the child's ends (inheritable) and the job into the launcher,
  asks `Confine::hand` for the frame, and queues it; from then the launcher owns its copies, and
  an error before that closes them in it (`DUPLICATE_CLOSE_SOURCE`). The run is an entry of the
  spawn slab with no process, its job followed on the port as ever, its output read as ever, and
  `runs` (run number to spawn, reserved) is how an answer finds it. Frames are written one at a
  time from a queue reserved to `TAGGED`.
- An answer. `Answer { run, code }` frames are cut from one reused buffer (`warm::Cut`, which
  carries a split answer over reads). The code ends the run once its output closed, as a started
  process's exit code does; `COULD_NOT` (125) is the launcher's own failure and is reported as
  the run's code.
- Kill. A handed run's job is sealed (`ActiveProcessLimit` 0, so a run the launcher has not yet
  started can't start in it) and then ended; the launcher answers it as it answers any run.
- The launcher ending. When its answers end (end of file or an error) it is ending: the control
  pipe closes and it ends once its process has exited, so a run answered just before the exit
  keeps its answer. A fault ends it at once: an answer for a run not in flight (never looked up
  in the slab, only in `runs`) or a frame that could not be written. Each run still in it then
  fails with `the sandbox's warm launcher <program> (process <pid>) ended <with code n | (why)>
  before the run did`, its job ended. The next handed spawn starts a new launcher; one that
  fails to start fails that spawn naming the launcher.
- Drop. The control pipe closes first (the launcher ends every run and exits), runs are ended
  by their jobs, then each launcher is waited on for up to `GIVE_BACK`.
- Allocations. A warm handed spawn allocates nothing in the driver beyond what `confine.hand`
  returns (the frame): the pipe names are built on the stack (`sys::WideName`), run directories
  come from reused paths (`RunDirs`, which started sandboxed spawns now use too), and the run
  map, frame queue and answers buffer are reserved or reused. Its exit (`Box<Exit>`) and its
  output are the spawn's own, as for any spawn.

Tests (`crates/blaze-core/src/iocp.rs`, Windows; the test binary is its own launcher, started
before `main` through `.CRT$XCU` when `TINY` is set, with a 52-byte frame of its own):
- `runs_handed_to_the_warm_launcher_end_with_their_own_codes_and_outputs`: two runs answered out
  of order end with codes 7 and 9 and their own stdout; a sandbox-off spawn starts as ever; one
  launcher serves them all.
- `a_killed_handed_run_ends_and_the_launcher_takes_the_next`: a run holding a child in its job is
  killed, and the same launcher takes the next run.
- `a_launcher_that_ends_fails_its_runs_naming_it_and_the_next_run_starts_another`: a launcher
  exiting 3 fails its run naming it and its code; a stale `KEY_WARM` post is dropped; the next
  run starts a second launcher; one answering a run not in flight is ended as a fault.
- `a_warm_handed_run_allocates_only_its_frame_exit_and_output` (`blaze-alloc-count`).
- `warm::tests::answers_cut_across_reads_are_the_records_written` (64 seeds),
  `rundirs::tests` (naming, leftovers, read-only files, a warm make and remove allocating
  nothing) and `sys::tests::a_wide_name_is_the_formatted_one`.

## Speed of light

| path | bound | where it lands |
|---|---|---|
| timer arm | one slab slot write (~48 B, one line) + one list tail link + one bitmap OR: ~3 lines touched, ~5 ns warm | the same: no search, no allocation |
| timer fire | `trailing_zeros` on the level bitmap + unlink + slab remove: ~3 lines, ~5 ns a timer | the same; a cascade moves each timer once a level, as before |
| run take/free | a cache pop + a slab insert, then a slab remove + a cache push: ~4 lines, ~10 ns | the same; the cross-thread floor is the job handoff (one line moved between cores, ~60 to 100 ns), which the slot does not add to |
| admission check | two relaxed loads on the tag's 128-byte line: ~1 ns uncontended, one line transfer (~60 ns) when another core just counted into the tag | the same; the throttle's arrays are owner-only |
| memory record | 19 × 5 relaxed loads (~19 lines) + the record (~1.5 KB of CBOR) | the same; asked rarely, so its 19 name strings are not worth interning |

## Open

- `blaze_mem::frame::measure` (lane mem), then the frame half of a task's peak in
  `exec::measured`.
- `arena::Bump` (a second frame, dedup-mem's finding) stays until blaze-mem can hand work its
  task's frame, bind a frame to a NUMA node, and cap a frame's growth with `Cap`'s spill and
  cancel-the-newest backstop (request C6). Without the last, the move would drop the daemon's
  hard backstop for task memory.
- `v2/requests/mem-core.md`: the daemon's `cx!` sets `Daemon::memory`, and its kinds come from
  `admission::kinds`; `blaze-ui` renders `Response::Memory`; lane db writes the front and back
  sub-ranges into `blaze_query::kinds`.
