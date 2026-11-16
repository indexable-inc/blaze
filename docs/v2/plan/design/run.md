---
status: plan
summary: "`blaze run` and `blaze test` with no daemon and no executable: the `blaze run` process is the program's parent for exactly its life; the program runs code records placed in its own process behind two words a function (the slot word callers jump through, the body word stubs jump through), each compiled on its first call and optimized across functions in the background; nothing observed is ever compiled into code: heat, test marks and the viewer's hooks are stubs swapped into the slot word. Built as sans-IO machines with simulations in crates/blaze-run (Parent, Slots, Heat, Tier, Registry, Tests); not built: the drivers, the real Compiler and Optimizer, `blaze export`."
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
  Registry: entry in <state>/runs/, events stream                 program threads: load slot word,
  Parent: lookups, Compiler on workers, single flight             jump; a need stub fires -> queue
  Tier: Optimizer on background workers                           -> control thread -> Need
        <==== Unix socket, blaze_launch::wire (deterministic CBOR), one per run ====>
  Welcome{slots, known: slot -> code record}  ------------->  maps table, area; places entry + pre-main
  Begin                                      ------------->  runs initializers, then main
                                             <-------------  Need{slot, thread}   (no known record)
  Publish{slot, tier, record} / Failed{slot, error} ------>  reads record from the store, places,
                                                             stores the words, wakes the thread
  Hooks{slot, hooks}                         ------------->  hook stub in or out of the slot word
                                             <-------------  Hot{slot}  (a count stub reached 0)
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
serves later callers. The `Compiler` trait: `CodeKey { function, tier }` -> `Made::Done(FastRecord
{ code, callees, reads })`, `Made::Need(keys)` or `Why`; its task writes the record before it
answers. No compile waits on another (bottom.md, "How a task waits"): on `Need` the slot is
`Blocked{left}` and holds no worker, each lacking record is made once however many compiles
lack it (`Make`, `made`), and the compile is entered again from the top when it lacks nothing;
one deadline covers all its entries. The `Optimizer` returns the same shape and `Tier` blocks
and re-enters its jobs the same way.

**Observing is never compiled in** (bottom.md, "Decide at run time"). A function's code, fast or
optimized, holds no counter, no mark and no hook, so its bytes are the same whatever any run
observes: a code record's key is `CodeKey { function, tier }`, with no hooks field, and one
record is shared by every run. What is observed is a stub in the slot word, in front of the
code, chosen at run time and gone when not wanted: a hot function, a function outside a test
run and a function nobody watches is called straight into its body and pays nothing. A slot's
watch (`Watch`) is a set of the run's two observers and the viewer's hooks:

- *Heat*: a count stub while the function is fast (below).
- *Mark*: a mark stub while tests run (Tests, below).
- *Hooks*: trace (calls and returns, the return through a trampoline the stub puts in the
  return address), exact counts and times, a breakpoint that parks the thread with its stack,
  sampling one call in N. Effects and allocations are traced by hooking the functions that make
  them (the syscall wrappers, the allocator's entries). What only changed code can see (each
  block entered, overflow and bounds checks inside one body) is not a hook: it is another
  program, keyed as such, outside this design.

`toggle(slots, hooks, on)` from the viewer sets each slot's hooks in the parent and sends
`Hooks{slot, hooks}` (the whole set); the control thread turns the hook stub in or out. Nothing
is compiled, so a toggle cannot fail, waits for nothing, and holds even before the function has
code (the stub then jumps to the need stub). `tiered` publishes optimized code whatever is
watched: a stub stays in front of the new body.

