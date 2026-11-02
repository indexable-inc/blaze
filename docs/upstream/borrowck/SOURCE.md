# docs/upstream/borrowck: what was fetched, from where, when

Every file here was fetched or produced on **2026-10-20** for
[docs/research/borrowck.md](../../research/borrowck.md). Files are verbatim unless the notes say
"excerpt" or "trimmed". In file names, `/` in the upstream path became `_`.

Pinned rustc: rust-lang/rust `a30aa9064df6a504b6a323c6884a2f4cfbfff666` (nightly-2026-10-09), the
commit `rustc +nightly-2026-10-09 -vV` reports.

## rustc-dev-guide/ (borrow check chapters)

Source: `https://raw.githubusercontent.com/rust-lang/rust/a30aa9064df6a504b6a323c6884a2f4cfbfff666/src/doc/rustc-dev-guide/src/<path>`
(the dev guide as vendored at the pinned commit; rendered at `https://rustc-dev-guide.rust-lang.org/<path without .md>.html`).
Licence: MIT OR Apache-2.0 (rust-lang/rust). Verbatim.

| file | `<path>` |
|---|---|
| `borrow-check.md` | `borrow-check.md` |
| `borrow-check_type-check.md` | `borrow-check/type-check.md` |
| `borrow-check_two-phase-borrows.md` | `borrow-check/two-phase-borrows.md` |
| `borrow-check_drop-check.md` | `borrow-check/drop-check.md` |
| `borrow-check_moves-and-initialization.md` | `borrow-check/moves-and-initialization.md` |
| `borrow-check_moves-and-initialization_move-paths.md` | `borrow-check/moves-and-initialization/move-paths.md` |
| `borrow-check_region-inference.md` | `borrow-check/region-inference.md` |
| `borrow-check_region-inference_closure-constraints.md` | `borrow-check/region-inference/closure-constraints.md` |
| `borrow-check_region-inference_constraint-propagation.md` | `borrow-check/region-inference/constraint-propagation.md` |
| `borrow-check_region-inference_error-reporting.md` | `borrow-check/region-inference/error-reporting.md` |
| `borrow-check_region-inference_lifetime-parameters.md` | `borrow-check/region-inference/lifetime-parameters.md` |
| `borrow-check_region-inference_member-constraints.md` | `borrow-check/region-inference/member-constraints.md` |
| `borrow-check_region-inference_placeholders-and-universes.md` | `borrow-check/region-inference/placeholders-and-universes.md` |

## project-goals/ (Rust project goals for Polonius)

