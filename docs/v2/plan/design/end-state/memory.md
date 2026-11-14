---
status: superseded
summary: End state for memory, admission and scheduling: the daemon holds no compiler state between requests; one gate every page commit passes makes the ceiling hold by construction; the give-back ladder and forecasts are deleted.
date: 2026-11-12
superseded_by: v2/plan/design/target.md
---
# Memory, admission and scheduling: the end state

Written 2026-11-12 from tree `/Volumes/Projects/blaze-int-1112`. Status of each claim: **measured** (with its source), **read from code**, or **design** (not run). Nothing here was built or run.

## 1. What this area must do, and the invariant

- Decide what bytes the daemon holds, who owns each, and when each leaves.
- Start as much work as the cores and the memory allow, and no more.
- Keep the edit loop warm without holding what a read from the store gives back in milliseconds.
- Share one machine between several daemons and their child processes.
- Say, for every byte and every wait, whose it is.

**The invariant:** `committed <= ceiling`, at every instant, for the daemon and for the machine, because no page is committed except through one reservation that can refuse. A second one follows from it: every task that fits alone under the ceiling finishes (no deadlock, no starvation), and one that does not fit alone fails by name; nothing else fails for memory.

The answer to the hard question: **a daemon holds no compiler state between requests.** It holds a fixed core and a pool of store pages; every compiler value is either scratch of a running task (dies with the task) or a record (in the store, read through the pool). "Warm" means "pages resident", never "a heap tree alive". The 195 s after a dropped front end (measured, memory.md) and the 191.6 s restart (measured, end-state.md) are one defect seen twice: state that exists only as heap. Relax shows the bound: nothing held, 87 MB peak, 52.5 ms no-op (measured, BRIEF).

## 2. How the best systems do it

