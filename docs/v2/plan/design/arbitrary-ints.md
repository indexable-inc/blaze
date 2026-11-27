---
status: plan
summary: Integers of any width, `u1`..`u128` and `i1`..`i128`, as Zig has them, behind the crate attribute `#![blaze::arbitrary_ints]`. They are real primitive types, shadowed like `u8`, stored in the smallest power-of-two container with the unused high bits as a niche, overflow at their width, and get their trait impls from blanket impls in core over a solver-builtin marker trait. Built on the new layer - names, core's impls, the cast intrinsic. Not built - the checker's type, the solver's marker, layout, literals and lowering.
date: 2026-11-02
---
# Integers of any width (`#![blaze::arbitrary_ints]`)

A blaze extension ([no-rustc.md](../../../no-rustc.md#what-counts-as-running-rustc), rule
4 superset-of-rust): a crate that says `#![blaze::arbitrary_ints]` gets the integer types
`u1`..`u128` and `i1`..`i128`, as in Zig. A crate that doesn't say it is unchanged, including
one that defines its own `struct u3` or `mod i7`.
## What is built, what is not

| Piece | State |
|---|---|
| The opt-in, read at the crate root | built: crates/blaze-surface/src/crate_root.rs `arbitrary_ints` |
| `u3` as a builtin name, shadowed like `u8` | built: crates/blaze-scope/src/builtin.rs `builtin_type_in` (`Builtin::Int { signed, bits }`) |
| core's traits and inherent items | built: library/core/src/num/arbitrary.rs (`ArbitraryInt`, `IntFrom`, `IntTryFrom`) |
| `as` between any two widths | built: `blaze_int_cast` in crates/blaze-build/src/intrinsics.rs and crates/blaze-consteval/src/machine.rs |
| LIR keeps a width in its storage integer | stated: crates/blaze-lir/src/ty.rs |
| A pattern type's exact bridge to a width (`u32 is 0..=1073741823` is `u30`) | built: crates/blaze-ty/src/pat_ty.rs |
| A type for `u3` in the type checker | not built: front2 refuses it as `NoCheckerType` (crates/blaze-ladder/src/front2/types.rs) |
| The solver's builtin `ArbitraryInt`, `IntFrom`, `IntTryFrom` | not built: listed as not ported in crates/blaze-solve/src/builtin.rs |
| Layout and niche, literals, the operators' normalization and checks | not built |

## The opt-in

`#![blaze::arbitrary_ints]` at the crate root. Why a crate attribute and not a manifest key or
a feature:

- It is in the source, so the crate means the same thing however it is built (cargo, blaze, a
  vendored copy). A manifest key would make the meaning of `u3` depend on a file next to it.
- The crate root's record holds it, next to `no_std` and `recursion_limit`, so no other task
  has to read the manifest.
- It is per crate, like `#![blaze::sans_io]`. A dependency's choice never leaks into its
  dependents: a non-opted crate can hold a `u3` it got from an opted one, but it can't name
  the type or write `5u3`.

## Representation

An arbitrary width is an integer type of its own kind, not a new kind of type: everything that
is true of an integer (casts, builtin binops, int-var unification, the impl index) stays true
without new arms. The width is 1..=127 excluding 8, 16, 32 and 64; the standard widths keep
their own types, so `u8` is never "width 8" and every type has exactly one spelling.

- The value width is N (3 for `u3`).
- The storage is the standard type it is held in (`u3` in a `u8`, `i24` in an `i32`, `u65` in
  a `u128`).
- A key or record names a standard type by its old code and an arbitrary width by 128 plus its
  width, so the bytes for the standard types do not change.

**Invariant.** A value of `uN` is held in its storage integer zero-extended from N bits, and a
value of `iN` is held sign-extended from N bits. Every producer of a value (an operator, a
cast, an intrinsic, a literal, a constant) leaves it in that form, and every consumer may
assume it. Comparisons, bitwise and/or/xor, `Shr`, `Not` of a signed value, widening casts and
moves need no extra work under this invariant. Everything else ends with **normalize to N**:
`x & (2^N - 1)` for unsigned, `(x << (S - N)) >> (S - N)` (arithmetic) for signed, where S is
the storage width.

## Layout (not built)

The storage integer with a valid range equal to the width's values. Unsigned is
`0..=2^N - 1`. Signed is the wrapping range from MIN's sign-extended pattern up to MAX; `i3` is
`0xFC..=0x03`. So:

- The size is the smallest power-of-two byte count that holds N bits: `u3` is 1 byte, `u24`
  is 4, `u65` is 16. The alignment is the storage integer's ABI alignment, which equals the
  size on every 64-bit target blaze builds for.
- The unused high bits are a niche, exactly like `bool`'s, so `Option<u7>` is 1 byte and
  `Option<u1>` is 1 byte.
- Inside `#[blaze::bits]` ([bits.md](bits.md)), a field takes exactly N bits.

## Names and literals

- `u3` resolves where the builtin types resolve, so an item, import, generic parameter or
  module named `u3` shadows it exactly as it would shadow `u8`. It resolves only in a crate
  with the opt-in. `u` or `i` followed by 1 to 127 written without a leading zero is accepted,
  and nothing that is a standard width (built: `builtin_type_in`).
- `5u3` is read as a `u3` literal only under the opt-in. Elsewhere the suffix is what it is
  under rustc (not built).
- An unsuffixed literal infers to `u3` by unification with an expected type. An integer
  variable never becomes an arbitrary width through trait selection alone: the impl lookup
  for an int var lists only the 12 standard kinds, and the solver's builtin `ArbitraryInt` is
  `NoSolution` for an int var. So `1 + 2` and `x.into()` infer exactly as before in every
  crate.
- **Out of range.** A literal that does not fit its arbitrary-width type (`9u3`, `-5i3`,
  `0b1000_u3`) is a hard error naming the literal as written, the type and its range:
  ``literal `9u3` out of range for `u3` `` with the note `` `u3` holds `0..=7` ``. It is
  not the `overflowing_literals` lint: no `allow` turns it off, because no existing program
  depends on a `u3` literal wrapping. A standard-width literal is linted as before.

## Operators, overflow and casts

The setting is the crate's `overflow_checks`. `#[rustc_inherit_overflow_checks]` is honored. A
`const` item's own body is always checked, and a `const fn` follows its crate's setting.

| Operation | Checks on | Checks off |
|---|---|---|
| `+ - *` | panics if the result leaves N bits | wraps: normalize to N |
| `/ %` | panics on zero and on `iN::MIN / -1` or `% -1` | the same: like rustc's, these checks don't depend on the setting |
| unary `-` (signed) | panics on `iN::MIN` | wraps to MIN |
| `<< >>` | panics if the amount, read as an unsigned number of its own type's value width, is ≥ N | shifts by that amount mod N, then normalizes (`Shl`) |
| `! & \| ^`, comparisons | never overflow | the same |
| `as` to an int | truncates to the destination width and sign- or zero-extends from it | the same |
| `as` from a float | saturates at the N-bit MIN/MAX, NaN gives 0 | the same |

Mod N for shifts generalizes rustc's `& (BITS - 1)`, which is mod BITS for power-of-two
widths. It is the one rule the constant evaluator and the code builder both implement.

The overflow flag of `AddWithOverflow`/`SubWithOverflow` is `raw != normalize(raw)`: two
N-bit operands of `+` or `-` never overflow the container. For `*` it is that, or-ed with the
container's own overflow flag, since an N-bit product needs 2N bits.

A panic names the operation and the type, in the same words at compile time and at run time:
``attempt to {add, subtract, multiply, divide, calculate the remainder, shift left, shift right}
with overflow at type `u3` ``, ``attempt to negate with overflow at type `i3` ``,
``attempt to divide by zero at type `u3` `` and
``attempt to calculate the remainder with a divisor of zero at type `u3` ``. A standard type
keeps today's messages.

The shift amount is read at its own type's value width wherever the amount is an arbitrary
width, also when the shifted value is a standard integer: `x << y` with `y: i3` equal to -1
shifts by 7 (mod the width of `x` when unchecked).

## Backends agree (not built)

- **Constant evaluation** (blaze-body-eval over blaze-consteval's memory). For an arbitrary
  width, every integer binop, unop, cast and integer intrinsic computes on the operands as
  i128/u128, checks the result against the N-bit range, and writes it back normalized to the
  storage size.
- **Code** (blaze-body-build to LIR). The normalization is emitted once, after each operation
  the table above marks, on the storage width's operations; LIR needs no new type.
- The tests below run each operation at compile time and at run time, at widths 3 and 7, with
  checks on and off, and compare.

## Trait impls and inherent items

The trait impls exist once, in core, not once per width (built: library/core/src/num/arbitrary.rs).
Writing them per width means 246 types times about 40 traits, and a macro still makes those
impls exist. Instead:

- **`ArbitraryInt`**, a sealed lang-item trait in core (`#[lang = "arbitrary_int"]`). The
  solver answers it as a builtin: `Yes` for an arbitrary-width integer, `NoSolution` for every
  other rigid type and for an int var, and ambiguous for a type var. It can't be implemented by
  hand. blaze-solve does not answer it yet.
- **Blanket impls written once over it.** `impl<T: ArbitraryInt> Add for T` and the other
  operator traits (with their assign forms and the `&T` forms), `PartialEq`, `Eq`,
  `PartialOrd`, `Ord`, `Hash`, `Debug`, `Display`, `LowerHex`, `UpperHex`, `Binary`, `Octal`,
  `Default`, `Clone` (with `TrivialClone`). `Copy` is the solver's builtin instead, as for the
  standard integers, since core cannot write `impl<T: ArbitraryInt> Copy for T` (E0206).
  Their bodies are ordinary Rust over core's generic intrinsics
  (`add_with_overflow`, `wrapping_*`, `saturating_*`, `ctpop`, `ctlz`, `cttz`,
  `three_way_compare`, and `blaze_int_cast::<S, D>`, in blaze's own section of the intrinsic table, that is `as` between two integer types).
  So the impls for every width are monomorphizations of one body. They are type-checked once,
  stored once, and have one code-identity hash.
- **From/TryFrom** come from two builtin two-parameter markers, `IntFrom<S>` (lossless: every
  value of S fits D) and `IntTryFrom<S>` (otherwise). They hold only when S ≠ D and at least
  one side is an arbitrary width, so `From<u8> for u16` keeps core's own impl and nothing
  overlaps it.
- **Inherent items** (`wrapping_*`, `checked_*`, `saturating_*`, `overflowing_*`, `MIN`,
  `MAX`, `BITS`, `count_ones`, `leading_zeros`, `trailing_zeros`) are default items of the
  `ArbitraryInt` trait. Method probe and path resolution treat them as inherent candidates of
  an arbitrary-width type, in the same way a parameter's where-clause candidates are treated,
  so `x.wrapping_add(y)` and `u3::MAX` work without importing a trait. `to_le_bytes` and
  `from_le_bytes` (and `be`, `ne`) take an array whose length depends on the width, so they
  are inherent impls in core, generated by a macro for the 22 byte-multiple widths (`u24`,
  `i40`, ...).
- **Why no other type is affected.** The blanket impls' self type is a parameter bounded by
  `ArbitraryInt`, so the impl index files them under the integer simplified types only.
  Selection for `String: Display` never sees them. For an integer self type they are rejected
  by the builtin before any unification work. blaze has no overlap checker, so they need no
  negative reasoning. The coherence they would need under rustc does not apply, because only
  blaze builds a crate that names `u3`.
- core says `#![blaze::arbitrary_ints]` for its own bodies.

## Tests (to write on the new layer)

Few, each named for a property:

- Arithmetic and overflow at widths 3 and 7, checked (a `const` item) and wrapping (a
  `const fn` in a crate without `overflow_checks`): add, sub, mul, neg, shl and casts, at
  compile time.
- The same programs at run time, compared with compile time (a ladder rung at every tier).
- An out-of-range literal is refused, naming it.
- A crate without the opt-in that defines `struct u3` is unchanged, and `u3` is not a type
  there (the name half exists: blaze-scope builtin.rs's test over both settings).
- `Option<u7>` is 1 byte; `u3`, `u24` and `u65` are 1, 4 and 16 bytes.

## Not done in the first cut

Recorded here so they are never mistaken for supported:

- `usize`-relative widths.
- `NonZero<u3>`.
- `FromStr`, `Step` (ranges of `u3` in `for` loops), `Sum`/`Product`.
- Atomics.
- `#[repr(u3)]` on enums.

Each is a typed error naming the type, not a fallback.
