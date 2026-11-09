---
status: plan
summary: Relaxing the orphan rule soundly as an opt-in blaze extension: `#[blaze::sealed]` traits whose crate may write `impl<T: Sealed> ForeignTrait for T` (built, with its checks in `hir-ty-coherence`), and the ranked alternatives, including a whole-workspace coherence domain (designed, not built).
date: 2026-11-02
---
# Relaxing the orphan rule, soundly (`#[blaze::sealed]`)

Lanes `orphan` (design and first build), `orphan2` and `orphan3` (review fixes), 2026-11-02.
Rules that bind: AGENTS.md 3 (no-fallback), 4 (superset-of-rust: opt-in extensions), 16, 18, 20,
25, 26.

## Answer first

- The built slice is **sealed-trait blanket impls**: `#[blaze::sealed] trait S {}` in crate L
  lets L write `impl<T: S> ForeignTrait for T` (and any header where a parameter bounded by `S`
  stands where a local type would). It is sound per crate as long as the restrictions below
  hold, needs no re-check of any upstream crate, and changes nothing for code that does not
  write `#[blaze::sealed]`. It is Jules Bertholet's proposal (IRLO 22718) with scottmcm's
  condition (the trait must be sealed).
- blaze has **no overlap check (E0119) for ordinary impls** (`blaze-diagnostics`'
  `checks/impls.rs`: overlap is the front end's, never built). The slice builds the overlap
  primitive (`hir-ty-coherence`) and runs it on relaxed impls only. Because no crate checks
  E0119 against the impls of other crates, the slice refuses every relaxed impl whose
  expansion a downstream crate could overlap (an uncovered type parameter beside the sealed
  one, rule 5 below). E0119 for every crate is the precondition for lifting that, for rank
  1b and for phase 3.
- The whole-program "workspace coherence domain" is designed as phase 3. It is not sound per
  crate: upstream crates' negative reasoning and codegen have to be re-run with the downstream
  impls visible, a much larger change.

## The hazard (why "just turn the orphan check off" is unsound)

From the IRLO thread "Allow disabling orphan rules for applications" (steffahn's posts) and
rust-lang/rust#136979:

1. **Coherence negative reasoning upstream.** Crate `up` has `impl<T: Tr> Other for T` and
   `impl Other for Foo`; `up`'s overlap check accepts them because `Foo: Tr` cannot hold: `Tr`
   and `Foo` are both `up`'s, so by the orphan rule no other crate can add that impl
   (`trait_ref_is_knowable`). A downstream orphan `impl Tr for up::Foo` makes `Foo: Other`
   ambiguous between two impls that were each checked in a world without it.
2. **Codegen upstream.** `up` monomorphizes knowing `Foo: !Tr`: a vtable slot whose method has
   `where Self: Tr` gets a stub; std's internal specialization picks the general impl. The same
   instance built downstream picks the other one. Two answers for one instance.
3. **Auto-trait and marker reasoning.** Explicit impls of auto traits change auto-impl
   candidates for everything that contains the type (rustc's E0321).
4. **Associated types.** Two impls that both apply give two values for `<Foo as Tr>::Assoc`,
   a safe transmute (Boxy, "An Incoherent Rust", 2026-03-23).

The orphan rule's real job: every impl that could exist has exactly one crate that may write
it, and every crate that can name the trait-ref also sees that crate. A relaxation is sound
exactly when it keeps that property, or when blaze replaces it with a check run where all the
impls involved are visible and re-runs every result that assumed the property.

## Proposals, ranked (soundness first, then usefulness, then cost)

| rank | proposal | sound per crate? | what blaze needs |
|---|---|---|---|
| 1 | Sealed-trait blankets (IRLO 22718 + "sealed") | yes, with the restrictions below | `#[blaze::sealed]`, the checks below. **Built.** |
| 1b | Same, unsealed trait (downstream may implement `S` for its types) | yes, if every crate runs E0119 | E0119 for every crate first |
| 2 | `#[fundamental]` on one's own type (t-types' answer in #136979; RFC 1023) | yes (rustc's rule) | nothing: `#![feature(fundamental)]` works on the pinned nightly, and both orphan walks read `StructFlags::FUNDAMENTAL` |
| 3 | `#[fundamental]` traits (RFC 1023) | yes | not a relaxation: lets downstream coherence reason negatively; would make rank 1's overlap check more precise |
| 4 | Workspace/binary coherence domain (IRLO 19243, 22172; Boxy's coherence domains; #136979) | no | phase 3 below |
| 5 | RFC 3482 extern impls | no as written (the defining crate's negative reasoning and codegen are not re-run with the impl crate visible) | a special case of rank 4 |
| 6 | Binary-crate exemption | no (hazards 1, 2 upstream of the binary) | subsumed by rank 4 |
| 7 | `#[marker]` traits (RFC 1268) | yes, exists | not a relaxation |
| 8 | Crate-level where-clauses (babysteps 2022-04-17), named impls (Boxy 2026) | n/a | language design |

Prior art checked on GitHub (repositories about "orphan rule" pushed since 2024): no tool or
fork implements a sound relaxation; the hits are newtype and derive helpers. rustc has no flag
(rust-lang/rust#136979, closed 2026-06-02, pointed at `#[fundamental]` types). RFC 3482 is open.

## Rank 1: `#[blaze::sealed]`

### Surface

```rust
#![register_tool(blaze)]            // inert under rustc, like every blaze:: attribute
#[blaze::sealed]
pub trait Wire {}                    // in crate L

pub struct Packet; pub struct Frame<T>(T);
impl Wire for Packet {}
impl<T> Wire for Frame<T> {}

impl<T: Wire> serde::Serialize for T { .. }        // allowed: T counts as local
impl<T: Wire> From<T> for Vec<u8> { .. }           // allowed: T is the first local type
```

The opt-in is the attribute on the trait, written by the crate that owns it. The relaxation is
about the trait's own impls, which only its crate can write; the meaning is in the source; and
a published crate that uses it opts nobody else in: its dependents see a trait they cannot
implement, and every impl it enables names only L's own types.

### Rules (each a typed refusal naming the items and the rule, AGENTS.md rule 16)

1. **Sealed** (checked in every crate that names `S`, registry dependencies included: see
   "Where the checks run"; `check_impl` and `check_trait`): no impl
   of `S` outside L; no trait outside L has `S` anywhere in its supertrait closure, `where
   Self:` bounds included (`hir_ty::all_super_traits`). Otherwise `dyn Sub` would be an
   instance of `S` for a type of another crate, through the object candidate, with no impl:
   with `trait LSub: S` in L and `trait DSub: l::LSub` in D, `dyn DSub: S` holds. The refusal
   names the trait, `S` and the chain (`DSub: LSub: S`).
2. **Closed over local types** (`sealed_closure`, a tracked query): every impl of `S` in L has
   a self type local by the orphan walk (a local ADT or `dyn` of a local trait, possibly under
   `&`, `&mut`, `Box`, `Pin`), with no type parameter or alias under a `#[fundamental]` type
   (walked as rustc walks them, every argument of each but `Box`; `impl<Q> S for LPair<A, Q>`
   with a fundamental `LPair` is refused, since a downstream crate may write
   `impl F for LPair<A, DLocal>`), and `S` is not an auto trait. Otherwise the relaxation is
   unavailable: the impl gets E0117/E0210 and a refusal naming the impl of `S` that breaks
   closure. (`#[blaze::sealed]` used only to seal may still be implemented for foreign types.)
3. **Param-as-local** (hir-ty's `check_orphan_rules` and blaze-diagnostics' E0210 walk, one
   predicate, `hir_ty::sealed_local_bound`): an impl type parameter with a direct where-clause
   `T: S`, `S` sealed, defined in the impl's crate and closed, is a local type. Only direct
   bounds count. Of several sealed bounds on one parameter, the first closed one in
   where-clause order is the one used, by the orphan walks and by `sealed_relaxation` alike, so
   the order of the bounds decides nothing; only when none is closed is the first named.
4. **Foreign traits it may not relax**: auto traits, `#[fundamental]` traits, and the lang
   items whose impls the solver builds in or rustc checks one type at a time (`Sized` and its
   kin, `Unsize`, `Copy`, `Drop`, `Destruct`, `CoerceUnsized`, `DispatchFromDyn`, the `Fn*` and
   `AsyncFn*` traits, `Unpin`, `Freeze`, `Receiver`, `DerefPure`, the structural-eq markers,
   `ConstParamTy_`; the list is `not_relaxable`). The pinned nightly has no
   `UnsizedConstParamTy`. Negative relaxed impls are refused.
   **Sized**: every parameter of a relaxed impl with a sealed bound, the one the relaxation
   leans on and any other (rule 5 lets a sealed parameter stand uncovered), must be `Sized`
   (the default), wherever it stands (`T`, `Box<T>`, `&T`). Then no `dyn` type, of any crate,
   ever stands for a sealed parameter, so the object candidate never adds an instance the
   closure and the overlap check can't see. (A `dyn` type can still be among the types the
   impl is for at a covered position, `F<Vec<Box<dyn X>>>`, where no other crate may write an
   impl.) Without it on the other parameter, `impl<T: S, U: O + ?Sized> F<Box<U>> for T` in L
   and `trait DO: l::LO` (`LO: O`) with `impl F<Box<dyn DO>> for l::A` in D give two values of
   `<A as F<Box<dyn DO>>>::Out`. This backs up rule 1, and it also covers `dyn LSub` for L's
   own subtraits (with `trait LSub: S + F`, `dyn LSub` would implement `F` through both the
   object candidate and the blanket, rustc's E0371 for the expanded `impl F for dyn LSub`).
   **What counts as relaxed**: an impl leans on `S` only when the first input type the orphan
   rules decide by (under `&`, `Box`, `Pin`: the first local type or parameter) is the sealed
   parameter. `impl<T: S> From<T> for Local` is legal Rust already and is not checked further.
5. **No uncovered parameter beside the sealed one** (`hir_ty::relaxed_uncovered_input`): no
   input type of the trait-ref (self type included) may hold, under fundamental types, a type
   parameter without a closed sealed bound, or an alias the orphan walk can't see through.
   Fundamental types are walked as rustc's orphan checker walks them
   (`rustc_next_trait_solver::coherence`, `OrphanChecker::visit_ty`; `hir_ty`'s
   `any_fundamental_leaf`): `&` and `&mut`, `Box`'s first argument alone, and every argument
   of any other `#[fundamental]` ADT. `impl<T: S, U> F<U> for T` and
   `impl<T: S, U> F for Pair<T, U>` (a fundamental `Pair<X, Y>`) are refused;
   `impl<T: S, U> F<Vec<U>> for T`, `impl<T: S> F for Pair<T, A>` and
   `impl<T: S, U: S2> F<U> for T` are allowed. See "Why it is sound" below.
6. **No overlap**: the relaxed impl is checked in `TypingMode::Coherence` against every impl
   of its trait visible from L (L and the crates it loads, blanket and derive impls, and the
   `#[rustc_reservation_impl]` impls, which the solver's tables leave out and
   `hir_ty::method_resolution::reservation_impls` keeps for coherence). Overlap is a refusal
   (E0119) naming both impls. A specializing relaxed impl overlaps by definition and is
   refused.
7. **Sealed knowability** (solver): in coherence, a trait-ref of a sealed trait never has a
   downstream conflict (`trait_ref_is_knowable`): no crate but S's may implement it (rule 1),
   and no type of another crate implements it without an impl (rule 1's supertrait half, and
   the `Sized` rule for the relaxed impls themselves). Without this, `impl<T: S> Display for
   T` always overlaps std's `impl<T: Display + ?Sized> Display for &T`, since a downstream
   crate could otherwise implement `S` for `&DLocal`. The upstream conflict (a future impl by
   the trait's own crate) is unchanged. Vendored patches: `rustc_type_ir` 0013-sealed-traits
   (`Interner::trait_is_sealed`, default `false`), `rustc_next_trait_solver`
   0013-sealed-traits-knowable.

### Why it is sound (the expansion argument)

Under rules 1 to 5, the relaxed impl `impl<P, T: S> F<..T..> for X` means exactly the set of
impls obtained by replacing `T` with the self type of each impl of `S`:
`{ impl<P, Q_i> F<..S_i..> for X : impl<Q_i> S for S_i in L }`. No other type implements `S`:
rule 1 keeps every impl of `S` in L and every `dyn` of another crate's trait out of `S`, and
the `Sized` rule, on every sealed parameter, keeps every `dyn` type out of the place of a
sealed parameter. Each member passes
rustc's orphan check in L: rule 2 makes `S_i` local, and the walk puts `T` where a local type
is allowed, before no uncovered parameter. So the program is one rustc would accept if L had
written those impls by hand, provided no impl of another crate overlaps them. Which crates
could write such an impl?

- `F`'s crate, the crates of the types in `X`, and L itself: all are L's dependencies or L, so
  rule 6, checked in L, sees their impls.
- A crate D downstream of L: by the orphan rules D needs a type local to D among the input
  types, under fundamental types, before any uncovered parameter of its own. At the sealed
  position every member has an L type with no parameter under a fundamental type (rule 2, so
  D has no place in it to put its own). At every other position, rule 5, walking fundamental
  types as rustc does, leaves either a type with no parameter (fixed, from L's view, so not
  D's), a parameter under a type that is not fundamental (`Vec<U>`, which no D-local type
  unifies with), or a sealed parameter (L's types again, never a `dyn`: rule 4's `Sized`). So no impl D may write unifies with a member. Without rule 5, D could write `impl
  F<DLocal> for l::A` against L's `impl<T: S, U> F<U> for T`; rustc refuses that in D with
  E0119, and blaze has no E0119, so rule 5 refuses L's side until it has.
- A cousin crate (one that does not depend on L) can't name the `S_i`.

Hence:

- Hazard 1 (upstream negative reasoning): every member has an L-local type where the orphan
  rule requires one, so upstream coherence already allowed for it. No upstream result assumed
  it absent.
- Hazard 2 (upstream codegen): a crate that can name `S_i` depends on L and sees the impl.
- Hazard 3: rule 4.
- Hazard 4: rules 5 and 6 (no overlap anywhere, so one value per associated type).

The same downstream gap exists without the extension for a hand-written `impl<U> F<U> for A`
in L, since blaze has no E0119 in D: that is the existing missing E0119, not something
`#[blaze::sealed]` adds.

Composition: D can't seal `DS` and write `impl<T: DS> l::S for T`: rule 1 refuses any impl of
`S` outside L, blankets included, so relaxations don't chain across crates (they could under
1b).

### Where the checks run

Decision (lane `orphan3`): **sealing is an opt-in extension of the sealing crate.** It binds
every crate that can name the sealed trait, including a registry crate that rustc accepted
(rustc treats `#[blaze::sealed]` as inert). When such a crate implements `S`, or extends it as a
supertrait, refusing it is what the extension means; it does not break superset-of-rust,
because the program uses the extension through L. A graph with no `#[blaze::sealed]` anywhere
behaves exactly as before. The refusal names both crates (the one that wrote the impl or trait,
and L), the trait and the rule.

- A workspace member, a path package (a `[patch]`ed one under a registry crate included) and
  a dependency under `BLAZE_DEPS=full` are checked whole. hir's item collector runs
  `check_impl`, `check_trait` and the orphan check on every item, bodies' blocks included.
- A crate of which only what the build's programs reach is checked (std, and any registry or
  git dependency, `blaze-cargo`'s `reached_only`) otherwise runs no item check. For such a
  unit, `compile::reached_only_diags` runs the sealed checks alone when
  `hir_ty::sealed_in_graph(krate)` holds, whatever `BLAZE_DEPS` says. That flag is true when the
  crate or any crate it depends on, directly or not, declares a sealed trait. The checks are
  `hir::check::check_sealed` and `blaze_diagnostics::check_sealed`: `check_impl` on every impl,
  `check_trait` on every trait, and rustc's orphan check on every impl of another crate's trait
  (blaze-diagnostics' `trait_impl_orphan::in_sealed_graph`, lane `orphan4`). They cover the
  crate's modules and the blocks of every body. Their errors fail the unit, as any
  error-level diagnostic does in `assemble`. Lints are left out, as `--cap-lints` leaves them.
- Why every impl of a foreign trait, not only those with a sealed bound: the knowability rule
  (rule 7) and the expansion argument assume every crate that can name `S` obeys rustc's
  orphan rule, and a registry D that depends on a blaze-only L was never checked by rustc.
  With `impl<T: S> up::Conv for T { type Out = u64; }` in L, D's
  `impl up::Conv for l::A { type Out = &'static str; }` (E0117 under rustc) would give two
  values of `<l::A as up::Conv>::Out`. The walker is rustc's (Proper, then Compat for the
  error), never hir's stricter `check_orphan_rules`, so a registry crate rustc accepts is
  still accepted (`impl F for Pair<l::A, DLocal>` with a two-argument fundamental `Pair`
  included). Each error names the impl, its trait and crate, the rule and the crate that
  seals. Tests: blaze-cargo's `a_reached_only_orphan_impl_in_a_sealed_graph_is_refused` and
  `a_reached_only_crate_rustc_accepts_stays_accepted_in_a_sealed_graph`.
- Keys (rule 18): a reached-only unit's record key holds its crate's front-end identity
  (`blaze_lower::Lowered::identity`), which hashes each dependency's identity and so L's source.
  When L gains `#[blaze::sealed]`, D's key moves, so a record of D made while its graph had no
  sealed trait (no check, no refusal) is never served for the sealed graph; in a kept daemon,
  `sealed_in_graph` is a tracked query over the same source. Test:
  `a_dependency_gaining_sealed_moves_a_reached_only_dependents_key_and_checks_it_again`.
- Cost: `sealed_in_graph` and `declares_sealed` are tracked per crate. `declares_sealed` reads
  each module-level trait's `AttrFlags`, and the crate's impl table for relaxed impls in
  blocks. A crate with no sealed trait in its graph pays that memo read and nothing else. A
  crate in a sealed graph also lowers (does not type-check) every body, to find the traits
  and impls in its blocks. A text filter over bodies, like `Look` in `hir_def`'s `demand`
  module, would cut that down (follow-up).

### Where it lives (AGENTS.md rule 26)

- `hir-def`: `AttrFlags::BLAZE_SEALED`, `TraitFlags::SEALED` (the last bit of `TraitFlags`'s
  `u16`: the next flag widens it), `blaze::sealed` registered as a blaze tool attribute.
- `vendor/rustc_type_ir`, `vendor/rustc_next_trait_solver`: the knowability hook.
- `hir-ty` (`traits.rs`, beside `check_orphan_rules`): `sealed_bounds`, `sealed_local_bound`,
  `sealed_closure` (tracked), `sealed_relaxation`, `relaxed_uncovered_input`,
  `param_is_sized`, `any_fundamental_leaf` (rustc's walk of fundamental types),
  `declares_sealed` and `sealed_in_graph` (tracked); `method_resolution::reservation_impls`
  (tracked, per crate, empty without a walk unless the crate enables `rustc_attrs`) and
  `TraitImpls::impls`.
- `hir` (`check::check_sealed`, `DiagnosticsCollector::collect_sealed`), `blaze-diagnostics`
  (`check_sealed`) and `blaze-cargo` (`compile::reached_only_diags`): the sealed checks of a
  crate no other check covers.
- `hir-ty-coherence`: the overlap primitive (`overlapping_impl`, one inference context with a
  probe per candidate, under a `sealed_overlap` span) and `check_impl`/`check_trait`, returning
  `CoherenceError { rule, message, note }`. `check_impl` makes no text unless it refuses.
  `hir`'s diagnostics push them as it does specialization's.
- `blaze-diagnostics`' `trait_impl_orphan::Checker` asks `sealed_local_bound` for a type
  parameter, so its E0210 walk agrees with hir-ty.

### Caching (AGENTS.md rule 18)

- `orphan_rules_hold(impl)` is tracked; it reads the sealed trait's signature flags and,
  through `sealed_closure(S)`, L's impls of `S` (`TraitImpls::for_crate_and_trait`), so it
  reruns when either changes.
- The store fact `Fact::Coherence(impl)` (`blaze-items/src/facts.rs`, `write_coherence`)
  hashes the orphan result and the impls the impl could overlap. For an impl over a sealed
  bound it adds every impl of each sealed bound trait (`part(b"sealed bound")`), every impl of
  its own trait when it has a simplified self type (`part(b"relaxed")`), and for an impl of a
  sealed trait the flag (`part(b"sealed trait")`). Each part is written only when it applies,
  so no other impl's fact bytes change. These cover the orphan result. They do not cover
  `hir-ty-coherence`'s overlap answer, which is not stored through this fact: it also reads
  the impls of every trait named in a candidate's where clauses and the reservation impls. If
  the overlap answer is ever stored, its key needs those (the crate fact would cover them).
- Upstream results never depend on a relaxed impl (the expansion argument), so nothing upstream
  needs a new key.

## Phase 3: workspace coherence domain (designed, not built)

Declaration: `[workspace.metadata.blaze] coherence-domain = ["a", "b", "c"]` in the workspace
root manifest only, path members only. The key in a member's own manifest, or in any registry
or git package, is refused with an error (a published crate can never opt its dependents in).

Semantics: an impl in domain member Y passes the orphan check when its trait or its local key
type is defined in any domain member. To stay sound:

1. **Domain view for solving.** Every query that reasons about impls (the solver, coherence,
   method resolution, codegen's impl lookup, vtables, specialization) for a domain member runs
   with the domain root view: the impls of all domain members and their dependencies.
   `DbInterner::krate` becomes a `CoherenceView` (a crate, or a domain) and
   `TraitImpls::for_crate_and_deps` takes it. This removes hazard 2.
2. **Negative reasoning re-checked.** Each member's overlap check runs in the domain view
   (hazard 1).
3. **Whole-domain overlap check** at the root: E0119 across members.
4. **What stays refused** (typed errors naming both impls): an orphan impl whose trait and type
   are both from outside the domain; an orphan impl of an auto trait or a builtin-checked lang
   item; a domain member depended on by a crate outside the domain.
5. **Keys.** The view is part of every key of a query that reads impls; the domain list is an
   input whose digest goes into each member's unit key.

Cost: the `krate` to view change touches every `DbInterner::new_with` caller (a whole port,
AGENTS.md rule 21). Rank 1 needs none of it.

## Follow-ups (not built)

- E0119 for every crate (lifts rule 5; enables 1b; precondition for phase 3). Measure it
  against the corpus first: an imprecise overlap check refuses programs rustc accepts.
- Merge the two orphan walkers (hir-ty's and blaze-diagnostics') into one in hir-ty.
- Supertrait elaboration for rule 3 (`T: Sub` where `Sub: S`).
- Lower fewer bodies for the sealed check of a crate in a sealed graph: only those whose text
  may declare a trait or an impl (a `Look` variant in `hir_def`'s `demand`).
- The orphan check's own walk (`unwrap_fundamental`, both orphan walkers) still follows the
  first argument of a fundamental type alone, so blaze refuses `impl F for Pair<l::A, DLocal>`
  with E0117 where rustc accepts it (a superset-of-rust gap, not a soundness one). Porting it
  to rustc's walk is safe for the relaxation now: rule 5 and rule 2 already walk every
  argument, so D's impl can no longer overlap a relaxed impl's expansion.

## What is built

Rank 1, in `hir-def`, the two vendored solver patches, `hir-ty`, `hir-ty-coherence`, `hir`,
`blaze-diagnostics`, `blaze-items` and `blaze-cargo` (the sealed check of reached-only units),
as described above. Tests in
`crates/hir-ty-coherence/src/tests.rs` (fixtures `up`, optionally `l`, and `main`, over
minicore), each named for the property it pins:

- `closed_sealed_blanket_is_allowed` (also beside an upstream `impl<T: Show + ?Sized> Show for
  &T`, which needs the knowability patch);
- `overlapping_sealed_blanket_is_refused`, `overlap_with_a_reservation_impl_is_refused`;
- `upstream_negative_reasoning_is_refused` (steffahn's case);
- `sealed_trait_is_refused_outside_its_crate`, `sealed_supertrait_at_any_depth_is_refused`
  (the `DSub: LSub: S` program, `where Self:` included);
- `without_sealed_the_blanket_is_an_orphan`;
- `unsized_self_and_builtin_traits_are_refused`,
  `every_sealed_parameter_of_a_relaxed_impl_must_be_sized` (the round-2 reviewer's `U: O +
  ?Sized` program),
  `sized_sealed_blanket_under_a_fundamental_type_is_allowed`,
  `const_param_ty_is_not_relaxable`;
- `uncovered_param_beside_the_sealed_one_is_refused`;
- `sealed_bound_choice_does_not_depend_on_order`;
- `every_argument_of_a_fundamental_type_is_walked` (rule 5 and rule 2 over `Pair<X, Y>`).

In `crates/blaze-cargo/src/serve/unit/tests.rs`:
`a_reached_only_crate_that_implements_a_trait_sealed_upstream_is_refused`. It lowers three
registry crates as a build does. `d` implements and extends `l`'s sealed trait and gets both
refusals. `l` gets none. `plain`, with no sealed trait in its graph, gets no check.
