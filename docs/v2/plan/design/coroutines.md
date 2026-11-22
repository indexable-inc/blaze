---
status: plan
summary: The coroutine transform, in two homes - crates/blaze-flow/src/suspend.rs (which locals a body saves) and crates/blaze-lir/src/coroutine.rs (the split into `poll` and `drop`, and the state). What turns the body of an `async fn` or an `async` block (later a generator) into a state and one `poll` function the code builder builds as ordinary code. Which locals are saved (liveness with borrows); the state and its tags; the split; drop on cancel as a per-point cancel path; where `.await` is desugared; the simulations that prove it with planted bugs; the lowering from a checked body; what the builder needs.
date: 2026-11-16
---
# Coroutines: the state, `resume`, and drop on cancel

A coroutine is the sibling of a closure (codegen.md, "Closures and the Fn traits"): a closure is an
environment and a body function taking it; a coroutine is an environment that also holds a tag and
the locals that outlive a suspension, and a body function that starts with a `match` on the tag.
It has two homes (one concept, one home each; the first interface crate, `crates/blaze-coroutine`,
was deleted once both held): `crates/blaze-flow/src/suspend.rs`, which locals a body saves, per
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
second case: as reported by the opaque-types lane on 2026-11-16 (blaze-lower desugar.rs `async_fn`,
not compiled when this was written), its root is the node a written `async move { .. }` lowers to
(`Expr::AsyncBlock { body, capture: Capture::Move }`, the parameter `let`s first in `body`), its
type `TyView::Future(root, T, interior)` is the fn's hidden type in `Checked::hidden`, and its
parameters are that block's by-move captures. So `root` is always an `Expr::AsyncBlock`, and the
fn itself is an ordinary body that makes the state and returns it.

Back (decision D73, which replaced hooks in the walk: homes for named bindings alone are unsound,
since the walk keeps temporaries in stack slots and its cached values do not dominate a resume
block): the builder builds the body as an ORDINARY function and one pass over LIR splits it.

## The LIR pass (crates/blaze-lir/src/coroutine.rs; 2026-11-16, blaze-lir "32 passed, 0 failed")

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
- Owed: debug rows of kept slots; overlap (D8); exact byte liveness; `Panicked` with unwinding.

## The builder's two emit points (the next leg: a NEW crates/blaze-build/src/coroutine.rs)

