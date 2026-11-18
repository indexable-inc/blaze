---
status: plan
summary: "The runtime's end state and the order of work (the owner, 2026-11-17: \"make a plan for the sota runtime\"). One page over engine.md, the store, names, heads, bodies, lints, tests, run and the remote read: what must hold (incremental after a restart first, with lints as fast; then remote records, memory, cold time, cleanup, disk), the floor of each number beside today's measurement, the ten design points with the evidence for each from two surveys of the fastest compilers, checkers, linters and incremental engines, and the pieces lanes write, in order."
date: 2026-11-17
---
# The runtime: the end state and the order of work

This page is the plan. engine.md is the detail of the execution engine and stays its home; where target.md,
bottom.md, drive.md, parallel.md or store.md disagree with this page, this page stands and that part of them is
due for deletion (rule 49). Every number here was measured in a real run on 2026-11-17 or is marked as a floor.
The evidence columns cite two surveys made that day (the fastest compilers, type checkers, linters, formatters
and build engines; then incremental engines in depth: self-adjusting computation, delta-based engines and the
internals of salsa, DICE, Skyframe, turbo-tasks and IntelliJ's indexes). Their reports are kept with the
session's notes, outside the tree.

## What it must do

The owner, 2026-11-17 18:05, in his order of importance:

1. Incremental compiles are short: on realistic huge Rust monorepos (millions of lines), on enormous crates and
   large files, and after the computer restarted with nothing running. Lints are as fast as the check.
2. Cached results can come from elsewhere (R2, peers).
3. Memory use is small.
4. A cold compile is fast.
5. Disk cleanup is good and never locks.
6. Disk use is small.

And: a restart re-indexes nothing ("even if we restart computer should not need to reindex"); a daemon is
allowed and not needed; caches are shared across the worktrees of a machine with no setup; best on 128 cores
(Graviton, EPYC), good on a typical Mac; data-oriented throughout (structures of arrays, 32-bit ids, records
read in place); tests are incremental (only those an edit can have changed run again).

## Floors, and where each number stands

A floor is what the design permits, stated before the work (rule 67). Each is a counter the profile prints
and a test asserts; a piece is done when its real run prints the floor.

| What | Floor | Today (2026-11-17) |
|---|---|---|
| Nothing changed, no process alive | one certificate read: under 10 ms, with findings standing too | 0.01 s on a small project; 5.3 s on the 1,100-crate workspace with errors standing (the 500 MB answer read whole: 2.5 s; the plan rebuilt: 1.2 s) |
| The stored answer (task rows and read lists) | under 1 KB a task; a request reads only the pages of the tasks it enters | 500 MB for 24,000 tasks, read whole |
| One error | stops only the tasks that read what it broke | one unresolved name stops the names of every crate above it: 13,538 of 16,041 bodies not checked |
| A comment added | 1 outline, 0 bodies | 0.04 s: 1 outline, identical; 0 bodies |
| One statement edited in a body | 1 outline, 1 body, its lints, one commit: under 50 ms | 0.19 s: 1 outline, 1 body |
| A private fn added | 1 names group, 1 head, 1 body | 12 bodies of 12 (signature reads are per file); 0.68 s |
| A fn's parameter type changed | that fn and exactly its callers | 12 bodies of 12 |
| An impl added | the bodies whose trait questions it can answer | not measured |
| An edit in a 400-crate workspace | files read: 1; pages written: 2 | 1 file read (399 read back), 1 outline, 1 body, 2 pages |
| An edit in one module of a 2,000-module crate | one group's files and solve: under 2 ms | 50 ms on green; 7 ms unadmitted (5 tests red) |
| Cold check, 1,100 crates and 16,000 bodies, 18 cores | under 10 s; about 1 ms of CPU a body; entries per result 1.0 | 215 s wall, 598 s of CPU with 1,614 bodies checked (the rest stopped by the one error above); a checked body restarts 16.65 times |
| Workers busy in the body stage | over 70% at 128 workers | 34% at 16 in the first whole run |
| Peak memory of that cold check | under 4 GB: no record held in the heap after its stage | 15.7 GB (23.5 GB in the first whole run); a names record held 3 to 4 times |
| A dependency's names a small program needs | the groups it reaches: 66 of 17,260 | all 27 crates whole, about 6 s on a fresh home |
| Lints | one walk a body; each lint's cost printed | no lints stage on the engine yet |
| Tests after one body edit | the tests whose reach holds that body | spec only (test-selection.md) |
| First check on a fresh machine | the toolchain's records fetched by byte range, not computed | not built (remote-read.md; parked at the owner's word of 11:29) |
| Cleanup | never waits a reader; a command never waits on compaction | the store's generation flip exists; not measured under the engine |

