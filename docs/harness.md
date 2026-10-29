---
status: current
summary: The harnesses that compare blaze with rustc, what each checks, and how CI runs them.
---
# The harnesses

blaze is tested by comparison with rustc. Almost every script under `harness/` builds
something with cargo and rustc, builds it again with blaze's code in place of rustc's for
some crates, runs both, and compares standard output, standard error and exit status. A
difference is a failure. So is a function blaze refused, in the harnesses that count them.

There are also Rust unit tests in `crates/blaze-engine` and in the code generator; the harnesses
are what carries the weight.

`checktargets.py` compares plain and explicit-test checks with cargo, using workspace and
registry dev-dependencies (#634, #632). Errors in unselected tests, examples and benches must
not fail a plain check; errors in libraries and binaries must, and an all-targets check must
report the dev targets' errors. Each target flag must pick what cargo's does: `--tests` no
example or bench, `--test NAME` the one test, `--lib` no unit tests, and a member only
depended on its library alone.

`nativespans.py`: A native proc macro asks for a span's source text, line, column, byte range and file; edits and a daemon restart must agree with Cargo, under native and wasm expansion (#823).

`gitindex.py`: A local git-index registry fetches two packages with Cargo-equal paths, source trees, archives and index cache bytes; each tool then reads the other's cache offline (#316).

`deriveerrors.py`: Unused invalid built-in derives fail check and build as Cargo's do; a valid edit and a daemon restart preserve the verdict (#751).

## Running them

You need Python 3, the pinned nightly (`harness/toolchain`) with `rust-src`, a C compiler
and linker, and a release build of blaze. `rust-src` is for rustc's side alone: a program
rustc builds names a file of std it inlines (a panic's location) where `rust-src` installed
it, as blaze names its own `library/`'s; blaze reads no toolchain's `rust-src`. Some harnesses need
more: `debuginfo.py` needs gdb and `llvm-dwarfdump`,
`buildmodel.py` needs the `aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`
targets with their cross tools and qemu, `remote.py` needs a MinIO or SeaweedFS binary,
`storefull.py` needs passwordless `sudo` to mount a tmpfs.

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
A change is not done until all of it exits 0 under both backends.
`harness/ci.py` is the one list of it: which harness runs in which part, in which tier, on
which hosts and under which backend.

    uv run --frozen python harness/ci.py list '{}'                  # every harness, its part, fast-tier seconds and hosts
    uv run --frozen python harness/ci.py parts '{}'                 # the full tier's parts on this machine (the train's)
    uv run --frozen python harness/ci.py parts '{"tier": "fast"}'   # the fast tier's parts here (quick-1 .. quick-N)
    uv run --frozen python harness/ci.py run '{"part": "core-1"}'   # run one part here, as CI does
    uv run --frozen python harness/ci.py run '{"part": "fast"}'     # every fast harness of this host, one after another

`ci.py` runs from the repository's root in its Python workspace (`pyproject.toml`, `uv.lock`),
and each harness in the Python that runs it. A harness moved to one typed input (AGENTS.md,
scripts-take-typed-input) reads it with the workspace's `typed_input`, so it runs the same way
by hand: `uv run --frozen python harness/keys.py '{"cases": ["a feature"]}'`.
`includes.py`, `keys.py`, `fuzz/edits.py`, `fnincr.py`, `check_json.py` and `ci.py` itself take their JSON so far; the
fields are in each script's docstring.

The flake's `scripts` shell supplies uv, Python 3.13 and Node for pyright, all from `flake.lock`:
`nix develop .#scripts --command uv run --frozen pyright` (the same workspace as CI).
The development, wasm and benchmark shells also provide uv and Python 3.13. On NixOS they
select Nix's interpreter explicitly and disable Python downloads; uv's standalone Linux Python
does not use Nix's dynamic loader. CI installs uv 0.13.5 on every harness runner, Windows
included, and `uv run --frozen --no-dev` installs the workspace helper without the lint tools.

For diagnostics, `uv run --frozen python harness/check_json.py '{"cases": ["two_codes"], "keep": true}'`
selects cases and keeps their scratch directory. An empty `cases` runs all cases; `blaze_bin`
names the blaze-engine executable, defaulting to `target/release/blaze-engine`.

The fast tier is the harnesses marked fast in `HARNESSES` that `FAST` (in `ci.py`) gives to
this host: each line there has the harness's measured seconds on CI (the slowest median of
2026-10-25 and 26) and the hosts it runs on. A harness whose result depends on the platform
(the loader, the linker, signing, the daemon's sockets) runs on all three; one that checks
generated code or the build model against rustc runs on Linux x86-64 alone in the fast tier and
everywhere in the full tier. `ci.py` packs them into parts of about 240 seconds (`PART_SECS`,
longest first), so adding a harness adds a part rather than lengthening one; a fast harness
with no `FAST` line is an error, and so is a `FAST` or `CORE` line that names no harness. Each
harness runs with an `BLAZE_HOME` of its own under `/tmp/rj-ci-*` (not `BLAZE_CACHE`, which would turn the store on), and its daemon is stopped
afterwards, so no two harnesses of a job share a daemon or a socket (on a runner `XDG_RUNTIME_DIR` would otherwise give every
harness `/run/user/1001/blaze`). `live_e2e.py` and `live_types.py` take a
home of their own when run by hand too.

A part runs all of its harnesses even when one fails, then names the ones that failed and how
long each took. Alongside the harnesses, CI runs `python3 harness/imports.py`, which imports
every script of the harness, and
`python3 harness/unittests.py`, which runs the Rust unit tests and holds them to its `OPEN`
list (below). It is one `cargo test` from the root over blaze's crates and the front end's
(`hir-ty`, `hir-def`, `hir-ide`, `ide-diagnostics` and the rest), all
members of the one workspace; the root `.cargo/config.toml` sets `CARGO_WORKSPACE_DIR`, where
expect-test finds the parser's `.rast` files.

### Under Cranelift

Cranelift is the default. Either leave `BLAZE_BACKEND` unset or set it:

    export BLAZE_BACKEND=cranelift

### Under LLVM

Build with the `llvm` feature in the flake's dev shell, which has the LLVM 23 it pins
([contributing.md](contributing.md#llvm-for---features-llvm)), then select the backend. The gate is run at `-O2` with fast instruction selection, and that is the
only LLVM configuration it covers.

    nix develop -c cargo build -p blaze-engine --release --features llvm
    export BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast

A binary built with `--features llvm` still uses Cranelift unless `BLAZE_BACKEND=llvm` is
set, so one build serves both runs. Under LLVM also run

    python3 backends.py corpus runner cases --bin runner --levels 2f

which reports how many functions LLVM compiled itself and how many it left to Cranelift,
with reasons. The daemon's log says `backend: llvm -O2 fast emit` when LLVM is in use.

## What each one checks

### Correctness of generated code

| Script | What it checks |
|---|---|
| `difftest.py <workspace> --bin <pkg> --crate <name>` | The base comparison. Builds the binary with cargo, compiles the named crates with blaze, links blaze's object ahead of rustc's rlibs, runs both. `--layouts` also compares the size and alignment of every type blaze laid out with rustc's `-Zprint-type-sizes`. `--bisect` links each blaze function in on its own to name the ones that change behaviour. `--mixed N` runs N builds with a random half of the functions left to rustc. `--memcheck` runs under valgrind |
| `harness/corpus/` | The workspace `difftest.py corpus` runs: a library with one module per area of the code generator and a runner that calls every function and prints the result |
| `wasm32.py [check...]` | The train's wasm32 job: harnesses run with `BLAZE_TARGET=wasm32-wasip1` (the corpus, `stress.py`) under wasmtime, each in a process of its own. `BLAZE_TARGET` takes any harness that builds a program there; see [reports/WASM32.md](reports/WASM32.md). Run in `nix develop .#wasm` |
| `breakers.py` | Every reduction under `harness/breakers/`, each run on its own so a crash in one does not hide the others. These are minimal cases of bugs found in real crates, rustc's tests and the fuzzer |
| `unwinds.py` | Edits to functions a panic unwinds through |
| `wasmsections.py [target ...]` | That the custom sections of the wasm object blaze writes for `harness/wasmsections` are those of rustc's object of it, for wasm32-unknown-unknown and wasm32-wasip1: a `#[link_section]` static is a custom section of that name on wasm (what wasm-bindgen reads), those of one name joined in order; and `target_features`, which wasm-bindgen reads to turn on externref |
| `growth.py` | That the code of one function grows in a straight line with its source: the `serialize` `#[derive(Serialize)]` gives an internally tagged enum, a `?` in every arm, at 160 and 320 arms (issue #5); and a `load` filling a struct of 200 and 400 fields, each a call's `?` while a value with a destructor is live, as x11-dl opens Xlib's 781 functions (the way out of each `?` once dropped every field before it on its own: 2,986,013 blocks, and 364 GB for bevy) |
| `asmedits.py` | Edits to `asm!` blocks and the code around them; with `BLAZE_ASSEMBLER=cc`, assembled by the C compiler, as a build without LLVM does |
| `wasmabi.py [target ...]` | That the wasm type of every `extern "C"` function blaze defines or imports in `harness/wasmabi` is that of rustc's object of it, for wasm32-unknown-unknown and wasm32-wasip1: an aggregate of one scalar (wasm-bindgen's `WasmRet<u32>`) goes as that scalar, a union or anything else in memory |
| `answers.py` | That blaze's own answers to rustc's questions (`-vV`, `--print cfg`, cargo's probe of file names, `target-spec-json`) are rustc's, byte for byte, on every target blaze has them of; `'{"write": true}'` regenerates them from the pinned rustc ([no-rustc.md](no-rustc.md), step B) |
| `scripts.py [name ...]` | Build scripts run by `crates/blaze-build-script` against the pinned nightly's cargo's runs of them, over the applications of `apps/apps.json` and `harness/scripts/` (openssl-sys, zstd-sys, libz-sys, libgit2-sys, ring, rustix and the scripts that read their metadata): for each run, the environment blaze computes against the one cargo gave, blaze's reading of cargo's output against cargo's own, and a run by blaze against cargo's. `--json` writes every difference ([no-rustc.md](no-rustc.md), step I) |
| `bscache.py record [project ...]`, `bscache.py report` | What each build script of the projects of `bench.py` reaches in the sandbox: `record` runs `cargo check --compile-time-deps` through `blaze-rustc`, so every script runs under `blaze-engine build-script` in the sandbox, and collects the sandbox's records; `report` classes each line as one a hermetic sandbox allows (the package, OUT_DIR, the toolchain, the C compiler and pkg-config) or would deny (network, writes outside OUT_DIR, other executables, other reads) and writes the list (`bench/results/2026-10-27-bscache/`). Needs the network |
| `toolchain_pins.py` | That the sha256 and size of each file of blaze's own Rust (`cargo/toolchain/pins.txt`) are what the release's manifest at static.rust-lang.org says, for every host blaze ships for; `'{"write": true}'` regenerates them when blaze moves to another release ([cargo.md](cargo.md)). Needs the network |
| `nightly_features.py '{"dry_run": true}'` | Not a check: keeps the GitHub issue tree of the pinned nightly's unstable features (#967), so every `#![feature(..)]` blaze has to accept is tracked. Each language feature of rustc_feature's `unstable.rs` at library/UPSTREAM's commit is an issue labelled `nightly-feature` under a group of at most 100 sub-issues; the library features (the `#[unstable]` and `#[rustc_const_unstable]` attributes of the sysroot crates in library/) are one issue, #969, for what blaze has to do for all of them (`'{"library": true}'` makes one each). A rerun creates only what is missing (by a hidden marker in each body), links what is not under its group, and comments on and closes the issue of a feature stabilized or removed since. `'{}'` makes the changes, paced under GitHub's limits. Needs `gh` |
| `norustc.py` | How often rustc and rustdoc run when `harness/norustc/fixture` is built, edited, tested, doctested and checked with `blaze`, through a copy of the toolchain that logs every run; a count above `norustc/baseline.json` fails ([no-rustc.md](no-rustc.md), step A). `--record` lowers the baseline |
| `fuzz/fuzz.py` | Generated programs, see below |
| `uitest.py <rust checkout>` | rustc's own `tests/ui` run-pass tests. Every file is either run or left out with a counted reason |
| `libsuite.py <rust checkout> coretests\|alloctests\|stdtests` | The standard library's test suites, cut down to what builds on stable |
| `cratetest.py <crate-dir>` | A crate's own integration tests: each test binary is relinked with blaze's objects and every test must report the same result |
| `cratesweep.sh`, `vendor.sh`, `sweepreport.py` | `cratetest.py` over a list of crates.io releases, and a summary |
| `bintest.py <name>` | A real program from `breakers/bins.json`: the same command lines given to cargo's binary and blaze's |
| `alone.py` | A program whose workspace crates rustc compiles nothing of, so no refused function can hide behind rustc's copy |
| `allblaze.py [whole] [unit] [stored]` | Programs with nothing rustc built in them, the standard library compiled by blaze ([no-rustc.md](no-rustc.md), steps F and G): a hello, one of collections, panics, threads and channels, one of 128-bit atomics raced by threads, and one of the paths of `split_paths_ref` and the XDG directories, each built with the library in the same build, over the library built alone (`--prebuilt`), and on a store and a remote that start empty, where the first build compiles and keeps the library and the later ones, one on a second store, take it. Each has to print and exit as cargo's build does. Linux |
| `macroserver.py` | The release's proc-macro server (`vendor/proc-macro-srv`) built by blaze, and the libraries blaze links of serde_derive, thiserror-impl, tokio-macros and clap_derive ([no-rustc.md](no-rustc.md), step J1): every call of a package's derives (with helper attributes), attributes and function-like macros is expanded by the toolchain's server with rustc's library and by blaze's with blaze's, and the outputs have to be equal, spans included; the package is then built whole with its proc macros by rustc and by blaze. `macroserver.py <app>` (or `all`) builds a bench app (fd, bat, hyperfine, just, nushell, helix) whole with `BLAZE_PROC_MACRO_SERVER`, so blaze links every proc macro, again into an empty target directory over that build's store (no library may be linked again), and again without, expanding every call by both servers; the counts, the calls not compared and why each proc macro was rustc's are printed, and both programs have to print the same. Needs crates.io. Linux |
| `frozenwarm.py` | Frozen def maps (#40, #161) of a dependency whose items, and a `macro_rules!` it exports, are in files `include!` reads and the VFS does not load at first, as the icu `*_data` crates': built on one store cold, warm twice, warm with `BLAZE_FROZEN=check` and `=verify`, and warm after an included file changed at the same path, which has its def map refused. Each has to print what cargo's build does. Linux |
| `depkeys.py '{}'` | Whether the store hands a build code of a dependency built with something else: a crate of a local registry is built with nothing of rustc's on an empty store, then one of its features, a cfg its build script sets, a variable its build script sets and a file its build script writes is changed and it is built again on that store. The program has to print what cargo's does after the change, and a second build with nothing changed has to compile nothing the first kept. Linux |
| `freshwarm.py PROJECT` | What a build costs on a fresh machine whose only warm thing is the remote (#41): a seed builds on an empty store and `blaze-engine cache push`es it; then for each edit (`--edits none,body,signature`, `none2` for a second unchanged one) the checkout is copied again to the same path, the home emptied, `blaze-engine cache pull` run and the build timed. Prints pull and build seconds, the time to ready, peak memory, what was compiled and found, the steps and threads' time and the `manifest:` lines; an unchanged build's program has to print what the seed's does. `--env` sets a variable on every step and `--fresh-env` on the fresh ones only (a build configured otherwise). `PROJECT` is one of `bench.py`'s or `blaze` (`--source` the checkout). Not in CI |
| `wholeprogram.py` | Programs blaze builds whole, with its own `main` (as `runbench.py` builds them): one on the default allocator, one with a `#[global_allocator]` of its own, one with a library's, one whose allocator is under a `#[cfg]` that is false. Each has to print and exit as cargo's build does, and a second build of it by blaze has to give the same object byte for byte ([#200](https://github.com/indexable-inc/blaze/issues/200)); `--release` builds both in the release profile |
| `bun_repros.py` | Nightly fixtures, one per thing bun's whole-program build found blaze doing wrong (`harness/bun-repros/`: `adt_const_params`, `core::mem::type_info`, macros, exported statics, frame pointers, …). Each compares output with cargo's binary and checks the report's refusals against the case's. A case can set its own `rustflags` and `env` (the frame-pointer case runs LLVM), and `alone` builds it as runbench builds bun, the link taking only blaze's objects. It builds with the pinned nightly of `harness/toolchain`, as everything else does. |
| `backends.py` | One program compiled by each code producer: counts, time per function, and what LLVM left to Cranelift |
| `lints_ui.py <rust checkout>` | The lints rust-analyzer finds at the level a source denies them, against rustc's UI tests that `deny` or `forbid` one: for each lint, the errors (lint and line) blaze reports that rustc does too (right), that rustc does not (wrong) and that only rustc reports (missed). A lint blaze fails builds with (`DENIED` in main.rs) has none wrong; one in a macro's expansion is not counted, as it fails no build. It scores against the pinned nightly's rustc; `--toolchain nightly-YYYY-MM-DD` against another. `lints_ui.py harness/lints_probes` scores the units written for the lints with few UI tests, one for each place a lint applies. `--deny LINT` scores one lint on every UI test, each read with `#![deny(LINT)]` first, for a lint few tests deny (`unused_mut`) |
| `borrowck_ui.py <rust checkout>` | The borrow checker ([borrowck.md](borrowck.md)) against rustc's borrow-check UI tests: each unit passes when blaze reports the errors (code and line) the pinned nightly rustc reports, under Polonius and NLL. `--min-pass N` fails below N Polonius passes; a panic, or an error on a unit rustc accepts, fails it too |
| `consteval_ui.py CONFIG` | The const evaluator against rustc's `tests/ui/consts` and `tests/ui/const-generics` at the pinned nightly's commit. Each revision of a test is a unit: blaze evaluates its free constants and statics and the constants of its inherent impls (`blaze-engine consteval`), and the unit passes when the ones that fail are the ones rustc's `error[E0080]`s name (none, for a pass test). The run-pass tests also go through `uitest.py`. Units rustc's verdict cannot be compared with (an auxiliary crate, a `-Z` flag, another error, a generic or anonymous constant) are counted out of scope with a reason. Writes `summary.md`, with every difference by kind, and `units.json` to `out`. `min_evaluation` and `min_run_pass` are the floors the train holds; a panic fails it too. Results: `bench/results/*-consteval/` |
| `check_apps.py [name ...]` | `blaze check` against `cargo check` on the applications of `apps/apps.json` (cloned and prepared as `borrowck_apps.py` does) and the corpus: both run `check --workspace --all-targets --message-format=json`, into target directories and daemon homes of their own, and the errors and warnings of the workspace's files are compared by file, line and code or lint. An error only blaze reports (invented: it fails a build rustc accepts) or only rustc does (missed) fails it; missed warnings are counted, as the checker writes errors only. Needs `blaze-proc-macro-srv` beside blaze (`.github/scripts/build_release.py --server-beside`); a cargo job compiles a build script with blaze in 1 to 2.5 GB, so a 16 GB VM takes `CARGO_BUILD_JOBS=4` |
| `scoreboard.py` | Runs the targets in `targets.json` and fails when behaviour differs or coverage drops below the committed `scoreboard.json` |
| `mirparity.py <workspace> --crate <name> [--level N] [--rustc-level 1\|2]` | blaze's MIR (`blaze-engine mir`, at `BLAZE_MIR_OPT` level N) against rustc's `-Zunpretty=mir`, function by function: the share that match in shape, statement counts, statements and types, and the most frequent differences. Not a gate: the numbers are in [research/mir-gap.md](research/mir-gap.md) |

### Reading the workspace as cargo does

| Script | What it checks |
|---|---|
| `resolve.py [bench\|crates\|workspaces]` | `crates/blaze-manifest` against the pinned nightly's cargo (or `CARGO_ORACLE`) on `cargo metadata --no-deps --format-version 1`: the projects of `bench.py`, 300 crates.io crates and 72 workspace repositories at pinned commits (`resolve/corpus.json`), the JSON compared as text after the root and `CARGO_HOME` are replaced, key order included, and the error text when both refuse a workspace. Prints the first difference of each workspace and the counts per part; [resolve.md](resolve.md) has them. `resolve.py DIR...` compares the workspaces at DIR... only. Needs the network |
| `resolve.py config` | The configuration as the pinned nightly's cargo merges it: `cargo -Zunstable-options config get` against blaze-manifest's `config` example, seven ways each, on the trees of `crates/blaze-manifest/tests/config-fixtures` and the root of every bench project and workspace repository; exit code, stdout and stderr compared as text |
| `resolve.py profiles` | Profiles as the pinned nightly's cargo gives them to units: the `profile` of every unit of `cargo <command> --unit-graph -Zunstable-options` (`RUSTC_BOOTSTRAP=1`) against blaze-manifest's `profiles` example given the same graph, for `build`, `build --release`, `test`, `bench`, `check` and a `--target` that is not the host, on the trees of `crates/blaze-manifest/tests/profile-fixtures` and every bench project and workspace repository; the warnings about profiles and cargo's error when it refuses them compared too |

The workspaces of `crates/blaze-manifest/tests/fixtures` (inheritance, members and targets
discovered, a package outside its workspace's members, config and manifest errors), the
config trees of `tests/config-fixtures` and the profile trees of `tests/profile-fixtures` (with
the unit graphs cargo printed) have cargo's output committed next to them, so
`cargo test -p blaze-manifest` checks them without cargo or the network; `resolve.py --fixtures`
writes them again.

### The daemon following edits

| Script | What it checks |
|---|---|
| `stress.py` | 100 edits of the kinds people make (bodies, signatures, new items, moved lines, broken code, reverts, an editor's edit while another client's build runs) applied in order to a three-crate workspace under one warm daemon, each compared with `cargo build` of the same source. The first edit cargo builds after broken code has to be a patch of the build before it ([#31](https://github.com/indexable-inc/blaze/issues/31)), and the edit sent during a build has to be taken ([#32](https://github.com/indexable-inc/blaze/issues/32)); it exits 1 when a step is wrong |
| `programs.py` | Two programs of one workspace built by one daemon. With `BLAZE_ALONE=1` rustc builds stand-ins for the workspace's crates and the daemon compiles every function of both programs, `main` too: under LLVM an edit of what both use has to reach each ([#63](https://github.com/indexable-inc/blaze/issues/63)) |
| `worktrees.py` | Several checkouts of one workspace built by one daemon, edited independently |
| `includes.py '{}'` | Edits to a file only `include!` names |
| `staticcallbacks.py` | A `LazyLock` static whose initializer calls an `#[inline(always)]` helper of a dependency: an edit of a sibling of the helper, then of the helper itself, under one daemon, each printing what cargo's build prints (astral-experiments/blaze#83, #87) |
| `latelisted.py` | What a file named by `include!` through a build script's variable declares is listed in its crate though the daemon reads the file only once the names are resolved, as bun's generated bindings are: an edit that adds a function then patches a few functions, not every one the file declares |
| `rustsrc.py` | A build without the standard library's source (`BLAZE_LIBRARY` naming an empty one) is turned down, saying how blaze gets it |
| `libraryfetch.py` | The release's `library-<digest>.tar.xz`, made by `library.py archive` and served locally, is installed from once (`BLAZE_LIBRARY_FROM`) with blaze's digest; a lying or missing archive is an error and nothing is kept |
| `splices.py` | Edits that are hard on the splice: thread-locals, statics, vtables, function pointers, closures, panics, foreign calls. Counts how each step was linked |
| `loadretry.py` | A refused load reports changed flags and recovers after configuration files are removed or rewritten, without restarting the daemon (#597). Both configuration filenames, inherited configuration, and cached source diagnostics are checked |
| `scriptenv.py` | A build script that says it reads `SDK_DIR` and fails without it (cef-dll-sys's shape) is given the command's value, with a fresh daemon and with one started without it, and another value or none reruns it; a path dependency that is a workspace of its own is given what the build's root manifest names in `build-script-env` ([#720](https://github.com/indexable-inc/blaze/issues/720)) |
| `farsplices.py` | Splices into a binary whose code is further from its end than an AArch64 branch reaches |
| `farfile.py` | Edits of a program whose file is 2.3 GB of a section nothing loads, as a debug build of bevy's is mostly debug info: bodies, a caller, a new function and taking it back each have to be spliced, which a patch placed as far from the code as it is behind the file is not ([#79](https://github.com/indexable-inc/blaze/issues/79)). Linux |
| `bigbinary.py` | Edits of a program larger than any place of the splice is sized for: 220 MB of code, over 16 MB of frames the unwinder reads from `__eh_frame` or `.eh_frame`, and on Linux 4.2 GB of `.debug_abbrev`. A function shorter than the long jump, one that panics through its replacement, 3,000 functions added and a new function each have to be spliced ([architecture.md](architecture.md#no-size-is-a-reason-to-link-in-full)). Nightly, Linux and macOS |
| `unreached.py` | Edits under `blaze` that call instances of another crate's generic function the executable did not reach when it was linked: each is spliced, not linked in full. The daemon's own links. Linux and macOS; not run on Windows, where `blaze` has been run by hand only |
| `anonstatics.py` | Edits under `blaze` that reach, through code rustc inlined at opt-level 1, a `static mut` inside a `const _: () = { .. }` of a crate rustc built (the shape of objc2's `declare_class!` ivar offsets, which winit's frame function reads): the daemon serves each build and refers to rustc's static, found under its `_` segment. Linux and macOS; not run on Windows, where `blaze` has been run by hand only |
| `artifactedits.py [case ...] [--release]` | An edit of a dependency reaches the file `blaze build` says it built ([#730](https://github.com/indexable-inc/blaze/issues/730)): a workspace of a library and a `bin`, `cdylib`, `dylib` or `staticlib` over it, built cold, unchanged, after a body edit that changes no behaviour (`std::hint::black_box`), one that does, and the edits taken back. A build that exits 0 has written the artifact, with bytes other than the build's before after each edit, and it computes what the source says (run, loaded with ctypes, or linked into a C program); one that cannot write it fails naming the kind (`dylib` and `staticlib` today, #262). `flags` is a program built with #731's rustflags (`-Z threads=8` with `RUSTC_BOOTSTRAP=1`, `--cfg tokio_unstable`, `-C target-cpu`, `-C force-frame-pointers=yes`, lld on AArch64 Linux), built and spliced by blaze. Linux and macOS |
| `loadedits.py` | A file written while a worker loads, after the worker read it and before it took its timestamp (held there with `BLAZE_LOAD_PAUSE_MS`), is built as written: the next build, and the one after an edit of another crate, print the new text ([#53](https://github.com/indexable-inc/blaze/issues/53)). Linux and macOS |
| `agents.py` | Random edits of the kind coding agents make, on a real workspace |
| `invalidation.py` | How much the database redoes after different kinds of edit |
| `testselect.py` | `blaze test` with `BLAZE_TESTS=affected` on a library's unit tests and an integration test of it, edited a step at a time: the tests that run are those whose code or what it calls changed, also through a library of the workspace both link (built without `cfg(test)`, which the daemon takes over from rustc), one added, one given `#[ignore]`, one that reads a file (only when the file changed, which `-v` names, from what the sandbox recorded), one that downcasts a `dyn Any` (only when what it calls changed), one in a private module named as a public function beside it (the harness names it through the module) and one that failed (until it passes); `BLAZE_TESTS=all` runs every test. Its workspace opts into sandboxed tests, so every test runs sandboxed: one that writes into the package leaves it as it was, and the harness fails where the sandbox can't be made (`BLAZE_SANDBOX=off` checks the unsandboxed path, where the hermetic tests are still kept); a package that has not opted in keeps its hermetic tests' passes all the same and runs its bench every time. Linux and macOS |
| `hermetic.py` | Every kind of reason a test is not hermetic, told from the code it reaches with nothing opted into: a library with a test of each kind (files, the environment, the network, a process started, `process::exit`, foreign code, assembly, a function pointer made of data, an atomic static, a `static mut`; the clock, a `HashMap`'s seed and a thread, which don't block) and pure ones. `blaze test --why --json` prints a sorted line a test, each with its kind and a call chain from the test, and `target/blaze/tests.ndjson` and `tests.summary.json` say the same; a second `blaze test` runs only the tests that are not hermetic; an edit three calls below a pure test runs it again and no other pure test; `--why` ranks the reasons. Linux and macOS |
| `sandboxbench.py` | What the sandbox of a test costs: a trivial program bare and in `blaze sandbox` (with and without a record), and a generated 600-test library, opted into sandboxed tests, under `blaze test` with `BLAZE_SANDBOX=off`, sandboxed under `all` and `affected`, and as a process per test, bare and sandboxed; medians of alternated rounds, pinned to `--cpus`. Linux and macOS |
| `doctestonce.py` | `blaze test --doc` on a workspace of three libraries against `cargo test --doc`: the workspace is loaded once for all of them, each library's results, `Doc-tests` line and cargo's lines of what failed are cargo's (every example passing, two failing with and without `--no-fail-fast`, one failing with it), and each example reads the directory, variables and dev-dependencies it has under rustdoc. Linux and macOS |
| `movedlines.py` | A panic reports its new line after lines are added above its function (seven ways to panic, each against cargo), and moving a function compiles nothing |
| `buildmodel.py` | One small cargo project per feature of cargo's build model, under both backends, with steps that change one input at a time |
| `blaze_cli.py` | `blaze` against plain cargo on seven real projects, 22 flag combinations, and 11 daemon lifecycle checks. Needs the network and about 20 minutes; not in CI |
| `debuginfo.py` | Backtraces, `llvm-dwarfdump --verify`, gdb stopping and stepping, locals |
| `live_e2e.py` | Edits of a running program that links `blaze-live`, each applied as a patch; then another program built meanwhile, a start again from the executable, and a panic that unwinds through the patched code. Linux x86-64 and macOS AArch64 |
| `live_types.py` | A running program's component types changed as a game changes them, on the daemon's own base: `V2` added beside `V1` with a derive, a static id cache and a trait impl, the code moved to it with the values converted and a static new in the edit, `V1` deleted, each a patch whose answer matches rustc's build and whose statics keep their values; then a field of `V2` changed, which restarts naming `struct V2`. Linux x86-64 and macOS AArch64 ([live.md](live.md#types)) |
| `live_latency.py` | Edit-to-running time of live edits on `harness/live`, `demo/life`, `harness/live-bevy` and `harness/live-egui`: isolated edits, bursts of valid keystrokes, real typing replayed key by key (most texts in between do not compile; a program told to restart is started again), and a session's restarts against `cargo run`. Writes p50 to p99 and every sample, with the machine and its battery, to `LATENCY_JSON`. Measures; checks nothing; macOS on AArch64, and Windows x86-64 where Python has no AF_UNIX and it talks to the daemon through Winsock ([live.md](live.md#latency-under-typing)) |
| `fncache.py` | `#[blaze::cache]` ([fn-cache.md](fn-cache.md)): a cold run computes and a warm one finds the result on disk; comment, format, moved-line and unrelated edits keep it, an edit of a function it reaches through another drops it; generic instances are kept apart; impure functions are refused saying where; a restarted daemon finds the same results; `blaze cache` lists them by function and `clear --stale` forgets the older code's alone; cargo's build caches nothing. `--live`: a live edit computes again and drops the old results, macOS on AArch64 |
| `spaces.py` | A two-crate workspace under a directory whose name has spaces, `$$`, `#` and quotes, built and edited with `blaze build`; every edit has to be applied by the daemon against rustc's symbols ([#14](https://github.com/indexable-inc/blaze/issues/14)) |
| `nocompact.py` | A program linked with `-no_compact_unwind`, as bun is, which has `__eh_frame` and no `__unwind_info`: its edits are spliced, and the unwinder finds the frame of the edited function where its symbol now is, behind the program's frames and, once a thousand new functions outgrow that room, with all of them moved into the header's ([#33](https://github.com/indexable-inc/blaze/issues/33)). macOS on AArch64 |
| `headerpad.py` | A program whose unwind table does not fit the room its link leaves after the load commands: the first link is told the room the unwind information of its objects wants and is linked once ([#173](https://github.com/indexable-inc/blaze/issues/173)), the edits after it are spliced, and a restarted daemon starts from the room kept beside the executable ([#13](https://github.com/indexable-inc/blaze/issues/13)). It is built for macOS 14, as nix's shell builds, which Apple's linker gives chained fixups unless told not to ([#30](https://github.com/indexable-inc/blaze/issues/30)). The daemon's own links. macOS on AArch64 |
| `tiers.py` | Tiers ([tiers.md](tiers.md)): `demo/lifebench` running under the daemon reaches levels 2 and 3 with output identical to cargo's, an edit of a tiered function demotes it and is applied at once, and `BLAZE_TIER=off` changes nothing. macOS on AArch64 with `--features llvm`; it exits 0 elsewhere |
| `tierbench.py` | The benchmark table of [tiers.md](tiers.md): `demo/lifebench` at level 1, 2 and 3 under a daemon and as cargo builds it (release, and `-O3` with the dev profile's checks), in interleaved rounds, the steady rate of each; `--together` runs a round's builds at once, for a loaded machine. Measures; checks nothing; not in CI |

### The store and the remote

| Script | What it checks |
|---|---|
| `keys.py '{"cases": [...], "shard": {"index": K, "count": N}}'` | Whether the store ever hands a build code compiled from something else. One case per thing code depends on besides its file's text; `cases` picks those whose names contain one of its strings, `shard` every Nth build from the Kth (CI's `keys-1..3`) |
| `storekills.py` | The daemon killed at random moments while writing its store |
| `storecorrupts.py` | Packs damaged on disk, and stores of other formats |
| `storefull.py` | A store on a disk with no room |
| `storeshared.py` | Several daemons and `blaze-engine cache` commands on one store at once |
| `storebuilds.py` | A store one build of blaze wrote serves another build of the same sources: the store keys by what blaze was built from, not by the bytes of its executable ([#105](https://github.com/indexable-inc/blaze/issues/105)). Linux and macOS |
| `storerevisions.py` | A fresh daemon on a checkout edited before it started, and on the undo of that edit from a store holding only the edited revision, gets nothing stale: each case changes one thing a function's code depends on besides its own text (a callee's body or signature, an inline helper, a rebound import, a module constant, a field, a layout, a constant table), and both builds have to print what a cold build of the same tree prints. The acceptance test of a per-function store key ([#1336](https://github.com/indexable-inc/blaze/issues/1336), [#1291](https://github.com/indexable-inc/blaze/issues/1291)); it asserts no reuse. From astral-experiments/blaze#66-#78. `uv run`, typed JSON. Linux and macOS |
| `deepnames.py` | Closures nested 20 deep keep the store under 64 MB, and a second daemon finds their entries again: a type spelled out longer than 1 KB is named by its hash (issue #29) |
| `expansions.py` | Uses the native proc-macro server. A second daemon on one store finds every derive and attribute expansion the first kept, and after an edit, with `BLAZE_PROC_MACRO_CACHE=check`, none expands otherwise than it was kept (issue #38) |
| `remote.py`, `s3fake.py` | A store shared by two machines through an S3 service, with the transfer broken, objects spoiled and manifests forged |
| `packs.py` | Not a check: the sealed pack's layout ([cache.md](cache.md)) read and written, for `remote.py`, `storeguards.py` and `bench.py` |
| `deps.py` | cargo's builds of registry crates pushed and pulled. Builds offline from cargo's registry cache, so it does not run on a clean CI machine |
| `netdown.py` | No build waits on the network ([cache.md](cache.md), "The standard library"): hello world built whole with the standard library's remote served fine, slowly, hung and at an address nothing answers. A warm store builds as fast as with the remote fine and prints no error; a cold one compiles the library from the start, as fast as with `BLAZE_STD_FROM=off`, and says the fetch failed in one line at most |

### Timing

`bench.py`, `editloop.py`, `edittimes.py`, `scaling.py`, `dxedits.py`, `linkbench.py`,
`shapes.py`, `heap.py`, `heapfold.py`, `leanprof.py`, `cpufold.py`, `memsplit.py`, `fnincr.py`, `realbench.py`, `spare.py`, `live_latency.py` and `testselectbench.py` measure and do not gate.
`testselectbench.py` runs `bench.py`'s projects' tests under a daemon with `BLAZE_TESTS=all` and
`affected`, before and after `bench.py`'s `leaf` edit: the wall time, the tests run and skipped,
and how many run every time and why, from what the daemon wrote beside each test program. It
exits 1 when a build after the daemon first served one went to cargo (#84): ripgrep's and
helix-core's edits are the daemon's to build.
`memsplit.py` says what each process of the daemon holds (the daemon, the project's worker, the
spare) and of what kind of page, once ready, once the daemon has built its own base and after
edits, with `--env` to try an allocator setting
([research/daemon-memory.md](research/daemon-memory.md#2026-10-23-the-own-base-and-the-allocator)).
`spare.py` starts a daemon with a tiny project, then builds one of `bench.py`'s projects under it
from an empty target directory and again once cargo has built it, each until the daemon is
ready, for builds of blaze given as `--variant LABEL:BIN[:VAR=VALUE...]` in interleaved
rounds: the second project's cold and warm start and what the daemon's processes hold before and
after, which is how the spare worker's default was chosen ([cargo.md](cargo.md#what-a-build-does)).
`heap.py` gives a heap profile's bytes to the salsa query whose value or record holds them, and to
blaze's own structures; `heapfold.py` makes of a profile on macOS what `jeprof --collapsed` makes
of it on Linux, for `heap.py` to read ([research/daemon-memory.md](research/daemon-memory.md)); `heap.py --syntax`
says what of it is syntax trees, of which kind and from which query, and `cpufold.py` folds a
`perf script` the same way for time ([flat-syntax.md](flat-syntax.md)). `leanprof.py` runs a
project's cold start, own base, edits and warm start as phases with the times they began and
ended, and `cpufold.py --phases` cuts a `perf` profile of the run by them, with what salsa was
revalidating and who asked ([research/lean-frontend.md](research/lean-frontend.md)). `fnincr.py` counts
the queries an edit inside one function of a big file runs (the file comes from `genbig.py`),
against the same functions in many small files ([reports/INCREMENTAL.md](reports/INCREMENTAL.md)). `realbench.py` times
`blaze build` against `cargo build` on ripgrep, just and nushell (cold, no-op and
three kinds of edit); the numbers are in [results.md](results.md#11-real-projects). `coldprof.py`
times a cold `blaze build` of a real project until the daemon is ready and its first
edit, from an empty or a filled store, with every line the daemon logged and the cores busy
along the way: the readiness check of [#11](https://github.com/indexable-inc/blaze/issues/11),
whose numbers are in [cargo.md](cargo.md#what-a-build-does). With `--max-wait S` it fails when
the daemon was ready more than S seconds after the command returned; `--until TEXT` waits for a
line the daemon logs, and `--memory` follows what its processes hold. `warmstart.py PROJECT`
times a start on a warm store with nothing of rustc's ([#41](https://github.com/indexable-inc/blaze/issues/41)):
a first checkout on an empty store, a second on the store it filled, that daemon stopped and
started, and once more, each followed by a body edit and its undoing; it keeps what each worker
said in `WORK/said-STEP.txt` and exits 1 when a program's output is not cargo's
([research/warm-start.md](research/warm-start.md)). Their docstrings say how to run them.
`bench.py` on real projects, with `benchreport.py`, is also what writes the benchmark
tables and the README's numbers: [benchmarks.md](benchmarks.md). Besides the edits `bench.py` times repeated cold builds
against fresh cargo with an empty and with a warm store (`--cold-runs`), the same steps under
the fastest cargo setups (`--setups`: mold, wild, `-Zthreads=8`, cg_clif and `cargo check`) and
the test suites under both (`--tests`). `bench.py --runtime` runs `runbench.py`, which times
programs blaze builds whole with LLVM against `cargo build --release`, and `benchreport.py`
writes that result to the runtime table in benchmarks.md, refusing it when any program's output differs
from cargo's. `abbench.py` runs two or more builds of blaze against each other on `bench.py`'s
edits, in interleaved rounds: an edit's wall time, the daemon's own time and (macOS) its
workers' cycles, the cold start, and the workers' peak memory. It is how the release build's
profile was chosen ([reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)), and with `--train` it
is the training of its PGO. `hotpatch.py` times an edit of a running program under Dioxus's
subsecond against `blaze build` and `cargo build` with a restart; its program is
`harness/hotpatch`. `benchhistory.py` appends a run to `docs/benchmarks/history/` (what the
site's /bench page charts) and, with `check`, fails when a median moved past its noise band.
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
`--render` every table and chart.
`live_latency.py` times live edits on macOS on AArch64, per keystroke under typing (`replay`,
`burst`) as well as one edit at a time (`socket`, or `cargo` through `blaze build`), and
the restarts of a session against `cargo run` (`restart`); its JSON goes to `benchhistory.py
live`, and the numbers measured on a MacBook are in [live.md](live.md#latency-under-typing).
`nix develop .#bench` has everything `bench.py` and `hotpatch.py` need (the nightly, mold, wild,
`dx`, bevy's libraries; zed's are in `.#bench-zed`, since every library directory slows each
link cargo makes), and `live_latency.py` runs under it as under rustup's copy of the pinned nightly. Under another
toolchain the daemon builds nothing, and the harness stops at once with what `blaze`
says of it.

## The fuzzer

`fuzz/fuzz.py` builds generated programs with cargo and with blaze under each backend and
compares what they print. `fuzz/fuzz.py smoke` is part of the gate. How to run longer
runs, what a finding is and what the fuzzer does not cover are in
[fuzzing.md](fuzzing.md).

## Adding a corpus case

The corpus is for things the code generator must handle that are small enough to state as
a function. Add a `pub fn` to the module for its area in
`harness/corpus/cases/src/lib.rs`, or a new `pub mod`. Call it from
`harness/corpus/runner/src/main.rs` inside `case!(...)`, which prints the expression and its
value. The result has to be deterministic and `Debug`. Then run

    python3 difftest.py corpus --bin runner --crate cases --layouts

It must print `MATCH`, and the count of functions from blaze must go up by what you added,
with none left to rustc.

## Adding a breaker

A breaker is the reduction of a bug found somewhere else. Add a file
`harness/breakers/cases/src/<name>.rs` with a `//!` comment at the top that says where the
bug was found and what the cause is, and a `pub fn run()` returning something `Debug`. Add
`<name>` to the `cases![...]` list in `harness/breakers/cases/src/lib.rs`. Then

    python3 breakers.py <name>

A breaker should fail before the fix and pass after. If you are recording a difference you
are not fixing, add the name to `OPEN` in `breakers.py`, or to `LEFT` if blaze refuses the
function and says so. Both lists are the project's list of known wrong or missing
behaviour, so keep the comment in the case honest about why it is open.

A bug that only shows in a crate's own tests goes in `CRATES` in `breakers.py` as a small
crate under `harness/breakers/`.

## Reproducing a failure

Start from the script's last lines. Each daemon harness prints the path of the daemon's log
and of the scratch copy of the source it was editing, for example

    99 of 99 right, 0 restarts; log /tmp/blaze-stress-out-XXXX/daemon.log; source /tmp/blaze-stress-XXXX

and the stepwise ones take `--only TEXT`, `--from N` and `--keep` (see each docstring).

For a difference in generated code:

1. Run `difftest.py` on the workspace with `--bisect`. It names the functions whose blaze
   version changes the output, with the first differing line.
2. `blaze --report FILE.tsv` writes the outcome of every function (compiled, skipped and
   why). `--deny FILE` leaves the functions listed in a file to rustc, and `--isolate DIR`
   writes one object per function.
3. `BLAZE_DUMP_MIR=<function name>` prints the MIR of functions with exactly that name
   (`{closure}` for closures). `BLAZE_DUMP_LLVM=1` prints the LLVM module of every function
   compiled, and `BLAZE_DUMP_CLIF=1` the Cranelift IR of drop glue.
4. Reduce it to a function, add it as a breaker, and fix it there.

For a fuzzer finding, `fuzz.py repro SEED-ROUND` rebuilds the program and
`fuzz.py check found/FILE.rs` runs a stored reduction.

For a CI failure, the last line of the harness step lists which scripts failed.

## CI

CI is a **merge gate** that takes minutes, and a **release train** that runs everything else on
main after the merge and releases main when it is green. Both call `blaze-platform.yaml` once
for each platform, each on a machine of its own kind.

| Workflow | Event | What runs | Wall time |
|---|---|---|---|
| `blaze.yaml` (`gate`) | pull request | on Linux x86-64: blaze builds blaze (`blaze build`) and tests it (`harness/unittests.py`, `blaze test`, only the tests the change reached), from the newest release and the store main pushed to blaze's remote; `harness/imports.py`, `harness/rustcsites.py` | 49 min on a store the release has not compiled into |
| `blaze.yaml` (`warm`) | push to main | the same on Linux x86-64, AArch64 and macOS, every test, then a push of the store for the runs after it. Its `linux x64 / build` passing is what cuts a canary (`canary.yaml`); the whole run green, unit tests on every platform, is what stable needs besides the promotion gate ([release.md](release.md)). And on Linux x86-64 the std cases of the smoke corpus (`harness/smoke.py`, [release.md](release.md#the-smoke-corpus): `blaze run` and `blaze test`, dev and `--release`) with the blaze blaze built, 17 to 30 min until [#899](https://github.com/indexable-inc/blaze/issues/899) (74 s with rustc's) | |
| `fmt.yaml` | push to main | `cargo fmt --all`, and a commit `fmt` when it changed a file | 2 min |
| `train.yaml` | push to main, by hand | everything below, then a release or the train's issue | about 45 min |
| `blaze.yaml` | a change the build and the unit tests do not read | the two scripts | 1 min |

The build and the unit tests run only for a change that touches a file they read; any other
change runs the two scripts alone. `.github/scripts/gate_changes.py` (the `changes` job) names
those files, and why each: the crates (`crates/`, with their test data, the LLVM pin
`crates/hir-ty-codegen/llvm.json` and the toolchain's `pins.txt`), the vendored crates
(`vendor/`), `Cargo.toml`, `Cargo.lock`, `.cargo/`, `.rustfmt.toml` and `.clippy.toml`, the
pinned nightly (`harness/toolchain`), `harness/unittests.py`, `.github/scripts/llvm.py`
(which fetches the pinned LLVM), the release of blaze that builds blaze (`.github/blaze-version`,
`install.sh`), `blaze.yaml`, `blaze-platform.yaml` and the actions they use (`toolchain`,
`blaze`, `blaze-remote`, `rust-tests`); on a push to main, the smoke corpus too (`harness/smoke.py`,
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
([contributing.md](contributing.md#llvm-for---features-llvm)): `.github/scripts/llvm.py fetch`
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
is the build and the unit tests: a change that does not build, or breaks a unit test, does not
merge. What the harnesses find, they find on main, in the train, which says
which commits since the last green train it came in with. Before the gate was this small, a
merge-queue run took 35 to 45 minutes, nearly all of it harnesses (harness/ci.py's `quick-2`
on every platform), and 51 of 140 queue runs on 2026-10-25 and 26 failed, most of them on
entries that later landed unchanged.

No check refuses a pull request for its formatting: `fmt.yaml` formats main after each push and
commits the result as `fmt`. Run `cargo fmt --all` before pushing all the same, so that the
diff of a pull request is its change.

`harness/imports.py` has every script of the harness imported, so one that only the train or a
person runs can't break unnoticed: a script does nothing when imported (its work is under `if
__name__ == "__main__":`). `harness/rustcsites.py` fails when blaze's code gains a place that
starts rustc, rustdoc or cargo ([no-rustc.md](no-rustc.md)).

The repository's Python is one uv workspace (`pyproject.toml`, `uv.lock`, Python 3.13 from
`.python-version`; shared helpers such as `python/typed-input` are its members). Its scripts run
with `uv run --frozen`, and the gate runs ruff and pyright over the ones the workspace lists
(today `.github/scripts/downstream.py`, `harness/focused.py` and `python/`); an older script
moves in when it is next changed. `harness/imports.py` runs in the workspace's Python too.

### Named harnesses, and the projects downstream

Two workflows run part of what the train does, by hand, for a change that wants an answer
before or without the whole train. Neither releases anything or is part of `ci`.

`focused.yaml` runs the harnesses it is given by name (`harness/focused.py`: a label of
`ci.py list '{}'`, or its start up to a space) against one build of the ref, on the platforms asked
for, under both backends, as the train builds and runs them:

    gh workflow run focused.yaml --ref <branch> -f harnesses='["anonstatics.py", "testselect.py"]'
    uv run --frozen python harness/focused.py run '{"harnesses": ["anonstatics.py"], "backend": "llvm"}'

The second line is the same in a lane's VM. A name that is no harness's, or that runs on none of
a platform's backends, fails rather than run nothing. This is the part of Astral's per-change
selection (astral-experiments/blaze#7, #58) that applies here: their fork had a model pick the
suites and harnesses of each pull request, with path rules when it gave no answer. Here the gate
is already the build and the unit tests alone, and `blaze test` picks which unit tests to run
from what the change reached (`BLAZE_TESTS=affected`, [cargo.md](cargo.md)), which a model's
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

Each has to print blaze's `Finished` line, so a run that built nothing does not pass. It tests a
release (`-f blaze=canary`, the default, `stable` or a version), or with `-f build-run=<id>` the
`blaze-linux-x64` build of a `focused.yaml` or train run: the way to try a branch on them before
it lands. Build scripts and proc macros run in blaze's sandbox, as for any user.

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
with the `llvm` feature, hands the binary to its other jobs, and runs:

- `harness/unittests.py` on each platform;
- every part of `ci.py parts` under each backend, a job each: `core-1` ... `core-N` (the
  harnesses of `core`, packed by the seconds `CORE` in ci.py says each takes, so that no part
  is longer than its longest harness, `wholeprogram.py` at 19 minutes on macOS), `keys-1..3`,
  `buildmodel`, `store-kills`, `store` and `daemon`;
- rustc's borrow-check tests with a floor under the passes (Linux x86-64), and the run-pass
  tests of `harness/ui-pinned.txt` (Linux x86-64 and macOS), from rustc's `tests/ui` at the
  pinned commit in the Actions cache (`.github/actions/rust-tests`, written by main's `warm`
  runs; a miss fetches it from GitHub with retries, since the plain fetch was refused with HTTP
  429 in 26 jobs in two days);
- the Bun fixtures (Linux x86-64);
- `harness/wasm32.py` (Linux x86-64, in the flake's `wasm` shell): the corpus and `stress.py`
  built for `wasm32-wasip1` and run under wasmtime against cargo's build;
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

- `OPEN` in `unittests.py`: Rust unit tests that fail;
- `OPEN` and `LEFT` in `breakers.py`: breakers that differ from rustc or that blaze refuses;
- `OPEN` in `debuginfo.py`: checks whose frames or variables are wrong;
- `"open"` in a `buildmodel/*/case.json`: a case that comes out WRONG, on every host or by
  host (`{"linux": "why"}`).

### The other workflows

`bench-readme.yaml` measures real projects on each platform's dedicated benchmark machine and
commits the tables and the README's numbers to main. It runs a platform only once the
repository variable `BENCH_RUNNERS` names it ([benchmarks.md](benchmarks.md#the-machines)).
`bench.yaml` (the store's microbenchmarks, two minutes) runs on pull requests and pushes too.
`claims.yaml` (all of rustc's `tests/ui`, both backends, six hours, and the scoreboard),
`fuzz.yaml` (a long differential fuzz run), `bench-bun.yaml` and `live-latency.yaml` keep
schedules of their own: each takes hours or a dedicated machine, and the train would wait on
them. On a pull request that changes the flake, `nix/` or the dependencies, `nix-live.yaml`
checks only the unit graphs. The flake's `dist` passes `--nix` to `build_release.py`, which
then checks that every binary loads through a `/nix/store` loader instead of the glibc 2.34
floor of a shipped release. `live-*.yaml` run on their own branches. None of them is part of `ci`. `site.yaml` deploys the
site, and `release.yaml` builds releases from tags, each target through
`.github/scripts/build_release.py` (the `dist` profile with PGO,
[reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)), the recipe the flake's `dist` runs too.
`pgo.yaml` trains the profiles it uses, for each release the train makes, by hand, and on
pushes to main that change Cargo.lock, a manifest or the toolchain, and pins them in
`pgo/profiles.json`.
