---
status: plan
summary: "How a program runs under the `edit` goal (D18, D21) - a launcher in the program's own process maps code records from the store behind one slot word a function, appends and never overwrites, retires old bodies by per-thread epochs and a stack scan, takes an edit as one gated transaction, and runs many tests in one process as domains. Built - step 1, `crates/blaze-launch` (the slot table, the append-only area, retirement and the transaction as a sans-IO core with its simulation, and the patch protocol's schema), and step 2, `crates/blaze-launch-rt` (the drivers - one mapping of words, stubs, thunks and code on macOS MAP_JIT and a Linux memfd, gates and lazy stubs as machine code, a safepoint's report, the control thread over a Unix socket), without the scan of a thread from outside. Not built - everything else; step 3's exact plan and the order are at the end."
date: 2026-11-11
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
| 3 | Records as the launcher takes them: slot-word calls, entry thunks, the unwind entry and name in the record; the index and the unwind hooks | a panic unwinds through lazily bound and replaced bodies, with names | `blaze-link/src/lazy.rs`'s refusal of unwinding code |
| 4 | `blaze run` under `edit` through the launcher: `Welcome`, `Need`, prefetch of the last run's set; eager check in the background | hello world and reth's node start; the first-edit and no-op timings quoted from spans | `blaze-lazy`'s stubs and wire; `blaze-inproc`'s code memory for runs |
| 5 | Edits: the classifier, `Probe`, the gated `Edit`, restart in place | a DST of the daemon's side: an edit landing while another is in flight never binds code of an old input; the edit loop on reth and tempo | `blaze-live` (cell, pooled tables, 64 generations); patch room and the dev splice for `edit` builds; the Mach-O re-sign of a dev patch; fences |
| 6 | Domains: `DomainOff32`, images, thread-locals, std's thread start, the shims | each test of a real suite in a domain equals its own process's outcome | the fork server a test; `live/log.rs` |
| 7 | Selection by marks and traces | a save reruns exactly the tests whose set changed (`blaze-testsel`'s oracle) | `blaze-testsel`'s reached-set path |
| 8 | Promotion in band 3 and its retreat on an edit | tiers.md's DST | `blaze_stencil::tier`, `Mode::Tiered`, `Choice::then`, `Build::tier_up` |

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

## Step 3: the call sequence and the record (plan, not built)

### What tier 0 emits under `edit`

For a call of a function of the program (a `Ref::Place` whose place is a function; not a
system import):

| | x86-64 | AArch64 |
|---|---|---|
| call | `call qword ptr [rip + word]` (`ff 15 rel32`) | `adrp x16, word@page; ldr x16, [x16, word@pageoff]; blr x16` |
| tail call | `jmp qword ptr [rip + word]` (`ff 25 rel32`) | the same with `br x16` |
| the function's address (`func_addr`, a vtable or static's pointer to it) | `lea reg, [rip + thunk]` | `adrp reg, thunk@page; add reg, reg, thunk@pageoff` |

`word` is the callee's slot word (`words + 8 * slot`), `thunk` its entry thunk; both are in
the launcher's mapping, within reach of every body. The word is, exactly, the function's GOT
entry: AArch64's sequence above is byte for byte what tier 0 emits today for a callee that is
not colocated under `pic` (`blaze-stencil/src/arch/aarch64/glue.rs:476-479`: `load_name` into
x16, `:641`, with `Aarch64AdrGotPage21` and `Aarch64Ld64GotLo12Nc`, then `blr x16`), and
x86-64's is today's `mov r11, [rip + got]; call r11` (`arch/x64/inst.rs:432-433`,
`load_ext_name` `:139`, `X86GOTPCRel4` `:149`) with the load folded into the call. So no new
instruction selection is needed, only the choice of which path a callee takes and what its
relocation means.

On first call the word names the slot's stub. The stub (`blaze-launch-rt/src/arch.rs`) puts
its own address in x16 (r11) and jumps to the head, which adds the launcher's context and
jumps to the resolver. The resolver saves every argument register, calls `table::resolve`,
which puts `Need { slot, thread }` on the control thread's queue and waits; the control
thread says `Need` to the daemon once a slot; the daemon answers with `Edit { binds: [slot ->
code(f, tier 0) by key], together: false }` (or `Refused { diagnostic }`); the control
thread stages, places and commits it (one store to the word) and answers the stub; the
resolver restores the registers and jumps *through the word*. All of that is built and
tested but the daemon's side, which a test stands in for.

### The hooks, in order (none made yet)

