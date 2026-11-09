---
status: plan
summary: `#[blaze::bits]`: Zig-style bit-packed structs with ordinary field syntax. Layout is the backing integer's; a field's width comes from its type; field places stay places through borrow checking and are lowered to integer ops by one MIR pass at the end of the runtime passes, so neither the interpreter nor the code generator sees a new projection.
date: 2026-11-02
---
# Bit-packed structs (`#[blaze::bits]`)

A blaze extension (AGENTS.md rule 4, superset-of-rust: opt-in per struct, and a program that
doesn't use it lays out and runs exactly as before). User-facing:
[const.md](../../../const.md#bit-packed-structs-blazebits). Widths of `uN`/`iN` come from
lane `uint` ([arbitrary-ints.md](arbitrary-ints.md)).

```rust
#[blaze::bits]                 // or #[blaze::bits(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Pte { present: bool, writable: bool, level: u3, frame: u40 }
```

## Where each piece lives

| Piece | Where | Why there |
|---|---|---|
| The attribute | `hir-def` `AttrFlags::BLAZE_BITS`, `AttrFlags::blaze_bits` (the backing argument), `StructFlags::IS_BLAZE_BITS`; inert in `nameres/attr_resolution.rs` | one bit test per struct on every hot path (`struct_flags`) |
| Width of a type | `hir-ty` `layout/bits.rs`, `bit_width_of(ty, env)` | layout is where widths live; shared with lane `uint`, which adds its `IntTy::Bits`/`UintTy::Bits` arm |
| The layout | `layout/bits.rs` `bits_layout(StructId)`; `layout/adt.rs` returns `Layout::scalar` of the backing integer | the struct *is* its backing integer: `FieldsShape::Primitive`, `BackendRepr::Scalar`, so ABI, size, alignment and `transmute` are the integer's with no new case anywhere |
| Field places | MIR pass `transform/bits.rs`, last of `run_passes` | see below |
| Reference refusal | `hir-ty-borrowck` `check.rs`, `consume_rvalue` (E0793) | borrowck already owns E0793 for packed fields and sees every `Ref`, raw or not, and `offset_of!` |
| Closure captures | `infer/closure/analysis.rs`, `restrict_repr_packed_field_ref_capture` | rustc's packed rule: a by-ref capture stops at the struct |
| Derives | `hir-expand` `derive_macro.rs`: a bits struct is `packed` | the packed derives already copy fields out |
| Definition errors | `blaze-diagnostics` `checks/defs/bits.rs` | reports `BitsError` at the struct, naming struct, field and type |
| Const values | matchck `bits_value_pat`, `display.rs`, the UB checker (an `Int` op), valtrees (refused) | the places that walk a constant's fields by byte offset |

## Field places: a legalization pass, not a new projection

The choice was between (a) a new `ProjectionElem::BitField { offset, width }` that the
interpreter, the code generator, the optimizer and every place-walking analysis learn, and (b)
keeping `ProjectionElem::Field` through borrow checking and drop elaboration, then rewriting
every access in one pass into operations the backends already have. (b) is built:

- One owner. The semantics of a bits field (shift, mask, sign extension, read-modify-write,
  literal packing, masked `==`) is in one file of about 950 lines. With (a) it would be in the
  interpreter, both code generators (stencils and LLVM), the optimizer's passes (copy
  propagation, GVN, SROA would each have to know a field place is not addressable) and the UB
  checker, five copies that must agree.
- The invariant "every place is addressable" keeps holding after the pass: nothing downstream
  sees a place without an address. The front end, where the invariant does not hold, already
  had a precedent: packed fields, which borrowck refuses references to.
- No backend changes. The pass emits only `Transmute` between the struct and an equal-size
  `uN`, `IntToInt`, `Shl`/`Shr` (masked shift amounts), `BitAnd`/`BitOr`, `Ne`, and
  `Discriminant`. A missing stencil for any of them is the backend's existing typed error.
- rustc does the same for `Box` (`ElaborateBoxDerefs`) and for packed drops
  (`AddMovesForPackedDrops`): a runtime pass that lowers a front-end place form.
- Const eval and run time agree by construction: both run the same runtime MIR.

The cost of (b): MIR after the pass is longer for code that uses bits structs (a read is ~4
statements), and the optimizer sees integer ops, not fields. For LLVM that is the form it wants;
for the stencil tier it is the same ops it already pattern-matches.

## What the pass does

For a place `s.f` (and `s.inner.f`, offsets summed) of a bits struct with backing `uN`:

- read: `t = transmute::<S, uN>(s); v = (t >> off) as F`. A standard integer is as wide as
  its field, so the cast's truncation is the read. A narrower field (`uN`/`iN`, an enum, a
  nested struct) is cut out first: masked, or for a signed one shifted to the top and
  arithmetic-shifted back, so a cast only ever sees a value in range and the read does not
  depend on how lane `uint` truncates. bool is `& 1 != 0`; an enum transmutes from its tag; a
  nested struct transmutes from its backing.
- write: `t = transmute(s); s = transmute((t & !(mask << off)) | ((bits(v) & mask) << off))`;
  the bits above the fields are kept.
- literal: OR of each field's bits at its offset; zero elsewhere.
- `==` (a constant pattern): compare the backings masked to the used bits.
- fake borrows of a field (match scrutinees) are removed; a real one cannot reach the pass
  (borrowck refused it).
- a call's or a `yield`'s destination in a field: written to a temporary, then a new block
  stores it.

A body with no bits place, literal or `==` is left untouched (`uses_bits` scans before
allocating anything), so programs that don't use the attribute are byte-identical.

## Not built yet

- `from_bits`/`into_bits`. `transmute` works. The methods want a core trait with a
  solver-builtin impl per bits struct (`type Bits`), the way lane `uint` adds its marker traits.
  One open question: `from_bits` of a value with an invalid enum field is UB, so it is either
  `unsafe` or checked.
- A bits struct as a const generic argument (valtrees refuse it, naming the type).
- A tuple bits struct's constructor called as a function value in compiled code (refused,
  naming the struct; the interpreter packs it).
- DWARF bit fields (`DW_AT_data_bit_offset`): debug info shows the backing integer.
- `format_args!` of a field (`println!("{}", s.f)`) is the E0793 refusal, as for packed; the
  user writes `{ s.f }`.
