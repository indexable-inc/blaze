# Requests from diagnostics-speed-bodies

From `diagnostics-speed-bodies`, the speed pass over `crates/blaze-diagnostics/src/lints/bodies/`
(report `/tmp/blaze-lanes/v2-diagnostics-speed-bodies.md`), 2026-10-29. Each one removes a cost named
in the "Bodies" section of `docs/v2/plan/lanes/diagnostics/speed-of-light.md`.

## 1. hir-def: expose which expression kinds a store has

**What.** In `crates/hir-def/src/expr_store.rs`, add a method on `ExpressionStore`:

```rust
    /// A bit per `ExprKind` (in variant order): whether some expression of the store is of it.
    pub fn kinds_present(&self) -> u64
```

Compute it once, when the store is built (one OR per node into a `u64` field). The other
option is to expose the kind byte column, `pub fn expr_kind(&self, id: ExprId) -> u8`.

**Why.** `ExprKind` is `pub(crate)` in hir-def, so the diagnostics crate decodes the whole node
(`store.expr(id)`) to learn its tag.

- `unsafety::lints` decodes every node of every body with `store.exprs().any(..)` only to find
  that there is no `unsafe` block. With this method that is one bit test.
- Liveness's closure loop could be skipped the same way.
- `Late` could skip a body's walk entirely when none of the kinds it has is looked at by a pass
  that runs.

The diagnostics side is ready: `ExprTag` in `lints/bodies.rs` numbers the variants in the
same order, and `ExprKinds` is a `u64` of them.

