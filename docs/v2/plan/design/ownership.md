---
status: plan
summary: Ownership properties derived by compile-time reflection. One const walk over what a type owns answers reachability questions (`Acyclic`, no capability, no secret, no cell); `where const { COND }` makes a blanket impl of its answer; generic const expressions put arithmetic in types; `Shared` over an acyclic type and `Shares<T, K, N, D>` hold linear values.
date: 2026-11-10
---
# Ownership by reflection: `Acyclic`, `Shared`, `Shares`

A property of a type's shape is const Rust over reflection, stated as a trait with a blanket
impl (rule `reflection-first`). Written, unbuilt (2026-11-10); ledger rows E16, E18, whose
checks name the tests (`hir-ty-mir` `tests/consteval/reach.rs` and `tests/where_const.rs` first).

## 1. The walk (`bstd::own::reach`, built)

`reaches::<T, Q>() -> Verdict` walks from `T` through what it owns and answers the question `Q`
(a type with `const ASK: Ask`: what to find, how far to follow). It is one const function over
`TypeId`s from `core::mem::type_info`, so it is decided per instance. `none::<T, Q>()` is
`true` or a panic whose message is the path.

How a type holds what is inside it:

| type | the walk |
|---|---|
| struct, enum, tuple | its fields, private ones too; array, slice: its elements |
| `unsafe impl Owns<How> for C<T>` | `C`'s type arguments, held as `How` says; `C`'s fields are not read |
| reference, `Weak` | owns nothing; primitive, `fn` pointer: holds nothing |
| raw pointer, union, `dyn Trait`, closure, foreign type | not seen into: a refusal, never a guess |

`How` is `Inline` (`Option`), `Solely` (`Box`, `Vec`, maps), `Jointly` (`Rc`, `Arc`, `Shared`,
`Shares`), `Mutably` (`UnsafeCell` and what is built on it: `RefCell`, locks, `Atomic`) or
`Nothing`. bstd states it for std's containers and its own (`own.rs`), each a trusted `unsafe impl`.

**The cycle question.** A value owns itself only through a counted pointer set after the value
exists. So `T` is refused exactly when the walk meets a `Jointly` pointer below a `Mutably`
holder whose pointee leads back to `T`; that first pointer under a cell decides for everything
below it, so the walk stops there. `List { tail: Option<Box<List>> }`, `Tree { kids: Vec<Tree> }`
and `Bc<T> { x: Rc<T> }` are acyclic; `Node { next: RefCell<Option<Rc<Node>>> }` and a parent
and child through a cell are not; `Pool { jobs: Mutex<Vec<Arc<Job>>> }` is acyclic exactly when
`Job` cannot reach `Pool`, and a type that merely holds a `Pool` is acyclic either way.

- **Termination.** The visited set is a fixed array of `(TypeId, behind a cell)`, compared with
  `TypeId`'s const `==`; each entry is expanded once; more than 192 is a refusal saying so. A
  tuple's `TypeKind` is now answered (`Tuple`; rustc ICEs there) so the walk can tell one.
- **The refusal names the path**: "`Child.parent`: a `Rc<Parent>` behind
  `RefCell<Option<Rc<Parent>>>` reaches `Parent.kids: Vec<Rc<Child>>`, which reaches `Child`".
  Names come from `core::reflect::name_of(id)`, added for this (intrinsic `blaze_type_name_of`).
- **`dyn`.** Rust has no `dyn Tr + Acyclic` (only auto traits may follow the first), so the
  object type carries it as a supertrait: `trait Job: Acyclic` makes `dyn Job` pass where it is
  owned outside every cell. Behind a cell it is refused whatever its trait says: the hidden
  type, acyclic alone, could hold a counted pointer back to the cell's owner.
- **Closures and `impl Trait`** are not seen into (phase 2 reflects no captures): left.
- **Trusted.** `unsafe impl Acyclic for X {}` answers for `X` alone; a holder of an `X` is still
  walked through `X`'s fields. It and `Owns` are `unsafe impl`s, for the lint
  `blaze::untrusted_unsafe` ([edition](edition.md)) to flag; no listing of their own is built.

## 2. Other questions (built)