Source: `https://raw.githubusercontent.com/rust-lang/goals/main/src/<path>`. The repository was
renamed from `rust-lang/rust-project-goals` to `rust-lang/goals` (the GitHub API answers "Moved
Permanently" for the old name; repository id 782098660). `main` was at `9a52f675` (2026-10-16).
Rendered at `https://rust-lang.github.io/rust-project-goals/<path without .md>.html`. Verbatim.

| file | `<path>` |
|---|---|
| `2025h1_polonius.md` | `2025h1/Polonius.md` |
| `2025h2_polonius.md` | `2025h2/polonius.md` |
| `2026_polonius.md` | `2026/polonius.md` ("Stabilize and model Polonius Alpha") |
| `2026_borrowck-fcw.md` | `2026/borrowck-fcw.md` |
| `2026_roadmap-borrow-checker-within.md` | `2026/roadmap-borrow-checker-within.md` |
| `goals-issue-118-status-updates.md` | all 21 comments of [rust-lang/goals#118](https://github.com/rust-lang/goals/issues/118), from `https://api.github.com/repos/rust-lang/goals/issues/118/comments?per_page=100`, rendered as markdown with a link per comment |

## polonius/ (the datalog Polonius repository)

Source: `https://raw.githubusercontent.com/rust-lang/polonius/main/<path>`, `main` at `41ae30d2`
(2026-10-02). Licence: MIT OR Apache-2.0. Verbatim.

| file | `<path>` |
|---|---|
| `README.md` | `README.md` |
| `RELEASES.md` | `RELEASES.md` |
| `book_src_current_status.md` | `book/src/current_status.md` |
| `book_src_what_is_polonius.md` | `book/src/what_is_polonius.md` |
| `book_src_rules_atoms.md` | `book/src/rules/atoms.md` |
| `book_src_rules_relations.md` | `book/src/rules/relations.md` |
| `polonius-engine_README.md` | `polonius-engine/README.md` |
| `polonius-engine_src_facts.rs` | `polonius-engine/src/facts.rs` (the `AllFacts` relations rustc emits under `-Zpolonius=legacy`) |

## blog/

| file | source | notes |
|---|---|---|
| `rust-blog-2026-08-04-enabling-polonius-alpha-on-nightly.md` | `https://raw.githubusercontent.com/rust-lang/blog.rust-lang.org/main/content/enabling-polonius-alpha-on-nightly/index.md`; page `https://blog.rust-lang.org/2026/08/04/enabling-polonius-alpha-on-nightly/` | verbatim; repository licence Apache-2.0 / MIT per the GitHub API |
| `inside-rust-2023-10-06-polonius-update.md` | `https://raw.githubusercontent.com/rust-lang/blog.rust-lang.org/main/content/inside-rust/polonius-update/index.md`; page `https://blog.rust-lang.org/inside-rust/2023/10/06/polonius-update/` | verbatim |
| `babysteps-2023-polonius-revisited-excerpts.md` | `https://smallcultfollowing.com/babysteps/blog/2023/09/22/polonius-part-1/` and `https://smallcultfollowing.com/babysteps/blog/2023/09/29/polonius-part-2/`, read through Exa | **excerpts** only (short quotations and a summary), the posts are Niko Matsakis's personal blog |

## rust-lang-rust/

| file | source | notes |
|---|---|---|
| `pinned-polonius-flag.md` | `https://raw.githubusercontent.com/rust-lang/rust/a30aa9064df6a504b6a323c6884a2f4cfbfff666/<path>` for `compiler/rustc_session/src/config.rs`, `compiler/rustc_session/src/options.rs`, `src/bootstrap/src/core/build_steps/compile.rs`, `src/tools/compiletest/src/common.rs`, `src/tools/compiletest/src/runtest.rs`, `src/ci/docker/scripts/x86_64-gnu-next-trait-solver-polonius.sh`; plus `rustc +nightly-2026-10-09 -Z help` run locally | **excerpts** with line numbers |
| `github-items.md` | GitHub REST API `https://api.github.com/repos/<repo>/issues/<n>` and `/pulls/<n>` for rust-lang/rust #159343, #160456, #160588, #153215, #161038, #161471, #164337, #164420, #164421, #161909, #143093, #150551; rust-lang/compiler-team #1015; rust-lang/rust-analyzer #23043 | item metadata (state, dates, labels) plus bodies, some truncated where marked; comments not included |
| `ui-polonius-revisions-table.md` | derived by a script from `tests/ui` of the sparse checkout of rust-lang/rust at the pinned commit (`/tmp/rust-bck`) | **derived**, not fetched: per-revision verdicts of the 46 tests with a polonius revision; method in the file header |
| `tests-ui/*.rs`, `tests-ui/*.stderr` | `tests/ui/<path>` at the pinned commit (same sparse checkout; identical to `https://raw.githubusercontent.com/rust-lang/rust/a30aa9064df6a504b6a323c6884a2f4cfbfff666/tests/ui/<path>`) | verbatim; `<path>` is the table's path for the file name (below) |

The `tests-ui/` `.rs` files are all 46 tests `ui-polonius-revisions-table.md` lists, named by
the table's path with `tests/ui/` dropped and `/` turned into `_` (read a name back through the
table, since upstream names have `_` of their own). The first 14 came from the sparse checkout on
2026-10-20; the other 32 were fetched on **2026-11-02** from
`https://raw.githubusercontent.com/rust-lang/rust/a30aa9064df6a504b6a323c6884a2f4cfbfff666/tests/ui/<path>`
for the borrowck-cursor lane (`harness/v2/frontend/borrowck_flow.py` runs them); the 14 already
here were fetched again then and are byte for byte the same. The two `.stderr` files are
`nll/polonius/nll-problem-case-3-issue-21906.nll.stderr` and
`nll/polonius/iterating-updating-cursor-issue-57165.polonius.stderr`.

## rust-analyzer/ (the borrowck rust-analyzer removed)

Source: `git show ed03b37400^:crates/<path>` in the blaze repository. Commit
`ed03b3740084da2cb9fc847d0dd7cdf4c97106e3` ("Remove borrowck", Chayim Refael Friedman, parent
`6817dd3094`) is rust-analyzer's own commit from
[rust-lang/rust-analyzer#23043](https://github.com/rust-lang/rust-analyzer/pull/23043) ("anti-feat:
Remove borrowck"), reachable in blaze's history through the `vendor/rust-analyzer/` subtree, so its
paths are `crates/...`. Upstream equivalent, assuming the subtree kept upstream's hashes (not
checked): `https://github.com/rust-lang/rust-analyzer/blob/6817dd3094203c8bc4911f5799baf897623003de/crates/<path>`.
Licence: MIT OR Apache-2.0.

| file | `<path>` | notes |
|---|---|---|
| `hir-ty_src_mir_borrowck.rs` | `hir-ty/src/mir/borrowck.rs` | verbatim (696 lines) |
| `ide-diagnostics_handlers_mutability_errors.rs` | `ide-diagnostics/src/handlers/mutability_errors.rs` | **trimmed** before `#[cfg(test)]` |
| `ide-diagnostics_handlers_moved_out_of_ref.rs` | `ide-diagnostics/src/handlers/moved_out_of_ref.rs` | **trimmed** before `#[cfg(test)]` |
| `removal-commit-ed03b37400-hir-lib.diff` | `git show ed03b37400 -- crates/hir/src/lib.rs` | **excerpt**: the part of the removal diff in `hir`, showing what the diagnostics consumed |
