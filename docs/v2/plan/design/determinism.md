---
status: plan
summary: Build-time code (build scripts, proc macros, sandboxed tests) is deterministic by construction. The end state is build code with no ambient authority, taking a build handle; until then an existing build.rs runs natively in the OS sandbox with randomness and the wall clock from a provider blaze controls (in blaze's std, at the source), a sandbox backstop that answers what still reaches the kernel, and Linux programs linked with no libc. Three rungs chosen before the build, four lints, `blaze explain build-scripts`, a twice-run check; the wasm rung is specified, not built.
date: 2026-11-10
---
# Deterministic build-time code

Why now (2026-11-10, Linux ARM64, glibc 2.44): a build script proved pure failed blaze's
cross-check because the sandbox saw `getrandom` in its process. It came from four places: std's
`RandomState`; glibc before `main` (`ld.so`, `ptmalloc_init`); blaze's allocator in the
launcher's forked child; and, invisibly, the vDSO (`getrandom` since glibc 2.41 on Linux 6.11,
the clock always). Recording such a call and refusing the run is correct and useless. This doc
makes them not happen, and answers the ones that still do.

It supersedes: the fixed `HASHMAP_KEYS` of `crates/blaze-macro-eval/src/os/random.rs` (one
provider now, section A); "randomness and the clock are only recorded" in
[cargo.md](../../../cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox) for a run given a
provider; and the open question in 240b8c4937 about the allocator's seed (section D).
[std.md](std.md) and [sys.md](sys.md) are what section B builds on.

Status of each part is in "What is built" at the end. Nothing here has been compiled or run.

## The end state: build-time code has no ambient authority

Determinism by construction, not by interception. Build code is ordinary Rust that receives
everything through a handle (bstd's model, [std.md](std.md): no ambient file system,
environment, clock, randomness or process):

```rust
fn build(cx: &Build) -> Outputs
```

Its result is a function of what it was handed and what it asked the handle for, exactly as a
query's is. The handle records each read, so the key is derived; nothing is traced and no
purity proof is needed. It runs in the const evaluator or as wasm: there is no native process
for the script's own logic.

What build.rs does today, and what replaces each class:

| # | Class | Replacement | In the tree today |
|---|---|---|---|
| 1 | cfg, feature and target probing; `cargo:rustc-cfg` | compile-time reflection and const evaluation over the target description | [comptime.md](comptime.md) phase 2 (`core::reflect`) |
| 2 | generating Rust source (tables, parsers, version strings) | comptime or `const fn`; `include!` of a handle's output | const eval; item generation is comptime phase 3, unbuilt |
| 3 | compiling C, C++, asm (`cc`) | native sources declared (`[native.<name>]`, or `cx.c_library(..)`): blaze runs the compiler as a tool step | pinned native inputs ([cargo.md](../../../cargo.md)), [toolchain-c.md](toolchain-c.md) |
| 4 | finding system libraries (`pkg-config`, vcpkg, env) | a declared dependency, resolved by a tool step whose reads are key inputs | nothing |
| 5 | bindgen | a C-import step: libclang run as a tool process, keyed by header hashes and flags; later a comptime `c_import` | nothing |
| 6 | probing the compiler (`autocfg`, `rustc --version`, try-compile) | blaze answers (it is the compiler): `cx.compiler().supports(..)`; nothing is spawned | blaze's `rustc` stand-in a script runs |
| 7 | env vars, files, timestamps, `git describe` | handle reads, recorded as inputs; the wall clock only as a declared input (`SOURCE_DATE_EPOCH`), never ambient | the trace (rung 2) |

A tool step is a sandboxed, traced process keyed by (tool identity, flags, input file hashes)
and cached like any unit.

What "deterministic" means here. The script's logic is exactly deterministic. A tool step is as
deterministic as its tool, so blaze (a) pins tools by content hash, (b) runs them sandboxed with
a fixed environment, the provider's clock and randomness (sections A to C), sorted listings and
no network, and (c) runs a step twice on request and compares (the twice-run check below), so
a nondeterministic tool is found and named, never hidden by the cache.

### The rungs (fixed, Andrew 2026-11-10)

Chosen before the build from the script's source and dependency graph, never by a failure
(rule no-fallback). One home: `blaze_build_script::rung`.

1. **Handle**: new-format build code taking the build handle. Not built; such code is a typed
   refusal (`NoRung`) until it is.
2. **Native**: an existing build.rs, a native process in the OS sandbox on the real host
   (seccomp and namespaces on Linux, Seatbelt on macOS) with the std provider and the backstop.
   It runs everything today, bindgen included.
