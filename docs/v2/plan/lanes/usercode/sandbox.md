---
status: plan
summary: How fast blaze's sandbox traces and confines a run, part by part, against the fastest known techniques (Bazel, BuildXL, tup, fsatrace, strace, eBPF, fanotify, Endpoint Security, Detours), and what closing each gap would take.
date: 2026-10-29
---
# blaze v2 usercode: the sandbox's speed against the state of the art

Notes of `usercode-sandbox` (child of `usercode`), as of 2026-10-29. Code:
`crates/blaze-cargo/src/sandbox.rs` and `crates/blaze-cargo/src/sandbox/`. Nothing here was
built or run for these notes. The lane's brief allows no builds. The costs are counted from
the code, not measured.

## What a traced run must give

What a run read: every path opened, looked at, listed or run, every variable read by name,
and what can't be traced (the realtime clock, randomness, the network). Each is said once per
scope and appended to the live record before the call that made it goes on. Enforcement
(what the run may not write or reach) is a separate layer: namespaces, overlayfs and Landlock
on Linux, Seatbelt on macOS, an AppContainer in a job object on Windows. A tracer that misses a
call makes a cached result wrong, so completeness comes before speed.

## Linux: seeing the calls

| Technique | Who uses it | Per traced call | Sees | Needs |
| --- | --- | --- | --- | --- |
| ptrace (strace) | strace, BuildXL for static binaries | 2 stops and several context switches | every call | nothing |
| seccomp user notification | blaze, gVisor's notify mode | one round trip to the supervisor, only for filtered calls | every filtered call, static binaries and raw syscalls too | Linux 5.0 (5.5 for `FLAG_CONTINUE`) |
| `LD_PRELOAD` interposer | fsatrace, BuildXL, tup before FUSE | a function call, a buffer append | only calls through libc | nothing |
| FUSE | tup | a round trip per file operation, on every file under the mount | file operations under the mount | `/dev/fuse` |
| eBPF (LSM or tracepoints) | tracing tools | in-kernel, no context switch | everything | `CAP_BPF` and `CAP_PERFMON`, or root |
| fanotify | file monitors | an event per open, batched reads | opens and accesses, not `stat`, not per thread | `CAP_SYS_ADMIN` for mount marks and permission events |
| Landlock | blaze (confined runs) | none | nothing: it only enforces | Linux 5.13 |

What blaze does: a classic BPF filter sends only the calls the record needs to the supervisor
and lets the rest by. That includes the monotonic clocks, which the filter tells from the
realtime ones by the clock ID. Each sent call is answered with `SECCOMP_USER_NOTIF_FLAG_CONTINUE`,
so the kernel runs the call itself and nothing is emulated. The listener asks for
`SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP` (Linux 6.6, written for gVisor): the held thread switches
straight to the supervisor on its own CPU. An older kernel refuses the flag, and that costs only
latency. The interposer (`linux_interpose.c`) adds only what no system call shows. That is a
variable read by name, said once per thread and scope from a fixed table per thread, with no
lock and no `malloc`. It also covers the clocks the vDSO answers without a call.

- **Beats** ptrace on cost per call, and beats fsatrace and BuildXL's interposer on
  completeness. A static binary, a Go program or a raw `syscall` is seen without falling back
  to ptrace. Unlike eBPF and fanotify, it needs no privilege.
- **Falls short** of an in-process interposer on cost per call. Each traced call is a round
  trip through the supervisor: `poll`, then `SECCOMP_IOCTL_NOTIF_RECV`, then `..._SEND`. The
  kernel has no ioctl that receives or answers several notifications at once. `RECV` also
  blocks without waking when the last process ends, so `poll` (which reports that) stays.
  *What it would take*: record the calls that go through libc inside the interposer, into a
  ring buffer per thread in memory shared with the supervisor. The filter would then allow
  calls made from the interposer's own `syscall` instruction (`seccomp_data` has the
  instruction pointer) and keep notifying all others. Calls through libc would cost what
  fsatrace's cost, and every other call would still be seen. A program that jumps to that
  instruction itself could hide a call. That forges only its own record, as it already can
  on macOS. It needs the interposer to be loaded, which a static binary still isn't, so the
  seccomp path stays for those.
- **Falls short** of eBPF, which is cheapest of all (no context switch). It needs privilege,
  which a build tool can't assume.

Per call, the supervisor does this:
- It finds the call in a const table indexed by syscall number, in one load.
- It reads each path with one `process_vm_readv` (two remote iovecs: to the end of the page,
  then on) into a buffer it keeps between calls. It finds the nul with core's word-at-a-time
  search.
- It reads a relative path's directory with one `readlink` of a `/proc` name formatted on the
  stack, and tidies the path into a reused `String`.
