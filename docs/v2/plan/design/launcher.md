---
status: plan
summary: "How a program runs under the `edit` goal (D18, D21): a launcher in the program's own process places code records behind one slot word a function, appends and never overwrites, retires old bodies by per-thread epochs, takes an edit as one gated transaction, and is to run many tests in one process as domains. Built - the core (`crates/blaze-launch`), the drivers (`crates/blaze-launch-rt`) and the binary (`crates/blaze-launcher`), whose run mode is the process of `blaze run` and `blaze test` (run.md); the edit transaction and retirement are built, not wired. Not built - the scan of a thread from outside, the back-edge poll, x86-64 runs, domains, selection by marks, promotion."
date: 2026-11-12
---
# The launcher: running a program under `edit`

How `blaze run` and `blaze test` reach the launcher (the parent, the socket pair on descriptor
3, records placed on first call, statics, the start list) is run.md's. This page is the
launcher's own design: slots, the code area, retirement, an edit as a transaction, domains.

## What exists

- `crates/blaze-launch` (`no_std` on `core` and blaze-own, `#![blaze::sans_io]`): `Launch` owns
  the slot table, the area (bump, never reused), the batches and the threads; `stage` validates
  and appends or changes nothing, `commit` returns the ordered `Write`s and makes the batch,
  `report` and `collect` retire. `record`, `place`, `frame`, `image` and `wire` (the launcher's
  protocols in deterministic CBOR, written and read without an allocator of its own). Held by
  the simulation `src/tests/sim.rs`, whose oracle states the invariants and whose controls plant
  a bug each.
- `crates/blaze-launch-rt`: the drivers. The code area (`sys`), the gate, stub and resolver
  templates (`arch`), the control thread, thread-locals, unwind registration, the Linux symbol
  image for backtraces (`symbols`), the GDB JIT interface (`jit_debug`), sampling, the fault
  handler and `run_mode`, the placement path `blaze run` uses.
- `crates/blaze-launcher`: the binary. Its run mode is the program's process under `blaze run`
  and `blaze test`; a `mode=run` stub sidecar is what a test's `CARGO_BIN_EXE_*` runs.

Built, not wired: the gated edit, restart in place and retirement by epochs (`drive::Driver`,
`control`) run in blaze-launch-rt's tests alone. The launcher's sidecar mode that would take an
edit has no server, so nothing sends a running program one.

## 1. Slots

