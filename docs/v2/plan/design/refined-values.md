---
status: plan
summary: Correct by construction for values. Narrow integer types, `Int<LO, HI>` as a library over `where const`, `#[blaze::invariant(..)]` on private-field structs, and `requires`/`ensures` that the compiler proves at every site or refuses with the missing relation. One prover (hir-ty-pure's reach, linear facts), one deny lint (`blaze::refinement`), and std measures (`len`) with a table of how std calls change them.
date: 2026-11-13
---
# Refined values: proved or refused

Andrew, 2026-11-13: `struct Twice { x: i32 }` with `Twice::new(i32::MAX)` must not build. The
valid inputs are `0..=i32::MAX/2`, which is exactly `u30`. "Do your best version of this." On
refinements such as `Vec<T>` with `len(v) == n`: not today in general, yes in a restricted form.
That restricted form is "Measures and relations" below.

The rule: a value claim is stated once, on a type or a function. The compiler proves it at every
site that could break it, or it refuses that site. It never adds a check at run time in place of
a proof (rule 3, no-fallback). The run-time-checked contracts of
[contracts.md](contracts.md) are a different tier and keep their meaning.

## 1. Narrow integers come first

The cheapest refinement is a type. `#![blaze::arbitrary_ints]` ([arbitrary-ints.md](arbitrary-ints.md))
gives `u1..u128` and `i1..i128`. `Twice::new(x: u30)` needs no prover at all. `x as i32 * 2`
fits by interval arithmetic over the type, and the unused bits are a niche. Every refusal below
whose range is a power-of-two span suggests the narrow type in its note: "`x` must be in
`0..=1073741823`: take a `u30`". This is `advice` in `reach/obligations.rs`, over `narrow_type`.

## 2. `Int<LO, HI>`: a library, not a feature

A range that is not a power of two is a const-generic library over `where const`
([comptime.md](comptime.md)), with no language change (rule 32, library-over-language):

```rust
struct Int<const LO: i128, const HI: i128>(i128);
fn new<const LO: i128, const HI: i128, const V: i128>() -> Int<LO, HI> where const { LO <= V && V <= HI }
fn add<..>(a: Int<A, B>, b: Int<C, D>) -> Int<{ A + C }, { B + D }>
```

The type checker already evaluates the condition per instance, unifies `{ A + C }` by value or by
normal form, and makes an overflowing bound an error rather than a wrapped value. The fixture is
`ranged_integers_are_checked_in_the_types` in `crates/hir-ty-mir/src/tests/where_const.rs`. That
covers type-level ranges. For a value known only at run time, the type is still `Int`, and its
constructor is the `#[blaze::invariant]` form of §3. `blaze_std` ships it once the two have run
together on real code.

## 3. `#[blaze::invariant(PRED)]`

On a struct declared at module level whose fields are all private (`pub(self)`). `PRED` reads
`self.f`, `self.f.len()`, integer literals, `iN::MAX`/`MIN`, `+ - *const /const`, the six
comparisons, `&& || !`, `true` and `false`. It is read once per struct (`refined::invariant`, a
tracked query). It is refused at the type, unwaivably, when:

- the item is an enum or a union, or the struct is declared in a block;
- a field is not private: "`Open.x` is not private, so code outside `mod a` sets it and the
  invariant is not kept";
- the predicate is outside the fragment: "not checkable: `#[blaze::invariant(self.x * self.y <
  10)]`", with the note "state it as a narrower type or a simpler bound".

The parts that the field types already imply (`self.small <= 255` on a `u8`) are set aside and
need no proof.

**Where it is checked.** Privacy makes the module the only writer. Inside the module:

- every `S { .. }` must show the predicate over the values it is built from;
- a write to a field, or a `&mut` to one, is allowed only through a `&mut S` parameter, and the
  predicate is shown again at every return of that function;
- a call that passes such a `&mut S` on must show it first, because the callee assumes it.

## 4. `requires` and `ensures`

This is blaze's native clause syntax ([contracts.md](contracts.md)), behind
`#![feature(blaze_verify)]`: `fn at(v: &Vec<u32>, i: usize) -> u32 requires i < v.len()`. Under
`blaze::refinement`:

- every call site must show the callee's `requires`, or it is refused with "this call of `at`
  needs `requires i < v.len()`";
- every return must show `ensures` over the parameters' entry values and the named result;
- the body assumes `requires`, and the caller assumes `ensures` once the call returns.

## 5. What one invariant buys

Every function that takes an `S` (by value, `&` or `&mut`) assumes the predicate at entry. Every
call that returns an `S` gives it to the caller. So the facts flow without being restated. In
`fn doubled(t: &Twice) -> i32 { t.x * 2 }` the overflow check is shown never to fail
(`tests/refined.rs`). The same goes for `cap - len` under `len <= cap`, and for indexing under a
length relation. Today these facts feed only lint verdicts: `blaze::refinement`,
`blaze::unreachable` and the overflow report. Removing the codegen checks they discharge is the
next step after the gate has run on real projects.

## 6. The fragment and its cost

