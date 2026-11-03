---
status: plan
summary: blaze's own lints (`blaze::hot_clone`, `hot_alloc`, `hot_alloc_call`, `hot_dyn`, `default_hasher`, `implicit_allocator`, `large_move`, `cold_error`, the group `blaze::hot`, and the rest) — what each flags, how to exempt a line, and what they miss; the page each finding's help line links to.
date: 2026-10-29
---
# blaze's own lints

`crates/blaze-lints/src/blaze.rs` and `src/blaze/`. They check the rules
`tools/v2/hot_lint.py` checks by pattern ([principles](../../goals/principles.md): allocate
like a game engine, no `dyn` in hot paths, no SipHash), but from types and resolutions: a
`.clone()` of a `Copy` struct is not flagged, a `String` behind a type alias is, and so is a
call of std's that allocates three calls down.

Each lint is one of two kinds (`blaze::Kind` in `blaze.rs`). The general ones name only std
and widely used crates' types and warn by default in every workspace `blaze check` checks: the
group `blaze::general` (`false_sharing`, `seqcst_unexplained`, `collect_then_consume`,
`hash_twice`, `outline_generic_body`, `cold_error`, `frozen_collection`, `intern_candidate`,
`small_map`, `arc_clone_only_borrowed`, `blocking_in_async`, `relaxed_publish`,
`unwind_across_ffi`, `unbounded_from_input`, `dos_hasher_on_input`, `panic_in_drop`,
`bounds_check_in_loop`, `unbuffered_io`, `stdout_lock_in_loop`, `front_vec_ops`,
`missing_size_hint`, `fmt_allocates`, `jagged_vec`, `stable_sort_needless`,
`dyn_in_hot_field`), documented
for users in
[docs/lints.md](../../../../lints.md). The rest need blaze's own types and conventions, the
group `blaze::internal`, and are allowed until a workspace turns them on under
`[workspace.metadata.blaze.lints]` (blaze's root `Cargo.toml` sets `internal = "warn"`; a group
there applies before the single lints it names). Turn the hot group on where code is hot:

```rust
#[deny(blaze::hot)]
fn step(world: &mut World) { /* ... */ }
```

or for a crate, `--deny=blaze::hot`. `#[allow(blaze::hot)]` (or `#[allow(blaze::hot_alloc)]`,
...) turns it off again for an item. `blaze::hot` is the six lints from `hot_clone` to
`implicit_allocator` below, `large_move` and `cold_error`.

## Exempting a line

A finding is dropped when its line, or the line above it, has a comment with the lint's tag and
a reason:

| lint | tag |
|---|---|
| `hot_clone` | `// clone: <reason>` |
| `hot_alloc`, `hot_alloc_call`, `implicit_allocator` | `// alloc: <reason>` |
| `default_hasher` | none: `#[expect(blaze::default_hasher, reason = "..")]` |
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
`HashMap` or `HashSet` (a map hashing with `RandomState` is `default_hasher`'s finding instead). A
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

## `default_hasher`

AGENTS.md rules standard-structures and deterministic-output: the hasher is an explicit choice.
Was `std_hashmap`, which saw only std's map with `RandomState`; the old name still works as an
alias (`#[allow(blaze::std_hashmap)]`, `--warn=blaze::std_hashmap`, the registry's renamed
lints). Warn everywhere, deny wherever `blaze::hot` is.

A collection's hasher is the type parameter of its ADT whose default implements `BuildHasher`,
or that the ADT bounds by it: std's `HashMap` and `HashSet`, hashbrown's, indexmap's,
dashmap's, papaya's, any ADT made that way, found by its declaration, never by its name. Two
forms:

- a written type (a signature, a field, a type alias, a `let`'s type, a turbofish) that leaves
  the hasher to its default (`HashMap<K, V>`, `hashbrown::HashMap<K, V>`) or writes one seeded
  at random per process (std's `RandomState`, foldhash's `fast`/`quality` `RandomState`,
  ahash's `RandomState`, rustc-hash's `FxRandomState`), resolved through aliases;
- an expression that makes one hashing with a random seed: `HashMap::new()`,
  `HashSet::with_capacity(n)`, `hashbrown::HashMap::new()`, `Default::default()`, `.collect()`
  (with no turbofish, which is a written type of its own).

The message names the item and the type and says what to choose: `blaze_hash::HashMap`/`HashSet`
(fixed-seed foldhash, deterministic, blaze's default), `blaze_hash::ByHash`/`ByHashSet` (on
`KeyHasher`) when keys are content hashes already, `blaze_hash::Blake3Hasher` for persisted
content ids; a random seed only where keys come from an untrusted party and HashDoS matters,
kept with `#[expect(blaze::default_hasher, reason = "..")]`. No comment exempts it. For std's
map or set written with its hasher left out, in a crate that depends on `blaze_hash` and with a
`Hash` key, the finding suggests `blaze_hash::HashMap` (or `HashSet`) for the path, as
`MaybeIncorrect`: `HashMap::new()` and `with_capacity` exist only for `RandomState`, and the
type may meet a signature that names std's map.

Not flagged: an alias that fixes a deterministic hasher (`blaze_hash::HashMap`, `ByHash`,
`FxHashMap`), a deterministic hasher written out (`HashMap<K, V, FixedState>`,
`BuildHasherDefault<FixedHasher>`), a hasher parameter `S` the caller picks. A
`let m: HashMap<K, V> = HashMap::new();` has two findings, the type and the call.

Baseline: `crates/blaze-lints/baselines/blaze.default_hasher.txt`, not made yet (no baseline
is: [lints.md](../lints.md#baselines) says why); it is started by `blaze check --workspace
--target aarch64-unknown-linux-gnu --baselines crates/blaze-lints/baselines --new-baseline
blaze::default_hasher`.

Misses: an ADT written with no generic arguments at all (every parameter defaulted), and an
alias with a hasher parameter of its own left to its default, in a written type; their
constructors are still found when the default is seeded at random. A hasher held outside a
collection (`s: RandomState` used for `hash_one`) is not looked at.

## `implicit_allocator`

AGENTS.md rule bump-allocate: every value names its allocator. Warn everywhere, deny wherever
`blaze::hot` is, with a shrink-only baseline (`crates/blaze-lints/baselines/blaze.implicit_allocator.txt`,
not made yet, as no baseline is ([lints.md](../lints.md#baselines)); it is started by `blaze
check --workspace --target aarch64-unknown-linux-gnu --baselines crates/blaze-lints/baselines
--new-baseline blaze::implicit_allocator`). A type is implicit when it is or holds:

- an ADT whose allocator parameter has `Global` as its argument (`Vec<String>` is
  `Vec<String, Global>`; `Box`, `VecDeque`, `Rc`, `Arc`, `BTreeMap`, hashbrown's maps, and any
  ADT of the workspace's alike). The parameter is found by what it is, a type parameter bounded
  by `Allocator` or defaulting to `Global`, never by its name;
- a type of std's that takes no allocator and always allocates from the global heap: `String`,
  `PathBuf`, `OsString`, and std's `HashMap` and `HashSet`, so `blaze_hash`'s, which are std's
  with a fixed hasher.

Three places, each reported once, at the outermost spot:

- a written type: a signature, a field, a type alias, a static or const, an impl header, a
  bound, a closure's parameter, a cast. `Vec<String>` is one finding, at the `Vec`;
- a call that makes one: its value or generic arguments hold an implicit type none of its
  inputs holds (`Vec::new()`, `vec![]`, `collect()`, `Box::new(..)`, `String::from(..)`, a
  turbofish). A call that hands one on (`unwrap`, `clone`, `mem::take`) is not one; a call of a
  workspace function whose own return type is implicit is found at that signature;
- a `let`: with a written type when the type is implicit, without one when the binding holds
  what a call in its initializer made. The call inside it is not a second finding.

A type behind a reference, pointer, function or `dyn` counts in a written type only.

The fix is the author's (no machine-applicable one: `blaze_mem::frame::current` lends the frame
to a closure, so no rewrite in place is sound): `Vec::new_in(frame)`, `Box::new_in(.., frame)`
and `blaze_mem::frame::collect(frame, iter)` inside `frame::current(|frame| ..)`, a request's
`ArenaLocal`, or the revision or image allocator. For the types that take no allocator the note
names `blaze_mem`'s own: `String` is `blaze_mem::String<A>` (`frame::string`, `format_in!`;
finished text can go in `Frame::alloc_str`), `PathBuf` is `blaze_mem::PathBuf<A>` and
`OsString` is `blaze_mem::OsString<A>` (both unix only: `frame::path`, `frame::os_string`), and
a map that names its allocator is `hashbrown::HashMap<K, V, blaze_hash::FixedState, &Frame>`. Exempt a line with
`// alloc: <why>`, an item with `#[expect(blaze::implicit_allocator, reason = "..")]`.

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
  `format_args!` is inside a closure handed to `blaze_query::db::attach` or
  `blaze_query::db::with_attached_db`. A value needs it when its type is an ADT of the crate `hir_ty` (its
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

## `thread_local_in_waiting_fn`

Warn (`blaze::internal`). AGENTS.md rule 24: LLVM treats a thread-local's address as constant
inside one function, so a function that reads a thread-local and also waits takes the address
once and keeps using it after the fiber resumed on another worker (the 2026-11-02 "RefCell
already borrowed" in `blaze_graph`'s pool: `CACHES.with` and `LOCAL.get()` inlined into
`blaze_codegen::driver::Build::walk` across `sched::host::wait`). A read is a call of
`LocalKey::{with, try_with, with_borrow, with_borrow_mut, get, set, take, replace}`, resolved by
inference. A body may park when the may-suspend check (`hir_ty_suspend::suspend_points`) has a
point in it. In such a body the lint finds:

- each read in the body itself, closures included, `#[inline(never)]` or not (its own wait
  splits it the same way);
- each call of a function that reads a thread-local in its own body and can be inlined there:
  not `#[inline(never)]`, and of the same crate, `#[inline]`, or generic. std's, core's and
  alloc's functions are not followed.

The finding names the thread-local (its `static`'s or `const`'s path, or the field holding it),
the function, the reader for a call, and the path to the fiber's switch. The fix is the pattern
of `pool::with_caches`, `pool::local` and `sched::host::job_begins`: a small `#[inline(never)]`
reader that does not wait. Not seen: a read two calls down, a trait method called through a type
parameter, and inlining across crates by LTO.

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

## `unrecorded_span`

Deny. AGENTS.md rule slow-is-p0-traced: a span the flight recorder drops is the same as no
span. The recorder (`blaze_profile::install`) is a blaze process's only subscriber and keeps a
span at INFO or above whose target starts with `blaze` or whose name ends in `_query`
(`blaze_profile::keeps`, which the lint calls). A span's target is its module's path unless it
names one, so a span in a `blaze*` crate is kept as written and one in a rust-analyzer crate
(`hir_def`, `syntax`, ..) is not. A finding is a span of the crate `tracing` (`info_span!`,
`warn_span!`, `error_span!`, `span!` at a written level, found by resolving the macro) or a
`#[instrument]` (by its path, `level` INFO when unwritten) at a kept level whose target, written
or its crate's module path, and name the recorder drops. The fix names `target: "blaze"`
(`target = "blaze"` in `#[instrument]`), machine-applicable; a written target that is not
`blaze*` is replaced, maybe-incorrect, since someone chose it. A span below INFO, one whose
target, level or name is not a literal, and one a macro writes are not findings.

`tracing` events are findings under the same rule. The recorder keeps an event at INFO or above
by `blaze_profile::keeps` too, as one row of the `events` table, and drops the rest. A finding
is an `error!`, `warn!` or `info!` (or `event!` at a written level) whose `target:`, written or
its crate's module path, the recorder drops. An event's name is `event <file>:<line>` unless it
writes `name:`, so a query's name keeps it only when written. The fix is the span's:
`target: "blaze", ` after the open bracket, machine-applicable when no target is written. The
lint's name stays `unrecorded_span`; its message says "drops this event".

## `fixed_key_lookup`

Warn, with a shrink-only baseline. AGENTS.md rule 10: one choice per need, and for a lookup
whose keys are all known at compile time that choice is a perfect hash built by `const fn`,
`blaze_phf::phf_table!(static TABLE = KEYS;)`: no table filled on first use, no hasher, no run
of compares, and a key named twice fails to compile. `TABLE.get(key)` (or `TABLE.index(bytes)`
for a `&[u8]`) is the key's index into a `const` array of the values. A finding is:

- a hash map or set (std's with any hasher, hashbrown's, `blaze_hash`'s) keyed by `&str` or
  `&[u8]`, built only from string literals (UTF-8 byte strings) and `const`s or `static`s, and
  never changed after. Built: `HashMap::from([(lit, ..), ..])`, `from_iter` or `collect` of an
  array literal or a `const` through `iter`, `into_iter`, `copied` and `cloned`, or `new()`
  (`default`, `with_capacity`, `with_hasher`, ..) bound to a local and followed at once by
  `insert(lit, ..)` statements. Never changed after: the value goes into a `static`'s
  `LazyLock` (`LazyCell`, once_cell's `Lazy`), into `get_or_init` of a `static`'s `OnceLock` or
  `OnceCell`, or into a local whose every other use is a `&self` method, a shared borrow or a
  move into one of these. A `&mut` use, a `&mut self` or `self` method, a return or a store into
  a field may change it, and is not a finding. papaya's and dashmap's maps insert through
  `&self`, so they are not checked;
- a `match` on a `&str` (or a `&[u8]` whose byte-string patterns are all UTF-8) with more than
  16 string-literal patterns in arms with no guard, or an `if`/`else if` chain of more than 16
  `place == "lit"` compares of one place (a local or static, a field of one, or a method of one
  with no arguments). rustc runs such a `match` as one compare an arm, in order; past about 16
  keys the one hash and one compare of a perfect hash cost less. Not in a `const fn`, a
  `const`, a `static`'s initializer or a `const` block (compile time), nor in code a macro writes.

The message names the item (the static, or the local and its function, or the function of
the `match`), the key count, and the rule. There is no fix: the keys move to a `const` slice and
the values to a `const` array in its order. Keep one with
`#[expect(blaze::fixed_key_lookup, reason = "..")]` (the order of a `match`'s arms that is a
priority, say). Misses: a map built through `map` or another adapter, a set of keys that are
not text (a perfect hash of `blaze_phf` is keyed by `&str`), and a map whose `let` binds a
pattern.

## `struct_update`

Warn, with a shrink-only baseline. For code written by agents: a new field must force a
decision at every literal. A finding is a struct literal with `..base`, `..Default::default()`,
`..Self::default()` (or `..` alone, default field values) for a struct of a workspace member
that leaves at least one field to the base. The struct's crate decides, by its origin: another
crate's struct may be `#[non_exhaustive]` or change under a version bump and is not checked. A
literal a macro or a derive wrote is not checked, nor one that writes every field (clippy's
`needless_update`).

The message names the struct, the fields taken (up to eight) and the base. The fix writes each
field taken as `field: base.field`, machine-applicable when the base is a place with no side
effect (a local, `self`, or a field or parenthesis of one) and the literal is not in a macro
call: `..base` moves or copies each field it takes out of the place one at a time, as
`field: base.field` does, so the rewrite is sound whether or not the fields are `Copy` and
whether or not `base` is used after. Any other base (a call, `Default::default()`, a `const`, an
index, a deref) is evaluated once, so the help names the fields to write and the author picks
each value. Keep one with `#[expect(blaze::struct_update, reason = "..")]`.

## `bool_param`

Warn, with a shrink-only baseline. A `bool` parameter (the type resolved, so an alias counts;
`&bool` and `Option<bool>` do not) of a function or method of the workspace, public,
crate-visible or private. A caller's `true` says nothing of what it chooses, two of them swap
without a type error, and a third answer has nowhere to go; a two-variant enum named for the
choice says it at each call, and a variant added later makes every `match` decide. The message
names the function and the parameter; its note names the enum from the parameter (`is_hot`
becomes `Hot`, `keep_going` `KeepGoing`), its variants left to the author. Not checked: a
trait impl's method (its signature is the trait's, found once at the trait's declaration when
the trait is the workspace's), a function with an ABI (`extern "C" fn`), a closure, a setter
`set_*` whose one parameter is the `bool`, test code, and a signature a macro wrote. There is
no fix. The message does not count call sites: no reference search runs inside a body's lint
query. Keep one with `#[expect(blaze::bool_param, reason = "..")]`.

## `sans_io_core`

Warn, with a shrink-only baseline. AGENTS.md rule 12: a core is a synchronous state machine
that reads time only from an injected clock (`advance_clock(now)`), owns no socket, file,
thread, timer or sleep, and hands back decisions and deadlines. A core is a module or a crate
marked with the inner attribute `#![blaze::sans_io]`, a blaze extension (`AttrFlags::SANS_IO`);
under rustc it is inert once the crate root has `#![feature(register_tool,
custom_inner_attributes)]` and `#![register_tool(blaze)]`. Its child modules are part of it;
test code is not checked.

A finding is a call in a core function whose instance reaches an ambient root through any
chain: `hot_alloc`'s walk (`reaches_root`) with the ambient root set, through the workspace's
own functions too. The roots: `Instant::now`, `SystemTime::now`, `std::thread`, `std::fs`,
`std::net`, `std::process`, `std::env`, `std::io::{stdin, stdout, stderr}`, `RandomState::new`,
`getrandom`, `rand::thread_rng` and `OsRng`, tokio and mio, channel timeouts
(`crossbeam_channel`'s `recv_timeout`, `after`, `tick` and the rest, std's `mpsc`
`recv_timeout`), and libc (`rustix`, `nix`). The walk ends, not reaching, at std's locks and
panic machinery (`std::sync`, `std::panicking`, `std::rt`, `std::alloc`), `parking_lot`,
`lock_api`, `tracing`, `log` and `blaze_mem`. A call through a trait object or a type parameter
the core takes resolves to no body and reaches nothing: that is how a driver injects IO. A
callee that is itself in a core is found in its own body, not at each call.

The message names the core function, the chain (`core fn` → ... → root, at most 24 steps), the
effect, the core, and the rule. No fix. Keep one with
`#[expect(blaze::sans_io_core, reason = "..")]`. Misses: an effect behind a `LazyLock` or
`OnceLock` initializer (the walk stops at `std::sync`), and FFI other than libc's.

## `key_ignores_field`

Warn, with a shrink-only baseline. AGENTS.md rule 18: a key covers every input its value
depends on. Checked: the body of a hand-written `impl Hash`, `impl PartialEq` or `impl Ord`
(a `derive` covers every field and is not checked, nor an `#[automatically_derived]` impl), and
a method that builds a key from `self`: one that calls `ContentId::of*`, a method of a type
named `Hasher` or of a `Hash`/`Hasher` trait, or a `*Key`'s constructor or struct literal, and
that returns a `ContentId` or `*Key`, takes a `Hasher`, or is named `key`, `id`, `content_id`,
`hash_content`, `hash_into`, `fingerprint`, `*_key` or `key_*`. A finding is:

- a struct pattern of the type with `..`, naming the fields it leaves out;
- a body that reads some fields of the type (`self.a`, `other.a`) but not all, when it hands no
  value of the type on whole (to a call or an operator, which may read the rest);
- `field: _` (or `_` in a tuple struct's pattern) with no `// not in key: <why>` on its line or
  the line above.

The fix is a destructure that names every field: `let Self { a, b, cache: _ } = self;`, with
`// not in key: <why>` on `cache`. The message names the impl or builder, the type, the fields
left out, and the rule. No machine fix. Keep one with
`#[expect(blaze::key_ignores_field, reason = "..")]`.

## `wildcard_arm`

Warn, with a shrink-only baseline. For code written by agents: a variant added to an enum must
be a compile error at every `match` on it, so each match decides what the variant does. A
finding is a catch-all in a `match` arm (`_`, or a binding with no subpattern, `other =>`) at a
place whose type, through references, is an enum of a workspace member that is not
`#[non_exhaustive]`. A catch-all nested in a pattern counts the same (`Some(_)` over an
`Option<Kind>`, `(Kind::A, _)`), and so does the one other arm of a two-arm match (clippy's
`match_wildcard_for_single_variants`). Not checked: another crate's enum, a catch-all that
stands for no variant (the arms above list them all; `unreachable_patterns` says so), a
`match` a macro wrote or a desugared one (`for`, `?`), and a catch-all a macro wrote.

Which variants a catch-all stands for is match checking's to say: `hir_ty_matchck::catch_alls`
lowers the match's arms, analyzes the arms above the catch-all's arm with
`rustc_pattern_analysis` (a guarded arm covers nothing), and takes the variants the witnesses
its arm can match have at the catch-all's place, a `_` witness standing for every inhabited
variant. Where it can't line two patterns up (integer ranges, slices) it takes the pattern to
match, so the list may name a variant already covered, never leave out one that isn't.

The message names the enum and the variants (up to eight, then "and N more"); the note says
why. The fix writes the variants in place of a `_`, joined by `|` (parenthesized under `&`,
`box` or `x @`), each with `(..)` or `{ .. }` as its fields need, by the path the match's other
arms name the enum's variants with (`Kind::`, `Self::`), or the enum's path from the body's
module when no arm does (or one names a variant bare). It is machine-applicable for a `_` whose
arm has no guard: the variants listed are every value that reaches it, so the match stays
exhaustive and each value takes the arm it took before. A binding gets help instead
(`other @ (Kind::B | Kind::C)`), and so does a guarded arm. Keep one with
`#[expect(blaze::wildcard_arm, reason = "..")]`.

## `false_sharing`

Warn, with a shrink-only baseline. A struct threads share whose fields written apart can sit
in one cache line: two atomics (std's `Atomic*`, `crossbeam_utils::atomic::AtomicCell`), or a
lock (std's `Mutex` or `RwLock`, `lock_api`'s, which `parking_lot`'s are) and an atomic. A
thread writing one takes the line from the core that wrote the other.

- **One line** is from the front end's layout: a line is 128 bytes (`cache-line-size` in
  `clippy.toml`), the pair of 64-byte lines arm64 and x86-64 prefetch together. Two fields can share one when the last byte of the first is less than 128 bytes
  before the second, or, the struct itself aligned to 128, when they meet in one line. A field
  whose own alignment is 128 or more (a `#[repr(align(128))]` wrapper,
  `crossbeam_utils::CachePadded`) is padded and never counts.
- **Written apart**: a method of the struct's impls writes a field when it calls a store,
  swap or read-modify-write on `self.field`, or takes its lock. Two fields are written apart
  when different methods write them. Writes from outside the impls are not seen.
- **Shared**, conservatively: the struct is held, through fields, arrays, tuples and type
  arguments, in a static (not a `thread_local!`'s) or an `Arc` in a field, a static or a
  function signature of a workspace crate that can name it.

The message names the two fields, their offsets and the methods; the note says where the
struct is held and how to pad. Keep one (a read-mostly field, one writer thread) with
`#[expect(blaze::false_sharing, reason = "read-mostly")]` on the struct.

## `seqcst_unexplained`

Warn, with a shrink-only baseline. An expression naming `core::sync::atomic::Ordering::SeqCst`
(std's re-export is the same item), resolved, so `use Ordering::SeqCst as Total` and
`use Ordering::*` are found, with no `// seqcst: <why>` on its line or the line above. A
pattern `Ordering::SeqCst =>` names it and is not found. There is no fix: only a person knows
whether some thread reasons about the one total order. The note names what the operation
takes instead, by the method the ordering is an argument of: `load` `Acquire`, `store`
`Release`, `swap` and `fetch_*` `AcqRel`, `compare_exchange` `AcqRel` for success and
`Acquire` for failure, `fetch_update` `Acquire` for its fetch ordering, `fence` `AcqRel`.

## `large_move`

In `blaze::hot`, so only in crates and items that deny it. A move by value of a type whose
layout is over 512 bytes (`LIMIT` in `large_move.rs`: eight 64-byte lines, past the copies
LLVM writes as a few vector moves), each a `memcpy` of all of it:

- a parameter taken by value (`self` included) and a return type, at the signature;
- a place (a local, a field, an index, a deref) moved into a call's argument or a struct
  literal's field. A value made in place (a literal, a call's result) is built in its slot and
  is not found;
- an enum whose largest variant is over 512 bytes and over 4× its second.

Sizes are the front end's layouts, so a reference, `Box`, `Vec` or `Arc` is small; a type whose
layout depends on a parameter it was not given is not checked, nor a trait impl's method (its
signature is the trait's). The message names the type and its size: pass by reference or box
the large variant.

## `collect_then_consume`

Warn, with a shrink-only baseline. An `Iterator::collect` into a `Vec`, `VecDeque`, `String` or a
hash or B-tree collection whose one use the iterator could answer itself: one `for` loop or
`.iter()`/`.into_iter()` chain over it, `len`, `is_empty`, `first` or `contains`, or being passed
where the parameter is a type parameter bounded by `IntoIterator`. The use is the `collect`'s
own receiver, or the one use of a plain `let v = ..` (not `mut`, not `ref`) outside any loop or
closure the `let` isn't in. What the use asks must be the same of the iterator: a set or map is
found only for `is_empty` (and a set for `contains`), a `String` only for `is_empty` of `char`s.
A delayed use is kept when a local the chain reads is named after the `collect` (collecting
first ends a borrow or takes items before they change). The fix, machine-applicable, rewrites
`.collect::<Vec<_>>().len()` to `.count()` and `.is_empty()` to `.next().is_none()` when the chain
is std's roots and `Iterator`'s adaptors (and, for `.next().is_none()`, closures that call and
assign nothing); the rest is help: drop the `collect`.

## `hash_twice`

Warn. A lookup (`get`, `get_mut`, `contains_key`, `contains`, `get_key_value`) followed by a
write (`insert`, `remove`, `remove_entry`, `entry`) of the same key on the same map or set, with
nothing between that may change either: each call hashes the key and probes the table. The maps
are `facts::hash_adt`'s (std's, hashbrown's, `blaze_hash`'s through their `Deref`, dashmap's) and
papaya's (`HashMap`, `HashSet` and what `pin()` lends), the method resolved on the map's impl. The
map is a place (a local or `static` and its fields, indices and derefs); the keys are the same
literal, `const`, local or place through `&`, `*` and value-keeping conversions (`clone`,
`to_owned`, `to_string`, `into`, `as_str`, `as_ref`, `borrow`). Not found: the two in different
branches of an `if` or arms of a `match`, one in a closure the other is not in, a write after a
`&mut self` call, `&mut` borrow or assignment of the map or the key, calls a macro wrote. The help
names the entry API, or what `insert`/`remove` return; on papaya `get_or_insert_with` and
`compute`, and on a concurrent map that the two calls race. Machine-applicable fix for one shape
only: `if !m.contains_key(&k) { m.insert(k, v); }` becomes `m.entry(k).or_insert(v);`
(`or_insert_with(|| v)` when `v` calls anything), and a set's `if !s.contains(&k) { s.insert(k); }`
becomes `s.insert(k);`, when `k` and `v` neither leave nor name the map and no comment is lost.

## `syscall_in_loop`

Warn, only in crates whose file system work is batched: one whose root denies `blaze::hot` or one
of its lints, `blaze_cas`, `blaze_vfs`, and `blaze_core`'s `journal` module; test code is not
checked. A call in the body of a `for` loop or in a closure given to a method of `Iterator` or
rayon's `ParallelIterator`/`IndexedParallelIterator` (resolved, not by name) that reaches, through
`hot_alloc`'s walk with the root set `Roots::Syscall`, std's `fs::{metadata, symlink_metadata,
read_dir, canonicalize, exists}`, `File::{open, create, create_new, open_buffered, metadata}`,
`OpenOptions::open`, `DirEntry::metadata`, or a stat or open of `libc`, `rustix` or `nix`. The
message names the call and the stat or open it reaches; the help names the batches blaze has:
`blaze_core::Effect::Stamp { files, tag }` (`IORING_OP_STATX`s in flight together on io_uring,
`fstatat` sweeps on the driver's threads elsewhere), `Effect::Entries { dir, tag }` and
`Effect::Journal { dir, since, tag }`.

## `query_in_loop`

Warn. A query (`lock_across_query`'s: made by a `blaze_query_macros` attribute, or a method of a
trait implemented for `base_db::Db`) called per element as `syscall_in_loop` counts it, with an
argument that reads a local holding the element. Found only when a batch query can be named: a
query of the same kind (a macro-made one in the same module, or a method of the same trait), first
by name, taking at least one key of other types than the per-item query, whose return type holds
the per-item return type as its element (a slice or array of it, an `IntoIterator` collection
other than `Option`/`Result` with it among its type arguments, alone or in a tuple, or a `Box`,
`Arc` or `Rc` of one, three wrappers deep). Queries returning `()` or `bool`, and generic ones,
are not matched. The message names both queries and says that every call records a dependency
edge.

## `string_key`

Warn, in crates depending on `intern` (not `intern` itself). A hash map or set keyed by `String`,
`&str`, `Box<str>`, `Arc<str>`, `Rc<str>` or `SmolStr` whose every key written in the body
(`insert`, `entry`, `get_or_insert`, `get_or_insert_with`) is made from a `hir_expand::name::Name`
or an `intern::Symbol`, through conversions (`as_str`, `to_string`, `to_owned`, `to_smolstr`,
`into`, `clone`), `&`, `*`, one-argument `from`/`new`, and up to two `let`s. A map with any other
key holds user data and is not found. Once per map, at its first write. The help suggests
`intern::Symbol` (`Name::symbol()`, `Symbol::intern`, `cmp_order` for ordered output).

## `outline_generic_body`

Warn. A generic function whose type parameters feed only a conversion at entry, while the body
after it is large: each instantiation copies the whole body though only the conversion differs.
Found when each of the function's own type parameters is the type of exactly one value
parameter (`p: P`, `p: &P`, `p: impl AsRef<Path>`; no const parameters), named once in the body,
as the receiver of `as_ref`, `as_mut`, `borrow`, `borrow_mut`, `into`, `to_string`, `to_owned`,
`as_str`, `as_slice`, `as_bytes`, `as_path` or `as_os_str`, of `into_iter()` then `collect()`, as
the argument of a one-argument `from`/`into`/.. call, or as the callee of a call (an `impl Fn`
called once); each conversion is the initializer of one of the body's first statements, which
are those `let`s alone; nothing after them is generic (no expression's type, callee's generic
arguments or the return type names a type parameter, and no identifier of the body is a named
one); and the rest has more than 30 HIR expressions (`REST` in `outline_generic_body.rs`: about
five to eight lines, under which LLVM inlines the body anyway). `async` functions and test code
are not checked.

The fix outlines the rest into a nested non-generic `fn inner(..)` that the shell calls with
the converted values (the "momo" pattern), machine-applicable only for a free function (no
`self`, not in an impl or trait), not `const`, `unsafe` or `extern`, whose return type is
written without a reference, lifetime, `impl` or `Self`, whose conversions read no other local,
whose parameters and `let`s are plain, distinct names, whose conversion types print as source
without a lifetime, and whose body names nothing `inner`. Everything else gets help. Keep one
with `#[expect(blaze::outline_generic_body, reason = "..")]`.

## `cold_error`

Warn everywhere for the first rule, and in `blaze::hot` for the second:

- **Large error.** A function (not test code, not a trait impl's method) returning `Result<T, E>`
  whose `E` is over 128 bytes by its layout, clippy's `result_large_err` default: every `Ok`
  is that big too. blaze-lints lists `result_large_err` but does not implement it; this is its
  port. The note says to box the error or its large variant.
- **Error built inline.** Where the group `blaze::hot` is on (`hot_clone` and `hot_dyn` both
  on, which nothing in the workspace turns on alone: the hot crates deny the group on their
  command line), in a function that is neither `#[cold]` nor `#[inline(never)]`: the argument
  of `Err(..)`, `.ok_or(..)` or `.context(..)`, or the closure of `.map_err(..)`,
  `.ok_or_else(..)` or `.with_context(..)` on a `Result` or `Option`, that builds a `String` (a
  call returning std's or `blaze_mem`'s `String` whose callee is not `#[cold]`, `format!`
  included) or a struct literal over 128 bytes. The note says to build the error in a `#[cold]
  #[inline(never)] fn` that returns it, so the hot path keeps one call.

No fix. Keep one with `#[expect(blaze::cold_error, reason = "..")]`.

## `frozen_collection`

Warn. A field of type `Vec<T, A>`, `String` or `blaze_mem::text::String<A>`, with any
visibility but `pub` (so every place that can reach it is in the crate), that no body of the
crate changes: no `&mut` or `&raw mut` of it, no adjustment that borrows it mutably or goes
through `DerefMut` (`push`, `iter_mut`), no assignment or compound assignment to it or a place
inside it, no `ref mut` binding of it in a struct pattern (or of anything in a tuple-struct
pattern of the struct), and no `match`/`if let`/`let` over it in a body that binds by `ref mut`.
Every body is read once per crate (the query `mutated_fields`): functions, consts, statics,
trait and impl items (derive output included) and items nested in blocks. A whole-struct
overwrite (`*self = ..`) and a move out are not changes. A `#[repr(..)]` struct is not checked.
The note suggests `Box<[T], A>` / `Box<str, A>`: two words, not three, frozen by its type. No
fix: the constructors change with it. Code the build's configuration leaves out (`#[cfg(test)]`
in a non-test build) is not seen.

## `intern_candidate`

Warn, general. A non-generic, non-`Copy` type of the crate that implements `Clone`, `PartialEq`
(by its lang item) and `core::hash::Hash`, recursive (a field holds the type behind `Box`,
`Vec`, `Rc`, `Arc` or another std collection, through `Option`, tuples, arrays and struct fields,
four types deep) or deep (owned heap types nested two or more levels), that the crate's bodies
use as a key (an expression whose type is a std, hashbrown, indexmap, papaya, dashmap or
`blaze_hash` map or set, or a `BTreeMap`/`BTreeSet`, keyed by it) and compare (`==`, `!=`,
`.eq`, `.ne`) or clone (`.clone()`, `Clone::clone(..)`) in at least `intern-candidate-uses`
places (default 8). Every body is read once per crate (the query `intern_uses`); the type's own
impls are not counted. No cross-crate counting, no fix. The help names `internment`, `lasso`,
`hashconsing`, `id-arena` and salsa's `#[salsa::interned]`.

## `small_map`

Warn, general. A map or set of std, hashbrown, indexmap or `blaze_hash` (not papaya or dashmap)
that provably holds at most `small-map-max-entries` (default 8) entries with constant keys
(literals, negated literals, `const`s, unit variants, maybe borrowed). A local: built by
`new`/`default`/`with_capacity(literal <= max)` or from a literal array (`from`, `from_iter`,
`[..].into_iter().collect()`), every use of the local a read method (`get`, `contains_key`,
`contains`, `len`, `is_empty`, `iter`, `keys`, `values`, `clone`, ...) or a constant-key
`insert` with no loop or closure between it and the `let`; any other use (an argument, a return,
`&m`, `entry`, `extend`) leaves it alone. A private field: the query `small_fields` reads every
body of the crate once; each struct literal builds the field as above, each use is a read, a
comparison, a constant-key insert, or a shared borrow handed to a `core`/`std` function named
`debug*`, `fmt` or `hash` (derive output); a binding pattern, move or assignment blocks it; at
most the limit of distinct keys across the crate. No fix. `fixed_key_lookup` stays the
blaze-internal rule for keys looked up through `blaze_phf`.

## `arc_clone_only_borrowed`

Warn, general. `x.clone()` / `Arc::clone(&x)` of std's `Arc` (its diagnostic item), or an
arc-swap `load_full()` (a method of an impl in the crate `arc_swap`), whose result is only
borrowed: inline as a receiver, field or index base adjusted through `Deref` or a shared
autoref, `&`, `*`, a comparison; or bound by a plain `let` whose every use is one of those and
none inside a closure. The source is a place (path, field, deref) whose root is a shared
reference, a non-`mut` `static`, or a non-`mut` local whose other uses are shared borrows.
Machine-applicable fix only for `let y = x.clone();` with no type written and every use of `y`
reading through `Deref` (a method or field of the value, `&*y`): `&` plus one `*` per reference
of `x`'s type plus one. Inside a loop the note names epoch or hazard reclamation (arc-swap
guards, crossbeam-epoch). No MIR: the HIR adjustments say what each use borrows.

## The correctness lints of `blaze::general`

Warn, general; shared helpers in `src/blaze/general_facts.rs` (std items found by crate and
first module, never by `std_def`'s table; the callee of a call with its `Self` type; the async
context of an expression; the panic sites of a body). Tests in
`src/blaze/correctness_general_tests.rs`.

- `blocking_in_async`: a resolved call of a blocking std or crossbeam item (a table by type and
  method prefix, plus every free fn of `std::fs` and `thread::sleep`) whose nearest enclosing
  closure is an async coroutine (fn, block or closure); a plain closure ends the walk. Narrowed:
  std `Mutex`/`RwLock` and crossbeam `Sender::send` are not counted; no interprocedural walk.
- `relaxed_publish`: per crate, the query `atomic_sites` maps each `AtomicBool`/`AtomicPtr`
  field or static to the bodies that `store`/`swap` it with `Relaxed` and those that load it to
  guard data (an `if` condition whose then-branch reads a field, deref or index; a pointer
  dereferenced directly or through a `let`). Both sides reported. `// relaxed: <why>` on the
  declaration or the line exempts it. Narrowed: integer atomics, `compare_exchange`/`fetch_*`
  publishes, guards through a callee.
- `unwind_across_ffi` and `panic_in_drop`: the panic sites of the body alone (`unwrap` family of
  `Option`/`Result`, std's panic machinery, index, `/` and `%` without a nonzero literal
  divisor). Skipped when the body calls `panic::catch_unwind` (resp. `thread::panicking`).
  Narrowed: no overflow of `+ - *`, no interprocedural walk.
- `unbounded_from_input`: a loop that reads input (`for` over `io::Lines`/`Bytes`/`Split`/
  `net::Incoming`, or a std `io`/`net` `read*`/`recv*`/`accept` call in its body) and grows a
  `Vec`, `VecDeque`, std/hashbrown hash collection or an unbounded sender living outside the
  loop, with no size-reading or limiting call on the same place in the loop.
- `dos_hasher_on_input`: only in a non-proc-macro library crate, in a `pub fn`; the map's
  resolved hasher argument is fixed-seed (`BuildHasherDefault` of non-SipHash, FxHash,
  foldhash `FixedState`, FNV, `blaze_hash`), and the key's root local is tainted by a text
  parameter (four rounds through `let` and `for`). It is in `blaze::YIELDS_TO_INTERNAL`: where
  `blaze::internal` is on and nothing names the lint, its default is `allow` (`Levels::of`'s
  `default_spec` in `level.rs`), so blaze's own workspace, which keys maps by fixed seeds on
  purpose, doesn't see it. Narrowed: ahash's `RandomState` is not counted.

## The speed lints of `blaze::general`

Warn, general. Shared helper `src/blaze/loops.rs`: the loop an expression runs once per
iteration of (a `loop`/`while`/`for` body, or an iterator closure by `per_item`'s rule; a `for`
loop's iterable is outside it). Tests in `src/blaze/perf_general_tests.rs`, over an `std` of
their own.

- `bounds_check_in_loop`: a `for` over a core `Range` with a plain binding pattern, whose body
  indexes a place (path or field chain) of slice or `Vec` type by the variable (or the variable
  plus a literal or a local). Text checks only suppress: the end written as `{place}.len()`, a
  reslice `let {place} = &..[..` or a statement holding `{place}.len()` and the bound before
  the loop (not a `debug_assert`). Narrowed: arrays skipped; a proof outside the text before
  the loop is not seen.
- `unbuffered_io`: writes (`write`, `write_all`, `write_fmt`) on std `File`, `TcpStream`,
  `UnixStream`, `ChildStdin`, `Stdout`, and reads (`read`, `read_exact`) on `File`, the sockets,
  `ChildStdout`, `ChildStderr` into an array of at most `unbuffered-io-small-read` (64) bytes,
  grouped by place (local, field, direction). Fires at `unbuffered-io-writes` (2) calls or one in
  a loop. Narrowed: a bare `write` in a loop (a partial-write retry) and `Stdout` in a loop
  (`stdout_lock_in_loop`'s) are skipped; test code skipped.
- `stdout_lock_in_loop`: std's `io::_print`/`_eprint` (what `println!` and the rest expand to)
  or a write on std `Stdout`/`Stderr`, in a loop; one finding per loop.
- `front_vec_ops`: `Vec`'s own `remove(0)`, `insert(0, _)`, `drain(..1)`/`drain(0..1)` in a
  loop; "used as a queue" when the same local is pushed to in the loop.
- `missing_size_hint`: `impl Iterator` for a struct of the crate (not a macro's, not a test's):
  no `size_hint` while `next` does `self.f += / -=` on an integer field and the struct holds a
  slice, array, `Vec`, boxed slice or a second integer field; or a `size_hint` whose tail is
  `(n, Some(n))` (same local or field) with no `ExactSizeIterator` impl.
- `fmt_allocates`: in `fmt` of an impl of a `core::fmt` trait, a call named `to_string`,
  `format`, `from`, `collect`, `to_owned`, `join`, ... of type `String` whose value only
  reaches a `Formatter`'s `write_str`/`write_fmt`/`pad`/`write_char` argument, through block
  tails and plain `let`s (every use). Narrowed: not through `match`, `if`, loops or closures.
- `jagged_vec`: a non-`pub` field `Vec<Vec<T>>`, `Vec<String>`, `Vec<Box<[T]>>`,
  `Vec<Box<str>>` not in `frozen_collection::mutated_fields`; `#[repr]` structs, macros and
  tests skipped. `frozen_collection` leaves such a field to it when it is on. Narrowed: fields
  only, not constructor locals.
- `stable_sort_needless`: std's `sort`, `sort_by`, `sort_by_key` on a slice, array or `Vec`
  whose element is indistinguishable when equal (ints, `bool`, `char`, `str`, `&str`, `String`,
  tuples and arrays of them, ADTs deriving all four of `PartialEq`/`Eq`/`PartialOrd`/`Ord` over
  such fields, non-generic enums; depth 4): `sort()` only when the element is not recursively
  primitive (clippy's `stable_sort_primitive`), `sort_by_key(|x| *x | x | x.clone())` and
  `sort_by(|a, b| a.cmp(b) | b.cmp(a))` always. Narrowed: sort-then-dedup is dropped as
  unsound, since stability decides which equal element survives; no fix yet.
- `dyn_in_hot_field`: a field `Box<dyn Trait>` (std's `Box`) or `&dyn Trait` of a non-`pub`
  struct implementing no workspace trait, called (`(self.f)(..)`, `self.f.m(..)`) in a loop of
  a method of the struct. Skipped when the crate holds the struct as an element of a slice,
  array or non-wrapper ADT's type argument in any field or signature (a module walk, capped
  at 4096 modules, where a cap hit stays quiet). The note says a type parameter trades code
  size. Narrowed: collections that are locals are not seen.

## Cost

The per-node part looks at path types only, and resolves one only when its last segment is
`HashMap` or `HashSet`; `implicit_allocator` resolves each outermost written type. The body part walks the body's HIR once, after the body's inference,
and allocates only to report. Whether an instance reaches the allocator is a front-end query
(`reaches_root`, keyed by its root set: the allocator's, or `sans_io_core`'s ambient effects), shared by every body that calls it and kept across edits that do not change
the functions it reaches.
