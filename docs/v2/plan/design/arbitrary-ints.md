---
status: plan
summary: Integers of any width, `u1`..`u128` and `i1`..`i128`, as Zig has them, behind the crate attribute `#![blaze::arbitrary_ints]`. They are real primitive types, shadowed like `u8`. They are stored in the smallest power-of-two container with the unused high bits as a niche, overflow at their width, and get their trait impls from blanket impls in core over a solver-builtin marker trait.
date: 2026-11-02
---
# Integers of any width (`#![blaze::arbitrary_ints]`)

A blaze extension ([no-rustc.md](../../../no-rustc.md#what-counts-as-running-rustc), rule
4 superset-of-rust): a crate that says `#![blaze::arbitrary_ints]` gets the integer types
`u1`..`u128` and `i1`..`i128`, as in Zig. A crate that doesn't say it is unchanged, including
one that defines its own `struct u3` or `mod i7`. User-facing:
[const.md](../../../const.md#integers-of-any-width).

## The opt-in

`#![blaze::arbitrary_ints]` at the crate root. Why a crate attribute and not a manifest key or
a feature:

- It is in the source, so the crate means the same thing however it is built (cargo, blaze, a
  vendored copy). A manifest key would make the meaning of `u3` depend on a file next to it.
- Name resolution reads it while it seeds the crate (`DefCollector::seed_with_top_level`), next
  to `no_std` and `recursion_limit`, so no other query has to read the manifest.
- It is per crate, like `#![blaze::sans_io]`. A dependency's choice never leaks into its
  dependents: a non-opted crate can hold a `u3` it got from an opted one, but it can't name
  the type or write `5u3`.

It is stored in `DefMapCrateData::arbitrary_ints`, persisted in the frozen def map
(`ENCODING` 16), and read through `DefMap::arbitrary_ints()`.

## Representation

`rustc_ast_ir` is vendored (`vendor/rustc_ast_ir`, `SOURCE.md`) to add
`IntTy::Bits(IntWidth)` and `UintTy::Bits(IntWidth)`, where `IntWidth` is 1..=127 excluding 8,
16, 32 and 64. The standard widths keep their own variants, so `u8` is `UintTy::U8` and never
`Bits(8)`, and every type has exactly one spelling. A new variant of `IntTy` and not a new
`TyKind`, because everything that is true of an integer (`is_integral`, casts, builtin
binops, `SimplifiedType::Uint`, int-var unification, the impl index) stays true without new
arms. `hir_def::BuiltinInt::Bits` and `BuiltinUint::Bits` mirror it.

- `bit_width()` is the value width (3 for `u3`).
- `storage()` is the standard type it is stored as (`u3` is stored as a `u8`, `i24` as an
  `i32`, `u65` as a `u128`).
- `code()` and `from_code()` give a stable byte: a standard type keeps its old variant
  number, and an arbitrary width is 128 plus its width. Persisted tables
  (`expr_store/persist.rs`, `next_solver/persist.rs`) and the method-index identity hash use
  it, so the bytes for the standard types are unchanged.

**Invariant.** A value of `uN` is held in its storage integer zero-extended from N bits, and a
value of `iN` is held sign-extended from N bits. Every producer of a value (an operator, a
cast, an intrinsic, a literal, a constant) leaves it in that form, and every consumer may
assume it. Comparisons, bitwise and/or/xor, `Shr`, `Not` of a signed value, widening casts and
moves need no extra work under this invariant. Everything else ends with **normalize to N**:
`x & (2^N - 1)` for unsigned, `(x << (S - N)) >> (S - N)` (arithmetic) for signed, where S is
the storage width.

## Layout

`hir_ty::layout::int_scalar`: the storage integer with `valid_range` equal to the width's
values. Unsigned is `0..=2^N - 1`. Signed is the wrapping range from MIN's sign-extended
pattern up to MAX; `i3` is `0xFC..=0x03`. So:

- The size is the smallest power-of-two byte count that holds N bits: `u3` is 1 byte, `u24`
  is 4, `u65` is 16. The alignment is the storage integer's ABI alignment, which equals the
  size on every 64-bit target blaze builds for.
- The unused high bits are a niche, exactly like `bool`'s, so `Option<u7>` is 1 byte and
  `Option<u1>` is 1 byte.
- Inside `#[blaze::bits]`, a field takes exactly N bits. The width comes from
  `hir_ty::layout::bit_width_of`, the bits lane's query (`/tmp/blaze-lanes/v2-bits.md`),
  which answers `IntTy::Bits(w)` / `UintTy::Bits(w)` as `BitWidth { bits: w.bits(), signed }`.

## Names and literals

- `u3` resolves where the builtin types resolve (`DefMap::resolve_name_in_module`'s
  `from_builtin`, the block-scope builtin shadow and the `u16::from_le_bytes` value-path
  fallback in `resolver.rs`). So an item, import, generic parameter or module named `u3`
  shadows it exactly as it would shadow `u8`. It resolves only in the root def map of a crate
  with the opt-in. `BuiltinType::arbitrary_by_text` accepts `u` or `i` followed by 1 to 127
  written without a leading zero, and nothing that is a standard width.
- `5u3` is read by `ExprCollector::lower_literal`, only under the opt-in. Elsewhere the
  suffix is what it is under rustc.
- An unsuffixed literal infers to `u3` by unification with an expected type. An integer
  variable never becomes an arbitrary width through trait selection alone: the impl lookup
  for an int var still lists only the 12 standard kinds (`interner.rs`), and the solver's
  builtin `ArbitraryInt` is `NoSolution` for an int var. So `1 + 2` and `x.into()` infer
  exactly as before in every crate.
- **Out of range.** A literal that does not fit its arbitrary-width type (`9u3`, `-5i3`,
  `0b1000_u3`) is a hard error naming the literal as written, the type and its range:
  ``literal `9u3` out of range for `u3` `` with the note `` `u3` holds `0..=7` ``. It is
  reported in `blaze-diagnostics`' `overflowing_literals` pass (`refuse_arbitrary`), but it is
  not that lint: no `allow` turns it off, because no existing program depends on a `u3`
  literal wrapping. A standard-width literal is linted as before.

## Operators, overflow and casts

The setting is the crate's `overflow_checks` cfg, which the MIR `asserts` pass reads.
`#[rustc_inherit_overflow_checks]` is honored as today. A `const` item's own body is always
checked, and a `const fn` follows its crate's setting.

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
widths. It is the one rule both backends implement.

The overflow flag of `AddWithOverflow`/`SubWithOverflow` is `raw != normalize(raw)`: two
N-bit operands of `+` or `-` never overflow the container. For `*` it is that, or-ed with the
container's own overflow flag, since an N-bit product needs 2N bits.

A panic names the operation and the type, in the same words in both backends:
``attempt to {add, subtract, multiply, divide, calculate the remainder, shift left, shift right}
with overflow at type `u3` ``, ``attempt to negate with overflow at type `i3` ``,
``attempt to divide by zero at type `u3` `` and
``attempt to calculate the remainder with a divisor of zero at type `u3` ``. Codegen panics
through `core::panicking::panic`. A standard type keeps today's messages.

The shift amount is read at its own type's value width wherever the amount is an arbitrary
width, also when the shifted value is a standard integer: `x << y` with `y: i3` equal to -1
shifts by 7 (mod the width of `x` when unchecked).

## Backends agree

- **Const eval** (`hir-ty-mir/src/mir/eval.rs`, `eval/shim.rs`). For an arbitrary width,
  every integer binop, unop, cast and integer intrinsic computes on the operands as
  i128/u128, checks the result against the N-bit range, and writes it back normalized to the
  storage size.
- **Codegen** (`hir-ty-codegen/src/mod.rs`). It builds Cranelift IR that both the stencils
  and LLVM consume, so the normalization is emitted once there: `band_imm` or
  `ishl_imm`+`sshr_imm` after each operation the table above marks. No new stencil is needed,
  because the operations are the storage width's. Any stencil or LLVM lowering that is missing
  is the existing typed `blaze_stencil::Error`/`Gap` naming the function.
- The tests below run each operation in const eval and at runtime, at widths 3 and 7, with
  checks on and off, and compare.

## Trait impls and inherent items

The trait impls exist once, in core, not once per width. Writing them per width means 246
types times about 40 traits, and a macro still makes those impls exist. Instead:

- **`ArbitraryInt`**, a sealed lang-item trait in core (`core::num::arbitrary`). The solver
  answers it as a builtin (vendored `rustc_next_trait_solver` patch, `PATCHES.md`): `Yes` for
  `Int(Bits)`/`Uint(Bits)`, `NoSolution` for every other rigid type and for an int var, and
  ambiguous for a type var. It can't be implemented by hand.
- **Blanket impls written once over it.** `impl<T: ArbitraryInt> Add for T` and the other
  operator traits (with their assign forms and the `&T` forms), `PartialEq`, `Eq`,
  `PartialOrd`, `Ord`, `Hash`, `Debug`, `Display`, `LowerHex`, `UpperHex`, `Binary`, `Octal`,
  `Default`, `Clone` (with `TrivialClone`). `Copy` is the solver's builtin instead, as for the
  standard integers, since core cannot write `impl<T: ArbitraryInt> Copy for T` (E0206).
  Their bodies are ordinary Rust over core's generic intrinsics
  (`add_with_overflow`, `wrapping_*`, `saturating_*`, `ctpop`, `ctlz`, `cttz`,
  `three_way_compare`, and a new `blaze_int_cast::<S, D>`, in blaze's own section of the intrinsic table, that is `as` between two integer types).
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
- core says `#![blaze::arbitrary_ints]` for its own bodies. `library/DIGEST` is updated
  (`python3 harness/library.py digest '{}'`).

## Tests

Few, each named for a property:

- `hir-ty-mir/src/tests/consteval.rs` `arbitrary_ints_*`: arithmetic and overflow at widths 3
  and 7, checked (a `const` item) and wrapping (a `const fn` in a crate without
  `overflow_checks`). It covers add, sub, mul, neg, shl and casts.
- The same programs at runtime under codegen, compared with const eval (the coordinator's
  run, `harness/arbitrary_ints.py`).
- An out-of-range literal is refused, naming it (blaze-diagnostics UI test).
- A crate without the opt-in that defines `struct u3` is unchanged, and `u3` is not a type
  there (hir-def nameres test).
- `Option<u7>` is 1 byte; `u3`, `u24` and `u65` are 1, 4 and 16 bytes
  (`hir-ty-mir/src/tests/layout.rs`).

## Not done in the first cut

Recorded here so they are never mistaken for supported:

- `usize`-relative widths.
- `NonZero<u3>`.
- `FromStr`, `Step` (ranges of `u3` in `for` loops), `Sum`/`Product`.
- Atomics.
- `#[repr(u3)]` on enums.

Each is a typed error naming the type, not a fallback.
