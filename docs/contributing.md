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
    tools/                    what builds, releases and measures blaze: nix/ (the flake's unit graphs), pgo/
                              (the pinned PGO profiles), typed-input/ (the Python workspace's shared
                              helper), v2/ (the lanes' scripts and queue)
    docs/                     these documents; docs/future/ the plans for work not started,
                              fed1eb428d:docs/reports/ working reports, docs/v2/ the v2 plan and the lanes'
                              requests
    demo/                     small projects the harnesses build, and the live-reload demo
    site/                     the website, and install.sh, the installer `curl | sh` and CI run
    licenses/                 NOTICE and the license texts of code blaze includes
    Cargo.toml, pyproject.toml, flake.nix
                              the Rust, Python (uv) and Nix workspaces, each with its lock

The crates to know first:

    crates/blaze-cli/          the program `blaze` (its commands: crates/blaze-cli-spec/)
    crates/blaze-engine/       the execution engine: one driver, tasks that end Done, Need or Failed
    crates/blaze-cas/        the content-addressed store of everything blaze keeps, which knows nothing about Rust
    crates/blaze-fuzz/       made-up and damaged inputs for the tests of what reads the store
    crates/blaze-build-script/ build scripts run as cargo runs them, with no cargo (no-rustc.md, step I)
    crates/blaze-resolve/     cargo's resolver owned by blaze: the package model, Cargo.lock, versions, features
    .github/SECURITY.md       what to know before sharing a store, and how to report a problem
    crates/syntax/, parser/, stdx/
                              rust-analyzer's parser and syntax tree (below)
    crates/blaze-scope/, blaze-ty/, blaze-typeck/, blaze-borrow/, ...
                              the front end (Where new code goes)
    crates/syntax-tree/             rowan 0.17.0 with a flat kind of tree behind its cursor (flat-syntax.md)
    crates/rustc_abi/         rustc's layout crate at the pinned nightly's commit
    vendor/source.rev         the commit of the old development branch this tree last took from

`crates/blaze-emit` and `crates/blaze-build` form the code generator over `crates/blaze-body`
and `crates/blaze-lir`. The front end's crates depend on none of blaze's machinery
(the code generator, the store).

## rust-analyzer

`crates/syntax`, `crates/parser`, `crates/stdx` and `crates/syntax-tree` started as
rust-analyzer's (DioxusLabs' rustd branch merged with upstream master at `0b21bd6596`); until
2026-10-28 they were a subtree under vendor/. They are not synced with upstream or with a fork
repository, and keep rust-analyzer's crate names. Edit them, build and test from the root, and
commit like any other file. A plain clone builds:

    git clone https://github.com/indexable-inc/blaze && cd blaze
    sh site/install.sh --version "$(cat .github/blaze-version)"   # the stage-0 blaze
    blaze build --release -p blaze-cli

`crates/rustc_abi` is rustc's `rustc_abi` at the pinned nightly's commit, patched in for
crates.io's `ra-ap-rustc_abi`, the same way. The fork repositories
(indexable-inc/rust-analyzer, indexable-inc/rustc_abi) are archived.

### Where new code goes

New code that is the language's semantics goes in the front end's crates: name resolution
(`blaze-scope`), traits (`blaze-solve`), types (`blaze-ty`, `blaze-typeck`), the body and its
lowering (`blaze-body`, `blaze-body-lower`), the flow and the borrow check (`blaze-flow`,
`blaze-borrow`), const evaluation (`blaze-consteval`), layout, and the hashes that say what a
function's code is made from. They ask for items only through `blaze-surface`'s `Items` and
`Source`. Where blaze's body differs from rustc's semantics, the fix goes in the lowering, and
the code generator relies on it: codegen does not compensate. A crate of its own is better than
growing one of these.

A new crate allocates as little as it can and names every allocator from its first line
(rule `bump-allocate`): it borrows or writes into the caller's buffer where that does, and a
function that must allocate is handed the allocator (a `&Frame` or an `A: Allocator`) and names
it where it makes a value: a collection by `new_in` (`AVec::new_in(frame)`: its one choice is
the allocator, D172), any other type in its one `T::new(TOpts { alloc, .. })` (rule
`options-struct`). No comment keeps an implied
allocator from `blaze::implicit_allocator`. The gate holds a new crate there: a crate with no row
in `tools/ratchets/alloc-sites.txt` is held to no `// alloc:` line and no `expect`
of the lint outside tests (and, once no `// alloc:` line is left anywhere, no line naming
`Global`), and every other crate's row only shrinks
(`scripts/src/bin/allocs.rs`).

A function that can fail returns an error type of its module: an enum or struct naming what
failed and the thing it failed on, with `Display` (rule `errors-name-the-thing`), never
`Result<_, String>`, `&str`, `Box<dyn Error>` or `anyhow`, which the caller can't match on and
which can't take an allocator. The gate holds this by crate, from the text, until blaze checks
its own crates with `blaze::untyped_error`: each crate's signatures with such an error, and its
type aliases of such a `Result`, equal its row in
`tools/ratchets/untyped-errors.txt`, which only shrinks, and a crate with no row is
held to none; a change over its row is refused with the new sites named
(`scripts/src/bin/untyped.rs`).

blaze depends on no third-party crate in the end: every crate depends only on crates of this
workspace, and a new crate starts at none (`docs/v2/plan/design/bottom.md`, "Also decided on
2026-11-15"). The gate holds the way there by crate: each workspace crate's third-party packages
in its normal and build dependencies, in its dev-dependencies only, and in the closure they reach
through the workspace's manifests and `Cargo.lock` equal its row in
`tools/ratchets/third-party.txt`, which only shrinks, and a crate with no row is held to none. A
fork under `vendor/` counts as third-party unless its manifest says
`[package.metadata.blaze] ours = true`. A change over its row is refused with the new packages and
the manifest lines they came in by (`scripts/src/bin/thirdparty.rs`); `docs/v2/plan/third-party.md`
says what replaces each.

blaze's own code uses no `std` and no `alloc` in the end: a crate is `#![no_std]` on `core`, takes
its collections from blaze-own and blaze-hash, and is passed its allocator (the same decision; rule
nothing-ambient). The gate holds the way there by crate, outside test code: the lines naming
`std` and those naming `alloc` as a path's first segment, 1 when the crate's root lacks
`#![no_std]`, and the lines naming a `std` or `alloc` collection (`Vec`, `Box`, `String`, `Rc`,
`Arc`, the `BTree*`, `VecDeque`, `BinaryHeap`, `HashMap`, `HashSet`) written bare and not imported
from blaze-own, blaze-mem or blaze-hash, or under `std::`/`alloc::`, equal its row in
`tools/ratchets/std-alloc.txt`, which only shrinks; a crate with no row is held to 0 in every
column. It reads the text, resolving no names: a rename, `vec!`, `format!` and a prelude use of
`alloc` with no path are missed, and its usage lists the rest
(`scripts/src/bin/ambient.rs`); blaze's lints are the definition once blaze checks its own crates.

blaze's machinery stays out of the front end, in crates of its own: the code generator's backends, object and debug-info
writing, the linker, the cargo drop-in, the store and the remote,
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

blaze supports one toolchain, the latest nightly, pinned by date in `pins/toolchain`
(`nightly-2026-10-27`, which says `rustc 1.101.0-nightly (16d1ffac1 2026-10-26)`). Stable
Rust is not a separate target: what a stable release builds, the nightly builds. blaze
itself is built with that nightly by blaze, which brings it (`blaze toolchain`): no rustup, and
no `cargo` or `rustc` of it is run. The pin is written in that
one file; everything else reads it or is written from it:

- `pins/rust.txt` (by `harness/toolchain_pins.py '{"write": true}'`): the
  sha256 and size of each file blaze downloads as its own toolchain, the nightly's `rustc -V`
  line and its rust-lang/rust commit. A rustc is taken for the pinned one by that whole line,
  since every nightly of a release has the same number.
- What mirrors rustc, all from that commit: `crates/rustc_abi` (rustc's `compiler/rustc_abi`),
  `crates/blaze-proc-macro` (blaze's proc-macro runner, which speaks the pinned
  `library/proc_macro` bridge's protocol by hand and checks it at load), the answers to rustc's
  questions (`pins/answers/rustc.json`), and the code written by hand to agree with rustc:
  the Rust ABI in the code generator, `vtable.rs`, `type_id.rs`, symbol names (v0) in
  `symbols.rs`, the intrinsics and lang items, the test harness's call into libtest.
- Rust reads every pinned fact from `crates/blaze-pins` (constants parsed at compile time from
  `pins/`, [pins/README.md](../pins/README.md)) and Python from `harness/toolchain_pins.py`; no
  copy of a pinned value is kept anywhere else.
- `site/install.sh` (which names `pins/toolchain` and lets blaze fetch it), the flake (`rust-bin.nightly.<date>`, for the dev shells and the
  bench nightly alike) and `Cargo.toml`'s `rust-version`.

### Moving to a newer nightly

The daily workflow does this every day and opens the pull request
([toolchain.md, "Moving the pin"](toolchain.md#moving-the-pin)); by hand it is for fixing what
a nightly breaks, on the bump's branch `toolchain/bump`, or for trying a nightly:

    uv run --frozen python harness/toolchain_bump.py '{}'                      # the newest nightly blaze can use
    uv run --frozen python harness/toolchain_bump.py '{"date": "2026-11-02"}'  # or a given one

picks the nightly (the newest of the last two weeks whose manifest has every component blaze
needs on every host it ships for, or the date given), writes `pins/toolchain` and
`pins.txt`, re-vendors `crates/rustc_abi` from rust-lang/rust at the new commit, merges that
nightly's standard library into `library/` (`harness/library.py merge`: upstream's changes applied three-way, so blaze's own changes to it
are kept; a conflict stops it and names the files), moves the pin where else it is written (`site/install.sh`, the
server's version string, `Cargo.toml`, the flake's rust-overlay input) and
installs the nightly with rustup. Each step names itself when it fails.

What it leaves to a person, and prints at the end:

1. `python3 harness/answers.py --write` on the Mac, where agent work runs. The target specs
   depend on the host rustc was built for, and no CI job runs the script, so the Linux hosts'
   answers are not refreshed by anything yet; then
   `uv run --frozen python tools/cpus.py '{"write": true}'` for the CPU table
   (`crates/target-features/src/cpus/table.rs`); commit both.
2. `blaze build -p blaze-cli --release`, fixing what the new `rustc_abi` and standard library
   ask for, and mirroring by hand what changed in rustc: the Rust ABI, vtables, `TypeId`,
   symbol names, new intrinsics and lang items. The move to `nightly-2026-10-25` needed
   `rustc_abi`'s renamed layout calls, vectors of at most 128 bits passed in registers on
   x86-64 and AArch64, Windows x64's rule for zero-sized types, the `f16b` and `ppcf128`
   layouts, libtest's new entry `test_main_env_args` and the intrinsic `abort_immediate`.
3. `uv run --frozen python harness/toolchain_bump.py checks '{"stage0": "/path/to/blaze"}'` on
   the Mac, with a blaze that builds the tree as stage 0. It builds blaze with that blaze (never cargo) and runs, in parallel,
   `toolchain_pins.py` and the gate's quick parts.
4. The full tier re-measured at the new commit: the crates' own test suites.
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
   reason, not lowered on a best-effort basis. A refusal is visible and
   counted. A wrong guess is a miscompile nobody sees.
3. **A refused function is a bug**, to be listed and fixed, not a normal path to design
   around.
4. **General fixes.** A fix that tests for one crate, one function name or one test is not
   a fix. Find what the case is an instance of.

Style follows rust-analyzer's guide
([style.md](https://github.com/rust-lang/rust-analyzer/blob/master/docs/book/src/contributing/style.md)): formatting by
`fmt.yaml` after the merge, no new
dependencies without a reason, small focused changes. Comments say what the code is for and
why, not what was changed. Messages printed to the user say what happened and what blaze
did about it in one line.

When a build is slow, reruns what it should reuse, holds too much memory or a cycle will not
settle, find out from the profile, not by guessing: the `debugging` skill
([.agents/skills/debugging/SKILL.md](../.agents/skills/debugging/SKILL.md)) says what a run
records, how to read it and the method.

## What a change needs

- A test that fails before and passes after: the real crate's own tests for a bug found in
  it, a unit test beside the code for one small enough to state, a step in the relevant edit
  harness for an incremental-build bug ([harness.md](harness.md#adding-a-test)).
- For concurrent, durable, crash-recovering, billing or data-deleting code: a deterministic
  simulation test (`blaze_sim::check`) whose oracle checks the invariant, with a planted bug it
  catches ([Tests](#tests)). A review does not stand in for it.
- The gate, on the platform you have. Say which platforms
  you did not run. CI's merge gate (`.github/workflows/blaze.yaml`, ix runners, Linux arm64) is the gate
  checks (`tools/gate.txt`), the Lean proofs, actionlint, and, for a change to code, incremental
  `blaze check . --stats` and `blaze test . --stats` with pinned v2 `blaze`
  (`.github/blaze-cli-version`, `.github/actions/blaze`). Its `ci` job is the one required check.
  The gate letting a change through is not the harnesses passing on it.
- blaze builds blaze, never cargo or rustc: with the release `.github/blaze-cli-version` names as
  stage 0 (`sh site/install.sh --version "$(cat .github/blaze-cli-version)"`, as
  `.github/actions/blaze` installs it), `blaze build --release -p blaze-cli`.
- Formatting: `fmt.yaml` runs the pinned release's `blaze fmt --all` on main after each merge and
  commits what it changes; the CLI built from this tree has no `fmt` command yet
  (`crates/blaze-fmt` is a library nothing calls), so nothing formats a pull request before it.
- `blaze test -p blaze-engine -p parser -p syntax` from the root (or
  `python3 harness/unittests.py`: every test of the workspace, with the failures it expects).
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

`scripts/src/bin/changed.rs` names what a change touches: the packages its files are in and the
packages that depend on them, each after what it needs, so the first error is the earliest
cause.

    blaze check
    blaze test $(blaze run -q -p scripts --bin changed -- '{"base": "origin/main", "print": "args"}')

`blaze check` takes a directory, not `-p` (checking only the named packages is not in the CLI
yet); `blaze test` takes the `-p` list.

- The session that integrates agents' work (rule fan-out-write-only) runs that on the Mac
  after each commit it takes, with `"base"` the commit before, in parallel with the build:
  the check of a few packages does not wait for the build of all of them, and an error goes
  back to the agent that wrote the code while it still has it in mind. `"dependents": false`
  checks the touched packages alone, for a first answer; the dependents follow.
- A change to a file every package's check reads (the root manifest, the lock file,
  `.clippy.toml`, the pinned toolchain) is the workspace's: the script prints `--workspace`,
  which `blaze test` does not take; run `blaze test` alone then.
- The gate's workspace check does the same first, so a pull request's first compile error
  comes in minutes ([harness.md](harness.md#ci)).

### Scripts that run commands

A harness, a lane script or an agent's loop checks the exit status of every command it runs and
stops at the first that fails, saying which; a command whose failure is expected says so where
it is run (`|| true` with a comment, a `check=False` whose result is read on the next line). In
a shell script that is `set -euo pipefail` on its first line; in Python,
`subprocess.run(..., check=True)` or the harness's own `engine.run(..., check=True)` and
`Ran.checked`.

### Tests

How blaze tests its own code is the `testing` skill
([.agents/skills/testing/SKILL.md](../.agents/skills/testing/SKILL.md)), the one home for it:
which tests to write and how few, property tests and simulations on blaze's own engine
(`crates/blaze-sim`; never proptest, quickcheck, arbitrary or bolero), deterministic simulation
tests on `blaze_core::sim::Sim` and their oracles, and testing a causal claim. Concurrent, durable,
crash-recovering, billing and data-deleting code is proven by a deterministic simulation test,
not by a code review (rule `dst-not-review`).

An incremental-build bug gets a regression test ([harness.md](harness.md#adding-a-test)): a
test in `crates/blaze-cli/tests/` or `crates/blaze-engine` with the project, the edit and what
must be made again.

## Documents

Every doc under `docs/` opens with front matter saying what it is (`scripts/src/bin/docs.rs` has
the fields): `current` (how blaze is now, kept true), `plan` (a design not fully built, dated),
`record` (research, a report or a measurement, true on its date and never brought up to date) or
`superseded` (with `superseded_by`, what replaced it). [docs/README.md](README.md) is the index,
generated from it; superseded docs are left out. CI's `blaze run -q -p scripts --bin docs -- check` holds every
doc's links, and a current doc's anchors, repository paths in backticks and `BLAZE_*` settings,
against the tree, and every doc that code names. A change that makes a current doc
untrue fixes it in the same change, or marks it superseded; `blaze run -q -p scripts --bin docs -- write`
regenerates the index.

## Working in parallel

Several lanes, people or agents, change blaze at once. A lane shares `git`'s metadata and
main with all the others, so:

- **Work comes from decisions and designs.** A lane is one task: a named piece of a row of
  [v2/plan/decisions.json](v2/plan/decisions.json) or of a design, done when its check passes. How a session
  picks, briefs, integrates and lands is [the plan's loop](v2/plan.md#how-the-work-is-run).
- **Work runs on the Mac**: builds, tests, smokes and benches, at most three heavy builds at
  once; no VM and no AWS host. Linux-only work runs in CI. Where neither can do something, stop
  and say what is missing (rule `work-on-the-mac`).
- **Every command has a timeout** sized to its expected time, and on expiry the status or
  stall line (`--status-every`, SIGINFO or SIGUSR1) is taken and quoted; a hang is a bug to
  report, never waited out (rule `every-command-has-a-timeout`).
- **Lanes write; the main agent compiles.** A lane writes in the one integration directory, in
  new files or one area's files no other lane touches, with the edit tools or `ast-grep` and
  never a heredoc, `sed` or a script (rule `edit-rust-by-syntax`), and commits its own piece;
  it builds nothing. A rewrite gets a fresh core in new crates beside the old code. The main
  agent builds once and admits each piece alone onto the last green head; no new wave of lanes starts while the tree does not compile (rules
  `fan-out-write-only`, `one-directory-for-lanes`, `agents-are-a-cost`). There is no worktree
  per lane and no cap on the number of lanes.
- **Never `git stash`.** It takes every lane's uncommitted changes in the directory, so a pop
  can take another lane's changes.
- **Run harnesses with `RUST_BACKTRACE=0`**, so a test that panics prints its message and
  not a backtrace into what the harness compares.
- **Land small and often, through a pull request** ([below](#landing-a-change)), each merge
  after the smoke run of its exact merge commit.
- **Clean up when done:** the lane's scratch homes and directories, any blaze process it left
  running and the merged branch (rule `clean-up-lane`).

## Landing a change

Main moves only through pull requests: nobody pushes to it or merges into it by hand. Nothing
merges, by `land.py` or by an admin, until a release blaze built from the exact merge commit (the
branch merged into the main it joins) has run the hello-world smoke on the Mac (a cold run, a
no-op, a body edit and a comment edit, each under a timeout, each counted only with exit 0 and the
program's own line, then `blaze check` and `blaze test` with one executed passing assertion) and the result is quoted in the pull request; a compile check is not evidence
that the binary runs (rule `proof-before-merge`; the `landing` skill has the steps). To make the
quote, merge origin/main into the branch, build the release `blaze-cli` and `blaze-launcher` of that commit in the same target directory and run
`cargo +$(cat pins/toolchain) run --locked -q -p scripts --bin smoke -- '{"blaze": "<that blaze>", "work": "<an empty directory>"}' > smoke.txt`
(`scripts/src/bin/smoke.rs`; it exits 0 only for `verdict ok`). The quote, the block in smoke.txt
whose first line is `blaze-smoke 2`, goes in the pull request's body, and `land.py` puts it there
from the file you give it. `land.py` refuses to open, resume or merge a pull request whose quote
does not name its head's exact tree, clean, with verdict ok, or whose head no longer contains
main; no flag skips this, and an admin merge needs the quote too. Once the branch's work is
committed, the local gate passed ([What a change needs](#what-a-change-needs)) and the smoke run,
run

    python3 .github/scripts/land.py '{"what": "what the branch does", "smoke": "smoke.txt"}'

It pushes the branch, opens its pull request ("Merge <branch>: what the branch does") with the
quote in its body, and starts the waiting that merges it. The pull request's own run is the merge gate
([What a change needs](#what-a-change-needs)); a failure there ends `land.py`
with the check's link. A red gate is fixed or its failure is listed as known with its cause, never passed by. Once it
passes, the waiting asks `smoke` again about the body's quote and the head GitHub has, and merges
the branch with a merge commit pinned to that head, as `git merge --no-ff` would. The branch
must contain main, since the quote is of the tree that becomes main: when main has moved past
the head, the waiting refuses, and the branch gets `git merge origin/main`, a new build and a new
smoke. The workflow also runs on `merge_group`, so a merge queue would get the same gate, but
`land.py` uses none: it merges with `gh pr merge --merge --match-head-commit <head>` itself. `land.py` waits for the merge and prints the commit, or says why
the pull request did not merge; then fix the branch and run it again. The waiting runs in a
process of its own that outlives `land.py`, its terminal and a tool that ends its process group,
writing to `.git/land/pr-N.log` in the main checkout; `land.py '{"follow": true}'` (or
`'{"follow": N}'`) follows it again, as does running `land.py` again on the branch, so nobody
polls GitHub by hand. Once it has merged, GitHub deletes the branch on origin;
remove the local branch (`git branch -D <branch>`).

## The build that ships

The release workflow (`.github/workflows/release.yaml`) installs the release
`.github/blaze-cli-version` names and builds `blaze` (`blaze-cli`) with it; blaze builds blaze,
never cargo or rustc.

Neither PGO training nor pinned PGO profiles work yet: blaze refuses `-C profile-generate`, since its linker writes no profile sections (`crates/blaze-code`), and the profiles pinned earlier were cargo's.

## LLVM

No crate of the workspace links LLVM (`llvm-sys` is an unused patch in `Cargo.lock`): native
code comes from blaze's own back end (`crates/blaze-emit`). `pins/llvm.json` pins one LLVM,
23.1.4 with our fixes, by its commit, version and per-target sha256, and
`vendor/llvm-sys/pin.rs` checks a prefix against that pin.

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
