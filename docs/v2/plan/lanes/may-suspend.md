---
status: plan
summary: lane may-suspend (2026-10-31): blaze rejects a value held across a call that may park a fiber when that value must not cross threads (`!Send`, or `#[must_not_suspend]`); may-suspend is inferred from the monomorphized call graph from corosensei's `Yielder::suspend`, written only as `#[blaze::may_suspend]` on a trait method or fn-pointer field. Crate `hir-ty-suspend`; replaces sched phase 2's hand audit.
date: 2026-10-31
---
# may-suspend: values held across a fiber switch

Andrew, 2026-10-31 20:41: "try to add `#[may_suspend]` as a compiler thing in blaze". A job on
a fiber (`docs/v2/plan/lanes/sched.md`, phase 2) can resume on another thread after any call
that waits. What its stack holds across that call then crosses threads, and the type system
cannot see it: `fiber.rs` says so in the `SAFETY` comment of `unsafe impl Send for Fiber`. This
lane makes blaze see it, so that once blaze builds itself, blaze's own fiber code is checked by
blaze. It replaces the hand "thread-local and lock audit" (sched.md, phase 2, step 4) for
everything but the carried thread-locals, which stay sched's.

Lead 20:45 adds a lint so only `crates/blaze-graph/src/sched` may name `sched::fiber`,
`corosensei` or the fiber driver: [may-suspend/fiber-boundary.md](may-suspend/fiber-boundary.md)
(child `may-suspend-lint`).

## One notion

**may-suspend** is a property of an instance (a function, closure or `async` body with its
generic arguments): running it can park the fiber it runs on. It is inferred, never written in
normal code:

- **The root** is corosensei's `Yielder::suspend` (crate `corosensei`, inherent method `suspend`
  of `Yielder`): the one call that switches a fiber out. Only `blaze_graph::sched::fiber` calls
  it, and the fiber-boundary lint keeps it there, so it is the fiber driver's suspend
  (`Cx::switch`) and nothing else.
- **An instance is may-suspend** when a call in its monomorphized runtime MIR reaches the root:
  directly, through a function it calls, a closure it calls through `Fn*`, drop glue, the
  `Future::poll` of an `async` block, or the impl's method of a trait method it calls with a
  concrete `Self`. The walk dispatches each call as the code generator does
  (`lookup_impl_method`, `is_dyn_method`, `Fn*` on closures and fn items, drop glue), so what it
  sees is what runs.
