---
status: current
summary: Const evaluation: blaze's own interpreter for constants and const fns, and what it covers.
---
# Const

A `const fn` is blaze's pure function: deterministic, touching no mutable state, so its result
can be computed at compile time, cached by its code and arguments, and trusted to be the same
on every machine. blaze evaluates constants with its own interpreter
(`crates/hir-ty-mir/src/mir/eval.rs`) and never asks rustc. Everything the pinned nightly
(`harness/toolchain`) accepts in const is accepted, and gives the value rustc gives; what goes
further is an extension a workspace opts into.

## Const traits, closures and std's const impls

As in the pinned nightly, under `#![feature(const_trait_impl, const_closures, derive_const)]`:

- `const trait Tr { .. }`, `const impl Tr for T { .. }` (the nightly refuses the older
  `impl const Tr for T`; blaze still parses it), bounds `T: [const] Tr` (const where the
  caller is), `T: const Tr` (always const) and the older `~const Tr`, default methods and
  `[const]` supertraits.
- Calls through those bounds in a `const fn`, a `const` item or a `static`, evaluated by the
  impl the type has.
- `#[derive_const(PartialEq, Clone, Default, ..)]`.
- std's const impls: `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Clone`, `Default`, `From`,
  `Into`, `TryFrom`, `AsRef`, the operator traits (arithmetic, bit, compound assignment,
  `Deref`, `Index`), `Iterator`, `IntoIterator`, `Sum`, `Product`, `Borrow` and `Destruct`.
- `const |x| ..` closures, called in const directly or passed as `impl [const] Fn(..)`,
  `[const] FnMut` and `[const] FnOnce`; a `const fn` item passed the same way.

Whether a call in a const body is const is decided as rustc decides it, by proving the callee's
`[const]` bounds const (`crates/hir-ty/src/effects.rs`): `T: [const] Tr` holds in a const fn
that has the bound, a `const impl` holds for its type when its own `[const]` bounds do, and a
closure or fn item is `[const] Fn` when it is const. `non_const_calls` lists the calls that are
not, for the const checker (#860) to refuse; ordinary type checking does not prove effects, so
it costs nothing outside const bodies. Tests: `crates/hir-ty-mir/src/tests/const_effects.rs`.

Tests: `crates/hir-ty-mir/src/tests/consteval/const_traits.rs` (values from rustc on the same
programs).

## Reflection: `core::mem::type_info`

The nightly's reflection API (`#![feature(type_info)]`: `Type::of::<T>()`, `TypeId::info`,
`TypeId::trait_info_of`, `Struct`/`Enum`/`Tuple` fields, generics, fn pointer facts) is
answered in const by `crates/hir-ty-mir/src/mir/eval/shim/reflection.rs`, as rustc's const eval
answers it:

- A field's id is the `TypeId` of its field representing type
  (`FieldRepresentingType<Base, VARIANT, FIELD>`), so it equals the id of the same `field_of!`
  type and means the same at run time; `field_offset::<F>()` (`Field::OFFSET`) is answered
  from it.
- `FnPtr::splatted()` reports the input marked `#[rustc_splat]`; `#[cfg]` on a fn pointer
  type's input is honored.
- A `DynMetadata<dyn Tr>` kept in a constant is emitted as the type's vtable for `Tr`.

Not yet: a `DynMetadata<*const ()>` in a constant (what `TypeId::vtable` returns) stops the
build with an error naming the type, because blaze's vtable ids do not record the trait;
`field_of!(..)` (`builtin # field_of`) is not parsed as a type; `field_offset` called at run
time is an unsupported intrinsic in codegen; `Type::of` of a tuple is refused (rustc ICEs).
Tests: `crates/hir-ty-mir/src/tests/consteval/intrinsics.rs` (`type_info_*`). Plan:
[docs/v2/plan/design/comptime.md](v2/plan/design/comptime.md).

## Extensions (beyond the pinned nightly)