- It interns the path once to a `u32`, and checks the line with one `FxHashSet` lookup.

A path seen before in that scope allocates nothing and writes nothing. A new line is built in a
reused buffer and appended with one `write` before the call goes on.

## The record (`Recorder`, every platform)

Names (paths, variables, scopes) are interned once in one `String`. A name's ID is a `u32`,
its hash is FxHash, and names of the same hash are chained by ID, so no key is stored twice. A
line is a 12-byte `(what, scope, seen)` of IDs, with a const size assertion. One `FxHashSet`
lookup decides it, and lines that are dropped are remembered too, so a repeat costs that lookup
alone. A thread's scope is an ID, so nothing is cloned per call. The record written at the end
is a sort of borrowed text, in one preallocated `String`. This matches what tup and BuildXL keep
per path (a hash set of paths). It beats them on memory per line, since a repeated path is not
stored again.

`not_inputs` (the program itself) is dropped only from the run's own process's lines, the
unscoped ones. A test that opens or runs its own binary has it as an input. The sandbox's own
files, such as the Linux interposer, are in no line (`Recorder::ours`).

## macOS

| Technique | Who uses it | Per traced call | Sees | Needs |
| --- | --- | --- | --- | --- |
| `DYLD_INSERT_LIBRARIES` interposing | blaze, fsatrace | a function call | calls through libSystem, in processes that keep `DYLD_*` | nothing |
| Endpoint Security | BuildXL (beside its older kernel extension) | an event per call, from the kernel | every process, SIP-protected ones too | an Apple entitlement, root, a system extension |
| `fs_usage` / DTrace | system tools | in-kernel | everything | root, SIP relaxed for DTrace |

What blaze does: dyld's `__DATA,__interpose` in every process of the run, with Seatbelt
enforcing whatever the library does. The library notes each line once per process. It looks
it up in a table of 1M slots, mapped once, with open addressing and no lock: a hit is one
load, and a new line takes its slot by compare-and-swap. The hash mixes 16 bytes per 128-bit
multiply, as wyhash does. The working directory is cached per thread and invalidated by a
global count of `chdir`s, so reading it takes no lock and writes nothing shared. Paths are
joined with `memcpy` into stack buffers. Each new line is one `write` to the live record
(`O_APPEND`), built in a stack buffer and safe in a signal handler. Nothing on the hot path
allocates.

- **Matches** fsatrace's model and cost, and adds the per-scope record and private overlay
  writes.
- **Falls short** of Endpoint Security on completeness. A program macOS protects (SIP) drops
  `DYLD_*`, so what it runs is not seen, and the record says so with an `X` line. *What it
  would take*: an Endpoint Security client, which needs Apple's entitlement, root and a system
  extension. That is a signed blaze agent, not a build tool's process.

## Windows

| Technique | Who uses it | Per traced call | Needs |
| --- | --- | --- | --- |
| Detours (DLL injection, API hooking) | BuildXL, fsatrace | a function call | nothing |
| ETW file I/O events | system tools | in-kernel, batched | administrator |
| AppContainer and a job object | blaze, Chromium, other AppContainer launchers | none: they only confine | nothing |

What blaze does: it confines with an AppContainer in a job object and does not trace. The
record is one `X` line, so a run's result is keyed as cargo keys it. The cost per run (DACL
reads, the lock between launchers, the profile, process creation) is in
[speed-link-edge-usercode.md](speed-link-edge-usercode.md) and in section 6 of
`v2/requests/usercode-sandbox.md`. 1, 2 and 4 were accepted. 3 (a write SID derived from the
directory) was declined, because it weakens the sandbox.

- **Falls short** of BuildXL and fsatrace, which trace with Detours. *What it would take*: a DLL
  injected into every process of the job, which hooks `NtCreateFile`, `NtQueryAttributesFile`,
  `NtQueryDirectoryFile`, `GetEnvironmentVariableW` and process creation. It would record into
  the same kind of lock-free table and one write per new line as on macOS. Injection would ride
  on the suspended start the launcher already does.

## Enforcement

On Linux, namespaces and overlayfs give the run its own view, and Landlock confines a run
where namespaces can't be made. Bazel's Linux sandbox does the same with namespaces and bind
mounts, and declares its inputs instead of tracing them. Making a sandbox costs a fork, the
mounts and the filter, once per run. Neither Bazel nor BuildXL is cheaper here.

## A test's threads and the fork server's socket

A test's own threads read for that test. On macOS they take the creating thread's scope
(through an interposed `pthread_create`). On Linux they take the one scope their process's
other threads share, or none when those differ or aren't known yet. A test may connect to
the Unix socket its read set names (the fork server's), and to no other socket outside its
own and writable directories.