## The end state

1. **Everything kept is a record in one store per machine.** A record is immutable, keyed from text before the
   work, written once into append-only segments under the home, so every worktree of a machine shares it with
   no setup. A segment is mapped once; a small record is read from the mapping or by one read into the task's
   frame; there is never one mapping per record. A worker writes its record straight to the store from its own
   slices (no joined copy, nothing held past the put); a request commits by one append and one sync. Hot
   tables are raw columns read in place (columns.md); cold families (expansion text, cold code) are compressed
   per segment. A segment carries a filter for absent keys. Compaction is scheduled, flips a generation and
   never blocks a read; the store has a byte cap.
   *Evidence:* no surveyed engine keeps fine-grained state across a restart and reads it in place (Zig, Buck2,
   Bazel, rust-analyzer and salsa lose it with the process; rustc rewrites a 77 MB dependency graph each
   session, 99% of it unchanged); Turbopack, the one that persists its graph, went through LMDB (3 minutes to
   persist a 1-minute build) and RocksDB to its own append-only store with mapped sorted files; Lean's mapped
   objects cost 7.6 s of a 10.2 s warm import on macOS because each of 52,490 files is its own mapping.

2. **A fact is a cell, and a read names the cell and what it answered.** A task's record lists the cells it
   read, each with a digest. There are three kinds:
   - *content cells:* a file's outline part, a name `(module, namespace, name)` (an absent name is a cell too),
     a head's aspect (a fn's types, its bounds; a struct's existence, its fields; a constant's type, its
     value), a module's textual macro sequence, a row of the module tree, a constant's value;
   - *question cells:* a cheap pure question over a table, recorded with the digest of its ANSWER and checked
     by asking it again, never by the table's digest: "which impls of this trait can match this self shape",
     "what does this glob closure bind for this name", "does this root reach this item". This is how facts of
     the whole graph stop being a floor: adding an impl moves a bucket, the bucket's askers are asked again
     (a lookup each), and only those whose answer moved are entered;
   - *settled classes:* the toolchain and each registry package are settled once per (toolchain, lock entry)
     and checked by one digest; nothing below them carries a read list.
   *Evidence:* Zinc, Swift, Zig and Pyrefly each fixed wide invalidation by tracking names, absent ones
   included (Pyrefly: 3.6 s to under 0.2 s); DICE's projection keys and turbo-tasks' cells are the question
   form; salsa's durability is the settled form; rustc's own source calls its one-query-per-trait impl set
   "unfortunate wrt. incremental compilation"; no shipped tool maintains reachability by deltas, and the
   delta engines measured (incremental Datalog, differential dataflow) hold 4 to 13 times the memory of a
   from-scratch run, so blaze keeps keyed records with question cells and uses no delta engine.

3. **The engine enters each task once.** The plan is made from text; stages run in order over shared immutable
   tables; a need found while running is made inline through a claim; a re-made result with equal bytes moves
   no reader; a failed task is a row. After an edit, candidates are found forward from the cells that moved,
   through a reverse index from cell to readers that is itself records (appended, never rewritten); a
   candidate is entered only if the request still needs it (a task whose last reader is gone is not made
   again). A read list is kept in the order the reads were made and checked in that order, stopping at the
   first that moved: a later read may exist only because of an earlier answer. A record is valid by its key
   alone only where the task is proven a function of its key (sandboxed scripts and macros, text-keyed
   stages); everything else is valid by its read list, which is what keeps the cutoff. AN ERROR STOPS ONLY
   WHAT READS IT: a task that found an error still writes its record (an unresolved name is a cell that
   answers "error"), so the crates above it are named and their bodies checked, and only a body that walks
   through the broken name fails. The certificate is a tree (request, member crate, file: a coarse proof
   covers all under it) and it stands with findings: nothing changed prints the standing findings from
   their own record and reads no page of the answer (engine.md).
   *Evidence:* Skyframe restarts a task at a missing dependency and blaze's body does the same today (16.65
   rounds); DICE and Zig left that model; "about 1 ms a body" is the per-line rate of the native TypeScript
   port and below Sorbet's, so the gap is engine overhead and not the cost of checking Rust. Forward from the
   change is the measured winner: on one engine, a no-change build fell from 3.3 s top-down to under 1 ms
   bottom-up and small edits were 10 to 1,800 times faster (Konat et al., 2018), with one known loss, the
   task no longer needed, which "changed and still needed" (Jane Street's Incremental) removes; the fixed
   stage order does the work of that engine's dependency-ordered queue. Every production engine (rustc,
   salsa, DICE, Adapton) checks dependencies in recorded order and stops at the first change. Shake and Ninja
   load their whole state at every start (2.0 s for a no-op build of GHC), and blaze's own 5.3 s above is the
   same mistake: the answer read whole. "Valid by key" is a deep constructive trace in the build-systems
   taxonomy: sound only for deterministic tasks, and it gives up the cutoff below it.

