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

`spans.fields` holds every field of an ad hoc span that fills no column of its own, as
`key=value` text in the order the span recorded them (cut at 1 KiB), so adding a field needs no
schema change: `WHERE fields LIKE '%why=memory%'`. Fields that name a column (`crate`,
`function`, `unit`, `awaits`, their aliases) still fill it; a value given later
(`span.record`) is added to the text. A span too short for a row of its own (`folded`) loses
its fields as it loses its `function`. The lint `blaze::span_field_unrecorded` now refuses
only a field whose name the text can't hold apart (not letters, digits and `_`) or that is a
span column only a kind may write (`owner`, `steps`, ...).

The schema's version stays 5: `fields` is a nullable column at the end of `spans` (the last
typed column of the registry), which `schema.rs` says never bumps it; a file from before reads
it as null through the table's schema file. The flight recorder's mapping is refused across
the change by its own fingerprint, which hashes the typed columns' names.

A text column was chosen over rows of a `span_fields` table (as `event_fields` is to
`events`): a span's columns are words of its slot, written as its row closes, so one more
name costs a hash and a thread-local lookup; a row a field would cost a ring row each for
every span that says one, on a path events are not on.

## c. Time with no span says where (built)

A 650 ms gap between spans on the critical path; a 3 s `FICLONE` on the loop thread inside a
wait with no span, found only with strace.

- `blaze slow` lists each stretch of the critical path of 20 ms or more under no span
  (`Report::gaps`, report version 9): how long, when, its lane and thread, the spans of its
  lane before and after it, and the stacks sampled on its thread in it; with no sample it says
  how to find what ran. `budgets.py`'s `gap_ms` fails an edit on the same stretches: the
  `unrecorded` blind spots summed, as before, and now each `no_span` gap on its own, naming
  the thread and the spans around it.
- The loop watchdog's `loop stalled` span already carried the step's name in `function`, but
  it is opened after the step, so it was shorter than any span the recorder writes a row for:
  folded into a count, with no `function` and no row for `budgets.py` to find. It is now
  `held` (never folded), and the stall is also a WARN event with `step` and `ms`, so it is a
  problem (a). The system call it blocked in comes from (d).

```
critical-path time under no span, by stretch (20.0 ms or more each):
 - 650.0 ms at 0.3 s on blaze-loop (tid 4711), thread 4711: a thread with no job, between two of its spans
     after `lower` serde v1.0.219 (lib), before `task` unit bodies (infer)
     sampled blocked in it 31x: ioctl < blaze_core::uring::clone_range < blaze_cas::place
   fix: put a span over what ran there (rule slow-is-p0-traced), then read it again
```

## h. One command for the edit loop (built: bd24ee432d, 436278ae06)

`blaze debug edits KIND [--file F] [--fn NAME[:LINE]] [--runs N] [--json] [-- BUILD ARGS]`
(named `debug edits`: `blaze bench` is cargo's) applies the Nth edit of one kind (no-op,
comment, body, signature, add-item, dep-body) to the file as found, runs `blaze build` with
this very executable, and repeats. Each run reports wall time, `builds.dur_ns`, the `timeline`
rows, the bounded `query_counts` and the `problems` rows. The first line is the medians from
run 2 on, or `FAILED:` naming the run and the build's own exit code, after which nothing is
timed. A run fails if its `builds.blaze` is not this binary's BLAKE3, or if no build or more
than one started in the daemon during it. The file is written back on every way out; a journal
in the state directory restores it after a kill. Each good measurement is appended to
`<state>/edits.jsonl`; the last record of the same workspace, kind, file and function is shown
beside this one, labelled `ANOTHER BINARY` or `ANOTHER TREE` when it is.

`blaze --version` prints `binary: <BLAKE3> <path>` after its first line, and `status`, `debug`,
`explain`, `why` and `slow` print one warning when the daemon that answers is another build
than the client. `builds.blaze` is filled for every command (verified).

## i. A failed build is not a slow one (built: b52d9192b8, 8c9a1eb79d)

