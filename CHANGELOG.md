# Changelog

All notable changes to ra-jit are written here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions after 0.1.0 are dated,
`0.YYYYMMDD.N` with N counting the releases of that day from 0, and the release train writes
each one's section: what `[Unreleased]` had, and the title of each change merged since
(docs/harness.md, "CI").

## [Unreleased]

### Added

- blaze is its own toolchain, named `blaze-<date>` (`blaze-2026-10-25`, today the Rust nightly
  of that date): `blaze toolchain` names it and `blaze toolchain --path` prints its directory,
  and a project that pins another toolchain is told so once, as a note, while blaze builds
  with its own (docs/toolchain.md; #402, #403 and #404 for building, hosting and bumping it ourselves).
- `cargo blaze doc` (and `blaze doc`) writes the HTML documentation of the packages and their
  dependencies from rust-analyzer's HIR (`crates/blaze-doc`), with no rustdoc: a page for
  every item with linked signatures, fields, variants, inherent, trait, blanket and auto
  impls, implementors and the members an impl leaves as the trait provides them; docs as
  rustdoc's Markdown with intra-doc links resolved (an unresolved one warns as rustdoc's
  does); source pages, an "All items" page, a search box (`s` or `/`) over every crate
  documented, light and dark themes. cargo's flags (`-p`, `--workspace`, `--no-deps`,
  `--document-private-items`, `--open`, features, `--target`, `--lib`, `--bin`, ...); the
  workspace loads once and the pages are written in parallel. `harness/docdiff.py` compares
  it with the pinned nightly's `cargo doc`: on fd and its dependencies blaze writes 100% of
  rustdoc's pages, 99.9% of the members and impls on them and 99.4% of the resolved links
  ([docs/doc.md](docs/doc.md)).

- `nonstandard_macro_braces` no longer lints a call inside another macro's arguments
  (`vec!(..)` in `assert!(..)`), which stock clippy leaves alone; `new_without_default`
  takes a type to be exported only when another crate can name it; the format lints read
  macros marked `#[clippy::format_args]`, as `log`'s are. helix's 20 diagnostics of the
  ported lints are now all written as stock clippy writes them, with none extra.
- `cargo blaze clippy` (and `blaze clippy`) lints with blaze's own lints, `crates/blaze-lints`:
  24 of clippy's ported onto rust-analyzer's analysis and run by the daemon, with no cargo,
  rustc or clippy-driver started. They are written as `cargo clippy` writes them (rustc's
  rendering, cargo's summary line and `--fix` hint, `--message-format=json`, status 101 for an
  error), at the levels `#[allow]`, `#[expect]`, `[lints.clippy]`, `clippy.toml` and `-- -D`
  give. On ripgrep and fd every diagnostic stock clippy writes of those lints, 567, is written
  the same, held so by `harness/clippydiff.py`; helix and biome lint nothing yet, as
  `cargo blaze check` of them fails. docs/cargo.md has the numbers.
- `cargo blaze fmt` (and `blaze fmt`) is blaze's own: blaze-fmt run in process, with
  cargo-fmt's options over the workspace's targets, or rustfmt's over the `.rs` files named
  (`blaze fmt src/main.rs`). No rustfmt or cargo-fmt is started.
- `crates/blaze-fmt`: blaze's own formatter, rustfmt and cargo-fmt at rust-lang/rust 12433c307
  ported onto a syntax tree of blaze's own, which rust-analyzer's parser fills with the spans
  rustc would give each node. It runs in process and starts no rustfmt; what it cannot
  lower yet (nightly syntax rust-analyzer does not parse, a `macro` with parameters, the
  `ignore` option) is refused with the reason, never formatted differently.
  `harness/fmtdiff.py` holds it to stock rustfmt byte for byte: ripgrep, nushell, fd, helix
  and biome with their own rustfmt.toml (6068 of 6068 files, `--check` exit codes and diffs
  too), the same with `max_width = 60` (6068 of 6068), and rustfmt's `tests/source` (773 of
  793; the rest are those refusals).
- `cargo blaze search`, `owner`, `yank`, `login` and `logout` are blaze's own: blaze-registry
  has a client of a registry's web API (crates-io 0.41's), with cargo's choice of registry, its
  token and `credentials.toml` as cargo writes it, and its errors (`harness/own_registry.py`,
  73 of 73 cases against a mock registry, requests and credentials files too). Credential
  providers other than `cargo:token` are refused.
- `cargo blaze vendor` is blaze's own for registry packages: each unpacked from its `.crate`
  with its `.cargo-checksum.json`, and the `[source]` configuration printed
  (`harness/own_vendor.py`, 18 of 18 cases, every file by its sha256). A git package is
  refused with the reason.
- `cargo blaze remove` is blaze's own: the dependencies out of the manifest, layout and
  comments kept, what named them cleaned up, and the lock file resolved again with every
  version it keeps (`harness/own_remove.py`, 43 of 43 cases, manifests and lock files byte
  for byte).
- `cargo blaze config get` is blaze's own: the configuration cargo reads, in TOML or JSON,
  merged or file by file, with where each value is from (`harness/own_config.py`, 41 of 41
  cases).
- `cargo blaze report future-incompatibilities` is blaze's own: the report a build left, as
  cargo prints it (`harness/own_report.py`, 23 of 23 cases). Every own command colors its
  lines as `--color` or `CARGO_TERM_COLOR` says.
- `cargo blaze uninstall` and `cargo blaze read-manifest` are blaze's own. `uninstall` is cargo
  1.99's, with its install tracker: `.crates.toml` and `.crates2.json` are written byte for
  byte as cargo writes them (`harness/own_uninstall.py`, 18 of 18 cases equal to cargo's).
  `read-manifest` is the package's entry of `metadata --no-deps` (`harness/own_commands.py`,
  now 130 of 130 over ripgrep, fd, helix, nushell and biome).
