# UI cases for the one call rule (D156)

Sixty rustc UI tests from `/Volumes/Projects/ref-rust-1115/tests/ui/{nll,regions,borrowck,lifetimes}`
(the pinned reference checkout, read only). They cover calls, closures, projections and `dyn`.
The verdict comes from the file's `//@` header: `check-pass`, `run-pass` or `build-pass` means
ACCEPT; no pass header (or `check-fail`) means REJECT, the compiletest default. "stderr" names the
expected-output file(s) next to the test, or `none`. These rows came from listing and grepping the
tree. None of the tests was run.

Paths are relative to `tests/ui/`.

## Calls: fn items, methods, operators, constructors

| # | Path | What it exercises | Verdict | stderr |
|---|---|---|---|---|
| 1 | regions/regions-infer-call.rs | calls `takes_two(&isize, &isize)` with two distinct caller lifetimes: fresh late-bound regions per call | accept (run-pass) | none |
| 2 | regions/regions-infer-call-3.rs | `select<'r>(&'r, &'r) -> &'r` called inside an `F: FnOnce(&isize) -> T` closure: the result escapes the binder | reject | regions-infer-call-3.stderr |
| 3 | regions/regions-early-bound-error-method.rs | a trait method `get(&self) -> &'a isize` with an early-bound `'a` from the trait, returned at a shorter lifetime | reject | .stderr |
| 4 | regions/regions-early-bound-error.rs | the same through a type parameter `T` in a bound (`&'a T`) | reject | .stderr |
| 5 | regions/regions-early-bound-used-in-bound-method.rs | the passing twin of 3: a trait's early-bound lifetime used correctly through a method | accept (run-pass) | none |
| 6 | regions/regions-free-region-ordering-callee.rs | `ordering1<'a, 'b>(x: &'a &'b usize) -> &'a usize`: implied `'b: 'a` from an argument type, and its violations | reject | .stderr |
| 7 | regions/regions-free-region-ordering-caller.rs | caller side of 6: building `&'a &'b` where `'b: 'a` does not hold | reject | .stderr |
| 8 | regions/region-lifetime-bounds-on-fns-where-clause.rs | `fn a<'a, 'b>(..) where 'b: 'a`: a callee's where-clause becomes a requirement at the call | reject | .stderr |
| 9 | regions/regions-lifetime-bounds-on-fns.rs | the same with an inline `'b: 'a` bound (early-bound by it) | reject | .stderr |
| 10 | regions/regions-bounded-method-type-parameters.rs | a method `<'y: 'x>` type-parameter region bound checked at the call | reject | .stderr |
| 11 | regions/regions-bounded-method-type-parameters-trait-bound.rs | `fn method<'y: 'x>(self, y: Inv<'y>)` on a trait: clause from the trait's method, invariant ADT | reject | .stderr |
| 12 | regions/regions-infer-invariance-due-to-mutability-3.rs | `Invariant<'r>` passed and returned as `Invariant<'static>`: ADT invariance by declaration | reject | .stderr |
| 13 | regions/regions-infer-covariance-due-to-decl.rs | a covariant ADT used in a contravariant position | reject | .stderr |
| 14 | regions/regions-infer-contravariance-due-to-ret.rs | `max<'r>(bi: &'r boxed_int, f: &'r isize)`: two arguments unify on one late-bound region | accept (run-pass) | none |
| 15 | regions/regions-link-fn-args.rs | `with<'a, F: FnOnce(&'a Vec<isize>) -> &'a Vec<isize>>`: early-bound `'a` through an `F: Fn` bound | accept (run-pass) | none |
| 16 | regions/regions-escape-method.rs | a method call whose type parameter would capture a region bound in the closure argument (`//@ run-rustfix`) | reject | .stderr (and .fixed) |
| 17 | lifetimes/lifetime-errors/ex2a-push-one-existing-name-early-bound.rs | `baz<'a, 'b, T>(x: &mut Vec<&'a T>, y: &T)`: `x.push(y)` through `&mut` invariance | reject | .stderr |
| 18 | lifetimes/lifetime-errors/ex3-both-anon-regions-using-fn-items.rs | `foo(x: fn(&u8, &u8), y: Vec<&u8>, z: &u8)`: a push between two anonymous regions | reject | .stderr |
| 19 | lifetimes/lifetime-errors/ex3-both-anon-regions-using-impl-items.rs | the same as an impl method `foo(x: &mut Vec<&u8>, y: &u8)` | reject | .stderr |
| 20 | borrowck/fn-item-check-type-params.rs | #104005: a fn item's type parameters' well-formedness (`T: 'static` hidden in an arg) checked at the call | reject (check-fail) | .stderr |
| 21 | borrowck/fn-item-check-trait-ref.rs | a method callable only for `'static` values through the impl's implied `T: 'static` | reject (check-fail) | .stderr |
| 22 | borrowck/assign-op-invariance-lifetime.rs | #52126: `+=` relates its LHS invariantly (an operator call through its trait method) | reject | .stderr |
| 23 | nll/user-annotations/method-call.rs | user-annotated method generics `x.method::<&'static u32>(..)` instantiated at the call | reject | .stderr |
| 24 | nll/user-annotations/method-ufcs-1.rs | the same via UFCS `<_ as Bazoom<_>>::method::<_>(..)` | reject | .stderr |
| 25 | nll/user-annotations/adt-tuple-struct-calls.rs | tuple-struct constructors as calls with annotated lifetimes (`SomeStruct::<&'static u32>(..)`) | reject | .stderr |
| 26 | regions/regions-infer-bound-from-trait.rs | `check_bound<'a, A: 'a>(x: Inv<'a>, a: A)`: a type-outlives clause met (or not) by bounds inherited through traits | reject | .stderr |

