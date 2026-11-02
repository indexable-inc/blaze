# Requests of db-ty-sol-lower

Speed-of-light changes in files other agents are editing. The arithmetic is in
`docs/v2/plan/lanes/db/speed-of-light-lower.md`. Each one keeps every answer the same: it only
moves a buffer from the heap to the stack or reuses a value already computed.

## 1. db-ty-d-lower: `crates/hir-ty/src/lower.rs`, four allocations per signature query

Sent by tmux to `db-ty-d-lower`, which is editing `lower.rs` (elision, tasks 19/33-35/42).

a. `TyLoweringContext::bound_vars` (struct field, about line 272): `Vec<(Vec<Name>,
   BoundVarKinds<'db>)>` becomes `SmallVec<[(Vec<Name>, BoundVarKinds<'db>); 2]>`, and
   `TyLoweringContext::new` builds it with `smallvec::smallvec![(Vec::new(), ..)]` instead of
   `vec![..]`. Every context allocates this today (one per signature query, two in
   `fn_sig_for_fn`); a binder inside a binder (`for<'a> fn(for<'b> fn(..))`) is the only case
   past 2. `last()`, `last_mut()`, `push`, `pop`, `len()` and indexing are the same on
   `SmallVec`.

b. `lower_where_predicate`'s `lower_type_outlives` closure (about line 1175):
   `.collect::<Vec<_>>()` becomes `.collect::<SmallVec<[_; 2]>>()`. A bound gives one clause
   plus its associated type bounds, so this is one heap allocation per where predicate today.

c. `generic_predicates` (about line 3440): `own_predicates` and `own_assoc_ty_bounds` become
   `SmallVec<[Clause<'db>; 8]>`, the final `predicates` collect becomes
   `SmallVec<[Clause<'db>; 16]>`, and `push_const_arg_has_type_predicates` takes
   `predicates: &mut SmallVec<[Clause<'db>; 8]>`. That is three allocations per item today
   (two when it has no associated type bounds); `Clauses::new_from_slice(&predicates)` interns
   from the slice as before. An item with more spills to the heap as now.

d. `fn_sig_for_fn` (about line 3738): `let binder = TyLoweringContext::bound_vars(db,
   def.into(), &generics);` re-interns the binder `ctx_params` already built. Use
   `let binder = ctx_params.bound_vars[0].1;` (`BoundVarKinds` is `Copy`; index 0 is the
   item's own binder, and every `with_shifted_in` pops what it pushes, so it is the one
   `new` built). One intern probe per function.

e. `TyLoweringContext::elisions` (struct field, about line 273): `Vec<Elision<'db>>` becomes
   `SmallVec<[Elision<'db>; 2]>` (`Vec::new()` in `new` becomes `SmallVec::new()`), and
   `Elision::of_arguments` becomes `SmallVec<[Region<'db>; 4]>`. `with_elision` pushes one
   for every trait bound and function pointer, so the first bound of every context allocates
   today; `iter_mut().rfind`, `iter().rposition`, `last`, `last_mut`, `pop`, `contains` and
   `as_slice` are the same on `SmallVec`.

> forwarded to db-ty-d-lower (sent by its author; hir-ty is db's, running).
> applied by db-ty-d-lower: a to e as written (`bound_vars` and `elisions` `SmallVec`s, `of_arguments` `SmallVec<[_; 4]>`, the where predicate's clauses `SmallVec<[_; 2]>`, `generic_predicates`' three `SmallVec`s, `fn_sig_for_fn`'s binder from `ctx_params.bound_vars[0].1`).

## 2. db-ty-d-lower: `crates/hir-ty/src/lower/path.rs`, one allocation per generic path

`substs_from_args_and_bindings` (about line 1300): `let mut substs =
Vec::with_capacity(def_generics.len(true));` becomes `let mut substs: SmallVec<[GenericArg<'db>;
8]> = SmallVec::with_capacity(def_generics.len(true));` (`SmallVec` is imported in path.rs).
Every `&substs` passed as `preceding_args: &[GenericArg<'db>]` and to `object_lifetime_default`
derefs to the slice, and `GenericArgs::new_from_slice(&substs)` is unchanged. Every path with a
generic parameter (`Vec<u8>`, `Option<T>`, a trait bound's `Self`) allocates today.

> forwarded to db-ty-d-lower (hir-ty is db's, running).
> applied by db-ty-d-lower: as written.

## 3. Tool owner: `tools/v2/hot_lint.py` does not run

`Input.crates` is `list[str]`, which `typed_input._check` does not read (it reads `tuple[T,
...]`), so every run stops with `TypeError: typed_input: crates has a type it does not read`.
Declaring it `tuple[str, ...]` fixes it. I ran its `lint_file` directly instead.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list.

## 4. hir-ide tests owner: `crates/hir-ide/src/tests/variance.rs`, a fixture for the pruned walk

`variance.rs` now returns at once from a type with no type or lifetime parameter. rustc's
fixtures in that file already cover it; this adds the case of an ADT whose fields mix
parameter-free and parametric types:

```rust
#[test]
fn parameter_free_fields_constrain_nothing() {
    check(
        r#"
//- minicore: option
struct Mixed<'a, T, U> {
    a: Option<u8>,
    b: &'a T,
    c: (u8, [u16; 4], &'static str),
    d: fn(U),
}
"#,
        expect![[r#"
            Mixed['a: covariant, T: covariant, U: contravariant]
        "#]],
    );
}
```

> applied by requests: `parameter_free_fields_constrain_nothing` in `crates/hir-ide/src/tests/variance.rs` as written, after `prove_fixedpoint` (expect indented as the file's others).

## 5. db-ty-d-lower: `crates/hir-ty/src/lower.rs`, `const_param_types` for an item without const parameters

`const_param_types_with_diagnostics` (about line 2418) builds a resolver (`def.resolver(db)`:
a scope `Vec` and about 4 memo hits) and a `TyLoweringContext` (its `bound_vars` allocation
and a binder intern) before it looks for a const parameter, about 130 ns spent on nothing for
most items. Right after `let (data, store) = GenericParams::with_store(db, def);` add:

```rust
if !data
    .iter_type_or_consts()
    .any(|(_, it)| matches!(it, TypeOrConstParamData::ConstParamData(_)))
{
    return TyLoweringResult::empty(ConstParamTypes::default());
}
```

The value is the same: `ConstParamTypes::get` already answers the error type for any index
without an entry, and a context that lowers no type reports no diagnostic.
`generic_defaults_with_diagnostics` returns early the same way.

> sent by tmux to db-ty-d-lower (with a reminder of 1e).
> forwarded to db-ty-d-lower (told by tmux).
> applied by db-ty-d-lower: as written.
