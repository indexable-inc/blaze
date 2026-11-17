---
status: plan
summary: The bottom of bstd (ledger row S4). One crate, blaze-sys, is the only home of system calls (Linux by raw call with no libc, macOS through libSystem, Windows through ntdll and kernel32); the runtime pieces Rust's std provided each get an owner; blaze-core moves onto it call by call behind a ban list that is the progress meter.
date: 2026-11-10
---
# blaze-sys and the runtime under bstd

[std.md](std.md) puts two layers under the drivers: `bstd::sys` (layer 1) and `bstd::rt`. This is
their design and the order they are built in. Slice 1 is built (written, not yet run): the crate
`crates/blaze-sys`, the first driver calls on it, and bstd as `#![no_std]`.

## One home for system calls

`blaze-sys` is `#![no_std]`, depends on nothing but `core`, and is the only crate that may make a
system call. blaze-core's drivers and waits are written on it; bstd never calls it (bstd speaks
effects). It is `bstd::sys` of std.md's layer table, a crate for now so that blaze-core can
depend on it without depending on bstd.

The API is typed per operation, not a `syscall(n, a, b, c)` passthrough, which would move the
unsafety and the errno reading to every caller. A function takes an `Fd`, a `Path` (a `&CStr`
view; `Path::in_buffer` makes one in the caller's buffer), flags as typed sets (one type per
call) and a `Timespec`, and returns `Result<T, Errno>`; `Errno` is an enum with each target's
numbers as data and `Unlisted(n)` for the rest. It never allocates or panics (`#[blaze::noalloc]`,
`#[blaze::nopanic]`) and never loops: an interruption and a short read or write go back to the
caller, whose retry is the one bounded loop (`blaze_core::wait::until`; rules deadline-waits,
bounded-progress). Each doc comment names the effect or wait that uses it.

## Linux: raw calls, no libc

