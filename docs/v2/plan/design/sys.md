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
restorer on x86-64: `linux::signal::set_action` now has blaze-sys's own (`restore_rt`, one per
architecture), and `rt_sigaction` and the sampler go through it. `accept4` carries no peer
address; `linux::sock`'s whole-header `sendmsg`/`recvmsg` carry control messages.

**No C library (2026-11-20).** On Linux blaze-sys references no C-library symbol: `posix::`
there is the kernel's forms (`linux::{signal, host, fdctl, sock, task}`), errors come back in
the return value (no `errno`), and what has no kernel form (`dlsym`, `pthread_*`, `getpwuid_r`,
`localtime_r`) is macOS-only. The check is `scripts/src/bin/sys_imports.rs` over the rlib's
undefined symbols; on macOS the imported libSystem set is the ratchet
`tools/ratchets/blaze-sys-libsystem.txt`.

**The process's facts (D185).** Page size, clock ticks, `AT_HWCAP` and the vDSO's base are one
value, `facts::Aux`, handed in, with one source per kind of process: `from_entry` (blaze's own
`_start`), `from_parent` (a child blaze starts; no addresses, which `execve` moves),
`for_top_level` (the transition's std-entered process: `PR_GET_AUXV` where the probed
`KernelFacts` says the kernel has it, else `/proc/self/auxv`, chosen by the fact), and macOS's
`from_system`. Kernel features are `linux::probe::KernelFacts`, probed once, and each operation
with several forms is a slot chosen from it.

**The clock.** `linux::vdso::Vdso::find(&Aux)` parses the vDSO (`AT_SYSINFO_EHDR`;
`__vdso_clock_gettime`, on aarch64 `__kernel_clock_gettime`), and `clock_gettime_vdso` calls it;
where the kernel has no vDSO the system call is the stated path. The plain `clock_gettime` stays
the system call until the drivers hold a `Vdso`. About 100 ns a read against about 20 ns: to
measure, not measured (10 million reads of `Monotonic`, the two forms).

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

Imports are declared directly, with no `windows-sys` (D180). `blaze_sys::windows` holds every
Win32 item the tree uses, under its Win32 name, by area (`foundation`, `fs`, `io`, `memory`,
`net`, `security`, `threading`), each linked `kind = "raw-dylib"` so a build needs no import
library and no SDK. A handle is a `Handle`, a `BOOL` a `Bool`, a socket a `Socket`; failures
are a `WinError` (the last error, or an `NTSTATUS` mapped by `RtlNtStatusToDosError`).
`windows.rs` keeps sys.md's typed operations as refusals (`UNBUILT`) beside them.

What a Windows build still lacks, by reading the tree on 2026-11-20 (none of it run):

- bstd's driver. blaze-core's IOCP driver (`iocp.rs` and `iocp/`, the USN journal reader, the
  waits in `wait/windows.rs`) is written against these declarations; bstd's sans-IO effects
  have no Windows driver turning them into the calls, and the typed operations above are
  refusals.
- The launcher. blaze-launch-rt's fault report and thread-local paths have Linux and macOS
  bodies only (`run_mode/fault_report.rs` returns no stack bounds elsewhere); nothing maps or
  enters a program's code records on Windows.
- The sandbox. Build scripts and proc macros run sandboxed by Seatbelt (macOS) and Landlock with
  seccomp (Linux); Windows has none (a restricted token, an AppContainer or a job), so rule 9
  stops a Windows build at its first build script.
- The journal's watcher. The change journal's `Cursor::Usn` exists and the IOCP side reads USN
  records, but no `blaze-watch` path follows a Windows volume.
- The build-script runner. blaze-script (`child.rs`, `lib.rs`) uses Unix-only std
  (`std::os::unix`, `Command::process_group`, `OsStr::as_bytes`) with no cfg, so it does not
  compile for Windows (4 errors, `cargo check --target x86_64-pc-windows-msvc`, 2026-11-20).
- Third-party build scripts. blake3, psm, stacker and zstd-sys want a C compiler for msvc, so a
  Windows check from a Mac stops at them and cannot reach any crate above them. Those packages
  are on D180's list; until they are gone the Windows check covers blaze-sys, blaze-mem,
  blaze-core, blaze-check, blaze-profile, blaze-graph and blaze-inproc-rt only.
- The toolchain shelf. The pinned nightly's answers are recorded for `x86_64-pc-windows-msvc`
  and `aarch64-pc-windows-msvc` (blaze-toolchain `side.rs`), but no shelf entry, linker or CRT
  is fetched for a Windows host.

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

## Wave 4: the users' std on blaze-sys (design, not started; 2026-11-20)

The owner, 2026-11-20: "port EVERYTHING to blaze sys", which includes the toolchain's own library
(`library/std`, 1,484 `libc::` sites, with `library/backtrace` 58, `library/std_detect` 50, the
vendored `unwinding` 19, `panic_abort` 3). It is the library every USER program is compiled
with, so it is a platform backend, not a search and replace, and rule 4 binds it: every program
the pinned nightly accepts behaves the same. Nothing in `library/` is edited until this design is
accepted.

**The shape: a `libc`-named crate in the tree, not a new `pal`.** std reaches the system through
the `libc` crate's paths (`libc::open`, `libc::stat64`, `libc::EINTR`), by Rust path, not by
symbol. So the smallest change that moves all 1,484 sites is in one manifest line:
`library/std/Cargo.toml`'s `libc = { version = "0.2.191", ... }` becomes
`libc = { path = "../blaze_libc", package = "blaze-libc" }`, a crate of the same names and
signatures (only the ones std and its four parts use: the list is the compiler's, it fails to
build on a missing one) whose bodies are blaze-sys. std's source is unchanged, so the next nightly's
library merges as it does today, and the parity argument is per function, against the libc crate
it replaces, not over std's whole system layer. A new `sys/pal/blaze` would be the better shape
only if std's Unix backend had to change its logic; it does not, and a fork of `pal/unix` (some
30,000 lines across `sys/`) would diverge from upstream with every nightly. The shim crate is
`no_std` on blaze-sys alone, every item `/// replaces libc::X`, its types checked against the libc
crate once (the same comparison tests blaze-sys has).