## Closures

| # | Path | What it exercises | Verdict | stderr |
|---|---|---|---|---|
| 27 | nll/closure-requirements/escape-argument-callee.rs | a closure stores its argument (anonymous `'a`) into a longer-lived `'b` spot: error inside the closure | reject | .stderr |
| 28 | nll/closure-requirements/escape-argument.rs | a closure's argument escapes to its caller's local | reject | .stderr |
| 29 | nll/closure-requirements/propagate-approximated-ref.rs | a relation between two bound regions in a closure's signature propagated to the owner's free regions | reject | .stderr |
| 30 | nll/closure-requirements/region-lbr1-does-outlive-lbr2-because-implied-bound.rs | `foo<'a, 'b>(x: &'a &'b u32) -> &'a u32`: the implied `'b: 'a` from the argument's type | accept (check-pass) | none |
| 31 | nll/closure-requirements/return-wrong-bound-region.rs | a closure returns its second argument where its signature says the first | reject | .stderr |
| 32 | nll/user-annotations/closure-sig.rs | #104477: a closure's annotated signature in a method on `Printer<'_, '_>` | accept (check-pass) | none |
| 33 | regions/closure-prop-issue-104477-case1.rs | closure propagation must not invent `'a: 'b` between two non-local upper bounds | accept (check-pass) | none |
| 34 | lifetimes/closure-must-return-bound-lifetime.rs | #52533: `impl for<'a> FnOnce(&'a u32, &u32) -> &'a u32` given a closure returning the second argument | reject | .stderr |
| 35 | borrowck/regions-escape-unboxed-closure.rs | `with_int(f: &mut dyn FnMut(&isize))`: a closure stores its higher-ranked argument outside | reject | .stderr |
| 36 | borrowck/regions-escape-bound-fn.rs | the same through `with_int<F>(f: F)` with an `F: FnOnce(Option<&isize>)` bound | reject | .stderr |
| 37 | regions/region-bound-on-closure-outlives-call.rs | `call_rec<F: FnMut(usize) -> usize>`: the closure called while borrowed by itself | reject | .stderr |
| 38 | nll/issue-112604-closure-output-normalize.rs | a closure whose output type is a GAT projection (`for<'a> Trait<Gat<'a> = ()>`) normalized | accept (check-pass; revisions current/next) | none |

## Projections and type-outlives clauses

