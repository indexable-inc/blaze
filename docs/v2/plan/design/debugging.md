---
status: plan
summary: What an agent or a person asking "why was this build slow, wrong or not reused" gets from one command, without reading code, guessing or attaching strace. Fourteen gaps (a to n) that each cost real time on 2026-11-10 and 11, with what closes each and what holds it closed. Each item says whether it is built.
date: 2026-11-11
---
# Debugging blaze without guessing

Andrew, 2026-11-11: "make the profiling and debugging experience as good as possible for you
... think long term ... where you have lacked info". The schema is
[profiling.md](../../../profiling.md); how to read it is the `debugging` skill. This page is
the spec of what was missing. An item marked *built* has its code and test in the tree (not yet
compiled where its commit says so); one marked *specified* is this text alone.

The order of an investigation is now three commands, then the rows:

1. `blaze explain problems`: what blaze itself said was wrong.
2. `blaze slow`: where the time went, and what the profile could not see.
3. `blaze why`: what was computed again and why.

Every command here has `--json` (m), and every named view is also reachable over the daemon's
socket (the explorer's `views` and `view` ops, `explore-wire.md`), so a harness or the app shows
the same rows.

## a. Problems: nothing the daemon knows is wrong lives only in a log (built)

The daemon wrote "the results of `build` were not kept after its answer was sent" to
`serve.log` after every build for hours, and no command said so.

- **One door to the log.** The daemon writes a line to its log only through
  `serve::said::logged!(LEVEL, ...)`, which writes the line and records an event of that level
  (target `blaze`), or through an `eprintln!` marked `// logged: <the event beside it>`. A test
  that reads the daemon's sources refuses any other `eprintln!`
  (`the_daemon_writes_no_line_to_its_log_that_is_not_an_event`).
- **One definition of a problem.** The `problems` view (`sql/examples.sql`): each WARN or ERROR
  event by callsite, with its last message and fields and how often it was said; each panic and
  fatal signal; each `blaze-bug` and `daemon-panic` error; a build that failed, stalled or
  crashed. An event of no build (said after the answer was sent) is the last build's to start
  before it. Newest build first, worst first.
- **Said first.** `blaze explain problems [build=N] [builds=5] [--json]` lists them;
  `blaze slow` lists its build's before the critical path (`Report::problems`, report version
  8); `blaze why` lists its build's before the reasons (and `problems` in its JSON).
  `budgets.py` already fails an edit whose build has a `not_kept` finding.

```
$ blaze explain problems
build_id  level  source                                             what                                                       count  first_ms  last_ms
--------  -----  -------------------------------------------------  ---------------------------------------------------------  -----  --------  -------
41        WARN   blaze event crates/blaze-cargo/src/serve/job.rs:…  a build's results were not kept after its answer was sent…  1      312.4     312.4
40        WARN   blaze event crates/blaze-cargo/src/serve/job.rs:…  a build's results were not kept after its answer was sent…  1      298.0     298.0
2 rows
```

## f. Names, not ids (built)

Provenance said `Crate(Id(21548))` and `` `ScopeInput.scope`(Id(164492)) ``; the crate was
found to be serde by guessing from symbol names.

The run log names each node as the build ends, with the database attached
(`blaze_query::db::take_log_in`). Naming is extended there, at the one place the text is made,
so every reader (`provenance`, `why_reran`, `what_reran`, `untracked_reads`) shows names with
no join and no schema change:

- Any crate may register a namer (`blaze_query::db::name_rows`, up to 8): base-db names a
  `FileText` by its path and a `Crate` by its name, version and target; hir-def names a
  `ScopeInput` and a `CrateResolution` by crate and module path.
- An id inside a memo's key (`Crate(Id(21548))`, `FunctionId(Id(77))`) gets its name after it:
  `Crate(Id(21548) = serde v1.0.219 (lib))`, `FunctionId(Id(77) = serde::de::Visitor::visit_u8)`.
  The id stays, so two rows of one id still join.
- A row no namer names keeps its id; nothing is guessed.

A separate `names` table joined by the views was considered and not built: every reader would
need the join, and the text is made once a build already.

## g. The schema is discoverable (built)

`blaze debug sql` refused queries for unknown columns and `DESCRIBE`; each miss was a VM round
trip.

- `blaze debug schema [TABLE] [--json]`: tables with their one-line meaning, or one table's
  columns, types and meaning, from `schema.rs` (what profiling.md is written from), then the
  views by name. No DuckDB, no daemon.
- `blaze debug sql "DESCRIBE spans"`, `"SHOW TABLES"` and `"DESCRIBE"` are answered by blaze
  from the same source before DuckDB is started.
- DuckDB's "column not found" error is followed by the columns of the tables the query names,
  the nearest names to the one it lacked first.
- New named views: `timeline`, `long_spans`, `waits_by_reason`, `counts_by_cause`, `writes`,
  `scripts`, `pool`, `kept` (each `blaze explain <name>`, and a view inside `blaze debug sql`).

