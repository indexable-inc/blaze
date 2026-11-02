---
status: plan
summary: lane dedyn's speed of light: no `dyn` call on a hot path, and what is left.
date: 2026-10-30
---
dedyn's bound is no hot `dyn`; it is met: `tools/v2/hot_lint.py`'s `dyn` rule went from 444 findings to 13, all Rust text inside string literals (none in code); left are 297 marked cold sites (`// dyn:` with a reason), the largest `SymbolResolver` in hir-ty-codegen (41, three implementations above it), which goes when v1's rustc-symbol linking does, not by dedyn ([lane notes](../dedyn.md)).