| # | Path | What it exercises | Verdict | stderr |
|---|---|---|---|---|
| 39 | nll/ty-outlives/projection-one-region-closure.rs | `<T as Anything<'b>>::AssocType: 'a` with no bounds in the trait: met only via `T: 'a` and `'b: 'a` | reject | .stderr |
| 40 | nll/ty-outlives/projection-where-clause-env.rs | `<T as MyTrait<'a>>::Output: 'a` in the environment: no need to prove `T: 'a` | accept (check-pass) | none |
| 41 | nll/ty-outlives/projection-where-clause-none.rs | the twin of 40 without the where-clause: `T: 'a` is not known | reject | .stderr |
| 42 | nll/ty-outlives/projection-no-regions-fn.rs | `Box<dyn Anything + 'a>` from `x.next()` of a `T: Iterator`: `T::Item: 'a` needed | reject | .stderr |
| 43 | nll/ty-outlives/ty-param-fn.rs | `Box<T>` into `Box<Debug + 'a>`: `T: 'a` from a where-clause or not (edition 2015..2021) | reject | .stderr |
| 44 | nll/projection-return.rs | `fn foo() -> <() as Foo>::Bar`: a return type normalized through the impl | accept (check-pass) | none |
| 45 | nll/issue-61311-normalize.rs | #61311: `Self::Proj` in an impl method's argument normalized (ICE regression) | accept (check-pass) | none |
| 46 | nll/normalization-bounds-error.rs | `visit_seq<'d, 'a: 'd>() -> <&'a () as Visitor<'d>>::Value`: the impl's bound checked when normalizing (Deserializer/Visitor shape) | reject | .stderr |
| 47 | regions/regions-assoc-type-region-bound-in-trait-not-met.rs | #20890: a trait's associated type region bound not met by the impl's value | reject | .stderr |
| 48 | regions/regions-implied-bounds-projection-gap-1.rs | the projection gap: `T::Foo: 'x` does not give `T: 'x` | reject | .stderr |
| 49 | nll/user-annotations/normalization.rs | a `&'static` requirement visible only after normalizing `<() as Foo>::Out` | reject | .stderr |
| 50 | borrowck/implied-bound-from-normalized-arg.rs | `test<'a, T>(_: <T as Ref<'a>>::Assoc)`: implied bounds of the normalized argument `&'a T` | accept (check-pass; revisions current/next) | none |

## `dyn`, fn pointers, opaque types

| # | Path | What it exercises | Verdict | stderr |
|---|---|---|---|---|
| 51 | regions/region-object-lifetime-1.rs | `borrowed<'a>(&'a self) -> &'a ()` called on a `dyn` receiver, one lifetime throughout | accept (check-pass) | none |
| 52 | regions/region-object-lifetime-2.rs | the same with two distinct lifetimes: the object's method result outlives the borrow | reject | .stderr |
| 53 | regions/regions-close-object-into-object-2.rs | `g<'a, T: 'static>(v: Box<dyn A<T> + 'a>) -> Box<dyn X + 'static>`: the object's own region | reject | .stderr |
| 54 | regions/regions-close-param-into-object.rs | `p1<T>(v: T) -> Box<dyn X + 'static>` without `T: 'static` (four variants) | reject | .stderr |
| 55 | regions/region-object-lifetime-in-coercion.rs | `&[u8]` coerced into `Box<dyn Foo + 'static>` | reject | .stderr |
| 56 | nll/issue-52663-trait-object.rs | a method called on a `Box<dyn Foo>` built from a borrow of a local that dies | reject | .stderr |
| 57 | lifetimes/dyn-trait-function-pointer-53419.rs | #53419: a `dyn` over a fn pointer type with a higher-ranked binder | accept (check-pass) | none |
| 58 | regions/regions-fn-subtyping-return-static-fail.rs | fn pointer subtyping: `fn(&S) -> &S` given where a `'static` return is wanted | reject | .stderr |
| 59 | nll/relate_tys/hr-fn-aba-as-aaa.rs | higher-ranked fn pointer upcast `for<'a, 'b> fn(&'a, &'b) -> &'a` to `for<'a> fn(&'a, &'a) -> &'a` (`-Zno-leak-check`) | accept (check-pass) | none |
| 60 | nll/ty-outlives/impl-trait-captures.rs | `foo<'a, T>(x: &T) -> impl Foo<'a>`: an opaque's captures, the hidden type's region not captured | reject | .stderr |

Notes:
- Verdicts 59 and 57, and every other row whose types cross a binder that a call does not
  instantiate, are refused today by `Refused::HigherRanked`. They pass only once the flow has
  placeholder checks.
- Rows 2, 16, 34, 35 and 36 need the closure's binder kept as a binder when the closure is
  passed to an `F: Fn` bound. The rule instantiates only the binder of the value being CALLED.
- Rows 20 and 21 also need well-formedness of the instantiated signature (`T: 'static` implied
  by an argument type). That check is not in this contract.
