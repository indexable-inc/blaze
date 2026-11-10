---
status: plan
summary: Every blocking kernel wait goes through one blaze-core primitive that takes an absolute deadline on the injected clock and recomputes the time left on every retry; the profile sampler stops interrupting threads with signals; a stuck daemon start names the span it is stuck in.
date: 2026-11-04
---
# Deadline waits and a signal-free sampler

## Why

On 2026-11-05 `blaze serve` 0.20261105.0 hung before it listened on Linux. The profile sampler
(`crates/blaze-profile/src/sampler/linux.rs`) sent SIGRTMIN+6 to every thread that records spans
about every 20 ms. Each `io_uring_enter` on the IO driver thread returned EINTR, and
`Ring::enter` (`crates/blaze-core/src/uring.rs`) entered again with the same relative 50 ms
timeout, so the wait never ended. The client said only "did not listen after 60s". #1748 fixed
that one call. This plan removes the class: a timeout that restarts on every retry, signals sent
at blaze's own threads, and a hang that cannot say where it is.

## What

1. **One deadline wait.** `blaze_core::wait` is the only code that blocks a thread in the
   kernel. It takes an absolute `Deadline` on the driver's clock (`Driver::now()`, the injected
   clock), recomputes the time left from that clock before every system call, treats an
   interrupted (EINTR) or spurious return as a retry, and ends in a typed outcome:
   `Waited::Ready(T)` or `Waited::Deadline`. A retry bound (fuel) ends a wait whose clock does
   not move with a typed error naming the wait (`bounded-progress`).
2. **`Driver::poll` takes a `Deadline`**, not a `Duration`. Every caller already holds deadlines
   (the daemon's loop computes them from its session, jobs and observer, then turned them into
   a relative wait), so the absolute time passes straight through and no layer restarts a
   timeout. `Sim::poll` honors it: virtual time stops at the deadline instead of jumping to the
   next completion past it.
3. **io_uring** keeps `IORING_ENTER_EXT_ARG`, with the timespec recomputed by `wait` on each
   entry, and passes a signal mask that blocks every asynchronous signal but the stop signals
   (SIGINT, SIGTERM, SIGHUP, SIGQUIT) during the wait. A process-directed signal goes to a
   thread that does not block it; a stop signal still interrupts, and `wait` retries with the
   time left. kqueue and IOCP (`GetQueuedCompletionStatusEx`) take their relative timeouts from
   `wait` the same way.
4. **The ban.** `.clippy.toml` bans direct kernel waits outside `crates/blaze-core/src/wait*`,
   each with its replacement in `blaze_core::wait`: `libc::{poll, ppoll, select, pselect,
   epoll_wait, epoll_pwait, kevent, kevent64, nanosleep, clock_nanosleep, usleep, sleep}` and
   the Windows waits. Existing callers move: `wait::readable` (an fd set with a deadline) for
   `poll`, `wait::sleep_until` for sleeps. The backends call `wait::uring_enter`,
   `wait::kevent` and `wait::completions` (IOCP), which hold the one system call each.
