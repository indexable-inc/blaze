# Patches on crates.io `quinn`

Base: crates.io `quinn` 0.11.12, sha256 `4051e23e9185c255a7e33ef59cdbca87a22d359052eecd22fc6b901fb37d9d11`.
Left out: `Cargo.lock`, `.cargo_vcs_info.json`, `examples`, `benches`.

This directory is the published `.crate` at the base, without the entries left out, with the
series below applied in order (docs/v2/plan/observe/no-tracing.md, section 6.2).
`tools/vendor/sync.py` re-fetches the base (checked against the crates.io index and
`Cargo.lock`), replays the series and stops at the first patch that no longer applies, naming it
and each failing hunk; run it on every bump. Each `### NNNN-name` heading is
`patches/NNNN-name.patch`, a diff of the whole crate taken with `a/` and `b/` prefixes. To change
the series: edit the tree, regenerate the patch as the diff between its stages, and keep this
file's headings and `patches/` one for one.

`Cargo.toml` is the published (normalized) manifest with the series applied; `Cargo.toml.orig`
is upstream's own, as the `.crate` ships it, never patched. blaze's root `Cargo.toml` patches
`quinn` to this directory and excludes it from the workspace, so its dev-dependencies stay out of
blaze's lock.

Gate: `blaze test` run in this directory (its own workspace; `0003` points it at the vendored
`quinn-proto`) builds and passes the crate's own tests (`src/tests.rs`, `tests/`).

## Series

### 0001-no-tracing

What: every `tracing` use is deleted, `warn!` and `error!` included: the `tracing::{warn, error,
debug}!` sites, the `debug_span!("drive", id)` span `ConnectionDriver` held and entered on each
poll (field dropped), the two `.instrument(Span::current())` on the spawned endpoint and
connection drivers, the `lock_tracking` mutex's lock-time warnings (and the timing only they
read), and in the tests the `subscribe()` fmt subscriber, `error_span!(..).entered()` guards,
`.instrument(error_span!(..))` and `info!` lines. The manifest drops `tracing`, the dev-dependencies
`tracing-subscriber` and `tracing-futures`, `tracing/log` and `udp/log` from the `log` feature, and
the `tracing` feature quinn forced on `quinn-udp` (`udp/log` would turn it back on: quinn-udp's
`log` is `["tracing/log"]`). The drivers' I/O errors, which upstream only logged, are dropped
(`let _ = driver.await`), as is the dual-stack `set_only_v6` failure.

Why: blaze drops `tracing` (docs/v2/plan/observe/no-tracing.md); blaze does not record
third-party logs, so the sites go rather than move to `blaze-trace`. quinn-udp keeps no copy: its
tracing is an optional feature, now enabled by nothing.

### 0002-no-examples-or-benches

What: the `[[example]]` and `[[bench]]` targets go, with the dev-dependencies only they used
(`anyhow`, `bencher`, `clap`, `directories-next`, `url`); `examples/` and `benches/` are left out.

Why: they are not the crate's tests, and they are `tracing-subscriber` demos; keeping them would
mean porting them in `0001` or keeping `tracing` in the gate's lock.

### 0003-tests-use-vendored-siblings

What: `[patch.crates-io] quinn-proto = { path = "../quinn-proto" }` in the manifest.

Why: the gate runs this crate on its own, outside blaze's workspace; without the patch its tests
would build against crates.io's `quinn-proto`, with `tracing`, instead of the copy blaze builds.
cargo reads `[patch]` only in the root manifest, so when blaze builds quinn as a dependency this
table is ignored and blaze's root `[patch.'crates-io']` decides.
