---
status: plan
summary: The codegen walk keeps each function's describe between builds. The next walk asks the front end whether a kept describe still holds, which the query engine answers by verifying a memo, and describes only what an edit changed or newly reaches. Covers what is kept and what keys it, the cost per node, what the passes after the walk cost, the whole-program dyn facts, where the kept walk lives, and what is not built yet.
date: 2026-11-10
---
# The incremental walk

Measured 2026-11-10 on `fn main() { println!("hi {}", 1); }` after a one-line body edit: the
bin's `codegen walk` took 9.5 s and ran 13,936 `codegen describe` spans (72 s of CPU), every
function `main` reaches, std's included. `blaze why` said 3 things were recomputed.
Invalidation was minimal; the walk was not.

## Design

1. A describe is a pure function of the database, the instance, who asks, and five facts of
   the front end (`Facing`: its units by content, build-time, image, which hashes, and the dyn
   world where the instance takes one). It is a tracked query, `described_memo`
   (crates/hir-ty-codegen/v2/frontend.rs), whose value is the small `Described`.
2. The driver keeps each describe it was given (`driver::Kept`, crates/blaze-codegen/src/driver/kept.rs),
   by `(instance, requester)` and round, shared with the node that uses it (`Arc`).
3. The next walk starts from the roots as always. For a function with a kept describe it asks
   `Frontend::redescribe(instance, requester, was)`. `None` means a describe now would give
   `was`; the walk takes it and no describe runs. Otherwise it gets the new describe, keeps
   it, and follows its references, so what is newly reached is described and what is no
   longer reached is never visited.
4. Everything else the walk computes (the recursion limit's graph, what the build's own roots
   reach, the sort, slots, places, built-in closure) is computed again from the describes in
   hand. Those passes read only the walk's own tables.

The walk over kept describes therefore runs the same code as the fresh walk; only where a
describe comes from differs. Equivalence rests on one statement: `redescribe` returns `None`
only when a describe would equal `was`.

## What is kept, and what keys it

| Kept | Key | Where |
|---|---|---|
| `Described` per function | instance, requester, `Facing` | the database's memo (`described_memo`) |
| the same value, shared | instance, requester, round | `Kept`, one per (database nonce, `Facing` without world) |
| the last walk's keys per program | hash of roots, static roots, check roots | `Kept::programs`, for the `dropped` count only |

The policy, the roots and the check roots are not inputs of a kept describe: a node's unit,
spec and slots are made from the describe and the build's policy every walk. A describe's
inputs beyond the database are exactly `Facing`, which is in the memo's key (rule 21). Units
are named by their place in a process-wide list interned by content (`give_units`).

One input is not the database's: what the build calls a file a body names by `file!()`
(blaze-cargo's naming). A body that names one is not memoized (`Memoed::NamesFiles`): it is
described each build and compared with the kept describe. The memo's own hashes panic if
asked for a file's name, so a disagreement between the two rules cannot key silently.

## How "unchanged" is decided, and its cost

`HirFrontend::redescribe` fetches the memo and compares it with `was`. The query engine
verifies the memo without running it: one compare for a row whose inputs are all a library's
(durability `HIGH`: std and registry crates), one summary test for a row none of whose input
fields were written since, else a walk of its edges, which marks every row it proves so
shared dependencies are checked once per build. Only a row whose inputs changed runs the
describe (MIR and all). Per unchanged node: one intern lookup of the key, the verification,
and a field-by-field compare of two `Described`s; no allocation, no MIR.

## What each pass costs after an edit

All are linear in the walk's functions and references, with no front-end work except where
said; each has a span.

- Levels (`codegen walk`: `described`, `verified`, `reached`, `dropped`, `fresh`): per
  function one kept lookup, `redescribe`, `Build::node` (the policy's choice and a hash of
  the name) and the recursion graph's inserts.
- `recursion limit: count a level` asks the front end for `Counted` of every function
  (untracked, a few reads each) and `recursion limit check` walks the graph.
- `codegen reach` (what the build's roots reach, the sort), `codegen classify` (slots),
  `codegen resolve` (places, built-in closure): hash lookups and pushes over the walk's tables.
- `codegen owners`, `codegen symbols`: one pass each over the nodes.
- `codegen templates`, `codegen statics`, `codegen dedupe` (inside `codegen finish`): one
  store read per function and a pass over all compiled code. Unchanged by this design.
- `devirt world` (a final binary): reads the walk facts of every reached function from
  their memos. Unchanged by this design.

Statics are described every build; a program has few.

## Whole-program dyn facts

A final binary's walk runs twice when its first round closes dyn facts (devirt.md). The two
rounds keep their describes apart. The memo is keyed by the world only for an instance that
takes it (`devirt::takes_world`: a workspace function or closure with a `dyn` site, inlined).
A changed world rekeys exactly those bodies and nothing else; a body whose read digests are
unchanged then gets the same code hash and no compile. Narrowing the re-describe to bodies
whose read digests name a changed fact needs the body query keyed by the facts read rather
than the world's id (`MirOptions::world`, hir-ty-mir), which is not done.

## Where it lives, and a restart

A describe names the front end's instances, which are ids of one database. So `Kept` is
memory beside that database, never a store record: `HirFrontend::kept` hands out one per
(database nonce, `Facing`), the eight last used. A restarted daemon or a database made again
has a new nonce, so the first walk describes everything; the span and `Built::walk` say
`fresh` with the reason. Nothing is restored, so nothing can be restored wrong. A lost `Kept`
over a live database costs a memo fetch and a clone per function, not a describe.

## The invariant, and its tests

The refinement: for any program and edit sequence, the build over kept describes equals the
build over none. `crates/blaze-codegen/src/driver/tests/kept.rs` draws a call graph (members,
dependencies, generics of one key, statics, a dyn site, a check root), up to eight edits, and
compares the two builds after each. Three controls plant a bug each. A counter test holds an
edit of one leaf among 300 functions to one describe.

## Not built

- `blaze why` does not print the `fresh` reason yet; it is on the span and in `Built::walk`.
- `Counted`, statics, `devirt world`, and the passes after the walk stay linear in the program.
- The walk itself is replayed, not patched: its cost is linear with a small constant.
