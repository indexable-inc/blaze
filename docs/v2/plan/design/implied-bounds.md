---
status: plan
summary: "Two opt-in gates. blaze_implied_bounds: the bounds a type or trait declares hold in every function, impl and trait whose header names it (struct C<A: X> gives impl<A> C<A> the bound A: X). blaze_anon_params: a `_` type argument in a function's parameter types or an impl's header is a parameter of its own, bounded by its position (fn len<T>(v: &Vec<T, _>)); it turns on implied bounds."
date: 2026-11-13
---
# Implied bounds and anonymous parameters

```rust
#![feature(blaze_anon_params)]
struct Pool<T, A: Allocator> { .. }
impl<T> Pool<T, _> { fn alloc_id(&self) -> u32 { .. } } // `_: Allocator`, from `Pool`
fn len<T>(v: &Vec<T, _>) -> usize { v.len() }            // any allocator
```

Rust makes every user of `struct C<A: X>` write `A: X` again (RFC 2089, implied bounds, was
accepted and never shipped). The bound is said once where it belongs and copied everywhere,
and a `Vec<T, A>` argument needs a named `A: Allocator` parameter for an allocator no one
cares about. Both gates are opt-in (rule `superset-of-rust`); with neither, no program
changes and every diagnostic is as before.

## Implied bounds: `#![feature(blaze_implied_bounds)]`

**The rule.** Each predicate declared on the definition of a type or trait named in an item's
header is added to the item's where clauses, with the definition's parameters replaced by the
written arguments.

