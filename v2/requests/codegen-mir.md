# Requests from `codegen-mir`

hir-ty-mir is written against these. Until 1–4 land it does not build; 5–9 are what other
crates must change to keep building and to lower the new MIR.

## 1. hir-ty `src/mir.rs` (interface types; db-ty)

- `AssertKind` (line 383): add rustc's
  ```rust
  MisalignedPointerDereference { required: Operand, found: Operand },
  NullPointerDereference,
  NullReferenceConstructed,
  InvalidEnumConstruction(Operand),
  ```
  with arms in `for_each_operand_mut` (405) and `for_each_operand` (421): `required` then `found`;
  the operand of `InvalidEnumConstruction`; nothing for the two null ones.
- `CastKind` (line 884): add `Transmute` (rustc's `CastKind::Transmute`: same size, bits kept).
- Beside `MirOptLevel` (line 2184):
  ```rust
  /// What a MIR body is optimized for: the level, whether rustc's UB checks are inserted
  /// (`sess.ub_checks()` of the crate the body is of), and whether the code generator emits
  /// lifetime markers (rustc removes storage markers only when it doesn't).
  #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, salsa::SalsaValue)]
  pub struct MirOptions { pub level: MirOptLevel, pub ub_checks: bool, pub lifetime_markers: bool }

  impl MirOptions {
      /// The interpreter's: no UB checks, markers as rustc keeps them without a code generator.
      pub fn at(level: MirOptLevel) -> MirOptions {
          MirOptions { level, ub_checks: false, lifetime_markers: level >= MirOptLevel::Release }
      }
  }
  ```
  (whatever derive `MirOptLevel` uses to be a query argument; `MirOptLevel` needs `PartialOrd`
  for `at`, or compare with `matches!(level, MirOptLevel::Release)`.)

> forwarded to db (db-ty): `crates/hir-ty/src/mir.rs` is its.
> applied by db-ty-d-mir: the four `AssertKind`s (with arms in both `for_each_operand*`: `required` then `found`, the operand of `InvalidEnumConstruction`, nothing for the null ones), `CastKind::Transmute`, and `MirOptions` with `MirOptions::at` beside `MirOptLevel`, as written. Tests in `mir.rs`: `the_ub_check_asserts_visit_their_operands_in_order`, `the_interpreter_keeps_lifetime_markers_only_at_release`.

## 2. hir-ty `src/db.rs` and hir-ide `src/impl_db_macro.rs` (db-ty-db)

The level argument of the optimized queries becomes `options: MirOptions` (db.rs 127, 133, 139,
147; impl_db_macro.rs 121–160 pass it to the same `mir::*_query`). Add four, mapped in
`impl_db_macro.rs` like their runtime twins (106–117, 182–200):

```rust
fn optimized_mir_body_for_coroutine<'db>(&'db self, def: InternedCoroutineId, options: MirOptions) -> Result<'db, ..>;
// -> mir::optimized_mir_body_for_coroutine_query(self, def, options)
fn optimized_mir_body_for_coroutine_by_move<'db>(&'db self, def: InternedCoroutineId, options: MirOptions) -> ..;
// -> mir::optimized_mir_body_for_coroutine_by_move_query
fn monomorphized_optimized_mir_body_for_coroutine<'db>(&'db self, def: InternedCoroutineId, options: MirOptions, subst, env) -> ..;
// -> mir::monomorphized_optimized_mir_body_for_coroutine_query
fn monomorphized_optimized_mir_body_for_coroutine_by_move<'db>(&'db self, def: InternedCoroutineId, options: MirOptions, subst, env) -> ..;
// -> mir::monomorphized_optimized_mir_body_for_coroutine_by_move_query
```
Same return types as the runtime coroutine twins. `crates/hir/src/evict.rs` (46–61): evict the
four new queries too.

> applied (hir-ide): `impl_hir_database!` passes `options: MirOptions` to the four optimized queries and maps the four coroutine twins to `mir::*_query`, as written here. forwarded to db for `hir-ty/src/db.rs` and `hir/src/evict.rs`.
> applied by db-ty-d-mir: `hir-ty/src/db.rs`'s four optimized queries take `options: MirOptions`, and the four coroutine twins are declared with the signatures `impl_db_macro.rs:121-204` implements; `hir/src/evict.rs`'s `drop_mir` evicts the four new queries. hir-ty's `every_query_has_a_kind` scans only hir-ty's source (these are hir-ty-mir's), so it needs nothing; `hir/src/tests.rs`'s `dropped_results_keep_cutoff_and_come_back_equal` runs `drop_mir` with them.

## 3. hir-def `src/lang_item.rs` (beside 611/616) and the symbols

```
PanicNullReferenceConstructed, sym::panic_null_reference_constructed, FunctionId;
PanicInvalidEnumConstruction,  sym::panic_invalid_enum_construction,  FunctionId;
SizeOf,                        sym::mem_size_const,                   ConstId;
AlignOf,                       sym::mem_align_const,                  ConstId;
```
with the four symbols interned in `intern`'s `sym`. (core: `SizedTypeProperties::SIZE`/`ALIGN`
carry `#[lang = "mem_size_const"]`/`"mem_align_const"`; rustc's checks read them as
unevaluated constants; the collector already takes associated consts, as for `RangeMin`.)

> forwarded to db (`hir-def/src/lang_item.rs`; `intern`'s `sym` is format's, db asks it or adds the four symbols with the lang items).
> applied by db-expand: `PanicNullReferenceConstructed`, `PanicInvalidEnumConstruction` (`FunctionId`), `SizeOf` (`sym::mem_size_const`) and `AlignOf` (`sym::mem_align_const`, `ConstId`) in `hir-def/src/lang_item.rs`, names checked in nightly-2026-10-25's `library/core`. Test `check_and_layout_lang_items_are_found`. The frozen encoding's version moves to `hir_def::nameres::frozen::ENCODING` (now 8, since the lang item list is in the bytes). What waits: format's four symbols in `intern/src/symbol/symbols.rs` (hir-def doesn't compile without them), and codegen's `blaze-engine/src/frozen.rs` key hashing `ENCODING` (asked in `v2/requests/db-expand.md`).

