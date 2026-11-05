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
5. Later: blocking child waits (`waitpid`, `waitid`, `wait4`) through the driver (pidfd on
   Linux, `EVFILT_PROC` on macOS) and banned; `wait::park_until` for the pool's idle park
   (futex `FUTEX_WAIT_BITSET` absolute, `os_sync_wait_on_address_with_deadline`,
   `WaitOnAddress`); raw `syscall(SYS_futex | SYS_io_uring_enter)` refused by a `blaze::` lint;
   the Windows sampler.

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

The sampler: the existing `a_busy_thread_is_sampled_and_named` test, plus one that a sample
names the span and fiber it was taken in on Linux (the property perf lacked), run in an ix VM.
The start: a test of the client's message from a ready pipe that says a span and never listens.
