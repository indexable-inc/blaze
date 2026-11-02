# Requests of db-ty-infer

## 1. db-ty (parent): `crates/hir-ty-mir/src/mir/lower.rs` reads `expr_adjustments` as a slice

What: `InferenceResult::expr_adjustments` is now a CSR column (`infer::columns::Csr`); its
`get(&expr)` gives `Option<&Run<Adjustment>>`, and `Run<T>` derefs to `[T]` but is unsized, so
`.cloned().unwrap_or_default()` no longer compiles. The one change:

```rust
// crates/hir-ty-mir/src/mir/lower.rs, ~3200
self.infer.expr_adjustments.get(&expr_id).map(|it| it.to_vec()).unwrap_or_default()
```

Where: `crates/hir-ty-mir/src/mir/lower.rs` around line 3200 (the only caller outside hir-ty
that does not compile against the columns; every other use of `expr_adjustments`,
`pat_adjustments`, `type_of_*`, `binding_modes`, `coercion_casts`, `closures_data` and the
accessors is unchanged in name, signature and meaning).
Why: the result's columns (db-ty-shared.md, db-ty-infer's part).

> forwarded to codegen: `crates/hir-ty-mir/` is lane codegen's, not db-ty's (`crates.md`).
> applied by codegen-rq-mir (2026-10-29): `lower_scalar_operand` (`lower.rs`) reads
> `self.infer.expr_adjustments.get(&expr_id).unwrap_or_default()`, the borrowed `&[Adjustment]`
> (`Csr::get` gives `Option<&[T]>`), not a `to_vec()` copy: nothing is allocated, and `rest` goes
> to `lower_expr_to_place_with_adjust` as the slice it takes. The other nine reads in hir-ty-mir
> already took the slice.

## 2. db-ty-hir: a fixture test of the inference record's round trip through the type table

What: one test in `crates/hir/src/tests.rs` (its `TestDB` pattern), for each body of the fixture
below: `let result = InferenceResult::of(db, def)`; `let record = result.to_record(interner)`;
`let bytes = blaze_format::Aligned::copy_from(&blaze_format::to_bytes(&record))`;
`let read = <InferenceRecord as blaze_format::Persist>::read(&bytes).unwrap()`;
`assert_eq!(read.to_result(interner).unwrap(), *result)`, and the same for
`record.as_ref().to_result(interner)`. `InferenceRecord` and `InferenceRecordRef` are in
`hir_ty::infer::persist`. The fixture holds a method call with generic arguments, a field and a
tuple field access, autoderef and autoref adjustments, an unsizing coercion, a closure capturing
by reference and by move, a `ref mut` binding, a match on a reference (match ergonomics), an
inline const, an `impl Trait` return, and two errors (an unresolved method, a type mismatch):

```rust
//- minicore: deref, coerce_unsized, fn, sized, index, slice
struct S { a: u32, b: (u8, bool) }
impl S { fn get<T: Default>(&self) -> T { T::default() } }
trait Tr { fn m(&self) -> u32; }
impl Tr for u32 { fn m(&self) -> u32 { *self } }
fn opaque() -> impl Tr { 5u32 }
fn f(s: &S, v: &mut Vec<u32>) -> u32 {
    let x: u8 = s.get::<u8>();
    let (ref mut y, _) = s.b;
    let slice: &[u32] = &[1, 2, 3];
    let mut count = 0;
    let mut add = |n: u32| count += n + s.a;
    add(1);
    let moved = move || x;
    match &s.b { (0, flag) => { let _: &bool = flag; } _ => {} }
    let k = const { 3 + 4 };
    s.missing();
    let wrong: bool = 1u32;
    opaque().m() + slice[0] + moved() as u32 + k
}
```

Where: `crates/hir/src/tests.rs`.
Why: db-ty-shared.md rule 4 asks a round-trip test for each `Persist` value; the one in
`crates/hir-ty/src/infer/persist.rs` covers every column, diagnostic kind and capture with a
list of types in place of the table (it needs no database), so the table itself
(`TyTableWriter`/`TyTableReader`) and `SolverDiagnostic::TraitUnimplemented` (whose trait
reference needs an interner) are covered only by a fixture test.

