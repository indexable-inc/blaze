---
status: plan
summary: Sound specialization as an opt-in blaze extension, `#![blaze::specialization]` with `#[blaze::specialization_trait]`, built on rustc's always-applicable rule (min_specialization) without its unsafe markers; rustc's min_specialization checks are ported for std and the crates that enable the feature.
date: 2026-11-02
---
# Sound specialization (lane `spec`)

Specialization (RFC 1210, tracking issue rust#31844) has been unstable since 2015. The full
feature is unsound because codegen erases lifetimes: an impl that applies only when `'a ==
'static`, or when two lifetimes are equal (`impl<T> Tr for (T, T)`), can't be selected after
erasure, so typeck and codegen pick different impls (aturon, "Shipping specialization: a
story of soundness", 2017, with withoutboats' follow-up in the same internals thread). std
ships the sound core, `min_specialization`, behind a feature, plus an unsound escape hatch,
`#[rustc_unsafe_specialization_marker]`, used for `Copy`, `TrustedLen` and `FusedIterator`.

blaze must keep accepting std's `min_specialization` and `specialization` (superset-of-rust),
and it adds a sound subset that user crates can opt into.

## The rule: always applicable

From Niko Matsakis, "Maximally minimal specialization: always applicable impls" (2018), and
aturon, "Sound and ergonomic specialization for Rust" (2018). A specializing impl must be
*always applicable*: whenever its parent applies and its header unifies, it applies, whatever
the lifetimes are. rustc's `check_min_specialization` checks this
(`rustc_hir_analysis/src/impl_wf_check/min_specialization.rs`). blaze ports it as written:

- **parent**: the nearest impl it specializes. For an impl of an always-applicable trait
  that specializes no impl, the parent is the trait, and the impl may assume nothing. `#[marker]`
  traits are skipped.
- **parent's arguments**: equate the trait refs and prove the parent's where clauses. A
  parameter left unresolved is refused ("could not resolve generic parameters on overridden
  impl").
- **no empty specialization**: an impl that specializes another must define an item.
- **no `'static`**: among the parent's arguments not constrained by its own projections.
- **no repeated parameter**: in those same arguments.
- **only always-applicable bounds**: each of the impl's (elaborated) where clauses must meet
  one of these:
  - global;
  - one of the parent's;
  - needed for the header's types to be well-formed;
  - a bound on an always-applicable trait or one of its supertraits;
  - a `ConstArgHasType`.

  Anything else is refused: a trait bound, an associated type equality, an outlives bound.

blaze's subset, `Rules::Blaze`, is rustc's rule with three changes:

1. A bound on a `#[rustc_unsafe_specialization_marker]` trait is refused (`Rule::UnsafeMarker`).
   rustc allows it, and that is the known hole.
2. Only `default fn` is specialized. `default type`, `default const` and `default impl` are
   refused: projecting a `default type` needs the reveal rules the full feature never got
   right.
3. An impl of a `#[blaze::specialization_trait]` trait is checked as always applicable in every
   crate. The trait's author opted in, so a downstream crate without the extension can't add
   an impl that breaks the trait's guarantee.

In every mode that specializes (`Blaze`, `Rustc` and `Unchecked`), rustc's
`check_specialization_validity` (E0520) also applies. Walking up the impls an impl specializes,
the first one with the overridden item must mark it `default` or be a `default impl`, and one
without it passes the question up only if it is a `default impl`. The error is at the
overriding item and names both impls and the item.

Errors carry rustc's code where rustc gives one (E0520). The rest have none, as in rustc. The
blaze-only rules (`UnsafeMarker`, `DefaultItem`, `DefaultImpl`) say "blaze extension" in the
message, and `Rule::is_blaze_extension` reports them.

Every refusal names the impl, its parent and the rule. For example: `cannot specialize on trait
`Plain` (in `impl C for T`, which specializes `impl C for T`; rule: always applicable: a bound
the parent lacks is on an always-applicable trait)`.

## Spelling

```rust
#![blaze::specialization]          // crate root: this crate may specialize, under blaze's rules

#[blaze::specialization_trait]     // a trait an impl may specialize on; its impls are checked
trait Fast {}

impl<T> Pick for T { default fn pick(&self) -> i32 { 1 } }
impl<T: Fast> Pick for T { fn pick(&self) -> i32 { 2 } }
```

Both attributes are inert `blaze::` tool attributes. rustc refuses them, since the `blaze`
tool is not registered, so a crate that uses them builds only under blaze, which is what an
extension may do. `#![blaze::specialization]` overrides either feature. Under it, `default fn`
needs no `#![feature]` and gets no feature-gate warning.

## Modes

`hir_ty::specialization_mode(db, krate)`:

| crate says | mode | checked by | may specialize |
|---|---|---|---|
| `#![blaze::specialization]` | `Blaze` | blaze's subset | yes |
| `#![feature(min_specialization)]` | `Rustc` | rustc's rule, unsafe markers allowed | yes |
| `#![feature(specialization)]` only | `Unchecked` | nothing, as rustc | yes |
| neither | `Off` | impls of `#[blaze::specialization_trait]` traits only; an impl of a `#[rustc_specialization_trait]` trait is refused as unstable | no |

## Codegen picks the impl rustc picks

Neither selection nor codegen is new:

- The solver prefers a specializing candidate (`next_solver/infer/select.rs`, through
  `specializes`).
- An item is looked up on the nearest ancestor that defines it
  (`method_resolution::lookup_impl_assoc_item_for_trait_ref` → `specialized_ancestor`, rustc's
  `leaf_def`).
- `inline.rs` leaves a `default fn` with unsettled arguments alone.

The extension only widens `may_specialize` to the `Blaze` mode, so a `#![blaze::specialization]`
crate goes through the same path as a `min_specialization` one.
`hir-ty-mir`'s `blaze_specialization_calls_the_specialized_impl` evaluates such a program.
The `Specializes` fact in blaze-items hashes the mode in place of the two feature bits.

A program with neither the attribute nor a feature gets the same mode (`Off`) and the same
`may_specialize` (false) as before, so its selection, MIR and objects don't change. The
checker reports nothing for it unless it implements a `#[blaze::specialization_trait]` trait,
which it can only do by using the extension.

## Where it lives

- `crates/hir-ty-specialization` holds the checker (semantics-in-front-end, a crate of its
  own). `check_impl(db, impl)` is a plain function that `hir`'s `collect_impl` calls for each
  impl.
- hir-def has the attribute flags and the trait flags `SPECIALIZATION_ALWAYS_APPLICABLE`,
  `SPECIALIZATION_UNSAFE_MARKER` and `BLAZE_SPECIALIZATION_TRAIT`.
- hir-ty has the mode.

## Prior art

- RFC 1210 and rust#31844, the tracking issue. rust#48538 and rust#68970 are the soundness
  issues and the `min_specialization` PR.
- aturon, [lifetime dispatch](https://aturon.github.io/blog/2017/07/08/lifetime-dispatch/)
  (2017) and [sound specialization](http://aturon.github.io/tech/2018/04/05/sound-specialization/)
  (2018).
- withoutboats in [Shipping specialization: a story of
  soundness](https://internals.rust-lang.org/t/shipping-specialization-a-story-of-soundness/5507/17).
- Niko Matsakis, [always applicable
  impls](https://smallcultfollowing.com/babysteps/blog/2018/02/09/maximally-minimal-specialization-always-applicable-impls/).
- LWN, [the state of specialization](https://lwn.net/Articles/997444/) (2024).
- Libraries that emulate it on stable: `zheland/try-specialize` (`TypeId` and autoref tricks,
  limited to `'static` types) and `AldaronLau/specializer`. Both choose at run time or
  through a type-id comparison. blaze's extension is compile-time impl selection with rustc's
  semantics.

## Not done yet

- An unresolved region of the parent under rustc's rules is left a variable, not resolved
  lexically as rustc does. Only a bound naming it can then be refused where rustc accepts. Run
  the checker over std's crates to show there are no such false positives before this is
  current.