## 4. Other exhaustive matches on `AssertKind`/`CastKind` outside hir-ty-mir

- `crates/blaze-items/src/form.rs:2630` hashes `AssertKind` into the code-identity hash: each
  new variant needs its own tag and its operands hashed (a missed variant is a stale-code bug).
  Hash `CastKind::Transmute` likewise wherever casts are hashed.
- `crates/blaze-diagnostics/src/lints/bodies/panics.rs:518`: the new variants (they never come
  from lowering, only from the checks; no lint).
- `crates/hir-ty-codegen/fncache/dispatch.rs:399`, `mod.rs:12418` (casts) and `mod.rs:13362`
  (asserts): see 5.
- hir-ty and hir-ty-borrowck: any exhaustive match on either enum.

> applied (items DONE): `blaze-items/src/form.rs` hashes the four new `AssertKind`s with tags 5-8 and their operands; casts hash `CastKind` by `Debug`, so `Transmute` is in already. forwarded to diagnostics (`panics.rs:518`), codegen (`hir-ty-codegen`), db (hir-ty, hir-ty-borrowck).
> applied by db-ty-d-mir: nothing to change in hir-ty, hir, hir-ty-borrowck or hir-ty-wfcheck: outside `mir.rs` (done) no match on `AssertKind` and `CastKind`'s one (`hir-ty-borrowck/src/regions.rs:4789`) ends in `_ => ()`.
> applied by codegen (2026-10-29), checked by the lead: hir-ty-codegen's part is 5 below (`mod.rs` lowers the four `AssertKind`s and `CastKind::Transmute`, `v2/refs.rs:1053` takes it); `fncache/dispatch.rs:393` matches casts with `_ => false`, so needs no arm.

