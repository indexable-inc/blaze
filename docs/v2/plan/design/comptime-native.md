---
status: plan
summary: How compile-time code (proc macros, build scripts) moves from Wasmtime to blaze's own execution path (D175). What the wasm path gives a guest today, file by file; the grants and the typed protocol (crates/blaze-comptime, on crates/blaze-roots); the std system layer at the launcher's symbol seam; how the guest is built and confined; the stages with their oracles and deletions; what is given up; the risks ranked.
date: 2026-11-20
---
# Compile-time code on blaze's own path

Decision D175 (end-state.md): WebAssembly is a bridge. A proc macro or build script will be
compiled by blaze's back end, run by blaze's launcher in its own confined process, against a
standard library whose system layer holds only the grants, with fuel and a deadline. This is the
plan for that, and step 1 of it: two crates, written and not yet compiled.

- `crates/blaze-roots`: what a run may see and what it saw. `Roots` (a digest-keyed tree, a
  package directory with globs, one output directory), `RelPath` (checked UTF-8, cannot escape
  its root), `Mounts` (a spelled path to a root and a `RelPath`), `ReadList` (every file with its
  digest, every failed lookup, every listing) and its one check, `ReadList::under(&Roots) ->
  Result<Checked, Refused>`. Shared with the C compiler's driver (D174) and `cx.hermetic` (D177).
- `crates/blaze-comptime`: `Grants` with its two constructors, the typed `Call` and `Answer`, the
  capped CBOR wire, the host's and the guest's sans-IO machines, and the equality harness's
  `first_difference`.

Paths below are under the tree root; `pm/` is `crates/blaze-proc-macro/src`, `bs/` is
`crates/blaze-build-script/src`, `wasi/` is `crates/blaze-wasi/src`.

## What exists

### 1. What the wasm path gives a guest

Twenty-four WASI calls are linked (`wasi/calls.rs` `link`, the list at `wasi/lib.rs:118-143`),
for macros at `pm/host.rs:346` and scripts at `pm/script.rs:323`; their answers come from
`pm/sandbox.rs:374-542` (a macro's `Sandbox`) and `pm/script_sandbox.rs:1265-1524` (a script's).

| Reach | Proc macro | Build script |
|---|---|---|
| arguments | none (`sandbox.rs:375`) | `build-script-build` (`:1267`) |
| environment list | empty (`:379-383`); variables one by one through the shim's `env_var` | the whole `Script::env` (`script_sandbox.rs:572-578`) |
| clocks | 0, every clock (`sandbox.rs:389-391`) | 0 (`:1278`) |
| randomness | zero bytes (`:393-396`) | zero bytes (`:1282`) |
| stdout, stderr | dropped (`:398-403`) | stdout kept within caps, stderr's last 64 KiB (`:836-853`, `:79`) |
| files | read-only under `/package` or `/workspace/<dir>` (`sandbox.rs:8-14, 64-69`); a write flag is `EROFS` (`:470`) | the package tree read-only (a write there STOPS the run, `:1412`); `OUT_DIR` read-write in memory (`:1443-1472`), 64 MiB and 4096 files (`bs/key.rs:51-53`) |
| escapes | `..` out of the root, a link out of the tree: `ProcLimit::Paths` (`sandbox.rs:80-92, 326-357`) | the same |
| links | `path_readlink` is `EINVAL` | the same |
| sleeping | `poll_oneoff` stops the guest (`calls.rs:212`) | the same |
| exit | `proc_exit` stops | the code is recorded, 0 is success (`script.rs:450`) |
| sockets | not linked: a module importing them fails at load (inferred from the list) | the same |
| commands | none | `blaze_build_script.spawn`/`reply` (`script.rs:224-237`), routed (`spawn_route.rs:152-170`): rustc from the probe table, `cargo -V`, `git` from the checkout, derived facts, the C mode (C tools of `probe.rs:52-54` kept as `c_requests`, the script told exit 0), else the executor or `NoSandbox` |

A refusal is an `errno`, a normal WASI answer (`layout.rs:244-255`); a stop traps and becomes a
typed error (`host.rs:634-646`). An unknown variable asked by a macro stops the run
(`sandbox.rs:292-308`).

