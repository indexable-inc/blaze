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
5. **The coordinator is off the hot path.** It makes the plan, starts each phase, and merges each
   phase's buffers in bulk, sorted by key (rule 25): puts to the store, rows to the answer table. Its
   cost is a constant per phase plus a sort, never a step per task.
6. **An edit.** The sweep gives the changed files; their outlines give the changed cells; the reverse
   index by cell gives the readers; those tasks are the plan, run in the same phase order. A reader whose
   cells did not move is not entered. Nothing changed is the certificate and nothing else.
7. **One front end.** `check` plans every body asked; `run` plans the entry and makes the rest on first
   call through the same inline rule; the census is `run` with every instance asked.
8. **A live process** is allowed and not needed: everything above is records, so a process that exits
   loses nothing and a restart re-indexes nothing. One may be added later to save process start.

## Floors (rule 67)

Cold: the semantic work divided by the cores, plus a merge per phase (tempo's first 17,000 bodies were
about 290 s of CPU: about 20 s on 18 cores). One body edit: one outline, one body, their cells' readers,
the certificate. Nothing changed: the pointer read and one get.

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

Order: E1 to E4 at once; E5 on E2; E6 when E1 and E5 pass their checks; E7 after E6.