## b. A span's fields are kept (built)

`awaits`, `what` and `function` recorded on a span reached no column.

`spans.fields` (schema 6) holds every field of an ad hoc span that fills no column of its own,
as `key=value` text in the order the callsite declares them, so adding a field needs no schema
change: `WHERE fields LIKE '%why=memory%'`. Fields that name a column (`crate`, `function`,
`unit`, `awaits`, their aliases) still fill it. The lint `blaze::span_field_unrecorded` now
refuses only a field whose name is malformed for that text (not an identifier) or collides
with a span column only a kind may write (`owner`, `steps`, ...).

## c. Time with no span says where (built in `blaze slow`; the watchdog specified)

A 650 ms gap between spans on the critical path; a 3 s `FICLONE` on the loop thread inside a
wait with no span, found only with strace.

- `blaze slow` lists each stretch of the critical path over 20 ms that ran inside no span
  (`Report::gaps`): its thread, the spans before and after it on that thread, and the top
  stacks sampled in it; with no samples it says how to get them. `budgets.py`'s `gap_ms`
  reads the same stretches (`unrecorded` blind spots and `gaps`).
- The loop watchdog's `loop stalled` span carries the step's name in `function`; the syscall
  it blocked in comes from (d).

## h. One command for the edit loop (see the fork's commit)

`blaze bench edits`: applies an edit of a kind (no-op, comment, body, signature, add item,
dependency body), builds, repeats, and prints per run the wall time, the daemon's time, the
timeline, the bounded counters and the problems, with the binary's and the tree's identity;
refuses to compare runs of different binaries without saying so; leaves the workspace as it
found it. Every answer names the binary that gave it; a client served by a daemon of another
binary says so in one line.

## i. A failed build is not a slow one (see the fork's commit)

A build that resumes or redoes work because the checkout's last build failed says so in its
first line and in `blaze why`.

## e. Which process did it (see the fork's commit)

A sandbox refusal carries the program, argv[0], pid, syscall name and number, whether it was
before `main` (a heuristic, said as one) and the module of the instruction pointer.
`blaze explain script <unit>` prints the last run's trace summary.

## k. A corrupt persisted database stops at restore (see the fork's commit)

A restore verifies checksums, the manifest and that every id a restored row references
exists; one that fails names the file and its format version and never serves or starts
empty. `blaze doctor` runs the same verification on demand and prints the command that
quarantines the file.

## j. `blaze why` never guesses (see the fork's commit)

When an explanation is unavailable the answer says exactly why and how to have it next time.

## l. A hung user-code run says what each process is blocked in (see the fork's commit)

After a bound, the stall report lists each process of the run with its state and the system
call or wait channel it is blocked in, from `/proc`.

## d. System calls without strace (specified; the contained part built)

`blaze debug syscalls --loop | --thread NAME | --unit UNIT [--over MS] [--json]`.

What is recorded, always on, per call over a threshold (1 ms; 20 ms is the loop's budget): the
call, its duration, the thread, the file's path where the caller has one, and the span it was
in. Sources, cheapest first:

1. **The driver's own timing.** Every effect the io driver runs (a ring operation's submit to
   completion, an off-ring call such as `FICLONE`) is already bracketed where it is issued; one
   over the threshold becomes a `syscall` span (`function` the call, `unit` the path). This
   covers the daemon's own IO, on Linux and macOS, with no privilege. *Built for the off-ring
   calls and the loop's inline steps; ring operations specified.*
2. **Blocked samples.** The sampler already records `cpu = 'off'` samples of threads blocked
   inside a span. On Linux it reads `/proc/self/task/<tid>/syscall` for a thread it finds
   blocked (number and arguments, no ptrace, no seccomp), and names the fd's path from
   `/proc/self/fd`. On macOS, without root, `proc_pidinfo(PROC_PIDTHREADINFO)` gives the
   thread's run state and `thread_get_state` its pc, named through the image; kperf's kdebug
   syscall tracing needs root and is not used (rule macos-private-apis: said in
   docs/macos.md). *Specified.*
3. **The view.** `syscalls`: the `syscall` spans and the blocked samples by thread and span,
   which `blaze debug syscalls` prints. `harness/v2/loop_syscalls.py` (strace) stays as the
   cross-check from outside.

## m. Agent-readable everything

Each command above takes `--json`: a view's rows as JSON lines with the view's columns; a
report (`blaze slow`, `blaze why`, `blaze doctor`, `blaze bench edits`) as one object with a
`version`. The views are served by the daemon's socket API as they are to the CLI.

## n. The skill

`.agents/skills/debugging/SKILL.md`: the first three steps, the new views and commands, and
what to check before trusting a measurement (the binary, the tree, the build succeeded, the
daemon is the one you think, compare from the second edit on after a cold build).
