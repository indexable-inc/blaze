---
status: plan
summary: "The runtime's end state and the order of work (the owner, 2026-11-17: \"make a plan for the sota runtime\"). One page over engine.md, the store, names, heads, bodies, lints, tests, run and the remote read: what must hold (incremental after a restart first, with lints as fast; then remote records, memory, cold time, cleanup, disk), the floor of each number beside today's measurement, the ten design points with their evidence from two surveys and one review, the answer's stored form, ten ideas past the surveyed state of the art, and the pieces lanes write, in order."
date: 2026-11-17
---
# The runtime: the end state and the order of work

This page is the plan. engine.md is the detail of the execution engine and stays its home; where target.md,
bottom.md, drive.md, parallel.md or store.md disagree with this page, this page stands and that part of them is
due for deletion (rule 49). Every "today" number was measured in a real run on 2026-11-17 on an 18-core Mac.
The evidence cites two surveys made that day (fast compilers, checkers, linters and build engines; then
incremental engines in depth) and one review of this plan against them; all are kept with the session's notes,
outside the tree. Second revision, 18:45: the review's twelve edits are in.

## What it must do

The owner, 2026-11-17 18:05, in his order of importance:

1. Incremental compiles are short: on realistic huge Rust monorepos (millions of lines), on enormous crates and
   large files, and after the computer restarted with nothing running. Lints are as fast as the check.
2. Cached results can come from elsewhere (R2, peers).
3. Memory use is small.
4. A cold compile is fast.
5. Disk cleanup is good and never locks.
6. Disk use is small.

And: a restart re-indexes nothing; a daemon is allowed and not needed; caches are shared across the worktrees of
a machine with no setup; best on 128 cores, good on a typical Mac; data-oriented throughout (structures of
arrays, 32-bit ids, records read in place); tests are incremental.

## Floors, and where each number stands

A floor is what the design permits, stated before the work (rule 67), as a counter the profile prints and a
test asserts. Where a row is a target and not yet a derived floor it says so.