Linux's system call ABI is stable, so libc adds only a dependency. The call is `asm!` in
`linux/raw.rs` (`syscall0` to `syscall6`, private to the layer). x86-64: `syscall`, the number in
`rax`, arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`, the result in `rax`, `rcx` and `r11`
clobbered. aarch64: `svc 0`, the number in `x8`, arguments in `x0` to `x5`, the result in `x0`,
nothing else clobbered. Both `nostack`.
A result in `-4095..=-1` is the errno negated. The numbers are data in `linux/nr.rs` (the
kernel's `syscall_64.tbl` and `asm-generic/unistd.h`), written from memory; none is marked
unverified, and every row is checked against the `libc` crate's `SYS_*` (a dev-dependency, never
in a build of user code) by `the_number_table_is_libcs`, as are the flags, struct sizes and errnos.

| call | x86-64 | aarch64 | | call | x86-64 | aarch64 | | call | x86-64 | aarch64 | | call | x86-64 | aarch64 | | call | x86-64 | aarch64 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| read | 0 | 63 | | renameat2 | 316 | 276 | | ppoll | 271 | 73 | | io_uring_enter | 426 | 426 | | eventfd2 | 290 | 19 |
| write | 1 | 64 | | unlinkat | 263 | 35 | | pidfd_open | 434 | 434 | | io_uring_register | 427 | 427 | | prctl | 157 | 167 |
| close | 3 | 57 | | mkdirat | 258 | 34 | | waitid | 247 | 95 | | socket | 41 | 198 | | seccomp | 317 | 277 |
| pread64 | 17 | 67 | | statx | 332 | 291 | | wait4 | 61 | 260 | | connect | 42 | 203 | | rt_sigaction | 13 | 134 |
| pwrite64 | 18 | 68 | | getdents64 | 217 | 61 | | clock_gettime | 228 | 113 | | bind | 49 | 200 | | rt_sigprocmask | 14 | 135 |
| openat | 257 | 56 | | ioctl | 16 | 29 | | clock_nanosleep | 230 | 115 | | listen | 50 | 201 | | getpid | 39 | 172 |
| openat2 | 437 | 437 | | mmap | 9 | 222 | | clone3 | 435 | 435 | | accept4 | 288 | 242 | | kill | 62 | 129 |
| fsync | 74 | 82 | | munmap | 11 | 215 | | exit | 60 | 93 | | sendmsg | 46 | 211 | |  | |  |
| fdatasync | 75 | 83 | | madvise | 28 | 233 | | exit_group | 231 | 94 | | recvmsg | 47 | 212 | |  | |  |
| sync_file_range | 277 | 84 | | mprotect | 10 | 226 | | getrandom | 318 | 278 | | shutdown | 48 | 210 | |  | |  |
| ftruncate | 77 | 46 | | futex | 202 | 98 | | io_uring_setup | 425 | 425 | | pipe2 | 293 | 59 | |  | |  |

Still to add as their callers move: inotify, fanotify, `linkat`, `symlinkat`, `readlinkat`,
`fchmod`, `utimensat`, `fstatfs`, `flock`, `execveat`, `setsockopt`.

**Decisions inside the layer.** `open_beneath` is `openat2` with `RESOLVE_BENEATH |
RESOLVE_NO_MAGICLINKS`, the semantics of `beneath.rs` on branch `core-effects-1109`: `EXDEV` for
what leaves the directory, `ENOSYS` before Linux 5.6, and nothing else opens beneath.
`clone3_process` makes only a child with a copy of the caller's memory and refuses `CLONE_VM`
before any call: a thread's child returns on a new stack, which needs an assembly entry and is
`rt`'s. `rt_sigaction` offers `SIG_DFL` and `SIG_IGN` only: a handler needs the `rt_sigreturn`
restorer on x86-64, also `rt`'s. `accept4` and `recvmsg` carry no peer address and no control
message yet.

**The clock.** `clock_gettime` is the system call in slice 1, not the vDSO: a known cost (rule
slow-is-p0-traced), about 100 ns a read against about 20 ns (to measure, not measured: 10
million reads of `Monotonic`, blaze-sys against libc). Nothing hot reads it yet: blaze-core's
clock stays `std::time::Instant` until slice 5, which parses the vDSO (`AT_SYSINFO_EHDR`;
`__vdso_clock_gettime`, on aarch64 `__kernel_clock_gettime`) before the clock moves.

## macOS: libSystem, private calls where they do more

macOS's syscall numbers are not an interface; libSystem's symbols are. `macos.rs` declares them
itself (`unsafe extern "C"`, no `libc` crate), the private call where rule macos-private-apis
applies, with which and why in `SYMBOLS`; `every_declared_symbol_exists` looks each up by `dlsym`.

| need | call | private | why |
|---|---|---|---|
| park, unpark | `__ulock_wait2`, `__ulock_wake` (`__ulock_wait` before macOS 11) | yes | a compare-and-wait on a word with a nanosecond timeout; the public `os_sync_wait_on_address` is 14.4 and later |
| submit and wait | `kevent_qos`; `kevent_id` for workloops | yes | a QoS per event, and a completion wakes the worker that owns it |
| a lock's slow path | `os_unfair_lock_lock_with_options` | yes | adaptive spin while the owner runs |
| the footprint limit | `memorystatus_control` | yes | the number the kernel enforces; no public call reads it |
| clone, copy | `clonefile`, `fclonefileat`; `fcopyfile` | no | a clone is never replaced by a copy |
| sync | `fcntl`: `F_BARRIERFSYNC` for `Durable::Started` and a journal barrier, `F_FULLFSYNC` for `Durable::Yes` | no | `fsync` alone leaves bytes in the drive's cache |
| open beneath | `openat` with `O_RESOLVE_BENEATH` (macOS 15) | no | refused where the kernel ignores the flag |
| clock, random | `mach_absolute_time`, `mach_continuous_time`; `getentropy` | no | a commpage read, no call |

Slice 1 builds the declarations and the symbol tests. Every typed operation is `Unbuilt { operation,
target }`, a refusal naming itself (`macos::UNBUILT` lists them).

## Windows: ntdll and kernel32

Imports are declared directly, with no `windows-sys`. Slice 1 specifies the table (in
`windows.rs`) and builds typed refusals only.

## Runtime: what a `no_std` world must own

`bstd::rt` (std.md) owns what Rust's std hid. Each piece, the decision, and when:

| piece | decision | slice 1 | later |
|---|---|---|---|
| entry, arguments | `_start` in `rt` reads `argc`, `argv`, `envp` and the auxiliary vector from the start block, builds the `Io` and calls `fn main(cx, io)`; arguments and environment exist only as `io.args()` and `io.env()` (S5) | nothing: `NativeWorld` is still filled by the caller | slice 5: `_start` for Linux; macOS enters through dyld's `main` |
| panic handler | prints the message and location to fd 2 with one raw `write`, then fails its task. Standard error is not a capability (std.md, "Decided"), so this is the single ambient use in bstd, marked `// ambient: a panic must be seen when the program holds no Out` | `Fd::STDERR` and `write` exist for it | slice 5: the handler |
| global allocator | none. `alloc` stays linked for its types (`Arc`, `Vec<T, A>`); the `#[global_allocator]` is a refuser that aborts through the panic path naming the size and, from `#[track_caller]` on bstd's own entry points, the site. Not linking `alloc` would also lose `Vec<T, A>` | bstd keeps today's global heap, each use with `// alloc:` | when effects stop owning their bytes (item 1 of the bstd improvements), the refuser goes in |
| threads | `clone3` with `CLONE_VM` and a stack from the page pool, entered by an assembly trampoline; a guard page below each stack by `mprotect`. bstd has no thread-locals, but `rt` keeps one word per thread (the worker pointer) in the thread pointer register, set by `CLONE_SETTLS`; no ELF TLS segment is needed while no linked crate has a `#[thread_local]` | `clone3_process` (fork only), `mmap`, `mprotect` | slice 5 |
| unwinding | kept: `panics-fail-one-job` needs a panic to end one task. On `library/unwind` and `library/panic_unwind` with blaze's own `rust_eh_personality` entry (the codegen and linker already emit and keep `.eh_frame`: `crates/hir-ty-codegen/unwind.rs`); under the `Frames` layout a panic takes the second return path instead. `panic = abort` is refused as the design: it would make a panic a process exit | bstd's `catch_unwind` is std's, in `rt.rs` | slice 5 |
| `memcpy` family | `library/compiler-builtins` with its `mem` feature | nothing (std links it) | slice 5 |
| stack probes | `__rust_probestack` from compiler-builtins on x86-64; inline probes on aarch64 | nothing | slice 5 |
| main thread's guard | the kernel's guard gap below the main stack is kept as it is; `rt` records the stack's bounds (the start block's address and `RLIMIT_STACK`) so an overflow's message can say so | nothing | slice 5 |