> forwarded to db: `crates/hir/` is lane db's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir/src/tests.rs:963` (`an_inference_record_round_trips_through_its_type_table`: the fixture, `to_record`, `Persist::read`, `to_result` and `as_ref().to_result`).

## 3. db-ty-types: `next_solver::persist::{TyTableWriter, TyTableReader}`, as the contract says

What: the API of db-ty-shared.md rule 2, unchanged. `crates/hir-ty/src/infer/persist.rs` uses
`TyTableWriter::new`, `.ty`, `.args`, `.write(&mut Writer) -> u64` and `TyTableReader::open`,
`.ty`, `.args`, writing the table into a buffer of its own (schema
`hir_ty::infer::persist::TyTable`) kept beside the columns as `u128`s, as
`lower/persist.rs` does.
Where: `crates/hir-ty/src/next_solver/persist.rs` (new), `pub mod persist;` in `next_solver.rs`.
Why: nothing else in the record depends on the interners' dense indices.

> forwarded to db: between lane db's children.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/src/next_solver/persist.rs:1770` (`TyTableWriter::{new, ty, args, write}`) and `:1888` (`TyTableReader::{open, ty, args}`); `pub mod persist;` at `next_solver.rs:26`.

## 4. db-ty-lower: share the lowering diagnostics' words

What: make `put_diagnostic` and `Words::diagnostic` (with `put_path_diagnostic`,
`Words::path_diagnostic` and their helpers) in `crates/hir-ty/src/lower/persist.rs`
`pub(crate)`, so `infer/persist.rs` writes `TyLoweringDiagnostic`/`PathLoweringDiagnostic`
through them instead of its own copy. The two encodings are word for word the same today, so the
change is to replace `Sink::lowering_diagnostic`/`path_diagnostic` and
`Source::lowering_diagnostic`/`path_diagnostic` in `infer/persist.rs` by calls to them (db-ty-infer
does it once they are reachable).
Where: `crates/hir-ty/src/lower/persist.rs`.
Why: one encoding of a type in one place.

> forwarded to db: between lane db's children.

> applied (db-ty): reachable as `crate::lower::persist::*`. db-ty-infer has switched to them and
> dropped its copy.

## 5. db-ty-db: the inference result's cold form

What: when `infer`/`infer_anon_const` results are evicted to the store, their cold form is
`InferenceResult::to_record(interner) -> InferenceRecord` (`impl blaze_format::Persist`), read
back with `InferenceRecordRef::to_result(interner) -> Result<InferenceResult<'db>, FormatError>`;
a read that fails is an error naming the record, never a recomputation in its place.
Where: `crates/hir-ty/src/db.rs`, `evict.rs`.
Why: db-ty-shared.md rule 4, values in the format.

> forwarded to db: between lane db's children.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/src/check_cache/part.rs:427`: the stored form is `InferenceRecord`, read with `to_result` and a failed read an error (`map_err(format_error)`), never a recomputation. `evict.rs`'s `drop_inference` drops the in-memory value only (salsa recomputes and backdates by fingerprint, the design of `docs/v2/plan/lanes/db.md` section 7), so nothing there needs the record.

## 6. Notes (no change asked)

- The pending obligations are `FulfillmentCtxt`'s (`next_solver/fulfill.rs`, db-ty-solver's).
  Inference adds and selects through `InferenceTable`/`ObligationCtxt` as before, so their queue
  is db-ty-solver's column; db-ty-infer uses its API unchanged.
- The diagnostics lane's request for an `unreachable()` pass (`v2/requests/diagnostics.md`,
  around line 165) is a semantic port outside db-ty-infer's part (columns and the record form);
  db-ty to assign.

> applied: noted; no change asked.

## 7. codegen (via db-ty) and db: CSR column lists are plain slices now

