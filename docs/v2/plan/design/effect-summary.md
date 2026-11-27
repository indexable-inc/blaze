---
status: plan
summary: One interprocedural effect summary per monomorphized instance (effects.md's walk, extended with sync and lock kinds, the lock classes taken, held-while-taking edges and guards held across a block), and the lints on it - loop_turn_effects and sync_on_answer_path (deny, on marked functions), lock_order and lock_across_wait (internal, allowed). Not built - the four lints are declared in crates/blaze-lint-defs and their attributes are known to blaze-surface, but no walk computes the summary and no checker reports them.
date: 2026-11-13
---
# The effect summary, and the lints on it

Builds on effects.md (the walk, the seed tables) and named-locks.md (lock names). A blocking
call on a loop's turn, an fsync on the answer path, two locks taken in opposite orders: each is a
property of the call graph and is decidable before the code runs.

**What exists.** The lints are declared in `crates/blaze-lint-defs/src/lints.rs`
(`loop_turn_effects` and `sync_on_answer_path` Deny, `lock_order` and `lock_across_wait` Allow);
`bounded_turn`, `answers_request` and `lock_checked` are known attributes
(`crates/blaze-surface/src/tool_attrs.rs`); `crates/blaze-real/extensions/a_bounded_turn.rs` is a
case a working layer refuses. No summary is computed and no checker reports any of the four, so a
Deny here refuses nothing today.

## The summary

One summary per instance (`Instance { def, args }` under its param env), not a second walk. Its
body form is **monomorphized runtime MIR**: the only form where a trait call on a known impl has
resolved to the impl's method and a generic's callee is known. A check of a generic definition
asks it of each instance the crate makes, and an `Open` kind marks a call only an instance
decides.

Recursion is a fixpoint over the cycle: every set only grows, with each witness keeping the
fewest calls, so the fixpoint settles. Each kind keeps one witness, the site it came through or
what the body does itself at a span, and a diagnostic prints the chain:
`` `turn` (loop.rs:2543) → `submit` (loop.rs:900) → `std::fs::File::sync_all`, which syncs the
file to the disk ``.

### The bits

| asked | kind (`Rule`) | from |
|---|---|---|
| Blocks | `Block` | MODELS block rows, ROWS, FOREIGN `FB` |
| Io | `Io` | the std models' leaves (files, network, stdio, processes) |
| Syncs | `Sync` | `File::sync_all`/`sync_data`, blaze_sys `fsync`/`fdatasync`/`sync_file_range`, FOREIGN `FS` |
| Allocates | `Alloc` (global), `AllocIn` (named allocator) | effects.md |
| Locks | `Lock` + the lock classes | lock rows: std, lock_api, `blaze_core::lock` |
| HoldsAcrossWait | `Locks::across` | guard live across a call that has `Block` |
| Panics | `Panic` | effects.md |
| Unbounded | `Loop`, `Recursion` | effects.md |

How calls are handled:
- A direct call unions the callee's set, and a trait call on a known impl is that call.
- A `dyn` call or function pointer takes the closed set of what the graph made pointers and
  vtables of, or `Dyn` at a root that is handed one.
- What the walk cannot follow is `Unchecked`, and a foreign function no table knows is
  `ForeignUnknown`. Every new effect refuses both, so an opaque call is refused by name and
  never guessed clean (no-fallback).

Seeds are three tables, sorted by home and kind: the toolchain's models by resolved path, the
registry crates' rows plus `blaze_core` and `blaze_sys`, and foreign functions by symbol (effects.md).
Code outside those tables is walked when its body lowers.

### Lock classes

The class of an acquire comes from its receiver, lockdep-style, followed back through borrows,
moves and the toolchain's calls that hand a lock on (`Arc`/`Box` `deref`, `Arc::new`):
- `Type.field` for a field of an ADT, through an `Arc` or `Box`; `Type.field[]` for an element;
- the static's path for a static (`STATICS[i]` too);
- the name, quoted, where `blaze_core::lock`'s `Lock::new("name", ..)` is in the same body;
- otherwise `type` and the lock's own type (a parameter, a lock read out of a collection).

A guard's class follows plain moves and the toolchain's `Option`/`Result` unwraps. A function
whose return slot holds a classed guard says so (`Locks::returns`), so a `fn state(&self) ->
Guard` accessor carries the class to its caller; one known only by type becomes the
accessor's path (`State::jobs()`) there, so two accessors are two classes.

Per instance, `Locks` holds three sets:
- `taken`: the classes taken, each with its fewest-calls witness.
- `order`: the edges "holds A while taking B", its own and its callees'.
- `across`: a guard held across a call that blocks. A call the guard is passed to (a condvar's
  `wait`) is not counted, because that call releases the guard. A call of a foreign function
  whose row has `Block` (`read`, `fsync`) counts too, unless it is trusted `nonblocking`. Such
  a call is not a site of the walk, so the walk keeps its own list of them for this pass.

Each set is sorted and capped at 32 entries. A summary past the cap, or one reaching a body
that does not lower, is `wide`: unknown, never taken as clean. The lock lints report only what
they can show (a cycle with both chains, a guard across a named call); a wide summary is
refused only at a function that asked (`#[blaze::lock_checked]` on it or `#![..]` on a module
around it, or a role attribute), and is silent elsewhere: most code reaches a body that does
not lower. An edge from a class to itself (two locks of one coarse class, or a re-take) is not
added: it would make every such class a cycle.

## The lints (role lints deny, lock lints allowed in `blaze::internal`)

- **`blaze::loop_turn_effects`**: `#[blaze::bounded_turn]` refuses `Block`, `Io`, `Sync`,
  `Lock`, `Unchecked` and `ForeignUnknown` through every callee. The allow-list of uncontended,
  loop-owned locks is effects.md's trust word: `#[blaze::bounded_turn(trusted = "why")]` on the
  accessor that takes such a lock. It is per function because a lock's class is not a name a list can hold across
  crates.
- **`blaze::sync_on_answer_path`**: `#[blaze::answers_request]` refuses `Sync`, `Unchecked`
  and `ForeignUnknown`.
- **`blaze::lock_order`**: the crate's graph of `order` edges, over the summaries of its
  non-generic functions (callees' edges included, so an order fixed in a dependency counts). A
  cycle is one finding at the function whose own body adds an edge of it, printing both
  chains: where A is held while B is taken, and the reverse.
- **`blaze::lock_across_wait`**: a guard of class A live across a call that has `Block`
  (thread-blocking, the gap between `held_across_wait`, which covers fiber parks, and
  `lock_held_across_slow`, which covers IO and allocation). The finding names the guard's
  class, the call and the call's block witness.
- **No allocation** is effects.md's `#[blaze::expect(..)]` without `alloc` (refuses `Alloc`,
  `Unchecked` and `ForeignUnknown` through every callee), not a second lint.
- **`blaze::pool_self_wait`**: needs a pool identity in the type (`Pool<Id>` with a zero-sized
  marker per pool), which would make it a lock-class-like edge; without one, which pool a wait
  goes to is a runtime value and the question cannot be decided statically.

## Not here

- **Keys cover every input** needs a `Reads` set per instance: every file and environment read
  reachable, beside the kinds. Then a key's declared inputs can be compared with what its value
  function reads.
- `#[blaze::expect(io, sync, lock)]` words: the positive list is the end state for the new
  kinds as well. Adding words changes every existing list's meaning (a list without `lock`
  would refuse locks), so they wait for a pass that respells the lists in the tree.
- What still collapses to the type: a lock handed in as a parameter, one read out of a map or a
  `Vec`, guards stored in structs or moved into closures. The elements of one array are one
  class, so two taken in both orders are unseen; a field reached both directly and through an
  accessor is two classes, so a cycle through both is missed. A false cycle through a coarse
  class takes `#[expect(blaze::lock_order, reason = ..)]`.
- `lock_across_wait` skips a direct acquire while a guard is held (that is `lock_order`'s), but a
  callee that only takes a lock still has `Block` and counts as a wait. Telling the two apart
  needs the block witness to say whether it is an acquire.