**Slots** (`slots::Slots`, in the program's control thread). Two words a function, each one
`u64`. The *slot word* is what callers load and jump through: the need stub, the function's
body, or the slot's watch stub. The *body word* is the current code: the need stub or a body;
a watch stub does its work, then jumps through it. (In the core a word is 0 for the need stub,
a stub bit with the watch, or body + 1 with a tier bit; in the real table an address.)
`publish(slot, tier, code)` writes `Place`, then `Body` (the body word), then `Slot` only when
the slot word held the old body word (no stub in it), then, if a body was replaced, `Epoch`;
publishing the code the body word already names is refused (`Same`). `observe(slot, watch)` and
`plain(slot, watch)` change the slot's watch and write `Slot`: the stub for what is still
watched, or with nothing left the body word as it is now, never one remembered from when the
stub went in. The control thread is the one writer of both words: a stub's runtime call (a
count reaching zero) queues its slot and stores nothing, so a publish and an observe or plain
never race as two writers on a word, in whatever order their requests come; writes are carried
in order, and an observe or plain may be issued while a publish's writes are still on their way.
The simulation holds that neither loses the other's store (with every write carried, each slot
word is exactly the stub of the asked watch or the body word, and each body word the newest
publish), with a planted `PublishOverStub` caught. Retirement is blaze-launch's protocol with
one body per batch (a first call and a promotion mean what they replace, so no gate): a thread
reports the epoch it loaded before walking its stack; it clears each retired body stamped at or
before that epoch that no frame of its stack is in; a body is freed once every thread alive at
its stamp cleared it or ended; a thread born later is told the epoch its parent loaded. A stub
is no safepoint: a thread between its load of the body word and its jump is held as a caller
between its load of the slot word and its call is.

**Heat** (`heat::Heat`, in the control thread). While a function is fast its slot word is a
count stub, which counts down a per-function `u32` in a data table beside the words, set to
`bound` when the stub goes in (a plain decrement: a lost one only delays the function a call).
At zero the stub's runtime call queues the slot; the control thread hands it to
`reached(slot)`, which makes the function hot exactly once per run (a stub that fires again
before its word is gone, or a late event, is nothing), then turns the count stub off
(`plain(slot, HEAT)`): a hot function stops paying for being counted, at most `bound` calls
after its first. Relax measured about 5% of cycles in count stubs before removing them at its
last tier; here they stop at hot, before the optimized tier. The count is of calls, not decayed
by time: a function called `bound` times is worth one background compile whenever the calls
came, and that compile never takes the worker kept for a thread's own function. `bound` is a
`Config`, to be measured on reth and tempo.

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
run. Beside it, `<name>.events`: the `RunEvent`s (began, compiling X, published X, tiered X,
failed X, test passed or failed, memory, ended) the machines push as `Append`, written each
flush as one blaze-format `events::Batch` in columns (`kinds`, `at`, `values`, `keys`, and
`checkouts` for the `Began` rows), each buffer at an `ALIGN` multiple so a viewer maps it in
place; opening one is O(1) in its rows. Keys and numbers only, names coming from records.

**Tests** (`suite::Tests`, in a test run's parent). A test run is a program run whose entry calls
each test. While tests run, each slot word is a mark stub (`MARK`), which stores 1 to the slot's
byte in the calling thread's bitmap and jumps through the body word: one store per call, in the
stub, only in a test run. At a test's end the control thread reads and clears the bytes set in
the bitmaps of the test's threads (its worker, and the threads it started, which the launcher's
`pthread_create` ties to it); `entered(marked, ids, inlined)` takes those slots and adds what the
optimizer inlined into each optimized body among them, since an inlined callee is never called
through its own slot. The cost is a scan of one byte a slot per test thread (a few microseconds
for 100k slots). `record(test, verdict, entered)` keeps postings from code identity to tests;
`affected(changed)` is a lookup per changed identity, plus every test with no record.

**Export** is not built. `blaze export` reuses: the Parent's lookup and Compiler for every
function the manifest reaches (no stubs: the walk is the program's reach, computed forward from
the entry), the Tier's records (it asks the Optimizer for each function without a valid
optimized record, at once, not by heat), the Slots layout of the code area (bodies placed as the
launcher places them, the table resolved to direct calls), the code record's fixups, and writes
one Mach-O or ELF whole. It keeps nothing between runs.