What: for `docs/v2/plan/design/unsafe.md`, `Run<T>` (an unsized wrapper over `[T]`, made by two
unsafe pointer casts, for no speed) is gone. `Csr::get` now gives `Option<&[T]>`. The callers
that wrote `&**it` to get from the old `&Box<[T]>` to `&[T]` take `it` as it is:

- `crates/hir-ty-mir/src/mir/lower.rs` ~2734: `.map_or(&[][..], |it| it)` (or `.unwrap_or_default()`).
- `crates/hir-ty-mir/src/mir/lower/match_tree.rs` ~285 and `pattern_matching.rs` ~311: the same.
- `crates/hir-ty-mir/src/mir/lower.rs` ~3200: request 1 still holds, `.map(<[_]>::to_vec)`.
- `crates/hir-ide/src/tests.rs` ~267: `.unwrap_or_default()` in place of
  `.map_or_else(Default::default, |it| &**it)`.
- `crates/hir-ide/src/diagnostics/match_check.rs` ~120: `.unwrap_or_default()` in place of
  `.map(|it| &**it).unwrap_or_default()`.

Every other use (`&it[..]`, `.iter()`, `.last()`, `.split_last()`, passing it as `&[T]`) is
unchanged. hir-ty's own callers are done.
Where: the lines above. `crates/hir-ty-mir/` is codegen's and `crates/hir-ide/` is db's.
Why: unsafe.md "Where unsafe is allowed".

> applied (hir-ide is the coordinator's): `tests.rs` and `diagnostics/match_check.rs` use `.unwrap_or_default()` on the slice. forwarded to codegen for the three `hir-ty-mir` sites.

> applied by codegen (lead, 2026-10-29): the three `hir-ty-mir` sites take the slice as it is, `.get(..).unwrap_or_default()`: `mir/lower.rs:3007` (`expr_adjustments`), `lower/match_tree.rs:440` and `lower/pattern_matching.rs:320` (`pat_adjustments`). No `&**it` is left in hir-ty-mir's lowering.

## 8. format (lead): a safe byte view of a `Record` slice

What: `pub fn bytes_of<T: Record>(values: &[T]) -> &[u8]` in `blaze_format` (the cast
`Writer::push_slice` already makes, lib.rs ~528). It would remove one copy of the type table
per decode in `infer/persist.rs`'s `table_bytes`, which now copies the words into an `Aligned`
buffer so that it needs no unsafe. Also, `crates/hir-ty/src/lower/persist.rs:190` (db-ty-lower's)
keeps its own unsafe `bytes_of` over `u128`s, which reads the words in native byte order while
they are written little-endian. Take this only if a profile shows the copy matters.
Where: `crates/blaze-format/src/lib.rs`.
Why: unsafe.md: unsafe in small modules behind a safe API, only for measured speed.

> forwarded to format.

## 9. safety (safety-fuzz): the inference record is a parser of untrusted bytes

No `unsafe` remains in `crates/hir-ty/src/infer.rs`, `infer/` or `autoderef.rs`, and both
modules say `#![forbid(unsafe_code)]` (hir-ty as a whole has unsafe elsewhere, so the crate
attribute is not db-ty-infer's). The record is safe code, so the target is for panics, hangs and
memory that grows faster than the input:

- `<InferenceRecord as blaze_format::Persist>::read(&[u8])`, then `InferenceRecordRef::decode`
  with the tests' `Model` codec (no database needed).
  - Fuzz target: arbitrary bytes, and a valid record with one word of `u64s`, `u32s` or `bytes`
    changed. The result must be an error or a result whose record writes back to the same columns.
    It must never panic.
  - Model: `a_result_with_every_column_round_trips` and `truncated_words_are_refused_not_misread`
    in `infer/persist.rs`'s tests (the seed corpus is their records).
  - Bounds: every list length is bounded by the words left (`Source::len`), and no column is
    sized by an index it reads, so memory is at most a fixed multiple of the input.

> forwarded to safety (safety-fuzz).