1. `Expr::AsyncBlock` in the PARENT body (one dispatch arm in walk.rs): build closure.rs's
   environment value into a slot of the state's size and alignment, store 0 at `tag_offset`.
   Both come from `LayoutFacts::coroutine(owner, ordinal)`, which is the pass's `StateLayout` for
   that body: so a body's LIR and its split are a record made before any parent that holds its
   state is built (the same order a closure's environment already needs).
2. The body: a third `Kind` in `run`: parameters as `poll`'s ABI has them (`Poll<T>` returned
   by the function's ordinary return form), captures bound at environment offsets off the state
   parameter exactly as a closure's are, `ret_value` wrapping in `Poll::Ready` by the ordinary
   enum construction. Nothing else in the walk changes: slots and cached values are the pass's.
3. `Expr::Await` (one dispatch arm): the Reference's steps: call `into_future` (the checker's
   callee row) into a `temp()` slot; a loop head block L; `Pin::new_unchecked(&mut tmp)` is the
   slot's address; call `Future::poll(pin, cx)` (`Adts::future_poll`, the instance at the held
   type; `cx` is the entry parameter); test the returned `Poll`'s tag with the walk's own
   discriminant read; Ready: its payload is the value. Pending: make block C, emit there what
   `return` emits for its exit (`drop_from(0)`, the future's temporary included) and then
   `cancelled` in place of the return; emit `suspend k, L, C, (Poll::Pending in return form)`.
4. The unresumed block: after the body, one block that drops the captures (closure.rs's
   environment drop) and ends in `cancelled`; its index is `Shape::unresumed`.
5. lib.rs: after `finish`, a function of this `Kind` goes through `blaze_lir::coroutine::split`;
   `poll` is emitted under the body's symbol, `drop` is the state type's drop glue
   (`build_drop_glue` calls it for `PNode::Future`), the layout is the record of item 1.
6. Proof: native tests in blaze-build (futures that count their drops, polled and cancelled by
   a hand-written executor): `an_async_block_polled_to_completion` first.

What the builder needs, by name (from a search of walk.rs, closure.rs, lib.rs, layout.rs):
- walk.rs dispatch: `Expr::AsyncBlock` (make the state: closure.rs's environment value plus the
  tag) and `Expr::Await` (the loop); `run` (a third `Kind`); `drop_from` (the cancel block);
  `temp()` (the awaited future's slot).
- closure.rs: `closures_in` (a sibling listing a body's async blocks), `Parts`, `run_entry`.
- layout.rs `closure_facts` and `LayoutFacts::closure`: a sibling `coroutine` giving the state's
  fields (upvars, saved locals' types, the tag); lib.rs `build_drop_glue` (the state's glue is
  the `match` above, not a tuple's); instance.rs's closure key (owner, ordinal, arguments).
- blaze-typeck: `Future::poll` at each `awaited` type as a callee; the exact interior (D9).
- The library: `Poll`, `Context`, `Pin` lang items, and the `panic_const_async_fn_resumed` path.

## Builder writer 4's notes (2026-11-16; piece 1 green on the integrator's run at 13:31: "test blaze-build: exit 0; 139 passed, 0 failed", filter coroutines "2 passed, 0 failed"; handed on by rule 12)

Built (crates/blaze-build; tests `src/tests/coroutines.rs`, run with the filter `coroutines`):

- **Identity** (coroutine.rs): owner, ORDINAL among the owner body's `Expr::AsyncBlock`s in
  expression order (`coroutines_in`), the owner's arguments: a closure's scheme. The checker's
  `PNode::Future(ExprId, output, interior)` holds a body-local expression id, so it cannot be a
  ground type. Interim ground form, one constructor (`identity`, exported as
  `coroutine_identity`) and one reader (`coroutine_of`): `Future(ExprId(ordinal), output,
  FnDef(owner, owner_args))`. instance.rs `ground` passes that form on and refuses the checker's
  form by name; the instance key hashes tag 22 and the ordinal, children the output and the
  `FnDef`. WANTED in blaze-typeck (not additive, so not done): `PNode::Coroutine { owner,
  ordinal, args }` made by `open_future`; then `identity`, `coroutine_of` and `ground_node` are
  the only three functions to change.
- **Facts** (layout.rs, both defaulted): `LayoutFacts::coroutine(owner, ordinal) ->
  Option<CoroutineFacts>` (the upvars' tuple and the output, in the owner's parameters; made by
  `coroutine_facts(body, checked, ordinal, generics, out, scratch) -> (env, output)`), and
  `LayoutFacts::coroutine_state(owner, ordinal, args, ground) -> Result<Option<StateShape>,
  Missing>`: per INSTANCE, since the pass's layout depends on the owner's arguments (the plan
  above said one fact; a generic `async fn` needs the arguments). `Err(Missing)` is "that body
  is not built and split yet": the layout caches nothing and is asked again.
- **Layout** (layout.rs `parts`, `compute`): one field, the upvars at offset 0, in the state's
  size and alignment; no scalar form, no niche; a state smaller than its upvars is refused.
- `Adts::future_poll() -> Option<ItemKey>` is declared (defaulted); nothing calls it yet.

Not built: every emit point (items 1 to 5 above) and their four tests. For the successor, what
the search found, in the order to write:

1. walk.rs treats a closure's type in three places a coroutine needs too: `repr` (the
   aggregate arm, near "A closure: a struct of one field"; the `PNode::Future` refusal below it
   goes), `is_agg`, and the walk's own layout rows (`parts.push((Name(0), env))`). Add a
   `coroutine_parts(g)` beside closure.rs `closure_parts`. The row's `needs_drop` must be TRUE
   always (core's `needs_drop`: "may return true for types that don't actually need to be
   dropped"); its drop is a CALL of the `drop_in_place` instance at `[the coroutine]`, which IS
   the split `drop` function: find where `drop_at`/`glue` choose between fields and an impl.
2. The type of an `Expr::AsyncBlock` in its own body: never through `ground` (refused there):
   a `Walk::coroutine_ty(e)` = `identity(self.ground, self.body.owner, ordinal, own args list,
   ground_ty(output))`, and `expr_ty`-based callers (`repr_at`, `expr_ty` then `ground_ty`)
   must reach it for a block's expression. A future NESTED in another type of the body's
   record (`Option<impl Future>` of a local) stays refused by name until the checker's change.
3. `Expr::AsyncBlock` value: closure.rs `closure_value` with the state's layout (`temp(repr)`,
   captures at `capture_offsets`' sibling), then a store of 0 in `StateShape::tag_ty` at
   `tag_offset` (ask `facts.coroutine_state` again; `Missing` becomes
   `BuildError::MissingRecord { item: owner }`). `Capture::Ref` versus `Capture::Move` is the
   checker's capture kind per row: build what the rows say, refuse a block with no rows that
   names an outer binding.
4. `Kind::Coroutine(ExprId)` and `build_coroutine(body, checked, ordinal, sig, subst, adts,
   alloc)` shaped as `build_closure`: parameters `(sret?, state: ptr, cx: ptr)`, `self.ret` the
   ground `Poll<output>` (`Adts::lang_adt(LangItem::Poll)` if the lang item exists in ids.rs:
   check), captures bound as `run_closure` binds an `FnOnce` body's (by-value ones owned:
   `own_pat`), `walk_root`, then the unresumed block, then `blaze_lir::coroutine::split`. It
   returns poll, drop and `StateShape::of(&split.layout)`: the driver keeps the shape as the
   record `coroutine_state` answers from. The poll instance is `(future_poll, [coroutine])`,
   the drop instance `(drop_in_place, [coroutine])`.
5. `Expr::Await`: item 3 of the list above; `Adts::future_poll` at `[held type]`; the held
   type needs a seam answer too (`Checked` has no `awaited` column: the checker keeps it in
   the machine only; wanted: a `Checked::awaited` table `(ExprId, PTy)`, or the builder
   normalizes `<T as IntoFuture>::IntoFuture` through `LayoutFacts::normalize`, which exists).
6. The resumed panic: pass the block that calls `Adts::lang_fn("panic_const_async_fn_resumed")`
   to the pass in place of trap 0xfe10 (the pass takes no such block yet: writer 3's side).

Wanted from the front end, by name: rows in `Checked::captures` for an async block (keyed by
the block's expression; an `async fn`'s parameters by value): the checker writes none today
(rules/coroutine.rs: "Capture kinds are not computed"); an `async fn`'s return type is
`PNode::Opaque`, which instance.rs `ground` refuses: the builder needs the hidden type through
a seam (`opaque_hidden`, answered in the identity form over the owner's parameters), and
blaze-typeck instance.rs refuses to export a `Future` (`Limit::Export`).

## Builder writer 5's notes (2026-11-16; green on the integrator's run at 14:00: filter coroutines "test blaze-build: exit 0; 6 passed, 0 failed", whole "test blaze-build: exit 0; 143 passed, 0 failed"; an async fn's opaque type must be filed with `Sized`, or a caller's `.await` of it does not check)

Built (crates/blaze-build/src; tests `tests/coroutines_emit.rs`, run with the filter
`coroutines`, aarch64 only):

- **Three functions per coroutine instance** (`build_coroutine` in lib.rs, `coroutine::build`):
  RESUME, the body split by the pass, parameters `(result, state, cx)`, instance
  `(AsyncItems::resume, [coroutine])`; the ENTRY, `Future::poll` at `[coroutine]`, which reads
  the state pointer out of its `Pin` and calls RESUME; DROP, `drop_in_place` at `[coroutine]`,
  the pass's `drop`. The entry exists because every aggregate crosses the Rust ABI by address,
  so `Pin<&mut Self>` arrives as a pointer to a pointer, and the pass needs the state pointer
  as an entry parameter. It goes when `Pin` crosses as its one scalar (an ABI decision, not
  taken here).
- **The state where the block is evaluated** (`coroutine_value`): closure.rs's environment
  writes, then the tag stored as `Unresumed`. A state is copied as bytes (`copy_state`, hooked
  in `Walk::copy`): its row has one field, the upvars.
- **An `async fn`** is its body's root `async move` block: `run` takes the block's coroutine
  type as the result type whatever the signature writes (`async_fn_output`).
- **`.await`** (`await_operand`, `Task::Await`, `await_poll`): the Reference's six steps; the
  cancel block drops the future's temporary, then `drop_from(0)`, then `cancelled`.
- **A block's type in its own body** is made by `Walk::ground_ty` (`own_block`,
  `own_coroutine_ty`), never by `ground`.

Seams the front end implements (all defaulted, all refuse by name until answered):
`Adts::future_poll() -> Option<ItemKey>`; `Adts::async_items() -> Option<AsyncItems>`
(`into_future`, `poll`, `ready`, `pending`, `pin`, `resume`);
`LayoutFacts::coroutine(owner, ordinal) -> Option<CoroutineFacts<'_>>`;
`LayoutFacts::coroutine_state(owner, ordinal, args, ground) -> Result<Option<StateShape>, Missing>`;
`LayoutFacts::opaque_hidden(opaque, args, ground) -> Result<PTy, BuildError>` (an `async fn`'s
opaque type is `coroutine_identity` at the ROOT block's ordinal, which is the last when the
body holds other async blocks); `LayoutFacts::normalize` at `(IntoFuture, "IntoFuture", [T])`
(it exists). The driver builds a coroutine BEFORE any body that holds its state by value, and
places the three functions under the three instances above.

Refused by name: `.await` outside an async block's body (an async closure's); an `async fn`
whose root is not its block; a block naming an outer binding with no row in
`Checked::captures` (the checker writes none for async blocks today: so NO capture and no
`async fn` parameter builds yet, and the upvar path has no test); a zero-sized awaited future;
a `Poll` or `Pin` not of the library's shape; a coroutine type asked of `build_drop_glue`.

Owed: capture rows from the checker (then a test with a parameter and a by-reference
capture); the resumed-after-completion panic block (item 6 above); each `.await` keeps its
`Poll` result slot and the future's slot in the state to the body's end (no `slot_dead` is
written: the pass's D8), and the body's one `Pin` slot too; a value in flight in an enclosing
expression at an await is dropped by neither path (the walk's `return` has the same gap).

## Front-end writer 6's notes (2026-11-16; the checker's half green at 14:20: "test blaze-typeck: exit 0; 175 passed, 0 failed"; the blaze-ladder half and the rung NEVER COMPILED when this was written)

What the front end did with async before this leg (read, not built): the checker already TYPED
it in full (`async fn` as its root `async move` block with the written output, rules/
opaque_def.rs; `async {}`, rules/coroutine.rs; `.await` as `<T as IntoFuture>::Output` with
E0728 outside async, rules/async_fmt.rs), and front2 already made an `async fn`'s head as
`impl Future<Output = T>` (opaque 0, with `Sized`). So a census "clean" async body was really
checked. Missing were: capture rows for async blocks, an honest `Send` answer, and every seam
answer of the run path.

Built, the checker (crates/blaze-typeck/src/rules/async_fn.rs, new):
- **Capture rows** (`analyze_coroutine_captures`, called in `end_sequence` after
  `analyze_closures`): the closure walk (`visit_for_captures`) run over each `async` block, rows
  in `Checked::captures` with the block in the first column. `async move`: every row by value,
  `Copy` never asked. Not `move`: the closure rule's modes, and what an inner `move` closure
  or `async move` block takes by value the outer block holds by value unless `Copy`.
- **An auto trait that failed on the superset interior** (`coroutine_interior_gap`, asked in
  `report_fulfill_errors` for an `Unsatisfied` goal): E0277 stands only when the self type is an
  `async move` block of this body capturing by value a variable that fails the trait; every
  other failure is `CheckError::Unported(INTERIOR_GAP)`, never a "no". The suite's two tests
  that pinned a false E0277 now pin the gap.

Built, the run path (crates/blaze-ladder; new front2/coroutines.rs, hooks in run.rs):
- `LayoutFacts::coroutine`: `CoroutineTable`, from `blaze_build::coroutine_facts` per block.
- `LayoutFacts::coroutine_state`: `States`. A state asked before it is made is `Missing` and is
  remembered as wanted; `build_from` catches the `MissingRecord`, runs `build_coroutines` (a
  stack: the wanted coroutine and, first, each coroutine its own state holds), records the
  states, and builds the asking instance again.
- `LayoutFacts::opaque_hidden`: from the defining fn's `Checked::hidden`; a hidden async block
  becomes `coroutine_identity` at its ordinal. Needs the seam's new `opaque_of`.
- `Adts::future_poll` (lang `poll`), `Adts::async_items` (lang `Poll`, `pin`,
  `coroutine_resume` through the seam's new `lang_named`; `IntoFuture` by path; the variants
  `Ready` and `Pending` by name in the `Poll` row).
- The three functions of an instance (`Future::poll`, the resume fn, `drop_in_place`, each at
  `[coroutine]`) are tabled as their OWNER's (`owner_of`, in the refs loop and for a vtable
  word), keyed by `key_args`, and taken from the one build's `BuiltParts` at their slots.
- The walk queues `Future::poll` impl fns once a reached body awaits (back_needs.rs `awaits`).

Proof written: suite/coroutine.rs (`async_move_block_captures_by_value`,
`outer_block_holds_by_value_what_an_inner_move_takes`, `await_output_mismatch_is_refused`,
`rc_held_across_await_is_a_gap`, `rc_dropped_before_await_is_a_gap_not_e0277`), and the rung
crates/blaze-cli/tests/ladder/front2/x0-async-01 (no_core, its own `future.rs`; expected
`front2/x0-async-01: PASS (status 42, stdout byte-equal)`; 1, 2, 3, 4 name the wrong build).

Typed gaps and refusals, each by name:
- `Send`/`Sync` of a future that fails only on a value not known live across an `.await`
  (`INTERIOR_GAP`). Needed: the flow layer's liveness at each `.await` (locals from binding to
  scope end less moves, temporaries to their statement's end), then the interior is exact and
  both suite expectations flip (`Ok([Unsatisfied])` and `Ok([])`).
- A CALLER's auto trait of an `async fn`'s future (`tokio::spawn(f())`): still the older gap
  "an auto trait of an opaque type: its hidden type is not recorded yet": front2's checker
  world answers `opaque_hidden` with `None` (world_items.rs). Needed: that world reading the
  defining body's `Checked::hidden`, and the future's identity spelled in the type
  (`PNode::Coroutine { owner, ordinal, args }`) so it means the same in another body.
- Both gaps are `CheckError::Unported(&'static str)` and cannot name the coroutine: a variant
  that does is not additive (blaze-real's and blaze-ladder's matches are exhaustive).
- Refused by the builder as before: async closures' `.await`, `async gen`, a zero-sized
  awaited future. Refused by the driver: an opaque type whose defining fn is not in the
  program or gave no hidden type; a coroutine whose state holds itself (E0733); an `async`
  `main` (entry.rs).
- Not done: a rung on the real core with a `RawWakerVTable` waker; a rung that drops a
  suspended future (the cancel path); `async_items` is `None` when no reached body names
  `Poll` (its ADT row is made on demand), which refuses by name.
- A wrong mode left in the closure rule, not this item's: a closure that is not `move` holding
  an inner `move` construct still records `ByRef` (the fix made for blocks applies as it is).
