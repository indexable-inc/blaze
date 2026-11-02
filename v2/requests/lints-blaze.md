# Requests from `lints-blaze` (blaze's own lints, `crates/blaze-lints/src/blaze*`)

All to `lints-driver` (via lane `lints`) unless said.

## 1. `rustc_type_ir` as a dependency of blaze-lints

- What: `rustc_type_ir.workspace = true` under `[dependencies]` in
  `crates/blaze-lints/Cargo.toml`.
- Where: `crates/blaze-lints/Cargo.toml`.
- Why: `blaze/alloc.rs` reads a callee's type (`TyKind::FnDef`) through
  `rustc_type_ir::inherent::IntoKind::kind`; `next_solver::Ty` has no inherent `kind`, and
  `hir_ty` does not re-export the trait.

> forwarded to lints (from requests, pass 17; told by tmux).
> applied by lints-driver (`rustc_type_ir.workspace = true` in `crates/blaze-lints/Cargo.toml`); checked by lane `lints`, 2026-10-29.

## 2. `crate::lints::On` wide enough for blaze's bits

- What: `lints::On` a `u64` (as planned), so `blaze::Lint::bit()` = `1 << (48 + lint as u32)`
  fits; `cx.on_any(mask)` and `cx.on_blaze(lint)` read those bits.
- Where: `crates/blaze-lints/src/lints/mod.rs`, `check.rs`, `level.rs`.
- Why: `Lint::bit` is `const` and overflows a `u32` at compile time.

> forwarded to lints (from requests, pass 17; told by tmux).
> applied by lints-driver (`On = u64`, blaze's bits at 48..52 (`lints/mod.rs`)); checked by lane `lints`, 2026-10-29.

## 3. Where `blaze::check_node` is called, and with what

- What: the walk calls `crate::blaze::check_node(cx, node)` for every node it visits, item level
  and body, the root included (`check.rs`'s preorder does). Blaze's body pass runs when
  `node == cx.root()`, `cx.in_file()` and `cx.analysis()` is `Some`: once per body. cx.md
  still names `blaze::check_body(cx)` / `blaze::check_items(cx)`; the brief and the code use
  this one entry, so cx.md's line should say `blaze::check_node(cx, node)`.
- Where: `check.rs` (the walk), `docs/v2/plan/lanes/lints/cx.md` (one line).
- Why: one entry point, as clippy's lints have.

> forwarded to lints (from requests, pass 17; told by tmux).
> applied by lints-driver (the walk calls `blaze::check_node(self, node)` for every node, root included; cx.md says so); checked by lane `lints`, 2026-10-29.

## 4. Levels: `blaze::<name>` and the group `blaze::hot`

- What: `level.rs` reads `blaze::hot_clone`, `blaze::hot_alloc`, `blaze::hot_dyn`,
  `blaze::std_hashmap` (`Lint::ALL`, `Lint::name()`) and the group `blaze::hot`
  (`blaze::HOT`, a `&[Lint]` of all four: `std_hashmap` too, as `tools/v2/hot_lint.py` checks
  SipHash maps in hot code; not `HotClone|HotAlloc|HotDyn` alone) on the command line (`--deny=blaze::hot`, `-D blaze::hot_alloc`)
  and in `#[allow/warn/deny/forbid(..)]`. Each is allow by default. The level note
  (`` `#[deny(blaze::hot_clone)]` implied by `#[deny(blaze::hot)]` ``) as rustc writes one for a
  group.
- Where: `level.rs`, `check.rs`.
- Why: the tests in `blaze/tests.rs` run with both forms.

> forwarded to lints (from requests, pass 17; told by tmux).
> applied by lints-driver (`level.rs` `Target::blaze`, `BLAZE_HOT` of all four, allow by default; group note as rustc's); checked by lane `lints`, 2026-10-29.

## 5. `cx.blaze` adds no help line of its own

- What: `cx.blaze(lint, at, span, message)` adds the level note but not a
  "for further information" help: blaze's lints add theirs (`Lint::docs_url()`, a link into
  `docs/v2/plan/lanes/lints/blaze.md`) after their own help, as clippy's order is.
- Where: `check.rs` (`Cx::blaze`).
- Why: otherwise every finding has two such lines.

> forwarded to lints (from requests, pass 17; told by tmux).
> applied by lints-driver (`Cx::blaze` adds only the level note; the driver adds clippy's docs help for clippy lints only); checked by lane `lints`, 2026-10-29.

## 6. `cx.path` outside a body

- What: `cx.path(path)` answers for a path written at item level (a field's type, a
  signature, a type alias), resolved in the scope of the item it is written in; today it is
  `None` outside a body.
- Where: `check.rs` / `query.rs` (`Cx::path`).
- Why: `std_hashmap`'s type form (`struct Index { names: HashMap<u32, u32> }`,
  `fn f(m: &HashMap<K, V>)`, `type Map = HashMap<K, V>`) resolves the written `HashMap` to std's.
  `blaze/tests.rs` (`std_hashmap`) expects those findings.

> forwarded to lints (from requests, pass 17; told by tmux).
> applied by lints-driver (`Cx::path` resolves through `Semantics` at item level, through `BodyAnalysis` in a body); checked by lane `lints`, 2026-10-29.

## 7. What blaze reads of `hir::BodyAnalysis` (to `db-ty-hir`, via `lints`)

- What, as `v2/requests/lints.md` asks: `body()` the same `Body` as `Body::of(db, def)`, its
  `ExprId`s the ones `infer()` keys; `def()` the `hir::DefWithBody`; `ty(Ty)` a `hir::Type`
  in the body's environment; `expr_range(ExprId)` a range of `cx.root()`'s tree (the
  expression's, or its outermost macro call's), `None` for what a macro made up with no call
  in this tree.
- Where: `crates/hir/src/body_analysis.rs`.
- Why: blaze's body pass walks `body().exprs()` and reports at `expr_range`, deduplicated, so
  a `format!` whose expansion makes several allocating calls is one finding at the call.

> forwarded to lints (from requests, pass 17; told by tmux).
> applied by lints-driver (carried to db in `v2/requests/lints.md` (`hir::BodyAnalysis`, the exact API)); checked by lane `lints`, 2026-10-29.
