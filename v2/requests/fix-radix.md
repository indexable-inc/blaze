
## fix-radix: the radix sort moves to a leaf crate `blaze-radix` (2026-10-29)

- What: root `Cargo.toml` `[workspace.dependencies]`: add `blaze-radix = { path = "crates/blaze-radix" }`
  (next to `blaze-ids`). The member is already covered by `crates/*`.
- What: `docs/v2/plan/reference/crates.md`: list `crates/blaze-radix` (owner `fix-radix`): the one
  stable LSD radix sort, a leaf crate with no dependencies.
- Why: `blaze-core` uses the radix sort, `blaze-check` (the model checker) depends on `blaze-core`,
  and `blaze-ids` takes `blaze-check` under `cfg(blaze_check)`: with the sort in `blaze-ids` that is
  a cycle (asked by safety-model-format; docs/v2/plan/lanes/safety/model.md keeps `blaze-core` below
  crates with model tests). `blaze-ids` now re-exports it (`pub use blaze_radix as radix;`), so every
  `blaze_ids::radix::...` call site is unchanged; `blaze-core` and `blaze-graph` use `blaze-radix`
  directly and no longer depend on `blaze-ids`.

> applied: `blaze-radix = { path = "crates/blaze-radix" }` is in the root `[workspace.dependencies]` beside `blaze-ids` (the crate has its `Cargo.toml` and `src/lib.rs`). crates.md's line: forwarded to coordinator. Lock: a new path crate.
> applied: root `blaze-radix = { path = "crates/blaze-radix" }` beside `blaze-ids` (the manifest lint found three users); the `crates.md` line is the coordinator's, on its list in `/tmp/blaze-lanes/v2-requests.md`.
