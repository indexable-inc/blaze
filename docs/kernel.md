---
status: current
summary: blaze's minimum Linux kernel (7.3), why, and every kernel feature blaze uses or plans to, with its version, use, owner and status.
---
# The Linux kernel blaze runs on

blaze needs **Linux 7.3 or later**. Below it, the client and `blaze serve` both stop before
they do anything else, with a typed refusal (`TooOld` in `crates/blaze-core/src/kernel.rs`,
checked by `blaze_core::kernel::check` from `blaze_cargo::cli::main`) that names the running
kernel, the floor, the features that need it and the fix:

    error: blaze needs Linux 7.3 or later, and this is Linux 6.8.0-1070-azure (rule
    kernel-floor, docs/kernel.md). It uses, with no slower path:
      - openat2 OPENAT2_REGULAR (Linux 7.2): store and cache reads that open only a regular file
      - ...
    Fix: boot Linux 7.3 or later: your distribution's newer kernel package (a mainline build,
    Ubuntu's HWE kernel, Fedora's or Arch's current kernel), or run blaze in an ix VM (`ix new`),
    which boots 7.3. A container shares its host's kernel, so the host needs it.

## Why 7.3

Andrew's call, 2026-11-05: fail on older kernels with a clear message, then use the latest
features freely. The floor was set at 7.2 and raised to 7.3 the same day. 7.3 is what ix VMs and
ix CI runners boot (7.3.y from November 2026), so every machine blaze is built, tested and
benchmarked on has it. The floor is the ix kernel, not the newest feature in use: the 7.3
features below are planned, and land on a floor that already has them. A floor means one code path per
feature: no probe, no "Linux X or later has it" message, no slower path for a kernel that lacks
it (`no-fallback`). What a 7.3 kernel can still lack is configuration (`CONFIG_IO_URING=n`,
Landlock off with `lsm=`), a sysctl (`kernel.io_uring_disabled`), a seccomp filter (Docker's
default profile refuses io_uring) or a file system's support; each is an error naming that, never
a version.

The floor removed, on 2026-11-05: the probe and workaround for Ubuntu linux-azure 6.8's inverted
check of a provided buffer ring's reserved words (`ReservedCheck`, #1790) and its hosted
`linux-6-8` CI job; the version-worded io_uring, cgroup, fanotify, Landlock, THP and
`close_range` errors and probes. The `kernel-floor` job in `.github/workflows/blaze.yaml` runs the
gate's binary under a faked older kernel (the `UNAME26` personality, `setarch --uname-2.6`) and
checks the refusal; `crates/blaze-core/src/kernel/tests.rs` does the same on one thread.

### CI

GitHub's hosted Linux runners boot 6.8 to 6.11, below the floor, so no job runs blaze on one:
every Linux job that runs blaze is on an ix runner (docs/harness.md, "ix runners"). Release,
PGO and install builds link against an Ubuntu 22.04 glibc sysroot (`.github/scripts/sysroot.py`)
so they still ask for an old glibc; blaze-link gives a libc under the C driver's `--sysroot` the
glibc ABI's interpreter (`fhs_target` in `crates/blaze-link/src/target.rs`). x86-64 is
cross-compiled on ix AArch64 and not run. llvm.yaml's Linux LLVM builds stay hosted: they run
cmake, not blaze. macOS jobs are unaffected.

## Feature map

Status: **in use** (blaze calls it, unconditionally), **planned** (owned by the lane named,
not built), **next** (an opportunity found, unowned).

### In use

| Feature | Linux | What blaze uses it for | Crate | Status |
|---|---|---|---|---|
| io_uring: `IORING_SETUP_SQPOLL` + `SINGLE_ISSUER`, registered files and buffers, sparse file table, file alloc range | 5.13 to 6.0 | Linux's one IO driver (`blaze_core::uring`) | blaze-core | in use |
| io_uring provided buffer rings (`IORING_REGISTER_PBUF_RING`), multishot receive | 5.19 | a connection's receives | blaze-core | in use |
| `IORING_OP_SEND_ZC` | 6.0 | large TCP sends from the asker's pages | blaze-core | in use |
| `statx` `STATX_DIOALIGN` | 6.1 | the alignment of writes that bypass the cache | blaze-core | in use |
| fanotify `FAN_REPORT_DFID_NAME` | 5.9 | file watches | blaze-core | in use |
| cgroup v2 `memory.peak`, `cgroup.kill` | 5.19, 5.14 | a spawned process's memory limit and peak | blaze-core | in use |
| Landlock, ABI 7 or later | 6.15 | the build-script and proc-macro sandbox | blaze-cargo | in use |
| `SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP`, `pidfd_open` | 6.6, 5.3 | the sandbox's supervisor | blaze-cargo | in use |
| `PR_SET_THP_DISABLE` with `PR_THP_DISABLE_EXCEPT_ADVISED` | 6.18 | huge pages only where blaze advises them | blaze-mem | in use |
| `MADV_POPULATE_WRITE`, `MADV_COLLAPSE` (advice) | 5.14, 6.1 | populating and collapsing the daemon's arenas | blaze-mem | in use |
| `close_range` | 5.9 | the test fork server's descriptor cleanup | blaze-usercode | in use |
| `statx` `STATX_MNT_ID` | 5.8 | ix-prof's FUSE check | ix-prof | in use |
| `openat2` `OPENAT2_REGULAR` | 7.2 | store and cache reads open only a regular file (`EFTYPE` otherwise) | blaze-core | in use (this lane) |
| file IO error reporting (`fserror`, delivered as fanotify `FAN_FS_ERROR`) | 7.0 | the store's durability path learns of a write the file system lost | blaze-core, blaze-cas | in use (this lane) |
| `RWF_DONTCACHE` | 6.14 (XFS 6.15, ext4 6.17, NFS 6.18) | store writes the build won't read back soon stay out of the page cache, on a file system that takes it | blaze-core | in use (this lane) |
| `mseal` | 6.10 | sealing the loader's code mappings that are never changed | blaze-live | in use (this lane) |
| `cachestat` | 6.5 | which store entries are warm (resident in the page cache) | blaze-cas | in use (this lane) |

