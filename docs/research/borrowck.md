---
status: record
summary: rustc's NLL and Polonius, rust-analyzer's removed borrowck, and what blaze builds.
date: 2026-10-20
---
# Borrow checking for blaze: rustc's NLL, Polonius, and what to build

Date: 2026-10-20. Pinned rustc: nightly-2026-10-09, rust-lang/rust
[`a30aa9064df6a504b6a323c6884a2f4cfbfff666`](https://github.com/rust-lang/rust/tree/a30aa9064df6a504b6a323c6884a2f4cfbfff666)
(abbreviated `a30aa906` below).

Each claim carries a mark:

- **(read)**: checked in the source or the document cited (a file and line at `a30aa906` unless
  another place is named, or a file under [docs/upstream/borrowck/](../upstream/borrowck/SOURCE.md)).
- **(reported)**: someone else says so, at the place cited; not checked here.
- **(unverified)**: my inference or memory, not checked.

Paths like `rustc_borrowck/src/lib.rs:115` are `compiler/rustc_borrowck/src/lib.rs` at `a30aa906`.
Fetched material is under `docs/upstream/borrowck/`, listed in its `SOURCE.md`.

## Key findings

1. **The pinned nightly already borrow-checks with the Polonius alpha by default.** `-Zpolonius`
   defaults to `next` when rustc is built with `CFG_DEFAULT_POLONIUS_NEXT`, and bootstrap sets that
   on the nightly and dev channels; `rustc +nightly-2026-10-09 -Z help` prints
   `polonius=val -- enable polonius-based borrow-checker (default: next)` (read:
   [pinned-polonius-flag.md](../upstream/borrowck/rust-lang-rust/pinned-polonius-flag.md),
   `rustc_session/src/config.rs:3656-3690`, `src/bootstrap/src/core/build_steps/compile.rs:1372-1377`).
   So "what nightly `-Zpolonius=next` does" is what plain `rustc +nightly-2026-10-09` does, and
   stable's NLL is `-Zpolonius=off`. The same bootstrap line makes `-Znext-solver=globally` the
   nightly default (read: same files, `config.rs:1049-1060`).
2. **Polonius alpha is NLL with one thing replaced: when a loan goes out of scope.** Region
   inference, type tests, universal-region (free region) errors and closure requirements are NLL's,
   location-insensitive; under `next`, loan liveness is computed by a reachability walk over a
   (region, point) graph and feeds the `Borrows` dataflow's kill set instead of NLL's region
   frontier (read: `rustc_borrowck/src/polonius/mod.rs:1-33,136`, `dataflow.rs:246,381-400`,
   `region_infer/region_context.rs:213-229`).
3. **Not stable yet.** Enabled on nightly by
   [rust-lang/rust#159343](https://github.com/rust-lang/rust/pull/159343) (merged 2026-08-05, live
   from nightly-2026-08-06); the blog post of 2026-08-04 aims to stabilize "prior to the end of the
   year"; no stabilization PR was found as of 2026-10-20 (reported:
   [github-items.md](../upstream/borrowck/rust-lang-rust/github-items.md),
   [blog](../upstream/borrowck/blog/rust-blog-2026-08-04-enabling-polonius-alpha-on-nightly.md);
   the "none found" part is a search, unverified as exhaustive).
4. **There are open I-unsound bugs that only the alpha has**: #161038, #161471, #164337, #164420,
   plus #153215 (dead regions outlived by opaques) and #164421 (one form also NLL). Matching rustc
   "exactly" means matching those acceptances too (reported: github-items.md).
5. **rustc moved away from the datalog Polonius because it did not scale**; the `legacy` code is
   kept only until `next` "reaches feature parity" (reported: 2026 goal FAQ, blog; read:
   `rustc_borrowck/src/polonius/legacy/mod.rs:1-4`).
6. **The alpha and legacy disagree in both directions**: legacy accepts the linked-list cursor
   loops (`iterating-updating-cursor-issue-{57165,63908,108704}.rs`) and
   `flow-sensitive-invariance.rs` that the alpha rejects; the alpha accepts
   `nll-legacy-unnecessary-error.rs` that legacy rejects (read: the tests at `a30aa906`;
   [ui-polonius-revisions-table.md](../upstream/borrowck/rust-lang-rust/ui-polonius-revisions-table.md)).
7. **46 `tests/ui` files carry a polonius revision**, 47 carry `ignore-compare-mode-polonius`;
   15 of the 46 are rejected by NLL and accepted by the alpha (read: scan of `tests/ui` at `a30aa906`).