4. **Stages, each at its own grain.** Units and scripts (keyed by the manifests and the lock, read back from
   rows when those stand); outlines (only changed files are read); names (one record per module group; a
   group's solve reads only its files and the others' records; a dependency's groups are solved on demand;
   macro expansions are their own records, shared by content, read only where a body needs them); heads (a
   task per item of your crates; a dependency's head frozen on first read into one table shared by the
   request); impl tables per (crate, bucket) with question cells; bodies (one round, frozen heads imported
   on the spot, trait goals and instantiations as shared records, borrows, unsafety); lints (one task per
   body, every selected lint in one walk dispatched by node kind); tests (a test's reads are the code
   records its run reached); code (a record per instance and tier).
   *Evidence:* every checker that scales has a signature layer computed without bodies and a cutoff on equal
   bytes (Flow types-first: about 6x at p90); the fast linters run all rules in one walk (ast-grep: 3.6 s to
   0.98 s; rustc and Clippy merged their passes); Clippy's lints were 72% of CPU on one project where type
   checking was 4%, so each lint's cost is printed.

5. **Nothing shared and mutable on the path of an item.** A stage's tables are immutable while it runs; a
   worker's state is its frame, reset per task; an interned id is (worker, index) so no table is locked to
   mint one, sized for more than 128 workers; a result shared by bodies (a trait goal, an instantiation, a
   frozen head) is a record made once through a claim, never a cache per worker.
   *Evidence:* the native TypeScript port keeps type caches per checker and its instantiations grow with
   the thread count (8,848 at one checker, 70,784 at eight); oxc sat near 50% of its cores until a globally
   locked interner went; rustc's parallel front end, locks added to a sequential engine, gains about 15% on
   a whole graph.

6. **Every hit is a read.** A cached answer of any kind (a goal record, a claimed record, a frozen head, an
   impl table) hands its reader the reads it was made from. An audit mode makes every hit again and compares
   the result and the reads; a hit without its reads is a planted bug the tests must catch.
   *Evidence:* Swift's request evaluator missed invalidations until every cache read was instrumented; Lean
   ships the re-run check.

7. **Your code first, lazy over the rest.** Your crates are checked whole, most recently changed first, and
   findings print as they land. A dependency owes a check only the names your paths walk to, the heads your
   bodies read and the impls your trait questions can reach; its bodies never.

8. **A program is a manifest over code records.** `run` makes a function on its first call and tiers up in
   the background; `build` writes the manifest; an edit patches bytes in proportion to it (run.md, tiers.md).
   *Evidence:* nobody surveyed keeps native code per function on disk by content; a faster back end alone
   moves little (the Cranelift back end: 5 to 12% end to end on a large Rust project); the fast linkers win by
   parallel copying, and Zig's in-place patching reaches 30 to 70 ms rebuilds.

9. **Published records are fetched, yours stay.** The toolchain's records (names, heads, code) and popular
   registry packages' are read by byte range from R2 against a signed head (remote-read.md); a project's
   records leave its machine only when its organisation opts in. A peer is another origin of the same form.

10. **A helper is allowed and holds nothing.** It may keep the set of changed files so the sweep costs the
    changes, keep segments mapped, tier code up and compact; killing it loses nothing, and no command needs it.

## The work

