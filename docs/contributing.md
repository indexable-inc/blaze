---
status: current
summary: How to build, test and change blaze: AI use, where new code goes, rules for code, working in parallel, landing.
---
# Contributing

## AI use

This code was written largely with AI assistance under human direction. The same is true
of the harnesses, the reports under `fed1eb428d:docs/reports/` and these documents. The reports are
written in the first person by the session that did the work; "I ran" in a report means
that session ran it.

What makes that workable is the testing method. Every change is judged by real crates' own
test suites passing under blaze and by tests that state their expected result, and a change
that breaks one does not land. If you contribute with AI assistance, the expectation is the same as without: you
understand the change, you ran the gate, and you say what you ran and what you did not.

rust-analyzer has its own AI policy for contributions to rust-analyzer. It applies to what
you send them, including fixes you find here and want to upstream.

## Layout of the tree

The root holds only what a reader needs first or a tool requires there; everything else is
one level down:

    crates/                   blaze's crates, and the front end's (below)
    library/                  the standard library blaze builds, upstream's sources with blaze's changes
    vendor/                   third-party crates blaze patches or builds as dependencies
    harness/                  the test harnesses and their fixtures
    tools/                    what builds, releases and measures blaze: agents/ (the agent rules
                              AGENTS.md is generated from), nix/ (the flake's unit graphs), pgo/
                              (the pinned PGO profiles), bench/remote-cache/ (the cache gateway's
                              measurements), remote-cache/ (the cache gateway Worker), typed-input/ (the Python
                              workspace's shared helper), v2/ (the lanes' scripts and queue)
    docs/                     these documents; docs/future/ the plans for work not started,
                              fed1eb428d:docs/reports/ working reports, docs/v2/ the v2 plan and the lanes'
                              requests
    demo/                     small projects the harnesses build, and the live-reload demo
    fuzz/                     the fuzz targets and their corpora (docs/fuzzing.md)
    site/                     the website, and install.sh, the installer `curl | sh` and CI run
    licenses/                 NOTICE and the license texts of code blaze includes
    hegel.toml                Hegel's settings for the property tests (read from the root)
    Cargo.toml, pyproject.toml, flake.nix
                              the Rust, Python (uv) and Nix workspaces, each with its lock

The crates to know first:

    crates/blaze-engine/       the program `blaze`: what runs before blaze's cargo, and pinning
    crates/blaze-cargo/        blaze's cargo: the client (`cli`), the daemon (`serve`), the sandbox
    crates/hir-ty-codegen/    the code generator: MIR to Cranelift IR, the producers
    crates/blaze-cas/        the content-addressed store of everything blaze keeps, which knows nothing about Rust
    crates/blaze-fuzz/       made-up and damaged inputs for the tests of what reads the store
    crates/blaze-build-script/ build scripts run as cargo runs them, with no cargo (no-rustc.md, step I)
    crates/blaze-resolve/     cargo's resolver owned by blaze: the package model, Cargo.lock, versions, features
    crates/blaze-registry/    registries read and packages fetched as cargo does, with no cargo
    .github/SECURITY.md       what to know before sharing a store, and how to report a problem
    crates/hir-def/, hir-ty/, syntax/, parser/, ...
                              the front end: crates derived from rust-analyzer (below)
    crates/hir-ty-borrowck/   the borrow checker, a crate next to hir-ty (borrowck.md)
    crates/syntax-tree/             rowan 0.17.0 with a flat kind of tree behind its cursor (flat-syntax.md)
    crates/rustc_abi/         rustc's layout crate at the pinned nightly's commit
    vendor/source.rev         the commit of the old development branch this tree last took from

`crates/hir-ty-codegen` is the code generator, a crate of its own over `hir-ty`'s MIR: what
it uses of `hir-ty` is public there. Its root is `mod.rs`, the file it was compiled from when
it was a module of `hir-ty`. The front end's crates depend on none of blaze's machinery
(the code generator, the store, the daemon).

## rust-analyzer

The front end's crates in `crates/` (`hir-ty`, `hir-def`, `hir-expand`, `syntax`, `parser`,
`base-db`, `syntax-tree` and the rest) started as a fork of rust-analyzer (DioxusLabs' rustd branch
merged with upstream master at `0b21bd6596`, with the MIR passes that make its MIR like rustc's
on top: [mir-passes.md](mir-passes.md)); until 2026-10-28 they were a subtree under vendor/.
They are no longer synced with upstream or with a fork repository, and keep rust-analyzer's crate
names. Edit them, build and test from the root, and commit like any other file. A plain clone
builds:

    git clone https://github.com/indexable-inc/blaze && cd blaze
    cargo +"$(cat harness/toolchain)" build -p blaze-engine --release   # cargo +nightly-2026-10-27