8. **rust-analyzer never had a real borrow checker** (no regions, no loans; it computed
   mutability and moves out of references), and upstream deleted it in
   [rust-lang/rust-analyzer#23043](https://github.com/rust-lang/rust-analyzer/pull/23043)
   (2026-08-05), which blaze's vendored copy carries as commit `ed03b37400` (read).
9. **Recommendation**: implement rustc's borrowck structure (NLL region inference plus the alpha's
   loan liveness) on rust-analyzer's MIR, with the default mode equal to the pinned nightly's
   default and an NLL mode equal to `-Zpolonius=off`; do not implement the datalog analysis.

## 1. rustc's borrow checker at the pinned nightly

### 1.1 Where it runs and what it reads

- `mir_borrowck` is a query on typeck roots; nested bodies (closures, coroutines, inline consts)
  are borrow-checked together with their root, nested bodies before parents, so a parent sees its
  closures' `used_mut_upvars` and outlives requirements (read: `rustc_borrowck/src/lib.rs:115`,
  `root_cx.rs:269-300`).
- Its input MIR is `mir_promoted`: built MIR after promotion, before drop elaboration, with fake
  reads, fake borrows for match guards, `StorageLive`/`StorageDead`, false edges and two-phase
  borrow kinds still present (read: `lib.rs` `borrowck_collect_region_constraints` at 318 takes the
  promoted body and clones it as the dev guide's "local copy",
  [borrow-check.md](../upstream/borrowck/rustc-dev-guide/borrow-check.md); the list of what that
  MIR still contains is from memory, unverified).
- Opaque types are handled across the whole root at once, applying closure requirements twice
  ("modulo opaques", then final) because defining uses and nested bodies depend on each other
  (read: `root_cx.rs:151-167`).

### 1.2 The pipeline, in order

Per body (read unless marked; `lib.rs:318-396` and `nll.rs:56-200` drive it):

1. **Renumber regions.** `nll::replace_regions_in_mir` builds `UniversalRegions` (the fn's early-
   and late-bound regions, `'static`, and the closure's captured regions) and replaces every other
   region in the body with a fresh inference variable (`renumber.rs`) (read: `nll.rs:56`).
2. **Indexing.** `MoveData::gather_moves` (move paths, rustc_mir_dataflow), `BorrowSet::build`
   (every `Rvalue::Ref` in the body, and the experimental `Rvalue::Reborrow`, with two-phase
   activation points; raw borrows are not loans; `borrow_set.rs:427,476`), a `PoloniusLocationTable` and a `DenseLocationMap` of points (read: `lib.rs:318-362`).
3. **MIR type check.** `type_check::type_check` re-type-checks the body and records region
   constraints: outlives constraints with `Locations::All` (from the signature and user type
   annotations) or `Locations::Single(location)` (from statements), type tests (`T: 'r`), member
   constraints for opaques, and the universal-region relations with implied bounds
   (`type_check/free_region_relations.rs`) (read: `lib.rs:362`, `type_check/mod.rs`).
4. **Liveness.** For each relevant local: use-live points by a backward walk from uses
   (`compute_use_live_points_for`), drop-live points from drops where the local may still be
   initialized (`compute_drop_live_points_for`, which lazily runs `MaybeInitializedPlaces`), and
   for drop-live locals the regions the type's destructor may touch (dropck outlives). Regions in
   live locals' types are made live at those points; universal regions are live everywhere (read:
   `type_check/liveness/trace.rs:524-560,700-731`, `type_check/liveness/mod.rs`).
   Under `next`, liveness also records each live region's variance for the localized graph
   (read: `polonius/liveness_constraints.rs`, `polonius/mod.rs:96-98`).
5. **Region inference** (`nll::compute_regions`, `region_infer/region_context.rs`):
   constraints become an SCC graph after placeholder handling (a region that would have to outlive
   a placeholder of a higher universe is made to outlive `'static`;
   `handle_placeholders.rs`); `solve` propagates liveness values through the SCC DAG
   (`propagate_constraints`, 861), checks type tests (`check_type_tests`, 900, via
   `eval_verify_bound`; in a closure, failures become requirements on the parent via
   `try_promote_type_test`, 1011) and checks universal regions (`check_universal_regions`, 1331:
   "lifetime may not live long enough") (read). Under `legacy` the last step is
   `check_polonius_subset_errors` instead; under `next` it is NLL's (read: `region_context.rs:213-229`).
