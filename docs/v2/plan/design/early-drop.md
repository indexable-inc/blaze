---
status: plan
summary: Edition change 11, `#![blaze::early_drop]`. A value whose destructor only frees memory is dropped after its last use, not at the end of its scope; guards and everything else keep Rust's drop. Which values, where the drop goes, raw pointers, order, what the borrow checker newly accepts, the `unused_guard` lint, what is built and what is not.
date: 2026-11-10
---
# Early drop

Rust drops a local where its scope ends. For a lock guard that is the point; for a `Vec` or a
`String` it only holds memory longer and makes the borrow checker count a use nobody wrote.
Under `#![blaze::early_drop]` a value whose destructor only frees memory is dropped after its
last use. A crate without the attribute builds to the same MIR as before (`superset-of-rust`).
[Linear](linear.md) values are never dropped implicitly, so they are outside this.

## 1. Which values

`drop_class(ty)` (`hir_ty_mir::mir::early_drop::class`) is `None` (no drop glue), `Early` or
`Scoped(why)`, decided on the type alone, the same in every crate and every body:

- **No `Drop` impl of its own** (a struct, enum, tuple, array, closure by its captures): early
  when every part with drop glue is early.
- **A toolchain type with a `Drop` impl**: early when it is in the table of owning containers
  (`Box`, `Vec`, `VecDeque`, `String`, `BTreeMap`, `BTreeSet`, `BinaryHeap`, `LinkedList`,
  `CString`, `HashMap`, `HashSet`), its element arguments are early or have no glue, and its
  allocator argument is `Global`. Any other allocator is scoped: its `deallocate` is code the
  rule has not seen. A toolchain `Drop` impl not in the table is scoped.
- **Shared ownership is scoped**: `Rc`, `Arc`, both `Weak`s. Dropping one writes a count other
  values read (`strong_count`, `try_unwrap`, `get_mut`, `Weak::upgrade`), so its timing is
  observable. Channels, guards, files, `Ref`/`RefMut`, `Drain` are not in the table: scoped.
- **Any other `Drop` impl is scoped unless its type says `#[blaze::drop(early)]`**, which is
  checked: the impl's `drop`, monomorphic or in its own where-clauses, must be *frees-only*
  (below), or the attribute is an error naming the effect. `#[blaze::drop(early, trusted =
  "why")]` asserts it unchecked (a pooled buffer whose drop returns it to its pool); a trusted
  destructor must not unwind. `#[blaze::drop(scoped)]` forbids early drop of the type and of
  anything holding it; on a type with no drop glue it is an error.
- A type parameter, an opaque or unnormalized alias, `dyn Trait`, a coroutine: scoped.

**Frees-only** (`hir_ty_pure::drop_effects`, over the effect walk's summary of the `drop`
instance): every kind it may have is `Alloc` (the global allocator, which the walk cannot split
into allocate and free; both touch only the heap); no `Io`, `Env`, `Clock`, `Thread`,
`MutableState`, `Address`, `Random`, `Foreign`, `Unchecked`, `Block`, `ForeignUnknown`,
`ForeignUnwind`, `AllocIn`, `Open`; and outside the toolchain no `Panic` or `Overflow` either
(std's destructors panic only in `unsafe` precondition checks, which abort on what is already
undefined). It must also write nothing through a pointer it holds (`MemoryEffects::writes <=
Direct`) unless that write is the allocator's.

Why declared, not inferred, for a crate's own `Drop` impl: (a) the pass runs while MIR is built
and the walk reads built MIR, so inferring it for a `Drop` impl of an early-drop crate is a
query cycle (`MirLowerError::Loop`); (b) adding a log line to a `drop` body would silently move
every user's drop and change which downstream programs borrow-check. The declaration is the
border; `blaze::edition` gets `unmarked_early_drop` later, as `unmarked_pure` is.

**Generic functions.** Borrow checking and drop elaboration both run on the generic body, before
monomorphization, so placement is decided once there: a local whose type mentions a type
parameter is scoped in every instance (`Vec<T>` in `fn f<T>` is scoped even for `T = u32`).
Acceptance and drop timing therefore never depend on the instance. Placing per instance would
need loan facts per instance; not planned.

## 2. Where

**The rule.** An early local is dropped at the first point after which it is not used on any
path and nothing that may hold a borrow of it is live. Whatever is still initialized at the end
of its scope is dropped there as today.

**How it is built** (`hir_ty_mir::mir::early_drop`, on the built body, before the borrow
checker, where `#![blaze::handles]` runs): the pass only *inserts* `Drop(x)` terminators; it
removes nothing. The scope-end and unwind drops stay, find `x` uninitialized and are elaborated
away, so drop flags, partial moves and conditional initialization are drop elaboration's, as
they are today, and a wrong insertion can never free twice. A use after an inserted drop is a
borrow-check error (a moved value), never undefined behavior.

- **Liveness** is the passes' one backward liveness (`optimize::live_on_entry`, shared with
  dead store elimination and handles), where a drop of an early local is no use and a drop of a
  scoped one is.
- **Holders.** `holders(x)` are the locals that may hold a borrow of `x`: the target of `&x` or
  of a borrow of a holder; any local a holder flows into; in a statement or call with a holder
  among its operands, every other operand and the destination whose type has a lifetime, and
  what such a reference points to. This is flow-insensitive and over-approximates NLL's loan
  liveness (every outlives constraint relates two types in one statement), so no inserted drop
  is refused: nothing accepted without the attribute is rejected with it. `x` is *held* where
  it or a holder is live.
