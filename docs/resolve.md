# Reading workspaces as cargo does

blaze reads `Cargo.toml` and `.cargo/config.toml` itself, the way cargo reads them (ported
from cargo 1.99.0, the pin then; `harness/resolve.py` now compares with the pinned nightly's
cargo), and never runs cargo to do it. `crates/blaze-manifest` is that reader; cargo is only the oracle
`harness/resolve.py` compares it with. Every manifest cargo accepts reads the same, and one
cargo rejects is rejected with cargo's message. Part of the self-contained blaze
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
  for dev, with git and registry sources. `cargo run -p blaze-manifest --example metadata --
  --no-deps` prints the same JSON, warnings and errors.
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
  are the caller's, from rustc.
- `cargo config get` as cargo 1.99 prints it (`blaze_manifest::config::get`, the `config`
  example): `toml` with and without `--show-origin`, `json`, `json-value`, `--merged no`, the
  variables that may affect a table, and the warnings of old and doubled config files. It is how
  the harness sees the merged configuration.
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
  so blaze-resolve's package ids fit it. The `profiles` example takes the unit graph cargo
  printed and prints every unit's `profile` as blaze computes it, deriving what each unit is
  built for from the graph's edges as cargo's `unit_dependencies` does and applying the strip
  and host debuginfo adjustments of cargo's `rebuild_unit_graph_shared`.
- A manifest that does not parse or fit the schema is shown as cargo shows it, a snippet with
  the place marked (`blaze_manifest::diagnostic`).

`cargo blaze` reads the workspace with it (`crates/ra-jit/src/cargo/metadata.rs`): every
command's `cargo metadata --no-deps`, with `--manifest-path` and `--config` as the command was
given them. A manifest cargo refuses ends the command with cargo's message and status 101.

The daemon's loads read the workspace with `crates/blaze-metadata` (2026-10-25): the root found
without `cargo locate-project`, every package the lock file locks fetched by blaze-registry
where cargo keeps it, and the graph of the build the load is for, not `cargo metadata`'s: the
features and edges `cargo build` with the load's `-p`, `--workspace`, `--exclude` and
`--features` would build, for the load's target, and the packages it reaches. `cargo metadata`
unifies features over every member, so a load of one member used to have edges to crates only
another member's features reach (axum's examples to `mime_guess` through `tower-http/fs`) whose
build scripts the build never ran, and `env!` of their variables failed. A lock file that is
missing or that the manifests no longer agree with is written as cargo writes it (2026-10-26)
when the resolve needs no registry version it does not have. What it does not read yet goes to
cargo's `cargo metadata` with the reason printed: a registry version the lock file lacks
(#321), `[replace]`, a registry with a git index.
`RA_JIT_OWN_RESOLVE=0` sends every load to cargo.

Not yet: the unit graph itself (the `profiles` example walks cargo's),
the unstable profile keys (`trim-paths`, `rustflags`, `codegen-backend`, which need nightly
`cargo-features`), `links` overrides in `[target.<triple>]`, a source defined twice under two names
(blaze-registry's, which has the `SourceId` display it names), and the warnings cargo prints for
unused manifest keys and lints. `{workspace-path-hash}` in `build.build-dir` is refused until
cargo's hash is ported. cargo checks a list against the lists that do not merge (`target.*.runner`,
`registry.credential-provider`, …) by a key path it never pops, so whether such a list merges
across files depends on the keys merged before it in `FxHashMap` order; blaze does the same.

## Parity

`python3 harness/resolve.py` on dev-aws-8, 2026-10-25, at commit 00a37aa505 of this branch:
cargo 1.99.0 and blaze-manifest on every workspace of `harness/resolve/corpus.json` and the
projects of `harness/bench.py`, each from its root with a `CARGO_HOME` of the harness's own, the
JSON compared as text with the root and `CARGO_HOME` replaced, key order included.

| Part | Equal | Total |
|---|---|---|
| bench projects of `bench.py` at their pinned revisions (zed, bevy, nushell, helix, …) | 13 | 13 |
| crates.io crates, the 300 most downloaded, newest stable versions | 300 | 300 |
| workspace repositories at pinned commits (rust, servo, polkadot-sdk, agave, deno, wasmtime, uv, ruff, polars, …) | 72 | 72 |

Together 4,204 packages, 11,152 targets and 61,432 dependencies, none different. On the Mac
the same comparison over 226 local workspaces gave 202 equal and 24 that both refused, all 24
with cargo's error text (unstable `cargo-features` and `cargo-features` lists, templates that
do not parse, members that are missing).

The workspaces of `crates/blaze-manifest/tests/fixtures` are the fast tier: cargo's output for
each is committed, and `cargo test -p blaze-manifest` compares without cargo or the network.

The `config` part runs `cargo -Zunstable-options config get` (with `RUSTC_BOOTSTRAP=1`, the
command being unstable) and the `config` example with the same arguments, seven ways each
(`toml`, `--show-origin`, `--merged no` with and without it, `json`, `json-value`, and `json`
with `--show-origin`, which both refuse), on the trees of
`crates/blaze-manifest/tests/config-fixtures` (each its own `case.json` of variables, current
directory and further runs: layered files with `include`, `--config` files and keys, variables
alone, quoting, lists that do not merge, and broken files of every kind) and the root of every
bench project and workspace repository, comparing exit code, stdout and stderr as text.

On dev-aws-8, 2026-10-25, at commit 2e59bae97b: 103 of 103 equal, 767 runs, none different:
the 18 trees, the 13 bench projects and the 72 workspace repositories, 47 of whose roots have
a `.cargo/config` or `.cargo/config.toml` of their own. The other parts gave the same counts as
above.

The trees' runs and cargo's output for them are committed (`expected.json`, written by
`resolve.py --fixtures`); `tests/config_get.rs` compares them offline, and
`tests/config_typed.rs` checks the typed tables, the rustflags order and `[env]` values as
`cargo build -v` and `cargo run` showed them.

The `profiles` part runs `cargo <command> --unit-graph -Zunstable-options` (with
`RUSTC_BOOTSTRAP=1`, the flag being unstable) for `build`, `build --release`, `test --no-run`,
`bench --no-run`, `check` and `build --target` a triple that is not the host's (so that host
and target units stay apart, and Apple's `split-debuginfo` shows from Linux), and gives the
graph cargo printed to the `profiles` example, which computes every unit's `profile` with
blaze-manifest. Each profile is compared with cargo's as JSON, with the warnings about
profiles (sorted: cargo prints those of manifests in `HashMap` order) and, when cargo refuses
the profiles, its error. The trees of `crates/blaze-manifest/tests/profile-fixtures` cover
package overrides and `"*"`, `build-override`, build scripts and proc macros shared between the
host and the target, custom profiles through `inherits` chains, config profiles from files,
`--config` and `CARGO_PROFILE_*`, `CARGO_INCREMENTAL`, `build.incremental` and `CI`, a virtual
workspace run from a member, and every error and warning of cargo's about profiles.

On dev-aws-8, 2026-10-25, at commit 2d1b0a3: 86 of 89 equal, 546 runs and 235,924 units, none
with a different profile:

| Part | Equal | Total |
|---|---|---|
| profile trees of `tests/profile-fixtures` (54 runs, 219 units) | 4 | 4 |
| bench projects (72 runs, 31,949 units) | 12 | 13 |
| workspace repositories (420 runs, 203,756 units) | 70 | 72 |

bun and agave are refused by cargo past their profiles (bun's vendored `lol_html` is not
checked out, agave needs rustc 1.99.1), so there is no graph to compare. rust-clippy turns
`profile-rustflags` on in its `.cargo/config.toml`'s `[unstable]` table, which cargo reads only
on nightly: the harness's cargo accepts it through `RUSTC_BOOTSTRAP`, and blaze, which reads
configuration as the stable channel does, refuses it with cargo's stable error (stable cargo
refuses the workspace too, its `build.rustflags` passing `-Z`). The other parts gave the counts
above again.