6. **Loan liveness under `next`.** Before `solve`, `PoloniusContext::compute_loan_liveness` builds
   the localized constraint graph and computes `LiveLoans` (section 2.2) (read: `nll.rs:153`,
   `polonius/mod.rs:136`).
7. **Dataflow.** `get_flow_results` runs `Borrows` (loans in scope), `MaybeUninitializedPlaces`
   and `EverInitializedPlaces` together (read: `lib.rs:605`). `Borrows` kills a loan where it goes
   out of scope, computed by `OutOfScopePrecomputer` (NLL: the first point not contained in the
   loan's region) or `PoloniusOutOfScopePrecomputer` (`next`: the frontier where the loan stops
   being live), and on assignment to, `StorageDead` of, or inline-asm output to the borrowed place
   (`kill_borrows_on_place`) (read: `dataflow.rs:133,238,246,381-400,420`).
8. **The check walk.** `MirBorrowckCtxt` visits every statement and terminator with the three
   dataflow states. Each access goes through `access_place` (1190): `check_access_permissions`
   (2406: mutability, giving E0594/E0596) and `check_access_for_conflict` (1268: every loan in
   scope whose place conflicts, by `places_conflict.rs` with `AccessDepth::{Shallow, Deep, Drop}`
   and a `PlaceConflictBias`). Moves and uses check initialization
   (`check_if_full_path_is_moved` 2032, `check_if_path_or_subpath_is_moved` 2137). Two-phase
   borrows are inactive between reservation and activation, decided with dominators
   (`path_utils.rs:60-80`), and `check_activations` (1873) checks each activation as a write.
   Function exit checks loans of locals that would dangle (`check_for_invalidation_at_exit`,
   1810); yields check loans held across them (`lib.rs:1864`) (read).
9. **Report.** Move errors, region errors and conflicts are buffered and emitted; then
   `lint_unused_mut` (read: `lib.rs:2736`).

### 1.3 Two-phase borrows

`TwoPhaseActivation::{NotTwoPhase, NotActivated, ActivatedAt(loc)}` per borrow; `is_active`
returns false only between reservation and activation (read: `path_utils.rs:60-80`,
`borrow_set.rs`). A reservation conflicting with a shared borrow is accepted at `a30aa906`
(read: `lib.rs:1313-1317`, the `(Reservation(_), BorrowKind::Fake(_) | BorrowKind::Shared)` arm
continues, with the comment "This used to be a future compatibility warning ... See
rust-lang/rust#56254"); the dev guide chapter still describes a lint (read:
[borrow-check_two-phase-borrows.md](../upstream/borrowck/rustc-dev-guide/borrow-check_two-phase-borrows.md)),
so trust the code over the guide here.

### 1.4 Error codes and lints borrowck emits

Constructors are in `rustc_borrowck/src/borrowck_errors.rs` unless noted; the "from" column is the
caller found by grep (read).

| code | message (short) | from |
|---|---|---|
| E0373 | closure may outlive the current function | `diagnostics/conflict_errors.rs:3755` |
| E0381 | used binding isn't initialized | `diagnostics/conflict_errors.rs:915` |
| E0382 | use of moved value | `conflict_errors.rs:170` |
| E0384 | cannot assign twice to immutable variable | `report_illegal_reassignment`, `conflict_errors.rs:4116` |
| E0499 | cannot borrow as mutable more than once | conflicting-borrow report, `conflict_errors.rs` |
| E0500, E0501, E0524 | closure requires unique access / conflicts with a closure borrow | `conflict_errors.rs:1935` and nearby |
| E0502 | cannot borrow as mutable because also borrowed as immutable | conflicting-borrow report |
| E0503 | cannot use because it was mutably borrowed | `conflict_errors.rs` |
| E0505 | cannot move out because it is borrowed | `session_diagnostics.rs:321` |
| E0506 | cannot assign because it is borrowed | `conflict_errors.rs` |
| E0507 | cannot move out of a shared reference / index | `diagnostics/move_errors.rs` |
| E0508 | cannot move out of a non-copy array | `move_errors.rs` |
| E0509 | cannot move out of a type implementing Drop | `move_errors.rs:293` |
| E0510 | cannot mutate in a match guard | `conflict_errors.rs:1942,4039` |
| E0515 | cannot return reference to local | `conflict_errors.rs:3629` |
| E0521 | borrowed data escapes outside of closure/function | `conflict_errors.rs:3815`, `diagnostics/region_errors.rs:869` |
| E0594, E0596 | cannot assign / borrow as mutable (not declared `mut`, behind `&`) | `mutability_errors.rs` |
| E0597 | borrowed value does not live long enough | `conflict_errors.rs` |
| E0626 | borrow may still be in use when coroutine yields | `lib.rs:1864` |
| E0712 | thread-local borrowed after function returns | `conflict_errors.rs:3401` |
| E0713 | borrow may still be in use when destructor runs | `conflict_errors.rs:3343` |
| E0716 | temporary value dropped while borrowed | `conflict_errors.rs:3434` |
| (none) | lifetime may not live long enough | `LifetimeOutliveErr`, `session_diagnostics.rs:115` |

