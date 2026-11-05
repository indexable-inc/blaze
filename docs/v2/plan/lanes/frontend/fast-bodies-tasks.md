---
status: plan
summary: frontend-fast-bodies-tasks — a check as one plane task per body and one per file module for its items, slots read in def-map order so the output is the same whatever ran first; a build tells the check the bodies its codegen reaches as the walk finds them, and those are checked first, while the unit's outcome still waits for every body.
date: 2026-10-29
---
# frontend-fast-bodies-tasks: bodies as plane tasks, reached ones first

Part 2 of section 5 of [fast.md](fast.md), under [fast-bodies.md](fast-bodies.md) section 2.
Costs are counted from the code as of 2026-10-29, not measured (nothing builds yet). Machine
figures: the top of [../db/speed-of-light.md](../db/speed-of-light.md).

## The work and its bound

- **A body's check** is its inference (bound ≈ 40K cycles, ≈ 11 µs; ≈ 30 µs today,
  [../db/speed-of-light-infer.md](../db/speed-of-light-infer.md), "Summary"), then its unsafety
  check, body validation, match checks, MIR and borrow check, const check. 11 µs is the floor of
  a typical body's check; the rest only adds to it.
- **A check of a crate** is every body's check and every item's (signatures, well-formedness,
  coherence, missing items). Bound: total work over the cores, or the longest single task,
  whichever is longer. With one task per body the longest task is one body; with today's runs
  of 32 items it is a run (up to 32 bodies, ≈ 1 ms at today's cost), which is the tail of the
  whole check on a machine with idle cores.
- **The overhead of one task** (what scheduling a body costs, against its 11 µs):
  - the job of `Db::run_all` (`run_on_pool`, `base-db/src/db.rs`): a job cell taken and freed
    (≈ 10 ns, [../mem/core.md](../mem/core.md)) and the cross-thread handoff of one job (one line
    moved between cores, 60 to 100 ns, same table), plus `frame::task`'s open and reset (a few
    ns);
  - the reached ring's test: two loads of read-mostly lines (`head`, `tail`), ≈ 2 to 10 ns;
  - the claim: one `fetch_or` on a word of 64 bodies' bits, ≈ 20 ns alone and up to ≈ 100 ns
    when the line moves between cores.

  About 200 ns at worst: 1.8% of the 11 µs bound, 0.7% of today's 30 µs. **No batching of
  bodies**: a batch would save at most that and give back the tail it makes.
- **Where the arithmetic says to batch.** An enum variant is a body (its discriminant), almost
  always empty: a few memo executions, well under the 11 µs a task is sized for, so the 200 ns
  would be a large share. Variants stay in their module's task (checked inline, as today). A
  body that is cheap for another reason (a required trait method with no default) is not known
  to be cheap without lowering its signature, which the plan must not do on the calling thread
  (below), so it is a task of its own: a few µs, overhead under 10% for that body and a
  negligible share of the crate.