- **vmcache / LeanStore / Umbra** (Leis et al., "Virtual-Memory Assisted Buffer Management", SIGMOD 2023; Neumann and Freitag, "Umbra", CIDR 2020). Take: one reserved range, a state word per page, and *one* budget in which the cache is simply what the working memory does not use; the buffer manager is the only owner of long-lived bytes. Not taken: exmap (a kernel module; blaze's eviction rate is thousands of pages a second, store-pool.md), dirty pages and write-back (records are immutable).
- **TigerBeetle static allocation** (`docs/TIGER_STYLE.md`, "static memory allocation"). Take: every long-lived structure has a size fixed at start, so the ceiling is arithmetic, not a reaction. Not taken: fixed sizes for task working memory; compiler tasks span five orders of magnitude (64 MiB to 34.4 GiB measured), so tasks get a growable reservation instead.
- **Wound-wait** (Rosenkrantz, Stearns, Lewis, "System level concurrency control for distributed database systems", TODS 1978). Take: order contenders by age; the elder never waits for the younger; the younger is preempted. That gives progress for a scarce resource with no forecast being right.
- **Relax** (`front/src/crate_image.rs` `GraphImage`; BRIEF's measurement). Take: with records read in place, holding nothing between runs costs nothing. Not taken: no daemon (the watcher and resident pages are worth a process), and mapped images (a fault blocks a worker; a failed read is SIGBUS; store-pool.md).
- **What not to take, from Bazel's `ResourceManager` and salsa/rust-analyzer:** admission by an *estimate* with nothing enforcing it, and memos on the global heap trimmed by LRU. blaze has both today, and the numbers below are what they give.

## 3. blaze today

Shape (read from code). `blaze_graph::plane::Plane` (`plane.rs`, 3,172 lines) grants each task a forecast and starts it when `memory <= free` (`pick`, plane.rs:2828). The forecast comes from two predictors layered: `admission::Estimates` (admission.rs:1-23) and `Plane::unknown_share` (serve/plane.rs; a third, `forecast::Correction`, was deleted 2026-11-12 after it forecast every task of a cold build at 66 GB), plus `PlaneInput::Forecast` and `Realized` (plane.rs:186-267) fed by a 10 ms look (`REALIZED_STEP`, serve/plane.rs:296). Nothing gates an allocation ("nothing gates an allocation", memory.md:237), so the charge is a footprint *reading* (`footprint::Charge`, footprint.rs:79), and what a reading finds too high is undone by `giveback::Planner`'s six rungs (giveback.rs:54-84), driven by `serve/job/giving.rs` and `unit/release.rs`. The store's pool exists (`blaze-cas/src/pool/`) but query memos, MIR, link images, the dependency graph and interners are not on it.

Smells under rule 66, each with its place:

- **A ladder for one job (giving memory back):** `Rung::{Allocator, Pool, Caches, Values, FrontEnd, Running}` (giveback.rs:54), each added because the one before did not reach the bytes; with hysteresis (`HOLD_FACTOR`, giveback.rs:140), a proportion rule (`FACTOR`, :123), a floor, and five tuning constants in the driver (giving.rs:74-105). The module's own doc says the rungs "do not reach the graph, the interners or the kept images" (memory.md:307).
- **Mechanisms covering the previous one's gap (admission):** grants mode and footprint mode in one type (footprint.rs:4-16, `by_footprint` :130); `unknown_share` over `Estimates` (and, until it was deleted 2026-11-12, `Correction` over both); `Realized` over the grant; `pressure::Pressured::cap` (pressure.rs:63) over the budget; `serve/ceiling.rs` (a guard that reads every 100 ms and exits the daemon, :30) over all of it; per-tag shares (`admission::share`, admission.rs:181; `fair_share` :264; `Squeeze` :328) beside the one budget.
- **A fallback:** a task that does not fit "starts alone" past its budget (`alone`, plane.rs:2836; `Charge::alone_ok`, footprint.rs:233). **A retry:** the pinned bound waits 50 ms at a time for five seconds, then `AtBound` (`blaze-cas/src/read.rs:97`, `pool/counts.rs:82`).
- **Unbounded state:** `Memo.edges: Vec<Edge>` and `Stored::Memory` (`blaze-query/src/lib.rs:240-267`) live for the daemon's life; `lru = 0` queries keep every value "until someone asks" (agents/annoyances/monowalk-1112-1.json).
- **A per-process key:** the machine ledger is a file rewritten whole under `flock` each quarter-second turn, with a lease of 1/16 of the ceiling (`ledger.rs:1-30`, `Plane::lease` plane.rs:1339); a daemon learns of room only at its turn.
- **A special-case list:** `RELEASED` tags (admission.rs:272), `EDIT_CRATES = 4` (release.rs:194), `starve_after` (plane.rs:181).
- **A release that needs the world stopped:** dropping MIR needs the front end exclusive (release.rs:8-10), so nothing can go while the task that asked for it runs.

Numbers (all **measured**, 2026-11-12, from BRIEF and memory.md unless said):

- tempo cold: footprint 122 GiB with one task running (heap live 70, blaze-mem regions 41.6, allocator kept 12.4). By tag: query 28.3, MIR 18.9 (all global heap), link 9.3, codegen 8.9, name resolution 6.9, interning 6.2. One walk kept 4,197,786 monomorphized MIR bodies it read once each (monowalk-1112-1).
- Forecasts: grants of 0.06-0.3 GiB peaked at 3-7 GiB; one codegen grant of 64 MiB peaked at 34.4 GiB (about 500 times low).
- Churn: link allocated 290 GiB to hold 9; the profiler allocated 128 GiB.
- reth: daemon 56-75 GiB warm; a 0.09 GiB shortfall dropped every kept front end (195 s next edit).
- bstd hello-world daemon: 7-26 GiB after its cold build; idle after give-back 3.85 GiB of query storage, 1.62 GiB of it 143 million edge rows; 2.3 GiB link images; 1.08 GiB free chunks; 0.8 GiB flight-recorder names.
- Mac, eight daemons: a building one under 21.8 GiB while idle ones held 22.6, 9.7, 6.1 GiB; 134 lookups each waited about 600 ms in admission on an idle machine (cause not established here; a guess is the ledger's turn).

## 4. The end state

### 4.1 Three owners, and nothing else

| Owner | What | Size | Leaves |
|---|---|---|---|
| **Core** | code, thread and fiber stacks, plane tables, watcher state, the pool's state words, the store index's root, one standing 8 MiB frame per worker | fixed at start from the worker count and the ceiling (`Core::size(config)`); refused at start if it does not fit | at exit |
| **Task** | a running task's `Frame`, its task heap, its pinned pool pages, its fiber stack past the standing one | its reservation; grows only through `Budget::grow` | whole, at the task's end |
| **Pool** | unpinned pages of store files | `ceiling - core - sum(task reservations)`; no share, no setting | evicted by the pool's one sweep, on demand, in the reservation call that needs the room |

There is no fourth owner. Concretely: no value on the global heap outlives a task. This is enforced by construction, not by a lint: a task's global-heap allocations go to **a mimalloc heap made for that task and destroyed at its end** (`heaps.rs` gives one heap per tag today; the end state is one per running task, from a pool of heaps). A value someone tries to keep past its task is a dangling pointer the first test finds, so "outputs are records" (rule 51) stops being a convention. The daemon-lifetime heap is part of the core and has a fixed size (a reserved mimalloc arena, OS allocation disallowed past it); filling it is a typed error naming the allocation site's tag.

### 4.2 The gate: `blaze_mem::budget::Budget`

One sans-IO state machine, the only thing that may let a page be committed. `blaze_mem::os` is the only caller of the kernel's commit calls, and each takes a `Reserved` token.

- State: `ceiling`, `core`, per-task `{reserved, committed, age}`, `pool_resident`, `pool_pinned`.
- `reserve(task, bytes) -> Granted | Evict(n) | Wound(victims) | TooBig`. It grants from free room; else asks the pool to evict `n` unpinned bytes (a synchronous `MADV_DONTNEED` / `MADV_FREE_REUSABLE`, no I/O, so it can run inside an allocation); else, if the asker is not the eldest task, the asker is **wounded**; if it is the eldest, the younger tasks holding the room are wounded; if the eldest alone with the pool empty of unpinned pages still does not fit, `TooBig` names the task, its bytes and the ceiling.
- A wounded task gets an allocation failure at the call that asked (blaze already turns one into a failure of that job: `record_alloc_failures`, serve/plane.rs:508), unwinds, drops its frame, heap and pins, and its node is ready again behind its elders. Its finished items are already records (4.4), so it loses one item. This is preemption for a named resource by a fixed order, not a retry of a failure: the eldest is never wounded, every task becomes eldest, so the number of wounds per task is bounded by the tasks older than it (rule 18). A waiter passes its age to the owner of the key it waits on (scheduler.md rule 6), so the effective eldest can always run.
- Reservations are in 2 MiB quanta (the frame's chunk), so the gate is one atomic add per chunk, not per allocation.
- Rule 50: the unit is footprint bytes. `counted >= footprint` holds because every commit is counted when reserved; the footprint reading stays as the audit (`footprint - counted` must be under a stated slack, a span and a gate check), never as the input to a decision. A reading cannot gate, because it arrives after the bytes do; that is why today's design needs a ladder. The kernel's limit (`memory.max`, memorystatus) is set to the ceiling and its kill count is a number that must be 0.

### 4.3 Admission and the scheduler

- A node (scheduler.md's model: one keyed pure function, pulled by a goal, banded by who waits) starts when a core of its pool is free and `forecast <= room`. The **forecast is for throughput only**; a low forecast costs a wound, never the ceiling.
- One predictor: a record `peak(node kind's code id, input size class) -> bytes`, written at task end from the task's own `committed` high-water mark, read at push. Unknown is one quantum. No correction factor, no unknown share, no realized look: growth through the gate is the truth.
- A task at or under its worker's standing frame takes no reservation and so cannot wait for memory: a lookup never sees admission.
- The plane keeps: nodes, single flight, bands and fair share, cores. It loses every memory input but `Done { peak }`.
- Long tasks reset their frame per item (a function in a unit, a body in a walk) and write each item's record as it is done. A value read once is frame scratch, not a record and not a memo (`dispatch::body_once`); a value read by others is a record. The query engine has no third kind.
- Code that cannot unwind from a failed allocation (LLVM) cannot be wounded in process. It runs as optimized-tier nodes in **worker processes** (the same transport scheduler.md names for remote workers), each reserving from the machine ledger (4.5) and killed to wound it. Tier 0 is blaze's own backend, in process.

### 4.4 Where every value of today goes

| Today | End state |
|---|---|
| Query results, MIR, inference as heap memos (`Stored::Memory`) | records in the store; a hit pins pages through the task's `PinSet`, charged to the task; small values inline in the row |
| Dependency edges in RAM (143 M rows) | the record's `reads` (end-state.md); edges of a sealed group are not resident; a live group's edges are a pool-backed column |
| Interners for the daemon's life (6.2 GiB) | a task interns into its frame; records carry names as content ids with a per-record table. No process-local id reaches a record or a key (rules 21, 25) |
| Link's kept images, codegen's kept walks | records keyed by what they were built from, read pinned by the next link or walk |
| Flight-recorder names, profile buffers | the profile's own file, appended as written; a fixed ring in the core |
| Free chunks, allocator-kept pages | none: a task heap is destroyed whole; a frame's chunks are un-reserved at task end |

### 4.5 The machine

`/tmp/blaze-<uid>/machine.<layout>` becomes a small shared mapping of atomics: `machine`, and 64 slots of `{pid, start, reserved, want}`. A process reserves by adding to its own slot and then summing the slots; over `machine`, it takes the add back and is refused (store-then-load: of two racing reservers at most one passes). A refused daemon sets its `want` and wakes the others (futex on the shared word; `os_sync_wait_on_address` shared on macOS, rule 45); a daemon with unpinned pool pages evicts and lowers its slot. A dead process's slot is pruned by pid and start time, as today. There are no turns, leases, allowances or yield targets: an idle daemon's slot is its core plus its pool, and its pool is the first thing any other daemon's want takes. No daemon exits for being idle.

### 4.6 Idle targets (design; none measured)

- **Drained** (another process wants the memory): the core only. Hello-world and reth alike: **at most 64 MiB**, since nothing in the core scales with the project but watcher directories and pool state words (8 bytes per page, committed only for windows touched).
- **Warm** (the machine has room): core plus resident pages. Hello-world: **under 150 MiB**. reth: **under 2 GiB**, a guess at the pages one edit loop touches; the pool is free to hold more while nobody wants it, since giving it back costs reads, not minutes.
- After any build, heap live outside the core: **0**.

### 4.7 Rules

3: no "alone past the budget", no bound-wait-then-fail; `TooBig` and core-heap-full are typed and named. 11: frame per task, reset per item; the task heap is for code blaze does not own. 14: `Budget`, the ledger and the sweep are sans-IO cores; the drivers are `os.rs` and the wake. 21/25: no interned id or address in a record; wounds change the schedule, never a record's bytes. 43: a Hegel DST over `Budget` (random reserves, growth, ends, crashes of ledger processes) whose oracle checks both invariants of section 1, with planted bugs (a commit that skips the gate; wounding the eldest; a pruned slot counted). 50: one measure, section 4.2. 51: enforced by the destroyed heap. 65: the peak record is per node kind and size class, never per crate.

**Where end-state.md is wrong for this area:** it keeps admission and give-back "as is until levels 1-3 have shrunk what the daemon holds" (its step 5). The gate and the destroyed task heap are what *make* level 3's "nothing is a global-heap memo" true rather than hoped, and tempo cannot finish a cold build to judge levels 2-4 without them. Steps 1-3 below belong before or beside its step 2.

## 5. Deleted, and what survives

Deleted: `giveback.rs` and its `Rung`s, `serve/job/giving.rs`, `serve/unit/release.rs` (`Release`, `Finished`, `Squeeze`, `EDIT_CRATES`), `admission::Estimates`, `share`, `fair_share`, `set_budgets`, `RELEASED`, `SHORT`, `YIELD_TO`; `footprint::Charge` and `past_ceiling`; `pressure.rs`; `PlaneInput::{Held, Footprint, Realized, Forecast, Allowance, Cap}`; `Plane::{short, wants, lease, room, turn_wanted, purge_due, unknown_share}`, the `alone` start and `starve_after`; the purger thread (`Purging`, serve/plane.rs:268); `ledger.rs`'s file format, turns and leases, and `serve/machine.rs`'s turn loop; `serve/ceiling.rs`'s guard as a mechanism; the pool's 16/64 share, `pinned_bound` and `AtBound`; `Stored::Memory` and `Stored::Evicted`, `Database::evict`, `Storage::sweep_values`, `stop_readers` for memory; `Tag` budgets (`tag::set_budget`, `over_budget`); `blaze_mem::Kept` and `blocks` for values (they become records); the heap per tag.

Survives: `blaze-mem`'s `Frame`, `Region`, `RegionVec`, `Pool`/`Slab` (for the core), `os.rs`; tags as *names* in spans and `blaze explain memory`; `arena::Counting`'s per-task account (it becomes the task heap's count); the store's pool, its sweep and loader; the plane's graph, single flight and fair share; the work-stealing pool and fibers; pid-and-start pruning.

## 6. Order of building

| Step | What | Judged by (measured how) |
|---|---|---|
| 1 | Read-once values are frame scratch: `body_once` for every per-instance MIR walk (the class in monowalk-1112-1), frame reset per item in those walks and in link and the profiler's writers | tempo cold build on `blaze-vm`, `blaze explain memory` by tag: `mir` heap 18.9 GiB to under 1; link churn 290 GiB to under 20; the build finishes. Largest measured holder, change local to a few files |
| 2 | `Budget` as the gate in `blaze-mem` with its DST; the pool sized by room, pins charged to the task; standing frames (no admission for small tasks); per-tag budgets deleted | kernel-limit kills 0 and `footprint <= counted` over a tempo cold build under a stated 64 GiB ceiling; the 134 lookups' admission wait p99 under 1 ms (spans) |
| 3 | A heap per task, destroyed at end; link images, codegen outputs and walks as records; the core heap fixed | bstd hello-world daemon after its cold build: footprint under 300 MiB (from 7-26 GiB); allocator-kept 0 |
| 4 | Admission by reservation with wound-wait; one peak record; forecast, give-back, release, pressure and both charge modes deleted | tempo cold: wounded CPU under 1% of task CPU; reth body edit no slower than before; a DST seed replaying the 0.09 GiB shortfall evicts pages and drops nothing |
| 5 | Memos and edges as records (with end-state.md steps 2-4); interning per task | hello-world idle drained at most 64 MiB; reth warm under 2 GiB; reth edit after a full pool eviction within 2x of warm (from 195 s) |
| 6 | The shared-memory ledger | eight daemons on the Mac: the building one's room is the machine less the others' cores, reached within 100 ms of its want |
| 7 | Optimized-tier nodes in worker processes | a planted LLVM task that outgrows the ceiling is killed and its unit fails by name; the daemon serves on |

## 7. Unsound, unknown, or needing a measurement first

1. **Is a pinned record read fast enough to replace a heap memo in inference's hot paths?** Measure ns per hit for `PinSet` against a memo hit on reth's check; if slow, the answer is a decode into the task's frame per task, not a daemon-lifetime cache.
2. **Is unwinding from an allocation failure sound everywhere in blaze's own crates?** A failure inside a critical section of a shared structure (a papaya insert) must leave it valid. Answer: run the tree's tests with the gate refusing at random (a DST plant), and list what breaks. If the list is long, the wound is delivered at the next frame-chunk boundary outside a lock instead, with a bounded overdraft per worker that the core reserves.
3. **How large is reth's and tempo's hot page set for one edit?** Unknown; it sets the warm number and whether 16 KiB pool pages are the right grain. Count distinct pages pinned over an edit loop once step 5's first records exist.
4. **Do per-record name tables cost more than the 6.2 GiB interner saved?** Measure record bytes and name-compare time on core and reth with names as 128-bit ids.
5. **LLVM in a worker process:** what does it cost per function (spawn amortized over a worker's life, IR in and object out as records)? `llvm compile` was about 13% of CPU (scheduler.md, measured); measure the transport on one crate before step 7. This crosses into the codegen area and needs its owner.
6. **Wound storms:** many unknown tasks starting at one quantum and all growing. The bound holds, but throughput may not. Replay tempo's task peaks through `Budget` in the transcript simulator before step 4 and count wounds.
7. **Two daemons over one store each read the same packs into their own pools**, where the page cache would have shared them. How much is duplicated on the Mac's eight daemons is not measured.
8. The cause of the 600 ms lookup waits is a guess; read the `admission` spans of that run before step 2 claims it.

## 8. How to cut it

One mind (it owns both invariants and their DST; about four files): `blaze-mem/src/budget.rs` (new), the commit path in `blaze-mem/src/os.rs`, the memory half of `blaze-graph/src/plane.rs`, and the wound's delivery in `blaze-cargo/src/serve/plane.rs`. The same mind writes the deletion list of section 5 as bans.

Whole in a few files each, independent once `Budget`'s interface is fixed:

- The shared ledger: `blaze-graph/src/ledger.rs`, `blaze-cargo/src/serve/machine.rs`, with its own DST.
- The task heap: `blaze-engine/src/heaps.rs`, `malloc.rs`, `blaze-graph/src/arena.rs`.
- The pool by room and pins charged to tasks: `blaze-cas/src/pool/{counts,loads,evict}.rs`, `read.rs`.
- Step 1, one builder per walk: `hir-ty-pure/src/walk.rs`, `hir-ty-mir/src/mir/dispatch.rs`, `hir-ty-identity` walk, `hir-ty-suspend` summary, layout and debuginfo callers.
- Link's images as records: `crates/blaze-link`. The profiler's writers: `serve/observe.rs`, `flightrec.rs`.
- The peak record and its reader: `blaze-graph/src/admission.rs` reduced to it.
- Memos, edges and interning belong to the front-end and engine sections; this area gives them only the rule (a task heap that dies) and the pin's charge.

## 9. Corrections from the lane that built the give-back ladder (2026-11-12, read from code)

These overrule the sections above where they disagree.

1. **A refused commit cannot be delivered as an allocation failure.** `GlobalAlloc::alloc` must not
   unwind; the refusal would land inside a held lock, a `Drop` during another unwind, the
   allocator's own commit or FFI frames. The primary design is section 7.2's: mark the task and
   unwind at a safe point, with a bounded overdraft held in the core. That path exists
   (`Storage::stop_readers`, `cancellable`, the `gives_way` flag the walk reads before each visit).
2. **"Counted >= footprint" needs a named ungated reserve.** The allocator's arena commits, the
   system malloc zones (LLVM in process, C libraries), thread stacks and the wasm runtime's
   mappings never pass the gate; tempo showed 6.2 GiB "outside". The reserve has its own bound, or
   step 2's "0 kernel kills" will not hold.
3. **Order.** A task's heap cannot be destroyed at its end (step 3) while memos, edges and interners
   are heap values (until step 5): either memos become records first, or step 3 leaves the engine's
   heap out. Release and give-back cannot be deleted (step 4) while 28 GiB of query memos have no
   other way out.
4. **Pre-emption by age** is sound only if a wounded task keeps its age when posted again and is not
   admitted again while the elder that wounded it runs; age orders within a fair-share band, so
   speculative work never wounds a task a client waits on.
5. **A wound costs the whole run today** (a compile puts nothing in the store until link). "Loses
   one item" needs per-batch hand-over first; it is a prerequisite of step 4.
6. **Eviction inside a reservation runs inside an allocation**: the pool's sweep takes no lock a
   task can hold while allocating and does not allocate; state what happens on a missed try-lock.
7. **Step 2's judge is wrong.** The 134 lookups' ~600 ms waits are not memory admission: the tasks
   take 0.15 ms each and start in two bursts (2.0 s and 2.6 s); between them the loop is busy (a
   143 ms front-end release, then 72 + 240 + 113 ms of its own CPU in `loop turn: completions` with
   no child span), so ended tasks are not taken back and their threads count as taken. Which part
   of those turns costs the time is not established; spans were added to name it.
8. **What carries over from the ladder:** its simulation's world model, the incident fixture
   (55 GiB held, 0.09 GiB short) and the oracle "nothing of value goes while something cheaper
   covers the wait"; the ledger's sharing simulation; the typed too-big failure naming the unit;
   `gives_way` as the wound's delivery; the per-unit `reached` count as a size class.