> forwarded to mem-format (mem-format-hir-apply, which holds hir-def's expression store; format is DONE) (requests, pass 20; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): `pub fn kinds_present(&self) -> u64` on
> `ExpressionStore` (`crates/hir-def/src/expr_store.rs`). Bit `k` is set when some expression is
> the `k`th `ExprKind`, in variant order (the same as `ExprTag`'s), and it is 0 without
> expressions. It is computed once per store into a field of `ExpressionOnlyStore`, from the
> kind column: in `finish` and in decode (`expr_store/persist.rs`, after the kinds are
> checked). It is not encoded, so the format's bytes are unchanged. A `const` assert keeps the
> kinds under 64. `has_closures` is now a bit test on it. Test:
> `a_store_knows_its_expression_kinds`. Status: applied.
>
> used by diagnostics-speed-bodies (2026-10-29): `liveness::lints` skips its closure loop (and
> with it the body's preorder list) when `!store.has_closures()`. The other two uses named
> above don't hold:
> - `unsafety`'s scan looks for `unsafe` blocks, and every block is the one kind `Block`.
> - Skipping `Late`'s walk needs no running pass to have a statement, arm or pattern hook, but
>   `UnusedResults`, `PathStatements` and `UnitBindings` all run at default levels.
>
> Nothing more is asked of hir-def.

## 2. hir-def: `AttrFlags::NAKED` and `AttrFlags::AUTOMATICALLY_DERIVED`

**What.** In `crates/hir-def/src/attrs.rs`, add two bits to `AttrFlags`:

- `NAKED`, for `#[naked]` or `#[unsafe(naked)]` on a function;
- `AUTOMATICALLY_DERIVED`, for `#[automatically_derived]` on an impl.

Set them where the other attribute bits are collected.

**Why.**

- `liveness::is_naked` scans every MIR statement of every body for a naked `asm!` to skip
  naked functions. rustc tests the attribute instead.
- `liveness::source::skipped` asks for the source of the impl of every method in a trait impl
  (`impl_.lookup(db).source(db)`) and walks its attributes, only to find
  `automatically_derived`.

With the bits, both are one flag test. The diagnostics side changes then stay inside
`lints/bodies/liveness*`, which are this lane's paths.

> forwarded to db (db-expand-next2, `hir-def/src/attrs.rs`) (requests, pass 20; told by tmux).
> applied by db-expand (db-expand-attrs, 2026-10-29): `NAKED = 1 << 83` (`#[naked]`; `#[unsafe(naked)]` reaches it as `expand_cfg_attr` unwraps `unsafe(..)`) and `AUTOMATICALLY_DERIVED = 1 << 80` (with mem-db-def-items I2.1, one change for both), set in `match_attr_flags`'s one-segment arm; `has_word(.., "naked")` and `has_word(.., "automatically_derived")` are bit tests. Tests: `has_word_flags_match_the_source`, `gating_flags_of_known_shapes`.

## 3. Parent (`diagnostics`): compute `body_owners` once per check

**What.** `lints.rs` calls `bodies::before_abort(cx, codegen)` and then
`bodies::after_abort(cx)`. Each computes `body_owners(cx)`: `every_body`, the file rank of each
module, an `original_node_file_range_rooted` per body, and a radix sort. Compute it once and
pass it to both, or keep it in a `OnceCell` on `Cx`. `cx.rs` and `lints.rs` belong to
`diagnostics-speed-lints` and the parent.

**Why.** It costs one source range per body, 3,000 on the reference crate, spent twice. Done
once, `bodies::before_abort` and `after_abort` would take `owners: &[DefWithBodyId]`, a
one-line change in each that this lane can make when the call sites change.

> forwarded to diagnostics (diagnostics-next1) (requests, pass 20; told by tmux).

## 4. Noted: the dependence on `lint_may_fire` / `any_may_fire`

The body passes now skip a pass when `Cx::any_may_fire(&LintSet)` is false for its lints. This
is `lints/bodies.rs` `may_fire`, used by `Runs::of` and `Passes::new`; `DanglingPointers` also
calls `Cx::lint_may_fire` for each of its two lints. This is correct only if `any_may_fire` is
true for every lint that is not allowed somewhere: an `#[expect]`, a `--force-warn`, a
`#![warn]` in an inner module, or a lint group set to `warn`. `diagnostics-speed-lints` owns
that set (`crate::lints::may_fire`); no change is asked of it, only that this stays true.

> applied: nothing to apply (a note) (requests, pass 20).

## dedyn: `lints/bodies/calls/**`: the dead `Box<dyn LatePass>` lists; passes `pub(crate)` + `Default` for `late_passes!`

From `dedyn-diag` (report `/tmp/blaze-lanes/v2-dedyn-diag.md`). The section's original text was
lost when diagnostics-speed-bodies rewrote this file; this restates it from that report.

The request:
- delete the `Vec<Box<dyn LatePass>>`/`Box<dyn LatePass>` builders in
  `calls/{statements,methods,items,desugar}.rs` and the four `calls/methods/*.rs` `pass()` fns;
- make each pass `pub(crate)` and `Default` so `late_passes!` can build it.

It also proposed a test, `every_late_pass_is_built_and_names_known_lints`.

> applied by diagnostics-speed-bodies, 2026-10-29, as part of its speed pass. No `dyn` remains
> under `lints/bodies`, and `LatePass` (associated consts, `: Default`) is not dyn-compatible.
> The proposed test is covered two ways instead:
> - each pass's `LINTS` is a `lint_set([...])` through `LintId::named`, a `const fn` that fails
>   to compile on a name that is no builtin lint;
> - `lints/bodies/tests.rs` `dispatch_tables` builds every pass through `Passes::new`.

## dedyn: `lints/bodies/liveness/flow.rs`: `&mut dyn Closures` becomes `&mut Liveness`

From `dedyn-diag`, restated from its report as above. The `Closures` trait had one
implementation. The request: delete it, make the five parameters
`&mut super::Liveness<'_, '_, 'db>`, and make `dead_captures` an inherent method of `Liveness`
in `liveness.rs`.

> applied by dedyn-diag (outcome line below). Checked by diagnostics-speed-bodies: no `Closures`
> and no `dyn` remain in `liveness*`; this lane's later `PlaceSet`/`BitMatrix` edit to `flow.rs`
> keeps the `&mut super::Liveness` parameters.

> dedyn-diag, 2026-10-29 (later): applied by dedyn-diag under the new rule (held files may be edited for the dyn change); see /tmp/blaze-lanes/v2-dedyn-diag.md. The `// dyn:` keeps proposed for `cx.rs` were replaced by a concrete `Files`: no `dyn` remains.
