---
status: plan
summary: lane sched (2026-10-31): a waiting job never blocks its thread, and the scheduler is a sans-IO state machine. Phase 1 is the synchronous core in crates/blaze-graph/src/sched/ with its property tests (written); phase 2 is stackful fibers (corosensei, decided 18:29): the driver and the query-wait hook are written, the pump's replacement and the thread-local audit follow.
date: 2026-10-31
---
# sched: a scheduler where waiting holds no thread

Andrew, 2026-10-31 18:24: a waiting job never blocks its thread, and the scheduler is a sans-IO
state machine. Until it lands nothing of `blaze_graph::pool`'s API or its callers changes.
Decisions since: phase 2 is stackful fibers, async is out (18:29); no IO inside a job (18:31);
the fastest design (18:33); core capacity, sockets and distances (18:40); a move flag checked
on query misses (18:41); no cargo bench gate, phase 2 lands once the self-build passes, and no
measuring: the restart comparison and the numeric targets are dropped (18:56).

## Why

Today a wait inside a task **pumps** (`crates/blaze-graph/src/pump.rs`): the waiting thread runs
other jobs on top of its own stack, up to `PUMP_DEPTH` (16) deep, and a wait that would close a
cycle, counting the edge from each pumped job down to the one under it, is `Cancelled`. That
has three costs:
- a job pumped above a waiter pins the waiter until it returns, even when the waiter's value is
  ready;
- the cycle check has to count thread-stack edges, so it refuses waits that no data cycle
  forces, and the refused task runs again from the start;
- a second pool that blocks (rayon, before lane one-pool removed it) deadlocks against it.

A job that suspends, freeing its core, removes all three: no stack edges, no pumping, no
blocked thread.

## Phase 1: the core (written)

`crates/blaze-graph/src/sched/` (`mod.rs`, `cores.rs`, `tests.rs`), module
`blaze_graph::sched`. It is synchronous: no thread, no clock read, no sleep, no IO. It has no
caller yet, and `pool`, `pump`, `par` and `exec` are untouched.

| Input | Meaning |
|---|---|
| `spawn(job, Spawn { parent, hint, critical, counter })` | a new job, from a running job or the driver; `critical` from the profiler; `counter` for a parallel map's items |
| `counter()` / `drop_counter(c)` | a count of jobs a waiter waits for all of, made and let go by a map's job |
| `claim(key, by)` | `by` (a job, or `None` for the driver: IO, a child process) makes `key`: single flight, `false` if it is taken already |
| `suspended(job, Wait)` | the running job waits for a job, a key's flight, a counter, or a time; `Ready` if that is done already, `Err(Cycle)` if it would close a cycle |
| `done(job)` | the running job ended, and with it every key it claimed |
| `key_done(key)` | a flight ended |
| `yielded(job)` | the running job saw its core's move flag at a query miss: `Moved` to a fast core, or `Stay` |
| `set_critical(job, bool)` | the profiler's critical path changed |
| `core_idle(core)` | the core's thread is free (started, woken, or spurious) |
| `advance_clock(now_ns)` | the driver read the clock |

| Output | Meaning |
|---|---|
| `take_decisions()` → `Run { core, job }` | the free core runs (or resumes) the job now; the driver clears the core's move flag |
| `take_decisions()` → `Wake { core, job }` | the job is a message to the core's inbox; a parked core is unparked and gets `Run` at its `core_idle` |
| `take_decisions()` → `Yield { core, job }` | set the core's move flag |
| `next_deadline()` | when to call `advance_clock` next |

The topology is an input, never read from the machine inside the core:
`Cores::new(Vec<CoreInfo { smt, l3, node, socket, capacity }>, &Distances)`. It is checked to be
nested (SMT siblings share an L3 group, node and socket; an L3 group is on one node; a node on
one socket), every node is in the distance table, and no capacity is 0. On Linux a driver
builds it from `topology::Topology`, the SMT sibling lists, `cpu_capacity` (or the hybrid
`cpu_core`/`cpu_atom` lists) and `/sys/devices/system/node/nodeN/distance`.