## Migration: no flag day

blaze-core's drivers move call by call from `libc` and `std::fs` to blaze-sys behind the function
boundaries they have, so the tree compiles at every save point (rule whole-ports). The ban:
`libc::`, `std::fs`, `std::net`, `std::thread`, `std::process` and `std::time::{Instant,
SystemTime}` are named only in blaze-sys. `harness/v2/nostd.json` lists every file (blaze-core,
bstd) or crate (the rest, S3) that still names one, with why, and `harness/v2/nostd.py` holds
the list to the tree: `progress` fails when an unlisted file names a banned path or a listed one
no longer does; `check`, S4's ledger check, passes only when the list is empty, every crate is
`#![no_std]`, none says `extern crate std`, and the lock file's closure is blaze's crates and
packages known to be `#![no_std]`. The list is the progress meter.

A `.clippy.toml` entry cannot say this (clippy bans items, not module trees, with no "except
these files"). The refusal by name resolution is std.md's `ambient` effect (S2) with blaze-sys
the one trusted crate; until then the script finds uses by text.

### The work list: blaze-core today

`libc::` and `std::` names by module, outside tests (801 and 575; `banned` is the std file,
network, thread, process and clock paths among them, 162):

| module | `libc::` | banned `std` | all `std::` | mostly |
|---|---|---|---|---|
| `uring` (+ files, flight, ops) | 210 | 10 | 36 | ring calls, open, statx, fallocate |
| `wait` (+ ring, linux, macos, unix) | 113 | 6 | 32 | io_uring_enter, poll, kevent, ulock, wait4 |
| `process` (+ spawn) | 105 | 14 | 22 | posix_spawn, fork, wait, signals |
| `raw`, `net` | 150 | 16 | 39 | shared libc helpers, sockets |
| `kqueue`, `signal` | 68 | 4 | 18 | kevent, fcntl, sigaction |
| `fs` (+ mapped, private) | 48 | 53 | 98 | `std::fs`, mmap, clone, statfs |
| `fanotify`, `watch` | 79 | 0 | 12 | fanotify, inotify |
| `threads` | 13 | 9 | 31 | `std::thread`, poll |
| `iocp` (+ 9 files), `create_process` | 0 | 18 | 158 | `windows-sys`; `std::ptr` and `std::mem` (core's) |
| `fsevents`, `cgroup`, `cores`, `footprint` | 15 | 27 | 59 | `std::fs`, sysctl |
| `lib`, `journal`, `lock`, `sim`, `wheel`, others | 0 | 5 | 70 | `std::path`, `std::io`, `std::sync` in the effect types |

The `std::` names that are not banned (`path`, `io`, `ffi`, `os::fd`, `sync`) go when the effect
types become the flat op records of std.md's boundary.

### Slices

| slice | what | off the list |
|---|---|---|
| 1 (built) | blaze-sys for Linux; the `threads` driver's Read, Append, Sync, Truncate and WriteAt (`crates/blaze-core/src/fs/linux.rs`); the direct wait driver's kernel as a patch against all-1110 (`crates/blaze-core/patches/wait-direct-on-blaze-sys.patch`); bstd `#![no_std]` with `rt.rs` its one std module | five effects' calls; `wait/direct.rs` when the patch applies; every bstd module but `rt.rs` |
| 2 | the rest of Linux: Write and Rename (and all-1110's `WriteUnsynced`, `RenameUnsynced`, `WriteBack` on `sync_file_range`), List, Stat, Clone; sockets; spawn and reap; the ring's calls; inotify and fanotify | `fs`, `net`, `process`, `raw`, `uring`, `watch`, `fanotify`, `wait/ring`, `wait/linux` |
| 3 | macOS: the typed layer on the declared symbols, then `kqueue`, `fsevents`, `wait/macos` | those |
| 4 | Windows: the imports, then `iocp` | `iocp/*` |
| 5 | `bstd::rt`: entry, panic handler, threads, unwinding, the vDSO clock; `threads`, `signal`, `cores`, `busy` move; bstd's `rt.rs` is deleted | the rest of blaze-core and bstd |
| S3 | every other crate, onto capabilities | the crate rows |

## Proof

Every Linux wrapper has a real-kernel test with `libc`'s same call as the oracle where one
exists (`crates/blaze-sys/src/linux/tests.rs`), including a child by `clone3` reaped by `waitid`
and `io_uring_setup` under a seccomp filter giving the typed `ENOSYS`, with its control. The
shared script every driver must end as the simulation does (ledger V4) gains a cut, a ranged
read, a sync and an append, and two planted bugs are caught:
`the_oracle_catches_a_short_write_not_continued`,
`the_oracle_catches_an_errno_mapped_to_the_wrong_error`.
