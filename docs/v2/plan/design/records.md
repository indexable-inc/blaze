---
status: plan
summary: Records and named arguments (edition change 4, ledger row E5). Under `#![blaze::records]`, `{ a: T, b: U }` is a structural type (core's `Record` over the tuple of its fields sorted by name), `{ a: x, b }` its value and `{ a, b: p }` its pattern; a record parameter whose fields have defaults is how arguments are named and left out. What the edition page left open is decided here.
date: 2026-11-10
---
# Records and named arguments

Deletes: builders, options structs, one-off structs. Off unless a crate says `#![blaze::records]`
plainly among the inner attributes that open its root (rule `superset-of-rust`); a crate without
it parses and builds as before, error for error.

## Syntax

| form | where | example |
|---|---|---|
| type | wherever a type is expected | `{ path: PathBuf, size: u64 }` |
| value | where an expression starts | `{ path: p, size }`, `{ size: 0, ..old }`, `{ create: true, .. }` |
| pattern | wherever a pattern is expected | `{ path, size: n }`, `{ path, .. }` |
| default | a field of a record type that is a function parameter's type | `opts: { create: bool = false }` |

Each reuses the struct form's nodes with no path before the braces; `RECORD_TYPE` is the one
new node kind. No Rust type or pattern starts with `{`, so those two need no lookahead.

**A value against a block.** Where an expression starts, a `{` opens a record value exactly when
the next three tokens are `IDENT` `:` and a token that is not a second `:`, or `IDENT` `,`. No
Rust block starts so (there is no type ascription, a label is a lifetime, nothing is followed by
a bare comma). The bound is four tokens with the `{`, kinds only, which is what an incremental
reparse already holds fixed. So: `{ a }` is a block (one shorthand field is `{ a, }` or
`{ a: a }`); `{}` is the unit block (the empty record has a type, `{}`, and no literal);
`{ ..base }` and `{ .. }` are blocks holding ranges; a field value starting with `::` is
parenthesized. Where the grammar requires a block (`fn`, `if`, `loop`, `unsafe` bodies) it never
asks. In a generic argument list `{` stays Rust's const block unless it starts `{ IDENT :`;
the subject of a `where` predicate that is a record is parenthesized.

**Gate.** `edition::Extensions::RECORDS` (bit 2), read off the root file's tokens by
`base_db::crate_extensions` and held by every parse. Folding into the integration branch
(`rework-int2-1109`) is mechanical: keep its `Extensions`, `Tokens`, `Parse` and
`EditionedFileId` plumbing, add the `RECORDS` constant, `records()` and one `EXTENSION_ATTRS`
row, and make `Parser::records_gate` read `self.inp.extensions_at(self.pos)`. Until then a
macro's expansion parses with no extension, so records cannot be written inside a macro call.

## The type

`{ a: A, b: B }` is `core::record::Record<"a,b", (A, B)>`: one `#[lang = "record"]` struct,
`#[repr(transparent)]` over the tuple of the field types in the order of the field names sorted
by their bytes, with the names joined by `,` as a `&'static str` const argument (`r#type` is the
name `type`). This is the "one generic struct per sorted name list" with the name list as an
argument, as error unions are `ErrorUnion<(A, B)>`. So:

- **Identity** is the sorted names and the field types, nothing else: two records are one type
  exactly when their (name, type) sets are equal, in every crate and every build, because the
  type is an ordinary generic instance of one core item. No new id scheme, nothing to keep
  stable. Defaults are not part of it.
- **Layout and ABI** are the tuple's (`repr(Rust)` over the sorted fields). Code without
  records sees no change. An `extern` function of a non-Rust ABI cannot take or return a record.
- **Traits** come from core, as a tuple's do: `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`,
  `Default`, `PartialOrd` and `Ord` (comparing fields in sorted-name order) when the tuple has
  them, so up to twelve fields beyond `Clone` and `Copy`; `Debug` prints `{ a: 1, b: 2 }`.
- **Impls are a tuple's.** A record is foreign to every crate, as a tuple is: an inherent impl
  (E0116) and an impl of a foreign trait (E0117, the orphan rule) are refused by the rules
  that refuse them on `core`'s `Record`, and a crate's own trait may be implemented for a
  record, as for a tuple. To give a record methods, name it with a struct.
- A duplicate field, a visibility, attribute or `unsafe` on a field, a tuple-index name, and a
  default outside a function parameter are each an error naming the field.

**Values.** A record's type comes from its literal (all fields written) or from an annotation;
inference from field use is not attempted. `r.a` on a type not yet known is the existing "type
must be known" error; `{ a: 1, ..base }` and `{ a: 1, .. }` need a known expected record type
and say so. Functional update takes the rest from a record with the same fields. Field values
are evaluated in the order written, then the base. A pattern binds by field name; without `..`
it names every field.

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
- A default is evaluated as a constant, once, and its value placed at each call that omits the
  field, in the callee's declaration order after the written fields; being constants, order
  and count are unobservable.
- **Key** (rule `keys-cover-every-input`): `record_param_default(callee, parameter index, field
  name)` is the constant. A caller reads it only for a field it omits, so changing one default
  recomputes the callers that omit that field and no other; a default is no part of the
  record's type or of the function's type.
- A call through a function pointer or a `Fn` value has no callee to ask: `..` there is an
  error saying so and naming the pointer's type. A trait method's defaults are its
  declaration's; an impl repeats the types and writes no defaults (an error if it does), so a
  call on a `dyn Trait` or a type parameter has them too (this is where this page departs from
  "a trait object has no defaults": the declaration is known statically).
- No positional-to-named conversion, no reordering of other parameters, no overloading.

## Lints (group `blaze::edition`, allow by default)

| lint | flags | fix |
|---|---|---|
| `blaze::many_bools` | a call passing two or more bare `true`/`false` literals positionally | none: suggests a record parameter |
| `blaze::options_struct` | a function whose last parameter is a struct of the same crate that implements `Default`, has only public fields and is named in no other signature | none in this slice |

`options_struct` judges the declaration only; "built with struct-update syntax at call sites"
needs every call site in the workspace and is left out.

## MIR and codegen

Nothing new past MIR building: a record is an ADT instance over a tuple. MIR building learns
three things, all in `hir-ty-mir`'s lowering: a record value is the struct aggregate of a tuple
aggregate (defaults as constant operands), `r.a` is the projection `.0` then the tuple field,
and a record pattern projects the same way. Places that match on a field access's base and
learn the extra projection: `mir/lower/as_place.rs`, `mir/lower/pattern_matching.rs`, the
closure capture walk (`infer/closure/analysis/expr_use_visitor.rs`), match checking
(`hir-ty-matchck/src/lower.rs`). Borrow checking, drop elaboration, layout and codegen see a
struct and a tuple.

## Use in blaze

None yet: blaze's crates are built by a stage 0 that does not read records (rule
`blaze-uses-its-extensions`, the stage-0 limit). First, once a release reads them:
`SourceFile::parse_shared_with(text, edition, extensions)` and its outline twin (`crates/syntax`),
`Reparse::parse(text, edition, extensions, outline, stored)` (`crates/base-db/src/reparse.rs`),
and `build_node(source, range, lexed, output, edition, extensions, kind)`
(`crates/syntax/src/parsing.rs`).

## Tests

`crates/syntax/src/tests/records.rs` (forms, near misses, the gate), `crates/hir-ty/src/tests/records.rs`
(one type in any order, traits, values, defaults, errors, the default key), `crates/hir-ty-mir`
(evaluation), and a Hegel property of the canonical form with two planted-bug controls.
