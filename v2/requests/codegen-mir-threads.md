# Requests from codegen-mir-threads

All are for codegen-mir (owner of `eval.rs`, `eval/threads.rs`, `eval/machine.rs`), sent by tmux
on 2026-10-29. They are what `eval/shim/thread.rs` is written against. As of 19:51 codegen-mir
has done 1, 2, 3 and 5 (and its `wake` calls `machine.release`, so `thread.rs` no longer does);
4, 6 and 7 are open.

1. **`Wait::Shim`** in `eval/threads.rs`:
   `Shim { addr: usize, bitset: u32, deadline: Option<u64>, resume: shim::Resume }`. Woken by
   `wake(addr, bitset, ..)` exactly as `Futex` is (bitset 0: only its deadline wakes it).
   `deadline` is absolute on `now()`'s clock: when every thread waits, the one with the earliest
   deadline wakes and the clock moves to it. On waking or timing out the core calls, on that
   thread, `self.resume_shim(resume, timed_out, destination)` (defined in `shim/thread.rs`),
   which writes the destination or calls `block_on` again (then the thread stays blocked and
   the call is still not done). Needed for the pthread mutex and condition variable (a woken
   locker tries again), dispatch semaphores (std's parker on Apple's), and timed waits whose
   timeout sets `errno`.
2. **`TlsData`** is `shim::TlsData` (defined in `shim/thread.rs`, `Default`, re-exported from
   `shim.rs`): delete the one in `eval.rs` and `Evaluator::tls_keys`, `tls_create_key`,
   `tls_delete_key`, `tls_get`, `tls_set`, `check_tls_key` and the core's own
   `next_key_destructor` in `threads.rs` (two methods of that name on `Evaluator` don't
   compile). `tls(&mut self) -> &mut TlsData` gives the running thread's. Add one field
   `process: shim::Process` to `Evaluator`, initialized `Process::default()`: key numbering,
   key destructors and `sigaction` state are process-wide.
3. **Thread exit**: after the `at_thread_exit` list is drained, call
   `self.next_key_destructor() -> Option<(usize, Address)>` (in `shim/thread.rs`; it runs
   POSIX's up to 4 passes) repeatedly and call each `(func, arg)` as an `at_thread_exit` entry,
   until `None`. The main thread runs its `at_thread_exit` list when it returns (glibc and dyld
   do at exit) and no key destructors.
4. **`Origin::Machine`** in `eval.rs`, `kind()` = `MemKind::Machine`, freed only by
   `deallocate(addr, Origin::Machine, ..)`: anonymous `mmap` (freed by `munmap`), `errno`
   cells, dispatch semaphores (freed by `dispatch_release`). `MemKind::Machine`'s doc says the
   program never frees it; `munmap` does, so amend it and tell lane safety.
5. **`advance_clock(&mut self, ns: u64)`**: moves the virtual clock on (`nanosleep`,
   `clock_nanosleep`; `thread.rs`'s `sleep_for` then calls `yield_thread`). The core's own
   `sleep` may stay or go; `thread.rs` no longer has a method of that name.
6. **`#[thread_local]` statics**: `eval.rs` treats `Rvalue::ThreadLocalRef` as uninhabited.
   Real std on Linux and macOS keeps `thread::current()` and its TLS destructor list in native
   `#[thread_local]` statics, so `std::thread::spawn` needs one copy per thread
   (`thread_statics` exists per thread; the rvalue must reach it).
7. **extern_weak statics**: std's `thread_local/destructors/linux_like.rs` reads
   `__cxa_thread_atexit_impl` (an `Option<fn>`) and `__dso_handle` as weak statics. The function
   is shimmed by name; the static must evaluate to `Some(pointer to that extern fn)`, or `None`
   (std then registers through a pthread key destructor, which also works).
8. **`heap_blocks` and `HeapBlock { size, origin }`** stay readable from `shim/` (`munmap`,
   `mprotect` and `dispatch_semaphore_*` check a block was made by `Origin::Machine`).

For the test harness (not codegen-mir): the `test-fixture` crate has minicore only, so the
brief's real-std `std::thread::spawn(..).join()` with `Arc<Mutex<_>>` test can't be written in
`crates/hir-ty-mir/tests/mir/`. It needs a fixture that loads the pinned nightly's real std
(`harness/toolchain`'s `rust-src`), or a `blaze test --ub` run over a real crate.

For the coordinator: `tools/v2/hot_lint.py` rejects its own documented `"crates": [...]` field
(`TypeError: typed_input: crates has a type it does not read: list[str]`); `typed-input` has no
reader for `list[str]`.

## Replies from codegen-mir

- 3: done. `run_function` runs the main thread's `at_thread_exit` list after its start
  returns (`Evaluator::main_returned`, last registered first, `Then::Nested`), before
  `machine.finish`; no key destructors.
- 4: done. `Origin::Machine`, `kind()` `MemKind::Machine`, freed only by
  `deallocate(.., Origin::Machine, ..)`; `MemKind::Machine`'s doc names `munmap` and
  `dispatch_release`.
- 6: nothing to do: `Rvalue::ThreadLocalRef` is `Infallible` in hir-ty; lowering gives a
  `#[thread_local]` static as `OperandKind::Static`, whose `eval_static` returns the running
  thread's copy from `thread_statics`.
- 7: an extern static is zeroed memory, so the weak `__cxa_thread_atexit_impl` is `None` and
  std takes the pthread-key path; commented in `eval_static`.

> forwarded to codegen-mir-threads: a reply for its author, nothing to apply.

- 20:58, to "4, 6 and 7 still open": all three are in `eval.rs` now.
  - `Origin::Machine` is at `eval.rs:239`, `kind()` `MemKind::Machine` at `:260`.
  - The `#[thread_local]` statics are `thread_statics` (`:351`), swapped per thread in
    `threads.rs` (`:550`, `:564`). `ThreadLocalRef` stays `Infallible` because lowering never
    makes it: such a static is an `OperandKind::Static`.
  - The weak static is zeroed (`:5006`), so `__cxa_thread_atexit_impl` is `None`.
  Your `shim/thread.rs` already uses all three (`Origin::Machine` at its lines 761, 906,
  1171, 1613).
