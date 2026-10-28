# Contributing

## AI use

This code was written largely with AI assistance under human direction. The same is true
of the harnesses, the reports under `docs/reports/` and these documents. The reports are
written in the first person by the session that did the work; "I ran" in a report means
that session ran it.

What makes that workable is the testing method. Every change is judged by comparing blaze's
output with rustc's on real programs, and a change that makes a harness differ does not
land. If you contribute with AI assistance, the expectation is the same as without: you
understand the change, you ran the gate, and you say what you ran and what you did not.

rust-analyzer has its own AI policy for contributions to rust-analyzer. It applies to what
you send them, including fixes you find here and want to upstream.

## Layout of the tree

    crates/blaze-engine/       the daemon, the cargo drop-in, the splice, the remote
    crates/blaze-engine/src/bin/ blaze, blaze-rustc
    crates/hir-ty-codegen/    the code generator: MIR to Cranelift IR, the producers
    crates/blaze-store/      the disk store of compiled code, which knows nothing about Rust
    crates/blaze-fuzz/       made-up and damaged inputs for the tests of what reads the store
    crates/blaze-build-script/ build scripts run as cargo runs them, with no cargo (no-rustc.md, step I)
    crates/blaze-resolve/     cargo's resolver owned by blaze: the package model, Cargo.lock, versions, features
    crates/blaze-registry/    registries read and packages fetched as cargo does, with no cargo
    crates/blaze-wasm/        experiment: the compiler running in a web page (not in the workspace)
    harness/                  the test harnesses and their fixtures
    demo/                     small projects the harnesses and the scoreboard build
    docs/                     these documents
    docs/reports/             working reports, one per piece of work, as written at the time
    tools/                    what builds, releases and measures blaze: agents/ (the agent rules
                              AGENTS.md is generated from), nix/ (the flake's unit graphs), pgo/
                              (the pinned PGO profiles), bench/ (recorded results), remote-cache/
                              (the cache gateway Worker)
    licenses/                 NOTICE and the license texts of code blaze includes
    .github/SECURITY.md       what to know before sharing a store, and how to report a problem
    crates/hir-def/, hir-ty/, syntax/, parser/, ...
                              the front end: crates derived from rust-analyzer (below)
    crates/hir-ty-borrowck/   the borrow checker, a crate next to hir-ty (borrowck.md)
    crates/syntax-tree/             rowan 0.17.0 with a flat kind of tree behind its cursor (flat-syntax.md)
    crates/rustc_abi/         rustc's layout crate at the pinned nightly's commit
    vendor/proc-macro-srv/    the pinned nightly's proc-macro server, built by blaze to load
                              the proc-macro libraries blaze links (no-rustc.md, step J1)
    vendor/source.rev         the commit of the old development branch this tree last took from

`crates/hir-ty-codegen` is the code generator, a crate of its own over `hir-ty`'s MIR: what
it uses of `hir-ty` is public there. Its root is `mod.rs`, the file it was compiled from when
it was a module of `hir-ty`. The front end's crates depend on none of blaze's machinery
(the code generator, the store, the daemon; [architecture-split.md](architecture-split.md)).

## rust-analyzer

The front end's crates in `crates/` (`hir-ty`, `hir-def`, `hir-expand`, `syntax`, `parser`,
`base-db`, `syntax-tree` and the rest) started as a fork of rust-analyzer (DioxusLabs' rustd branch
merged with upstream master at `0b21bd6596`, with the MIR passes that make its MIR like rustc's
on top: [mir-passes.md](mir-passes.md)); until 2026-10-28 they lived in `vendor/rust-analyzer`.
They are no longer synced with upstream or with a fork repository, and keep rust-analyzer's crate
names. Edit them, build and test from the root, and commit like any other file. A plain clone
builds:

    git clone https://github.com/indexable-inc/blaze && cd blaze
    cargo +"$(cat harness/toolchain)" build -p blaze-engine --release   # cargo +nightly-2026-10-25

`crates/rustc_abi` is rustc's `rustc_abi` at the pinned nightly's commit, patched in for
crates.io's `ra-ap-rustc_abi`, the same way. The fork repositories
(indexable-inc/rust-analyzer, indexable-inc/rustc_abi) are archived.