The proc-macro bridge: host imports in module `blaze_proc_macro` (`host.rs:668-730`, guest
`shim.rs:25-39`: `dispatch`, `reply`, `output` once and at most 2^28 bytes, `env_var`,
`current_dir`); guest exports `_initialize`, `blaze_proc_macro_alloc`,
`blaze_proc_macro_expand` and the `__rustc_proc_macro_decls_*` global. Framing: a tag byte and
arguments, reply byte 0 for Ok, little-endian integers, 8-byte `usize`, nonzero `u32` handles
(`wire.rs:1-7, 80-97`). Caps (`server.rs:45-46`): 2^24 tokens, 2^28 bytes of symbol text, 2^24
spans, 2^22 handles. The 31 methods (`protocol.rs:69-101`): `track_env_var` and `track_path` are
queued and recorded after the run (`host.rs:859-874`); `ts_expand_expr` always errs;
`span_parent` and `span_subspan` are always `None`; `span_source`, `span_start`, `span_end`
return the handle; `span_source_text` is the input tokens' text; `span_byte_range`, `span_line`,
`span_column`, `span_file`, `span_local_file` are refused (`ServerError::Position`,
`:586-592`). The shim wraps `getenv` (`shim.rs:69-83`): each variable answers from `PackageEnv`
as set, unset or unknown (the table at `package_env.rs:15-28`); `CARGO_MANIFEST_DIR` is the
mount (`seen_env.rs:25-31`). The shim changes into the package root before each call
(`shim.rs:56-67`), so `sol!`'s relative reads work (tested at `sandbox.rs:595-621`).

A build script's environment (`bs/env.rs:8-22, 215-315`): `CARGO_MANIFEST_DIR`,
`CARGO_MANIFEST_PATH`, `CARGO_MANIFEST_LINKS`, `CARGO_FEATURE_*`, `CARGO_CFG_*`, `OUT_DIR`,
`TARGET`, `HOST`, `OPT_LEVEL=0`, `DEBUG=true`, `PROFILE=debug`, `RUSTC`, `RUSTDOC`, `CARGO`,
`CARGO_ENCODED_RUSTFLAGS=""`, `CARGO_PKG_*`, `DEP_<LINKS>_<KEY>`, and `NUM_JOBS=1` at run time,
not keyed; no `PATH`, no `HOME`. `rerun-if-*` directives are parsed and dropped
(`outputs.rs:4-6`). A script whose closure has `cc`, `cmake`, `bindgen`, `clang-sys` or
`pkg-config` runs NATIVELY today (`bs/native.rs:17-33`).

Bounds: fuel on (Wasmtime `consume_fuel`), 2^34 for a macro (`host.rs:105`) and 2^36 for a
script (`key.rs:52`); memory 1 GiB, tables 2^20, a 4 MiB wasm stack; no epoch interruption and no
wall deadline; script stdout 16 MiB, 256 KiB a line.

Keys: an expansion's base (`record.rs:532-562`) is the host version, the module's SHA-256, the
export index, kind and name, the call kind, the edition and the input and attribute tokens; its
read list has `env:`, `file:`, `dir:` and `stat:` rows with a digest or `Absent`, stored per
crate (`table.rs`). A script's base (`key.rs:131-158`) is the module, the whole environment, the
answers table, the C mode, every run limit, the runner and the `links` views; its read list also
has `git:` and `spawn:` rows, at most 2^16.

### 2. How the guest is built

`scripts/src/bin/proc_macro_modules.rs`, the D52 interim exception to rule 1: the pinned
nightly's rustc with `-Zbuild-std=std,panic_abort,proc_macro` for
`guest/wasm32-wasip1-macro.json` (`:1600`), as `RUSTC_WRAPPER`, linking wasi-libc and
`crt1-reactor.o` with `--wrap=getenv` (`:1257-1320`), offline under Seatbelt or bwrap. Build
scripts are wasip1 commands from a `library/` copy with the process shim swapped in. The wasm
standard library comes from rust-src and self-contained wasi-libc. The runtime reads only modules
keyed `builder bootstrap` (`module.rs:249`). (`macro_side`, in blaze-toolchain `side.rs` and
blaze-scope `units/macro_side.rs`, is a different thing: the wasm32-wasip1 source pack name
resolution reads.)

### 3. blaze's own execution path

