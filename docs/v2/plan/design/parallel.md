---
status: plan
summary: How one command uses every core and holds little memory. The end state (architect, 2026-11-16) first: one work-stealing thread pool, item-grain tasks that never block (a missing record parks the task on a per-key claim and it is re-entered), no global interner, a frame per worker reset per item, records as the only thing that outlives a task, footprint as the one measure, a refused allocation as a typed failure of one job, determinism by sorting at record boundaries; then the gap against today's code and the pieces in build order. Below them, the 2026-11-15 audit of the older layer, kept as a record.
date: 2026-11-17
---
# Parallelism on 128 cores, and on a Mac

## End state (architect, 2026-11-16)

Workload: hundreds of thousands of tasks of 10 us to 10 ms (an outline, a crate's names, a
signature, a body, an instance), some needing a record another task is writing. Measured today
(tempo, one thread, port27/tempo-front-1116/tempo-front-9.txt): names of 1,026 crates 29.5 s,
heap 2,309 MB live after them; 20,000 bodies of one crate 31.2 s (640 a second); peak 2,589 MB.

1. **One scheduler: bstd's `Threads` pool; threads, work stealing, no fibers** (task/threads.rs
   stands: a worker per core of every level, LIFO slot, Chase-Lev deque, injection per LLC
   group). A parked fiber costs a stack per wait (hundreds of thousands of them) and carries
   thread-bound values across it (rule 27). A task never waits and is sans-IO (bottom.md), and
   it is NEVER STARTED OVER: as built, a task that answers `Need` is run again from its first
   line, 3.0 entries a `Signature`, 3.1 a `BodyCheck`, 2.9 a `CrateScope` (tempo cold g080231:
   51,396 / 17,132; 50,330 / 16,041; 3,531 / 1,208). (a) PLAN FROM TEXT: what a task can be
   seen to need from its text and the outlines (a body's paths and the heads they name, a
   signature's types, a crate's dependencies' scopes) is asked and settled before its first
   entry, in phases over shared tables (outlines, names, signatures, the impl index, bodies:
   the shape Sorbet and Pyrefly get their throughput from), so the common case has entries ==
   made. (b) A need found only while running (a method on an inferred type, a const in a type,
   an opaque's hidden type) is made inline by the same worker when its key is unclaimed (point
   3). Claimed by another worker: a CHEAP kind is made again (a head, a signature, a const:
   about 0.5 ms; equal bytes by key, rule 25, so the duplicate is correct, bounded and rare); a
   DEAR kind (a crate's scope, 20 ms and its frame; a body's hidden type) yields as a state
   machine that KEEPS its state (its frame stays its own while parked) and resumes at the
   question, never at line one. Every kind reports entries per made; the target is 1.0.
2. **Grain.** One item a task: a file's outline, a module's names (a glob cycle one `Group`), a
   signature, a body, an instance. The driver hands out batches of about 1 ms of predicted cost
   (learned per code identity; text length the first time), the items of one file; each item
   keeps its own key, record and failure. Dispatch stays under 1%. THE COORDINATOR IS NOT ON
   THE HOT PATH: today one thread makes every task's job and steps the machine once a batch
   (g044908: `jobs made` 33.2 s, steps 13.6 s of a 152 s run; g080231: 26.9 s and 13.0 s, puts
   7.1 s, of 168 s). A worker makes its own task's job; batches are planned in bulk and results
   merged in bulk, sorted at the boundary (point 8), so the serial part is a small constant
   per phase, not a step per task.
3. **Waiting without a graph.** A record being written has a claim: a slot in a fixed lock-free
   table by key (`blaze_core::claims`): empty, claimed by a task, or done with its address. A
   task needing key k claims it and runs it inline if free (help first: no restart), reads it if
   done, else (point 1 b, by kind) makes it again or parks: it joins k's waiter list keeping
   its state, and the publisher pushes the waiters onto its own deque. Each slot knows its owner and each parked task its one slot;
   before parking a task walks owner, owner's wait, ... (at most `live` steps): reaching itself
   is a cycle, found at the edge that closes it (SeqCst: of two tasks closing one cycle at once,
   one sees it). A module-scope cycle becomes one `Group`; any other is `Failure::MadeCycle`
   naming the items. The watchdog names a claim past its kind's deadline (`Hung`, rule 18).
4. **Sharing.** Only immutable bytes: records read in place (mapped, or a fresh record in its
   writer's per-worker segment buffer, published by the claim's Release store) and the claim
   table; counters per worker. **No global interner**: each task interns into a table in its
   frame; a record carries its own canonical table in the first-use order its key fixes; a
   reader re-interns what it touches (stands: typeck-store.md, "The type tables", option A,
   "process-global" read as "per task"). No shard lock, no id counter, no stop-the-world.
   NOTHING PRIVATE PER JOB: every global fact a body reads (names, heads and signatures, the
   impl index, lang items) is an immutable record read in place by every worker (impl buckets
   on blaze-surface/blaze-impls records, lane BK; heads frozen, piece 9); a body task's own
   state is only its inference tables, in its frame. Today each file job builds a private
   world per owner and makes again every head it reads (blaze-cli body_types.rs `Checker`,
   `FileTypes::new`). rustc's parallel front end scales poorly past 16 threads on shared
   mutable state: that is not rebuilt here.
5. **Memory by lifetime.** A frame per worker, reset per item; reserve is address space
   (`FRAME_RESERVE`, 4 GiB), committed pages are footprint. What outlives an item leaves as a
   record, never a pointer into a frame. The command's frame holds the request's rows and what
   the answer prints, nothing per task (a job names its file by index into the request's file
   table). Footprint = workers x largest working set in flight + the rows; mapped records are
   clean pages. Tempo keeps every crate's solved names live today (2.3 GB); here a crate's solve
   leaves as its `CrateScope` record. Estimate: under 0.5 GB private on 8 workers, about 4 GB on 128.
6. **One measure, one ceiling.** Footprint (phys_footprint; on Linux the cgroup's charge) per
   task (its frame's peak) and per command; the kernel's limit is the ceiling; nothing admits,
   throttles or narrows by memory (rule 50; withdraws scheduler.md's point 7).
7. **A refused allocation fails one job, typed**: `FrameFull { task, frame, asked, peak,
   reserve }`; the build goes on. Closed by a type: blaze-own's growing methods that cannot fail
   (`push`, `reserve`, `with_capacity_in`) need `A: NeverRefuses` (global heap, `Kept`); a frame
   is not one, so only `try_*` and `?`. Until the bound lands, the lint `blaze::infallible_on_frame`
   denies them and the job seam reads the refusal from a slot the alloc hook fills, never a panic's text.
8. **Determinism.** A record's bytes are a function of its key and answers. Merges sort at the
   boundary: a unit's chunk and manifest by item key, findings by (item, ordinal), per-worker
   store batches by key at commit, gap lists by key (not by text). A test compares bytes of 1
   worker, 8, and reversed order.
9. **Sized by one rule.** Workers = cores of every level (Mac perflevel0 + perflevel1 at
   user-initiated; Linux the cgroup quota), started in proportion to queued work (a one-body edit
   stays on the main thread); frames and queues scale with workers, the claim table with the
   keys the request names (a power of two over twice that; 48 bytes a slot). Only that number differs between 8 cores and 128.
10. **Expected** (estimates; wall = T1/P + Tinf). Tempo cold front end at today's item costs:
   T1 about 130 s (names 29.5, heads 5, about 60,000 own bodies at 640 a second 94); Tinf, the
   crate chain's names, assumed 3 s (1 s with a crate's modules in parallel): 8 cores about 17 s,
   16 about 9 s, 128 about 2 to 4 s (today about 130 s). blaze's workspace (198 crates, 32.8 s
   today, mostly the one-thread names run): about 5, 3 and 1 s. Floor (a body 14 us, the same
   chain at its floor): tempo under 1 s on 16 cores, about 0.2 s on 128.

| System | What blaze matches, or does better by design |
|---|---|
| rustc parallel front end | sharded interner locks, blocking query waits, a deadlock-detector thread; blaze: no shared interner, a cycle found at the edge that closes it |
| Sorbet | frozen global state, files checked in parallel, merged on one thread; blaze merges by sorting per unit and runs names in parallel too |
| Hack, Flow | a lock-free shared hash heap over forked workers; blaze's mapped records are that heap, with threads and no heap collector |
| TypeScript's Go port | N checkers with their own types, some files checked twice; blaze: tables per task, each body once (its claim) |
| Zig | InternPool shards per thread under one index; blaze needs no index: records are keyed by content |
| mold | sizes first, then every thread writes at computed offsets; blaze writes segments and manifests so |
| Tokio, Rayon | LIFO slot, bounded queues, stealing, helping joins: bstd's pool; here no task blocks a worker |
| Fiber job systems (Go, Naughty Dog) | a stack parked per wait; blaze parks a restartable item with no stack |
| Roc, Carbon | an arena per module or check unit, canonical exposed types; blaze the same at item grain |
| salsa | blocking waits, cycles from an in-memory graph; blaze keeps no graph |

### Gap (tree of 2026-11-17 09:09, green b13ac2dcb9, by search)

Paths are under crates/; front2 is blaze-ladder/src/front2. Profile: tempo cold check g080231
(port27/tempo-deps/check-g080231.profile.txt, 168.2 s request).

| End state needs | Today, with evidence | Missing | In `blaze check`? |
|---|---|---|---|
| Body checks on every core | A file's bodies run one after another in one pool job (`for (name, place, handed) in job.bodies`, blaze-cli work.rs:2137) over one world: `FileTypes::check` runs `each_covered(&mut checker.world, ..)` (blaze-cli body_types.rs:923); `ProjectWorld::check(&mut self, ..)` (front2 project.rs:258); `Records.store: &'r mut Store` (front2 check.rs:128); missing heads are made in place (`make_round`, check.rs:327); `Store.tys` is one `PortableTys` (front2 heads.rs:554) over `Alloc`, a `NonNull<Header>` (blaze-own alloc_ref.rs:169-171), so `!Send`, `!Sync`; `fills`/`gaps` are `RefCell` (front2 world_items.rs:213-214). Sample g082400: the main thread parked on a `PooledBodies` join, one worker in `FileTypes::check` | frozen `Sync` heads and an overlay; heads by claim; a file's bodies across workers | Yes, typed, one file a worker |
| Names on every core | The library's `resolve_crates_with` runs `for &id in wave` serially (blaze-scope resolve.rs:875); `Par::map<T, F: Fn(usize) -> T>` has no `Send`/`Sync` bound (blaze-scope task/fill.rs:28). The command resolves a crate per pool task (`may_overlap("crate scope", ..)`, work.rs:714; `pooled_scope`, work.rs:2519), its job made on the main thread (`scope_job`, work.rs:2478); a crate with no job runs there too (`Spawned::Here`, work.rs:718). g080231: `CrateScope` on main 2,323x 7.0 s, pooled 1,074x 21.9 s | `Par` bounded; the job made on the worker | Yes, a crate a task |
| Names leave as records, not a live heap | `Box::leak(Box::new_in(done, alloc))` per crate (blaze-scope resolve.rs:880); the command's names world leaks its reader, frame and decoded graph (blaze-cli names.rs:622-632) | the solve in its task's frame, its record read back mapped | Partly |
| A command frame sized for the request, nothing per task in it | Root frame `MAIN_SCRATCH = TASK_FRAME` = 64 MiB (blaze-std entry.rs:51, mem.rs:62); `job_of` copies each task's path into the round's `jobs` frame (work.rs:1656) | reserve `FRAME_RESERVE`; jobs by file index | Yes |
| A refusal is a typed failure | `blaze_own::error::fail` panics (blaze-own error.rs:49); the hook makes std's refusals panics (work.rs:1318); `pooled` reports `Why::Panicked { used, limit }` (work.rs:2373); no `FrameFull` or `NeverRefuses` in the tree | `FrameFull`; `A: NeverRefuses`; the lint | Yes, as a panic |
| Tasks run on the pool | Pooled: outlines (work.rs:704), crate scopes (714), build scripts (745), a file's bodies (838), a file's lints (848), opens (2315). On the main thread: each task's JOB (below), and `Spawned::Here` (work.rs:718, 828) | jobs made on workers; every kind pooled | Partly |
| Waiting for a record in flight | `blaze_core::claim_table` is built and tested (blaze-core lib.rs:43; 9 tests with `claim_watch`, ask 090947-PAR-cbfbe9) and used nowhere outside blaze-core. A body waits by the driver's `Need`: `prepare` answers `Need` for the unit graph and owners (blaze-cli body_types.rs:514, 545), then for the owners' scopes (589) and heads (630), and the task is entered again. g080231: `BodyCheck` made 16,041, entered 50,330; `Signature` 17,132 / 51,396; `CrateScope` 1,208 / 3,531 | claims in the command; no re-entry | No |
| The coordinator's cost per event | One thread does three things per round (`run_batches`, work.rs:644-861; `answer_batch`, 934-1128). (1) JOBS MADE: for every task of every batch it makes the task's job: a body's `body_types::prepare` (work.rs:772) reads its graph, owners and scope inputs and builds its `TypesJob`, about 0.35 ms each, three times a body (the `Need` row above); a crate's `scope_job`; a script's `scripts::job`. (2) A STEP per batch: `req.step(Event::Ran ..)` (work.rs:1116) -> `Request::step` (blaze-drive request.rs:1477) -> `ran_batch` -> `pump`, about 0.26 ms. (3) The step's puts. g080231: jobs made 554x 26.9 s (Signature 36,616x 13.8 s, BodyCheck 36,432x 12.2 s, CrateScope 3,397x 0.77 s); machine step 49,703x 13.0 s; effect put 101,463x 7.05 s; 47 s serial of 168 s; the main thread waited on joins 113 s. g044908: jobs made 33.2 s, steps 13.7 s | a task makes its own job on its worker; claims instead of re-entry (a third of the steps); puts off the main thread | Yes |
| No global heap per item | `check_rounds_as` takes the store's allocator (`r.store.tys.alloc()`, front2 check.rs:202) for `InferCtx::new_in` (213) and its round vectors, `all_gaps` on `Global` (204) | the worker's frame, reset per body | No |
| Deterministic merges | `BuildOrder::Reversed` (front2 check.rs:141, used at 246); gaps kept sorted by `format!("{g:?}")`, a format per comparison (262-263); `make_round` picks a refusal by `format!` (336) | sort by key; a 1-vs-8-worker test of the command | n/a |

### Pieces, in build order (A = can be written at the same time as the others marked A)

Status of 2026-11-17 09:09 (green b13ac2dcb9). engine.md (D131) replaces 9 and 10 (E2, the
view: frozen heads, the overlay), 11 (engine point 4), 14 and the coordinator row (E1, point 5),
and the re-entry (E5); it uses 2 and 13 as built (point 3). The order test is
blaze-cli src/tests/order_tests.rs: 1 and 8 workers answer alike today; reversed order does not
(`reversed_order_answers_alike`, ignored until E2: three impl fns' `Signature` outs move with
the order in the old per-job `Store`). Its fixture: `BodyCheck` 84 entries / 21 made,
`Signature` 72 / 24, `CrateScope` 17 / 7, `FileItems` 4 / 4.

1. **A. Command frame**: root frame reserve `FRAME_RESERVE`; `Job` names its file by index; try_ growth; `CliError` naming the frame. Test: a check of a few thousand generated files under a small command frame finishes; with the copy back it fails typed, not by panic. NOT BUILT (root frame still 64 MiB; paths copied per round).
2. **A. `blaze_core::claim_table`**: the claim table, waiter lists, the cycle walk. Test: two-thread schedules with oracles, three planted bugs caught. BUILT: `claims_waits_and_cycles_hold_on_drawn_schedules` and its controls pass (ask 090947-PAR-cbfbe9); used nowhere yet.
3. **A. Typed refusal at the job seam** (`Why::FrameFull`): the hook writes the refused layout into the running job's slot. Test: a job planted to over-ask its frame fails with `FrameFull` naming the bytes; the scope is not cancelled. NOT BUILT.
4. **A. `Par` bounded**: `fn map<T: Send, F: Fn(usize) -> T + Sync>`. Test: per_crate's threaded run equals `Serial` and `Reversed` bytes. NOT BUILT (task/fill.rs:28).
5. **A. Frame per body**: `InferCtx` and round vectors in the caller's frame; gaps sorted by key. Test: a counting allocator sees zero global allocations per checked body. NOT BUILT.
6. **A. `NeverRefuses`** and the lint on infallible growth over a frame. Test: lint UI cases. NOT BUILT.
7. **A. Footprint per task**: each task's frame peak and the process footprint at batch end into the profile. PARTLY: frame peaks per job and batch (work.rs:869, 876, 2367); no footprint per batch.
8. **Names, a crate per task**: each crate a pool task in its own frame, its record read back mapped. PARTLY: pooled per crate, `pooled_scope_tests::a_pooled_check_writes_the_records_a_serial_one_does` (and its control); the solve still leaked to the heap, the job made on main.
9. **Frozen heads and an overlay**: published heads are `Sync`; a body's fills go to its overlay. Test: the determinism test under both orders, and 8 workers equal 1. NOT BUILT.
10. **Heads by claim**, after 2 and 9: a body's fill of head k claims k: mine, make and publish; done, read; claimed, park on it or help, never re-enter the whole task. Test: two bodies needing one head make it once (a planted double-claim is caught). NOT BUILT.
11. **Bodies on the pool**, after 10: a file's bodies as batches across workers, a worker frame reset per body, results sorted by item key. Test: `blaze check` of a fixture with 1 and 8 workers gives equal answers and record keys. NOT BUILT (a file a job, work.rs:838).
12. **Typed `BodyCheck` in the command**. Test: a type error is printed by `blaze check`. BUILT: blaze-cli tests/check_written_bodies.rs `every_written_body_is_checked_by_the_command` (a planted E0308).
13. **Watchdog over claims**, after 2: a claim past its kind's deadline named as `Hung`. Test: virtual clock, a planted stuck owner. PARTLY: `blaze_core::claim_watch` built and tested (`one_stuck_owner_is_named_once_at_its_deadline`, ask 090947-PAR-cbfbe9); nothing calls it.
14. **Jobs made on the worker** (new; the coordinator row): a task's `prepare` runs on its worker, the main thread only steps; with 10 no task is entered more than once. Test: the fixture's profile shows entries equal to made for `BodyCheck` and `Signature`. NOT BUILT.

## Audit of the older layer (record, 2026-11-15)

Kept for its findings on crates outside the rebuilt path (blaze-launch-rt, blaze-cas, hir-ty);
where it differs from the end state above, the end state holds.

The rule is [bottom.md](bottom.md) section 1: no global lock and no single-threaded stage on a hot
path; one code path sized by the machine. This page audits the layer written on 2026-11-15
(branch rebuild-1115, HEAD `9b8ea50c8b`; `crates/blaze-drive/src/request.rs` was being edited in
the working tree during the audit, so its line numbers are HEAD's). It does not redesign two
pieces being written now, bstd's threaded layout ([std.md](std.md#layouts)) and the driver's pool
([drive.md](drive.md)); it states what they must meet.

Every time below is from the tree; "unmeasured" means nothing in the tree measured it. Nothing in
the tree has measured any piece above 16 threads, except an old-path incident on 64 cores
(`crates/blaze-mem/src/os.rs:14-19`).

## What a hot path is here

A cold `blaze check` (new crate, empty local store, std from CI) runs per file, per module, per
item: outline, names, expansion, signatures, bodies, findings. An incremental check runs a handful
of tasks on the main thread (bottom.md 9.6), so 128 cores matter for the first fill, a branch
switch, a manifest edit and `prepare-std`; the edit loop's need is that the pieces below cost
nothing when one thread uses them. The goal order decides every trade: after-restart
incremental time, remote records, memory, cold time, cleanup, disk.

## The pieces

S = shared between threads. Hot = touched per item, per token, per type or per call.

| Piece | Unit, time | Shared (how; hot?) | Serial today (inherent / accident) | First limit, 1 to 16 to 128 cores |
|---|---|---|---|---|
| Outline, `blaze-file-items` | one file; about 1 us a line, one thread, M-series (bottom.md:599-603) | none in the crate; parser scratch is `thread_local!` (`crates/parser/src/event.rs:103-104`, `crates/blaze-mem/src/scratch.rs:160-162`): per thread, not contended | a large file outlined whole (inherent until per-item re-outline, bottom.md 5) | the largest file (30 ms at 30k lines) as a straggler; scaling unmeasured, harness exists (`examples/time.rs:64-104`) |
| Records, `blaze-surface` | one record build; unmeasured | none; views are borrowed bytes; `MemSurface` is test-only and O(modules) a lookup (`src/testing.rs:120-151`) | `build_impl_index` sorts every impl row of a crate twice (`src/impl_index.rs:312-340`): accident | one crate-wide build per impl change on a large crate |
| Names, `blaze-scope` | one module (`Resolver`), one glob SCC (`Group`); unmeasured; the 10 ms gate unmeasured (bottom.md:214-220) | none (`src/resolver.rs:303,452-461`) | a `Group` is solved on one thread (`resolver.rs:1119-1351`, `lookup.rs:102-145`): inherent per SCC; a fresh `Resolver` per task entry, re-decoding the crate root and re-sorting the extern prelude for every module (`crates/blaze-drive/src/scope_task.rs:77`, `resolver.rs:500-509`): accident; the parallel first fill exists only in the doc | the largest glob SCC; then d-fold redo per Need chain of depth d |
| Expansion, `blaze-expand` | one call; unmeasured; no caller outside tests | **S**: `intern::Symbol::intern` per identifier and literal of every definition and call (`src/bridge.rs:89,101`), a process-static store, lock-free read, shard lock on insert, never freed (`crates/intern/src/arena.rs:1-12`); `tt` thread-local scratch and `Global` tables (`crates/tt/src/storage.rs:178-184,852-857`) | none | shard locks on new identifiers; memory that never returns (goal 3) |
| Impls, `blaze-impls` | one lookup; unmeasured | none; `ImplRecords::note(&self)` (`src/lookup.rs:61`) forces interior mutability on the caller's context | none; O(crates/64) words a lookup (inherent, cheap) | a shared `note` accumulator if a context is ever shared |
| Bodies, `blaze-typeck` | one body; unmeasured (typeck.md:231-237); no task runs it (`crates/blaze-cli/src/work.rs:8-9`) | none in the crate (`src/lib.rs:14-17`); `TypeCtx` is `&mut`, no `Sync` (`src/ty.rs:116`); `World` is `&self`, no bound (`src/world.rs:226`) | none | whatever `TypeCtx` becomes: planned as hir-ty's global tables (typeck.md:167-182) |
| Type tables, `hir-ty` (parked) | per body, many goals; hit 9.0 ns in cache, 26 ns out, one thread (`next_solver/interner.rs:4641-4646`) | **S, hot**: `static TYS` and siblings (`next_solver/ty.rs:61`, `region.rs:24`, `consts.rs:33`, `predicate.rs:189`, `interner.rs:601`); 64 fixed shards (`interner.rs:4602-4603`), unpadded (`:4751-4757`); one `next.fetch_add` per table per new row (`:5626,5902,6151`); global `RETIRED` lock on growth (`:4690`); flood switch takes all 64 locks (`:5353-5392`) | `release_dead`/`release_all` stop the world (`:4875-4916`); goal_store `flush` gathers and sorts on one thread (`goal_store.rs:570-596`) | the per-table `next` line, then 64 shard locks shared by 128 threads |
| Solver caches (parked) | per goal; a hit "one FxHash, about 1.5 probes, three acquire loads" (comment, `vendor/rustc_type_ir/src/search_graph/global_cache.rs:66-67`) | **S, hot**: `SHARED` 16,384 slots, one lock to make an entry (`interner/tls_cache.rs:121-148,207-212`); `OF_QUERY` 512 slots for all threads (`:546`); `GlobalCache` sharded 4 x cores, padded 128 (`global_cache.rs:384-400`): good; goal_store's `Tally`, four `fetch_add`s on shared lines per miss (`crates/blaze-record/src/chain.rs:62-95`); 64 `Pending` shards by round-robin (`goal_store/pending.rs:20-68`) | as above | `Tally` lines on every miss; `OF_QUERY` eviction at 128 threads (uncertain) |
| Interners, `blaze-ids` `Sharded` | per value; unmeasured | **S, hot**: 4 x cores shards, `repr(align(128))` but not `repr(C)` (`src/lib.rs:366-401`); one global `next: AtomicU64`, once per 64 ids per shard (`:354-361,477-480`); under `Spin`: re-probe, first-use allocation of a `64<<k` slot chunk, the index insert or whole-table grow (`:466-496`; `crates/blaze-swiss/src/lib.rs:335-363`) | none | a `Spin` held across an O(n) grow while others spin |
| Spin lock, `blaze_swiss::sync::Spin` | per lock; unmeasured | test-and-test-and-set, no backoff, no yield, panics past `1<<36` rounds (`crates/blaze-swiss/src/sync.rs:30,219-234`); unfair, every waiter swaps at release | none | the release stampede when 128 waiters swap one line |
| Hash-consing, `blaze-hashcons` | per value; unmeasured | shards from `shard_count()`, padded 128, parking lock, id = `local << shift \| shard`, no global counter (`src/table.rs:67-99,143,193`): the best-scaling table in the tree | none | a contended `blaze_core` lock registers in the global `static WAITS` mutex when a profiler probe is installed (`crates/blaze-core/src/lock.rs:356-364`, `lock/graph.rs:54,111`) |
| Hashes, `blaze-hash` | per hash; 9.0 ns / 26 ns per kinds hit (`src/adaptive.rs:8-10`) | none at run time | none | none |
| Request machine, `blaze-drive` | one task per `Run`; `Profile::ran` never called, so time is 0 (`src/profile.rs:55-59`) | one `Request` stepped by one caller (`src/request.rs:386-516`); state in sorted vectors (`crates/blaze-own/src/sorted.rs:22-27`) | one `Event::Ran` per completion back into one machine (`request.rs:681`); `ready.remove` of the first element, insert/remove shifting vectors, `unsettled()` scanning every job (`:891-923`): accident | the coordinator: O(n) per completion with n live jobs |
| The cli, `blaze-cli` | per effect | none | **every task on one thread**, effects in FIFO order (`src/main.rs:152-216`); `workers` = cores only caps what is emitted (`:125,182`); sweep reads and hashes every file in order (`src/sweep.rs:45-65`); each printed finding re-reads its file (`src/print.rs:25`): all accident | everything: speedup 1 at any core count |
| Stand-in store, `blaze-cli` | per record | one flat directory of one file per record, `stat` then read, `stat` then publish (`src/store.rs:29-70`) | none | directory inode lock on 128 concurrent creates; a stand-in, replaced by the plain store |
| Plain store, `blaze-cas` `plain/` | per get/put; flip "tens of microseconds" (`src/plain.rs:58-59`) against "milliseconds" (`plain/core.rs:2378`), neither measured | lookups lock-free over mapped segments and a papaya map (`plain/handle.rs:59,103-126`); **every put takes one `Lock<Driven>`** (`handle.rs:57,186-199`); every miss takes it while anything is uncommitted (`:155-162`); per-worker `Batch` exists (`core.rs:313-319`) but no caller passes one (`handle.rs:203`); `len`/`uncommitted` adjacent, unpadded (`:61,65`) | segment digest over the whole segment on one thread (`core.rs:1201`); compaction single-threaded and holds the writer lock per 64 MiB slice (`handle.rs:212-223`, `core.rs:1361-1386`); open-time BLAKE3 of unsealed segments one after another (`core.rs:1822-1827`); macOS driver has 2 file threads (`plain/driven.rs:185`) | the writer lock on every put of a cold fill |
| Stamp sweep, `blaze-solo` | per file; "64 statx ~30 us" stated, not measured (`crates/blaze-core/src/uring.rs:185`) | Linux stampers claim chunks through `Claims` `next`/`finished`, adjacent, unpadded, written by every stamper (`crates/blaze-core/src/handoff.rs:26-32`) | `FILE_THREADS = 4` hard-coded (`src/bin/solo.rs:51,96,120`); hashing on the command's thread (`src/run.rs:530-533`); stamp comparison after the last batch (`run.rs:502-513`); `items` file by file (`run.rs:871-880`): accident | serial hashing on a branch switch |
| Collections, `blaze-own` | per op | `Arc` count unpadded (`src/arc.rs:44,204-207`): usual | none | only if an `Arc` is cloned per job (std.md says send flat messages) |
| Frames and arenas, `blaze-mem` | per task frame; "about 2 ns a bump" (comment, `src/frame.rs:37-39`) | per thread: frame shelves (`frame.rs:1147-1164`), ids in blocks of 1024 (`:1167-1178`), tag counters folded at 1 MiB (`src/tag.rs:463-506`), pool caches moving 64 at a time (`src/pool.rs:5-11`); **S, hot**: `Kept`'s one `warm` head per size class, no per-thread layer (`src/blocks.rs:146-192,300`, `src/stack.rs:37-39`) and the tag line written per take (`blocks.rs:283-284`); `Arena.current` one CAS per 64 KiB for a whole request (`src/arena.rs:121-133`) | none | `Kept`: "threads sharing a class's stack each pay about a hundred times a lone thread's pair" (`benches/kept.rs:25-26`) |
| Kernel, through `blaze-mem` | per commit, span | Linux commit is `MADV_POPULATE_WRITE` (mmap_lock read) (`src/os.rs:571-578`); macOS commit is `mprotect` (write) (`os.rs:584`); each span over 1 MiB is its own `mmap` and `MADV_DONTNEED` on return (`frame.rs:538-541,1054`, `os.rs:815`): a TLB shootdown per span | none | span decommits: shootdowns across every core of the process |
| Run machines, `blaze-run` | sans-IO, unmeasured; heat bound "to be measured" (run.md:122) | none (`src/lib.rs:5-31`) | `Slots`/`Heat` on the program's control thread (inherent); retirement O(batches x threads) per report (`src/slots.rs:364,424,448-452,468`): accident; `Tests::affected` scans the verdict column though its doc says it never does (`src/suite.rs:11-12,150-154`) | retirement at 128 program threads |
| Launcher, `blaze-launch` | sans-IO; warm no-op `run` 15.1 ms on 32-core arm64 (launcher.md:492) | none | the same O(B x T) retirement (`src/launch.rs:528-616`), a second copy of `Slots`' protocol: accident (rule 30) | as above |
| Launcher runtime, `blaze-launch-rt` | per stub fire, publish | **S**: every report and first call pushes a `SegQueue` and rings one pthread mutex + condvar (`src/table.rs:86-87,160-162`, `src/sys.rs:881-921`); an `Arc<Handoff>` allocated per fire (`table.rs:375`); `epoch`/`waiting`/`threads` adjacent, unpadded (`table.rs:71-75`); **count stub**: a plain `u32` in a 16-byte `Counter`, 4 a line (`src/observe.rs:41-47,191-193`); `symbols.rs:51` one global `Mutex` per `dl_iterate_phdr` | one control thread (inherent); `membarrier` twice per edit (`src/drive.rs:362,373`, `sys.rs:697-704`): accident; decommit by `PUNCH_HOLE` per range (`sys.rs:710-716`) | the bell mutex; true and false sharing on hot counters |
| Watch, `blaze-watch` | per event; unmeasured, unused | per-thread SPSC seqlock rings, slots and heads `align(64)` (`src/ring.rs:1-24,181-182`): right; lane table unpadded but written only at claim/end (`ring.rs:477-525`) | one drain thread (inherent, off the hot path) | `align(64)` is half a line on Apple silicon (128 bytes) |
| Codegen, `blaze-codegen` (excluded, `Cargo.toml:15-19`) | per function; old cold hello-world walk 2,938 of 9,727 ms (`driver/start.rs:5`) | one MPMC `SegQueue` worklist for all jobs (`driver/worklist.rs:44-46`); lock-free memo, replaced a `RwLock<HashMap>` for this reason (`memo.rs:7-54`); `static PACKAGES` hash-cons (`plan.rs:182-189`) | dedupe fixpoint on one thread (`dedupe.rs:301-330`, call `driver.rs:3761`) | the shared worklist head when it returns |
| LIR, `blaze-lir` | per function; unmeasured | none, sans-IO (`src/lib.rs:43`) | none | none |
| bstd tasks, `blaze-std` (being written) | per task | only `Layout::Inline` exists, `workers()` = 1 (`src/task.rs:70-110`); `Chan` is a lock and two condvars (`src/chan.rs:75-77`); `Flight` scans under one lock (`src/sync/flight.rs:19-40`); `PerWorker<T>` padded slots promised (std.md:363-366), not in code | **every `par::` chunk runs inline** today | n/a until the layout lands |

## The state of the art, taken or not

| Technique | Where it belongs here | Taken? Why, by goal order |
|---|---|---|
| Work-stealing deques (Chase-Lev 2005; Lê et al. 2013 for weak memory) | bstd's `Threads`/`Frames` layouts; the driver's pool | Yes. Per-worker owner-LIFO, thief-FIFO; with drive.md's ~1 ms batches a bounded ring per worker that steals half is enough (Tokio's design); no shared injector on the hot path |
| Topology-aware stealing (steal within the L3 group first: an EPYC CCD is 8 cores, a Graviton cluster; then node, socket) | the same pool | Yes, reuse `crates/blaze-graph/src/topology.rs` and `sched/cores.rs` (tiers Sibling, L3, Node, Socket, Remote; already built and tested for steals across nodes, `sched/tests.rs:162-165`) |
| NUMA-local memory | `blaze-mem` chunk spaces per node (`src/chunks.rs:93-97`) | Already; keep. Graviton is one node; dual-socket EPYC needs it |
| Per-thread caches with remote frees (Bonwick magazines 2001; mimalloc's per-page free lists, 2019) | `Kept` | Yes: the one shared head per class is the measured 100x. Memory cost is bounded by a magazine size per thread (goal 3 accepts it) |
| Sharded or lock-free hash tables; ids without a global counter | `blaze-ids`, hir-ty tables | Yes, in the `blaze-hashcons` form already in the tree: shards ~4 x cores, padded to 128 bytes, id = local index and shard |
| Task-local tables over a frozen shared base | the type context | Yes, the larger win: a body's inference types never leave the body (its record is the portable form, typeck.md:190-193), so they need no global interner at all |
| Epoch reclamation (Fraser 2004; crossbeam-epoch) | code retirement in `blaze-launch`/`blaze-run`; `Swap` (blaze-reclaim) | Yes: one announced epoch per thread, a batch freed when the minimum passes its stamp: O(T) a round, not O(B x T x T) |
| Hazard pointers (Michael 2004) | nowhere | No: epochs already exist (blaze-reclaim); per-pointer publication costs a fence per read on the type hot path |
| Read-copy-update for tables read on every call | the slot table (one writer, the control thread) | Already the shape (`retired.rs:15-23`); keep |
| Per-core append logs | store writes | Yes as per-worker `Batch`es merged at commit, sorted so two builders write equal bytes (rule 25); not one segment file per worker (more files, more index probes after a restart: goal 1 loses) |
| Batching to amortize the coordinator | the request machine | Yes, drive.md's ~1 ms batches; workers resolve stored needs themselves so a stored hit never crosses the coordinator |
| Huge pages for store maps | `blaze-core` mapped files | Already: `MADV_HUGEPAGE` at 2 MiB and up, no populate (`crates/blaze-core/src/fs/mapped.rs:103-105,350-356`); keep, never populate (restart pages, goal 1) |
| io_uring or batched `pread`s | cold reads of many small records | Linux: the uring driver exists. macOS: the plain store's driver has 2 file threads (`plain/driven.rs:185`); size it as the sweep's lanes |
| Padding to 128 bytes, not 64 | every per-worker or per-shard line | Yes: Apple silicon's line is 128 bytes and x86's adjacent-line prefetcher pairs 64-byte lines |
| Distributed counters (per-thread, summed when read) | profile counters, `Tally`, tags | Yes; `blaze-mem`'s tag folding is the pattern |
| A futex/ulock doorbell over per-thread SPSC rings | launcher runtime to control thread | Yes, the blaze-watch ring protocol already proves the push side |

## Ranked changes

Ranked by what a 128-core cold check hits first, correctness first; each names its measure
(rule 16: before and after from spans or counters). Owners are the crate's lane.

| # | Crate, file | Change | Measure |
|---|---|---|---|
| 1 | `blaze-launch-rt`, `src/observe.rs:185-205` | **Correctness**: the AArch64 count stub loads into w1 and writes w1 (`ldr_w(1, 16, 4)`, `movz(1, 1, 0)`), destroying the counted function's second argument. Use x16/x17 only: count down with `subs w17, w17, #1`, store, `b.ne body`, as run.md and `blaze-run/src/heat.rs:4-7` say; at zero, call the runtime (nothing reads `queued` today) | a test calling a two-argument function through the count stub; then the counter's ns per call |
| 2 | `blaze-cli`, `src/main.rs:152-216`, `src/work.rs` | The pool (drive.md lane, being written): check it runs tasks on `workers` threads, performs `Get`s in the workers, and reports one `Ran` per batch, not per task | `Profile::ran` filled (`blaze-drive/src/profile.rs:55-59`); wall/CPU ratio of `check` on library/ at 1, 16, 128 |
| 3 | `blaze-drive`, `src/request.rs:891-923` | O(1) ready queue (a ring, not `remove(0)`), a live-job counter instead of `unsettled()`'s scan, tickets and jobs in an open-addressed table, not sorted vectors | coordinator span per step; ns per completion against live jobs |
| 4 | `blaze-cas`, `src/plain/handle.rs:57,155-223` | Wire the per-worker `Batch` (`core.rs:313-319`) through the handle: a put touches its worker's batch only; a miss reads the worker's own batch, never the writer lock; compaction drops the lock between slices and never holds it over a copy (store-next.md:21's claim, held in-process) | puts/s and lock wait span at 1, 16, 128 threads; `Busy` count |
| 5 | `blaze-mem`, `src/blocks.rs:146-300`, `src/stack.rs` | Per-thread magazines for `Kept` (take and give 64 at a time to the class stack, as `pool.rs` `Cache` does); remote frees onto the owner's list; fold `tag::add` per thread as `tag.rs:463-506` does | `benches/kept.rs` with its 8-thread cap raised to the machine (`:12,96`): ns per pair, shared against lone |
| 6 | the extracted type context (blaze-ty, from `crates/hir-ty/src/next_solver/interner.rs`) | Body-local interner in the task's frame for inference types; the shared table only for types read from records, frozen per request; where shared: shards 4 x cores, `repr(align(128))`, ids as `blaze-hashcons` makes them (no `next.fetch_add`), per-shard retire list instead of `RETIRED` | inserts/s and hits/s at 1..128 threads; `self_cpu_ns` of `BodyCheck` per body on std |
| 7 | `blaze-expand`, `src/bridge.rs:80-109` | Stop interning every token into the process-global `intern` store: tokens name the expansion's own string table (as `src/tokens.rs:2` already claims), so a call allocates in its frame and frees with it | `Expansion` span; process footprint after expanding std's calls |
| 8 | `blaze-ids`, `src/lib.rs:388-496`; `blaze-swiss`, `src/sync.rs:219-234`, `src/lib.rs:335-363` | `repr(C)` shard with the lock on its own line apart from `index.level`; allocate the slot chunk before taking the lock; grow by a new table published by pointer (readers keep the old one until the epoch passes), never an O(n) copy under `Spin`; `Spin` gets exponential backoff then `yield`, bounded as now | intern ns at 1..128 threads; spin rounds counter |
| 9 | `blaze-scope`, `crates/blaze-drive/src/scope_task.rs:67-117`; `src/resolver.rs:500-509` | The first fill as rounds over all of a crate's modules (drive.md), with the crate root and extern prelude decoded once per crate into a frozen value shared by the round, not per module per entry | first-fill span per crate on reth and tempo (the 10 ms gate); `Resolver::new_in` count per module |
| 10 | `blaze-launch-rt`, `src/table.rs:71-87,160-162,271,375` | One SPSC ring per program thread to the control thread (blaze-watch's push), one ulock/futex doorbell word rung only on empty-to-nonempty; the `Handoff` preallocated per thread; `epoch`, `waiting`, `threads` on separate 128-byte lines | stub-miss latency (run.md floor: about 10 us) with 1 and 128 threads missing at once |
| 11 | `blaze-solo`, `src/bin/solo.rs:51`, `src/run.rs:502-533`; `blaze-core`, `src/handoff.rs:26-32` | Lanes from the machine's workers; hash in the lanes; compare stamps per batch as it arrives; pad `Claims` | sweep span on reth after a branch switch |
| 12 | `blaze-launch`, `src/launch.rs:515-616`; `blaze-run`, `src/slots.rs:364-468` | One retirement protocol (rule 30: delete `Slots`' copy or `Launch`'s); per-thread announced epoch, O(T) a round | report span with 128 program threads |
| 13 | `blaze-launch-rt`, `src/observe.rs:41-47` (the count stub's `Counter`) | Stride counters so neighbouring slots' counters sit on different lines; a plain decrement stays (a lost one delays a call, run.md:113) | counted call ns with 1 and 16 threads calling neighbours |
| 14 | `blaze-mem`, `src/frame.rs:538-541,1054` | Spans over 1 MiB kept on the thread's shelf and decommitted in batches (`process_madvise`, as chunks do, `os.rs:986-990`), not one `MADV_DONTNEED` per return | TLB shootdown count (`/proc/interrupts` TLB line) per cold check |
| 15 | `blaze-core`, `src/lock.rs:356-364`, `src/lock/graph.rs:54` | Contended-lock registration into per-thread rows (the `waits.rs` table), not one `static WAITS` mutex | contended-lock span with the probe on |
| 16 | `blaze-record`, `src/chain.rs:62-95` | `Tally` per thread, summed when read | goal_store miss ns at 128 threads |
| 17 | `blaze-surface`, `src/impl_index.rs:312-340` | Build impl buckets independently, in parallel, and only the buckets whose module digests moved | impl index span per crate edit |
| 18 | `blaze-cas`, `src/plain/core.rs:1201,1822-1827`; `plain/driven.rs:185` | Segment digest in chunks over the workers (BLAKE3's tree mode is parallel); open-time checks of several segments at once; macOS file threads = workers | commit span; first-open span after a boot |
| 19 | `blaze-launch-rt`, `src/drive.rs:362,373` | One `membarrier` per edit, not two | membarrier count per edit |
| 20 | `blaze-run`, `src/suite.rs:95-154` | Postings from code identity to tests, not a scan of the verdict column | `affected` span on reth's suite |
| 21 | `blaze-std` (layout lane), `src/task.rs`, `src/sync/flight.rs` | Check the threaded layout against this page: per-worker deque, steal by `blaze-graph` tiers, `PerWorker<T>` at 128 bytes, workers from topology, QoS by class (below); `Flight` with a hashed core before the driver uses it | `par::reduce` scaling on 1..128 workers |
| 22 | `blaze-codegen` (when back), `src/driver/worklist.rs:44-46`, `src/dedupe.rs:301-330` | Per-worker worklists under the same pool; dedupe's candidate hashing over the workers | codegen walk span |

Not changed: the stand-in store's flat directory (`blaze-cli/src/store.rs`) is replaced by the
plain store (drive.md gap), so it gets no fix of its own; one segment per worker is refused
(above); hazard pointers are refused.

## The whole build as a pipeline

Cold `blaze check` of a million-line workspace on 128 cores, std from CI. Measured inputs (M) and
assumptions (A), each labelled:

- M1: outline 1 us a line, one thread (bottom.md:599-603): 1.0 s of CPU for 1 M lines.
- M2: 207,552 items in 1.53 M lines of `library/` (bottom.md:601): about 0.136 items a line, so
  about 136k items.
- M3: `core`'s 180,413 lines in 299 files (bottom.md:600): about 600 lines a file, about 1,700
  files.
- A1: body checking costs 10 x the outline a line: 10 s of CPU. Unmeasured; replace with
  `BodyCheck` time on std once blaze-ty lands. Names, expansion and signatures are folded into it.
- A2: a coordinator step costs 2 us a completion (sorted-vector inserts grow it with live jobs).
- A3: a put holds the store's writer lock 0.5 us; about 3 records an item: about 400k puts.
- A4: the sweep hashes about 40 MB at about 1 GB/s on one thread.
- A5: the critical path is 20 serial Need rounds of one 1 ms batch each (unmeasured: bottom.md's
  "serial `Need` rounds per body on std").

| Stage | Today | After changes 2 to 4 and 11 (pool, coordinator, store batches, sweep) | Floor |
|---|---|---|---|
| Task CPU, 11 s | serial: 11 s (`main.rs:152-156`) | 11 s / 128 = 86 ms | 86 ms |
| Coordinator | inside the 11 s | per batch: about 11,000 batches x 2 us = 22 ms (A2) | 0 (workers resolve stored needs) |
| Store writes | inside the 11 s | 0 serial (per-worker batches; one commit) | one commit |
| Sweep | 40 ms (A4) | 0.3 ms | 0.3 ms |
| Critical path | n/a | 20 ms (A5) | 20 ms (A5) |
| **Total** | **about 11 s, speedup 1** | **about 130 ms, speedup about 85** | **about 106 ms** |

Ratio to the floor (rule 67): today about 100; after the changes about 1.2. With the pool alone
and the coordinator and store lock left as they are, the serial part is about 150k completions x
2 us = 0.3 s plus 400k x 0.5 us = 0.2 s plus the serial sweep and the critical path, about
0.65 s in all: speedup about 17. That is Amdahl's law at a serial fraction of about 5%. Two checks on the arithmetic: memory bandwidth is not a
limit (the outline reads 32 to 51 MB/s a core, M1, so 6.5 GB/s on 128 cores); and the interner,
the allocator and the kernel do not appear in the table because their costs are unmeasured.
Changes 5, 6, 8 and 14 exist so they stay out of it; experiments 2 and 3 below say whether they do.

## A typical Mac

8 to 16 cores in two performance levels, 16 to 64 GB, 128-byte cache lines, one NUMA node.

- **What macOS offers.** No thread pinning. A thread's QoS class steers it: `USER_INTERACTIVE`,
  `USER_INITIATED`, `DEFAULT`, `UTILITY`, `BACKGROUND` (`pthread_set_qos_class_self_np`, or
  per-block QoS overrides). `UTILITY` and below prefer the efficiency cores and are throttled
  under load; `BACKGROUND` also throttles disk and network IO. The levels' sizes are
  `hw.perflevel0.logicalcpu` and `hw.perflevel1.logicalcpu`, read by
  `blaze_graph::topology::Topology::from_perflevels`.
- **Measured in the tree.** Utility workers starved to about 2% of a core each on a loaded host
  (2026-11-02, `crates/blaze-core/src/cores.rs:246-248`); hence workers run `user-initiated`, or
  `default` when the process is lowered (`crates/blaze-host/src/workers.rs:217-219`,
  docs/macos.md:96, still to measure as M9). The store's background work already lowers its
  thread to `UTILITY` (`crates/blaze-cas/src/plain/driven.rs:515-524`).
- **The same design there.** Foreground tasks (what the command was asked, drive.md's class) run
  on workers at `USER_INITIATED`, one per CPU of both levels: a cold check uses the efficiency
  cores too, and work stealing in ~1 ms batches keeps a slow core from holding the tail.
  Background tasks (the optimizing tier, speculative code, compaction, scrub, sealing) run at
  `UTILITY`; their starvation under load is the point, since they are background, and a
  background task is emitted only when no foreground task is queued (drive.md). The stealing
  tiers treat a performance level as the L3 group.
- **Must not regress.** A one-body edit stays on the main thread with no pool started (bottom.md
  9.6); the pool starts in proportion to queued work. Per-thread magazines (change 5) and
  per-worker store batches (change 4) are bounded per thread, so 16 threads cost kilobytes, not
  a share of a 16 GB machine. `blaze-mem` commits by `mprotect` on macOS (`os.rs:584`), which
  takes the map lock for writing: at 16 threads this is tolerable, and the 128-core path (Linux)
  already avoids it. Padding is 128 bytes everywhere, which Apple silicon needs (blaze-watch's
  `align(64)`, `src/ring.rs:181-182`, and `waits.rs:178-195` share lines there).

## What to measure first

Each runs in under an hour on the Mac and on a many-core Linux machine; each keeps its tree and
binary (rule 22).

1. **The outline divides by N** (bottom.md:624). `cargo run --release -p blaze-file-items
   --example time` over `library/` at 1, 8, 16 and the machine's threads (the harness exists,
   `examples/time.rs:64-104`; it pulls files from one `AtomicUsize`). Report lines/s per thread
   and the largest file's share. Settles whether the parser's thread scratch and the global heap
   it touches (`examples/time.rs:66`) scale.
2. **The allocator.** `benches/kept.rs` in `blaze-mem` with its thread cap (`:12,96`) raised to
   the machine; then the same with change 5. ns per pair, lone against shared.
3. **Interner inserts.** A bench (to write, `crates/blaze-hashcons/benches/`) inserting fresh and
   repeated values from 1..128 threads into `blaze-hashcons`, `blaze-ids` `Sharded` and a copy of
   hir-ty's `Rows`; ns per insert and per hit. Decides change 6's shared half and change 8.
4. **The coordinator.** A bench (to write, `crates/blaze-drive/benches/`) driving `Request` with
   N no-op `FileItems` tasks answered at once by a stub pool; ns per step against live jobs, per
   task and per batch. Decides change 3 and the A2 assumption.
5. **Store puts.** A bench (to write, `crates/blaze-cas/benches/`) of `PlainStore::put` from
   1..128 threads with the handle as today, then with per-worker batches; puts/s and the writer
   lock's wait span. Decides change 4 and A3.

## Looks parallel, is not

- `workers` is the machine's core count and the machine emits that many runs; the cli runs them
  one after another (`blaze-cli/src/main.rs:152-156,182`).
- Every bstd `par::` chunk and `may_overlap` runs inline: the only layout is `Inline`,
  `workers()` is 1 (`blaze-std/src/task.rs:70-110`).
- `Kept` is lock-free and still serial: one head per size class for every core
  (`blaze-mem/src/blocks.rs:300`).
- The plain store's per-worker `Batch` has "no shared word" (`plain/core.rs:313-319`); the
  handle puts everything through one `Lock` and passes no batch (`plain/handle.rs:57,186-203`),
  and a miss takes that lock whenever anything is uncommitted (`:155-162`), which in a cold fill
  is always.
- Compaction "never blocks a reader or a writer" (store-next.md:21): in-process it holds the
  writer lock per slice (`plain/handle.rs:212-223`).
- `blaze-expand` is a pure function per call over a process-global interner that never frees
  (`src/bridge.rs:89,101`; `crates/intern/src/arena.rs:1-5`).
- `blaze-typeck` is pure per body ("no global state", typeck.md:46); its planned `TypeCtx` is
  hir-ty's `static TYS` with 64 unpadded shards and a global row counter
  (`hir-ty/src/next_solver/interner.rs:4602,4751-4757,5626`).
- `Sharded` documents "a few hundred instructions" under its lock (`blaze-ids/src/lib.rs:390-393`)
  and does a chunk allocation and an O(n) table grow there (`:485-493`).
- Named locks are sharded, but every contended one meets `static WAITS` once a profiler probe is
  installed (`blaze-core/src/lock.rs:356`, `lock/graph.rs:54`).
- The sweep's batch is spread over `lanes`, and `lanes` is 4 (`blaze-solo/src/bin/solo.rs:51`).
- Every program thread's report and first call takes one pthread mutex
  (`blaze-launch-rt/src/table.rs:160-162`).
- `Tests::affected` is "never a scan of the tests" and scans the verdict column
  (`blaze-run/src/suite.rs:11-12,150-154`).
- Test-only single-thread shapes that a real implementor would copy: `MemSurface`'s linear scans
  (`blaze-surface/src/testing.rs:120-151`); `ImplRecords::note(&self)` met with a `RefCell`
  (`blaze-impls/src/lookup.rs:61`, `tests/impls.rs:18`); the parser's thread-local scratch, which
  ties an outline to its OS thread (`crates/parser/src/event.rs:103-104`).

## Not determined by reading

How hot `codegen.driver.export_spare` is (`blaze-codegen/src/driver.rs:1170`); whether
`symbols.rs`'s mutex is reached by the unwinder; whether the field layout of `blaze-ids`'
`Shard` and `next` really shares lines (layout is the compiler's without `repr(C)`); whether a
tracing subscriber makes the parser's three spans per file contend; `OF_QUERY`'s eviction rate at
128 threads; the real size of glob SCCs and of Need chains on std, reth and tempo; and every
cost in the pipeline table marked A.