**What the shim must be on each system.**
- macOS: libSystem stays the floor (above); the shim's functions are blaze-sys's typed
  declarations of the same libSystem symbols, so behaviour is libSystem's by construction. The
  gain is one declared, ratcheted set of imports; the risk is none beyond a wrong signature, which
  the comparison test catches.
- Linux: there is no libSystem to forward to, so a shim of raw calls is a C runtime, and it must
  own what glibc did beyond system calls: `errno` (std's `io::Error::last_os_error` reads it, so
  the shim keeps one per thread and sets it from the raw call's `-errno`); the program's start
  (`_start`, the auxiliary vector, `environ`, the static TLS block and the thread pointer, since
  no `ld.so` sets them); threads (`pthread_create` is `clone3` plus a stack and a TLS block,
  `pthread_join`, keys, `pthread_getattr_np` for std's stack guard); the allocator (std's
  `System` calls `malloc`, `posix_memalign`, `free`); `dl_iterate_phdr` for `backtrace` (the
  image's program headers are `AT_PHDR`/`AT_PHNUM`, and a static program has one image);
  `getaddrinfo` (a resolver of `/etc/hosts` and `/etc/resolv.conf`); `getpwuid_r` for
  `home_dir`'s last resort; `__cxa_thread_atexit_impl` for TLS destructors. This is the Eyra
  design (c-ward and origin: std on rustix with a libc ABI in Rust); it is real work, about the
  size of origin plus c-scape's used subset, and it is where rule 4 is at risk.

**What breaks if user programs on Linux have no C library, and the decision it needs.** A program
built this way is static, starts at its own entry and has no dynamic loader. Then:
- C code linked into it (any `-sys` crate built by `cc`, C imports under D174: openssl, zstd,
  sqlite, rocksdb) calls glibc's names: `malloc`/`free` across the boundary, `stdio`, `pthread_*`,
  `locale`, `getaddrinfo`. Those would bind to nothing, or to a second runtime: Rust threads made
  by the shim's `clone3` would call C code expecting glibc's thread descriptor at the thread
  pointer, and crash. A Rust-only shim and C code in one process cannot be mixed.
- `dlopen` of plugins, `LD_PRELOAD`, sanitizers' interposition and tools that expect `ld.so`
  (valgrind's default mode, `ltrace`) stop working.
- Name service: glibc's NSS (mDNS, LDAP, systemd-resolved, `sss`) is not consulted, so host and
  user lookups can answer differently: a behaviour change for `ToSocketAddrs` and `home_dir`.
- Locale and `iconv` are not offered (std uses neither).
So the backend is a FACT of the program, chosen before its build and stated in its profile line:
a program whose link closure holds no C object and no `dlopen` gets the no-C-library backend
(static, own entry); one with C code links glibc exactly as today. That is selection, not a
fallback (rule 3). Whether the no-C-library backend is the default for Rust-only programs, an
opt-in, or blaze's own binaries only, is the owner's decision; my proposal is blaze's own binaries
first (they leave std anyway), then Rust-only user programs by default once the oracle below is
green.

**The compile-time seam (D175, comptime-native.md) moves with it.** Today a compile-time child's
unchanged std imports C names and the launcher binds 265 of them (blaze-comptime-sys `NAMES`) to
blaze's answers at placement; a missing one is a `BindRefused`. With std on the shim, std imports
no C names: its system calls are the shim's functions, and the shim's backend is a slot (D185)
filled once at the child's entry: `Real` (blaze-sys) for a program run, `Guest` (one protocol
`Call` per function, today's `child.rs` bodies) for a compile-time child. The table becomes the
shim's own function set, typed and complete by construction (a function std needs and the shim
lacks fails std's build, not a placement), and binding by symbol remains only for C objects in a
compile-time closure, which comptime-native already treats apart. One std build serves both (no
second set of std records): the slot costs an indirect call per system call. The two designs
change in one piece: the shim's `Guest` backend is `child.rs` moved, and `NAMES` shrinks to what C
objects import.

**The oracle.** std's own test suite (`library/std` tests, `alloctests`, `coretests` where they
touch `sys`) and the three acceptance projects' tests (reth, tempo, blaze), each run twice: on the
libc crate and on the shim, on Linux x86-64 and aarch64 and on macOS, with every difference a
finding. The shim's types are compared with the libc crate's once, as blaze-sys's are. Only
then does the manifest line change by default.

**Order.** (1) the shim crate on macOS (forwarding: proves the names and the oracle); (2) Linux
for blaze's own helper binaries (static, own entry: the runtime pieces of "Runtime" above);
(3) the program-is-Rust-only fact and user programs; (4) the comptime seam's `Guest` backend.

## Proof

Every Linux wrapper has a real-kernel test with `libc`'s same call as the oracle where one
exists (`crates/blaze-sys/src/linux/tests.rs`), including a child by `clone3` reaped by `waitid`
and `io_uring_setup` under a seccomp filter giving the typed `ENOSYS`, with its control. The
shared script every driver must end as the simulation does (ledger V4) gains a cut, a ranged
read, a sync and an append, and two planted bugs are caught:
`the_oracle_catches_a_short_write_not_continued`,
`the_oracle_catches_an_errno_mapped_to_the_wrong_error`.