`run_program` (blaze-cli `run_loop.rs:150-171`) makes a `RunSpec` with no sandbox and no limit
fields (blaze-std `procs/linked/run.rs:60-69`); `launch_env` passes `BLAZE_LAUNCH_FD=3`, the
store, the entry and `BLAZE_LAUNCH_GRANTS` (`run_parent.rs:322-341`; grants are `kind=name`
text, at most 64, names only: `grants.rs`). The launcher (`main.rs:158-233`, `run_mode`) gets
code lazily: the child asks `Up::Need` and the parent answers `Declare`, `Publish`, `Data`;
records are mapped from `<store>/<64 hex>` and BLAKE3-checked (`run_mode.rs:1233-1293`);
outside C names are bound by `dlsym(RTLD_DEFAULT)` (`:1454-1471`); a `Hosted` entry gets a
`HostTable`, a `c-main` one `argc`/`argv` (`:1759-1800`); it ends with `libc::exit` (`:1749`);
AArch64 only. The wire is CBOR frames behind a 4-byte big-endian length, 16 MiB at most,
`RUN_SCHEMA` 4 (blaze-launch `wire/run.rs:1-121`). A hosted run gets an empty environment, root
`.`, no file, and a wire of `Timer` and `Random` (`main.rs:143-145, 241-279`).

Confinement that EXISTS: native build scripts (`pm/native_sandbox.rs`): macOS `sandbox-exec`
deny-default (`:162-163`); Linux `NO_NEW_PRIVS`, user and network namespaces, seccomp on
`socket`, Landlock (`:239-320`); a wall deadline with `SIGKILL` (`:135-156`). Tests run under
Seatbelt on macOS only (`test_drive.rs`, blaze-run `child.rs:156-184`). What does NOT: blaze-core's
`Confine` (`lib.rs:162-195`) has no production implementation; the zygote's `sandbox_init` is
not live; blaze-sys's Landlock and seccomp wrappers have no callers; a `blaze run` child is
unconfined. Fuel: the LIR evaluator's (blaze-lir `eval.rs:25, 211-215`, one per instruction) is
test-only; the launcher meters nothing; the only bounds are wall deadlines from outside (none for
`run`, a budget per test, `ASK_BOUND` 300 s); no `rlimit` is set anywhere (macOS refuses
`RLIMIT_AS`); a footprint watcher exists in blaze-core. `world_run.rs` (blaze-cli tests) counts
fuel in LIR instructions (`:188-189`) and names its gaps: G1 `MakeKeepsNoLir`, G2
`NoBstdInTheWorld`, G3 `NoHostCalls`, G4 `NoUnwindNoAtomicsNoLibc`, G5 `LauncherOwnsTheProcess`,
G6 `NoGatePlant` (`:247-280`).

### 4. The std system layer

`library/` is nightly rust-src at `16d1ffac` (`UPSTREAM:5-8`). The platform layer is chosen by
`cfg_select` in `sys/pal/mod.rs:6-69`, each subsystem picking its own module. There is no
`target_os = "blaze"` and no `cfg(blaze)` in std (blaze does not set `cfg(blaze)` yet:
`std.md:276`); the blaze cfgs present are `target_arch = "naga"` and `wasm64-blaze-wasip1` with
`wasi_rt` (`library/std/Cargo.toml:86-89`). The one interception in std is `sys/provider`
(`__BLAZE_PROVIDER`: the seed and the wall time, on Linux and Apple), which nothing sets.
`library/blaze_inproc_rt` is a development runtime and hooks no `open` or `getenv`. bstd's hosted
mode (blaze-std `guest.rs`): `Run` (`:117`) through a thread-local during `bstd_hosted_start`,
offering out, err, clock, args and rng (`:46-54`), a table of write, clock, arg, grant, effect,
exit (`wire.rs:154-182`), exported by `#[bstd::main]` (`entry.rs:308-395`). Unix std reaches the
system through libc: `open64`, `statx`/`stat64`, `opendir`/`readdir64`, `getenv` and `environ`,
`_NSGetArgv`, `clock_gettime`, `getrandom` or `CCRandomGenerateBytes`, `write(1|2)`,
`pthread_create`, `posix_spawn`/`fork`, `socket` and `getaddrinfo`, `exit`. `wasi_rt` defines
`open`, `stat`, `readdir`, `getenv`, `clock_gettime`, `write` and `pthread_create` (`ENOTSUP`). A
Rust std program run by `blaze run` binds those C names by `dlsym` into the launcher's libSystem:
it has ambient access.