**One word a function.** A function's slot is an 8-byte word in a table the launcher owns: the
address to run. Every call from blaze's code is `call [rip + word]` (x86-64) or `adrp x16; ldr
x16, [x16, lo]; blr x16` (AArch64): one load, as a call through a GOT with no PLT. A function
whose address is taken gets an entry thunk (the same three instructions, written once), so a
`fn` pointer never names a body. As built, every slot has its thunk at a fixed place (`thunks +
slot * 16`, or `* 8` on x86-64), written when the slot is declared and never freed: a function's
address is then a constant of its slot, known before any code is placed. The word is the lazy stub, the patch point and the tier switch:

| The word names | When |
|---|---|
| the slot's stub (`adr x16, .; b head`, 8 bytes; `lea r11, [rip - 7]; jmp head`, 16 on x86-64; written when the slot is declared) | not called yet: the resolver saves the argument registers, asks the control thread for the slot, which asks the parent (`Up::Need`), and jumps *through the word again*, never to an address it was handed |
| a tier 0 body | after the first call, and again when an edit takes away an optimized body that built the edited function in |
| an optimized body | when promotion ([backend-next.md](backend-next.md)) lands it |
| a gate entry | for the length of one transaction's commit (section 4) |

Rejected: a direct branch a function (`bl island; island: b body`, the island rewritten). It
saves the load and costs a second taken branch, and on AArch64 it needs every body within 128
MiB of its island (spans of the area with a replica of the table each, veneers where one does
not reach), an instruction-cache flush and, on macOS, a write to JIT memory for every switch.
A word is data: one aligned store, any distance, the same on every target. Rejected: a
cell and a table (two loads; a pool of tables); a patch of each call site (not atomic, and unbounded).

**Code is appended.** The area is address space reserved once, in one mapping with the table:
`| words | stubs | entry thunks | code area |`, 2 GiB in all (nothing is committed until
written). The bound is the call's own: it loads its word relative to itself, and x86-64
reaches 2 GiB (`rel32`), AArch64 4 GiB (`adrp`); with the table at the mapping's start and the
mapping no longer than that, every body reaches every word and there is one table. The
words lie in the code's mapping, and only the control thread stores to one, as to a gate's flag. At a
few hundred bytes a tier 0 body, 2 GiB is millions of edits; then the restart in place. A body goes at the end, is hashed against its record's content id, and only
then can a word name it. **An offset is handed out once and never again**: a program counter
names one body for the life of the process, so a profile sample, a backtrace or an unwind
lookup taken at any time is still right later. What comes back is memory, not addresses: a
whole page all of whose bodies are freed is given to the OS (a hole punched in the memfd on
Linux, where `MADV_DONTNEED` would not free a shared mapping's pages; `MADV_FREE_REUSABLE` on
macOS). An area with no room left is a restart in place that says so.
Rejected: a free list of ranges: it saves address space, which is not scarce.

**Retirement.** A commit that takes a body out of every word puts it, with everything else the
commit unlinked (the gate, entry thunks of removed functions), in one *batch* stamped with the
epoch the commit ends by publishing. A thread *clears* a batch when, having loaded an epoch at
or past the stamp, it walks its stack and finds no frame in any range of the batch (a return
address `a` is in `[start, end)` when `start < a <= end`; a walk that does not finish holds
everything; a frame in the batch's gate holds the whole batch, since the gate may still jump to
an old body). Once clear, always clear: no word names the body, and the resolver and thunks
never hold a body's address. A batch is freed when every thread alive at its stamp has cleared
it or ended. A thread born later is told the epoch before it runs and holds nothing older.

How a thread reports: (a) at a safepoint, itself: the resolver's exit, `Live::poll`, a domain's
start and end, and the poll tier 0 code makes on a loop's back edge while a batch waits (one
load and a branch not taken otherwise); (b) a thread that has not reported while a batch waits
is scanned from outside by the launcher's control thread: stopped (`thread_suspend` and
`thread_get_state` on macOS, as the sampler does; on Linux the launcher's one reserved real-time
signal, taken on the thread's alternate stack with `SA_RESTART`), its registers read and its
stack walked, then resumed. Such a walk also reports the program counter and x16: on AArch64 a
call holds its target there between the word's load and the jump. Rejected: safepoints alone
(a thread blocked in `read` holds every edit's code for ever); a count of activations
a body (a store on every call and return).

## 2. The process

- **The launcher is the process.** The kernel starts it; it never forks the program. So a
  crash is the program's alone, the sandbox is the one its parent spawned it in, signals and
  `ptrace` are real, and the parent holds none of the program's code.
- **No link.** Code records are named by content id; the launcher copies each into the area,
  applies the record's fixups (slot words, domain offsets, its own constants), checks the hash,
  and commits. Imports from system libraries are bound with `dlsym` as records name them.
- **The linker stays** for `max` and for a file that must stand alone.
- **The control thread** is the launcher's one thread that mutates: it reads the socket, runs
  the core, writes code and words. A program thread only loads words and reports.

**macOS.** Anonymous memory is the write view and `mach_vm_remap` aliases it read and execute
at the code address, each view's maximum protection lowered (`crates/blaze-launch-rt/src/sys.rs`
says why not `MAP_JIT`). After a write, `sys_icache_invalidate`. The pages carry no code
signature, so they run only where code signing is not enforced on the map; the launcher asks
`csops` first and refuses by name. Under `edit` none of the program's code is in a Mach-O.

**Linux.** The area is a `memfd`, mapped twice: executable where code runs, writable at an
address only the control thread uses. No `mprotect`, and no anonymous writable-executable
memory (SELinux `execmem`). After a write on AArch64: `dc cvau`/`ic ivau`, then
`membarrier(PRIVATE_EXPEDITED_SYNC_CORE)` before the word is stored. x86-64 needs neither:
new code was never executed and a word is data.

## 3. Unwinding, debuggers, profilers

A code record carries its unwind entry and line table (LIR has both). The launcher keeps one
append-only index of the area, sorted by offset (appending keeps it sorted): `offset -> (len,
record id, unwind entry)`, read without a lock, and an entry leaves only when its batch is
freed.

- **Panics and backtraces.** The unwinder asks the launcher, not a list of registered frames:
  Apple's libunwind through `__unw_add_find_dynamic_unwind_sections` (private; commented and
  symbol-tested, `macos-private-apis`), ELF through the same hook in the LLVM libunwind that
  blaze's std builds, and later (Windows is out of scope now) Windows through
  `RtlInstallFunctionTableCallback`. Lazy code unwinds.
  std's backtrace names a frame by asking the dev runtime: the index gives the record, whose
  name and lines come from the store.
- **A debugger** gets the GDB JIT interface (`__jit_debug_descriptor`): one object a commit,
  made from the records' debug info (`crates/blaze-launch-rt/src/jit_debug.rs`), and only once a tracer is attached
  (`TracerPid`; `P_TRACED`).
- **A profiler.** Linux: `/tmp/perf-<pid>.map` appended a commit. blaze's own profiler asks
  for the index over the socket. Because offsets are never reused, a sample taken before an
  edit still names its function.

## 4. An edit is one transaction

Checking stays eager: under `edit` blaze type- and borrow-checks the whole workspace in
the background, the build's status is not `ok` until that is done, and `run` starts as soon as
the code it needs first is there. A stub that fires on a function with an error stops the
program with the diagnostic (`Refused`), not a trap.

An edit: (1) blaze checks the whole workspace with it; any error and nothing is sent, the
program runs on. (2) It classifies each changed item. (3) It compiles tier 0 code for every
changed function *that is bound* (an unbound slot stays a stub), plus the tier 0 code of every
bound optimized body that built a changed function in (its trace, backend-next.md). (4) `Edit{txn,
binds, together}`. The launcher **stages** (appends, hashes; nothing is reachable; any failure
frees the staged code and answers `Failed`) and **commits**, which cannot fail:

1. a gate is appended: for each bind, `if *flag { jump new } else { jump old }`;
2. each word is stored to its gate entry (nothing changes: the flag is clear);
3. **the flag is set: this one store is the commit**;
4. each word is stored to its new body; the epoch is published; the old bodies and the gate
   become a batch.

A call resolves old or new at its gate's load of the flag, so every call before the store runs
the old program and every call after it the new: no thread sees one function of the edit new
and then another old. A function running when the edit lands finishes as it was. A commit of
one bind, or of bodies that mean the same (promotion), stores the words directly.

| Edit | Live? | Why |
|---|---|---|
| a body | yes | its key holds its signature and the layouts it names; they did not change |
| a const's value | yes | every function whose code id moved is rebound |
| a new function, a new static | yes | a new slot; a new offset, filled in each domain |
| a removed function | yes | its callers are in the same edit; frames in it finish |
| a signature | yes, if `Probe` finds no frame in a bound caller; else restart | the function gets a *new slot* (a slot is a path and an ABI): new callers call it, and an old caller on a stack would call the old body for as long as its frame lives |
| a type's layout | yes, if no function ever bound names the layout; else restart | then no value of it exists; else one may be on a heap |
| a static's type, or the initial value of a mutable static | restart (a test: the next domain) | its value is in each domain's image |
| the launcher's schema, std, a dependency's layout | restart | |

**Restart in place**: the launcher answers `Restarting`, and `execve`s itself: the same pid,
terminal, sandbox and debugger; the parent has the new code ready before it says `Restart`.
**The slot is a path and an ABI.** A function whose signature changes gets another slot: its
old body stays bound in the old slot for the frames of old callers still on a stack, the new
body is bound in a new slot, and every caller is a changed body that names the new slot. So a
changed signature needs no `Probe` and no restart: it is an added function plus its callers'
body edits, switched together through a gate. The slot's ABI is a fingerprint of what a
caller's code assumes of the callee beyond its address: the calling convention, each
argument's and the result's passing mode, and the layout of every type the signature names,
through references and pointers too, closures by their captures' layouts, generics by their
arguments. It comes from the front end, never from code hashes: with `g` handing `&Foo` through
an unchanged `h` to `f`, an edit of `Foo`'s layout changes `g` and `f` but not `h`, and reading
`f`'s interface off its unchanged caller would apply live an edit that a thread inside old `g`
would see half of.

## 5. Domains

A *domain* is one instance of the program's mutable statics. Code addresses a mutable static
as `domain base + offset`; the base is one word of the thread's block at a fixed offset from
the thread pointer (`fs:[N]`; `tpidr_el0 + N`; on macOS `tpidrro_el0` and a reserved TSD key
(private, `macos-private-apis`)), so a static's address is one load and an add. A static that
is immutable (no interior mutability) stays shared and PC-relative.

- **The image.** Each record of a static gives its initial bytes and the fixups in them. The launcher keeps the initial
  image as a `memfd`; a domain is a private copy-on-write mapping of it with the pointers
  that point into the image rebased, so starting one costs the pages it touches. A const that
  points at a mutable static becomes domain data (as Relax's `rehome_const`).
- **Thread-locals** are a second word: the thread's block for its current domain, allocated
  on first touch, its destructors run when the thread leaves the domain or ends.
- **Threads.** std's thread start (blaze builds std) copies the parent's base. A domain ends
  when its entry returns and its threads have ended; its image is unmapped then.
- **What code generation changes.** LIR's `data_addr` of a mutable static lowers to the base
  load and a `DomainOff32` fixup in place of the GOT load, and `tls_addr` to the second word.
  Under `max` nothing changes: a static is resolved absolutely.
- **What a domain cannot hold**: what the OS keeps for the process (cwd, environment,
  descriptors 0 to 2, signal handlers, umask, limits, `exit` and `abort`, `fork`) and the
  writable data of foreign code (a C library's globals). The launcher binds those imports
  (`chdir`, `setenv`, `sigaction`, `dup2`, `exit`, ..., and every import of an object with
  writable data) to a shim. In a domain that shares its process the shim does not do the
  effect: it ends the domain as `NeedsProcess{effect}`, and the test is run again alone in a
  process, where the shim records and performs it. The record is kept with the test, so next
  time it starts in its own process. A crash in a shared process reruns the tests that were
  in it, each alone.
- Rejected: a pinned register for the base (x18 is the OS's on macOS, x86-64 has none to
  spare, and foreign callbacks arrive with it unset); a remap of one image a switch (domains
  could not run at once); a copy of the code a domain.

## 6. Test selection

A test's result is keyed by [test.md](test.md)'s rule, the one `blaze test` runs: a record
read-addressed by the functions the test entered and the outside reads it made, found after an
edit by a postings record (crates/blaze-run). What the launcher adds is how the entered set is
seen: a stub fires once a process, so with domains the set is taken a domain: under
`blaze test` every body's entry sets one byte of its domain (`base + mark + slot`), and a
test's set is its marked slots, each widened by the bodies its code built in. The launcher's
recording wrappers are not built (test.md).

## Not built

- The scan of a thread from outside (section 1, (b)) and the poll on a back edge: a thread that
  calls no new function reports nothing, so a replaced body's memory stays (sound, and a few
  hundred bytes an edit).
- `Probe` and the classifier of section 4; sending an edit to a running program at all.
- x86-64: the call sequences and the placement are tested as bytes; nothing of the launcher has
  run there, and thread-locals are refused by name.
- File and line in a backtrace frame (no DWARF line table is made), a body placed after std's
  first backtrace, and macOS backtrace names (std asks dyld's image list there).
- Domains (section 5) and selection by marks (section 6).
- Promotion of optimized bodies ([backend-next.md](backend-next.md)) and its retreat on an edit.

### Environment variables

The launcher and `blaze run` parent exchange run-mode parameters and telemetry controls via environment variables (stripped before user code starts):

- `BLAZE_LAUNCH_GRANTS`: the grants `blaze run` hands a hosted program (`crates/blaze-launch/src/grants.rs`).
- `BLAZE_LAUNCH_STORE`: directory holding the run's code records (paired with `BLAZE_LAUNCH_FD=3`).
- `BLAZE_LAUNCH_NATIVE`: 64-hex-digit native library record ID when the program links C code.
- `BLAZE_LAUNCH_TELEMETRY`: path to the per-run shared-memory telemetry ring file (`crates/blaze-launch/src/telemetry.rs`).
- `BLAZE_LAUNCH_EXPLORE`: when set to `1` (by `blaze run --open`), instructs the launcher to ensure the local viewer (a helper process it starts with `--viewer`, which nothing depends on for correctness) is running and open Explore if no tab is connected.
- `BLAZE_LAUNCH_SANDBOX_TEMP`: per-test `$TMPDIR` root passed by `blaze test` on Linux so `blaze-launcher` enters the Landlock + seccomp + mount/net/IPC/user namespace sandbox before guest code starts (removed from env before guest execution).
- `BLAZE_LAUNCH_UNWOUND_FILE`: per-test unwind sentinel path or `fd:4` passed by `blaze test` for harnessed tests so uncaught Rust panics are distinguished from `SIGABRT` (removed from env before guest execution).
- `BLAZE_LAUNCH_BIN_MAP`: newline-separated `<guest_path>\t<host_stub_path>` map passed by `blaze test` on macOS when an integration-test or benchmark suite exposes `CARGO_BIN_EXE_*` binaries, enabling async-signal-safe path remapping of compile-time `/target/debug/bin/<package>/<name>` literals in `LauncherOwn` (removed from env before guest execution).
- `BLAZE_LAUNCH_BINS_DIR`: directory holding the test command's `CARGO_BIN_EXE_*` launcher stubs, bind-mounted read-only at `/target/debug/bin` inside the Linux test sandbox so compile-time paths exist as real kernel-visible files (removed from env before guest execution).
- `BLAZE_LAUNCH_BINS_SOCK_DIR`: short, user-private (`0o700`) directory holding the `BinServer` Unix domain socket, preserved read-only across the Linux test sandbox `/tmp` mask (removed from env before guest execution).
- `BLAZE_ALLOC_SAMPLE_BYTES`: byte-stride override for allocation telemetry sampling in `blaze-launcher`.
- `BLAZE_EXPLORE_LOCAL`: when set, opens the local loopback `/explore` URL instead of `https://ix.dev/blaze/explore`.
- `BLAZE_NO_TELEMETRY`: opt-out of per-run live shared-memory telemetry.
