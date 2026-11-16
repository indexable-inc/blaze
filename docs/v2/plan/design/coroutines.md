---
status: plan
summary: The coroutine transform (crates/blaze-coroutine). What turns the body of an `async fn` or an `async` block (later a generator) into a state and one `resume` function the code builder builds as ordinary code. The small form it works on; which locals are saved (liveness with borrows); the state and its tags; the rewrite; drop on cancel as a per-point cancel path; where `.await` is desugared; the simulation that proves it with five planted bugs; the adapter from a checked body and back; what the builder needs.
date: 2026-11-16
---
# Coroutines: the state, `resume`, and drop on cancel

A coroutine is the sibling of a closure (codegen.md, "Closures and the Fn traits"): a closure is an
environment and a body function taking it; a coroutine is an environment that also holds a tag and
the locals that outlive a suspension, and a body function that starts with a `match` on the tag.
The interface crate is `crates/blaze-coroutine`: the form, the analysis, the rewrite, and a
simulation with planted bugs. Nothing in it knows the checker or the builder (rule 79).

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

1. **A value and one function.** The value is the state. `resume(state: Pin<&mut State>, cx: &mut
   Context<'_>) -> Poll<Output>` is built by the ordinary builder. An `async fn` only makes the
   state (its arguments become the upvars, the tag `Unresumed`) and returns it; nothing runs.
2. **The form** (`ir.rs`): numbered locals (`0..upvars` are the upvars), blocks as ranges of one
   statement array, a terminator per block. Statements define, use, borrow, load and store through
   a reference, call, poll, drop, and end a local's storage. `Suspend { point, resume, drop }` is
   the only suspension: where to go on when polled again, and where to go when the state is
   dropped while parked there. The cancel path ends in `Cancelled`.
3. **Which locals are saved** (`live.rs`, `layout.rs`). A local is saved when, at some `Suspend`,
   it is live by use on the resume or the cancel path, or it may have been borrowed and its storage
   has not ended. The second rule is what makes the state self-referential: a saved reference
   points at a saved local, so the state must not move once polled. That is the promise `Pin<&mut
   State>` carries, and why the checker never makes the future `Unpin`. Every other local stays an
   ordinary local of `resume`, new at each poll. `cx` is one of those: it is "the current task
   context" of each poll and is never saved.
4. **The state** holds the upvars first, in order (so a state before its first poll is a closure
   environment plus a tag), then one slot per saved local in declaration order, and the tag:
   `Unresumed = 0`, `Returned = 1`, `Panicked = 2`, point `k` is `3 + k`.
5. **The rewrite** (`transform.rs`) is small because the form was chosen for it: each `Suspend`
   becomes `Park(point)` (store the tag, return `Poll::Pending`), and the head `match` is a table,
   `Machine::entry(tag)`: start, the resume block of point `k`, or the library's panic "`async fn`
   resumed after completion". `Return(v)` in a machine stores `Returned` and returns `Ready(v)`.
6. **Drop on cancel** is a second function from the same blocks: `Machine::drop_entry(tag)` is the
   cancel path of point `k`; at `Unresumed` a block that drops the upvars; at `Returned` nothing.

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
  points at (planted bug `SavedByCopy`).
- D4. Borrows are counted coarsely: borrowed once, kept until `StorageDead`. Exact (the loan's
  liveness from blaze-flow) is a later refinement and can only save fewer locals.
- D5. Upvars are always saved, and dropped at `Unresumed` in declaration order: the Reference
  leaves a closure's order unspecified, and this is the order of closure.rs's environment tuple.
- D6. `Panicked` is reserved and never stored yet: the rebuilt path aborts on a panic (flow.md
  section 1). With unwinding, a poll that unwinds stores it first.
- D7. A cancel path may not suspend and may not return a value (no async drop); a resume path may
  not reach `Cancelled`. Both are refused by name (`TransformError`).
- D8. One slot per saved local, no overlap. Overlap is the later optimisation: two saved locals
  may share bytes when their storage is never live at once. It needs `StorageLive` beside
  `StorageDead` at statement grain and the conflict matrix; `Layout::across` is its first input.
- D9. `Layout::across` is also the checker's owed exact interior (rules/coroutine.rs, "Owed"): the
  types of the locals saved at some point, in place of today's superset.

## Proof by simulation (`interp.rs`, `tests.rs`)