## The protocol (crates/blaze-comptime)

**Grants** (`grant.rs`). One value holds everything a run may reach: `roots` and `mounts`
(blaze-roots; read-only roots and at most one output), `env` (sorted, each `Set(value)` or
`Unset`; a name not there is unset to the run and asking it is a refusal), `args`, `spawns`
(`None`, or `CMode`: the build-script routing above, never the machine's programs), `output`
caps, `fuel`, `deadline`, `seed`, `clock`, the read list's `limits`. `Grants::proc_macro` (the
derive: no roots; `sol!`: the package root, the working directory its mount, so relative includes
resolve) refuses an output root; `Grants::build_script` (the package tree and `links` views,
`OUT_DIR` as the output root, cargo's variables, `build-script-build`, `CMode`) requires one.
Fuel (decided 2026-11-20): retired instructions, read by the driver as the kernel counts them
(macOS `proc_pid_rusage` `ri_instructions`, as blaze-meter reads it; Linux `perf_event_open`
where the system permits it), else CPU nanoseconds; the count of calls and the wall deadline
apply as well (`Event::Retired`, `Fuel { instructions, cpu_ns, calls }`). Fuel is never in a key,
so its inexactness is harmless: it decides only whether a run is stopped, and a stopped run
leaves no record. `RLIMIT_CPU` one second above is the kernel's own wall. The seed is BLAKE3's derivation of the call's key; the random stream is the keyed
extendable output; the clock is fixed at 0, as the wasm host answers today.

**Calls** (`call.rs`). `Hello`, `Look { path, want: Read | Stat | List }`, `Env`, `EnvAll`,
`Args`, `Cwd`, `Clock`, `Random`, `Write { Out | Err }`, `WriteFile`, `SetLen`, `CreateDir`,
`Remove`, `Rename`, `Sleep`, `Thread`, `Socket`, `Spawn`, `Bridge { method, args }` (the 31
methods by their tag; the token server answers), `TrackEnv`, `TrackPath`, `Return`, `Exit`.
Answers: `Hello`, `File`, `Stat`, `Listing`, `Value`, `Words`, `Clock`, `Random`, `Done`,
`Spawned`, `Bridge`, `Error(SysError)`. A file is read whole in one crossing; the guest's library
serves `read`, `seek` and `tell` from the bytes.

**Refusals.** A call outside the grants is answered with the error the real system gives a process
that lacks the thing (`ENOENT` for a path, `ENOTSUP` for a thread or a socket, `EROFS` for a
write outside the output root, unset for a variable), so a crate that probes behaves as it does
today, and a `Finding` names the path or name and the `Grant` that would allow it (`Mount`,
`Glob { root }`, `Write`, `Env`, `Spawn`, `Thread`, `Network`, or `Unaddressable`). A refusal is
never an end. Two changes from today, both decided by the owner (2026-11-20): a macro's unknown
variable is answered unset with a finding, and goes on the facts as asked-and-absent (so granting
it later reruns the call), instead of stopping the run; a script's write into its package is
`EROFS` with a finding instead of a stop. A name the system layer refuses outright (`dlopen`,
`fork`, `chmod`, below) crosses as `Call::Denied { name }` (wire tag 23, `Grant::Never`): a
finding, and the real errno to the code. A directory seen by `stat`, or opened where a file was
asked, is `Seen::Dir` (only its being a directory is an input); a listed one is `Seen::Listing`.
`PROTOCOL` is 2.

**The host's machine** (`host.rs`). States `Starting`, `Running`, `Answering` (a call in flight,
waiting on the driver's lookup, token server or routing), and the ends `Finished`,
`StoppedByFuel`, `StoppedByDeadline`, `Crashed`. Events: a frame, `Looked`, `Bridged`,
`Spawned`, `Cpu`, `Tick`, `Gone`. Effects: `Send`, `Look { root, path }`, `Bridge`, `Spawn`,
`Kill`. A lookup's answer is filtered by the root's globs (a file no glob declares is absent; a
listing shows only declared names, sorted by the host), pushed onto the read list and answered.
`finish` gives a `Done` (output, log, exit, the output root's files, `Checked` reads, sorted
facts, findings) only for a finished run whose read list passes `under`; every other end is an
`Ended` that holds no output.

**What keys the record.** The call's key (as today: module or code identity, export, inputs) plus
the checked read list's digest and the facts' digest (each variable answered with its value's
digest or unset, each command routed with its request and the digest of its answer). Randomness
and the clock need no entry: both are functions of the key.

**The wire** (`wire.rs`). A 4-byte big-endian length (the launcher's prefix), then one
deterministic CBOR array `[tag, fields..]`; `WireLimits` caps the frame (256 MiB), a name
(4 KiB), a word list (1 MiB) and randomness (1 MiB) before anything is sized by them.

**The guest's machine** (`guest.rs`). `call`, `next_effect`, `step` with the answer frame,
`answer`, which checks the answer fits its call (`fits`).

## The std backend

The seam is the one that already exists: the launcher binds a program's outside C names by
symbol at placement (`run_mode.rs:1454-1471`). For a compile-time run it binds them to blaze's
own functions instead of libSystem's: `open`, `open64`, `openat`, `read`, `pread`, `lseek`,
`close`, `stat`, `fstat`, `lstat`, `statx`, `opendir`, `readdir`, `closedir`, `getcwd`, `getenv`,
`environ` (a table built at start), `clock_gettime`, `gettimeofday`, `getrandom`, `getentropy`,
`CCRandomGenerateBytes`, `arc4random_buf`, `write`, `pthread_create`, `posix_spawn`, `fork`,
`execve`, `socket`, `getaddrinfo`, `nanosleep`, `exit`, `_exit`, `abort`, and the proc-macro
shim's five imports. Each is a small function over the guest machine: it encodes one `Call`, writes
the frame on the launcher's channel, reads the answer and sets `errno`. A name the table lacks is
a placement error naming the symbol, never a quiet bind to the system's. Unmodified std and
unmodified crates run on it, because the library under them changed and their code did not.

### The system layer (crates/blaze-comptime-sys)

**The table** (`table.rs`, `NAMES`, `find`). 265 rows, sorted bytewise and found by binary
search: 166 for both systems, 42 macOS only, 57 Linux only. Each row says how the name is
answered: `Own` (77: the process's own definition: memory, strings, unwinding, the one thread's
locks, `errno`'s accessor), `OwnChecked` (3: `mmap`, `mmap64`, `mprotect` after an argument
check), `Local` (106: the layer's own state: descriptors, directory streams, `dup`, `lseek`,
`pipe`, fixed identities), `Asks(CallKind)` (53: one protocol call), `Refused(errno)` (25: a
`Denied` finding and the real errno), `Data` (1: Linux `environ`).

How the list was derived: the macOS rows are every undefined symbol of the pinned toolchain's
aarch64-apple-darwin `libstd` (`ar x` of the rlib, `nm -u --no-llvm-bc` on its object files),
plus `proc_macro`, `core` and `panic_unwind`'s (`memset`, `_Unwind_RaiseException`,
`_Unwind_DeleteException`): 199 names. The Linux-only rows are read from std's source (each
`libc::` call under `unix`/`linux`, with glibc's `64` aliases): 58 names, unchecked by `nm`
until it runs on the Linux VM. Eight rows come from the brief and from crates over std:
`arc4random_buf`, `execve`, `gettimeofday`, `mach_absolute_time`, `posix_spawn`, `readdir_r`,
`rewinddir`, Linux's plain `realpath`.

**A name not in the table** is refused when the record is bound: `binding(name, os)` returns
`BindRefused { name, why: NotInTable | OtherSystem { on } }`, and the launcher refuses the record
naming the symbol and the item that referenced it. It is never bound to the real C library.
`dlsym` by the code goes through the same table (a weak lookup of an unlisted name finds NULL).

**Memory.** `mmap` is the real one only for `fd == -1`, `MAP_PRIVATE | MAP_ANON` plus the
harmless flags (`MAP_NORESERVE`; `MAP_STACK` on Linux), with `prot` within `READ | WRITE`; a file
mapping, `MAP_SHARED`, `MAP_FIXED`, `MAP_JIT` or `PROT_EXEC` is `EPERM` and a finding. `munmap`
is the process's own.

**The core** (`sys.rs`, `Sys`): a sans-IO machine (`new`, `step(now, event)`, `next_effect`,
`next_deadline`, `state`) over blaze-comptime's guest machine. It holds the descriptor table (at
most 256), directory streams, the environment overlay (`setenv` changes only the run's view;
nothing crosses), the working directory, pipes and spawned commands' results. A file opened for
reading is read whole by one `Look { Read }` and served by `read`, `pread`, `lseek` and `fstat`
from the bytes; one file is at most 64 MiB and all held files at most 512 MiB together (`Limits`),
and a bigger one is `EFBIG` to the code (the host's `TooLarge`). A write crosses when made
(`WriteFile` at its offset), so `fsync` has nothing to do.

**The C functions** (`child.rs`, feature `child`): one `unsafe extern "C"` function per row, NOT
exported under the C name (the launcher's own calls keep the real library); `binding` hands the
launcher their addresses. The layer lives in one process cell, the named exception to C7: a
compile-time child has one thread (`pthread_create` is `EAGAIN` after a `Thread` finding), and a
reentrant call is a typed fault. The launcher implements `Transport` (send a frame, receive one,
its clock, end, fault) and hands `Real` (`errno`'s accessor, `mmap`, `mprotect`, `dlsym`).

Later, and only when it removes code: a `target_os = "blaze"` platform module in `library/std`
whose `fs`, `env`, `time`, `random`, `thread` and `process` call the protocol directly, deleting
the C-name table. Not first: it needs a std build of its own, which is what D175 deletes.

## How the guest is built and launched

Built by blaze's own back end to code records, like any program blaze runs (D159, D163): the
proc-macro crate with its closure (syn, quote, proc-macro2 against blaze's `proc_macro`, which is
the guest end of the bridge), or the build script with its build dependencies. No wasm target, no
second std build, no `proc_macro_modules.rs`.

Launched by blaze's launcher as a child, entry `comptime:<symbol>`, with the C-name table above
and the protocol on fd 4 (fd 3 stays the launcher's own channel; "The launch" below). Confined by what exists today in `pm/native_sandbox.rs`, moved behind one
`Confine` implementation (D177's `Level::Inputs`): Seatbelt deny-default on macOS with no file
rule at all (the guest reads nothing itself: every byte comes over fd 4), and on Linux
`NO_NEW_PRIVS`, a user and network namespace, Landlock with no paths, seccomp allowing the
launcher's own calls. The driver reads the child's CPU time each tick (`proc_pid_rusage` on
macOS, `/proc/<pid>/stat` or `getrusage` of the reaped child on Linux) as `Event::Cpu`, sets
`RLIMIT_CPU` one second above the fuel, and kills on `Effect::Kill`.

### The proc_macro route

Unchanged: `proc_macro`'s client side, the `with_api!` method table and its buffer encoding (a
`u8` method tag in `with_api!` order; a `&str` as an 8-byte little-endian length and its bytes;
an `Option` tag first, `None` 0; a reply `Result<_, PanicMessage>`, `Ok` 0). Only the transport
moves: the shim's (`blaze-proc-macro/guest/shim.rs`) import block, under `cfg(blaze_native)`,
names plain C symbols (`blaze_comptime_dispatch`, `_reply`, `_output`, `_env_var`,
`_current_dir`) instead of the `blaze_proc_macro` wasm module, and the table gets five `Local`
rows for them with functions in `child.rs` (not yet written: they come with the shim edit, whose
signatures they must match): `dispatch` sends methods 0 and 1 (`track_env_var`, `track_path`) as
`Call::TrackEnv` and `Call::TrackPath` and replies `[0]` (`Ok(())`); every other method is
`Call::Bridge { method: buf[0], args: &buf[1..] }`, its answer's bytes the reply buffer
(`Sys::call`, `Sys::bridge_reply`). The host side is already a sans-IO machine:
blaze-comptime's host emits `Effect::Bridge` and takes `Event::Bridged`, which the existing token
server answers; nothing new is needed there.

Where the entry is found. The wasm build exports `blaze_proc_macro_expand(decls, index, input,
len)` and the host reads the registrations from `__rustc_proc_macro_decls_<hash>__`, which
`scripts/src/bin/proc_macro_modules.rs` lists as `macros` lines (index, kind, name, helpers). A
code-record program names the same static by its item: the proc-macro crate's harness static,
which blaze's front end must generate (it does not yet: no `_DECLS` item is made for a
`proc-macro` crate, blocker 2 below), and the entry `comptime:blaze_proc_macro_expand` with the
decls static's record and the macro's index as its two first arguments.

## The first rung

`crates/blaze-comptime/rungs/derive-hello/` (its own workspace): `#[derive(Hello)]` using only
`proc_macro`, `input.rs` and `expected.rs`:

    impl Greeter { pub const HELLO: &'static str = "Greeter"; }

The harness call (in `comptime_equal.rs`'s test, `#[ignore]` until the blockers clear):

    let wasm = expand_wasm(&call)?;            // today's engine: tokens, diagnostics, reads
    adapt(&wasm.reads, &package, &mut wasm_entries, &mut wasm_facts)?;
    let native = expand_native(&call, &spec)?; // the launch below; its host's Done
    let w = Outcome { tokens: &wasm.tokens, diagnostics: &wasm.diags, reads: &wasm_entries, facts: &wasm_facts };
    let n = Outcome { tokens: &native.tokens, diagnostics: &native.diags, reads: native.reads, facts: native.facts };
    assert_eq!(blaze_comptime::first_difference(&w, &n), None);
    assert_eq!(n.tokens, bridge_tokens_of(include_str!("rungs/derive-hello/expected.rs")));

Blockers, in order:

1. **G1, make keeps no LIR** (`blaze-ladder/src/run/make_one.rs`): the code records of the
   macro crate are not kept for the launcher.
2. **No proc-macro harness static**: blaze's front end makes no `_DECLS` registration item for a
   `proc-macro` crate, so a native build has no entry table.
3. **`proc_macro` and `std` as records**: built from the shelf's `library/` by blaze's back end,
   with the shim's native import block (`cfg(blaze_native)`).
4. **G5, the launcher owns the process**: `run_mode`'s `run` ends in `libc::exit` (about :1749)
   and `stop_with` in `_exit` (about :1597); a comptime run needs the entry's status returned and
   a `comptime:` entry form.
5. **The table bound** in `run_mode`'s outside binding (below).
6. **G4** (no unwinding, atomics or libc in the evaluator) only for the evaluator differential,
   not for the native run.

## The launch

    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Records { name: "derive-hello", code: macro_crate_code },
        args: &[],
        env: &[("SOURCE_DATE_EPOCH", "0")],
        roots, mounts: &[], cwd: "/",         // Grants::proc_macro: no roots for a derive
        helpers: &[], helper_folders: &[], channels: &[],
        clock: ClockSpec::Fixed { unix_secs: 0 },
        seed: key_seed,                        // BLAKE3 derivation of the call key
        budget: Budget { cpu, memory, steps: Some(fuel_instructions) },
        deadline,
        streams: HermeticStreams { stdout: Capture::Null, stderr: Capture::Null },
        level: Level::Execution,
        report: ReportSpec::WholeRoots,
    })?;
    let run = Hermetic::new(HermeticCapOpts { spawner, clock }).run(cx, &spec, &files, alloc)?;

`Level::Execution` goes to `EXECUTION_BACKENDS`; `BlazeLauncher` answers `LAUNCHER_ABSENT`
today and becomes: start blaze-launcher with `BLAZE_LAUNCH_FD=3` (its own channel, as today),
`BLAZE_LAUNCH_ENTRY=comptime:blaze_proc_macro_expand`, the protocol on fd 4, and the
OS confinement of `Level::Inputs` (Seatbelt, or Landlock and seccomp) on top; `steps` is the
retired-instruction budget. On Linux, none of Landlock, seccomp, the namespaces or
`perf_event_open` has ever been run in this tree.

## The equality harness

`first_difference(wasm, native)` (`equal.rs`) compares, in order, the tokens (bridge wire), the
diagnostics, the read list (checked entries) and the facts, and returns the first difference as
a typed `Difference` with both sides. Over tempo: for each proc-macro call and build-script run
of a cold `blaze check` of tempo, run the wasm engine (today's) and the native one on the same
call key, turn the wasm read list's rows into blaze-roots entries and facts (an adapter in
blaze-proc-macro, `comptime_equal.rs`: `file:` and `stat:` of a file to `Seen::File`, `dir:`
to `Seen::Listing`, `stat:` of a directory and `file:` where a directory is to `Seen::Dir`,
`env:` to an `Env` fact; each subject answered again from the package, because the two sides
digest differently, and a row found on one side and absent on the other is a typed error), and
write one row per call: the macro,
the call's span, `equal` or the `Difference`. The verdict is zero differences on tempo, reth and
blaze; each difference is a reduced world test (`world_run.rs`) before it is fixed.

## The stages

Each stage names its oracle and what it deletes; nothing is deleted before its oracle passes on
the named projects.

1. **The protocol, simulated** (this step). Oracle: the simulation in `blaze-comptime/src/tests.rs`
   (below) and blaze-roots' own, with controls. Deletes nothing.
2. **The host driver on today's wasm guests.** The WASI view (`sandbox.rs`, `script_sandbox.rs`)
   is reimplemented as a translation from WASI calls to `Call`s served by the host machine; the
   proc-macro and build-script hosts take their read lists from it. Oracle: every expansion and
   script output of tempo byte-equal before and after, and the read lists equal under the
   adapter. Deletes: the two sandboxes' own answering code and read-list builders.
3. **One native guest end to end.** A derive (serde's) built by blaze's back end, launched with
   the C-name table, confined. Oracle: `first_difference` is `None` for every serde derive in
   tempo; a guest that opens `/etc/passwd` gets `ENOENT` and a finding. Deletes nothing.
4. **All proc macros native.** Oracle: zero differences over tempo, reth and blaze's own macros
   (`sol!`, clap, thiserror among them). Deletes: Wasmtime from the proc-macro host, the
   `blaze_proc_macro` wasm imports and shim, the macro target JSON, the macro half of
   `proc_macro_modules.rs`.
5. **All build scripts native**, the C mode through D174. Oracle: zero differences on every
   script output of the three projects. Deletes: Wasmtime and its crates, blaze-wasi, the wasip1
   build of `library/`, wasi-libc in the shelf, `proc_macro_modules.rs`, and `bs/native.rs`'s
   unconfined runner. Rule 10 is rewritten.

## What is given up

One expansion record valid on every architecture by construction. Native code may differ
across architectures in floating-point edge cases (fused multiply-add, `libm` results, NaN
payloads), so the record's key names the architecture until a test shows outputs equal across
them; a macro that does no floating point could later drop it by an effect fact (no `f32`/`f64`
operation reachable), which the effect summary can give.

Also: exact fuel. Retired instructions (and, where they cannot be read, CPU time) vary a little
between machines, so whether a run near its budget finishes can differ; a finished run's output
cannot, and fuel is in no key.

## Risks, ranked

1. **The back end must build syn-scale code correctly first.** Every proc macro in the three
   projects pulls syn, quote and proc-macro2: the largest generic-heavy code blaze would run.
   Until blaze's back end runs it from code records (D159, D163, world_run.rs G1 to G5: no LIR
   kept, no bstd in the world, no host calls, no unwinding, atomics or libc, the launcher not
   owning the process), nothing past stage 2 starts.
2. **The libc seam's completeness.** A name std or a crate reaches that the table lacks is a
   placement error (good: loud), but `syscall(2)` or inline assembly reaches the kernel past it;
   only the OS sandbox stops that. Linux confinement has not been run in this tree (tests answer
   `NoSandbox` there), so stage 3 must run on the Linux VM with a guest that tries each call.
3. **Fuel is read from outside.** A guest that spins in its own code is bounded by the kernel's
   instruction count (or CPU time) and the deadline; threads are refused and spawns go through
   the host, which must stay so under the seccomp profile.
4. **Arch in the key** halves sharing between arm64 and x86-64 machines until the float test
   exists.
5. **Behaviour changes at the refusals** (unset instead of a stop for unknown variables; `EROFS`
   instead of a stop for writes into a package): a crate that reacted to today's stop by never
   running now runs; the harness reports it as a difference, which is the point.

## Decided (owner, 2026-11-20)

- Fuel: retired instructions where the kernel gives them, else CPU nanoseconds, with the call
  count and the wall deadline; never in a key.
- An unknown variable: unset, a finding, and an asked-and-absent fact. A script's write into its
  package: `EROFS` and a finding.
- The architecture is in the expansion key until the equality harness shows tempo's expansions
  equal across the two architectures.
- The wasm path goes (the owner: "can we remove web assembly stuff tho?"): stages 4 and 5 delete
  it; about 27% of the Names stage's CPU in tempo's first check is Cranelift compiling the macro
  modules today.
