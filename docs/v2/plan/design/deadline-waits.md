---
status: plan
summary: Every blocking kernel wait goes through one blaze-core primitive that takes an absolute deadline on the injected clock and recomputes the time left on every retry; each wait's form is chosen from probed kernel facts, with no kernel floor; the profile sampler interrupts no thread with a signal. What is built and what is not.
date: 2026-11-04
---
# Deadline waits and a signal-free sampler

## Why

A profile sampler that sent SIGRTMIN+6 to every span-recording thread about every 20 ms made each
`io_uring_enter` return EINTR, and the ring entered again with the same relative 50 ms timeout,
so the wait never ended (2026-11-05, #1748). This design removes the class: a timeout that
restarts on every retry, and signals sent at blaze's own threads.

## What

1. **One deadline wait.** `blaze_core::wait` is the only code that blocks a thread in the
   kernel. It takes an absolute `Deadline` on the driver's clock (`Driver::now()`, the injected
   clock), recomputes the time left from that clock before every system call, treats an
   interrupted (EINTR) or spurious return as a retry, and ends in a typed outcome:
   `Waited::Ready(T)` or `Waited::Deadline`. A retry bound (fuel) ends a wait whose clock does
   not move with a typed error naming the wait (`bounded-progress`).
2. **`Driver::poll` takes a `Deadline`**, not a `Duration`, so the absolute time passes straight
   through and no layer restarts a timeout. `Sim::poll` honors it: virtual time stops at the deadline instead of jumping to the
   next completion past it.