### Where new code goes

New code that is the language's semantics goes in the front end's crates: building MIR and
the passes over it, borrow checking, const evaluation, layout, and the hashes that say what a
function's code is made from, as salsa queries. These are what rustc keeps in `rustc_middle`,
`rustc_mir_transform` and `rustc_borrowck`; rust-analyzer could take them, and a query lives
with the database it reads. Where rust-analyzer's MIR differs from rustc's, the fix goes in
the fork's lowering or in a pass of its runtime MIR queries, and the code generator relies on
it: codegen does not compensate.

There, a crate of its own is better than growing `hir-ty` or `hir-def`, which are already the
longest builds in the workspace.

blaze's machinery stays out of the front end, in crates of its own: the code generator's backends, object and debug-info
writing, the splice, the daemon and its socket, the cargo drop-in, the store and the remote,
tiers and sampling, the live runtime, and the harnesses. A change there is one ordinary
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
(`nightly-2026-10-25`, which says `rustc 1.101.0-nightly (12433c307 2026-10-24)`). Stable
Rust is not a separate target: what a stable release builds, the nightly builds. blaze
itself is built with that nightly (`cargo +"$(cat harness/toolchain)"`; in `nix develop`
plain `cargo` is it), and so are the harnesses' reference builds. The pin is written in that
one file; everything else reads it or is written from it:

- `crates/blaze-cargo/src/cargo/toolchain/pins.txt` (by `harness/toolchain_pins.py --write`): the
  sha256 and size of each file blaze downloads as its own toolchain, the nightly's `rustc -V`
  line and its rust-lang/rust commit. A rustc is taken for the pinned one by that whole line,
  since every nightly of a release has the same number.
- What mirrors rustc, all from that commit: `crates/rustc_abi` (rustc's `compiler/rustc_abi`),
  `vendor/proc-macro-srv` (the nightly's proc-macro server with blaze's J1 changes and its
  `RUSTC_VERSION_STRING`), the answers to rustc's questions
  (`crates/blaze-cargo/src/answers/rustc.json`), and the code written by hand to agree with rustc:
  the Rust ABI in the code generator, `vtable.rs`, `type_id.rs`, symbol names (v0) in
  `symbols.rs`, the intrinsics and lang items, the test harness's call into libtest.
- `install.sh`'s `TOOLCHAIN`, the flake (`rust-bin.nightly.<date>`, for the dev shells and the
  bench nightly alike) and `Cargo.toml`'s `rust-version`.

### Moving to a newer nightly

