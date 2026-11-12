---
status: plan
summary: The codegen step after an edit. Describes are memos; the driver keeps each program's walk (its items, the graph of what each refers to, what the roots reach) and its last `Built` by row, and a build patches both - the walk by the changed describes and a delete-and-re-derive rule over the graph, the `Built` by the rows of what the walk touched - so every pass costs the edit, not the program. Covers what is kept and its keys, each pass's dependency rule, why the patch equals the whole build, the cost of each pass, and what is not built.
date: 2026-11-10
---
# The incremental walk

Measured 2026-11-10 on `fn main() { println!("hi {}", 1); }` (13,936 functions) after a
one-line body edit: `unit codegen` 507 ms, of it `codegen walk` 340, `templates` 76, `finish`
60 (`delta` 44). The goal (ledger D32): under 10 ms, every pass in proportion to the edit.

## What is kept, and what keys it

| Kept | Key | Lives |
|---|---|---|
| a describe, a count | instance, requester, `Facing` | the database (memo) |
| the same, shared | instance, requester, round | `Kept`, per (database nonce, `Facing`) |
| a program's walk (`patch::Walked`) | roots, static roots, check roots, policy; compared by value | `Kept::walks` |
| its last `Built` by row (`lists::Lists`) | the same | `Kept::lists` |
| the entries of a `Built` not kept by row | the same and the lazy entry | `Kept::built` |

All of it names one database's ids: memory, never a store record. A restart starts with none
(`Fresh::Restart`). A build takes its program's walk and lists and puts its own back when it
ends well; a refused build keeps neither, so the next runs whole and its delta has no base.

A walk keeps (crates/blaze-codegen/src/driver/patch.rs): each *item* it described (a function
as one origin's code asks for it, a static) with its describe, key and count; the graph of
what each refers to with two sets over it (`support.rs`: `ALL`, what the check's roots and
the build's reach; `BUILT`, what the build's own roots reach); the nodes and statics in
`rows::Rows`, a sorted map in 64-entry chunks shared by reference count, so a change copies
one chunk; and what finds an item from the front end's name of it (by instance and
requester, by instance, by static) with the lists of those asked each build.

## Finding the changed set

The codegen walk starts from the set the front end names (written 2026-11-11, unbuilt and
untested). `patched` asks `Frontend::changed_since(view)` with the view the kept walk was
made in (`Walked::seen`) and gets the functions (instance and requester) whose describe, the
instances whose count and the statics whose describe may be another, or `Unknown` with why.
It asks again (`redescribe` and `counted`, on all cores) only the items it holds of those,
each found by a lookup (`Keep::functions`, `instances`, `values`), and the items asked each
build (`Frontend::each_build`, `static_each_build`): those whose describe reads beside the
database, a body or a static's value that names a file. A name the walk holds no item of is
ignored. A front end that tells no view (`Frontend::view` is `None`: one with no engine)
says by that that every item is asked each build, and the `ask functions` span says `every`.
A front end that tells a view and answers `Unknown` has lost its changed set: the build is
refused with the front end's reason, the kept walk and lists put back as they were
(`no_changed_set_error`; rule no-fallback), where until 2026-11-11 the walk asked of
everything it held.
`HirFrontend` answers from `described_memo::changed_since`, `counted_memo::changed_since`
and `described_static_memo::changed_since`, each key read back by the query's `arguments`
(crates/hir-ty-codegen/v2/frontend.rs). `Frontend::durable`, `group`, `written_since`,
`settle` and the walk's `Hangs` are gone; the durable view is still compared, and another
one walks whole. `Work` counts `asked`, `named` (what the front end named), `held` (what
the walk holds of it) and `each_build`.

### The changed set from the engine (2026-11-11; written, unbuilt and untested)