The prover is hir-ty-pure's reach: a forward abstract interpreter over MIR. Its facts are linear
inequalities over hash-consed terms (locals, fields, `len` measures, `MutRef` epochs). It does
join at merges and widen at loop heads, and its implication query `lin_le` searches to depth 4
with fuel 256.

- **Decidable fragment.** Each clause is a boolean combination of comparisons between linear
  forms with constant coefficients, over integer locals, fields and measures. A clause outside
  the fragment is refused where it is written, never assumed.
- **Domain choice.** Linear facts with bounded search are more general than zones (`x - y <= c`)
  and octagons (`±x ± y <= c`). Those two are closed under transitive closure in O(n³) per merge,
  and so are octagon's widen and join. Reach pays only for the facts a site asks about: depth 4,
  fuel 256, terms hash-consed. So the cost is linear in the number of statements times the
  bounded search, not cubic in the number of variables. A full closure is the fallback design if
  the bounded search misses on real code. It would be per function, and n is the locals that
  appear in a clause.
- **Join and widen.** A join keeps a fact only if it holds on both sides. A widen at a loop head
  keeps the facts the body preserves, so `i < v.len()` survives a `for i in 0..v.len()` loop,
  while a counter's upper bound is dropped unless a guard restates it.
- **The missing relation is named.** A refusal says which clause was needed, the value as
  written, and where the facts that are known come from: "`x` must be in `0..=1073741823`, and
  is shown only in `0..=2147483647`".
- **Granularity.** The verdicts are a per-body query (`refined::of_body`). The invariant and the
  contract are per-item queries keyed by their own attribute and clause text (rule 65,
  granular-caching). Editing one body re-proves only that body and its direct callers' sites.

## 7. Opt-in and compatibility

`refined::in_play(krate)` is false unless the crate or one of its dependencies enables
`blaze_verify` or declares an invariant. A crate that does neither runs no part of this, and its
output is byte-identical. A program without the attribute or the clauses is unchanged (rule 4,
superset-of-rust). `#[blaze::invariant]` is an inert attribute word (`attr_resolution.rs`), so it
expands to nothing and changes no item. The lint is
`deny`, and `#[allow]` on it is itself refused (`check_waiver`). A refinement that can be waived
proves nothing.

**Not covered.** `unsafe` code can break an invariant by writing through a raw pointer. It is
trusted, and that trust is listed beside the other trusted items in
[formal-verification.md](formal-verification.md).

## Measures and relations

Andrew's `Vec<T>` with `len(v) == n` comes in a restricted form. The predicate is the same
linear fragment, and each type gets one measure, `len`.

**The std measure table** (`reach/models.rs`). Each row maps a call to its effect on `len` and
sets the receiver's new epoch:

| call | effect |
|---|---|
| `Vec::new`, `String::new`, `with_capacity` | `len == 0` |
| `vec![x; n]` (`alloc::vec::from_elem`) | `len == n` |
| `push`, `extend_from_slice(s)` | `len' == len + 1`, `len' == len + s.len()`; `len' >= 1` |
| `pop` | `len == 0` ⇒ `None`, else `Some` and `len' == len - 1` |
| `clear`, `truncate(n)` | `len' == 0`, `len' == min(len, n)` |
| `len`, `is_empty`, `Index` | read the measure; `Index` requires `i < len` |

**The relational domain.** Facts can relate measures across values: `self.starts.len() ==
self.ends.len()`, or `i < v.len()`. Cubic closure is avoided as described in §6.

**Kept fields.** After `(*p).f = v`, every other field of `*p`, and its measure, keeps its old
value under the new epoch. A method that changes one of two related vectors is therefore refused
at its return: "this return gives `self` back, and the `Spans` it leads to needs
`self.starts.len() == self.ends.len()`".

**Not yet:**

- tracking a push to a field through `&mut self` as a field place, so a method that pushes to
  both fields is refused too today;
- facts behind a `&mut` parameter are lost across an unrelated call;
- closures, nested fields, `Option<S>` niches;
- `split_at`, range slices and `collect`.

**Proved versus checked.** A `requires` here is proved at the call, or the call does not build.
A contract of [contracts.md](contracts.md) in the dev profile is checked when the program runs,
and a property test samples it. The same clause text serves both tiers. The tier is chosen by the
crate (`blaze_verify` with the lint), not by the clause.

## Next steps

1. Run the gate on reth, tempo and blaze's own crates. Count refusals by class, then fix the
   prover or the model table, never the program.
2. Track `&mut self` field places, and keep the facts of a `&mut` parameter across calls whose
   effect summary shows they do not touch it.
3. Drop the overflow, bounds and `unwrap` checks that a proved fact discharges, in codegen. That
   needs a translation-validation obligation ([formal-verification.md](formal-verification.md)).
4. Merge `refined::Clause` with `hir-ty-contracts/clause.rs` (rule 30, one-concept-one-home), and
   read nightly's `core::contracts` attributes as the rustc-compatible spelling.
5. Generate property tests from invariants for the cases where the prover refuses.
6. Add an `AttrFlags` bit for "has an invariant", so `in_play` does not walk every struct's
   attributes.
