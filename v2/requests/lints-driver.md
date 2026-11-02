# Requests of `lints-driver` (crates/blaze-lints: driver, queries, Cx)

## db: three more methods of `hir::BodyAnalysis`

- What: `resolve_type`, `resolve_macro_call` and `resolve_name`, added to the db section of
  `v2/requests/lints.md` (exact signatures there).
- Where: `crates/hir/src/body_analysis.rs`.
- Why: `check::Cx::{resolve_type, macro_def, resolve_name}` answered through `Semantics`, which
  needs the file's whole tree; inside `body_lints` that makes the query depend on every byte of
  the file, so an edit anywhere in it would lint the body again. In a body `Cx` now asks
  `BodyAnalysis`; `Semantics` stays only for `item_lints`, which reads the whole file anyway.

> forwarded to db (db-next2, `crates/hir/src/body_analysis.rs`) (requests, pass 19; told by tmux).
> applied by db-audit-other-bodyanalysis (2026-10-29 23:10): all three, with no inference, in
> the scope of the block (or nested item's signature or fields) the node is in; a macro call by
> its path in that scope (`MacroSubNs::Bang`), a name as `speculative_resolve_mod_path` does.
> Tested in `crates/hir/src/body_analysis/tests.rs`; not built.
> checked by db-audit-other (23:16): in the tree at `crates/hir/src/body_analysis.rs`: `resolve_type` :300, `resolve_macro_call` :307, `resolve_name` :328, with the signatures `blaze-lints/src/check.rs` calls. Not compiled.

## lints-blaze: `blaze::Lint` public

- What: `crates/blaze-lints/src/blaze.rs`: `pub(crate) enum Lint` becomes `pub enum Lint`, with
  `pub` on `name` (lib.rs declares `mod blaze;`, so nothing new is reachable from outside but
  the type).
- Why: `level::Lint` (public, in `pub mod level`) has the variant `Blaze(blaze::Lint)`; with
  `blaze::Lint` crate-private, rustc's `private_interfaces` lint warns on it, and the workspace
  denies warnings in CI.

> applied by its owner (checked: `crates/blaze-lints/src/blaze.rs` has `pub enum Lint`) (requests, pass 19).
