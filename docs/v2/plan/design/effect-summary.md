---
status: plan
summary: One interprocedural effect summary per monomorphized instance (hir-ty-pure's walk, extended with sync and lock kinds, the lock classes taken, held-while-taking edges and guards held across a block), and the deny-by-default lints on it - loop_turn_effects, sync_on_answer_path, lock_order, lock_across_wait - with no_alloc served by the existing noalloc.
date: 2026-11-13
---
# The effect summary, and the lints on it

Author: claude-opus-5-5. Builds on effects.md (the walk, `EffectDef`, the seed tables) and
named-locks.md (lock names, `lock_held_across_slow`, `held_across_wait`).

## Why

A capture of the daemon (2026-11-12) showed its loop thread 60 s inside `loop turn > the
plane's step > unit narrowed identity`: front-end work, on the thread that must only turn. The
rule (`daemon.rs`, `turn_after_wait`) is written down and was held by a run-time span alone,
because `#[blaze::nonblocking]` on the turn is off for a stage-0 bug. Nothing checks it. A
blocking call, an fsync on the answer path, two locks taken in opposite orders: each is a
property of the call graph and is decidable before the code runs.

## The query

There is one summary, `hir-ty-pure`'s `walk::Summary`, computed by `summarize` as the tracked
query `sealed::summary(env, Instance)`. It is not a second walk. Its key is the instance:
`Instance { def, args }` under a `StoredParamEnvAndCrate`, so the cache is per function,
monomorphized (granular-caching). Its body form is **monomorphized runtime MIR**
(`dispatch::body_once`). That is the lowest form the lints already get, and the only one where
a trait call on a known impl has resolved to the impl's method and a generic's callee is
known. A check of a generic definition (`check_generic`) asks it of each instance the crate
makes, and an `Open` kind marks a call only an instance decides.

Recursion is a fixpoint over the cycle: `Summary::recursive()` is the cycle's starting value,
and every set only grows, with each witness keeping the fewest calls, so the fixpoint settles.

Each kind keeps one witness: the site it came through, or what the body does itself at a
span. `function::refusal` unrolls the witnesses into the chain a diagnostic prints:
`` `turn` (daemon.rs:2543) → `submit` (daemon.rs:900) → `std::fs::File::sync_all`, which syncs
the file to the disk ``.

### The bits

| asked | kind (`Rule`) | from |
|---|---|---|
| Blocks | `Block` | MODELS block rows, ROWS, FOREIGN `FB` |
| Io | `Io` | the std models' leaves (files, network, stdio, processes) |
| Syncs | `Sync` (new) | `File::sync_all`/`sync_data`, blaze_sys `fsync`/`fdatasync`/`sync_file_range`, FOREIGN `FS` |
| Allocates | `Alloc` (global), `AllocIn` (named allocator) | existing |
| Locks | `Lock` (new) + the lock classes (`locks.rs`) | lock rows: std, lock_api, `blaze_core::lock` |
| HoldsAcrossWait | `Locks::across` | guard live across a call that has `Block` |
| Panics | `Panic` | existing |
| Unbounded | `Loop`, `Recursion` | existing (`loops.rs`) |

How calls are handled is the walk's, unchanged:
- A direct call unions the callee's set, and a trait call on a known impl is that call.
- A `dyn` call or function pointer takes the closed set of what the graph made pointers and
  vtables of, or `Dyn` at a root that is handed one.
- What the walk cannot follow is `Unchecked`, and a foreign function no table knows is
  `ForeignUnknown`. Every new effect refuses both, so an opaque call is refused by name and
  never guessed clean (no-fallback).

Seeds stay in their three existing tables, which are sorted by home and kind, not merged into
one file:
- `effects::MODELS`, the toolchain, by resolved path.
- `effect_attrs::ROWS`, the registry crates plus `blaze_core` and `blaze_sys`.
- `effect_attrs::FOREIGN`, by symbol.

Code outside those tables is walked when its body lowers.

### Lock classes

Lock names are runtime strings (`Lock::new("x", ..)`), so the static class of an acquire comes
from its receiver, lockdep-style:
- `Type.field` when the receiver is `&(*p).field` of an ADT;
- the static's path when it is `&STATIC`;
- otherwise the lock's own type.

A guard's class follows plain moves and the toolchain's `Option`/`Result` unwraps. A function
whose return slot holds a classed guard says so (`Locks::returns`), so a `fn state(&self) ->
Guard` accessor carries the class to its caller.

Per instance, `Locks` holds three sets:
- `taken`: the classes taken, each with its fewest-calls witness.
- `order`: the edges "holds A while taking B", its own and its callees'.
- `across`: a guard held across a call that blocks. A call the guard is passed to (a condvar's
  `wait`) is not counted, because that call releases the guard.

Each set is sorted and capped at 32 entries. A summary past the cap, or one reaching a body
that does not lower, is `wide`: a lint treats it as unknown and refuses it once, at the function
where it starts (no callee of it is wide), not at every caller. An edge from a class to itself
(two locks of one coarse class, or a re-take) is not added: it would make every such class a
cycle.

## The lints (deny by default)

- **`blaze::loop_turn_effects`**: `#[blaze::bounded_turn]` (new attribute, effect
  `BOUNDED_TURN`, `roles.rs`) refuses `Block`, `Io`, `Sync`, `Lock`, `Unchecked` and
  `ForeignUnknown` through every callee. The allow-list of uncontended, loop-owned locks is the
  existing trust word: `#[blaze::bounded_turn(trusted = "why")]` on the accessor that takes such
  a lock. It is per function because a lock's class is not a name a list can hold across
  crates. Applied to `Daemon::turn_after_wait` and `Daemon::step_plane`.
- **`blaze::sync_on_answer_path`**: `#[blaze::answers_request]` (effect `ANSWERS_REQUEST`)
  refuses `Sync`, `Unchecked` and `ForeignUnknown`. Applied to `Daemon::reply`,
  `inspect::Inspect::answer` and `observe::answer`.
- **`blaze::lock_order`**: the crate's graph of `order` edges, over the summaries of its
  non-generic functions (callees' edges included, so an order fixed in a dependency counts). A
  cycle is one finding at the function whose own body adds an edge of it, printing both
  chains: where A is held while B is taken, and the reverse.
- **`blaze::lock_across_wait`**: a guard of class A live across a call that has `Block`
  (thread-blocking, the gap between `held_across_wait`, which covers fiber parks, and
  `lock_held_across_slow`, which covers IO and allocation). The finding names the guard's
  class, the call and the call's block witness.
- **`blaze::no_alloc`** is the existing `blaze::noalloc` (`NOALLOC`: refuses `Alloc`,
  `Unchecked` and `ForeignUnknown` through every callee). A second lint for the same thing
  would break one-concept-one-home, so the work here is applying it to hot functions read to
  be allocation-free.
- **`blaze::pool_self_wait`**: not built. `blaze_graph::pool::Pool` is one non-generic type,
  and which pool a `submit` or a wait goes to is a runtime value (an `Arc<Pool>` field, a
  thread-local current pool). No type or path distinguishes a worker's own pool from another,
  so "this task waits on its own pool" cannot be decided statically. Deciding it needs a pool
  identity in the type, `Pool<Id>` with a zero-sized marker per pool, which would make it a
  lock-class-like edge. That change is the pool's to make, and this lint follows it.

## Not here

- **Keys cover every input** needs a `Reads` set per instance: every query, file and
  environment read reachable, beside `kinds`. Then a key's declared inputs can be compared
  with what its value function reads. `memory.rs`'s `MemoryEffects` is the nearest existing
  half. Not attempted.
- `#[blaze::expect(io, sync, lock)]` words: the positive list is the end state for the new
  kinds as well. Adding words changes every existing list's meaning (a list without `lock`
  would refuse locks), so they wait for a pass that respells the lists in the tree.
- Lock classes through `Arc<Lock<T>>` fields, guards stored in structs, and guards moved into
  closures fall back to the lock's type as the class, which is coarser, never unsound for
  order: two locks of one type share a class. That can report an order cycle between two
  different locks of the same type, and such a site takes `#[expect(blaze::lock_order, reason
  = ..)]`.
- `lock_across_wait` skips a direct acquire while a guard is held (that is `lock_order`'s), but a
  callee that only takes a lock still has `Block` and counts as a wait. Telling the two apart
  needs the block witness to say whether it is an acquire.