3. **Wasm**: an existing build.rs that qualifies, as host-presenting wasm (section F). Spec
   only; a script that would qualify stays on rung 2 and `Why::WasmNotBuilt` says so.

A script with a dependency that links or loads native code (`*-sys` with a build script, a
`links` key, `clang-sys`, `libloading`) is on rung 2, as is one whose manifest says
`package.metadata.blaze.build-script = "native"`.

### Lints (group `blaze::edition`, the workspace's own crates only)

| Lint | Fires when | Names |
|---|---|---|
| `build_script_ambient` (exists) | a script uses ambient authority | the uses |
| `build_script_untraceable` | a run got what blaze could neither control nor answer | what (clock, random, network, descriptor); the result is not shared across machines |
| `build_script_native` | the script links or loads native code, so it stays on rung 2 | the dependency and the tool step that replaces it |
| `build_script_replaceable` | the whole script is one known pattern (only `cc`, only cfg probing, only `pkg-config`) | the declaration or build function that replaces it; a machine-applicable fix where mechanical, once the replacement exists |

Levels go through `blaze_lint_defs::ready`: warn in blaze's own workspace now; elsewhere warn
only once the named replacement exists in that blaze, allow until then.

### `blaze explain build-scripts` and the twice-run check

`blaze explain build-scripts` lists every build script in the dependency graph, dependencies'
included: its rung, why, what its last run got that can't be traced, and whether its last
output was reproducible. The twice-run check is opt-in (`BLAZE_SCRIPT_CACHE=twice`, the third
value of the variable that already has `check`): each script runs twice with equal
inputs in fresh sandboxes and the two results are compared byte for byte (stdout, stderr,
every file of `OUT_DIR`); a difference is an error naming the script and the first differing
file.

### Migration of the most-used build-script crates

| Crate | Class | Replacement | Shim? |
|---|---|---|---|
| `cc`, `cmake`, `cxx-build` | 3 | `[native.<name>]` / `cx.c_library(..)` (cmake: a tool step running cmake) | yes for `cc` |
| `pkg-config`, `vcpkg` | 4 | `[native.<name>] pkg-config = ".."` | yes |
| `bindgen` | 5 | bindgen as a tool step, then `c_import` | no: its API is libclang's shape |
| `autocfg`, `version_check`, `rustversion` | 6 | `cx.compiler().supports(..)`; a `cfg` computed at compile time | yes |
| `prost-build`, `tonic-build` | 2 (+ `protoc`, a tool) | a tool step for `protoc`, generation in comptime | no |
| `built`, `vergen` | 7 | handle reads (`cx.git()`, `SOURCE_DATE_EPOCH`) | yes |

The shim route: blaze provides a crate with the original's name and API, implemented over the
build handle, substituted only when the script's whole dependency graph is shims (so the script
moves to rung 1 unchanged). It is in keeping with useful-not-parity (real code depends on these
APIs) and with one-concept-one-home as long as a shim holds no logic of its own: each call is
one handle call. Decision: shims for `cc`, `pkg-config`, `autocfg`/`version_check` and
`vergen`/`built`, in that order, after the handle exists; none for bindgen or the protobuf
generators.

## A. The provider

One process-wide source of randomness and wall-clock time, chosen as the process starts.

- **The system's** (the default): std is the nightly's. A program that knows nothing of blaze
  is byte for byte as before (rule superset-of-rust).
- **Given**: whoever started the process put `__BLAZE_PROVIDER=1:<seed, 64 hex digits>:<wall>`
  in its environment; `<wall>` is `os` or seconds since the Unix epoch.

Why an environment variable: it costs no system call (an inherited descriptor costs a read the
sandbox would see; a mapped page costs an `mmap` and a fixed address), it survives `exec`, and
a child inherits it with no help. std reads it from `environ` itself, never `getenv`, so the
interposer does not take it for a variable the program read; it reads it in `rt::init`, before
`main` can change the environment, and lazily at first use in a program whose `main` is not
Rust's. An auxiliary vector entry is not ours to add.

Everything derived is SipHash-1-3 keyed by the seed's first half over (its second half, a
domain, two numbers): `library/std/src/sys/provider/derive.rs`, `core`-only, which blaze
includes by path (`blaze_usercode::provider::derive`), so std and blaze cannot disagree.

