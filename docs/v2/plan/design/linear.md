---
status: plan
summary: "#[blaze::linear]: a type whose values may not be dropped implicitly, checked on built MIR with the borrow check; generic code admits one only through a `#[blaze::linear]` type parameter, `Option` and `Result` do, and an error exit abandons a value through its type's `Drop`."
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

Two things make this usable with fallible code. `Option` and `Result` hold a linear value and
hand it on (rule 4), and an exit that returns a failure abandons a live value through its
type's `Drop`, as a panic does (rule 6):

```rust
#[blaze::linear]
struct File { .. }
impl Drop for File { fn drop(&mut self) { /* what abandoning one does */ } }

fn save() -> Result<(), Error> {
    let file = open()?; // a `Result<File, Error>` through `?`
    file.write()?;      // leaves with `file` live: its `Drop` runs
    file.close()?;      // consumed (`close` is `#[blaze::consumes]`)
    Ok(())
}
```

Under rustc the attributes are inert tool attributes (`register_tool(blaze)`). A program with no
mark gets no error and no change: the check writes no MIR, and every type it asks about holds no
linear value. The test `unmarked_code_gets_no_error` pins this. Rules 4 and 6 keep it: the
marks in library/core are inert attributes on impls that are only split in place, a callee's
parameters are looked at only for a type argument that reaches a linear value, and the error
exits of a body are looked for only once one of its locals holds one.

## Where it lives

Rule 26 (semantics in the front end) puts the check in the front end, in its own crate:

- `crates/hir-ty-linear`: the check. `check_body(db, body, kind, out)` runs on one built MIR body.
- `crates/hir-ty-mir/src/mir/transform/drop_states.rs`:
  `drop_states(db, body, root, abandoned_at, abandons)`. It reuses drop elaboration's move paths
  and maybe-init/maybe-uninit analysis (`MoveData::gather_rooted`), rooted at the locals a
  predicate picks rather than at those with drop glue. It hands back each `Drop` with what each
  move path below the dropped place may hold there. At each location of `abandoned_at` (rule
  6's error exits) the locals `abandons` picks are taken as moved out, an effect only this
  analysis has (`Effects::add`): drop elaboration's own run is untouched.
- `library/core/src/{option,result}.rs` and `crates/test-utils/src/minicore.rs` (the tests'
  core): the marks of rule 4.
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
   - a projection of an associated type marked so, or of any associated type of a type
     parameter marked so (`<T as Try>::Output` of a `#[blaze::linear] T`: see the binding
     rule 4 admits);
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

   References, raw pointers, `ManuallyDrop`, `PhantomData` and unions hold none. So an
   `Option<Txn>` or a `Result<Txn, E>` is linear by its variant, with no mark of its own: it
   is consumed by a `match` (or `if let`, `let .. else`) that moves the payload out, and on the
   arm of the other variant it holds nothing (the analysis knows which variant an arm is in).
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
   that gives one back, is refused for a type parameter not marked `#[blaze::linear]`. What a
   callable takes does not count: generic code can call it only with a value it holds, and it
   holds a linear one only through a parameter that admits it. The check covers every function a
   body names (each `FnDef` operand), with these resolutions:
   - A trait method is checked as the impl method it resolves to (`lookup_impl_method`).
   - A call of a closure, function item, function pointer or type parameter through `Fn*` /
     `AsyncFn*` goes into a body that is checked on its own. (For a parameter `F`, that is
     whatever `F` is given. std's own `Fn*` impls for `&F`, `&mut F` and `Box<F>` only hand
     the arguments on.)
   - A constructor of a type with no `Drop` impl only builds the value.
   - Builtin derive methods drop nothing.

   A marked parameter is checked once, in the generic body, where it is linear. Admission is
   therefore per impl or per function, never per call: a method admits a linear argument for an
   impl's parameter when the impl marks it, and every method of that impl is then checked to
   consume each value of it.

   An impl that binds an associated type to a linear type is refused when the trait's associated
   type is not marked. One binding is admitted without the mark: to the impl's own marked
   parameter, when the impl's self type holds a value of it (`type Output = T` of
   `impl<#[blaze::linear] T> Try for Option<T>`). The self type is then linear wherever the
   binding is, so code generic over the trait is given it only for a marked parameter of its
   own, where the projection is linear (rule 1). Marking `Try::Output` instead would make every
   value of that type linear in all of core's code generic over `Try`.

   **`Option` and `Result`.** library/core marks the parameters of the impls whose methods hand
   the payload on or panic, by writing each type's inherent impl as several (`library/UPSTREAM`
   has the list; `library/DIGEST` covers it):
   - `Option<T>`, `T` marked: `is_some`, `is_none`, `as_ref`, `as_mut`, `expect`, `unwrap`,
     `map`, `ok_or`, `ok_or_else`, `and_then`.
   - `Result<T, E>`, both marked: `is_ok`, `is_err`, `as_ref`, `as_mut`, `map`, `map_err`,
     `and_then`, `or_else`, `unwrap_or_else`. `T` marked: `ok`, `expect`, `unwrap` (they drop or
     print the `Err`).
   - `Try` and `FromResidual` for both, `T` marked: what `?` calls. `from_residual` only
     converts the error, so no `T` exists in it.

   Every other item stays in an impl with no mark and is refused for a linear payload, with
   the item named: `unwrap_or`, `unwrap_or_default`, `or`, `and`, `xor`, `filter`, `zip`,
   `is_some_and`, `inspect`, `map_or`, `err`, `unwrap_err`, iteration, `Clone`, `PartialEq`
   and the rest drop a payload or have not been checked; `take`, `replace` and `insert` go
   through `mem::replace`, which is not marked. A closure given to `map` or `and_then` may
   take the linear payload but not give a linear value back (use `match`). An error type that
   is linear cannot go through `?` (`E` is not marked in `FromResidual`: `From::from` is not).
   The rule for moving a method into a marked impl: its body, checked with the parameter
   linear, reports nothing.
