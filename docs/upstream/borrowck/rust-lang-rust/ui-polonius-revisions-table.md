# Verdicts of the `tests/ui` files whose `//@ revisions:` line names a polonius revision

Derived 2026-10-20 from the sparse checkout of rust-lang/rust at
`a30aa9064df6a504b6a323c6884a2f4cfbfff666` (`/tmp/rust-bck/tests/ui`) by a script that reads, for
each revision, its `compile-flags` (mode: `nll` = `-Zpolonius=off`, `next` = `-Zpolonius=next`,
`legacy` = `-Zpolonius=legacy` or bare `-Zpolonius`, `default` = no `-Zpolonius` flag, which on the
pinned nightly means `next`), its pass directive (`check-pass` etc.), `known-bug`, `//[rev]~ ERROR`
annotations and the presence of a `<name>.<rev>.stderr` file. "error" means the revision is
expected to fail to compile somewhere in the file; it is per file, not per function.

Files: 46 (`rg -l --glob '*.rs' '^//@\s*revisions:.*polonius' tests/ui`).

| test | revisions: name (mode) = verdict |
|---|---|
| `tests/ui/borrowck/alias-liveness/drop-liveness-invariance-issue-160670.rs` | nll (nll) = error, polonius (next) = error, legacy (legacy) = error |
| `tests/ui/borrowck/alias-liveness/gat-hide-lifetime-for-swap.rs` | edition2015 (nll) = error, edition2024 (nll) = error, polonius_alpha (next) = error |
| `tests/ui/borrowck/alias-liveness/gat-hide-lifetime-in-type-arg-for-swap.rs` | edition2015 (nll) = error, edition2024 (nll) = error, polonius_alpha (next) = error |
| `tests/ui/borrowck/alias-liveness/gat-outlives-env.rs` | edition2015 (nll) = error, edition2024 (nll) = error, polonius_alpha (next) = error |
| `tests/ui/borrowck/alias-liveness/gat-outlives-implied.rs` | edition2024 (nll) = error, polonius_alpha (next) = error |
| `tests/ui/borrowck/alias-liveness/gat-outlives.rs` | edition2024 (default) = pass, polonius_alpha (default) = pass |
| `tests/ui/borrowck/alias-liveness/rpit-hide-lifetime-for-swap.rs` | edition2015 (nll) = error, edition2024 (nll) = error, polonius_alpha (next) = error |
| `tests/ui/borrowck/alias-liveness/rpit-hide-lifetime-in-type-arg-for-swap.rs` | edition2015 (nll) = error, edition2024 (nll) = error, polonius_alpha (next) = error |
| `tests/ui/borrowck/alias-liveness/rpitit-hide-lifetime-for-swap.rs` | edition2015 (nll) = error, edition2024 (nll) = error, polonius_alpha (next) = error |
| `tests/ui/drop/dropck-normalize-errors.rs` | nll (nll) = error, polonius (next) = error |
| `tests/ui/nll/get_default.rs` | nll (nll) = error, polonius (next) = error, legacy (legacy) = error |
| `tests/ui/nll/issue-46589.rs` | nll (nll) = error [known-bug], polonius (next) = error [known-bug], legacy (legacy) = pass |
| `tests/ui/nll/polonius/assignment-kills-loans.rs` | polonius_next (next) = pass, polonius (legacy) = pass |
| `tests/ui/nll/polonius/assignment-to-differing-field.rs` | polonius (next) = error, legacy (legacy) = error |
| `tests/ui/nll/polonius/boring-local-drop-liveness.rs` | nll (nll) = error, polonius_next (next) = error |
| `tests/ui/nll/polonius/boring-local-liveness.rs` | nll (nll) = error, polonius_next (next) = error |
| `tests/ui/nll/polonius/call-kills-loans.rs` | polonius_next (next) = pass, polonius (legacy) = pass |
| `tests/ui/nll/polonius/filtering-lending-iterator-issue-92985.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/flow-sensitive-invariance.rs` | nll (default) = error, polonius (next) = error, legacy (legacy) = pass |
| `tests/ui/nll/polonius/iterating-updating-cursor-issue-108704.rs` | nll (nll) = error [known-bug], polonius (next) = error [known-bug], legacy (legacy) = pass |
| `tests/ui/nll/polonius/iterating-updating-cursor-issue-57165.rs` | nll (nll) = error [known-bug], polonius (next) = error [known-bug], legacy (legacy) = pass |
| `tests/ui/nll/polonius/iterating-updating-cursor-issue-63908.rs` | nll (nll) = error [known-bug], polonius (next) = error [known-bug], legacy (legacy) = pass |
| `tests/ui/nll/polonius/iterating-updating-mutref.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/lending-iterator-sanity-checks.rs` | nll (default) = error, polonius (next) = error, legacy (legacy) = error |
| `tests/ui/nll/polonius/location-insensitive-constraints-issue-70044.rs` | nll (nll) = error, polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/location-insensitive-scopes-issue-116657.rs` | nll (nll) = error, polonius (next) = error |
| `tests/ui/nll/polonius/location-insensitive-scopes-issue-117146.rs` | nll (nll) = error, polonius (next) = error |
| `tests/ui/nll/polonius/location-insensitive-scopes-liveness.rs` | nll (nll) = pass, polonius_next (next) = pass, polonius (legacy) = pass |
| `tests/ui/nll/polonius/nll-legacy-unnecessary-error.rs` | nll (nll) = error, polonius (next) = pass, legacy (legacy) = error |
| `tests/ui/nll/polonius/nll-problem-case-2-issue-92038.rs` | nll (nll) = error, polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-112087.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-123839.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-124070.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-124254.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-21906.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-51526.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-51545.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-54663.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-58787.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/nll-problem-case-3-issue-68934.rs` | nll (nll) = error [known-bug], polonius (next) = pass, legacy (legacy) = pass |
| `tests/ui/nll/polonius/opaque-multiple-defining-uses-160669.rs` | nll (nll) = error, polonius (next) = error, next (next) = error |
| `tests/ui/nll/polonius/polonius-smoke-test.rs` | polonius (next) = error, legacy (legacy) = error |
| `tests/ui/nll/polonius/storagedead-kills-loans.rs` | polonius_next (next) = pass, polonius (legacy) = pass |
| `tests/ui/nll/polonius/subset-relations.rs` | polonius (next) = error, legacy (legacy) = error |
| `tests/ui/regions/issue-78262.rs` | base (default) = error, polonius (legacy) = error |
| `tests/ui/wf/hir-wf-check-erase-regions.rs` | nll (nll) = error, polonius (next) = error |