Two are planned; v2 has neither yet. rustc refuses the programs, so a workspace that uses them
would build with blaze only, each opt-in by name in the root manifest:

```toml
[workspace.metadata.blaze]
experimental = ["const-hash", "const-fn-pointers"]
```

Today v2 knows only the experiments `sandboxed-tests` and `const-macros`
(`crates/blaze-units/src/load/sandbox.rs`, `KNOWN`) and stops at any other name, so this table
is refused.

- `const-hash`: `core::hash::Hash`, `Hasher` and `BuildHasher` are const traits, so
  `T: [const] Hash`, `const impl Hasher for MyHasher` and `#[derive(Hash)]` types hash in
  const. In the pinned nightly none of the three is const ("`[const]` can only be applied to
  `const` traits"). Hashing in const is what lets a build graph key its steps by a hash
  computed at compile time. The `library/` blaze builds declares them as the nightly does
  (`library/core/src/hash/mod.rs`), so v2 refuses these programs.
- `const-fn-pointers`: calling a `fn` pointer in const. The nightly refuses it ("function
  pointer calls are not allowed in constant functions"), and so does v2's const checker.

A third is planned as a crate feature rather than a manifest name, since it adds syntax and
`core` items to the crate that opts in: `#![feature(blaze_comptime)]`, generic code over a
type's fields and variants. `for const f in core::reflect::fields_of::<T>() { .. value.[f] .. }`
unrolls once per field at monomorphization, with a body type-checked once against bounds that
quantify over the fields (`where for<F: Field<T>> F::Type: Hash`), so a type with a non-`Hash`
field is refused at the call, naming the field. `core::reflect::fn_info(f).code_id` gives a
function's code identity. rustc refuses `#![feature(blaze_comptime)]` (unknown feature), and v2
has none of it yet. Design: [docs/v2/plan/design/comptime.md](v2/plan/design/comptime.md).

## Bit-packed structs (`#[blaze::bits]`)

A blaze extension, opt-in per struct (rustc refuses the attribute: `blaze` is no tool it
knows). A struct marked `#[blaze::bits]` packs its fields at bit offsets, as Zig's `packed
struct` does, with ordinary field syntax:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Mode { Off, Read, Write }          // 3 variants: 2 bits

#[blaze::bits]                          // or #[blaze::bits(u16)] for an explicit backing
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Flags { valid: bool, mode: Mode, level: u8 }   // 1 + 2 + 8 = 11 bits: a u16

let mut f = Flags { valid: true, mode: Mode::Write, level: 7 };
f.level += 1;                           // read-modify-write of the backing integer
let Flags { mode, .. } = f;             // patterns and `..` update work as for any struct
```

- Layout: the first field at bit 0, the rest in declaration order. The struct is its backing
  integer: the smallest of `u8`..`u128` that holds the fields, or the one
  `#[blaze::bits(uN)]` names (refused when the fields don't fit). Size, alignment and ABI are
  that integer's, so `transmute` to and from it works; bits above the fields are zero in a
  literal and kept by a field write.
- A field's width comes from its type: `bool` is 1, an integer its width (`u8` is 8; with
  [integers of any width](v2/plan/design/arbitrary-ints.md), `level: u3` is 3), a fieldless enum the bits
  that hold its discriminants (`⌈log2 n⌉` for `0..n`, signed when one is negative, its repr's
  width when it has an integer `#[repr]`), a nested `#[blaze::bits]` struct its fields' total.
  `usize`, `isize`, any other type, a generic struct, a `#[repr]` on the struct, a field type
  that implements `Drop`, and more than 128 bits are errors naming the struct, and the field
  and its type where one is at fault. The attribute on an enum or a union is an error too.
- A field has no address. `&s.f`, `&mut s.f`, `&raw const s.f`, `addr_of!`, `offset_of!` and a
  method that takes `&self` on a field (`s.mode.clone()`) are errors naming the struct and
  the field (E0793, as for a misaligned field of a packed struct). Copy the field out instead,
  `{ s.f }`, as for `println!("{}", { s.f })`. A reference to the whole struct is fine. A
  closure that uses a field by reference captures the whole struct, as for a packed struct.
- Derives work as for a packed struct: each copies the fields out, so the fields' types must
  be `Copy`. `Debug` prints the field names and values.
- Const eval and run time agree: both run the same rewritten MIR
  ([mir-passes.md](mir-passes.md), pass 6).

Not yet: `from_bits`/`into_bits` methods (use `transmute`); a bits struct as a const generic
argument (refused, naming the type); a tuple bits struct's constructor used as a function value
in compiled code (refused, naming the struct: call it in a closure); debug info shows the
backing integer, not the fields. Plan: [v2/plan/lanes/frontend/bits.md](v2/plan/design/bits.md).

## Integers of any width

A blaze extension, opt-in per crate (rustc refuses the attribute: `blaze` is no tool it knows).
A crate whose root says `#![blaze::arbitrary_ints]` has an integer type for every width from 1
to 128 bits, as Zig has: `u1`..`u128` and `i1`..`i128`. `u8`, `u16`, `u32`, `u64`, `u128` and
their signed twins are the same types as everywhere; `u3`, `i7`, `u24` and the rest are new
primitives.

```rust
#![blaze::arbitrary_ints]

const A: u3 = 5 + 2;            // 7
const B: i7 = -64;              // the least i7
const C: u24 = 0xff_ffff;
const D: u8 = A as u8;          // 7: widening keeps the value
const E: u3 = 13u8 as u3;       // 5: `as` truncates to 3 bits
const F: u3 = A + 1;            // error: attempt to add with overflow at type `u3`
const G: u3 = 9;                // error: literal `9` out of range for `u3`
```

- **Names.** `u3` resolves where `u8` does, and an item, import, generic parameter or module
  named `u3` shadows it as one would shadow `u8`. A crate without the attribute is unchanged:
  its `struct u3` is its own type, and `5u3` is the invalid-suffix error rustc gives. A value of
  type `u3` can reach such a crate (a function of an opted-in crate returns one), but only an
  opted-in crate can name the type or write `5u3`.
- **Literals.** `5u3`, or unsuffixed where the type is known (`let x: u3 = 5;`). An integer
  variable never becomes an arbitrary width through trait selection, so `1 + 2` and
  `x.into()` infer as before. A literal that doesn't fit is a hard error naming the literal,
  the type and its range (`` `u3` holds `0..=7` ``). It is not the `overflowing_literals`
  lint, and no `allow` turns it off.
- **Layout.** A `uN` is stored in the smallest of `u8`..`u128` that holds N bits, so `u3` is 1
  byte, `u24` 4 and `u65` 16, aligned as that integer is. Its bits above N are a niche, so
  `Option<u7>` is 1 byte. A `u3` field of a [`#[blaze::bits]`](#bit-packed-structs-blazebits)
  struct takes 3 bits.
- **Overflow.** Arithmetic is at the type's width. A `const` item's own body always checks
  overflow, as rustc's does, and a function (a `const fn` too) follows its crate's
  `overflow-checks`.

  | Operation | Checks on | Checks off |
  |---|---|---|
  | `+ - *` | panics if the result needs more than N bits | wraps at N bits |
  | unary `-` | panics on `iN::MIN` | wraps to `iN::MIN` |
  | `/ %` | panics on zero, and on `iN::MIN / -1` and `% -1` | the same |
  | `<< >>` | panics if the amount is N or more | shifts by the amount mod N |
  | `as` an integer | truncates to N bits and sign- or zero-extends | the same |
  | `as` from a float | rounds toward zero, saturates at N bits, NaN is 0 | the same |

  A shift amount is read as an unsigned number at its own type's width, so `-1i3` is 7. A
  panic names the operation and the type: ``attempt to add with overflow at type `u3` ``,
  ``attempt to divide by zero at type `i7` ``.
- **Intrinsics.** Const eval computes core's integer intrinsics at the width:
  `wrapping_*`, `*_with_overflow`, `saturating_*`, `unchecked_*` (whose overflow is undefined
  behavior it reports), `ctpop`, `ctlz`, `cttz`, `bitreverse`, the rotates (by the amount
  mod N), `bswap` for a width that is a whole number of bytes (`u24`), and blaze's own
  `blaze_int_cast::<S, D>` (`x as D` between any two integer types, which core's items of these
  types are written with). `bswap` of `u3`,
  `carrying_mul_add`, `carryless_mul` and the funnel shifts are errors naming the type.
- **Const eval and compiled code agree.** Codegen normalizes the storage integer after each
  operation above, and panics with the same words.

- **Traits and methods.** core gives every one of these types, by blanket impls over a marker
  trait the solver answers (`core::num::arbitrary::ArbitraryInt`, which no crate can
  implement), and not by an impl per width:
  - the operators and their `*Assign` and `&T` forms, `-` for the signed ones only, and shifts
    by any integer type;
  - `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Hash`, `Default`, `Clone` and `Copy`;
  - `Debug`, `Display`, `LowerHex`, `UpperHex`, `Binary` and `Octal`;
  - `From` where every value fits (`u3` into `u8`, `i7`, `u4`) and `TryFrom` otherwise, with
    `TryFromIntError`;
  - as inherent items: `BITS`, `MIN` and `MAX`; `wrapping_`, `checked_`, `overflowing_` and
    `saturating_` arithmetic; `pow`, `div_euclid` and `rem_euclid`; `count_ones`,
    `count_zeros`, `leading_zeros` and `trailing_zeros`; and `to_`/`from_` `le`/`be`/`ne`
    `_bytes` for a width that is a whole number of bytes (`u24` has `[u8; 3]`).

  A `const fn` can call these methods. A generic `const fn f<T: ArbitraryInt>` needs
  `T: [const] ArbitraryInt`, as with any const trait. A panic in one of core's bodies says
  the type at run time (``attempt to add with overflow at type `u3` ``) but only the operation
  in const eval, which can't format it. `a + b` itself is a builtin operation and says the
  type in both.

The evaluator's arithmetic is `crates/hir-ty-mir/src/mir/eval/narrow.rs`, codegen's
`crates/hir-ty-codegen/arbitrary_int.rs`. Tests: `arbitrary_ints_*` in
`crates/hir-ty-mir/src/tests/` (`consteval.rs`, `layout.rs`, `simple.rs`), the literal error
in `crates/blaze-diagnostics/src/tests/lints/bodies/types/limits.rs`, and the smoke case
`harness/smoke/arbitrary_ints`, which runs the same calls compiled and in const eval, in dev
and release.

Not yet: `From<bool>`, `Sum`, `Product`, `FromStr`, `Step` (a `for` loop over a range of `u3`),
`LowerExp` and `UpperExp`; `abs`, `signum`, `rotate_*`, `ilog`, `isqrt`, `strict_*`,
`unchecked_*` and the `*_signed`/`*_unsigned` methods; `NonZero<u3>`; atomics; `#[repr(u3)]`
on an enum. Each is an error naming the type.

Plan: [v2/plan/lanes/frontend/arbitrary-ints.md](v2/plan/design/arbitrary-ints.md).

blaze checks const bodies against rustc's `check_consts` rules (#914, #1440,
`crates/hir-ty-mir/src/mir/const_check.rs`): E0015 for a non-const call, a call of a `fn`
pointer, an intrinsic not declared `const`, and const stability (`const_check/calls.rs`: the
nightly's atomics are `const` only under `#![feature(const_atomic)]`). Its errors are part of a
unit's check (`crates/hir/src/diagnostics.rs`, `crates/blaze-cargo/src/serve/unit/checks.rs`),
so they fail the build as rustc's do.