- **Where the call graph can't be seen** (a call through `dyn Trait` or a function pointer), the
  only written marker says it: `#[blaze::may_suspend]` on the trait method, or on the struct
  field whose type is the function pointer (an attribute cannot go on a type; the field is where
  the pointer's provenance is written). rustc builds the same source:
  `#![feature(register_tool)] #![register_tool(blaze)]` in the crate, which blaze honors already
  (`hir-def`'s `registered_tools`). A call through `dyn` of a marked method, or through a
  pointer loaded from a marked field, is may-suspend. **An unmarked one promises it never
  suspends**, and the promise is checked where the code that could break it is made:
  - an unsizing cast to `dyn Trait` of a concrete type (the vtable is made there): each method
    of the trait and its supertraits that is unmarked, whose impl for that type is may-suspend,
    is an error at the cast, naming the method and its path to the suspend;
  - a reify of a may-suspend function or closure to a function pointer (`ReifyFnPointer`,
    `ClosureFnPointer`) whose value goes anywhere but a marked field (a struct literal's marked
    field, or an assignment to one) is an error at the reify.

  The promise makes a `dyn` call cheap to judge (one attribute bit) and closes the world the
  same way `hir-ty-pure` does: every `dyn` call's target was put in a vtable somewhere, and every
  pointer was reified somewhere, and those places are checked.
- Recursion: the summary of an instance is a query whose cycle starts at "never suspends" and
  rises to a fixpoint (may-suspend only grows), as `hir-ty-pure::walk::summary` does.

The summary keeps only the next hop toward the root (one `Instance`), never a string; the path
(`db.variance` → `fetch` → `get_or_run` → `suspend`) is rebuilt by following hops only when an
error is reported.

## The check

At each call to a may-suspend instance, in every body walked, a local that is held across the
call is an error when its type, monomorphized, is

- not `Send` (asked of the trait solver, `TypingMode::Codegen`, the instance's environment):
  `Rc`, a `&T` with `T: !Sync` (a reference into a thread-local's `Cell` or `RefCell`), std's lock
  guards, raw pointers; or
- an ADT marked `#[must_not_suspend]` (rustc's own attribute, which std puts on `MutexGuard`,
  `MappedMutexGuard`, the `RwLock` guards, and core on `RefCell`'s `Ref` and `RefMut`), the
  value itself or held as rustc's `check_must_not_suspend_ty` looks: through `Box`, tuples,
  arrays, slices and references, never into another ADT's fields (a struct holding a guard is
  caught by `Send` when the guard is `!Send`, as std's are).

**Held across the call** is exactly what rustc's coroutine transform keeps across a yield,
computed by blaze's port of it (`hir_ty::mir::held`: `MaybeBorrowedLocals`,
`MaybeRequiresStorage`, `MaybeLiveLocals` over the runtime body, drops elaborated): live after
the call (used again, dropped later while maybe initialized) or borrowed before it with its
storage still live, minus the call's own destination. A call is a suspend point as a `Yield`
is; nothing else differs. The one change to `held.rs` is that the suspend points are given (the
blocks of the may-suspend calls) and the first parameter is kept (for a coroutine it is the
future itself, for a function an ordinary argument): requested of codegen, below.

**The fiber driver's own frames** (bodies in `blaze_graph::sched::fiber`) are the root's
implementation: their locals across a switch are the fiber's handle (`Cx`, its `&Yielder`) and
its payload, covered by the `SAFETY` comment of `Fiber: Send`. They are not checked; what they
call through `Work::run` is. Everywhere else, the handle a job holds across its waits is `&Cx`,
which must then be `Send`: `Cx` becomes `Sync`, soundly, by `Cx::switch` checking that it runs
on its own fiber (one thread-local read it already makes: `HERE`'s job is `self.job`) and
panicking otherwise. Requested of sched, below.

The error names the value, its type, why it can't cross threads, the call, and the path:

```text
error: `guard` (`MutexGuard<'_, Vec<u32>>`) is held across a call that may suspend this fiber
  --> crates/x/src/lib.rs:12:9
   |
11 |     let guard = TABLE.lock().unwrap();
   |         ----- held from here
12 |     let v = db.variance(def);
   |             ^^^^^^^^^^^^^^^^ this may suspend: `variance` → `fetch` → `get_or_run` → `suspend`
13 |     guard.push(v);
   |     ----- and used after it
   = note: the job may resume on another thread, and `MutexGuard` is `#[must_not_suspend]`
   = help: end its scope before the call (a block around its uses), or take what you need out of it first
```

It is an error, not a lint: a value that crosses threads under a type that says it must not is
undefined behavior, like a data race. It costs nothing for a crate whose graph has no
`corosensei`: the root is looked up once per crate graph and, absent, nothing is walked.

## Where it lives

| Path | What |
|---|---|
| `crates/hir-ty-suspend/` (new, this lane) | a front-end crate (semantics-in-front-end): `root`, the root's `FunctionId`; `dispatch`, a call's targets as codegen dispatches them; `summary`, the may-suspend query per instance; `check`, the held-across check per body and the promise checks; `suspend_check(db, krate) -> Found` (its `Held` and `Broken`), what blaze-diagnostics lowers; for blaze-lints (01497), `suspend_points` (every value held at every may-suspend call of a body, with why it is rejected or `None`), `may_suspend_at` and `on_fiber` |
| `crates/hir-ty/src/mir/held.rs` (codegen) | one added entry: `MirBody::held_across_calls_in(frame, points)`, the same analyses with given suspend points |
| `crates/hir-def/src/attrs.rs` (db) | two flags: `#[blaze::may_suspend]` (functions and fields) and `#[must_not_suspend]` (ADTs, word or `= "reason"`) |
| `crates/blaze-diagnostics/` (diagnostics) | lowering of `hir_ty_suspend::Held` and `Broken` into errors, and the call from a crate's check |
| `crates/blaze-graph/src/sched/fiber.rs` (sched) | `#[blaze::may_suspend]` on `Here::wait` and `Here::relocate`; `register_tool(blaze)` in `blaze-graph`'s `lib.rs`; `Cx: Sync` with the own-fiber check |

The dispatch of a call is written today in `hir-ty-codegen` (four places), `hir-ty-pure::walk` and
`hir-ty-mir`'s inliner. This lane's `dispatch` module is written to be the one copy: queued to
lane dedup to move the others onto it.

## Tests (each a tiny crate, `crates/hir-ty-suspend/src/tests.rs`)

The fixtures declare a crate `corosensei` with `Yielder::suspend` and a small engine
(`get_or_run` calling it, `fetch` calling `get_or_run`, a query method calling `fetch`), and
std's `Mutex`/`MutexGuard` (`#[must_not_suspend]`), `RefCell`, `Rc` and `Cell`.

- `a_guard_held_across_a_query_call_is_rejected_naming_the_path`: the error is at the call, names
  `guard`, and its path is `variance → fetch → get_or_run → suspend`.
- `the_same_guard_dropped_before_the_call_is_accepted`: the guard moved away before the call;
  also a guard in an inner block that ends before it.
- `a_guard_borrowed_then_moved_away_is_held_until_its_scope_ends`: as rustc holds it across an
  `.await` (`MaybeRequiresStorage` keeps a borrowed local's storage after a move), so the way
  to release a guard that was used is to end its scope.
- `a_dyn_method_without_the_marker_whose_impl_suspends_is_rejected`: the error is at the
  `&x as &dyn Trait` cast and names the method.
- `a_marked_dyn_method_suspends_its_callers`: a guard across `d.m()` on a `&dyn Trait` whose `m`
  is `#[blaze::may_suspend]` is rejected; across an unmarked one, accepted.
- `an_rc_or_a_thread_local_borrow_across_a_suspend_is_rejected`: `Rc<u32>` and a `&Cell<u32>`
  (`!Sync`, so the reference is `!Send`); a `Send` value (`Vec<u32>`, `&Mutex<u32>`) is accepted.
- `a_generic_caller_suspends_through_its_closure`: `fn run<F: FnOnce()>(f: F)` holding a guard
  across `f()` is rejected for the instance whose closure queries, accepted for one that doesn't.
- `a_reified_suspending_function_needs_a_marked_field`: into a marked field, accepted; into a
  `let p: fn()`, rejected.
- `recursion_reaches_its_fixpoint`: two functions calling each other, one of them suspending:
  both are may-suspend.
- `a_call_through_a_marked_field_suspends_its_caller`: a guard across `(here.wait)(db)` where
  `wait` is a `#[blaze::may_suspend]` field is rejected.
- `no_corosensei_no_walk`: a crate graph without it finds no root, and the check returns before
  asking for any MIR.

## Not in scope

- Compiling may-suspend functions as state machines: a design note in
  [indexable-inc/blaze#1577](https://github.com/indexable-inc/blaze/issues/1577), not built.
- Thread-local reloads in codegen. The rule today is sched's (`fiber.rs`): a function's
  thread-local reads must not span a wait, so they sit in `#[inline(never)]` functions. LLVM
  marks a thread-local's address with `llvm.threadlocal.address` (LLVM 17+): within one function
  LLVM may still reuse the address across a call, because nothing tells it the thread changed;
  the intrinsic makes the address a value computed at a point, not a constant of the function,
  and it guarantees nothing across a call that switches threads. A later step can mark
  may-suspend calls so codegen recomputes thread-local addresses after them (as Swift's and
  C++ coroutine lowering must), and then drop the `#[inline(never)]` rule.

## Speed

[may-suspend/speed-of-light.md](may-suspend/speed-of-light.md).