- **Listing the bodies** (the plan): one pass over each file module's scope, its inline
  modules, traits' and impls' items, mirroring the collector's walk (≈ 5 ns a body, ≈ 10 ns per
  memo hit of `ImplItems`/`TraitItems`). `ImplItems::of` may be cold at the start of a check
  (method resolution's tables hold impl ids, not items), and a cold one parses the impl, a few
  µs: so the listing is itself a batch of `run_all`, one task per file module, and its work is
  what the module's item checks would do first anyway (the second read is a memo hit). Its
  cost is one barrier, ≈ one module's listing.
- **Reached first.** A build's critical path is codegen of what its roots reach; a body's check
  shares that body's inference and MIR with codegen's `describe` (one memo). The bound is that a
  body codegen is about to describe is never queued behind bodies nothing reaches: the latency
  from "the walk found it" to "a worker starts its check" is at most one body's check on every
  busy worker, ≈ 11 to 30 µs.

## What the code does today

- `hir::check::check_crate` (`crates/hir/src/check.rs`) lists file modules
  (`file_modules`), groups them into runs of at least `ITEMS_PER_TASK` = 32 items (`tasks`), and
  runs each run as one task of `Db::run_all`; each module's diagnostics go in its slot
  (`OnceLock`), read in the def maps' order.
- A module's task is `DiagnosticsCollector::collect` (`crates/hir/src/diagnostics.rs`), which
  walks the module: its def-map diagnostics, macro calls, then each declaration
  (`collect_module_def`): signature, then the body (`collect_def_with_body`: expression store,
  inference errors, unsafety, validation, match checks, borrow check, const check), then
  well-formedness; inline modules recursively; then legacy macros; then impls (signature, their
  items' bodies, coherence, orphan rules, safety, missing items). Body diagnostics come out
  interleaved with item diagnostics in that order.
- A build's unit (`crates/blaze-cargo/src/serve/unit/compile.rs`, `make()`) runs
  `blaze_diagnostics::check_crate` of the whole crate and only then, if nothing failed,
  `codegen::generate`: codegen waits for the whole check.
- The reachable set is computed in `crates/blaze-codegen/src/driver.rs`, `Build::walk`: a
  breadth-first walk from the roots (`HirFrontend::roots`: `main`, tests, exported functions,
  the allocator shim), a level at a time; each level's instances are described in parallel
  (`Frontend::describe`, `hir-ty-codegen/v2/frontend.rs`, which builds the instance's MIR, so
  infers its body), and each description's `refs` (callees, statics) are the next level. The
  front end hears nothing of the walk but the `describe` calls.

## The design

### One task per body, one per file module's items

The plan (`crates/hir/src/check/bodies.rs`, `Plan::new`) lists, per file module, the bodies the
collector would meet at the module's level, in the order it meets them: functions, consts,
statics and unnamed consts of the module and its inline modules, and the functions and consts of
its traits and impls. Enum variants are not listed (above). Nested items (inside a body, or a
block in a signature) are not listed: they are checked inline by whoever meets them, as today.

`Plan::run` is one `run_all` batch of `modules + bodies` tasks, in the order module, its bodies,
next module. A module's task runs the collector with a `Defer` (`collect_deferred`): when the
collector meets the next listed body, it marks its place in the module's diagnostics and leaves
it. A body's task runs `collect_body` (the body half of `collect_def_with_body`). A body the
collector meets out of the listed order is not taken: it is checked inline and the listed one
stays unmet, and the module's task stops with a panic naming the module and body (the listing
and the collector disagree, a bug, never a quiet change of output).

Assembly (`Plan::assemble`) reads each module's diagnostics and puts each body's slot at its
mark: the result is exactly the sequence the collector gives walking the module whole
(`check_module`), whatever order the tasks ran in and on whichever threads.

### Reached first

The queue (`Queue` in `bodies.rs`) has per body a `claimed` bit and a `posted` bit, and a ring
of reached bodies (`reached`, `head`, `tail`; each body posted at most once, so the ring is the
body count long and never wraps). Every task, before its own work, pops the ring and checks each
body it claims; then it does its own module, or its own body if no one claimed it. A task is a
job of `run_on_pool`; the jobs start roughly in index order, so the unreached bodies go in
def-map order, and a reached body is taken by the next task that starts on any worker.

`Plan::reached(&[DefWithBodyId])` posts bodies, from any thread, before or while the check runs;
a body the plan does not list (another crate's, a generic of a dependency) is passed over. The
index from body to its place is built on the first call (`OnceLock`), so a check nobody tells of
a reached body builds none.

A task unwound by salsa's `WouldWaitOnItself` runs again in `run_on_pool`'s next round. Each
task keeps the body it is checking (`running`, one word per task), so its run again takes that
body up first: a claimed body is never left unchecked.

### The interface with codegen and fastcompile (requests)

- `blaze-codegen`'s `Frontend` (`backend.rs`) gains `reached(&self, functions, statics)`,
  called by `Build::walk` with each level before it is described, and `stopped(&self) -> bool`,
  read by `Build::run` between its phases and by the walk at each level: a build whose check
  failed stops its codegen at the next step, whose output nobody reads.
- `hir-ty-codegen`'s `HirFrontend` holds an optional `&dyn BodyChecks` (one dynamic call per
  walk level and per phase: cold), set by `with_checks`. Its `reached` maps each instance to the
  body whose check covers its code (`body_of`: a function or vtable shim to its function, a
  closure or coroutine to its owner's body, `start` to `main`; shims of no body to none) and
  each static to its initializer's body.
- `blaze-cargo`'s new `serve/unit/checks.rs` (`Checking`) holds the unit's plan, implements
  `BodyChecks`, and runs the check as a job of the pool beside codegen on the calling thread
  (`alongside`). `make()` builds code only from the check's outcome: if any body failed, the
  unit fails with the check's diagnostics, and whatever codegen made is dropped.
- Fastcompile builds through the same driver: `Build::walk` is the only caller of
  `Frontend::describe` in the tree, so its builds tell the check what they reach with nothing
  of its own to change (told to lane fastcompile).

### The order of checks across subsystems (a limit, stated)

Reached and unreached bodies are ordered among the check's own tasks. Codegen's walk and
compiles run on rayon's threads, which the OS shares with the pool's: an unreached body's check
does not yield a core to codegen. Making it do so needs a low class in the pool (a queue a
worker takes from only when its own and its group's are empty) and codegen on the same pool;
both are lane core's and codegen's, outside this part.

Two more, from where `make()` runs. A unit's compile is itself a job of the pool, so the check
job it submits lands in that worker's LIFO slot: another worker takes it when one is free, and
when none is (every worker on another unit), it runs once codegen is done, in the scope's wait.
The outcome is the same and no core idles; only the overlap is lost while the pool is full. And
the check job's memory is counted by the arena measure of whichever task's worker runs it, not
always the unit's: a unit's reported peak can leave its check out. A low class in the pool
(above) fixes the first; the second needs the measure to follow a scope's jobs (lane core).

## What rustc behavior this keeps

- **Which programs are rejected.** rustc checks every body of every crate it compiles, in
  `cargo build` as in `cargo check`. Reachability decides only the order: the unit's outcome
  waits for every listed body's check (the `run_all` batch returns only when every task ran),
  fails with the check's codes and spans if any body fails, and its outputs are withheld until
  then (`make()` reads codegen's result only after the check passed). The test
  `a_type_error_nothing_reaches_still_fails_the_check` holds this.
- **Diagnostic order.** The sequence is the collector's whole-module walk in def-map order,
  byte for byte, which `blaze-diagnostics` then sorts by rustc's phase (stable), as today.
- **Codes and spans** come from the same collector code; nothing here builds a diagnostic.

## Prior art

- rustc's parallel front end: `par_hir_body_owners` and `par_hir_for_each_module` on rayon,
  one body or module per job, with its diagnostics in whatever order the threads emit them.
  This matches its granularity and, unlike it, gives one order whatever the threads.
- Work stealing with a priority lane: Cilk-style deques with a shared high-priority FIFO, as in
  Tokio's LIFO slot plus global inject queue, and the "steal half, check the injector first"
  rule of Go's scheduler. Here the high lane is a bounded lock-free ring read before any task's
  own work.
- Zig's compiler checks function bodies lazily as semantic analysis reaches them; that is the
  order here, but rustc's rule (check everything) is kept.

## Tests

`crates/hir/src/check/bodies/tests.rs`:

- `a_check_is_the_same_whatever_order_its_bodies_ran_in`: a fixture with bodies at every level
  (functions, impl and trait methods, consts, unnamed consts, statics, an inner function, an enum
  with a discriminant, file and inline modules), checked under `RunOrder::InOrder`, `Reversed`,
  `Strided(2)`, `Strided(3)` and with reached bodies posted in reverse: the same diagnostics.
- `a_check_by_bodies_is_the_collector_walking_each_module_whole`: in one database, the plan's
  output equals `check_module` of each file module concatenated, by `Debug`.
- `a_type_error_nothing_reaches_still_fails_the_check`.
- `reached_bodies_are_checked_before_the_others`: the order the plan ran its work in, under
  `InOrder` and `Reversed`: the posted bodies first, in the order posted.
- `a_body_the_plan_does_not_list_is_passed_over`.
- the queue alone: each body is posted once, a claimed body is not posted, the ring is first in
  first out, every body is claimed exactly once with posters and claimers on several threads,
  and posting, popping and claiming allocate nothing.
- the existing `a_check_is_the_same_whatever_order_its_modules_ran_in`,
  `a_check_of_a_crate_is_its_modules_checked_one_by_one` and
  `a_check_of_some_bodies_leaves_the_others_out` (`crates/hir/src/tests.rs`) still hold;
  `small_modules_share_a_task` goes with the runs of modules.

`crates/blaze-cargo/src/serve/unit/checks/tests.rs`:

- `a_check_fails_its_unit_on_an_error_cargo_shows_and_nothing_else`: what stops codegen is
  exactly what fails the unit in `make()` (an error, a denied lint; not a warning, not a closing
  line cargo hides), so a passing unit is never stopped.
- `the_check_runs_beside_codegen_and_both_are_waited_for`: each side waits to see the other
  begin, so they ran at once; both results come back.
- `a_panic_of_the_check_is_raised_by_its_caller`.

`crates/blaze-codegen/src/driver/tests.rs` (requested of lane codegen):
`the_front_end_hears_each_level_before_it_and_a_stop_ends_the_build`: levels `[1]`, `[2, 3]`,
`[4, 5]` told in walk order; with the front end stopped, the build errs and prepares nothing.
