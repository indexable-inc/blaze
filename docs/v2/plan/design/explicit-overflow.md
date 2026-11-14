---
status: plan
summary: "blaze::implicit_overflow: integer + - * << >>, unary -, compound forms and value-losing casts must say what overflow means (checked_*, wrapping_*, saturating_*, overflowing_*, try_from) unless the reach prover shows they fit; one proof, the same in both profiles."
date: 2026-11-12
---
# Explicit overflow

```rust
fn total(a: u32, b: u32) -> u32 { a + b }                // refused: nothing bounds `a` or `b`
fn next(v: &[u32], i: usize) -> usize { if i < v.len() { i + 1 } else { 0 } } // fits
fn sum(a: u32, b: u32) -> Option<u32> { a.checked_add(b) } // says what overflow is
```

## Why

`a + b` panics on overflow with overflow checks and wraps without, so the same source has two
meanings and the profile picks one. Code written by LLMs uses plain operators everywhere. The
lint makes each operation either say which meaning it wants, or be one blaze proves cannot
overflow, so its meaning is the same in every profile.

## What is refused

Integer `+ - * << >>`, their compound forms, unary `-` of a signed integer, an `as` that can
lose an integer's value (narrower, signed and unsigned, a float to an integer), and calls whose
overflow is the profile's (`pow`, `abs`, `next_power_of_two`, integer `Iterator::sum` and
`product`) or a panic in every profile (`strict_*`; D29 bans panics in `blaze-std` and
`blaze-mem`). These are the explicit forms: `checked_*` (with `?` or a typed error),
`wrapping_*` and `Wrapping<T>`, `saturating_*`, `overflowing_*`, `TryFrom`.

`/` and `%` are not covered. A zero divisor, or `MIN / -1`, panics the same way in every
profile, so the meaning does not depend on the profile. `nopanic` and the prover already treat
them as panic sites. Floats are not covered either, except a float cast to an integer, which
the prover does not follow, so it is always refused.

## Design

One query, `hir_ty_pure::reach::overflow_of_body(def)`, runs the reach prover (the abstract
interpretation over runtime MIR behind `blaze::unreachable!()`) in a separate overflow mode.
It covers the body and each closure's body.

- **Assume-fits semantics.** An operation that is not shown to fit is recorded, and from then
  on it is taken to fit: a fresh value equal to the exact result. The optional overflow
  `Assert`s add nothing. So the profile with checks and the one without give the same answer:
  a body with nothing recorded overflows in neither.
- **Verdicts, not just yes or no.** Each site records what fact is missing: an operand nothing
  bounds, `b <= a` for an unsigned difference, the value the bounds let it reach, a shift
  amount below the bit width, `a != MIN`, a cast's range end. The message names that fact.
- **Shifts** are judged by their amount only: dropping high bits is defined in every profile.
- **Casts** are checked against the target's portable range: `usize` and `isize` count as
  32 bits on the target side, as `as_cast` counts them. The candidates come from `as_cast`'s
  classification, its one home. Where this lint is on and reports a cast, `as_cast` gives way
  there, so one cast has one finding.
- **Masks and remainders** bound their result (`x & 0xff`, `i % len`), so code that means to
  truncate passes once it says so.
- `u128` values above `i128::MAX` are outside the prover's range, so any `u128` operation is
  reported as such. Nothing is assumed about them.
- A body that does not lower, or whose proof does not settle in its budget, has every
  operation refused with that reason. There is no fallback.

**Rejected alternative:** lowering every body under both profiles and comparing the two.
That doubles MIR and proof work. It would still need a verdict per site, and wrap semantics
cannot be compared against panic semantics without the same assume-fits rule anyway.

**Cost against the floor.** The floor is one forward pass over the body's MIR plus a lookup
per operation. The query adds one fixpoint solve per body, and one more walk of the settled
states to record verdicts. It is memoised per definition and runs only where the lint is on
and the body has an operation. Not measured yet.

## Fixes

`blaze check --fix` makes a fix only where the rewrite is clear (machine-applicable):

- In a function returning `Option`, outside closures: `a + b` becomes `a.checked_add(b)?`, and
  likewise for `-`, `*` and unary `-`. `<<` and `>>` are fixed only when the amount is `u32`.
  `a += b` becomes `a = a.checked_add(b)?` when `a` is a local. A cast `x as T` becomes
  `T::try_from(x).ok()?`.
- In a function returning `Result<_, E>` where `E: From<TryFromIntError>`: a cast becomes
  `T::try_from(x)?`.

## Levels

The lint is in `blaze::edition` and allowed by default. In this workspace's ratchet it is
`allow`, and crates opt in. `blaze-std` and `blaze-mem` deny it at the crate root once
`bstd-fallible-alloc` lands and a stage-0 release knows the lint.

## Tests

- `crates/blaze-lints/src/blaze/implicit_overflow_tests.rs`: each operator and cast is found
  when unbounded and accepted when guarded; each explicit form is accepted; `strict_add`,
  `pow` and `abs` are found; macro arguments and closures are covered; the `--fix` output is
  checked. Every case runs under both profiles, and the results must match.
- `crates/hir-ty-pure/src/tests/reach_oracle.rs`: a Hegel oracle. If an operation is shown to
  fit, a run with overflow checks must not panic, and the proof must be the same in both
  profiles. A control plants a prover that takes every `+` to fit (`PLANT_ADD_ALWAYS_FITS`),
  and the oracle catches it.

## Not done

- Fixes for arithmetic in a `Result` function. That needs a typed overflow error the caller
  can take (`blaze_mem::size::Overflow` on `bstd-fallible-alloc`).
- A checked float-to-integer conversion in bstd, as the explicit form for float casts.
- `u128` values above `i128::MAX`.
- Models for `checked_*` results and for calls like `abs`, so that a result the prover has
  bounded keeps its bound.
- Async bodies (coroutines), which the prover refuses. Every operation in them is refused.