## 5. Codegen must lower (speed-backend-lower, `crates/hir-ty-codegen/mod.rs`)

- `Assert` with the new messages calls, when the condition fails, with `cleanup` absent
  (rustc's `UnwindAction::Unreachable`: these panics are `#[rustc_nounwind]`), all
  `#[track_caller]` so the caller location goes last as for `panic_bounds_check`:
  - `MisalignedPointerDereference { required, found }` → `panic_misaligned_pointer_dereference(required: usize, found: usize)`
  - `NullPointerDereference` → `panic_null_pointer_dereference()`
  - `NullReferenceConstructed` → `panic_null_reference_constructed()`
  - `InvalidEnumConstruction(source)` → `panic_invalid_enum_construction(source: u128)`
- `CastKind::Transmute`: a bit-for-bit reinterpretation of equal sizes. The checks emit:
  a `Box`'s field 0 (`Unique`/`NonNull` newtype) to `*const T`; a thin `*const ()` to `usize`;
  a value to `[MaybeUninit<u8>; N]`; a value to an unsigned integer of its size.
- The MIR level of a body (mod.rs 3485, 3495, 5353): ask
  `MirOptions { level: mir_opt_level_of(db, krate), ub_checks: debug_assertions(db, krate), lifetime_markers: true }`
  where `krate` is the crate whose MIR it is (rustc's `sess.ub_checks()` of that crate, not
  `ub_checks_of`, which is the `ub_checks()` intrinsic of the calling crate). The runtime body
  stays for `level == Off && !ub_checks` only.
- Coroutine bodies (mod.rs ~3527–3546, the runtime coroutine queries): when `ub_checks`, use
  the new optimized coroutine queries (they insert only the checks; coroutines are not
  optimized further, as today).

> forwarded to codegen.

## 6. Refs walk (codegen-adapter-refs, `crates/hir-ty-codegen/v2/refs.rs`)

- `refs.rs:978` (assert panics): list `panic_misaligned_pointer_dereference`,
  `panic_null_pointer_dereference`, `panic_null_reference_constructed`,
  `panic_invalid_enum_construction` for the new `AssertKind`s.
- `refs.rs:862` (casts): `CastKind::Transmute => Ok(())`.
- The checks read `<T as SizedTypeProperties>::SIZE`/`ALIGN` as unevaluated constants: the walk
  must evaluate or list them like any other unevaluated constant operand.

> forwarded to codegen.

## 7. The adapter's `v2::build::Build`

Carries, per unit, the `MirOptions` its bodies are asked with: the crate's MIR level
(`mir_opt_level_of`), its `ub_checks` (`debug_assertions` of the crate), and
`lifetime_markers: true`. Each is part of the MIR query's key, so nothing else is needed for
the memo to see it.

> forwarded to codegen.

## 8. Callers of the optimized queries

`crates/hir-ide/src/tests/mir/eval.rs:65` and `tests/mir/optimize.rs:69`,
`crates/blaze-engine/src/mirdump.rs:157,176`: pass `MirOptions::at(level)` where they pass
`level`.

> applied: `hir-ide/src/tests/mir/eval.rs` and `optimize.rs` pass `MirOptions::at(level)`. forwarded to codegen for `blaze-engine/src/mirdump.rs`.

## 9. `name_sources` (engine, `crates/blaze-engine/src/lib.rs:827`; parent's)

It now takes the database and a key naming the rule (the same key is the same naming; a new key
re-evaluates what read a name):
```rust
hir_ty_codegen::name_sources(
    db, // &mut dyn HirDatabase
    &format!("{}\0{:?}", named.0.display(), named.1),
    move |db, file| source_path(db, &named.0, named.1.as_deref(), file),
);
```
`Resolver::of` holds a `&RootDatabase` there; the call moves to where the engine still has the
database mutably (before it is shared), or `Resolver::of` takes `&mut`. The re-export at
`hir-ty-codegen/mod.rs:87` stays as is.

> forwarded to codegen (`blaze-engine` is its).

> applied by codegen (parent), 2026-10-29:
> - 5: `hir-ty-codegen/mod.rs` lowers the four new `AssertKind`s (`ub_check_panic_if`: cold
>   block, `core::panicking::*` with the operands then the caller location, no landing pad
>   when `cleanup` is `None`; `call_panic` passes each argument as its own type, so the
>   `u128` of `InvalidEnumConstruction` is an `I128`), `CastKind::Transmute` (a copy of equal
>   sizes, else refused naming both types). Bodies are asked with `MirOptions`
>   (`mir_options_of`, `body_options`), the runtime body only when `level == Off &&
>   !ub_checks`; coroutines use the optimized coroutine queries when `ub_checks`. v1's
>   entry key is version 14.
> - 6: `v2/refs.rs` lists the four panics and takes `Transmute`; unevaluated `SIZE`/`ALIGN`
>   go through the walk's constant evaluation as before.
> - 7: `v2::build::Build::options` is now read: `instance_body(.., build: Option<Build>)`,
>   `Some(Build::WALK)` from the walk, `v2::build::building()` in the IR builder.
> - 8: `blaze-engine/src/mirdump.rs` passes `MirOptions::at(level)`.
> - 9: `blaze-engine` sets the naming rule in `Checkouts::add` (`name_sources`), keyed by
>   every loaded checkout root: one database builds the programs of several checkouts, so
>   the rule names a file by the innermost loaded checkout it is in.

## 10. hir-ty `src/mir.rs` (db-ty): `MirEvalError::Aborted(String)`

The evaluator aborts the program (a panic in a cleanup block, a panic while unwinding, a
panic out of a frame that can't unwind, out of a thread's start or a resumed shim) as
rustc's `panic_cannot_unwind`/`panic_in_cleanup` do. It reports that now as
`MirEvalError::NotSupported("the program aborted: ..")`, which is not what it is. Wanted:
`Aborted(String)` (the message std's abort prints), displayed as "the program aborted: {0}".
Then `unwind.rs`'s `abort` returns it.

> forwarded to db (hir-ty is db's; db-ty-next2 told).
> applied by db-ty (db-ty-next3, 21:35): `MirEvalError::Aborted(String)` after `Panic`; its
> `Debug` prints "the program aborted: {0}". `unwind.rs`'s `abort` is codegen's to switch, and
> hir-ide's `mir_pretty/errors.rs` needs the arm: both in `v2/requests/db-ty.md` "## db-ty-next3".

## 11. hir-def `src/lang_item.rs` (db-def): the `panic_const_*` lang items

A failed `Assert` (overflow, bounds, division by zero, ..) unwinds with the message but
doesn't call core's `panic_const_add_overflow` and the rest, so std's panic hook and a
`#[panic_handler]` don't see it. Wanted: the lang items `panic_const_add_overflow`, ..
(rustc's `AssertKind::panic_function` list) and `panic_bounds_check`,
`panic_misaligned_pointer_dereference`, `panic_null_pointer_dereference`, so the evaluator
calls them as rustc's codegen does.

> forwarded to db (hir-def is db's; db-next2 told).
> forwarded by db-audit-other (22:50) to format (through requests-next3). The `lang_item.rs` lines need 22 `sym::panic_const_*` names in `crates/intern` (format's), so the two halves must land as one change. `v2/requests/db.md` `## format (through requests-next3; from db-audit-other): the panic_const_* symbols and lang items` has the exact text for both and db's go for the `lang_item.rs` half. `panic_bounds_check`, `panic_misaligned_pointer_dereference` and `panic_null_pointer_dereference` are in already (`crates/hir-def/src/lang_item.rs:603-609`). Codegen finds these functions by path (`hir-ty-codegen/v2/refs.rs:1185`), so only the evaluator is left to call them.

## 12. hir-ty `src/infer.rs` (db-ty): value resolutions and adjustments as columns

For MIR building's speed of light (`docs/v2/plan/lanes/codegen/speed-of-light.md`, section 3).
Lowering resolves every path expression a second time through the resolver
(`update_to_inner_scope` + `resolve_path_in_value_ns_fully`, 200 to 500 cycles each), though
inference already resolved it, and finds each expression's adjustments by a hash probe
(`expr_adjustments.get(&expr)`). Wanted in `InferenceResult`:
- the `ValueNs` each path expression and path pattern resolved to, readable by `ExprId` /
  `PatId` (an `ArenaMap`, as the expression types are);
- `expr_adjustments` as an `ArenaMap<ExprId, Box<[Adjustment]>>` (or one flat array with
  per-expression ranges) instead of a hash map.
Then `lower.rs` reads both by index and its resolver calls for values go.

> forwarded to db (`crates/hir-ty/src/infer.rs`) (from requests, pass 16; told by tmux).
> applied (db-ty, checked by db-ty-next3): `InferenceResult::value_resolution(ExprOrPatId) ->
> Option<ValueNs>` (db-ty-d-infer2's task 17, a column by node) and `expr_adjustments` as a CSR
> by `ExprId` (db-ty-infer's columns). `lower.rs`'s switch to them is codegen's.

## dedyn: crates/hir-ty-mir/src/mir/eval: three `dyn` lines kept, justified (from dedyn-codegen, 2026-10-29)

> applied by dedyn-codegen (2026-10-29): the three `// dyn:` lines are in; hot_lint finds no `dyn` in hir-ty-mir.

From `dedyn-codegen` (report `/tmp/blaze-lanes/v2-dedyn-codegen.md`). hot_lint flags these three;
each stays, with a `// dyn:` line inserted above it (indented as the line). No code changes.

1. `eval/shim/reflection.rs`, above `type FieldBytes<'a, 'db, M> = &'a mut dyn FnMut(`:

   ```rust
   // dyn: once per reflected field (`type_info`, once per constant), which allocates; ~20 closures would each copy the walkers
   ```

2. `eval/shim/reflection.rs` `reflected_slice`, above
   `element: &mut dyn FnMut(&mut Self, Ty<'db>, usize) -> Result<'db, Made<M::Shadow>>,`:

   ```rust
   // dyn: once per element of a reflected slice, which allocates; as `FieldBytes`
   ```

3. `eval/unwind.rs` `payload_message`, above `Ok("Box<dyn Any>".to_owned())`:

   ```rust
   // dyn: rustc's text for a payload that is no string, not a trait object
   ```

`mir/location.rs`'s `type SourcePaths = dyn Fn(..)` (codegen-mir-store's) already has its
`// dyn:` line.

> forwarded to codegen (codegen-mir-next2, `crates/hir-ty-mir/`) (requests, pass 20; told by tmux).

## 13. To codegen-adapter-refs (`crates/hir-ty-codegen/v2/refs.rs`): inlined callees (2026-10-29)

`optimize`'s doc now says a Release-only pass adds no call, constant, static, drop or vtable a
`Debug` body lacks (`tests/mir/references.rs` holds it with `inline` off; adapter request 6).
Once codegen-inline's pass is registered, a Release body with `MirOptions::inline` has its
callees' references in it. They are still reached from `Debug`, but only through the callee's
own body, so the walk must visit each `body.inlined.callees[..]` body as it visits a call.

> forwarded to codegen (`hir-ty-codegen/v2/refs.rs`; codegen-v2-next1 told by tmux; requests, pass 23).
> applied by codegen-v2 (2026-10-29): already in the tree, no change. For an instance a unit inlines into, `refs::code_refs` walks its body a second time at `Build::WALK_INLINED` (`inlines: true`), so the callees' calls, constants, statics, drops and vtables in the inlined blocks are listed after the plain walk. `Walk::inlined_callees` then lists each `body.inlined.callees[..]` as built in. Held by `v2/tests.rs` `the_walk_lists_what_inlined_code_calls` (inlined `f`'s call of `g` is listed only by the inlined walk, and the inlined IR's requests place only against it) and `a_callee_inlined_twice_is_built_in_once`.
