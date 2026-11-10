---
status: plan
summary: The codegen walk after an edit. Each function's describe is a memo of the front end's database; the driver keeps the describes and the whole walk of each program; a build patches the kept walk when an edit changes what functions are and not what they refer to, and walks whole otherwise, saying why. `Built` carries an id and a delta against the last build. Covers what is kept and what keys it, the patch's conditions and why it equals the whole walk, the cost of each pass, and what is not built.
date: 2026-11-10
---
# The incremental walk

Measured 2026-11-10 on `fn main() { println!("hi {}", 1); }` after a one-line body edit: the
bin's `codegen walk` took 9.5 s and ran 13,936 `codegen describe` spans (72 s of CPU), every
function `main` reaches. `blaze why` said 3 things were recomputed.

## The layers

1. **Describes are memos.** `described_memo`, `described_static_memo` and `counted_memo`
   (crates/hir-ty-codegen/v2/frontend.rs) are tracked queries keyed by the instance, who asks
   and `Facing`: what describing reads of the front end beside the database (its units by
   content, build-time, image, which hashes, and the dyn world only where the instance takes
   one). A body or static that names a file by `file!()` is no memo's: the name is
   blaze-cargo's, not the database's. The memo's own hashes panic if asked for a name.
2. **The driver keeps describes** (`driver::Kept`, crates/blaze-codegen/src/driver/kept.rs) by
   `(instance, requester)` and round, shared by every program walked over one database and
   `Facing`. A whole walk asks `Frontend::redescribe(instance, requester, was)` for each kept
   one; `None` takes it with no describe.
3. **The driver keeps each program's walk** (`patch::Walked`): nodes with their places,
   statics, roots, who asked for what, the reverse of every place, and which functions are
   not durable. A build patches it (below) or walks whole.
4. **`Built` carries a delta** (`driver::Delta`, `BuiltId`; crates/blaze-codegen/src/driver/delta.rs).

## What is kept, and what keys it

| Kept | Key | Lives |
|---|---|---|
| a describe | instance, requester, `Facing` | the database (memo) |
| the same, shared | instance, requester, round | `Kept`, per (database nonce, `Facing`) |
| a program's walk | roots, static roots, check roots, policy id; compared by value | `Kept::walks` |
| a program's last `Built`: id and entries | the same and the lazy entry | `Kept::built` |

`HirFrontend::kept` hands out one `Kept` per (database nonce, `Facing` without world), the
eight last used. All of it names one database's interned ids, so it is memory, not a store
record; a restart starts with none (`Fresh::Restart`).

## Finding the changed set

blaze-query gained `plumbing::durable::<F>(db, key)` (the row is current and read inputs of
kind `HIGH` alone; a tracked function has `durable(db, args..)`) and
`Storage::durable_view()` (the view the last `HIGH` write opened, never renumbered). After a
whole walk the driver asks `Frontend::durable` of each function once and keeps the rest as
*volatile*. While the durable view is the kept one, durable functions are not looked at;
each volatile one is verified by its memo. The engine keeps no reverse index from written
fields to rows (it would hold one entry per edge again), so candidates are the volatile
functions: O(functions that read a workspace input), not O(changed).

## The patched walk

Conditions, each checked (crates/blaze-codegen/src/driver/patch.rs); failing one, the walk
runs whole and `Walk::whole` says which:

1. The inputs are the kept walk's; the build is not lazy.
2. The front end closes no dyn facts (`Frontend::closes`: no unit MIR-inlines, a dev build)
   and its durable view is the kept one.
3. Every volatile function's count and every static's describe are the kept ones, and each
   changed describe keeps its shape: item, origin, references, built-in places, symbol,
   linkage, address, `editable`, `over_editable`.
4. A changed function is one describe's key, is built into no other, takes no key the walk
   holds, and keeps its slot kind.

Why the result is the whole walk's. The whole walk is a function of each reached function's
describe and count and each static's describe. By 2 and 3 every count, static and reference
list is the kept one, so the graph, the levels, the recursion limit's claims and what the
roots reach are the kept walk's, which had no error. A slot (`rebind`) reads the policy's
choice, `editable`, `over_editable` and the slots and built-in places of what a node refers
to: all kept. A node's places read its references and each callee's key, slot and weak
symbol: only a changed callee's key differs, and every referrer is renamed through the
reverse edges. A built-in closure names built-in callees' keys: by 4 none changed. So each
changed node takes its new describe, unit and key at its place in key order, and each place
that named the old key names the new one. The refinement test holds this.

## `Built`'s delta

An entry is one content id of all a `Built` says of a function or static (listed at
`Delta`). `Delta { base, functions, statics, fresh }` names the keys added, changed and
removed since the `Built` whose id is `base`. `BuiltId` hashes the word-wise wrapping sums of
the entries, so it moves by the entries added and removed and equals a fresh build's.

## Cost after a leaf body edit

N functions, R references, V volatile functions, S statics, C changed, D referrers of changed.

| Pass | Cost | Still linear because |
|---|---|---|
| describes | O(C) | |
| `codegen walk`, patched | O(V + S) asks, O(C + D) touched, plus moving up to N nodes one place in key order | `Sorted` is three arrays in key order |
| `codegen walk`, whole (first build, a changed reference, a release build, a lazy build) | O(N + R) | replayed over kept describes |
| `codegen owners`, `symbols` | O(N + R) | not patched |
| `codegen templates` | O(N) store reads | every template is read and instantiated |
| `codegen statics`, `dedupe`, `finish` | O(N) and all code bytes | `Built`'s lists are made whole |
| `codegen delta` | O(N) and all code bytes | entries are made from the whole `Built` |

## Not built

- **Deletion and addition.** A changed reference walks whole. The rule to build: keep support
  counts; drop a node at zero and what it supported; then re-derive from the roots over the
  dropped set only. Sound: a node reachable in the new graph is kept or found by the
  re-derive, which starts from kept nodes' edges into the dropped set. Complete: a kept node
  has a supporter that is kept, by induction from the roots; a cycle with no outside support
  is dropped whole and not re-derived.
- **The recursion limit's claims.** They are rustc's depth-first order over the whole graph;
  with a changed edge the claims from the first changed one on must be re-run. Not kept.
- **Owners, symbols, templates, dedupe, the assembly of `Built`, the delta's entries.** Each
  needs the kept result of its pass and `Built`'s lists shared rather than rebuilt; the delta
  is defined so they can produce it directly.
- **The dyn world** is rebuilt whole and rekeys every body that takes it; a build that may
  close facts walks whole. Narrowing needs the body query keyed by the facts read
  (`MirOptions::world`, hir-ty-mir).
- **Lazy builds** walk whole: the walk order is not kept.
- **Persistence.** A kept walk names `HirInstance`s. Item and instance keys are content ids,
  so a record could hold the graph by key; restoring needs a `HirInstance` made again from
  content (its item path and arguments re-interned), which nothing does yet.
- **`blaze why`** does not print `Walk::whole` or `Delta::fresh`; both are on spans and in `Built`.
- A patch that gives way after asking has asked every volatile function once more than the
  whole walk that follows needs.
