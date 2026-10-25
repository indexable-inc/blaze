# blaze is self-contained

## The goal

Andrew, 2026-10-25: "ideally installing is a curl command (precompiled) and you will not need
to set a Rust toolchain; it will just work with whatever we have, because it doesn't need to
depend on rustc. Ideally not depend on cargo either! And just match cargo resolution, i.e.
self-contained."

So: `curl -fsSL https://ix.dev/blaze/install.sh | sh` gives one blaze that builds, tests and
runs any Cargo project on a machine with no rustup, no toolchain pin and no cargo, and that
resolves the project exactly as cargo does. The issue is
[#298](https://github.com/indexable-inc/blaze/issues/298).

What a machine still needs is what any native build needs: a C compiler and linker (`cc` on
Linux, the Xcode command line tools on macOS), which build scripts and the link use. They are
outside the rule of [no-rustc.md](no-rustc.md#what-counts-as-running-rustc).

## Which Rust a project gets

blaze compiles one release of Rust, the one in `harness/toolchain` (1.99.0): its standard
library, its layouts and its answers to rustc's questions. Which toolchain the project names
(`rust-toolchain.toml`, `rustup override`, `+toolchain`, rustup's default) does not change
that. Since [#296](https://github.com/indexable-inc/blaze/pull/296) a pin to another release
no longer sends the build to cargo: blaze builds with 1.99.0 and says so when the build fails.

A project builds under blaze when its code builds under Rust 1.99.0. Rust keeps building old
code, so a project pinned to an older stable release builds the same. What a project can need
that blaze's Rust lacks, said plainly when it happens:

- **A nightly feature** (`#![feature(...)]`, `-Z` flags): 1.99.0 is a stable release and
  refuses them, as rustc 1.99.0 does. blaze's error says the project's toolchain is a nightly
  and blaze builds with 1.99.0.
- **A newer release's library or language** (an API stabilized after 1.99.0): the error is
  rustc's, with the same note. It goes away when blaze moves to that release.
- **`rust-version` above 1.99**: cargo refuses such a package, and so does blaze, with
  cargo's message.

## What blaze takes from a Rust toolchain today

Measured on main at 012f0e91e3 (2026-10-25). Each piece, what uses it, and the step that ends
the need for it ([no-rustc.md](no-rustc.md#the-plan)).

| Piece | Size (xz, Linux x86-64) | What uses it | Removed by |
|---|---|---|---|
| `cargo` | 11.0 MB | the first build (`cargo/state.rs` `Built::by`); `cargo metadata` for every load (rust-analyzer's project model; the client's `--no-deps` reading is blaze-manifest's since 2026-10-25, `cargo/metadata.rs`); `cargo build --unit-graph` (`plan.rs`); rust-analyzer's `cargo check --compile-time-deps`; fetching crates into `~/.cargo/registry` | blaze's resolver and fetcher ([below](#resolution-and-fetching)), then K and M |
| `rustc` (with `librustc_driver`, LLVM) | 84.0 MB, with the next two | the first build of every workspace, proc macros, the build scripts blaze does not compile yet, the functions left to rustc, the rustc base of the first edits | K, J, E, M |
| `rust-lld` (`lib/rustlib/<host>/bin/gcc-ld`) | in `rustc` | blaze's own link on ELF targets (`link.rs`), as rustc links | blaze shipping its own lld, or the system linker |
| `rust-analyzer-proc-macro-srv` (`libexec`) | in `rustc` | expanding proc macros with the libraries rustc built | J1 and J2 |
| `rust-std-<host>` (the sysroot's rlibs) | 31.6 MB | linking what cargo and rustc built; `libstd`'s dynamic library for a proc macro's tests | F and G as the only build |
| `rust-src` | 5.9 MB | the standard library's source, which blaze compiles (and which the shipped std unit is keyed by) | stays: it is source. Later shipped with blaze |
| rustup | | finding 1.99.0 and its `rust-src` (`cargo/toolchain.rs`, #296), `multirust-channel-manifest.toml` for the release and commit (`answers/own.rs`, `main.rs`) | blaze's own toolchain ([below](#blazes-own-toolchain-the-stopgap)) |

On macOS the same pieces are 50.3, 9.0, 30.7 and 5.9 MB (95.8 MB); on Linux AArch64 117.3 MB.
blaze's own archive is 18 to 21 MB.

## blaze's own toolchain, the stopgap

Until K, J and M land, blaze still runs cargo and rustc for the first build and the proc
macros. So that installing needs no rustup now, blaze keeps a toolchain of its own, private
to it, that no project's pin or `PATH` changes:

- **What:** Rust 1.99.0's `rustc`, `cargo`, `rust-std-<host>` and `rust-src` components, as
  the Rust project publishes them on static.rust-lang.org, unchanged. Not `rustfmt`, `clippy`,
  `rust-docs` or `rust-analyzer`.
- **Size:** 132.5 MB to download on Linux x86-64, 117.3 MB on Linux AArch64, 95.8 MB on
  macOS, the same bytes rustup's `minimal` profile and `rust-src` download today.
- **Where:** `~/.blaze/toolchains/1.99.0-<host>/` (`BLAZE_HOME` moves it), laid out as rustup
  lays out a toolchain, with its channel manifest, so that blaze's sysroot rules read it
  unchanged.
- **Checked:** the sha256 of each component is compiled into blaze, from the channel
  manifest of the release; a download whose bytes differ is not used.
- **When:** install.sh fetches it after installing blaze, and blaze fetches it itself on the
  first build that needs it and finds no other copy (a `cargo install` of blaze, a deleted
  directory). A rustup that already has 1.99.0 with `rust-src` is used as it is, rather than
  downloading the same bytes again.
- **Who sees it:** nobody. cargo, rustc and the daemon blaze starts run from it (`CARGO`,
  `RUSTC`, `RUSTDOC` and `PATH` for the processes it starts); the user's own `cargo` and
  `rustc`, if any, are untouched.

Its removal, piece by piece, as the steps land: `cargo` when the resolver and fetcher below are
the only source of a build's shape and K has no first cargo build; `rustc`, with `rust-lld`
and the proc-macro server, when K, J and M are done and blaze links with a linker it ships or
the system's; `rust-std` when G is the only build; `rust-src` becomes part of blaze's release,
pruned to what the standard library's build reads. Each removal shrinks the download, and the
last leaves the 18 to 21 MB of blaze alone.

## Resolution and fetching

blaze reads the project as cargo does, in crates of its own, with cargo as the oracle of a
differential harness and never as a step of a build.

- **`crates/blaze-manifest`**: `Cargo.toml` as cargo reads it: packages and virtual
  workspaces, members, `exclude` and `default-members`, `workspace = true` inheritance of
  package fields, dependencies and lints, target discovery (`src/main.rs`, `src/bin`,
  `examples`, `tests`, `benches`, `build.rs`, `autobins` and the rest), `[[bin]]` and friends,
  profiles with their defaults and overrides, `[patch]` and `[replace]`, path, git and
  registry dependencies, renamed, target-specific, build and dev dependencies. And
  `.cargo/config.toml` in every directory cargo reads it from, with `--config` and `CARGO_*`
  variables: registries, source replacement and vendoring, `[env]`, `target-dir`, `rustflags`,
  `[target.<cfg>]`, `net.offline`.
- **`crates/blaze-registry`**: the crates.io sparse index (and other sparse registries, and
  git indexes), with cargo's cache rules; `.crate` files downloaded and checked against the
  index's or the lock file's sha256, kept in blaze's content-addressed store by that hash, and
  unpacked where cargo unpacks them (`$CARGO_HOME/registry/src/<registry>-<hash>/<name>-<version>`),
  so that paths in panics and `file!()` are cargo's and cargo and blaze share them; git
  dependencies checked out at cargo's paths; vendored directories, local registries and
  offline mode.
- **`crates/blaze-resolve`**: the resolver. `Cargo.lock` (versions 1 to 4) honoured exactly
  when it is there and up to date, and written as cargo writes it; otherwise versions chosen
  as cargo chooses them (newest compatible, the lock's choices kept, one version per
  semver-compatible range and `links`, prereleases, `[patch]`, resolver 3's `rust-version`
  preference). Features unified as resolver 1 and 2 (and 3) do: per unit, host and target
  apart, dev and build dependencies apart, `default-features`, `dep:` and `dep?/feature`,
  optional dependencies.
- **What it produces** is what blaze reads from cargo today, the same bytes after
  normalizing paths: `cargo metadata --format-version 1` (packages, versions, sources,
  targets, the resolve graph with features) and `cargo build --unit-graph` (units, with the
  features, profile, target and mode of each). The flip replaces cargo's output with these at
  the places that read it, one at a time, behind the existing path until the harness holds
  them equal.

The differential harness (`harness/resolve.py`) runs blaze and cargo on a corpus, the bench
projects at their pinned revisions and a few hundred popular crates and workspaces of
crates.io, with and without a lock file and over feature and target selections, and counts
the projects whose normalized output is the same. It runs where cargo is installed, as the
oracle only.

## Build scripts

blaze compiles build scripts (step I) and keeps their runs in the store; cargo still runs
them. Running them itself means setting what cargo sets for a script (`OUT_DIR`, `TARGET`,
`HOST`, `NUM_JOBS`, `OPT_LEVEL`, `DEBUG`, `PROFILE`, `CARGO_CFG_*`, `CARGO_FEATURE_*`,
`CARGO_MANIFEST_*`, `CARGO_PKG_*`, `CARGO_ENCODED_RUSTFLAGS`, `RUSTC` naming blaze, the
`DEP_<links>_<key>` of the packages it depends on, the jobserver) and applying what it prints,
in both the `cargo:` and `cargo::` forms, as cargo does: `rustc-cfg`, `rustc-check-cfg`,
`rustc-env`, `rustc-link-lib`, `rustc-link-search`, `rustc-link-arg` and its per-target forms,
`rustc-cdylib-link-arg`, `rerun-if-changed`, `rerun-if-env-changed`, `warning`, `error` and
`metadata`. Scripts run in the sandbox of [#147](https://github.com/indexable-inc/blaze/issues/147).
A harness runs a corpus of scripts under cargo and under blaze and compares the environment
each saw and what each produced.

## J, K and M

These three no-rustc steps ([no-rustc.md](no-rustc.md#the-plan)) are what remove most of
blaze's own toolchain: rustc, with its `rust-lld` and proc-macro server, and the sysroot's
rlibs are 116 of its 132 MB on Linux x86-64, and neither can go while any build blaze runs
still has rustc compile something. No one step frees either alone: K's cold build needs J's
proc macros, and M deletes what K and J leave unused. So J goes first, K right behind it, M
last; cargo (11 MB) goes with the switches of [resolution](#resolution-and-fetching) and K.

**J, proc macros** ([#90](https://github.com/indexable-inc/blaze/issues/90)). What is there:
blaze's proc-macro server (`vendor/proc-macro-srv`, the release's own server with a loader that
reads blaze's list of macros), proc-macro crates linked by blaze as shared libraries with that
list (`macro_list.rs`, `--exe --shared`), and `harness/macroserver.py` holding the expansions of
serde_derive, thiserror-impl, tokio-macros and clap_derive equal to the toolchain's. In order:

1. J1's wiring (landed 2026-10-25): when rust-analyzer loads a workspace, each proc macro
   cargo asks for is linked by blaze, and blaze's server expands with it, behind
   `RA_JIT_PROC_MACRO_SERVER`. macroserver.py compares every macro call of the bench apps
   ([Status](#status)).
2. The server ships with blaze (landed 2026-10-25, Linux): built by the release workflow with
   blaze (`--exe`) beside the standard library's unit, in the archive as
   `blaze-proc-macro-srv`, so no machine builds it and the toolchain's
   `libexec/rust-analyzer-proc-macro-srv` is never started for a library blaze linked. That
   is blaze's own executable, which the rule allows to be built with rustc (its serde
   derives); each later release builds its server with the one before.
3. On by default for the daemon's loads, the checker and the build scripts that link a proc
   macro (J0); a proc macro's library kept in the store by its content, as any other crate's.
   rustc then compiles no proc macro and none of `syn`, `quote` and `proc-macro2` for one: most
   of what rustc compiles in the cold build of a crate with derives.

**K, the cold build** ([#91](https://github.com/indexable-inc/blaze/issues/91)). Today, with
#310 (54301ec5's first-build lane), cargo builds the workspace and the daemon takes over after.
K replaces cargo's build in that flow, keeping 54301ec5's lane on the rest of it:

1. `cargo blaze build` and `run` from nothing build the program whole with blaze (step G's
   build, every crate from source over the shipped standard library, linked by blaze), and
   write each executable where cargo would, so `run`, scripts and IDEs find it. The build
   scripts are compiled by blaze (I) and run by blaze's runner ([build scripts](#build-scripts));
   the proc macros are J's. What blaze cannot compile and the program reaches fails the build
   with the reason: there is no build by cargo to go to. While it is shown equal on the bench
   apps (each program's runs printing what cargo's does, as `allrajit.py` checks), it is behind
   a setting, then the default.
2. The first edits patch blaze's own executable, not rustc's (#11): no rustc base.
3. `cargo blaze test` from nothing the same, with blaze's harness (H).

The bar is step K's: with a warm remote, no slower than cargo's cold build; with an empty
store, measured and published beside cargo's.

**M, the wrapper and the fallback** ([#93](https://github.com/indexable-inc/blaze/issues/93)).
Once K and J are the default: `ra-jit-rustc`, `ra-jit-rustc-first`, the stand-ins, the dep-info
runs, `settle`, `by_cargo`, `ToCargo` and `only_cargo` are deleted; plain cargo after `blaze
enable` names blaze as `build.rustc`; the count of [no-rustc.md](no-rustc.md#how-often) is zero.
Then blaze's own toolchain loses `rustc` and `rust-std`. The link still needs an lld on ELF
targets (rustc's `rust-lld` today, through `gcc-ld`): blaze ships one in its release, or links
with the system's `cc` and an lld it finds, a choice for that step, measured on the bench apps.

## Order

1. #296: the project's pin no longer sends builds to cargo (#269).
2. blaze's own toolchain; install.sh and the README drop rustup.
3. The manifest reader, the registry and the resolver land as crates of their own, with the
   harness and its counts, used by nothing.
4. Build scripts run by blaze.
5. J, then K's first part, then M ([above](#j-k-and-m)).
6. The flips: blaze's metadata in place of `cargo metadata` for loads, its units in place of
   `--unit-graph`, its fetching in place of cargo's; then with K and M, no cargo at all. Each
   piece of the stopgap is removed as above.

## Status

2026-10-25: this plan. Lanes: the toolchain (2), the manifest and the harness, the registry,
the resolver, build scripts. Counts and pull requests are added here as they land.

2026-10-25, J (proc macros): with `RA_JIT_PROC_MACRO_SERVER`, a load into ra-jit's own
target directory has ra-jit link the proc macros and its server expand them; over fd, bat,
hyperfine, just, nushell and helix, 3,465 calls expanded the same as the toolchain's server
with rustc's libraries, 12 not compared (`pest_consume_macros`,
[#327](https://github.com/indexable-inc/blaze/issues/327)). The Linux archive has the server
beside `ra-jit` (built by it in 30 s on dev-aws-8), and `RA_JIT_PROC_MACRO_SERVER=1` takes it
from there: a load of fd starts no toolchain server.
