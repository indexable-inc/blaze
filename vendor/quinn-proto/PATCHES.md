# Patches on crates.io `quinn-proto`

Base: crates.io `quinn-proto` 0.11.19, sha256 `0e750cca55fe4f0439a15d0bb529da9651e79993e8e72c61a899a36d462befbe`.
Left out: `Cargo.lock`, `.cargo_vcs_info.json`.

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
`quinn-proto` to this directory and excludes it from the workspace, so its dev-dependencies stay
out of blaze's lock.

Gate: `blaze test` run in this directory (its own workspace) builds and passes the crate's own
tests (`src/tests/`, the unit tests beside the code).

## Series

### 0001-no-tracing

What: every `tracing` use is deleted, `warn!` and `error!` included: about 200
`trace!`/`debug!`/`warn!`/`error!` statements, the `trace_span!` spans (`send` with fields
`space` and `pn`, held in `PacketBuilder::_span`; `first recv`; `recv` with `space` and `pn`;
`frame` with `ty`; all fields dropped), the `IncomingImproperDropWarner` drop warning (its `Drop`
impl goes; the type stays, so `Incoming` is unchanged), the frame-kind `match` that existed only
to log, and in the tests the `subscribe()` fmt subscriber with its `TestWriter`, the
`info_span!` guards of `Pair` and the `info!` lines. Where a deletion left an empty block or an
unused binding, the code is tidied (`Err(_)`, `let _ =` on qlog's emit, a plain `continue` for
`Padding` frames), with behaviour unchanged. The manifest drops `tracing` and the dev-dependency
`tracing-subscriber`; the `log` feature stays, empty, so quinn's `proto/log` still resolves.

Why: blaze drops `tracing` (docs/v2/plan/observe/no-tracing.md); blaze does not record
third-party logs, so the sites go rather than move to `blaze-trace`. A disabled `tracing` site
evaluates nothing, so deleting one is the behaviour blaze had with no subscriber.
