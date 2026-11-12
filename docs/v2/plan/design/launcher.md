---
status: plan
summary: "How a program runs under the `edit` goal (D18, D21) - a launcher in the program's own process maps code records from the store behind one slot word a function, appends and never overwrites, retires old bodies by per-thread epochs and a stack scan, takes an edit as one gated transaction, and runs many tests in one process as domains. Built - step 1, `crates/blaze-launch` (the slot table, the append-only area, retirement and the transaction as a sans-IO core with its simulation, and the patch protocol's schema). Not built - everything else; the order is at the end."
date: 2026-11-11
---
# The launcher: running a program under `edit`

## What exists (read 2026-11-11, tree `all-1111`)

| Piece | Where | What it does, and what is wrong with it for `edit` |
|---|---|---|
| Patch room and the splice | `blaze-link/src/place.rs:54` `patch_room` (the code's size, 64 MiB to 1 GiB, per patch region); `lib.rs:200` `Refusal::NoRoom`, `:227` `NoThunk`; [patch-in-place.md](patch-in-place.md) | New code goes into room the image kept, never where old code was. A change with no room, or an AArch64 branch out of reach, is a link from scratch. Promotion of a whole program fills the room ([tiers.md](tiers.md), "Promotion"). |
| The dev run | `blaze-inproc/src/lib.rs:1-70`; `library/blaze_inproc_rt/src/sys/macos.rs:1-16` | The program is already its own process, started by the kernel from a linked file. A start hook lays the daemon's memory object over the image: writable in the daemon, executable in the run. The daemon holds every dev program's code in its own memory. On macOS the pages are unsigned and run only without `CS_ENFORCEMENT` (`check_unsigned_code`, `:642`). |
| The live runtime | `blaze-live/src/slots.rs:1-22`, `slots/switch.rs:1-40`; stubs `blaze-link/src/table.rs:50-110` | A call loads a cell, then a slot of the table it names (two dependent loads, 16 or 24 bytes of stub). A patch stores one cell, so it is atomic across slots; 64 generations and a pool of tables are kept for that. Only a live image's members are called through slots (`blaze-codegen/src/plan.rs:492` `patchable = live && member`); every other call is `Target::Direct` (`blaze-codegen/src/lib.rs:436`). |
| Fences | `library/blaze_inproc_rt/src/fence.rs:1-29`, `runtime.rs:354` `quiesce`; `blaze-inproc/src/lib.rs:515` `retire`, `:528` `collect` | A thread at a quiescent point loads the epoch, walks at most 512 frames and reports the oldest retired range it returns into. Sound, and the base of this design. A thread that never reports holds every range (`tests.rs:871`); only `Live::poll` reports (`slots.rs:329`); a forking process stops all collection (`lib.rs:533`). |
| Unwind tables | `blaze-live/src/slots/switch.rs:74-84`, `:329-335` `Lasting`; `slots/linux.rs:247`, `macos.rs:90`, `windows.rs:250` | `__register_frame` (a run of entries, or one FDE on Apple), `RtlAddFunctionTable`. Never taken back, so patched code is never freed. Nothing names a program counter: no perf map, no GDB JIT interface anywhere in the tree. |
| `blaze_lazy` | `blaze-lazy/src/lib.rs:1-31`, `stub.rs:8-44`, `mode.rs:69` `allow`; `blaze-link/src/lazy.rs:359` | A stub a function, one round trip on first call, single flight in the daemon. Dormant: blaze-cargo uses only `allow`, and answers an ask with an error (`serve/unit/run.rs:41`). Code with call frame information is refused, so nothing that can unwind is lazy. |
| The second tier ladder | `blaze-stencil/src/tier.rs:69` `Tier`; `blaze-codegen/src/driver.rs:3229` `Build::tier_up`; `plan.rs:287` `Choice::then` | Count-driven tier-up of in-process runs. No path of blaze-cargo runs it. |
| Statics | `hir-ty-codegen/target.rs:708` `addressing`; `blaze-stencil/src/arch/x64/inst.rs:136-163`, `arch/aarch64/glue.rs:638-668`; `hir-ty-codegen/llvm/lower.rs:2400` | Tier 0 reaches every static through the GOT on ELF and Mach-O; LLVM reaches a colocated one PC-relative. Thread-locals are general-dynamic or a TLV call. LIR has `data_addr` and `tls_addr` (`blaze-lir/src/op.rs:336`) and no backend that lowers them. No domain, no instance base, no pinned register. |
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
`fn` pointer never names a body. The word is the lazy stub, the patch point and the tier switch:

| The word names | When |
|---|---|
| the slot's stub (`adr x16, .; b resolve`, 8 bytes, written once at start) | not called yet: the resolver saves the argument registers, asks the daemon for the slot (`Need`), and jumps *through the word again*, never to an address it was handed |
| a tier 0 body | after the first call, and again when an edit takes away an optimized body that built the edited function in |
| an optimized body | when promotion ([tiers.md](tiers.md)) lands it |
| a gate entry | for the length of one transaction's commit (section 4) |

Rejected: a direct branch a function (`bl island; island: b body`, the island rewritten). It
saves the load and costs a second taken branch, and on AArch64 it needs every body within 128
MiB of its island (spans of the area with a replica of the table each, veneers where one does
not reach), an instruction-cache flush and, on macOS, a write to JIT memory for every switch.
A word is data: one aligned store, any distance, the same on every target. Rejected: today's
cell and table (two loads; a pool of tables); a patch of each call site (not atomic, and unbounded).

**Code is appended.** The area is address space reserved once (64 GiB; nothing is committed
until written). A body goes at the end, is hashed against its record's content id, and only
then can a word name it. **An offset is handed out once and never again**: a program counter
names one body for the life of the process, so a profile sample, a backtrace or an unwind
lookup taken at any time is still right later. What comes back is memory, not addresses: a
whole page all of whose bodies are freed is given to the OS (`MADV_DONTNEED`;
`MADV_FREE_REUSABLE` on macOS). An area with no room left is a restart in place that says so.
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
| 2 | Drivers: map the area and table, write, publish, the control thread, the resolver, gates and thunks, the walk and the outside scan; Linux x86-64 and AArch64, macOS | a test program of hand-made bodies: lazy bind, a gated edit under running threads, a batch freed; symbol tests for the private calls | |
| 3 | Records as the launcher takes them: slot-word calls, entry thunks, the unwind entry and name in the record; the index and the unwind hooks | a panic unwinds through lazily bound and replaced bodies, with names | `blaze-link/src/lazy.rs`'s refusal of unwinding code |
| 4 | `blaze run` under `edit` through the launcher: `Welcome`, `Need`, prefetch of the last run's set; eager check in the background | hello world and reth's node start; the first-edit and no-op timings quoted from spans | `blaze-lazy`'s stubs and wire; `blaze-inproc`'s code memory for runs |
| 5 | Edits: the classifier, `Probe`, the gated `Edit`, restart in place | a DST of the daemon's side: an edit landing while another is in flight never binds code of an old input; the edit loop on reth and tempo | `blaze-live` (cell, pooled tables, 64 generations); patch room and the dev splice for `edit` builds; the Mach-O re-sign of a dev patch; fences |
| 6 | Domains: `DomainOff32`, images, thread-locals, std's thread start, the shims | each test of a real suite in a domain equals its own process's outcome | the fork server a test; `live/log.rs` |
| 7 | Selection by marks and traces | a save reruns exactly the tests whose set changed (`blaze-testsel`'s oracle) | `blaze-testsel`'s reached-set path |
| 8 | Promotion in band 3 and its retreat on an edit | tiers.md's DST | `blaze_stencil::tier`, `Mode::Tiered`, `Choice::then`, `Build::tier_up` |

## Step 1 as built

`crates/blaze-launch` (`no_std`, on bstd, `#![blaze::sans_io]`): `Launch` owns the table
(`Slot -> Stub | Body`), the area (bump, never reused), the batches and the threads. `stage`
validates and appends or changes nothing; `commit` returns the ordered `Write`s for a driver
and makes the batch; `abort` frees what was staged; `report` takes a thread's frames (in any
order: what a thread cleared stays cleared) and `collect` frees what every thread cleared, as
`Write::Forget` and whole pages as `Write::Decommit`. The simulation's machine is the
driver's contract written down: step 2 implements exactly it.
`wire` is the schema (`SCHEMA = 1`): frames as blaze-protocol's, bodies in its serde shape
(a struct a map keyed by field names in canonical order), written and read by hand without an
allocator, and checked against `blaze_protocol::cbor::check`.