### Planned (owned elsewhere, not built here)

| Feature | Linux | What blaze would use it for | Owner | Status |
|---|---|---|---|---|
| io_uring `IORING_OP_FUTEX_WAIT`/`WAKE`/`WAITV` | 6.7 | a driver waits on a futex in the ring with its other IO | deadline-waits | planned |
| io_uring `IORING_OP_WAITID` | 6.7 | a child's exit reaped in the ring, not by polling its pidfd | deadline-waits | planned |
| io_uring SQE-embedded timeout (`IORING_TIMEOUT_IMMEDIATE_ARG`) | 7.1 | a timeout's nanoseconds in `sqe->addr`, no timespec to keep alive | deadline-waits | planned |
| io_uring registered wait regions (`IORING_REGISTER_MEM_REGION`, `IORING_ENTER_EXT_ARG_REG`) | 6.13 | the wait's deadline and sigmask read from a registered region, no copy per enter | deadline-waits | planned |
| `MAP_DROPPABLE` | 6.11 | caches the kernel may zero under pressure | daemon-memory | planned |
| cgroup `memory.reclaim`, PSI triggers (unprivileged from 6.4) | 5.19, 5.2 | the daemon reclaims before it is killed, on a pressure trigger | daemon-memory | planned |
| `MADV_COLLAPSE` on demand, mTHP sizes (`hugepages-<N>kB`) | 6.1, 6.8 | right-sized huge pages for arenas | daemon-memory | planned |
| io_uring incremental provided buffers (`IOU_PBUF_RING_INC`) with `min_left` | 6.12, 7.1 | a receive fills a buffer in pieces, retired below a minimum | blaze-core uring | planned |
| io_uring registered-buffer send/recv (`IORING_RECVSEND_FIXED_BUF` on `SEND`/`RECV`) | 7.2 | sends and receives from registered buffers, no page pinning per call | blaze-core uring | planned |
| Landlock scoping (`LANDLOCK_SCOPE_*`) | 6.12 | abstract Unix sockets and signals scoped to the sandbox | sandbox lane | planned |
| Landlock `LANDLOCK_RESTRICT_SELF_TSYNC` (ABI 8) | 7.0 | one ruleset on every thread of a process at once | sandbox lane | planned |
| Landlock `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` (ABI 9) | 7.1 | Unix socket connects by path ruled | sandbox lane | planned |
| io_uring cBPF filters and per-task inherited restrictions (`IORING_REGISTER_BPF_FILTER`, `IORING_REGISTER_RESTRICTIONS` with fd -1) | 7.0 | what sandboxed code may ask of io_uring, inherited across fork | sandbox lane | planned |
| `OPEN_TREE_NAMESPACE`, nullfs | 7.0 | a sandbox's mount namespace from one tree, on an empty immutable root | sandbox lane | planned |
| clone3 `CLONE_AUTOREAP` (and `CLONE_PIDFD_AUTOKILL`, `CLONE_NNP`) | 7.1 | sandboxed children reaped and killed with their parent | sandbox lane | planned |
| pidfd `PIDFD_THREAD`, `PIDFD_GET_INFO` | 6.9, 6.13 | a thread's pidfd; a child's exit status and cgroup from its pidfd | sandbox lane | planned |
| rseq time-slice extension (`PR_RSEQ_SLICE_EXTENSION`, `rseq_slice_yield`) | 7.0 | a scheduler lock's holder asks for its slice to be extended | scheduler locks (unowned) | planned |
| failfs and `fchroot(2)` | 7.3 | blaze's own worker and helper processes sandboxed alongside Landlock: a root changed through a descriptor, onto a file system that refuses everything not bound into it | sandbox lane | planned |
| io_uring futex in-flight accounting fix | 7.3 | the deadline wait's futex-through-the-ring step (`IORING_OP_FUTEX_WAIT` above) relies on it; before 7.3 a ring's futex waits were miscounted in flight | deadline-waits | planned |
| userfaultfd read-write protection (`UFFDIO_REGISTER_MODE_RWP`) | 7.3 | which mapped store and cache pages are hot: a read or write of a protected page faults once and is counted | store (unowned) | planned |

### Next (found while setting the floor, unowned)

| Feature | Linux | Where | Why |
|---|---|---|---|
| `IORING_OP_FTRUNCATE` | 6.9 | `Effect::Truncate` in `crates/blaze-core/src/uring.rs` | the cut is a synchronous open and `ftruncate` off the ring; on the ring it needs an open linked before it |
| `IORING_OP_STATX` with `AT_EMPTY_PATH` on a descriptor | 5.6 | `crates/blaze-core/src/uring/files.rs`, a descriptor's size | `size_of_file` is synchronous |
| `statmount`/`listmount`, `STATX_MNT_ID_UNIQUE` | 6.8 | ix-prof's mount check | in place of parsing `/proc/self/mountinfo` |
| `O_EMPTYPATH` | 7.2 | reopening a held descriptor | no `/proc/self/fd` path |

Sources: kernelnewbies.org Linux_7.0, Linux_7.1 and Linux_7.2, and each constant read from
torvalds/linux's uapi headers at the release tag that added it.
