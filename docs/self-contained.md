---
status: plan
summary: blaze self-contained: one curl install that builds any Cargo project with no rustup, toolchain or cargo.
date: 2026-10-25
---
# blaze is self-contained

## The goal

Andrew, 2026-10-25: "ideally installing is a curl command (precompiled) and you will not need
to set a Rust toolchain; it will just work with whatever we have, because it doesn't need to
depend on rustc. Ideally not depend on cargo either! And just match cargo resolution, i.e.
self-contained."

So: `curl -fsSL https://ix.dev/blaze | sh` gives one blaze that builds, tests and
runs any Cargo project on a machine with no rustup, no toolchain pin and no cargo, and that
resolves the project exactly as cargo does. The issue is
[#298](https://github.com/indexable-inc/blaze/issues/298).

What a machine still needs is what any native build needs: a C compiler and linker (`cc` on
Linux, the Xcode command line tools on macOS), which build scripts and the link use. They are
outside the rule of [no-rustc.md](no-rustc.md#what-counts-as-running-rustc).

## Which Rust a project gets

blaze is its own toolchain, `blaze-2026-10-25` ([toolchain.md](toolchain.md)). It compiles one
toolchain of Rust, the dated nightly in `harness/toolchain`
(`nightly-2026-10-25`, `rustc 1.101.0-nightly (12433c307 2026-10-24)`): its standard library,
its layouts and its answers to rustc's questions. It follows the latest nightly, moved by
`harness/toolchain_bump.py`. Which toolchain the project names (`rust-toolchain.toml`,
`rustup override`, `+toolchain`, rustup's default) does not change that. Since
[#296](https://github.com/indexable-inc/blaze/pull/296) a pin to another toolchain no longer
sends the build to cargo: blaze builds with its nightly and says so when the build fails.

A project builds under blaze when its code builds under that nightly. Rust keeps building old
code, so a project pinned to a stable release or an older nightly builds the same, and
`#![feature(...)]` code builds since the toolchain is a nightly
([support.md](support.md#nightly-features)). What a project can need that blaze's Rust lacks,
said plainly when it happens:

- **A feature the nightly removed or changed** (`box` patterns, gone upstream for
  `deref_patterns`): rustc's error, with a note that the project's toolchain is another and
  blaze builds with its nightly.
- **A newer nightly's library or language**: the same error and note. It goes away when blaze
  moves to that nightly.
- **`rust-version` above the nightly's release (1.101)**: cargo refuses such a package, and so
  does blaze, with cargo's message.

`-Z` flags are covered in [support.md](support.md).

## What blaze takes from a Rust toolchain today

Measured on main at 012f0e91e3 (2026-10-25); the sizes are those of `nightly-2026-10-25`'s
components, as `crates/blaze-cargo/src/cargo/toolchain/pins.txt` lists them. Each piece, what uses
it, and the step that ends the need for it ([no-rustc.md](no-rustc.md#the-plan)).

| Piece | Size (xz, Linux x86-64) | What uses it | Removed by |
|---|---|---|---|
| `cargo` | 10.9 MB | the first build (`cargo/state.rs` `Built::by`); `cargo metadata` for every load (rust-analyzer's project model; the client's `--no-deps` reading is blaze-manifest's since 2026-10-25, `cargo/metadata.rs`); `cargo build --unit-graph` (`plan.rs`); rust-analyzer's `cargo check --compile-time-deps`; fetching crates into `~/.cargo/registry` | blaze's resolver and fetcher ([below](#resolution-and-fetching)), then K and M |
| `rustc` (with `librustc_driver`, LLVM) | 84.9 MB, with the next two | the first build of every workspace, proc macros, the build scripts blaze does not compile yet, the functions left to rustc, the rustc base of the first edits | K, J, E, M |
| `rust-lld` (`lib/rustlib/<host>/bin/gcc-ld`) | in `rustc` | blaze's own link on ELF targets (`link.rs`), as rustc links | blaze shipping its own lld, or the system linker |
| `rust-analyzer-proc-macro-srv` (`libexec`) | in `rustc` | expanding proc macros with the libraries rustc built | J1 and J2 |
| `rust-std-<host>` (the sysroot's rlibs) | 31.7 MB | linking what cargo and rustc built; `libstd`'s dynamic library for a proc macro's tests | F and G as the only build |
| `rust-src` | 6.0 MB | not part of the toolchain since 2026-10-28: the standard library's source is blaze's `library/` ([toolchain.md](toolchain.md#the-standard-library-library)), kept in `~/.blaze/library/<digest>` and keyed by its digest, copied from the checkout blaze was built in or downloaded as the release's `library-<digest>.tar.xz` (6.0 MB). Nothing installs or reads `rust-src`: not blaze's toolchain, not the front end (`project-model`'s sysroot reads `BLAZE_LIBRARY` alone, and `RUST_SRC_PATH` not at all) | gone (#402) |
| rustup | | finding the pinned nightly (`cargo/toolchain.rs`, #296), `multirust-channel-manifest.toml` for the release (`answers/own.rs`) | blaze's own toolchain ([below](#blazes-own-toolchain-the-stopgap)) |

On macOS the same pieces are 51.6, 8.9, 30.9 and 6.0 MB (97.4 MB); on Linux AArch64 118.2 MB.
Unpacked they are 710 MB on Linux x86-64 and 548 MB on macOS.
blaze's own archive is 18 to 21 MB.

## blaze's own toolchain, the stopgap

Until K, J and M land, blaze still runs cargo and rustc for the first build and the proc
macros. So that installing needs no rustup now, blaze keeps a toolchain of its own, private
to it, that no project's pin or `PATH` changes:

- **What:** the pinned nightly's `rustc`, `cargo`, `rust-std-<host>` and `rust-std-wasm32-wasip1`
  (the WASI libc a proc macro's module is linked with, `own_macro.rs`) components, as the Rust
  project publishes them on static.rust-lang.org, unchanged. Not `rust-src`: the standard
  library's source is blaze's own (`library/`, `~/.blaze/library/<digest>`). Not `rustfmt`, `clippy`,
  `rust-docs` or `rust-analyzer`.
- **Size:** 152.5 MB to download on Linux x86-64, 136.9 MB on Linux AArch64, 116.0 MB on
  macOS (`pins.txt`), the same bytes rustup's `minimal` profile and the `wasm32-wasip1` target
  download; 6.0 MB less than with `rust-src`, and blaze's library archive (6.0 MB) comes
  beside it.
- **Where:** `~/.blaze/toolchains/nightly-2026-10-25-<host>/` (`BLAZE_HOME` moves it), laid out as rustup
  lays out a toolchain, with its channel manifest, so that blaze's sysroot rules read it
  unchanged.
- **Checked:** the sha256 of each component is compiled into blaze, from the channel
  manifest of the nightly (`crates/blaze-cargo/src/cargo/toolchain/pins.txt`); a download whose
  bytes differ is not used.
- **When:** install.sh fetches it after installing blaze, and blaze fetches it itself on the
  first build that needs it and finds no other copy (a `cargo install` of blaze, a deleted
  directory). A rustup that already has `nightly-2026-10-25` is used as it is, rather than
  downloading the same bytes again.
- **Who sees it:** nobody. cargo, rustc and the daemon blaze starts run from it (`CARGO`,
  `RUSTC`, `RUSTDOC` and `PATH` for the processes it starts); the user's own `cargo` and
  `rustc`, if any, are untouched.

Its removal, piece by piece, as the steps land: `cargo` when the resolver and fetcher below are
the only source of a build's shape and K has no first cargo build; `rustc`, with `rust-lld`
and the proc-macro server, when K, J and M are done and blaze links with a linker it ships or
the system's; `rust-std` when G is the only build. `rust-src` is gone already: the
standard library's source is blaze's `library/`, part of blaze's release. Each removal shrinks the download, and the
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

2026-10-25, the resolver (`crates/blaze-resolve`): the package model (#305), `Cargo.lock`
(#309) and the async `Registry` (#312), landed together in #314; cargo's resolver with its
tests (#331), features and the members `-p` and `--features` choose (#332), the `resolve` of
`cargo metadata` (#333), and a workspace's two resolves with `[patch]` and the lock file's
locks (#339).
`harness/resolve_graph.py` compares the `resolve` blaze-resolve makes of the packages cargo
lists with cargo's, over the corpus of `harness/resolve` (dev-aws-8, at 6e4bbe91): with the
lock file each project ships, 322 of 324 equal; with none, 381 of 385. The rest are those
cargo 1.99.0, the oracle before blaze moved to the nightly (#372), does not resolve there (bun, swc, rust-clippy, embassy). Its registry holds the
versions cargo chose; choosing among all of the index's waits on #321, cargo's SAT-checked
resolver tests on #337, `cargo update` and path overrides on #338.

2026-10-25, the first switch: the daemon's loads read the workspace with `crates/blaze-metadata`
instead of `cargo locate-project` and `cargo metadata` ([resolve.md](resolve.md)): the root,
every locked package fetched by blaze-registry, and the graph of the build the load is for.
`harness/own_metadata.py` on dev-aws-14 over the corpus and the bench projects, each with the
lock file it ships, blaze fetching into a `CARGO_HOME` of its own: `cargo metadata
--filter-platform` 321 of 321 equal as JSON, and the packages, features and edges of `cargo
build --unit-graph` 321 of 321 equal. yew's lock file is one cargo rewrites, which blaze
leaves to cargo (#321); bun, swc and agave cargo 1.99.0 (the oracle then) does not resolve there. With it axum's
whole build builds (5.9 and 6.5 s in two runs, cargo's 6.2 and 6.1 s, on a shared host): its example no longer reaches `mime_guess`, whose
build script its build never ran. Left: the unit graph (#335), choosing versions (#321).

2026-10-25, the programs of a whole build: `blaze build` with `BLAZE_WHOLE_BUILD=1`
takes the programs it links from blaze-metadata's `programs` (the binaries of the build's
members whose `required-features` it has, or the `--bin` or `--example` named), for the
target's cfg as blaze answers `rustc --print cfg` itself, instead of `cargo build
--unit-graph`. Where it cannot, it says why and asks cargo; `BLAZE_OWN_RESOLVE=0` asks cargo
always. `harness/own_metadata.py` compares them with the unit graph's executable roots in its
`build` check: 321 of 321 equal over the corpus (dev-aws-14).

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
   `BLAZE_PROC_MACRO_SERVER`. macroserver.py compares every macro call of the bench apps
   ([Status](#status)).
2. The server ships with blaze (landed 2026-10-25, Linux): built by the release workflow with
   blaze (`--exe`) beside the standard library's unit, in the archive as
   `blaze-proc-macro-srv`, so no machine builds it and the toolchain's
   `libexec/rust-analyzer-proc-macro-srv` is never started for a library blaze linked. That
   is blaze's own executable, which the rule allows to be built with rustc (its serde
   derives); each later release builds its server with the one before.
3. On by default for the daemon's loads, the checker and the build scripts that link a proc
   macro (J0); a proc macro's library kept in the store by its content, as any other crate's
   (landed 2026-10-25).
   rustc then compiles no proc macro and none of `syn`, `quote` and `proc-macro2` for one: most
   of what rustc compiles in the cold build of a crate with derives.

**K, the cold build** ([#91](https://github.com/indexable-inc/blaze/issues/91)). Today, with
#310 (54301ec5's first-build lane), cargo builds the workspace and the daemon takes over after.
K replaces cargo's build in that flow, keeping 54301ec5's lane on the rest of it:

1. `blaze build` and `run` from nothing build the program whole with blaze (step G's
   build, every crate from source over the shipped standard library, linked by blaze), and
   write each executable where cargo would, so `run`, scripts and IDEs find it. The build
   scripts are compiled by blaze (I) and run by blaze's runner ([build scripts](#build-scripts));
   the proc macros are J's. What blaze cannot compile and the program reaches fails the build
   with the reason: there is no build by cargo to go to. While it is shown equal on the bench
   apps (each program's runs printing what cargo's does, as `allblaze.py` checks), it is behind
   a setting, then the default. Behind `BLAZE_WHOLE_BUILD=1` since 2026-10-25, with the build
   scripts and proc macros still the load's ([Status](#status)).
2. The first edits patch blaze's own executable, not rustc's (#11): no rustc base.
3. `blaze test` from nothing the same, with blaze's harness (H).

The bar is step K's: with a warm remote, no slower than cargo's cold build; with an empty
store, measured and published beside cargo's.

**M, the wrapper and the fallback** ([#93](https://github.com/indexable-inc/blaze/issues/93)).
Once K and J are the default: `blaze-rustc`, `blaze-rustc-first`, the stand-ins, the dep-info
runs, `settle`, `by_cargo`, `ToCargo` and `only_cargo` are deleted; the count of
[no-rustc.md](no-rustc.md#how-often) is zero. Done of it (2026-10-27): `blaze enable` and the
wrapper behind plain cargo, `blaze-rustc-first`, `by_cargo`, `ToCargo` and `only_cargo` are
gone, plain cargo is cargo's alone, and `blaze-rustc` is left as a load's wrapper only.
Then blaze's own toolchain loses `rustc` and `rust-std`. The link still needs an lld on ELF
targets (rustc's `rust-lld` today, through `gcc-ld`): blaze ships one in its release, or links
with the system's `cc` and an lld it finds, a choice for that step, measured on the bench apps.

## The whole build in place of `cargo build`: what stands between them (2026-10-25)

Step G's whole build (`blaze <package> --crate <root> --out --exe`, every crate compiled by
blaze over the shipped standard library and linked by blaze) run on bench.py's projects from an
empty store, then again on the store it filled, against cargo's own cold build into a target
directory of its own, on dev-aws-8 (Cranelift, main at 3e03d4298a, the host shared at a load
of 20-56, nothing pinned: the times are rough). rustc and cargo were stand-ins that log each
run, in a copy of 1.99.0, which blaze built with until #372 moved it to `nightly-2026-10-25`.
Each claim: **read** (measured or read in the code), **reported**, **unverified**. These are
the times before #330 and the cold build's later work; the current ones are below the list.

| Project | Whole? | Same output | blaze, empty store | blaze, warm store | cargo cold |
|---|---|---|---|---|---|
| ripgrep | yes, 0 stand-ins | yes | 19.4 s | 7.9 s | 6.0 s |
| fd | yes | yes | 49.8 s | 13.4 s | 28.7 s |
| hyperfine | yes | yes | 22.2 s | 7.7 s | 5.9 s |
| just | yes | yes | 29.9 s | 13.0 s | 8.6 s |
| bat | yes | yes | 32.5 s | 16.7 s | 11.2 s |
| nushell | yes | yes | 95.9 s | 77.1 s | 36.9 s |
| axum (`example-hello-world`) | no: `env!("MIME_TYPES_GENERATED_PATH")` in mime_guess is not set | | fails at 30.7 s | | 9.5 s |
| biome | no: `env!("OUT_DIR")` in htmlize is not set | | fails at 355 s, 38 GB | | 91.0 s |
| cargo, bevy, zed | not here: cargo's own build fails too (openssl, libudev and wayland, x11 not on the host) | | | | |
| helix | not measured: the measuring script failed (a file not found) | | | | |
| bun | not measured: it needs bench.py's `prepare` | | | | |

All **read**. What stands between this and replacing `cargo build`:

- **Build scripts and proc macros are still rustc's.** rust-analyzer's load runs
  `cargo check --compile-time-deps`, and that cargo builds them with rustc: ripgrep 7 build
  scripts and 1 library for them, fd 8 and clap_derive, hyperfine 18 and 3 proc macros,
  nushell 50 and 30 (179 rustc runs, 17 of them questions), against cargo's 551. **read**. The
  build-scripts lane (#304, #306) and J (proc macros) take those. On the warm store the load
  found its target directory built: 1 rustc run, a question. **read**
- **Two crates' build-script output is missing.** axum and biome fail on an `env!` of a
  variable a build script sets. Why the load has no output for them is **unverified**.
- **No stand-ins** were left in any program that linked: every function those programs reach
  blaze compiled. **read**
- **Time.** On an empty store blaze is 1.7-3.7 times cargo's cold build (fd is the exception,
  where cargo's is slow), and 2.6 times on nushell. Part is the standard library: the remote
  `blaze-engine std` makes has only the unit of the do-nothing build script, at no debug info, and a
  program's dev build keys its unit at line tables, so every build compiled the library again
  (about 10 s, 22,920 of 39,232 functions). **read**. `blaze-engine std` now ships the dev program's
  unit too ([#330](https://github.com/indexable-inc/blaze/pull/330)): ripgrep from an empty store
  went from 19.4 s to 9.9 s. **read**. The warm store is not yet as warm as it can be: nushell's second
  build found 88,116 entries that "call what cannot be used" and compiled 95,813 functions
  again (77 s), ripgrep's 6,818 (7.9 s). **read**; why is **unverified**.
- **cargo still runs** 12-14 times a build (`metadata` four times, the unit graph,
  `config get`, `check --compile-time-deps`, `locate-project`, `rustc --print`, `--version`).
  That is M's and the resolver's. **read**
- biome's load expanded 13,548 proc-macro calls in 167 s, 5,478 function-like ones in 136 s,
  and the build held 38 GB. **read**
- `harness/allblaze.py`'s set (five programs, a shared library, a proc-macro crate, built whole,
  over the unit and stored) passes on the same host: each built and linked in 1.6-17.5 s and
  ran as cargo's build does. **read**

2026-10-26, the same comparison (`harness/coldbuild.py bench`, blaze from an empty store on
its nightly, cargo cold into a target directory of its own) on dev-aws-14 while it was quiet:

| Project | blaze, empty store | cargo cold |
|---|---|---|
| ripgrep | 4.8 s | 4.0 s |
| fd | 22.1 s | 16.8 s |
| hyperfine | 7.0 s | 3.8 s |
| just | 11.0 s | 5.5 s |
| bat | 12.6 s | 7.0 s |
| axum (`example-hello-world`) | 6.1 s | 6.1 s |
| nushell | 49.3 s | 23.8 s |

All **read**. axum builds whole now (its build scripts' variables are set); biome does not
yet: 17 functions the frontend turns down, and its proc macros take 149 s to expand. fd's load
waits 16.7 s on jemalloc's build script. What is left of nushell's is in
[no-rustc.md](no-rustc.md) (K). These were taken on a host shared at other times; a table on a
host of its own replaces them.

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

2026-10-25, the registry: `crates/blaze-registry` reads the sparse index and its cache as cargo
1.99 does, fetches `.crate` files into the store by their sha256 and unpacks them where cargo
does (#307, landed with #314), and replaces sources, reads vendored directories and checks
out git dependencies with the `git` command at cargo's paths. `harness/fetchdiff.py` on
dev-aws-8, against `cargo fetch` and `cargo vendor` over 314 lock files (the bench projects
at their pins but bun, the 300 most downloaded crates, git dependencies of every kind of
reference, a second sparse registry): 3684/3684 unpacked packages and 38/38 git checkouts
equal, file by file, and every directory name; crates.io replaced by a mirror 42/42; ripgrep's
and zed's vendored packages 51/51 and 1533/1533. Not yet: blaze-resolve's `Registry` over it (#321),
registries that need credentials (#315), git indexes (#316), cargo's last-use record (#317),
a path package's file list (#318).

2026-10-25, J (proc macros): with `BLAZE_PROC_MACRO_SERVER`, a load into blaze's own
target directory has blaze link the proc macros and its server expand them; over fd, bat,
hyperfine, just, nushell and helix, 3,465 calls expanded the same as the toolchain's server
with rustc's libraries, 12 not compared (`pest_consume_macros`,
[#327](https://github.com/indexable-inc/blaze/issues/327)). The Linux archive has the server
beside `blaze` (built by it in 30 s on dev-aws-8), and `BLAZE_PROC_MACRO_SERVER=1` takes it
from there: a load of fd starts no toolchain server. It is the default: the daemon's loads,
the checker's and build scripts' expand with blaze's libraries, made from each crate's record
and kept in the store; built again over that store, the six apps link none again and expand
the same 3,465 calls.

2026-10-25, no rustup: `blaze` reads the workspace with blaze-manifest where it ran
`cargo metadata --no-deps` (#319; 385 of 385 workspaces equal, [resolve.md](resolve.md)), and
install.sh installs blaze on a machine with no Rust: blaze downloads its own 1.99.0 (#322 and
#340, landed together in #340). Measured on dev-aws-8 in a Debian 13 container with curl and
gcc and no rustup, cargo or rustc, the archive served from the host and Rust from
static.rust-lang.org over AWS's network: install.sh 5.5 s, the first `blaze run` of a new
project 3.8 s (the daemon's load; `Finished ... [blaze]`), 9.2 s from the first `curl` to
`Hello, world!`. On disk: 88 MB in `~/.cargo/bin`, 672 MB in `~/.blaze/toolchains`, 17 MB in
`~/.cache`, no `~/.rustup`. The same install through rustup before (Debian 12, 2026-10-25):
10.7 s, 91 MB and 672 MB in `~/.rustup`. All **read**. The Linux release needs glibc 2.39, so
Debian 12 cannot run it ([#351](https://github.com/indexable-inc/blaze/issues/351)).

2026-10-25, K's first part: with `BLAZE_WHOLE_BUILD=1`, `blaze build` and `run` from
nothing are the whole build above, no `cargo build` ([cargo.md](cargo.md#what-a-build-does));
`harness/coldbuild.py` holds its fixtures to cargo's, and with `bench` does bench.py's projects:
`blaze build` from nothing against `cargo build`, each program's output the same, on
dev-aws-8 (loaded): ripgrep 22.2 s against 6.0 s, fd 48.4 against 28.6, hyperfine 24.5 against
5.8, just 29.2 against 8.3, bat 33.2 against 10.5, nushell 102.4 against 36.2. **read**. Off by
default until axum's and biome's builds are whole and the cold times are near cargo's.

Git-index registry support (#316) reads `config.json` and package entries directly from
the fetched Git tree and shares Cargo's index caches. `harness/gitindex.py` checks source
trees, archive bytes, paths and index-cache bytes against the pinned Cargo, then swaps
the tools' cache homes offline. `blaze-registry` tests also cover updates, removed entries,
old commit-versioned caches and a failed fetch that must not answer from an older tree.
