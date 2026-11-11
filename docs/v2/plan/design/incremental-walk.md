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
one chunk; and when each function is asked again.

## Finding the changed set

A function is asked again (`redescribe` and `counted`, on all cores) by what its inputs are:
never while the durable view is the same (`Frontend::durable`); when an input of its group
was written since the kept view (`Frontend::group`, `written_since`, `view`); else each build.
Statics are asked each build. blaze-query has no reverse index from written fields to rows,
and a row's Bloom summary is all ones once it reads a crate's name resolution, so the engine
cannot name the changed memos; it gained `plumbing::input_written_since::<I>(storage, id,
view)` (a load per field, exact or `true` after renumbering) for a front end to tell a
group's writes by. **`HirFrontend` does not implement `group` yet**
(crates/hir-ty-codegen/v2/frontend.rs, beside `durable`): until it does, every function that
is not durable is asked each build.

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

N functions, C changed or new, D their referrers, G groups, L functions asked each build.

| Pass | Cost | Still linear because |
|---|---|---|
| asking | O(L + G + statics), on all cores | `HirFrontend::group` is not implemented, so L is every function not durable |
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

## Not built

- **`HirFrontend::group`** and a per-crate input for it to ask `input_written_since` of.
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
