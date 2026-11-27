---
status: current
summary: Reading Cargo.toml and .cargo/config.toml as cargo does (blaze-manifest), and the fixtures that hold it to cargo's output.
---
# Reading workspaces as cargo does

blaze reads `Cargo.toml` and `.cargo/config.toml` itself, the way cargo reads them (ported
from cargo 1.99.0, the pin then), and never runs cargo to do it. `crates/blaze-manifest` is
that reader. Every manifest cargo accepts reads the same, and one cargo rejects is rejected
with cargo's message; the committed fixtures ([below](#fixtures)) hold it there. Part of the
self-contained blaze
([#298](https://github.com/indexable-inc/blaze/issues/298)).

## What it reads

- A workspace as `cargo metadata --no-deps --format-version 1` prints it: the root found from
  the current directory or `--manifest-path` (the roots read so far first, then up the
  directories, stopping at `CARGO_HOME`), members from `members` globs, `exclude`,
  `default-members` and path dependencies under the root, `workspace = true` fields and
  dependencies inherited (features joined, `default-features` merged as each edition does),
  `[workspace.metadata]` and `package.metadata` in their order, targets discovered and
  configured (`[lib]`, `[[bin]]`, examples, tests, benches, build scripts, `auto*`, editions,
  the 2015 inference warning), `publish`, `readme` found by default, `license-file`,
  `rust-version`, `links`, `default-run`, and dependencies renamed, per target, for build and
  for dev, with git and registry sources.
- `cargo-features`: the stable ones accepted, the unstable ones refused with cargo's message on
  the stable channel, the removed ones refused.
- `.cargo/config.toml` files from the current directory up and in `CARGO_HOME`, merged as cargo
  merges them, with `include`, `--config` (files and dotted keys) and `CARGO_*` variables. As in
  cargo, the files are read at the first value something asks for, so a broken one is an error
  only where configuration is read, and `Config::start` does what cargo does on start (the
  `alias.<command>` lookup and its warning, then `--config`).
- Typed values through a port of cargo's config deserializer (`Config::get::<T>(key)`), so a
  table read into cargo's own types takes each field from the files or its variable and fails
  or warns of an unused key with cargo's text: `[build]` (`target`, `target-dir`, `build-dir`,
  `jobs`, rustflags, …), `[net]`, `[http]`, `[env]` (relative and forced values, the variables
  it refuses), `[target.<triple>]` and `[target.'cfg(..)']` (rustflags in cargo's precedence,
  `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` over the target tables over `build.rustflags`; the
  runner and the linker, one matching `cfg` at most), `[registry]` and `[registries]`,
  `[source]` with each source's location checked and `replace-with` followed, `[resolver]`, and
  `[profile.<name>]` read as cargo's `TomlProfile`. The `cfg` of a target and the host triple
  are the caller's.
- `cargo config get` as cargo 1.99 prints it (`blaze_manifest::config::get`): `toml` with and without `--show-origin`, `json`, `json-value`, `--merged no`, the
  variables that may affect a table, and the warnings of old and doubled config files.
- Profiles as cargo 1.99 gives them to units (`blaze_manifest::profiles`, a port of cargo's
  `profiles.rs`): the root manifest's `[profile]` tables with the configuration's
  (`[profile]` in config files, `--config`, `CARGO_PROFILE_*`) merged over them, the built-in
  `dev`, `release`, `test`, `bench` and `debug`, `inherits` chains and their errors, and for a
  unit its package overrides (`[profile.x.package.<spec>]`, `"*"` for non-members),
  `build-override` for the host, `panic` cleared for tests and the host, `split-debuginfo` on
  Apple targets, `incremental` from `CARGO_INCREMENTAL`, `build.incremental` or `CI`, and the
  warnings of package specs that match no package. Each manifest's profiles are checked as
  cargo checks them reading it. What a unit is built for (`UnitFor`) is the unit graph's to
  compute; a package is anything with a name, a version and a `SourceId` (`ProfilePackage`),
  so blaze-resolve's package ids fit it.
- A manifest that does not parse or fit the schema is shown as cargo shows it, a snippet with
  the place marked (`blaze_manifest::diagnostic`).

The reader above is blaze-manifest's `cargo` feature, which no crate of the `blaze` binary
turns on: blaze-cli, blaze-drive and blaze-unit-graph take blaze-manifest with
`default-features = false`, its sans-IO `layout` reader and its `lock`, and runs no cargo.

Not yet: the unstable profile keys (`trim-paths`, `rustflags`, `codegen-backend`, which need nightly
`cargo-features`), `links` overrides in `[target.<triple>]`, a source defined twice under two names, and the warnings cargo prints for
unused manifest keys and lints. `{workspace-path-hash}` in `build.build-dir` is refused until
cargo's hash is ported. cargo checks a list against the lists that do not merge (`target.*.runner`,
`registry.credential-provider`, …) by a key path it never pops, so whether such a list merges
across files depends on the keys merged before it in `FxHashMap` order; blaze does the same.

## Fixtures

The expected output of each fixture is cargo 1.99.0's, committed, and `blaze test -p
blaze-manifest` checks it without cargo or the network:

- `crates/blaze-manifest/tests/fixtures`: workspaces, as `cargo metadata` printed them.
- `crates/blaze-manifest/tests/config-fixtures`: 18 trees, some with a `case.json` of
  variables, current directory and runs (layered files with `include`, `--config` files and keys,
  variables alone, quoting, lists that do not merge, broken files of every kind), with
  `cargo config get`'s exit code, stdout and stderr in `expected.json`; `tests/config_get.rs`
  checks them, and `tests/config_typed.rs` the typed tables, the rustflags order and `[env]`
  values as `cargo build -v` and `cargo run` showed them.
- `crates/blaze-manifest/tests/profile-fixtures` (`tests/profiles.rs`): package overrides and
  `"*"`, `build-override`, build scripts and proc macros shared between the host and the
  target, custom profiles through `inherits` chains, config profiles from files, `--config` and
  `CARGO_PROFILE_*`, `CARGO_INCREMENTAL`, `build.incremental` and `CI`, a virtual workspace run
  from a member, and every error and warning of cargo's about profiles.

## Decisions

**No cargo crate.** Manifests and config files are read, with every key's byte range, and
`cargo config get` written in cargo's spelling, by blaze's own `crates/blaze-toml`.

**cargo's config deserializer, ported.** The typed config layer is cargo's `de.rs`, `value.rs`, `key.rs`,
`path.rs`, `error.rs` and `schema.rs` ported onto blaze-manifest's `Config`: what a table reads
and every message it fails with come from the same serde walk as cargo's.
