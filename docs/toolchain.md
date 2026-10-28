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

blaze compiles your code itself (Cranelift or LLVM, from rust-analyzer's MIR) and links it with
code rustc compiled. The two must agree on how types are laid out, how functions are called,
what a `TypeId` hashes, the names of symbols, libtest's entry point, the proc-macro bridge and
what rustc answers about targets. blaze knows those facts for one toolchain, checked by the
corpus difftest, and that toolchain is the base blaze replaces piece by piece
([no-rustc.md](no-rustc.md): cargo, then rustc, then the standard library's build). Following
each project's pin would mean knowing every release's facts and checking every release; the
goal is the reverse, no rustc at all. So the toolchain is part of blaze, versioned with it, like
the standard library is part of a compiler.

## What it is today

- **Name:** `blaze-<date>` (`blaze-2026-10-27`), after the date of the Rust nightly it is built
  from (`harness/toolchain`, `nightly-2026-10-27`). `blaze toolchain` prints it and where it
  is; `blaze toolchain --path` prints the directory alone.
- **Bytes:** the upstream nightly's `rustc`, `cargo`, `rust-std` (the host's, and `wasm32-wasip1`'s,
  which proc macros are built for) and `rust-src`, unchanged, from
  static.rust-lang.org, each checked against the sha256 compiled into blaze
  (`crates/blaze-cargo/src/cargo/toolchain/pins.txt`, `harness/toolchain_pins.py`). So `rustc -V`
  still says `1.101.0-nightly (16d1ffac1 2026-10-26)`.
- **Where:** `~/.blaze/toolchains/nightly-2026-10-27-<host>`, or rustup's `nightly-2026-10-27`
  when rustup already has it with `rust-src` (the same bytes). The installer downloads it once;
  `blaze toolchain --path` prints its directory.
- **Who uses it:** cargo, rustc and the daemon that blaze starts (`CARGO`, `RUSTC`, `RUSTDOC`,
  `RUSTUP_TOOLCHAIN`, `PATH` for its own processes), whatever they were set to. The user's
  `cargo`, `rustc` and pins are untouched, and blaze reads none of them.
- **What rustc says:** blaze answers every question asked of rustc (`--version`, `-vV`,
  `--print cfg|sysroot|target-libdir|target-spec-json|...`) itself, for this release, whoever
  asks: cargo through blaze's wrapper, rust-analyzer in process, a build script through blaze's
  rustc stand-in, which also checks the probes build scripts compile (`autocfg`, `rustversion`,
  `version_check`, `anyhow`'s and `thiserror`'s). No rustc is run to find any of it
  ([no-rustc.md](no-rustc.md), step B).
- **Bumps:** every day, to the newest nightly, by a workflow, with the merge gate and the
  release train as the gate ([The daily bump](#the-daily-bump)).

## The daily bump

blaze tracks the latest Rust nightly. `.github/workflows/toolchain-bump.yaml` runs every day at
05:40 UTC (nightlies are published at about 00:00 to 02:00 UTC), and by hand with a date
(`gh workflow run toolchain-bump.yaml -f date=2026-11-02`):

1. On the branch `toolchain/bump` (the open bump's with main merged in, or a new one from main),
   `harness/toolchain_bump.py` pins the newest nightly whose manifest has every component blaze
   needs on every host it ships for. It writes `harness/toolchain` and `pins.txt`, re-vendors
   `crates/rustc_abi` and the proc-macro server from rust-lang/rust at the new commit, merges
   the new nightly's standard library into `library/` ([The standard
   library](#the-standard-library-library)), and moves the pin in `install.sh`, the server's
   manifest, `Cargo.toml` and the flake.
2. `harness/answers.py --write` on Linux x86-64, Linux AArch64 and macOS arm64, since the
   target specs depend on the host rustc was built for; the three are merged into
   `answers/rustc.json`.
3. The pull request `toolchain: pin nightly-YYYY-MM-DD` is opened, or updated when one is open,
   which runs the merge gate, and the release train is started on the branch and on
   `toolchain/base`, the main commit the branch is on, to compare with. (main's own trains are
   cancelled by the next merge more often than not, and an older one has red that main gained
   since.) The train is
   what a bump needs beyond the gate: the corpus difftest with `--layouts` and every other
   harness on four platforms, rustc's borrow-check UI tests in both modes against the new
   nightly's own verdicts (at its commit, `--min-pass` as the floor) and
   `blaze` on real projects.
4. When the train finishes, `harness/stdcount.py` compiles the standard library with the bump's
   build and with `toolchain/base`'s (the same main commit, the old pin), each against its own
   nightly's `rust-src`, and counts the functions each refuses. Against main's newest build
   instead, the first bump counted +1463 that were main's std fixes since the bump's commit. The verdict (`.github/scripts/toolchain_bump_ci.py verdict`):
   - **green**: no job or harness red that is green on `toolchain/base`'s train (waited for, or
     started if there is none; main's newest finished train only if it was cancelled), and no
     more std refusals than main: `gh pr merge --auto`, so it lands once the gate passes, and
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
paths, which panics and `file!()` show, are the ones `rust-src` gives. Until blaze reads it
from here (the next step of #402), it compiles the nightly's `rust-src`, whose files these are.

- `library/UPSTREAM` says which: the commit, the nightly and the sha256 of its `rust-src`,
  read from the nightly's channel manifest (`harness/library.py import` wrote it).
- It is not a member of blaze's workspace: no fmt, clippy or test of blaze's reads it, and
  `.gitattributes` keeps upstream's bytes.
- The daily bump moves it with the pin as a merge (`harness/library.py merge`): upstream's
  changes from the old nightly to the new one are applied three-way, so blaze's own changes
  are kept. A conflict stops the bump, names the files and opens the bump's issue.

To change the standard library, edit it under `library/` and commit. Nothing checks it against
upstream file by file: the integration tests (smoke, the rustc comparison harnesses, real
crates) are what hold blaze's behaviour to the nightly's.

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
  not read: blaze runs nothing to learn what it names. Under `-v`, a rust-toolchain file blaze
  passes over gets one line: `blaze: <file> is ignored: blaze builds with its own Rust, ...`.
- `edition` applies as cargo applies it. A workspace member whose `rust-version` is newer than
  blaze's Rust is one error naming both versions and saying to update blaze (and pin it);
  `--ignore-rust-version` builds it anyway, as with cargo.
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
with rust-lld and the proc-macro server, then `rust-std`; what is left is `rust-src`, shipped as
part of blaze, and the toolchain is blaze alone.

- [#402](https://github.com/indexable-inc/blaze/issues/402): ship the standard library's
  source (`rust-src` of the pinned nightly) with blaze's release, so blaze's toolchain needs no
  upstream download once rustc is gone.
- [#403](https://github.com/indexable-inc/blaze/issues/403): shrink the download as each
  no-rustc step removes a piece (`cargo`, `rustc`, `rust-std`), down to blaze alone.
- [#404](https://github.com/indexable-inc/blaze/issues/404): the bump, every day
  ([The daily bump](#the-daily-bump)), the `blaze-<date>` name moving with it.