The daemon keeps, per checkout, its last build that did not end well. The next build says so
as its view opens ("the last build of /w (build 41, `build`) failed with 212 of its 340 units
not built ...: this build first does the work that one left, so its time is not a no-op's"),
its misses take one row of step `work the last build left unfinished`, which `blaze why` says
before every other group and `blaze slow` ranks first (finding `unfinished`). Spec:
[why.md](why.md).

## e. Which process did it (built on Linux: f87fe256af, 3f58c4890b)

On the first untraceable call of each kind per process, the sandbox supervisor reads
`/proc/<pid>/{exe,cmdline,status,maps}` and records the program, `argv[0]`, the pid as the
supervisor and as the sandbox number it, the system call's name and number, the instruction
pointer and the module it is in, and whether it was before `main` (a stated heuristic: yes
when the call came from the loader or before the interposer said it loaded, else "not
known"). The refusal appends who read it. `blaze explain script UNIT [--json]` prints the last
run's trace summary from the events the daemon writes as the run ends (`script run traced`,
`script call`), joined with the run's `processes` row. On macOS the interposer cannot say
which process, and the refusal says so.

## k. A corrupt persisted database stops at restore (built: 0dfebb7bbf)

The daemon image's references are checked as it opens: a checksum-valid record that names
what the image lacks is `DbError::Dangling`, the start is refused naming the file, and nothing
of it is used. The engine's "an ID this database never gave" panic names the file, layout and
hash the process restored from. `blaze doctor [--json]` runs the same checks on the files with
no daemon, prints the command that sets the state aside whole, and deletes nothing. Spec:
[restore.md](restore.md). The cause of 2026-11-10's forged key was not found.

## j. `blaze why` never guesses (built: 45ffd373db; the cap's accounting fixed by 2449b43295)

Each reason says what was recorded; where the record cannot tell two things apart it says
both and that which is not recorded; where the record is gone it says why and the command
that reads what is left (`blaze explain key_part_diff after=N before=M`). Spec:
[why.md](why.md).

## l. A hung user-code run says what each process is blocked in (built on Linux: 3d43868436)

When a build script's run is killed for making no progress, the error appends a line a
process of the run: its state, the system call it is in, the file its descriptor is open on
and the kernel's wait channel, from `/proc`, bounded to 8192 entries, 32 processes and 200 ms.
On macOS it names `ps -o state,wchan` and `sample PID`. Proc-macro and test hangs do not go
through this bound yet.

## d. System calls without strace (the view and command built; two sources specified)

`blaze debug syscalls [--loop | --thread NAME] [--unit TEXT] [--over MS] [--build N] [--json]`
prints the `syscalls` view: what a build's threads blocked in, as the daemon itself recorded
it, in the order it began. `--loop` is the daemon's loop thread (the thread of its
`request: read to reply`, `loop: ...` and `loop stalled` spans).

```
$ blaze debug syscalls --loop --over 20
at_ms    ms       thread      seen_as            call                                   inside                times
-------  -------  ----------  -----------------  -------------------------------------  --------------------  -----
212.4             blaze       loop step in line  place outputs: 889 ms                                        1
214.0    889.012  blaze-io-2  io call            io clone /w/target/debug/hello         loop: place outputs   1
2 rows
```

What it reads today, each already in the profile, on Linux and macOS with no privilege:

1. **The drivers' file calls off the loop** (`io call`): `blaze_core::lock::io_wait` brackets
   each clone, write-back and listing a driver makes off its ring in a `wait` span whose
   `awaits` is `io <op> <file>` (commit 97d29fb72c), so the call, its file, its time, its
   thread and the span it was made in are rows.
2. **The loop's steps that ran in line past 20 ms** (`loop step in line`): the `loop stalled`
   spans (c).
3. **Blocked samples** (`blocked samples`): the sampler's `cpu = 'off'` samples of threads
   blocked inside a span, by stack and span, with the time they stand for at 49 a second.

What is specified and not built, in the order to build it:

- **Ring operations.** The io_uring driver stamps each submission; a completion that arrives
  more than 1 ms after it becomes a `wait` span of kind io, `awaits` `io <opcode> <file>`, as
  the off-ring calls are. That makes every slow operation of the daemon's own IO a row, always
  on, at the cost of a clock read per submission.
- **The blocked thread's system call (Linux).** The release sampler (perf_event) takes no
  off-CPU sample, and a blocked sample has no stack. The stall watchdog's reader thread, once
  a second, reads `/proc/self/task/<tid>/stat` for each thread with an open span and, for one
  in state `S` or `D` longer than 20 ms, `/proc/self/task/<tid>/syscall` (the call's number
  and arguments; no ptrace, no seccomp) and `/proc/self/fd/<n>` for a descriptor argument,
  and writes a `blocked` sample with the call and the path as its stack's one frame. The
  parsing is `blaze_usercode::blocked` (item l), which already reads the same files of a
  sandboxed run.
- **macOS.** Without root: `proc_pidinfo(PROC_PIDTHREADINFO)` gives each thread's run state
  and `thread_get_state` its pc, named through the images; `ulock` waits are already `wait`
  spans (`blaze_core::wait`). kperf's kdebug system call tracing needs root and is not used;
  docs/macos.md's audit says which private interfaces the tree calls.

`harness/v2/loop_syscalls.py` (strace) stays as the cross-check from outside: a system call no
driver times and no sample caught is only there.

## m. Agent-readable everything

Each command above takes `--json`: a view's rows as JSON lines with the view's columns; a
report (`blaze slow`, `blaze why`, `blaze doctor`, `blaze bench edits`) as one object with a
`version`. The views are served by the daemon's socket API as they are to the CLI.

## n. The skill

`.agents/skills/debugging/SKILL.md`: the first three steps, the new views and commands, and
what to check before trusting a measurement (the binary, the tree, the build succeeded, the
daemon is the one you think, compare from the second edit on after a cold build).