5. **Async.** A coroutine body (an `async fn` or `async` block) that holds a linear value in any
   local is refused. The future can be dropped at any `.await`, and no code of the body runs to
   consume it.

6. **An error exit abandons.** In a body that returns a `Result` or an `Option`, the exit
   that returns the failure abandons the live values that have an abandonment path: they are
   dropped there as a panic drops them, and that is no error. The exit is recognised on built
   MIR as the write of the failure to the return place:
   - `Err(..)` or `None` built into it: `return Err(e)`, `return None`, and either as the value
     the body ends with (`if bad { Err(e) } else { .. }`);
   - the value of `FromResidual::from_residual` called into it, which is what `?` returns.

   After that write a body only drops what is in scope and returns, so every way on from it
   is the error exit. The init analysis takes each abandoned local as moved out at the write.
   That keeps the check exact where exits share a drop (lowering shares one drop of a local
   among every `return` and `?` that leaves it): a `return Ok(..)` that gets to the same drop
   with the value live is still reported, and named.

   A value has an abandonment path when every linear value it holds is of a marked type with
   a `Drop` impl its author wrote for it: the type's own, or for a struct that of a field
   whose type the same crate defines. The second form is the guard a type keeps in a field
   (`Scope { live: Live, .. }` with `impl Drop for Live`) so that its finishing call can still
   take its other fields out, which a `Drop` impl on the type itself forbids (E0509). A marked
   type with neither has none (`Txn { id: u32 }`, a marked type parameter, a marked enum with
   no `Drop`), and a live value of it at a `?` is the error it was, now saying so: "give its
   linear type an abandonment `Drop` or finish it before the `?`". A local is abandoned whole
   or not at all: `(File, Txn)` is not.

   Everything else is as before. Falling off the end, `return Ok(..)`, `return Some(..)`,
   `break` and `continue` must have consumed the value. A body that returns any other type
   has no error exit, so nothing changes in it (`Poll<Result<..>>` and `ControlFlow` among
   them). A `?` inside a `try` block leaves the block, not the body, and abandons nothing.
   With contracts on the function (`ensures`), `?` writes the return place through the
   contract's check and is not recognised. Returning `None` counts as the failure even where
   a function means nothing bad by it.

   This is Niko's "`?` is the hard part" answered the way unwinding already is: the type
   author says once what abandonment does, and both a panic and an error return run it.

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

- **Std generics.** `Vec<Txn>`, `Box::new(t)`, `format!("{t:?}")` (`Argument::new_debug<T>`),
  `mem::forget`, `mem::replace` (so `Option::take`) and every `Option` and `Result` item
  rule 4 does not list are refused, because their parameters are not marked. The fix is
  inferred admission: a parameter admits linear types when its body consumes every value of
  it, computed as a query over the callee's MIR. That would let `identity` pass without a
  mark, and replace the hand-kept list in library/core.
- **A callable that gives a linear value** to a parameter whose bound names the output
  (`F: FnOnce(T) -> U` with `U` marked) could be admitted: the callee's body sees the output
  as `U`. Today the callable is refused, so `map` cannot turn one linear value into another.
- **bstd.** `Task` and `ReadWrite` (so `File<ReadWrite>`) have no abandonment path, so a `?`
  with one live is still refused. `Scope`, `Pending`, `Listener` and `Conn` have one (a guard
  field).
- An impl that only binds associated types, with no fn, is not checked. The impl-binding check
  runs at the impl's first fn body.
- Bodies that `check_owner` does not reach:
  - `BLAZE_BORROWCK=off`;
  - bodies with type errors;
  - bodies refused for `DropInConst`;
  - statics' and consts' own bodies, which are checked only when borrowck runs on them.
- No published `blaze::consume` crate yet. Define the two-line `consume` above.
- The tests in `crates/hir-ty-linear/src/tests.rs` have been type-checked but not run; the
  ones for rules 4's `Option`/`Result` items and rule 6 were written without a build.
