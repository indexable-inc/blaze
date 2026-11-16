---
status: plan
summary: "`blaze run` and `blaze test` with no daemon and no executable: the `blaze run` process is the program's parent for exactly its life; the program runs code records placed in its own process behind one slot word a function, each compiled on its first call and optimized across functions in the background. Built as sans-IO machines with simulations in crates/blaze-run (Parent, Slots, Heat, Tier, Registry, Tests); not built: the drivers, the real Compiler and Optimizer, `blaze export`."
date: 2026-11-15
---
# Run and test

Andrew, 2026-11-15: no daemon, no `blaze build`; `blaze run` and `blaze test` run a program from
code records placed in its own process, compiled on first call, optimized across functions in
the background while it runs; "make it exist properly all in one go". This page re-seats the
launcher (launcher.md), tiers (tiers.md) and lazy compilation (blaze-lazy) without the daemon.
The machines are `crates/blaze-run`; this page is what they decide and what is not built.

## The picture

```
blaze run (parent: alive exactly as long as the program)      the program's process (blaze-launcher)
  certificate stands? -> manifest of the program (records)       control thread: Slots, Heat
  Registry: entry in <state>/runs/, events stream                 program threads: load word, jump
  Parent: lookups, Compiler on workers, single flight             a stub fires -> queue -> control
  Tier: Optimizer on background workers                           thread -> Need over the socket
        <==== Unix socket, blaze_launch::wire (deterministic CBOR), one per run ====>
  Welcome{slots, known: slot -> code record}  ------------->  maps table, area; places entry + pre-main
  Begin                                      ------------->  runs initializers, then main
                                             <-------------  Need{slot, thread}   (no known record)
  Publish{slot, tier, record} / Failed{slot, error} ------>  reads record from the store, places,
                                                             stores the word, wakes the thread
                                             <-------------  Hot{slot}  (heat crossed its bound)
                                             <-------------  TestEnded{test, verdict, entered}
                                             <-------------  Exited{status}  -> parent unregisters, exits
```

