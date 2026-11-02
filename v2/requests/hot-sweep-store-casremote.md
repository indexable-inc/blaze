# Requests from hot-sweep-store-casremote

## `tools/v2/hot_lint.py`: treat `#[cfg(all(test, ...))]` modules as test code

- What: `in_test_module` matches only the literal `#[cfg(test)]`. Also match a `cfg` whose
  predicate is `test` inside `all(...)`, e.g. `#[cfg(all(test, not(blaze_check)))]`.
- Where: `tools/v2/hot_lint.py`, `in_test_module` (line ~49).
- Why: `crates/blaze-cas/src/remote.rs` keeps its tests under
  `#[cfg(all(test, not(blaze_check)))]`, so the lint reports 80 findings inside that test module
  (the module starts at line 876), which the hot-sweep briefs say are not to be changed. 39
  files under `crates/` use `cfg(all(test, ...))`, so other sweep lanes see the same false count.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list with the other hot-sweep `in_test_module` requests.
