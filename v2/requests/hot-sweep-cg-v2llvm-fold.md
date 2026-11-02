# Requests from hot-sweep-cg-v2llvm-fold

## `tools/v2/hot_lint.py`: test modules it misreads as hot code (54 false findings)

What: make `in_test_module` (1) start a test module at any `#[cfg(...)]` whose predicate
includes `test` positively (e.g. `#[cfg(all(test, feature = "llvm"))]`), not only the literal
`#[cfg(test)]`; (2) count braces with string and char literals stripped (`"{"`, `'{'`, and
`format!` escapes `{{`/`}}`), and with `//` comments stripped.

Where: `tools/v2/hot_lint.py`, `in_test_module`.

Why: `crates/hir-ty-codegen/v2/release.rs:615` is `#[cfg(all(test, feature = "llvm"))] mod tests`,
so its 24 findings (lines 648..811) are test code reported as hot. In
`crates/hir-ty-codegen/v2/fold.rs`, the tests module (986..1921) has
`format!(" {} {}}}", ..)` at line 1296, whose `}}}` the brace count takes as three closes, so it
leaves the module at line 1369 and reports 30 findings (1425..1915) in test code. Test code is
not the sweep's to change, so these stay until the lint reads them as tests.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list with hot-sweep-graph, hot-sweep-mir and hot-sweep-syntax (one change to `in_test_module` and the file walk).
