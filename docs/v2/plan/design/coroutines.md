---
status: current
summary: The coroutine transform - what turns an `async fn` or `async` block into a state and one `poll` function built as ordinary code. Which locals are saved (crates/blaze-flow/src/suspend.rs), the split into `poll` and `drop` and the state's layout (crates/blaze-lir/src/coroutine.rs), the builder's three functions per instance (crates/blaze-build/src/coroutine.rs), the checker's capture rows and the run path's facts; drop on cancel as a per-point cancel path; the decisions where the Reference is silent; the simulation proof; what is refused by name.
---
# Coroutines: the state, `resume`, and drop on cancel

A coroutine is the sibling of a closure (codegen.md, "Closures and the Fn traits"): a closure is an
environment and a body function taking it; a coroutine is an environment that also holds a tag and
the locals that outlive a suspension, and a body function that starts with a `match` on the tag.
It has two homes (one concept, one home each): `crates/blaze-flow/src/suspend.rs`, which locals a body saves, per
body on the flow form, and `crates/blaze-lir/src/coroutine.rs`, the split of one LIR function into
`poll` and `drop` and the state's layout (decision D73). Neither knows the checker (rule 79).

## Sources, quoted

- Reference, Await expressions: "Await expressions are legal only within an async context". The
  steps: "1. Create a future by calling IntoFuture::into_future on the future operand. 2. Evaluate
  the future to a future tmp; 3. Pin tmp using Pin::new_unchecked; 4. This pinned future is then
  polled by calling the Future::poll method and passing it the current task context; 5. If the call
  to poll returns Poll::Pending, then the future returns Poll::Pending, suspending its state so
  that, when the surrounding async context is re-polled, execution returns to step 3; 6. Otherwise
  the call to poll must have returned Poll::Ready, in which case the value contained in the
  Poll::Ready variant is used as the result of the await expression itself." Its approximate
  desugaring: `match operand.into_future() { mut pinned => loop { let mut pin = unsafe {
  Pin::new_unchecked(&mut pinned) }; [the poll of `pin` with the current context, matched:]
  Poll::Ready(r) => break r, Poll::Pending => yield Poll::Pending, } } }`, "where the yield
  pseudo-code returns Poll::Pending and, when re-invoked, resumes execution from that point."
- Reference, Async blocks: "An async block is a variant of a block expression which evaluates to a
  future." "The future type that rustc generates is roughly equivalent to an enum with one variant
  per await point, where each variant stores the data needed to resume from its corresponding
  point." "Async blocks capture variables from their environment using the same capture modes as
  closures." "async move { .. } blocks however will move all referenced variables into the
  resulting future." "return <expr> from within an async block will return the result of <expr> as
  the output of the future."
- Reference, Async functions: "An async function is roughly equivalent to a function that returns
  impl Future and with an async move block as its body". "The async move block in the body captures
  all function parameters, including those that are unused or bound to a _ pattern. This ensures
  that function parameters are dropped in the same order as they would be if the function were not
  async, except that the drop occurs when the returned future has been fully awaited."
- Reference, Destructors: "When control flow leaves a drop scope all variables associated to that
  scope are dropped in reverse order of declaration (for variables) or creation (for temporaries)."
  "The fields of a struct are dropped in declaration order." "The variables that a closure captures
  by move are dropped in an unspecified order."
- The pinned library (nightly-2026-10-27, library/core/src): future/future.rs `fn poll(self:
  Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;` future/into_future.rs `pub trait
  IntoFuture { type Output; type IntoFuture: Future<Output = Self::Output>; fn into_future(self) ->
  Self::IntoFuture; }` task/poll.rs `pub enum Poll<T> { Ready(T), Pending }` pin.rs `pub const
  unsafe fn new_unchecked(pointer: Ptr) -> Pin<Ptr>` panicking.rs `panic_const_async_fn_resumed =
  "`async fn` resumed after completion"`.

## The design

1. **A value and one function.** The value is the state. `poll(state: Pin<&mut State>, cx: &mut
   Context<'_>) -> Poll<Output>` is built by the ordinary builder. An `async fn` only makes the
   state (its arguments become the upvars, the tag `Unresumed`) and returns it; nothing runs.
