---
status: plan
summary: "One typed stage protocol for the check engine (lane ST1, 2026-11-18): a stage says how its tasks are planned and what one task does through a TaskCx; one sans-IO driver (crates/blaze-stage) owns the schedule, frames, arena, commit, merge in task order, stop, the profile and how a failure reaches its waiters. The laws (a)-(g) with a planted bug each, the memory law with its floor, and the map and order for moving the seven stages."
date: 2026-11-18
---
# One stage protocol

The engine's seven stages (Units, Scripts, Outlines, Names, Heads, Bodies, Lints) each wrote the
phase protocol by hand: its own `run_phase`, its own `may_overlap` loop, results held as values until
the merge, its own handling of failure and stop. On the 700-crate run that cost a stop that waited
800 s (Heads had no stop hook inside its loop), a 90 GB peak against a 24 GB limit (read lists held
three or four times as values until the merge), a build script's unkeepable outputs that had no way
to be said, and failures whose cause was lost. The class (rule 54) is "each stage owns the
protocol". The fix is that no stage owns it: `crates/blaze-stage` holds the one protocol, and its
types leave a stage no way to break it.

## The protocol

```rust
trait Stage: Sync {
    type Job: Copy + Send;          // one task's input
    type Out: TaskOut;              // Copy + 'static, <= OUT_MOST (64) bytes: ranges and counts
    type Fail: TaskFail;            // Copy + 'static, <= FAIL_MOST (48) bytes: a Code and fields
    type Read: Copy + Ord + Send;   // what one read names (the engine's cell and digest)
    const KIND: blaze_engine::Stage;
    fn plan(&self, plan: &mut Plan<'_, Self::Job>) -> Result<(), PlanError>;
    fn run(&self, job: Self::Job, task: &mut TaskCx<'_, '_, Self::Read>)
        -> Result<Outcome<Self::Out, Self::Fail>, Stopped>;
}
enum Outcome<O, F> { Made(O), Unkept(O), Failed(F) }
```

`TaskCx` (crates/blaze-stage/src/cx.rs) is everything a task may touch:

- `scratch()`: the worker's frame, reset by the driver after every task;
- `put(kind, key, bytes) -> PutRef`: a record, its bytes in the lane's byte column and its
  (kind, key, digest, span) row in the puts column;
- `read(cell)`: appended to the lane's ONE reads column; `finish` sorts and deduplicates the task's
  range in place, so a read list is held once, in the arena, as a span; `read_uncovered()` counts a
  read that no key covers (it makes the task `Unkept`);
- `say(code, place, args)`: a finding as a `Code`, an item-relative `Place` and typed `Arg`s, with no
  rendered text (rule 81);
- `stopped() -> Result<(), Stopped>`, and the same `Err(Stopped)` from every other call once a stop
  is asked or a column is at its cap;
- `open(name)` / `close(open)`: a span row (a guard type would hold `&mut TaskCx` across the body).

`Stopped` has a private field, so only the context makes one, and a halt reaches the driver even
when the stage drops the `Err`: the context remembers it in `TaskRanges::halted`.

## The driver

