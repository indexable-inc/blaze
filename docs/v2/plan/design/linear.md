---
status: plan
summary: "#[blaze::linear]: a type whose values may not be dropped implicitly, checked on built MIR with the borrow check; generic code admits one only through a `#[blaze::linear]` type parameter."
date: 2026-11-02
---
# Lane linear: `#[blaze::linear]`

## What it is

```rust
#![feature(register_tool)]
#![register_tool(blaze)]

#[blaze::linear]
pub struct Txn { id: u32 }

impl Txn {
    #[blaze::consumes]
    pub fn commit(self) { /* .. */ }
}

#[blaze::consumes]
pub fn consume<#[blaze::linear] T>(_: T) {}
```

A value of a `#[blaze::linear]` struct or enum may not be dropped implicitly. Every path out of
the code that holds one must consume it, in one of three ways:

- move it into a function that takes it by value (which must consume it in turn);
- destructure it (`let Txn { id } = t;`);
- give it to a function marked `#[blaze::consumes]`. Its by-value parameters are not linear in
  its body, so the function may drop them. `consume` above is the explicit drop.

A drop the program may run on a path that does not unwind is an error. Its message names the
value (`t`, `p.b`, `(x as Some).0`), its type, and the path:

- the end of its scope;
- an overwrite (`t = ..`);
- an early exit: `return`, `?`, `break`, `continue` or `become`;
- a branch that does not move it.

The error is reported at that line, with a label at the declaration.

Under rustc the attributes are inert tool attributes (`register_tool(blaze)`). A program with no
mark gets no error and no change: the check writes no MIR, and every type it asks about holds no
linear value. The test `unmarked_code_gets_no_error` pins this.

## Where it lives

Rule 26 (semantics in the front end) puts the check in the front end, in its own crate:

- `crates/hir-ty-linear`: the check. `check_body(db, body, kind, out)` runs on one built MIR body.
- `crates/hir-ty-mir/src/mir/transform/drop_states.rs`: `drop_states(db, body, root)`. It reuses
  drop elaboration's move paths and maybe-init/maybe-uninit analysis (`MoveData::gather_rooted`),
  rooted at the locals a predicate picks rather than at those with drop glue. It hands back each
  `Drop` with what each move path below the dropped place may hold there.
- `crates/hir-def`: `AttrFlags::LINEAR` and `AttrFlags::CONSUMES`, and the inert `blaze::linear` /
  `blaze::consumes` names in `attr_resolution.rs`.
- `crates/hir-ty-borrowck`: `check_owner` calls `check_body` for each body it checks: the owner's,
  closures, coroutines and inline consts. It reports the results as `BorrowckDiagnostic`s with code
  `LINEAR`. This gives three things:
  - A linear error refuses the item exactly as a borrow error does (`gate::checked`).
  - The verdict filed in the item store covers the check. The attribute queries the check makes
    are among the reads `body_store::capture` records, and the store key includes the front end's
    build ID.
  - hir reports the errors with the borrow checker's.

## The rules

1. **Which types are linear.** These types are linear in themselves:
   - an ADT marked `#[blaze::linear]`;
   - a type parameter or a trait (its `Self`) marked so;
   - an associated type marked so;
   - a projection of an associated type marked so;
   - a `dyn` of a marked trait.

   A type *holds* a linear value when a part of it does. The parts are:
   - fields (substituted);
   - variants;
   - tuple and array elements, where an array has a length other than 0;
   - slices;
   - `Box` contents;
   - captures of a closure, a coroutine-closure or a coroutine;
   - the hidden type of an opaque type;
   - the inner type of an unsafe binder.

   References, raw pointers, `ManuallyDrop`, `PhantomData` and unions hold none.
2. **Drops.** The check reads built MIR, where every place that may be dropped has a `Drop`.
   - A path whose type is linear itself is consumed when one of its children is moved out on
     every path to the drop (it was destructured, in part or in full). Otherwise it is an error if
     it may be initialised there.
   - A part that no move path reaches is dropped with its parent, so it is an error if it holds a
     linear value.
   - A place that is not tracked (behind a reference, or a field of a type with a `Drop` impl) is
     an error if its type holds one. `*r = new` through `&mut Txn` is this case: rustc drops the
     old value.
