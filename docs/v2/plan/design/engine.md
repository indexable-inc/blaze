---
status: plan
summary: "The execution engine rebuilt (D131, the owner 2026-11-17: \"full send the new design u can delete the old code\"). Kept: records keyed from text, read lists, the certificate, the store, and every semantic function. Replaced: the per-event coordinator, restart on need, the private world of a file job, names per crate, and the three front-end paths. New: a plan made from text, phases over shared immutable tables, bodies at body grain on the pool reading by cell, needs made inline, results merged in bulk."
date: 2026-11-17
---
# The engine: phases over shared tables

parallel.md is the end state for threads, frames, memory and determinism; this page is the engine that
runs on it. It replaces bottom.md's "a task returns `Need(keys)` and is entered again" and drive.md's
machine stepped once per task event. Where this page and those disagree, this page stands.

## Why (tempo's cold `blaze check`, 18 cores, tags g044908, g080231, g082400)

| Measured | Number |
|---|---|
| CPU used over wall time | about 500 s in 573 s; one busy worker in a 15-minute run's sample |
| Coordinator thread | 13.6 s of machine steps and 33.2 s of `jobs made` in a 152 s run |
| Entries per result | Signature 3.0, BodyCheck 3.1, CrateScope 2.9 (a task restarts at each need) |
| Private per file job | a `ProjectWorld` with its own heads and impl buckets: 8.4 GB, a sort per ask |
| One private fn added | 7 Signatures and 7 BodyChecks made in a six-body crate; the floor is 1 and 1 |
| What already works | nothing changed 0.09 s, one body edit 0.26 s, no process alive |

## What stays, what goes

Stays: blaze-format records, blaze-cas, keys from text (blaze-drive key.rs, record.rs, cells.rs), the
certificate, the answer rows' form, and every semantic function (outline, names solve, heads, impl
rows, body check, borrow check, build, emit). They are pure; only how they are called changes.

Goes, deleted when `check` runs on the engine: blaze-drive request.rs's per-event machine and `Need`
re-entry; blaze-cli work.rs's job loop; front2's `ProjectWorld`/`Records`/`Store`/`Buckets` as mutable
per-job state; whole reads of `CrateScope` and `Surface`; the items record's bucket section; the eager
`Front2Program` and the lazy program as separate paths (one front end remains, asked for all or on demand).

## The design

1. **Tables.** Every fact more than one task reads is an immutable record read in place by all workers:
   `Outline` per file; `Names` per module (a glob or macro cycle one `Group`), with a cell table;
   `Head` per item, frozen, each with its own type table; `Impls` per (crate, bucket) with a directory
   and `NoImplsOf` per crate (blaze-surface, blaze-impls); `LangItems`; `Body` per body (findings, the
   typed result, the cells it read). A task's private state is its own frame, nothing else.
2. **The plan is made from text.** The unit graph and the outlines name every Names, Head and Impls
   task and its key before any runs. A request is phases in order: outlines; names (crates in
   dependency waves, modules inside); heads and impl tables; bodies. A phase starts when what it reads
   is complete, so a task in the plan is entered once: entries equal results. Two things the text
   does not show are phases of their own, never restarts: build scripts and macro modules run first,
   in the unit graph's order (their keys are from manifests and text); and a crate's names are rounds
   to a fixed point: solve the modules, expand in parallel the macro calls that resolved, outline
   what they made, plan the next round from those outlines, until no call is left.