`machine::Phase` (crates/blaze-stage/src/machine.rs) is a sans-IO state machine. The thin driver
(the engine's, in blaze-cli) feeds it events with the request clock's `now` and carries out each
effect in order.

| | |
|---|---|
| States | `Planned`, `Running`, `Committing`, `Merged`, `Stopped`, and `Refused(Refusal)` (a commit that failed or missed its deadline) |
| Events | `TaskDone { task, ranges, done }`, `WorkerStalled(w)`, `Stop`, `CommitDone(result)`, `Deadline` |
| Effects | `Start { task, worker }`, `ResetFrame(w)`, `Commit`, `Merge`, `Report(Status)`, `DropArena`, `LeakArena { lost }` |

A task's end is only the driver's: `End { Made, Unkept, Failed(Cause), Waited { on, root }, Stopped,
NotRun }`. `Cause` is `Stage(F)`, `Panicked`, `Stalled`, `Full { column, cap }` or `NoWorker`.
`Waited` is made only by the driver: `on` is the lowest failed input, `root` the failure it carries
(rule 19: the printer groups waiters by root, `Phase::waits`). `Done::Panicked` from the thin
driver's unwind catch fails only that task (rule 20). Every wait has a deadline (rule 18): a running
task past `Limits::stall_after` fails as `Stalled` and its worker is lost until its late return; a
commit past `commit_within` refuses the phase; with every worker lost, the tasks still to start fail
as `NoWorker` one stall later. `next_deadline()` gives the instant to wait until.

## The laws

Each is checked by the simulation (crates/blaze-stage/src/sim_tests.rs, on blaze-sim), and each has a
planted bug that the simulation catches (crates/blaze-stage/src/plant.rs):

| Law | What holds | Planted bug |
|---|---|---|
| (a) | every task ends in exactly one `End`, with at most one profile row | (checked in every run) |
| (b) | every `Waited` names a root that truly `Failed`; nothing waits on an `Unkept`; a task with an `Unkept` input is never `Made` | `WaitedOnDirect`: the root is the direct input |
| (c) | `Merge` only after `CommitDone(Ok)`: no record is readable before its commit finished | `VisibleBeforeCommit` |
| (d) | after a stop no task starts, and the partial profile holds every task that ran | `StartAfterStop` |
| (e) | what one task made in its worker's frame cannot be reached by the next task on that worker | `FrameNotReset` |
| (f) | the merge order and the committed bytes are the same for every completion order | `CompletionOrder` |
| (g) | the phase's arena holds no live block at its drop | (checked in every run, by `Root::used().live_blocks`) |

The rule-23 control `an_uncovered_read_is_unkept_not_failed_and_its_readers_too` removes the cause
(an uncovered read) and watches the effect (`Unkept` and its spread) go.

## The memory law

After `DropArena` nothing of the phase is live: every column, slot, row and dependency edge is in
the phase's arena (one `blaze_mem::Root` refusing a live block at its drop), and every task's
scratch is in its worker's frame. Per task, after it returns, the driver holds one slot.

- The floor is `size_of::<Out>() + size_of::<Held>()`: its out and its ranges (`Held` is a lane and
  three spans, 28 bytes). The read list itself is a span into the reads column, held once.
- The measured figure is `size_of::<Slot<Out, Fail>>()`. `Status` reports both, and the simulation
  prints both. Neither has been measured yet, because nothing was compiled. When the slot is an
  enum, it exceeds the floor by its tag and the largest of the `Waiting` state (12 bytes) and
  `Cause<F>`. The aim is under 1.25 times the floor. The ratio is to be quoted from the first run.
- On the big run that means about 100 bytes a task where today a task holds its read list as a
  `SVec` of 40-byte `(CellId, ContentId)` pairs, three or four times over (see Heads below).

## Where this differs from the brief, and why

- `TaskCx<'t, 'p, R>` has two lifetimes: `&'t mut Lane<'p, R>` is invariant in `'p`.
- `Stage::Read` is an associated type, so the protocol does not name the engine's cell type. Moving a
  stage fixes it to the engine's `(CellId, ContentId)`.
- `run` returns `Result<Outcome, Stopped>`, so `?` carries a halt out. `Stopped` holds
  `Halt::{Asked, Full}`.
- The columns are per worker lane, so a task's ranges point into its worker's lane and no lock is
  taken per put.
- `Unkept` is contagious: a `Made` task with an `Unkept` input becomes `Unkept`, and it is used by
  this command and owed again by the next.
- On a stop, the finished `Made` records are committed by key, so the work is not lost, and nothing
  is merged.
- `LeakArena` replaces `DropArena` when a stalled worker may still write into its lane. The leak is
  the price of not waiting forever, and the report says so.
- A `Stalled` cause carries no time, so an end is a function of the plan and never of the schedule.
  The row has the times.
- `span(name)` became `open`/`close`.

## The map: the seven stages in this protocol

Read from the tree on 2026-11-18. All paths are under crates/blaze-cli/src/. The sizes are the
700-crate run's figures from the brief: 36 GB held as Heads read lists, 20 GB in Bodies, 15 GB in
the print tail.

**What every stage shares today:**

- `Done` (crates/blaze-engine/src/engine.rs:306, `failed` at :318; built at
  engine_script_reads.rs:295 and engine_lints.rs:258) has only
  `failed: bool`, and no unkept or owed state. It must get one before Scripts or Bodies can say
  `Unkept` (blaze-engine, lane E6).
- The command's stop is checked only between stages (engine_check.rs:784, :896; engine_status.rs:222)
  and inside a few loops. The `stopped` at engine_names.rs:230, engine_lints.rs:211,
  engine_scripts.rs:427 and engine_heads.rs:350, :1407 is the pool's join error, not the command's
  stop.

| Stage | Job | Out | Fail | What it holds that the protocol forbids | Stop and failure gaps |
|---|---|---|---|---|---|
| **Units** | none (one task: discover) | the graph's key and counts | `UnitsCause` (engine_units.rs:105-149) | `discover` (:376) and `Failure<'f>` (:167) hold names and words in the frame until printed | a single task; no stop inside; its failure is printed whole, not as a code |
| **Scripts** | one build-script run (unit, script) | puts' span, reads' span | a code for "a build script failed", "printed `cargo::error`", "no fetched sources" (scripts.rs:564-580, :872-875) | engine_scripts.rs:55 holds `reads: Vec<(Name, Vec<Name>)>` per task, nested; the reads are copied into `SVec<(CellId, ContentId)>` at :497 and :544; its own loop at :297, :424 | a read no cell names is already `unkept` (scripts.rs:895-899), but `Done` cannot carry it; no stop inside a run |
| **Outlines** | one file | outline out and findings count (two cells: WHOLE, SURFACE, HEADS) | the planner's refusal | engine_check.rs:1430 `SVec` reads per task; `Done.defines` is a `SVec` per task (engine_tasks.rs:128-142) | the only stage already on the shared pool (engine_pool.rs:99, engine_check.rs:1527); stop via `board.start` |
| **Names** | one crate's scope | scope out, defines' span | a scope refusal code | engine_names.rs:323 `SVec<(CellId, ContentId)>` per task, held until the merge; own loop at :114, :218 | no stop inside the loop |
| **Lints** | one body's lint walk | findings' span and count | none (a lint finding is a `say`, not a failure) | engine_lints.rs:77 `Made` held per task; own loop at :138, :205; `Done` with empty reads at :258 | no stop inside the loop |
| **Bodies** | one body (or one cut) | the body's out and the says' span | a code for each refusal (engine_bodies.rs:396, :428 `NoNamesRecord`, …) | read lists per task as `SVec<(CellId, ContentId)>` at engine_bodies.rs:799, :818, :831, :1722, :1892, :1925; `Global` vectors at :1063, :1108; refusal words formatted at :1859; about 20 GB | stop checked at :1583, :1700 only; the cap path loses the cause: `Own::PastCap { kept }` (:2157, :2181) from engine_seeds.rs:683, :1170 names a count, not the failed task |
| **Heads** | one head task (a file under each unit that holds it) | the out, defines' span, payload's span | a code per refusal (engine_head_notes.rs) | each task's reads held 3-4 times: `WorkedHead.reads` (engine_heads.rs:1088, `Vec<CellId, Global>`), `HeadFailed.reads` (:1103), `UnitsMade.reads` (:1120), the `SVec` at :1505, and `record_reads` (:1509, `Global`); the impl-table task does the same at :149 and :418; `WorkedHead.payload` (:1091) and `failed` (:1093) are `Global` vectors; about 36 GB | stop checked at :242 and :1722 only, not between a task's units; own loops at :220, :344, :1400 |
| *print tail* | (engine_lines.rs, engine_check.rs:1066-1078) | | | every block is kept as rendered words until printed; about 15 GB | which task a `Waited` waited on is not kept; the protocol's `Waited { on, root }` and `waits()` replace it |

## The order to move them, simplest first

1. **Lints.** It has no failure, no reads to keep and one loop, so it proves the thin driver and
   `say`.
2. **Units.** It is one task, so it proves `plan` and `Report` with no concurrency.
3. **Outlines.** It is already on a pool, so it replaces engine_pool.rs and gives the first measured
   ratio for the memory law.
4. **Names.** Its read lists move into the reads column, and the first memory saving is measured.
5. **Scripts.** It needs `Unkept` in the engine's `Done` (E6) first, and gives the first real
   `put`s.
6. **Bodies.** It needs the cap path's root cause (`Waited { root }` replaces `Own::PastCap`), and
   it is the 20 GB.
7. **Heads.** It is the largest stage, with one read list for the union of its units and the stop
   moved between units. It is the 36 GB.

The print tail moves with Bodies: it renders from `say` rows and `waits()` at print time.

Each move deletes that stage's `run_phase` loop. When Outlines moves, engine_pool.rs is deleted.
Nothing in this change moves a stage.
