---
status: plan
summary: Records and named arguments (edition change 4, ledger row E5). Under `#![blaze::records]`, `{ a: T, b: U }` is a structural type (core's `Record` over the tuple of its fields sorted by name), `{ a: x, b }` its value and `{ a, b: p }` its pattern; a record parameter whose fields have defaults is how arguments are named and left out. What the edition page left open is decided here.
date: 2026-11-10
---
# Records and named arguments

Deletes builders, options structs and one-off structs. Off unless a crate's root opens with
`#![blaze::records]`; a crate without it parses and builds as before (rule `superset-of-rust`).

## Syntax

| form | where | example |
|---|---|---|
| type | wherever a type is expected | `{ path: PathBuf, size: u64 }` |
| value | where an expression starts | `{ path: p, size }`, `{ size: 0, ..old }`, `{ create: true, .. }` |
| pattern | wherever a pattern is expected | `{ path, size: n }`, `{ path, .. }` |
| default | a field of a record type that is a function parameter's type | `opts: { create: bool = false }` |

Each reuses the struct form's nodes with no path; `RECORD_TYPE` is the one new node kind. No
Rust type or pattern starts with `{`, so those two need no lookahead.

**A value against a block.** Where an expression starts, a `{` opens a record value exactly when
the next three tokens are `IDENT` `:` and a token that is not a second `:`, or `IDENT` `,`. No
Rust block starts so. The bound is four tokens with the `{`, kinds only, which is what an
incremental reparse already holds fixed. So `{ a }` is a block (one shorthand field is `{ a, }`
or `{ a: a }`), `{}` is the unit block (the empty record has a type, `{}`, and no literal),
`{ ..base }` and `{ .. }` are blocks holding ranges, and a field value starting with `::` is
parenthesized. Where the grammar requires a block (`fn`, `if`, `loop`, `unsafe` bodies) it never
asks. In a generic argument list `{` stays Rust's const block unless it starts `{ IDENT :`.

**Gate.** `edition::Extensions::RECORDS` (bit 2), read off the root file's tokens by
`base_db::crate_extensions` and held by every parse. Folding into the integration branch
(`rework-int2-1109`) is mechanical: keep its `Extensions`, `Tokens`, `Parse` and
`EditionedFileId` plumbing, add the `RECORDS` constant, `records()` and one `EXTENSION_ATTRS`
row, and make `Parser::records_gate` read `self.inp.extensions_at(self.pos)`. Until then a
macro's expansion parses with no extension, so records cannot be written inside a macro call.

## The type

`{ a: A, b: B }` is `core::record::Record<"a,b", (A, B)>`: one `#[lang = "record"]` struct,
`#[repr(transparent)]` over the tuple of the field types in the order of the field names sorted
by their bytes, the names joined by `,` as a `&'static str` const argument. It is the "one
generic struct per sorted name list" with the list as an argument, as error unions are
`ErrorUnion<(A, B)>`. So:

- **Identity** is the sorted names and the field types, nothing else: two records are one type
  exactly when their (name, type) sets are equal, in every crate and every build, because the
  type is an ordinary generic instance of one core item. There is no id of blaze's own to keep
  stable, and a default is no part of it.
- **Layout and ABI** are the tuple's (`repr(Rust)` over the sorted fields); code without
  records sees no change. A function of another ABI cannot take a record.
- **Traits** come from core, as a tuple's do: `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`,
  `Default`, `PartialOrd` and `Ord` (in sorted-name order) when the tuple has them, so up to
  twelve fields beyond `Clone` and `Copy`; `Debug` prints `{a: 1, b: 2}`.
- **Impls are a tuple's.** A record is foreign to every crate, as a tuple is: an inherent impl
  (E0116) and an impl of a foreign trait (E0117, the orphan rule) are refused by the rules
  that refuse them on `core`'s `Record`, and a crate's own trait may be implemented for a
  record, as for a tuple. To give a record methods, name it with a struct.
- A duplicate field and a default outside a function parameter are errors naming the field; a
  visibility, attribute, `unsafe` or tuple index on a field is a syntax error.

**Values.** A record's type comes from its literal (all fields written) or from an annotation,
never from how a field is used: `r.a` on a type not yet known is the existing "type must be
known" error, and `{ a: 1, ..base }` and `{ a: 1, .. }` need a known expected record type and
say so. A functional update takes the rest from a record with the same fields. Field values are
evaluated in the order written, then the base. A pattern binds by field name; without `..` it
names every field.

## Defaults and named arguments

`fn open(path: &Path, opts: { create: bool = false, mode: u32 = 0o644 })`. The parameter's type
is `{ create: bool, mode: u32 }`. A default is a const expression of the callee: an anonymous
constant of the function's signature, type checked there against the field's type, which may
name what the signature can (consts, const fns, the function's const and type parameters are
refused in this slice), never another parameter.

- `open(p, { create: true, .. })`: each field left out takes the callee's default; a field with
  no default must be written (error naming the field and the function). Without `..` every
  field is written. `{ .. }` alone stays Rust's block around the full range, so a call that
  wants every default names one field; a spelling for "all defaults" is open.
- A default is a constant, placed at each call that omits the field, in the callee's
  declaration order after the written fields; order and count are unobservable.
- **Key** (rule `keys-cover-every-input`): `record_param_default(callee, parameter, field)` is
  the constant. A caller reads it only for a field it omits, so a changed default recomputes
  the callers that omit that field and no other.
- A call through a function pointer, a closure or a `Fn` value has no callee to ask: `..` there
  is an error saying so. A trait method's defaults are its declaration's; an impl repeats the
  types and writes none (an error naming the field), so a call on a `dyn Trait` or a type
  parameter has them too, the declaration being known statically. Nothing else changes about
  a call: no positional-to-named conversion, no reordering, no overloading.

## Lints (allow by default; errors under the edition)

`blaze::many_bools`: a call passing two or more bare `bool` literals by position; no fix. Not
built: `blaze::options_struct`, which needs every use of the struct in the workspace.

## MIR and codegen

Nothing new past MIR building: a record is an ADT instance over a tuple, which is what borrow
checking, drop elaboration, layout and codegen see. Four places learned the projection through
`Record`'s one field into the tuple: `hir-ty-mir`'s `mir/lower.rs` (a record value: a tuple
aggregate, then the struct's, defaults as constant operands; and `r.a`) and
`mir/lower/pattern_matching.rs`, the closure capture walk (`expr_use_visitor.rs`), and match
checking (`hir-ty-matchck/src/lower.rs`). Coarser but correct: the capture walk of
`{ a: 1, ..base }` consumes all of `base`, and `match_tree.rs` gives up on a record pattern.

## Not built in this slice

A spelling for "every default"; records inside a macro call (see Gate); `options_struct`; a
"name it" help on E0116 and E0117; refusing a record an `extern` function returns (a parameter
is refused); defaults that name the function's generic parameters.

## Use in blaze

None yet: blaze's crates are built by a stage 0 that does not read records (rule
`blaze-uses-its-extensions`). First, once a release reads them, three runs of positional
edition, extensions and outline arguments: `SourceFile::parse_shared_with` (`crates/syntax`),
`Reparse::parse` (`crates/base-db/src/reparse.rs`), `build_node` (`crates/syntax/src/parsing.rs`).