3. **Unwinding is allowed.** Cleanup drops are not checked. Every call may unwind, so a rule over
   unwind edges would leave no call legal (Niko's post below reaches the same conclusion). A
   panic runs the type's `Drop` impl, if it has one, as its abandonment path (a rollback), or else
   its fields' glue. With `panic=abort` nothing runs. The error text says this.
4. **Generic code.** A type argument that holds a linear value, or a function, closure or `dyn`
   whose signature takes or gives one, is refused for a type parameter not marked
   `#[blaze::linear]`. The check covers every function a body names (each `FnDef` operand), with
   these resolutions:
   - A trait method is checked as the impl method it resolves to (`lookup_impl_method`).
   - A call of a closure, function item or function pointer through `Fn*` / `AsyncFn*` goes into
     a body that is checked on its own.
   - A constructor of a type with no `Drop` impl only builds the value.
   - Builtin derive methods drop nothing.

   An impl that binds an associated type to a linear type is refused when the trait's associated
   type is not marked.
5. **Async.** A coroutine body (an `async fn` or `async` block) that holds a linear value in any
   local is refused. The future can be dropped at any `.await`, and no code of the body runs to
   consume it.

## Design choice and prior art

Admission is opt-in per parameter (`<#[blaze::linear] T>`), the `?Drop` / `~Copyable` shape. Every
existing generic item keeps its meaning, which is what makes linearity an extension that changes
nothing for unmarked code. A `T: Linear` bound would be backwards: linear types are the larger
set, so the bound has to relax a default rather than add one.

- Niko Matsakis, "Must move types" (2023). Proposes `?Drop` as a relaxing bound, destructuring
  as a move, `Option<T: ?Drop>`-style conditional linearity, and `?` and panics as the hard parts:
  https://smallcultfollowing.com/babysteps/blog/2023/03/16/must-move-types/
  Our rules 1 to 3 follow it. Unwinding runs drops, as it proposes.
- Gankra, "The Pain Of Real Linear Types in Rust" (2017). Explains why every generic std API and
  every panic path is affected, which is why std generics stay refused here (below).
- Yosh Wuyts, "Linearity and Control" and "Linear Types One-Pager" (2023), and withoutboats,
  "Asynchronous clean-up" (2024). These separate *unforgettable* (`Leak`) from *undroppable*
  types. Ours are undroppable. The async refusal is boats' point that a future can be dropped
  mid-await.
- rust-lang/rfcs#776 "Add linear type facility". Its review argues for the `?Drop` direction over
  a `Linear` trait.
- Swift SE-0390 (noncopyable types, `~Copyable` as the relaxing generic constraint), SE-0366
  (`consume` operator) and SE-0427 (noncopyable generics). Swift relaxes copy, not drop, but the
  generic opt-in is the same shape as ours. `blaze::consume` plays the role of `consume x` +
  `discard`.
- Austral: linear types are checked per path through every branch and loop, and are consumed
  only by destructuring or a consuming call. That is our rule 2.
- Vale, "Higher RAII": a type may forbid implicit destruction, so the destructor is an explicit
  call that takes arguments. That is the `Pending::finish(db)` use below.
- Existing Rust crates:
  - `Manishearth/humpty_dumpty`: a rustc lint plugin for implicit-drop protection. It is the
    nearest thing in Rust and is unmaintained.
  - `matklad/drop_bomb` and `ray-kast/dispose`: runtime-checked versions, which panic in `Drop`.
  - `#[must_use]`: only warns, and only for an unused expression, not for a value dropped later.

Found with `gh api -X GET search/repositories -f q="linear types rust"` and `"drop bomb rust"`, Exa,
and fed1eb428d:docs/research (nothing about linear types there).

## Applied in blaze

`hir_ty_borrowck::gate::Pending` is `#[blaze::linear]`, and `Pending::finish` is
`#[blaze::consumes]`. A live patch that drops its `Pending` without finishing it never decides
whether the patch is rolled back. Until now only `#[must_use]` guarded this, which says nothing
once the value is bound to a variable.

## Not checked yet (follow-ups)

- **Std generics.** `Vec<Txn>`, `Box::new(t)`, `Option<Txn>` through `?`, `format!("{t:?}")`
  (`Argument::new_debug<T>`) and `mem::forget` are all refused, because std's parameters are not
  marked. The fix is inferred admission: a parameter admits linear types when its body consumes
  every value of it, computed as a query over the callee's MIR. That would let `Option::map` or
  `identity` pass without marking std.
- An impl that only binds associated types, with no fn, is not checked. The impl-binding check
  runs at the impl's first fn body.
- Bodies that `check_owner` does not reach:
  - `BLAZE_BORROWCK=off`;
  - bodies with type errors;
  - bodies refused for `DropInConst`;
  - statics' and consts' own bodies, which are checked only when borrowck runs on them.
- No published `blaze::consume` crate yet. Define the two-line `consume` above.
- The tests in `crates/hir-ty-linear/src/tests.rs` have been type-checked but not run.
