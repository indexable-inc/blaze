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
8. **Lints and tests are stages too.** A lint is a task after Bodies: one per (lint, body), reading that
   body's typed result record and the cells the lint asks, its findings a record keyed like the check
   (rule 81); an edited body reruns its own lints, a changed lint reruns that lint alone. Errors print
   with the answer; advice and whole-program analyses are later phases that never hold the answer back.
   A test is a task whose reads are the code records its run reached (the run's reach list), so a
   request reruns exactly the tests whose reached code moved, and the rest are reused by key.
9. **A live process** is allowed and not needed: everything above is records, so a process that exits
   loses nothing and a restart re-indexes nothing. One may be added later to save process start.

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
| E8 lints | `Stage::Lints`: a task per (lint, body) over the typed result; advice after the answer | a body edit reruns its lints only; a lint's edit reruns that lint only; tempo's lint time beside its check time |
| E9 tests | `Stage::Tests`: a test's reads are the code records its run reached | one body edit reruns only the tests that reach it; the rest reused |
| E10 published records fetched (D132) | in this order: local names fast first; names records small (core's is 83 MB, 48 MB of it expansions kept as text); then R2 for optimized CODE of std and popular crates (dear to compute, small to ship); names records only by byte range, the pages a check touches (remote-read.md); never a whole pack of names (about 160 MB for a 6 s compute) | a fresh home's first check of a small crate: under 1 s for the shelf's names computed locally; bytes fetched per check quoted |
| E11 why | the cause of every entered task (new, its key moved, or the first read that moved: cell, old and new digest), a count of tasks redone to the same bytes, an "entered because" table in the profile, and `blaze why <file or item>` printing the chain from the edit (the owner, 10:29: "could u add better telm", on a private fn added costing 113 s with no row saying why) | on tempo, one private fn added: the table names each cause; redone-identical is 0 |
| E12 `blaze clean` | removes the checkout's `.blaze/v2`; `--store` the home's store; and a two-values refusal names the record's kind and name and says what to run (the owner hit one on 2026-11-17 with no way out) | the refusal's text; the command on a scratch home |
| E13 lazy names of dependencies | a dependency's names made on demand per cell (then per group as a record), from the paths your crates walk and the impl questions your bodies ask; measured first: the share of each dependency's modules a real project reaches | the first check of a small crate on a fresh home resolves a fraction of the shelf (today 27 crates whole, 6 s: gimli 2.3 s, object 1.8 s, libc 1.5 s, core 1.4 s); answers equal to the whole solve |
| E14 aspects | a head's cell split into parts with their own digests (types, bounds, lifetimes; existence, fields, layout; type, value; head, member list, member) and each read recorded by part | a field added to a struct enters no body that only names the type; a constant's value edited enters no reader of its type |

Order: E1 to E4 at once; E5 on E2; E6 when E1 and E5 pass their checks; E7 after E6; E8 and E9 on E6.

State, 2026-11-17 10:00: E1 admitted (crates/blaze-engine, simulated; a failed task is a row). E3's writer,
reader and equality proof admitted on one fixture. E4's cell table and recorder seam admitted. E2 in two
lanes (frozen heads; the view and the body task). E6's driver (`blaze engine-check`) started.