Measured on ripgrep 15.1.0, a one-line edit inside `fn main` (build 49): `codegen walk` 348
ms, 38 ms of it the walk's own thread, `patched: asked 86708 functions and statics of 0
groups, described 2`; `described_memo` verified 86,480. The same shape was in the unit's
check (`unit bodies` 484 ms, `body_check_clean` executed 6, verified 1,149): every memo of
the program or the crate verified by pull to find the handful an edit changed.

Why `group` is not the fix. A per-crate group is exact only if it covers every input a
describe can read (the files of every crate it reaches and thirty-odd other input kinds),
and it is too coarse: an edit of a binary's `main` writes the group of the binary, which
holds every function the binary's code asks for.

What is built (crates/blaze-query/src/db/reverse.rs, the core, and frontier.rs, the driver):

1. **Feeds**, one push each where the engine already does the work, never on a hit: a run
   that stores edges says which span it replaced; an input field written is logged; a row
   whose stamp moves to the view (not backdated; a cycle's rows; a row emptied) having had
   a value before, and a tracked field rewritten with another value, say they changed; a
   run that made an untracked read says so.
2. **Reverse edges** live in one index a storage, folded at each view boundary (`&mut`, no
   reader, before the edge array settles) from the edges runs stored since: a rerun adds
   only the nodes it gained; an edge is never taken out (a stale reader costs one
   verification, a missed one is a stale result), and when the index holds more than twice
   the edges the rows name it is made again from the rows. Memory: 4 to 8 bytes a reader
   and 4 a node, at most twice the forward array's 4 bytes an edge; reported as `reverse
   edges` in the storage's memory usage. At a million memos and ten edges each: 40 to 80
   MiB beside the forward array's 40. Interned values have no node, so no reader list.
   Not persisted: the storage is memory, and the index is made from the rows' edges.
3. **The walk** (`plumbing::walk_changed_set`, one walker at a time under its own flight,
   span `changed set` with `written`, `untracked`, `visited`, `edges`, `changed`,
   `unwound`): from the fields written since the last walk, every node that said it
   changed and every row whose last run was untracked, the readers are verified as a
   reader's edge is (run again where a read changed, backdated where equal), a level at a
   time in node order, on the pool past 64 rows; whatever changes says so and its readers
   are next; an unchanged row stops it. Each row is verified once and each node's readers
   gathered once, so it is bounded by the nodes it reaches.
4. **What a walk leaves**: every row made before it is verified in its view or unchanged.
   So until the next write, a pull of a row not verified in the view marks it with no look
   at its edges (`frontier::clean`): a host's pass over everything it holds costs a few
   loads a row. This is what a task of the body check pays for a memo the edit did not
   reach. The codegen walk and the body check ask only what item 5 names.
5. **Naming**: `plumbing::changed_since::<F>(db, view) -> Changed<F::Key>` and each tracked
   query's generated `changed_since` (IDs): the keys of `F` whose value changed since
   `view`, from a log of the walks' findings, or `Unknown` naming why (a view that is none
   of this storage's, or asked inside a query while a walk is due), which is a host's error:
   the body check and the codegen walk refuse to build on it (rule no-fallback). The log
   answers from any earlier view: past 256 walks or 2^20 nodes its oldest findings are
   merged into one of the newest view among them (`merge_oldest`), never dropped, so a host
   that asks from before them is named a few rows too many and is never left to sweep.
   It may name more, and only: a value that changed and changed back, a row whose
   verification unwound, a change at or before `view` when no walk ran between. A row
   first made after `view` is not named.

Exactness. A row changes only by running again, and runs again only for a read that
changed after it was verified, an untracked read, or being emptied. By induction over
walks, the first changed read `d` of a row that changes after the last walk changed after
that walk too, so `d` is a written field or said it changed, and the row is among `d`'s
readers: a run re-reads its edges up to and including the first changed one, so that edge
survives the run, and no edge leaves the index. Durability needs nothing: a written field
is followed whatever its kind. An untracked read puts its row in every walk.

Rows nothing asks for. A walk may verify a row whose key names what an edit removed, which
a pull would never run. A verification that unwinds for any reason but the caller's own (a
pending write, fuel, a wait that would close a cycle of tasks) is contained: the row's
word is put back by the engine's guard, the walk logs it (`unwound`), names it as changed,
goes on to its readers, never takes it as clean, and does not verify it again until
something else brought it up to date. Watch `unwound` and the panic hook's output on edits
that delete items.

Proof (`dst-not-review`): crates/blaze-query/src/db/tests/changed.rs. Random programs
(sums, cutoffs, reads chosen by a value, untracked reads) under random writes, pulls in
views no walk runs in, evictions, epochs and asks from any earlier ask; the oracle is a
from-scratch evaluation after every step: every row made by `V` whose value differs is
named, every row named moved in between, every row answers as the model, and the walk
visits no more rows than were made. Planted bugs it catches: a reverse edge not recorded,
a rerun's new edges not folded, a changed reader that does not pass the walk on, a cutoff
not applied, an evicted row passed over. Not in the simulation: cycles, tracked fields,
two tasks at once, and a verification that unwinds.

Next. The codegen walk starts from the named set ("Finding the changed set"); its
refinement and controls are in `driver/tests/kept.rs` (a changed function the set does not
name, a function asked each build that is not asked, the asks of one edit among 100 and
10,000 functions). The refinements of
`driver/tests/kept.rs` already hold a patched walk to the whole walk's.

The body check does the same (crates/hir/src/check/bodies.rs; written 2026-11-11, not yet
built or run). A plan of every body keeps, in memory by (database nonce, crates, style
lints), the file modules, each module's listed bodies and which checks gave a diagnostic,
with the view it ran in. The next plan asks `changed_since` of `crate_file_modules`,
`module_bodies` (both queries now), `module_check_clean` and `body_check_clean`, and holds
a slot only for the modules touched and a task only for what is named, what was not clean
(its diagnostics are not kept) and the bodies new in a module listed again; every other
answer is the kept one, clean, with no task and no fetch. It is whole, and its spans say
why (`body check plan`, `body check sweep`: `whole`, `why`, held, named, visited,
`kept_unvisited`), when nothing is kept, the engine answers `Unknown`, or a crate's file
modules changed. Its refinement test holds it to a whole plan's diagnostics over edits,
with two planted bugs and a count of the tasks of one edit among 20 bodies and 200. What
still grows with more than the edit: the slots of a touched module are all its bodies, and
a body or module that is not clean (a warning counts) is visited every build.
Where a check is kept and the engine answers `Unknown`, there is no plan: `NoChangedSet`
names the crates and the engine's reason, and the kept check goes back as it was.

### The work of an edit, counted, and what else follows the edit (2026-11-11; written, unbuilt and untested)

**The measure.** Each stage says once what it visited as a kept event `edit work` with its
stage's name and its counters (crates/blaze-query/src/db/edit_work.rs names the stages and
lists the counters): the engine's walk (`changed set`), each unit's plan (`body check`),
each program's walk (`codegen walk`), each dependency serve (`dependency image keys`) and
each link (`link`). `blaze explain edit_work` prints them by stage with the engine's memos
run and verified from `query_counts`; a build that counted nothing is an error. The test of
the whole front half is hir's `a_body_edit_touches_only_what_reads_the_body`: three crates,
the same body edit among 200 functions and among 2,000, every count equal and small; its
control edits a signature, which reaches every caller.

**Dependency image keys.** A key is the database's memo (`hir_def::deps`'
`crate_image_key`), where it was a host's walk forgotten at every view: an edit of a member
makes none again (0 at 4 libraries and at 40), and a key that changes is held to a fresh
database's.

**At the save.** The watcher's commit of a save, with no request waiting on it, starts the
head's last build-shaped request as a check on the new root, as an intent's job
(`Session::save_ahead`): the file is read into the database, the engine marks what read it
and the changed bodies are checked before `blaze run` is typed, and that request takes the
job over. Nothing is linked, patched or run ahead. At most `SAVES_AHEAD` between two
requests that build, and never beside a request's job.

**Small loops run here.** A loop of at most 32 items predicted under 2 ms from its own
site's small runs (`blaze_graph::par::Small`) runs on its calling thread with no job
submitted: the body check's tasks, the codegen walk's asks. The changed-set walk's levels
keep their own bound (under 64 rows on the walker's thread): a level's rows read each
other, and at 32 a level of about 55 rows went to the pool and its jobs waited on each
other without end (a hello-world body edit hung in `names`, 2026-11-11).
A unit's phases are still plane tasks, each a dispatch: the edit's chain is one thread
within a stage, not yet across them.

**Still the program's size, not the edit's.** A link goes over every name and every
patchable function of the program (`link`'s `symbols` and `patchable`), and a `Built`'s
lists are copied each build (`codegen walk`'s `copied`); the slots of a touched module are
all its bodies.

## The patched walk

Conditions, each checked; failing one, the walk runs whole and `Walk::whole` says which:
the inputs and the durable view are the kept walk's; the front end closes no dyn facts; the
build is not lazy; each changed function keeps its frame (item, origin, symbol, linkage,
address, `editable`, `over_editable`, `overflow_checks`), is one describe's key, is built into
no other and takes no key the walk holds; and nothing came of the edit that a whole walk
refuses (a failed describe, the recursion limit, a symbol defined twice, a dependency
referring to the workspace, a patchable function built in).

**Added and removed edges** (`support.rs`). Each member of a set has a level and a support:
its in-edges from members of a lower level, a root counting one more. The invariant: every
member has support and its out-edges go to members; so members are exactly what the roots
reach, and a cycle cannot hold itself up (its member of least level has no supporter in it).
An added edge supports its target or brings it in with what it reaches (new items are
described, a round at a time). A removed edge takes support away; a member left with none is
dropped, and in turn what it alone supported; then each dropped one that an edge from a
member still reaches is brought back above that member, with what it reaches among the
dropped. What stays dropped has no edge from a member, so no root reaches it.

**The recursion limit.** A whole walk with no refusal describes exactly what its roots
reach (a deferred instance the check claims is sent back; one past the limit is a refusal),
so the check decides only whether the build is refused. An instance's depth is at most the
other instances of its body, so with no body of more instances than the least limit admits
(drop glue a quarter), no order of claims refuses one: kept as counts, O(changed). Else
rustc's claim order is run again over the whole kept graph (`recursion::claims_past`), from
the first root: O(items + edges) array steps. A claim past the limit gives way to the whole
walk, which reports it.

**Each pass's rule**, evaluated again for the nodes one of its inputs changed for, found
through the graph's reverse edges:

| Pass | A node's value reads | Evaluated again for |
|---|---|---|
| unit, key, settings | its describe, the policy | changed and new |
| slot (`rebind`) | its describe, its callees' `patchable`, the slots of what it builds in | changed and new; a changed slot of one built into another gives way |
| places (`resolve`) | its references; each callee's key, slot, weak symbol | changed, new, and referrers of a node whose key or slot changed |
| built-in closure | its places at its built-in references, their closures | changed and new |
| symbols | each definer's symbol | added and removed (a count a symbol) |
| owners' check | its owner, its places' owners | changed, new, re-resolved, referrers of a changed owner |
| what the roots reach | the edges | `support.rs` |

## `Built` is kept and patched

`Lists` (lists.rs) holds a row a function and a row a static in `Rows`: everything the
`Built` says of it, its function as made and as dedupe left it, and its entry. A patched walk
names what it touched (`patch::Touched`); only those rows are made again:

| Pass | Rule | Evaluated again for |
|---|---|---|
| templates | the store's value under the node's template key, instantiated with its key, places and anchors | touched functions |
| statics | the same, under its definition's key | touched statics |
| versions, digests, names, flags, owner, `made` | pure of the node and its places | touched |
| dedupe | the least merge closed under "equal code after renaming" | touched; whole when a touched or removed function is in a class or equals a kept one |
| reach | flat arrays by place in key order | touched nodes' edges looked up; the rest renumbered |
| entries, delta, `BuiltId` | an entry a row; the ID through `Keyed` | rows replaced: the delta is the record of them |

A kept row is the value a whole build would make: the template key covers every input of the
template - template code, type arguments and settings (the template instance's key), the
bodies built in (`stored`), each place's kind and which is itself (`placed`), the toolchain
(`kernel::template`) - and the function is that template with the node's key, places and
anchors, all unchanged for an untouched node (rule keys-cover-every-input). A function
folded or built by a release backend is compiled with the whole build: such a build's lists
are made whole.

`Built`'s fields stay contiguous `Vec`s, which blaze-link, blaze-premap and
hir-ty-codegen index and slice: they are copied from the rows each build (`Work::copied`).

## Cost after an edit

N functions, C changed or new, D their referrers, L functions and statics asked each build
(those that name a file), M what the front end names as changed.

| Pass | Cost | Still linear because |
|---|---|---|
| asking | O(L + M) asks on all cores, each name a lookup; the engine's walk itself O(rows the edit reaches); O(N) when the front end names no set (`Unknown`) | |
| describes | O(C) | |
| graph, slots, places, built-in, symbols, owners | O(C + D) | |
| recursion limit | O(C), or O(N + edges) array steps when a body has more instances than the least limit | rustc's claim order is one depth-first walk; its state at the first changed claim is not kept |
| templates, statics, versions | O(C + D) lookups | |
| dedupe | O(C + D) hashes; O(N) when a class changes | a class's merge can cascade to its callers |
| reach | O(C + D) lookups, plus one copy of the arrays | readers index it by place |
| entries, delta, id | O(C + D) rows, O((C + D) log N) hashes | |
| the `Built`'s lists | one copy a row (N) | readers outside this crate take `Vec`s |

`Work` (in `Built::work`, and on the spans of `codegen walk` and `codegen finish`) counts
each; `the_work_of_an_edit_is_the_same_among_a_hundred_functions_and_ten_thousand` holds
them to the edit.

## The first edit after a cold build (2026-11-11; read, not yet measured)

Measured on ripgrep 15.1.0, 64 cores, one daemon: the first body edit after a cold build
5.7 s, each later one 0.95 s. Read from the code, what the first edit makes that a later one
finds:

- **Kept by a cold build, so not the difference:** its walk and lists (a whole build that is
  not lazy and ends well keeps both under its program; `codegen delta` now says `walk_kept`
  and `lists_kept`, `yes` or the reason), the describes' memos, the bodies' and modules'
  check memos (`body_check_clean`, `module_check_clean`: every unit's check is
  `Plan::of_every_body`), and the image's rows the link's delta is made against.
- **Let go by the cold build's release, and read again by value:** the edited crate's
  inference (by name, as the crate finished), and every crate's MIR and constants' values
  (by tag, at each release with no unit open and at the build's end). The check memos hold,
  so no check task reads a body; the readers are the lints of bodies
  (blaze-diagnostics' `lints/bodies.rs`, `before_abort` and `after_abort`), which read every
  body's inference, built MIR and runtime MIR in one loop on one thread. On ripgrep's
  binary crate that is about 1,165 bodies of each made again one after another, where the
  cold build had made them on all cores.

Done: a build that is no edit's holds the inference of the workspace crates it finished
last, at most `EDIT_CRATES` (blaze-cargo's `serve/unit/release.rs`, `Release::plan_of`),
so the first edit infers what changed. Not done, and what is left of the difference: the
MIR goes by its tag, which keeps none by crate, so the first edit still lowers each body's
built and runtime MIR again on that one thread. Either closes it: the lints of bodies run
a body a task on all cores, as the check does (then any state of the values costs the
cores' share, a restart's and a memory squeeze's too), or the release keeps the held
crates' `mir_body_query` and `runtime_mir_body_query` values, which needs an eviction by
tag that takes keys to leave. To tell from a profile: the first edit's `query_counts` rows
with `evicted_asked` (made again because let go) against `ran_changed` (an input changed).

## Not built

- **The `Vec` copy.** Removing it means `Built`'s lists are `Rows`, or `CompiledFunction`'s
  buffers shared, and every reader ported.
- **The dyn world.** A build that may close facts walks whole; narrowing needs the world's
  facts by digest and the body query keyed by the facts read (`MirOptions::world`).
- **Lazy builds** walk whole: the walk order is not kept.
- **Folded and release functions**: their compiles read the whole build.
- **A key of two describes** that changes, a changed function built into another, a changed
  frame: each gives way to the whole walk.
- **Persistence.** The kept walk names `HirInstance`s; restoring needs one made again from
  content.
- **`blaze why`** does not print `Walk::whole`, `Delta::fresh` or why the lists were made
  whole; they are on `codegen walk`'s span (`whole`, `fresh`), in `Built::walk`, and an event.
