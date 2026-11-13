---
status: plan
summary: "How a program runs under the `edit` goal (D18, D21) - a launcher in the program's own process places code records behind one slot word a function, appends and never overwrites, retires old bodies by per-thread epochs, takes an edit as one gated transaction, and is to run many tests in one process as domains. Built - steps 1 and 2 (`crates/blaze-launch`, `crates/blaze-launch-rt`), step 3 (tier 0 code that calls through slot words, the record, its placement, data, statics, thread-locals, unwind entries), step 4 behind `BLAZE_LAUNCH=1` (`blaze run` of a binary runs in `blaze-launcher`, unlinked; the daemon still compiles the whole program first), step 5 in a first version (one changed body applied to the running program, anything else a restart in place), the walk's frontier form in the codegen driver (one function ready to compile with no walk; nothing in the daemon calls it yet). Not built - the scan of a thread from outside, lazy serving of needs in the daemon (the seam is written here), placed code kept across runs, test binaries, backtrace names, x86-64 (compiled, never run), domains, promotion."
date: 2026-11-12
---
# The launcher: running a program under `edit`

## What exists (read 2026-11-11, tree `all-1111`)

| Piece | Where | What it does, and what is wrong with it for `edit` |
|---|---|---|
| Patch room and the splice | `blaze-link/src/place.rs:54` `patch_room` (the code's size, 64 MiB to 1 GiB, per patch region); `lib.rs:200` `Refusal::NoRoom`, `:227` `NoThunk`; [patch-in-place.md](patch-in-place.md) | New code goes into room the image kept, never where old code was. A change with no room, or an AArch64 branch out of reach, is a link from scratch. Promotion of a whole program fills the room ([tiers.md](tiers.md), "Promotion"). |
| The dev run | `blaze-inproc/src/lib.rs:1-70`; `library/blaze_inproc_rt/src/sys/macos.rs:1-16` | The program is already its own process, started by the kernel from a linked file. A start hook lays the daemon's memory object over the image: writable in the daemon, executable in the run. The daemon holds every dev program's code in its own memory. On macOS the pages are unsigned and run only without `CS_ENFORCEMENT` (`check_unsigned_code`, `:642`). |
| The live runtime | `blaze-live/src/slots.rs:1-22`, `slots/switch.rs:1-40`; stubs `blaze-link/src/table.rs:50-110` | A call loads a cell, then a slot of the table it names (two dependent loads, 16 or 24 bytes of stub). A patch stores one cell, so it is atomic across slots; 64 generations and a pool of tables are kept for that. Only a live image's members are called through slots (`blaze-codegen/src/plan.rs:492` `patchable = live && member`); every other call is `Target::Direct` (`blaze-codegen/src/lib.rs:438`; `:436` is `Target::Function`, the call through a slot). |
| Fences | `library/blaze_inproc_rt/src/fence.rs:1-29`, `runtime.rs:354` `quiesce`; `blaze-inproc/src/lib.rs:515` `retire`, `:528` `collect` | A thread at a quiescent point loads the epoch, walks at most 512 frames and reports the oldest retired range it returns into. Sound, and the base of this design. A thread that never reports holds every range (`tests.rs:871`); only `Live::poll` reports (`slots.rs:329`); a forking process stops all collection (`lib.rs:533`). |
| Unwind tables | `blaze-live/src/slots/switch.rs:74-84`, `:329-335` `Lasting`; `slots/linux.rs:247`, `macos.rs:90`, `windows.rs:250` | `__register_frame` (a run of entries, or one FDE on Apple), `RtlAddFunctionTable`. Never taken back, so patched code is never freed. Nothing names a program counter: no perf map, no GDB JIT interface anywhere in the tree. |
| `blaze_lazy` | `blaze-lazy/src/lib.rs:1-31`, `stub.rs:8-44`, `mode.rs:69` `allow`; `blaze-link/src/lazy.rs:359` | A stub a function, one round trip on first call, single flight in the daemon. Dormant: blaze-cargo uses only `allow` (`serve/unit/run.rs:106`), and answers an ask with an error (`serve/unit/run/inproc.rs:835`). Code with call frame information is refused, so nothing that can unwind is lazy. |
| The second tier ladder | `blaze-stencil/src/tier.rs:69` `Tier`; `blaze-codegen/src/driver.rs:3610` `Build::tier_up`; `plan.rs:287` `Choice::then` | Count-driven tier-up of in-process runs. No path of blaze-cargo runs it. |
| Statics | `hir-ty-codegen/target.rs:736` `addressing`; `blaze-stencil/src/arch/x64/inst.rs:136-163`, `arch/aarch64/glue.rs:638-668`; `hir-ty-codegen/llvm/lower.rs:2400` | Tier 0 reaches every static through the GOT on ELF and Mach-O; LLVM reaches a colocated one PC-relative. Thread-locals are general-dynamic or a TLV call. LIR has `func_addr`, `data_addr` and `tls_addr` (`blaze-lir/src/op.rs:336`, `:338`, `:340`) and no backend that lowers them: no crate depends on blaze-lir yet. No domain, no instance base, no pinned register. |
| Tests | `blaze-usercode/src/test/queue.rs:4-34`, `fork_server.rs:11`; `blaze-testsel/src/lib.rs:9`; `library/blaze_inproc_rt/src/live/log.rs:1-22`; `blaze-usercode/src/trace.rs:37-59` | A process a test, forked from a fork server. A pass is kept under the functions the test executed (a byte a slot, armed in the forked child) and the values it read. The trace has reads, stats, listings, env reads and refusals; no cwd, no signal, no descriptor effect. |

So the task's premise needs one correction: today's path does not write over a running body. It
fails in other ways: a body that outgrows the room is a relink; the room is in every dev file;
only workspace members switch; unwind entries pin code forever; a call pays two loads; the
daemon holds the code; lazy code cannot unwind. Relax (`/tmp/relax-src`, read only) runs the
program inside the compiler. Every decision below keeps the program in its own process.

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
| the slot's stub (`adr x16, .; b head`, 8 bytes; `lea r11, [rip - 7]; jmp head`, 16 on x86-64; written when the slot is declared) | not called yet: the resolver saves the argument registers, asks the control thread for the slot, which asks the daemon (`Need`), and jumps *through the word again*, never to an address it was handed |
| a tier 0 body | after the first call, and again when an edit takes away an optimized body that built the edited function in |
| an optimized body | when promotion ([tiers.md](tiers.md)) lands it |
| a gate entry | for the length of one transaction's commit (section 4) |

Rejected: a direct branch a function (`bl island; island: b body`, the island rewritten). It
saves the load and costs a second taken branch, and on AArch64 it needs every body within 128
MiB of its island (spans of the area with a replica of the table each, veneers where one does
not reach), an instruction-cache flush and, on macOS, a write to JIT memory for every switch.
A word is data: one aligned store, any distance, the same on every target. Rejected: today's
cell and table (two loads; a pool of tables); a patch of each call site (not atomic, and unbounded).

**Code is appended.** The area is address space reserved once, in one mapping with the table:
`| words | stubs | entry thunks | code area |`, 2 GiB in all (nothing is committed until
written). The bound is the call's own: it loads its word relative to itself, and x86-64
reaches 2 GiB (`rel32`), AArch64 4 GiB (`adrp`); with the table at the mapping's start and the
mapping no longer than that, every body reaches every word and there is one table. (An
earlier draft said 64 GiB, which no call could reach across. And on macOS a `MAP_JIT` mapping
cannot be put at a chosen address beside another, so the words lie in the code's mapping and
a store to one is made under the control thread's write switch, as a gate's flag is.) At a
few hundred bytes a tier 0 body, 2 GiB is millions of edits; then the restart in place. A body goes at the end, is hashed against its record's content id, and only
then can a word name it. **An offset is handed out once and never again**: a program counter
names one body for the life of the process, so a profile sample, a backtrace or an unwind
lookup taken at any time is still right later. What comes back is memory, not addresses: a
whole page all of whose bodies are freed is given to the OS (a hole punched in the memfd on
Linux, where `MADV_DONTNEED` would not free a shared mapping's pages; `MADV_FREE_REUSABLE` on
macOS). An area with no room left is a restart in place that says so.
Rejected: a free list of ranges (today's arena): it saves address space, which is not scarce.

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
(a thread blocked in `read` holds every edit's code for ever: today); a count of activations
a body (a store on every call and return).

## 2. The process

```
blaze run          daemon                              the program's process
   |---request-----> | builds main's tier 0 code            |
   |                 |---spawn (sandbox)------------------> launcher (blaze's, signed once)
   |                 | <====== Unix socket: blaze_launch::wire, deterministic CBOR ======>
   |                 |  Welcome{slots, statics, store}      maps the table, the area, domain 0
   |                 |  Edit{binds} (the last run's set)    appends, commits, jumps to main
   |                 | <-- Need{slot} ------------------    a stub fired
   |                 |  Edit{..} / Refused{diagnostic}      appends and binds, or stops
```

- **The launcher is the process.** The kernel starts it; it never forks the program. So a
  crash is the program's alone, the sandbox is the one the daemon spawned it in, signals and
  `ptrace` are real, and the daemon holds none of the program's code. It is a `no_std` binary
  on bstd: the core (`blaze-launch`), a driver a platform, the resolver, gates and thunks as
  byte templates.
- **No link.** The daemon names code records by content id; the launcher reads them from the
  store (a read-only directory capability the daemon passes), copies each into the area,
  applies the record's fixups (slot words, domain offsets, its own constants), checks the hash,
  and commits. Imports from system libraries are bound with `dlsym` as records name them.
- **The file at cargo's path** is a clone of the launcher with the program's id in a sidecar
  (`<exe>.blaze-program`). Run by hand, or by a test through `CARGO_BIN_EXE_*`, it finds the
  daemon and asks for the program; with no daemon, sidecar or store it stops, naming which
  (`no-fallback`). `current_exe` is cargo's path, as now.
- **The linker stays** for `max` and for a file that must stand alone (`blaze build
  --standalone` under `edit` links tier 0 code, without patch room). Rejected: keeping the
  daemon's memory object (it gave W^X for nothing, but the daemon pays every program's code
  and a remote daemon cannot do it).
- **The control thread** is the launcher's one thread that mutates: it reads the socket, runs
  the core, writes code and words. A program thread only loads words and reports.

**macOS.** The area is `mmap(MAP_JIT)`. The control thread alone ever calls
`pthread_jit_write_protect_np(0)`: the switch is per thread, so program threads keep running
and never see the area writable. After a write, `sys_icache_invalidate`. The launcher's Mach-O
is signed once at blaze's release with `com.apple.security.cs.allow-jit`, so it runs under the
hardened runtime too; today's unsigned pages cannot. Why not patch a signed Mach-O's own pages:
each page of its text is checked against the code directory's hash when it is paged in, a
page written in memory no longer matches, and on arm64 running it kills the process (`Code
Signature Invalid`) unless it is being debugged; and patching the file means hashing and
signing again (`blaze-link/src/macho/sign.rs:112`, today's cost). Under `edit` none of the
program's code is in a Mach-O. Slot words and domain images are plain data.

**Linux.** The area is a `memfd`, mapped twice: executable where code runs, writable at an
address only the control thread uses. No `mprotect`, and no anonymous writable-executable
memory (SELinux `execmem`). After a write on AArch64: `__clear_cache`, then
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
  blaze's std builds, Windows through `RtlInstallFunctionTableCallback`. Lazy code unwinds
  (today refused). std's backtrace names a frame by asking the dev runtime: the index gives
  the record, whose name and lines come from the store.
- **A debugger** gets the GDB JIT interface (`__jit_debug_descriptor`): one object a commit,
  made by the daemon from the records' debug info, and only once a tracer is attached
  (`TracerPid`; `P_TRACED`).
- **A profiler.** Linux: `/tmp/perf-<pid>.map` appended a commit. blaze's own profiler asks
  for the index over the socket. Because offsets are never reused, a sample taken before an
  edit still names its function.

## 4. An edit is one transaction

Checking stays eager: under `edit` the daemon type- and borrow-checks the whole workspace in
the background, the build's status is not `ok` until that is done, and `run` starts as soon as
the code it needs first is there. A stub that fires on a function with an error stops the
program with the diagnostic (`Refused`), not a trap.

An edit: (1) the daemon checks the whole workspace with it; any error and nothing is sent, the
program runs on. (2) It classifies each changed item. (3) It compiles tier 0 code for every
changed function *that is bound* (an unbound slot stays a stub), plus the tier 0 code of every
bound optimized body that built a changed function in (its trace, tiers.md). (4) `Edit{txn,
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
terminal, sandbox and debugger; the daemon has the new code ready before it says `Restart`.

## 5. Domains

A *domain* is one instance of the program's mutable statics. Code addresses a mutable static
as `domain base + offset`; the base is one word of the thread's block at a fixed offset from
the thread pointer (`fs:[N]`; `tpidr_el0 + N`; on macOS `tpidrro_el0` and a reserved TSD key
(private, `macos-private-apis`)), so a static's address is one load and an add. A static that
is immutable (no interior mutability) stays shared and PC-relative.

- **The image.** Each record of a static gives its initial bytes and the fixups in them
  (`Init::Bytes { relocs }`, `blaze-codegen/src/lib.rs:329`). The launcher keeps the initial
  image as a `memfd`; a domain is a private copy-on-write mapping of it with the pointers
  that point into the image rebased, so starting one costs the pages it touches. A const that
  points at a mutable static becomes domain data (as Relax's `rehome_const`).
- **Thread-locals** are a second word: the thread's block for its current domain, allocated
  on first touch, its destructors run when the thread leaves the domain or ends.
- **Threads.** std's thread start (blaze builds std) copies the parent's base. A domain ends
  when its entry returns and its threads have ended; its image is unmapped then.
- **What codegen changes.** Tier 0: the `symbol_value` glue (`arch/x64/inst.rs:136`,
  `arch/aarch64/glue.rs:638`) emits the base load and a new `DomainOff32` fixup in place of
  the GOT load; `tls_value` the second word. LLVM: `lower.rs:2400` lowers a mutable global to
  the same two instructions, the base marked invariant in a function. LIR's `data_addr` and
  `tls_addr` lower so from the start. Under `max` nothing changes: the linker resolves a
  static absolutely.
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

A stub fires once a process, so with domains the dependency set is taken a domain: under
`blaze test` every body's entry sets one byte of its domain (`base + mark + slot`). A test's
*set* is its marked slots, each widened by the bodies its code built in. The result is stored
under (test identity, toolchain) as entries of `{set: [(function, code id)], reads: [(what,
hash)], effects, outcome}`; an entry is reused when every function's code id and every read
is as recorded (the same trace shape as optimized code in tiers.md), so it is shared across
machines by code identity. A save reruns the tests whose set holds a function whose code id
moved; the daemon keeps `function -> tests` to find them. This replaces the armed log stubs
(`live/log.rs`) and the fork a test.

## 7. Order of building

| # | Step | Check | Deleted when it lands |
|---|---|---|---|
| 1 | `crates/blaze-launch`: the core (table, area, batches, staged and gated commit) and `wire` | **built**: `src/tests/sim.rs` (the invariants and their planted bugs), `src/tests/wire.rs` | |
| 2 | Drivers: map the area and table, write, publish, the control thread, the resolver, gates and thunks, the walk; Linux x86-64 and AArch64, macOS | **built** (`crates/blaze-launch-rt/src/tests.rs`): hand-made bodies called through slots, a lazy bind over a socket, a gated edit under a running thread, a batch held by a silent thread and freed when it ends, a page given back; a test that each macOS symbol resolves | |
| 2b | The scan of a thread from outside (`thread_suspend` and `thread_get_state`; Linux's one real-time signal), so a thread that never reaches a safepoint does not hold every batch | a thread blocked in `read` while an edit lands: its batch is freed | |
| 3 | Records as the launcher takes them: slot-word calls, entry thunks, the unwind entry and name in the record; the index and the unwind hooks | **built**, but for names in a backtrace: a panic unwinds through lazily bound and replaced bodies ("Step 3 as built") | `blaze-link/src/lazy.rs`'s refusal of unwinding code (not yet: see the end) |
| 4 | `blaze run` under `edit` through the launcher: `Welcome`, `Need`, prefetch of the last run's set; eager check in the background | **built behind `BLAZE_LAUNCH=1`** for a binary, with the whole program still compiled first: hello world on macOS and Linux arm64 ("Step 4 as built"); reth not tried | `blaze-lazy`'s stubs and wire; `blaze-inproc`'s code memory for runs (not yet: see the end) |
| 5 | Edits: the classifier, `Probe`, the gated `Edit`, restart in place | **first version built** (one changed body live, else restart in place; no `Probe`): the DST of the daemon's side, and a loop program edited 22 times in one process ("Step 5 as built"); reth and tempo not tried | `blaze-live` (cell, pooled tables, 64 generations); patch room and the dev splice for `edit` builds; the Mach-O re-sign of a dev patch; fences |
| 6 | Domains: `DomainOff32`, images, thread-locals, std's thread start, the shims | each test of a real suite in a domain equals its own process's outcome | the fork server a test; `live/log.rs` |
| 7 | Selection by marks and traces | a save reruns exactly the tests whose set changed (`blaze-testsel`'s oracle) | `blaze-testsel`'s reached-set path |
| 8 | Promotion in band 3 and its retreat on an edit | tiers.md's DST | `blaze_stencil::tier`, `Mode::Tiered`, `Choice::then`, `Build::tier_up` |

### What the patch room costs on disk (measured 2026-11-11, lane disk-1111)

Hello-world's `edit` executable on arm64 macOS is 158.5 MB long and holds 23.2 MB on disk:
`__TEXT` is 83.67 MiB (19.7 MiB of code and its patch region), `__DATA` 64.73 MiB (0.7 MiB and
its patch region), `__LINKEDIT` 2.57 MiB. The two patch regions are 128 MiB of the file, all
holes. Each is sized by `blaze-link/src/place.rs` as the image's own code (or data) clamped to
`PATCH` (64 MiB) below and `PATCH_MAX` (1 GiB) above, so for any program under 64 MiB of code
the room is a constant 128 MiB whatever the program is, and past that it doubles the file. The
holes cost nothing on APFS, ext4 or btrfs, and everything where a file is copied without them:
`cp` without `-c`, an archive, `rsync` without `--sparse`, a network file system, an upload.
The build directory holds the file twice (`debug/hello` and `debug/build/hello/<id>/out/hello`,
each its own 23.2 MB on disk, and each `.dSYM` its own 8.1 MB), and the store's
`placed/*.derived` files are clones of it (450 MB as `du` counts them, 9.6 MB of their own).

Does `edit` still need the room once functions are reached through entry slots? No: from step
4 a body is bound into the launcher's area in the running process and from step 5 an edit
replaces a slot's word, so no edit writes new code or data into the executable, and step 5's
row deletes the patch room and the dev splice for `edit` builds. Until step 5 lands the room
is what an in-place patch writes into, so it stays; `max` builds' live images keep theirs.
When step 5 deletes it, the check to add is the file's length: hello-world's `edit`
executable is under 25 MB long, not 158 MB, and the build directory holds it once.

## Step 1 as built

`crates/blaze-launch` (`no_std`, on `core` and `alloc`; on bstd once rustc accepts bstd or blaze
alone builds blaze again; `#![blaze::sans_io]`): `Launch` owns the table
(`Slot -> Stub | Body`), the area (bump, never reused), the batches and the threads. `stage`
validates and appends or changes nothing; `commit` returns the ordered `Write`s for a driver
and makes the batch; `abort` frees what was staged; `report` takes a thread's frames (in any
order: what a thread cleared stays cleared) and `collect` frees what every thread cleared, as
`Write::Forget` and whole pages as `Write::Decommit`. The simulation's machine is the
driver's contract written down: step 2 implements exactly it.
`wire` is the schema (`SCHEMA = 1`): frames as blaze-protocol's, bodies in its serde shape
(a struct a map keyed by field names in canonical order), written and read by hand without an
allocator, and checked against `blaze_protocol::cbor::check`.

## Step 2 as built

`crates/blaze-launch-rt` (on `std`, as blaze-launch is on `core`: not bstd until rustc accepts
bstd). The simulation's machine, made real:

- `sys::Code`: one mapping. macOS: `mmap(MAP_JIT)`, written only inside
  `pthread_jit_write_protect_np(0)` on the calling thread, then `sys_icache_invalidate`. Linux:
  a memfd mapped read-execute and read-write; on AArch64 `dc cvau`/`ic ivau` and
  `membarrier(PRIVATE_EXPEDITED_SYNC_CORE)` after a write. **Never unmapped** (no `Drop`): an
  address in it is good for the life of the process.
- `arch`: byte templates. A gate entry is 40 bytes on both instruction sets (`ldr x16, flag;
  cbnz; ldr x16, old; br x16; ldr x16, new; br x16; .quad old, new` / `cmp qword [rip+flag], 0;
  jne; jmp [rip+old]; jmp [rip+new]`); the stub, its head and the resolver (a naked function
  that saves x0-x8 and q0-q7, or rdi, rsi, rdx, rcx, r8, r9, rax, r10 and xmm0-7, calls
  `table::resolve`, and jumps through the word); the entry thunk.
- `table::Shared`: what a program thread touches. `birth` (the parent loads the epoch and
  announces, then starts the thread), `Mutator::safepoint` (when a batch waits: load the
  epoch, walk the thread's own stack by frame records, report), `resolve` (a stub's wait).
- `drive::Driver`: the core plus the mapping; carries out each `Write` in order; `edit` is
  stage, fill (from `Records`, the seam where step 3's store reader goes), commit, or abort.
- `control::run`: the control thread. One queue for everything (a thread's birth, report and
  end; a stub's need; a frame from the reader thread; the socket's end), so the births are
  drained before each collect by construction. One `Need` a slot however many threads wait.
  A failure is kept, answers every waiting and later stub `Lost` (the program stops, naming
  it), and is returned when the loop is stopped.

What it does not do, each a refusal or a note in the code, none a quiet path: no scan from
outside (row 2b); `Write::Forget` takes nothing back, since no unwind or symbol entry is kept
yet; the walk needs frame records (always there on AArch64 macOS; tier 0 code keeps them,
`blaze-stencil/src/arch/x64/frame.rs:570`, `arch/aarch64/frame.rs:326`), and step 3's unwind
entries replace it; the resolver has no unwind entry, so a panic cannot unwind through a
stub that is resolving; `Probe` and `Restart` end in `NotBuilt`; the launcher's waits are
named in comments, not spans, because the launcher has no profile yet.

## Step 3 as built (2026-11-12, lane launch3-1111)

### What tier 0 emits for the launcher

`Spec::slots` (`blaze-codegen/src/settings.rs`; in the settings, so in the key) is set by
`plan::Mode::Launched` for every function of the program, whoever wrote it: members,
dependencies and std all start as tier 0 code, each with a slot (`Choice::then` keeps the
optimized spec promotion will land). Under it (`blaze_stencil::Target::slots`):

| | x86-64 | AArch64 |
|---|---|---|
| call | `call qword ptr [rip + word]` (`ff 15`, `X86GOTPCRel4`) | `adrp x16, word; ldr x16, [x16, lo12]; blr x16` (the GOT relocations) |
| tail call | `jmp qword ptr [rip + word]` (`ff 25`) | the same into `x17`, then `br x17` |
| a colocated function's address | `lea reg, [rip + thunk]` (`X86PCRel4`) | `adrp reg, thunk; add reg, reg, lo12` |

No new relocation name was needed, and `colocate` (`hir-ty-codegen/v2/backend.rs`) is
unchanged: the glue ignores `colocated` for a call under `slots` and takes a colocated
function's address PC-relative. So a GOT relocation against a function is always a call
(its slot word) and any other is its address (its entry thunk), and the launcher reads them
apart by kind. Tests: `under_slots_a_call_loads_the_slot_word_and_an_address_is_the_thunk`
(both instruction sets, blaze-stencil), `a_launched_programs_functions_all_start_as_tier_zero_through_slots`
(blaze-codegen).

### The record, and how each thing it names is resolved at install

`blaze_launch::record`: one function's code or one static's value, position- and
process-independent, in deterministic CBOR; its id is the BLAKE3 hash of its bytes
(round-trip test `every_record_is_deterministic_cbor_and_reads_back`, with its control). It
is derived by the daemon from the compiled function of `code(f, tier 0)`
(`blaze-launch-host/src/record.rs`); a fixup names what it is against by *place*, and the
bind gives each function place its slot and each static place its number, so a callee's body
edit changes no caller's record.

| a fixup against | through the GOT | any other kind |
|---|---|---|
| a function | its slot word | its entry thunk |
| a static, an outside symbol | a cell of the record's own pool, after its code, that holds the address | the address |
| the record's own data | a pool cell | the datum's address |
| a thread-local static | the pool's descriptor (Mach-O: a cell, then thunk, argument, zero; ELF: the descriptor pair), whose thunk is the launcher's | refused |
| the code itself | a pool cell | the placed code |

- **Constants** (strings, vtables, promoted constants, a position's `Location` and its file
  name): placed once a process by the datum's id in the data mapping (`sys::Data`, 4 GiB
  reserved, never unmapped) and never freed, since the program may keep an address past the
  body that named it.
- **Statics**: defined by the daemon before the program starts (`Tell::Statics`: every
  static's memory first, then each value, so statics may name one another), at one address
  for the life of the process. Tier 0 code takes every data address from its GOT, which here
  is the record's pool, so the data mapping may lie anywhere.
- **Thread-locals**: a number a static; each thread keeps a table of its own copies off one
  pthread key, a copy made on first touch (`blaze-launch-rt/src/tls.rs`). AArch64 only;
  x86-64 is refused by name.
- **Outside symbols**: a name the program itself defines (`#[no_mangle]`) is its function's
  slot; any other is `dlsym` of the process, asked once, and a refusal naming it when no
  library has it.
- **Unwind information**: the record's CIE and FDE instructions made an `.eh_frame` entry
  with absolute addresses, the personality routine its entry thunk, registered
  (`__register_frame`) before a word names the body and taken back when the core frees it.
  A panic unwinds through lazily bound and replaced bodies.
- **Positions** are data of the record, so a record follows the lines of what it names: a
  bound function below an edit in its file is bound again, compiled by no one.

Records come over the socket (`Tell::Record`), not from a directory of the store: a record
is a few hundred bytes the daemon has when it answers, and the launcher needs no reader of
the store's packs. `wire` is schema 2 (`Start`, `Record`, `Statics`, `Unheld`, a bind's
`refs`).

Test of the whole of step 3 on a real machine:
`tier_zero_code_runs_in_the_launcher_compiled_on_first_call_and_edited_while_it_runs`
(blaze-stencil): real tier 0 code, compiled when its stub asks, placed, called through slot
words, a thunk, a static, `labs`, a thread-local on two threads, a panic through placed code,
a body edit under a calling thread, the old body freed. 20 of 20 runs on macOS arm64 and on
Linux arm64 (where blaze-launch-rt's own tests ran for the first time: 8 of 8, 20 of 20).

## Step 4 as built: `blaze run` under the launcher

With `BLAZE_LAUNCH=1` in the daemon's environment a binary built for `edit` is not linked
(`serve/unit/run/launch.rs`; the switch stays until what is listed under "not built" below
is, when it becomes what `edit` means). The file at cargo's path is `blaze-launcher`'s
bytes (858 KB on macOS, 944 KB on Linux, where the dev executable is 158 MB long) and a
sidecar beside it (`hello.blaze-program`: the daemon's launch socket and the program's id),
both placed by the program's first build only. `blaze run` execs the file as before; run by
hand it works the same.

The launcher (`crates/blaze-launcher`) connects, says `Hello`, and is told `Welcome` (the
slots), the statics, the functions the runs before it called (one edit, before it starts),
and `Start`; it runs the program's initializers (`.init_array`, `__mod_init_func`) and its
`main(argc, argv)` on the process's first thread and ends through `exit`. The program's
threads are started through the launcher's `pthread_create`, which tells the core of each.
Checked by hand on macOS (`/private/tmp/blaze-proc-launch`): arguments, environment,
`current_exe`, stdin from a pipe, exit status 7, a thread, `catch_unwind`, a panic (status
101, `src/main.rs:4:17`), `abort` (134), SIGTERM (143).

The daemon's side is `crates/blaze-launch-host` (sans-IO): slots by identity, what each
launcher has bound, the prefetch. blaze-cargo owns the socket, an accept thread and a reader
and a writer thread a launcher.

A program thread talks to the control thread through a queue and pthread condition
variables (`sys::Handoff`), never a channel: a stub can fire while its thread is ending,
after the launcher's own thread-locals for it are gone, and a channel's send and receive
need one. A stub that fires in a forked child stops the child, saying so (the child has no
control thread; `Command`'s children run, since nothing they call before `exec` is new).

**Not what the plan says, each stated:**

- The build still compiles every function before the program starts
  (`Build::run_with_statics`, `serve/unit/codegen.rs`); a `Need` is answered from the
  build's functions. So "compiled on first call" is true of the launcher (a function has no
  code in the process until it is called) and not of the daemon: `Later::compile_kept`
  (`blaze-codegen/src/driver/later.rs`) is still uncalled, because answering a `Need` with
  it needs the front end's database after the job that built the program ended. The cold
  run is therefore no faster than today's (below).
- A launched program's unit is replayed from its record only while the daemon's program
  holds the code of the build that record is of (`launch::not_replayed`: the program keeps
  the whole record key of the build it took, and a replay is taken when the unit's key now
  is that one). The record names the launcher's file, the same for every build, so the
  file says nothing of whose code the daemon holds; the key does. Any other build of the
  unit is made, and given to the program. Until 5226c3d363 no record of a launched unit was
  replayed, which kept every no-op `run` out of the no-op lane and, once other projects had
  been built, walked the whole program again (12 s).
- Test binaries stay dev executables.
- `RUST_BACKTRACE` names the program's frames on Linux (written 2026-11-12, case
  `a_panic_under_the_launcher_names_its_frames`, not yet run): the program's
  `dl_iterate_phdr` is the launcher's (`blaze-launch-rt/src/symbols.rs`), which reports the
  code mapping as one more image whose file, a memfd, is an ELF symbol table of the placed
  bodies; std's symbolizer reads it as it reads any image's. Not given: a file and a line
  a frame (no DWARF line table is made; the panic's message has its own), a body placed
  after the process's first backtrace (std asks for the images once and keeps the symbols
  it parsed, so a test binary's later panics miss functions first called after its first),
  and macOS (std there asks dyld's image list and reads Mach-O: the same idea through
  `_dyld_image_count` and its three siblings, not written).
- The launcher's file is not an input of the unit's record: a new blaze with another
  launcher places it only for a program it builds first.
- x86-64: the call sequences and the placement are tested as bytes and the launcher's
  crates compile for x86-64 Linux (`cargo check`); nothing of the launcher has run there,
  and thread-locals are refused by name.

## Step 5 as built, first version: a body edit while the program runs

Each build of a launched program is given to `Program::built`, which compares it with the
build before and tells each running launcher (the table in `program.rs`): one changed body
is an edit of the bound slots whose records changed, switched together; lines only is an
ungated edit; an added function is a declared slot; a changed static, or two bodies changed
at once, is `Restart` with the reason, and the launcher `execv`s itself in place (and ends
the process, saying why, if it cannot: it never runs on). "One body" is the sound rule
available without comparing signatures: a function's code hash takes its callees by their
interface, so a changed signature changes its callers' code too. `Probe` is not built.

Simulated (`blaze-launch-host/src/tests.rs`,
`no_schedule_leaves_a_launcher_on_code_of_an_old_build`): builds landing while frames and
needs are in flight; the oracle is that a launcher that carried out everything it was sent
holds the newest build's record in every bound slot or was told to start again; two planted
bugs.

Not built: the scan of a thread from outside and the poll on a back edge, so a thread that
calls no new function reports nothing and a replaced body's memory stays (sound, and a few
hundred bytes an edit); retirement is exercised by the tests, and in a real program only
at a stub.

### Measured (2026-11-12; hello world, and a loop that prints a function's value every millisecond)

Two machines. The Linux arm64 VM (32 cores) ran nothing else: its numbers are the
comparison. The Mac was shared with other lanes (load average 17 to 100 through every run),
so its ranges are the machine's, and the three ways were interleaved there (one of each a
round) to see the same load. Every run checks the program's output and exit status; each
series has one fixed `env -i` environment; medians, with the range. Binaries of e34a3b446f
(today's path is the same code with `BLAZE_LAUNCH` unset).

Linux arm64 VM, quiet:

| | under the launcher | today (dev executable) |
|---|---|---|
| cold `run` to exit, fresh home and daemon (n=15) | 13.0 s (12.4 to 14.1) | 13.3 s (12.8 to 13.8) |
| warm no-op `run` (n=20) | 32.3 ms (30.6 to 56.6) | 13.3 ms (11.8 to 16.8) |
| body edit, then `run`, save to exit (n=20) | 55.8 ms (49.6 to 214.6) | 70.5 ms (63.4 to 153.2) |
| body edit while the program runs, save to its new output (n=20) | 47.9 ms (40.3 to 323.9), of which the build's answer is 47.8; one process through 22 edits | never: 22 of 22 edits not shown within 20 s; a new `run` is the 70.5 ms above, and a new process |

Mac, shared (interleaved rounds, n=20 each; two runs, at load 34 to 56 and at 17 to 38):

| | under the launcher | today | Relax (`relax run --quiet hello`) |
|---|---|---|---|
| warm no-op `run` | 160.4 ms (73.9 to 244.1); 141.8 (79.2 to 967.2) | 120.8 (64.6 to 244.5); 80.4 (35.9 to 633.9) | 86.8 (50.0 to 127.4); 70.9 (32.8 to 122.9) |
| body edit, then `run` | 98.5 ms (64.8 to 318.5); 147.9 (55.0 to 12067.5) | 205.7 (147.7 to 357.2); 162.7 (114.8 to 9542.7) | 160.8 (103.2 to 317.3); 136.6 (51.7 to 218.9) |

Mac, each way alone: under the launcher, cold 10.7 s (8.4 to 30.4, n=15), body edit while
running 76.5 ms (54.6 to 7254.9, n=20; none of 22 missed); today, cold 17.5 s (11.5 to 25.1),
body edit then run 115.8 ms (101.4 to 604.7), and none of 22 edits reached the running
program; Relax, after `relax clean` 396 ms (363 to 536, n=15), warm 49.3 ms (43.3 to 60.8),
body edit then run 92.1 ms (53.0 to 216.4).

What these say, and do not:

- **A body edit reaches a running program**, in the time the build takes to answer (48 ms on
  the quiet machine): no link, no restart, the same process. Today's path and Relax both
  need a new process. The 20 ms aimed for is not met, and what is left is the daemon's edit
  build, not the launcher: the new output is seen within the tick (1 ms) of the answer.
- **Edit then run** is faster than today's on both machines (56 against 71 ms quiet; the
  launcher's best on the Mac is 55 ms where today's is 115 and Relax's 52). On the shared
  Mac the medians of the three overlap: nothing here shows blaze ahead of Relax there.
- **The warm no-op `run` is slower than today's** (32 against 13 ms quiet): the launcher
  connects, is told its statics and the last run's functions, and places them, where a
  linked file just starts. Under 100 ms on the quiet machine, not on the loaded Mac.
- **The cold run is what it was**: the whole program is still compiled first.
- Found by the output checks while measuring, and fixed: a replayed record left the
  program on another build's code (now `not_replayed`, by the record's key); every build of
  a launched program put the launcher's 858 KB in the store (a body edit then run was 910 ms
  at the median until then).
- **Every launcher number of this table is 6 to 8 ms too high a request.** The series had
  `BLAZE_LAUNCH=1` in every command's environment, and any `BLAZE_*` variable but
  `BLAZE_HOME` sends the thin client to `blaze-engine` (`crates/blaze-thin/src/route.rs`).
  The switch is read by the daemon alone: it belongs on the command that starts the daemon.
  The table below is measured so.

### Measured again (2026-11-12, tree b2da8a9186; the VM, restarted before the series)

The switch on the daemon-starting command only; launcher and today's path interleaved a
round; output and exit status checked; one fixed `env -i` environment a series.

| | under the launcher | today (dev executable) |
|---|---|---|
| warm no-op `run` (n=20) | 15.1 ms (10.4 to 17.2) | 12.1 ms (9.6 to 26.5) |
| body edit, then `run` (n=20) | 44.1 ms (41.1 to 122.3) | 62.3 ms (55.6 to 175.3) |
| body edit while the program runs, save to new output (n=20) | 30.0 ms (26.5 to 589.0); one process through 22 edits | never (as above) |
| the program alone, exec to exit (n=55, tree 5226c3d363) | 2.84 ms (2.70 to 3.04) | 1.10 ms (1.02 to 1.22) |
| the program's file | 883 KB | 158 MB |

What moved the no-op from 32 to 15 ms: the routing above; the keyed replay; the daemon
keeps its answer to `Hello` for a build (641 KB of frames, 0.8 to 1.0 ms to make each run);
slots are declared as records first name them (`Welcome` declared every identity of the
program); the launcher writes an edit's records with one `membarrier` and reads a socket
read's frames with one ring. **It is not at parity**: what is left (about 1.7 ms of the
program's 2.84) is carrying, hashing and placing 641 KB in 210 records each run. Parity
needs the placed code kept across runs (a file of the area the launcher maps, checked
against the daemon's ids); not built.

The live edit's 30 ms, by the daemon's spans (medians of the 20 edit builds): `plan` 3.0 ms,
`lower workspace` 1.65, the binary's `unit names` 1.7 to 2.8, `unit codegen` 1.6, `unit
link` 4.0 (all of it `launch link`: the build's span tables 1.7, the program taking the
build 2.0, which was 3.5 until b2da8a9186), `unit store` 1.1. A launched build has no file
link, no placement after the first build and no debug side file: `unit link` is the
program's intake alone. The span tables are still made (`HirFrontend::link_inputs`: the
records read them); the record is still stored (the keyed replay reads it). What the spans
do not name: between `lower workspace` ending and the binary's `unit names` starting, 5 to
11 ms pass in which the workers' spans add to under 1 ms. That stretch is the daemon's
loop (`serve/job.rs`), outside this lane's files, and is the largest part of the edit.

The revision driver holds this: `a_body_edit_reaches_a_running_program`
(`crates/blaze-engine/tests/revisions`; steps `start`, `await MS TEXT`, `count TIMES TEXT`,
`end`) keeps one program running through two body edits, bounds the save-to-output time and
counts its starts; without the switch it fails at the first `await`.

## Placed code kept across runs (written 2026-11-12; not yet compiled or run)

A launcher that starts is sent its statics and the functions the runs before it called,
641 KB in 210 records for hello world, and hashes, decodes and places each: about 1.7 ms of
the program's 2.84 ms, the same work every run of one build. All of it is a function of
what the daemon sends. So the daemon does it once and keeps the outcome, an **image**
(`blaze-launch/src/image.rs`):

- **What it is.** The bytes of the code area from its first offset and of the program's
  data, placed against the addresses of no process (the code mapping at 0, the data at
  2^44); the list of their words that hold an absolute address, each with what it is an
  address of (the code mapping, the data, an outside symbol by name, the thread-local
  thunk, a thread-local); and the tables a launcher keeps of what it placed (each body's
  slot, range, code identity, name and unwind entry; each datum by id; each static by
  number). Placed code is position-independent but for those words: a body reaches a slot's
  word, an entry thunk and its own pool relative to itself, all in the launcher's one
  mapping, and anything else through a pool cell. `place::absolutes` names the cells; a
  fixup relative to something outside the mapping is refused, and then there is no image.
- **Who makes it, and where it lives.** The daemon (`blaze-launch-host`, `start_image`),
  with the launcher's own placement code (`blaze_launch::place`, pure), from its own
  records, when a launcher says `Hello` and no kept answer is of this build and this set
  of functions: the first run after a build too. It is kept beside that answer, in the
  daemon's memory, keyed by the build, the set and the launcher's system and page size,
  and every later launcher is sent the same bytes. It is not a file: a launched program
  does not outlive its daemon's code (`Tell::Unheld`), so nothing would read one. When
  programs replay after a daemon restart, the image becomes a store record under the
  build's record key, the launcher binary's key and the set.
- **How it reaches the launcher.** One message, `Tell::Image { bytes }` (schema 3), in
  place of `Welcome`, the records, `Statics`, the first `Edit` and `Start`.
- **What the launcher verifies before using it** (`Driver::image`, `Loaded::image`): the
  layout version; its instruction set, system and page size; that the entry thunks' and
  the area's offsets in the mapping are its own; that it has declared and placed nothing;
  that its core hands every body the range the image has it at; that every outside symbol
  is in the process; every offset in the image's tables against the bytes it is into
  (`Image::decode`). It does not hash the bytes: they arrive on the socket every record
  arrives on, from the daemon that made them from its records. Any failure stops the
  program by name; a launcher is never sent an image and then the records.
- **What it then does.** Declares the slots, fills the data with one copy, resolves each
  outside symbol once, adds each mapping's address to the listed words, copies the code
  with one write and one barrier, registers the unwind entries, and commits as an edit of
  every body does.
- **It is the one way a launcher starts** (rule `no-fallback`, as rewritten 2026-11-12:
  a failed launch is better than one that took another route). The daemon no longer sends
  a start as `Welcome`, records, `Statics`, an `Edit` and `Start`: that route is deleted
  from `blaze-launch-host`. Where a program has something an image cannot hold, the
  launcher's `Hello` is refused and the program does not start: the error names the case
  (`image::Why`), the function or static, and what to do (run the program without the
  launcher: `blaze stop`, then build with `BLAZE_LAUNCH` unset). Records still come one at
  a time for what an image is not of: a first call (`Need`), an edit, a static added by an
  edit.

Each case is work to remove, with what it needs:

| Case | Refused when | To remove it |
|---|---|---|
| aligned past a page | a static or datum is aligned to more than the page size | the launcher reserves its data at an address aligned to the largest alignment the image names (one more header field; over-reserve and trim) |
| a thread-local here | a thread-local static on x86-64 | the launcher's thread-locals on x86-64 (`tls`: the descriptor call sequence; launcher.md step 3 lists it) |
| code relative to data | a function reaches a static, a datum or an outside symbol relative to itself | nothing in tier 0 emits this (every data address is taken from the GOT); if a backend does, either a pool cell for it or the data inside the code mapping's reach, as one reservation with a fixed distance |
| longer than a message | more than 12 MiB of code and data in the start | the image by descriptor (a sealed memfd passed with the message, mapped and copied once), which also removes the socket's copies |

Still in the launcher and the wire, though no daemon sends them at a start: `Welcome` and
`Start` (`blaze-launch-rt`'s control thread). The stand-in daemons of `blaze-launch-rt`'s
and `blaze-stencil`'s tests start their launchers with them; they go when those tests
start from images.

Held to: `blaze-launch` `tests/image.rs` (the refinement: an image rebased to a process's
addresses is byte for byte what placing the same records there gives; a control that
leaves the outside symbols' words out; refusals of other layouts), `blaze-launch-rt`
`tests.rs` (a launcher started from an image runs its function and holds its static; an
image of another geometry is refused), `blaze-launch-host` `tests.rs` (a start is one
message that binds what the runs before called; a program an image cannot hold is refused
by name with what to do; the simulation's launchers all start from images).

Not known, to be measured with the phase timings of branch `launch3-1111-probe`: what the
1.7 ms was made of, and what a start from an image costs. Expected to remain, from the
code: the launcher's own start (two threads, a 2 GiB and a 4 GiB reservation, the stub
head), the connect and the round trip of `Hello`, about 700 KB through the socket and
three copies of it (the reader's buffer, the frame, the area and data), one `dlsym` an
outside symbol, one `__register_frame` a body. If the socket's copies show, the image goes
by descriptor (a sealed memfd on Linux, passed with the message) and is copied once.

## More than one changed body, and a changed signature (written 2026-11-12; not yet compiled or run)

Today one changed body is applied to a running program and anything else starts it again
(`blaze-launch-host`, `Program::built`). What it takes to apply more, and one shortcut
that looks right and is not.

**The slot is a path and an ABI** (section 1). A function whose signature changes gets
another slot: its old body stays bound in the old slot for the frames of old callers still
on a stack, the new body is bound in a new slot, and every caller is a changed body that
names the new slot. So with the ABI in a slot's identity, a changed signature needs no
`Probe` and no restart: it is an added function plus its callers' body edits, switched
together through a gate. What is then left to start again for is what it is today for a
static (another value or type) and for the entry's own frame.

**The fingerprint** is what a caller's code assumes of the callee beyond its address:
the calling convention, each argument's and the result's passing mode, and the layout of
every type the signature names, through references and pointers to sized types too (a
`&Foo` argument is read by the callee at `Foo`'s offsets), closures by their captures'
layouts, generics by their arguments. It is a fact of the front end's describe
(`blaze_codegen::backend::Described`, made in `hir-ty-codegen`): one more field, an id,
beside `identity`. The host then keys a slot by `subject` and that id where it keys it by
`subject` now, and the classifier's rule "more than one body" becomes "a body whose slot
identity is the same": every such body is rebound, together when there are several.
The field exists (`Described::abi`, lane describes-1112, carried to `Built::abis` by key;
it hashes the convention, variadic, `#[track_caller]` and each type of the signature by
its identity, which is its layout and interface through every reference and pointer). The
host keys a slot by it (`blaze-launch-host`, `slot_identities`): a build that tells ABIs
has its changed bodies rebound together and a changed signature is another slot; a build
that tells none keeps today's rule, one body or a restart. Held by
`a_changed_signature_is_another_slot_and_changed_bodies_are_one_edit` and its control (a
slot kept across a changed signature). Not covered: the entry's own signature (a restart,
as before) and a static of another type (a restart, as before).

**The shortcut that is not sound.** A function's code hash takes its callees by their
interface, so it is tempting to read the fingerprint off the hashes: "a changed body whose
caller did not change has the interface it had". Take `g`, which builds a `Foo` and hands
`&Foo` to `h`, which hands it on to `f`, which reads it; an edit changes `Foo`'s layout.
`g` and `f` change (each reads the layout); `h` does not (it passes a reference). By the
shortcut `f`'s interface is unchanged, since its caller `h` is unchanged, and the edit is
applied live. A thread that is inside old `g` then calls, through `h`, new `f` with a
reference to an old-layout `Foo`. Today's rule starts the program again for this edit (two
bodies), which is right; the shortcut would not. The fingerprint must name layouts, so it
must come from the front end.

**Test binaries** (step 5 for them) wait on what lane reth-1112 reports is linked whole a
test binary; nothing is written. What is known from this lane's files: `launch::launched`
takes a binary's or an example's unit only, and a test binary's run goes through
`blaze test`'s own runner, not `blaze run`'s exec of the file.

## The lazy path: the seam (design; not built)

Measured on the VM (merged tree 7f20879f6d): a cold `run` is 11.2 s, of which `unit
codegen` is 5.1 s and its `codegen walk` 4.6 s ("described 14233, reached 12071"), the
compiles after the walk about 0.5 s. The first run of a new project in a warm daemon is
11.9 and 12.7 s: 13,873 `codegen describe` spans, 44.5 s of CPU, `mir` 13.7 s. So the cost
of a first run is describing the program, not compiling it, and it is the same describes
for every project (std's). Two things remove it, in this order:

1. **Describes are lookups** (lanes content-keys-1112 and memstrat-1111): a function's
   describe is a store record keyed by its code identity, so a second project, a restart and
   a cold start read std's. Until then a lazy path only moves the describes later.
2. **The frontier walk, served by the kept front end.** Built, bottom-up
   (`blaze-codegen/src/driver/near.rs`, `Build::near` and `Build::compile_near`): one
   function made ready to compile from its own describe and those of what it refers to
   directly, held by a Hegel property to the whole walk's key, unit, places, built-in set
   and code. The file names each pass of the whole walk the frontier form leaves out
   (which references are direct, the recursion limit, symbols defined twice, the front
   end's closed facts, the least origin of a key, dedupe) and why a launched first call
   goes without it.

The seam in the daemon, for whoever builds it (none of it is built; `job.rs`, `unit.rs`
and `linked.rs` are other lanes'):

- **Where the front end is after the answer.** Every build's answer is followed by the
  kept-front-end task (`serve/job.rs`, `Does::StoreKept`: `kept_run_log`, `store_kept`,
  `release_kept(&mut front, wanted)`). It owns the front end alone, as the job's writer
  did, until the checkout's next request sets `wanted`. That task is where needs are
  served: between `store_kept` and `release_kept` it answers `Need`s with `Build::near`
  and `compile_near` against the front end it holds, until `wanted` is set or the
  launcher's program has no launcher attached, and only then releases.
- **Why not a reader.** A `UnitReader` holds `Arc<Lowered>`, and a write waits for every
  reader (`Writer::apply` waits on `Arc::get_mut`; `keep` refuses with a reader alive). A
  reader kept while a program runs would block the next edit: the opposite of the goal.
  The task above holds what the writer held, and gives it back the moment a request wants
  it.
- **What the build does instead of the whole walk.** For a launched unit, `generate`
  compiles the entry and what the start sections name (`Build::run_later`'s set), and the
  whole walk runs behind the program as the eager check (section 4): its refusals (the
  recursion limit, a symbol defined twice, a function whose IR does not build) arrive as a
  diagnostic and a `Refused`, not before `Start`. A need that arrives while a request holds
  the front end waits for that build, which answers it from its own walk.
- **What it holds, and what releases it.** The checkout's lowered front end (what
  `KeptLowered` is today, with its MIR not yet released), for as long as a launcher is
  attached and no request wants it. Footprint decides (rule 50): past the limit's line
  `release_kept` runs whatever is attached, and a later need lowers what it reads again.
- **An edit.** The next request takes the front end as today; its build gives
  `Program::built` the functions the program has bound (their records made by the frontier
  form against the new revision) instead of a whole `Built`. `Program` today reads a whole
  `Built` (identities, owners, statics, names); its intake becomes "these identities
  changed, these were added", which the kept walk's changed set already says.

Not checked, each to be read before building: how the plane settles a job whose
kept-front-end task stays open (today the task ends in microseconds to milliseconds;
`blaze stop`, the idle give-back and a second checkout's admission may all wait for it);
what `Pressure::now().squeezed()` does to a task that is parked on a socket; whether
`store_kept`'s puts may land while needs are served (they are made on the loop after the
task returns today); and a daemon restart, after which no front end is kept and the first
need lowers the checkout (the cold cost again, unless describes are lookups).

### What goes, and why each thing that stays does

Nothing was deleted: the launcher's path is behind `BLAZE_LAUNCH`, and each thing below is
still on the path `edit` takes without it.

| Stays | Until |
|---|---|
| Patch room and the dev splice for `edit` builds (`blaze-link/src/place.rs`) | the switch is the default: test binaries run under the launcher, a program replays after a daemon restart, x86-64 has run. Under the switch no file has patch room. |
| blaze-inproc's host and code memory for dev runs (`serve/unit/run/inproc.rs`) | the same; test binaries are its last users then |
| blaze-lazy's stubs, hook and wire; `blaze-link/src/lazy.rs` | nothing on either path runs them, so they can go now; left because `blaze-link`'s `stage.rs` and `native.rs` name `blaze_lazy::{Session, stub, hook}` in files another lane is editing, and `blaze_lazy::allow` is still what `run.rs` asks |
| `blaze_stencil::tier`, `Mode::Tiered`, `Build::tier_up` | promotion (step 8). `Choice::then` is now what a launched function's optimized spec is kept in |
| blaze-live, fences | step 5 for test binaries |