- **The header** is an impl's self type and trait, a function's parameter and return types (a
  method also has its impl's, as its parent), and a trait's supertraits. Field types, bodies,
  where clauses and associated type values are not part of it.
- **What is named** is found at every depth of those types: each struct, enum and union, each
  `dyn Tr<..>` principal and each projection's trait (`<T as Tr<X>>::A`), and the trait and
  supertraits themselves. A trait's parameter bounds (`trait T<X: Y>`) hold wherever `T<..>`
  is named in a header.
- **Only declared predicates count**: what a definition writes (its bounds, where clauses,
  implicit `Sized`, `where const`), never what it is itself given by this rule. They are then
  elaborated through supertraits as any where clause is.
- **Which predicates.** One whose subject is a type parameter or a projection rooted at one
  (`T: X`, `T::A: X`, `T: 'a`), or a lifetime or `where const` condition over parameters. Any
  other (`C<u8>` with `u8: X`) stays what it is today, an obligation the header's
  well-formedness proves. A predicate under a late-bound lifetime of a function is left out:
  a where clause would make that lifetime early-bound, which changes the function's type.
- **The gate** is the crate's that owns the item that gets the bounds. A dependency's
  definitions, gated or not, give their bounds to a gated crate's items; a gated crate's give
  none to an ungated crate's.
- A caller of the item proves the implied predicates as it proves written ones, refused with
  the same E0277, pointing at the bound where the definition declares it.

**The short form.** `A::Item` reads the bounds the header implies on `A` as well as the
written ones: `fn sum<I>(it: Wrapper<I>) -> I::Item` with `struct Wrapper<I: Iterator>`, and
the same in an impl's methods through `impl<I> Wrapper<I>`. The header contains the shorthand,
so it is read as written, not lowered: name resolution alone finds each struct, enum, union or
trait that names `A` as an argument, then that definition's declared trait bounds on that
parameter. Only a bound that names the definition's other parameters lowers the header's
arguments for them. Two implied bounds whose traits each define `Item` are E0221, as two
written ones are.

**Not given.** A shorthand projection inside the header (`A::Out`) is skipped when the
header is searched for implied bounds, because which trait it projects from is itself a
shorthand resolution. So a bound that only its trait declares (`trait Tr<B: Iterator>`,
reached through `A::Out`) is implied for everything except another shorthand: `B::Item`
there is E0220 "not found", never a silent accept. The same holds for a type alias in the
header (`type Al<B> = W<B>`): the header as written names the alias, not `W`. The lint
`blaze::implied_bound_written` asks the same header reading before it reports a bound, so it
never suggests removing one that a shorthand needs. A bound whose other arguments are written
with the same shorthand is a cycle: `fn f<A>(_: C<A, A::Out>)` with `struct C<A: Tr<B>, B>`
needs `A::Out` to lower `C`'s `B`, which needs `A`'s bounds, so it is E0391 naming `A`, as
rustc's `fn f<A: Tr<A::Out>>` is.

## Anonymous parameters: `#![feature(blaze_anon_params)]`

It turns on implied bounds: a `_` is bounded by nothing else.

**The rule.** A `_` in type-argument position in a function's parameter types, or in an
impl's self type or trait, is a fresh type parameter no one names, as argument-position
`impl Trait` is. Its bounds are what its position implies.

- Each `_` is its own parameter: `fn ids(a: &V<u8, _>, b: &V<u8, _>)` takes two allocators.
- They come after the named parameters; explicit arguments (`first::<u8>(..)`) give only the
  named ones, as with `impl Trait`.
- `x: _`, a whole parameter type, is a parameter bounded by `Sized` alone: `impl Sized`. It is
  accepted, since it is sound as written.
- It displays as `_`; nothing downstream reads a parameter's name (an `impl Trait`
  argument's has none either).

**Refused**, each E0121 with a note naming the rule and the position:

- in a return type: the function chooses the type, not the caller; write it or `impl Trait`;
- in a field type: a type's parameters are its interface;
- in a const's or a static's type: they have no parameters;
- as a const argument (`N<_>` where `N`'s parameter is a `const`): the natural next step is an
  anonymous const parameter, bounded by its type;
- anywhere else (a where clause, a bound, a type alias, a trait's items).

## Soundness

An implied predicate is one the header's well-formedness already requires: every use of the
item names those types, so every caller proves exactly what the item now assumes. An
anonymous parameter is an ordinary parameter, with its bounds proved at each use as any is.

## What opting in changes

- Programs that were refused for a missing bound the header implies are accepted.
- A function or impl gets more where clauses: a body can call through them, and type
  inference in a body may pick a where clause where it picked an impl before (as when the
  bound is written).
- A parameter `T: ?Sized` named as `C<T>` with `C<A>` (implicitly `A: Sized`) becomes
  `Sized`, as `C<T>` already required.
- `_` in a function's parameter types or an impl's header is no longer E0121.
- The lint `blaze::implied_bound_written` reports a bound written again that the header
  implies (docs/lints.md).

## Built in

- `hir-def`: the gates (`unstable_features.rs`, `UnstableFeatures::implied_bounds`);
  `TypeParamProvenance::Anonymous` (`hir/generics.rs`, persisted in
  `deps/signatures/codes.rs`); a `_` lowered as a parameter (`expr_store/lower.rs`
  `lower_type_ref`'s `InferType` arm, `lower_impl`; `expr_store/lower/generics.rs`
  `ImplTraitLowering::anonymous`, `ImplHeader`, `collect_impl_header`).
- `hir-ty`: `lower/implied.rs` (the rule; `shorthand_bounds`, the header read as written for
  the short form); `lower.rs` `lower_generic_predicates_with` (the clauses added after the
  written ones), `resolve_type_param_assoc_type_shorthand_in` (implied bounds after written),
  `declared_trait_predicates`, `PredicateSource::Implied`, `written_implied_predicates`;
  `lower/path.rs`
  `type_looks_like_const` (a `_` where a const goes stays E0121).
- `hir`: `bound_source` (an implied predicate points at its declaration),
  `GenericDef::written_implied_bounds`.
- `blaze-diagnostics`: `lower/infer_vars_not_allowed.rs` `anon_params_note`.
- `blaze-lints`: `blaze/implied_bound_written.rs`.
- `library/core/src/blaze.rs`: `ImpliedBounds`, `AnonParams`, which carry the feature names.