## Records, keys and read lists

| Record | Key | Read list / validity |
|---|---|---|
| Fast code (`CodeRecord`, blaze-keyed tag 27) | `CodeKey`: the instance's template identity and the fast tier; nothing observed (stubs are the run's) | none beyond the key: a callee's edit moves no caller's fast code (calls are through slots) |
| Optimized code | the instance at the optimized tier | read-addressed: its shape is the inlined list, each fact a callee's code identity; a changed callee is a new address, so the stale record is never found and costs nothing |
| Program manifest | the checkout's certificate and the entry | slot -> (instance, code identity, fast record key); what the run reached last (`history`) |
| Test result (`TestRun`, tag 8) | the test's base key | its asks: the code identities it entered, and its outside reads |
| Findings of a refused compile | as the compile's | as the compile's |
| Registry entry, event batches | not in the store: a live run's state, gone with it (batches are blaze-format buffers) | — |

## What crosses the boundary, and how

The need stub that exists is blaze-launch-rt's: a slot's word names it (`arch`: `adr x16, .;
b head` on AArch64), whose resolver saves the argument registers and calls `table::resolve`,
which pushes `Event::Need { slot, thread, reply }` on the control thread's queue (a lock-free
`SegQueue`, not a channel, since a stub can fire while its thread ends) and waits on a
`sys::Handoff<Bound>`; the control thread speaks `blaze_launch::wire` (deterministic CBOR) over
a Unix socket. That stays: the parent takes the daemon's place at the socket's other end. On
`Bound::Code` the resolver jumps through the word again, never to an address it was handed;
`Bound::Refused` becomes the caller's panic. blaze-lazy's hook (`blaze_lazy_resolve` over
blaze-inproc's shared-memory asks to the daemon) is the daemon's and goes. The count, mark and
hook stubs, the body word and the swap steps are blaze-launch's and blaze-launch-rt's (another
lane); they keep the rule above: a stub's runtime call queues and never stores a word.

The control thread places a known record without the parent: `Welcome` carries, for each slot
whose fast record the manifest names and the store has, its key. Only a slot with no record
crosses the socket. New messages on the wire: `Hot{slot}`, `TestEnded{test, verdict, entered}`,
`Publish{slot, tier, record}`, `Hooks{slot, hooks}`, `Failed{slot, error}`; from the viewer to
the parent,
`Toggle{slots, hooks, on}` (over the parent's own socket `<name>.sock`, mode 0600, beside the
registry entry; `viewer`: the array `[1, 0, hooks, on, [slot...]]` in deterministic CBOR, a
non-shortest head, a hook bit no hook has, a trailing byte or slots past the caller's cap
refused by name).

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
  `membarrier` on Linux AArch64), two atomic stores (one under a stub), an epoch store when a
  body is replaced. Floor: under 1 us past the copy.
- **A call.** Into a function nobody observes: a load of the slot word and an indirect jump, as
  before. Through a stub: that, plus the stub's work (a decrement and a branch; a byte store)
  and a second load and jump through the body word. The floor of observing nothing is nothing.

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

1. Who owns the slot table across runs: slots are fixed for a run; a later run of an edited
   program numbers its own. A stable numbering (by instance) would let the history record be
   slots; today it is instances, mapped at start.
2. Whether a newer snapshot should also stop the parent's speculative compiles of this run.
3. Live patching (not now): an edit while running makes `callers_of` the set to take back to
   fast code, through `Launch`'s gate.
4. A thread that never reaches a safepoint holds every retired body (blaze-launch-rt has no
   scan from outside yet): with one run per program and no edits, only promotions retire, so
   the cost is the old fast bodies' memory until the program ends.
5. The parent's own footprint with the front end resident for compiles; rule 50.
