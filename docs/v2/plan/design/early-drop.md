---
status: plan
summary: Edition change 11, `#![blaze::early_drop]`. A value whose destructor only frees memory is dropped after its last use, not at the end of its scope; guards and everything else keep Rust's drop. Which values, where the drop goes, raw pointers, order, what the borrow checker newly accepts, the `unused_guard` lint, what is built and what is not.
date: 2026-11-10
---
# Early drop

Rust drops a local where its scope ends. For a lock guard that is the point; for a `Vec` it
only holds memory longer and makes the borrow checker count a use nobody wrote. Under
`#![blaze::early_drop]` a value whose destructor only frees memory is dropped after its last
use. A crate without the attribute builds to the same MIR as before (`superset-of-rust`).
[Linear](linear.md) values are never dropped implicitly, so they are outside this.

## 1. Which values

`drop_class(ty)` (`hir_ty_mir::mir::early_drop::class`) is `None` (no drop glue), `Early` or
`Scoped(why)`, decided on the type alone, the same in every crate and every body:

- **No `Drop` impl of its own** (a struct, enum, tuple, array, closure by its captures): early
  when every part with drop glue is early.
- **A toolchain type with a `Drop` impl**: early when it is in the table of owning containers
  (`Box`, `Vec`, `VecDeque`, `String`, `BTreeMap`, `BTreeSet`, `BinaryHeap`, `LinkedList`,
  `CString`, `HashMap`, `HashSet`), its elements are early or have no glue, and its allocator
  is `Global`. Any other allocator is scoped: its `deallocate` is code the rule has not seen.
  A toolchain `Drop` impl not in the table (guards, files, channels, `Drain`) is scoped.
- **Shared ownership is scoped**: `Rc`, `Arc`, both `Weak`s. Dropping one writes a count other
  values read (`strong_count`, `try_unwrap`, `get_mut`, `upgrade`): its timing is observable.
- **Any other `Drop` impl is scoped unless its type says `#[blaze::drop(early)]`**, which is
  checked: the impl's `drop` must be *frees-only*, or the attribute is an error naming the
  effect. `#[blaze::drop(early, trusted = "why")]` asserts it unchecked (a pooled buffer); a
  trusted destructor must not unwind. Its type arguments are still classified. `#[blaze::drop
  (scoped)]` forbids early drop of the type and of anything holding it; on a type that drops
  nothing it is an error.
- A type parameter, an opaque or unnormalized alias, `dyn Trait`, a coroutine: scoped.

**Frees-only** (`hir_ty_pure::drop_effects`, the effect walk's summary of the impl's `drop` in
its own where-clauses): its kinds are within `Alloc` (the global allocator; the walk does not
split allocate from free, and both touch only the heap) and `Open` (a type argument's drop,
which each use's type answers). No `Io`, `Env`, `Clock`, `Thread`, `MutableState`, `Address`,
`Random`, `Foreign`, `Unchecked`, `Block`, `AllocIn`, and outside the toolchain no `Panic`
(std's destructors panic only in `unsafe` precondition checks). The walk cannot tell a write
to a shared count from a free, so a hand-written reference count must not be marked.

Why declared and not inferred for a crate's own impl: (a) the pass runs while MIR is built and
the walk reads built MIR, so inferring it for a `Drop` impl of an early-drop crate is a query
cycle (`MirLowerError::Loop`); (b) a log line added to a `drop` body would silently move every
user's drop and change which downstream programs borrow-check.

**Generic functions.** Borrow checking and drop elaboration run on the generic body, so
placement is decided once there: a local whose type mentions a type parameter is scoped in
every instance (`Vec<T>` in `fn f<T>`). Acceptance and timing never depend on the instance.

## 2. Where

**The rule.** An early local is dropped at the first point after which it is not used on any
path and nothing that may hold a borrow of it is live. What is still initialized at the end of
its scope is dropped there as today.

**How** (`hir_ty_mir::mir::early_drop`, on the built body, before the borrow checker, where
`#![blaze::handles]` runs): the pass only *inserts* `Drop(x)` terminators. The scope-end and
unwind drops stay, find `x` uninitialized and are elaborated away, so drop flags and partial
moves are drop elaboration's as today, and a wrong insertion can never free twice: a use after
an inserted drop is a borrow-check error, never undefined behavior.

- **Liveness** is the passes' one (`optimize::live_on_entry`, shared with dead store
  elimination and handles); a drop of an early local is no use, a drop of a scoped one is.
- **Holders** of `x` are the locals that may hold a borrow of it: the target of `&x` or of a
  borrow of a holder; a local a holder flows into; in a statement or call with a holder among
  its operands, every other operand and the destination whose type has a lifetime, and what
  such a reference points to. Flow-insensitive, and an over-approximation of NLL's loan
  liveness (every outlives constraint relates two types of one statement), so the borrow
  checker refuses no inserted drop: nothing accepted without the attribute is refused with it.
- **Frontier.** `Drop(x)` goes on each CFG edge `B -> S` (never an unwind edge) where `x` or a
  holder was live or touched in `B`, none is live on entry to `S`, `x` has storage and was
  not last moved out, and `S` is not where `x`'s scope ends anyway. Blocks end at calls, so
  this is right after the call of the last use.
- Branches: each arm has its own edge. Loops: a local used in the body is held around the back
  edge and dropped on the exit edges. Conditionally initialized and partially moved locals:
  the inserted drop is conditional or open after elaboration. Temporaries are locals: one of
  an early type dies after its last use, not at the end of the statement or `match`; which
  borrows of it are legal is unchanged. A closure is classified by its captures; a capture by
  reference makes it a holder. `async` bodies and `const fn` are not touched yet.
- **Unwinding.** Cleanup blocks are unchanged: they drop what is still initialized (less).
- Not yet: the exact frontier from the borrow checker's loans (holders are coarser where a
  context with a lifetime is passed everywhere), a drop before the terminator of the last
  use's block (what makes more calls tail calls), an earlier `StorageDead`.