Pieces are written by lanes that only write (the rule file is LANES-1810.md in the session's notes); the
coordinator compiles each piece against the green tree, runs its tests, admits it alone, then makes the real
run that judges it (rules 35, 82). "Check" is what that run or test must print.

| Id | Piece | Check | State |
|---|---|---|---|
| A1 | A body in one round: a missing head imported on the spot, impl questions answered on the spot | rounds == 1 on the 58 real bodies, answers and read lists equal | groundwork on green; in a lane |
| A2 | A head task per item of your crates; the per-file signature cell goes | a private fn added enters 1 body; a signature change enters its callers | in a lane |
| A3 | A dependency's head frozen once per request into the shared table, through a claim | heads frozen equals distinct heads read; none frozen twice at 16 workers | next |
| A4 | Trait goals and instantiations as shared records; every hit a read; the audit | goals solved against goals asked; a planted hit without reads is caught | next |
| A5 | Head aspects | a field added enters no body that only names the type | after A2 |
| A6 | Bodies inside a block's module are planned (a derive's impl fns) | the derive fixture's planted error is reported; the 1,100-crate run refuses no derive body | next |
| A7 | The engine prints a body's findings as `check` does and counts them | "2 errors" where two callers fail | in a lane |
| A8 | An error stops only what reads it: a names task with an unresolved name writes its record; the crates above are planned | the 1,100-crate run: "its crate has no Names task" 0 times; bodies checked over 15,000 | next, first |
| B1 | The module tree as a record; a one-group solve reads one group's files | byte-equal on every fixture; edit ratio in the hundreds on 2,000 modules | in a lane (5 tests red) |
| B2 | A dependency's names on demand over the tree record | the std hello world solves about 66 of 17,260 groups, byte-equal | after B1 |
| B3 | Macro expansions as records by content, out of the names record | the core library's names record under half its 83 MB | after B1 |
| C1 | No record held in the heap: vectored put, payload released, a commit per phase | "held after the merge: 0" per stage; peak memory of the 1,100-crate run | on green 0253586b90 for names records (held 0, one copy a put; 98 simulated crash cuts pass); the real run is owed |
| C2 | A large record streamed to its place, no heap copy; one flip per request | copies made: 0 for records over 64 KiB; the commit under 10 ms; simulated crashes pass | after C1 |
| C3 | Readers map: crate records and outlines read in place | bytes of records read into the heap: 0 | after C1 |
| C4 | Absent-key filters; cold families compressed | disk bytes and cold-read time before and after | later |
| D1 | Engine core: growing the plan, expansion rounds as phases, claims for inline needs, retiring tasks | the history simulation with planted bugs | next |
| D2 | Units, fetches and the graph read back from rows; only moved pages written; `why` reads the edition | a 400-crate no-op and one-edit: fixed cost in ms, by part | next |
| D6 | The certificate stands with findings; a request reads only the pages of the tasks it enters; rows and read lists as columns | the 1,100-crate no-op with errors standing: under 50 ms, pages read 0; the answer under 1 KB a task | next |
| D7 | Read lists in read order, checked in order to the first that moved | the engine simulation with a planted "later read checked first" | with D1 |
| D3 | Question cells in the engine; an impl added enters only its askers | an unrelated impl enters 0 bodies | after D1 |
| D4 | Settled classes; the certificate as a tree | a second project on one toolchain reads no dependency read list | after D1 |
| D5 | The switch: `check` is the engine; the old path deleted | answers equal on fixtures, the minis and the 1,100-crate workspace; then the deletion list of engine.md | after A1, A2, A6, A7, B1, C1 |
| E1 | Lints stage: one walk a body | a body edit reruns that walk only; each lint's cost printed | next |
| E2 | Tests stage by reach | one body edit reruns only the tests that reach it | after D5 |
| F1 | The std hello world prints through `run` | exit 0 twice | in a lane |
| F2 | `run` and the census on the engine | hello world; the census equals the last one | after D5 |
| G1 | 128 workers: the busy share by stage, the contention named | over 70% busy in bodies on the 128-core machine | after A1 |
| G2 | Huge inputs: a 5,000-module crate, a 100,000-line file | the floors above, as exact counts | with B1 and A2 |
| H1 | Published records fetched by byte range | a fresh home's first check of a small crate: bytes fetched, seconds | after D5, sooner at the owner's word |

Order: everything marked "in a lane" or "next" is written in parallel now; D5 is the gate after which the old
check path is deleted and E2, F2 and H1 start. A piece whose real run misses its floor is not followed by a
second fix to the same mechanism without an outside read of the design (rule 66).