Region errors are also reported through `rustc_trait_selection` with codes E0310/E0311 (generic
bound failure, `error_reporting/infer/region.rs:756-757`), E0621 (explicit lifetime required,
`NiceRegionError::try_report_from_nll`) and E0700 (hidden type captures lifetime) (read in the
local rust checkout at `ad3d0bc`, not re-checked at `a30aa906`: unverified there).

Lints: borrowck emits **`unused_mut`** (`UNUSED_MUT`, diagnostic `VarNeedNotMut`) for `mut`
locals and arguments never used mutably, skipping `_`-prefixed and desugared ones (read:
`lib.rs:2736-2762`; captured upvars through `used_muts.rs`), and **`tail_expr_drop_order`** from
`check_backward_incompatible_drop` (read: `lib.rs:1402`). `unused_variables` and
`unused_assignments` are not borrowck's (unverified: from memory they come from MIR liveness in
`rustc_mir_transform`).

### 1.5 Other things in the crate

- `consumers.rs` exposes bodies with facts for external tools (Prusti, Flux, etc.) (read).
- An experimental `Rvalue::Reborrow` (the generic `Reborrow` trait experiment) is handled in
  `borrow_set.rs`, `dataflow.rs` and `lib.rs` (read; nightly-only feature, not needed for stable code).

## 2. Polonius

### 2.1 `-Zpolonius` at the pinned commit