- `enable` makes plain `cargo fmt` and `cargo clippy` blaze's, for each of the two that is
  blaze's own: links named `cargo-fmt` and `cargo-clippy` in `~/.cargo/bin`, which cargo looks
  in before `PATH`, to `cargo-blaze`. What they replace, rustup's proxies, is kept and
  `disable` puts it back; `status` says whose each command is. Not an `[alias]`, which cargo
  warns of on every run (rust-lang/cargo#10049).
- `cargo blaze tree`, `pkgid`, `fetch`, `clean`, `new` and `init` are blaze's own, with no
  cargo: `tree` from the new `crates/blaze-tree` (cargo 1.101's, on blaze-resolve), `new` and
  `init` from the new `crates/blaze-new`. `harness/own_tree.py` (130 of 130 equal over
  ripgrep, fd, helix, nushell and biome), `harness/own_commands.py` (120 of 120, with
  `metadata`, `locate-project` and `verify-project`) and `harness/own_new.py` (50 of 50) hold
  them equal to cargo's. `cargo blaze metadata` has cargo 1.101's `min-opt-level` in a
  package's `hints`, so nushell's and biome's full metadata are equal too.

- `RA_JIT_WHOLE_BUILD=1`: the whole build runs in the daemon. Its worker loads the workspace,
  builds and links each program, and answers once they are linked; the next build asks the
  same worker, which builds the edits since on what it has loaded and links again, with no
  load and no `cargo build`. A whole build starts no spare worker, and its objects keep each
  function in a section of its own, so the link leaves out what the program never reaches.
- A whole build's edits are spliced into the executable the daemon linked, as the edits of a
  build over rustc's are, instead of compiling and linking the whole program again.
- A whole build plans its programs with blaze's own resolution (blaze-metadata's `programs`)
  rather than `cargo build --unit-graph`: the members' binaries with their
  `required-features`, or the `--bin` or `--example` named, held equal to the unit graph's by
  `harness/own_metadata.py`.
- The daemon's loads read the workspace with blaze's own resolution (`crates/blaze-metadata`),
  not `cargo locate-project` and `cargo metadata`: every locked package fetched by
  blaze-registry, and the graph of the build the load is for, with the features `cargo build`
  of its selection would have, not those of every member unified. A load of axum's example
  no longer reaches crates whose build scripts its build never ran (`env!` of `mime_guess`'s
  variable), and its whole build (`RA_JIT_WHOLE_BUILD=1`) builds. What blaze does not read yet
  (no lock file or a stale one, `[replace]`, a git index) goes to cargo with the reason;
  `RA_JIT_OWN_RESOLVE=0` sends every load there. `harness/own_metadata.py` holds the output
  equal to `cargo metadata --filter-platform` and to `cargo build --unit-graph`'s packages,
  features and edges.
- blaze on Linux needs glibc 2.34 or newer (RHEL 9, Debian 12, Ubuntu 22.04), and the release
  holds it there (#351): `build_release.py` refuses a Linux binary, the proc-macro server's
  included, that asks for a newer glibc; install.sh says what a system below it lacks instead
  of a program that does not start; and the install workflow builds this commit as the release
  does, on Ubuntu 22.04 with the server, and installs it with no Rust in Debian 13, Debian 12,
  Ubuntu 22.04 and RHEL 9 (UBI 9) containers, each running `blaze run` and a derive from
  crates.io expanded by blaze's server, edited once.
- `RA_JIT_WHOLE_BUILD=1` (or `BLAZE_WHOLE_BUILD=1`): `cargo blaze build` and `run` from nothing
  build each program whole with blaze, every crate compiled by blaze over the shipped standard
  library and linked by blaze, and put it where cargo does, with no `cargo build`
  ([no-rustc.md](docs/no-rustc.md), step K, #91). A function the program can reach that blaze
  cannot compile fails the build with which and why, and nothing goes to cargo.
  `harness/coldbuild.py` holds the programs of a fixture equal to cargo's, and with `bench`
  does bench.py's projects.
- `crates/blaze-registry` fetches every kind of source a lock file names, as cargo 1.99
  does (#298): `[source]` replacement as cargo's `SourceConfigMap` (`replace-with` chains to a
  registry, a local registry, a directory, a git repository or a `[registries]` name, and
  cargo's refusals); vendored directories, each file checked against `.cargo-checksum.json`;
  and git dependencies with the `git` command, a database per repository at
  `$CARGO_HOME/git/db/<name>-<hash>` and a checkout per commit at
  `git/checkouts/<name>-<hash>/<short id>`, branch, tag, rev and locked commits, GitHub's fast
  path, submodules from databases of their own, offline. `harness/fetchdiff.py` holds it to
  `cargo fetch` and `cargo vendor` on dev-aws-8 over 314 lock files (the bench projects at
  their pins but bun, the 300 most downloaded crates, git dependencies of every kind of
  reference, a second sparse registry): 3684/3684 unpacked packages and 38/38 git checkouts
  the same as cargo's, file by file, with the same directory names; crates.io replaced by a
  mirror 42/42; ripgrep and zed vendored, 51/51 and 1533/1533 read where cargo vendor put
  them.
- `cargo blaze` reads the workspace with `blaze-manifest`, as cargo 1.99.0 reads it, where it
  ran `cargo metadata --no-deps` for every command (#298): equal to cargo on 385 of 385
  workspaces of `harness/resolve.py`'s corpus. The command's `--config` arguments now count,
  and a manifest cargo refuses ends the command with cargo's message and status. The daemon's
  loads still ask cargo.
- `ra-jit std` ships the standard library's unit for a `dev` program too, not only a build
  script's: a whole build of a dev program from an empty store (step K, #91) fetches it where
  it compiled the library again: ripgrep from nothing 19.4 s to 9.9 s on dev-aws-8.
- Live edits on Windows x86-64: `ra-jit-live` takes patches in a running PE program, named by
  the GUID of its PDB, and registers their unwind tables, and the daemon lays a patch of a PE
  executable out for it. An executable a program runs is spliced through a copy put in its
  place, as on Unix, instead of being linked again; the daemon wakes the program through a
  named event; and `ra_jit_live::edit` reaches the daemon, so `demo/life`'s editor builds as you
  type. life's edit to running is 26.6 ms at p50 (`socket`) on a Windows 11 desktop
  ([live.md](docs/live.md#latency-under-typing)). A static with another value still asks for a
  restart.

- `crates/blaze-build-script`: the environment cargo 1.99.0 gives a build script, computed
  from a plain description of the run with no cargo, ported from cargo's own rules and tests
  (no-rustc step I, #89; #298). Nothing uses it yet; blaze's own runs of scripts come next.
- `blaze-build-script` reads what a build script prints as cargo 1.99.0 does (both
  `cargo:` and `cargo::`, the `rust-version` rule of the latter, every directive and cargo's
  errors for each) and says who receives it: the scripts each unit is linked with, the `-L`,
  `-l`, link arguments, cfgs and variables of each target, and when a script runs again.
- `blaze-build-script` runs a build script as cargo 1.99.0 does (its package's directory, no
  input, `OUT_DIR` made first, a failed exit or a `cargo::error=` line failing the build,
  cargo's `output`, `stderr` and `root-output` files beside `OUT_DIR`), and
  `harness/scripts.py` holds it to cargo over 463 script runs of the benchmark applications and
  crates whose scripts build C or read others' metadata. It found two things cargo gives a
  script that the port had missed: a `cargo check` puts no `target/debug` in the library path,
  and on Linux cargo's libgit2 hands every script the `SSL_CERT_FILE` and `SSL_CERT_DIR` it
  found.
- blaze needs no Rust and no rustup installed (#298). With neither, it downloads Rust 1.99.0
  once into `~/.blaze/toolchains` (rustc, cargo, the standard library and its source: 97 MB on
  macOS, 133 MB on Linux x86-64), laid out as rustup lays out a toolchain, and builds with it.
  Each file's sha256 is built into blaze from the release's published manifest, and a file
  with other bytes is refused. Downloads run together over blaze's own HTTPS client, unpack
  into a directory beside the toolchain's and are renamed into place, concurrent builds
  download once, and a download that stopped goes on from where it stopped. rustup's 1.99.0,
  when installed, is still the one used. `blaze build`, `run`, `test`, `check` and `stop` are
  `cargo blaze`'s commands without cargo; `blaze toolchain` says which Rust blaze builds with.
- install.sh installs blaze on a machine with no Rust (#298). It no longer runs rustup: after
  installing, blaze downloads its own Rust (none when rustup has 1.99.0), `--no-toolchain`
  leaves that to the first build, it says what to install when there is no C compiler to link
  with, and its last line is `blaze run`. `BLAZE_ARCHIVE` installs an archive from a file.
  install.yaml installs this commit's build in a Debian container with no Rust and runs
  `blaze run` in a project written by hand.
- `crates/blaze-resolve`, cargo's resolver owned by blaze (#298), starts with cargo 1.99.0's package
  model, used by nothing yet: `SourceId` (path, git and sparse registries, git at a branch, tag or
  rev with its commit, directory, local registry) compared by cargo's canonical URL and written as
  lock files, package ids and messages write it; `PackageId`, `PackageIdSpec` (`-p` and
  `cargo metadata`'s ids), `Dependency` with renames, kinds, targets and its locked or `--precise`
  requirement; `Summary` and its feature map, with the implicit features of optional dependencies
  and cargo's errors for features; and the `Registry` the resolver asks, with yanked and offline
  versions marked. Its tests include cargo's own for these types.
- `blaze-resolve`'s `Registry` is cargo 1.99.0's: `query(&self, ..)` is an `async fn` that
  awaits what it fetches, in place of answering `Poll::Pending` and a `block_until_ready` (#298).
- `blaze-resolve` reads and writes `Cargo.lock` as cargo 1.99.0 does (#298): every encoding
  from 1 (checksums in `[metadata]`) to 4 (git references URL-encoded), `[[patch.unused]]` and
  `[replace]`, the comments above cargo's header kept, and a lock file written back byte for
  byte. As in cargo, an unchanged lock file keeps its encoding, a changed one is raised to the
  workspace's default (from its lowest `rust-version`), under `--locked` or `--frozen` a change
  is an error, and the checksums of the old file must agree with the new resolve. Its tests write
  back the lock files of ripgrep and fd releases and of git workspaces cargo generated, one of
  each encoding, and port cargo's `lockfile_compat` tests; `BLAZE_RESOLVE_LOCKFILES=dir` adds a
  directory of lock files (24 releases of twelve projects, zed's 70 git sources among them,
  write back byte for byte). A registry read from a lock file before its manifest is now named
  as cargo names it.
- `crates/blaze-registry`, registries read and packages fetched as cargo 1.99 does, with no
  cargo, used by nothing yet (`docs/self-contained.md`, #298). The sparse
  index (crates.io's and any other's): `config.json` and its `dl` templates, the index paths,
  each line read as cargo reads it (schema `v`, `features2`, `rust_version`, `pubtime`,
  `yanked`, `links`, renamed and other-registry dependencies, unknown fields, and the lines
  cargo skips as invalid or unsupported), with cargo's cache of each file in cargo's format and
  its `If-None-Match` / `If-Modified-Since` requests, so cargo and blaze share
  `$CARGO_HOME/registry/index`. A `.crate` file is checked against its sha256, kept in the
  store by that hash and in cargo's cache, and unpacked where cargo unpacks it
  (`registry/src/index.crates.io-1949cf8c6b5b557f/<name>-<version>/`, the directory named by
  cargo's hash of the source), with cargo's checks (entries under `<name>-<version>/` only, no
  links, the size limit, the umask, `.cargo-ok`), several at once, retried as `net.retry`
  says, under cargo's package cache lock.
- `crates/blaze-manifest`, `Cargo.toml` and `.cargo/config.toml` read as cargo 1.99.0 reads
  them, without running cargo: `cargo metadata --no-deps` computed by blaze (its `metadata`
  example prints the same JSON, warnings and errors), on cargo's own manifest schema
  (`cargo-util-schemas` 0.14.3) ([resolve.md](docs/resolve.md)). `harness/resolve.py` compares
  it with cargo on the bench projects, 300 crates.io crates and 72 workspace repositories at
  pinned commits: all 385 equal, key order included. Nothing in blaze uses it yet.
- blaze-manifest reads `.cargo/config.toml` into cargo's own config types through a port of
  cargo's config deserializer: `[build]`, `[net]`, `[http]`, `[env]`, `[target]` (rustflags in
  cargo's precedence, runners and linkers), `[registry]`, `[registries]`, `[source]`
  replacement, `[resolver]` and `[profile]`, each field from its file or its `CARGO_*`
  variable, with cargo's errors and unused-key warnings. Its `config` example is
  `cargo config get`, and `harness/resolve.py config` compares the two on config trees and
  every corpus repository: 103 of 103 equal over 767 runs ([resolve.md](docs/resolve.md)).
- blaze-manifest computes profiles as cargo 1.99 gives them to units (`blaze_manifest::profiles`,
  a port of cargo's): `dev`, `release`, `test`, `bench` and custom profiles through `inherits`,
  package overrides, `build-override`, profiles from config files, `--config` and
  `CARGO_PROFILE_*`, `CARGO_INCREMENTAL` and `CI`, with cargo's errors and warnings, and checks
  each manifest's profiles as cargo does reading it. `harness/resolve.py profiles` compares
  every unit's profile with cargo's `--unit-graph`: 86 of 89 trees and repositories equal over
  546 runs and 235,924 units, none with a different profile, the other three refused by
  cargo or nightly-only ([resolve.md](docs/resolve.md)).
- `cargo ra-jit test` with `RA_JIT_TESTS=affected` runs, on Linux, each test that reaches a
  file, the environment, a process or the network in a sandbox of its own (#257): a process
  in new user, mount, network, IPC and PID namespaces, the machine's files read-only, the
  workspace and target directory overlaid so its writes vanish, an empty `/tmp`, loopback
  only, and a fixed environment. A seccomp supervisor records every path it touches; once it
  passes, what was there is kept, and it runs again only when its code, its environment or one
  of those changed, where it ran every time. `-v` names the file. Its output, panics,
  `#[should_panic]` and exit status stay libtest's, as ra-jit writes the harness. On ripgrep
  the tests run with no edit go from 493 to 24 of 1,168 (the 469 that ran every time are
  sandboxed and skipped); the first run, which runs those 469 sandboxed, takes 85.5 s where it
  took 83.6 s. `ra-jit sandbox` is the command, for build scripts and proc macros next (#147).
  `RA_JIT_SANDBOX=off` runs them every time as before; where no sandbox can be made (macOS and
  Windows for now, Ubuntu's AppArmor userns restriction) the command says why once.
- `vendor/proc-macro-srv`, the 1.99.0 release's proc-macro server with a loader that reads
  the list of macros ra-jit writes into a proc-macro library, built by ra-jit with nothing of
  rustc's. `RA_JIT_PROC_MACRO_COMPARE` and `RA_JIT_PROC_MACRO_LIBS` expand each call again
  with it and ra-jit's library of the crate and compare the output with the toolchain's;
  `harness/macroserver.py` holds serde_derive's, thiserror's, tokio's and clap's equal
  ([no-rustc.md](docs/no-rustc.md), step J1). A proc-macro crate's `fn main` is no longer
  taken for an entry point.
- `RA_JIT_PROC_MACRO_SERVER` naming that server has a load into ra-jit's own target directory
  link its proc macros with ra-jit and expand them with the server, the toolchain's server
  and rustc's libraries left for what ra-jit did not link (#90). `macroserver.py all` builds
  fd, bat, hyperfine, just, nushell and helix both ways: 3,465 calls the same, 12 of
  `pest_consume_macros` not compared, as ra-jit cannot compile it yet (#327).
- The Linux release has blaze's proc-macro server, `blaze-proc-macro-srv`, beside `ra-jit`,
  built whole by the release's ra-jit (`build_release.py --server`);
  `RA_JIT_PROC_MACRO_SERVER=1` uses it, and says so when it is not there. A load whose proc
  macros are all ra-jit's starts no toolchain server.
- The SSE and AVX compares compile (#87): `llvm.x86.sse.cmp.ps` and `.ss`, `sse2.cmp.pd` and
  `.sd` and AVX's 256-bit ones give a lane of all ones where the immediate's predicate holds, as
  rustc's do. glam's `Vec4::cmpeq` and `is_finite_mask`, which bevy calls, were left to rustc;
  `harness/breakers` checks each predicate, NaN lanes included, against rustc's program.
- `cargo ra-jit check --message-format=json` (and `short`, `json-diagnostic-short`,
  `json-diagnostic-rendered-ansi`, `json-render-diagnostics`) writes the daemon's verdict as
  `cargo check` does: each error a `compiler-message` with rustc's code, level, spans (bytes,
  lines, columns counted as rustc counts them, the source lines and their highlights) and
  `rendered`, cargo's package id and target, the failure notes, `build-finished` and the
  `could not compile` lines. rust-analyzer's check-on-save and editors read it, so they show
  ra-jit's verdict and not rustc's. `-Z`, `--exclude` and several `--target`s are the
  checker's too (a checker for each target, four held at once), where they went to cargo.
  `harness/check_json.py` compares it with `cargo check` on the same errors: 7 packages, every
  format, every compared field the same; the labels, help and notes, other spans and
  `code.explanation` are rustc's only, and the messages are rust-analyzer's wording.

- `RA_JIT_UNITS=1` compiles a whole program's functions many to an LLVM module, as rustc
  compiles a crate's codegen units (#176): the functions are found from their IR before any
  is compiled, cut into units of about 20,000 instructions in the order they were found in,
  and compiled on every core, each unit with the union of its functions' imports linked in.
  An import LLVM leaves a call of is kept as the code of its function, so the functions are
  found in one round, and each function's code is read out of the unit's object and put in
  the program's as a compile of it alone is. It is not the default: on 16 cores helix builds
  in 71.1 s against 69.7 s and runs 1.28x of cargo's release build against 1.21x, and LLVM
  takes four times rustc's time for code of the same size either way
  ([codegen-units.md](docs/research/codegen-units.md)). Imports are linked into a compile's
  module once, after being linked to each other two at a time, which took the lowering of a
  module of many functions from 23.7 s to 8.1 s summed over ripgrep's cores.

- An edit that needs a build in full no longer links in full when the daemon has no base of
  its own (it gave it up, or hasn't built it yet) (#159): on Linux the program is built onto
  the executable rustc linked and spliced into it whole, and the edits after it are patches of
  that. nushell's signature edit with its base given up at 2 GB is spliced in 1.5 s, and the
  edits after splice in about 10 ms. macOS still links such an edit in full, but no longer
  twice: the link takes the room for splices the executable there wanted. `rustcbase.py`
  checks that every build in full of the rustc phase is spliced whole on Linux, and
  `own-base-budget-full` a given-up base's.
- Const eval answers nightly's reflection, `core::mem::type_info` (#34): a type's variants
  and fields, a field's type, name and offset, a type's size, whether it is signed, an array's
  length and element, what a pointer points to, a variant's name, as rustc does, and two of
  its `TypeId`s compare. Bun's `MultiArrayList` asks it of every type it holds, which left 907
  of bun's functions to rustc; `harness/bun-repros/type_info` asks what it does and now
  compiles. A constant that holds a `TypeId` is still refused: the evaluator's ids are not
  rustc's hashes.
- Const eval answers every reflection intrinsic the pinned nightly's core declares
  (`core::intrinsics::reflection`), as rustc's const eval does: `type_id_size_of` and
  `type_id_variant_name` under their new names, and `type_id_type_of` (`Type::of`'s kind, a
  `dyn`'s principal and auto traits), `type_id_generics`, `type_id_function_ptr`,
  `type_id_non_exhaustive` and `type_id_variant_non_exhaustive`. `type_id_vtable` answers
  `None` where the solver rules the trait out and is refused otherwise: rustc proves it in a
  typing mode the solver here does not have. The `Type` of a tuple is refused, as rustc at
  the pinned nightly ICEs on it.
- The daemon's workers hold at most half of the machine's memory together, or what
  `RA_JIT_MEMORY_MAX` says (a size, or `off`). Over it, the worker asked longest ago is
  stopped; `cargo ra-jit status` shows the limit and what each worker holds. `docs/reports/SOAK.md`
  described the variable, but it was never brought over from the soak branch (#60).
- `RA_JIT_OTHER_TOOLCHAIN=1` has `cargo ra-jit` build with a rustc other than 1.99.0, for a
  project pinned to a nightly of the next release that 1.99.0 can't build (bun's
  `nightly-2026-09-15`). Without it such a build goes to cargo, as before, and says why. The
  builds of a nightly cargo are the daemon's: `-Z embed-metadata=no`, which it passes every
  crate, no longer sends them to cargo, and build scripts' output and the seeded target are
  found in its second build-directory layout (`soakrows.py`, "a project pinned to a nightly").
- The `bench-bun` workflow measures bun nightly on the benchmark machine (edits and runtime),
  in `nix develop .#bench-bun`: bun's nightly and what its own build of its C and C++ needs.
  Too slow for every push.
- `benchreport.py --name NAME` stores a project measured on its own in
  `docs/benchmarks/NAME-<os>-<arch>.json` (`runtime-NAME-…` for the runtime), and every README
  table and chart shows it as a project like the others, in the geometric means too, with a
  footnote naming the machine, commit and date it was measured at.
- `bench.py bun` times the edits on bun (body, signature, leaf, generic, new function) in a copy
  of the tree `bunprep.py` prepares, under bun's pinned nightly; bun is measured only when
  named.
- bun (its Rust port, 1.4.3: 1.25M lines in 103 crates, with a JavaScript runtime's C and C++)
  in `apps.json`, so `apps.py bun` and `runbench.py bun` build it whole-program and compare
  its runs with cargo's. An entry's `prepare` names a module of the harness that makes what
  the program's own build makes before cargo: `bunprep.py` runs bun's build of its C and C++
  once per revision and adds a member, `bun-exe`, that rustc links with them.
- `RA_JIT_MEMORY` reports after every request a worker answers too (`request-N`), and on
  macOS its resident figure is the physical footprint rather than 0. A `heap-profile` build
  writes where it was loaded beside each heap profile, which `harness/heapfold.py` needs to
  name the stacks on macOS, and `harness/heap.py` gives the bytes to the salsa query whose
  value or record holds them. What they found
  ([#39](https://github.com/indexable-inc/ra-jit/issues/39),
  [docs/research/daemon-memory.md](docs/research/daemon-memory.md)): nushell's worker holds
  10.0 GB live once ready, 2.7 GB of it what a debugger is told of types; ripgrep's 1.6 GB;
  and the spare worker the daemon starts after the first build holds 940 MB more on ripgrep.
  salsa's `lru` cannot evict `infer` or MIR (they have cycle handling), and on `described`
  it freed 206 MB of ripgrep's live heap and none of its footprint, so no eviction is on.
- Proc-macro expansions are kept in the store
  ([#38](https://github.com/indexable-inc/ra-jit/issues/38)): a derive or attribute macro's
  output is found again by the next daemon, keyed by the macro library's content, the
  macro, its input tokens and the shape of their spans, the environment and the directory.
  An expansion that asked the compiler anything, or failed, is not kept. Function-like macros
  are kept with `RA_JIT_PROC_MACRO_CACHE=all`, a macro is left out by name with
  `RA_JIT_PROC_MACRO_CACHE_SKIP`, and `RA_JIT_PROC_MACRO_CACHE=check` expands anyway and
  reports any that differ ([docs/cache.md](docs/cache.md), `harness/expansions.py`). On a
  warm start helix's 1455 kept expansions take resolve from 6.0 to 4.3 s and the time to
  ready from 9.6 to 8.0 s, nushell's 1588 from 5.8 to 4.5 s and from 10.1 to 8.8 s (Linux
  x86-64, 16 cores of an otherwise idle host, medians of four runs each); cold starts
  (helix 35.5 and 35.2 s, nushell 36.8 and 36.7 s) and memory do not change.
- `#[ra_jit::cache]` ([#17](https://github.com/indexable-inc/ra-jit/issues/17)): the
  `ra-jit-cache` crate's attribute keeps what a pure function returns, in memory and under
  `target/ra-jit/fn-cache`, by its arguments and a hash of the code ra-jit compiled for it and
  for everything it reaches. Comments, formatting and edits elsewhere keep the results; an
  edit to anything it reaches drops them, live edits too. Functions that read statics that
  change, thread-locals or the system (I/O, time, randomness) are refused with where that is
  written. Under plain cargo it does nothing. `cargo ra-jit cache clear` forgets what was kept
  ([docs/fn-cache.md](docs/fn-cache.md), `harness/fncache.py`).
- `harness/abbench.py` runs builds of ra-jit against each other on `bench.py`'s edits, in
  interleaved rounds: edit and cold-start times, the workers' cycles on macOS and their peak
  memory. With `--train` it is the training of the release build's PGO.
- rustc's MIR optimizations, optionally: `RA_JIT_MIR_OPT=1` runs what rustc runs on a debug
  build's MIR (copy propagation, removing unused locals, storage markers and drops of what
  needs none, folding constant branches, simplifying the control flow and instructions),
  `RA_JIT_MIR_OPT=2` what it runs above `-Copt-level=0` that is ported (also dead stores and
  unreachable blocks). Off by default. The interpreter's tests run every program at all three
  levels and require the same result ([docs/research/mir-gap.md](docs/research/mir-gap.md)).
- `ra-jit mir <dir> --crate <name> [--level N]` prints a crate's MIR as ra-jit compiles it,
  in rustc's `-Zunpretty=mir` format, and `harness/mirparity.py` compares it with rustc's
  function by function: at level 1, 21.5% of functions are identical to rustc's, against 4.0%
  without the passes.
- The README has a table of how fast programs ra-jit builds with LLVM run against
  `cargo build --release`, written by `bench.py --runtime` and `benchreport.py` and kept current
  by the `bench-readme` workflow on the Linux benchmark machine. A run in which any program's
  output differs from cargo's is never published. The first, on Linux x86-64, has every
  program's output the same as cargo's; on macOS arm64, ripgrep, fd, bat and jsonrt do not
  build whole yet (NEON intrinsics ra-jit does not have).
- [docs/research/](docs/research/README.md): what 2026's Rust compiler work means for ra-jit,
  and the MIR gap with rustc with the production gaps ranked; rustc's pass sources in
  [docs/upstream/rustc-mir-transform/](docs/upstream/rustc-mir-transform/SOURCE.md).

- Tiers ([#6](https://github.com/indexable-inc/ra-jit/issues/6)): a program that takes
  edits with `ra-jit-live` is sampled while it runs, and the functions it spends its time in
  are compiled again with LLVM, at `-O3` with what they call built in (level 2) and then with
  the branch counts level 2 took as `!prof` (level 3), and swapped in as patches without a
  restart. An edit sends an edited function back to Cranelift in the same build.
  `RA_JIT_TIER=off` turns it off. The Game of Life window shows each hot function's level
  and the promotions as they happen; `demo/lifebench` and `harness/tiers.py` measure and
  check it ([docs/tiers.md](docs/tiers.md)).
- `ra_jit_live::tiers()`, and the sampler that the daemon turns on in a running program.
  It stops once the worker that reads it is gone.
- Benchmarks on real projects that keep the README current: `harness/bench.py` measures
  ripgrep, fd, bat, hyperfine, just, axum and nushell at pinned commits (cold build, no-op,
  four kinds of edit, and how fast ripgrep's program runs) against incremental cargo, on both
  tiers, keeping every sample; `harness/benchreport.py` writes the README table and
  [docs/benchmarks.md](docs/benchmarks.md) from the result; the `bench-readme` workflow runs
  them on a dedicated Linux x86-64 machine after pushes to main and nightly, and commits the
  numbers to main when they moved beyond noise.
- `harness/bench.py` measures what [#7](https://github.com/indexable-inc/ra-jit/issues/7)
  asks of real projects: helix, a bevy example and zed (1.7M lines) join the seven, every project gets two more
  edits (a new function, a struct field set wherever the struct is built), the cold build is
  run `--cold-runs` times against fresh cargo, each with an empty store, and as many times
  in a second checkout with the store the first filled, the store's size is kept, and `--tests`
  runs each project's test suite under `cargo ra-jit test` and `cargo test` and fails on any
  test that ends differently. A project whose build fails keeps what it measured, with the
  error, and the run goes on to the next. A run with anything wrong or failed still writes its
  result and ends with exit code 3; `benchreport.py` publishes a test suite that ended differently in the tests
  table (a program that prints something else is still refused), and the workflow commits the
  numbers, then fails.
- `bench.py --setups` builds every step again with the fastest ways to build Rust there
  already are ([#8](https://github.com/indexable-inc/ra-jit/issues/8)): mold, wild,
  `-Zthreads=8`, rustc_codegen_cranelift on a pinned nightly, all three at once, and
  `cargo check`, each into its own target directory; `harness/hotpatch.py` times an edit of
  a running program under Dioxus's subsecond (`dx serve --hotpatch`) against `cargo ra-jit
  build` and `cargo build` with a restart, stopping the program before the build as `cargo
  watch` does. The bench shell has mold, wild, `dx` and the nightly, on Linux and on macOS.
  Measured on a MacBook (`docs/benchmarks/hotpatch/macbook.json`): subsecond 346 ms
  median, `cargo ra-jit build` with a restart 469 ms, `cargo build` with a restart 520 ms; a
  live patch through `cargo ra-jit build` 12.5 ms and through the editor's socket 4.0 ms
  (`docs/benchmarks/live/macbook-probe-cargo.json`). On Linux, 16 cores of a c7i.16xlarge
  (`docs/benchmarks/hotpatch/linux-x86_64.json`): subsecond 289 ms, `cargo ra-jit build`
  with a restart 40 ms, `cargo build` with a restart 361 ms. On ripgrep's cold build mold,
  wild, `-Zthreads=8` and cg_clif together are fastest (3.25 s against cargo's 5.56 s), and
  every edit there and on nushell builds faster under ra-jit than under any of them, `cargo
  check` included; with nothing changed, on ripgrep, cargo with mold is faster (28 ms against 33).
- Benchmark history and regression alerts
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)): `harness/benchhistory.py`
  appends each run's medians to `docs/benchmarks/history/*.jsonl` and checks the last against
  the runs before it on the same machine; the `bench-readme` workflow commits the history on
  every run, starts the site's deploy, and on a regression past the noise band opens (or
  comments on) an issue labelled `bench-regression` and fails. The live-edit latency workflow
  runs every night on main on a macOS arm64 runner and does the same with its history.
- [ix.dev/blaze/bench](https://ix.dev/blaze/bench): the benchmark history over commits, from
  `docs/benchmarks/history`: the latest run's table per project, and charts of each edit,
  the cold build, the daemon's peak memory, the store size and live edit-to-running latency
  ([#10](https://github.com/indexable-inc/ra-jit/issues/10)).
- LLVM is given what rustc gives it: reference parameters marked `noalias`, `readonly`,
  `dereferenceable`, `align`, `nonnull` and `noundef` as rustc marks them, addresses as
  pointers rather than integers, branches threaded before `-O3`, unsigned overflow checked as
  rustc checks it, a second round of cleanup after `-O3`, and, at a level above 1, the bodies
  of the callees it keeps calling and of what those call. Level 3 of `demo/lifebench` runs
  at 88% of a release build and 94 to 96% of one that checks overflow as the dev profile
  does, from 71 to 74% and 77 to 81%, measured side by side on a loaded machine
  ([docs/tiers.md](docs/tiers.md)).
- `harness/tierbench.py`, the benchmark table of docs/tiers.md; `--together` runs the builds
  of a round at once, so that a loaded machine slows them alike.
- `RA_JIT_DUMP_LLVM=after` prints the LLVM IR as the passes left it.
- Live-edit latency under real typing ([#9](https://github.com/indexable-inc/ra-jit/issues/9)):
  `harness/live_latency.py replay` types scripted edits one key at a time at 30 to 100 ms a
  key, with pauses at word ends, typos and backspaces, sends the whole file on each key as an
  editor does, and times every key whose text compiles to the program running it, by patch or
  by a restart as a launcher would do one; it counts the keys that do not compile, the
  superseded edits and the restarts. `restart` times a session's struct changes against
  `cargo run`. Every table has p95 and p99, and `LATENCY_JSON` carries the machine, its
  battery, the commit and the run, which `benchhistory.py live` reads. `harness/live-bevy`
  (bevy 0.20, generic systems) and `harness/live-egui` (egui 0.37, generic widgets) are real
  programs that take edits. Measured on a MacBook Pro in
  [docs/live.md](docs/live.md#latency-under-typing).
- A borrow checker, the crate `hir-ty-borrowck` in the rust-analyzer subtree, written to give
  rustc's verdict: Polonius alpha by default as nightly rustc does, NLL with
  `RA_JIT_BORROWCK=nll`. It gives rustc's errors on
  1541 of 1581 of rustc's own borrow-check tests that it can run, and none on any of the 246
  rustc accepts, scored by `harness/borrowck_ui.py`, and finds nothing in ripgrep, fd, bat,
  hyperfine, just, axum, nushell, delta, eza, tokei, starship and cargo. Its errors, and rustc's
  `unused_mut` warning, are rust-analyzer diagnostics and `ra-jit borrowck` prints them ([docs/borrowck.md](docs/borrowck.md)).
- Item well-formedness, the crate `hir-ty-wfcheck` next to the borrow checker: rustc's E0309
  and E0310 outside function bodies, for struct, enum and union fields (`&'static T` without
  `T: 'static`), a trait impl's associated types, an impl of a trait bounded by `'static`,
  and the where clauses of ADTs, impls, traits and functions. Its errors are refused as the borrow checker's are.
- The daemon refuses to build an edit with a borrow-check error, as it does one with a type
  error; `RA_JIT_BORROWCK_REFUSE=0` turns that off. The checker still misses about a third
  of rustc's borrow-check errors, so `cargo check` stays the final word.
- `ra-jit borrowck`: the borrow-check errors of single files, or of a cargo workspace with
  `--project`.
- Inlining across crates, on by default in a program built whole with LLVM at `-O2` or
  `-O3`: it has what it calls in every crate built into it or shown to LLVM, the standard
  library's generic and `#[inline]` functions as rustc does across crates.
  `RA_JIT_INLINE=crate` keeps that to the function's own crate, `RA_JIT_INLINE=all` does it
  in other builds too. On Linux x86-64 (AWS c6i.16xlarge, at 13ee5a5339) the geometric mean
  of 14 runtime benchmarks was 1.15x the time of `cargo build --release` at `-O3` and 1.16x
  at `-O2`, against 3.89x with each crate alone; the programs other than the kernels took
  2.3 to 3.7 times as long to build as with each crate alone (ripgrep 30.1 s against 8.5 s).
  The README's table, measured at the merge into main, has 1.21x over 15 benchmarks
  ([README](README.md#how-fast)): numeric's ra-jit build took 57 ms in three of its five
  runs and 32 ms in the other two, where release's took 31 ms in all. A whole program's
  build from cold against `cargo build --release`'s from cold: ripgrep 31.7 s against
  8.4 s, tokei 50.7 s against 13.2 s, bat 125 s against 53.3 s, eza 38.9 s against 34.6 s.
  Commit 99cbb902ad's message
  says the levels alone turned it on; until b460bbe327 they did not, and it took
  `RA_JIT_INLINE=all`.
- `blaze-resolve` resolves as cargo 1.99.0 does (#298): its resolver is cargo's, ported, with
  backtracking, the conflict cache, `links`, prereleases, yanked versions only when locked,
  `[replace]`, and versions preferred by `rust-version` or lowest first; the errors are cargo's.
  Its tests are cargo's `resolver-tests`, the 37 of `resolve.rs` and the 20 of `validated.rs`
  (whose check against a SAT solver is left out). cargo's unstable `-Zdirect-minimal-versions`
  and publish-age policy are not ported.
- `blaze-resolve` picks features as cargo 1.99.0 does (#298): the members `-p`, `--features`,
  `--all-features` and `--no-default-features` select, by resolver 1's rules (`--features` for
  the package in the current directory, `member/feat` for another) or resolver 2's, with
  cargo's errors and suggestions; and cargo's feature resolver, which under resolver 2 keeps the
  features of build dependencies and proc macros apart from the rest, leaves out those of
  dev-dependencies when nothing is tested, and of dependencies for platforms not built for, and
  enables `dep?/feat` once `dep` is. A package's targets and the platforms' `cfg` come from
  whoever calls it. Tests take cases of cargo's `features2`, `weak_dep_features` and
  `package_features` tests.
- `blaze-resolve` writes the `resolve` of `cargo metadata` as cargo 1.99.0 does (#298): each
  package reached from the members, its dependencies by the name its code uses, their kinds and
  platforms, and its features, filtered by `--filter-platform`. The test compares the JSON with
  cargo's for a workspace with a renamed dependency, one used as both normal and dev, a build
  dependency and a `cfg(unix)` one. `harness/resolve_graph.py` compares, over the corpus of
  `harness/resolve`, the `resolve` blaze-resolve makes of the packages cargo lists with cargo's,
  with each project's lock file and with none.
- `blaze-resolve` resolves a workspace as cargo 1.99.0 does around its resolver (#298): the
  resolve `Cargo.lock` is written from (every member, every feature), guided by the lock file,
  then the one asked for, guided by the first, with cargo's `PackageRegistry` applying
  `[patch]` and locking each summary to the previous resolve. So a lock file keeps the versions
  it names, and the graph is cargo's where the two resolves differ, as when a patched member
  is reached both as itself and as the patch. `harness/resolve_graph.py` resolves each
  project so, from the lock file it ships or none: 322 of 324 equal with it and 381 of 385
  without (from 292 and 362), the rest projects cargo itself does not resolve there.

### Changed

- A build script's `RUSTC` is blaze, so its probes run no rustc: `rustc -vV` and `--print`
  are blaze's answers, and a probe compile (`autocfg`, `anyhow`, `thiserror`, `rustix`,
  `proc-macro2`) gets `cargo blaze check`'s verdict, or an error naming the command where blaze
  can't give one. cargo's `-Clinker-features=-lld` no longer sends a question to rustc
  (docs/no-rustc.md, step I).

- LLVM's inliner runs at threshold 500 (was the level's 225 or 250): the runtime set's
  geomean goes from 1.091 to 1.080 of rustc's (tokei 1.28 to 1.26, ripgrep 1.14 to 1.11) for
  2 to 5% more code and no more compile time; docs/tiers.md has the sweep (225, 500, 1000, and
  nothing built in by blaze). `RA_JIT_LLVM_INLINE_THRESHOLD` is in the store's key.
- The LLVM backend is on LLVM 23.1.4 (was 20.1.8), our fork's `ix/23.x` at the commit
  wasm-host's ix-aot pins, so both run one LLVM: inkwell 0.11 and llvm-sys 231. Blaze
  builds that LLVM itself for each target (`.github/workflows/llvm.yaml`) and pins the
  archives by sha256 in `crates/hir-ty-codegen/llvm.json`; the flake, CI and Windows fetch
  them. It is linked in statically, with libstdc++ on Linux, so a binary built with
  `--features llvm` loads no libLLVM. llvm-sys links it (`vendor/llvm-sys`), so GNU ld sees the
  archives after inkwell, which calls them. LLVM 21 removed `nocapture`: an `sret` parameter is
  `captures(none)`, and an attribute name the linked LLVM does not know now panics naming it
  rather than crashing LLVM's verifier.
- The site moved to [ix.dev/blaze](https://ix.dev/blaze/). https://ra-jit.ix.dev and
  https://blaze.ix.dev answer every path with a 301 to the same path and query under
  https://ix.dev/blaze, so old links (the README's updates button, `/bench`, RFCs) keep
  working. Every page, script, stylesheet, image, narration file and form sits under `/blaze`
  (`Served` on `site` in `site/content/site.flecs`); pages carry a canonical URL and Open Graph
  tags, `/blaze/sitemap.xml` lists them, and `/blaze/install.sh` serves `install.sh`.
- The site says blaze, not ra-jit: page titles, the top bar, `og:site_name`, the home page, the
  charter, the RFCs, `/bench` and `/updates`, and their narration, voiced again. ra-jit is left
  only where it is still the literal name: the `ra-jit-live` crate and `#[ra_jit::cache]`, and
  the line that blaze was called ra-jit until 2026-10-24 and `cargo ra-jit` still works.
  Comments keep their pages and blocks (ids unchanged).
- No `cargo blaze` command is passed to cargo any more (Andrew, 2026-10-26: commands that are
  no builds "should go to us and not cargo"). `metadata`, `locate-project`, `verify-project`,
  `version` and `help` are blaze's own, from blaze-manifest, blaze-registry and blaze-resolve
  with no cargo or rustc started; every other command of cargo's (`fmt`, `clippy`, `doc`,
  `tree`, `publish` ...) fails with status 101, saying blaze has none yet and that nothing was
  done, and a command cargo does not have is `no such command` rather than a cargo plugin run.
  `cargo blaze --list` and the table in docs/cargo.md ("Every subcommand") say which is which;
  flags typed before the command are read after it.
- `cargo blaze test` is built whole by blaze too, with no first build by cargo and no rustc:
  blaze's resolution plans what cargo would test (each member's library, binaries and
  integration tests with `test = true`, the examples built beside them, and the binaries an
  integration test runs, `CARGO_BIN_EXE_<name>`), or what `--lib`, `--bin`, `--example` and
  one `--test` name. The unit tests are built by a worker that loads each tested crate twice,
  its copy of tests alone with `cfg(test)` as rustc's `--test` has it, the rest by a worker
  that loads it once, and each test runs as before, in its package's
  directory and in the sandbox. A program of tests compiles libtest (`test`, and what only it
  links) with its own crates, as the library built apart leaves it out and no rlib of the
  toolchain is linked. A unit test with `harness = false`, several `--test`s and the
  other target flags fail the build and say so. The tests run in cargo's order, a package's
  library first, and each run asks for what each test reaches, so `RA_JIT_TESTS=affected`
  picks as before. The doc tests (and `--doc` alone) are run by blaze with no cargo, rustdoc
  or rustc: blaze writes down for each library the run of rustdoc cargo would make, and one
  run loads the workspace and builds every example. An example blaze cannot build (one
  rust-analyzer finds an error in, one that mixes copies of the library, one that calls what
  blaze cannot compile) fails, saying why, where rustc built it before; a `compile_fail` one
  passes when rust-analyzer finds an error in it, and fails, saying so, when it finds none.
  `harness/doctests.py` counts those apart from the examples that end otherwise than
  cargo's, and ra-jit is no longer cargo's `RUSTDOC`.
- `cargo blaze build` and `run` have one path, and it runs no rustc: each program is built
  whole by blaze, as `RA_JIT_WHOLE_BUILD=1` had it, and the setting is gone. No first build
  by cargo, no rustc base under the first edits, and no fallback: what blaze cannot compile,
  plan (`RA_JIT_OWN_RESOLVE=0` no longer asks cargo for the programs) or build yet fails the
  build and says what (the targets `--bins`, `--tests` and their kin pick, several
  `--target`s, `--target-dir`, `--message-format`, `-Z`, `--build-plan`). A build of
  libraries alone is the daemon's check of them. The cold build is slower than cargo's for
  now (docs/self-contained.md has the figures). blaze's link gives the program a GNU build ID on Linux, by which it takes live edits. On macOS a program finds the statics and functions of the standard library blaze built once by their names without Mach-O's leading underscore, as it asks for them, so std's `GLOBAL_PANIC_COUNT` is no longer refused as having no symbol. A patch compiles its own copy of a function of the program the link left out, as LLVM does one it inlined into every caller, instead of asking for it by name and restarting the program. A
  patch the splice declines (one that calls what the link left out as unreached) has the
  program built in full and linked, where the link of the object alone left the edit out. A
  workspace with no lock file, or one its manifests no longer agree with, has it written by
  blaze's resolver as cargo would, when no registry package is needed at a version the lock
  file lacks (#321). `harness/live_fallback.py`, `harness/aheadready.py` and
  `harness/renamedstatics.py`, of cargo's first build, are gone; `harness/unreached.py` and
  `harness/storetracked.py` take a program built in full where its edit calls what the link
  left out.
- A whole build's cold compile no longer waits on itself where a hundred threads meet. What
  the store is given of a function names each function and static it reaches by a name its
  crate is listed to find again: each crate is listed once, by the first thread to ask and
  outside the lock the others take, a name with types for arguments is not looked for in any
  listing, a name that is exactly one root of the program is taken as that root (a callee's
  lifetimes erased), and the statics are listed by the threads compiling, beside the compile,
  not by every thread at once after it. The threads compiling ahead also warm the values of the
  statics and the places of the panics the object thread asks for, are woken one for each new
  job rather than all for each, and the object thread waits on a condvar of its own. A crate's
  content id is hashed once, the type sets debuginfo tells of are sharded by environment, each
  thread keeps the values it interned in front of the shared shards, and the frozen parts are
  encoded on every core (their line says how long writing took). nushell on dev-aws-14,
  `harness/coldbuild.py bench`: the store's entries are made in 0.84 s where they took 1.6 to
  1.8 s (the threads' wall in them 48 s where 310 s), the frozen parts written in 1.3 s; the
  cold build is 47.9 s (cargo 24.0 s), still waiting on single long jobs and on roots found one
  after another, which [no-rustc.md](docs/no-rustc.md) (K) records.
- blaze builds with one Rust, the latest nightly: `nightly-2026-10-25` (rust-lang/rust
  12433c307), pinned in `harness/toolchain` and moved with `harness/toolchain_bump.py`, in
  place of stable 1.99.0. `vendor/rustc_abi` and the proc-macro server are taken from that
  commit, the answers to rustc's questions are that nightly's, and `TypeId` hashes, the abort
  intrinsic and the `core::arch` LLVM intrinsics follow it. A crate with `#![feature(...)]`
  builds like any other (`harness/live-alloc` runs a program on `allocator_api` with live
  edits); deref patterns are lowered as `rustc_mir_build` lowers them; every `-Z` flag of the
  nightly is ignored, modelled or refused by one table (`-Zthreads` is ignored); and cargo's
  second build-directory layout (`build/<pkg>/<hash>/{out,fingerprint,run}`) is followed
  beside the first.
- The first edit after a daemon starts on the store no longer compiles every function from
  the store in the edited file to tell what changed. Each entry names the items of the
  checkout its code comes from, by a hash of their text: the body it is written in, the
  bodies whose places it tells (with where each begins), those it mentions a coroutine of,
  and those that give an `impl Trait` in it its type. After an edit of nothing but bodies,
  code whose items read as they did is taken as it was (the timeline's `asked:` line says how
  much). warmstart.py's restart of ripgrep, dev-aws-7: the patch asks 17 functions where it
  asked 382, and the edit takes 0.22 to 0.23 s where it took 0.30 to 0.32 s. nushell's asks
  18 where 156 and takes as long, 0.53 to 0.70 s: what is left on both is the code that
  changed, compiled on a cold database (main before #360 compiled everything at the start and
  took 0.09 s and 0.37 s, its start 4.8 s longer on nushell)
  ([warm-start.md](docs/research/warm-start.md)). A stored mark no longer hashes where the
  named locals are kept for a debugger, which a full build and a later compile of the same
  code can tell apart, and which had every edit after a restart compile such code again.
  Entries are kept under new keys. `storetracked.py` has the edits of one body take some
  code by its items, a daemon that checks everything find none of it changed, and the
  closure an `impl Trait` is change.
- A closure's stable name is its place among the closures of its body, as rust-analyzer
  interns it, and a hash of its source, where it was the index of its expression, which an
  edit above it moved (one with no source, as an `async fn`'s coroutine, keeps that). What
  calls a closure does not say what it takes, so the hash names a closure that takes another's
  place apart from it. An edit above a closure renamed it and every instance generic over
  it, and a patch compiled again each of those that came from the store: the first edit
  after warmstart.py's restart of ripgrep patches 19 functions where it patched 176 (nushell
  17 where 66), and takes 0.26 s where it took 0.28 to 0.29 s (nushell 0.54 to 0.56 s where
  0.57 to 0.58 s). A patch says how many instances it compiled that the base had no code
  for. `storetracked.py` holds the edits of a generic function's body and of a line above a
  closure to none, and has a closure added above another, which took that one's name and was
  called with its code until the hash, rename it.
- A function whose code calls rustc's copy of a function of the checkout is kept in the
  store, and so is what calls it (#216). Such a copy is of a small function that is not
  generic, which rustc compiled into the crate that calls it, as ripgrep's `Match::start`
  in `rg`; the entry names the function and the copy, and the build that takes it from the
  store calls the copy as one that compiled the caller would. A restart of nushell with
  nothing changed (warmstart.py, dev-aws-5, two interleaved rounds) compiles 17 functions
  where it compiled 25,844, finds 7,622 of its 12,559 roots in the store where it found
  4,888, and takes 19.7 to 19.9 s where it took 22.8 to 22.9 s; ripgrep's compiles none
  where it compiled 2,375 and takes 4.25 to 4.33 s where it took 4.61 to 4.68 s. The first
  edit after such a start takes longer (nushell 0.58 s where 0.33 s, ripgrep 0.29 s where
  0.08 s): a closure is named by where in its body it is written, so an edit above it
  renames it and what is generic over it, which a patch compiles again when it came from the
  store. `storetracked.py` checks that a restart keeps such calls and that an edit of the
  copied function reaches the callers from the store.
- Proc macros are blaze's by default where its server is beside `ra-jit`: the daemon's
  loads, `--check` and build scripts expand with libraries ra-jit makes from each crate's
  record, kept in the store by what they were linked from, so another target directory takes
  them from there (#90). `RA_JIT_PROC_MACRO_SERVER=0` goes back to rustc's libraries and the
  toolchain's server. `macroserver.py all` builds each app again over the first store: no
  library linked again, the same 3,465 calls.
||||||| 0858e8535d
- The first `cargo blaze build` of a workspace is blaze's, not cargo's: once cargo's build
  succeeds it waits for the daemon to load the workspace ("Waiting for blaze to load the
  workspace") and build the programs, where it handed back cargo's and the next build was the
  first the daemon made. On a dev host (16 cores, 2 runs) it ends at 6.1–6.7 s on ripgrep,
  where cargo's ended at 5.4–6.7 s and the daemon was ready at 5.9–7.5 s, and at 41.0–41.9 s on
  nushell, where cargo's ended at 36.6–37.9 s and the daemon was ready at 41.6–42.5 s. The
  daemon still patches what rustc linked by default: building its own base instead
  (`RA_JIT_RUSTC_BASE=0`) has it ready at 7.5–10.3 s on ripgrep and 58.0–58.5 s on nushell,
  as it compiles every function of the program again after cargo.
- `cargo blaze` builds with Rust 1.99.0, the release it compiles, whatever toolchain the
  project names (#269). A project pinned to another release, or on a machine whose default
  is another, no longer has every build go to cargo: cargo, rustc and the daemon run under
  1.99.0 (`RUSTUP_TOOLCHAIN` and `CARGO` name it), and plain `cargo` keeps the project's
  toolchain in the same target directory, which cargo keys by the rustc. Code that only the
  project's own toolchain builds (a nightly's `#![feature]`, a newer release's library) fails
  with rustc's error and a note naming both releases. Without 1.99.0 or its `rust-src`, the
  build fails saying the `rustup` command that installs it, where it went to cargo. A
  changed toolchain file no longer sends the next build to cargo. `BLAZE_OTHER_TOOLCHAIN=1`
  still builds with the project's toolchain. `cargo_ra_jit.py` builds and edits projects
  pinned to an older stable and to a nightly, through toolchains whose rustc compiles nothing.
- A daemon that links what rustc built beside its own code (every cargo daemon) takes the
  checkout's roots from the store too, where only a build with nothing of rustc's did. A
  build spliced into rustc's executable still compiles them. ripgrep's `restart`
  (warmstart.py, dev-aws-7, three interleaved rounds) compiles 2,377 functions where it
  compiled 3,618 and takes 4.96 s where it took 5.09 s; nushell's compiles 25,843 where it
  compiled 30,039 and takes the same 25.7 s, as finding its roots in the store (0.8 s) costs
  what it saves while most are refused for calling rustc's copy of a function of the checkout
  ([#216](https://github.com/indexable-inc/blaze/issues/216)). `harness/storetracked.py` checks
  that a start found roots.
- The store is keyed by what ra-jit was built from, not by the bytes of its executable
  ([#105](https://github.com/indexable-inc/blaze/issues/105)): `crates/ra-jit/build.rs`
  hashes the workspace's crates, the vendored ones, the manifest, lock file, cargo config and
  `harness/toolchain`, the features, the target and whether debug assertions
  are on. Another build of the same sources, on another machine or after a commit that
  changes only harnesses or docs, finds what the first kept: `harness/storebuilds.py` builds
  its workspace into an empty store, then starts a copy of ra-jit with other bytes, which
  finds 66 entries and compiles 0 of the 701 the first added, where a build keyed by its bytes
  found 0 and compiled all 701. A ra-jit built outside the workspace keys by its bytes as
  before. A crate fetched by cargo is named by its place below `CARGO_HOME`, not by where
  `CARGO_HOME` is, so its frozen def map and functions are found from another home. A build
  script's run is keyed with its package's directory and the target directory apart, and
  what it printed and wrote is put back with this checkout's; the key still hashes the
  script's executable, which differs between target directories, so a second checkout still
  runs its scripts (none restored in warmstart's `warm` step on ripgrep or nushell) until a
  script is keyed by what it was built from.
- A daemon that starts on a store holding the checkout's code no longer compiles that code
  again before the first edit can be patched ([#105](https://github.com/indexable-inc/blaze/issues/105)).
  Each function of the checkout goes into the store with a mark of its code and of what it
  calls and reads, by stable names, so the patch base takes what it found as it is and an
  edit recompiles only what it touched; what an edit leaves no caller of (a closure renamed by
  a line above it, and what is generic over it) is left uncalled, as an instance is. nushell's
  `restart` (warmstart.py, dev-aws-5, `RA_JIT_RUSTC_BASE=0`) goes from 30.8 s to 21.4 s, the
  73,586 functions and 9.4 s of "compiling what the store had of the checkout" gone, and its
  `warm` step is ready in 15.4 s where it took 25.3 s; ripgrep's `restart` from 5.67 s to
  4.63 s. The first edit after is a patch as before (nushell 0.30 s, was 0.23 s; ripgrep
  0.08 s). A program linked with rustc's code, an LLVM build in units and
  `#[ra_jit::cache]` functions keep no marks and still take that build. `harness/storetracked.py`
  holds it.

- ra-jit is now **blaze**. The command is `cargo blaze` (`cargo blaze build`, `test`, `run`,
  `check`, `status`, `stop`); `cargo ra-jit` keeps working and, on a terminal, says once per run
  that it was renamed. The crate on crates.io is `cargo-blaze` (`cargo install cargo-blaze`),
  release archives are `blaze-v<version>-<target>.tar.gz`, and the flake's package and app are
  `blaze` (`ra-jit` stays as another name for them). Every setting can be given as `BLAZE_<NAME>`
  as well as `RA_JIT_<NAME>`, and the `BLAZE_` one wins. `--version` says `blaze`. The engine
  binary, the crates it is made of, the daemon's directory and the store keep their names, so an
  upgrade keeps what the store holds.
- `install.sh` installs blaze on macOS (Apple silicon) and Linux (x86-64, ARM64) with one line,
  `curl -fsSL https://raw.githubusercontent.com/indexable-inc/blaze/main/install.sh | sh`: the
  newest release (or `--version`), checked against its SHA256SUMS, into `~/.cargo/bin`, with Rust
  1.99.0 and `rust-src` through rustup. The README starts with it; the `install` workflow runs it
  on each platform.
- A build with a toolchain other than 1.99.0 says how to switch the project to it
  (`rustup override set 1.99.0`).
- A start on a warm store with nothing of rustc's (`RA_JIT_RUSTC_BASE=0`) is no longer held up
  by the store and the interner (#41): each store entry and set of types is read once however
  many threads ask for it, a value already interned is found under the shard's read lock, the
  walk of the types an instance mentions goes into each type once (a future's type reached the
  same few along exponentially many ways), and an edit does not wait for the store to have what
  the build before it kept. `harness/warmstart.py helix` on Linux x86-64 (dev-aws-7,
  c7i.16xlarge, 64 cores, one run each, main `c48ca3f800` against it with this change): a
  restarted daemon's build on a store that has everything takes 38.3 s instead of 106.5, its
  "store read" 130.8 s of thread time instead of 561.1, the first edit's rebuild 12.2 s instead
  of 45.5, and a second checkout is ready 59.0 s after its build instead of 113.0; nushell's
  takes 35.4 s instead of 37.8 ([research/warm-start.md](docs/research/warm-start.md)).
- `unused_mut` a source denies fails the build, with the seven deny lints before it (#84).
  `harness/lints_ui.py --deny unused_mut` scores it on every rustc UI test: 51 right, 0 wrong,
  5 missed against the nightly, 49, 0, 5 against 1.99.0. `RA_JIT_BORROWCK_REFUSE=0` turns it
  off with the borrow checker's errors. `fn f(mut self)` gets it too, where none was given.
- Each debug type is described once, by itself, and the types made of each other are kept
  together as one group that every set of debug types reaching them shares
  ([#29](https://github.com/indexable-inc/ra-jit/issues/29)); the DWARF is the same. On Linux
  x86-64 (16 cores, `memsplit.py`, one run each) the daemon after its own base and edits holds
  7.36 GB instead of 7.63 on nushell, 5.87 instead of 6.55 on helix and 14.47 instead of
  15.97 on bun, at most 7.9, 6.1 and 15.8 GB at an edit instead of 11.7, 9.8 and 22.9, and the
  first edit after the base takes 0.37 s instead of 2.06 on nushell, 0.19 instead of 1.71 on
  helix and 1.51 instead of 7.23 on bun. zed's own base, which passed 64 GB, finishes with no
  bound of its own at 62.6 GB in 662 s; at the default bound, a quarter of memory, it is still
  given up (#188).
- A whole program's compiles under LLVM import what they call, as ThinLTO's backends do,
  where each built its callees again from MIR and optimized them from scratch. Each function
  a compile calls is lowered and run through LLVM's pre-link pipeline once in the process,
  and a compile links in the bodies small enough to be built in (100 instructions, 300 for
  `#[inline]`, the whole budget of 40 000 for `#[inline(always)]`), and those of what they
  call with 0.7 of the room each call deeper, 8 deep, as `available_externally`; what LLVM
  does not build in is dropped. Bun on 16 cores (c8a.24xlarge) builds in 118 s where it took
  564 s, against 135 s for cargo's release build, and runs 1.244x cargo's where it ran 1.267x.
  `RA_JIT_IMPORT=0` builds the callees in as before.
- The daemon keeps what each debug type is told as once, rather than in every set of debug
  types it is in ([#55](https://github.com/indexable-inc/ra-jit/issues/55)); the DWARF it
  writes is the same. On Linux x86-64 (16 cores, `memsplit.py`, one run each) the daemon
  after its own base and edits holds 7.5 GB instead of 9.7 on nushell and 6.4 instead of 11.3
  on helix, and bun's own base, which was given up at a quarter of a 124 GiB host's memory,
  is built at 18.9 GB, so its signature edit is a 6.3 s patch instead of a 32.8 s full link.
  The own base asks the allocator to give back what it freed before it gives up for memory.
- ra-jit allocates with jemalloc on Linux and macOS, and with mimalloc on Windows only
  ([#39](https://github.com/indexable-inc/ra-jit/issues/39)). mimalloc 3 kept more of what a
  build freed, transparent huge pages included: on Linux x86-64 (16 cores, `bench.py`, two
  interleaved rounds) the daemon after its edits holds 1.57 GB instead of 2.03 on ripgrep, 9.52
  instead of 10.80 on nushell, 11.07 instead of 12.13 on helix and 24.3 instead of 27.0 on bun,
  with the same edit times. `RA_JIT_MEMORY` reports the live heap without the `heap-profile`
  feature, which now only adds jemalloc's profiler.
- The daemon no longer keeps a spare worker loaded for the next project
  ([#56](https://github.com/indexable-inc/ra-jit/issues/56)). It starts one as it starts, for
  the build that started it, and one each time a build of a new workspace begins, which loads
  the workspace beside cargo as before; none waits once a workspace is loaded. On Linux x86-64
  (16 cores, 3 runs), the daemon after the edits of `bench.py`: ripgrep 3.63 to 2.31 GB, fd
  3.42 to 2.04 GB, nushell 12.70 to 11.36 GB, with the same edit times; an idle daemon 2.64
  to 1.28 GB. A second project's cold start is as before (ripgrep 4.74 and 4.86 s, nushell 30.5
  and 30.7 s); a warm one, which cargo had built, loads the library first: ripgrep 1.29 to
  2.62 s, fd 2.19 to 3.48 s, nushell 8.62 to 9.51 s (`harness/spare.py`, 3 interleaved rounds).
  `RA_JIT_SPARE=ready` keeps one loaded as before; `RA_JIT_SPARE=0` still starts none.
- With LLVM, `memcmp` is called by its own name in the function's module rather than as one
  of the functions asked for, so LLVM writes a comparison of a few bytes it knows the number of
  as loads, and one only for equality as `bcmp`, as it does for rustc. On Linux x86-64 (AMD
  EPYC 9R45) tokei calls the C library's comparisons 155.9 million times instead of 278.6
  million (cargo's release build: 155.1 million) and runs at 1.34 times the release build
  instead of 1.51. words' machine code is the same, 48 bytes further on.
- With LLVM at `-O3` the bodies of the functions a function calls are built into it before
  the default pipeline, and the branches on what they return threaded and the values
  numbered again, as is done for what Cranelift builds in: a loop over `i + 1..n` inside one
  over `0..n` counted with `phi + 1` before, which alias analysis can't see past. On
  Graviton4 (Linux aarch64) nbody runs at 1.28 times cargo's release build instead of 1.97,
  and words at 1.27 instead of 1.36; on Linux x86-64 (c8a.metal) words at 1.28 instead of
  1.37. Whole-program builds take 1-4% longer. x86-64's nbody is still 1.27: LLVM 20 weighs
  the five-body nest above its full-unroll threshold, and rustc's LLVM 23 below it.
- On macOS a patch whose unwind frames are larger than the program's unwind table is spliced
  rather than linked in full: both of the unwinder's tables move into the room the link leaves
  after its load commands, the program's frames where the table has them and the patch's
  behind. ripgrep's first edit (244 KB of frames, a table of 218 KB) is a patch there too
  (`harness/rustcbase.py`'s `moved-panic` and `cached-reached`).

- After a fresh build through cargo the daemon is ready for edits once it has loaded the
  workspace and read the symbols of the executable rustc linked, instead of after building
  every program once more itself ([#11](https://github.com/indexable-inc/ra-jit/issues/11)):
  the first edits are patches spliced into rustc's executable, and the daemon builds its own
  base in the background once nothing has waited for a second. On Linux x86-64 (16 cores),
  ready after a cold build from an empty store: just 10.4 to 8.9 s, ripgrep 7.8 to 6.6 s,
  nushell 58.8 to 47.0 s, against 7.4, 5.0 and 35.1 s for plain `cargo build`; from a filled
  store 10.7 to 8.7 s, 8.2 to 6.6 s and 64.1 to 46.3 s. That is 0.25 to 0.5 s after the
  command returns on just and ripgrep, and 3 to 4 s on nushell, which reads the symbols of
  233 000 functions. On macOS, nushell 43 to 36 s against 25 s for cargo. The command itself
  is as slow as before: the spare worker that loads the workspace beside cargo still costs
  1.0 s on just, 1.3 s on ripgrep and 8 to 9 s on nushell.
  The first edit over rustc's executable is slower than over the daemon's own build, since
  it reads the edited file's functions and resolves its names for the first time: just 0.08
  to 0.4 s, nushell 0.3–0.45 to 1.6–1.9 s. An edit that is more than function bodies, of a
  generic, `#[inline]` or default trait function or one that moves such a function's lines
  (rustc copies those into their callers, line numbers and all), of a static, or of a file
  whose functions were not read before is built in full, and the log says why
  (`no patch: ...`): ripgrep's `main` sits above a generic function, so its first edit takes
  1.4 to 1.7 s.
  The own base is done some 2.5 s after ready on just and ripgrep and 11 s on nushell.
  `RA_JIT_RUSTC_BASE=0` keeps the old behaviour. On macOS rustc's link of a program is given
  `-Wl,-headerpad,0x400000` and `-Wl,-bind_at_load`, which the daemon's own links leave out.
  `harness/rustcbase.py` checks every kind of first edit against cargo, and
  `harness/coldprof.py --max-wait S` fails when the daemon is ready more than S seconds after
  the command.
- `cargo ra-jit build` from cold is closer to cargo's, and readiness follows it sooner
  ([#11](https://github.com/indexable-inc/ra-jit/issues/11)). The daemon tells the spare
  worker what cargo built from a thread of its own and the spare reads its input from the
  start: before, a spare still loading the standard library let the pipe fill, which held up
  the daemon and with it cargo, whose output went unread. While cargo builds, the spare
  resolves the standard library and compiles the functions of each crate cargo is done with on
  a quarter of the cores, leaving the generic instances they reach to the edits, reads the
  symbols of each library as cargo writes it, and once cargo is done compiles nothing more
  before it is ready. The first edit reads the edited file's functions on every core. An edit
  of a generic, `#[inline]` or default trait function, or one that moves such a function's
  lines, is now a patch too when every copy rustc made of it is found in the executable: the
  patch replaces the copies, and ripgrep's first edit is a patch. Thread-locals of a patch
  over rustc's executable are reached as rustc's code reaches them (on Linux x86-64 the
  general-dynamic sequence becomes local-exec, on macOS through the variable's descriptor).
  On Linux x86-64 (16 vCPUs, 8 cores), interleaved runs from an empty store, command and
  ready: just 8.5 to 8.2 s and 9.0 to 8.6 s, ripgrep 6.6 to 5.7 s and 6.9 to 5.9 s, nushell
  43.5 to 37.6 s and 46.7 to 39.2 s, against 7.6, 5.4 and 35.7 s for plain `cargo build`. The
  first edit: just 0.36 to 0.5 s, ripgrep 1.7 s built in full to 1.5 s patched, nushell 2.0 s
  as before. The command's remaining cost is the spare's own: what it resolves of the
  standard library and the workspace takes cores from cargo (docs/cargo.md).
- An edit in a package whose build script cargo runs again for it (one that names no
  `rerun-if-changed`, as ripgrep's, fd's and hyperfine's) no longer waits for a
  `cargo check` that runs every build script of the workspace: the daemon runs that one
  script again as cargo ran it and compares what it prints and writes with what it loaded,
  as `cargo ra-jit` already did, and asks cargo only when the two differ, the script is older
  than its source, or it was compiled reading a variable cargo did not set. A script
  `cargo ra-jit` ran again for the same build, with nothing it reads written since, is not
  run a second time. On Linux x86-64 an edit of ripgrep's `search` goes from 72 to 48 ms,
  fd's `run` from 78 to 42 ms and hyperfine's `run` from 80 to 35 ms.
- ra-jit is built one way, whether downloaded or built by nix: the new `dist` profile (thin
  LTO), optimized with a profile of ra-jit's own work (PGO) trained on edits of fd, bat, just
  and hyperfine. `.github/scripts/build_release.py` is the recipe; the release workflow runs it
  for every target and the flake's `daemon` (so `nix run`, `nix profile install` and the demo)
  runs it inside its derivation. The profiles are trained by the new `pgo` workflow, which
  runs again whenever Cargo.lock, a manifest or the toolchain changes, and pinned by sha256 in
  `pgo/profiles.json`, so every build uses the same file; the build says how much of the code
  changed since its profile was trained, and a release refuses a profile of other crates.
  `cargo install --locked --profile dist ra-jit` builds the same profile without PGO, which
  starts about 10% and edits about 3% slower than a release's binaries: cargo takes no compiler
  flags from a crate, and crates.io's crates are not the ones a profile can be trained on.
  Against the plain `release` build, on Linux: the
  cold start 14% shorter on nushell and on ripgrep, an edit of a function 8% shorter on
  nushell, the same peak memory. mimalloc v3 stays the allocator: jemalloc, mimalloc v2 and
  snmalloc start colder and edit no faster (an edit spends 1.8% of its time allocating, 73%
  revalidating salsa's memos). `release` itself is unchanged and stays the build to work with,
  so rebuilding ra-jit after a change takes as long as before
  ([docs/reports/RELEASE-BUILD.md](docs/reports/RELEASE-BUILD.md)).
- Under `RA_JIT_BACKEND=llvm`, a body of more than 10 000 Cranelift instructions is compiled
  by Cranelift. Since LLVM lowers a handler that takes more than the exception (tiers), it
  compiled every function, and an edit of nushell's `main` (35 778 instructions) waited
  229 ms for LLVM: on Linux x86-64 the edit went from 343 ms to 472 ms, and is 280 ms now.
  65 of nushell's 160 177 functions are over the limit, none of ripgrep's or of
  `demo/lifebench`'s. A tier still compiles such a body with LLVM.
- `vendor/rust-analyzer` and `vendor/rustc_abi` are git subtrees instead of submodules: a
  plain `git clone` builds, and the flake builds from its own tree with no second pin of the
  forks.
- The first build of a workspace through cargo has the daemon load it and compile ahead
  while cargo builds ([#11](https://github.com/indexable-inc/ra-jit/issues/11)): a spare
  worker reads the workspace and compiles what it can while cargo runs, hears what the
  build scripts said and where the proc macros are, and is the worker the build is handed
  to. On nushell, Linux (32 cores): ready for edits 58 s after a cold build from an empty
  store instead of 71 s; cargo's own build takes about 3 s longer, 40 s instead of 37 s,
  while the worker loads beside it.
- A build through cargo asks the toolchain's `rustc` beside `cargo` for its version and
  sysroot, not the rustup proxy: 16.6 ms to 7.3 ms each of its three calls on Linux.
- An edit to function bodies only lists no functions again and finds the program's tests
  as they were: 5.5 ms to 0.5 ms of an edit of nushell's `main`.
- `RA_JIT_STATS` shows where a splice and the refusals before a link spend their time
  (`splice-us`, `link-us`, `refuse-us`).
- `harness/realbench.py --pause S` waits once the daemon is ready, as a person would before
  the first edit.
- `vendor/subtree.py` replaces `vendor/subtree.sh`. `push` sends what main changed under
  `vendor/` to the fork, one fork commit per commit of main, and only fast-forwards the fork;
  the `fork sync` workflow runs it on every push to main, so the forks no longer fall behind
  (rust-analyzer's had: "warm edits" never reached it). `upstream` merges an upstream
  rust-analyzer release into `vendor/rust-analyzer` in one command, with conflicts left in
  the tree as `git merge` leaves them; before, that took a clone of the fork and a pull.

- Code ra-jit gives LLVM is faster: SIMD intrinsics on 128-bit lanes of integers (add,
  subtract, bitwise, saturating, compare, `simd_bitmask`) are vector instructions instead of a
  loop over lanes, and LLVM's own; a slot whose address is rounded up to an alignment above 16
  (`__m256i`) is an alloca of that alignment, which LLVM can take apart; `memcpy`, `memmove`
  and `memset` are LLVM's intrinsics rather than calls it knows nothing of, so a copy of a
  known size is a few loads and stores.
- A program built whole runs faster with LLVM: a landing pad whose cleanup only resumes is
  left out, so LLVM sees a call that cannot unwind into anything; what a check that is off
  (`ub_checks()` and its kind) guards is not compiled, so its panics do not use up what may be
  built in; a `match` is a chain of
  comparisons of up to 256 that LLVM makes a switch of; bodies are built in and shown up to
  16 calls deep (an iterator's adapters reach `Vec::push` that deep); `offset` is a
  `getelementptr inbounds`, also once Cranelift has inlined it; and a body built to be
  measured and then not used gives back what its build counted of its call's share, so that
  `vec![false; n]`'s `from_elem` is shown to LLVM, which then knows the vector's length and
  drops the sieve kernel's bounds checks. A body shown to LLVM carries its `#[inline]`,
  `#[inline(always)]` or `#[inline(never)]` as rustc emits it. An address the code computed
  as an integer (from `ptrtoint`s, through adds, phis and selects) is rebuilt after the passes
  as pointers and `getelementptr`s on the pointers it came from, so that LLVM can follow what
  is stored and loaded through it: nbody's array of deltas leaves the stack. On Linux x86-64 the geometric mean at `-O3` goes
  from 1.20x (ra-jit f3eeb5295e) to 1.15x of release's time at 13ee5a5339.
- On macOS arm64, ripgrep, fd, bat and jsonrt build whole: NEON's pairwise maximum, minimum
  and add, its table lookups (`tbl1`, `tbx1`), `sqdmulh`, `uaddlp` and `saddlp`, the CRC-32
  instructions, and the generic `simd_reduce_max`, `simd_reduce_min` and `simd_ctpop` are
  compiled, lane by lane or as one instruction. On a MacBook Pro (Apple M5 Max, macOS 27.0.1),
  median of three runs: ripgrep 1.15x, fd 1.01x, bat 1.16x and jsonrt 1.26x of release's
  time, every output the same as release's.
- x86-64's `simd_select_bitmask` (AVX-512's masked blends), `simd_fma`, the AVX-512 two-table
  permutes (`vpermi2var` of bytes, words, dwords and qwords), SSE4.1's `phminposuw` and
  `vzeroupper` are compiled, lane by lane, which nushell's dependencies reach for
  (corpus `simd_x86`).
- AArch64's SHA-1, SHA-256, SHA-512 and AES instructions (`vsha256hq_u32`, `vsha512hq_u64`,
  `vaeseq_u8` and the rest, as sha2 calls them) are compiled, each as a function of the one
  instruction, and NEON's sums of every lane made wider (`vaddlvq_u8`) and doubled products
  made wider (`vqdmull_s16`) lane by lane. `simd_reduce_add_ordered` and `_unordered` (and
  `mul`) are compiled, which `vaddv_u8` and AVX-512's `_mm512_reduce_add_epi32` are, and
  `simd_masked_load` and `simd_masked_store` (AVX-512's `_mm512_mask_loadu_epi8`, in fff_search
  under nushell): a lane the mask leaves out is not read or written (corpus `crypto_lanes`).
- A daemon started on a filled store finds the functions it has stored faster: looking an
  entry up takes a lock of its own shard rather than one for all of them, and the types of an
  entry's variables are read when the function is described for a debugger, on the threads
  that describe, rather than when the entry is looked up. nushell's restart on dev-aws-14
  (64 cores, shared), five rounds each, interleaved with main's: 15.6 s (15.5 to 15.7) on main, 14.7 s (14.5 to 14.7)
  with this; the stored roots take 1.3 s, from 3.5 s, and the object 1.8 s, from 1.0 s.
- The def maps and impls of crates that use procedural macros, and those of the crates of
  procedural macros, are frozen in the store like any other dependency's, so a build reads
  them rather than resolving their names again. A macro whose library is not loaded, or is
  turned off, is so only in that process, and its crate is not frozen. nushell's restart
  resolves all 434 of its dependencies' def maps from the store, in 1.8 s rather than 3.1 s,
  and takes 12.6 s (12.5 to 12.7) with the entry above, against main's 15.6 s.

### Fixed

- A foreign function or static whose `#[link_name]` starts with `\u{1}` links: the mark names
  the symbol exactly, as rustc reads it, and bindgen writes it where a symbol differs from its
  Rust name (`aws-lc-sys`, so anything on `rustls` with its default provider). blaze passed the name
  to the object writer as written, which put Mach-O's `_` before the mark, so the link failed
  with `"_\u{1}_strlen"` undefined (shown as `__strlen`); on ELF the mark stayed in the name.
  `harness/breakers` has the case (`link_name_exact`).
- The borrow checker: lifetime elision counts none of the impl's lifetimes in a parameter
  that writes `Self`, as rustc counts only what a parameter's type writes or elides, so
  `fn store(self, db: &dyn Db) -> &Store` in `impl Id<'_>` returns a borrow of `db`. It
  gave "lifetime may not live long enough" there (`InferBodyId::store_and_root_expr`, which
  failed blaze's own build of `hir-ty`), and at a call took the result as borrowing nothing.
  blaze's own tree: 10 false errors to 2.
- The borrow checker's implied bounds: a trait object's `'r` outlives what a supertrait
  requires of `Self` (`dyn 'db + SourceDatabase` that `'db: 'static`, through salsa's
  `ZalsaDatabase: Any`), and the types in its arguments imply what they do
  (`&dyn Fn() -> &'a G<'db>` that `'db: 'a`), as rustc's well-formedness walks into them.
  The last 2 false errors in blaze's own tree (`module_for_assoc_item_loc`, `path_to_const`).
- The LLVM backend: an error LLVM diagnoses while it writes an object is an error naming
  what LLVM said, not an object with an instruction left out. LLVM 23's assembler printed
  "can't encode 'ah'" for `mov ah, sil` in an `asm!` block and wrote the object without it
  (LLVM 20 ended the process). A `reg_byte` in a block that names a high byte register is one
  of a, b, c and d, which always encodes (corpus case `inline_asm::high_of`).
- The LLVM backend's store: a function's key has the `asm!` blocks LLVM puts in place of its
  calls of their wrappers. Two functions that each pass a `u64` through a block of their own
  were one key, and the second got the first's code.

- On macOS the doctests are ra-jit's, as on Linux
  ([#255](https://github.com/indexable-inc/blaze/issues/255)). Every example was left to rustc
  for a writable static of std (`CLEANUP`, `GLOBAL_PANIC_COUNT`) "without a rustc symbol": the
  standard library's unit was weakened like every object ra-jit writes, which makes each name
  of Rust's a private external that `defined_symbols` does not count as defined, so the example
  found nothing of the unit, compiled std's startup itself and stopped at its first writable
  static. The unit is linked with nothing of rustc's and gives way to nothing, so it is written
  as it is; one an older ra-jit kept in the store is not taken. `harness/doctestonce.py` no
  longer accepts that reason on macOS.

- A guarded match nested inside another match arm's guard preserves the enclosing guard's
  binding references, including with or-patterns. Later reads in the outer guard no longer
  use uninitialized by-value bindings, which could produce wrong results or a crash.
- A whole-program build gives the same object every time ([#200](https://github.com/indexable-inc/blaze/issues/200)).
  Functions went into the object in the order the compile-ahead threads finished them, so
  their names, the ids of what they call and their places in the program changed from one
  build of the same source to the next; the layout alone moved a benchmark by 8%. The object
  is now given roots and instances in the order they were asked for, the thread that puts
  them in it waiting for the next in line while the others compile on, and the threads take
  instances in that order too. `harness/wholeprogram.py` builds each case twice and holds the
  objects to the same bytes.
- `cargo blaze doc` documents the crates cargo does: of two of one name the one cargo keeps
  (none of a proc macro's two `syn`s), nothing only a left-out one or a test needs. It links
  a crate not documented at its `html_root_url`, finds blanket impls of every crate linked and
  applies them as solved in the documented crate, leaves out more provided methods that
  cannot apply, warns of a broken link where rustdoc does (lint levels, `cfg_attr`, no
  warning in another crate's macro), and documents what a macro expands to past
  rust-analyzer's token limit (biome_wasm's 2271 types). Against `cargo doc`, biome goes from
  93.5% to 99.0% of the pages, helix from 98.2% to 99.0% of the members and impls; docs/doc.md
  has all five bench apps.
- The borrow checker reports nothing on helix (14 false errors before) or biome (534), as
  rustc does; ripgrep, fd and nushell stay at none, and the UI scores are unchanged (Polonius
  1545 of 1585, NLL 1542 of 1583). It follows rustc where it did not: the outlives
  components of a trait object's projections and of fn pointers; an opaque's arguments
  related by what it captures; `use<>` on an `impl Trait` inside the return type; implied
  bounds through a projection and from a type's inferred outlives; a trait object's default
  lifetime not counted by lifetime elision; one lifetime for an alias's elided lifetime
  wherever the alias uses it; a closure's annotated references and the pointer fn items
  coerce to as higher-ranked; promotion by the value, not its type, with `Freeze` seen
  through projections.

- A macro call in parentheses or brackets with no `;` after it has to expand to one
  expression, as rustc parses it ([#154](https://github.com/indexable-inc/blaze/issues/154)):
  `twice!(twice!(n += 1;));`, whose outer call expands to two such calls with nothing between,
  built and ran where rustc reports an error. A function with such a call whose expansion is
  statements is now refused with the reason, as one with a call that failed to expand is.

- `cargo install --git https://github.com/indexable-inc/blaze ra-jit` builds ra-jit
  ([#242](https://github.com/indexable-inc/blaze/issues/242), #293, by @gakonst). It failed
  before compiling anything: for a git source cargo checks that every package is a member of
  the workspace above it, and the root claimed `vendor/rust-analyzer`'s crates and rowan, which
  are members of the fork's own workspace. The root now excludes `vendor/rust-analyzer`, so the
  fork's crates are tested there (in `vendor/rust-analyzer`, as
  `harness/unittests.py` does), and that workspace patches salsa to `vendor/salsa*` as the root
  does.

- A local that no place of its body mentions has no stack slot. rust-analyzer's MIR gives a
  closure's body a local for each capture and for each binding of the function it is written
  in, and a closure called by reference never touches them; each had a slot, so a closure
  over a large value had room for it twice in every frame. rgb's audio callback captures a
  synthesizer of 120 KB, and cpal's two closures around it each reserved 240 KB, overflowing
  CoreAudio's 512 KB IO thread; they now reserve what rustc's do. Breaker `captured_frames`.

- An integration test or benchmark the daemon runs has `CARGO_BIN_EXE_<name>` of each program
  of its package set, as cargo sets it since rust-lang/cargo#16421. With cargo's second layout
  the test is no longer beside the programs, so hyperfine's and bat's tests (assert_cmd's
  `cargo_bin`) found none and every one failed under ra-jit.

- The `TypeId` of a type that holds a closure, `async` block or `async fn` future of a crate
  rustc compiled is rustc's: its `DefPath` (`{closure#n}` under the function, as its symbols
  are named) with all of its arguments, the kind, signature or resume type and captures
  included. It was refused, as was every function that asks for one: tonic's
  `Streaming::new` builds its body from `body.map_frame(|frame| ..)`, and axum-core's
  `try_downcast` asks for its `TypeId`. An `async` closure's future and an `async gen` one
  are still refused.

- The `TypeId` of a type that holds a constructor used as a function (`stream.map(Ok)`,
  `iter.map(Some)`, a tuple struct's name) is hashed as rustc hashes it, as the `Ctor` child
  of the variant or struct with the ADT's parameters
  ([#369](https://github.com/indexable-inc/blaze/issues/369)). It was refused, and with it
  every edit that rebuilt a function giving `axum::body::Body::from_stream` such a stream,
  and tonic's clients, which map requests through `Ok`.

- On x86-64, an edit that replaces a function shorter than a jump (5 bytes) is spliced
  instead of built in full ([#242](https://github.com/indexable-inc/blaze/issues/242)).
  The jump goes over the `int3`s and `nop`s the linker padded the function with. When there
  is no padding, as for a getter packed against the next function, the function gets a
  2-byte `jmp` to a run of `int3`s within 127 bytes, which jumps on. biome's leaf edit
  (`JsFormatOptions::quote_style`, whose patch also replaces `line_ending` and
  `trailing_commas`, 4 bytes each) goes from 360.7 s and a 26.4 GB peak to 0.50 s (dev-aws-5,
  16 cores, cranelift, 1 run; cargo 12.1 s). `harness/rustcbase.py` `short` and `hop` hold it.
- A function a build left to rustc, and an instance of a function generic over nothing that
  did not compile, keep rustc's copy when no edit since the daemon started reached its crate
  or the crates below it. Each was taken for stale when a build in full left it to rustc for
  the first time, and the build went to cargo ([#242](https://github.com/indexable-inc/blaze/issues/242)).
  With the jumps above, biome's generic edit (`format_array_node`) is the daemon's (204.5 s;
  it went to cargo after 190.9 s, refused on `biome_analyze::profiling::is_enabled`) and its
  struct edit is spliced in 0.51 s, where it went to cargo after 176.7 s (dev-aws-5, 16 cores,
  cranelift, 1 run).
- A patch asks rustc's executable whether it has a function of a name once per name, where
  each asking searched every symbol of it. biome's body edits after the first take 1.6–1.7 s,
  where they took 2.2–2.8 s (cargo 2.3 s).
- On Windows x86-64 (MSVC), `cargo blaze build` and `run` rebuild through the daemon and print
  `[ra-jit]` instead of handing every build to cargo. The daemon takes COFF programs (also with
  `--target x86_64-pc-windows-msvc`), links them with the `rust-lld` the repository's cargo
  configuration names, and closes an existing directory of its own that others can enter, as
  on Unix. Live edits of a running program are still macOS and Linux only.
- A profile with `debug = "line-tables-only"` (or `1`, or `"limited"`) no longer has the type
  of every variable worked out. Only the lines were written, but the types were described for
  each function compiled and then thrown away. On Biome (`biome_cli`, 518 crates, whose dev
  profile is `line-tables-only`), 0.1.0's worker went past 44 GB in its cold build and was
  killed before it was ready. On main, the cold build's peak goes from 40.4 GB to 30.6 GB and
  its time from 139.5 s to 98.8 s, with 726,516 `told_type` memos down to none.
  `harness/debuginfo.py`'s cost part checks that no type is told of at `line-tables-only`. What
  such a build keeps in the store, which has no variables, is kept apart from what a build with
  them keeps, so a `debug = true` build later takes none of it; the cost part builds at
  `line-tables-only` and then at `true` from one store and checks that it has every variable
  one at `true` after another at `true` has.
- A registry crate with a build script has the same frozen def map in every checkout (#105).
  Its key held the absolute `OUT_DIR`, so another checkout of nushell took 221 of 478 crates
  frozen where a restart of the first took 374, and of ripgrep 41 of 51; both now take all of
  them (374 and 51). The key holds `OUT_DIR` from cargo's build directory on and the files
  written there, and the bytes name a file below the `OUT_DIR` of any crate of the closure,
  and a literal such as the path `include!(concat!(env!("OUT_DIR"), ..))` reads, from that
  `OUT_DIR`, so another checkout reads what its own build scripts wrote; bytes that still name
  a target directory are not written. `frozenwarm.py` builds a second checkout with such a
  crate, the first's target directory put away.
- The first start of a daemon after a cold `cargo ra-jit build` finds what that build stored
  (#41). The two named the toolchain differently (the sysroot's libstd alone, or rustc's link
  line with libcore and liballoc too) and the store hashes that name into every key: on
  nushell the restart found 0 functions, took no frozen def map and expanded every proc macro
  again. It now finds 103,750 functions, 374 frozen crates and 1,586 expansions, and its cold
  compile is 11.9 s where it was 17.9 s. `warmstart.py` fails when the restart takes nothing.
  docs/cargo.md no longer says the store is off unless `RA_JIT_CACHE` is set.
- A daemon that stops lets its workers finish writing to the store what their last build kept,
  a minute at most each, where it killed them (#41). A stop just after an edit lost it: the
  next checkout of helix compiled 131,609 functions instead of 115,706.
- helix builds again: since #212 the borrow checker said "higher-ranked lifetime error" of
  `v.sort_by(match .. { a => |x: &T, y: &T| .., b => |x: &T, y: &T| .. })` and of
  `match .. { a => render_mode, b => render_spinner }` returned as an `impl Fn(&mut ..)`.
  rust-analyzer gives the lifetimes a signature elides as errors, where rustc makes each
  late-bound, so the fn pointer such a `match` makes of fn items or closures had no
  `for<'a>`; the placeholders of the bound were then held to those lifetimes. The checker
  now takes them for the lifetimes rustc would bind. The corpus's `higher_ranked` case does
  both (three errors before), and ide-diagnostics has a test.
- The borrow checker no longer panics on rustc's UI tests (`lints_ui.py --deny unused_mut`
  over all 20,450 files: 4 panics before, none now, and ra-jit's time from 67 s to 14 s). The
  causes were in the trait solver and the lowering it reads: a trait object's projections
  that become the same after substitution were deduplicated before being related, so
  `dyn A<X = T> + B<X = T>` failed to relate to itself (rustc relates them all, #136458); the
  `for<'b>` of an associated type's bound in a `for<'a>` was lowered as a binder of its own
  instead of concatenated onto the outer one, as rustc does; and upcasting a higher-ranked
  trait object built its supertraits outside the source's binder. Each has a regression test
  in ide-diagnostics, which panicked as rustc's test did before the fix.
- A 5,204-arm `else if` chain (rustc's `expr-stack-overflow.rs`, #206) is borrow checked in
  4.5 s, where it took over 300 s and then overflowed the stack: finding each expression's
  syntax walked down from the body's root, quadratic in the chain, and lowering, inference
  and MIR building recursed once an arm. The walk now goes to the child that contains the
  range, and the recursions grow their stack as rustc's `ensure_sufficient_stack` does
  (`stdx::ensure_sufficient_stack`). 1,000 arms take 2.2 s (5.35 s before), 2,000 arms 2.5 s
  (26 s); ide-diagnostics tests the 5,204-arm chain under 60 s.
- A destructuring assignment that moves out of a reference (`S { b, .. } = *r`) is an error
  at `*r`, as rustc reports it, and one that copies is not an error.
- `&[]` and `&mut []` are `'static` whatever their element type, as rustc promotes them: the
  borrow checker asked the type, `[T; 0]`, whether it needs dropping or is mutable inside, and
  reported `fn f<T>() -> &'static [T] { &[] }` as returning a reference to a local.
- ide-diagnostics's 24 failing tests pass (761 pass, 1 ignored, slow tests included), and CI's
  fast tier runs them in the fork job. Its `minicore_smoke_test`, which runs only under `CI` or
  `RUN_SLOW_TESTS`, found the `&[]` above and minicore's `ptr::read`, which moved out of a raw
  pointer as rustc refuses to and now calls `read_via_copy` as core does.
- The first build after one of cargo's is the daemon's where a proc macro or build script is
  built on what a build script wrote to `OUT_DIR`, as biome's macros are on
  `biome_aria_metadata`'s ARIA tables: the file was written while cargo built, after the build
  began, so it counted as changed since, and the build went to cargo ("what runs during the
  build: …/out/roles_and_attributes.rs changed"). Cargo's build replaced the worker, which built
  the program in full again: biome's next edit after that took 130 s and 24 GB. Such a file is
  only written again when the script runs again, which the script's own guards are for.
- The first build after one of cargo's is the daemon's where a build script makes a source of
  its crate newer, as biome's analyzer crates touch `src/lint/nursery.rs` for cargo to build
  them again when a rule is added: the build still went to cargo ("what runs during the build:
  …/biome_yaml_analyze/src/lint/nursery.rs changed") and its worker built the program in full
  again, 232 s at a 27 GB peak. A source that runs during the build is now changed when it is
  newer than cargo's last run of rustc on it, as cargo has it, not than the build's start.
  Every edit of memsplit.py's biome is now the daemon's, the first body edit 0.7 s and the
  ones after it 0.4 to 0.5 s (16 cores). soakrows.py's `proc macro its script touches` does it.
  soakrows.py's `proc macro on generated code` has the proc macro `include!` its script's
  output and checks that the first edit after the build is the daemon's.
- On Linux the daemon's bounds on memory count what its control group allows, where they
  counted the machine's: the workers' `RA_JIT_MEMORY_MAX` (half) and the own base's
  `RA_JIT_OWN_BASE_MEMORY` (a quarter) were of the host's memory in a container, a CI job or a
  systemd unit that allows less, so the kernel killed the worker before either was reached. It
  is the least of `MemTotal` and the `memory.max` (version 2) or `memory.limit_in_bytes`
  (version 1) of the process's group and those above it. soakrows.py's `control group's
  memory` starts the daemon in a 6 GB scope and checks that its workers hold at most 3072 MB;
  it said 96,021 MB on a 187 GB host before.
- A function the daemon compiles ahead, while it reads the symbols rustc built, is given up on
  once they are read, where the worker used to finish it before it was ready. bevy's second
  warm-store run began its derived `release_state` of a read-only query struct, which the
  trait solver takes about two minutes over, and was ready 148 s after the build in place of
  26 s; it is now ready in 26.5 s. A worker's own cancellation was not enough: salsa ignores it
  inside a query that recovers from cycles, as inference and MIR building do, so the threads are
  stopped as a write stops them, and the trait solver checks for that at each goal. A bin over
  bevy_ecs with that struct stopped compiling ahead 9 to 20 s after its symbols were read, and now
  within a second. `harness/aheadready.py` holds each function compiled ahead for ten minutes
  and checks that the daemon is ready within two.
- An expression of type `()` that computes nothing (an assignment, a block without a tail, a
  `break` or `return` without a value, an `if` without an `else`) gives its place the `()`, as
  rustc's MIR does: `let z: () = x = 1; let w = z;` was E0381, a build failed on code rustc
  accepts. A comment-only `asm!` writes its `inout` registers back, so the variable is
  mutated.
- A daemon's link that fails on names nothing defines, used only by code rustc's own link
  takes out, is run again as rustc's line has it, with `--gc-sections` (or `-dead_strip`,
  `/OPT:REF`). The daemon leaves those out so a patch can call what the executable's code no
  longer reaches, and zed's webrtc then needs X11's `libXtst`, which nothing links: its own
  base failed to link after 662 s ([#29](https://github.com/indexable-inc/ra-jit/issues/29)).
  An executable linked so takes edits that call into what it left out as builds in full.
- The borrow checker holds a struct or enum to what its fields need of its lifetimes,
  rustc's inferred outlives bounds, where it is named or built: `B(x)` with
  `struct B<'a, 'b>(&'b mut F<'a, 'b>)` needs `'a: 'b`. A closure passed for a bound with
  `for<'r>`, `impl FnOnce(&mut T)`, takes the bound's other lifetimes, so what it needs of
  them reaches its creator. A function item's return type is well-formed but for what names
  a late-bound lifetime, as rustc's: `foo` returning `(&'c (), PhantomData<&'b &'a ()>)`
  needs `'a: 'b` where it is a method's receiver. A trait object's associated type is held
  to the bounds its trait declares for it: `dyn Trait<Assoc = &'a ()>` with
  `type Assoc: 'static` needs `'a: 'static`. 1523 of 1580 of rustc's tests now (was 1520).
- A match arm with a guard binds what it binds by value once the guard holds, as rustc does,
  so `(mut long, ref short) if g => long.0 = &short` is accepted: `short` is declared first
  and dropped last. In a `const` or `static` a mutable borrow is promoted only of `[]` and a
  `const fn` call only where the item certainly runs it, so `helper(&mut 42)` gives E0716, as
  does `&(ALL[i] + 1)` in a function. An `impl Trait` inside the return type,
  `(impl Iterator, &'b str)`, gets the member constraints a returned one does, and the
  lifetime chosen for its hidden type is applied, so what that needs of a type parameter is
  checked (E0309). `borrowck_ui.py` reads each unit with its revision's `--cfg`, so
  `#[cfg($rev)]` in a macro is set. 1532 of 1581 of rustc's tests now (was 1523).
- Assigning `x` in `x[1][{ x = y; 2 }]`, where `x[1]` indexes a slice, is rustc's E0510: the
  bounds check already done must hold. A returned
  `impl IntoIterator<Item = impl Cap<'a> + Cap<'b>>` gets its member constraints, so a
  lifetime it does not capture gives E0700 with early-bound lifetimes as with late-bound
  ones. A closure assigning `bx.0` of a `Box` it captured from a `bx` declared without `mut`
  is rustc's E0594, which names the capture by the place it captured, `*bx.0`, as ra-jit now
  does. 1535 of 1581 of rustc's tests now (was 1532).
- A returned `impl Trait` holds the borrows of the lifetimes elided in its function's
  parameters, so mutating `self` while the iterator of
  `fn evens(&self) -> impl Iterator<Item = &i32> + '_` is live gives rustc's E0502; it was
  missed. Edition 2024 captures every elided lifetime, earlier editions the one `'_` or an
  elided lifetime in the bounds stands for (the lifetime of `&self`, else the parameters'
  only one), and `use<'_>` that one too. Type lowering in rust-analyzer gives the
  `impl Trait` a lifetime parameter for each, as rustc does (corpus `elided_captures`).
  1536 of 1581 of rustc's tests now (was 1535).
- The borrow checker proves `for<'a>` bounds for placeholders, as rustc does, and those an
  impl's where clauses need on the way: `&'static str: DeserializeOwned`, which needs
  `&'static str: for<'de> Deserialize<'de>` where only `'de: 'static` is implemented, gives
  rustc's "higher-ranked lifetime error", as does a forwarding impl that needs
  `T: for<'a> Bar<&'a isize>` of a `T` that is only `Bar<&'b isize>`. They were missed
  (corpus `higher_ranked`). 1539 of 1581 of rustc's tests now (was 1536).
- The borrow checker checks the constants of a signature: an array length or a const
  generic's default written as a block, `[u8; { .. }]` or `struct X<const N: usize = { .. }>`,
  which no item's body holds. `const N: usize = { let s: String; s.len() }` is rustc's E0381,
  and a move out of a reference in an array length its E0507; both were missed. 1541 of 1581
  of rustc's tests now (was 1539).
- A returned `impl Trait` hiding a type parameter is checked when the function's lifetimes
  are late-bound or elided: `fn f<T>(d: T, p: &mut ()) -> impl Sized + '_ { (d, p) }` gives
  rustc's E0311, and with `p: &'b mut ()` its E0309. The borrow checker relates the hidden
  type to an elided `+ '_` bound, so `dest: &mut T` returned behind `impl FnOnce() + '_ + 'a`
  gives E0621. 1520 of 1580 of rustc's tests now (was 1517).
- The borrow checker reports what normalizing a signature's associated type needs of its
  lifetimes (`-> <&'a () as Visitor<'d>>::Value` proven by `impl<'a, 'd: 'a>`), and relates
  an associated type through a where clause's projection (`T: Fun<F<'a> = [u8]>` borrowing for
  `'a`); item checking reports what the impl of an associated type in a field needs
  (`<T as Dummy<'a>>::Out` that `T: 'a`). 1517 of 1580 of rustc's tests now (was 1511).
- A body imported through another was called with the importer's type for it: an export
  declares what it calls with the types it calls it through, and one has as an integer what
  another has as a pointer. LLVM takes a call of a body through a type it does not have to be
  undefined, and of `ptr::write::<ForOf>` imported through a function that had the address as
  an integer it kept nothing of what was written, so bun's parser crashed. Such a call is now
  made through the callee's own type, the arguments converted (a unit test of `llvm.rs`).
- On Linux AArch64, an `asm!` of a feature LLVM's assembler has no `.arch_extension` for was
  refused: constant_time_eq's `mrs {}, dit` under `#[target_feature(enable = "dit")]`, which
  failed bun's build there. Such a feature (`dit`, `bti`, `jsconv` and the others of a later
  Armv8 or Armv9, and `v8.4a` and the like themselves) now starts the block with `.arch` of
  the version that has it.
- `runbench.py` runs cargo's release build from beside ra-jit's, in the workspace, where it ran
  it from cargo's target directory. helix looks for its `runtime` next to itself, so only
  cargo's build warned that there was none, and helix was reported as running differently.
- The borrow checker panicked on the corpus's `release_state`, whose impl's where clauses
  name associated types of concrete types (`Option<&'static C>: QueryData<ReadOnly: ..>`), so
  every error in the corpus's main file went unreported. As rustc, it now normalizes those
  where clauses before using them, and relates two associated types by what they normalize to
  (a GAT's `Self::Item<'w, '_>` and `Self::Item<'w, 'static>` are the same `&'w T`, where it
  said "lifetime may not live long enough"). A panic now fails the check where it was
  skipped: `ra-jit borrowck --project` exits with an error and counts panics,
  `borrowck_ui.py` fails on a panic or on any error in a test rustc accepts, and CI checks the
  corpus and `mixfutures` crates.
- The borrow checker no longer reports "temporary value dropped while borrowed" on every
  field of a `tracing::trace!` with tracing 0.1.44, which names fields with a `const` in a
  block inside its callsite's `static`: a `const fn` method call there is promoted, as rustc
  promotes it, so cargo's edit build goes through again
  ([#109](https://github.com/indexable-inc/ra-jit/issues/109)). A trait object given for a
  type parameter bounded `T: 'a` lives for `'a`, as rustc's object lifetime default, where
  rust-analyzer made it `'static` and the checker said "lifetime may not live long enough"
  ([#84](https://github.com/indexable-inc/ra-jit/issues/84)). All of cargo 0.99 now checks
  clean, where five false errors were left: rusqlite's `params![..]` of locals (an impl
  header's `&dyn ToSql` is for any lifetime, not only `'static`), `unwrap_or(&|_| true)` (a
  closure capturing nothing is promoted), git2 callbacks borrowing a counter declared after
  them (a panic's path drops nothing for a value with no drop glue), and an `async fn`
  returning `impl Iterator + 'b` of `&'a self` with `'a: 'b`.
- A program whose `main` is C's, as bun's, is built as a whole program: LLVM gets every body
  and inlines across crates, where each function was compiled alone before. `derive(PartialEq)`
  of an enum compares the discriminants first, as rustc's does. bun's whole program, every
  function ra-jit's, runs at 1.28x cargo's release build on Linux x86-64 (was 5.80x) and 1.25x
  on macOS arm64, where it now links: `runbench.py` no longer passes `-C link-dead-code`
  (which dropped `-dead_strip`), and the names of C in ra-jit's Mach-O objects stay
  exportable, as bun's `napi_*` must be
  ([#35](https://github.com/indexable-inc/ra-jit/issues/35)). The build takes longer, 565 s
  against 87 s on 16 cores: every compile carries the bodies it may inline.
- Adding a function to bun took 19.8 s, more than cargo's 14.5 s; it is a patch now, 0.64 s on
  Linux x86-64 and 0.56 s on macOS arm64. An added function in a program whose every function
  is an entry no longer forces a full build, and a crate the daemon listed before the files
  its `include!`s name were loaded is listed again once they are (`harness/latelisted.py`), so
  bun's 2,834 generated C-ABI functions are entries from the start. The edit's peak memory
  falls from 33.5 to 27.3 GB.
- helix's own build threw its first round away (about 30 s) because std's `Context::with`
  closure was refused for "a writable static without a rustc symbol": the static is declared by
  `thread_local!` in a body, which rust-analyzer expands twice, and ra-jit looked for the
  static's place in the other expansion. It finds rustc's symbol now, and the build takes one
  round: the daemon's own build in the background is done at 114.5 s instead of 130.3 s, a
  second daemon's at 85.9 s instead of 109.4 s, and with `RA_JIT_RUSTC_BASE=0` the daemon is
  ready at 116.2 s instead of 142.3 s (Linux x86-64, 16 cores, medians of three). Nothing is
  left to rustc for it, and `harness/norustc` counts the same functions left to rustc on
  ripgrep (53) and nushell (534) (#37, no-rustc step E).
- A `#[global_allocator]` in an inline `#[cfg(test)] mod test { .. }` of a library (ureq's)
  gave the library's stand-in an allocator outside the test build, whose functions nothing
  defined: every allocation of the program reached the stand-in's trap, so nushell, which links
  ureq, stopped at its first allocation. The `#[cfg]`s of the inline modules an allocator is in
  are now the forwarding allocator's.
- A constant that read a static holding `&dyn` of another static failed to evaluate
  (`InvalidVTableId`): the vtable was the id the static's own evaluation gave the type, which
  meant nothing to the reading evaluation, and a static the reference pointed into was not
  looked through for it. rustls's `ALL_CIPHER_SUITES` is one, which kept nushell from linking
  (corpus `dyn_static_suites`).
- A tuple struct with fields seen in other modules than the struct, both `pub(in path)` (ring's
  `aes::Counter`, `pub(in super::super)` with a `pub(super)` field), had no constructor:
  the least of those visibilities was folded from none, which is none. Its construction and
  patterns were refused, and with them ring's AES-GCM under nushell (corpus
  `narrow_constructors`).
- An `async fn` or block that awaited the `impl Future` of a provided method of a trait,
  which the impl leaves as the trait has it, had no layout: the trait's opaque type was left
  for the solver, which reveals nothing of a trait's, and came out an error. It is the hidden
  type of the trait's body now. rmcp's `ServiceExt::serve` is one, in nushell's MCP server
  (corpus `provided_futures`).
- `-C force-frame-pointers` in a build's flags keeps frame pointers in ra-jit's code too: LLVM
  gave the register to values, so code that walks frames by it read garbage. bun, built with
  every function ra-jit's, faulted in `bun_core::return_address` on its first `-e`; it now
  gives cargo's output on each of its runs. `harness/apps/apps.json` gives bun its own build's
  `-Cforce-frame-pointers=yes`. `harness/bun-repros/frame_pointers`.
- On ELF, a build that no edits follow puts each function and datum of its object in a
  section of its own, as rustc does, so the link takes out what nothing reaches. A program
  whose `main` is the C runtime's has every function compiled, and bun's link was missing
  what only such code calls and nothing defines on Linux: `GetLastError` of
  `bun_windows_sys`, and `ShellInterpreter__getConstructor` of a binding bun's C++ leaves
  out. The daemon's builds keep one section, as their links keep everything.
  `harness/bun-repros/unreached_externs`.
- A crate a build names after the program is one the program links, also when a binary of
  another package has its name: a build of bun with every crate ra-jit's took `--crate
  bun_sema` for the library and for `bun_sema_standalone`'s `bun-sema` too, whose code calls a
  library bun does not link, and the link failed on `bun_sema_standalone::parse`.
  `harness/bun-repros/same_name`.
- A `#[no_mangle]` or `#[export_name]` static of a program whose every crate is ra-jit's is
  defined under that name, also when it is private and when no Rust code uses it. Our copy had
  a name of its own, as rustc's was the one code of another language found, and one only that
  code reads was not compiled at all: bun's link was missing `Bun__version`,
  `Bun__Node__ZeroFillBuffers` and the other statics its C++ reads.
  `harness/bun-repros/exported_statics`.
- An `$n:expr` passed on to another macro is one operand there, as rustc has it: in
  `tags!(@count (0u8 + 1u8) + 1u8,)` the rule `(@ $n:expr,)` took `count (0u8 + 1u8)` for a
  call and matched, so bun's `task_tag::COUNT` expanded to nothing and `TaskTag::name`, the one
  function of bun's left to rustc for a reason other than having no instance, failed on
  `[&str; {const error}]`. The parser now stops an expression where such a fragment starts.
  `harness/bun-repros/macro_fragments`.
- A `macro_rules!` that a macro of another crate defines, called by a name a `use` imports
  (`use cfg_if::cfg_if; cfg_if! { .. macro_rules! trace { .. } }`), is in the textual scope of
  what follows. The call waited for imports to be resolved, by when `#[macro_use]` had taken
  the module's macros to its parent without it, and every `trace!` of lol_html's parser was a
  missing expression: 43 of bun's functions (the `StateMachine` impls of `Lexer` and
  `TagScanner`, `TransformStream::write`) were left to rustc. Such a call is now expanded where
  it is written, as one by a path to the other crate was. `harness/bun-repros/cfg_if_macros`.
- `--crate NAME` names no crate of the standard library's when a crate of the program has the
  name. std vendors foldhash 0.2.0, libc and hashbrown, and its foldhash, which rustc does not
  build (hashbrown has it only with its default hasher), sees no `core` in rust-analyzer's
  graph: bun's whole-program build compiled it beside bun's two foldhash, and its 27 functions
  that did not lower were counted as left to rustc. `harness/bun-repros/std_copies` checks it.
- A dependency's impl that a macro writes in a function body is found: dtoa's `write` calls
  `dtoa!`, which calls `diyfp!`, which writes `impl DiyFp` there, and `normalize_boundary`
  went unresolved because no `impl` is in `write`'s own text. A dependency's function is now
  looked into for impls when it calls a `macro_rules!` of its crate or of a dependency that
  writes one, in its text or through another macro. The dtoa functions bun left to rustc
  compile; `harness/bun-repros/macro_impls` builds dtoa.
- On Linux AArch64, an `asm!` of an instruction that the function's `#[target_feature]`
  enables and the target's baseline lacks (zlib-rs's `crc32w`, in bat) was refused by the
  assembler: the block is assembled apart from its function, so it now starts with the
  `.arch_extension` of each such feature. The runtime benchmark's whole-program builds of
  tokei and eza no longer fail to link there: they kept rustc's object of the names a
  program keeps out of the line, which is how GNU ld takes the panic runtime (#78).
- A checkout that an edit broke could be answered ok, linked with rustc's copy of the function
  from before the edit: a program whose build was interrupted, or panicked, or whose checkout
  was loaded again with the same link, forgot what rustc's copies are copies of, and its next
  build took the source as it was then for what rustc compiled. It keeps that now (#71).
- An edit of code two programs share ran wrong with the LLVM backend when the daemon compiled a
  program's `main` itself (on macOS and Windows, whose temporary paths go through a link): it
  took that build for one of a whole program and inlined across crates, and a caller holding
  the old body of an edited function was kept, as what is kept is compared by each function's
  own code. A daemon or `--watch` no longer inlines across crates; `programs.py` with
  `RA_JIT_ALONE=1` builds both programs that way in CI (#63).
- Under plain cargo, a test that ran the workspace's program could find none: the build the
  daemon loads afterwards ran beside the tests, and linking the program into place again
  deletes it first. It now waits for the user's cargo to exit (#61).
- `cargo ra-jit stop` returns once the daemon has exited, with its workers stopped and its log
  written, rather than once its socket is gone; nothing starts a spare worker while the daemon
  ends, where one could outlive it. A client that replaces a daemon of another version waits
  the same way (#65).
- A daemon's socket is free as soon as the daemon lets go of it, also while another thread of
  the process starts a program, which shares the lock's file until it runs (#58).
- The daemon's own base, built once nothing is asked after a cold build, started over every
  second and was never done when a crate it loads includes a file that is not there: zed's
  `livekit_api` reads `OUT_DIR`, which only a dev-dependency's feature brings into the
  daemon's crates, and whose build script cargo never ran. A file an `include!`,
  `include_str!` or `include_bytes!` names and that cannot be read is now read again once it
  is there, rather than taken for one still to load before each request
  (`harness/rustcbase.py`'s `unbuilt-member`). And since a base nobody waits for must not
  take the machine, a worker that holds more than a quarter of its memory while it builds one
  gives it up and says so; edits go on patching rustc's executable (`RA_JIT_OWN_BASE_MEMORY`,
  `own-base-budget`). zed's base, which the loop had hidden, passes it: 51 GB of 74 GB live are
  debuginfo types
  ([#55](https://github.com/indexable-inc/ra-jit/issues/55)).
- bat builds whole: an `impl` inside a function body of a dependency (bincode's `EnumAccess`
  for `&mut Deserializer`, inside `deserialize_enum`) was not found, as rust-analyzer only
  looked into the bodies of workspace crates; a dependency's file is now lexed for an `impl`
  inside a function's braces, and only the functions holding one are lowered.
- A whole program's build no longer builds a body it found too large again at each call
  that allows no more: with what such a body counted given back, ripgrep's build across
  crates took 416 s on Linux x86-64 and numeric's 151 s; 30.1 s and 8.7 s now.
- LLVM's `instcombine`, as ra-jit's pipelines name it, runs without verifying that it reached
  its fixpoint, as the one in `default<O3>` does: an LLVM built with that check ended the
  process on a function of jsonrt, and of nushell at `-O3` across crates.
- fd built with inlining across crates crashed the LLVM backend on Linux and macOS: an address
  rebuilt as a `getelementptr` may be a sum that is a constant (a global's address plus an
  offset), which is in no block, and LLVM read freed memory when the builder was put before it.
- A `const { .. }` block in a function is a constant evaluated for each instance of the
  function, as rustc's inline consts are, and no longer code the function runs: what only
  compile time may run in one (`#[rustc_comptime]`, `type_info`'s `field`, bun's
  `MultiArrayList::check`) was refused with "intrinsic `type_id_field_representing_type`",
  and every function reaching it with it (591 of bun's 680 refusals on Linux). A block met
  while evaluating a constant runs in that evaluation, so its `TypeId`s compare. `n += i` on an
  integer variable whose operator was not looked up while its type was unknown is lowered as
  the built-in it is, where it was "adjustment of binary op was missing" (`bun_repros.py`
  `type_info`).
- A constant of a struct, enum, tuple or array type given as a const generic argument
  (`adt_const_params`: `check::<{ Requirement::Within { low: 3, high: 9 } }>`,
  `filtered::<{ Filter { mask: 0xf0, strict: true } }>`) compiles and evaluates, and
  `#[derive(ConstParamTy)]` expands. Every function reaching one was refused ("constant of type
  JSType", bun's `alloc`, `test_correct_state` and `completions`) (`bun_repros.py`
  `adt_const_params`).
- A path through a type alias of an inherent impl (`ThreadPool::Worker::get(..)`, `Self::Slot`,
  `Slots<u32>::Slot`, nightly's `inherent_associated_types`) is the alias's type: the
  functions that name one had no type for it and were left to rustc, and rustc's copy of
  bun's `run_nested_rename_task` sent every edit of its crate to cargo (`bun_repros.py`
  `inherent_associated_types`).
- A build in full no longer keeps rustc's copy of an instance that did not compile once an
  edit changed it, as `located::<u8>` of a generic function with a `Location::caller()`
  constant, which `main` calls: the program printed what it did before the edit. Such an edit
  goes to cargo, which names the function (`soakrows.py`, "an instance rustc's copy stands in
  for, edited").
- A static declared in a function's body, as the `FLAG` of a method of bun's
  `impl<const SSL: bool, ..> NewServer<SSL, ..>`, is rustc's in a crate with a function left
  to rustc that reaches what ra-jit can't tell: rustc's copy of that function reaches the
  static through rustc's copy of the other. A build in full had its own copy beside rustc's,
  and the two counted apart, in the base the daemon builds after the first patch
  (`soakrows.py`, "statics one codegen unit uses").
- A borrowed constant with a cast in it, `&[b"node" as &[u8]][..]` returned for `'static`, is
  promoted as rustc promotes it, where it was built on the stack and the reference outlived
  it: every cast is promotable but of a pointer to an integer. bun's `new Bun.Transpiler()`
  crashed on it (breakers, `promoted_index_of_casts`).
- A `match` arm's guard with `let` (`if let` guards and their chains) dropped what it bound
  and its temporaries at the end of the guard, before the arm ran, in editions before 2024;
  rustc keeps them until the arm ends or the next arm is tried
  ([#46](https://github.com/indexable-inc/ra-jit/issues/46)). The four rustc tests that
  caught it run on every pull request now (`harness/ui-pinned.txt`).
- The harnesses set `RUST_BACKTRACE=0` for what they run rather than unsetting it: cargo's
  `[env]` (as in `~/.cargo/config.toml`) fills in only what is unset, and a backtrace changed
  what a panicking program printed.
- Large workspaces are ready in a fraction of the time and memory
  ([#29](https://github.com/indexable-inc/ra-jit/issues/29)): a closure's type holds its inner
  closure twice, so the store's name for a type of nested closures doubled with each level
  (one of helix's was 665 MB). A type is now spelled once per daemon, and one spelled longer
  than 1 KB is named by the SHA-256 of its spelling; shorter names, and the store entries
  under them, are unchanged. helix is ready in 109 s at 18.4 GB of memory instead of 631 s at
  100.7 GB, its store is 1.6 GB instead of 14 GB, and its first `cargo ra-jit run` takes
  46.9 s instead of 640.8 s (`harness/deepnames.py`).
- A `Box` beside an allocator that has a size (`Box<[T], &Arena>`, as bun's arenas) is
  dereferenced and dropped through its pointer wherever its layout put it: a deref of one was
  left to rustc, and its drop read the pointer from the start of the `Box`. `breakers.py`
  runs the crates of `NIGHTLY_CRATES` with RUSTC_BOOTSTRAP=1 (`allocated`, `allocator_api`).
- `simd_reduce_max` and `simd_reduce_min` of integer lanes are compiled (AArch64's `vmaxvq_u8`
  and the other `vmaxv`/`vminv`, AVX-512's `_mm512_reduce_max_*`): a function that reached
  them was left to rustc (bun's ASCII checks; `simd_reduce_max` breaker).
- `RA_JIT_OPT=3`'s last instruction combining no longer aborts the process when one run of it
  leaves something to combine ("Instruction Combining … did not reach a fixpoint after 1
  iterations", bun's whole-program build on AArch64): the default pipelines never ask LLVM to
  check that, and a pass named in a pipeline did.
- `apps.py`'s and `runbench.py`'s whole-program build of a program whose C calls its Rust
  by an unmangled name (a `#[no_mangle]` function) goes on when cargo's link of the stand-ins
  fails for want of those functions: what is taken from it is the link line, and ra-jit's
  objects define them. It ended at "cargo failed" (bun; `nativecalls` in `apps.json`).
- What stands in for a crate in cargo's build (the source with no code that cargo links when
  ra-jit compiles the crate: `apps.py`'s and `runbench.py`'s whole-program builds, and the
  crates the daemon builds while cargo builds the rest) builds with a nightly cargo that names
  each dependency twice, its `.rlib` and its `.rmeta`. It had an `extern crate` for each
  (`E0259`, bun 1.4.3 on `nightly-2026-09-15`).
- A `macro_rules!` that a macro of another crate defines (`dep::define_log!(log)`, as bun's
  `bun_core::define_scoped_log!` at the top of a file) is in the textual scope of the modules
  after it, as in rustc. rust-analyzer expanded a call by a path only after the file's modules
  were collected, so their calls of the macro were missing expressions and their functions were
  left to rustc (`IncompleteExpr`); in bun one such function in `bun_runtime` sent every
  signature edit of the crate to cargo (`harness/breakers`, `macro_defined_by_foreign_macro`).
- `harness/apps.py`'s perf stage ends with its timings again: since the rename from rustd it
  stored ra-jit's as `ra-jit_s` and printed `ra_jit_s`, a `KeyError` after every workload.
  wasmrun builds wasmtime 36.0.19 (was 25.0.3, which Dependabot's alerts name); its workload
  is the same as rustc's on both.
- `cargo ra-jit run` from many clients of one checkout at once starts each client's program,
  on macOS too. A package with a `default-run` (or one program of several) went to plain
  `cargo run`, which on macOS removes and copies again the program cargo has under
  `target/debug` before it starts it, so a client that started it meanwhile found no file
  ("No such file or directory") or hung before its first instruction; and rustc built the
  edit again. ra-jit now runs the `default-run` itself, holds cargo's lock on the build
  directory from putting the daemon's programs in place until they have started (a `cargo
  test` executable too), takes it to give cargo back its own, and on macOS puts a program in
  place as a clone, as cargo does: a program started from a hard link just made can be
  killed as it starts ([rust-lang/cargo#7821](https://github.com/rust-lang/cargo/issues/7821)).
  `cargo_ra_jit.py`'s 16 clients on 4 worktrees run 8 rounds of edits, all by the daemon.
- On Linux a program `cargo ra-jit` started just after a full link failed now and then with
  "could not execute" (exit 1; another row of `cargo_ra_jit.py`'s flags each run): the daemon
  writes what a debugger reads of a new executable in place while it waits for the next edit,
  and the kernel refuses to start a file open for writing ("text file busy"). The daemon now
  writes an executable in place under an exclusive lock on it, and a client starts a program
  under a shared one.
- A program or test `cargo ra-jit` starts itself has what the `[env]` of cargo's
  configuration sets, as under `cargo run` and `cargo test`: the nearest `.cargo/config.toml`
  (or `config`) says a variable, then `$CARGO_HOME`'s, with `force` and `relative`.
- `cargo_ra_jit.py`'s life workspace no longer races with itself on macOS: the test that runs
  `cargo build` inside and the one that starts `other` take turns, since cargo removes and
  copies each program again there on every build and the second sometimes found none (plain
  cargo failed it as often as ra-jit).
- `harness/ui-known.txt` names the 15 tests of rustc's tests/ui run-pass that differ on the
  whole suite, each with why (11 assert a bug of rustc's, 4 are ra-jit's to do), so the
  `claims` gate says only what is new; a line can name the backends it is known on.
- `harness/debuginfo.py`'s perf check takes a sample in a naked function without a line as
  rustc has it (its body is assembly, which rustc gives no line either), not as lost debug
  info: one of the corpus's on Linux arm64 failed it now and then. It reads every frame perf
  prints, those of inlined functions and those whose names have spaces (`<T as Trait>::f`)
  too: it skipped them, so where perf looks CFI up a page off the frame (both Linux runners)
  the frame it took for the one sampled could be one unwound to anywhere, a static on Linux
  x86-64 once. rustc's corpus binary has as many frames unwound to data there.
- `breakers.py`'s `frame_of_tables`, `frame_of_results` and `frame_of_array` ask whether
  ra-jit's frames fit where rustc's do, not whether they are as large: a whole program's
  build with LLVM makes them smaller than rustc's, which each case's crate has room for too,
  and they differed. `frame_of_array` is open on Cranelift alone, whose frames keep three
  arrays where rustc's keep two.
- `cargo_ra_jit.py` says what a client that failed or hung said, and what it was doing,
  rather than ending on a timeout with nothing.
- A workspace the daemon loads after a change whose build scripts did not all run is not
  loaded: the crates of a script that failed were read without its `OUT_DIR`, so thiserror's
  `include!` went missing, every derived `Error` was unimplemented and `?` resolved to the
  body-less `FromResidual::from_residual` for the rest of the daemon's life (what hyperfine's
  edits under plain cargo met now and then on Linux before the edit stopped running every
  script through cargo). It says which script failed instead, as an edit that runs the
  scripts again already did.
- `bench.py` without `--reference` built plain cargo into `target-ref` inside the checkout,
  which cargo takes for a source of the package: ripgrep's, fd's and hyperfine's build scripts
  name no `rerun-if-changed`, so cargo ran them again and built the package again at every
  build, and "nothing changed" took cargo 300 to 390 ms instead of 30 to 60. The README run of
  2026-10-20 at `c8f58ac7d9` had it; cargo's target directories are now outside the checkout
  (`WORK/reference` by default).
- `nix profile install github:indexable-inc/ra-jit#ra-jit`, as the README says to run it,
  installs ra-jit and the programs it runs; the flake had no package of that name.
- A release's archive and the flake's build have `ra-jit-rustc-first`, which `cargo ra-jit`
  runs in the first build of a workspace; they had only ra-jit, cargo-ra-jit and ra-jit-rustc.
- A module whose file is outside its package, by `#[path = "../../shared/outside.rs"]` as
  bun's runtime has, is compiled by ra-jit and its edits are built. The file was in no
  package's directory and so not loaded: the functions that called into it were left to
  rustc, an edit to it was not seen and the program kept running the old code, and in a
  binary crate the daemon could not load the workspace ("unresolved module ... [E0583]").
  The file is now put in the source root of the one that declares it, as a file an
  `include!` names is. `soakrows.py`'s "a module file outside its package" fails without
  this.
- An edit that could change a function ra-jit left to rustc from the start, such as an item
  added elsewhere in its crate, is built by cargo, as docs/cargo.md says. It failed the
  build: when the function was one the frontend turned down, ra-jit took that for an error
  of the source ("has an error ra-jit cannot say more of"), and every build after it failed
  too until cargo built once. A function the frontend turns down only after an edit is still
  reported as an error. Found building bun; `soakrows.py`'s "an item added beside a function
  left to rustc" fails without this.
- A worker that loaded its workspace beside cargo's first build
  ([#11](https://github.com/indexable-inc/ra-jit/issues/11)) ends when its daemon does, as
  every other worker does: once told what cargo built, it waited for a lock on its standard
  input that it held itself, so a daemon that crashed or was killed left the worker running,
  holding its memory (2 GB on fd) and its socket, and the next daemon's first build fell
  back to cargo ("another daemon serves"). `cargo_ra_jit.py`'s "workers of the killed
  daemon are gone" fails without this.
- fd, tokei and eza build whole and run as cargo's builds do: a crate rustc built nothing
  of keeps its `static`s ra-jit's own (fd's `INIT`); a package built only for a build script
  or a proc macro, which `cargo metadata`'s single graph can still show as a dependency of
  the program's, checks overflow as the program does, so rust-analyzer's view of it no
  longer mixes two profiles; and `runbench.py` turns the release profile's `lto` off for the
  whole build, which needs rustc's libraries. bat and nushell do not link yet.
- A program ra-jit builds whole, with its own `main` (`runbench.py`'s builds), ran: on macOS
  the code-free stand-in rustc builds for the program had a weak `main` of its own, and
  ra-jit's object was weakened too, so the linker kept the stand-in's and the program
  exited 0 at once, printing nothing. ra-jit's `main` now stays strong. On macOS and Linux
  a `#[global_allocator]` was ignored in such a program, as the stand-ins declare none and
  rustc's allocator shim went to the default allocator; a stand-in now declares one that
  forwards to functions rust-analyzer's expansion of the attribute defines, under the
  `#[cfg]`s of the static and of the modules it is in, and not for one in a string, as
  heapless's docs have (`wholeprogram.py`).
- `&(1 / 2)`, a division by a constant, and `&mut []` are promoted to constants as rustc
  promotes them, so a reference to one lives as long as the program.
- Before the 2024 edition, a panic in a block's tail drops the tail's temporaries after the
  block's locals, as rustc does; ra-jit dropped them first, so a `Drop` that printed or
  locked saw the other order (`breakers.py unwind_tail_after_local`).
- What leaving a scope early drops (`break`, `continue`, `return`, a future dropped at an
  `await`) is placed on the line where the scope ends, as rustc places it, so a backtrace or
  a debugger's step through those drops shows the same lines under ra-jit as under rustc.
- On macOS, signing a splice reuses the hash of each appended page that is the same as the
  last splice's: 21 ms to 5 ms of each edit of nushell.
- The value of a `match` arm's guard was marked dead before the branch that reads it; with
  storage markers kept, as rustc's optimized MIR keeps them above `-Copt-level=0`, the guard
  could take the wrong arm.
- The code of a function grows in a straight line with the arms of a match that leave
  through `?`, as `#[derive(Serialize, Deserialize)]` writes for an enum: it grew with
  their square, 329 MB for one function of 285 arms, which an AArch64 branch cannot reach
  across ([#5](https://github.com/indexable-inc/ra-jit/issues/5)). `harness/growth.py`
  checks it.
- An edit that arrives while a function is being compiled at a higher level no longer waits
  for LLVM: the compile lets go of the database before LLVM runs.
- A workspace under a directory whose name has a space in it is built against what rustc
  built of it ([#14](https://github.com/indexable-inc/ra-jit/issues/14)). The daemon cut
  the dep-info file that names a crate's sources at every space, found none of the
  workspace's crates among rustc's, and named their functions itself, so a function it
  refused had no copy of rustc's to link to. Dep-info is now read and written as rustc writes
  it, in one place: a space in a path is `\ `, and `$`, `#` and backslashes are themselves;
  the values of the variables in it are unescaped too. `harness/spaces.py` builds a
  workspace under such a directory end to end.
- On macOS a program whose unwind table is larger than 1 MB takes edits as splices, and so
  live in the running program ([#13](https://github.com/indexable-inc/ra-jit/issues/13)).
  The daemon gave Apple's linker a fixed `-headerpad` of 1 MiB, and a splice moves the
  table into that room, so a GUI app of 570 crates with 3 MB of table, or nushell with 3.2
  MiB, linked every edit in full. The link now reads its headerpad from a file beside the
  executable, last on the line: 1 MiB, or what the line's own flags ask for. After each full
  link the daemon reads the header, and when the room is short it links once again with what
  the table needs and a quarter more, which the links after start from, in later daemons
  too. nushell's edit of `main` went from 878 ms to 276 ms on an M5 Max, and a library edit
  from 693 ms to 166 ms. `harness/headerpad.py` checks it.
- The first edit after the daemon is ready is as fast as the ones after it
  ([#12](https://github.com/indexable-inc/ra-jit/issues/12)). Two things made it slow.
  On Linux the first splice after a full link took 2.5 s of nushell's: it copied every
  debug section it added to, about 500 MB, behind the code it appended, and it cut the
  file to size, which on XFS writes out at the close all of the file the linker had not
  yet written. The sections now go where splices write them right after the link, and a
  splice only lengthens the file, so the first splice's link takes 7 ms, as those after it
  do. And code the daemon took from the store does not say where in the checkout it is
  from, so the first edit of a crate it came from compiled all of it and wrote the whole
  object again: 71,661 functions of nushell. The worker now compiles that code again
  by itself after its first build, and the daemon says the checkout is ready once it has.
  nushell's first edit of `main` after a cold build from a warm store went from 9.1 s to
  0.37 s on Linux and from 7-8 s to 0.3-0.4 s on an M5 Max. `harness/storefirst.py`
  checks it.
- On macOS a spliced executable lists its sections in the order of their addresses, and
  each segment a splice adds has a section, as `dyld_info` requires: it read none of the
  spliced executables on macOS 27 (`harness/splices.py`: 11 of 71 right, now 71).
- Tiered code that calls what the executable has no symbol of (`memcmp` where rustc's code
  calls `bcmp`, a `pthread_` function only rustc's inlined code reached) no longer has the
  program linked in full and started over at level 1, after which every later patch of it
  was linked in full too: that function stays where it was, and says why
  ([docs/tiers.md](docs/tiers.md), "Code no patch can hold").
- The thread that compiles at a higher level has a stack of 64 MiB, and the bodies a body
  built in shows are ten deep at most: a deep chain of them overflowed the 2 MiB default and
  took the worker down.
- Tiered code that calls what the executable has no symbol of (`memcmp` where rustc's code
  calls `bcmp`, a `pthread_` function only rustc's inlined code reached) no longer has the
  program linked in full and started over at level 1, after which every later patch of it
  was linked in full too: that function stays where it was, and says why
  ([docs/tiers.md](docs/tiers.md), "Code no patch can hold").
- The thread that compiles at a higher level has a stack of 64 MiB, and the bodies a body
  built in shows are ten deep at most: a deep chain of them overflowed the 2 MiB default and
  took the worker down.
- `RA_JIT_RUSTC_BASE=0` reaches the worker of the build that sets it, which is a worker of its
  own: it reached one only when the build that set it had started the daemon, so behind a
  daemon another build started the first edits were patches of rustc's executable, and an edit
  that adds a type asked for a restart (`live_types.py` in the gate, after the other live
  tests).
- The first edit after the daemon is ready, while it is still rustc's executable that edits
  patch, is faster when it is a build in full, as one that moves an `async` body is (reth's
  `fn main`, [#259](https://github.com/indexable-inc/blaze/issues/259), by @gakonst): the
  pass that puts the code in the object, which runs on one thread, wanted every function
  compiled on demand again for each static rustc's code has, and walked a whole definition with
  its macros for each closure and each static in a body (tracing's `META` and `__CALLSITE` in
  every event). Each crate's on-demand functions are wanted once, and each definition is
  walked once for all its closures or all its statics of a name. reth's first edit on 48 cores:
  64.4 s and 64.3 s before, 58.0 s and 57.5 s after.

## [0.1.0] - 2026-10-20

The first release.

### Added

- `ra-jit`, a compiler daemon that keeps a workspace loaded in rust-analyzer and generates
  code from its MIR with Cranelift. After the first build, an edit rebuilds only the
  functions that changed.
- `cargo ra-jit build|run|test|check`, with cargo's own flags. Anything the daemon cannot
  build falls back to cargo, and says so.
- `ra-jit enable` and `ra-jit disable`: route plain `cargo build|run|test|check` through
  ra-jit, and put cargo's config back.
- `ra-jit-live` (on crates.io), a crate that applies edits to a running program, keeping
  its state (macOS on Apple silicon).
- A nix flake: `nix run github:indexable-inc/ra-jit` opens a Game of Life you can edit
  while it runs; `#ra-jit` is the cargo wrapper.
- Prebuilt binaries for Linux (x86_64, aarch64) and macOS (aarch64).
- `cargo install ra-jit` from crates.io (Cranelift; `--features llvm` needs LLVM 20). The
  rust-analyzer and rustc_abi forks it builds on are published as `ra-jit-<name>`.

### Known limits

- Rust 1.99.0 only.
- No borrow checker: keep running `cargo check`.
- Changing a struct the running program uses asks for a restart.

[Unreleased]: https://github.com/indexable-inc/ra-jit/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/indexable-inc/ra-jit/releases/tag/v0.1.0