| What a program asks | Under a given provider |
|---|---|
| `RandomState::new` (`HashMap`, `HashSet`) | keys = `H(seed, HASH, thread id, 0 or 1)`, then std's own `+1` per map |
| std's `fill_bytes` (`std::random`) | the thread's stream: `H(seed, FILL, thread id, block)` |
| `SystemTime::now` | `<wall>` when it is a time, whole seconds, the same however often asked; else the system's |
| `Instant::now` | the system's monotonic clock, always |
| `Command::spawn` / `exec` | the child's variable names `child(seed, n)` for the parent's `n`th spawn |
| the `getrandom` crate, `rand`, C code, `libc::getrandom`, `/dev/urandom` | the kernel, so section C |

The thread id is std's `ThreadId`, which the spawning thread draws: threads one thread starts
in order have the same ids on every run. The child's seed is installed by rewriting the
process's own `environ` entry in place under a lock held across the spawn, so a child given
`environ` whole (the interposer's lent environment) still inherits its own seed. A program that
removed the variable gives its children none; in the sandbox section C answers them.

`RandomState` promises only resistance to collision attacks and an unspecified order. A seed
no one outside the build knows keeps the first for a build script's inputs; the second was
never a promise. So a given seed is inside std's contract.

**Keys.** The seed is `blake3("blaze v2 provider seed 1", kind, run key)`
(`Provider::of`): it is derived from the key, so it adds no input, and two scripts never share
one.

**The clock, per kind of user code.**

- Build scripts and proc macros: `SystemTime::now` is the build's `SOURCE_DATE_EPOCH` when the
  build sets it. That is what the variable means (the time a build is to say it was made), the
  user set it, and the run is recorded as reading that variable (std marks the read with one
  `access` of the sandbox's variable mark; the supervisor notes a read of the variable when it
  answers the kernel call), so the key covers it. When the build sets none, the script reads
  the system's clock, as today: recorded as untraceable and keyed as cargo keys it. A fixed
  made-up time would break scripts that embed a build date; the build's start time in every
  key would rerun every script on every build.
- `Instant`: always real. It yields durations, never a time; a cached result is one
  legitimate execution's, as with `RandomState`. A script proved pure may not read it (its
  output could differ between two cold runs).
- Sandboxed tests: the seed is given (hash maps, `getrandom`), the wall clock is always the
  system's: tests sleep, time out and check deadlines against it.

A proof of purity (`hir_ty_pure`) stops at `RandomState::new` (`Effects::SEEDED`) and
`SystemTime::now` (`Effects::WALL`). A proof with `WALL` adds `SOURCE_DATE_EPOCH` to the
variables of the pure key; where a run has none set, the script is not kept as pure.

The same policy type serves wasm: the proc-macro host's `random_get` and `clock_time_get`
are to answer from `Provider` (`Answers::random` with the module as its one process). Not yet
wired: the evaluator still has its fixed keys and real bytes.

bstd: the build-script root capability carries the same `Provider`, so `Random` and `Clock`
capabilities handed to rung-1 code draw from the same streams. One concept.

## B. std without libc on Linux

A Rust program blaze builds for Linux with no C in it links with no libc: std talks to the
kernel through `blaze-sys`, and there is no C start-up code making hidden calls.