NLL (or unflagged) rejects, next accepts: 15
  ./nll/polonius/filtering-lending-iterator-issue-92985.rs
  ./nll/polonius/iterating-updating-mutref.rs
  ./nll/polonius/location-insensitive-constraints-issue-70044.rs
  ./nll/polonius/nll-legacy-unnecessary-error.rs
  ./nll/polonius/nll-problem-case-2-issue-92038.rs
  ./nll/polonius/nll-problem-case-3-issue-112087.rs
  ./nll/polonius/nll-problem-case-3-issue-123839.rs
  ./nll/polonius/nll-problem-case-3-issue-124070.rs
  ./nll/polonius/nll-problem-case-3-issue-124254.rs
  ./nll/polonius/nll-problem-case-3-issue-21906.rs
  ./nll/polonius/nll-problem-case-3-issue-51526.rs
  ./nll/polonius/nll-problem-case-3-issue-51545.rs
  ./nll/polonius/nll-problem-case-3-issue-54663.rs
  ./nll/polonius/nll-problem-case-3-issue-58787.rs
  ./nll/polonius/nll-problem-case-3-issue-68934.rs
legacy accepts, next rejects: 5
  ./nll/issue-46589.rs
  ./nll/polonius/flow-sensitive-invariance.rs
  ./nll/polonius/iterating-updating-cursor-issue-108704.rs
  ./nll/polonius/iterating-updating-cursor-issue-57165.rs
  ./nll/polonius/iterating-updating-cursor-issue-63908.rs
next accepts, legacy rejects: 1
  ./nll/polonius/nll-legacy-unnecessary-error.rs