- **Frontier.** `Drop(x)` goes on each CFG edge `B -> S` (never an unwind edge) where `x` was
  held or touched in `B`, is not held on entry to `S`, has storage, and was not last moved
  out. Blocks end at calls, so this is after the statement or call of the last use.
- Branches: each arm gets its own edge. Loops: a local used in the body is held around the back
  edge and dropped on the exit edges; one declared in the body is dropped each turn.
  Conditionally initialized and partially moved locals: the inserted drop is conditional or
  open after elaboration. Temporaries are locals: one of an early type dies after its last
  use, not at the end of the statement, match or `if let`; which borrows of it are legal is
  unchanged. A closure is classified by its captures and capturing is a use; a by-reference
  capture makes the closure a holder. Coroutine bodies (`async`) are not touched yet.
- **Unwinding.** Cleanup blocks are not changed: they drop what is still initialized, which is
  less. An inserted drop unwinds where the terminator before it does.
- Not yet: the exact frontier from the borrow checker's loans (holders are coarser where a
  context with a lifetime is passed everywhere), a drop before the terminator of the last
  use's block (what makes more calls tail calls), an earlier `StorageDead`.

## 3. Raw pointers and unsafe

The borrow checker cannot see a raw pointer's uses. A local is *pinned*, scoped for the whole
body, when a value that flows from a holder of it (a) has a type that is or names `*const`,
`*mut` or `NonNull` in its arguments, or is an ADT outside the toolchain with such a field,
recursively; (b) is cast to an integer or transmuted; (c) is an operand of an intrinsic call.
So `v.as_ptr()`, `&raw const v`, `NonNull::from(&v)` and `ptr::from_ref(&v)` pin `v`. A value
moved out of a pinned local is pinned (the buffer moves with it). The test errs toward
scoped. An FFI call that takes a reference and returns no pointer ends its loan at the call, as
today. Not seen: a pointer hidden in a toolchain type built by `unsafe` code in another
function; such code must mark its type `#[blaze::drop(scoped)]`.

## 4. Order

Locals that die on one edge are dropped in the order their scopes would have dropped them
(`MirBody::scope_ends`: innermost scope first, last declared first), then any other by
descending local index. Never by hash order; the output is the same on every run.

## 5. What can observe it

Allowed to differ: allocation counters, `GlobalAlloc` hooks and heap profilers see frees
earlier; a freed address may be reused sooner (`ptr::addr` of a later allocation); peak memory
and stack use; a debugger or a core dump sees the value gone. A custom `#[global_allocator]`
is ambient, as in Rust, where allocation is not an observable effect either: its `dealloc` runs
earlier, and one that logs sees a different order. Not affected: `size_of_val`, `mem::forget`
and `ManuallyDrop` (no drop to move), `Rc`/`Arc` counts (scoped), anything a scoped destructor
does, panics outside trusted destructors.

## 6. What the borrow checker newly accepts

Only programs where a local's drop was a *use of a lifetime*: its type, or a field's, has a
`Drop` impl with a lifetime or type argument and no `#[may_dangle]`, and the type is early
(marked `#[blaze::drop(early)]`, or a container of one). std's containers already have
`#[may_dangle]` and types without a `Drop` impl have no drop use, so `Vec<&T>` and a struct of
references change nothing. `Cursor<'a>` below wraps `&'a mut Vec<u8>` and has a marked `Drop`.

```rust
let c = Cursor::new(&mut buf); c.put(1);
buf.len();                       // 1. E0502 today: `c` is dropped at the end of the scope
if flag { c.put(1); buf.len(); } else { c.put(2); }   // 2. the same, on one arm
match Cursor::new(&mut buf).first() {                 // 3. a temporary in a scrutinee
    Some(b) => buf.push(b),      //    E0499 today: the temporary lives to the end of the match
    None => {}
}
```

Not accepted: pushing a borrow of a later-declared local into an earlier container (`names.push
(&name)`, E0597): the order rule is Rust's. Tests: each example is accepted under the attribute
and refused without it; every fixture accepted without it is accepted with it; a body with no
early local is the same MIR.

## 7. The lint

`blaze::unused_guard`, in `blaze::edition`: `let _ = expr;` where `expr`'s type is scoped drops
it at once, and `let _name = expr;` that is never read keeps it; the first almost never means
it. Warns on `let _ = <scoped>` with the fix `let _guard = `. Not built in this slice.

## 8. Performance, and how each is measured

Expected: lower peak footprint in long functions (the main effect: `blaze explain` memory by
task on blaze's own crates, before and after); the allocator reusing a block just freed (hits
of the thread cache); a little smaller stack once storage ends early (not yet). Not expected:
any change in steady-state throughput. Compile time: one liveness pass per body of a crate
with the attribute, traced as `mir.early_drop`.

## Built and not built

Built on `early-drop-1110`: the gate, `#[blaze::drop(..)]`, `drop_class`, the pass, the
frees-only check, the tests. Not built: the check of `#[blaze::drop(early)]` is not yet
reported by `blaze check` (the function exists); the lint; coroutine bodies; the three "not
yet" of section 2. `edition::Extensions` (`rework-int2-1109`) gets `early_drop` as it does
`handles`; under `terminates-effect`'s `#[blaze::may(..)]` frees-only is `may(alloc)` on `drop`.