The same walk, other `Ask`s: `AnyCapability` (a type implementing `bstd::own::Capability`, owned
or borrowed) with the derived trait `NoCapability`; `AnySecret` (`keys::Secret`); `AnyCell`
(a cell in the value's own bytes: `Freeze`). As a bound, or `const _: () = assert!(none::<..>())`.

## 3. `where const { COND }` (built, `#![feature(blaze_comptime)]`)

A where predicate that is a `bool` constant over the generic parameters:

```rust
unsafe impl<T: ?Sized> Acyclic for T where const { none::<T, Cycle>() } {}
```

- **Syntax.** `const` then a block. No type starts with `const`, so rustc refuses it; the
  parser takes it in every crate and lowering refuses it without the feature.
- **Meaning.** It lowers to `ConstEvaluatable(c)`, `c` the block as an anonymous constant with
  the item's arguments (`GenericParams::where_consts`). The clause holds when `c` evaluates to
  `true` for the arguments it is asked with. `false`, a panic or any failed evaluation: it does
  not hold. Arguments naming a generic parameter: it does not hold unless the environment has
  the same condition (by identity or normal form, section 4), so generic code states the bound.
  An inference variable: undecided.
- **The reason.** A condition explains itself by panicking. The unmet bound is reported as
  E0277 of the bound (`Node: Acyclic`) with the panic's message as a note
  (`hir_ty::where_const::bound_refusal`, `hir::UnimplementedTrait::condition`). General: every
  reflection-derived trait gets it.
- **Evaluation, caching, cycles.** `anon_const_eval(constant, arguments)`, a memoized query:
  once per instance, keyed by both. A condition must not ask the bound it guards: the walk asks
  `Acyclic` only of a `dyn` type that is not its start (answered by the supertrait). One that
  does ask is a query cycle, which is a failed evaluation.
- **Coherence.** blaze has no general overlap check, and whether a conditioned impl applies is
  known only per type. The rule (`hir-ty-coherence`: `condition-not-marker`,
  `condition-elsewhere`): such an impl is of a `#[marker]` trait, in the trait's own crate. A
  marker trait has no items, so impls that both apply are indistinguishable and the solver
  merges them (RFC 1268): `unsafe impl Acyclic for X {}` stands beside the derived impl with no
  specialization. A trait with items is refused until blaze has the overlap check.

## 4. Generic const expressions (part built, `#![feature(blaze_const_exprs)]`, E16)

A const generic argument or array length may be an expression over generic parameters
(`Shares<T, K, { N + M }, D>`), integer or `bool` in this slice. Its own gate, not
`generic_const_exprs`, whose unfinished rustc rules blaze does not carry.

- **Representation.** An anonymous constant with its owner's generic arguments, substituted like
  any argument (`consteval::anon_const_is_dependent`).
- **Concrete: by evaluation.** With concrete arguments the solver normalizes it to its value.
- **Generic: by normal form** (`hir_ty::const_exprs`). An integer expression is a polynomial
  over atoms (parameters, named constants, other constants) with integer coefficients: `+`, `-`,
  `*` and literals; like monomials merged, zeros dropped. A `bool` is a comparison as its
  difference against zero (`a == b`, `a < b`, `a <= b`, with `>`, `>=`, `!=` rewritten), `!`,
  and `&&`/`||` flattened, sorted, deduplicated. Anything else (`/`, `%`, shifts, calls, casts)
  has no normal form and equals only itself. The arithmetic is over the integers: two
  expressions with one form agree wherever both evaluate.
- **Well-formedness.** An expression that overflows or panics at an instantiation is that
  instantiation's error. Built for a `where const` condition (the evaluator's message is the
  reason). **Not built:** the error for an argument in a type (today an unreported error constant).
- **Inference.** Nothing is solved for: `{ N + M }` against `3` is an ambiguity error.
- **Left:** a body's turbofish; `{ T::N + 1 }`; other types; a same-bytes-without-the-gate test.

## 5. The handles (written, `bstd::own`)

**`Shared<T>`, `LocalShared<T>`** are bstd's own types over `Arc` and `Rc`. For an ordinary `T`
nothing changes: `new`, `Clone`, drop. A linear `T` goes in through `Shared::holding(value)`,
in `impl<#[blaze::linear] T: Acyclic>`: the mark admits a linear type, the bound refuses one
that could own itself, with the path. Admission conditional on a property is thus the existing
per-impl mark plus an ordinary bound, with no new marker; the cost is a second constructor
name. (One `new` for both needs "is `T` linear" as a const fact,
`where const { !linear::<T>() || none::<T, Cycle>() }`: not built.) The handle is then linear:
`share()` copies it (`Clone`'s impl admits no linear `T`) and `release(self) -> Released<T>`
(`Last(T)` or `NotLast`) ends it; the caller finishes `T` in the `Last` arm. `release` is
`Arc::into_inner`, which gives the value to exactly one of the handles released together.

**`#[blaze::holds]`** (new, `hir-ty-linear`, linear.md rule 7) makes such a handle linear: on
a struct or enum it says a value holds a value of each type argument though no field does. The
type then holds a linear value when an argument does, and the bodies of its own crate's impls,
which move the contents by trusted code, are not checked for those arguments.

**`Shares<T, K, const N: usize, const D: usize>`**: `N` of the `D` parts of one `T`, a pointer
and nothing else, always linear.

```rust
fn new(token: Token<K>, value: T) -> Shares<T, K, D, D> where const { D >= 1 };   // T: Acyclic
fn split<const A: usize, const B: usize>(self) -> (Shares<T, K, A, D>, Shares<T, K, B, D>)
    where const { A >= 1 && B >= 1 && A + B == N };
fn join<const M: usize>(self, other: Shares<T, K, M, D>) -> Shares<T, K, { N + M }, D>;
fn whole(self) -> T where const { N == D };
```

- **Access.** Any part reads (`&T` by `Deref`); only the whole writes (`get_mut`) or takes it out.
- **No count, and sound**: `K` is a token's kind (`bstd::cell`), taken by value, so one kind
  is one allocation and `join` compares nothing (why `K` was added); the parts always add up
  to `D`; so the holder of `D` is the only handle.
- **No cycle**: `new` asks `T: Acyclic` and a `Shares` is a `Jointly` pointer to the walk; and
  a part stored anywhere is still a linear value, so the whole cannot be taken without it.
  When to use which is in [std](std.md), "Shared mutation".