One executor gives the statements their meaning; two drivers use it. The direct run keeps one
frame alive and treats `Suspend` as "yielded at k, go on or cancel". The machine run keeps only
the state between polls: a new frame each poll, entry by the tag, `Park` to leave. A reference
into a frame of an earlier poll is a fault. For random bodies from a grammar (straight code,
branches, counted loops, an await inside a loop, a borrow held across an await, a write through
it, early return, moves before and after an await, drop flags), every schedule of Pending and
Ready answers, and a cancel after every yield (and before the first poll, and after the return),
the two traces (calls with arguments, polls, drops in order, yields, the final value) are equal.
Planted bugs, each caught by a control: `LiveLeftOut`, `BorrowsNotCounted`, `CancelDropsAll`,
`ResumeNext`, `SavedByCopy`. Ten hand-written bodies have their traces written out.

## From a checked body and back (the next leg)

```rust
// crates/blaze-coroutine-lower, beside blaze-flow-lower
pub fn lower<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>, checked: &Checked, root: ExprId, facts: &F, alloc: A,
) -> Result<Lowered<A>, LowerError>;
pub struct Lowered<A: Allocator> {
    pub body: blaze_coroutine::Body<A>,
    /// What each local is: a binding, a temporary of an expression, an await's future, a flag.
    pub local_of: Vec<Origin, A>,
    /// The `Expr::Await` of each point.
    pub point_of: Vec<ExprId, A>,
}
```

In: the same walk blaze-flow-lower does (one mid-level form per body, D61), with three additions:
`Expr::Await` becomes the Reference's loop (a local for the future, `Poll`, a switch, `Suspend`),
each `Suspend` gets a cancel path made by the routine that makes a `return`'s exit, and a binding
whose initialisation depends on the path gets a flag local (blaze-flow moves.rs `drop_kind` says
which). The upvars come from the checker's captures (`Checked::captures`). An `async fn` is not a
second case: as reported by the opaque-types lane on 2026-11-16 (blaze-lower desugar.rs `async_fn`,
not compiled when this was written), its root is the node a written `async move { .. }` lowers to
(`Expr::AsyncBlock { body, capture: Capture::Move }`, the parameter `let`s first in `body`), its
type `TyView::Future(root, T, interior)` is the fn's hidden type in `Checked::hidden`, and its
parameters are that block's by-move captures. So `root` is always an `Expr::AsyncBlock`, and the
fn itself is an ordinary body that makes the state and returns it. Best is to make it a second output of blaze-flow-lower itself, so there is one
walk from the typed body to statements, not two.

Back: the builder does NOT walk the machine's blocks. blaze-build walks the expression tree with
structured drop scopes (walk.rs `scope_pats`, `drop_from`); a second builder from a block form
would double every expression form. So the walk goes on walking the tree and takes three things
from `Machine`: (1) homes: a saved binding or temporary lives at its state field, as a captured
binding lives at its environment field; (2) at `Expr::Await`, the loop, and on Pending "store tag
`3 + k`, return Pending", with the loop head as the resume block of `k`; (3) at the head, a
switch on the tag over those blocks. The drop glue is a second function: per point, a snapshot of
the walk's drop scopes at the await, replayed by `drop_from` with every home in the state. The
simulation proves the plan; that the walk emits what the plan says is proven by native tests in
blaze-build (futures that count their drops, polled and cancelled by a hand-written executor).

What the builder needs, by name (from a search of walk.rs, closure.rs, lib.rs, layout.rs):
- walk.rs dispatch: `Expr::AsyncBlock` (make the state: closure.rs's environment value plus the
  tag) and `Expr::Await` (the loop); `run` (a third `Kind`: parameters `(state, cx)`, the head
  switch); `own`, `own_pat`, `home` and `slots` (a home that is a state field); `scan_flags` and
  `set_flag` (a flag's home in the state when the layout saves it); `drop_local`, `drop_held`,
  `drop_owned`, `drop_from`, `scope_start` (replayed in the glue from a snapshot).
- closure.rs: `closures_in` (a sibling listing a body's async blocks), `Parts`, `run_entry`.
- layout.rs `closure_facts` and `LayoutFacts::closure`: a sibling `coroutine` giving the state's
  fields (upvars, saved locals' types, the tag); lib.rs `build_drop_glue` (the state's glue is
  the `match` above, not a tuple's); instance.rs's closure key (owner, ordinal, arguments).
- blaze-typeck: `Future::poll` at each `awaited` type as a callee; the exact interior (D9).
- The library: `Poll`, `Context`, `Pin` lang items, and the `panic_const_async_fn_resumed` path.