No process outlives the program. What the next run needs is in records: the code (by instance
key), the optimized code (read-addressed by what it inlined), the program's manifest and what it
ran (by the checkout's certificate), each test's record. A run started while another run of the
same checkout lives is its own process with its own parent; they share only the store.

## The machines (crates/blaze-run)

Each takes an event and pushes effects into a caller's buffer; time is an event field
(`Nanos`); process liveness is a trait; every table is in the allocator its maker passes.

**Parent** (`parent::Parent`, in `blaze run`). Per slot: `Unasked`, `Looking{priority}`,
`Queued{priority}`, `Compiling{priority, deadline}`, `Ready{tier}`, `Failed{error}`.
In: `start(needed, history)`, `need(thread, slot)`, `looked(slot, found)`,
`compiled(slot, result)`, `tick(now)`. Out: `Lookup`, `Compile`, `Publish`, `Answer`, `Begin`,
`NoStart`, `Say(RunEvent)`. A need is looked up first (another run may have made the record),
compiled only on a miss, at most once however many threads ask; every asker is answered after
the publish, with the same answer. Priorities: `Asked` (a thread or the start waits), `Callee`
(a published function calls it), `History` (the last run ran it). Background work never takes
the last free worker (workers >= 2), so a thread waits only on its own function. A failed
compile answers every caller `Failed(CompileError)`: in the program, the call panics on that
thread with that error (rule 20) and every other function goes on. A compile has a deadline on
the driver's clock; past it the askers fail with `Why::Overran`, and a late success still
serves later callers. The `Compiler` trait: instance key -> `FastRecord { code, callees, reads }`
or `Why`; its task writes the record before it answers.

**Slots** (`slots::Slots`, in the program's control thread). A word is one `u64`: 0 the stub,
else body + 1 with a tier bit (in the real table an address; the tier is the body's). States
Stub -> Fast -> Optimized, or Stub -> Optimized when a valid optimized record exists; nothing
else (`FastTwice`, `AlreadyOptimized`). `publish` writes `Place`, then `Store` (one aligned
atomic store, release), then, if a body was replaced, `Epoch`. Retirement is blaze-launch's
protocol with one body per batch (a first call and a promotion mean what they replace, so no
gate): a thread reports the epoch it loaded before walking its stack; it clears each retired
body stamped at or before that epoch that no frame of its stack is in; a body is freed once
every thread alive at its stamp cleared it or ended; a thread born later is told the epoch its
parent loaded.

**Heat** (`heat::Heat`, in the control thread). Fast code's prologue adds one to its slot's
`u32` beside the words (a plain add: a lost increment only cools a function a little). The
control thread samples the counters every period of its clock and charges the differences.
At `bound` a function is hot, exactly once per run; every `period` each count shifts right by
`shift`. The parameters are a `Config`; the first values are to be measured (open questions).

**Tier** (`tier::Tier`, in the parent). Per slot: `Fast`, `Queued`, `Optimizing{snapshot}`,
`Optimized`, `Failed`, `Dropped`. `hot(slot, instance)` -> `Optimize(job)`; the `Optimizer`
trait: instance + the callees it may inline (instance and code identity) -> `Optimized { code,
inlined: [(instance, code identity)], transitively }`; `done` -> `File{slot}` and
`Publish{slot, code}`. A job begun before a newer snapshot (another run of the checkout began on
newer text) is dropped when it ends; what is queued then never starts. `callers_of(code
identity)` is the reverse map `inlined_into`.

**Registry** (`registry::Registry`, in the parent). An entry file `run-<pid>-<start>` (pid and
the OS's start time of the process, so a reused pid is another name), 88 bytes, written whole by
rename; removed at the run's end. `read(listing, processes)` lists the live runs and removes
each entry whose pid has no process or a process of another start: a read never lists a dead
run. Beside it, `<name>.events`, appended with `RunEvent`s (began, compiling X, published X,
tiered X, failed X, test passed or failed, memory, ended): fixed little-endian, keys and numbers
only, so a viewer on any machine reads them, names coming from records.

**Tests** (`suite::Tests`, in a test run's parent). A test run is a program run whose entry calls
each test. Entered functions are collected by one byte a slot a lane: the fast prologue stores
1 to `lanes[lane][slot]` (the lane a thread-local the harness sets per test worker and a thread
inherits through the launcher's `pthread_create`); at a test's end the control thread reads the
set bytes and clears them, and an optimized body counts the bodies it absorbed. That is one
store per call and a scan of one byte a slot per test (a few microseconds for 100k slots);
the alternative, re-arming every word as a stub per test, costs a miss per function per test.
`record(test, verdict, entered)` keeps postings from code identity to tests; `affected(changed)`
is a lookup per changed identity, plus every test with no record.

**Export** is not built. `blaze export` reuses: the Parent's lookup and Compiler for every
function the manifest reaches (no stubs: the walk is the program's reach, computed forward from
the entry), the Tier's records (it asks the Optimizer for each function without a valid
optimized record, at once, not by heat), the Slots layout of the code area (bodies placed as the
launcher places them, the table resolved to direct calls), the code record's fixups, and writes
one Mach-O or ELF whole. It keeps nothing between runs.

## Records, keys and read lists

| Record | Key | Read list / validity |
|---|---|---|
| Fast code (`CodeRecord`, blaze-keyed tag 27) | the instance's template identity at the fast tier | none beyond the key: a callee's edit moves no caller's fast code (calls are through slots) |
| Optimized code | the instance at the optimized tier | read-addressed: its shape is the inlined list, each fact a callee's code identity; a changed callee is a new address, so the stale record is never found and costs nothing |
| Program manifest | the checkout's certificate and the entry | slot -> (instance, code identity, fast record key); what the run reached last (`history`) |
| Test result (`TestRun`, tag 8) | the test's base key | its asks: the code identities it entered, and its outside reads |
| Findings of a refused compile | as the compile's | as the compile's |
| Registry entry, events | not records: a live run's process state, gone with it | — |

## What crosses the boundary, and how

The hook that exists is blaze-launch-rt's: a slot's word names its stub (`arch`: `adr x16, .;
b head` on AArch64), whose resolver saves the argument registers and calls `table::resolve`,
which pushes `Event::Need { slot, thread, reply }` on the control thread's queue (a lock-free
`SegQueue`, not a channel, since a stub can fire while its thread ends) and waits on a
`sys::Handoff<Bound>`; the control thread speaks `blaze_launch::wire` (deterministic CBOR) over
a Unix socket. That stays: the parent takes the daemon's place at the socket's other end. On
`Bound::Code` the resolver jumps through the word again, never to an address it was handed;
`Bound::Refused` becomes the caller's panic. blaze-lazy's hook (`blaze_lazy_resolve` over
blaze-inproc's shared-memory asks to the daemon) is the daemon's and goes.

The control thread places a known record without the parent: `Welcome` carries, for each slot
whose fast record the manifest names and the store has, its key. Only a slot with no record
crosses the socket. New messages on the wire: `Hot{slot}`, `TestEnded{test, verdict, entered}`,
`Publish{slot, tier, record}`, `Failed{slot, error}`.

## Floors (rule 67), with everything a record

- **`blaze run` to main's first instruction.** The certificate look (bottom.md: under 20 ms,
  a few pages), one `posix_spawn` of the launcher (about 1 ms), the socket's connect, the area's
  mapping, and the entry and pre-main records read and placed: a few hundred records of a few
  hundred bytes each by `pread` from packs already in the page cache, about 1 ms, plus the
  instruction-cache maintenance. Floor: about 3 ms past the certificate. Target: under 25 ms in
  all. Not measured.
- **One stub miss with the record present** (known at Welcome): resolver, queue, wake of the
  control thread (a futex: 2 to 5 us), one `pread`, copy and fixups (about 1 us a KB), `Store`,
  wake of the thread: floor about 10 us. Without a record it is that plus a socket round trip
  (10 to 20 us) plus the compile. The daemon-era path was a round trip on every miss.
- **One publish.** The bytes written into the area once, an icache flush of their lines (and
  `membarrier` on Linux AArch64), one atomic store, an epoch store when a body is replaced.
  Floor: under 1 us past the copy.

## Reused, mirrored, deleted

- Mirrored from `blaze_lazy::fetch::Fetcher`: single flight, waiters in ask order, priorities
  (`Asked`, `Callee`, `History`), a worker kept for asks, a failure answered to every asker. Not
  kept: its daemon waiter (client, seq), and `Stop` on the first failed ask (rule 20 now).
- Mirrored from `blaze_launch::launch::Launch`: the epoch protocol, its error names, the planted
  `FreeEarly` control. `Slots` drops the gate and multi-body batches (edits while running are
  not now); when live patching comes, `Launch` is the core and `Slots` folds into it.
- Reused as is: blaze-launch-rt (stubs, resolver, queue, `Handoff`, `drive`, `records`, `tls`,
  `unwind`, `symbols`), blaze-launch's area and placement, blaze-keyed's `CodeRecord` and
  `TestRun` kinds and read-addressing.
- Deleted when the drivers land: blaze-lazy (hook, wire, stubs, fetcher, runner, session, plan),
  blaze-inproc's code memory and host for runs, blaze-launch-host's daemon side, the
  `BLAZE_LAUNCH` switch, blaze-stencil's count-driven tier ladder (`Heat` and `Tier` replace it).

## Open questions

1. Heat's bound, period and shift; whether the prologue counter is worth its add on every call
   against sampling the program counter instead (no store on the call path, coarser).
2. Who owns the slot table across runs: slots are fixed for a run; a later run of an edited
   program numbers its own. A stable numbering (by instance) would let the history record be
   slots; today it is instances, mapped at start.
3. A test lane per worker thread costs a thread-local load in every fast prologue; tests run
   one at a time per process would not. Measure on reth's suite.
4. Whether a newer snapshot should also stop the parent's speculative compiles of this run.
5. Live patching (not now): an edit while running makes `callers_of` the set to take back to
   fast code, through `Launch`'s gate.
6. A thread that never reaches a safepoint holds every retired body (blaze-launch-rt has no
   scan from outside yet): with one run per program and no edits, only promotions retire, so
   the cost is the old fast bodies' memory until the program ends.
7. The parent's own footprint with the front end resident for compiles; rule 50.
