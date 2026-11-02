# Requests of `diagnostics-lints-names-items-speed`

## 1. For `diagnostics-lints-names` (parent): one walk of the crate for the three late entry points

DONE: granted by the parent. `items::Walk::of(cx)` walks once, `names::after_abort` passes
`&walk` to all three entry points, and `items.rs` takes it. The proposal below is kept as
written.

Where: `crates/blaze-diagnostics/src/lints/names.rs` and `lints/names/items.rs`'s interface.

What: `names::after_abort` calls `items::private_in_public`, `items::after_abort` and
`items::clashing_extern_declarations`, and each of them calls `walk::items(cx)`: the crate's
items are walked, parsed, macro-expanded and `cfg`-tested three times after the abort (four
with `before_abort`). Proposed interface:

```rust
// items.rs
pub(super) struct Items(Vec<walk::Item>);
pub(super) fn walk(cx: &Cx<'_, '_>) -> Items;
pub(super) fn private_in_public(cx: &Cx<'_, '_>, items: &Items) -> Vec<Found>;
pub(super) fn after_abort(cx: &Cx<'_, '_>, items: &Items) -> Vec<Found>;
pub(super) fn clashing_extern_declarations(cx: &Cx<'_, '_>, items: &Items) -> Vec<Found>;
```

with `names::after_abort` doing `let items = items::walk(cx);` once, before `private_in_public`.
`before_abort` runs in `lints::early`, before the abort, and keeps its own walk unless `Cx` gets
a per-check slot for it.

Why: the walk is the largest fixed cost of these passes (one `parse_or_expand` per module and
macro call, one `cfg` test per attributed node). Until this lands, each entry point walks once
and shares that walk among all of its lints.

> forwarded to diagnostics (told by tmux).

## 2. For `diagnostics` / `diagnostics-lints-levels`: which lints can fire (same as imports' request 8)

Where: `crates/blaze-diagnostics/src/cx.rs` (`Cx`).

What: `cx.lint_may_fire(name) -> bool`, from a per-crate bitset over lint ids, false when the
command line and every attribute in the crate leave the lint allowed.

Why: most of this lane's lints are allow-by-default (`missing_docs`, `unreachable_pub`,
`missing_copy_implementations`, `missing_debug_implementations`, `single_use_lifetimes`,
`unused_lifetimes`, ..), and their passes run anyway. rustc skips them
(`lints_that_dont_need_to_run`). With the flag, `after_abort` would skip `derives`, `docs` and
`publicity`, and `Effective::of` too when none of them can fire.

> forwarded to diagnostics (told by tmux).

## 3. For whoever owns `tools/v2/hot_lint.py`: cover `blaze-diagnostics`, and fix `crates`

Where: `tools/v2/hot_lint.py`.

What: `{"root": ".", "crates": ["blaze-diagnostics"]}` fails with
`TypeError: typed_input: crates has a type it does not read: list[str]`, since `typed_input`
reads `tuple[str, ...]` and not `list[str]`. Make `Input.crates` a `tuple[str, ...]`, and add
`blaze-diagnostics` to `HOT` if the coordinator counts it as a hot crate.

Why: as it stands, the lint can't be run on this lane's paths.

> forwarded to the coordinator (`tools/`), listed in requests' report.
