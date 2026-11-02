---
status: plan
summary: dedup-scan-cargo (2026-10-29): the copies between the cargo crates, blaze-units, blaze-cargo, load-cargo and blaze-build-script, each given one owner (blaze-units for what the load ported from v1, blaze-resolve for the feature map, blaze-manifest for target kinds, blaze-build-script for script keys, blaze-core for signal names), with the edits requested of lane cargo, usercode and the coordinator.
date: 2026-10-29
---
# dedup-scan-cargo: the cargo crates

Part of [dedup](../dedup.md). The one shared file written here is
[`crates/blaze-core/src/signal.rs`](../../../../../crates/blaze-core/src/signal.rs); everything
else is a request, because no path it touches is free:

- `v2/requests/cargo.md` "dedup: one copy of each piece the load ported from v1's cargo crates"
- `v2/requests/coordinator.md` "dedup: the v1 cargo crates call blaze-units' copies"
- `v2/requests/usercode.md` "dedup: one list of build-script keys, one table of signal names"

## Which way the copies point

`blaze-metadata` depends on `blaze-units`, so when v2's load (`blaze-units/src/load/`) needed
v1's summary reader, index parser and `[source]` map, it ported them. The owner is the lower
crate: `blaze-units` makes each one public and outside the `query` feature (none reads a file),
and blaze-metadata and blaze-registry call down. The other way (blaze-units calling v1) would need
blaze-metadata below blaze-units, which its graph code can't be. Lane cargo's request 1 (the grant
of the five cargo crates) covers the v1 half; the coordinator request is that half written out
until the grant lands.

## Per primitive

| Piece | Owner | Copies that go |
| --- | --- | --- |
| Summary of a `cargo metadata` package, `[patch]`/`[replace]` | `blaze_units::read` | blaze-metadata `summary.rs`, blaze-resolve `examples/metadata_resolve.rs` |
| Index line parser, `crate_url`, `dep_path` | `blaze_units::index` | blaze-registry `index.rs` (but `Summaries`), `query.rs`, `download.rs` |
| `[source]` replacement, `describe` | `blaze_units::sources::SourceMap` | blaze-registry `sources.rs`, `query.rs`; blaze-units `load/source.rs` |
| Feature map | `blaze_resolve::build_feature_map` | blaze-manifest `build_summary_features` body |
| `CrateType`, `TargetKind` | `blaze_manifest::targets` | blaze-units `load/plan/hash.rs` enums |
| Target answers (`rustc.json`) | `blaze_units::stdlib::target` | blaze-cargo `answers/own.rs` data layer, `answers/rustc.json` |
| Reserved `cargo::` keys | `blaze_build_script::output::has_reserved_prefix` | blaze-cargo `directives.rs` `RESERVED` |
| Signal names | `blaze_core::signal::described` | blaze-build-script `run.rs`, blaze-cargo `say.rs` |
| Size-capped reader | `blaze_registry::download::LimitErrorReader` | blaze-cargo `serve/fetch/archive.rs` |
| Lexical path normalizing | `blaze_manifest::paths::normalize_path` (cargo-util's) | blaze-cargo `remove.rs`, `place.rs`; blaze-registry `git.rs`; blaze-metadata `summary.rs` |
| `/`-path normalizing | `blaze_units::load::normal_text` | load-cargo `v2/files.rs`, blaze-units `load/source/tree.rs` |
| Dev profile fixture | `schema::Profile::dev()` | four test `profile()`s |

Kept on purpose: `blaze-cargo/src/cargo/metadata.rs` and `blaze-metadata/examples/units.rs` both
fill `blaze_metadata::Asked`, with different values; they are two uses of one API, not copies.

## Speed of light

- Signal names: one `match` on an `i32`, compiled to a jump table; O(1), no allocation, a
  `&'static str`. The caller's `format!` is on the error path only.
- Reserved keys: `split_once('=')` and one match on the key, no allocation; `directives.rs` today
  formats one string per key per line it reads.
- Index lines: one serde pass over the line's bytes (after the `v` and `rust_version` probes, as
  cargo), so the floor is the JSON parse; the dedup makes no line slower and blaze-registry's cache
  format (`Summaries`, `CACHE_VERSION`) is unchanged.
- Feature map: linear in features plus dependencies, three walks of the dependency list and a hash
  map of names; the same as both copies.
- `CrateType`/`TargetKind`: one-word `matches!`; the `-C metadata` hash is unchanged (same
  variants, same order, the same derived `Hash`).
- Target answers: `cfg` sets are `Bits` word operations over the one `rustc.json` (423 KB,
  embedded once instead of twice).

## Found on the way

- blaze-manifest's feature map names the unused optional dependency by iterating an
  `FxHashMap`, so which one its error names depends on hashing; cargo and blaze-resolve take the
  first in `[dependencies]` order. Moving onto blaze-resolve's fixes it.
- v1 and v2's `[source]` maps name a doubly defined source's two names in opposite orders
  (`blaze-registry/src/sources.rs` test at 495 holds v1's). cargo 1.99's `SourceConfigMap::add`
  decides; the cargo source at hand had no `SourceConfigMap`, so this is left to lane cargo.
- The simple `normalize` copies drop a relative path's leading `..`; cargo-util keeps it. Every
  current caller passes absolute paths, so no output changes.
- `blaze-new/src/lib.rs:691` has the same simple copy; blaze-new does not depend on
  blaze-manifest (left to the coordinator).
- blaze-manifest's `source::SourceId` beside blaze-source's is the larger copy behind several of
  these; one `SourceId` is for after request 1.