1. `blaze-stencil/src/lib.rs:53` `Target`: a field `slots: bool` ("a call of a function goes
   through its slot word; a function's address is its entry thunk"), set where `counts` is
   (`hir-ty-codegen/v2/backend.rs:1291`) from the unit's policy.
2. `hir-ty-codegen/v2/backend.rs:951` `colocate`: under `slots`, a function callee
   (`namespace` 0) is never colocated, own or not, so `call` takes the GOT path. Nothing in
   `hir-ty-codegen/mod.rs` changes: `call_as` (`:11914`), `invoke` (`:15360`) and
   `tail_call` (`:9832`) emit the same IR.
3. `blaze-stencil/src/arch/x64/inst.rs:424-434` `call` and `:482` `return_call`: under
   `slots`, a `Callee::Direct` that is not colocated emits `ff 15`/`ff 25` with
   `X86GOTPCRel4` instead of the load into r11; `bound` (`:912-935`) takes the shorter form.
   AArch64 (`glue.rs:467-482`, `:511`) is unchanged.
4. A function's address: `blaze-stencil/src/code.rs:72` `Name` gets `Thunk { index }`;
   under `slots`, `FuncAddr` (`arch/x64/inst.rs:200-211`) and `address`
   (`arch/aarch64/glue.rs:671-704`) name it with the near, PC-relative form (`X86PCRel4`;
   `adrp`/`add`), never the GOT; `hir-ty-codegen/v2/convert.rs:568` maps it to a new
   `blaze_codegen::Target::Thunk(InstanceKey)` (`blaze-codegen/src/lib.rs:433`) and
   `template::Ref::Thunk(u32)` (`template.rs:38`). A static's initial bytes that point at a
   function (`Init::Bytes { relocs }`, `lib.rs:329`) name the thunk the same way.
5. The record the launcher takes is the stored `Template` (`blaze-codegen/src/template.rs:72`,
   written by `put_of`, `driver/later.rs:191`, keyed by `dev_template_key`, `driver.rs:418`)
   plus, for each `Ref::Place` it names, that place's slot: `Bind` gains nothing; the daemon
   sends a second list a bind, `refs: [slot]`, in `wire` (schema 2). The launcher's
   `Records` (new, in blaze-launch-rt) reads the template from the store's directory, copies
   its code and applies each relocation: a GOT kind against a function place is that slot's
   word (`Place::word`); a PC-relative kind against `Ref::Thunk` is `Place::thunk`;
   `Ref::Own` is the body itself; anything else against a function (`Call26`, `PcRel32`: a
   direct call) is a refusal naming the record, since code built without `slots` must not
   be placed. Statics wait for step 6 (until then a record that names a mutable static is
   refused by name). It then hashes what it read against `bind.record`.
6. The daemon's side of `Need`: `code(f, tier 0)` by key is `Later::compile_kept`
   (`blaze-codegen/src/driver/later.rs:476`: the store's template, then the memo, then a
   compile), which nothing in blaze-cargo calls today. `blaze_lazy::runner::Compiler`
   (`blaze-lazy/src/runner.rs:57-70`) is the same shape and has no implementation; step 4
   implements the launcher's server on `compile_kept` directly and blaze-lazy goes.
7. `blaze-codegen/src/plan.rs:238` `Mode`: a variant `Launched` for a run under `edit`,
   chosen in `blaze-cargo/src/serve/unit/codegen.rs:870-872` where `Live` is chosen today;
   under it `plan.rs:492`'s `patchable` is true for every function, member or not, and
   `driver.rs:2704-2707` `resolve` gives `Target::Function` for every callee. `Goal`
   (`blaze-api/src/goal.rs:15`) still does not reach codegen: the mode does.

Hooks 1 to 4 are in blaze-stencil and hir-ty-codegen, 5 in blaze-launch-rt and `wire`, 6 and
7 in blaze-codegen's driver and blaze-cargo's serve, which other lanes are editing now; that
is why this stopped at the plan.

### What goes when it lands

| Goes | Where | Why it can |
|---|---|---|
| blaze-lazy: the 16-byte entries, the shared resolver, the hook, its wire | `blaze-lazy/src/stub.rs`, `hook.rs:37`, `:73`, `wire.rs:50`, `runner.rs`, `fetch.rs`, `compile.rs` | the launcher's stub, resolver and `Need` are the same thing with a slot for a `LazyId`; nothing implements `runner::Compiler` or `compile::Source` today |
| The lazy image and its placer | `blaze-link/src/lazy.rs` (`LazyPlacer` `:182`, the refusal of unwinding code `:359-364`) | nothing outside blaze-link calls it; the launcher places code |
| The answer "no lazy runner serves this run" | `blaze-cargo/src/serve/unit/run/inproc.rs:835-836` | a `Need` is served |
| The two-load slot stubs | `blaze-link/src/table.rs:51-121` (`X86_STUB`, `A64_STUB`, `write_stub`) for `edit` builds | a call loads its word itself; kept for `max`'s live images until step 5 |
| `retire` and `collect` by fences for code memory | `blaze-inproc/src/lib.rs:515`, `:528` | only `lazy.rs:388/394` retires; the core's batches replace them |

Kept until step 5: blaze-live's cell and tables (`slots.rs`, `slots/switch.rs`), the fences
(`library/blaze_inproc_rt/src/fence.rs`, `runtime.rs:354`), patch room.
