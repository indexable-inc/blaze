---
status: plan
summary: The speed of light of the MIR evaluator's thread shims (pthread, futex, ulock, os_sync, dispatch semaphores, keys, clock), what bounds them and how `eval/shim/thread.rs` reaches it.
date: 2026-10-29
---
# The thread shims' speed of light: notes of codegen-mir-threads

Notes of `codegen-mir-threads`, as of 2026-10-29. Code: `crates/hir-ty-mir/src/mir/eval/shim/thread.rs`;
the thread core it calls is codegen-mir's `eval/threads.rs`.

## The bound

A shim runs once per call the program makes, between two MIR terminators the evaluator spends
on the order of 100 ns to 1 µs interpreting (operand evaluation, place projection, the
machine's hooks). The work of a shim itself is a few loads and stores of the object's own bytes
(a mutex's owner, count and kind; a futex word; a key's value), plus at most one call into the
core (`block_on`, `wake`, `spawn_thread`). The speed of light is therefore the cost of those few
memory accesses, about 10 to 50 ns: well under the call's own dispatch, so a shim is at the
bound when it does no work proportional to anything but its arguments and allocates nothing on
the path a running program repeats (lock, unlock, wait, wake, get and set of a key).

## How the code reaches it

- **State in the object's own bytes.** A mutex, condition variable or attribute keeps its state
  at fixed offsets of its own memory (glibc's fields, after Apple's `__sig`), so a lock or
  unlock is three 4-byte reads and writes, with no side table to hash into.
- **Keys by index.** `pthread_getspecific`/`setspecific` index the thread's `TlsData::values`
  by key number: one bounds check; a set grows the vector only the first time a key is used on
  a thread.
- **No allocation on repeated paths.** Locks, unlocks, futex waits and wakes, `sched_yield`,
  `clock_gettime` and key reads allocate nothing. Allocation happens only at setup (the first
  `errno` cell of a thread, `sigaction`'s saved bytes, `mmap`, a dispatch semaphore) and in
  errors that end the run, each marked `// alloc:`.
- **The target read once.** OS, architecture, syscall numbers, page size and errno codes come
  from the crate's cfg on the first thread shim and are cached in `Process::target`, so later
  calls branch on a `Copy` struct.
- **Dispatch by name** is the evaluator's existing extern-C path (a string `match` per extern
  call), shared with every other shim. Turning that into a pre-interned `u16` symbol table is
  the evaluator's change, not this file's; the thread shims would then dispatch on the enum.

## What is left to the core

Waking cost is the core's: `wake` scans the threads for waiters on an address (at most 240
threads, so a scan of a few hundred bytes). A per-address waiter list would make it
proportional to the waiters only; with today's thread counts in tests the scan is below the
call's own dispatch.
