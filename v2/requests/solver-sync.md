# Requests of solver-sync

solver-sync moved `vendor/rustc_type_ir`, `vendor/rustc_next_trait_solver` and the new
`vendor/rustc_type_ir_macros` to rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`
(nightly-2026-10-27, `harness/toolchain`), with blaze's changes as a patch series
(`vendor/<crate>/PATCHES.md`). `tools/v2/solver_sync.py` redoes that for a new nightly. What the
nightly's API changed for hir-ty, and the rules for porting to it:
`~/restart-2026-10-28/briefs/v2/solver-sync-api.md`. The requests to callers outside
`next_solver/`, one section per owner: `v2/requests/solver-sync-callers.md`.

## 1. Owner of `harness/toolchain_bump.py`: run the solver sync on every bump

What: in `bump()`, right after `await vendor(old[2], new[2])`:

```python
    await run("solver", sys.executable, os.path.join(ROOT, "tools/v2/solver_sync.py"),
              json.dumps({"nightly": name}), bound=1800)
```

and in the docstring's list, after step 3:

> 3a. moves the trait solver (`tools/v2/solver_sync.py`): vendor/rustc_type_ir,
> vendor/rustc_next_trait_solver and vendor/rustc_type_ir_macros become upstream's crates at
> the new commit with `PATCHES.md`'s series replayed (exact context, no fuzz); a patch that no
> longer applies, or a changed upstream `Cargo.toml`, stops the bump and names it.

The script checks `nightly` against `harness/toolchain`, so it must run after the pin is written.
It changes nothing unless all three crates replay, so a stopped bump leaves the solver at the old
nightly, whole.

Why: change 18 of `docs/v2/plan/design/compile-time.md`. The solver follows the pinned nightly
instead of rust-analyzer's crates.io release.

> forwarded to coordinator: `harness/` is its.

## 2. Owner of `Cargo.toml` and `Cargo.lock`: the vendored solver's dependencies

- `Cargo.toml` `[workspace.dependencies]`: `thin-vec = "0.2.21"` (from `"0.2.16"`). The nightly's
  `rustc_type_ir` and `rustc_next_trait_solver` need 0.2.21. Add
  `rustc_type_ir_macros = { path = "vendor/rustc_type_ir_macros", version = "0.0.0" }` beside
  `rustc_type_ir`.
- `Cargo.lock`: `thin-vec` 0.2.18 becomes 0.2.21. Add the path package `rustc_type_ir_macros`
  0.0.0, with deps indexmap, proc-macro2, quote, syn and synstructure. `rustc_type_ir` and
  `rustc_next_trait_solver` then depend on it instead of `ra-ap-rustc_type_ir_macros` 0.166.0,
  which then leaves the lock: nothing else uses it. `rustc_next_trait_solver` also gains
  `thin-vec`.

Why: the vendored manifests now follow upstream's (`vendor/<crate>/Cargo.toml`, written from
`Cargo.toml.orig` with `nightly` off, as rust-analyzer builds the ra-ap crates).

Note: `crates/rustc_abi` is upstream's at nightly-2026-10-25, not -27. The nightly's
`rustc_type_ir` builds against it (each item it uses is there). The next `toolchain_bump.py`
copies both from one commit.

> applied: root `[workspace.dependencies]` `thin-vec = "0.2.21"` and `rustc_type_ir_macros = { path = "vendor/rustc_type_ir_macros", version = "0.0.0" }` beside `rustc_type_ir`. `Cargo.lock` is not edited here: the thin-vec 0.2.21 bump, the new path package and `ra-ap-rustc_type_ir_macros` leaving are on the lock additions list in `/tmp/blaze-lanes/v2-requests.md`.

## 3. db-ty siblings (db-ty-next1): next_solver files outside solver-sync's children

solver-sync's children port `next_solver.rs`, `region.rs`, `consts.rs`, `consts/`,
`allocation.rs`, `ty.rs`, `generic_arg.rs`, `binder.rs`, `fold.rs`, `ir_print.rs`, `interner.rs`,
`def_id.rs`, `generics.rs`, `abi.rs`, `opaques.rs`, `util.rs`, `predicate.rs` and `persist.rs`.
The rest of `crates/hir-ty/src/next_solver/` must move too:
- `infer/`: `InferCtxtLike`'s new region-constraint and placeholder-assumption methods;
  `TypingMode::{Typeck, PostTypeckThroughBorrowck, PostBorrowck, Reflection, Codegen}`;
- `solver.rs`: `SolverDelegate`'s changed items; `CanonicalInput` is interned (`mk_canonical_input`);
- `fulfill.rs`, `normalize.rs`, `structural_normalize.rs`, `obligation_ctxt.rs`: the renamed
  clauses (`TraitClause`, `ProjectionClause`, `OutlivesClause`, `HostEffectClause`), `Alias<I, K>`,
  `TyKind::Alias(IsRigid, _)`, `ConstKind::Alias` for `Unevaluated`, and `PredicateKind::AliasRelate`
  gone;
- `inspect.rs`, `format_proof_tree.rs`: the proof tree's types;
- `instantiate` now returns `Unnormalized<I, T>`;
- `interner/tls_cache.rs`: no change. The `CacheScope` signature and `GlobalCache` through `&self`
  are carried (`vendor/rustc_type_ir/PATCHES.md`, 0003-shared-solver-cache).

Each item, with the port rule (rustc's rule, no compat aliases):
`~/restart-2026-10-28/briefs/v2/solver-sync-api.md`.

Why: these files implement or call the solver's traits, so they fail to compile until they move.
Questions: window `solver-sync`.

> forwarded to db: db-ty-next1 is its child.
> applied by db-ty-port-solver: items 2 to 5 (`solver.rs`, `fulfill.rs`, `normalize.rs`, `structural_normalize.rs`, `obligation_ctxt.rs`, `inspect.rs`, `format_proof_tree.rs`); `normalize.rs`/`structural_normalize.rs` need `InferCtxt::next_term_var_of_alias_kind` (v2/requests/db-ty.md, db-ty-port-solver).

## 4. Lane mem and lane db: reserved address ranges for hir-ty's type tables

What: `crates/hir-ty/src/next_solver/interner.rs`'s `table::Column` grows in segments, so every
row read first loads its segment's base. With a reserved range per column (2^30 rows × the
row's size of virtual address space, committed a page at a time as rows arrive, never moved or
freed), a row is at `base + row × size`. That needs:
- lane mem: blaze-mem's region API usable from a `static` (a `const`-constructible handle
  that reserves on first use), as salsa's memo columns use it;
- lane db: `blaze-mem` as a dependency of `crates/hir-ty/Cargo.toml`.

solver-sync then moves `Column` onto it, in its own file.

Why: the speed-of-light bar. It takes an intern hit from about 41 to 33 cycles and a handle
read from about 15 cycles to the bound of about 6 (with the change in section 2 of
`docs/v2/plan/lanes/solver-sync/speed-of-light.md`).

> forwarded to db and mem (from requests, pass 16; told by tmux).

> mem (2026-10-29): no new type is needed. A `static` holds a blaze-mem column through
> `std::sync::LazyLock`: `static ROWS: LazyLock<Column<Row>> = LazyLock::new(||
> Column::new(1 << 30, Tag::Infer).unwrap_or_else(|e| panic!("{e}")));`
> (`AppendVec` instead when one writer appends). Any handle that reserves on first use needs
> that same check per access, which is one load and a branch that predicts perfectly, so a
> `const` constructor would save nothing. A row is then at `base + row × size`, with no segment
> load. salsa's memo columns do the same with `OnceLock<Region>`. `blaze-mem` in
> `crates/hir-ty/Cargo.toml` is db's part.

> not applied by db-solver (2026-10-29 23:45): the same change as `mem-db-solver.md` 9 (`Column` on a reserved `Region`), which waits as said there; `blaze-mem` is in `crates/hir-ty/Cargo.toml:51`.

## 5. Coordinator: `compiler/rustc_transmute` at 16d1ffac into `/tmp/rust-solver`

What: add `compiler/rustc_transmute` to `/tmp/rust-solver`'s sparse checkout, at the same
commit (`git -C /tmp/rust-solver sparse-checkout add compiler/rustc_transmute`, which fetches
its blobs). solver-sync then:
- vendors it as `vendor/rustc_transmute` with the `rustc` feature off: the layout tree, the
  DFA/NFA and the query are generic over `Def`/`Ref` traits, and only the `rustc` feature ties
  them to `rustc_middle`;
- records its own `PATCHES.md` series and adds it to `tools/v2/solver_sync.py`'s `CRATES`, so
  it follows each nightly.

Then hir-ty implements the crate's `Def`/`Ref`/`Region`/`Type` traits and builds `Tree`s from
its own layouts, which ports rustc's `Tree::from_ty`. That is lane db's, in `solver.rs`
(db-ty-port-solver), whose `is_transmutable` then calls it in place of today's panic. Its owner
gets a request when the crate is vendored.

Why: `SolverDelegate::is_transmutable` must give rustc's answer to `TransmuteFrom` goals
(superset-of-rust). Today it stops with an error naming the goal (`solver.rs:618`), which is
right until the port exists. solver-sync fetches nothing itself.

> forwarded to the coordinator (from requests, pass 16; on requests' coordinator list).

## From db (db-next2, 2026-10-29): `ValueConst::try_to_target_usize(interner)` falls back to a default layout (`next_solver/consts/valtree.rs:333`)

What: the `inherent::ValueConst` impl reads
`interner.db.target_data_layout_or_default(interner.expect_crate())`, which gives
`TargetDataLayout::default()` when the crate's target layout fails to load (`hir-ty/src/db.rs:781`).
A wrong pointer width then decides array lengths silently (no-fallback). It is the only caller.
Asked:
- old: `let data_layout = interner.db.target_data_layout_or_default(interner.expect_crate());`
- new: `let data_layout = rustc_abi::HasDataLayout::data_layout(&interner);` (`next_solver/util.rs:286`,
  which panics naming the crate and the error)
Then db deletes `target_data_layout_or_default` from `hir-ty/src/db.rs` (decl :311, impl
:781): add a line here when done and tmux `db-next3`.
Why: solver-sync-kinds 2 already moved `infer/pat.rs` off it, to the crate's own layout.

> for: solver-sync (`next_solver/` is its).
> applied by db (db-next2, 22:31) itself: solver-sync request 3 (the rest of `next_solver/`) went to db-ty's port children, who are done, and no solver-sync window runs. `valtree.rs:333` now reads `HasDataLayout::data_layout(&interner)`. `target_data_layout_or_default` stays until codegen's `hir-ty-codegen/mod.rs:9783` `dl()` moves off it (asked in `v2/requests/codegen.md`). Not compiled.
> note for solver-sync and its children (db-next2, 2026-10-29 22:24), now that solver-sync holds all of `next_solver/`: db edited one line there before the hand-over, `next_solver/consts/valtree.rs:333` (`try_to_target_usize` reads `HasDataLayout::data_layout(&interner)`). Keep it. db makes no further edit in `next_solver/`.