2. **The suspension in the flow form** (crates/blaze-flow, `StmtKind::Suspend { resume, cancel }`):
   where to go on when polled again, and where to go when the state is dropped while parked there.
   An async block's body is its own flow (crates/blaze-flow-lower/src/coroutine.rs `lower_async`):
   its upvars are its arguments, `.await` is the Reference's loop, and each cancel path is the
   drops a `return` there makes. So async bodies are move- and borrow-checked like any body.
3. **Which locals are saved** (crates/blaze-flow/src/suspend.rs `saved`). A local is saved when, at
   some `Suspend`, it is maybe storage-live and either live by use on its resume or cancel path, or
   maybe borrowed since its `StorageLive`. The second rule is what makes the state
   self-referential: a saved reference points at a saved local, so the state must not move once
   polled. That is the promise `Pin<&mut State>` carries, and why the checker never makes the
   future `Unpin`. Every other local stays an ordinary local of `poll`, new at each poll. `cx` is
   one of those: it is "the current task context" of each poll and is never saved. The saved
   locals' types are the exact interior (blaze-flow-lower coroutine.rs `interior`), known per body
   before any instance.
4. **The split** (crates/blaze-lir/src/coroutine.rs `split`, decision D73): the builder builds the
   body as an ordinary LIR function, each await ending a block in `suspend point, resume, cancel`,
   and one pass splits it into `poll` and `drop(state)` with a `StateLayout`. The section "The LIR
   pass" below is its full statement.
5. **The state** holds the upvars first, in order (so a state before its first poll is a closure
   environment plus a tag), then the kept slots and values by falling alignment, then the tag:
   `UNRESUMED = 0`, `RETURNED = 1`, `PANICKED = 2`, point `k` is `FIRST_POINT + k` (3 + k), in the
   narrowest of `i8`, `i16`, `i32`.
6. **Drop on cancel** is the second output from the same blocks: `drop` switches on the tag into
   point `k`'s cancel path; at `Unresumed` the block that drops the upvars; at `Returned` nothing.

## Decisions (where the Reference is silent)

- D1. `.await` is desugared once, AFTER the check, in the adapter. The lowering keeps `Expr::Await`
  as one node (blaze-lower desugar.rs: "its poll loop is a state machine's, made where code is")
  and the checker already types it by rule (rules/async_fmt.rs `expr_await`: the `IntoFuture`
  goal, the callee row for `into_future`, the held future in `awaited`). A desugar in the lowering
  would need a `yield` and a context the surface language cannot name, and would spread one
  diagnostic site over six nodes. The adapter adds one callee the checker does not record:
  `Future::poll` at the held type.