What each unit is built for comes from the graph's edges, so the part checks the profiles, not
the unit graph: blaze's own unit graph is its own work. The walk reproduces cargo's order where
it decides a profile: the roots are interned before their dependencies and all of a unit's
dependencies before any is walked, a unit equal to one interned earlier is that one (profiles
compare without whether their debuginfo is deferred), and the first way a unit is reached in
the sharing pass decides whether the debuginfo of a host unit is lowered. Package overrides are
checked against the packages of `Cargo.lock`, as cargo checks them against the workspace's
resolve.

The `sysroot` part is the standard library's workspace in the toolchain
(`lib/rustlib/src/rust/library`), read in its directory (whose `.cargo/config.toml` replaces
crates.io with `library/vendor`) as rust-analyzer's `Sysroot::load_workspace` asks for it:
`--features sysroot/backtrace`, cargo told it is a nightly, `--no-deps` and full, with no
`--filter-platform` and with each target of `harness/answers.py`. cargo reads the full metadata
against a copy of the lock file (`CARGO_RESOLVER_LOCKFILE_PATH`); blaze-metadata's
`metadata_locked` never writes it and refuses a lock file the manifests no longer agree with, as
`--locked` does. ra-jit answers rust-analyzer with these (`ra-jit/src/cargo/metadata.rs`
`library`, through `OwnMetadata.library`), so loading a workspace runs no cargo for the sysroot.

On VM nc-sysroot (Linux arm64), 2026-10-27, with the pinned nightly: 26 of 26 equal (no platform and
the 12 targets, each `--no-deps` and full; 15 packages without dependencies, 30 to 50 with).

## Decisions

**cargo's schema crate, pinned.** `cargo-util-schemas = "=0.14.3"` is the version cargo 1.99.0
is built with: its published source is the same, file for file, as `crates/cargo-util-schemas`
in cargo's 1.99.0 tree. Using it means a manifest deserializes into exactly the types cargo's
does and fails with exactly cargo's messages (package names, versions, `PackageIdSpec`, git
references, lints), and the lock file and index types are there for blaze-resolve and
blaze-registry. It depends on neither the `cargo` crate nor anything that does: serde (with
`serde-untagged` and `serde-value`), `toml` 1.x (the one blaze already has), `semver`, `url`,
`jiff`, `thiserror`, `unicode-ident`. Owning
copies of the types would mean keeping thousands of lines in step with cargo by hand, and every
drift would be a manifest read differently. The pin moves with the toolchain blaze pins (the
versions here are cargo 1.99.0's and have not been checked against `nightly-2026-10-25`'s cargo
yet); if blaze ever needs to change one of its types, it is vendored then.

**cargo's renderer, pinned.** `annotate-snippets = "=0.12.16"`, the version cargo 1.99.0 renders
manifest diagnostics with, so that a manifest error reads character for character as cargo's.

**cargo's config deserializer, ported.** The typed config layer is cargo's `de.rs`, `value.rs`,
`key.rs`, `path.rs`, `error.rs` and `schema.rs` ported onto blaze-manifest's `Config`, not a
schema of blaze's own over `toml::Value`: what a table reads and every message it fails with
come from the same serde walk as cargo's, `Value<T>` keeps where each value was defined, and a
type cargo reads (cargo-util-schemas' `TomlProfile`) reads here unchanged. `cargo config get`
prints with the `toml_edit` (`=0.25.12`) and `shell-escape` (`=0.1.5`) cargo 1.99 prints with.