- Values: `off` or `no` (NLL), `legacy` or no value (datalog), `next` (alpha) (read:
  `rustc_session/src/options.rs:1000-1017`). The enabling PR's body mentions `-Zpolonius=nll`, which
  the parser at `a30aa906` does not accept (reported: #159343 body; read: parser).
- Default: `next` on nightly/dev builds, `off` otherwise (read: finding 1). Beta and stable
  therefore still use NLL (reported: #164337's table shows 1.100.0-beta.5 and 1.99.0 rejecting what
  nightly accepts).

### 2.2 How the alpha computes loan liveness

All read in `rustc_borrowck/src/polonius/` at `a30aa906`:

- A **localized constraint graph** has nodes (region, point). Outlives constraints from typeck
  connect regions at the same point (`Locations::Single`) or at every point (`Locations::All`);
  liveness connects a live region at a point to itself at the successor point
  (`constraints.rs:41`, `mod.rs:1-20`).
- Liveness edges follow variance: covariant regions get forward edges, contravariant backward,
  invariant both (`ConstraintDirection`, `mod.rs:114-123`); universal regions only forward, and an
  unknown variance falls back to bidirectional (`liveness.rs`). Backward edges let loans flow "back
  in time" (`mod.rs:17-18`).
- The graph is built lazily during traversal (since
  [#150551](https://github.com/rust-lang/rust/pull/150551), merged 2026-02-13, reported). For each
  loan the walk starts at (loan region, reservation point); the loan is live at a point when the
  walk reaches a region **live** at that point; results go in `LiveLoans`, a points × borrows
  bitset (`mod.rs:64-85,136`). Region liveness for this comes from `DeferredRegionLiveness`
  (`liveness.rs:64`).
- **Kills are not used in the traversal**; that simplification is what makes it cheap and still
  handles NLL problem case 3 (reported: [#143093](https://github.com/rust-lang/rust/pull/143093)
  body: "we also can simplify traversal by removing looking at kills").
- The out-of-scope points for `Borrows` are the frontier where a loan stops being live
  (`dataflow.rs:246`); errors are still "an access conflicts with a loan in scope" (`mod.rs:22-28`).
- The alpha has more "boring" locals than NLL (locals whose types contain only regions outliving
  free regions), and records NLL's set to keep diagnostics identical (`mod.rs:100-104`).
- What stays NLL: SCC region inference, type tests, member constraints at SCC level, placeholder
  handling, and free-region errors (`region_context.rs:213-229`). This is why
  `flow-sensitive-invariance.rs` errors under the alpha and not under legacy (read: test). The
  goals update of 2025-11-25 says member constraints are still applied at the SCC level and need
  localizing (reported: [goals-issue-118](../upstream/borrowck/project-goals/goals-issue-118-status-updates.md)).
- The 2026 goal FAQ calls the alpha "a superset of the current NLL analysis" (reported:
  [2026_polonius.md](../upstream/borrowck/project-goals/2026_polonius.md)); in the 46 tests no
  revision passes under NLL and fails under the alpha (read: table).

### 2.3 The legacy (datalog) analysis and why it was left

- Under `legacy`, rustc emits `AllFacts` (`loan_issued_at`, `cfg_edge`, `loan_killed_at`,
  `subset_base`, `loan_invalidated_at`, `var_used_at`/`var_defined_at`/`var_dropped_at`,
  `use_of_var_derefs_origin`, `drop_of_var_derefs_origin`, `child_path`, `path_is_var`,
  `path_assigned_at_base`, `path_moved_at_base`, `path_accessed_at_base`,
  `known_placeholder_subset`, `placeholder`) and runs `polonius_engine::Output::compute` with the
  algorithm from the `POLONIUS_ALGORITHM` environment variable, `Hybrid` by default (read:
  `nll.rs:166-180`, `polonius/legacy/facts.rs`,
  [polonius-engine_src_facts.rs](../upstream/borrowck/polonius/polonius-engine_src_facts.rs)).
  Legacy reimplements liveness itself (reported: #153215 body).
- Why it was left: it was too slow, with some programs "considerably slower than NLL to the extent
  that using that implementation/formulation of Polonius was a non-starter" (reported: blog
  2026-08-04); "did not scale and had no path to stabilization" (reported: 2026 goal FAQ). The 2023
  "Polonius revisited" formulation instead extends NLL's machinery (reported:
  [babysteps excerpts](../upstream/borrowck/blog/babysteps-2023-polonius-revisited-excerpts.md),
  [Inside Rust 2023-10-06](../upstream/borrowck/blog/inside-rust-2023-10-06-polonius-update.md)).
- rustc's legacy module: "Will be removed in the future, once the in-tree `-Zpolonius=next`
  implementation reaches feature parity" (read: `polonius/legacy/mod.rs:1-4`).

### 2.4 Goals and status timeline

| when | what | mark |
|---|---|---|
| 2025h1 | goal "Scalable Polonius support on nightly": a location-sensitive prototype that scales | reported: [2025h1_polonius.md](../upstream/borrowck/project-goals/2025h1_polonius.md) |
| 2025-08-10 | alpha analysis lands ([#143093](https://github.com/rust-lang/rust/pull/143093)) | reported |
| 2025h2 | goal "Stabilizable Polonius support on nightly": perf, crater, soundness of opaque-type liveness | reported: [2025h2_polonius.md](../upstream/borrowck/project-goals/2025h2_polonius.md), goals#118 |
| 2026-02-13 | lazy localized constraints ([#150551](https://github.com/rust-lang/rust/pull/150551)); "feels stabilizable" | reported |
| 2026 | goal "Stabilize and model Polonius Alpha": fix the opaque-liveness soundness issue, nightly preview, CI testing, model in a-mir-formality, perf (10-20% accepted), stabilization report | reported: [2026_polonius.md](../upstream/borrowck/project-goals/2026_polonius.md) |
| 2026-07-02 | +1.4% average on the top 20,000 crates, worst ~2.5x; CI job `x86_64-gnu-next-trait-solver-polonius` runs `tests/ui --compare-mode polonius` on every PR | reported: goals#118; CI script read at `a30aa906` |
| 2026-08-05 | [#159343](https://github.com/rust-lang/rust/pull/159343) "Enable polonius alpha on nightly" merged (MCP [compiler-team#1015](https://github.com/rust-lang/compiler-team/issues/1015)); default from nightly-2026-08-06; tracking issue [#160456](https://github.com/rust-lang/rust/issues/160456) | reported |
| 2026-08-04 | blog: aiming to stabilize "prior to the end of the year"; alpha is not a subset of legacy; opt out with `-Zpolonius=off` | reported |
| 2026-08-21 | next trait solver becomes nightly's default (#160619) | reported: #164420 body |
| 2026-10-20 | no stabilization PR found; six open soundness issues (below) | reported / unverified as exhaustive |

Open soundness issues touching the alpha (reported, bodies in
[github-items.md](../upstream/borrowck/rust-lang-rust/github-items.md)):

- [#153215](https://github.com/rust-lang/rust/issues/153215): the liveness free-region visitor
  marks regions outlived by an opaque as dead; the alpha relies on liveness to propagate loans.
- [#161038](https://github.com/rust-lang/rust/issues/161038): impl Trait, segfault on nightly.
- [#161471](https://github.com/rust-lang/rust/issues/161471): higher-ranked fn pointers.
- [#164337](https://github.com/rust-lang/rust/issues/164337): `impl Trait` return through a
  projection-typed parameter called in a closure; accepted by `next` since 2025-07.
- [#164420](https://github.com/rust-lang/rust/issues/164420): fix for #160669 skipped with a
  higher-ranked type, old trait solver only (default nightly rejects).
- [#164421](https://github.com/rust-lang/rust/issues/164421): borrow held by an `impl Trait`
  value lost through a closure; the fn-pointer form is accepted by NLL and stable too.

### 2.5 NLL problem case #3 and what the alpha accepts and rejects

Problem case #3 is a conditional return of a borrow: in the `Some` arm the borrow flows into the
return region `'r`, so NLL makes it live everywhere `'r` is, including the `None` arm that
re-borrows the map (reported: blog 2026-08-04, 2026 goal). The alpha accepts it because the loan is
only live where a live region reaches it (read: section 2.2). Examples from the tests at `a30aa906`
(read):

- **Accepted by the alpha, rejected by NLL**: the ten
  `tests/ui/nll/polonius/nll-problem-case-3-issue-{21906,51526,51545,54663,58787,68934,112087,123839,124070,124254}.rs`,
  `nll-problem-case-2-issue-92038.rs`, `filtering-lending-iterator-issue-92985.rs` (lending
  iterator), `iterating-updating-mutref.rs`, `location-insensitive-constraints-issue-70044.rs`,
  and `nll-legacy-unnecessary-error.rs` (rejected by both NLL and legacy).
- **Rejected by the alpha, accepted by legacy**: `iterating-updating-cursor-issue-{57165,63908,108704}.rs`
  (linked-list cursor with conditional reborrow; `known-bug` in both NLL and alpha revisions, and
  the blog's `conditional()` example), `flow-sensitive-invariance.rs`, `tests/ui/nll/issue-46589.rs`.
- **Rejected by all**: `lending-iterator-sanity-checks.rs`, `polonius-smoke-test.rs`,
  `subset-relations.rs`, `assignment-to-differing-field.rs`, `tests/ui/nll/get_default.rs`.

### 2.6 How rustc tests Polonius

- compiletest's `--compare-mode polonius` adds `-Zpolonius=next` to every UI test and, when there is
  no `.polonius.stderr`, compares with the normal `.stderr` (read: `src/tools/compiletest/src/runtest.rs:1939-1945,2757-2762`).
- Tests that pin modes themselves use revisions, typically
  `//@ revisions: nll polonius legacy` with `//@ [nll] compile-flags: -Zpolonius=off`,
  `//@ [polonius] compile-flags: -Z polonius=next`, `//@ [legacy] compile-flags: -Z polonius=legacy`,
  plus `//@ ignore-compare-mode-polonius (explicit revisions)`; each revision has its own
  `.<rev>.stderr` and `//[rev]~ ERROR` annotations (read: e.g.
  [rpit-hide-lifetime-for-swap.rs](../upstream/borrowck/rust-lang-rust/tests-ui/borrowck_alias-liveness_rpit-hide-lifetime-for-swap.rs)).
- A revision without a `-Zpolonius` flag gets the build's default, which on the nightly/dev
  builds CI tests is `next` (read: finding 1). Two `nll`-named revisions have no flag
  (`flow-sensitive-invariance.rs`, `lending-iterator-sanity-checks.rs`) (read: table).
- **Counts** at `a30aa906` (read: `rg` over `tests/ui`):
  - 46 `.rs` files whose `//@ revisions:` names a polonius revision: 32 in `tests/ui/nll/polonius`
    (of its 34 `.rs` files), 9 in `tests/ui/borrowck/alias-liveness`, and one each in
    `wf/hir-wf-check-erase-regions.rs`, `regions/issue-78262.rs`, `nll/issue-46589.rs`,
    `nll/get_default.rs`, `drop/dropck-normalize-errors.rs`;
  - 47 files contain `ignore-compare-mode-polonius`;
  - revision names used: `polonius` 36, `nll` 31, `legacy` 26, `polonius_alpha` 8, `polonius_next` 6;
  - of the 46: 15 rejected by NLL and accepted by the alpha, 5 accepted by legacy and rejected by
    the alpha, 1 accepted by the alpha and rejected by legacy
    ([table](../upstream/borrowck/rust-lang-rust/ui-polonius-revisions-table.md)).

## 3. rust-analyzer's borrowck and blaze's state

- rust-analyzer's `hir-ty/src/mir/borrowck.rs` (696 lines) called itself "an ad-hoc
  implementation, only useful for mutability analysis" (read:
  [hir-ty_src_mir_borrowck.rs](../upstream/borrowck/rust-analyzer/hir-ty_src_mir_borrowck.rs)).
  Its `BorrowckResult` held `mutability_of_locals` (`MutabilityReason::{Mut{spans}, Not, Unused}`),
  `moved_out_of_ref`, `partially_moved` and `borrow_regions`, computed with an "ever initialized"
  pass; it had no regions, loans, liveness or conflict checking (read: same file).
- It fed IDE diagnostics: `need-mut` (shown as E0384 "cannot mutate immutable variable"),
  `unused-mut`, `unused-variables`, and `moved-out-of-ref` (E0507) (read:
  [mutability_errors.rs](../upstream/borrowck/rust-analyzer/ide-diagnostics_handlers_mutability_errors.rs),
  [moved_out_of_ref.rs](../upstream/borrowck/rust-analyzer/ide-diagnostics_handlers_moved_out_of_ref.rs)).
  rustc gives E0596/E0594 for most "needs mut" cases and E0384 only for reassignment (read: section 1.4),
  so the codes did not line up.
- Upstream removed it in [rust-lang/rust-analyzer#23043](https://github.com/rust-lang/rust-analyzer/pull/23043)
  ("Our borrowck is both hopeless and not important", merged 2026-08-05) (reported). In blaze's
  history that is commit `ed03b3740084` (parent `6817dd3094`), reached through the
  `vendor/rust-analyzer/` subtree, deleting about 3,100 lines in 22 files (read: `git show --stat`).
  blaze did not remove it; it inherited the removal.
- blaze's documentation says there is no borrow checker and to keep running `cargo check`; a warm
  daemon ran a program using a `String` after a move that rustc rejects with E0382 (read:
  `README.md:108`, `docs/support.md:179-185`).
- In this worktree another lane has uncommitted `crates/blaze-engine/src/borrowck.rs`
  (`blaze-engine borrowck [--edition E]`, JSON diagnostics per file) and `harness/borrowck_ui.py`, which
  scores against rustc's own UI tests in two modes, passing `-Zpolonius=next` for the primary score
  and `-Zpolonius=off` for NLL (read: `harness/borrowck_ui.py:1-30,244`; not committed, may change).
  Its `polonius_of` maps `-Zpolonius=no` to legacy, where rustc treats `no` as off; no test at
  `a30aa906` uses `no`, so it does not matter today (read: line 105 and a grep).
- rust-analyzer's MIR at the vendored commit has what borrowck needs in places: `FakeRead`,
  `StorageLive`/`StorageDead`, `Deinit`, shallow (fake) borrows, `MutBorrowKind::TwoPhasedBorrow`,
  a `MirPhase::Built` before drop elaboration, and promoted locals (read:
  `crates/hir-ty/src/mir.rs:672-710,1052-1064,1356-1409`). It lacks
  `AscribeUserType` (commented out at `mir.rs:1063`), which rustc uses for `let x: &'a T`
  annotations, and MIR lowering erases regions (read: `mir/lower.rs:3294,3765`). Erased regions are
  fine for locals since rustc renumbers them anyway; user annotations and the signature's
  universal regions have to come from elsewhere (unverified as a complete list of gaps).

## 4. Recommendation

**Target the pinned nightly exactly.** The reference for "correct" is rustc nightly-2026-10-09 on
the same file: its default (the Polonius alpha with the next trait solver) for blaze's default
mode, and `-Zpolonius=off` for an NLL mode that matches beta and stable. A unit is right when the
set of (error code, line) matches, as the harness scores it. Pass the flag explicitly when running
rustc as the oracle; never rely on the default, which differs between nightly and stable builds.

Implications:

- Matching exactly means also accepting what the alpha wrongly accepts (section 2.4's issues). That
  is what `blaze` users get from nightly rustc anyway; keep those cases as known
  divergences from soundness, and follow upstream fixes when the pin moves (unverified judgment).
- Do not implement the datalog analysis: rustc is deleting it and it is not the target (read and
  reported: section 2.3). Treat `legacy` revisions as out of scope, as the harness does.
- Since the alpha only changes when loans go out of scope (section 2.2), the NLL mode is the same
  implementation with the NLL out-of-scope computation swapped in. Build NLL first, then add the
  alpha's loan liveness; the NLL-mode score is a checkpoint, not a separate project.

Pieces to build, each mirroring a rustc module (all paths `compiler/rustc_borrowck/src/` at
`a30aa906` unless noted):

1. **Borrowck MIR.** rust-analyzer's `MirPhase::Built` body plus what is missing: user type
   annotations (`AscribeUserType`), and a check that fake borrows, fake reads and false edges match
   rustc's `mir_promoted` closely enough. Mirror `rustc_mir_build` output, not a module here.
2. **Universal regions and implied bounds**: `universal_regions.rs`,
   `type_check/free_region_relations.rs`, closure signatures and captured regions.
3. **Renumbering**: `renumber.rs`.
4. **MIR type check producing constraints** with `Locations::All`/`Single`, type tests and member
   constraints: `type_check/mod.rs`, `type_check/relate_tys.rs`, `type_check/constraint_conversion.rs`,
   `type_check/input_output.rs`. This needs region obligations out of rust-analyzer's next-solver
   (`rustc_type_ir` based); how much it already exposes is unverified.
5. **Move data and initialization**: `rustc_mir_dataflow::move_paths`, `MaybeUninitializedPlaces`,
   `EverInitializedPlaces`, `MaybeInitializedPlaces` (for drop-liveness).
6. **Liveness**: `type_check/liveness/{mod,trace,local_use_map}.rs`, including dropck outlives
   (which needs the trait solver) and the boring/relevant local split.
7. **Region inference**: `handle_placeholders.rs`, `constraints/`, `region_infer/` (SCCs, values,
   `solve`, type tests with `eval_verify_bound`, universal-region checks, closure requirement
   promotion), and `root_cx.rs` for nested bodies and opaque types (`region_infer/opaque_types.rs`).
8. **Borrow set and two-phase borrows**: `borrow_set.rs`, `path_utils.rs`.
9. **Loans in scope**: `dataflow.rs` (`Borrows` with both out-of-scope precomputers,
   `kill_borrows_on_place`).
10. **Polonius alpha loan liveness**: `polonius/{mod,constraints,liveness,liveness_constraints}.rs`
    (localized graph, variance directions, per-loan traversal, `LiveLoans`).
11. **The check walk**: `lib.rs` (`access_place`, `check_access_for_conflict`,
    `check_access_permissions`, move and initialization checks, activations, exit and yield
    checks), `places_conflict.rs`, `prefixes.rs`.
12. **`unused_mut`**: `used_muts.rs` and `lint_unused_mut`.
13. **Diagnostics**: the error codes of section 1.4 with rustc's primary spans first (that is what
    the harness scores); message text and notes later, from `diagnostics/`.

Order of work, judged by what the UI tests score (unverified estimate): moves and initialization
(E0381, E0382, E0505, E0507-E0509) and mutability (E0384, E0594, E0596, `unused_mut`) need no
region inference and cover many tests; then loans with NLL scopes (E0499-E0506, E0597, E0716);
then universal regions and type tests (E0515, E0521, "lifetime may not live long enough"); then the
alpha's liveness, scored on the 15 tests where the modes differ; opaque types and closures'
requirements last, where the open soundness bugs also are.

## Sources

Everything cited is in [docs/upstream/borrowck/](../upstream/borrowck/SOURCE.md) or at
`a30aa906` in rust-lang/rust (read from a sparse checkout of that commit). The dev guide chapters
there (`rustc-dev-guide/`) are the background for sections 1.2-1.3; the Polonius book and
`polonius-engine` facts for 2.3; the project goals, goals#118 updates, the 2026-08-04 blog post and
the GitHub items for 2.4.