## 3. Raw pointers and unsafe

The borrow checker cannot see a raw pointer's uses. A local is *pinned*, scoped for the whole
body, when a local that may hold a borrow of it (a) has a type that is or names `*const`,
`*mut` or `NonNull` in its arguments, or is an ADT outside the toolchain with such a field,
recursively; (b) is cast to an integer or transmuted; (c) is an operand of an intrinsic or
`asm!`. So `v.as_ptr()`, `&raw const v` and `NonNull::from(&v)` pin `v`, and a value moved out
of a pinned local is pinned. It errs toward scoped. An FFI call that takes a reference and
returns no pointer ends its loan at the call, as today. Not seen: a pointer kept inside a
toolchain type by `unsafe` code elsewhere.

## 4. Order

Locals that die on one edge drop in the order their scopes would have dropped them
(`MirBody::scope_ends`: innermost first, last declared first), then by descending local index.

## 5. What can observe it

Allowed to differ: allocation counters, `GlobalAlloc` hooks and heap profilers see frees
earlier (a custom `#[global_allocator]` is ambient, as in Rust); a freed address may be reused
sooner; peak memory; a debugger sees the value gone. Not affected: `size_of_val`,
`mem::forget`, `ManuallyDrop`, `Rc`/`Arc` counts, what a scoped destructor does.

## 6. What the borrow checker newly accepts

Only programs where a local's drop was a *use of a lifetime*: its type, or a part's, has a
`Drop` impl with a lifetime or type argument and no `#[may_dangle]`, and the type is early.
std's containers have `#[may_dangle]` and a type without a `Drop` impl has no drop use, so
`Vec<&T>` and a struct of references change nothing. The gain is for a crate's own types
marked early. `Cursor<'a>` wraps `&'a mut Vec<u8>` and has a marked `Drop`:

```rust
let c = Cursor::new(&mut buf); c.put(1);
buf.len();                       // 1. E0502 today: `c` is dropped at the end of the scope
if flag { c.put(1); buf.len(); } else { c.put(2); }   // 2. the same, on one arm
match Cursor::new(&mut buf).first() {                 // 3. a temporary in a scrutinee
    Some(b) => buf.push(b),      //    E0499 today: the temporary lives to the end of the match
    None => {}
}
```

Not accepted: a borrow of a later-declared local pushed into an earlier container (E0597): the
order rule is Rust's. Tests: each example accepted under the attribute and refused without;
every fixture accepted without it accepted with it; a body with no early local the same MIR.

## 7. The lint

`blaze::unused_guard` (`blaze::edition`): `let _ = expr;` of a scoped type drops it at once,
which is almost never meant. Fix: `let _guard = `. Not built in this slice.

## 8. Performance, and how each is measured

Expected: lower peak footprint in long functions (the main effect: `blaze explain`, memory by
task, on blaze's own crates before and after) and the allocator reusing a block just freed;
with the "not yet" of section 2, more tail calls and shared stack slots. Not expected: a
change in steady-state throughput. Cost: one liveness pass per body, span `mir.early_drop`.

## Built and not built

Built on `early-drop-1110`: the gate, `#[blaze::drop(..)]`, `drop_class`, the pass, the
frees-only check, the tests. Not built: `blaze check` reporting `check_drop_attr`, the lint,
`async` bodies, the "not yet" of section 2. `edition::Extensions` (`rework-int2-1109`) gets
`early_drop` as `handles`; under `#[blaze::may(..)]` frees-only is a `drop` that may `alloc`.