**How it is chosen.** A link mode, `libc = none | system`, decided by blaze from the crate
graph: `none` when no crate links a native library (`links`, `#[link]`, a build script's
`cargo:rustc-link-lib`) and none declares `extern "C"` items std's own Linux path does not
cover; `system` otherwise, with no change in behaviour. It is an extension of the build, not
of the language: std's observable behaviour is the same, minus glibc's own start-up side
effects (`ld.so`'s reads, its `getrandom`, locale and `nsswitch` files), which no Rust program
can observe except through timing and `strace`. Inside the fork it is a `target_env`-like
choice (`cfg(blaze_libc = "none")`), set by blaze, never by the user's `cfg`.

**The executable.** Static PIE; its own `_start` (reads `argc`, `argv`, `envp`, `auxv` from
the stack); relocations applied by blaze-link at link time where the image is not relocated,
and by a self-relocator in `_start` (`R_*_RELATIVE` only) for PIE; TLS by the local-exec
model, the main thread's block set up by `_start` from `PT_TLS`.

| Area | What std takes from libc today | In blaze-sys | Decision |
|---|---|---|---|
| start-up, args, env, auxv | `crt1`, `__libc_start_main`, `.init_array` args, `environ`, `getauxval` | nothing | to write: `_start`, a stack parser; `environ` is std's own static |
| exit | `exit`, `atexit` | `exit_group` | exists; std has no `atexit` users of its own |
| allocator | `malloc`, `posix_memalign`, `realloc`, `free` | `mmap`, `munmap`, `madvise` | `System` maps to blaze-mem's allocator; a program's `#[global_allocator]` still wins |
| TLS, thread creation | `pthread_create`, `pthread_join`, `__tls_get_addr`, `pthread_key` destructors | `clone3_process` only | to write: `clone3` with `CLONE_VM|THREAD|SETTLS`, own stack and guard page, TLS block per thread; needs design (stack sizes, destructors' order) |
| locks, condvars, park | already raw `futex` | `futex_wait_bitset`, `futex_wake` | exists |
| fs | `open`, `read`, `stat`, `readdir`, ... | most (`openat`, `statx`, `getdents64`, `renameat2`, ...) | to add: `fchmod`, `fchown`, `linkat`, `symlinkat`, `readlinkat`, `utimensat`, `lseek`, `fcntl`, `copy_file_range`, `sendfile` |
| net | `socket`, `connect`, `send`, `recv`, `setsockopt`, ... | `socket` to `shutdown` | to add: `setsockopt`, `getsockopt`, `getsockname`, `getpeername`, `sendto`, `recvfrom` |
| process | `posix_spawn`, `fork`, `execvp`, `waitpid`, `pipe2`, `dup` | `clone3_process`, `waitid`, `pipe2`, `kill` | to add: `execve`, `dup3`, `chdir`, `setsid`, `setuid`/`setgid`; `Command` forks by `clone3` |
| time | `clock_gettime` (vDSO), `nanosleep` | `clock_gettime`, `clock_nanosleep_until` | exists; by system call, no vDSO (slice 1 measures the cost) |
| random | `getrandom`, `/dev/urandom` | `getrandom` | exists |
| stdio | `read`, `write`, `writev` | `read`, `write` | to add: `readv`, `writev` |
| signals | `sigaction`, `sigaltstack` (stack-overflow handler), `SIGPIPE` | `rt_sigaction`, `rt_sigprocmask` | to add: `sigaltstack`, the `rt_sigreturn` trampoline |
| unwinding | `_Unwind_*` from `libgcc_s` | nothing | `library/unwind` built in (LLVM's libunwind in Rust form is not in the tree): needs design; until then `libc = none` implies `panic = abort` only where the program says so, else `system` |
| backtraces | `dl_iterate_phdr` | nothing | own program headers from `auxv` (`AT_PHDR`): one static image |
| `errno` | `__errno_location` | the raw return | per call, from the result; no thread-local |
| DNS (`ToSocketAddrs`) | `getaddrinfo` | nothing | own resolver over blaze-sys sockets reading `/etc/hosts` and `/etc/resolv.conf`; until built, a program that names `ToSocketAddrs` with a host name links `system` |
| formatting, locale | none | | nothing to do |

**Slices**, each landing working: (1) random, time and the provider, libc still present
(section A: built); (2) start-up, args, env, auxv, allocator, exit; (3) threads and TLS;
(4) fs, net, process over blaze-sys; (5) unwinding and backtraces; (6) DNS. Build scripts and
proc-macro hosts are the first users of `libc = none`: with it a script's process makes no
call before `main` at all.

Slice 2 is not written in this lane: `_start`, the self-relocator and TLS set-up depend on
what blaze-link emits for a static PIE (the dynamic section, `PT_TLS`, which relocations
remain), which cannot be settled without linking and running one. The blaze-sys additions it
needs are the "to add" cells above.

## C. The sandbox backstop

What still reaches the kernel in a sandboxed process (C code, `libc::getrandom`, glibc's
start-up, a child that is not blaze-built) is **answered** by the supervisor from the same
provider, and noted as answered, not as untraceable.

| Call | Answer | Noted as |
|---|---|---|
| `getrandom` | the process's stream, `H(seed, SYSTEM, process, block)`, written into its memory (`process_vm_writev`, after `SECCOMP_IOCTL_NOTIF_ID_VALID`); at most 1 MiB a call | `T answered random` |
| read-only `open`/`openat` of `/dev/urandom`, `/dev/random` | a memfd of 64 KiB of the open's own stream, handed over read-only (`SECCOMP_ADDFD_FLAG_SEND`) | `T answered device` |
| `clock_gettime(CLOCK_REALTIME[_COARSE])`, `gettimeofday`, when the provider has a wall clock | that time | a read of `SOURCE_DATE_EPOCH` |

A process's number is its id in the sandbox's PID namespace (the kernel gives them out in
order, so the same fork order gives the same numbers); in a confined sandbox, which has no
namespace, its place among those that asked. The state is one count per process
(`blaze_usercode::provider::Answers`, a sans-IO core with a Hegel property against its model).

An answered line is no input and is dropped as the trace is assembled; `pure::check` therefore
passes it. A call that could not be answered is still a refusal (rule no-fallback): `openat2`
of a device, the 257th device open of a run, a device file read to its end (seen through the
shared open file's offset as the run ends), `CLOCK_TAI`, `time`, and any realtime clock read
when the provider has no wall clock.

What the device's stand-in is not: `fstat` says a regular file, and it ends. A program that
checks for a character device (libsodium, BoringSSL) fails loudly; none was met in build
scripts.

**The vDSO.**

- `getrandom` (both architectures): the vDSO function needs a state the C library maps with
  `MAP_DROPPABLE`, the only user of that mapping type. A traced run's filter refuses such an
  `mmap` with `EINVAL`, as a kernel before 6.11 does, and glibc then asks by system call for
  good. In-kernel, no notification, no cost. Built.
- The clock: there is no per-process switch on x86-64 or aarch64 (`vdso=0` is a boot option;
  a time namespace offsets only the monotonic clocks; `PR_SET_MM_AUXV` does not survive
  `exec`). For dynamically linked programs the interposer already wraps `clock_gettime`,
  `gettimeofday`, `time` and `timespec_get`: with a wall clock given it now makes the real
  system call, which the supervisor answers. For a static program or one that parses `auxv`
  itself (Go), the sound mechanism on both architectures is a ptrace stop at each `exec`
  (`PTRACE_O_TRACEEXEC`) in which the supervisor rewrites `AT_SYSINFO_EHDR` to `AT_IGNORE`,
  so the program finds no vDSO; two context switches per `exec`, nothing per system call.
  Not built. Until it is, such a process never says it loaded the interposer and the run is
  already marked incomplete (`X`), which a pure script's check refuses: nothing is accepted
  silently. A `libc = none` program blaze built has no vDSO code at all and is to be exempt
  from that mark.

**What stays unanswerable.** `rdtsc`/`rdtscp` and `CNTVCT_EL0` are readable from user space;
seccomp sees neither, and `PR_SET_TSC` (x86 only) would kill vDSO users. They are timing
sources, the same class as `Instant`: real, and not an input. `RDRAND`/`RDSEED` and arm64
`RNDR` are hardware entropy no process-level mechanism traps (only a hypervisor can hide the
CPU feature). The cross-check cannot see them; the proof is the guard: `hir_ty_pure` refuses
inline assembly and foreign code it does not know, so a script proved pure contains none. For
an unproved script they are the script's own nondeterminism, as under cargo, and the twice-run
check is what finds it.

## D. blaze's allocator in a forked child

The seed is mimalloc's: a thread's part of a heap, made on first use, keys itself from
`getrandom(32, GRND_NONBLOCK)`. It protects free-list encoding and the order pages are
extended in, against a heap corrupted by an attacker; in a child that only sets up a sandbox
and execs, it protects nothing that outlives the `exec`. Decision: mimalloc is not patched.
Before the filter is installed the child's calls are invisible to the supervisor and harmless.
From the install to the `exec` the child allocates nothing (240b8c4937), which a test now
holds it to (`the_launchers_window_makes_no_call_the_supervisor_sees`, with a planted seed as
its control); a call there would be answered from the provider, as the program's. The daemon
keeps the system's entropy.

## E. macOS

| | Today | Decision |
|---|---|---|
| A (provider) | built: std's Apple path (`CCRandomGenerateBytes`, the generic `hashmap_random_keys`, `SystemTime::now`) asks the provider; the variable is passed by the same launcher code | to verify on a Mac that the wall-clock mark reaches the recorder through the Seatbelt path |
| B (no libc) | not possible: libSystem is the system call ABI | never; blaze-sys on macOS is libSystem's symbols |
| C (backstop) | Seatbelt cannot answer a call; the commpage clock and `arc4random`/`getentropy` make no interceptable call | the interposer (`DYLD_INSERT_LIBRARIES`) is to answer `getentropy`, `arc4random_buf`, `CCRandomGenerateBytes`, `SecRandomCopyBytes`, `clock_gettime`, `gettimeofday` from the provider in process; a program that does not load it (hardened runtime, static) is marked incomplete as on Linux. Not built: today these are recorded as untraceable, a typed refusal for a pure script |

## F. Build scripts on wasm (rung 3, spec only)

The route that is the same on Linux and macOS. A Linux container is ruled out: on macOS the
script would be a Linux program and could not see or run the host's tools.

**The split.** The script's own computation runs as wasm in Wasmtime, compiled by blaze's
front end as proc macros are (rule proc-macros-on-wasm): randomness, clock, environment and
file system are host functions, so its reads are known exactly with no tracing. The tools it
runs (`cc`, `pkg-config`, `xcrun`, a `rustc` probe) run natively through one host import,
`spawn`, which runs the child in the existing native sandbox (sections A to C) and returns
its status, output and traced reads and writes as the script's inputs and outputs.

**Identity.** Built for `wasm32-wasip1` a script sees `cfg(unix)` false, `target_os = "wasi"`
and 32-bit pointers, which changes scripts that use `#[cfg(unix)]`, `std::os::unix` or
`cfg!(target_os = "macos")` to describe the host (rule superset-of-rust). So: a blaze-owned
wasm target that presents as the host: the host's `target_os`, `target_family`, `target_env`,
`target_vendor`, endianness, and `target_pointer_width = "64"` through memory64; std's sys
layer for it over host imports shaped like the host's OS interface (Linux: system-call shaped,
as WALI; macOS: the libSystem surface std's darwin pal uses: files, directories, symlinks,
permissions, env, args, pipes, spawn and wait, time, entropy), so `std::os::unix` works.
`target_arch` is the one value that cannot be the host's for code generation; `cfg` reports
the host's, and what follows from that is refused below.

**Typed refusals** (each names the item; all decided before the build): a build dependency
that links native code; inline assembly and `core::arch` intrinsics; FFI outside the import
surface (decision: a `libc` shim for the calls of the surface, a refusal naming the symbol
for the rest); threads (decision: refused at first; wasm threads later if the corpus needs
them); `mmap` of files; signals. The opt-out that keeps such a script on rung 2 is the
manifest's `build-script = "native"` or detection from the dependency graph, never a failure.

**Keys.** Every script becomes cacheable by (script code id, the env it read, the files it
read with hashes, each spawned tool's key and outputs). The purity proof becomes an
optimization (skip the run), not a requirement.

**Costs.** Compiling the script and its build dependencies to wasm (front end, Cranelift in
Wasmtime); start-up per script; memory64 in Wasmtime and Cranelift on each host (bounds
checks without guard pages are slower); compute-heavy scripts run slower than native; one
import surface per OS to write and keep. How much of crates.io runs unmodified, by class
(an estimate, to be replaced by the corpus run): env and cfg only, and `autocfg`-style
probes: nearly all; `cc` and `pkg-config` users: all, the tools go through `spawn`; bindgen:
none, libclang is native code loaded by the script (rung 2 until it is a tool step); scripts
with native build dependencies: none.

**Recommendation.** A to C first: they fix today's failures and serve all native user code,
tests included. Then rung 1 (the handle and tool steps), which removes scripts rather than
moving them. Rung 3 only if a corpus run shows enough scripts that neither rung 1's shims nor
rung 2 serve well.

## Order of work and ledger rows

1. The std provider and the Linux backstop (E19, E20). Built in this lane, not yet run.
2. `blaze explain build-scripts` and the twice-run check (E21).
3. The three lints (E22).
4. Rung 1: the build handle and tool steps: C compilation first (`[native.<name>]`,
   `cx.c_library(..)`), then pkg-config, then bindgen as a tool, then a comptime `c_import`
   once comptime phase 3 exists (E23). Spec only.
5. Rung 3, built only if a corpus run justifies it (E24). Spec only.

Beside them: `libc = none` (S7, slices 2 to 6), the exec-stop that hides the clock's vDSO
(in E20) and the macOS backstop (E25).

## What is built

Written in lane det-scripts-1110, none of it compiled or run:

- A: `library/std/src/sys/provider/` and its call sites (recorded in `library/UPSTREAM`);
  `blaze_usercode::provider`; the build script's command carries the variable; the purity
  leaves and the pure key's `SOURCE_DATE_EPOCH`. Not built: the wasm host and the evaluator on
  `Provider`; a provider for sandboxed tests and proc-macro processes (only `script::prepare`
  passes one).
- C: `getrandom`, the devices and the given wall clock answered by the Linux supervisor; the
  `MAP_DROPPABLE` refusal; the interposer's real clock calls. Not built: the exec-stop.
- D: the window's test and its control.
- B, E's backstop, F and rung 1: specified only.
