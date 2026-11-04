# Patches on crates.io `wtransport`

Base: crates.io `wtransport` 0.7.2, sha256 `b4273ce3157a3262a68665f8d3f20a0ac0c5b8a69ffd67f05ae986832ebec036`.
Left out: `Cargo.lock`, `.cargo_vcs_info.json`, `examples`.

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
`wtransport` to this directory and excludes it from the workspace, so its dev-dependencies stay
out of blaze's lock.

Gate: `blaze test` run in this directory (its own workspace; `0003` points it at the vendored
`quinn` and `quinn-proto`) builds and passes the crate's own tests (the unit tests in
`src/driver/utils.rs` and `src/tls.rs`, and the doc tests).

## Series

### 0001-no-tracing

What: every `tracing` use is deleted: the `debug!`/`trace!` statements, the
`.instrument(debug_span!("Driver", quic_id))` on the worker task and the two
`.instrument(debug_span!("Stream", "id={}"))` on stream tasks (with the `stream_id` they alone
read), and `#[instrument(name = "Stream", fields(id))]` on `handle_bi_h3_stream`; span fields
`quic_id` and `id` are dropped. The manifest drops `tracing` and the dev-dependency
`tracing-subscriber`.

Why: blaze drops `tracing` (docs/v2/plan/observe/no-tracing.md); blaze does not record
third-party logs, so the sites go rather than move to `blaze-trace`.

### 0002-no-examples-or-benches

What: the `[[example]]` targets go, with the dev-dependencies only they used (`axum`, `hyper`);
`examples/` is left out. `anyhow` stays: the doc tests use it.

Why: the examples are not the crate's tests, they set up `tracing-subscriber`, and `axum`'s
default features pull `tracing` into the gate's lock.

### 0003-tests-use-vendored-siblings

What: `[patch.crates-io]` entries for `quinn = { path = "../quinn" }` and
`quinn-proto = { path = "../quinn-proto" }` in the manifest.

Why: the gate runs this crate on its own, outside blaze's workspace; without them its tests would
build against crates.io's quinn stack, with `tracing`. cargo reads `[patch]` only in the root
manifest, so when blaze builds wtransport as a dependency this table is ignored and blaze's root
`[patch.'crates-io']` decides.
