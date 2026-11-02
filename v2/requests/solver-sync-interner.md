# solver-sync-interner: requests outside `next_solver/{interner.rs, interner/, def_id.rs, generics.rs, abi.rs, opaques.rs, util.rs}`

From `solver-sync-interner`, a child of `solver-sync` (port of `impl Interner for DbInterner` to
nightly-2026-10-27's `rustc_type_ir`).

## 1. `intern`: a declared `anon` symbol

- Where: `crates/intern/src/symbol/symbols.rs`, the `@PLAIN:` list, between `allow,` (`:200`)
  and `any,` (`:201`).
- What: add the line `    anon,`.
- Why: the nightly's `inherent::Symbol` has `const SYM_ANON: Self` (rustc's `sym::anon`, what
  `Region::get_name_or_anon` names an unnamed region). `next_solver/interner.rs` implements it as
  `const SYM_ANON: Self = Symbol(sym::anon);`, which needs `intern::sym::anon` as a `const`;
  the `@PLAIN` list gives exactly that. Its test `the_keyword_symbols_are_rustcs` checks the
  text is `"anon"`. Owner: format (`docs/v2/plan/reference/crates.md`).

> forwarded to format: `crates/intern/` is its.

## 2. `region.rs` (solver-sync-kinds): `EarlyParamRegion::get_name` (told by tmux; applied, `region.rs:256`)

- Where: `crates/hir-ty/src/next_solver/region.rs:256-258`.
- Old: `Some(Symbol)`. `Symbol` is now `pub struct Symbol(pub intern::Symbol)`, so this no
  longer compiles.
- New (rustc's rule, the declared name, `None` for an elided one):

  ```rust
  fn get_name(&self, interner: DbInterner<'db>) -> Option<Symbol> {
      let name = *hir_def::hir::generics::GenericParams::of(interner.db, self.id.parent)
          [self.id.local_id]
          .name
          .symbol();
      (name != intern::sym::tick_underscore).then_some(Symbol(name))
  }
  ```

> applied: by solver-sync-kinds (`region.rs:256`), as its heading says.

## 3. hir-def: record `type const` items (`min_generic_const_args`)

- Where: `crates/hir-def/src/signatures.rs` `ConstSignature` (and the trait/impl associated
  const lowering that builds it). The parser already parses `type const`
  (`crates/parser/src/grammar/items.rs:283`, test `type_const`); hir-def drops the keyword.
- What: a flag `ConstFlags::TYPE_CONST` (or `is_type_const: bool`) set when the item is written
  `type const`.
- Why: the nightly's `Interner::is_direct_const(AliasConstKind)` is rustc's `is_type_const`, and
  `const_of_item` returns the item's right-hand side as a const argument for such an item. Until
  hir-def records the keyword, `interner.rs` answers `false`/`None` for every const, so a
  `type const` is evaluated like a plain const (`evaluate_const`), which is rustc's rule for every
  item that is not `type const`. Once the flag exists, `is_direct_const` returns it, and
  `const_of_item` returns the lowered right-hand side for those items (that lowering, a const
  argument to a `Const`, is the lower lane's). Owner: hir-def (format for `hir/`, db for
  `signatures.rs`).

> forwarded to db: `hir-def/src/signatures.rs` is its.

> not applied by db-solver (2026-10-29 23:45): waits on hir-def's `ConstFlags::TYPE_CONST` (not in `signatures.rs` yet). `is_direct_const` / `const_of_item` (`interner.rs` ~1448) stay rustc's rule for every const not written `type const`, which until the flag is every const hir-def records.

## 4. Tests that need a database (hir-ide, which has the test fixture)

hir-ty has no test database; `crates/hir-ide/src/tests/` does. Rules ported here that only a
fixture can exercise, for whoever owns those tests (solver-sync-callers collects):

- `GenericsOf::param_region_def_id`: for `fn f<'a, T>()` and an impl method `fn g<'b>()` in
  `impl<'a> S<'a>`, the early region at each index maps to the declaring item (`f`, the impl,
  `g`), and a non-lifetime index panics.
- `Interner::item_name`: a struct, function, const, trait and enum variant give their names; an
  impl, closure and anonymous const give `'_` (so `BoundRegionKind::Named` of them has no name).
- `Interner::check_alias_term_args_compatible`: an inherent associated type `S::Assoc<u8>` in
  "Self form" (args `[S, u8]`) holds, `[u8]` (no self) and `[S]` (one missing) do not; a
  projection holds only with the trait's and the item's every parameter.
- `Interner::type_of` for an `ImplId` (the impl's self type), a `ConstId`, a `StaticId` and an
  `AnonConstId` (its type).

> taken by requests (child `requests-solver-tests`, new `crates/hir-ide/src/tests/next_solver_items.rs`); the outcome follows when it is done.
> applied: `param_region_def_id_is_the_declaring_item`, `param_region_def_id_at_a_type_parameter_panics` (`#[should_panic]`), `item_name_is_the_declared_name_or_anonymous`, `alias_term_args_compatible_by_form` and `type_of_impls_consts_statics_and_anon_consts`, in `crates/hir-ide/src/tests/next_solver_items.rs` (`mod next_solver_items;` in `crates/hir-ide/src/tests.rs`); not yet compiled.
> forwarded to requests-solver-tests: a child of `requests` (another `requests` agent started it; my own duplicate, requests-interner, was stopped before it wrote anything).
