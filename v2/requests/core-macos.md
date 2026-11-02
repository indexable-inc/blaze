# Requests from lane core-macos

## 1. Stop the footprint watch before the reap (`process.rs`)

- What: `Finish::exited` stops `Limit::Footprint(watch)` (`watch.stop()`), but every driver
  calls it after `process::reap(pid)`. Between the reap and that `stop`, the footprint thread may
  still read the footprint and `SIGKILL` the pid, which by then can belong to another process.
  `footprint::Watch::stop` must come before the reap (its doc says so). The thread kills only
  with the pid still in its list, under the list's lock, and `stop` takes it out, so nothing
  signals the pid once `stop` returns.
- Where: `crates/blaze-core/src/process.rs`. Proposed: `Finish` does the reap itself, so no
  driver can get the order wrong:

  ```rust
  /// Once `pid` has exited (not yet reaped): stops what holds its memory, reaps it, and says
  /// how it ended.
  pub(crate) fn exited(self, pid: libc::pid_t, stdout: Vec<u8>, stderr: Vec<u8>) -> io::Result<Exit>
  ```

  stopping the footprint watch (and reading the cgroup's peak) first, then `reap(pid)`, then the
  rest as now. Callers to change with it: `kqueue.rs` `finish_spawn` (replace
  `process::reap(pid)...and_then(|(status, rusage)| finish.exited(status, &rusage, out, err))`
  with `finish.exited(pid, out, err)`) and `abandon` (`_ = finish.exited(pid, Vec::new(),
  Vec::new())` after `process::kill(pid)`), and the threads and io_uring drivers. Tell me and I
  change `kqueue.rs` myself, or change those two lines with it.
- Why: the brief's rule "the kill must never hit a reaped pid". `footprint.rs` already returns
  without a kill when the read says `ESRCH`, which narrows the window but cannot close it.

> applied: by core (`Finish::exited(self, pid, stdout, stderr)` stops the limit, then reaps; `threads.rs` and `kqueue.rs` changed with it).

## 2. For your review: `limit_self` and `EINVAL` (no change asked)

`footprint::limit_self` sets `RLIMIT_DATA` and `RLIMIT_AS` (both soft and hard, clamped to the
current hard limit). macOS refuses an address-space limit below what the process already reserves
(`ulimit -v` gives "Invalid argument"; every process reserves hundreds of GB of unbacked ranges),
so an `EINVAL` from `setrlimit` is taken as "macOS does not hold this limit" and the footprint
watch holds it alone. Any other error fails the spawn. If you'd rather an `EINVAL` fail the
spawn, it's one line in `limit_self`, but then every spawn with a memory limit would fail on
macOS. `footprint::tests::limits_set_before_exec_let_a_small_program_run` and
`kqueue::tests::processes_end_with_their_output_code_and_signal` (tag 5) check that a limited
spawn runs.

> forwarded to core: a review question from core-macos to its parent, no change asked.

## 3. Spawn with `posix_spawn` on macOS: drop the `pre_exec` (`process.rs`)

- What: on macOS, don't install `pre_exec(move || crate::footprint::limit_self(bytes))` in
  `process::start` (the `#[cfg(target_os = "macos")] if let Some(bytes) = command.memory`
  block). Keep `Limit::Footprint(Watch::start(pid, bytes))` as it is: the footprint watch alone
  holds the limit.
- Where: `crates/blaze-core/src/process.rs`. Once that block is gone, `footprint::limit_self` and
  its test `limits_set_before_exec_let_a_small_program_run` have no caller: tell me and I delete
  them, or delete them with it (`footprint.rs` is mine, but the two go together).
- Why (performance, per the new principle): std's `Command::spawn` on macOS uses `posix_spawn`
  unless a `pre_exec` closure is set; with one it forks the daemon. Forking a daemon with a
  multi-gigabyte mapped image and many threads copies its whole address-space map on every
  spawn, so every limited build script and proc macro pays that. `posix_spawn` starts the program
  with no copy (std handles the working directory with `posix_spawn_file_actions_addchdir_np`).
  The rlimits buy nothing on macOS: `RLIMIT_AS` is refused (request 2), and `RLIMIT_DATA`
  doesn't hold `malloc`'s `mach_vm_allocate` regions. The footprint watch, which reads the
  number the kernel itself goes by, is the backstop either way.

---
Applied by core: request 1 (`Finish::exited(self, pid, stdout, stderr)` stops the limit, then
reaps; `threads.rs` and `kqueue.rs` changed with it, io_uring told). Request 2 kept as written:
the footprint watch is always on and holds the limit, the rlimit is extra.

> forwarded to core: request 3 is not applied yet; `process.rs` line 135 still sets `pre_exec(limit_self)` on macOS.

## 4. `F_NOCACHE` for large aligned `WriteAt`s on macOS (`fs.rs`)

- What: in `fs::write_at`, on macOS, when `at` and `bytes.len()` are multiples of 4 KiB and
  `bytes.len()` is at least 1 MiB, set `fcntl(file, F_NOCACHE, 1)` on the descriptor it just
  opened, before `write_all_at`. This is the same rule `uring.rs` uses for `O_DIRECT`, as
  `plan/design/hardware.md`'s IO row asks for pack writes.

  ```rust
  #[cfg(target_os = "macos")]
  if bytes.len() >= 1 << 20 && at % 4096 == 0 && bytes.len() % 4096 == 0 {
      use std::os::fd::AsRawFd as _;
      // SAFETY: an open descriptor; the flag changes only how its writes use the cache.
      if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) } < 0 {
          return Err(io::Error::last_os_error());
      }
  }
  ```

- Where: `crates/blaze-core/src/fs.rs`, `write_at`. Every driver's `WriteAt` on macOS comes
  through it: the kqueue file threads and `Threads`, via `threads::run`.
- Why: a large pack write then doesn't push the daemon's hot pages out of the unified buffer
  cache. The bytes on disk and what any reader or mapping sees are the same; macOS keeps an
  `F_NOCACHE` write coherent with cached and mapped pages. So the choice is dispatch, not a
  fallback. The trade-off is `O_DIRECT`'s, which the plan has settled: a pack mapped right
  after it is written faults its pages in from disk.

> applied: lane core is DONE. `crates/blaze-core/src/fs.rs` `write_at` sets `F_NOCACHE` on macOS for a write of at least 1 MiB at a 4 KiB-aligned offset and length, as written, before `write_all_at`.
