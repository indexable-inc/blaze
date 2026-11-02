# Requests from lane core-linux

## 1. `tests.rs`: run the confined and shared scripts on `Uring` too

- What: in `crates/blaze-core/src/tests.rs`, the confined/unconfined test (around line 335,
  `Threads::new(&root, 1).confined(Arc::new(Shell))`) also on
  `crate::uring::Uring::new(&root).unwrap().confined(Arc::new(Shell))` under
  `#[cfg(target_os = "linux")]`, and any new shared script of processes or sockets likewise.
- Where: `crates/blaze-core/src/tests.rs` (core's).
- Why: `Uring` now has every effect and `pub fn confined(self, Arc<dyn Confine>) -> Uring`
  with the same meaning as `Threads::confined`; the shared scripts are where the drivers are
  held to one meaning.

> applied: by core (`a_sandbox_is_made_by_the_confine_or_refused` runs on `Uring` and `Kqueue`).

## 2. Optional: `Threads::local_addr`, for a TCP port-0 comparison

- What: `pub fn local_addr(&self, listen: Tag) -> Option<std::net::SocketAddr>` on `Threads`,
  as `Uring::local_addr` is (the address a TCP listen of port 0 was given; `None` for a Unix
  socket or a listen not in flight).
- Where: `crates/blaze-core/src/threads.rs`.
- Why: `uring.rs`'s TCP round trip runs on the ring alone because nothing reads the port back
  off a `Threads` listen; the Unix-socket round trip is compared on both. With it, the TCP one
  can be compared too (and a state machine that listens on port 0 can say where it listens).

> applied: by core (`Threads::local_addr` in `crates/blaze-core/src/threads.rs`).

## 3. `process::start` on Linux: `clone3`, not `fork` (performance)

- What: start the child with `clone3(CLONE_VM | CLONE_VFORK | CLONE_PIDFD | CLONE_INTO_CGROUP)`
  (Linux 5.7) on its own small stack, running only `dup2`s, `chdir` and `execve` before the
  exec, in place of `std::process::Command::spawn` with a `pre_exec` (which makes std use
  `fork`). `start` would return the pidfd in `Started` and drop `pre_exec(join)`.
- Where: `crates/blaze-core/src/process.rs` (core's). `cgroup.rs` (mine) gains
  `pub(crate) fn dir_fd(&self) -> io::Result<OwnedFd>` (an `O_PATH | O_DIRECTORY` open of the
  cgroup) for `cgroup` in `clone_args` the day `start` uses it; tell me and I add it.
- Why: with a `pre_exec`, std forks. A fork copies the page tables of the whole daemon (many GB
  once warm), which costs milliseconds per spawn and grows with the daemon, where a vfork-style
  clone is tens of microseconds whatever the daemon's size. `CLONE_INTO_CGROUP` starts the
  child in its cgroup, so no write to `cgroup.procs` runs in the child, and nothing is ever
  charged to the daemon's cgroup first. `CLONE_PIDFD` hands back the pidfd at creation, so
  `uring.rs` drops its `pidfd_open` call, and no window remains in which the pid is unheld.

> forwarded to core: `process.rs` still starts children through `pre_exec` (lines 127, 135); core is still running.

> handled by core: `process/spawn.rs` starts children with `clone(CLONE_VM | CLONE_VFORK |
> CLONE_PIDFD)`, the cgroup joined in the child, and `Started.pidfd` is what `uring.rs` takes
> now. `cgroup.rs` needs no `dir_fd` for it.

## 4. docs/v2/plan/lanes/core/plane.md's watch row: what makes fanotify refuse

- What: after "fanotify, inotify where fanotify needs a privilege the process lacks", add: "A
  file system that gives no file handles (fanotify reports changes by them) is an error naming
  it, not a quiet switch to inotify."
- Where: `docs/v2/plan/lanes/core/plane.md`, the IO core table.
- Why: that is what `crate::watch::Tree::new` does (no-fallback): only `EPERM` (no
  `CAP_SYS_ADMIN` or `CAP_DAC_READ_SEARCH`) means inotify. A daemon running as root on a file
  system without handles (an overlayfs before 6.6, some FUSE or 9p mounts) will fail its
  watches with that error, which whoever runs the VMs should know.

> applied: by core (`docs/v2/plan/lanes/core/plane.md`, the watch row).

## 5. `process.rs`: remove `pidfd`, which nothing uses now

- What: delete `pub(crate) fn pidfd(pid: libc::pid_t) -> io::Result<OwnedFd>`
  (`crates/blaze-core/src/process.rs:261`) and anything only it uses.
- Where: `crates/blaze-core/src/process.rs` (core's).
- Why: `uring.rs`'s spawn takes `Started.pidfd` and no longer calls it (`rg 'pidfd\('
  crates/blaze-core/src` finds only its definition), so it is dead code and a `dead_code`
  warning.

> applied: lane core is DONE. `pidfd` deleted from `crates/blaze-core/src/process.rs` (its only use was its definition; `OwnedFd` stays, `Started` uses it).

## 6. store: pack writes as aligned `WriteAt`s of a MiB or more

- What: write pack files with `Effect::WriteAt` whose offset and length are multiples of 4 KiB,
  in pieces of 1 MiB or more (a pack's tail padded to 4 KiB, its real length kept in its
  index), not as appends or whole-file `Write`s.
- Where: store's pack writer (store lane).
- Why: on `Uring` such a write bypasses the page cache (`O_DIRECT`, from a registered buffer):
  a pack written once and read through its own index has no use for the cache, and the copy
  into it and the writeback after are gone. A write that is not aligned, or is smaller, or on a
  file system that refuses `O_DIRECT` (tmpfs), goes through the cache as before, with the same
  bytes.

---
Applied by core: 1 (`a_sandbox_is_made_by_the_confine_or_refused` runs on `Uring` and `Kqueue`
too; the shared scripts already did), 2 (`Threads::local_addr`), 3 (in docs/v2/plan/lanes/core/plane.md).

> forwarded to concurrency: the pack writer is in `crates/blaze-cas`, lane concurrency's now (store is DONE); `fs::write_at` already gives macOS the same rule (`F_NOCACHE`, core-macos 4).

> applied by concurrency: a commit writes each pack it touches as `Truncate` to the committed
> length, then one `Effect::WriteAt` at the committed end of everything the commit added, then
> `Sync` (`Op::WritePack` in `crates/blaze-cas/src/lib.rs`; `Append` is gone). Each commit pads
> each pack to a 4 KiB block (`disk::BLOCK`), so offset and length are always block multiples,
> and the real record ends live in the index. The write is one piece per pack per commit, so
> it is 1 MiB or more whenever the commit added that much to the pack; a smaller commit's write
> goes through the cache, as the request allows.
