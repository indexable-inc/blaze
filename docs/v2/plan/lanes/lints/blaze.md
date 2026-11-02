---
status: plan
summary: blaze's own lints (`blaze::hot_clone`, `hot_alloc`, `hot_alloc_call`, `hot_dyn`, `std_hashmap`, the group `blaze::hot`) — what each flags, how to exempt a line, and what they miss; the page each finding's help line links to.
date: 2026-10-29
---
# blaze's own lints

`crates/blaze-lints/src/blaze.rs` and `src/blaze/`. They check the rules
`tools/v2/hot_lint.py` checks by pattern ([principles](../../goals/principles.md): allocate
like a game engine, no `dyn` in hot paths, no SipHash), but from types and resolutions: a
`.clone()` of a `Copy` struct is not flagged, a `String` behind a type alias is, and so is a
call of std's that allocates three calls down.

Each lint is allow by default. Turn the group on where code is hot:

```rust
#[deny(blaze::hot)]
fn step(world: &mut World) { /* ... */ }
```

or for a crate, `--deny=blaze::hot`. `#[allow(blaze::hot)]` (or `#[allow(blaze::hot_alloc)]`,
...) turns it off again for an item. `blaze::hot` is all five lints below.

## Exempting a line

A finding is dropped when its line, or the line above it, has a comment with the lint's tag and
a reason:

| lint | tag |
|---|---|
| `hot_clone` | `// clone: <reason>` |
| `hot_alloc`, `hot_alloc_call`, `std_hashmap` | `// alloc: <reason>` |
| `hot_dyn` | `// dyn: <reason>` |

```rust
let name = name.clone(); // clone: kept past the frame, once per load
// alloc: the error path
let message = format!("missing {name}");
```

The reason must not be empty: `// clone:` alone exempts nothing. A doc comment (`///`, `//!`)
is not an exemption. These are the tags `hot_lint.py` reads, so a line exempt for one is exempt
for the other.

## `hot_clone`

A call of `Clone::clone` (`x.clone()`, `Clone::clone(&x)`, `T::clone(&x)`) whose `Self` costs
more than a copy. A type is copy-like, and its clone not flagged, when it is `Copy`; a
reference, raw pointer, function or function pointer, primitive or `!`; or a tuple, array or
struct or enum whose `Clone` is built in or made by `#[derive(Clone)]` and every field of which
is copy-like. The message says why the clone costs:

- *allocates*: a hand-written `Clone` that reaches the allocator (`String`, `Vec`, `Box`);
- *bumps a reference count*: `Arc`, `Rc` and their `Weak`s;
- *runs a hand-written `Clone`*: one that does not allocate, but is code of its own;
- *clones a type not known to be `Copy`*: a type parameter bound by `Clone` and not `Copy`, a
  projection;
- *clones what a closure captures*.

When the cost is in a field (`#[derive(Clone)] struct Named { name: String }`), a note names
the field's type. Fix: borrow, copy an ID, or make the type not `Clone`.

## `hot_alloc`

A call that reaches the global allocator. A function reaches it when it is the allocator's
(`#[rustc_allocator]`, `#[rustc_allocator_zeroed]`, `#[rustc_reallocator]`: the
`__rust_alloc` family), the `exchange_malloc` lang item or the `box_new` intrinsic, or when a
call in its body reaches it. This is asked of the instance called, its generic arguments
given, with each trait method resolved to the impl that runs: std's `Vec::with_capacity`
allocates only through `A: Allocator` a few calls down, and `collect` only through
`B: FromIterator`, so a question about the function alone (not its arguments) would answer
"no" for both. Recursion is fine: a cycle of functions starts as not reaching and is iterated
to its answer.

