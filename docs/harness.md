---
status: current
summary: The harnesses that test blaze (real crates' own tests, and blaze held to itself), what each checks, and how CI runs them.
---
# The harnesses

blaze is held to real crates' own test suites and to itself, not to rustc. A crate's tests
pass under blaze or they don't: `downstream.yaml` judges each test by its own verdict. The
other harnesses check what blaze
promises of itself: a warm daemon's build after an edit gives what blaze's own build of the same
source from nothing gives (`blaze_cli.Scratch`, `storelib.py`'s daemon in an empty home), the store never hands a
build something stale, two builds are the same byte for byte, the sandbox holds, and each check
states the result it expects. No harness compares blaze's output with rustc's or cargo's: that
was dropped on 2026-10-30 ("we use other ppl['s] test[s] [of] the other crates ... it
over-constrains us", Andrew), so how a rule of rustc's is proven ported is a crate whose own
tests exercise it, or a unit test ([AGENTS.md](../AGENTS.md), best-long-term-design). Benches
may still time cargo as their baseline.

There are also Rust unit tests in `crates/blaze-engine` and in the code generator; the harnesses
are what carries the weight.

## Real projects: the verdict

blaze is judged by building big real Rust workspaces with blaze and running their own test
suites ([AGENTS.md](../AGENTS.md), verify-on-real-projects): a project whose tests pass under
blaze is correct for what they cover, and every failure is a blaze bug. No rustc and no cargo
run: the projects are fetched by git and built and tested by blaze alone, and nothing is
compared with another compiler's output. `harness/v2/realprojects.py` does it:

    uv run --frozen python harness/v2/realprojects.py run '{"out": "/tmp/rp"}'
    uv run --frozen python harness/v2/realprojects.py run '{"out": "/tmp/rp", "projects": ["ripgrep", "tokio"], "baseline": "/tmp/rp-last/result.json"}'
    uv run --frozen python harness/v2/realprojects.py run '{"out": "/tmp/rp", "where": "ix", "projects": ["bevy"]}'
    uv run --frozen python harness/v2/realprojects.py merge '{"results": ["a/result.json", "b/result.json"], "out": "/tmp/rp"}'
    uv run --frozen python harness/v2/realprojects.py selftest

The corpus is the `workspaces` of `harness/v2/corpus.json` (its shape in `harness/v2/pins.py`,
shared with `corpus.py`'s crates.io crates): blaze itself first, at the run's `commit`, then
ripgrep, nushell, helix, wasmtime, rust-analyzer, cargo, bevy, tokio, serde, rayon, regex,
hyper and polars, each at a commit and that commit's git tree id. A fetch whose commit or tree
is another is refused; wasmtime's submodules come at the commits its tree pins. Where blaze_cli.py
pins the same project it pins the same commit (the selftest holds them together). A project
that commits its `Cargo.lock` builds with it; for one that does not (bevy, tokio, serde, rayon,
regex, hyper) the lock is the harness's, `harness/v2/locks/<name>.Cargo.lock`, so two runs
build the same dependencies. Until it is committed that project stops at its `lock` step; a run
with `"write_locks": true` has blaze resolve it and writes it there. A workspace with
`"sandbox_tests": true` (ripgrep) has its tests run in blaze's sandbox: a `sandbox` step adds
`sandboxed-tests` to `experimental` in `[workspace.metadata.blaze]` of the copy's root
`Cargo.toml` (never the fetched source; refused where it cannot be merged), and `result.json` and
`report.md` say so per project, with the copy's BLAKE3 both before the edit and as built.

For each project, `jobs` at a time, the longest first, each with a daemon home of its own
(which reads the machine's toolchain, `~/.blaze/toolchains`, as every `BLAZE_HOME` does):

| Step | What it does and records |
|---|---|
| `fetch` | `git fetch --depth 1` of the commit into a sources directory kept between runs, checked against the pin, then a block-cloned copy |
| `lock` | the project's `Cargo.lock` or the harness's; either is built `--locked` |
| `tree` | the exact tree (rule measure-the-exact-tree): the BLAKE3 of every file of the copy, the lock's sha256 and the blaze binary's BLAKE3 |
| `build` | `blaze build --workspace --all-targets --message-format=json`: units built, and each unit that failed with blaze's error |
| `test` | `blaze test --workspace --no-fail-fast --message-format=json`, after a failed build too: each test passed, failed or ignored, the panic of each failure, and each test executable that crashed or never built (`blaze-unchecked`) |

Each step is bounded (the workspace's `minutes` for the build and the tests), and one the
harness could not finish names the step, the command, its exit status and the end of what it
printed. Failures are grouped into clusters by blaze's error with numbers, paths and hashes
taken out, so one bug that fails 300 tests is one row. The run writes `result.json` (every
test's outcome, every failure, the trees, the wall times), `report.md` (a table of all
projects, then per project its steps and its top build and test clusters) and
`logs/<project>/<step>.log`. Against a `baseline` (an earlier `result.json`) each project at the
same commit and lock lists the tests and units that newly broke and those that got fixed. The
exit status is 1 when the harness could not finish a project or something newly broke (with
`strict`, when anything failed).

It runs on the Mac or, with `"where": "ix"`, in one ix VM per project: this repository's commit
goes over as a bundle, the release of blaze `vm.blaze` names (else `.github/blaze-version`) is
installed by `site/install.sh`, and the project's out directory comes back under `vm/<name>/`.
`realprojects.yaml` runs every workspace daily (and by hand with a list), one job each on the
stage 0 blaze (`.github/actions/blaze`), then merges them and diffs against the last scheduled
run's result. A workspace whose job sends no result (it hit the runner's limit) is an error in
the merged report, never left out; one whose lock is the harness's runs there once that lock is
committed.

## What runs against v2 today

Since 2026-10-30 the blaze under test is v2's `blaze` (`crates/blaze-engine/src/program.rs` in
front of `blaze_cargo::cli`): cargo's `build`, `check`, `run`, `test` and `bench`, the daemon's
`serve`, `stop`, `status` and `daemon install|uninstall`, the agent API's `codegen-inputs`,
`trace` and `tables`, and `pin`, `version-fetch`, `version-resolve` and `sandbox`. v1's entry
points are gone: compiling the crates `--crate` names into `--out` with `--report`, `--deny` and
`--symbols`, v1's daemon (`--socket`, `--patch`, `--link-cmd`) and its `sync` and `patch`
requests, and the commands `cache`, `lock`, `build-script`, `blaze-rustc` and the rest. So the
harnesses are of three kinds:

- **v2's own**, which run against v2 as written: the store harnesses on `storelib.py`
  (`storekills.py`, `storecorrupts.py`, `storefull.py`, `storeguards.py`, `storetracked.py`),
  `storerevisions.py`, `fuzz/store.py` and `harness/v2/` ([The v2 proof](#the-v2-proof), which
  still sets `BLAZE_BACKEND`).
- **Through the command line**: harnesses that drive `blaze build`, `run`, `test`, `check`,
  `stop` and `status`, which v2 has, and so run against v2. Some read v1's output or set v1's
  settings, which v2 doesn't have; those are named in their rows. Most of them read
  `blaze_cli.how_built`, which says whether the daemon built it by the `build-finished` line every
  build prints under `--message-format=json-render-diagnostics` (`blaze_cli.Side` adds it, and
  keeps in `stdout` only the program's or the tests' own output after that line; rustc's
  diagnostics stay rendered on stderr), or else its first error. v2 builds only through its
  daemon, and that no rustc runs is `norustc.py`'s. The daemon's pid is its lock's
  (`storelib.daemon_pid`), which names a daemon that is busy or stopping too; the rest of what
  it says of itself is `blaze status --json`'s (`storelib.blaze_status`).
- **Edit, rebuild, compare with a fresh build**, on `rebuild.py`: an edit gives the program a
  fresh build of the edited source gives, which is what incremental builds rest on. A
  `rebuild.Project` builds a checkout with `blaze build` (or `test`, `run`) under a warm daemon
  of its own home (`storelib.Daemon`), runs what the build's `artifact` message names, and holds
  what it prints and how it exits to a build of the same checkout, at the same path, in an
  empty home and target directory; an edit the fresh build refuses has to be refused warm too.
  The profile picks the backend (dev is Cranelift's, release LLVM's). These are `programs.py`, `includes.py` and `unwinds.py`,
  `stress.py`, `worktrees.py`, `keys.py`, `growth.py`, `selects.py`, `rustsrc.py`,
  `wholeprogram.py`, `debuginfo.py` and `fuzz/edits.py`. They were v1's on its daemon's
  `--socket`, `--patch` and `--watch`, and moved onto the command line on 2026-10-30; what
  they checked of a live patch went with v1's daemon (below).

The harnesses that could not move were deleted on 2026-10-30: those built on v1's mixed model
(compile each crate with `blaze-engine WORKSPACE --crate ...` and link it on rustc's line,
itself a no-rustc bug) or on v1's live patch, `blaze-engine std`, `cache` or `blaze-rustc`.
Among them were `splices.py`, `farsplices.py`, `asmedits.py`, `farfile.py`, `bigbinary.py`,
`cratetest.py`, `libsuite.py`, `allblaze.py`, `runbench.py`, `stdcount.py` and `wasm32.py`.
What they held that v2 still needs is queued as v2 harnesses: each live-patch property
(splices, far splices, `asm!` edits, a patch after refused text and during a build, the large
programs, a patched function's debug info) for once the v2 API has a live-patch request, and
a runtime bench, borrowck over the apps and wasm32 programs. The helpers other harnesses used
moved: the DWARF reading of `debuginfo.py` to `dwarf.py`, and checking out and running the
apps of `apps/apps.json` to `appruns.py`.

`nativespans.py`: A proc macro, run as a wasm module as every build runs it, asks for a span's source text, line, column, byte range and file; edits and a daemon restart must print what blaze's build of the same source from nothing prints (#823).

`deriveerrors.py`: Unused invalid built-in derives fail check and build with the error each case names; a valid edit and a daemon restart preserve the verdict (#751).

## Running them

You need Python 3, the pinned nightly (`harness/toolchain`), a C compiler and linker, and a
release build of blaze. blaze reads no toolchain's `rust-src`. Some harnesses need more:
`debuginfo.py` and `realdebug.py` need gdb and `llvm-dwarfdump`, `growth.py` `llvm-nm`, `storefull.py` needs passwordless `sudo` to
mount a tmpfs.

    cargo +"$(cat harness/toolchain)" build -p blaze-engine --release
    cd harness
    export BLAZE_BIN=$PWD/../target/release/blaze-engine

`BLAZE_BIN` names the blaze under test and defaults to `../target/release/blaze-engine`. The
scripts pick the toolchain from `harness/toolchain` unless `RUSTUP_TOOLCHAIN` is set. They
also default `RUSTFLAGS` to `-C link-dead-code`, which `blaze` does not use, and
`CARGO_INCREMENTAL` to 0. Scratch directories go under `TMPDIR`; several of the harnesses
need a few gigabytes there.

### The gate

The gate is what a change runs before it lands: its fast tier in the lane's ix VM. CI's merge
gate is only the build and the unit tests; all of it, every part on every platform, runs in the
release train on main after the merge ([CI](#ci)), which finds what the fast tier would have.
A change is not done until all of it exits 0, with a native blaze, which has LLVM (the
release profile is LLVM's). `harness/ci.py` is the one list of it: which harness runs in which
part, in which tier, on which hosts.

    uv run --frozen python harness/ci.py list '{}'                  # every harness, its part, fast-tier seconds and hosts
    uv run --frozen python harness/ci.py parts '{}'                 # the full tier's parts on this machine (the train's)
    uv run --frozen python harness/ci.py parts '{"tier": "fast"}'   # the fast tier's parts here (quick-1 .. quick-N)
    uv run --frozen python harness/ci.py run '{"part": "core-1"}'   # run one part here, as CI does
    uv run --frozen python harness/ci.py run '{"part": "fast"}'     # every fast harness of this host, one after another

`ci.py` runs from the repository's root in its Python workspace (`pyproject.toml`, `uv.lock`),
and each harness in the Python that runs it. A harness moved to one typed input (AGENTS.md,
scripts-take-typed-input) reads it with the workspace's `typed_input`, so it runs the same way
by hand: `uv run --frozen python harness/storekills.py '{"blaze": "target/release/blaze", "kills": 10}'`.
The store harnesses, `own_inputs.py`, `fuzz/store.py`, `includes.py`, `incremental.py`, `keys.py`, `fuzz/edits.py`,
`focused.py`, `toolchain_bump.py` and `ci.py` itself take their JSON so far; the fields are in each script's docstring.

The flake's `scripts` shell supplies uv, Python 3.13 and Node for pyright, all from `flake.lock`:
`nix develop .#scripts --command uv run --frozen pyright` (the same workspace as CI).
The development, wasm and benchmark shells also provide uv and Python 3.13. On NixOS they
select Nix's interpreter explicitly and disable Python downloads; uv's standalone Linux Python
does not use Nix's dynamic loader. CI installs uv 0.13.5 on every harness runner, Windows
included, and `uv run --frozen --no-dev` installs the workspace helper without the lint tools.

The fast tier is the harnesses marked fast in `HARNESSES` that `FAST` (in `ci.py`) gives to
this host: each line there has the harness's measured seconds on CI (the slowest median of
2026-10-25 and 26) and the hosts it runs on. A harness whose result depends on the platform
(the loader, the linker, signing, the daemon's sockets) runs on all three; one that checks
generated code or the build model runs on Linux x86-64 alone in the fast tier and everywhere in
the full tier. `ci.py` packs them into parts of about 240 seconds (`PART_SECS`,
longest first), so adding a harness adds a part rather than lengthening one; a fast harness
with no `FAST` line is an error, and so is a `FAST` or `CORE` line that names no harness. Each
harness runs with a `BLAZE_HOME` of its own under `/tmp/rj-ci-*`, so its daemon's state and store (`<home>/v2`) are its own, and its daemon is stopped
afterwards, so no two harnesses of a job share a daemon or a socket (on a runner `XDG_RUNTIME_DIR` would otherwise give every
harness `/run/user/1001/blaze`). The toolchain is in no home: every home reads the machine's
(`~/.blaze/toolchains`), which `ci.py` has `blaze toolchain` download once before a part's first
harness. `live_e2e.py` and `live_types.py` take a
home of their own when run by hand too.

A part runs all of its harnesses even when one fails, then names the ones that failed and how
long each took. Alongside the harnesses, CI runs `python3 harness/imports.py`, which imports
every script of the harness, and
`python3 harness/unittests.py`, which runs the Rust unit tests and holds them to
`harness/known-failures.toml` (below). It is one `cargo test` from the root over blaze's crates and the front end's
(`hir-ty`, `hir-ty-mir`, `hir-def`, `ide-diagnostics` and the rest), all
members of the one workspace; the root `.cargo/config.toml` sets `CARGO_WORKSPACE_DIR`, where
expect-test finds the parser's `.rast` files.

### Under Cranelift

v2 picks the backend from the profile's `opt-level`, never from the environment
(`crates/blaze-codegen/src/plan.rs`, `OptLevel::backend`): 0 and 1 are Cranelift, so the dev
profile is.

### Under LLVM

Build in the flake's dev shell, which has the LLVM 23 it pins
([contributing.md](contributing.md#llvm)). Under v2 an `opt-level` of 2,
`s` or `z` is then LLVM at `-O2` and 3 is LLVM at `-O3`, so the release profile is LLVM; a
profile that asks for LLVM of a blaze built without it is refused.

    nix develop -c cargo build -p blaze-engine --release

`BLAZE_BACKEND` and `BLAZE_OPT` are v1's settings: under v2, either one set at all makes every
unit fail, saying the variable changes code its key has no part of
(`crates/hir-ty-codegen/v2/backend.rs`, `CHANGES_CODE`), and `BLAZE_EMIT` is read only by v1's
code generator left in `crates/hir-ty-codegen/backend.rs`. So no harness sets them, and none
takes them away from its caller's environment: a run with one set fails as v2 says. CI's
matrix has no backend; a harness that is to cover LLVM builds the release profile, as
`fuzz/store.py` and `fuzz/edits.py` do (`"profiles": ["dev", "release"]`), `wholeprogram.py
'{"profile": "release"}'`, and `bench.py`'s `llvm` (`--release` on both sides). `harness/v2/engine.py` (the
proof's) still sets `BLAZE_BACKEND` for every command ([The v2 proof](#the-v2-proof)). v1's
recipe, for the record, was `BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast` on that build.

## What each one checks

### Correctness of generated code

| Script | What it checks |
|---|---|
| `answers.py --write` | Not a check: writes what the pinned rustc answers to the questions blaze answers in its place (`-vV`, `--print cfg`, cargo's probe of file names, `target-spec-json`) into `crates/blaze-units/src/stdlib/rustc.json`, after a toolchain bump, on each CI host ([no-rustc.md](no-rustc.md), step B) |
| `toolchain_pins.py` | That the sha256 and size of each file of blaze's own Rust (`cargo/toolchain/pins.txt`) are what the release's manifest at static.rust-lang.org says, for every host blaze ships for; `'{"write": true}'` regenerates them when blaze moves to another release ([cargo.md](cargo.md)). Needs the network |
| `nightly_features.py '{"dry_run": true}'` | Not a check: keeps the GitHub issue tree of the pinned nightly's unstable features (#967), so every `#![feature(..)]` blaze has to accept is tracked. Each language feature of rustc_feature's `unstable.rs` at library/UPSTREAM's commit is an issue labelled `nightly-feature` under a group of at most 100 sub-issues; the library features (the `#[unstable]` and `#[rustc_const_unstable]` attributes of the sysroot crates in library/) are one issue, #969, for what blaze has to do for all of them (`'{"library": true}'` makes one each). A rerun creates only what is missing (by a hidden marker in each body), links what is not under its group, and comments on and closes the issue of a feature stabilized or removed since. `'{}'` makes the changes, paced under GitHub's limits. Needs `gh` |
| `unwinds.py` | Edits to functions a panic unwinds through, each held to a fresh build (`rebuild.py`) |
| `growth.py` | That the code of one function grows in a straight line with its source: the `serialize` `#[derive(Serialize)]` gives an internally tagged enum, a `?` in every arm, at 160 and 320 arms (issue #5); and a `load` filling a struct of 200 and 400 fields, each a call's `?` while a value with a destructor is live, as x11-dl opens Xlib's 781 functions. Built by `blaze build` (dev profile), measured with `llvm-nm`. Linux and macOS |
| `wholeprogram.py` | Programs blaze builds whole, with its own `main`: one on the default allocator, one with a `#[global_allocator]` of its own, one with a library's, one whose allocator is under a `#[cfg]` that is false, a trait's provided method calling a function declared in its body, a C `main`, and `become` tail calls (an interpreter of ten million steps on a 64 KiB stack). Each is built by `blaze build` in an empty home and has to print and exit as its source says (`EXPECTED`), and a second build in another empty home has to give the same executable byte for byte ([#200](https://github.com/indexable-inc/blaze/issues/200)); `'{"profile": "release"}'` builds them in the release profile. Its input is one JSON object (`cases`, `profile`, `keep`) |
| `norustc.py` | How often rustc and rustdoc run when `harness/norustc/fixture` is built, edited, tested, doctested and checked with `blaze`, through a copy of blaze's own toolchain (`blaze toolchain --json`'s sysroot, placed where blaze looks for it in the run's home and first on PATH) whose rustc, rustdoc and cargo log every run; a count above `norustc/baseline.json` fails ([no-rustc.md](no-rustc.md), step A). `--record` lowers the baseline |

### Reading the workspace as cargo does

The workspaces of `crates/blaze-manifest/tests/fixtures` (inheritance, members and targets
discovered, a package outside its workspace's members, config and manifest errors), the
config trees of `tests/config-fixtures` and the profile trees of `tests/profile-fixtures` (with
the unit graphs cargo printed) have their expected output committed next to them, so
`blaze test -p blaze-manifest` checks them without cargo or the network. The files were first
written from what cargo printed (by `resolve.py`, deleted 2026-10-30); they are not
written from cargo again, and a change to one is a change to what blaze expects.

### The daemon following edits

| Script | What it checks |
|---|---|
| `stress.py` | 100 edits of the kinds people make (bodies, signatures, new items, moved lines, broken code, reverts, a file edited again while a build of it runs) applied in order to a three-crate workspace under one warm daemon, each built by `blaze build` and held to a fresh build of the same source (`rebuild.py`); broken code has to be refused by both and the daemon has to survive it. Writes `STRESS.md`; exits 1 when a step is wrong |
| `programs.py` | Two programs of one workspace built by one daemon, a step at a time, each asked for by `blaze build -p NAME` and held to a fresh build ([#63](https://github.com/indexable-inc/blaze/issues/63)) |
| `worktrees.py [--together]` | Three checkouts of one workspace built by one daemon (one home), edited independently, each program asked for held to a fresh build of its checkout; `--together` asks for a step's programs at once |
| `includes.py '{}'` | Edits to a file only `include!` names, and to files outside the package that `include_str!` and `include_bytes!` read, each held to a fresh build |
| `outdirwarn.py '{}'` | A warning in a file a build script writes under `OUT_DIR`, taken in by `include!(concat!(env!("OUT_DIR"), ..))`, is shown with its line of the file and where it is, and the build succeeds: the check keeps the text of each file no mount holds for the diagnostics it writes |
| `rustsrc.py` | A build without the standard library's source (`BLAZE_LIBRARY` naming a library with an empty `core`) is turned down before anything is built, saying what the library lacks |
| `selects.py` | blaze runs no build script and no proc macro the selection does not need: `blaze build -p app`, `blaze test -p app --no-run` and `blaze build --workspace`, each in an empty home on a fresh copy, run only the scripts and macros of what was asked for. Linux |
| `debuginfo.py` | Programs built by `blaze build` (dev profile) in empty homes: backtraces held to the panic sites the harness writes, `llvm-dwarfdump --verify`, gdb stopping and stepping, locals, and perf unwinding by `.eh_frame`; a program followed through edits, rebuilt after each; each `debug` setting of the profile followed, and the same executable from a home that built another setting first as from a fresh one. Linux |
| `staticcallbacks.py` | A `LazyLock` static whose initializer calls an `#[inline(always)]` helper of a dependency: an edit of a sibling of the helper, then of the helper itself, under one daemon, each printing the value its source states (astral-experiments/blaze#83, #87) |
| `latelisted.py` | What a file named by `include!` through a build script's variable declares is listed in its crate though the daemon reads the file only once the names are resolved, as bun's generated bindings are: an edit that adds a function then compiles a few functions (`compiled` of its `blaze-work` line), not every one the file declares |
| `libraryfetch.py` | The release's `library-<digest>.tar.xz`, made by `library.py archive` and served locally, is installed from once (`BLAZE_LIBRARY_FROM`) with blaze's digest; a lying or missing archive is an error and nothing is kept |
| `loadretry.py` | A refused load reports changed flags and recovers after configuration files are removed or rewritten, without restarting the daemon (#597). Both configuration filenames, inherited configuration, and a source error said again under the same daemon are checked |
| `scriptenv.py` | A build script that says it reads `SDK_DIR` and fails without it (cef-dll-sys's shape) is given the command's value, with a fresh daemon and with one started without it, and another value or none reruns it; a path dependency that is a workspace of its own is given what the build's root manifest names in `build-script-env` ([#720](https://github.com/indexable-inc/blaze/issues/720)) |
| `unreached.py` | Edits under `blaze` that call instances of another crate's generic function the executable did not reach when it was linked: each is spliced, or laid out in full where a splice declines, as each build's `blaze-work` line says (`--message-format=json`). Linux and macOS; not run on Windows, where `blaze` has been run by hand only |
| `anonstatics.py` | Edits under `blaze` that reach, through code rustc inlined at opt-level 1, a `static mut` inside a `const _: () = { .. }` of a crate rustc built (the shape of objc2's `declare_class!` ivar offsets, which winit's frame function reads): the daemon serves each build and refers to rustc's static, found under its `_` segment (v1's mixed model: under v2 no crate is rustc's). Linux and macOS; not run on Windows, where `blaze` has been run by hand only |
| `artifactedits.py [case ...] [--release]` | An edit of a dependency reaches the file `blaze build` says it built ([#730](https://github.com/indexable-inc/blaze/issues/730)): a workspace of a library and a `bin`, `cdylib`, `dylib` or `staticlib` over it, built cold, unchanged, after a body edit that changes no behaviour (`std::hint::black_box`), one that does, and the edits taken back. A build that exits 0 has written the artifact, with bytes other than the build's before after each edit, and it computes what the source says (run, loaded with ctypes, or linked into a C program); one that cannot write it fails naming the kind (`dylib` today, #262). `staticlib-native` is a `staticlib` whose build script compiles C it links `static`: the C program is linked with the archive alone, which has to bundle it. `flags` is a program built with #731's rustflags (`-Z threads=8` with `RUSTC_BOOTSTRAP=1`, `--cfg tokio_unstable`, `-C target-cpu`, `-C force-frame-pointers=yes`, lld on AArch64 Linux), built and spliced by blaze. Linux and macOS |
| `testselect.py` | `blaze test` with `BLAZE_TESTS=affected` on a library's unit tests and an integration test of it, edited a step at a time: the tests that run are those whose code or what it calls changed, also through a library of the workspace both link (built without `cfg(test)`, which the daemon takes over from rustc), one added, one given `#[ignore]`, one that reads a file (only when the file changed, which `-v` names, from what the sandbox recorded), one that downcasts a `dyn Any` (only when what it calls changed), one in a private module named as a public function beside it (the harness names it through the module) and one that failed (until it passes); `BLAZE_TESTS=all` runs every test. Its workspace opts into sandboxed tests, so every test runs sandboxed: one that writes into the package leaves it as it was, and the harness fails where the sandbox can't be made (`BLAZE_SANDBOX=off` checks the unsandboxed path, where the hermetic tests are still kept); a package that has not opted in keeps its hermetic tests' passes all the same and runs its bench every time. Every `blaze test` runs with `--message-format=json`, and one without a `blaze-work` line was not served by the daemon. Linux and macOS. v1's: `BLAZE_TESTS` and `BLAZE_TESTS_FROM` are read by no v2 crate; v2 runs only the tests an edit reached under `blaze test --reached` (`crates/blaze-cargo/src/cli/args.rs`), which the harness does not use yet |
| `hermetic.py` | A test blaze sees whole is kept, one it can't see is said and run every time, read from `blaze test --reached --message-format=json`'s `blaze-test-result` lines (storelib's `test_results`): a package that opts into sandboxed tests has a pure test three calls deep, one that reads a file of the package, one that reads an environment variable and one its `unsandboxed-tests` names (it writes the temporary directory and dials outside); a package that does not opt in has a pure test. The first run runs all five, the two outside said `outside`; a second runs only those two, the three sandboxed ones cached by what they executed; changing the file, then the variable in the client's environment, then the function below the pure test runs only the test that depends on it. In no run is a test both cached and outside. Linux and macOS |
| `sandboxbench.py` | What the sandbox of a test costs: a trivial program (built by `blaze build --release`) bare and in `blaze sandbox` (with and without a record), and a generated 600-test library, opted into sandboxed tests, under `blaze test` with `BLAZE_SANDBOX=off`, sandboxed under `all` and `affected`, and as a process per test, bare and sandboxed; medians of alternated rounds, pinned to `--cpus`. The first `blaze test --message-format=json` has to print a `blaze-work` line, and its `compiler-artifact` names the tests' program. Linux and macOS. `all` and `affected` are v1's `BLAZE_TESTS`, which no v2 crate reads |
| `doctestonce.py` | `blaze test --doc` on a workspace of three libraries, each step held to what `STEPS` states: the workspace is loaded once for all of them, each library's passed count and failed examples in order, the exit status and the packages named as failed (every example passing, two failing under `--fail-fast`, by default and with `--no-fail-fast`, one failing with it), and no example left to rustdoc. Linux and macOS |
| `blaze_cli.py` | `blaze` on seven real projects, 22 flag combinations, and the daemon's lifecycle checks (one daemon by its lock `<home>/v2/serve.lock`, reused, shared by a worktree, gone when idle; v1's worker, write-in-place and `ready` checks are gone with v1), each held to blaze's own run from nothing (`blaze_cli.Scratch`: a fresh home, store and `target-scratch`) and to what it states: every build but `broken` exits 0, every test passes by its own verdict, each flag case exits as its source says. Needs the network and about 20 minutes; not in CI |
| `live_e2e.py` | Edits of a running program that links `blaze-live`, each applied as a patch; then another program built meanwhile, a start again from the executable, and a panic that unwinds through the patched code. Linux x86-64 and macOS AArch64 |
| `live_types.py` | A running program's component types changed as a game changes them, on the daemon's own base: `V2` added beside `V1` with a derive, a static id cache and a trait impl, the code moved to it with the values converted and a static new in the edit, `V1` deleted, each a patch whose answer matches blaze's build of the same stage from nothing and whose statics keep their values; then a field of `V2` changed, which restarts naming `struct V2`. One typed JSON input (`blaze`, `env`, `keep`, `tmp`). Linux x86-64 and macOS AArch64 ([live.md](live.md#types)) |
| `live_latency.py` | Edit-to-running time of live edits on `harness/live`, `demo/life`, `harness/live-bevy` and `harness/live-egui`, one edit at a time through `blaze build`, with one typed JSON input (`blaze`, `program`, `edits`, `json`, `machine`, `tmp`). Writes p50 to p99 and every sample, with the machine and its battery, to the input's `json`. Measures; checks nothing; macOS on AArch64 ([live.md](live.md#latency-under-typing), superseded) |
| `spaces.py` | A two-crate workspace under a directory whose name has spaces, `$$`, `#` and quotes, built and edited with `blaze build`; every build has to be the daemon's (a `blaze-work` line) and the program has to print what the source says ([#14](https://github.com/indexable-inc/blaze/issues/14)) |
| `unwindsplice.py` | A panic unwinds through spliced code: a function the edits change panics under `catch_unwind` with a guard on its frame, and after every edit the guard is dropped and the panic caught; every edit after the first is spliced, as each build's `blaze-work` line says, and the last adds a thousand functions, which outgrow the room kept for their unwind information, and a new frame the panic unwinds through: spliced or laid out in full saying why. v1's `nocompact.py` ([#33](https://github.com/indexable-inc/blaze/issues/33)), whose `-no_compact_unwind` reaches no linker under blaze's own. macOS on AArch64 |
| `machoimage.py` | The Mach-O executable the daemon links and splices, read from the file with the standard library: its load commands end before its first section's bytes after the first link, every splice and an edit that adds a library the image did not load (`LC_LOAD_DYLIB`); `LC_BUILD_VERSION` says macOS 14.0, as the build asked, and it is fixed up by chains, as ld-prime gives that macOS ([#30](https://github.com/indexable-inc/blaze/issues/30)); it has `__unwind_info`; each build links it once ([#173](https://github.com/indexable-inc/blaze/issues/173)), every edit after the first is spliced, and a daemon started anew links it once. v1's `headerpad.py` ([#13](https://github.com/indexable-inc/blaze/issues/13)), whose `-headerpad` reaches no linker under blaze's own. macOS on AArch64 |
| `tiers.py` | Tiers ([tiers.md](tiers.md)): `demo/lifebench` running under the daemon reaches levels 2 and 3 with output identical to blaze's build from nothing with `BLAZE_TIER=off`, an edit of a tiered function demotes it and is applied at once, and `BLAZE_TIER=off` changes nothing. macOS on AArch64 (any native blaze, which has LLVM); it exits 0 elsewhere |
| `tierbench.py` | The benchmark table of [tiers.md](tiers.md): `demo/lifebench` at level 1, 2 and 3 under a daemon and as cargo builds it (release, and `-O3` with the dev profile's checks), in interleaved rounds, the steady rate of each; `--together` runs a round's builds at once, for a loaded machine. Measures; checks nothing; not in CI |

### Incremental cost

`incremental.py` is the gate on what an incremental `blaze check` of blaze's own workspace costs,
by count. Until it, no harness measured that cost, and on 2026-11-04 a check after a one-line
edit had grown to about 40 minutes unnoticed, through five unrelated bugs of one class: memory
never given back, a memory forecast that serialised the scheduler, cache keys over temporary
directories' variables, proc-macro output marked unknown, and cycle results never reused. Each
makes work that should verify run again, so the gate counts the work rather than chasing each.

One daemon in a home of its own runs five `blaze check --workspace` (`args`) of the checkout:
warm (cold in that home; only reported), a no-op from another shell (another `TMPDIR`,
`OLDPWD`, `SHLVL` and `PWD`, from `crates/`, and a variable the daemon never saw), the same check
after the daemon is killed with SIGKILL (it has to log `restored the daemon image`), a body edit
of a leaf function of `blaze-format` (`edit`, a crate most of the workspace depends on), and the
edit taken back. The no-op and the restore have to run nothing: no plane node
(`builds.misses`), no query (`query_counts.executed` summed over the build) and no `misses` row.
The edit and its undo may run at most `edit.max_ran` plane nodes and `edit.max_executed` queries,
and no `misses` row may be of an item outside `blaze-format`: a dependent crate whose
signatures did not change verifies. The daemon's `live bytes` at the end of the no-op may exceed
the warm check's, and the undo's the no-op's, by at most `heap_growth_bytes`.

It reads every count from the profile history with `blaze debug sql --json`, under the DuckDB the
flake pins (`nix build .#duckdb`, or `duckdb` in its input): the check's `builds` row, its
`query_counts` and its `misses` ([profiling.md](profiling.md)). `query_counts.written`, each
input's fields its setters wrote in the build, names what the build changed; the counter was
added for this gate (`crates/blaze-query/src/db/counts.rs`, `WRITTEN`). On a failure it prints,
in one pass, every bound each check broke and, for those checks, the query functions that ran
most, the inputs written and every `misses` row with its cause and what changed. Wall time and
the daemon's peak heap are printed beside the counts and gate nothing. The edited file is
restored on every exit. Linux and macOS; the train's part `incremental`.

    uv run --frozen python harness/incremental.py '{"blaze": "target/release/blaze"}'

### The store

The `store*.py` harnesses and `fuzz/store.py` run v2's daemon (`blaze serve`, its store
`<home>/v2/store` as `crates/blaze-cas` lays it out) through `harness/storelib.py`, and take one
typed JSON input naming the `blaze` command line (`{"blaze": "target/release/blaze"}`). Linux
and macOS: v2's command line speaks to its daemon over a Unix socket. v2 has no remote of its
own to test here: its remote is the read-through tier of `crates/blaze-cas/src/remote.rs`.

| Script | What it checks |
|---|---|
| `selfrestore.py` | the real self-build restored: a warm build, the daemon killed (SIGKILL), the same build again restores the image (`restored the daemon image`), compiles no unit and answers within its budget; with a fresh home, the same tree built from nothing gives byte-identical outputs |
| `storekills.py` | v2's daemon killed (SIGKILL) at random moments while it opens, builds into and commits its store: after each kill the next daemon in the same home, with or without its journal and image, opens the store and builds what a build from nothing prints |
| `storecorrupts.py` | A head, index, pack or `file-*` of the store flipped, cut, grown, zeroed, replaced or removed: the next daemon refuses to start naming the file, or a build stops naming it, or the build prints what a build from nothing does; a head of an older, newer or no format is refused naming the head and the format, and the store left as it was |
| `storefull.py` | The store on a tmpfs of its own with no room: the daemon refuses saying the disk is full, or each build is right or stops saying so; with room the store opens and builds right, and room made under a running daemon is kept by the next. Linux |
| `storeguards.py` | The store is its user's alone: the directory 0700 and its files 0600, a pack others can write to not read, a store directory others can write to refused, each said naming the file |
| `keys.py '{"cases": [...], "shard": {"index": K, "count": N}}'` | Whether the store ever hands a build code compiled from something else. One case per thing code depends on besides its file's text: each builds `harness/keys` in an empty home, leaves the home only its store (`storelib.forget`), changes that thing (in place, or in a copy at another path) and builds again; both programs have to do what a fresh build does (`rebuild.py`). `cases` picks those whose names contain one of its strings, `shard` every Nth of those runs (each case in place and elsewhere) from the Kth (CI's `keys-1..3`) |
| `storetracked.py` | A daemon on a store holding the checkout's code (with nothing else of the home), and one in another home with a copy of that store on a copy of the checkout elsewhere, build it right, and every edit after: a generic body, closures moved, a static, an `#[inline]` function, an `impl Trait`'s closure, all taken back ([#105](https://github.com/indexable-inc/blaze/issues/105), [#216](https://github.com/indexable-inc/blaze/issues/216)) |
| `storerevisions.py` | A fresh daemon on a checkout edited before it started, and on the undo of that edit from a store holding only the edited revision, gets nothing stale: each case changes one thing a function's code depends on besides its own text (a callee's body or signature, an inline helper, a rebound import, a module constant, a field, a layout, a constant table), and both builds have to print what a cold build of the same tree prints. The acceptance test of v2's keys by every input ([#1336](https://github.com/indexable-inc/blaze/issues/1336), [#1291](https://github.com/indexable-inc/blaze/issues/1291), [#1439](https://github.com/indexable-inc/blaze/issues/1439)); it asserts no reuse and prints the `blaze-work` counts. From astral-experiments/blaze#66-#78. `uv run` |

### Timing

`bench.py`, `linkbench.py`, `heap.py`, `heapfold.py`, `leanprof.py`, `cpufold.py`,
`memsplit.py`, `realbench.py`, `spare.py`, `live_latency.py` and `testselectbench.py` measure
and do not gate. All of them were written against v1, and none has measured v2 yet. They drive
`blaze build` and the daemon through the command line but lean on v1's settings and output:
`bench.py` picks the backend by the profile ([Under LLVM](#under-llvm)) and tells a step that
finished by `blaze_cli.how_built` (its `build-finished` line);
`testselectbench.py` and `sandboxbench.py` set `BLAZE_TESTS`, `memsplit.py`
measures v1's spare worker (`BLAZE_SPARE`), and `coldprof.py` and `realbench.py` set
`BLAZE_IDLE_SECS`, none of which a v2 crate reads.
`testselectbench.py` runs `bench.py`'s projects' tests under a daemon with `BLAZE_TESTS=all` and
`affected`, before and after `bench.py`'s `leaf` edit: the wall time, the tests run and skipped,
and how many run every time and why, from what the daemon wrote beside each test program. It
exits 1 when a `blaze test --message-format=json` printed no `blaze-work` line, that is, the
daemon did not serve it (#84).
`memsplit.py` says what each process of the daemon holds (the daemon, the project's worker, the
spare) and of what kind of page, once ready, once the daemon has built its own base and after
edits, with `--env` to try an allocator setting
([research/daemon-memory.md](research/daemon-memory.md#2026-10-23-the-own-base-and-the-allocator)).
`spare.py` starts a daemon with a tiny project, then builds one of `bench.py`'s projects under it
from an empty target directory and again once it has built it, for builds of blaze given as
`--variant LABEL:BIN[:VAR=VALUE...]` in interleaved rounds: the second project's cold and warm
start and what the daemon (found by its lock) holds before and after. Under v1 it chose the spare
worker's default ([cargo.md](cargo.md#what-a-build-does)); v2 has no spare workers and no wait
until ready, so it measures the build and the memory alone.
`heap.py` gives a heap profile's bytes to the salsa query whose value or record holds them, and to
blaze's own structures; `heapfold.py` makes of a profile on macOS what `jeprof --collapsed` makes
of it on Linux, for `heap.py` to read ([research/daemon-memory.md](research/daemon-memory.md)); `heap.py --syntax`
says what of it is syntax trees, of which kind and from which query, and `cpufold.py` folds a
`perf script` the same way for time ([flat-syntax.md](flat-syntax.md)). `leanprof.py` runs a
project's cold start, own base, edits and warm start as phases with the times they began and
ended, and `cpufold.py --phases` cuts a `perf` profile of the run by them, with what salsa was
revalidating and who asked ([research/lean-frontend.md](research/lean-frontend.md)). `realbench.py` times
`blaze build` against `cargo build` on ripgrep, just and nushell (cold, no-op and
three kinds of edit), each `blaze build` row saying from its `blaze-work` line how the daemon
linked it; the numbers are in [results.md](results.md#11-real-projects). `coldprof.py`
times a cold `blaze build` of a real project and its first
edit, from an empty or a filled store, with every line the daemon logged (`<home>/v2/serve.log`)
and the cores busy along the way; it fails when the build fails. v1's readiness check
([#11](https://github.com/indexable-inc/blaze/issues/11), `--max-wait S`, whose numbers are in
[cargo.md](cargo.md#what-a-build-does)) is gone: v2 has no workers to get ready, and `realbench.py`
no longer times a "ready" either. `--until TEXT` waits for a
line the daemon logs, and `--memory` follows what its processes hold. `warmstart.py PROJECT`
times a start on a warm store with nothing of rustc's ([#41](https://github.com/indexable-inc/blaze/issues/41)):
a first checkout on an empty store, a second on the store it filled, that daemon stopped and
started, and once more, each followed by a body edit and its undoing; it keeps what each worker
said in `WORK/said-STEP.txt` and exits 1 when a program's output is not what blaze's run of its checkout from nothing prints
([research/warm-start.md](research/warm-start.md)). Their docstrings say how to run them.
`bench.py` on real projects, with `benchreport.py`, is also what writes the benchmark
tables and the README's numbers: [benchmarks.md](benchmarks.md). It runs every blaze build
under `--message-format=json` and reads how it linked, what it compiled and its steps' times
from the `blaze-work` line (`links`, `compiled`, `steps_us`), and the daemon's pid from its lock.
Besides the edits `bench.py` times repeated cold builds
against fresh cargo with an empty and with a warm store (`--cold-runs`), the same steps under
the fastest cargo setups (`--setups`: mold, wild, `-Zthreads=8`, cg_clif and `cargo check`) and
the test suites under both (`--tests`). v2 has no runtime bench yet: benchmarks.md's runtime
table is v1's `runbench.py`'s last result. `abbench.py` runs two or more builds of blaze against each other on `bench.py`'s
edits, in interleaved rounds: an edit's wall time, its steps' times from the `blaze-work`
line's `steps_us`, the daemon's cycles and instructions (counters.py; the cold start's on macOS
alone), the cold start, and the daemon's peak memory. v1's edit-to-binary time, from a worker's
log, has no v2 counterpart and is gone. It is how the release build's
profile was chosen ([reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)), and with `--train` it
is the training of its PGO. `hotpatch.py` times an edit of a running program under Dioxus's
subsecond against `blaze build` and `cargo build` with a restart; its program is
`harness/hotpatch`. `benchhistory.py` appends a run to `docs/benchmarks/history/` (what the
site's /bench page charts) and, with `check`, fails when a median moved past its noise band:
instructions first, with a 1% band, then wall time and memory with theirs, after a summary of
what regressed and improved. `counters.py` is how every harness counts instructions and cycles
(Linux's `perf_event_open`, macOS's `proc_pid_rusage`), null with the reason where it cannot
([benchmarks.md](benchmarks.md#instruction-counts)).
`memory.py` is how every harness measures memory: `memory.run` runs a command and returns its
time with the exact peak of its largest process (`wait4`) and the peak of its whole tree
(sampled), `memory.Sampler` follows a long-lived process such as the daemon with its workers,
and `memory.in_child` measures a function that runs commands itself
([benchmarks.md](benchmarks.md#memory-and-disk)); a harness that reports memory uses it rather
than a way of its own. Every published benchmark is a matrix over the platforms of
`benchplatforms.py` (macOS arm64, Linux aarch64, Linux x86-64): it names a result's platform from
its host, files it as `docs/benchmarks/<platform>.json`, and gives the workflows the platforms
whose runner is registered. `benchreport.py` writes each table from the newest stored result of
every platform, so one that did not run keeps its last numbers. `benchcharts.py` draws the
benchmark charts (SVG, a light and a dark version, a bar per platform) from the same results;
`benchreport.py` regenerates them whenever it writes a table, `--charts` on its own, and
`--render` every table and chart. With one typed JSON input (`'{}'` for the repository's own
paths) `benchcharts.py` also draws the trends, blaze against its own past runs, from
`docs/benchmarks/history` into the README and benchmarks.md
([benchmarks.md](benchmarks.md#over-time)); every workflow that appends to the history runs it and
commits through `.github/scripts/bench_commit.py`, which draws them again on top of main.
`live_latency.py` times live edits on macOS on AArch64 one at a time through `blaze build`, with
one typed JSON input (`{"blaze": PATH, "program": "probe" | "life" | "bevy" | "egui", "edits": 10,
"json": PATH}`; the fields are in its docstring); its JSON goes to `benchhistory.py live`. The
numbers once measured on a MacBook are in [live.md](live.md#latency-under-typing) (superseded).
`nix develop .#bench` has everything `bench.py` and `live_latency.py` need: blaze's stage 0,
the pinned LLVM, mold, wild and bevy's libraries, and no Rust toolchain (blaze brings its own);
zed's libraries are in `.#bench-zed`, since every library directory slows each link.

### The v2 proof

`harness/v2/` proves blaze v2 ([v2/proof.md](v2/plan/lanes/proof.md)). Every script takes one typed JSON
value with an `engine`, `{"kind": "v2", "blaze": PATH}` (`Engine.of` refuses `"v1"`, whose
worker logs and `ready` lines v2 doesn't have), and drives blaze only through
the `blaze` command line and `engine.py`. It was written to run against v1 first and v2 later,
unchanged; since 2026-10-30 there is only v2, so `"kind": "v2"` is the one that has a blaze to
run. One thing stops it on v2 today: `Engine.env` sets `BLAZE_BACKEND` for every command (and
`BLAZE_OPT` and `BLAZE_EMIT` for LLVM), and v2 refuses every unit while `BLAZE_BACKEND` or
`BLAZE_OPT` is set ([Under LLVM](#under-llvm)).

A daemon keeps its store in `<home>/v2/store` and reads no other, so a harness gives a build a
store of its own with a home of its own, and a store shared or warm across homes is copied to
`<home>/v2/store` before that home's daemon starts (`engine.store_of`, `storelib.store_of`).
`Engine.env` refuses a `store` anywhere else.

- Where v1 has no answer (codegen inputs, a query trace, table sizes, the critical path), the
  outcome is `unmeasured`.
- v2 answers them through the command line's forms of the agent API: the `blaze-work` line
  each build, test and run prints under `--message-format=json` (what it reran, compiled and
  found, how it linked, its critical path, its run time by node kind as `steps_us`, a unit's
  names, bodies, codegen, link and store each a node timed by what it does; under
  `BLAZE_STATS` the same as a `timeline-us:` line on stderr in every format),
  `blaze codegen-inputs --format=json`, `blaze trace
  --out PATH` and `blaze tables --format=json`. An answer missing or unreadable is an error
  naming the command.
- On v2, `unmeasured` fails too.

Each script writes `report.py`'s result (pass, fail, unmeasured or error per outcome) and has a
`selftest` that needs neither blaze nor the network. Run them with `uv run --frozen python
harness/v2/<script>.py '<json>'`. They are not in CI yet.

| Script | What it checks |
|---|---|
| `proof.py '{"engine": ..., "out": DIR}'` | Runs every check below with one engine (or those `checks` names, each with its own `given` input), and writes `DIR/report.json` and `DIR/report.md`: a line per check, the budgets as a table, every outcome that did not pass |
| `oracle.py run '{"engine": ...}'` | The invalidation oracle. Random edit sequences over `fixture.py`'s three-crate workspace, one slot per kind of edit of [v2/items.md](v2/plan/lanes/items.md) section 6, plus fixed sequences for [#906](https://github.com/indexable-inc/blaze/issues/906), [#1325](https://github.com/indexable-inc/blaze/issues/1325), [#1326](https://github.com/indexable-inc/blaze/issues/1326) and [#805](https://github.com/indexable-inc/blaze/issues/805). The slots cover bodies, signatures, impls, renames, moves, formatting, comments, cfgs, features, closure captures, `include_str!` inputs deleted, literal whitespace, names `type_name` and `module_path!` read, positions `line!` and `#[track_caller]` read, and the environment. After every edit, a daemon kept for the sequence must build what a daemon with an empty home and a std-only store builds of the same source: the same errors, program output and tests, and on v2 the same output bytes, codegen inputs and query trace (compared by `crates/blaze-v2-proof`). A state `fixture.py` marks as one Rust refuses has to be refused by both builds (`refuses/...`). An edit [v2/plan/targets.md](v2/plan/goals/targets.md) says compiles nothing (formatting, comments, renames and moves nothing reads, `PATH`) fails if it compiles anything. Findings land in `found/`, reduced, and `oracle.py replay` builds them again |
| `corpus.py` | blaze, nushell, ripgrep, helix and the 52 pinned crates.io crates of `corpus.json` (each `.crate` checked against the sparse index's checksum), built, run and tested by blaze: each build exits 0, each run exits 0, and each test passes by its own verdict (`blaze_cli.test_verdict`) unless the project's `OPEN` list names it; the backends are held to each other. It still resolves each project with `cargo fetch`, a no-rustc bug; whole workspaces are [realprojects.py's](#real-projects-the-verdict) |
| `frontend/cases.py` | The front end's cases under `frontend/cases/<area>/`, each held to its own header: `//@ expect: reject KEYS` has blaze give exactly those errors, `accept` none, and an accepted program exits 0 (or panics, with `//@ run: panic`) and prints its `.stdout` when one is beside it. `rejects/` holds the cases of [#1522](https://github.com/indexable-inc/blaze/issues/1522) and [#1440](https://github.com/indexable-inc/blaze/issues/1440); `bodies-cache/` is built twice on one store, the second serving each dependency from it |
| `determinism.py` | Two cold builds at the same path, pinned to different core counts (Linux), dev and release, of pinned projects and the oracle's fixture, must be byte for byte the same |
| `worktree_keys.py` | `a_second_checkout_at_another_path_reuses_every_key` (after kache's benchmark): one commit of ripgrep copied to checkouts at three paths and built in order by one daemon with one store, A from an empty store. B and C must compile nothing A compiled: every unit `fresh`, every build-script run found and naming only its own checkout's places, no `macro expand` span and no `compiled` macro module in their builds' history, the same `codegen-inputs`. C tells the portable keys' first-use verification apart from a key that differs. Naming each function B compiled with its key's parts needs `blaze-keys` lines blaze does not print yet, an error until it does |
| `tightmemory.py` | A cold build in a cgroup limited to 1.0 and 0.5 times cargo's peak, with swap off and `BLAZE_MEMORY_BUDGET` set (which no v2 crate reads yet), finishes with a program that prints what the unlimited cold build's prints, or refuses below 1.0 saying what it needs. It must never be OOM-killed or hang (Linux; `systemd-run --user` or a writable cgroup) |
| `budgets.py` | The targets of [v2/plan/targets.md](v2/plan/goals/targets.md) as rows, each with the number measured, the budget and today's figure: edit to result, signature and formatting edits, no-op, warm start, memory against cargo, dev and release code speed, a big crate, the cold build's core use and critical path, CI for a small PR. [v2/plan/data.md](v2/plan/design/data.md)'s rows: bytes per token, node, expression, statement, memo, edge and relocation, memory per thousand lines, and cache misses per edit |

The fixtures and adapters beside them are `fixture.py` (the oracle's workspace, a pure function
of a state), `engine.py` (the v2 adapter, the v1 one kept only for `proof.py`'s selftest of its log reader, and
cargo for fetching) and `report.py` (the
result's shape).

## The fuzzer

`fuzz/store.py` generates programs (`fuzz/gen.py`) and edits of them, and holds what a warm
daemon and a warm store build to what blaze builds of the same source from nothing. How to run
longer runs, what a finding is and what the fuzzer does not cover are in [fuzzing.md](fuzzing.md).

## Adding a test

A bug found in a real crate is proven fixed by that crate's own tests passing under blaze
(`blaze test` in the crate), or, where it
is small enough to state, by a unit test beside the
code that was wrong. A test states its expected result itself; none is "what rustc does". A
bug of the daemon, the store or the splice goes in the harness of that area as a step whose
warm build has to give what blaze's build from nothing gives.

## Reproducing a failure

Start from the script's last lines. Each daemon harness prints the path of the daemon's log
and of the scratch copy of the source it was editing, for example

    99 of 99 right, 0 restarts; log /tmp/blaze-store-out-XXXX/daemon.log; source /tmp/blaze-store-XXXX

and the stepwise ones take `--only TEXT`, `--from N` and `--keep` (see each docstring).

For wrong generated code:

1. `blaze codegen-inputs --format=json` lists each instance the last build made with its code
   hash, and `blaze trace --out PATH` writes every result of the last build with its edges, as
   CBOR. (v1's `--report`, `--deny` and `--isolate`, which listed, left to rustc and split out
   functions, are gone with its crate-at-a-time entry.)
2. `BLAZE_DUMP_MIR=<function name>` prints the MIR of functions with exactly that name
   (`{closure}` for closures). `BLAZE_DUMP_LLVM=1` prints the LLVM module of every function
   compiled, and `BLAZE_DUMP_CLIF=1` the Cranelift IR of drop glue.
3. Reduce it to a test ([Adding a test](#adding-a-test)), and fix it there.

For a CI failure, the last line of the harness step lists which scripts failed.

## CI

CI is a **merge gate** that takes minutes, and a **release train** that runs everything else on
main after the merge and releases main when it is green. Both call `blaze-platform.yaml` once
for each platform, each on a machine of its own kind.

| Workflow | Event | What runs | Wall time |
|---|---|---|---|
| `blaze.yaml` (`gate`) | pull request | on Linux AArch64, on a 64-vCPU ix runner ([ix runners](#ix-runners)): blaze builds blaze (`blaze build`) and tests it (`harness/unittests.py`, `blaze test`, only the tests the change reached), from the newest release and the store main pushed to blaze's remote; on Linux AArch64 (ix), `blaze check --workspace --message-format short` with the release `.github/blaze-version` names and the pinned LLVM, then astlog's rules over the crates and tools (`tools/astlog/scan.py`, [its README](../tools/astlog/README.md#rules-in-blaze)) (`workspace-check`, from main's last green run's snapshot); on Linux AArch64, the lint baselines, `blaze check --workspace --target aarch64-unknown-linux-gnu --baselines crates/blaze-lints/baselines` with the blaze the tree builds there (as the build job does, from main's warm store), not the release, so the counts are of the lints the tree defines (`lint-baselines`, [cargo.md](cargo.md#lints)); `harness/imports.py`, `harness/rustcsites.py`, actionlint over `.github/workflows` | 49 min on a store the release has not compiled into |
| `blaze.yaml` (`warm`) | push to main | the same on Linux AArch64 (ix) and macOS, every test, then a push of the store for the runs after it, and `linux x64 (cross-compiled, not run)`, blaze cross-compiled for x86-64 and not run ([ix runners](#ix-runners); TODO(x86-64): no x86-64 tests). Its `linux arm64 / build` passing is what cuts a canary (`canary.yaml`); the whole run green, unit tests on every platform, is what stable needs besides the promotion gate ([release.md](release.md)). And on Linux AArch64 the std cases of the smoke corpus (`harness/smoke.py`, [release.md](release.md#the-smoke-corpus): `blaze run` and `blaze test`, dev and `--release`) with the blaze blaze built, 17 to 30 min until [#899](https://github.com/indexable-inc/blaze/issues/899) (74 s with rustc's) | |
| `blaze-platform.yaml` (`profile-gate`) | with each platform's build, but Windows | the profile's gate (`blaze debug gate`, [profiling.md](profiling.md#the-gate-what-a-profile-must-hold)) over the history the build's self-build wrote, with the blaze it made, held to `harness/profile-gate-known.toml`, and each fixture of `crates/blaze-profile/fixtures/gate/` failing its own check (`harness/profile_gate.py`); uploads the build's per-build summary as `profile-summary-<platform>-<runner>-<commit>-<tree>-<blaze>`. No secret | a few min |
| `profile-nightly.yaml` | nightly, by hand | `slower_between_commits` over main's last 10 summaries of one runner class, the newest against the rest (`harness/profile_trend.py`); fails naming each span 1.25 times and 50 ms slower or more | 5 min |
| `fmt.yaml` | push to main | `blaze fmt --all` with the release `.github/blaze-version` names, and a commit `fmt` when it changed a file | 2 min |
| `train.yaml` | push to main, by hand | everything below, then a release or the train's issue | about 45 min |
| `blaze.yaml` | a change the build and the unit tests do not read | the two scripts | 1 min |

The build and the unit tests run only for a change that touches a file they read; any other
change runs the two scripts alone. `.github/scripts/gate_changes.py` (the `changes` job) names
those files, and why each: the crates (`crates/`, with their test data and the LLVM pin
`crates/hir-ty-codegen/llvm.json`), the vendored crates
(`vendor/`), `Cargo.toml`, `Cargo.lock`, `.cargo/`, `.rustfmt.toml` and `.clippy.toml`, the
pinned nightly (`harness/toolchain`), `harness/unittests.py`, `.github/scripts/llvm.py`
(which fetches the pinned LLVM), the profile gate's script and known list
(`harness/profile_gate.py`, `harness/profile-gate-known.toml`) and the flake that pins its
DuckDB (`flake.nix`, `flake.lock`), the release of blaze that builds blaze (`.github/blaze-version`,
`site/install.sh`), `blaze.yaml`, `blaze-platform.yaml` and the actions they use (`toolchain`,
`blaze`, `blaze-remote`); on a push to main, the smoke corpus too (`harness/smoke.py`,
`harness/smoke/`), which the `warm` tier runs. It names what is code rather than what is not,
so a new directory of documents, benches or harnesses runs no build; a list of files that
cannot be had (a run by hand, a comparison of 300 files or more, GitHub refusing) counts as
code. When the build or the unit tests start reading a new file, add it there.

The build and the unit tests of the `gate` and `warm` tiers install no rustup and no Nix: a
checkout, then `.github/actions/blaze`, which installs the release of blaze as a user does, with
install.sh fetching its own Rust (the release's pinned `rustc`, `cargo`, `rust-std` and
`rust-src`, into `~/.blaze/toolchains`,
[self-contained.md](self-contained.md#blazes-own-toolchain-the-stopgap)), and for the build the
pinned LLVM as a contributor outside Nix gets it
([contributing.md](contributing.md#llvm)): `.github/scripts/llvm.py fetch`
into `/usr/local/lib/blaze-llvm/<triple>`, checked against `crates/hir-ty-codegen/llvm.json`'s
sha256, as `LLVM_SYS_231_PREFIX`. Neither is cached by CI. The C compiler, linker and libc are
the runner's (gcc and glibc 2.39 on Ubuntu 24.04, Xcode's clang on macOS), so what the build and
the tests link runs on the runner; the train's tiers still build with cargo in the flake's dev
shell, against its glibc, and their harness jobs install Nix for that.

A change reaches main through a **pull request** alone
([contributing.md](contributing.md#landing-a-change)), merged directly once `ci` passes on it;
there is no merge queue, and the branch need not be up to date with main (removed 2026-10-27:
the queue's wait and its reruns on entries that later landed unchanged cost more than the rare
pair of changes that pass alone and break together, which the train catches on main). The gate
is the build and the unit tests, the released blaze checking the workspace, and the profile's
gate over the build's own history: a change that does not build, breaks a unit test, leaves a
workspace the release cannot load and check, or makes a gate check fail or stop running that the
known list does not name, does not merge. What the harnesses find, they find on main, in the train, which says
which commits since the last green train it came in with. Before the gate was this small, a
merge-queue run took 35 to 45 minutes, nearly all of it harnesses (harness/ci.py's `quick-2`
on every platform), and 51 of 140 queue runs on 2026-10-25 and 26 failed, most of them on
entries that later landed unchanged.

No check refuses a pull request for its formatting: `fmt.yaml` formats main after each push and
commits the result as `fmt`. Run `blaze fmt --all` before pushing all the same, so that the
diff of a pull request is its change.

`harness/imports.py` has every script of the harness imported, so one that only the train or a
person runs can't break unnoticed: a script does nothing when imported (its work is under `if
__name__ == "__main__":`). `harness/rustcsites.py` fails when blaze's code gains a place that
starts rustc, rustdoc or cargo ([no-rustc.md](no-rustc.md)).

actionlint (the release binary, pinned by its sha256 in `blaze.yaml`'s `workflows` job) checks
every workflow, and each local action where a workflow uses it, on every pull request: a workflow
whose YAML does not parse fails on push with no jobs and no error anywhere (#1691). Its shellcheck
and pyflakes passes are off; `.github/actionlint.yaml` lists what it ignores, each with why.

The repository's Python is one uv workspace (`pyproject.toml`, `uv.lock`, Python 3.13 from
`.python-version`; shared helpers such as `tools/typed-input` are its members). Its scripts run
with `uv run --frozen`, and the gate runs ruff and pyright over the ones the workspace lists
(today `.github/scripts/downstream.py`, `harness/focused.py` and `python/`); an older script
moves in when it is next changed. `harness/imports.py` runs in the workspace's Python too.

### ix runners

blaze's CI runs on indexable's own GitHub Actions runners, ix's
([ix.dev/docs/runners](https://ix.dev/docs/runners)), with no switch to turn it off, and on
Linux AArch64 alone: ix has no x86-64 capacity today (its runner plane refuses a job for an
architecture it has no node for, and an `x64` job then waits a day), so every Linux job runs on
`arm64`, builds there and tests there.

**TODO(x86-64): blaze's x86-64 tests are missing.** No job runs blaze's tests on Linux x86-64
until ix has x86-64 capacity; then `linux x64` comes back to `blaze.yaml`, the train,
`focused.yaml`, promote.yaml's smoke and apps, install.yaml's installs and toolchain-bump's
answers. Until then `linux x64 (cross-compiled, not run)` (`blaze.yaml`, every tier, not part of `ci`)
cross-compiles `blaze` and `blaze-engine` for `x86_64-unknown-linux-gnu` on an ix AArch64
runner, with Ubuntu's x86-64 cross gcc and glibc for C and the link, and qemu-user to run the
x86-64 `llvm-config` the pinned LLVM's build script asks; it checks the binaries are x86-64 ELF
and runs nothing.

- The gate's build and unit tests (`linux arm64 / build`, `linux arm64 / tests`),
  `workspace-check` and `lint-baselines` run on `[ix, arm64, ix-cpu-64, blaze-<job>]`: 64
  vCPUs, and memory that grows on demand up to 256 GiB, where a hosted runner has 4 vCPUs and
  16 GB. A cold build of blaze needs about 28 GB and on a hosted runner swapped for over an
  hour; across main's last 150 runs before the move, hosted `linux x64 / build` passed 0 of 54
  times (median 86.5 min to its failure).
- Every job of `blaze-platform.yaml` for `linux-arm64` (the train's and `focused.yaml`'s
  harnesses, smoke, the larger projects, the profile gate, the bench) runs on ix: 64 vCPUs for
  the build, the unit tests and the projects, 16 for the rest, each with a label of its own.
- The small jobs (`changes`, `scripts`, `actionlint`, `ci`, the train's verdict and release,
  canary and release cuts, the profile trend, the bench workflows' matrix jobs, ...) run on
  `[ix, arm64]`, ix's default size.
- Clean-machine jobs and jobs holding a long-lived secret run on **cold** runners,
  `[ix, arm64, ix-mode-cold]`: booted from the base image every time, never snapshotted and
  never a seed, so nothing an earlier job left is there and nothing they leave reaches a later
  one. The clean machines: install.yaml's installs and promote.yaml's smoke and apps. The
  secrets (the bot token, Cloudflare's, crates.io's): fmt, site, the bench and live-latency
  history commits, promote.yaml's and yank's metadata, release.yaml's notes-free jobs (crates,
  publish, metadata), pgo's pin and toolchain-bump's branch and pull request.

Every warm (not cold) job asks for `ix-trusted` in place of `ix` on main's `push`, `schedule`
and `workflow_dispatch` runs, and for `ix` everywhere else:
`"${{ (github.event_name == 'push' || ...) && github.ref == 'refs/heads/main' && 'ix-trusted' || 'ix' }}"`.
ix restores trusted (pre-scrub) memory only into a runner registered as `ix-trusted` without `ix`,
so GitHub can never hand a pull request's job to it; before this, a PR job could land on a
trusted runner and fail with "IX job admission Denied: untrusted assignment cannot execute with
inherited trusted memory" (ix's docs/design/ci-on-ix.md, "Trusted memory"). Both labels fork one
seed, so pull requests still start warm from main's clean snapshot.

Every warm ix runner is a fork of the snapshot of the last green job on main with the same
labels, its disk included: the checkout, `target/`, the toolchain, the pinned LLVM, `/nix/store`
and blaze's store, which `.github/actions/blaze` keeps under `$HOME/.cache/blaze-ci/store` on a
self-hosted runner (ix's scrub deletes `_work/_temp` before a snapshot) and bounds at 200G. The
pull from the remote then fetches only what main pushed since. No swap is made on ix: its scrub
refuses to snapshot a machine with swap on. The image is Ubuntu 24.04 with build-essential, git,
gh, jq, python3 and Nix, and lacks some of what a hosted image has: `.github/actions/rustup`
installs rustup when there is none, `.github/actions/nix` uses the image's Nix in place of
Determinate's installer, and an `apt-get install` first fetches the package lists the image does
not ship.

A run off main never becomes a snapshot, and before every snapshot ix's scrub removes what a job
could leave for the next (processes, the job's credentials, git credentials and logins, `/tmp`),
since a fork's pull request restores the same snapshots. A pull request from a fork runs only
once a maintainer approves it: the repository's fork pull request policy is
`all_external_contributors`, and no workflow uses `pull_request_target`.

What stays on GitHub's hosted runners, and why:

- **One macOS job in the gate path, `macos arm64` in `blaze.yaml`'s `warm` tier**: blaze's
  Mach-O output, its ad hoc signature, dyld and the macOS std are only exercised by running on
  macOS, and blaze cannot cross-compile for `aarch64-apple-darwin` from Linux today (below). The
  gate's `workspace-check` is Linux alone (TODO(macos): a cross `blaze check --target
  aarch64-apple-darwin` once the SDK is in CI). The other macOS jobs run what users run on a
  Mac: release.yaml's, llvm.yaml's and pgo.yaml's darwin builds, install.yaml's and
  promote.yaml's macOS installs and smoke, toolchain-bump's and profile-nightly's macOS entries.
- **Builds of what users run on Linux**: release.yaml, llvm.yaml's and pgo.yaml's builds and
  install.yaml's archive (and the no-Rust containers that install it) run on hosted Ubuntu 22.04
  so that the binaries ask for an old glibc (`build_release.py` refuses one newer than RHEL 9's
  2.34); the ix image's glibc is 2.39. **TODO(sysroot)**: blaze has no `--sysroot`; it finds crt
  objects and libc through `$CC_<triple> -print-search-dirs`, and the sysroot archives of
  [toolchain-c.md](v2/plan/toolchain-c.md) are not built. A driver given one
  (`CC_aarch64_unknown_linux_gnu="gcc --sysroot=<glibc 2.34 tree>"`) finds the old glibc's files,
  but blaze-link then takes the ELF interpreter from inside that tree, as libc's directory is not
  an FHS one (`crates/blaze-link/src/target.rs`, `fhs`), so the binaries would not run elsewhere. Once blaze links against an old glibc
  sysroot, these move to ix.
- Windows, which ix does not serve.
- The bench workflows' measurements run on the machines `BENCH_RUNNERS` registers.

How far blaze cross-compiles today: `--target` takes one foreign triple; std is built from
`library/` for it, Cranelift and LLVM emit for it, and blaze-link writes ELF and Mach-O itself,
signing Mach-O ad hoc (no `codesign`). What a cross build still borrows from the host: a C cross
compiler as `CC_<triple>` (crt objects, libc, build scripts' `cc`), read when the daemon starts;
the pinned LLVM's build script runs the target archive's own `llvm-config` (qemu-user for
x86-64, impossible for a Mach-O one on Linux); and `build_release.py` builds only for the host.
For `aarch64-apple-darwin` on Linux it also needs a macOS SDK at `SDKROOT`, a darwin clang as
`CC_aarch64_apple_darwin` and `otool`, none of which CI has, so no darwin cross job runs.

### Named harnesses, and the projects downstream

Two workflows run part of what the train does, by hand, for a change that wants an answer
before or without the whole train. Neither releases anything or is part of `ci`.

`focused.yaml` runs the harnesses it is given by name (`harness/focused.py`: a label of
`ci.py list '{}'`, or its start up to a space) against one build of the ref, on the platforms asked
for, as the train builds and runs them:

    gh workflow run focused.yaml --ref <branch> -f harnesses='["anonstatics.py", "testselect.py"]'
    uv run --frozen python harness/focused.py run '{"harnesses": ["anonstatics.py"]}'

The second line is the same in a lane's VM. A name that is no harness's, or that runs nowhere
on a platform, fails rather than run nothing. This is the part of Astral's per-change
selection (astral-experiments/blaze#7, #58) that applies here: their fork had a model pick the
suites and harnesses of each pull request, with path rules when it gave no answer. Here the gate
is already the build and the unit tests alone, and `blaze test` picks which unit tests to run
from what the change reached (`blaze-platform.yaml` sets `BLAZE_TESTS=affected` for the
released v1 blaze that runs the gate; v2 reads no `BLAZE_TESTS`, and its form is `blaze test
--reached`), which a model's
guess or a path rule would only make less certain; so whoever makes the change names the
harnesses, and the train still runs all of them on main.

`downstream.yaml` runs ruff's (with ty) and uv's own test suites under `blaze test`, at pinned
commits (`.github/scripts/downstream.py`, from astral-experiments/blaze#38, #41, #44 and #80):

| Suite | What it checks |
|---|---|
| `ruff-and-ty` | ruff's workspace with every feature, one libtest thread (its language-server tests install a process-wide panic hook) |
| `uv` | uv's workspace with its Linux test features, on the filesystems its tests ask for (a tmpfs, ext4's hardlink limit), a Secret Service keyring and the managed Pythons; one libtest thread (its native authentication tests share the credential store); the three cases that share one inline snapshot each in a process of its own; its five copy-on-write tests on a Btrfs loop image on a second runner, with the executables the first built |
| `uv-archive-fixtures` | uv's 39 ZIP extraction cases alone |
| `uv-lock-edit`, `ty-semantic-edit` | a diagnostic and its test's expected text edited, then the diagnostic restored: the test passes, passes, then fails printing the edited text, so an executable the edit did not reach fails it |

Each runs under `--message-format=json-render-diagnostics` and has to print blaze's `blaze-work`
message, so a run that built nothing does not pass. It tests a release (`-f blaze=canary`, the
default, `stable` or a version), or with `-f build-run=<id>` the `blaze-linux-arm64` build of a
`focused.yaml` or train run: the way to try a branch on them before it lands. Build scripts and proc macros run in blaze's sandbox, as for any user.

### The release train

`train.yaml` runs by hand only for now (`gh workflow run train.yaml` on main; `--ref <branch>`
runs it on a branch, without the release or the issue). On every push to main it never
finished and was red for reasons the merge gate (`ci`) does not share, so releases are cut by
hand until it runs on pushes again: `gh workflow run release-cut.yaml` runs the train's release
job alone on main (a tag on main's HEAD, and release.yaml on it). One train runs at a time: a
push while it runs waits, and a newer push replaces the one waiting, so the train always runs
on the newest commit of main once the one before it is done. It does not cancel the running
train: with a merge every few minutes, a train of 45 would never end.

It builds blaze once per platform (Linux x86-64, Linux AArch64, macOS arm64, Windows x86-64),
with LLVM, as every native blaze has it, hands the binary to its other jobs, and runs:

- `harness/unittests.py` on each platform;
- every part of `ci.py parts`, a job each: `core-1` ... `core-N` (the
  harnesses of `core`, packed by the seconds `CORE` in ci.py says each takes, so that no part
  is longer than its longest harness), `keys-1` ... `keys-3`, `store-kills`, `store`,
  `daemon` and `incremental` ([Incremental cost](#incremental-cost));
- `blaze_cli.py`: its checks, ripgrep and hyperfine, and fd (Linux x86-64 and macOS), and
  ripgrep, fd, bat, hyperfine, just, axum and nushell with each project's tests (Linux AArch64;
  axum and nushell not blocking, [#26](https://github.com/indexable-inc/blaze/issues/26) and
  [#734](https://github.com/indexable-inc/blaze/issues/734));
- the ripgrep bench (not blocking).

`nix-live.yaml` (the unit graphs, the flake's builds, the demo and live edits on macOS) runs on
the same pushes to main in a workflow of its own, one run at a time: a cold nix build takes up to
two hours, and while it was in the train's run main's next train waited for it. Nothing waits on
it.

Versions are dated and chosen by nobody: `0.YYYYMMDD.N`, N counting the releases of that day
(UTC) from 0 (0.20261026.0, then 0.20261026.1), which is semver, as cargo and crates.io need.
`blaze --version` says the commit and its date too, `blaze 0.20261026.0 (fb82b09 2026-10-26)`, and a
build of the tree `blaze 0.0.0-dev (fb82b09)`: crates/blaze-engine's build.rs takes the release from
`BLAZE_RELEASE` (release.yaml sets it on a tag) and the commit from `BLAZE_GIT_SHA` and
`BLAZE_GIT_DATE` (release.yaml and the flake set them) or the checkout's HEAD. The version is
never committed to main (Cargo.toml stays `0.0.0`), so a release changes no crate's identity
and no build of main misses the store after one ([release.md](release.md#versions)).

The job `train` is the train's verdict: every job but the bench. When it is green and
main has changes since the last release, the job `release` (`.github/scripts/train_release.py`)
tags main's commit `v0.YYYYMMDD.N`, today's next release, through the API and runs `release.yaml`
on the tag. No commit goes to main. The last release is the newest dated tag on main's history.
The bots' own commits
(`fmt`, `pgo: ...`, `release: ...`) are not changes to release.

When the train is red, or its release failed, the job `train issue`
(`.github/scripts/train_issue.py`) opens the issue "Release train is red", or comments on it
while it is open: the jobs that failed, the harnesses that failed in each harness part, and the
commits since the last green train. The next green train closes it.

A check that fails now and then is **quarantined** in the fast tier, never dropped: it runs,
its failure and output are printed with "FLAKY, not counted in the fast tier" and its issue,
and it still counts in the full tier. The list is `FLAKY` in the harness (today one step of
`testselect.py`, [#412](https://github.com/indexable-inc/blaze/issues/412)); a check comes off
it when its issue is fixed. The fast tier (`ci.py parts '{"tier": "fast"}'`, `ci.py run '{"part": "fast"}'`) is what a lane
runs by hand in its ix VM: every harness of `FAST`, in parts of about four minutes.

### Concurrency, the check and the ruleset

On a pull request a newer push cancels the gate's run before it. Every push to main runs the gate to the end in a concurrency group of its own: with one group for
main, a merge every few minutes replaced each waiting run with the next, and 7 of 10 runs on
main were cancelled.

`ci` is the gate's job that says whether everything that ran passed, and it is the one check
the branch rules require. A job that is renamed or split does not change it.

The repository ruleset `main` (Settings, Rules) is active and holds the default branch to three
rules: a change needs `ci` to have passed on its commit (not strict: the branch need not be up to
date with main), the branch can't be deleted, and it can't be force-pushed. There is no merge
queue (removed 2026-10-27). Repository admins bypass the
rules (`bypass_actors`), for repairs; work lands through pull requests. A ruleset can't exempt the
token a workflow runs with, so the workflows that commit to main (`fmt.yaml`, the train's
`release`, `pgo.yaml`'s pin, `bench-readme.yaml`, `bench-bun.yaml`, `live-latency.yaml`) check
out with the secret `BLAZE_BOT_TOKEN`, a fine-grained token of a repository admin (Contents
read and write on this repository alone) that the admin bypass lets past; without it their push
is refused. The release tag is made with the workflow's own token, which starts no workflow, and
`release.yaml` is started on it by dispatch. `gh api repos/indexable-inc/blaze/rulesets/25516056`
shows the rules, and Insights, Rule insights what they refused.

What is known to be wrong is written down, says why, and does not fail the run. Once it comes
out right, it fails the run until it is taken off the list:

- `harness/known-failures.toml`: Rust unit tests that fail, by crate and full name, each with
  an owner (a lane or an issue) and a one-line cause, held by `harness/failures.py`;
- `OPEN` in `dwarf.py`: `realdebug.py`'s checks whose frames or variables are wrong;

`failures.py` reads a run's log (`cargo test` or `blaze test`, libtest's text or JSON format)
and fails on a failing test that is not listed, a listed test that passes, is ignored or no
longer exists, and a test binary that did not finish (a crash leaves the rest of its tests
unchecked); each failure names the crate, the test and the tail of its output. Entries owned by
`unowned` are listed without failing the run, and listed tests the run could not check (their
crate did not run, filtered out, unreached by `blaze test`, or `only` for another runner,
profile or OS) are counted, so a partial run never reads as clean. CI's `unittests.py` calls it
on its log; a lane runs it on the crates it touched before saying a failure was there before:

    uv run --frozen harness/failures.py '{"run": {"runner": "blaze", "packages": ["hir-def", "hir-ty-mir"]}}'
    uv run --frozen harness/failures.py '{"results": ["test.log"], "runner": "cargo", "profile": "dev", "os": "macos"}'

A failure joins the list only with an owner and a cause, in the change that explains it.

### The other workflows

`bench-readme.yaml` measures real projects on each platform's dedicated benchmark machine and
commits the tables and the README's numbers to main. It runs a platform only once the
repository variable `BENCH_RUNNERS` names it ([benchmarks.md](benchmarks.md#the-machines)).
`bench-counts.yaml` runs on every push to main on the same machines: the stress corpus and two
small projects, judged by instruction counts first
([benchmarks.md](benchmarks.md#the-stress-corpus-and-the-per-merge-run)).
`bench.yaml` (the store's microbenchmarks, two minutes) runs on pull requests and pushes too.
`bench-bun.yaml`, `live-latency.yaml` and `realprojects.yaml` ([Real projects](#real-projects-the-verdict)) keep
schedules of their own: each takes hours or a dedicated machine, and the train would wait on
them. On a pull request that changes the flake, `nix/` or the dependencies, `nix-live.yaml`
checks only the unit graphs. The flake's `dist` still calls `build_release.py` with the retired
cargo recipe's flags, which the script now refuses (blaze builds blaze). `live-*.yaml` run on their own branches. None of them is part of `ci`. `site.yaml` deploys the
site, and `release.yaml` builds releases from tags, each target through
`.github/scripts/build_release.py` (the `dist` profile, built by the stage-0 blaze, without
PGO for now). `pgo.yaml` trains profiles with `tools/pgo/pgo.py`, by hand, and pins them in
`tools/pgo/profiles.json`; blaze cannot train one yet (it refuses `-C profile-generate`).