| What | Floor | Today |
|---|---|---|
| Nothing changed, no process alive | change detection plus one certificate read. Change detection is the OS journal read since the stored event on macOS, one batched stat per workspace file on Linux, and nothing statted under a settled class; under 10 ms on the 1,100-crate workspace (823 source files), after a reboot | 0.01 s on a small project; 5.3 s on the 1,100-crate workspace with failures standing (the 500 MB answer read whole 2.5 s, the unit graph rebuilt 1.2 s); 0.09 s on the old engine |
| The stored answer | at most 96 B fixed a row, 4 B a read, 24 B a defined cell; a request reads only the pages of the tasks it enters and of the cells that moved | 406 B a row before its first read and 81 B a read (24.5 KB for a 300-read body); read whole |
| One error | stops only the tasks that read what it broke | one unresolved name stops every crate above it: 13,538 of 16,041 bodies not checked |
| A comment added | 1 outline, 0 bodies | 1 outline, identical; 0 bodies; 0.04 s |
| One statement edited, small project | 1 outline, 1 body, its lints, one commit | 0.19 s |
| One statement edited, 1,100-crate workspace | the same counts; under 50 ms (a target) | not measured; it cannot beat the 5.3 s no-op today |
| A private fn added | 1 names group (2 where a child module globs its parent), 1 head, 1 body | 12 bodies of 12 |
| A fn's parameter type changed | that fn and exactly its callers | 12 bodies of 12 |
| `impl From<X> for E` added | the bodies whose trait questions it can answer, by the goal's self and argument shapes | not measured |
| A generic fn's body edited | instances made: one per shape the next run calls | not measured |
| A second worktree at the same commit | 0 tasks entered: the answer is found by the tree's digest | not built |
| An edit in a 400-crate workspace | files read 1; pages written 2 | 1 file read, 1 outline, 1 body, 2 pages |
| One module edited in a 2,000-module crate | that group's files and solve; whole-to-edit ratio in the hundreds | 17 ms against 173 ms whole: ratio 10 |
| One item edited in a 100,000-line file | a scan of the file at memory speed and one item's outline | not measured; the outline is made again whole |
| Cold check, dependencies settled, 18 cores | under 10 s (the owner's target); as a sum: names of your crates, heads, 16,000 bodies at the mean CPU a body with the borrow check in | 215 s, but with 10% of bodies checked: not yet a cold-check number |
| CPU to check one body | 1 to 3 ms at the mean (from per-line rates of checkers without traits or a borrow check: 11,000 to 40,000 lines a core-second) | 11.5 ms at the median, at 4.43 entries a body (16.65 that morning) |
| Speedup at N workers | wall x workers / CPU at one worker of at least 0.7, and the coordinator under 5% of wall | 598 CPU-s in 215 s on 16 workers: 2.8 cores busy |
| Peak memory, cold check | workers x the largest task's working set, plus the plan: about 1.2 GB at 16 workers, whatever the project's size; mapped records are the page cache's | 15.7 GB |
| Peak memory, one edit | one task's working set and the pages it reads: under 100 MB (Relax reports 1,087 MB for a warm one-body-edit check) | not measured |
| Peak memory, first check of a 17-line project | under 100 MB: it reaches 66 name groups | 2.1 GB: 27 dependency crates' names solved whole |
| A dependency's names a small program needs | the groups it reaches: 66 of 17,260 | all 27 crates whole, about 6 s on a fresh home |
| Lints | one walk a body; each lint's cost printed | no lints stage on the engine yet |
| Tests after one body edit | the tests whose reach holds that body | spec only (test-selection.md) |
| First check on a fresh machine | the settled classes read by byte range: bytes, round-trip waves (at most 3, by a prefetch list in the head) and seconds | not built (remote-read.md) |
| Cleanup | the longest wait of any command on compaction is 0 at 128 workers; disk after 1,000 edits at most twice the live bytes | not measured under the engine |

## The end state

1. **Everything kept is a record in one store per machine.** A record is immutable, keyed from text before the
   work, written once into append-only segments under the home, so every worktree of a machine shares it. A
   segment is mapped once; a small record is read from the mapping or by one read into the task's frame. A
   worker writes its record straight to the store from its own slices; a request commits by one append and one
   sync. Hot tables are raw columns read in place (columns.md); cold families are compressed per segment; a
   segment carries a filter for absent keys. Compaction is scheduled, flips a generation and never blocks a
   read; the store has a byte cap. Bytes written per task have a floor too: the write side is what dominated
   the one engine that persists its graph.
   *Evidence:* no surveyed engine keeps fine-grained state across a restart and reads it in place (Zig, Buck2,
   Bazel, rust-analyzer and salsa lose it with the process; rustc rewrites a 77 MB dependency graph each
   session, 99% of it unchanged); Turbopack went through LMDB (3 minutes to persist a 1-minute build) and
   RocksDB to its own append-only store with mapped sorted files and holds its whole graph at about 0.4 KB a
   task; fixed-address mapped images are costly on macOS (Lean: 7.6 s of a 10.2 s warm import).

2. **A fact is a cell, and a read names the cell and what it answered.** Three kinds:
   - *content cells:* a file's outline part, a name `(module, namespace, name)` (an absent name too), a head's
     aspect, a module's textual macro sequence, a row of the module tree, a constant's value;
   - *question cells:* a cheap pure question over a table, recorded with the digest of its ANSWER and checked
     by asking it again: "which impls can answer this goal", keyed by the canonical goal's self and argument
     shapes (so one new `From` impl does not re-enter every `?`); "what does this glob closure bind for this
     name". A whole-graph fact that is not a lookup (which items a root reaches) is a record made by a walk
     and is not asked per request: `run` learns reach by running;
   - *settled classes:* the toolchain and each registry package, settled once per (toolchain, lock entry) and
     checked by one digest; nothing below them carries a read list or is statted.
   *Evidence:* Zinc, Swift, Zig and Pyrefly each fixed wide invalidation by tracking names, absent ones
   included (Pyrefly: 3.6 s to under 0.2 s); DICE's projection keys, salsa's tracked fields and turbo-tasks'
   cells are the question form; salsa's durability is the settled form; rustc's source calls its
   one-query-per-trait impl set "unfortunate wrt. incremental compilation". No shipped tool maintains
   reachability by deltas, and incremental Datalog engines measured on points-to analyses hold 4.3 to 22
   times the memory of a from-scratch run, so blaze uses no delta engine.

3. **The engine enters each task once, and finds work forward from what changed.**
   - The plan is made from text; stages run in order over shared immutable tables; inside Heads and Bodies a
     row's wave is one more than the highest same-stage row it read and never goes down, so a head that reads
     a head and a body that reads a body's fact are ordered and never refused or left stale.
   - A need found while running is made inline through a claim; a re-made result with equal bytes moves no
     reader. A candidate is entered only if the request still needs it.
   - A read list is kept in read order and checked in that order, stopping at the first that moved.
   - AN ERROR STOPS ONLY WHAT READS IT: a task that found an error still writes its record (an unresolved name
     answers "error"); a task that fails outright records the reads its failure saw, so it stands until one
     of them moves and is never entered again for nothing.
   - A record is valid by its key alone only where the task is proven a function of its key (sandboxed
     scripts and macros, text-keyed stages); everything else by its read list, which is what keeps the cutoff.
   - "Nothing changed" is change detection and one certificate read, with findings standing too: they print
     from their own record. The certificate is keyed by the digest of the tree it answers, so a state seen
     before (another worktree, a branch switched back, a commit CI checked) is a lookup.
   *Evidence:* Skyframe restarts a task at a missing dependency; DICE and Zig left that model. Forward from the
   change is the measured winner (Konat et al., 2018: a no-change build from 3.3 s to under 1 ms, small edits
   10 to 1,800 times faster) with one known loss, the task no longer needed, removed by "changed and still
   needed" (Jane Street's Incremental, whose heights are the wave rule). Every production engine checks
   dependencies in recorded order. Whether a body's remaining cost at one entry is engine overhead or the
   cost of checking Rust is NOT known: no surveyed checker has traits and a borrow check.

4. **Stages, each at its own grain.** Units and scripts (keyed by the manifests and the lock, read back when
   those stand); outlines (only changed files are read, and inside a file only the item chunks that differ
   from the stored outline by common prefix and suffix); names (a record per module group; a dependency's
   groups on demand; expansions as their own records by content); heads (a task per item of your crates; a
   dependency's head frozen once a machine, as a record of its settled class); impl tables per (crate,
   bucket) with question cells, and the overlap and orphan checks as a task per bucket; bodies (one round,
   heads imported on the spot, trait goals as shared records, borrows, unsafety); lints (one task per body,
   every selected lint in one walk; whole-crate lints over a stored reference graph); tests (a test's reads
   are the code records its run reached); code (a record per shape at the first tier).
   *Evidence:* every checker that scales has a signature layer computed without bodies and a cutoff on equal
   bytes (Flow types-first: about 6x at p90); one walk for all lints (ast-grep: 3.6 s to 0.98 s; rustc and
   Clippy merged their passes); Clippy's lints were 72% of CPU on one project.

5. **Nothing shared and mutable on the path of an item, and no single thread on the path of a stage.** A
   stage's tables are immutable while it runs; a worker's state is its frame, reset per task; an interned id
   is (worker, index), sized for more than 128 workers; a shared result is a record made once through a claim;
   each worker writes its own sorted run of rows and index entries, so the coordinator merges runs and never
   inserts per read.
   *Evidence:* the native TypeScript port keeps type caches per checker and its instantiations grow with the
   thread count (8,848 at one, 70,784 at eight); oxc sat near 50% of its cores until a globally locked
   interner went; today's merge is one thread doing an ordered insert per read: about 4 million for 16,000
   bodies, about a second, against 0.125 s for the whole body stage at the floor on 128 workers.

6. **Every hit is a read.** A hit is ONE read, of the hit record's result; the record keeps its own reads.
   Of a cycle of trait goals only the root, the least key, is a record. A goal past a fixed count of steps is
   a typed failure naming it; no limit is in wall-clock time. An audit mode makes every hit again and
   compares; a hit without its read is a planted bug the tests must catch.
   *Evidence:* every production engine records one edge per hit; rustc documents that a cycle participant's
   result depends on which goal is the root; Swift dropped wall-clock limits as non-deterministic; one typo
   made rustc's solver run for hours; Lean ships the re-run check.

7. **Your code first, lazy over the rest.** Your crates are checked whole, most recently changed first, and
   findings print as they land. A dependency owes only the names your paths walk to, the heads your bodies
   read and the impls your trait questions can reach.

8. **A program is a manifest over code records.** `run` makes a function on its first call and tiers up in
   the background; `export` writes a file others can run; an edit patches bytes in proportion to it. One code
   record per shape at the first tier; line tables with the code, full debug info after.
   *Evidence:* nobody surveyed keeps native code per function on disk by content; a faster back end alone
   moves little (the Cranelift back end: 5 to 12% end to end on a large Rust project, with a per-function
   cache as its own next goal); Zig's in-place patching reaches 30 to 70 ms rebuilds.

9. **Published records are fetched, yours stay.** What is published is exactly the settled classes; a
   published head carries a prefetch list so a fresh home needs few round trips; a project's records leave
   its machine only when its organisation opts in. A peer is another origin of the same form.

10. **A helper is allowed and holds nothing.** It may keep segments mapped, tier code up and compact; killing
    it loses nothing, and no command needs it.

## The answer's stored form

The answer is what a checkout keeps between requests: one row per task, the cells, and the index from a cell
to its readers. It is small, local to the checkout, and separate from the records, which hold the bytes and
are shared by content.

- Tasks and cells have dense 32-bit ids inside one answer; names are in a dictionary.
- A row: fixed fields (key, out, serial, wave, counts) in at most 96 B; its reads as cell ids in read order,
  4 B or less each; its defined cells at 24 B each. Causes and findings are not in the row.
- A row stands when no cell it read moved after the row's serial (a generation per cell). The digests of
  what a task read stay once, in the task's record: they are what another worktree, a restart from a lost
  answer, or a fetched record is checked against.
- The reverse index is appended sorted runs of (cell, reader, +1 or -1), written by each worker for its
  phase and merged by compaction; a lookup probes a bounded number of runs behind a filter. Signed entries
  make "the last reader is gone" exact.
- Rows and runs are immutable mapped segments under a root; the root flip is the commit; the root holds the
  tree's digest, the summary the closing line prints, and the key of the lines record.
- The root and its segments are also put in the shared store under the tree's digest, which is how a second
  worktree at the same commit enters nothing.

It is written interface-first as one small crate with its simulation (rule 79) and replaces the page form
(406 B a row, 81 B a read, the index rebuilt from every row at each request).

## Past the surveyed state of the art

Ideas no surveyed system has, or has only in a weaker form, each with the measurement that decides it. The
owner, 2026-11-17 18:40, took X1, X2, X3 and X5 into the design ("sure"; of X3: "this is a JIT too so can do
that"): they are pieces in the work table (F0, V1, K1, W1). The rest are experiments, in no order of work
until their number is in.

| Id | Idea | What decides it |
|---|---|---|
| X1 | The certificate keyed by the tree's digest, answers adopted across worktrees, branches and CI (in the answer's form above) | a branch switched away and back: tasks entered 0 |
| X2 | A body's result kept as a checkable certificate (every type, chosen impl and coercion): re-validating it after a read moved is a linear pass with no inference; a fetched one needs no trust | validate time against infer time on the 58 real bodies; the share of moved-read bodies whose certificate still holds |
| X3 | One code record per generic fn at the first tier (size, alignment and trait tables passed in), specialised only when hot | code records and build seconds of the 1,100-crate census; run-time cost on its tests |
| X4 | Keys from normalised tokens: formatting, comments and a local's rename move no key | tasks entered after formatting a whole crate: 0 |
| X5 | Check an edit before it exists: `check --with <patch>` fills records for a tree not yet written | the save after a pre-checked patch: tasks entered 0 |
| X6 | Optimistic stages: bodies start against last request's heads while names and heads are re-made, then their reads are validated | edit latency as the longest stage, not the sum; the share of bodies re-run |
| X7 | Goal and head records shared by content across projects and users (with X2, no trust needed) | hit rate of a second project's goals on a first project's records |
| X8 | Inside one item: block grain for very large generated fns | the largest body's re-check after a one-line edit |
| X9 | 128-core specifics: workers placed by crate per NUMA node, huge pages for mapped segments, batched reads | the speedup row at 128 workers before and after |

## The long-term form: the program is a database

The owner, 2026-11-17 18:40: "ideally this whole file tree thing, this whole program, should itself be a
database almost like unison eventually, so would be more like less cli and more like a ts interface or
something like `world.file.patch...`, `world.check` or something with transactions ... this is the long
term goal."

The design above already makes this its natural end. A tree is a value named by its digest (the
certificate's key) and a record is found by content, so a WORLD is an immutable value: a tree's digest and
the answer for it. An edit is a function from a world to a world: `world.patch(edits)` returns a new world
whose changed items have new keys and whose unchanged items share every record; nothing touches the working
files until `world.commit()`. `world.check()`, `world.run()`, `world.test()`, `world.why(item)` and
`world.query(..)` are answers about a world. A transaction is several patches checked as one world: the
middle states are never checked (rule 80). Files become a projection of the item database, the outline
being the mapping: Unison's "code is content-addressed definitions" in a language that keeps its files.

The interface is typed data, not flags and text: one versioned schema in deterministic CBOR over a Unix
socket or HTTP (rule 33), a TypeScript client over it, and the command line as a thin caller of the same
calls. The pieces are W0 to W3 below; W1 is the first, because it needs only what the engine already has.

## The work

Pieces are written by lanes that only write (LANES-1810.md in the session's notes); the coordinator compiles
each piece against the green tree, runs its tests, admits it alone, then makes the real run that judges it
(rules 35, 82).

| Id | Piece | Check | State |
|---|---|---|---|
| A8 | An error stops only what reads it; a failure records its reads | the 1,100-crate run: "its crate has no Names task" 0 times; over 15,000 bodies checked; the no-op certified | in a lane; FIRST: until it lands every cold, memory and scaling number is measured on a tenth of the bodies |
| A1 | A body in one round: heads and impl answers on the spot | rounds == 1 on the 58 real bodies, answers and read lists equal | heads on green (16.65 to 4.43 rounds); impl answers written, compiling |
| A2 | A head task per item; the per-file signature cell goes; the wave rule for heads that read heads | a private fn added enters 1 body; a signature change enters its callers | in a lane |
| A3 | A dependency's head frozen once a machine, as a record of its settled class, found through a claim | heads frozen equals distinct heads read; a second request freezes none | next |
| A4 | Trait goals as shared records: one read a hit, the root of a cycle only, a step cap, the audit | goals solved against goals asked; a planted hit without its read is caught | after A1 (same files) |
| A5 | Head aspects | a field added enters no body that only names the type | after A2 |
| A6 | Bodies inside a block's module are planned (a derive's impl fns) | the derive fixture's planted error is reported; the 1,100-crate run refuses no derive body | in a lane |
| A7 | The engine prints a body's findings as `check` does | "2 errors" where two callers fail | on green 0e495ebbbe |
| B1 | The module tree as a record; a one-group solve reads one group's files | byte-equal on every fixture; the ratio row above | on green 8a47687182 at ratio 10; the driver's O(groups) work is next |
| B2 | A dependency's names on demand over the tree record | the std hello world solves about 66 of 17,260 groups, byte-equal | next |
| B3 | Macro expansions as records by content, out of the names record | the core library's names record under half its 83 MB | next |
| C1 | No record held in the heap on the write side | "held after the merge: 0" per stage | on green 0253586b90 for names records |
| C2 | A large record streamed to its place; one flip per request | copies made 0 over 64 KiB; the commit under 10 ms; simulated crashes pass | after C3 |
| C3 | Readers map: crate records and outlines read in place | bytes of records copied into the heap: 0 | in a lane |
| C5 | Cleanup under the engine: roots with expiry, the cap, compaction that no command waits on | the cleanup row above | next |
| F0 | The answer's stored form, as one crate with its simulation: dense ids, generation validity, signed sorted runs, the root keyed by the tree's digest | one signature edit on the 1,100-crate workspace reads the entered tasks' pages and the moved cells' index runs only; the row sizes above | next, interface-first; it replaces the page form |
| D1 | Engine core: ordered reads, changed and still needed, a plan that grows, claims for inline needs, waves inside a stage | the history simulation with a planted bug for each | in a lane |
| D2 | Units, fetches and the graph read back from rows; `why` reads the edition | the no-op's "plan" phase in ms on the 1,100-crate workspace | in a lane |
| D3 | Question cells in the engine; an impl added enters only its askers | an unrelated impl enters 0 bodies | after D1; the cell's type lands with A1 |
| D4 | Settled classes: nothing under them listed or statted | reads a body row: before and after; a second project on one toolchain reads no dependency read list | with F0 |
| D6 | The certificate with findings; only moved pages written; exact "owed" | the three-finding fixture certified with 0 pages read | written, compiling (on the page form, until F0) |
| S1 | Change detection: the OS journal on macOS, batched stats on Linux, nothing under a settled class | the no-op row after a reboot | next |
| D5 | The switch: `check` is the engine; the old path deleted | answers equal on fixtures, the minis and the 1,100-crate workspace with over 15,000 bodies checked; the no-op no slower than the old path's 0.09 s | after A8, F0, A1, A2, A6, B1 |
| E1 | Lints stage: one walk a body, over the typed-result record | a body edit reruns that walk only; each lint's cost printed; lint seconds beside check seconds | next |
| E2 | Tests stage by reach | one body edit reruns only the tests that reach it | after D5 |
| F1 | The std hello world prints through `run` | exit 0 twice | in a lane (a regression fixed at 35cb00b0e6) |
| F2 | `run` and the census on the engine | hello world; the census equals the last one | after D5 |
| G0 | Fixtures and the scaling harness: a 5,000-module crate, a 100,000-line file, a `--workers` sweep | the huge-input rows and the speedup row print | next; waits only on A8 |
| H0 | The remote contract: the published set is the settled classes; a publish job; a fresh home reads by range from a directory origin | bytes, round-trip waves and seconds for a small project | next (the owner parked the R2 service itself at 11:29; this needs none) |
| H1 | Code records fetched | a fresh home's first `run` | after F2 |
| V1 | A body's result as a checkable certificate (X2): validate without inference when a read moved; a fetched certificate needs no trust | validate against infer time on the 58 real bodies; the share of moved-read bodies that still validate | after A1 and A4; spec first |
| K1 | One code record per generic fn at the first tier (X3): size, alignment and trait tables passed in; specialised when hot | code records and build seconds of the 1,100-crate census; its tests' run time | after F2; spec first |
| W0 | The world interface: the schema (worlds, patches, transactions, answers) and a TypeScript client; the CLI calls it | a script that patches, checks and commits through the client | spec next |
| W1 | `check` of a world that differs from disk by a patch (X5): records filled for a tree not yet written | the save after a pre-checked patch enters 0 tasks | after F0 (the tree's digest is the key) |
| W2 | Transactions from an agent's edit tool, with item keys given by the tool so no file is outlined | an agent edit of one body: files outlined 0 | after W0 |
| W3 | Commit as projection: the files of a world written from its items | a world patched twice and committed equals the same edits made on disk | after W0 |

A piece whose real run misses its floor is not followed by a second fix to the same mechanism without an
outside read of the design (rule 66).
