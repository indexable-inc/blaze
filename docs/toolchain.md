---
status: current
summary: blaze's own pinned nightly: what it is, how it is fetched and checked, and the daily bump.
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
the blaze version instead.

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
  from (`harness/toolchain`, `nightly-2026-10-27`; `blaze_cargo::toolchain::name`). v2's blaze
  has no command that prints it or where it is (v1's `blaze toolchain` did).
- **Bytes:** the upstream nightly's `rustc`, `cargo` and the host's `rust-std`, unchanged,
  from static.rust-lang.org, each checked against the sha256 compiled into blaze
  (`crates/blaze-cargo/src/toolchain/pins.txt`, `harness/toolchain_pins.py`). So `rustc -V`
  still says `1.101.0-nightly (16d1ffac1 2026-10-26)`.
- **Where:** `~/.blaze/toolchains/nightly-2026-10-27-<host>`, or rustup's `nightly-2026-10-27`
  when rustup already has it (the same bytes), found with nothing run
  (`crates/blaze-cargo/src/toolchain.rs`, `found`). The installer downloads it by running
  `blaze toolchain`, which v2's blaze does not have, and no v2 build downloads it
  (`toolchain::own::fetch` has no caller): today it must already be in one of those two places,
  or a build stops with "blaze's toolchain is not installed".
- **The standard library's source** is not the toolchain's `rust-src`, which blaze neither
  downloads nor reads: it is blaze's own `library/` ([below](#the-standard-library-library)).
  A rustup toolchain that has `rust-src` is used without it, `RUST_SRC_PATH` is not read,
  and the daemon's front end reads the library blaze fetched (`crates/blaze-cargo/src/toolchain/library.rs`,
  the build's `library` mount) rather than looking for a `rust-src`. Where
  `rust-src` is still installed it is for something that is not blaze: rustc's side of the
  harnesses (`.github/actions/toolchain`, [harness.md](harness.md#running-them)), rust-analyzer
  in an editor of this repository (the dev shell of `flake.nix`), and `harness/library.py`,
  which imports `library/` from the nightly's `rust-src` tarball.
- **Who uses it:** the daemon, which runs none of its programs. Its linker takes the C runtime
  WASI links and the sanitizer runtimes from the toolchain's sysroot
  (`blaze_link::target::locate`), and blaze answers `rustc --print sysroot` with it. A build
  script's `RUSTC` and `RUSTDOC` are blaze itself, linked as `rustc` and `rustdoc` in the
  daemon's `tools` directory (`crates/blaze-engine/src/as_rustc.rs`). The user's `cargo`,
  `rustc` and pins are untouched, and blaze reads none of them.
- **What rustc says:** blaze answers every question asked of rustc (`--version`, `-vV`,
  `--print cfg|sysroot|target-libdir|target-spec-json|...`) itself, for this release
  (`blaze_cargo::answers`): the daemon in process, and a build script through blaze's rustc
  stand-in, which also checks the probes build scripts compile (`autocfg`, `rustversion`,
  `version_check`, `anyhow`'s and `thiserror`'s) with `blaze check`'s verdict. No rustc is run to find any of it
  ([no-rustc.md](no-rustc.md), step B).
- **Bumps:** every day, to the newest nightly, by a workflow, with the merge gate and the
  release train as the gate ([The daily bump](#the-daily-bump)).

## The LLVM blaze is built with

Building blaze itself takes one more pinned piece, LLVM: `crates/hir-ty-codegen/llvm.json` names
its commit and version (23.1.4, our fork's `ix/23.x`) and the sha256 of each target's release
archive. `nix develop` fetches it and sets `LLVM_SYS_231_PREFIX` to it (the flake's
`blaze-llvm`); outside the shell, `python3 .github/scripts/llvm.py fetch` unpacks the same
archive; for a target with no archive, the `llvm` workflow (`.github/workflows/llvm.yaml`) builds
one. llvm-sys's build script refuses any other prefix with one error before anything links it
([contributing.md](contributing.md#llvm-for---features-llvm)). The LLVM inside a blaze binary
is not a toolchain for projects: it builds them with the toolchain above.

## The daily bump

blaze tracks the latest Rust nightly. `.github/workflows/toolchain-bump.yaml` runs every day at
05:40 UTC (nightlies are published at about 00:00 to 02:00 UTC), and by hand with a date
(`gh workflow run toolchain-bump.yaml -f date=2026-11-02`):

1. On the branch `toolchain/bump` (the open bump's with main merged in, or a new one from main),
   `harness/toolchain_bump.py` pins the newest nightly whose manifest has every component blaze
   needs on every host it ships for. It writes `harness/toolchain` and `pins.txt`, re-vendors
   `crates/rustc_abi` from rust-lang/rust at the new commit, merges
   the new nightly's standard library into `library/` ([The standard
   library](#the-standard-library-library)), and moves the pin in `install.sh`, `Cargo.toml`
   and the flake.
2. `harness/answers.py --write` on Linux x86-64, Linux AArch64 and macOS arm64, since the
   target specs depend on the host rustc was built for; the three are merged into
   `crates/blaze-units/src/stdlib/rustc.json`.
3. The pull request `toolchain: pin nightly-YYYY-MM-DD` is opened, or updated when one is open,
   which runs the merge gate, and the release train is started on the branch and on
   `toolchain/base`, the main commit the branch is on, to compare with. (main's own trains are
   cancelled by the next merge more often than not, and an older one has red that main gained
   since.) The train is
   what a bump needs beyond the gate: every harness on four platforms and `blaze` on real
   projects.
4. When the train finishes, the verdict (`.github/scripts/toolchain_bump_ci.py verdict`)
   compares its jobs with `toolchain/base`'s train. (v1's count of the functions the standard
   library refused, `stdcount.py`, was deleted on 2026-10-30: v2 compiles std's crates as units
   of the build like any other, and the train's harnesses build them.)
   - **green**: no job or harness red that is green on `toolchain/base`'s train (waited for, or
     started if there is none; main's newest finished train only if it was cancelled):
     `gh pr merge --auto`, so it lands once the gate passes, and
     the next release of blaze carries the new pin. The gate passed hours before, and GitHub
     acts on auto-merge only when a check finishes after it is set, so the gate's `ci` job runs
     again (seconds). A merge made for GITHUB_TOKEN starts no workflow, so once it merges the
     verdict dispatches on main every workflow a push to main starts (`train.yaml`,
     `blaze.yaml`, ...). A job red on main too is listed and not
     counted against the bump; the train's own issue covers it.
   - **red on the first attempt**: the train's failed jobs run again (`gh run rerun --failed`),
     and the verdict of that attempt decides, so a harness that fails one run in ten does not
     block a day's bump. A job red on main too runs again with them.
   - **red**: a comment on the pull request with what broke, and the issue
     "Toolchain bump to <date> is blocked", opened or commented on and retitled to the
     newest date, with how many days main's pin is behind. The pull request stays open.
   - **no verdict** (GitHub's API failing past its retries, a bug in the script): the same
     issue says so, with `gh workflow run toolchain-bump.yaml -f train=<run id>`, which counts
     and decides again with main's scripts.
5. A step before the pull request that fails (a vendoring conflict, a conflict merging the
   standard library, no usable nightly in two weeks, a conflict merging main into the bump) opens or comments on the same issue with what
   failed, and so does a bump open for a day without merging. Nothing is skipped quietly.
6. Once a bump has merged, `python3 harness/nightly_features.py '{}'` on main moves the
   tree of nightly features (#967) to the new pin: an issue for each new language feature, and
   a comment and a close on the issue of each one stabilized or removed. The workflow does not
   run it yet; it is run by hand after the merge.

Fixes for what a nightly breaks (layouts, the ABI, std's source, intrinsics, borrow-check
verdicts) are pushed to `toolchain/bump`. The next day's run merges main into it and moves it to
that day's nightly from there, so they are kept, and the train runs again.

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

Where blaze reads it (`crates/blaze-cargo/src/toolchain/library.rs`): it is known by its
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
  taken over the checkout too: `harness/libraryfetch.py` installs from an archive served
  locally that way, so the release's download is tried before a tag ships. Whichever way it
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
- The daily bump moves it with the pin as a merge (`harness/library.py merge`): it makes the
  new nightly's snapshot on `upstream/library` and applies upstream's changes from the old
  snapshot to the new one three-way, so blaze's own changes are kept, then pushes the snapshot
  with its branch. A conflict stops the bump, names the files and opens the bump's issue.

To change the standard library, edit it under `library/`, run
`python3 harness/library.py digest '{}'` (the test `the_library_is_the_one_its_digest_says`
fails on a stale `DIGEST`) and commit. The next build copies the changed library into
`~/.blaze/library/<new digest>`, and the store keys what it compiles of it apart from the old.
Nothing checks it against upstream file by file: the integration tests (smoke, the rustc
comparison harnesses, real crates) are what hold blaze's behaviour to the nightly's.

## How far behind it gets, and how long a bump takes

Measured 2026-10-27, on the first bump the workflow made.

- **Before the daily bump** the pin moved by hand: Rust 1.99.0 until 2026-10-25 (ce555b5f2d),
  then that day's nightly, two days old when the first automatic bump started.
- **Which nightly:** every one of the 120 nightlies up to 2026-10-27 had every component blaze
  needs on all four hosts (a scan of their manifests with `toolchain_bump.missing`), so the
  newest usable nightly is that day's. How far behind main is comes from the gates alone: the
  run at 05:40 UTC takes a nightly about 4 to 6 hours old, which lands hours later, so main's pin
  is under a day old on a green day and one day older for each red day.
- **The first bump** (to the nightly of 2026-10-27, #543): dispatched 11:40:01Z, merged 16:53:36Z, 5 h 14
  min. The bump and the answers of three hosts took 80 s. The train took 2 h 11 min (most of it
  the `projects (nushell)` jobs, red on main too, running into their 120-minute timeout), the
  verdict 32 min waiting for `toolchain/base`'s train, the rerun of one job red only on the bump
  (`remote.py`, a remote-cache flake) 2 h 1 min, and the verdict on it 3 min. blaze built on the
  new nightly with no change to its code: the standard library refused 1940 of 25960 functions
  against main's 1940 of 25942 (+0), borrow-check UI was green, and no harness was red that is
  green on main's train.
  The changes it needed were to the bump itself (#540, #578, #615, #657, #691, #699).
- **A typical day,** then: about 2 h 45 min from the run to the merge when the train has no new
  red, about 5 h with one rerun. The nushell jobs' timeout is most of each train while they are
  red on main.

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
- [#404](https://github.com/indexable-inc/blaze/issues/404): the bump, every day
  ([The daily bump](#the-daily-bump)), the `blaze-<date>` name moving with it.