The walk stops at a function of a workspace member (a crate whose origin is local): its
allocations are found in its own body, once, and a call of it is not a finding. It goes on
through std and registry crates, and through a workspace function they call back through a
generic only for `hot_alloc_call`. So a crate denying `hot_alloc` (`hot_alloc = "deny"` in
`[workspace.metadata.blaze.lints]`, or `#![deny(blaze::hot_alloc)]` in its root) is told where each allocation is made, not at every caller up the stack.

Flagged: `String::from`, `.to_string()`, `.to_owned()`, `.to_vec()`, `Vec::with_capacity`,
`.collect()` into an owning type, `Box::new`, `Arc::new`, a call of a registry crate's function
that does any of these. A macro call is reported once, at the call: `format!`, `vec![1, 2]`.

Also flagged, though it allocates nothing yet: a call that makes a value of the global
allocator's, which allocates from the global heap once it grows (rule `bump-allocate`):
`Vec::new()`, `vec![]`, `String::new()`, `VecDeque::default()`, `FxHashMap::default()`. A value is
the global allocator's when a type argument of it is `Global`, or it is `String` or std's
`HashMap` or `HashSet` (a map hashing with `RandomState` is `std_hashmap`'s finding instead). A
call makes one when its callee is a trait's method other than `clone`, or a method of the type's
own impl; a function that hands one on (`mem::take`) does not. Fix: name the allocator
(`Vec::new_in(frame)`, from `blaze_mem::frame::current`), grow a buffer the caller keeps, or say
why it needs the global heap with `// alloc: <why>`.

Not flagged:

- A collection that names its allocator: `Vec::new_in(frame)`.
- A value of the global allocator's made by a method of another type
  (`opt.unwrap_or_default()`, `s.into_bytes()`) or a struct of your own holding one
  (`Index::default()`): only the call that makes the collection itself is found.
- `Clone::clone`: `hot_clone` says it.
- A `&mut self` method on a place (`buf.push(x)`, `self.out.extend(..)`): a buffer the caller
  keeps grows only past its capacity, which a reused buffer reaches once. On a temporary it is
  flagged.

What it misses, and why:

- A call of a workspace function that allocates: `hot_alloc` finds it in that function, and
  `hot_alloc_call` at the call.
- Operators (`a + b` on `String`), `Drop`, and calls a pattern or `for` loop makes: only call and
  method call expressions are followed.
- A call through `dyn` or through a type parameter of the hot function itself (`f: F` with
  `F: Fn()`, `T: Clone`): which impl runs is not known in the function, so the call reaches
  nothing. Through `dyn` it is `hot_dyn`'s.
- A closure handed to a function (`map(|x| x.to_string())`) where the closure, not the
  function, allocates and the closure's body is not this body's: a closure written in the hot
  body is part of it, and its calls are checked there.
- An instance whose generic arguments grow past 256 type nodes (a function that instantiates
  itself with ever larger arguments, which rustc refuses at its `type_length_limit`): not
  followed.
- Derived methods other than `clone` and `default` (`fmt`, `hash`, `eq`), which have no body of
  their own: counted as not reaching. Formatting allocates only when written into a `String`,
  which the `format!` call is flagged for.

## `hot_alloc_call`

A call that reaches the global allocator only through a function of a workspace member, which
`hot_alloc` does not walk into. It is in `blaze::hot` and not turned on by `hot_alloc`: an item
marked `#[deny(blaze::hot)]` promises that nothing it calls allocates, the workspace's own
helpers included, so the call of `helper()` is a finding there, through any depth of calls.
Under `hot_alloc` alone the same allocation is one finding in `helper` itself.

## `hot_dyn`

A call that goes through a vtable: a method call or path call resolved to a trait's method with
`Self` a `dyn Trait` once references and boxes are peeled (`shape.area()` with
`shape: &dyn Shape`, `Shape::area(&*boxed)`), and a call of a `dyn Fn*` value through any
pointer (`f(x)` with `f: &dyn Fn(u32)` or `Box<dyn Fn()>`). `dyn` written in a type, with no
call through it, is not flagged. Fix: a concrete type, a generic, or a `match` on a tag.

## `std_hashmap`

std's `HashMap` or `HashSet` hashing with `RandomState` (SipHash, seeded per map). Two forms:

- a written type: `HashMap<K, V>` or `HashMap<K, V, RandomState>` in a signature, a field, a type
  alias, a `let`'s type, a turbofish;
- an expression that makes one: `HashMap::new()`, `HashSet::with_capacity(n)`,
  `Default::default()`, `.collect()` (with no turbofish, which is a written type of its own).

Not flagged: any other hasher (`blaze_hash::HashMap`, `HashMap<K, V, FixedState>`, a hasher
parameter `S`). A `let m: HashMap<K, V> = HashMap::new();` has two findings, the type and the
call; one `// alloc:` comment exempts both. Fix: `blaze_hash::HashMap` (or `HashSet`), or index
by content hash (`blaze_hash::ByHash`).

Misses: a `HashMap` imported under another name (`use std::collections::HashMap as Map;`
then `Map<K, V>`) in a written type, which is found by its last segment's name before it is
resolved; the expression form still finds what such a type makes.

## Moving code to the standard structures

The moves of AGENTS.md rule 9 (std's locks to `parking_lot`'s, `std::sync::mpsc` to
`crossbeam_channel`, `rustc_hash` to `blaze_hash`) are `disallowed-*` entries of `clippy.toml`
with a `replacement`, not lints of blaze's own: clippy's `disallowed_types`, `disallowed_methods`
and `disallowed_macros` find them by resolution and `blaze check --fix` makes the fixes
([lints.md](../lints.md#clippytoml-bans-disallowed_types-disallowed_methods-disallowed_macros)).

## `error_debug`

Deny. AGENTS.md rule 15: formatting an error must not panic, and rule 14: an error names its item
in words. An error's message is the body of `Display::fmt` of a type implementing
`core::error::Error`, a `format_args!` whose value flows (through calls and blocks, at most 12
expressions up, never out of a closure or loop) into an expression whose type implements
`Error` (`Failed::new(format_args!(..))`, `MyError::Parse(format!(..))`), or a thiserror
`#[error("..")]` template on an error type or variant. In one:

- a `{:?}` placeholder is a finding, whatever it formats;
- a placeholder of any trait whose value needs the attached database is a finding, unless the
  `format_args!` is inside a closure handed to `hir_ty::attach_db`, `attach_db_allow_change` or
  `with_attached_db`. A value needs it when its type is an ADT of the crate `hir_ty` (its
  `next_solver` types included), has one as a generic argument, or is an ADT outside the
  standard library with a field (three ADTs deep) that does; the note names the `hir_ty` type
  a holder holds.

What a `format_args!` formats is read off its lowering, the calls of
`core::fmt::rt::Argument::new_debug` (`new_display`, ...) with the argument's type, so captured
names (`{ty:?}`), a renamed import of the macro and one written by a macro are all found. The
message names the error type and the type formatted. A thiserror template's placeholder is
matched to a field by name or index; a template given arguments of its own is judged only for
`{:?}`. Fix: render what needs the database into the error when it is made, while the database
is attached, and name the item with `Display`.

## `dropped_error`

Warn, with a shrink-only baseline. AGENTS.md rule 3: an error thrown away for a value is a quiet
fallback. On a `core::result::Result`, found by resolution (the method is of `Result`'s own
impl, the pattern's variant is the lang item `Err`, so a renamed `Result` and a macro's code are
found):

- a `match` arm `Err(_)`, `Err(..)` or `Err(_name)` whose value is neither `()` nor `!` (an arm
  that returns, breaks or panics ends the function instead);
- `.ok()`, `.unwrap_or(..)`, `.unwrap_or_default()`;
- `.unwrap_or_else(|_| ..)` and `.map_err(|_| ..)` whose closure's one parameter is `_` or
  `_name`; a closure that reads the error, or a function path, is not one.

The message names the error type thrown away. Where dropping is right, say why with
`#[expect(blaze::dropped_error, reason = "..")]`.

Overlap with the clippy ports: when `clippy::unused_result_ok` is on, `x.ok();` as a statement is
its finding, and when `clippy::map_err_ignore` is on, `.map_err(|_| ..)` with a non-`move`
closure whose parameter is exactly `_` is its; this lint leaves those places out, so no place is
reported twice. Everything else (`let _ = r.ok()`, `.ok()` in a chain, `|_why|`, a `move`
closure, `unwrap_or*`, `Err` arms) is only this lint's.

## `untyped_error`

Warn, with a shrink-only baseline. AGENTS.md rule 14: an error names its item and the rule that
refused it, and a caller can tell one failure from another. A function whose return type or a
parameter's type holds, anywhere (`Option<Result<_, String>>` too), `Result<_, String>`,
`Result<_, Box<dyn Error>>` (with any auto traits) or `anyhow::Error` (so `anyhow::Result<T>`).
Types are resolved: a renamed import, an alias and a macro-written function are found. The
message names the function and the type.

Checked in a library target, and in a binary or example outside its crate root (the `main`
module may report to a person and exit; a module path decides it, not a file name). Not checked:
test, bench and build-script targets, code under `#[test]` or `#[cfg(test)]`, and a method of a
trait impl, whose signature is the trait's and is found there. Fix: an enum of what can fail,
each variant naming its item.

## `hash_order_output`

Warn, with a shrink-only baseline. AGENTS.md rule 20: no output depends on hash-map iteration
order. A hash map or set (std's `HashMap`/`HashSet` with any hasher, hashbrown's, papaya's,
dashmap's, `blaze_hash`'s) iterated with `iter`, `keys`, `values`, `drain`, `into_iter` (and the
`_mut` and `into_` forms), whose order reaches, with no sort between:

- a `collect` into a `Vec`, `VecDeque` or `String` that is returned, stored in a field or place,
  or handed to a call, itself or through the local it is bound to; a local some `sort*` method
  is called on, and a chain through `sorted*`, are sorted;
- in a `for` loop over it, a push into a `Vec` or `String` (`push`, `push_back`, `push_str`,
  `extend`) never sorted after, a write (`write!`, `write_str`, `write_all`), or a hash (`hash`,
  a `Hasher`'s `write_*`).

A chain ending anywhere else (`sum`, `count`, `max`, a `collect` into a `BTreeMap` or another
hash map) has no order to leak. Calls are resolved, so renamed map types, aliases (`FxHashMap`)
and macro-written loops are found. The message names the map's type and where its order goes.
Misses: an order carried through a closure (`for_each(|k| out.push(k))`) or a second local.

## `wall_clock`

Warn. AGENTS.md rules 11 and 20: a core reads time only from its injected clock, and no output
depends on the wall clock. No walk of its own: `Instant::now`, `SystemTime::now` and
`Instant::elapsed` are entries built into the `disallowed-methods` query, in every crate, and
their findings are reported under this lint (stock clippy refuses a `lint` key in
`clippy.toml`, so the entries can't live there;
[lints.md](../lints.md#clippytoml-bans-disallowed_types-disallowed_methods-disallowed_macros)):
its level, `#[expect(blaze::wall_clock, reason = "..")]` and its baseline apply, not
`clippy::disallowed_methods`'. A driver that does the IO keeps its reads with an `#[expect]`
saying why.

## `held_across_wait`

Deny. AGENTS.md rule 22: a fiber can resume on another thread after any wait, so nothing held
across a wait may be tied to a thread. The calls that may park the fiber, and what is live across
each, are the may-suspend check's (`hir_ty_suspend::suspend_points`, docs/v2/plan/lanes/may-suspend.md),
never a second analysis. A value that check already rejects (not `Send`, or `#[must_not_suspend]`)
is its error, and is not repeated here. This lint finds the values whose type is `Send` and that
are still tied to the thread:

- a reference lent into the thread's storage: the parameter of a closure handed to
  `LocalKey::with`, `try_with`, `with_borrow` or `with_borrow_mut`, or to `blaze_mem::frame::current`
  (a `&AtomicU32` from a `thread_local!` is `Send`, and points at the first thread's slot);
- a value that is or holds `blaze_graph`'s `NoWait`, the marker of a region that must not wait;
- a value with a drop that holds a `LocalKey` (the guard of stdx's `lend`, the 2026-10-31 crash):
  its drop gives back to the slot of whichever thread the job resumed on.

One finding per call, at the call, naming each value (its binding, or "a temporary", and its
type), why it is tied, and the path of calls to the fiber's switch (`` `variance` → `fetch` →
`get_or_run` → `suspend` ``). A generic body is checked as its own crate instantiates it; an
instance only another crate makes is the may-suspend check's to report.

## `lock_across_query`

Deny. AGENTS.md rule 9: a lock is held only for a field update, never across a query call. A
query may run long or wait itself, and every task that wants the lock queues behind it, so the
lint holds whether or not the query parks the fiber.

A query is a function made by an attribute macro of `blaze_query_macros` (`tracked`, `input`,
`interned`), read off the macro the attribute on the callee's own definition resolves to, so a
renamed import of the attribute counts and a function merely named like one does not; or a
method of a trait implemented for `base_db::Db` (`HirDatabase`'s). A guard is `lock_api`'s
(`parking_lot`'s guards are its types) or std's, itself or inside a tuple, array or field. What
is live across the call is the MIR's (`MirBody::held_across_calls_in` over the runtime body and
its closures), so a guard dropped before the call is fine however its scope is written.

One finding per call, at the call, naming each guard (its binding or "a temporary", and its
type), the query, and what makes it one.

## `blocking_on_pool`

Warn. AGENTS.md rule 9: on a pool thread or fiber, a wait goes through the scheduler. A blocked
pool thread runs no other job until the call returns. The functions that run there are the
may-suspend check's (`hir_ty_suspend::on_fiber`: those the fiber driver's code reaches, and
those that may suspend), never a second analysis. In them, a finding is a call of:

- a `Condvar` wait (std's, `parking_lot`'s), a channel receive or its blocking iterator (std's
  `mpsc`, `crossbeam_channel`'s), `JoinHandle::join`, `thread::park`;
- `thread::sleep`, `spin_loop`;
- blocking IO: a function of `std::fs`, the IO methods of `File`, `OpenOptions::open`, the
  sockets of `std::net`, and `Read` or `Write` on a file or a stream.

The finding names the call and the function, and says what to wait with instead: for a sleep,
`sched::wait(Wait::Until(deadline))`; for the others, an `Event` or `Wait::Key` the other side
completes; for IO, a driver thread that sets an `Event`. The non-blocking neighbours
(`try_recv`, `notify_*`, `File::options`) are fine. `on_fiber` lists functions, so a closure is
checked as part of the function it is written in.

## `unnamed_wait`

Warn. AGENTS.md rule 12: a wait opens a span naming what it waits for. The profile test fails on
a wait with no name when it runs; this lint finds one where it is written. It is deleted once the
typed wait API (%195) makes the name part of the call's type. A finding is:

- `Flights::get_or_run` or `hold`, which name the wait only by the key's hash (use the `_named`
  forms);
- `get_or_run_named` or `hold_named` whose naming closure writes nothing, or only a placeholder
  (`""`, `"wait"`, `"todo"`, `"fixme"`, `"..."` and the like);
- `wait::wait`, `wait::lock` or `wait::wait_until` named by a placeholder literal;
- `sched::wait` or `EventWait::wait`, which take no name, and a `Condvar` wait on a driver
  (functions not on the pool; on the pool it is `blocking_on_pool`'s), outside the block of a
  `wait::wait` that names them.

The API is found by name resolution in the dependency named `blaze_graph`, so renamed imports
and re-exports count; `blaze_graph` itself, where the API is written, is not linted.

## `shared_collection_lock`

Warn. AGENTS.md rule 9: a shared map is `papaya::HashMap` (a shared set `papaya::HashSet`), and
the results of parallel items come back from `blaze_graph::par` in order, never pushed into a
`Mutex<Vec>`. A finding is:

- a field (of a struct, a tuple struct or a variant) or a static whose type is a `Mutex` or
  `RwLock`, std's or `lock_api`'s (`parking_lot`'s are its types), around a map (std's
  `HashMap` with any hasher or `BTreeMap`, hashbrown's and blaze-hash's `HashMap`, indexmap's
  `IndexMap`) or a set (std's `HashSet` or `BTreeSet`, hashbrown's and blaze-hash's `HashSet`,
  indexmap's `IndexSet`); the lock itself or inside `Arc`s and lazy or once cells (std's
  `LazyLock` and `OnceLock`, once_cell's `Lazy` and `OnceCell`). The types are resolved, so a
  renamed lock, an alias (`FxHashMap`, `parking_lot::Mutex`) and a macro-written field count.
  For a hashed collection written out with its arguments the fix replaces the lock's type with
  `papaya::HashMap<..>` or `papaya::HashSet<..>` of the same arguments, `MaybeIncorrect`: the
  uses change with it (`.lock().insert(..)` becomes `.pin().insert(..)`). A static gets the fix
  only inside a cell (papaya's types have no `const` constructor; the note says to put it in a
  `LazyLock`). An ordered collection has no fix;
- a local bound to a lock around such a collection inside at least one `Arc`, where the `Arc`
  is made: an initializer that is a path or a `clone` hands on one made elsewhere and is not a
  finding. No fix (the local's type is inferred); the note names `Arc<papaya::HashMap<..>>`;
- a `push` or `extend` of `Vec`, through a lock guard (std's or `lock_api`'s `Mutex` guard or
  `RwLock` write guard), inside a closure passed to a function of `blaze_graph::par`, found by
  resolution as above. The message names the `par` function and the function it is in; the
  note points at `par::map`, `filter_map` and `par::extend`.

A lock around anything else, a collection alone, `papaya`'s own types and a lock in a local
with no `Arc` are not findings.

## `open_wait_loop`

Warn. AGENTS.md rule 13: every loop that waits for progress has a bound (fuel, depth or a
deadline from the injected clock) and ends in a typed outcome. A finding is a `loop` or `while`
loop whose body, outside the loops and closures in it, sleeps, spins or waits: it calls
`std::thread::sleep`, `yield_now`, `park` or `park_timeout`, `spin_loop` (core's or std's), a
`Condvar` wait (std's or `parking_lot`'s) or `blaze_graph`'s `wait` or `wait_until`, found by
resolution, so a renamed import and a macro-written loop count. The loop is bounded, and not a
finding, when a condition it ends on compares something: the `while` condition, or the `if`
condition or match guard around a `break` of the loop or a `return` in it, holds an ordering
comparison (`tries < 8`, `start.elapsed() > timeout`) or an equality of integers (`left == 0`).
The message names the loop's function, the wait and the condition it waits for. A loop with no
way out (a service loop), a `for` loop and a loop that never waits are not findings. There is no
fix: the bound and the error it ends in are the author's to pick.

## Cost

The per-node part looks at path types only, and resolves one only when its last segment is
`HashMap` or `HashSet`. The body part walks the body's HIR once, after the body's inference,
and allocates only to report. Whether an instance reaches the allocator is a front-end query
(`reaches_alloc`), shared by every body that calls it and kept across edits that do not change
the functions it reaches.