The daily workflow does this every day and opens the pull request
([toolchain.md, "The daily bump"](toolchain.md#the-daily-bump)); by hand it is for fixing what
a nightly breaks, on the bump's branch `toolchain/bump`, or for trying a nightly:

    python3 harness/toolchain_bump.py '{}'                      # the newest nightly blaze can use
    python3 harness/toolchain_bump.py '{"date": "2026-11-02"}'  # or a given one

picks the nightly (the newest of the last two weeks whose manifest has every component blaze
needs on every host it ships for, or the date given), writes `harness/toolchain` and
`pins.txt`, re-vendors `crates/rustc_abi` and `vendor/proc-macro-srv` from rust-lang/rust at
the new commit (blaze's changes to the server carried over by a three-way apply; a conflict
stops it and names the files), merges that nightly's standard library into `library/`
(`harness/library.py merge`: upstream's changes applied three-way, so blaze's own changes to it
are kept; a conflict stops it and names the files), moves the pin where else it is written (`install.sh`, the
server's version string and manifest, `Cargo.toml`, the flake's rust-overlay input) and
installs the nightly with rustup. Each step names itself when it fails.

What it leaves to a person, and prints at the end:

1. `python3 harness/answers.py --write` on x86-64 Linux, AArch64 Linux and arm64 macOS, each
   with that host's release build of blaze, since the target specs depend on the host rustc
   was built for; commit `crates/blaze-cargo/src/answers/rustc.json`.
2. `cargo build -p blaze-engine --release`, fixing what the new `rustc_abi` and standard library
   ask for, and mirroring by hand what changed in rustc: the Rust ABI, vtables, `TypeId`,
   symbol names, new intrinsics and lang items. The move to `nightly-2026-10-25` needed
   `rustc_abi`'s renamed layout calls, vectors of at most 128 bits passed in registers on
   x86-64 and AArch64, Windows x64's rule for zero-sized types, the `f16b` and `ppcf128`
   layouts, libtest's new entry `test_main_env_args` and the intrinsic `abort_immediate`.
3. `python3 harness/toolchain_bump.py checks '{}'` in an ix VM, under Cranelift and under LLVM
   (in `nix develop`, with `BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`). It builds
   blaze and runs, in parallel, what proves blaze and the nightly agree: `toolchain_pins.py`,
   `answers.py`, `macroserver.py`, the corpus with its `nightly` feature and `--layouts`, and
   the gate's quick parts.
4. The full tier re-measured at the new commit: `uitest.py`, `libsuite.py` and
   `borrowck_ui.py` against rustc's own tests.
5. One commit for the bump, landed like any other change.

A blaze built before the bump refuses the new nightly until it is rebuilt, since the two
match by the whole version line.

## Rules for code

1. **rustc is the reference.** If blaze's program behaves differently from rustc's, blaze
   is wrong, even where rustc's behaviour looks like an accident. The harnesses run rustc to
   compare with; blaze itself never does ([no-rustc.md](no-rustc.md)).
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

## What a change needs

- A reduced case that fails before and passes after: a corpus function for a code
  generator feature, a breaker for a bug found in real code, a step in the relevant edit
  harness for a daemon bug. [harness.md](harness.md) says how to add each.
- The gate, under Cranelift and under LLVM, on the platform you have. Say which platforms
  you did not run. CI's merge gate is the build and the unit tests on Linux x86-64; every
  harness on every platform, Windows too, and the slow ones (`wholeprogram.py`, `keys.py`,
  `buildmodel.py`, the store, `blaze_cli.py` on real projects) run in the release train on
  main after the merge, and a red train opens or adds to the issue "Release train is red",
  with the commits since the last green one ([harness.md](harness.md#ci)). The gate letting a
  change through is not the harnesses passing on it.
- The LLVM backend builds in the flake's dev shell, against the LLVM it pins:
  `nix develop -c cargo build -p blaze-engine --release --features llvm`
  ([LLVM for `--features llvm`](#llvm-for---features-llvm)).
- `cargo fmt --all` (nothing checks it, and `fmt.yaml` formats main after the merge, but a
  pull request's diff should be its change), and
  `cargo test --release -p blaze-engine -p hir-ty -p hir-def -p parser -p syntax` from the root (or
  `python3 harness/unittests.py`, the one `cargo test` CI runs, with the failures it expects).
- For anything that changes numbers in [results.md](results.md): the command, the commit
  and the machine, so the entry can be replaced and not just appended to.

When you report what you did, separate what you ran from what you inferred. The reports
under `docs/reports/` do this and it is the reason they can be used.

## Working in parallel

Several lanes, people or agents, change blaze at once. A lane shares `git`'s metadata and
main with all the others, so:

- **Builds, tests, benchmarks and experiments run in the lane's ix VM**, never on the Mac and
  never on an AWS dev host or any other AWS machine. The Mac edits, reads and runs `git`; a
  build, a harness, a unit test or a benchmark goes to the VM.
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
  - Where an ix VM can't do something, stop and say what is missing; never take it to a dev
    host or the Mac instead.
- **One worktree per lane, named for it** (`blaze-<lane>-<date>`), with its own
  `CARGO_TARGET_DIR`. Two lanes building into one target directory wait on each other's lock
  and overwrite each other's binaries.
- **Never `git stash`.** The stash is one stack for every worktree of the repository, so a pop
  can take another lane's changes. Commit work in progress to the lane's branch.
- **Run harnesses with `RUST_BACKTRACE=0`**, so a test that panics prints its message and
  not a backtrace into what the harness compares.
- **Land small and often, through a pull request** ([below](#landing-a-change)). A lane that
  waits hours for one large merge meets every other lane's changes at once. For anything the
  fast tier does not cover, run the release train on the branch first
  (`gh workflow run train.yaml --ref <branch>`: everything, without the release).
- **Clean up when done:** remove the worktree (`git worktree remove`), its target directory
  and the lane's VM (`ix rm <lane>`); delete the merged branch.

## Landing a change

Main moves only through pull requests: nobody pushes to it or merges into it by hand. Once the
branch's work is committed and the local gate passed ([What a change needs](#what-a-change-needs)),
run in the lane's worktree

    python3 .github/scripts/land.py "what the branch does"

It pushes the branch, opens its pull request ("Merge <branch>: what the branch does") and turns
on auto-merge. The pull request's own run is the merge gate, the build and the unit tests on
Linux x86-64 (about five minutes, [harness.md](harness.md#ci)); a failure there ends `land.py`
with the check's link. Once it passes, GitHub merges the branch with a merge commit, as
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

`cargo build --release` is the build to work with: the harnesses, the gate and CI use it, and
it rebuilds quickly after a change. What users get is one other build: the `dist` profile
(thin LTO) optimized with the PGO profile `tools/pgo/profiles.json` pins, built by
`.github/scripts/build_release.py`. The release workflow runs that script, and the flake's
`dist` runs it inside its derivation with the same profile fetched by its sha256.
`cargo install --profile dist` builds the same profile without PGO, from the release after 0.1.0.
[reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md) has the numbers.

The PGO profiles are trained by the `pgo` workflow, one per target on the runner its release
is built on; it uploads them to the `pgo-profiles` release and commits their names and hashes
to `tools/pgo/profiles.json`. It runs by itself when Cargo.lock, a manifest or `harness/toolchain`
changes on main: a profile finds each function by its symbol, which carries a hash of the
whole graph of crates, so after such a change the old one finds almost nothing (the build
warns, and a release refuses). A profile trained on older code of the same crates still
applies to every function that has not changed since, and `build_release.py` ends by saying
how many have: 24 after a one-line edit, about 1,600 for code 23 commits and 6,700 lines older
(of the 187,000 functions a profile has). Run the workflow by hand when that count passes a
few thousand and before a release after large changes to the daemon.

## LLVM for `--features llvm`

The LLVM backend builds against one LLVM, 23.1.4 with our fixes, built by blaze for each target it ships on
and pinned by its sha256 in `crates/hir-ty-codegen/llvm.json`. The flake's dev shell fetches it:

    nix develop -c cargo build -p blaze-engine --release --features llvm

Outside the shell, fetch the same archive and point the build at it:

    python3 .github/scripts/llvm.py fetch --target <triple> --dir ~/.cache/blaze-llvm
    LLVM_SYS_231_PREFIX=~/.cache/blaze-llvm cargo build -p blaze-engine --release --features llvm

`build.rs` checks the prefix's `blaze-llvm.json` against the pin and stops otherwise: neither
Homebrew's LLVM nor a distribution's is a source. LLVM is linked in statically, and on Linux so
is libstdc++, so a binary built with the feature needs no LLVM where it runs. inkwell's llvm-sys
does the linking (vendored in `vendor/llvm-sys` for the static libstdc++; its SOURCE.md says why
the libraries must come from a crate below inkwell for GNU ld). CI and Windows
fetch the archive the same way (`.github/actions/toolchain`).

The archives are built by the `llvm` workflow (`.github/workflows/llvm.yaml`, recipe
`.github/scripts/llvm.py build`): our fork `indexable-inc/llvm` at the `ix/23.x` commit
wasm-host's ix-aot pins (LLVM 23.1.4 and our fixes, so both run one LLVM), the X86 and AArch64 targets,
no zlib, zstd, libxml2 or terminfo, on the release's runners (Ubuntu 22.04 for an old glibc,
macOS 13 as the deployment target, the static CRT on Windows). To move the pin, change
`ref`, `commit` and `version` in `llvm.json`, run the workflow, and write the `targets` its
summary prints (`llvm.py pin`). LLVM's own release archives do not do: the macOS ones hold
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
