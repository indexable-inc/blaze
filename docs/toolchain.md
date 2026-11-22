---
status: current
summary: blaze's own pinned nightly: what it is, how it is fetched and checked, and how its pin moves.
---
# blaze's own toolchain

blaze is its own Rust toolchain. It builds every project with one toolchain that blaze
controls, names and ships, whatever the project pins: `blaze-2026-10-27` today. It is not the
project's toolchain and it does not try to be. The project's own toolchain, and its CI, stay
the final word on whether the code builds.

Andrew, 2026-10-26, answering "blaze uses its pinned nightly even when the project pins stable":
"you should use YOUR OWN NIGHTLY that you control since you are trying to replace rustc".
Andrew, 2026-10-28 03:30Z: zero Rust toolchain pinning. blaze uses the language and std its own
binary ships, ignores rust-toolchain, rust-toolchain.toml, rustup's settings and
`RUSTUP_TOOLCHAIN`, and never runs or asks rustc, not even with rustup installed. Projects pin
the blaze version instead (`[workspace.metadata.blaze] version`): a released `blaze` runs the
release a workspace pins, a build of blaze's tree runs itself in blaze's tree and runs the
pinned release elsewhere, as a release does, and `blaze --ignore-pin` runs the blaze given
whatever is pinned
([cargo.md](cargo.md#pinning-a-release-of-blaze)).

## Why one toolchain, and why blaze's

blaze compiles your code itself (Cranelift or LLVM, from its front end's MIR), the standard
library too (from `library/`, below), and must behave as that nightly's rustc would: how types
are laid out, how functions are called, what a `TypeId` hashes, the names of symbols, libtest's
entry point, the proc-macro bridge and what rustc answers about targets. blaze knows those facts
for one toolchain, and that toolchain is the base blaze replaces piece by piece
([no-rustc.md](no-rustc.md): cargo, then rustc, then the standard library's build). Following
each project's pin would mean knowing every release's facts and checking every release; the
goal is the reverse, no rustc at all. So the toolchain is part of blaze, versioned with it, like
the standard library is part of a compiler.

## What it is today

- **Name:** `blaze-<date>` (`blaze-2026-10-27`), after the date of the Rust nightly it is built
  from (`pins/toolchain`, `nightly-2026-10-27`; `blaze_cargo::toolchain::name`).
  `blaze toolchain` prints it and where it is, downloading it first when it is not there;
  `blaze toolchain --json` prints one object (`name`, `nightly`, `rustc`, `release`, `commit`,
  the host's tuple as `host`, and `sysroot`), which a harness reads instead of running
  `rustc -vV`.
- **Bytes:** the upstream nightly's `rustc`, `cargo`, the host's `rust-std` and
  wasm32-wasip1's (for wasi-libc, which blaze's linker links every proc macro's module with), unchanged,
  from static.rust-lang.org, each checked against the sha256 compiled into blaze
  (`pins/rust.txt`, written by `harness/toolchain_pins.py` and read at compile time by
  `crates/blaze-pins`, the one Rust reader of `pins/`; [pins/README.md](../pins/README.md)). So `rustc -V`
  still says `1.101.0-nightly (16d1ffac1 2026-10-26)`.
- **Where:** `~/.blaze/toolchains/nightly-2026-10-27-<host>`, the machine's: every `BLAZE_HOME` reads
  this one copy (`BLAZE_HOME` moves a daemon's settings and state, not the toolchain).
  `BLAZE_TOOLCHAINS=<dir>` names another directory in place of `~/.blaze/toolchains`
  (`toolchain::own::toolchains`), for a machine whose home is small, read-only or not there;
  the daemon keeps the one it was started with. `~` is the user's home as blaze finds it
  everywhere (`blaze_core::home::user`): `HOME` when it is set and not empty, else the
  system's record of the user (`getpwuid_r` of the effective uid; on Windows `USERPROFILE`,
  then the profile directory), so a systemd unit, a CI runner or cron with no `HOME` finds
  the same copy a shell does. With neither, blaze stops, saying what it tried and naming
  `BLAZE_TOOLCHAINS`. It is found
  with nothing run (`crates/blaze-cli/src/toolchain.rs`). rustup's directory is never
  read, not even when it has the same nightly: a toolchain found by its directory's name is
  trusted by name, whatever was installed there ([v2/plan/toolchain-c.md](v2/plan/design/toolchain-c.md),
  which also plans the end of this download). The installer downloads it by running
  `blaze toolchain`, and every build checks it is there first (`toolchain::ensure`, which
  fetches it with `toolchain::own::fetch` when it is not).
- **The standard library's source** is not the toolchain's `rust-src`, which blaze neither
  downloads nor reads: it is blaze's own `library/` ([below](#the-standard-library-library)).
  A rustup toolchain that has `rust-src` is used without it, `RUST_SRC_PATH` is not read,
  and the engine's front end reads the library blaze fetched (`crates/blaze-toolchain`,
  the build's `library` mount) rather than looking for a `rust-src`. Where
  `rust-src` is still installed it is for something that is not blaze: CI's `cargo` job
  (`.github/actions/toolchain`, [harness.md](harness.md#ci)), rust-analyzer
  in an editor of this repository (the dev shell of `flake.nix`), and `harness/library.py`,
  which imports `library/` from the nightly's `rust-src` tarball.
- **Who uses it:** the daemon, which runs none of its programs. Its linker takes the C runtime
  WASI links and the sanitizer runtimes from the toolchain's sysroot
  (`blaze_link::target::locate`), and blaze answers `rustc --print sysroot` with it. A build
  script's `RUSTC` and `RUSTDOC` are answered by blaze itself (`crates/blaze-build-script`). The user's `cargo`,
  `rustc` and pins are untouched, and blaze reads none of them.
- **What rustc says:** blaze answers every question asked of rustc (`--version`, `-vV`,
  `--print cfg|sysroot|target-libdir|target-spec-json|...`) itself, for this release
  (`blaze_cargo::answers`): the daemon in process, and a build script through blaze's rustc
  stand-in, which also checks the probes build scripts compile (`autocfg`, `rustversion`,
  `version_check`, `anyhow`'s and `thiserror`'s) with `blaze check`'s verdict. No rustc is run to find any of it
  ([no-rustc.md](no-rustc.md), step B).
- **Bumps:** by hand, to a newer nightly, with `harness/toolchain_bump.py`
  ([Moving the pin](#moving-the-pin)).

## The LLVM blaze is built with

Building blaze itself takes one more pinned piece, LLVM: `pins/llvm.json` names
its commit and version (23.1.4, our fork's `ix/23.x`) and the sha256 of each target's release
archive. blaze fetches it itself, by that sha256, and hands it to the build scripts that link
it: the root `Cargo.toml` declares it a pinned native input
([cargo.md](cargo.md#pinned-native-inputs)), so no Nix and no `LLVM_SYS_231_PREFIX` is needed
(`nix develop` still sets the variable to the same archive, which blaze's value overrides); a
stage 0 older than that table needs the variable, from `.github/scripts/llvm.py` with `{"command": "fetch", ...}`.
For a target with no archive, the `llvm` workflow (`.github/workflows/llvm.yaml`) builds one. llvm-sys's build script refuses any other prefix with one error before anything links it
([contributing.md](contributing.md#llvm)). The LLVM inside a blaze binary
is not a toolchain for projects: it builds them with the toolchain above. Its MC assembler is
what assembles `asm!`, `naked_asm!` and `global_asm!` in process, never the machine's clang or
`cc`. The seed, built without LLVM, assembles them with rsasm in process (hir-ty-codegen's
`rsasm`; `vendor/rsasm/SOURCE.md`). What else
blaze still takes from the machine's C toolchain (the C driver the linker asks for crt files,
the macOS SDK, MSVC's libraries, build scripts' `CC`) and the plan to replace it with one C
toolchain built from this LLVM: [v2/plan/toolchain-c.md](v2/plan/design/toolchain-c.md).

## Moving the pin

The daily bump workflow was deleted on 2026-11-15; the pin moves by hand.
`uv run --frozen python harness/toolchain_bump.py '{}'` (or `'{"date": "YYYY-MM-DD"}'`) picks
the newest nightly whose manifest has every component blaze needs on every host it ships for,
writes `pins/toolchain` and `pins/rust.txt`
(`harness/toolchain_pins.py`), re-vendors the crates blaze takes from rust-lang/rust at the new
commit, merges the new nightly's standard library into `library/` ([The standard
library](#the-standard-library-library)) and writes blaze's registry of rustc's lints again
(`crates/blaze-lint-defs/src/rustc.rs`, `scripts/src/bin/lint_table.rs`); each step names
itself when it fails, and stops there (the script's docstring lists them). After it,
`harness/answers.py --write` on each host writes `pins/answers/rustc.json`,
and `harness/nightly_features.py '{}'` moves the tree of nightly features (#967) to the new pin.
The gate's `lint_table check` fails a tree whose registry was written from another commit than
`library/UPSTREAM`'s, or was edited.

## The standard library: library/

The source of the standard library blaze compiles is in the repository, at `library/`, so
that blaze can change it (Andrew, 2026-10-28: "make a ./library and just make it current
nightly rustc, and use that, so we use our own Rust source, which allows us to change it in
the future"). It is the Rust project's at the commit of blaze's nightly, as that nightly's
`rust-src` has it: `library/` of rust-lang/rust with the `backtrace` submodule, byte for
byte, and the third-party crates of `library/Cargo.lock` (hashbrown, libc, compiler_builtins'
deps, addr2line, gimli, object, ...) vendored in `library/vendor` with the
`library/.cargo/config.toml` that reads them from there. They are vendored rather than fetched
from crates.io so that a cold build needs no network beyond blaze's store, and so that their
paths, which panics and `file!()` show, are the ones `rust-src` gives: blaze names a file of
it `/rustc/<commit>/library/...` as rustc does, wherever it is on disk.

Where blaze reads it (`crates/blaze-toolchain`): it is known by its
digest, `library/DIGEST` (the sha256 of a line per file, `harness/library.py digest` writes
it), which is compiled into blaze and is what the store keys the standard library by
(`library=<digest>` in place of the nightly's commit and `rustc_driver` name). Every process
of blaze is told where it is in `BLAZE_LIBRARY`, the first of:

- `BLAZE_LIBRARY` itself, set by hand to try a changed library (its own `DIGEST` is used);
- `share/blaze/library` above the program (the Nix package installs it there);
- `~/.blaze/library/<digest>/library`, which blaze makes once, beside the toolchain's
  download: copied from the checkout it was built in when that has the same library, else
  downloaded as the release's `library-<digest>.tar.xz` (6.0 MB, `harness/library.py
  archive` writes it, the release workflow attaches it). `BLAZE_LIBRARY_FROM` names another
  place to get it from (an address or a directory with that archive, or a library to copy),
  taken over the checkout too. Whichever way it
  came it is checked against the digest file by file. A checkout's own
  `library/` is never read in place: what the store keeps of a crate whose data names the
  checkout being built is that checkout's alone, and the standard library's is everyone's.

- `library/UPSTREAM` says which: the commit, the nightly and the sha256 of its `rust-src`,
  read from the nightly's channel manifest (`harness/library.py import` wrote it).
- It is not a member of blaze's workspace: no fmt, clippy or test of blaze's reads it, and
  `.gitattributes` keeps upstream's bytes.
- The branch `upstream/library` is upstream's standard library alone: one commit (a
  snapshot) a nightly, each the child of the one before, holding nothing but `library/` as the
  nightly's `rust-src` has it. `harness/library.py` makes a snapshot the same way wherever it
  runs (same bytes, parent, author and date: the same commit). `git diff upstream/library --
  library/` is blaze's changes (`python3 harness/library.py snapshot '{}'` points the local
  branch at `UPSTREAM`'s snapshot first).
- A bump moves it with the pin as a merge (`harness/library.py merge`): it makes the
  new nightly's snapshot on `upstream/library` and applies upstream's changes from the old
  snapshot to the new one three-way, so blaze's own changes are kept, then pushes the snapshot
  with its branch. A conflict stops the bump and names the files.

To change the standard library, edit it under `library/`, run
`python3 harness/library.py digest '{}'` (the test `the_library_is_the_one_its_digest_says`
fails on a stale `DIGEST`) and commit. The next build copies the changed library into
`~/.blaze/library/<new digest>`, and the store keys what it compiles of it apart from the old.
Nothing checks it against upstream file by file: the smoke corpus and real crates' own tests
are what hold blaze's behaviour to the nightly's.

## What it means for a project

- A project that pins another toolchain (a stable release, an older nightly, in a
  rust-toolchain file, `rustup override` or `RUSTUP_TOOLCHAIN`) builds with blaze's. The pin is
  not read: blaze runs nothing to learn what it names, and says nothing about the file.
- `edition` applies as cargo applies it. `rust-version` is read for resolving dependencies
  (cargo's `incompatible-rust-versions`), but v2 does not yet refuse a member whose
  `rust-version` is newer than blaze's Rust, as cargo does; `--ignore-rust-version` is taken
  and changes nothing.
- Rust keeps building old code, so stable code builds; `#![feature(...)]` code builds too.
- Code blaze's toolchain cannot build (a nightly feature it removed, a library item newer than
  it) fails with blaze's error. A project that needs another Rust pins another blaze.
- Keep `cargo build`/`cargo test` with the project's toolchain in CI. blaze is for the edit
  loop; the project's toolchain decides.

## Where it goes: no rustc

blaze is moving off rustc ([no-rustc.md](no-rustc.md)); the rule is in [AGENTS.md](../AGENTS.md).
blaze's nightly is the definition of the language and the standard library blaze matches while
rustc is still in the tree: its layouts, ABI, `TypeId`s and answers are what blaze agrees with.
It is not a rustc blaze maintains. As the no-rustc steps land its pieces go: cargo, then rustc
with rust-lld, then `rust-std`. The standard library's source is
already blaze's (`library/`, downloaded with blaze's release rather than as the nightly's
`rust-src`), so what is left then is blaze alone.

- [#402](https://github.com/indexable-inc/blaze/issues/402): ship the standard library's
  source with blaze's release, so blaze's toolchain needs no upstream download once rustc is
  gone. Done: `library/`, the release's `library-<digest>.tar.xz`.
- [#403](https://github.com/indexable-inc/blaze/issues/403): shrink the download as each
  no-rustc step removes a piece (`cargo`, `rustc`, `rust-std`), down to blaze alone.
- [#404](https://github.com/indexable-inc/blaze/issues/404): the bump
  ([Moving the pin](#moving-the-pin)), the `blaze-<date>` name moving with it.
