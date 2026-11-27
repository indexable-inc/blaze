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
`RUSTUP_TOOLCHAIN`, and never runs or asks rustc, not even with rustup installed.

## Why one toolchain, and why blaze's

blaze compiles your code itself (through its own LIR), the standard
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
  from (`pins/toolchain`, `nightly-2026-10-27`). `blaze toolchain` prints the pin
  (`Rust nightly-2026-10-27`) and takes no flags.
- **Bytes:** the upstream nightly's archives are named with their sha256 in `pins/rust.txt`
  (written by `harness/toolchain_pins.py`, read at compile time by `crates/blaze-pins`, the one
  Rust reader of `pins/`; [pins/README.md](../pins/README.md)), with its version line,
  `1.101.0-nightly (16d1ffac1 2026-10-26)`.
- **Where:** v2 downloads none of these bytes. It reads the toolchain as records from the
  per-user shelf `~/.blaze/v2/toolchain/` (`crates/blaze-cli/src/toolchain.rs`), cut from the
  tree by `scripts/src/bin/toolchain_cut.rs` ([v2/plan/design/toolchain.md](v2/plan/design/toolchain.md)).
  `~` is the user's home as `blaze_core::home::user` finds it. rustup's directory is never read.
- **The standard library's source** is not the toolchain's `rust-src`, which blaze neither
  downloads nor reads: it is blaze's own `library/` ([below](#the-standard-library-library)).
  `RUST_SRC_PATH` is not read; the front end reads `library/` from the toolchain shelf
  (`crates/blaze-toolchain`). Where
  `rust-src` is still installed it is for something that is not blaze: CI's `cargo` job
  (`.github/actions/toolchain`, [harness.md](harness.md#ci)), rust-analyzer
  in an editor of this repository (the dev shell of `flake.nix`), and `harness/library.py`,
  which imports `library/` from the nightly's `rust-src` tarball.
- **Who uses it:** a build script's `RUSTC` and `RUSTDOC` are answered by blaze itself
  (`crates/blaze-build-script`), from the recorded answers `pins/answers/rustc.json`
  (`crates/blaze-pins`). The user's `cargo`, `rustc` and pins are untouched, and blaze reads none
  of them. No rustc is run to find any of it ([no-rustc.md](no-rustc.md), step B).
- **Bumps:** by hand, to a newer nightly, with `harness/toolchain_bump.py`
  ([Moving the pin](#moving-the-pin)).

## The LLVM blaze is built with

Building blaze itself takes one more pinned piece, LLVM: `pins/llvm.json` names
its commit and version (23.1.4, our fork's `ix/23.x`) and the sha256 of each target's release
archive. llvm-sys's build script takes it from `LLVM_SYS_231_PREFIX`, which
`.github/scripts/llvm.py` with `{"command": "fetch", ...}` fills by that sha256 (`nix develop`
sets it to the same archive). For a target with no archive, the `llvm` workflow (`.github/workflows/llvm.yaml`) builds one. llvm-sys's build script refuses any other prefix with one error before anything links it
([contributing.md](contributing.md#llvm)). The LLVM inside a blaze binary
is not a toolchain for projects: it builds them with the toolchain above. Its MC assembler is
what assembles `asm!`, `naked_asm!` and `global_asm!` in process, never the machine's clang or
`cc`. What else
blaze still takes from the machine's C toolchain (the C driver the linker asks for crt files,
the macOS SDK, MSVC's libraries, build scripts' `CC`) and the plan to replace it with one C
toolchain built from this LLVM: [c.md](v2/plan/design/c.md).

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

Where blaze reads it (`crates/blaze-toolchain`): as source packs on the toolchain shelf, listed
by one manifest record whose digest, `crates/blaze-toolchain/PINNED`, is compiled into blaze
([v2/plan/design/toolchain.md](v2/plan/design/toolchain.md)). `library/DIGEST`
(`harness/library.py digest` writes it) names the release's `library-<digest>.tar.xz`.

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
`python3 harness/library.py digest '{}'`, cut the shelf again (`toolchain_cut`) and commit.
Nothing checks it against upstream file by file: the smoke corpus and real crates' own tests
are what hold blaze's behaviour to the nightly's.

## What it means for a project

- A project that pins another toolchain (a stable release, an older nightly, in a
  rust-toolchain file, `rustup override` or `RUSTUP_TOOLCHAIN`) builds with blaze's. The pin is
  not read: blaze runs nothing to learn what it names, and says nothing about the file.
- `edition` applies as cargo applies it. `rust-version` is read for resolving dependencies
  (cargo's `incompatible-rust-versions`), but v2 does not yet refuse a member whose
  `rust-version` is newer than blaze's Rust, as cargo does.
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
already blaze's (`library/`, not the nightly's `rust-src`), so what is left then is blaze
alone.

- [#402](https://github.com/indexable-inc/blaze/issues/402): ship the standard library's
  source with blaze's release, so blaze's toolchain needs no upstream download once rustc is
  gone. Done: `library/`, the release's `library-<digest>.tar.xz`.
- [#403](https://github.com/indexable-inc/blaze/issues/403): shrink the download as each
  no-rustc step removes a piece (`cargo`, `rustc`, `rust-std`), down to blaze alone.
- [#404](https://github.com/indexable-inc/blaze/issues/404): the bump
  ([Moving the pin](#moving-the-pin)), the `blaze-<date>` name moving with it.