- D2. Cancel is a PATH per point, not a table of locals per point (a change to the brief's frame).
  "The saved locals initialised at k" is not a static set: a local moved on one branch before the
  suspension is initialised on some arrivals only. The one who knows scopes (the adapter) writes
  the cancel path exactly as it writes a `return`'s exit: reverse declaration order, a flag test
  where initialisation depends on the path. Flags are ordinary locals; liveness over the cancel
  path saves them with no special rule.
- D3. A local's home never changes: saved means every access goes to the state, from its first
  write. No copy out at a suspension and back at a resume: that moves a local a saved reference
  points at (on LIR: every `slot_addr` of a kept slot is the state pointer plus its offset).
- D4. Borrows are counted coarsely: borrowed once, kept until `StorageDead`. Exact (the loan's
  liveness from blaze-flow) is a later refinement and can only save fewer locals.
- D5. Upvars are always saved, and dropped at `Unresumed` in declaration order: the Reference
  leaves a closure's order unspecified, and this is the order of closure.rs's environment tuple.
- D6. `Panicked` is reserved and never stored yet: the rebuilt path aborts on a panic (flow.md
  section 1). With unwinding, a poll that unwinds stores it first.
- D7. A cancel path may not suspend and may not return a value (no async drop); a resume path may
  not reach `cancelled`. Each is refused by name (blaze-lir `CoroutineError::SuspendOnCancel`,
  `ReturnOnCancel`, `CancelledOnResume`).
- D8. One slot per saved local, no overlap. Overlap is the later optimisation: two saved locals
  may share bytes when their storage is never live at once. It needs `StorageLive` beside
  `StorageDead` at statement grain and the conflict matrix; blaze-flow's `saved` is its first
  input.
- D9. blaze-flow's `saved` is also the checker's exact interior (blaze-flow-lower coroutine.rs
  `interior`): the types of the locals saved at some point, in place of the superset.

## Proof by simulation (blaze-lir coroutine_tests.rs; blaze-flow suspend_tests.rs)

The split (crates/blaze-lir/src/coroutine_tests.rs): one interpreter gives the LIR operations
their meaning; two drivers use it. The direct run keeps one frame alive from the first poll to the
end and treats `suspend` as "this poll returned; go on at the resume block, or at the cancel block
if the state is dropped here". The machine run keeps only the state between polls: a new frame for
each call of `poll`, then `drop` when the schedule cancels. A pointer into the frame of an earlier
call is a fault, as is a load of bytes nothing wrote. For random bodies, every schedule of Pending
and Ready answers and a cancel after every yield, the two traces are equal. Planted bugs, each
caught by a control: `SlotLeftOut`, `NoReloadOnCancel`, `TagAfterReturn`, `ResumeNext`
(blaze-lir coroutine.rs `Plant`).

The saved set (crates/blaze-flow/src/suspend_tests.rs): a local used after the suspension is
saved, one dead before it is not, one used only on the cancel path is saved, and a borrowed local
is saved even when unused after; the last has a planted control (`SavedPlant::NoBorrows`). The
exact interior (crates/blaze-flow-lower/src/coroutine_tests.rs) has its control
`InteriorPlant::EveryLocal`.

## From a checked body and back

In: the walk blaze-flow-lower does (one mid-level form per body, D61; built as
crates/blaze-flow-lower/src/coroutine.rs `lower_async`, a second output of blaze-flow-lower itself,
so there is one walk from the typed body to statements), with three additions:
`Expr::Await` becomes the Reference's loop (a local for the future, `Poll`, a switch, `Suspend`),
each `Suspend` gets a cancel path made by the routine that makes a `return`'s exit, and a binding
whose initialisation depends on the path gets a flag local (blaze-flow moves.rs `drop_kind` says
which). The upvars come from the checker's captures (`Checked::captures`). An `async fn` is not a
second case: its root is the node a written `async move { .. }` lowers to
(`Expr::AsyncBlock { body, capture: Capture::Move }`, the parameter `let`s first in `body`), its
type `TyView::Future(root, T, interior)` is the fn's hidden type in `Checked::hidden`, and its
parameters are that block's by-move captures. So `root` is always an `Expr::AsyncBlock`, and the
fn itself is an ordinary body that makes the state and returns it.

Back (decision D73, which replaced hooks in the walk: homes for named bindings alone are unsound,
since the walk keeps temporaries in stack slots and its cached values do not dominate a resume
block): the builder builds the body as an ORDINARY function and one pass over LIR splits it.

## The LIR pass (crates/blaze-lir/src/coroutine.rs)

In: one function whose entry parameters are what every poll passes (the return pointer if any,
the state pointer, the context) and whose returns are `Poll<T>`'s; each await ends a block in
`suspend point, resume, cancel, (the Pending this poll returns)`; a cancel path ends in
`cancelled`; and `Shape { state_param, prefix_size, prefix_align, unresumed }`: the upvars are
the first bytes of the state, read through the state pointer as a closure reads its environment,
and `unresumed` is the block that drops them. Out: `poll`, `drop(state)` and `StateLayout { size,
align, tag_offset, tag_ty, points, fields }`.

- A slot is kept when at some suspend its address may have been taken and its storage not ended
  (forward, made by `slot_addr`/`slot_live`, ended by `slot_dead`): D4's coarse rule on LIR. Every
  `slot_addr` of a kept slot is the state pointer plus its offset (D3).
- A value is kept when it is live into a resume or cancel block. It is stored once where it is
  made and loaded in each other block that uses it; constants and addresses are made again. Entry
  parameters are never kept: each poll passes its own.
- The state: upvars, then kept slots and values by falling alignment, then the tag (`i8`, `i16`
  or `i32`). The head of `poll` switches on the tag (a completed state traps, 0xfe10, until the
  library's panic is passed in); a suspend stores its tag and returns its values; a return stores
  `Returned`. `drop` switches into the cancel blocks, `unresumed` at tag 0.
- The verifier has no "pass done" flag: a `suspend` is a two-way edge in the body, and the
  ordinary dominance rule refuses, in the outputs, any value crossing a poll outside the state.
- Proof (coroutine_tests.rs): random bodies run directly and as the machine, every schedule,
  traces equal; planted `SlotLeftOut`, `NoReloadOnCancel`, `TagAfterReturn`, `ResumeNext`.

## The builder (crates/blaze-build/src/coroutine.rs)

Tests: `crates/blaze-build/src/tests/coroutines.rs` and `crates/blaze-build/src/tests/coroutines_emit.rs`
(filter `coroutines`, aarch64).

- **Identity**: the owner, the ORDINAL among the owner body's `Expr::AsyncBlock`s in expression
  order, and the owner's arguments: a closure's scheme. The ground form is
  `Future(ExprId(ordinal), output, FnDef(owner, owner_args))`, made by one constructor
  (`coroutine_identity`) and read by one reader (`coroutine_of`); the checker's body-local form is
  refused by name in `ground`.
- **Three functions per instance** (`build_coroutine` in lib.rs, `coroutine::build`): RESUME, the
  body split by the pass, parameters `(result, state, cx)`; the ENTRY, `Future::poll` at
  `[coroutine]`, which reads the state pointer out of its `Pin` and calls RESUME; DROP,
  `drop_in_place` at `[coroutine]`, the pass's `drop`. The entry exists because every aggregate
  crosses the Rust ABI by address, so `Pin<&mut Self>` arrives as a pointer to a pointer, and the
  pass needs the state pointer as an entry parameter.
- **The state where the block is evaluated** (`coroutine_value`): closure.rs's environment
  writes, then the tag stored as `Unresumed`. A state is copied as bytes (`copy_state`).
- **An `async fn`** is its body's root `async move` block (`async_fn_output`).
- **`.await`** (`await_operand`, `await_poll`): the Reference's steps; the cancel block drops
  the future's temporary, then `drop_from(0)`, then `cancelled`.
- **What it asks**: `LayoutFacts::coroutine(owner, ordinal)` (the upvars and the output),
  `LayoutFacts::coroutine_state(owner, ordinal, args, ground)` (per instance; `Missing` means
  that body is not built and split yet), `LayoutFacts::opaque_hidden`, `Adts::future_poll` and
  `Adts::async_items`.

## The checker and the run path

- **Capture rows** (`crates/blaze-typeck/src/rules/async_fn.rs`,
  `analyze_coroutine_captures`): the closure walk run over each `async` block, rows in
  `Checked::captures` with the block in the first column. `async move`: every row by value. Not
  `move`: the closure rule's modes, and what an inner `move` closure or `async move` block takes
  by value the outer block holds by value unless `Copy`.
- **An auto trait that failed on the superset interior**: E0277 stands only when the self type
  is an `async move` block of this body capturing by value a variable that fails the trait;
  every other failure is `CheckError::Unported(INTERIOR_GAP)`, never a "no".
- **The run path** (`crates/blaze-ladder/src/front2/coroutines.rs`) answers the builder's facts.
  A state asked before it is made is `Missing`; the driver builds the wanted coroutine (and
  first each coroutine its own state holds), records the states, and builds the asking instance
  again. The three functions of an instance are tabled as their owner's. The rung is
  `crates/blaze-cli/tests/ladder/front2/x0-async-01`.

## Refused by name

`.await` outside an async block's body (an async closure's); `async gen`; a zero-sized awaited
future; a `Poll` or `Pin` not of the library's shape; an `async fn` whose root is not its block;
an opaque type whose defining fn is not in the program or gave no hidden type; a coroutine whose
state holds itself (E0733); an `async` `main`. `Send`/`Sync` of a future that fails only on a
value not known live across an `.await`, and a caller's auto trait of an `async fn`'s future,
are `CheckError::Unported`.