**macOS** pins no thread to a core. There `Cores::classes(&[(capacity, count)])` makes one
worker per thread of each QoS class (user-interactive for the fast class, utility for the
rest), with no locality between workers, and the core decides per class, never per core id:
"move to a fast core" means "hand the job to a worker of the fast class", and the kernel
picks the core.

### Policy

- **One queue per core, plus one for its critical jobs; a job runs where it was spawned.** A
  child goes on its spawner's queue. The owner pops the newest (depth-first, as Chase-Lev and
  Rayon), critical first; a thief takes the oldest half. `Affinity` can say otherwise: a core,
  an L3 group, a node, or the least loaded core (of the job's class on a tie).
- **A waiter wakes on the core that finished what it waited for**, in that core's LIFO slot
  ahead of its queues, because that core's cache holds the value it reads. After `LIFO_RUNS`
  (3) slot runs in a row the core takes its inbox and queues first (Tokio's rule), so a chain
  of waiters cannot starve them. For a flight of the driver's (IO) or a timer, "the core that
  finished" is the core the waiter last ran on.
- **A parallel map is one suspend.** Its items count towards a counter; the map's job waits
  on the counter once, and wakes on the core that ended the last item.
- **Cross-core wakeups are messages**: a job handed to another core goes into that core's inbox
  (`Wake`). Inbox, queues and slot are all stealable.
- **Work-conserving, nearest first, one sleeper per job.** While any core is parked, no job
  sits stealable: a job made runnable on a busy core goes at once to the nearest parked core.
  Each core's tiers, nearest first: its SMT siblings, its L3 group, its node, the other nodes
  of its socket by the kernel's distance, then other sockets' nodes by distance (never a
  guess from ids). A core with nothing of its own steals half the nearest victim's queue in
  the same order. Within a tier the search starts at the core after the thief, so thieves
  spread out. One consequence: a steal crosses a node only when the thief's node has nothing
  runnable.
- **Critical jobs run on the fastest cores; other work goes to slower ones first.** A
  critical job made runnable goes to the nearest parked core of the top capacity, then any;
  another job goes to a slower parked core first within each tier. A fast thief steals
  critical jobs first, a slow one the rest first.
- **A fast core is never idle while a slow core runs a critical job.** When a fast core parks
  (or a job turns critical, or one starts on a slow core) while that holds, the core emits
  `Yield` for the slow core. The job reads the flag (one relaxed load) at its next query
  miss, never on a hit, calls `yielded`, and moves. Nothing is preempted: no signals.
- **Cycles are refused when they are asked for.** A wait that would close a cycle, followed
  through job waits, key flights to their claimer and counters to each live item, returns
  `Cycle { jobs }` in wait order (the asker first) and changes nothing. The check is a DFS
  over suspended jobs with an epoch mark, so it allocates nothing.

### Invariants and their tests

`sched/tests.rs` drives the core with a simulated driver over random interleavings drawn by
Hegel. The inputs are spawns with random hints and criticality, parallel maps on counters,
waits on jobs, keys and times, claims, ends, key flights by jobs and by the driver, query
misses, `set_critical`, `core_idle` (requested and spurious) and clock advances. They run on
random nested shapes: 1 to 2 sockets × 1 to 2 nodes × 1 to 2 L3 groups × 1 to 2 physical cores
× 1 to 2 SMT threads, each physical core fast or slow, with a random symmetric distance
table. After every input, `Sched::audit` checks that every structure agrees and that no job
is stealable while a core is parked. The driver checks each decision against the state, and
it checks that no slow core runs a critical job unasked while a fast core is parked.

| Test | Property |
|---|---|
| `every_runnable_job_runs` | no lost wakeup: once nothing new arrives, every job ends, each after a `Run`, and every core parks |
| `no_deadlock_when_waits_form_a_dag` | waits that form a DAG (ranks drawn at random), including parallel maps whose items wait for other jobs (the rayon case), all end on 1 to 3 cores |
| `a_cycle_of_waits_is_reported_naming_its_jobs` | arbitrary waits: each refusal is a cycle of real waits (each job waits for the next in the model's own graph) starting at the asker, and no wait that closes no cycle is refused. With cycles refused, every job still ends |
| `a_steal_never_crosses_a_node_while_its_own_has_work` | on shapes of two or more nodes, every steal or handoff across a node happens while the thief's node holds no stealable job (counted by a scan at the moment of the steal) |
| `a_critical_job_is_never_left_on_a_slow_core_while_a_fast_one_idles` | on fast/slow class shapes: after every input, a slow core running a critical job while a fast core is parked has its move flag set, and a move lands on a fast core |

The unit tests pin each rule: `a_job_runs_where_it_was_spawned`,
`a_waiter_wakes_on_the_finishing_core_ahead_of_its_queue`, `a_waiting_job_frees_its_core`,
`an_idle_core_steals_its_sibling_then_its_l3_then_its_node_then_anywhere`,
`other_nodes_are_stolen_from_nearest_by_the_distance_table`, `a_thief_takes_half_a_queue`,
`a_job_made_runnable_goes_to_the_nearest_parked_core`, `a_cross_core_wake_is_a_message_to_its_inbox`,
`a_key_flight_ends_with_its_job`, `a_parallel_map_is_one_suspend_on_its_counter`,
`a_cycle_names_its_jobs_in_wait_order`, `a_cycle_through_a_counter_names_the_item`,
`a_timed_wait_wakes_at_its_deadline`, `the_slot_gives_way_to_the_queue_after_its_runs`,
`critical_jobs_go_to_fast_cores_and_the_rest_to_slow_ones`,
`with_only_slow_cores_free_a_critical_job_still_runs`,
`a_critical_job_moves_to_an_idle_fast_core_within_one_miss` (1000 misses, no wait; it moves at
the first miss after the fast core frees), `a_move_finds_the_fast_core_taken_and_stays`,
`a_shape_that_is_not_nested_is_refused`, `first_from_wraps_and_respects_the_mask_and_filter`,
and `a_warm_scheduler_allocates_nothing` (spawns, runs, steals, waits, flights, counters and
timers, measured with `blaze-alloc-count`: zero allocations).

### Choices made

- **Key waits mean "the key's current flight".** A wait on a key with no flight is `Ready`: the
  job re-reads the memo and, finding nothing, claims the key itself. That is the single-flight
  protocol `pump::Flights::get_or_run` already follows, and it closes the race between reading
  the memo and the flight ending without the core remembering every key ever done.
- **A job's end ends its flights.** A claimed key whose job ends without `key_done` wakes its
  waiters anyway; they find no value in the memo and the plane reports the job's error. Waiters
  are never left asleep.
- **A dropped counter with items left** is freed when the last one ends; a counter with a
  waiter cannot be dropped.
- **`set_critical` on a queued job** takes effect at its next move (a pick, a steal, a
  handoff); a running one is asked to move at once if a fast core is parked. A stale move
  flag (the job stopped being critical) makes `yielded` answer `Stay`.
- **Hot and cold state are split**: the fields every spawn, pick, wait and end touch are one
  32-byte record per job; counter links and the cycle check's marks are a 20-byte record
  apart.
- **Timed waits** use `blaze_core::wheel::Wheel` (so `blaze-graph` now depends on `blaze-core`;
  `blaze-core` depends on neither `blaze-graph` nor anything that does).
- **Contract violations panic** (a `done` for a job that isn't running, a wait for a job never
  spawned): they are driver bugs, and the message names the job.

## Phase 2: stackful fibers (decided 18:29; started 19:40 ahead of the self-build, at the lead's word)

A job runs on a stack of its own; `suspended` switches back to the worker's loop, and `Run`
switches into the job's stack. Async queries are out: they would change about 4100 call sites
in 491 files, the 462 tracked functions and up to 3852 `&Db` functions, and need an async fork
of the trait solver (`rustc_type_ir`'s 117-method `Interner`, 2093 solver functions in 80
files). Fibers change no query call site.

### The design (18:33)

- **Runtime structures**: a Chase-Lev deque per core for its queue and a `runnext` slot (the
  LIFO slot), owner-pushed and owner-popped, thieves taking half by CAS; the inbox a
  multi-producer queue; "parked" and "stealable" the same bitmasks, as atomics. As written,
  the driver keeps `Sched`'s queues and masks behind its one lock. Next (20:40), the `Sched`
  splits: a sans-IO wait graph (wait states, keys, counters, timers, the cycle check) stays
  behind the lock, taken only by a wait, a claim, a key's end or a wake; the run queues move
  to the driver as per-core Chase-Lev deques, a runnext slot, an inbox, and parked and
  stealable masks as atomics; placement (nearest parked, least loaded, steal order by tier)
  stays sans-IO, as pure functions over a snapshot of them. A spawn, and an end that wakes no
  one, take no lock. The concurrent parts go through `crate::sync`, model-checked under
  `blaze_check`.
- **Fibers**: a recycled pool (corosensei 0.3.4's mmap'd stacks: reserved, guard page below,
  committed as touched), grown while more jobs are started at once, which admission bounds.
  `crates/blaze-check/src/coro.rs`'s own switch (the model checker's) is to be folded into it
  or deleted, one way per job.
- **Waits on counters**: one suspend per parallel map, as phase 1 already does.
- **Idle**: spin briefly, then park on one atomic word (futex / `__ulock_wait`), waking one
  sleeper per new job.
- **Order**: critical path first, from the profiler (`Spawn::critical`, `set_critical`).
- **Moves**: a per-core "move me" flag read with one relaxed load on query misses only.

No bench gate and no measuring (18:56): no restart comparison and no numeric targets. Phase 2
lands once the self-build passes.

### The work, in order

1. **The fibers driver (written)**: `crates/blaze-graph/src/sched/fiber.rs`, `Driver<W: Work>`.
   One worker thread per core of the shape, set up by a caller's `setup` (binding, QoS class)
   before any job runs. Each worker resumes the fiber `Run` names; a recycled pool of
   corosensei fibers (`Config::fibers` made at start, more made and kept while more jobs are
   started at once; 16 MiB reserved stacks with a guard page) runs job after job and never
   ends until shutdown. Every call into `Sched` is made on the worker's stack after the fiber
   has switched out, so a job is never made runnable while still on its old core's stack. The
   scheduler is one state machine behind one lock (each transition is short and allocates
   nothing warm). `Yield` sets the core's move flag, `Wake` unparks the core's thread, an idle
   worker spins `Config::spin` times then parks (`thread::park`, a futex or `__ulock_wait`),
   and the next timer deadline bounds the park. A job reaches the driver through
   `Cx` (spawn, `map` = one counter and one suspend, claim, `key_done`, `wait`, `wait_job`
   through a generation-checked `Handle`, `miss`, `set_critical`, `now`); code without a `Cx`
   uses `fiber::current_job`, `fiber::wait`, `fiber::claim`, `fiber::key_done` and
   `fiber::miss`. A job's panic is caught on its fiber and raised by `Driver::join` once every
   job ended.
2. **Hook the query wait (written)**: in `crates/blaze-graph/src/pump.rs`, on a fiber,
   `Flights::get_or_run` goes to `get_or_run_on_fiber`: the task is its job (an id in slot 0,
   which no thread has, so the thread-stack cycle check never sees it); a run claims its own
   scheduler flight (the key mixed with its runner and the `Flights`, claimed under the shard
   lock); a waiter suspends on that flight instead of pumping, and the scheduler's `Cycle` is
   `Cancelled`; the runner ends the flight after it has written every waiter's value. A miss
   reads the move flag (`fiber::miss`). Off a fiber nothing changed. While pool threads and
   fibers both run tasks (until step 3), a fiber waiting on a thread's run has no flight to
   suspend on and spins on the waiter until that run ends.
3. **Replace the wait-and-pump** (after the self-build, with lane core): `pump.rs`'s
   pumping, `PUMP_DEPTH`, its per-thread `Stack` and thread-stack edges are deleted; `pool`,
   `par` and `exec` run on the driver; the driver takes over `pool`'s binding (`setup`).
4. **The thread-local and lock audit**: what a job keeps in a thread-local across a wait is
   carried with it: `fiber::carry(Carried { take, put })` registers a pair, `take` called when
   the job parks (leaving the thread's value empty) and `put` on the thread that resumes it;
   a wait that comes back at once moves nothing. Both run under the driver's lock. A region that must not wait (a lock held, a thread-local borrowed) holds a
   `NoWait`; a wait inside one panics, naming the job. Found so far:
   - `crates/blaze-query`: `LOCAL` (`db/local.rs`: active frames, edges, captures),
     `INSIDE`, `STACKS`, `UNREADABLE` (`lib.rs`) are per run and are carried
     (`blaze_query::carry_with_fiber_jobs`, from `Database::blank` and `Storage::new`); on a
     fiber `inside_a_query` is the job's own `INSIDE`; `LANE` (`segment.rs`) stays per
     thread. Done by db (01435).
   - `stdx::ensure_sufficient_stack` (57 call sites through it): stacker keeps the stack limit
     per thread, so on a fiber it either grows a new segment on every call or never grows and
     overflows into the guard page. Fixed (01434): `stdx::fiber_stack_limit`, which
     `ensure_sufficient_stack` checks on a fiber; the driver sets it from each fiber's stack at
     job start, clears it at job end, and carries it (registered by `Driver::start`). A job
     that waits inside a grown segment carries the segment's limit.
   - Thread-local reads must not span a wait inside one function: the compiler may keep a
     thread-local's address across the call, and the job may resume on another thread. The
     driver's and pump's reads sit in `#[inline(never)]` functions; a carried pair's `take`
     and `put` must too.
   - Locks: `pump::Lock` is never held across work or a query call (its own rule); the
     driver's lock is never held while a fiber runs.

Tests (`crates/blaze-graph/src/sched/fiber_tests.rs`), all on the real driver:
`a_waiting_job_holds_no_thread` (a chain of 1000 jobs, each waiting for its child, on one
core), `every_job_of_a_dag_of_waits_runs_once_on_fibers` (Hegel: random DAGs of waits with
parallel maps whose items wait for other jobs, on 1 to 4 cores, every node and item once),
`a_dag_with_maps_runs_every_node_and_item_once`,
`a_cycle_of_waits_on_fibers_is_refused_naming_both_jobs`,
`a_critical_job_that_never_waits_moves_to_a_freed_fast_core_at_a_miss` (it resumes on the fast
core's thread with its stack intact), `a_flight_of_the_drivers_wakes_its_waiter` (IO),
`a_timed_wait_on_fibers_resumes_at_its_deadline`, `a_wait_inside_a_no_wait_region_panics`,
`off_a_fiber_there_is_no_job_to_suspend`, `the_free_wait_suspends_the_running_job`,
`a_key_asked_on_fibers_runs_once_and_its_waiters_suspend` and
`crossed_runs_on_fibers_give_way_once` (through `pump::Flights`), and
`a_carried_thread_local_follows_its_job_across_waits`,
`a_job_recursing_past_its_fiber_stack_grows_and_keeps_its_limit_across_waits`. The self-build's deadlock in the
wait-and-pump (self7; repro's `crates/blaze-graph/tests/scope_wait.rs`) runs on fibers as
`a_scope_job_asking_its_owners_key_on_fibers_gives_way`,
`a_scope_job_asking_its_owners_key_under_a_waiting_parent_gives_way` and
`a_scope_wait_on_fibers_never_leaves_every_core_parked`: a run that waits on a scope whose job
asks for its key. The scheduler sees that cycle whole (the key's flight, its claimer, the
counter, the scope's job), so the wait that closes it gives way.

### What changes

- `crates/blaze-graph/src/pump.rs` (1430 lines): pumping, `PUMP_DEPTH`, the per-thread `Stack`
  and the thread-stack edges go; `Flights` keeps single flight and calls `Sched::claim` /
  `suspended` / `key_done`.
- `crates/blaze-graph/src/pool.rs` (2113 lines): the worker loop becomes `core_idle` → `Run` →
  resume, and parking follows `Wake`. Its API (`submit`, `submit_next`, `submit_critical`,
  `scope`, `install`) maps onto `spawn` with `Spawn`.
- `crates/blaze-graph/src/par.rs` (829 lines): a scope is a counter and one suspend; the 114
  `par::` calls in 36 files are unchanged.
- `crates/blaze-graph/src/exec.rs` (1936 lines): submit becomes spawn.
- The query engine's wait sites, the only places a task waits today: 32 matches of
  `get_or_run` / `run_if_idle` / `wait_until` / `pump::wait` in 3 files, two of them in the
  query engine (`crates/blaze-query/src/db/function.rs:324`,
  `crates/blaze-query/src/lib.rs:2442-2444`). The move-flag check goes on the miss path.
- Per-run state kept in thread-locals across a query call becomes per-job, because a job can
  resume on another core: `crates/blaze-query/src/lib.rs` (`INSIDE`, `STACKS`, `BUFFERS`,
  `UNREADABLE`), `crates/blaze-query/src/db/local.rs` (`LOCAL`: the active frames, edges,
  captures), and `crates/blaze-query/src/segment.rs` (`LANE`, fine as long as it is read per
  write and never held across a wait). The tree has 163 `thread_local!` in 130 files; the rest
  are scratch buffers used within one call with no wait inside, to be audited for exactly that.
- **Risk**: the type system cannot see a fiber switch. Anything `!Send` held across a wait
  (an `Rc`, a `RefCell` guard, a reference into a thread-local) moves threads when the job
  resumes elsewhere. Waits happen only inside the query engine's `get_or_run`, so the audit is
  "what is borrowed across a query call", plus a debug check that a job resumes with no
  thread-local borrow open. The 57 `stacker` / `ensure_sufficient_stack` sites work on any
  stack.

### No IO inside a job (18:31)

A job makes driver requests (io_uring on Linux; kqueue or one IO thread on macOS) and suspends
on their completion: `claim(key, None)` for the request, `Wait::Key` on it, `key_done` from
the driver. The inventory of every IO call reachable from job code today, path:line with its
owner, is [sched/io.md](sched/io.md). In short:
- **cargo**: host reads in `Front::compile` (the profile-use file, native libraries), the
  rustup scan in `toolchain.rs` (an own-nightly bug too), and the library and fetch tasks
  (HTTP, `git`, unpacking) on the IO pool.
- **link**: toolchain discovery (`cc`, `xcrun`, `vswhere` runs, cached per process) and
  per-link reads, `-l` directory listings and linker scripts in `blaze-link/src/target.rs`.
- **codegen**: the external assembler for `asm!` when LLVM is absent; an unconditional
  LLVM-warning `eprintln!`; env-gated dumps.
- **usercode**: `blaze-macro-eval`'s emulated-OS file calls and its warnings.
- **db**: the one front-end read, `include_*!` outside the VFS (`base-db/src/files.rs:557`);
  env-gated dumps; `RUSTD_TRACE` and `BLAZE_BORROWCK` read as hidden inputs.

The rest of the front end and the store read mapped memory, so a cold page is a fault, not a
request. Build-script, test and run starts do their file work on the loop, not in a job.
