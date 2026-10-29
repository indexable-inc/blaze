---
status: current
summary: Reading Cargo.toml and .cargo/config.toml as cargo does (blaze-manifest), compared with cargo by harness/resolve.py.
---
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

`blaze` reads the workspace with it (`crates/blaze-cargo/src/cargo/metadata.rs`): every
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
missing or that the manifests no longer agree with is written as cargo writes it (2026-10-26),
a registry package it does not pin chosen from the registry's index as cargo chooses it
(blaze-registry's `IndexRegistry`, #321). What it does not read yet goes to cargo's `cargo
metadata` with the reason printed: a git or vendored package the lock file does not pin.
`[replace]` is read as cargo 1.99's `toml::replace` reads it (2026-10-28).
`BLAZE_OWN_RESOLVE=0` sends every load to cargo.

The unit graph is blaze's own (`crates/blaze-units`, 2026-10-27): a port of cargo's unit
generator (`ops/cargo_compile/unit_generator.rs`), `compiler/unit_dependencies.rs` and the
sharing pass of `rebuild_unit_graph_shared`, made from blaze-metadata's resolve, its feature
resolution and blaze-manifest's profiles, and printed in cargo's `--unit-graph` schema
(version 1). It reads no file and runs no process, and builds for wasm32-wasip1 (issue #458;
wasm32-unknown-unknown waits on blaze-resolve's `url` file paths). `blaze_metadata::units`
does the reading and the resolve around it, and blaze's `plan.rs` plans every load from it:
no `cargo build --unit-graph` is run. A build it does not port (cargo's `-Z` flags,
`-Zbuild-std`, artifact dependencies, multiple build scripts, per-package targets) is an error
naming it, not a cargo run.

Not yet: the unstable profile keys (`trim-paths`, `rustflags`, `codegen-backend`, which need nightly
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
the unit graph: the `units` part below checks blaze's own graph. The walk reproduces cargo's order where
it decides a profile: the roots are interned before their dependencies and all of a unit's
dependencies before any is walked, a unit equal to one interned earlier is that one (profiles
compare without whether their debuginfo is deferred), and the first way a unit is reached in
the sharing pass decides whether the debuginfo of a host unit is lowered. Package overrides are
checked against the packages of `Cargo.lock`, as cargo checks them against the workspace's
resolve.

The `sysroot` part is the standard library's workspace, blaze's `library/` (`BLAZE_LIBRARY`,
[toolchain.md](toolchain.md#the-standard-library-library)), read in its directory (whose `.cargo/config.toml` replaces
crates.io with `library/vendor`) as rust-analyzer's `Sysroot::load_workspace` asks for it:
`--features sysroot/backtrace`, cargo told it is a nightly, `--no-deps` and full, with no
`--filter-platform` and with each target of `harness/answers.py`. cargo reads the full metadata
against a copy of the lock file (`CARGO_RESOLVER_LOCKFILE_PATH`); blaze-metadata's
`metadata_locked` never writes it and refuses a lock file the manifests no longer agree with, as
`--locked` does. blaze answers rust-analyzer with these (`blaze/src/cargo/metadata.rs`
`library`, through `OwnMetadata.library`), so loading a workspace runs no cargo for the sysroot.

On VM nc-sysroot (Linux arm64), 2026-10-27, with the pinned nightly: 26 of 26 equal (no platform and
the 12 targets, each `--no-deps` and full; 15 packages without dependencies, 30 to 50 with).

The `toolchain` part asks what a workspace load asked cargo of the toolchain and the
configuration, of cargo and of `blaze-engine cargo-answer`, which answers it with no cargo run
(`crates/blaze-cargo/src/answers/cargo.rs` over blaze-manifest): `cargo --version`, `cargo config
get` as TOML with `--show-origin` and as JSON, `cargo rustc --print cfg -- -O` and `--print
target-spec-json` for the host and five other targets (and for `build.target`), comparing exit
code and stdout as text, and, in a tree with a package, the flags cargo hands rustc for each
target (seen by a `RUSTC_WRAPPER` that prints them) against those `plan.rs` reads
(`cargo-answer --rustflags`). The trees: `harness/resolve/toolchain-fixtures` (`build.target`,
`[target.'cfg(..)']` tables, `RUSTFLAGS` per target and a forced `[env] RUSTC_BOOTSTRAP=-1`,
which cargo hands the rustc it asks, so the stable channel's cfgs print), the config fixtures,
`harness/norustc/fixture` and the bench projects.

On the ix VM nc-toolq (Linux AArch64), 2026-10-27: 35 of 35 equal, 575 questions, 219 of them
refused by both (the config fixtures have no package, and some have broken files).

The `units` part runs `cargo <run> --unit-graph -Zunstable-options` (with `RUSTC_BOOTSTRAP=1`)
and blaze-metadata's `units` example (blaze-units' graph) for `build`, `build --all-targets`,
`build --release`, `build --profile test --all-targets`, `build --profile bench --all-targets`
(what blaze's `plan.rs` asks for its builds, tests and benchmarks) and `test --no-run`, on
harness/norustc/fixture and the bench projects, and compares the whole JSON: every unit in
order, with its package, target, profile, platform, mode, features and dependencies, and the
roots. A build both refuse must be refused with cargo's error text. One order is not compared:
cargo sorts units that differ only in what they depend on by `dep_hash`, a hash of the
addresses of its interned dependencies (`std::ptr::hash` in `compiler/unit.rs`), so on bat
two runs of the same cargo print its two `regex` units in either order. The harness matches
such units by a key of everything each one reaches and puts blaze's in cargo's order before
comparing; blaze-units orders them by their dependencies.

On an arm64 ix VM, 2026-10-27: 13 of 14 equal, 78 runs and 34,124 units, none different
(zed 10,155, biome 6,016, bevy 4,625, nushell 3,649, cargo 2,322, helix 2,043, bat 1,586,
axum 1,215, just 837, hyperfine 800, fd 499, ripgrep 332, the fixture 45). bun is refused by
both, its vendored sources being made by its own build (`harness/bunprep.py`): cargo names the
missing vendored lolhtml first, blaze-metadata the missing vendored rust-argon2, as it reads
path packages in another order than cargo's resolver asks for them.

The `ctdeps` part (named only: it builds what it compares) holds blaze's own build of a load's
build scripts and proc macros (`crates/blaze-cargo/src/cargo/compile_time.rs`, in place of
rust-analyzer's `cargo check --compile-time-deps`) to cargo's. For harness/norustc/fixture and
each bench project, copied into a directory of its own, and for a build and a test load (the
test adds `--cfg-test`, cargo's `--all-targets`, and the project's `test=[...]` flags of
`bench.py`), it runs `cargo check --compile-time-deps --message-format=json` as rust-analyzer's
`build_command` does and reads the messages as `WorkspaceBuildScripts` does, then `blaze
compile-time-deps` with the same flags, which loads the workspace as a build's daemon does
(`--own-target`) and prints what the load read. Per package it compares the script's cfgs and
variables (`OUT_DIR` and other paths replaced), every file of `OUT_DIR` by sha256 (its own path
replaced in the bytes), and which packages have a proc macro built; a difference fails the part.
cargo's target directory is named so that its `OUT_DIR`s are as long as blaze's, which keeps
the offsets in a C library the same. An object or archive is hashed without its debug info,
whose string table gcc orders by the strings' hashes, so another `OUT_DIR` reorders it; a
configure log's temporary file names (`ccXXXXXX.o`) and `config.status`'s split strings are
read as one; a probe's output (autocfg's `autocfg_<random>_N`, `probeN.ll` before autocfg 1.2)
only has to be there, as blaze writes it empty. When both fail, they count as the same
failure when blaze names the package cargo named, and the result says why when each stopped at
something the machine or checkout lacks (a program a script spawns, the linker, a path package).

On main 8470a70e6a (#658, arm64 ix VM, 2026-10-27), 10 of 14 equal: the fixture, ripgrep,
cargo, bat, hyperfine, just, axum, nushell, helix and bevy. fd differs only in jemalloc's
`config.log` and `config.status`, which record `SHELL`: the build-script sandbox gives a script
only the variables `script_sandbox.rs` `given` lets through, so `configure` records its own
`/bin/sh` where cargo's run had the shell's; with `BLAZE_SANDBOX=off` its 9 packages are equal.
biome stops at a syn-1 proc macro blaze cannot compile (`darling_macro`, #327). zed fails for
both (cargo at gcc's missing `-fuse-ld=lld`, blaze at `cmake`), and bun for both at a path
package its own build makes (`harness/bunprep.py`).

## Decisions

**cargo's schema crate, pinned.** `cargo-util-schemas = "=0.14.3"` is the version cargo 1.99.0
is built with: its published source is the same, file for file, as
[crates/cargo-util-schemas](https://github.com/rust-lang/cargo/tree/1.99.0/crates/cargo-util-schemas)
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

Git-index registries are read from their fetched tree without a checkout (#316). Cargo and
blaze share the blob-versioned index cache; old commit-versioned caches are accepted too.
`python3 harness/fetchdiff.py --git-index-only` compares a local two-package registry with
Cargo and then tests offline cache exchange. `harness/gitindex.py` runs the same fixture
through the `blaze fetch` CLI. Crates.io with `registries.crates-io.protocol = "git"` uses
the same index implementation; `fetchdiff.py --git-index` selects it for the full corpus.
Fetching the whole public index is not part of the local fixture.
