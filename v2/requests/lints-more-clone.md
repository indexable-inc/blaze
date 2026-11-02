# Requests of lints-more-clone (crates/blaze-lints, child of lane `lints`)

## lints-driver (`crates/blaze-lints/src/lints/mod.rs`, `check.rs`, `tests.rs`)

- What, in `lints/mod.rs`:
  - `mod clone_on_ref_ptr; mod format_collect; mod implicit_clone; mod inefficient_to_string;
    mod redundant_clone; mod str_to_string; mod unnecessary_to_owned;` and
    `#[cfg(test)] mod more_clone_tests;`.
  - In `implemented!`, all `Late`: `CloneOnRefPtr = "clone_on_ref_ptr"`,
    `FormatCollect = "format_collect"`, `ImplicitClone = "implicit_clone"`,
    `InefficientToString = "inefficient_to_string"`, `RedundantClone = "redundant_clone"`,
    `StrToString = "str_to_string"`, `UnnecessaryToOwned = "unnecessary_to_owned"`. With
    lints-more-types' 12 that is 43 lints: `On` must be `u64`.
  - From `body_lints`' walk, for every node written in the file or handed to a macro (each
    casts to `ast::MethodCallExpr` inside and returns for anything else):
    `clone_on_ref_ptr::check(cx, node)`, `format_collect::check`, `implicit_clone::check`,
    `inefficient_to_string::check`, `redundant_clone::check`, `str_to_string::check`,
    `unnecessary_to_owned::check`. Each has the shape
    `pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>)`.
- What, in `tests.rs`: `pub(crate) fn lint_with(fixture: &str, flags: &str, conf: Conf) -> String`,
  `lint` as it is with the `Conf` given rather than `Conf::default()` (and `lint` calling it with
  `Conf::default()`).
- Why: the root `Cargo.toml`'s `[workspace.lints.clippy]` sets these seven and blaze's clippy had
  none of them. `lint_with`: `inefficient_to_string` says nothing unless the crate keeps to a
  Rust before 1.82 (`msrv`), which only a `Conf` sets; `more_clone_tests.rs` asks it with
  `Conf { msrv: Some(Version(1, 81, 0)), ..Conf::default() }`.
- `Cx` used, all as `docs/v2/plan/lanes/lints/cx.md` has them: `on`, `db`, `conf`, `krate`,
  `root`, `range`, `snippet`, `source_of`, `span`, `is_written` (through `utils::method_call`),
  `expansion`, `std_macro`, `ty`, `adjusted_ty`, `adjustments`, `method`, `path`, `local`,
  `famous`, `lang_trait`, `std_def`, `std_trait`, `assoc_ty`, `lint`. No addition.

> told lints-driver by tmux (2026-10-29).
> applied by lints-driver (checked: the seven `mod`s in `lints/mod.rs`, `tests.rs` `lint_with`) (requests, pass 18).
