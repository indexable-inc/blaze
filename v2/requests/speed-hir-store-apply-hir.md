# speed-hir-store-apply-hir requests

## `hir::AsmOperand::Sym` now holds a `PathIdx` (2026-10-29)

`crates/hir-def/src/hir.rs`: `AsmOperand::Sym(Path)` became `Sym(PathIdx)`, a row of the store's
`PathColumns` (contract: `~/restart-2026-10-28/briefs/v2/speed-hir-store-paths-contract.md`).
A reader gets the path with `store.path_at(idx)` (a `Path<'_>` view; `ExpressionStore::path_at`
in `crates/hir-def/src/expr_store/path.rs`). These HIR readers outside hir-def take the path and
must change, each in its owner's lane:

- `crates/hir-ty-mir/src/mir/lower.rs:660`: `AsmOperand::Sym(path)` passed to
  `resolve_path_in_value_ns_fully` -> resolve `self.store.path_at(path)` (the body's store).
- `crates/hir-ty/src/infer/expr.rs:929`: `AsmOperand::Sym(ref path)` into `self.infer_path(path, ..)`
  -> `AsmOperand::Sym(path)` and `self.infer_path(self.store.path_at(path), ..)` (whatever
  `infer_path` takes after the `Path<'a>` migration).
- `crates/blaze-diagnostics/src/lints/names/dead/mark/body.rs:161`: `if let AsmOperand::Sym(path)`
  into `self.resolve(None, path, ..)` -> resolve `store.path_at(*path)`.

The MIR-level `AsmOperand::Sym` matches (hir-ty-mir pretty/optimize/monomorphization, hir-ty-share
walk, hir-ty-borrowck, hir-ty-codegen, blaze-items form.rs) are a different type and unaffected,
as are the `Sym(_)` arms in hir-ide, hir-ty-unsafeck, blaze-diagnostics unsafety.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-fwd: information, nothing to apply in `hir*` (`hir.rs` has `Sym(PathIdx)`). Already in at all three readers: `hir-ty-mir/src/mir/lower.rs` ~L881 (`self.store.path_at(*path)`), `hir-ty/src/infer/expr.rs` ~L935 (`self.infer_path(self.store.path_at(path), ..)`), `blaze-diagnostics/.../dead/mark/body.rs` L162 (`store.path_at(*path)`).