That is stage 0, `blaze 0.0.0-dev`. The root `Cargo.toml` pins a release of blaze
(`[workspace.metadata.blaze] version`) for everyone who runs a released `blaze` here, and a
build of the tree runs itself in any checkout of blaze's tree (told by
`crates/blaze-engine/Cargo.toml` naming `blaze-engine`, not by path), whatever that pins, so
stage 0 builds blaze with no flag:

    target/release/blaze build -p blaze-engine --release   # stage 1, built by blaze

That holds for a stage 0 built from another checkout or copy of the tree (a worktree, a VM's
copy at another path, the wasm seed) too. Elsewhere, in a project that pins a release, a build
of the tree runs that release, saying so on stderr; `blaze --ignore-pin <command>` runs the
build itself ([cargo.md](cargo.md#pinning-a-release-of-blaze)).

`crates/rustc_abi` is rustc's `rustc_abi` at the pinned nightly's commit, patched in for
crates.io's `ra-ap-rustc_abi`, the same way. The fork repositories
(indexable-inc/rust-analyzer, indexable-inc/rustc_abi) are archived.

### Where new code goes

New code that is the language's semantics goes in the front end's crates: building MIR and
the passes over it, borrow checking, const evaluation, layout, and the hashes that say what a
function's code is made from, as queries on blaze's own engine (`blaze_query::db`, derived from salsa). These are what rustc keeps in `rustc_middle`,
`rustc_mir_transform` and `rustc_borrowck`; rust-analyzer could take them, and a query lives
with the database it reads. Where rust-analyzer's MIR differs from rustc's, the fix goes in
the fork's lowering or in a pass of its runtime MIR queries, and the code generator relies on
it: codegen does not compensate.

There, a crate of its own is better than growing `hir-ty` or `hir-def`, which are already the
longest builds in the workspace.

blaze's machinery stays out of the front end, in crates of its own: the code generator's backends, object and debug-info
writing, the linker, the daemon and its socket, the cargo drop-in, the store and the remote,
the sandbox, and the harnesses. A change there is one ordinary
commit.

New code never adds a path to rustc ([no-rustc.md](no-rustc.md)). blaze compiles
everything itself, so a change does not start rustc, rustdoc or a cargo that compiles, does not
read `RUSTC` or `RUSTDOC`, does not add a reason for a build to go to cargo or a kind of function
to leave to rustc, and does not use rustc's rlibs, objects, symbols or link line for anything
new. Where a feature needs what rustc does, the change builds that piece in blaze, or waits for
it. The paths that remain may be fixed where they are wrong, and are otherwise only removed.

### Where the history is

`main` began as a tree generated from a development branch that was rust-analyzer's tree
with blaze's changes made in place. That branch and its history are kept as
`blaze-collate` and `blaze-spike`. `publish/split.py` there is what wrote this layout.

## The toolchain

blaze supports one toolchain, the latest nightly, pinned by date in `harness/toolchain`
(`nightly-2026-10-27`, which says `rustc 1.101.0-nightly (16d1ffac1 2026-10-26)`). Stable
Rust is not a separate target: what a stable release builds, the nightly builds. blaze
itself is built with that nightly by blaze, which brings it (`blaze toolchain`): no rustup, and
no `cargo` or `rustc` of it is run. The pin is written in that
one file; everything else reads it or is written from it:

- `crates/blaze-cargo/src/toolchain/pins.txt` (by `harness/toolchain_pins.py '{"write": true}'`): the
  sha256 and size of each file blaze downloads as its own toolchain, the nightly's `rustc -V`
  line and its rust-lang/rust commit. A rustc is taken for the pinned one by that whole line,
  since every nightly of a release has the same number.
- What mirrors rustc, all from that commit: `crates/rustc_abi` (rustc's `compiler/rustc_abi`),
  `crates/blaze-macro-eval` (blaze's proc-macro server, which speaks the pinned
  `library/proc_macro` bridge's protocol by hand and checks it at load), the answers to rustc's
  questions (`crates/blaze-units/src/stdlib/rustc.json`), and the code written by hand to agree with rustc:
  the Rust ABI in the code generator, `vtable.rs`, `type_id.rs`, symbol names (v0) in
  `symbols.rs`, the intrinsics and lang items, the test harness's call into libtest.
- `site/install.sh`'s `TOOLCHAIN`, the flake (`rust-bin.nightly.<date>`, for the dev shells and the
  bench nightly alike) and `Cargo.toml`'s `rust-version`.

### Moving to a newer nightly

The daily workflow does this every day and opens the pull request
([toolchain.md, "The daily bump"](toolchain.md#the-daily-bump)); by hand it is for fixing what
a nightly breaks, on the bump's branch `toolchain/bump`, or for trying a nightly:

    uv run --frozen python harness/toolchain_bump.py '{}'                      # the newest nightly blaze can use
    uv run --frozen python harness/toolchain_bump.py '{"date": "2026-11-02"}'  # or a given one

picks the nightly (the newest of the last two weeks whose manifest has every component blaze
needs on every host it ships for, or the date given), writes `harness/toolchain` and
`pins.txt`, re-vendors `crates/rustc_abi` from rust-lang/rust at the new commit, merges that
nightly's standard library into `library/` (`harness/library.py merge`: upstream's changes applied three-way, so blaze's own changes to it
are kept; a conflict stops it and names the files), moves the pin where else it is written (`site/install.sh`, the
server's version string, `Cargo.toml`, the flake's rust-overlay input) and
installs the nightly with rustup. Each step names itself when it fails.

What it leaves to a person, and prints at the end:

1. `python3 harness/answers.py --write` on x86-64 Linux, AArch64 Linux and arm64 macOS, since
   the target specs depend on the host rustc was built for; then
   `uv run --frozen python tools/cpus.py '{"write": true}'` for the CPU table
   (`crates/target-features/src/cpus/table.rs`); commit both.
2. `blaze build -p blaze-engine --release`, fixing what the new `rustc_abi` and standard library
   ask for, and mirroring by hand what changed in rustc: the Rust ABI, vtables, `TypeId`,
   symbol names, new intrinsics and lang items. The move to `nightly-2026-10-25` needed
   `rustc_abi`'s renamed layout calls, vectors of at most 128 bits passed in registers on
   x86-64 and AArch64, Windows x64's rule for zero-sized types, the `f16b` and `ppcf128`
   layouts, libtest's new entry `test_main_env_args` and the intrinsic `abort_immediate`.
3. `uv run --frozen python harness/toolchain_bump.py checks '{"stage0": "/path/to/blaze"}'` in an
   ix VM, with a blaze that builds the tree as stage 0, which has LLVM as every native blaze
   does: the profile picks the backend, and the harnesses that cover LLVM build the release
   profile. It builds blaze with that blaze (never cargo) and runs, in parallel,
   `toolchain_pins.py` and the gate's quick parts.
4. The full tier re-measured at the new commit: the train and the crates' own test suites.
5. One commit for the bump, landed like any other change.

A blaze built before the bump refuses the new nightly until it is rebuilt, since the two
match by the whole version line.

## Rules for code

1. **rustc is the reference.** If blaze's program behaves differently from rustc's, blaze
   is wrong, even where rustc's behaviour looks like an accident. That is proven by real
   crates' own test suites passing under blaze and by tests that state their expected result,
   not by harnesses that run rustc to compare with (deleted on 2026-10-30); blaze itself never
   runs rustc ([no-rustc.md](no-rustc.md)).
2. **Never guess.** Input the code generator has no explicit lowering for is refused with a
   reason (`CodegenError`), not lowered on a best-effort basis. A refusal is visible and
   counted. A wrong guess is a miscompile nobody sees.
3. **A refused function is a bug**, to be listed and fixed, not a normal path to design
   around.
4. **General fixes.** A fix that tests for one crate, one function name or one test is not
   a fix. Find what the case is an instance of.

Style follows rust-analyzer's guide
([style.md](https://github.com/rust-lang/rust-analyzer/blob/master/docs/book/src/contributing/style.md)): `cargo fmt`, no new
dependencies without a reason, small focused changes. Comments say what the code is for and
why, not what was changed. Messages printed to the user say what happened and what blaze
did about it in one line.

When a build is slow, reruns what it should reuse, holds too much memory or a cycle will not
settle, find out from the profile, not by guessing: the `debugging` skill
([.agents/skills/debugging/SKILL.md](../.agents/skills/debugging/SKILL.md)) says where its
files are, what they hold, the DuckDB queries that answer each question and the method.

## What a change needs

- A test that fails before and passes after: the real crate's own tests for a bug found in
  it, a unit test beside the code for one small enough to state, a step in the relevant edit
  harness for a daemon bug ([harness.md](harness.md#adding-a-test)).
- For concurrent, durable, crash-recovering, billing or data-deleting code: a Hegel
  deterministic simulation test whose oracle checks the invariant, with a planted bug it
  catches ([Tests](#tests)). A review does not stand in for it.
- The gate, under Cranelift and under LLVM, on the platform you have. Say which platforms
  you did not run. CI's merge gate is the build and the unit tests on Linux AArch64 (on ix runners), and
  `blaze check --workspace` with the released blaze on Linux AArch64 and macOS, with astlog's
  rules after it (`tools/astlog/README.md`), and the lint baselines held by the blaze the
  change builds ([cargo.md](cargo.md#lints)); every
  harness on every platform, Windows too, and the slow ones (`norustc.py`, the store,
  `blaze_cli.py` on real projects) run in the release train on
  main after the merge, and a red train opens or adds to the issue "Release train is red",
  with the commits since the last green one ([harness.md](harness.md#ci)). The gate letting a
  change through is not the harnesses passing on it.
- blaze builds blaze, never cargo or rustc: with the release `.github/blaze-version` names as
  stage 0 (`sh site/install.sh --version "$(cat .github/blaze-version)"`, as
  `.github/actions/blaze` installs it), `blaze build --release --locked -p blaze-engine`, against
  the LLVM blaze pins and fetches itself ([LLVM](#llvm)).
- `blaze fmt --all` (nothing checks it, and `fmt.yaml` formats main after the merge, but a
  pull request's diff should be its change), and
  `blaze test --release --locked -p blaze-engine -p hir-ty -p hir-def -p parser -p syntax` from the root (or
  `python3 harness/unittests.py`, what CI runs: every test of the workspace, with the failures it expects).
  A test that fails is new unless `harness/known-failures.toml` lists it:
  `uv run --frozen harness/failures.py '{"run": {"packages": [...]}}'` runs the
  crates you touched and says which failures are new ([harness.md](harness.md#ci)).
- For anything that changes numbers in [results.md](results.md): the command, the commit
  and the machine, so the entry can be replaced and not just appended to.

When you report what you did, separate what you ran from what you inferred. The reports
under `fed1eb428d:docs/reports/` do this and it is the reason they can be used.

### Checking a change fast

Code is checked as it is written, a commit at a time, on the packages it touched. The
workspace's build and check take an hour and, with the released blaze, more memory than most
machines have; when that is the only loop, thousands of lines are written before anything
compiles them, and the first error (2026-11-10: a shadowed binding, then a struct named like
its crate) arrives an hour in, with the next one an hour after.

`harness/v2/changed.py` names what a change touches: the packages its files are in and the
packages that depend on them, each after what it needs, so the first error is the earliest
cause.

    blaze check $(uv run --frozen python harness/v2/changed.py '{"base": "origin/main", "print": "args"}') --lib --tests

- The session that integrates agents' work (rule fan-out-write-only) runs that in the lane's
  VM after each commit it takes, with `"base"` the commit before, in parallel with the build:
  the check of a few packages does not wait for the build of all of them, and an error goes
  back to the agent that wrote the code while it still has it in mind. `"dependents": false`
  checks the touched packages alone, for a first answer; the dependents follow.
- A change to a file every package's check reads (the root manifest, the lock file,
  `.clippy.toml`, the pinned toolchain) is the workspace's: the script prints `--workspace`.
- The gate's workspace check does the same first, so a pull request's first compile error
  comes in minutes ([harness.md](harness.md#ci)).

### Scripts that run commands

A harness, a lane script or an agent's loop checks the exit status of every command it runs and
stops at the first that fails, saying which; a command whose failure is expected says so where
it is run (`|| true` with a comment, a `check=False` whose result is read on the next line). On
2026-11-10 a script called `blaze daemon stop`, which is no command, for hours: the command line
said so and exited 101, the script went on, and every measurement after it ran against the
daemon it believed stopped. In a shell script that is `set -euo pipefail` on its first line; in
Python, `subprocess.run(..., check=True)` or the harness's own `engine.run(..., check=True)`
and `Ran.checked`. The command line now names the command meant (`blaze daemon stop`: did you
mean `blaze stop`), which helps a person; a script is helped only by stopping.

### Tests

How blaze tests its own code is the `testing` skill
([.agents/skills/testing/SKILL.md](../.agents/skills/testing/SKILL.md)), the one home for it:
which tests to write and how few, property tests with Hegel (vendor/hegeltest, imported as
`hegel`; never proptest, quickcheck, arbitrary or bolero), deterministic simulation tests on
`blaze_core::sim::Sim` and their oracles, and testing a causal claim. Concurrent, durable,
crash-recovering, billing and data-deleting code is proven by a deterministic simulation test,
not by a code review (rule `dst-not-review`).

## Documents

Every doc under `docs/` opens with front matter saying what it is (`tools/docs.py` has the
fields): `current` (how blaze is now, kept true), `plan` (a design not fully built, dated),
`record` (research, a report or a measurement, true on its date and never brought up to date) or
`superseded` (with `superseded_by`, what replaced it). [docs/README.md](README.md) is the index,
generated from it; superseded docs are left out. CI's `python3 tools/docs.py check` holds every
doc's links, and a current doc's anchors, repository paths in backticks and `BLAZE_*` settings,
against the tree, and every doc that code names. A change that makes a current doc
untrue fixes it in the same change, or marks it superseded; `python3 tools/docs.py write`
regenerates the index.

## Working in parallel

Several lanes, people or agents, change blaze at once. A lane shares `git`'s metadata and
main with all the others, so:

- **Work comes from the ledger.** A lane is one task: an objective of
  [v2/plan/objectives.md](v2/plan/objectives.md), done when its check passes. How a session
  picks, briefs, integrates and lands is [the plan's loop](v2/plan.md#how-the-work-is-run).
- **Builds, tests, benchmarks and experiments run on the Mac or in the lane's ix VM**, never
  on an AWS dev host or any other AWS machine. Anything large (a cold build, a harness, a
  benchmark) goes to the VM: blaze's daemons have run the Mac out of memory.
  - Make it: `ix new --name <lane> --no-shell`. The default image is NixOS with `nix`, `git`
    and `gh`; `nix develop -c ...` in the checkout brings up `harness/toolchain`'s nightly
    (about 30 s the first time). `--cpus` (2 to 128, the default is the architecture's cap) and
    `--memory` (a ceiling in GiB it grows to on demand; add `--memory-commit` for a build,
    whose memory ramps faster than that) size it, `--arch x86_64` or `--arch arm64` picks the
    platform, and `--ttl 8h` deletes it on its own.
  - Use it: `ix shell <lane> -- <command>`. Its arguments go through as they are (no remote
    shell parses them again, as ssh's does) and stdin streams, so send a checkout with
    `git bundle create - <branch> | ix shell <lane> -- bash -c 'cat > src.bundle && git clone
    -b <branch> src.bundle src'` (git can't clone a bundle from a pipe).
    The VM has no GitHub or AWS credentials. For a benchmark, give it its own VM, pinned to a
    host CPU class (`--cpu epyc-5`, `xeon-6` or `graviton-5`) with `--memory-commit`, and build
    nothing else there.
  - Remove it: `ix rm <lane>` when the lane is done.
  - Where neither can do something, stop and say what is missing; never take it to AWS.
- **One worktree per lane, named for it** (`blaze-<lane>-<date>`). Its build directory is its
  own without a `CARGO_TARGET_DIR`: blaze builds each workspace root into a directory of its
  own under blaze's root, never in the checkout ([cargo.md](cargo.md#where-a-build-writes)).
  Two lanes that pass one `CARGO_TARGET_DIR` still wait on each other's lock and overwrite each
  other's binaries.
- **Never `git stash`.** The stash is one stack for every worktree of the repository, so a pop
  can take another lane's changes. Commit work in progress to the lane's branch.
- **Run harnesses with `RUST_BACKTRACE=0`**, so a test that panics prints its message and
  not a backtrace into what the harness compares.
- **Land small and often, through a pull request** ([below](#landing-a-change)). A lane that
  waits hours for one large merge meets every other lane's changes at once. For anything the
  fast tier does not cover, run the release train on the branch first
  (`gh workflow run train.yaml --ref <branch>`: everything, without the release).
- **Clean up when done:** remove the worktree's build directory (`blaze clean` in it), the
  worktree (`git worktree remove`) and the lane's VM (`ix rm <lane>`); delete the merged branch.

## Landing a change

Main moves only through pull requests: nobody pushes to it or merges into it by hand. Once the
branch's work is committed and the local gate passed ([What a change needs](#what-a-change-needs)),
run in the lane's worktree

    python3 .github/scripts/land.py "what the branch does"

It pushes the branch, opens its pull request ("Merge <branch>: what the branch does") and turns
on auto-merge. The pull request's own run is the merge gate, the build and the unit tests on
Linux x86-64 and the released blaze's `blaze check --workspace` on Linux x86-64 and macOS, with
astlog's rules after it, and the lint baselines ([harness.md](harness.md#ci)); a failure there ends `land.py`
with the check's link. The gate also holds the budget smoke's guards with the blaze the change
built ([harness.md](harness.md#the-smokes-guards)): what a warm no-op build, a comment edit and a
body edit of two small fixtures may cost, and that blaze runs inside its build-script sandbox. A
merge that goes around the gate goes around them: on 2026-11-10, with 4 of the last 300 gate
runs green and merges going around it, every build had been lowering its program anew for days.
A red gate is fixed or its failure is listed as known with its cause, never passed by. Once it passes, GitHub merges the branch with a merge commit, as
`git merge --no-ff` would. There is no merge queue and the branch need not be up to date with
main: two changes that each pass but break together are caught on main by the release train.
`land.py` waits for the merge and prints the commit, or says why the pull request did not
merge; then fix the branch and run it again. The waiting runs in a process of
its own that outlives `land.py`, its terminal and a tool that ends its process group, writing to
`.git/land/pr-N.log` in the main checkout; `land.py --wait` (or `--wait N`) follows it again, as
does running `land.py` again on the branch, so nobody polls GitHub by hand. A branch that conflicts with main
gets `git merge origin/main` first. Once it has merged, GitHub deletes the branch on origin;
remove the worktree and the local branch
(`git worktree remove <dir> && git branch -D <branch>`).

## The build that ships

`blaze build --release` is the build to work with: the harnesses, the gate and CI use it, and
it rebuilds quickly after a change. What users get is one other build: the `dist` profile,
built by the stage-0 blaze `.github/blaze-version` names, through
`.github/scripts/build_release.py` (one JSON input, `{"target": ..., "out": ...}`). The release
workflow and install.yaml run that script; blaze builds blaze, never cargo or rustc. `dist`'s
`lto = "thin"` and `codegen-units = 1` are what blaze's release codegen does anyway (one LLVM unit
per function with ThinLTO's summaries and imports across the program, [v2-codegen.md](v2/plan/design/codegen.md)).
reports/RELEASE-BUILD.md has the numbers of the older cargo build.

PGO is off: releases build with `"pgo": "off"`. The `pgo` workflow trains profiles with
`tools/pgo/pgo.py`, which builds blaze instrumented with the stage-0 blaze, and `"pgo": "pinned"`
builds with the profile `tools/pgo/profiles.json` pins, only one trained on blaze's own build
(`"builder": "blaze"`). Neither works yet: blaze refuses `-C profile-generate`, since its linker
writes no profile sections (`crates/blaze-cargo/src/serve/unit/codegen.rs`, `pgo`), and the
profiles pinned today are cargo's, which the script refuses with that error.

## LLVM

blaze builds against one LLVM, 23.1.4 with our fixes, built by blaze for each target it ships on
and pinned by its commit, version and per-target sha256 in `crates/hir-ty-codegen/llvm.json`.
Every native build of blaze links it (blaze-engine's default `native` feature, through
hir-ty-codegen's `llvm`): std and every opt-level 2/3 unit are LLVM's, and `blaze-stencil`'s
build script has it make the stencils. Only the seed (`--no-default-features --features seed`)
has none. blaze fetches it itself: the root `Cargo.toml` declares it a pinned native input
(`[workspace.metadata.blaze.native.llvm]`, [cargo.md](cargo.md#pinned-native-inputs)), so the
first build downloads the archive `llvm.json` names for the target, checks its sha256, keeps it
in blaze's store and unpacks it read-only under `~/.blaze/native/` (`BLAZE_HOME/native/`), and
sets `LLVM_SYS_231_PREFIX` to it for llvm-sys's and blaze-stencil's build scripts. A plain
build on a clean Mac or Linux machine needs no Nix and no variable:

    blaze build -p blaze-engine --release

The variable blaze sets wins over one your shell has (`nix develop` still sets it to the
flake's `blaze-llvm`, the same bytes), so a build always links what the pin names. A stage 0
older than the table (`.github/blaze-version` before this change) ignores it and still needs the
variable, with the archive somewhere its sandbox lets llvm-sys search, below `/usr/local/lib`
(`crates/blaze-cargo/src/directives.rs`, `SYSTEM`), as CI's merge gate does
(`.github/actions/blaze`):

    sudo mkdir -p /usr/local/lib/blaze-llvm && sudo chown "$USER" /usr/local/lib/blaze-llvm
    python3 .github/scripts/llvm.py '{"command": "fetch", "target": "<triple>", "dir": "/usr/local/lib/blaze-llvm/<triple>"}'
    LLVM_SYS_231_PREFIX=/usr/local/lib/blaze-llvm/<triple> blaze build -p blaze-engine --release

Every crate that uses LLVM (`hir-ty-codegen` through inkwell, `blaze-stencil`'s build script)
links it through llvm-sys, whose build script checks the prefix before anything else
(`vendor/llvm-sys/pin.rs`): its `blaze-llvm.json` must name the pinned commit and version and
its `llvm-config --version` must say that version. `LLVM_SYS_231_PREFIX` unset, a stock LLVM
(no `blaze-llvm.json`) or another build stops the build with one error naming what it found,
the pin and the fix. Nothing else is searched: not `PATH`, Homebrew's LLVM, a distribution's or
another `LLVM_SYS_*_PREFIX`. LLVM is linked in statically, and on Linux so
is libstdc++, so a blaze binary needs no LLVM where it runs. inkwell's llvm-sys
does the linking (vendored in `vendor/llvm-sys` for the static libstdc++; its SOURCE.md says why
the libraries must come from a crate below inkwell for GNU ld). CI and Windows
fetch the archive the same way (`.github/actions/toolchain`).

The archives are built by the `llvm` workflow (`.github/workflows/llvm.yaml`, recipe
`.github/scripts/llvm.py build`): our fork `indexable-inc/llvm` at the `ix/23.x` commit
wasm-host's ix-aot pins (LLVM 23.1.4 and our fixes, so both run one LLVM), the X86 and AArch64 targets,
no zlib, zstd, libxml2 or terminfo, on the release's runners (Ubuntu 22.04 for an old glibc,
macOS 13 as the deployment target, the static CRT on Windows). For a target `llvm.json` has no
archive for, run the workflow for it and add the `targets` entry its summary prints. To move the pin, change
`ref`, `commit` and `version` in `llvm.json`, run the workflow, and write the `targets` its
summary prints (`llvm.py` with `{"command": "pin", "root": DIR}`), then write the new LLVM's CPU aliases (`apple-m1` of `apple-a14`, which
`-C target-cpu` takes and `rustc --print target-cpus` leaves out) with
`uv run --frozen python tools/cpus.py '{"write": true, "fetch_llvm": true}'`, which writes
crates/target-features/llvm_cpus.json and the CPU table (`target_features::cpus`) from it. LLVM's own release archives do not do: the macOS ones hold
bitcode, and the Linux ones need zlib, zstd and libxml2.

### The stencil pin

`crates/blaze-stencil/stencils.json` pins the stencil libraries the way `llvm.json` pins LLVM:
each library's file and its id, the BLAKE3 hash of its bytes with the id field zeroed.
`blaze-stencil`'s build script checks every library it includes against the pin. It checks that
the library hashes to the id in its header, and that this id is the pinned one:

- A build with LLVM (every native one: hir-ty-codegen's `llvm` turns on blaze-stencil's
  `llvm-stencils`) makes the libraries and writes their
  pin beside them, as `stencils.json` in blaze-stencil's `OUT_DIR`. When that differs from the
  tree's pin, the build stops and names each id. When a change to the generator (`gen/`,
  `src/shape.rs`, `src/enc.rs`, `src/table/format.rs`, LLVM) changes what it makes, pin the
  result in the same change with the command the error prints:

      cp <blaze-stencil's OUT_DIR>/stencils.json crates/blaze-stencil/stencils.json && cp <blaze-stencil's OUT_DIR>/*.stencils crates/blaze-stencil/prebuilt/

  The libraries themselves are committed in `crates/blaze-stencil/prebuilt/` (about 570 KB
  each), and the build also stops when what it made differs from them. If nothing the
  generator reads changed, the output isn't deterministic, which is a bug (rule 25).
- The seed profile (`blaze-engine --no-default-features --features seed`) has no LLVM. It
  includes the committed libraries in `crates/blaze-stencil/prebuilt/`; no variable points
  elsewhere. A library whose bytes aren't the pinned ones is refused, and the error names the
  file, the pinned id and the file's own id.

The code identity (`blaze-codegen-code`) is keyed by the pinned ids.

## Credit

A commit or pull request that carries someone else's idea, diagnosis, workaround or patch
(from an issue, a review, a discussion or another pull request) names them with a
`Co-authored-by:` trailer, one line per person, after a blank line at the end of the
message, so GitHub shows them as an author:

```
Co-authored-by: Name <id+login@users.noreply.github.com>
```

Use the address their own commits use, or GitHub's private one built from
`gh api users/<login> -q .id` and their login. Name the issue or comment the idea came
from in the message too. When a squash or a merge rewrites commits, keep every trailer.

## Licence

blaze is MIT or Apache-2.0, like rust-analyzer. Contributions are accepted under the same
terms.
