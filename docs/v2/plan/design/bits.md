---
status: plan
summary: `#[blaze::bits]`: Zig-style bit-packed structs with ordinary field syntax. Layout is the backing integer's; a field's width comes from its type; field places stay places through the borrow check and are lowered to integer ops by one body-to-body pass before evaluation and code building, so neither the evaluator nor the code builder sees a new projection. On the new layer only the attribute's name and the derives' packed handling are built.
date: 2026-11-02
---
# Bit-packed structs (`#[blaze::bits]`)

A blaze extension (AGENTS.md rule 4, superset-of-rust: opt-in per struct, and a program that
doesn't use it lays out and runs exactly as before). Widths of `uN`/`iN` come from
[arbitrary-ints.md](arbitrary-ints.md).

```rust
#[blaze::bits]                 // or #[blaze::bits(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Pte { present: bool, writable: bool, level: u3, frame: u40 }
```

## What is built, what is not

Built on the new layer:

- The attribute is a known tool attribute (crates/blaze-surface/src/tool_attrs.rs).
- The derives treat a bits struct as `packed`, so they copy fields out
  (crates/blaze-expand/src/adt.rs).
- The lint `blaze::packable` suggests the attribute (crates/blaze-lint-defs/src/lints.rs).
- The fixture crates/blaze-real/extensions/b_bits.rs (expects accept, size 1).

Not built:

- A field's width from its type, and the layout: the struct *is* its backing integer (a
  scalar of that integer), so ABI, size, alignment and `transmute` are the integer's with no
  new case anywhere.
- The definition errors, reported at the struct, naming struct, field and type.
- The reference refusal (E0793, as for packed fields) in the borrow check, and closure
  captures that stop at the struct (rustc's packed rule).
- The field-place pass below, and constant values (patterns, display, the UB checker) that
  walk a constant's fields by bit offset instead of byte offset.

extensions.md proposes, undecided, that `bits` become a library type computing its layout by
reflection; that would replace this design.

## Field places: a legalization pass, not a new projection

The choice is between (a) a new field projection `{ offset, width }` that the evaluator, the
code builder, the optimizer and every place-walking analysis learn, and (b) keeping the
ordinary field projection through the borrow check and drop elaboration, then rewriting every
access in one body-to-body pass (on blaze-body's form, D119) into operations the backends
already have. (b) is the design:

- One owner. The semantics of a bits field (shift, mask, sign extension, read-modify-write,
  literal packing, masked `==`) is in one place. With (a) it would be in the evaluator, the
  code builder's tiers, the optimizer's passes (copy propagation, GVN, SROA would each have to
  know a field place is not addressable) and the UB checker: copies that must agree.
- The invariant "every place is addressable" keeps holding after the pass: nothing downstream
  sees a place without an address. The front end, where the invariant does not hold, has a
  precedent: packed fields, which the borrow check refuses references to.
- No backend changes. The pass emits only `Transmute` between the struct and an equal-size
  `uN`, integer casts, `Shl`/`Shr` (masked shift amounts), `BitAnd`/`BitOr`, `Ne`, and
  `Discriminant`.
- rustc does the same for `Box` (`ElaborateBoxDerefs`) and for packed drops
  (`AddMovesForPackedDrops`): a runtime pass that lowers a front-end place form.
- Compile time and run time agree by construction: both read the same lowered body.

The cost of (b): the body after the pass is longer for code that uses bits structs (a read is
~4 statements), and the optimizer sees integer ops, not fields. For LLVM that is the form it
wants; for the fast tier it is the same ops it already composes.

## What the pass does

For a place `s.f` (and `s.inner.f`, offsets summed) of a bits struct with backing `uN`:

- read: `t = transmute::<S, uN>(s); v = (t >> off) as F`. A standard integer is as wide as
  its field, so the cast's truncation is the read. A narrower field (`uN`/`iN`, an enum, a
  nested struct) is cut out first: masked, or for a signed one shifted to the top and
  arithmetic-shifted back, so a cast only ever sees a value in range and the read does not
  depend on how an arbitrary-width cast truncates. bool is `& 1 != 0`; an enum transmutes from
  its tag; a nested struct transmutes from its backing.
- write: `t = transmute(s); s = transmute((t & !(mask << off)) | ((bits(v) & mask) << off))`;
  the bits above the fields are kept.
- literal: OR of each field's bits at its offset; zero elsewhere.
- `==` (a constant pattern): compare the backings masked to the used bits.
- fake borrows of a field (match scrutinees) are removed; a real one cannot reach the pass
  (the borrow check refused it).
- a call's or a `yield`'s destination in a field: written to a temporary, then a new block
  stores it.

A body with no bits place, literal or `==` is left untouched (a scan before allocating
anything), so programs that don't use the attribute are byte-identical.

## Open, after the first cut

- `from_bits`/`into_bits`. `transmute` works. The methods want a core trait with a
  solver-builtin impl per bits struct (`type Bits`), the way arbitrary-ints adds its marker
  traits. One open question: `from_bits` of a value with an invalid enum field is UB, so it is
  either `unsafe` or checked.
- A bits struct as a const generic argument (refused, naming the type).
- A tuple bits struct's constructor called as a function value in compiled code (refused,
  naming the struct).
- DWARF bit fields (`DW_AT_data_bit_offset`): debug info shows the backing integer.
- `format_args!` of a field (`println!("{}", s.f)`) is the E0793 refusal, as for packed; the
  user writes `{ s.f }`.