3. **Needs found while running are made inline.** A body that needs something the plan could not see
   (a method on an inferred type, a constant in a type, an opaque type's hidden type) claims its key in
   the claim table (blaze-core claim_table.rs): free, it makes it on its own stack and publishes;
   done, it reads; held by another worker, it makes it too when the kind is small (records are equal
   bytes by key, so the duplicate is correct) or parks on the claim with a deadline when it is large.
   Nothing starts over. A cycle is found by the claim walk and is a typed failure naming the items.
4. **Bodies at body grain.** A body is one task on the work-stealing pool, batched by predicted cost,
   its frame reset per body. Its world is a read-only view over the tables plus a recorder of the cells
   it read. Results go to a per-worker buffer.
   YOUR CODE FIRST (the owner, 2026-11-17): bodies are independent, so their order is a choice: the files
   changed most recently first, then the member the command stands in, then the rest; findings print as
   they land, sorted within what has landed. A dependency owes a check only its names and the heads a
   body of yours reads: a dependency's head is made when first read (point 3), its names once a machine
   per version and features, and its bodies never.
   LAZY OVER WHAT IS NOT YOURS (the owner, 2026-11-17 11:42: "lets actually make blaze check lazy? as
   well ... and sure take other things from its design", of Zig, which analyses only what is referenced).
   Your own crates are checked whole: an error in code nothing calls is still an error in Rust. Everything
   else is made on demand and at the grain asked: a dependency's NAME is resolved when a path walks to it
   (the cell `(module, namespace, name)`, through its module's named imports and globs), never its whole
   crate; its head when a body reads it; its bodies never. What Rust does not let be lazy: whether a type
   implements a trait needs every impl of the crates that could hold one (the orphan rule names them), so
   such a crate's item-level macros are expanded and its impl headers resolved, once a machine; a crate
   whose types and traits no goal of yours names is never expanded. `blaze run` already checks only what
   the program reaches.
   ASPECTS (from Zig's dependency kinds: a declaration's type, its value, a namespace's one name): a reader
   depends on the aspect it read, not on the declaration: a fn's parameter and return types, its bounds,
   its lifetimes; an ADT's existence and generics, its fields, its layout; a constant's type, its value; a
   trait's head, its member list, each member's signature; an impl's header, its members. Each is a part
   of the item's cell with its own digest (the cell id already carries a part).
5. **The coordinator is off the hot path.** It makes the plan, starts each phase, and merges each
   phase's buffers in bulk, sorted by key (rule 25): puts to the store, rows to the answer table. Its
   cost is a constant per phase plus a sort, never a step per task.
6. **An edit.** The sweep gives the changed files; their outlines give the changed cells; the reverse
   index by cell gives the readers; those tasks are the plan, run in the same phase order. A reader whose
   cells did not move is not entered. Nothing changed is the certificate and nothing else.
7. **One front end.** `check` plans every body asked; `run` plans the entry and makes the rest on first
   call through the same inline rule; the census is `run` with every instance asked.
8. **Lints and tests are stages too.** A lint task comes after Bodies and is ONE PER BODY (D135; it was one
   per (lint, body) until the survey of 2026-11-17): every selected lint runs in one walk over that body's
   typed result record, dispatched by node kind (each lint names the kinds it wants; the lints are
   compiled in, no dynamic call per node), reading the cells the lints ask. The record is keyed by the body
   and the hash of the selected lints' code ids, and holds the findings per lint (rule 81), so an edited
   body reruns its own walk and nothing else; a lint's cost is counted per lint in the profile. Errors print
   with the answer; advice and whole-program analyses are later phases that never hold the answer back.
   A test is a task whose reads are the code records its run reached (the run's reach list), so a
   request reruns exactly the tests whose reached code moved, and the rest are reused by key.
9. **A live process** is allowed and not needed: everything above is records, so a process that exits
   loses nothing and a restart re-indexes nothing. One may be added later to save process start.

## An edit costs what it moved (D149, the coordinator's design, 2026-11-19)

MEASURED (tempo, one body edited, tree 9cec6e9546; a 1 ms stack sample, 951 main-thread samples):
the body's own check is about 10 samples. The Bodies phase listed every member's bodies (list_with
142, index_by_chain 93, outlining files again 176), the answer was printed whole (300: a file lexed
once per finding; every finding hashed), and one crate's heads were listed whole to place the
body's reads (56). 39,740 expansion records were read. The command took 2.61 s; its floor is the
one body (0.1 s) and the reads of what stands. Three partial fixes are in (owed files only; one lex
a file; no expansion text in the head walk). What is left is three records (rule 51: what outlives
a task is a record; rule 77: never rebuild what a record holds):

1. **A member's listing row** (piece I1): what the Bodies phase derives from a member's WHOLE
   listing and publishes each command (its uncovered bodies: the count and the first, and its
   bodies' item keys by file and outline entry) is a record keyed by the member's names row's out
   and its files' outline digests. A member none of whose files is owed is read, never listed:
   `groups` opens only members that hold an owed file. Counter: members opened M of T (floor: the
   owed files' members).
2. **A file's head listing** (piece I2): a crate's listing of heads is the in-place join of its
   files' listing records, each keyed by the crate key, the file's surface, its inside-body facts
   and the expansion rows its calls name (engine_heads.rs's `ListingKey` is a key within one command
   only). A plain body edit moves none of these: 0 listings made. Restore equals fresh in bytes.
3. **A file's rendered lines** (piece I3): the stored rendered answer holds one section a FILE
   (form BER2), keyed by the presentation, the path, the file's text digest, the digest of its
   findings rows and its ids' shown lengths; an unmoved file's section is written out as stored and
   the file is not read; a moved file is rendered alone. The text digest is in the key because
   `file:line:col` exists only here (rule 71). Counter: files rendered R, replayed P (floor: R = the
   files that moved).

Also (piece I4): one request for an expansion's marks reads its record once (front2 source.rs
`text_marks` asks the load twice past the items memo).

## A command's fixed costs do not grow with the project (D150, the coordinator's design, 2026-11-19)

THE TARGET (the owner, 2026-11-19): a check after an edit on tempo in UNDER ONE SECOND, "much, much
faster than Rust". MEASURED (tempo, tree 9cec6e9546): a no-op is 221 ms in main, of which **journal
replayed 152 ms**, sweep 55 ms, store committed 8.5 ms, everything else under 12 ms; an edit adds
**plan (the units read back) 196 ms**, plan (files outlined) 11 ms, layout 11 ms, stored answer
read 9 ms. These four are paid by every command whatever moved. Their floors:

1. **The journal** (152 ms): a command replays the store's whole journal to learn its state. The
   state after record N is a record (rule 51): the replayed state is checkpointed (keyed by the
   journal's prefix digest and length), and a command replays only the suffix appended since the
   newest checkpoint it can verify. Floor: O(records appended since the last command), a few
   hundred microseconds on a no-op. A torn or mismatched checkpoint is ignored and the journal is
   replayed whole (counted in the profile; this is the journal's own recovery rule, not a fallback
   to a different answer). DST first (rule 43): crashes at every cut, a planted stale checkpoint.

   **What the phase is (read 2026-11-19, F1).** The store has no journal: blaze-cas opens a head
   and a manifest and maps segments lazily ("store open", under 1 ms). The phase named "journal
   replayed" (engine_check.rs) held two things: the last answer's printed lines read
   (engine_answer.rs `load_lines`) and the replay of the SYSTEM's journal of changes
   (journal_sweep.rs `take`: one FSEvents stream `sinceWhen` the pointer's saved event ID per
   unit, the checkout's units and blaze's folder in the home, one after another, each waited to
   `HistoryDone`). The lines read is now its own phase ("stored answer's lines read"), so the
   profile splits them. The state the replay rebuilds is the set of rows to stat; the place it
   replays from is the pointer's, and a certificate wrote nothing, so every no-op replayed from
   the place of the last command that STORED an answer, a suffix that grows with every other
   write on the volume, and a unit refused once (no place saved: tempo's checkout at the
   driver's root, whose first answer saves no top-level folder; a reset journal) was refused on
   every no-op after it ("refused: no place ... statted 1242 of 1245" on tempo's no-op).

   **The checkpoint (journal_checkpoint.rs).** HOLDS: `"BJC1" | the sealed pointer's key [32]
   | places len u32 | places` (the journal_sweep `Stored` form: per unit, where, name, cursor).
   KEY: the content id of those bytes; the root `<answer root>.journal` names it (pinned like
   the answer, one key). MEANING: "the answer under that pointer was certified standing at these
   places": every write after them is in the journal from them. WRITTEN: only by a command whose
   certificate stood on that pointer (nothing moved, nothing owed), with the places its own
   replay returned (taken before its stats, as the pointer's are), merged per unit with the
   places it replayed from (the later of the two on one journal; the new one across a reset), in
   the same commit after the answer as the roots' renewal; and only when DUE: the replay from
   the places used covered at least `DUE_EVENTS` (1: any place that moved) event IDs, or a unit
   had no place on this journal (the next no-op would sweep it whole). So every no-op replays
   only the IDs since the last command's place, the floor, for one small record and one root in
   the commit it makes anyway. The threshold was 65,536 until 2026-11-20, set unmeasured: a no-op
   loop then replayed every ID since the last stored answer, and fseventsd's replay costs about
   0.3 microseconds a system-wide ID above a fixed cost (tempo: 99,681 IDs in 139 ms, 5,364,028
   in 1.73 s; utilization.md U9). FOUND: after the pointer, the root's one key, the record got. VERIFIED,
   before any place in it is used: the record is served only whole and checked, and it rides one
   commit with its root, all or nothing (the store's own proofs: blaze-cas root_commit_tests.rs,
   plain_place.rs `Held`; not checked twice, rule 77), the magic and lengths fit and the places
   parse whole, and its pointer key
   is the key the answer's root named in THIS command (a checkpoint of any other answer, an
   older one or a concurrent command's, is stale and ignored). A place earlier than the
   pointer's on the same journal is never used (the later wins), so a checkpoint can only
   shorten a replay over states the certificate saw. Ignored or absent: the pointer's places,
   the whole suffix, named in the profile. Line: `journal: replayed N of M event ids
   (checkpoint at K; floor: the ids appended since the last command); ...`. Proof
   (tests/journal_checkpoint_tests.rs): a DST over a model journal and store (edits, certified
   and storing commands, a concurrent certifier, resets, crashes before and after the commit,
   every prefix of a record) with the oracle "the rows a run would stat
   from the chosen places cover every file that differs from the stored answer, exactly as a
   replay from the pointer's places does", and plants: an unbound checkpoint (any answer), a
   cursor taken after the stats, a checkpoint written by a command whose certificate did not
   stand (then cut before its answer was stored).
2. **The plan read back** (196 ms). THE PREVIOUS ANSWER IS THE PLAN (2026-11-20): the units
   stage reads it back only under the sweep's `Stood` proof (engine_units.rs: no layout row the
   last answer read moved), reading its rows one at a time as asked. The plan index
   (engine_plan_index.rs, 2026-11-18, never wired) is deleted: it was a memo of the cheap part
   (about 7 ms of the 180 ms of the read back on tempo, 54 rows read). The 180 ms is the names
   world made again from the stored graph (names.rs `world`), each step now under its own span
   (the graph decoded, the shelf opened, the fetch plan, the crate list and the toolchain's packs,
   the packages placed and the world made, its subjects and scripts), its counts on the profile's
   `plan:` line (worlds built, graph decodes, fetched records mapped and their bytes, sources
   statted). Taken already: the fetched records are mapped in place, never read whole into the
   names frame (every package's sources were copied each command); under `Stood` no fetched
   source is statted, its one look being the world's map of it (rule 74; one deleted by hand is no
   mapping, so no world, so the stage discovers, where the fetch's stat finds it gone and fetches
   it again). Left, the end state: under `Stood` no world is built and no graph decoded (floors 0,
   0; today 1, 1). The stored crate list (blaze-world crate_list.rs, read in place by
   `CrateList::open`) already holds what a body edit's readers ask of the world: each crate's
   key, root, deps, side, script and env facts, and each tree by content. Its consumers on this
   path read the list directly: engine_check.rs `plan` (the members' roots), `NamesStage` (each
   crate's key and deps), `tell_public` (the trees' origins), the bodies' handles (body_types.rs
   already serves them from the list) and crate_label. The world is then needed only where it is
   made: a discover, which writes the list, and a script or proc-macro run, which places its
   package's files. Floor: O(changed files), under 1 ms.

   A build script rightly woken by an edit (its `git status` answer moved, D168) does not stand
   in front of the body check: on the one driver (D157), a body that reads nothing of a script
   does not wait for it, so the script's 68 ms runs beside the 31 ms body, not before it. Nothing
   is built for this here: it comes with the one driver.
3. **The sweep** (55 ms): every file of the checkout is statted to find what moved. The kernel
   already knows: on macOS the file-system event log is kept by the system and can be read FROM A
   STORED EVENT ID with no process of ours alive (FSEvents `sinceWhen`, with the volume's UUID
   checked; rule 45: a system API where it is better, its symbol tested); a history the system
   says is incomplete (the must-scan flag, a changed UUID, an id from the future) is a full sweep,
   by name. Linux has no such log without a live process: the full sweep stays there until the
   optional helper with an expiry (scope) carries an inotify/fanotify set. Floor: O(changed paths).
4. **Layout and the stored answer's open** (20 ms together): in place, by key, behind 1-3.

With these and D149's three records an edit of one body is the body (0.1 s on tempo's
`is_empty`), its file's lines, and well under 50 ms of fixed work.

## Floors (rule 67)

Cold, tempo on an 18-core Mac: UNDER 10 SECONDS (the owner, 2026-11-17, on hearing "about 3 minutes from
parallelism alone": "that seems.... wrong like it should be way faster"). The count: about 1 ms of CPU a
body, so tempo's 15,793 bodies are about 16 s of CPU and 1 to 2 s on 18 cores; the names of its 1,208 crates
are 39 s of CPU today, a few seconds in parallel, and the dependencies' share is paid once a machine; a
merge per phase. The old engine on the same input (tag g090627): 1,445 s, 3,369 s of CPU, about 200 ms a
body: each body entered 4 times, 1,704 GB allocated in the run, a private world per file job (58.5 impl
lookups a question against 1.1 from shared tables), and a solver that sorts its read list and copies its
terms per goal. None of that is the cost of checking Rust. Every real run is reported as a ratio to 10 s,
and a stage whose CPU a body passes 1 ms names where it goes before anything else is built on it.

One body edit: one outline, one body, their cells' readers, the certificate. Nothing changed: the pointer
read and one get (0.09 s measured on tempo with no process alive).

## Pieces (each a lane; new code in new files; the old path stays until the switch)

| Piece | What | Check |
|---|---|---|
| E1 engine core | new crate `blaze-engine`, sans-IO: the plan from a unit graph and outlines, phases, bulk merge, dirtying by cell, the certificate; simulated on blaze-sim | history simulation: every request equals a fresh build; made equals the floor; planted bugs caught |
| E2 the view | `Tables`: the read-only world the checker asks (names by cell, heads, impls, lang items) with the per-task recorder; frozen heads | 1, 8 and reversed workers give equal bytes; a body's reads are exactly its cells |
| E3 impl tables | blaze-surface/blaze-impls records written from the impl rows; lookups through them | answers equal to today's buckets on fixtures; one probe per (crate, bucket) |
| E4 names by cell | the cell table in the names record; per-module records after | a private fn added makes 1 Signature and 1 BodyCheck |
| E5 tasks | names, heads, impls and body as functions over `Tables` with inline needs through claims | entries per result is 1.0 for planned kinds |
| E6 the switch | `blaze check` on the engine beside the old path, equal answers on fixtures and tempo; then the old path deleted | tempo cold seconds, GB and the edit loop quoted before and after; the deletion list above gone |
| E7 run and census | the lazy program and the eager program replaced by point 7 | hello world prints; the census equals the last one on the old path |
| E8 lints | `Stage::Lints`: one task per body running every selected lint in one walk dispatched by node kind (D135); findings per lint in one record; suppressions as records (an unused one is a finding); fixes with a safety level, applied to a fixed point of at most 100 rounds with the body checked again; advice after the answer | a body edit reruns that body's walk only; the profile gives each lint's cost; tempo's lint time beside its check time |
| E9 tests | `Stage::Tests`: a test's reads are the code records its run reached | one body edit reruns only the tests that reach it; the rest reused |
| E10 published records fetched (D132) | in this order: local names fast first; names records small (core's is 83 MB, 48 MB of it expansions kept as text); then R2 for optimized CODE of std and popular crates (dear to compute, small to ship); names records only by byte range, the pages a check touches (remote-read.md); never a whole pack of names (about 160 MB for a 6 s compute) | a fresh home's first check of a small crate: under 1 s for the shelf's names computed locally; bytes fetched per check quoted |
| E11 why | the cause of every entered task (new, its key moved, or the first read that moved: cell, old and new digest), a count of tasks redone to the same bytes, an "entered because" table in the profile, and `blaze why <file or item>` printing the chain from the edit (the owner, 10:29: "could u add better telm", on a private fn added costing 113 s with no row saying why) | on tempo, one private fn added: the table names each cause; redone-identical is 0 |
| E12 `blaze clean` | removes the checkout's `.blaze/v2`; `--store` the home's store; and a two-values refusal names the record's kind and name and says what to run (the owner hit one on 2026-11-17 with no way out) | the refusal's text; the command on a scratch home |
| E13 lazy names of dependencies | a dependency's names made on demand per cell (then per group as a record), from the paths your crates walk and the impl questions your bodies ask; measured first: the share of each dependency's modules a real project reaches | the first check of a small crate on a fresh home resolves a fraction of the shelf (today 27 crates whole, 6 s: gimli 2.3 s, object 1.8 s, libc 1.5 s, core 1.4 s); answers equal to the whole solve |
| E14 aspects | a head's cell split into parts with their own digests (types, bounds, lifetimes; existence, fields, layout; type, value; head, member list, member) and each read recorded by part | a field added to a struct enters no body that only names the type; a constant's value edited enters no reader of its type |

| E15 shared goals and instances | trait goals (canonical form) and generic instantiations are request-wide records made once through claims and read by every body, never a cache per worker or per body (the native TypeScript port keeps one per checker: instantiations and memory grow with the thread count) | goals solved per request against goals asked; memory flat from 1 to 16 workers on tempo |
| E16 every hit is a read | a cache hit of any kind (a solver memo, a claimed record, a frozen head imported, an impl table) appends the reads of what it returned to the reader's read list; an audit mode re-makes every hit and compares result and reads (Lean's differential check) | a planted "hit without its reads" is caught; the audit on the 58 real bodies reads equal |
| E17 an impl added | adding or removing an impl enters only the bodies whose recorded trait questions (trait, self shape) it can answer | an impl added for an unrelated type enters 0 bodies; for a queried shape, exactly its askers |
| E18 settled classes | facts of the toolchain and of registry packages are settled once per (toolchain, lock entry) and checked by one digest, never by read lists (salsa's durability, as records) | a second project on the same toolchain reads no dependency read list |

Order: E1 to E4 at once; E5 on E2; E6 when E1 and E5 pass their checks; E7 after E6; E8 and E9 on E6.

From the survey of 2026-11-17 (the owner asked for it; the report "Fast compiler and tooling architectures" and
its six sourced notes are kept with the session's notes, outside the tree): no surveyed system keeps fine-grained semantic state across a restart and reads it in place
(Zig, Buck2, Bazel, rust-analyzer lose it with the process; rustc rewrites its 77 MB dependency graph each
session), so that part of this design has no model to copy and every piece of it is measured here. What the
survey changed: point 8 (one lint walk per body) and pieces E15 to E18. What it confirmed with numbers: no
restarts (Skyframe restarts; DICE and Zig do not), heads computed without bodies with a cutoff on equal bytes
(Flow types-first: about 6x at p90), reads at name grain with absence (Zinc, Pyrefly: 3.6 s to under 0.2 s), no
positions in a head (Flow's blank-line rechecks), small records packed and never one mapping a file on macOS
(Lean: 7.6 s of a 10.2 s warm import spent mapping 52,490 files), no global lock on an interner (oxc: +30%),
whole-graph facts as keyed records (Zig: 30 ms of a 37 ms update is one reachability walk), deterministic work
caps in the solver (Swift dropped wall-clock limits), and that a faster back end alone moves little (the
Cranelift back end: 5 to 12% end to end on a large Rust project).

State, 2026-11-17 10:00: E1 admitted (crates/blaze-engine, simulated; a failed task is a row). E3's writer,
reader and equality proof admitted on one fixture. E4's cell table and recorder seam admitted. E2 in two
lanes (frozen heads; the view and the body task). E6's driver (`blaze engine-check`) started.