3. **io_uring** enters with a registered wait region holding the absolute deadline
   (`IORING_ENTER_EXT_ARG_REG | ABS_TIMER`; a retry rewrites only the region's time) and a signal mask that blocks every asynchronous signal but the stop signals
   (SIGINT, SIGTERM, SIGHUP, SIGQUIT) during the wait. A process-directed signal goes to a
   thread that does not block it; a stop signal still interrupts, and `wait` retries with the
   time left. kqueue, and later IOCP (`GetQueuedCompletionStatusEx`; Windows is out of scope
   now), take their relative timeouts from `wait` the same way.
4. **The ban.** `.clippy.toml` bans direct kernel waits outside `crates/blaze-core/src/wait*`,
   each with its replacement in `blaze_core::wait`: `libc::{poll, ppoll, select, pselect,
   epoll_wait, epoll_pwait, kevent, kevent64, nanosleep, clock_nanosleep, usleep, sleep}` and
   the Windows waits. Existing callers move: `wait::readable` (an fd set with a deadline) for
   `poll`, `wait::sleep_until` for sleeps. The backends call `wait::uring_enter`,
   `wait::kevent` and `wait::completions` (IOCP), which hold the one system call each.
5. **Pool tasks never block in the kernel.** Only IO driver threads (and the pool's idle park)
   block there. A task that lacks a fact returns `Need`, parks, and is resumed when its
   producers have merged (D157, D161); a timer is on the driver's wheel.
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
   - Span and fiber attribution: perf's samples arrive after the fact, so a sample matched to
     the flight rows reads span 0. Each flight ring has a **context log**: a fixed ring of `(ticks, span, job, fiber)` entries its thread
     writes at every change of its innermost span and every fiber switch (enter, exit,
     `take_open` and its restore, `switched`), atomics only. The sampler matches a sample's
     time to the entry in force then. A matched span that closed before the sampler read it
     (and may have been folded) gives way to its innermost ancestor still open; a sample older
     than the log's oldest entry reads span 0 and is counted in the dump's sampler line.
   - macOS: `thread_suspend` + `thread_get_state` + `thread_resume`; it reads the same
     per-thread slot (innermost span, job, fiber) directly while the thread is stopped.
   - Windows (later): `SuspendThread` + `GetThreadContext` + `ResumeThread`, the macOS shape.
   - Where perf cannot be opened (`perf_event_paranoid` above 2 without `CAP_PERFMON`, a seccomp
     filter), the sampler is off and every dump says why: no fallback to a signal.

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
- `thread_suspend` / `thread_get_state` / `thread_resume` (Mach): the sampler.
- `mach_wait_until` (Mach, absolute `mach_absolute_time` deadline): `wait::sleep_until` on
  macOS, so a sleep's deadline is the kernel's and no relative restart exists.

- `os_sync_wait_on_address_with_deadline` (public since 14.4, absolute deadline on
  `OS_CLOCK_MACH_ABSOLUTE_TIME`) over `__ulock_wait2` (private, relative ns): `wait::Parker`
  on macOS (`wait/macos.rs`), so the pool's idle park. The public one is
  more capable here (absolute deadline), so it wins.

Every macOS interface in use, each one decided against and what is still to measure is the
audit in [docs/macos.md](../../../macos.md).

Later, where each wins:
- `thread_selfcounts` (private, per-thread cycles and instructions): `self_cpu_ns` and an
  instructions column per span.
- `kevent_id` / workloop kqueues: per-thread queues, if a second driver thread appears.
- `sandbox_init_with_parameters`: the macOS sandbox profile.
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

## Kernel interfaces (D185)

No minimum kernel. Each wait with several forms (the ring's absolute-timer enter and registered
wait region, `IORING_OP_FUTEX_WAIT`/`WAKE`, `IORING_OP_WAITID`, the SQE-embedded timeout, or
`futex`, `waitid`, `pidfd_open` and `ppoll` by direct call) is chosen once at start from the
probed `KernelFacts` (blaze-sys), printed in the profile, never tried and then retried another
way; a wait with no form on this kernel is a typed refusal naming the missing fact. Inside
blaze's sandbox, whose seccomp filter refuses `io_uring_setup`, the launcher's mark
(`__BLAZE_SANDBOXED=1`, read once by `wait::choose_from_env` before any thread waits) selects the
direct calls.

## Built

- The wait: `crates/blaze-core/src/wait.rs` and `wait/` (`retry.rs`: `Deadline`, `Waited`, the
  retry core; `unix.rs`: `readable`; `sleep_until` per system); `Driver::poll` takes a
  `Deadline` (blaze-core lib.rs, kqueue.rs).
- Linux: a per-thread wait ring (`wait/ring.rs`: `SINGLE_ISSUER | DEFER_TASKRUN`, a registered
  wait region holding the absolute deadline and the signal mask); `wait::Parker`
  (`wait/parking.rs`) on `IORING_OP_FUTEX_WAIT`/`WAKE` for the pool's idle park; child exits on
  `IORING_OP_WAITID` (`wait/reaping.rs`); the direct driver (`wait/direct.rs`) and its choice
  (`wait/choice.rs`). A pool whose park fails stops with the error (`blaze-graph/src/pool.rs`,
  `Broken`).
- macOS: `mach_wait_until` for sleeps and `os_sync_wait_on_address_with_deadline` for the park
  (`wait/macos.rs`).
- The `.clippy.toml` bans on `poll`, `ppoll`, `select`, `pselect`, `epoll_wait`, `epoll_pwait`,
  `kevent`, `kevent64`, `nanosleep`, `clock_nanosleep` and `std::thread::park*`.
- The perf sampler with the context log (`crates/blaze-profile/src/sampler/linux.rs` on
  `ix_prof::perf`); no signal sampler exists.

## Not built

- The choice of forms from `KernelFacts`: the ring's forms are assumed, not read from the probe.
- The bans on `waitpid`, `waitid`, `wait4` and raw `syscall(SYS_futex)` outside `wait`.
- `pool::block_until`'s client park and the pool's stopper, still on `thread::park`.
- The uring driver's pidfd `POLL_ADD` moved onto `OP_WAITID`.
- Windows (later, out of scope now): the `WaitOnAddress` park and the sampler.

## How it is proven

A DST (`dst-not-review`), `crates/blaze-core/src/wait/tests.rs`: the tape draws a sequence of
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

The direct driver runs the park's and the reap's plans of that DST through its own
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
names the span and fiber it was taken in on Linux (the property perf lacked), run in CI.
