---
status: plan
summary: "`type_of!(desc)`, a type computed by const code (row Q7 of sql.md; phase 3 of comptime.md). A description is a value of core's `reflect::TypeDesc`; `type_of!` names `<Described<'a, {desc}, Args> as TypeOf>::Type`, an associated type the trait solver normalizes by reading the description's value tree. No new syntax, no new kind of type: a described record is core's `Record`, so identity is by the description's value. blaze-sql's checker runs in a `const static` block and returns a description; a SQL error is the block's panic."
date: 2026-11-13
---
# `type_of!`: a type from a const value

blaze-sql's typed surface needs `query::<"select ..">(params)` to have a row type and a
parameter record computed from the text and the schema (sql.md, row Q7). The smallest general
primitive that does it is one builtin associated type over a const generic argument; everything
else (the checker, the schema by reflection, the records) already exists.

## The form

```rust
#![feature(blaze_comptime)]
use core::reflect::{type_of, FieldDesc, Prim, TypeDesc};
const ROW: TypeDesc = TypeDesc::Record(&[
    FieldDesc { name: "name", ty: TypeDesc::Arg(0) },
    FieldDesc { name: "age", ty: TypeDesc::Prim(Prim::I64) },
]);
type Row = type_of!(ROW; String);             // Record<"age,name", (i64, String)>
fn bind<'p>(p: type_of!('p, PARAMS; Person)) { .. }
```

`type_of!` is a `macro` of core: `type_of!(D)`, `type_of!(D; A, B)` and `type_of!('a, D; A, B)`
expand to `<Described<'a, { D }, (A, B)> as TypeOf>::Type` (`'static` when no lifetime is
written). `Described` is a lang-item struct and `TypeOf` a lang-item trait with one
associated type; both are `#[rustc_deny_explicit_impl]`. No parser change, no `SyntaxKind`,
no `TypeRef` variant: a macro in type position and a const argument are Rust already.

## The description type

`core::reflect::TypeDesc`, beside phase 2's reflection types, `ConstParamTy` so it is a const
argument (core has `adt_const_params` and `unsized_const_params`):

| Variant | Type |
|---|---|
| `Prim(Prim)` | `bool`, `char`, an integer, `f32`, `f64`, `str` |
| `Arg(i)` | the `i`th type of `Args` (types the description cannot name: `String`, `PersonId`) |
| `FieldOf(&d, i)` | the type of field `i` (declaration order) of the struct or tuple `d` |
| `ArgOf(&d, i)` | the `i`th type argument of the ADT `d` (`Option<T>` gives `T`) |
| `Option(&d)`, `Ref(&d)`, `Slice(&d)` | `Option<d>`, `&'a d`, `[d]` |
| `Tuple(&[d..])` | the tuple |
| `Record(&[FieldDesc { name, ty }])` | core's `Record` of the names sorted by bytes over their types |

`TypeId` is not a variant: it has no value tree (its bytes are an evaluator-local index), so a
type reaches a description through `Args` and `FieldOf`/`ArgOf`. For SQL, `Args` is the
schema's tuple of table structs and a column's type is `FieldOf(&FieldOf(&Arg(0), t), c)`.

## Identity

By value. `Described<'a, D, Args>` is an ordinary type whose const argument is interned by its
value tree (row Q5), and its normal form is a type built from parts that are themselves
structural: a record is `Record<names, tuple>`, an `Option` is core's. Two descriptions with
equal values over equal `Args` are one type in every crate; nothing is keyed by where a
description was written. `type_name` of the result is the result's own (a record's).

## Evaluation and its bound

The solver's builtin candidate for `Described<..>: TypeOf` and for `NormalizesTo` of
`TypeOf::Type` structurally normalizes `D` (the solver's `evaluate_const`, so the evaluator's
step limit is the bound), then asks the interner (`reflect_type_of`) to read the value tree:
a walk over its nodes, at most `VALTREE_MAX_NODES`, so it is bounded. A `D` that names a
generic parameter or an inference variable is ambiguous, so a generic `type_of!` is a rigid
alias until its arguments are concrete, as comptime.md's phase 3 wants.

SQL: the typed surface writes `type_of!('p, const static { sql::params(&APP, Q) }; Tables)`;
the `const static` block runs blaze-sql's checker with no const checking (non-const functions,
`Vec`, iterators and closures run; comptime.md phase 2b) and leaks the description, whose
`&'static` slices the evaluator makes global.

## Errors

- A description that names no type (`Arg(i)` past `Args`, `FieldOf` of a non-struct, a field
  past its fields, two record fields of one name, an empty name or one with `,`, `str` or a
  `Slice` anywhere but directly under `Ref`, `ArgOf` past the ADT's type arguments, a
  `TypeDesc` value the reader does not know): the trait goal fails, so the use is
  the unmet bound `Described<..>: TypeOf`. Built. Wanted: its own diagnostic naming the variant
  and the path into the description (`Record[2].ty: Arg(3) past 2 types`); not built.
- A SQL error: the checker panics with its message in the block, so const eval fails at the
  block and the error carries the panic message (E0080) at the `type_of!` use. Not tested.
- A cycle (a description whose evaluation needs its own `type_of!`): blaze-query's cycle
  between `anon_const_eval` and type lowering. Wanted: a typed error naming the alias and the
  constant; not built, and what the cycle reports today is not checked.

## What blaze-sql needs from the evaluator

blaze-sql (`crates/blaze-sql` on main) has no `const fn`: every function allocates through `A:
Allocator`, uses iterators and closures, `fmt::Display` for errors, `str::parse::<f64>` for
real literals, and `TypeId::of` in `RustTy`. None of it is const-checkable, all of it runs in
a `const static` block:

| Need | State |
|---|---|
| non-const calls, closures, `Iterator` adapters | phase 2b, built |
| `Vec<_, Global>`, `Box<_, Global>` (heap in the block) | phase 2b, built |
| `fmt` for a panic message | phase 2b, built |
| `str::parse::<f64>` (dec2flt, pure code) | not checked |
| a `Description` to a `TypeDesc` | not built: a function `sql::desc_of(&Description) -> TypeDesc` leaking its slices, and the column-to-`FieldOf` path, in blaze-sql |
| `RustTy`'s `TypeId` | not needed for the type (built from `FieldOf`), only for messages |

## Floor and cost

Floor per use: evaluate `D` once (the checker over the text: linear in the text and the tables
it names) and build the result type once. Cost: `D` is an anonymous constant cached by
blaze-query (one evaluation per distinct text and schema), the read is one walk of its value
tree per normalization, and the result type is interned, so a second use of the same query
costs one normalization lookup. A table's struct edited reruns the anon const of each query
whose evaluation read that table's reflected fields, and nothing else.

## Built (2026-11-13), and where the code stops

- core: `TypeDesc`, `FieldDesc`, `Prim`, `Described`, `TypeOf`, `type_of!`
  (library/core/src/reflect.rs).
- Lang items `blaze_described`, `blaze_type_of`, `blaze_type_of_type`; solver items
  `ReflectTypeOf`, `ReflectTypeOfType`; the trait and normalizes-to candidates
  (vendor/rustc_next_trait_solver/src/solve/{reflect,trait_goals,normalizes_to}.rs); the
  interner's `reflect_described_desc` and `reflect_type_of`; the reading,
  crates/hir-ty/src/type_of.rs.
- Tests: crates/hir-ty/src/tests/type_of.rs.
- Stops at: the own diagnostic for a refused description, the cycle error, blaze-sql's
  `desc_of`, and the typed surface (`query::<Q>`), which is row Q7's check.