5. **Fibers never block in the kernel.** Only IO driver threads (and the pool's idle park)
   block there. A fiber that waits, with a deadline or without, parks on the scheduler: a key,
   a counter, a job, or a timer on the driver's wheel. This was already the rule
   (`standard-structures`); it moves into `deadline-waits`, one home.
6. **Signal-free sampling.** Nothing signals a pool or driver thread.
   - Linux: one `perf_event_open` task-clock event per thread that holds a flight ring,
     `PERF_SAMPLE_TID | TIME | CALLCHAIN` (user call chains the kernel walks by frame pointer,
     `use_clockid` `CLOCK_MONOTONIC`), with `context_switch` records in the same ring, read by
     the sampler thread. Off the CPU: a switch-out that is not a preemption, inside a span,
     becomes off-CPU samples weighted by how long it blocked, with no stack. The kernel's stack
     of a block is a kernel-mode sample of a context-switch counter, which `exclude_kernel`
     drops and kernel sampling needs `perf_event_paranoid` 1 or lower; the sampler must run at
     2 (the common default), so the span says where it blocked (the raw-block gate decides by
     span ancestry) and macOS keeps its blocked stack. A preemption counts as on the CPU
     (runnable), as macOS's `TH_STATE_RUNNING` does. The perf ring reader is
     `crates/ix-prof/src/perf.rs`, shared with ix-prof's sampler, so blaze has one perf reader.
   - Span and fiber attribution: perf's samples arrive after the fact, which is why the
     signal sampler replaced perf on 2026-11-04 (every sample read span 0). Each flight ring
     gets a **context log**: a fixed ring of `(ticks, span, job, fiber)` entries its thread
     writes at every change of its innermost span and every fiber switch (enter, exit,
     `take_open` and its restore, `switched`), atomics only. The sampler matches a sample's
     time to the entry in force then. A matched span that closed before the sampler read it
     (and may have been folded) gives way to its innermost ancestor still open; a sample older
     than the log's oldest entry reads span 0 and is counted in the dump's sampler line.
   - macOS: `thread_suspend` + `thread_get_state` + `thread_resume`, as today; it reads the
     same per-thread slot (innermost span, job, fiber) directly while the thread is stopped.
   - Windows: `SuspendThread` + `GetThreadContext` + `ResumeThread`, the macOS shape. Later
     step: there is no Windows sampler today, so nothing signals there.
   - The SIGRT sampler, its alternate signal stacks and `process_vm_readv` probe are deleted.
     Where perf cannot be opened (`perf_event_paranoid` above 2 without `CAP_PERFMON`, a seccomp
     filter), the sampler is off and every dump says why, as today: no fallback to a signal.
7. **Hangs name themselves.** The daemon already writes its start phase (launched, state,
   setup, waiting, listening) on the ready pipe. It also writes, every 500 ms until it
   listens, what its starting thread is inside: the innermost span's name, its fields and how
   long it has been open, from the flight ring (`threads_now`). The client keeps the last one
   and a start that never listens says it: "did not listen at ...: it was setting up for 60s,
   inside `open store` (path=..., 59.8s)".

## Losing alternatives

- Absolute `IORING_OP_TIMEOUT` (`IORING_TIMEOUT_ABS`) in the ring: needs a timeout request kept
  in step with the wheel's next deadline (cancel or update on every change), and the retry
  bug moves to whoever recomputes it; one recomputing primitive covers every backend alike.
  `IORING_ENTER_ABS_TIMER` needs Linux 6.12.
- Keeping the signal sampler and blocking SIGRT around every wait: every blocking call in every
  dependency would need the mask; the next one that does not hangs again.
- Correlating perf samples with the flight rows themselves: rows are folded and wrap at 32 k;
  the context log is small, unfolded and written at exactly the points attribution needs.

## macOS private APIs (rule `macos-private-apis`)

Adopted now:
- `thread_suspend` / `thread_get_state` / `thread_resume` (Mach): the sampler, as today.
- `mach_wait_until` (Mach, absolute `mach_absolute_time` deadline): `wait::sleep_until` on
  macOS, so a sleep's deadline is the kernel's and no relative restart exists.

Later, where each wins:
- `os_sync_wait_on_address_with_deadline` (public since 14.4, absolute deadline on
  `OS_CLOCK_MACH_ABSOLUTE_TIME`) over `__ulock_wait2` (private, relative ns): the pool's idle
  park, once it moves off `std::thread::park` into `wait::park_until`. The public one is more
  capable here (absolute deadline), so it wins; `__ulock_wait2` stays the candidate for older
  systems blaze does not support.
- `thread_selfcounts` (private, per-thread cycles and instructions): `self_cpu_ns` and an
  instructions column per span.
- `kevent_id` / workloop kqueues: per-thread queues, if a second driver thread appears.
- `sandbox_init_with_parameters`: the build-script sandbox (`crates/blaze-cargo/src/sandbox/macos`).
- `kdebug_trace` / `os_signpost`: span boundaries in Instruments.

Each private symbol gets a comment naming it and why, and a test that resolves it at run time
(`dlsym`) and fails naming the symbol if it is gone.

## Cost budget

- The wait: one clock read per entry, as `poll` already did. No allocation.
- The context log: three relaxed stores and a clock read the flight ring already made, per span
  enter and exit and per switch; 4096 entries of 40 bytes (with the parent span), 160 KiB a ring.
- perf: one task-clock event with switch records per ring-holding thread, one mmap ring of
  64 KiB. Switch records cost a record per switch; measured on a self-build before
  and after, quoted in the sampler PR from the spans (`slow-is-p0-traced`).

## Migration

Whole ports (`whole-ports`): `Driver::poll` changes signature with every caller in one PR; the
`.clippy.toml` ban lands with the callers moved and no baseline growth. The sampler swap is one
PR: the SIGRT code is deleted in it.

Steps, each its own PR:
1. Rules `deadline-waits` and `macos-private-apis`, and this spec.
2. `blaze_core::wait`, `Driver::poll(Deadline)`, the backends and `Sim` on it, the io_uring
   signal mask, the ban with callers moved, and the DST.
3. The daemon's start says what it is inside.
4. The perf sampler with the context log, fiber attribution on both systems, SIGRT deleted.
5. Every kernel wait through the ring (Linux floor 7.3, the ix VM kernel, on Andrew's call;
   no probe, no fallback; what is adopted is recorded in docs/kernel.md):
   - The deadline wait itself: the SQE-embedded timeout (7.1) in place of `EXT_ARG`'s
     separate timespec, and a registered wait region (6.13) holding the deadline and the
     signal mask, so `Ring::enter` passes nothing per call and a retry rewrites only the
     region's absolute time.
   - Idle pool threads (`blaze-graph` `Local::park`, `pool.rs:2211`; the older fiber
     driver's `idle`, `fiber.rs:1455`): each worker parks on its own small ring with
     `IORING_OP_FUTEX_WAIT` (6.7) on its slot word and one absolute timeout for the next
     due job or stall check; `Shared::wake` and `unpark_workers` wake it with
     `IORING_OP_FUTEX_WAKE` (or a plain `FUTEX_WAKE` from a thread with no ring). This step
     needs 7.3: it fixes io_uring futex in-flight accounting, so the waits are private
     (`FUTEX2_PRIVATE`) only and a wake is not counted as a request in flight. The pool's
     timers are then driven by that timeout instead of being seen only at the next wake.
   - A client thread off a fiber (`pool::block_until`, `pool.rs:2489`) parks the same way.
   - Child processes: `IORING_OP_WAITID` (6.7) on the driver's ring replaces the blocking
     `wait4` in `process::reap` (`process.rs:476`) and the `waitid` of the Threads driver's
     errand thread (`process.rs:499`); `blaze test`'s `wait_exited`
     (`blaze-cargo/src/raw/process.rs:110`) and `ended_within` move onto a ring of their own.
   - The driver's own sleeps: the cgroup `rmdir` retry and `wait_empty` poll
     (`cgroup.rs:246,276`) and the `ETXTBSY` exec retry (`busy.rs:40`) become ring timeouts
     and a ring `POLL_ADD` on `cgroup.events`, so the driver's thread never sleeps outside
     its ring.
   - The stampers' channel (`uring.rs:1503`) stays a futex through the pool's park above.
   - Out of scope: the sandbox launcher (`sandbox/linux.rs`), whose seccomp filter refuses
     io_uring to the program it launches; its `waitpid` and `poll` stay on `wait::`. What it
     launches waits with direct system calls (step 6).
   Then `waitpid`/`waitid`/`wait4`/`thread::park*` and raw `syscall(SYS_futex)` are banned
   outside `blaze_core::wait`, the macOS park takes `os_sync_wait_on_address_with_deadline`,
   Windows `WaitOnAddress`, and the Windows sampler follows.

   Landed (2026-11-05, branch `ring-waits`): a per-thread wait ring in
   `blaze-core/src/wait/ring.rs` (`SINGLE_ISSUER | DEFER_TASKRUN`, a registered wait region
   holding the absolute deadline and the signal mask); `wait::Parker` (core
   `wait/parking.rs`) on `IORING_OP_FUTEX_WAIT`/`WAKE` for the pool's idle park and wakes;
   `wait::exited`/`wait::reap` (core `wait/reaping.rs`) on `IORING_OP_WAITID` with `WNOWAIT`
   then `wait4(WNOHANG)` for `process::reap`, `wait_exited` (and so the Threads errand) and
   blaze-cargo's `wait_exited`; `wait::pollable` (ring `POLL_ADD`) for `ended_within` and
   `cgroup.events`; `wait::sleep_until` as a ring `TIMEOUT` for the cgroup and `ETXTBSY`
   retries. The DST covers the park (planted `ParkPlant::CheckThenStore`, a lost wakeup) and
   the reap (planted `ReapPlant::ReapInWait`, a reaped child never reported).
   Still to do: the uring driver's pidfd `POLL_ADD` onto `OP_WAITID`, the pool's stopper and
   `block_until` client parks, `fiber.rs`'s idle, the SQE-embedded timeout, the bans above,
   the macOS and Windows parks and the Windows sampler.
6. A second Linux driver for a process inside blaze's sandbox (landed 2026-11-10). From step 5
   (commit efcabd8fc6) every park was a request on the thread's ring, and the sandbox's filter
   answers `io_uring_setup` with `ENOSYS`: blaze as the `rustc` a build script runs (anyhow's
   probe, and most of crates.io's) started its pool, each worker's first park failed, the
   worker ended unheard (the probe's stderr is `/dev/null`) and the main thread waited forever
   for jobs no worker would run.
   - `wait/direct.rs`: the same cores (`parking::park`, `reaping::reap`, `until`'s retry) over
     direct calls. A park is `futex(FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG)` to an absolute
     `CLOCK_MONOTONIC` time and `FUTEX_WAKE`; a child's exit is `waitid(P_PID, WEXITED |
     WNOHANG | WNOWAIT)` to look, `pidfd_open` and `ppoll` of the pidfd to wait, and the look
     again; `pollable` is `ppoll` under the ring's signal mask; a sleep is
     `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)`. Each call's time is made from what
     `until` recomputed just before it, never carried across a retry. The sandbox's filter
     refuses only `io_uring_setup`, two terminal ioctls and another architecture's calls, so
     all of these are allowed in it and the filter is not widened.
   - Chosen once, never probed (`wait/choice.rs`, rule `no-fallback`): the Linux launcher sets
     `__BLAZE_SANDBOXED=1` in the environment of the program it runs (`mark_sandboxed`), which
     every program below it inherits, and blaze's `main` reads it before any thread exists or
     waits (`wait::choose_from_env`): the direct driver with the mark, the ring without, an
     error for any other value. A second choice, or one after a thread has waited, panics
     naming both. Without the mark a refused ring stays an error, which names the mark.
   - A worker that cannot wait stops its pool (`blaze-graph/src/pool.rs`, `Broken`): the
     park's error is kept and said once, the workers run what is queued and end without
     parking again, and every scope on the pool fails with the error, a thread blocked on one
     (`block`) woken to do so; a worker's loop that panics does the same. A wait on a scope
     whose jobs are parked with no worker left cannot be failed through the scheduler, and
     ends the process, saying so.
   Not covered: a thread that waits on a broken pool through something other than a scope (a
   channel a detached `Pool::submit` answers on) is not woken; and a program that clears its
   environment before it runs blaze inside the sandbox loses the mark, and that blaze fails at
   its first park, naming it.

## How it is proven

A DST (`dst-not-review`), `crates/blaze-core/src/wait/tests.rs`: Hegel draws a sequence of
waits (a deadline, or none), when the awaited event arrives (before, at or after the deadline,
or never), and a swarm of kernel faults on a simulated kernel with a virtual clock: spurious
EINTR at any blocking call (and a drawn train of them at a fixed period, the sampler's shape),
spurious wakeups with nothing ready, a timeout returned early, and system-call latency. The
oracle, at every wait: it ends by its deadline (virtual time at return is at most the deadline
plus one call's latency), it never reports `Deadline` before the deadline, and it reports
`Ready` whenever the event arrived first. The planted bug (`wait::Plant::SameTimeout`, a
test-only switch) re-enters with the first relative timeout after an interrupt; the DST must
find a counterexample within its case budget. `Sim` gets the same fault
(`Faults::interrupt_one_in`): its `poll` goes through `wait::until`, so every existing DST on
`Sim` runs its waits under interruption too.

The direct driver (step 6) runs the park's and the reap's plans of that DST through its own
calls (`Through::Direct`): the simulated kernel answers `futex`, `waitid`, `pidfd_open` and
`ppoll` with the errors the kernel returns, on a `CLOCK_MONOTONIC` of another origin than the
wait's clock, and the same oracles and plants hold (`no_unpark_is_lost_through_the_direct_driver`,
`every_reaped_child_is_reported_through_the_direct_driver`, and their `the_oracle_catches_...`
controls). `wait/direct.rs`'s own tests run it on the real kernel: a park woken, one ended by
its deadline, one interrupted by signals, a child waited for past a deadline and reaped, a
poll, a sleep. In `blaze-graph/src/pool.rs` (`mod broken`),
`a_sandboxed_process_parks_its_workers_with_no_io_uring` runs the test binary again under a
seccomp filter that refuses `io_uring_setup`, marked as the launcher marks it: its workers park
and wake and a `par` loop runs on the process's pool; the control, unmarked, fails naming the
ring (`the_oracle_catches_a_sandboxed_process_on_the_ring`). A pool whose parks fail fails its
scoped waiter within a bound, and the planted worker that ends silently leaves it parked
(`the_oracle_catches_a_worker_that_ends_silently`).

The sampler: the existing `a_busy_thread_is_sampled_and_named` test, plus one that a sample
names the span and fiber it was taken in on Linux (the property perf lacked), run in an ix VM.
The start: a test of the client's message from a ready pipe that says a span and never listens.
