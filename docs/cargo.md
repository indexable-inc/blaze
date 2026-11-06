---
status: current
summary: Every blaze command and setting: cargo's commands and flags, what a build does, sandboxing, manifests.
---
# Using blaze

`blaze <command>` takes cargo's commands and flags (`blaze build`, `blaze test`, `blaze run`),
described here. Plain `cargo` never goes through blaze (the section at the end).

blaze had another name and another command before 2026-10-28; [rename.md](rename.md) maps them.

This file describes blaze v2, checked against its code on 2026-10-30: the program `blaze`
(`crates/blaze-engine/src/program.rs`), in front of the client (`crates/blaze-cargo/src/cli.rs`
and `cli/`) of the daemon `blaze serve` (`crates/blaze-cargo/src/serve/`), on the crates of
[v2/plan/reference/crates.md](v2/plan/reference/crates.md). v1's code was deleted that day;
where v1 had a setting or command v2 lacks, the section says so. v2's design is
[v2/plan.md](v2/plan.md).

## Install

`blaze` is one program, also installed as `blaze-engine`. The [installer](../site/install.sh) and
`cargo install` both install it:

    curl -fsSL https://ix.dev/blaze | sh
    cargo +nightly-2026-10-27 install --locked blaze-engine

From the release after 0.1.0, `--profile dist` builds the profile a release is built with, less
the PGO a release adds ([reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)); 0.1.0 has no such
profile and fails with it. From a checkout, the release's own recipe builds all of it with the
blaze on PATH (the stage 0 `.github/blaze-version` names), for this machine's target:

    uv run --frozen --no-dev python .github/scripts/build_release.py '{"target": "aarch64-apple-darwin", "out": "shipped"}'
    cp shipped/* ~/.cargo/bin/

That build has the LLVM backend (the release profile's, [below](#backends)) and the LLVM tiers, as the
installer's binaries do, linking the LLVM 23 blaze pins in statically ([which
LLVM](contributing.md#llvm)). blaze itself fetches that LLVM, by the sha256
`crates/hir-ty-codegen/llvm.json` pins, and hands it to llvm-sys's build script: the root
manifest declares it a [pinned native input](#pinned-native-inputs), so a plain
`blaze build -p blaze-engine --release` on a clean Mac or Linux machine needs no Nix and no
variable. A stage 0 from before that change does not read the table, and still needs
`LLVM_SYS_231_PREFIX` set to that LLVM, as `cargo install` from crates.io does. `blaze build -p
blaze-engine --release` is the quicker build the harnesses and CI use while working on blaze;
it is not what ships.

`blaze` builds with the one toolchain blaze compiles, the dated nightly in
`harness/toolchain` (`nightly-2026-10-27`): the language and standard library are the ones this
blaze ships. It ignores `rust-toolchain`/`rust-toolchain.toml`, `rustup override`,
`+toolchain`, `RUSTUP_TOOLCHAIN`, rustup's default and `RUSTC`, and never runs or asks a rustc
to learn anything ([toolchain.md](toolchain.md)). The nightly builds
what older releases built, so a project on a stable release or an older nightly builds as it
is, `#![feature(...)]` included. A project that needs another Rust pins another blaze. Plain
`cargo` still builds with the project's toolchain, in its own `target/` in the checkout:
blaze's build directory is elsewhere ([Where a build writes](#where-a-build-writes)).

Where the nightly's files come from, found with nothing run: blaze's own copy in
`~/.blaze/toolchains/nightly-2026-10-27-<host>` (`BLAZE_HOME` moves `~/.blaze`), laid out as rustup lays
out a toolchain, never rustup's own (its directory is not read, not even when it has the same
nightly). blaze downloads its copy the first time it needs it, once, with a line on
stderr while it does: rustc, cargo and the built standard library (the host's, and
wasm32-wasip1's for the wasi-libc proc macros link with) from the Rust project's
own servers (the standard library's source is blaze's own `library/`, a 6.0 MB archive from
blaze's release, [toolchain.md](toolchain.md#the-standard-library-library)), 122 MB on macOS, 159 MB on Linux x86-64 and 143 MB on Linux ARM64 (about 654 MB on macOS
and 816 MB on Linux x86-64 once unpacked, wasm32-wasip1's 106 MB of it). Each file's sha256 is built
into blaze, from the nightly's published manifest (`harness/toolchain_pins.py` writes them into
`crates/blaze-cargo/src/toolchain/pins.txt`), and a file with other bytes is refused. Concurrent
builds download it once; a download that stopped goes on from where it stopped the next time.
A build script is given blaze itself as `CARGO`, and as `RUSTC` and `RUSTDOC` links to blaze
in the daemon's tools directory, which answer what a script asks of rustc with no rustc run
(`crates/blaze-engine/src/as_rustc.rs`).
`BLAZE_RUST_DIST` names a mirror of `https://static.rust-lang.org/dist` (or a directory with
the files), for a machine that cannot reach it. v1's `blaze toolchain`, which said which Rust
blaze builds with and fetched its copy, is gone; v2 has no such command (`blaze --version`
names blaze's release and commit, and why it is the one that runs), and fetches the copy when
a build first needs it.

## Commands

    blaze build [cargo's flags]                                       (b)
    blaze check [cargo's flags] [--fix [--lint NAME]...]               (c)
    blaze check [cargo's flags] --replace FROM=TO [--replace FROM=TO ...]
    blaze check --workspace --target TRIPLE --baselines DIR [--bless-baselines] [--new-baseline LINT[,LINT...]]
    blaze run   [cargo's flags] [-- program arguments]                (r)
    blaze test  [cargo's flags] [TESTNAME] [-- test arguments]        (t)
    blaze bench [cargo's flags] [BENCHNAME] [-- bench arguments]
    blaze doc   [cargo's flags]                                       (d)
    blaze fmt   [--check] [-p NAME]... [--all] [--manifest-path PATH] [-v|-q] [-- RUSTFMT_ARGS]
    blaze fmt   [RUSTFMT_ARGS] FILE.rs...
    blaze serve [--socket PATH] [--http ADDR] [--activated] [--ready-fd N] [--stall-bound SECS]
    blaze stop
    blaze status [--json]
    blaze toolchain [--json]
    blaze daemon install | uninstall
    blaze codegen-inputs --format=json
    blaze trace --out PATH
    blaze tables --format=json
    blaze pack -p PACKAGE --out PATH [cargo's flags]
    blaze pin
    blaze --version

`blaze help <command>` lists the flags a command takes: cargo's, read as cargo's clap reads them
(`crates/blaze-cargo/src/cli/args.rs`), with clap's errors and exit code 1 for a command line
cargo would refuse. `-C DIRECTORY` (with `-Z unstable-options`) and the global flags before
the command work as in cargo. `test` alone takes four flags of blaze's own (extensions,
[below](#which-tests-run)): `--reached`, `--ub`, `--ub-seed N` and `--ub-schedules K`. `build`,
`check`, `run`, `test` and `bench` take `--refused N|all`: how many of the functions blaze could
not compile a build's error names, the first `N` (16 without it) or every one.
`serve` is the daemon itself ([below](#the-daemon)); `codegen-inputs`, `trace` and `tables`
print what the daemon's last build of the checkout did (each instance it made with its code
hash, every result with its edges as CBOR, the size of each long-lived table); `pack` checks
the package for `wasm32-unknown-unknown` with debug assertions off (as `--release`), its build
scripts run and proc macros loaded in the sandbox as any check's, and writes the program's crate
and every crate it is made of, with each macro expansion its names and bodies needed, to `PATH`
as one JSON file for the in-browser compiler (`crates/blaze-wasm-pack`, the agent API's
`Request::Pack`); `check --replace std::sync::Mutex=parking_lot::Mutex` retargets every
reference that resolves to the item `FROM` names to the item `TO` names, each a def path from a
crate's name: the daemon's front end finds them by name resolution (imports, renames, globs,
re-exports, the prelude, a macro's input), never by text, rewrites a `use` of it or a path that
qualifies it, leaves a path through the checkout's own import of it (which moves instead),
and commits the edits to the checkout's head, and the client writes them to its files
(`crates/blaze-intent/src/replace.rs`, the agent API's `Edit::Replace`). A crate that would have
to name `TO` without depending on its crate, or a name the front end can't place (a
`macro_rules!` body, cfg'd out code), stops it with an error naming the file and line, before
anything is written, and nothing is checked; `pin` is [below](#pinning-a-release-of-blaze). `check`'s lints, `--fix`
and `--baselines` are [below](#lints). There is no `clippy` and no `fix` command: `check` is both. Any other command (v1's `tree`,
`metadata`, ...) fails with `error: no such command` and status 101; none is
passed to cargo, and no cargo plugin is run ([Every subcommand](#every-subcommand)).

`fmt` is `cargo fmt` and rustfmt, run in the client's process with blaze's own formatter
(`crates/blaze-fmt`, `blaze_fmt::command`); it asks nothing of the daemon and starts no rustfmt
or cargo-fmt. With no `.rs` file named before `--` it formats the targets of the current
package, or of every member at the workspace's root (`-p NAME` picks packages, `--all` adds
their local path dependencies), each with its target's edition; `--check` writes the diff and
exits 1 when a file would change; words after `--` go to the formatter (rustfmt's options, and
more files). Given `.rs` files it formats those, each with the edition of the target or package
that has it unless `--edition` says. Each file reads the nearest `rustfmt.toml` or
`.rustfmt.toml`; one with an option blaze fmt does not run (a name rustfmt 1.11.0 does not
know, or a non-empty `ignore`) stops it with an error naming the file and each option.

`blaze check` checks the targets `cargo check` with the same flags does: a plain one
the libraries and binaries of the packages selected, and `--lib`, `--bins`, `--tests`,
`--examples`, `--benches`, `--all-targets` and `--bin`, `--test`, `--example` and `--bench`
with a name those they pick, as cargo's unit graph of the check has them. A target picked as
a test (a library's unit tests under `--tests`) is checked with `cfg(test)` and the
dev-dependencies; a member the selected packages only depend on is checked as its library.

`build`, `run`, `test` and `bench` take the flags cargo takes and leave the files cargo would
in the same layout, under the workspace's build directory, which is never in the checkout
([Where a build writes](#where-a-build-writes)). The client reads no manifest: it finds the checkout, sends the
daemon one request with the whole command line, its environment and the configuration files
outside the checkout, and prints the responses as cargo prints its build (`Compiling`,
`Fresh` under `-v`, warnings, `Finished`), with cargo's exit codes. `check` is the daemon's
check of every body of each crate the command picks, as rustc checks them
(`crates/blaze-cargo/src/serve/unit/checks.rs`): the errors a build would fail with, or
`Finished`, and no rustc. `--message-format` takes each of cargo's values; in JSON the
messages are cargo's (`compiler-artifact`, `build-script-executed`, `compiler-message` with
rustc's diagnostic, `build-finished`), written as cargo and rustc write them
(`crates/blaze-cargo/src/cli/json.rs`), which is what rust-analyzer's check-on-save and other
editors read. The wording of a diagnostic is blaze's (`crates/blaze-diagnostics`), not always
rustc's. `status` prints the daemon's process, the requests it has served, the checkouts it
watches and the memory it holds of its budget (`--json`: one object, [The daemon](#the-daemon)).
`stop` has the daemon write its state and end.

Under a JSON `--message-format` every `build`, `check`, `run`, `test` and `bench` ends its build
with one `build-finished` line on stdout, the last of its JSON messages and before anything the
program or the tests print. cargo's has only `success`; blaze's carries what a tool would
otherwise read from the human `Finished` line and the daemon's log, each field always present,
`null` where there is nothing to say:

    {"reason":"build-finished","success":true,"profile":"dev","served":"daemon","took_ms":812,"build_id":7,"units":{"built":2,"replayed":40},"links":{"first":0,"spliced":1,"unchanged":0,"full":0}}

- `success`: as cargo's.
- `profile`: the profile's name, `null` when the build placed nothing (it failed first).
- `served`: `daemon`, or `in-process` for the seed's one-shot engine (docs/future/bootstrap.md).
- `took_ms`: from the command's start to the build's end.
- `build_id`: the daemon's id of the build, which the profile history's rows carry
  (`blaze slow --build N`, `blaze debug sql`); `null` with no work report.
- `units`: the units made again (`built`) and replayed from the store as they were
  (`replayed`); `null` for a request that sent no work report (a load that failed).
- `links`: each image linked, by how: its first link, spliced into the last one, unchanged, or
  laid out whole again (`full`, which the `link:` line on stderr says why of).

`--message-format=json-render-diagnostics` keeps the diagnostics rendered on stderr as the
human format has them and puts only the JSON messages on stdout: what harness/storelib.py's
`split_build_output` reads (`crates/blaze-cargo/src/cli/json.rs`, `build_finished`).

`doc` takes `cargo doc`'s flags (`--open`, `--no-deps`, `--document-private-items`, `--lib`,
`--bins`, `--bin`, `--examples`, `--example`, and the package, feature, profile and target
ones) and documents what `cargo doc` documents, with no rustdoc: the daemon checks the targets
`cargo doc` picks (a library, and each binary not named as it; `Intent::Doc` in
`crates/blaze-units`) and every dependency, as any check, then makes every page from that front
end with `cfg(doc)` set (`crates/blaze-doc`, `crates/blaze-cargo/src/serve/unit/doc.rs`, the
agent API's `Request::Doc`). The client writes the pages to `doc` in the build directory, or
`<triple>/doc` there with `--target`, prints that path, keeps the crates earlier runs documented there in the
search's list, and says `Documenting`, `Finished` and `Generated` (or `Opening`, and opens the
page with `$BROWSER` or the system's opener, for `--open`) as cargo does. What `cargo doc`
reads and blaze can't honor yet stops it with an error naming it: rustdoc's flags
(`RUSTDOCFLAGS`, `build.rustdocflags`, `target.<triple>.rustdocflags`), `doc.browser` under
`--open`, a JSON `--message-format`, `--unit-graph` and `--timings`; doctests are
`blaze test --doc`'s. What the pages hold is in [doc.md](doc.md), a record of v1's
`blaze doc`, which wrote them from the same crate.

While a build waits, the client prints each unit's lines as the daemon sends them; there is no
live progress view in v2.

## What a build does

blaze never runs rustc, and a build has one path ([no-rustc.md](no-rustc.md)). The daemon
reads the command line as cargo does, into cargo's unit graph (`crates/blaze-units`: the
packages, features, profiles, targets and build-script runs cargo would have), and builds
each unit itself: every crate is compiled by blaze over blaze's standard library, a
function at a time ([v2/plan/design/model.md](v2/plan/design/model.md)), and each program is
linked by blaze's linker (`crates/blaze-link`) where cargo puts it in the build directory
(`debug/<name>`, `release/<name>`, `examples/` for an example). No `cargo build` runs. A function the
program can reach that blaze cannot compile fails its unit, naming the function and why;
nothing is handed to cargo or rustc.

What a unit makes (`crates/blaze-cargo/src/serve/unit/codegen.rs`, `makes`): a binary, an
example, a test or a build script is an executable; a `cdylib` (a native Node addon, a Python
extension, a plugin) or a proc macro is a shared library, at cargo's path
(`target/debug/lib<name>.so`, `.dylib` on macOS), linked with the package's
`cargo:rustc-link-arg` and `cargo:rustc-cdylib-link-arg`; a library (`lib`, `rlib`) is written
with no object code, its functions kept in the store for the programs that link it. A crate
type of `staticlib` or `dylib` fails the build, naming the target: blaze's linker writes
neither yet ([#262](https://github.com/indexable-inc/blaze/issues/262)). So does what else
blaze's load does not build, saying what: an artifact dependency (`-Z bindeps`), a
`metabuild`, a target spec file, a target blaze has no data of, and a rustc `-Z` flag in
`RUSTFLAGS` that `crates/blaze-cargo/src/unstable.rs` refuses.

blaze keeps going by default, so one run shows every error (`blaze build`, `check`, `test`,
`bench`, `run` and `doc` alike; cargo stops at the first failure). `--fail-fast` stops at the
first failure, as cargo does: the first unit that fails ends the build, and the units after it
are named as skipped. `--keep-going` and `test`'s `--no-fail-fast` are accepted and change
nothing. The exit status is non-zero whenever anything failed.

Unless `--fail-fast`, a failed unit stops only what needs it. A unit that is only checked
(`blaze check`, or a library a check reaches) and failed only in its functions' bodies or in
lints, its items (signatures, types, impls) sound, fails open: it stays failed, nothing is
built or linked from it, and the units that depend on it are still checked against its items,
so one pass shows their errors too. A unit whose items are broken, or one that is built rather
than checked, still holds back what depends on it. Which it was is part of the unit's check
result and its record (`crates/blaze-cargo/src/serve/unit/compile.rs`, `open`); an item
declared inside a body's block counts as the body's. At the end of a build that failed, blaze
says on stderr how many units it left unchecked and behind which failure, and which units an
open failure let through; under `--message-format json` the same comes as one
`{"reason":"blaze-unchecked",...}` line on stdout (`Response::Unchecked` in `blaze-api`).

The build is the daemon's, and the daemon keeps what it built: its database and its store.
The next build compiles only the functions an edit changed, and links each program again
against an image it kept of the last link at that path, so the link writes only what changed
(`crates/blaze-cargo/src/serve/unit/codegen.rs`, `Linked`). Build scripts and proc macros
are compiled by blaze too, as units of the same graph ([below](#build-scripts-and-proc-macros-run-in-a-sandbox)).

The standard library is built to unwind, whatever the profile's `panic`, as the toolchain's
sysroot is; only the proc macros' and a target's that only aborts are built to abort
(`std_key`, `crates/blaze-units/src/load/units.rs`). So a `panic = "abort"` program links it
with `panic_abort`, and its tests, benches and build scripts, which unwind under any profile as
cargo builds them, link the same std. Under an explicit `-Zbuild-std` that builds `std`, with
`panic = "abort"`, cargo builds std to abort; blaze builds it to unwind there too, so the
build's tests link the same std. That costs the program std's landing pads and unwind tables
in size and changes nothing it does. A `-Zbuild-std` with neither `std` nor `panic_unwind` (`core`,
`core,alloc`) is built with the profile's `panic`, as cargo builds it: nothing in it defines
`rust_eh_personality`, which a `core` built to unwind would need.

Linking checks most of what rustc's `dependency_format` checks (`linked_crates`,
`crates/hir-ty-codegen/mod.rs`): one panic runtime, built with the program's strategy, and no
crate built to abort in a program that unwinds; each refusal names the crates. rustc also
refuses, in a program that aborts, a crate built to unwind that makes a call that may unwind out
of foreign code (`has_ffi_unwind_calls`). blaze does not read bodies for that, so it links a
crate built to unwind into a program that aborts only from the standard library, which rustc
links that way from its sysroot, and refuses any other by name. Cargo's unit graphs never ask
for one.

`test` and `bench` build what cargo would, as its unit graph has them: each member's library,
binaries and integration tests with `test = true`, the examples built beside them, and the
binaries an integration test runs (`CARGO_BIN_EXE_<name>`), or what the target flags pick. A
test crate's harness is written into its root before its unit is compiled
(`crates/blaze-cargo/src/serve/unit/harness.rs`). An integration test or a benchmark is told
`CARGO_BIN_EXE_<name>` of its package's binaries and `CARGO_TARGET_TMPDIR`, as cargo tells
them.

`run` then becomes the program and `test` runs the test executables the client was told of, as
cargo's `ops::run_tests` does (`crates/blaze-cargo/src/cli/test.rs`): in cargo's order, on the
terminal, with the variables and working directory cargo gives, cargo's `Running` lines, then
the doc tests; `--no-run` lists them. By default blaze builds every unit it can, runs every
test executable and doc test that built whichever failed, and ends listing the targets that
failed (with what reruns each), then each test executable or doc test that never built and the
failure that held it back: its own compile's, or the unit it was skipped behind
(`blaze_api::Unchecked::tests`). The exit status is then 101. `--fail-fast` stops at the first
test executable that fails, with its exit code, as cargo does without `--no-fail-fast`.
That runs every test, outside any sandbox, as cargo does. blaze's `--reached` runs only the
tests an edit can have changed, in the daemon, and `--ub` runs them in blaze's evaluator
([below](#which-tests-run)). Doc tests are blaze's own build of each example (below).

### Lock files

A build resolves the workspace against `Cargo.lock` and may change two files at the root:
`Cargo.lock`, cargo's own (byte for byte what cargo writes, so cargo and its tools read it), and
`blaze.lock`, which pins what blaze builds with besides the crates (below). Which a build may do
(`crates/blaze-units/src/lock.rs`, `policy`):

| Asked | A build that would change a lock file |
|---|---|
| `--locked` or `--frozen` with `--no-locked` | error: they disagree |
| `--locked` or `--frozen` | refused |
| `--no-locked`, or `BLAZE_LOCK=write` in the environment | writes it, saying what moved |
| `CI` set (not empty, `false` or `0`), or `TF_BUILD` | refused |
| `[workspace.metadata.blaze] lock = "required"` | refused |
| none of these | writes it, saying what moved |

`blaze lock` resolves the workspace (taking `check`'s package and feature flags), writes both
files and says what moved, whatever CI or `lock = "required"` says, and stops: nothing is
checked or built and no build script runs. blaze's own tree sets `lock = "required"`.
blaze-metadata's commands (`metadata`, `remove`, `vendor`, `tree`) decide their lock files in the
same place (`blaze_units::lock::after_resolve`), so the same table holds for them.

`BLAZE_LOCK=write` is `--no-locked` said once for everything a harness starts: blaze's harnesses,
e2e tests and release checks make throwaway workspaces with no committed lock, and set it where
they start their children (`harness/ci.py`, `harness/smoke.py`, `harness/blaze_cli.py`, the repro
tests) instead of passing `--no-locked` to each command. A typed `--locked` or `--frozen` still
wins over it; any value but `write` is an error.

A refusal names every move and why, then the fix:

    error: Cargo.lock would change, and this build may not change it: `CI` is set, which makes --locked the default
      Cargo.lock:
        ~ serde 1.0.200 → 1.0.219 (`app v0.1.0` asks `1.0.210`, which 1.0.200 does not match)
        + itoa 1.0.15 (`serde_json v1.0.140` asks `1`)
        - ryu 1.0.18 (nothing depends on it now)
    fix: run `blaze lock` where the workspace is and commit Cargo.lock; or pass --no-locked to let this build write it

A write is never silent. A new lock file says one line; a changed one says what moved, under it:

    note: wrote Cargo.lock and blaze.lock: 143 packages, 1 native input, index as of 2026-11-04T10:12Z (commit them)
    note: updated Cargo.lock: 1 package moved (commit it)
    note:   Cargo.lock: ~ serde 1.0.200 → 1.0.219 (`app v0.1.0` asks `1.0.210`, which 1.0.200 does not match)

The resolve keeps every package the lock pins that the manifests still allow, as cargo's does,
so a manifest edit moves only what it has to. Under a refusal, a lock file that reads back to
the same resolve (another encoding or spacing) is the same, as cargo's `--locked` has it.

`blaze.lock` sits beside `Cargo.lock` and holds, in TOML:

- `[blaze] release`, `toolchain`: only for a workspace that pins a blaze version
  (`[workspace.metadata.blaze] version = "0.YYYYMMDD.N"`): that release and the nightly it ships
  (`harness/toolchain`), recorded when that release runs it. An unpinned workspace, or one pinned
  to a channel, has no `[blaze]`: it runs whichever blaze it gets, and a lock naming one would
  change with every upgrade. Run by another blaze (a build of blaze's tree, or `--ignore-pin`),
  the entry is kept as it is; one that names another release than the pin disagrees with it and
  is an error naming both, since only the pinned release can say what it ships: `blaze lock`
  with the pinned release records it.
- `[index] newest`: the newest publish time (the sparse index's `pubtime`) of any package the
  resolve took from a registry's index, shown as "index as of": the index as the lock saw it,
  told without a clock. A build that took nothing new keeps it.
- `[native.<name>] pin` and `[native.<name>.targets]`: each pinned native input's pin file and
  every target's sha256 ([Pinned native inputs](#pinned-native-inputs)): every input the root
  manifest declares and every target its pin names, whatever this build needs.

A pinned input that disagrees with `blaze.lock` (a pin file edited, another blaze release) is a
lock change like a moved package: refused where changes are refused, written and said elsewhere.
A `blaze.lock` that would pin nothing is not written. Unknown keys and other format versions are
errors naming the file. Why a second file and not `Cargo.lock`'s `[metadata]`: that table is a
flat map of strings cargo copies through `cargo update` untouched, so a pin there would go stale
without a word, and anything blaze adds to `Cargo.lock` stops it being what cargo writes. One
file holds every pin that is not a crate (docs/v2/plan/lock-policy.md).

### Link arguments

blaze links every program with its own linker, so a link argument reaches no `cc`, ld64, GNU
ld or lld. `-C link-arg=ARG` and `-C link-args=ARGS` in the rustflags (`RUSTFLAGS`,
`CARGO_ENCODED_RUSTFLAGS`, `build.rustflags` and `target.<triple>.rustflags` in
`.cargo/config.toml`, a profile's `rustflags`) and a build script's `cargo:rustc-link-arg*`
(and `rustc-cdylib-link-arg`) are read by `crates/blaze-link/src/args.rs`: the C driver's layer
comes off (`-Wl,a,b` is the linker's `a` and `b`, `-Xlinker a` its `a`), and each word is read in
the spelling of the linker the target's rustc would run, ld64's on macOS and GNU ld's (and
lld's) on Linux. What blaze's linker implements:

| Option | ld64 (macOS) | GNU ld, lld (Linux) |
|---|---|---|
| a library | `-lNAME`, `-framework NAME`, `-needed_framework NAME`, `-needed-lNAME`, `-pthread` | `-lNAME`, `-l NAME`, `-l:FILE`, `--library=NAME`, `-pthread` |
| where to look | `-L DIR`, `-F DIR` | `-L DIR`, `--library-path=DIR` |
| rpath | `-rpath DIR` (`LC_RPATH`) | `-rpath DIR`, `-rpath=DIR`, `--rpath`, `-R DIR` for a directory (`DT_RUNPATH`) |
| header room | `-headerpad SIZE` (hex), `-headerpad_max_install_names` | |
| undefined symbols | `-undefined dynamic_lookup` (flat lookup, executables too), `-undefined error` | `-z defs` |
| a shared library's name | `-install_name NAME` | `-soname NAME`, `-h NAME`, `-soname=NAME` |
| a shared library's exports | `-exported_symbol _NAME` (added to the crate's) | |
| the main thread's stack | `-stack_size SIZE` (hex, whole 4 KiB pages) | |
| never unloaded | | `-z nodelete` |

Taken because blaze's linker always does it: ld64's `-dead_strip`, `-pie`, `-search_paths_first`,
`-adhoc_codesign`, `-undefined error` for an executable; GNU's `--gc-sections`, `--as-needed`,
`--eh-frame-hdr`, `-Bsymbolic`, `--hash-style=gnu`, `-pie`, `-z now`, `-z relro`,
`-z noexecstack`, `-z defs` for an executable. Taken because it asks nothing of the image:
`-fuse-ld=...` and `--ld-path=...` (which system linker, and blaze runs none), GNU's `-O1`, ld64's
`-w`, `-z nodelete` for an executable, and `--no-as-needed` with no library after it.

Every other argument fails the link with one error naming each refused argument, the word of it
refused, where it came from (the rustflags, or the build script of which package) and the target.
Among those refused today: weak and re-exported libraries (`-weak_framework`, `-weak-l`), exports
of an executable and export lists (`-rdynamic`, `--export-dynamic`, `-export_dynamic`,
`-exported_symbols_list`, `--version-script`, `--dynamic-list`), layout scripts (`-T`), static and
start-file-less links (`-static`, `-nostartfiles`), `-z lazy`, `-z execstack`, `-ObjC`,
`-no_compact_unwind`, `-sectcreate`, `--wrap`, an object or archive given by path (give it as
`cargo:rustc-link-lib` with `cargo:rustc-link-search`), and every link argument of a Windows or
wasm target, which rustc gives `link.exe` or `wasm-ld` directly.

The rest of the rustflags that are the link's, rather than the code's, are read by
`crates/blaze-link/src/args/rustflags.rs`, and none is dropped:

| Flag | What blaze does |
|---|---|
| `-l [KIND[:MODIFIERS]=]NAME` | linked as a build script's `cargo:rustc-link-lib` is, in the place of the crate whose rustflags give it |
| `-L [KIND=]DIR` | searched as a build script's `cargo:rustc-link-search` is, before the build scripts' directories; `crate=` and `dependency=` find crates, which blaze finds by the unit graph |
| `-C rpath` | a macOS shared library is named `@rpath/FILE`, as rustc names it; rustc's other rpaths are to Rust dylibs, and blaze links every Rust crate into the image |
| `-C relro-level` | `full` is what blaze's linker always does on ELF; `partial`, `off` and `none` are refused there, and on macOS every value but `none` |
| `-C link-dead-code` | off is taken; on is refused: blaze's linker always drops unreferenced sections |
| `-C strip` | `none` is taken, `debuginfo` links the image without debug info, `symbols` is refused: blaze always writes the functions' local symbols |
| `-C linker` | taken when it names a C driver or a system linker (`cc`, `gcc`, `clang`, `ld`, `ld.lld`, `mold`, ..., with a version or a cross prefix): blaze's linker writes what it would; any other program (a wrapper script, `zig cc`) is refused, since it can change the link in ways blaze cannot see |
| `-C linker-flavor` | taken when it is the spelling blaze reads the target's link arguments in (`gcc`, `gnu-cc` on Linux, `darwin-cc` on macOS, `msvc` on Windows, `wasm-ld`); refused otherwise |
| `-C default-linker-libraries`, `-C link-self-contained`, `-C prefer-dynamic`, `-C target-feature=+crt-static` | off is taken; on is refused |

A refused option fails the link with an error naming the flag as written, that it came from the
rustflags, the target and why. A unit that links nothing (a library) is not refused for one, as
rustc does not use it there either. The rustflags of a proc macro, which blaze builds for wasm,
are the host's, meant for the native library rustc would make of it: none of them is applied to
its wasm module, which is no output of the build.

A unit's record key covers all of these: its own rustflags through its plan, the `-l`s of the
rustflags of the crates it links in as a part of their own, and its build scripts' link
arguments as a part of theirs.

### Sandboxed tests (an experiment, opt-in)

Tests run in a sandbox only where the daemon runs them (`blaze test --reached`,
[below](#which-tests-run)) and the workspace opts in, in its root manifest:

```toml
[workspace.metadata.blaze]
experimental = ["sandboxed-tests"]
```

or where a package does, in its own manifest, with `tests = true` under
`[package.metadata.blaze.sandbox]`. `experimental` lists, by name, the experiments a workspace
opts into: `sandboxed-tests`, and `const-macros` (proc macros written as functions of a file of
the package, run by blaze's const evaluator: [future/const-macros.md](future/const-macros.md)).
Any other name, or a value that is not a list of names, is an error that names the manifest and
the names known (`crates/blaze-units/src/load/sandbox.rs`). A test that is not sandboxed is
never kept as passed ([below](#which-tests-run)). Build scripts run in the sandbox
whatever the workspace says ([below](#build-scripts-and-proc-macros-run-in-a-sandbox)), and
proc macros run in blaze's own process, as WebAssembly modules.

In a workspace that opts in, each test binary the daemon runs starts once in a sandbox of its
own, tracing what it reads, as a fork server, and each test runs in a child forked from it
(`crates/blaze-usercode/src/test/queue.rs`). In it the machine's files are read-only; the workspace, the target directory and
cargo's home (`CARGO_HOME`, `~/.cargo`) are as they are, but what a test writes there is kept
to the run and gone when it ends (every cargo takes its locks in its home and records there
what it used, so a test that runs cargo, as trybuild's do, writes it); `/tmp`,
`/var/tmp` and `/dev/shm` (on macOS the temporary directory) start empty; there is no network
but the sandbox's own loopback, and no Unix socket but those the run makes itself; and the
environment is fixed. A write outside gets the error a read-only file system gives, and a
connection to anything but loopback is refused. The program is a session of its own, with no
controlling terminal, so it can't push input into the terminal it was started from
(`TIOCSTI`); Ctrl-C and `SIGTERM` reach it all the same, passed on by the sandbox's own
process.

The sandbox records what each test read under the test's own scope: the forked child marks
its scope before the test runs, and what the record has under that scope (and under none, the
server's own) is what the test read.

| a test | in the sandbox |
|---|---|
| writes the workspace or the target directory | allowed, kept to its run (an overlay) |
| writes `/tmp`, `/var/tmp`, `/dev/shm` | allowed, empty at its start |
| writes anywhere else | denied (`EROFS`): the test sees the error |
| reads a file, lists a directory, looks a path up | allowed, and recorded: part of what its pass is kept by |
| connects to, binds or sends to loopback | allowed: a network namespace of its own |
| connects to any other address, or a Unix socket of the machine's | denied (`ENETUNREACH`, `EACCES`): the test sees the error; so no name is resolved: a DNS server outside is denied, and one on the machine's loopback is not in the sandbox's |
| starts a program | allowed on Linux: what the program reads, its executable and libraries too, is recorded with the test's reads; on macOS a program macOS protects (`/bin/sh`) is started without the interposer, what it reads is not known, and the test's pass is not kept |

v1 failed a test the sandbox denied something, whatever it ended with, naming what was denied
and the way out; v2 records the refusal (`W` and `N` lines of the record) and the test ends as
it ends.

A package of a workspace that opts in takes its tests out of the sandbox in its manifest:

```toml
[package.metadata.blaze.sandbox]
tests = false                                 # every test of the package runs outside
unsandboxed-tests = ["tests::writes_home"]    # or these, by libtest's names
```

Any other key there is an error. The tests named run from a second server of the binary,
started outside, after every sandboxed test (`crates/blaze-cargo/src/serve/unit/test.rs`);
`-v` says which and why. `BLAZE_SANDBOX=off` in the command's environment runs every test and
build script of the command outside, as cargo does. A test outside the sandbox runs every
time: what it read is not known, so its pass is never kept.

Where no sandbox can be made for a test that is opted in, `blaze test` stops before any test runs, says why and how
to fix it, and exits 101. It does not go on outside with a warning: a run that did would break,
unseen, the promise that a test can't change the machine or reach the network, and a warning
in a long log is not seen. `BLAZE_SANDBOX=off` is the explicit way to run outside.

On Linux the sandbox is user, mount, network, IPC and PID namespaces: the machine bound
read-only, overlays for the workspace and target directory, tmpfs for the empty directories, a
network namespace with only loopback up, and a seccomp filter whose supervisor reads the paths
asked for. `/run` starts empty too, but for the links at its top (NixOS's `PATH` goes through
`/run/current-system`), and so does the Nix daemon's socket directory: a network namespace
hides abstract sockets but not the machine's Unix sockets (the Docker, D-Bus, systemd, Nix
daemon and agents' sockets of `/run` and `/run/user/$UID`), which would let a test have a daemon
outside do what it may not. Its overlays need namespaces a user can make; Ubuntu's AppArmor
stops them unless `kernel.apparmor_restrict_unprivileged_userns` is 0, which the error says
(a build script, which has no overlays, is confined with Landlock there instead). A filter is
installed on every run. It refuses `TIOCSTI` and `TIOCLINUX` (`EPERM`); `io_uring_setup`
(`ENOSYS`, as a kernel built without it answers and as Docker's default profile refuses it:
seccomp never sees an `io_uring` operation, so a ring would read, write and connect past the
record and the socket check, and libraries fall back to plain calls); and the calls of another
architecture's table (32-bit or x32 calls on x86-64, `ENOSYS`), which the filter's numbers
don't name. It asks the supervisor about each `connect` and each `sendto` with an address: one
to a Unix socket outside the overlays, the writable directories and the empty ones, with every
link on its path followed as the program sees them, is refused (`EACCES`) and recorded as an
`N` line, `unix:` and the path. When the run is recorded it also notifies writes, the network
and the harness's marks, and reads too when they are recorded (`affected`).

On macOS the sandbox is Seatbelt, applied with `sandbox_init_with_parameters` in the child
before it runs the program (as Nix does), and needs no root: a profile that allows everything
but writes (allowed only to the sandbox's scratch directory, the writable directories and the
null and random devices, not a terminal by name), the network (loopback, and Unix sockets
below the scratch and writable directories only, which keeps out `mDNSResponder`, `syslog`,
`usbmuxd` and every other daemon's) and Mach services. A service runs outside the sandbox and
writes for whoever asks: `cfprefsd` wrote a test's `defaults write` into
`~/Library/Preferences`, and launchd, LaunchServices and the background task manager take
jobs and login items. So every lookup is refused but these, which only answer or log:

| service | for |
|---|---|
| `com.apple.system.opendirectoryd.libinfo`, `.membership` | `getpwuid`, `getgrgid`, groups (Nix allows the first) |
| `com.apple.system.notification_center` | libSystem's notifications (time zone, network changes) |
| `com.apple.logd`, `com.apple.diagnosticd`, `com.apple.system.logger` | `os_log`, `syslog`, what a crash reports |
| `com.apple.FSEvents` | watching files (the `notify` crate) |
| `com.apple.trustd`, `com.apple.trustd.agent` | checking a certificate against the system's roots (Nix allows these too) |
| `com.apple.MTLCompilerService` (an XPC service) | Metal compiling a library from source at run time |

They were found on 2026-10-26 (macOS 27, M5 Max) by running, under a profile that refused
every lookup, the C toolchain (`cc`, `ld`), `xcrun metal` and `metallib` (which need none), a
Metal program that compiles a kernel at run time and dispatches it (the compiler service), an
FSEvents stream (`FSEvents`) and `getpwuid` (`libinfo`); a cold `blaze build` of a
workspace with `ring`, `cc`, `serde` and a build script that runs `xcrun metal` builds under
the list, and a build script's `defaults write` and `launchctl submit` fail. An interposer library blaze builds and loads with
`DYLD_INSERT_LIBRARIES` records what is read, makes the workspace and target directory
copy-on-write (`clonefile`, whiteouts for removed files, merged listings) and gives the empty
temporary directory. Seatbelt alone can neither record reads without root (`fs_usage` needs
it, and `(with report)` logs nothing a user can read) nor give private writes, hence the
interposer. Its limits: a program macOS protects (`/bin/sh`, `/usr/bin/*`) is started without
the interposer, so what it reads is not known (the test then runs every time under
`affected`, and `-v` says so) and its writes to the workspace are refused rather than kept
private; a test that makes raw system calls, or tampers with the interposer in its own process,
gets past the recording and the private writes but not Seatbelt's read-only machine or its
network rule; a sandbox inside the sandbox fails with `EPERM`.

What it cost v1, measured with `harness/sandboxbench.py` on 2026-10-26 (one sandbox per
executable, v1's way, against a sandbox per test). v2's fork server is a third way, one sandbox
per binary and a fork per test, not measured here; its costs are in
[v2/plan/lanes/usercode/tests.md](v2/plan/lanes/usercode/tests.md):

| | Linux x86-64 (dev host, 16 cores) | macOS (M5 Max, 8 at a time, loaded) |
|---|---|---|
| a trivial program, bare | 0.86 ms | 5.2 ms |
| in a sandbox | 8.0 ms | 29 ms |
| in a sandbox, recording reads | 9.3 ms | 25 ms |
| 600-test library, test phase, `BLAZE_SANDBOX=off` | 18 ms | 20 ms |
| the same, sandboxed, `all` | 37 ms | 68 ms |
| the same, sandboxed, `affected` (every test forced to run) | 80 ms | 100 ms |
| the same, a sandbox per test (16 at a time on Linux, 8 on macOS) | 0.68 s | 2.26 s |

A sandbox per test executable costs one launch, about 8 ms on Linux, whatever its number of
tests, and about 25 ms on macOS (mostly the fork, Seatbelt and dyld); a sandbox per test cost
the 600-test library ten to thirty times as much. On
macOS a stat under the interposer goes from 0.8 to 1.1 µs, and to 3 to 5 µs on a workspace
whose overlay has been written.

On Windows the sandbox is an AppContainer in a job object
(`crates/blaze-cargo/src/sandbox/windows.rs`): reads are granted to the container, writes to a
capability of the run's own and taken back as it ends, and there is no network without a
capability a run is not given. What a program reads is not traced there, so a pass of a test
run on Windows is never kept.

What is left: On Linux `openat2` always counts as a write; the
check of a Unix socket's path reads the address from the program's memory and follows its
links before the call goes on, so a second thread that changes the address, or a link swapped
between the check and the call, gets past it (seccomp's notifications can't be a security
boundary for pointers; the empty `/run`, `/tmp` and Nix socket directory are what keeps the
usual daemons out of reach whatever the program names); and a `sendmsg` with an address is only
asked about when the run is recorded, so a datagram to a Unix socket outside those directories
is not refused in a run that records nothing. A test that needs `io_uring`, a 32-bit helper
program or a terminal of its own (`/dev/tty`, or `/dev/ptmx` on macOS) runs outside
(`unsandboxed-tests`). On macOS a test that makes raw system calls gets past the interposer's
recording (above); Metal's compiler service and the log write outside on the program's behalf
(the shader cache, the system log), as the allowed services do. The plan is
[#257](https://github.com/indexable-inc/blaze/issues/257). Build scripts and proc macros run
in the same sandbox, below.

### Build scripts and proc macros run in a sandbox

Every build script, the workspace's and its dependencies', is compiled by blaze and runs in
the sandbox tests run in ([#147](https://github.com/indexable-inc/blaze/issues/147)), as a
spawn of the daemon's driver through blaze's launcher (`blaze sandbox --v2`,
`crates/blaze-cargo/src/sandbox/v2.rs`); `crates/blaze-cargo/src/serve/unit/script.rs` is the
one place a run is made. A script is given the command's whole environment with what cargo
sets for scripts over it (`CARGO_*`, `DEP_*`, `OUT_DIR`, `TARGET`, `PROFILE`, ...), as cargo
gives it (`script.rs`, `inherited`). The variables are the command's, not those of whoever
started the daemon: `SDK_DIR=/opt/sdk blaze build` gives the script `SDK_DIR` with a daemon
started without it. What the run read, its files and its variables, the sandbox traces, and the run is kept in
the store by its code, what it was given and what it read (`blaze_usercode::run`), so a
command whose inputs to it are as they were finds it there and runs nothing. On Linux and
macOS the variables passed through from the command (all but what cargo sets) are lent to the
script rather than put in its environment (`blaze_usercode::confine::GIVEN`,
`crates/blaze-cargo/src/sandbox/v2.rs`): it sees each when it reads it by name, and that read
is traced, so a variable it never reads (a shell's `OLDPWD` or `SHLVL`) is in no key. What the
C library and the loader read themselves (`PATH`, `TZ`, `LANG`, `LC_*`, `LD_LIBRARY_PATH`,
... : `READ_BY_THE_C_LIBRARY` there) stays in its environment and is in every such key. A
script that walks its environment (`env::vars`, a `Command` it changes the environment of) is
lent all of it then, and is keyed by all of it. A program it starts with its environment gets
all of it, as with no sandbox, and is keyed by what it reads: by all of it, unless its
executable proves std alone walks its environment (`__blaze_std_says_walks`, from blaze's
linker), which no C compiler or shell does, so a script that runs `cc` (through `cc-rs`) is
still keyed by every variable; a program the sandbox can't load its interposer in (a static
one, one macOS protects) makes the run not whole, kept as cargo keys it. The run's temporary directory is
the sandbox's own, under every name one goes by (`TMPDIR`, `TMP`, `TEMP`, `TEMPDIR`,
`NIX_BUILD_TOP`), so a shell's (`/tmp/nix-shell.XXXXXX`) is never in a key. A script that provably prints
only constants is neither compiled nor run (`blaze_usercode::script::inert`;
`BLAZE_ELIDE=verify` compiles and runs it as well and fails the build where the two differ).
The root manifest is the one of the workspace built, also for a path dependency that is a
workspace of its own. The machine is read-only; it writes
its `OUT_DIR` for real, which is the one directory the Cargo book lets a script write ("Scripts
should not modify any files outside of that directory"); the temporary directories start empty;
there is no network but the sandbox's own loopback. Its package's directory, the workspace and
the target directory are seen as they are, even below `/tmp` or `/dev/shm`. On macOS the
per-user cache directory (`getconf DARWIN_USER_CACHE_DIR`, `/var/folders/../C`) is writable
too: clang's module cache, `xcrun`'s and the Metal compiler's live there, are found by
`confstr` and not by a variable a private one could be named in, and check what they cache.
Cargo's own rules for running a script again are unchanged.

A script that fails having been refused a write or a connection fails as cargo says it
fails; v1's note under its output naming what was refused and the way out is not in v2.

A script that downloads (a prebuilt `v8`, `ort-sys`, `webrtc-sys`, `skia-bindings`, a clone of
FFmpeg) fails in its sandbox, which has no network. v1 gave such scripts a pinned copy
(`blaze lock --fetches`, `Cargo.fetches.lock`); v2 has neither the command nor the lock file.

The ways out, in the workspace's root manifest, which is the only one a dependency from a
registry can be named in:

```toml
[workspace.metadata.blaze.sandbox]
unsandboxed-build-scripts = ["hegeltest", "flecs_ecs_sys"]   # by package name
build-script-link-dirs = ["/opt/vendor/lib", "${OPENSSL_DIR}/lib"]   # rustc-link-search may name
build-script-env = ["OPENSSL_DIR"]                                   # a ${NAME} above must be listed here
```

An entry that starts `${NAME}` is under the variable's value in the environment the script runs
in, for a library whose place differs by machine; `build-script-env` must list `NAME`, and a
script run where it is unset, or is no absolute path, fails naming it.

The table may also have `unsandboxed-proc-macros`, `build-script-env` and `wasi-build-scripts`,
each a list of names, which v1 read: v2 checks that each is a list of strings and does nothing
else with it beyond naming the variables `build-script-link-dirs` may use
(`crates/blaze-units/src/load/sandbox.rs`). Proc macros run in blaze's own
process as WebAssembly modules, confined by the host, which is their sandbox and has no way out
([below](#proc-macros-run-as-webassembly)), and a script is given the command's whole
environment already.

What a sandboxed script prints is checked before it is used (`crates/blaze-cargo/src/directives.rs`), since
it goes into every rustc and linker run of its package and of what links it. A
`rustc-link-search` (or `rustc-flags`' `-L`) names a directory in its `OUT_DIR`, its package,
the target directory's build directory (its dependencies' `OUT_DIR`s), a system directory
(`/usr/lib`, `/lib64`, `/usr/local/lib`, the Nix store, Homebrew's, Apple's SDKs), one of
`build-script-link-dirs`, or the layout of a [pinned native input](#pinned-native-inputs) the
script was given; a `rustc-link-arg` is one of a short list (`-pthread`, `-rdynamic`,
`-Wl,--as-needed`, `-Wl,-z,now`, `-Wl,-soname,NAME`, `-lNAME`, ...) or an rpath to `$ORIGIN`
or a directory a search may name; a `rustc-env` sets no variable cargo or rustc gives the crate
itself (`CARGO_PKG_*`, `CARGO_MANIFEST_DIR`, `OUT_DIR`, `RUSTC*`, ...); what cargo gives only
build scripts (`CARGO_CFG_*`, `CARGO_FEATURE_*`, `CARGO_ENCODED_RUSTFLAGS`) it may forward, as
rav1e's does. Anything
else fails the build, naming the script and each line. Over the bench corpus's 587 runs the
rules refuse none (`tools/bench/results/2026-10-27-bscache/directives.md`).

and, for a package of the workspace, in its own manifest:

```toml
[package.metadata.blaze.sandbox]
build-script = false
```

Any other key in either table is an error, and so is a value that is not a list of names (a
`bool` for `build-script`). A package from a registry or git can't take itself out with its own
manifest. `BLAZE_SANDBOX=off` runs every build script and test of the command outside, as
cargo does. Where no sandbox can be made, the script fails with why, as a test run does:
nothing ever runs outside instead, nor goes to cargo or rustc.

Where a user may not make a user namespace, a build script's sandbox is confined with Landlock
instead (Linux 5.13 on). Ubuntu 23.10 on keeps users from them through AppArmor
(`kernel.apparmor_restrict_unprivileged_userns=1`: `/proc/self/setgroups: Permission
denied`), and Docker's default seccomp profile refuses `unshare(CLONE_NEWUSER)` to a process
without `CAP_SYS_ADMIN` ([#420](https://github.com/indexable-inc/blaze/issues/420)). What a
build script is promised holds the same: it writes only its `OUT_DIR` (and the per-user cache
on macOS), reaches no network, sees the environment it was given, and what it read is
recorded. Landlock refuses every write outside the writable directories, a `TMPDIR` of the
run's own and the terminal and null devices, and every TCP `bind` and `connect`; the seccomp
supervisor refuses every address a call names that is not a Unix socket of the sandbox's own
(the loopback and the abstract sockets are the machine's there) and every signal to a process
that is not the sandbox's; the sandbox's first process reaps what the program leaves and kills
it as the program ends. What namespaces hide and this does not: the machine's `/tmp`, `/run`
and processes are seen (read-only), and SysV IPC and a file's mode, owner and times are not
refused. A sandboxed test's overlays need a mount namespace, so a workspace that opted into
sandboxed tests still needs user namespaces: where they can't be made, its tests fail saying
so, with the ways on (`sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`, an
AppArmor profile that allows `userns` for blaze; in a container `--security-opt
seccomp=unconfined`, or on Kubernetes `securityContext.seccompProfile.type: Unconfined`) or
`BLAZE_SANDBOX=off`, which runs them outside, as cargo does. Where neither namespaces nor
Landlock can be had, a build script fails the same way, naming both.

A sandbox can be given a view (`sandbox::View`, the spec's `view`) in place of the machine,
which is what a build script is to see under the Nix model (Andrew, 2026-10-27): its declared
closure and nothing else. On Linux the view is a root of the sandbox's own, a tmpfs that
becomes `/` (`pivot_root`) with the machine's tree detached from it, holding what the view
binds, each where it says (the package's source and the toolchain read-only, a store path
unpacked in blaze's cache at its `/nix/store/<hash>-<name>` so that nixpkgs' interpreters,
RPATHs and scripts resolve, `OUT_DIR` written for real), and besides them only a `/proc` of
the sandbox's processes, a `/dev` of `null`, `zero`, `full`, `random`, `urandom` and `tty`
with an empty `shm`, an empty `/tmp`, and an `/etc` of `passwd` and `group` (one line for the
user, one for `nobody`) and `hosts` (`localhost`), as Nix's sandbox has. `/usr`, `$HOME`, the
machine's `/etc` and every store path not bound are not there, for the script and every
program it runs, so a read of something undeclared fails with `ENOENT`; the root and what is
bound read-only refuse writes, and the network is the loopback alone, as in every sandbox.
A view whose bind is not there fails before anything runs, naming it. It is made by an
unprivileged user namespace alone, and costs a mount per bind, about 20 µs each: `true` takes
6.1 ms in a view of 40 binds and 10.7 ms in one of 300, against 4.9 ms in the sandbox without
a view and 0.33 ms bare (median of 50, an arm64 ix VM of 16 cores, 2026-10-27). On macOS
a view is not made (decided): the spec's other fields apply and the binds are not read.

v2 runs no build script in a view yet: v1's setting that ran one in its
declared Nix closure (with `[workspace.metadata.blaze.system-deps]` and `blaze-engine closure
fetch`), is gone, and v2 has no setting for it.

A build script's compile is a unit like any crate's, its functions kept in the store by their
code. Its run is a run of user code (`crates/blaze-usercode/src/run.rs`): found in the store
when one is there for its inputs as they are now, else spawned in a sandbox that traces, its
result kept under its code, what it was given and exactly what it read, so `rerun-if-*` is a
hint and the record is what counts. What is kept is its exit, what it printed and what it left
in `OUT_DIR`, which a reused run writes back as it was. A run that failed is not kept, and
runs again on the next build, as cargo reruns it. A run outside the sandbox
(`unsandboxed-build-scripts`, `BLAZE_SANDBOX=off`) is never kept. The design is
[v2/plan/lanes/usercode/scripts.md](v2/plan/lanes/usercode/scripts.md).

A build script's sandbox costs one launch (8 ms on Linux, 38 ms on macOS, measured on
2026-10-26), which is lost in a cold build's noise.

#### Pinned native inputs

A build script that needs a prebuilt native library (llvm-sys needs LLVM) is given it by the
workspace's root manifest, pinned by its digest, in place of a variable each machine sets:

```toml
[workspace.metadata.blaze.native.llvm]
pin = "crates/hir-ty-codegen/llvm.json"     # the pin file, relative to the root manifest
env = "LLVM_SYS_231_PREFIX"                 # the variable the script reads the directory from
packages = ["llvm-sys", "blaze-stencil/llvm-stencils"]
```

The pin file is the one home of the pin: JSON with `release`, the `https://` URL the archives
are under, and `targets`, each target triple's `{"name": "<file>", "sha256": "<64 hex>"}`;
its other keys are its own (llvm.json's `version`, `commit`, ...). A package is named alone, or
as `name/feature`, for its script only when the run has that feature on (blaze's wasm seed
builds blaze-stencil without `llvm-stencils`, and asks for no LLVM). The load reads the pin
through the build's files, so an edit to it is an edit to the build, and for each script run of
a named package takes the archive of the target the run is built for; a pin with no archive for
that target, a `release` that is not `https://`, a `name` that is not one file name or a
`sha256` that is not 64 lowercase hex digits fails the load naming the input, the target, the
pin file and the package (`crates/blaze-units/src/load/native.rs`). `env` may not be a variable
cargo or blaze sets, or a loader, shell, make, compiler driver or reproducible build reads:
`OUT_DIR`, `TARGET`, `HOST`, `NUM_JOBS`, `OPT_LEVEL`, `DEBUG`, `PROFILE`, `CARGO`, `PATH`,
`HOME`, `TMP`, `TEMP`, `TMPDIR`, `SHELL`, `BASH_ENV`, `ENV`, `IFS`, `MAKEFLAGS`, `MFLAGS`,
`SDKROOT`, `SOURCE_DATE_EPOCH`, `MACOSX_DEPLOYMENT_TARGET`, `CC`, `CXX`, `AR`, `CFLAGS`,
`CXXFLAGS`, `CPPFLAGS`, `LDFLAGS`, or any starting `CARGO_`, `DEP_`, `RUST` (`RUSTC`,
`RUSTFLAGS`, `RUSTDOCFLAGS`, ...), `BLAZE_`, `__BLAZE`, `LD_`, `DYLD_`, `PKG_CONFIG`, or the
`cc` crate's per-target `CC_`, `CXX_`, `AR_`, `CFLAGS_`, `CXXFLAGS_`, `LDFLAGS_`, `TARGET_`,
`HOST_`. Two inputs may not give one script the same variable.

Before any of the build's units is lowered (or a trace's or verify's view is read), the
daemon lays out each input the build needs, on an IO thread
(`crates/blaze-cargo/src/serve/native.rs`): one layout task a digest, shared by every job
asking it whatever it names the input, so the daemon never waits on its own lock; a wait for
another process laying out the same digest stops once every job waiting on it is cancelled or
ended (one job's cancel never fails another that joined it). The
archive is got through blaze-fetch by its sha256, from the store when it has it and else
downloaded, checked and kept there, then unpacked into `<BLAZE_HOME>/native/sha256-<hex>`
(`~/.blaze/native/...` when `BLAZE_HOME` is unset), read-only, one directory per digest. The
bytes unpacked are hashed as they are read and held to the pin before the layout is used. It is
made under a lock in a directory beside the final one (what a stopped layout left there is
removed first), every file and directory synced, its mark `.blaze-native` written last and
synced, and renamed into place whole, so a directory there is whole. The mark holds the digest
and a listing of every file and link with its kind, BLAKE3 and length; the first time a daemon
hands a layout to a build script it walks it against that listing, hashing every byte again
(a span, `check pinned native input layout`, says how many files and bytes and how long), and a
layout with no mark
for its digest, a file changed, added, removed or made writable is refused naming the file and
how to remove the layout (`chmod -R u+w <dir> && rm -rf <dir>`), never used or written over.

The archive (a `.tar.xz`, as `llvm.py pack` writes) goes through blaze-fetch's one checked
unpacker (`crates/blaze-fetch/src/unpack.rs`, which a registry's `.crate` goes through too): it
must be one top directory, which is stripped; an entry with an absolute path, a `..`, a `\` or
a NUL, a device, FIFO or sparse file, two entries that are one name under Unicode's
canonical caseless match (NFD, then full case folding from CaseFolding.txt, then NFD; meant to
cover what a case- and normalization-insensitive disk such as APFS takes as one, which uses its
own tables), an entry under a file or link, an entry named as the mark
in any case, a symbolic link whose target leaves the layout or passes through another link
(names compared the same way), a link the disk resolves outside the layout or to nothing, or a
hard link to anything but a file written before it fails the layout, and so do more than 2^20
entries, 16 GiB of files, or an xz dictionary over 256 MiB. Every directory is made before any
link, so nothing is written through one. A download that fails or is not the pinned bytes fails
the build naming the input, the target, the URL, and the pinned and received digests; there is
no other place a script is pointed at. Old layouts are not yet collected (a follow-up for
docs/gc.md): remove one with the command above.

Each script run of a named package is then started with `env` set to that directory. The
variable is set over the command's own (blaze's choice, 2026-11-04): a shell that sets it (`nix
develop` sets `LLVM_SYS_231_PREFIX` to the flake's LLVM) keeps working with no change, and the
script still links exactly the bytes the pin names, never the machine's, so two machines build
the same thing. To try another LLVM, change the pin. The script's sandbox may read the
directory and its `rustc-link-search` and rpaths may name it; each input's digest is a part of
the run's key (`blaze_usercode::script::Place::pinned`), so another archive is another run.

### Proc macros run as WebAssembly

Every build runs its proc macros as `wasm32-wasip1` modules under Wasmtime, in the daemon's own
process (`crates/blaze-macro-eval/src/wasm.rs`, `blaze_macro_eval::load_wasm`;
[v2/plan/lanes/usercode/macros.md](v2/plan/lanes/usercode/macros.md)). blaze builds each
proc-macro crate, and every crate it depends on, for `wasm32-wasip1` with its own front end and
code generator, against the real std and `library/proc_macro` (`UnitPlan::macro_wasm`,
`crates/blaze-units/src/load.rs`): nothing rustc compiles is loaded and no process is started.
The crate's root gets the module `__blaze_proc_macros` (its list of macros and the bridge's
glue, `crates/blaze-macro-eval/src/wasm/glue.rs` and `guest/entry.rs`), and it is linked as a
WASI reactor with no debug sections. A proc-macro crate's lang items, and its dependencies',
never reach the crates that use its macros.

These units are kept apart from the build's own. The unit graph marks each (`"macro_wasm":
true`, a key cargo's graph has not), they are keyed and fingerprinted on their own, and their
files go under `target/wasm32-wasip1+macros/<profile>/`, beside `target/<profile>/` and
`target/<triple>/<profile>/`. So a `blaze build --target wasm32-wasip1` has two units of a
package its code and its macros both use, one for each, which share no key and no file. The
front end keeps them apart as well: a crate's key (`blaze_items::key::crate_key`) covers what
it is built for (the host, a triple, or that triple's wasm macros), so its standard library
and the macros' are two crates of two keys. A `--target` of the host's own triple asks a
standard library for its host units and one for its target units. In a dev build these are
two (two `StdKey`s, two directories): the host's units run where they are built, so their std
is compiled for this machine's own CPU, while the target's, which may be shipped, is compiled
for the target's baseline (`blaze_units::schema::runs_here`, the one rule both codegen and
`StdKey` read). In a release build both run elsewhere, so they are one key and share one set
of std units. Its
own units abort too: a profile's `panic = "unwind"`, cargo's default, gets no `-C panic` from
cargo, so blaze builds each unit with its target's own strategy, as rustc does
(`crates/blaze-units/src/load/units.rs`). The standard library the macros link is their own
too: planned and keyed as macro units, with their own metadata and directory, built with
`panic = "abort"`, as every macro unit is, and with the bridge (std's `hashmap_random_keys` is
the bridge's import of the evaluator's fixed keys, so `RandomState` reads no WASI randomness).
A proc-macro crate's own
`#[test]`s are built for the host and run there, as cargo builds them; its integration tests use
its macros through the module, as any crate does. `blaze check` checks a proc macro as the
module it builds, with `wasm32-wasip1`'s cfg.

`-Z wasm-macros`, which asked for this before it was the default, is still accepted and
changes nothing. There is no setting that runs a macro
any other way.

The module's only imports are the `proc_macro` bridge's shims and the WASI preview 1 functions
std calls (`src/wasm/abi.rs`). A module that imports anything else is refused at load with an
error naming the crate and the import. The bridge's requests reach blaze's own server, with a
`usize` in eight bytes from every client, as the pinned bridge encodes it (`MAX_USIZE_SIZE`,
`src/wire.rs`). The WASI calls see blaze-macro-eval's machine and are traced
(`src/wasm/wasi.rs`, `src/os.rs`): the macro opens and reads files and directories, but only
below the calling crate's `CARGO_MANIFEST_DIR` and `OUT_DIR`; it writes nothing (a path opened
to create, truncate, append or write stops the expansion naming it), sets no variable and makes
no system call blaze has no shim for. The glue's `getenv` replaces wasi-libc's, which reads the
whole environment on its first call, and asks the host one variable at a time (the bridge's
`env_var`). So `env::var` keys an expansion by the variables it read, and a per-run variable it
never reads (`BLAZE_PROFILE`) never makes it miss. Only `env::vars` reads them all.

Each expansion runs in a fresh instance under a fuel budget (`src/wasm/host.rs`, `FUEL`) and a
memory budget. The instance runs on a kept expansion thread, never on a fiber, because a
Wasmtime activation lives in its thread's locals and a fiber moves threads when it waits
(`src/wasm/thread.rs`). The job waits on an event, holding no thread, and answers what the
macro asks the database of a span outside any wasm call. Each expansion opens a `proc macro
expansion` span naming the macro and the input item. A module's machine code is compiled once a
process and kept in the store under the module's id and Wasmtime's version and settings
(`src/wasm/code.rs`), so the next process loads it without compiling. A module that can't be
compiled, loaded or run is an error naming the macro, and nothing then runs it another way. On
wasm32 a `#[thread_local]` static is plain data (no TLS without atomics), a panic's `Location`
is four 32-bit words, and a direct call of an undefined weak function traps as wasm-ld's stub
does.

A macro's state carries from one call to the next as it does under
rustc: the link lists each static of the macro's crates its code can write in the module's
`blaze.statics` section, and the host writes what the calls before left into each fresh
instance before the macro runs, then reads back what it changed (`src/wasm/statics.rs`). A
module without that section is refused at load. A static left holding an address of the heap
(a lazily built table, a cache behind a pointer: logos's, `proc-macro-crate`'s) can't be given
to a fresh instance, which has no such heap; from then on the calling crate's earlier calls of
the macro run again, in order, in the next call's instance before it, as they ran in one
process under rustc, and as the evaluator runs them. A call is kept and found under the state
it started from, and only when it left that state as it was; one after the earlier calls run
again is never kept.

Each expansion is kept in the store (`crates/proc-macro-api/src/kept.rs`) by the macro's
library, its name, input and attribute tokens, the crate's variables, its directory and the
toolchain, and found by what else it read; `BLAZE_PROC_MACRO_CACHE=check` expands every kept
call anyway and fails naming a macro whose output differs.

blaze's MIR evaluator (`crates/blaze-macro-eval`,
[v2/plan/design/compile-time.md](v2/plan/design/compile-time.md), change 20) can run a proc
macro's code too, on the same machine and server, and stays as const eval's and as the
reference the module's output is tested against (`crates/blaze-macro-eval/tests/wasm.rs`). A
build never runs a macro on it: a proc macro that reaches a build built for the host is an
error naming it (`crates/blaze-cargo/src/serve/unit/compile.rs`, `macros`).

v1 could compile a build script for `wasm32-wasip1` and run it under Wasmtime (`wasi-compare`,
`wasi-build-scripts`), and could expand with rust-analyzer's out-of-process proc-macro server.
v2 does neither, and their crates are deleted: every build script runs natively in the
sandbox. Nothing in blaze reads v1's variables or manifest lists for them, so setting them
changes nothing. The comparison of the two ways of running scripts is
[reports/BSWASI.md](reports/BSWASI.md).

### Which tests run

Plain `blaze test` runs every test, as `cargo test` does ([above](#what-a-build-does)). Two
flags of blaze's own (extensions, opt-in; `crates/blaze-cargo/src/cli/args.rs`) have the daemon
run them instead and send each result, which the client prints in libtest's format:

- `--reached` runs only the tests an edit can have changed. Each test runs in a process of
  its own, forked from its binary's fork server, on one queue over every binary and every
  core, the longest expected first (`crates/blaze-usercode/src/test/queue.rs`). Its pass is
  kept in the store under the functions it executed (a byte per function slot the runtime
  writes on the slot's first call) and, in the sandbox, what it read; a test whose executed
  functions and reads are as they were when it passed is not run, and is reported cached with
  its last pass (`crates/blaze-testsel`). A test that ran outside the sandbox is never kept,
  and the client names those last, on stderr. With a JSON `--message-format` each result is a
  `blaze-test-result` line on stdout instead of libtest's text: `name`, `binary`,
  `executable`, `outcome`, `millis`, `outside`, `output`, and `cached` (null for a test that
  ran; else the kept run's `basis`, `executed` or `reached`, its `why`, `millis` and
  `outcome`), as `crates/blaze-cargo/src/cli/json.rs` (`test_result`) writes it. The design is
  [v2/plan/lanes/usercode/tests.md](v2/plan/lanes/usercode/tests.md) and
  [v2/plan/design/compile-time.md](v2/plan/design/compile-time.md), idea 3.
- `--ub` runs the tests picked (every one, or with `--reached` those it would run) in blaze's
  MIR evaluator under the checker of undefined behavior (`crates/hir-ty-ub`), compiling
  nothing; `--ub-seed N` and `--ub-schedules K` set its seed and how many thread schedules it
  tries. `--ub` with `--no-run` is refused.

v1's test cache is gone with v1: `BLAZE_TESTS` (`affected`, `verify`, `all`),
`BLAZE_TESTS_FROM`, `BLAZE_TEST_ENV`, `blaze test --why`, and the `tests-passed`,
`tests-programs` and `target/blaze/tests.ndjson` files it wrote. No v2 crate reads or writes
them.

### Tests are hermetic, or say why not (v1, not in v2)

v1 read each test from the code it compiled, found what it reached that its code did not
decide (files, the environment, the network, processes, foreign code, assembly, mutable
statics, the clock, randomness, threads) and kept the pass of a test that reached none by its
code alone, writing what it found to `target/blaze/tests.ndjson` and
`tests.summary.json` (`blaze test --why`). The walk is still in
`crates/hir-ty-codegen/fncache.rs` (`tests_reach`), but nothing in v2 calls it; v2 keeps a
pass by the functions the test executed and what it read ([above](#which-tests-run)).

v1 also started a spare worker while cargo built, to be ready to patch rustc's executable
(`BLAZE_SPARE`); v2 runs no cargo build and has no spare worker.

## blaze never builds with cargo

v1 handed a build to cargo when it could not be sure of its own (`blaze: bug: <reason>;
building with cargo`). v2 has no such path: the client parses cargo's command line itself
(`crates/blaze-cargo/src/cli/args.rs`, with clap's and cargo's own refusals and texts), and
the daemon resolves the workspace (`--workspace`, `--exclude`, `-p`, features, `--target`),
plans the units and builds them, or stops with an error naming what it can't do and where
([above](#what-a-build-does)). A changed manifest, `Cargo.lock` or configuration file is read
again by the next request; there is no cargo build in between.

Doc tests are built by blaze, never rustdoc: `blaze test` plans a library's doctest unit as
cargo does, and `blaze test --doc` plans only those. The daemon finds the examples with
rustdoc's rules and makes each its own crate, program and process, never merged; the unit's
program is a libtest runner that runs each under rustdoc's name and judges it as rustdoc
does. The examples are built side by side, an edit of one example rebuilds and relinks that
one alone, and a `test_harness` example is built as a test under its own libtest harness, as
rustdoc's `--test` builds it. A unit blaze can't build fails alone: unless
`--fail-fast`, every other unit's tests still run, and the end names the doc tests it held back. The design and what is left are in [v2/plan/doctests.md](v2/plan/doctests.md).

## The daemon

There is one daemon per user, `blaze serve` (`crates/blaze-cargo/src/serve.rs`). The first
`blaze` that finds nothing listening starts it in a process group of its own, so it outlives
the command, and waits on a pipe (`--ready-fd`) the daemon writes a byte to as it enters each
phase of its start (reading its state, setting up its store), then a newline once its socket is
bound; the command connects only after that newline, and gives each phase 60 s. A start that
stops short is an error naming the phase it reached and, when the daemon ended, its exit code
or signal; or systemd or launchd start it on the first connection
after `blaze daemon install` (socket activation; `blaze daemon uninstall` removes the units). It does not exit
when idle: it stops on `blaze stop` or SIGTERM, letting go of its socket first and then writing its state.

A daemon holds `serve.lock` in its state directory, which names its pid, for as long as it runs.
One started while the one before it still ends (writing its image) says so on the pipe and
waits at most 300 s for that lock, then stops with an error naming the other's pid; it never
starts over a live daemon. A listener removes its socket's file only while that file is still
the socket it bound, so an ending daemon never removes the next one's. A command that finds a
daemon of another build tells it to stop and waits for its process to end before starting
its own.

It is one process, with no workers: one loop holds the API server, the watched files, the
database, the store and the scheduler, for every workspace and checkout it is asked about.
What it may hold is the machine's memory less a margin of 1 GiB: the least of what the
machine has, the control group's limit on Linux and the process's `RLIMIT_AS`, less what is
in use there (`blaze_graph::arena::machine_budget`); where none can be read it stops with an
error. `blaze status` prints:

    pid: 10969
    requests: 1 (7)
    views: 2
    memory: 1.3 GiB of 30.1 GiB
    bugs: 0
    builds: 1
      #7 test (2 min 05 s): waits for its pinned native inputs, 1 of 1 layouts left
        downloading llvm-21.1-aarch64-apple-darwin.tar.xz: 34.0 of 410.0 MB (8%), 2.1 MB/s, 3 min 00 s left
    downloads: 1
      llvm-21.1-aarch64-apple-darwin.tar.xz: 34.0 of 410.0 MB (8%), 2.1 MB/s, 3 min 00 s left
        from https://…/llvm-21.1-aarch64-apple-darwin.tar.xz

the requests it is answering, how many views (a workspace at a checkout) it holds, and what it
holds of its budget; then each build a client waits on, as the daemon looks at it once a
second (its request, command and age, where it is, and for each thread working for it the
outermost and innermost open span with their ages), and every download in flight with its
progress. `blaze status --json` prints one object instead, for a script or an agent,
and starts no daemon:

    {"format":1,"version":"blaze 0.1.0 (...)","api_schema":41,"home":"/Users/me/.cache/blaze","state":"/Users/me/.cache/blaze/v2","socket":"/Users/me/.cache/blaze/v2.sock","daemon":{"pid":10969,"build":"<64 hex>","exe":"/usr/local/bin/blaze","uptime_ms":5230000,"memory_bytes":1395864371,"budget_bytes":32319628083,"bugs":0,"requests":[7],"views":2,"history":null,"settings":{"BLAZE_DAEMON_MEMORY_GB":null},"builds":[],"downloads":[]}}

`format` is the object's own shape (a new number when a field changes meaning or goes);
`version` and `api_schema` are this blaze's (`blaze --version`, the daemon API's schema);
`home` is blaze's root (`BLAZE_HOME`, else `blaze` in the user's cache directory), `state` the
daemon's directory under it and `socket` where the CLI connects. `daemon` is `null` when none
answers; else its pid, the hash of its executable as it started (`build`) and that file, how
long it has run, its memory and budget in bytes, the bugs it went on past (every site's summed;
the human `blaze status` lists each), the requests under way, its views, the profile history's
last error, the settings it was started with, its `builds` (`id`, `what`, `phase`, `spans`,
`for_ms`) and its `downloads` (`what`, `url`, `done`, `total`, `rate`, `idle_ms`,
`elapsed_ms`). `blaze toolchain --json` is the same for the
Rust blaze builds with: `{"name","nightly","rustc","release","commit","host","sysroot"}`, the
host's tuple among them, what a script asks instead of `rustc -vV`
([toolchain.md](toolchain.md)).

A build whose tasks run while no span closes, no query completes and no task or step ends for
the stall bound fails with an error naming what each thread is inside, and the daemon stops
([profiling.md](profiling.md#the-stall-watchdog)). The bound is 120 s; `blaze serve
--stall-bound SECS` sets another, for a daemon started by hand (`blaze stop` first) on a host
so loaded that one piece of work can go that long with nothing else moving.

### Cores and priority

By default blaze leaves two cores to the rest of the machine and runs at background priority,
so a long build does not freeze the desktop (on 2026-11-03 a release build at every core and
normal priority took an 18-core Mac to load 154 and froze its GUI). `BLAZE_CORES` says
otherwise:

| `BLAZE_CORES` | Cores | Priority |
|---|---|---|
| unset (or empty) | the machine's less 2, at least 1 | background |
| `all` | every core | normal (CI and ix VMs set this) |
| `n`, a whole number of 1 or more | exactly `n` | background |

Anything else is an error naming the variable, the value and what it takes. The budget sizes
the daemon's plane, its worker pool, test runs (a test binary's `--test-threads` is taken from
the same budget as the processes running beside it) and the `NUM_JOBS` a build script's `cc`
is told (a quarter of the budget: blaze has no jobserver to share, and scripts often run a
few at once). `blaze test`'s live view runs a quarter of the budget's test binaries at once,
each with `RUST_TEST_THREADS` at a quarter, unless the command sets `RUST_TEST_THREADS`
itself. Background priority is nice 10 (`setpriority`), which every thread and child process
inherits; on macOS the worker pool's threads also drop from `user-initiated` to the `default`
QoS class. macOS's own background state (`PRIO_DARWIN_BG`) is not used: it throttles disk and
network IO and keeps the work on the efficiency cores. A priority blaze cannot lower is a
warning naming the call and errno, and the build goes on at the priority it started with.

The client and the daemon each read it once, as they start (`blaze_core::cores`). A daemon's
budget and priority are fixed for its life, so a command that sets a `BLAZE_CORES` giving
another budget or priority than the running daemon's is refused before any request, in one
error with every other daemon setting it differs in; `blaze stop` and run it again, and the
daemon that command starts takes its setting. A command that leaves it unset runs on the
daemon's. A daemon systemd or launchd starts (`blaze daemon install`) reads its unit's
environment, which captures the installing shell's `BLAZE_CORES`.

### Files

The socket is `v2.sock` in `$BLAZE_HOME` if set, else `$XDG_RUNTIME_DIR/blaze`, else
`~/.cache/blaze` (`blaze serve --socket PATH` names another). What outlives the daemon (the
watched files' journal, the database's image, the store, the scheduler's estimates) and its log
`serve.log` are in `$BLAZE_HOME/v2`, else `$XDG_CACHE_HOME/blaze/v2`, else `~/.cache/blaze/v2`,
never in `XDG_RUNTIME_DIR`, which does not outlive the session. The directories must belong to
you and be closed to group and others (mode 0700); otherwise the client and the daemon refuse
to use them. The socket is 0600, and a connection from another user is refused before its
request is read. `blaze serve --http ADDR` listens on TCP as well, only with a bearer token in
`BLAZE_API_TOKEN`.

In a project, the daemon writes the programs and test executables where cargo would in the
workspace's build directory, under `<profile>/` there.

### Where a build writes

blaze writes no build file into the checkout: there is no `./target`. A workspace builds into
`<root>/build/<name>-<id>`, where `<root>` is blaze's root on the machine (`$BLAZE_HOME`, else
`blaze` in `$XDG_CACHE_HOME`, else `~/.cache/blaze`; the daemon's state is `v2/` beside it),
`<name>` is the last part of the workspace root's path and `<id>` is the first 32 hex digits of
the BLAKE3 of that path (domain `blaze workspace build dir 1`,
`crates/blaze-manifest/src/home.rs`). Two checkouts of one repository, worktrees among them,
get two directories; a lane needs no `CARGO_TARGET_DIR` of its own. The directory has no `v2/`
on purpose: it is the user's to open and delete, not the daemon's state.

The directory holds what has to be a real path: build scripts' `OUT_DIR`s, the executables
that are run (programs, tests, build scripts) and the `doc` pages a user opens. What is
addressed by content lives in the store. An output of 16 KiB or more (a library, an executable,
a test) is placed in the directory as a clone of the store's read-only copy of its bytes
([v2-store.md](v2-store.md#chunks-and-placing)), so a second checkout of the same project adds
only the blocks its own outputs differ in; a volume that cannot clone (or a build directory on
another volume than the store) fails the build, naming both volumes, and nothing is copied
instead. Each build reports the bytes it placed by clone, wrote and copied in its `blaze-work`
line (`placed`). `OUT_DIR`s are still written whole on restore and `doc` pages by the client.

The packages a build fetches (registry `.crate`s and git checkouts) are kept in `<root>/sources`,
laid out as cargo lays out its home (`registry/src/<registry dir>/<name>-<version>`,
`git/checkouts/<ident>/<commit>`, the whole commit where cargo takes a short one): each is
mounted there for the front end and written there whole at its fetch (into a directory beside
it, its files read-only, then renamed), because a build script runs in its package's directory,
and its sandbox is given that directory, as are the C compiler and the other programs it runs.
The last file written into each is `.blaze-ok`, the content id of its files: a copy without it
or with another id (edited in place, written by another fetch) is refused by name, never read
as the package. A write is refused anywhere but under `registry/src/` and `git/checkouts/` of
`<root>/sources`, and a lock file's package name that cargo refuses, or a git commit that is no
object id, is refused before any fetch. blaze keeps no packages in `$CARGO_HOME` (it still
reads `$CARGO_HOME/config.toml`, as cargo does): a machine with no `~/.cargo` builds the same.
A package whose copy was deleted is fetched again at the daemon's next start, and a write a
stopped daemon left half done (`.<name>.part-<pid>-<n>`) is removed then
(`crates/blaze-cargo/src/serve/fetch.rs`, `keep_on_disk` and `sweep_parts`).

`--target-dir`, `CARGO_TARGET_DIR` and `build.target-dir` still name the directory, as tools
and scripts pass them; `build.build-dir` still names where the intermediate files go. The paths
blaze prints (a test executable, `blaze doc`'s pages) are the real ones, and `cargo metadata`'s
`target_directory` and `build_directory` (as `crates/blaze-metadata` makes them) name the same
directory, so rust-analyzer and editors look there.

The daemon tells when its watch of a checkout has caught up with a request by flushing the
watch, so it writes no sync file into the checkout either (`crates/blaze-cargo/src/serve/heads.rs`).
On macOS the flush is a cookie in a directory of the watch's own, outside the checkout; on
Linux io_uring drains the watch's queue. On Windows the checkout's watch reads the volume's
NTFS USN journal (unprivileged, Windows 10 1709 or later), and a flush is the journal's own
position: the driver asks where the journal ends (`FSCTL_QUERY_USN_JOURNAL`) after the request
arrived and holds the request until the watch has told every record before it
(`crates/blaze-core/src/journal/barrier.rs`). A file a program still holds open and writes is
heard as it is first written and as it is closed, not at each write. A checkout on a network
share, on a volume with no USN journal (FAT, exFAT) or on one whose journal was never made
can't be watched: each request on it fails, naming the checkout and why (the last says how an
administrator makes the journal).

`blaze clean` is normally not needed: the daemon collects what is unused on its own, and works
harder the fuller the disk gets, so free space stays above its floor without anyone asking
([gc.md](gc.md#pacing-how-hard-it-works)). It stays as an escape hatch. It removes the
workspace's build directory (the one named by `--target-dir` or the
configuration, if one is), with `--dry-run` and cargo's `Removed` summary. `blaze clean --list`
prints every workspace's directory under blaze's root with its size, whether a build holds it,
when it was last used and its checkout, and `blaze clean --all` removes them all. A directory a
build holds is refused, naming it. Only a directory that is no symlink, is not the same directory as (by device and inode) and
does not hold the checkout, `$HOME`, `/` or the current directory, and carries a build directory's mark (blaze's
lease or record, or a `CACHEDIR.TAG` with its signature) is removed: renamed away and unlinked
while its lease is held ([gc.md](gc.md#commands)). `blaze cache gc` runs one collection now,
and `blaze cache gc --dry-run` shows each volume's free space, its levels and what would go
([gc.md](gc.md)).

The store is always on: one per daemon, in `$BLAZE_HOME/v2/store`, the user's alone (the
daemon refuses one another user owns or others may write to;
[v2/plan/lanes/store.md](v2/plan/lanes/store.md)).

## Pinning a release of blaze

A project pins blaze, not Rust: each release of blaze builds with the Rust it ships, and
`rust-toolchain` files are not read ([toolchain.md](toolchain.md)). `blaze pin` writes the
version of the blaze it is into the workspace's root `Cargo.toml` (`blaze pin 0.YYYYMMDD.N`
writes that release's, fetched and checked first, or `blaze pin canary` a channel's; blaze says
on every command while the pin is behind the newest release, and `blaze update` never moves
it: [install.md](install.md#updates)):

    [workspace.metadata.blaze]
    version = "0.20261027.0"

(the rest of the manifest is kept as it was; in a package with no `[workspace]` that adds
one, which makes it the root of its own workspace, as it is anyway when no other is above
it). From then on the `blaze` anyone runs in that workspace runs that
release, as Bazelisk runs the Bazel a `.bazelversion` names: when it is another release
itself, it runs `~/.blaze/versions/<version>/blaze` (`$BLAZE_HOME/versions` with `BLAZE_HOME`)
with the same arguments, and when that is not there it first downloads
`blaze-v<version>-<host>.tar.xz` from blaze's releases (`https://blaze-releases.ix.dev/v<version>/`,
[release.md](release.md#where-the-bytes-are); the `.tar.gz`, three times the
size, for a release from before the `.tar.xz`), in ranges eight at once, saying how far it has
come (on a terminal one line redrawn in place, off one a line every five seconds) and stopping
with an error naming the address after 30 s without a byte, checks it against every list of
sums the release publishes (`SHA256SUMS`, which every release has, and `B3SUMS`, BLAKE3, which
every release dated 2026-10-31 or later has too, and is refused without) and unpacks it there.
The sums come from the same release as the archive, so they catch a corrupt or cut-short
download, not a release whose assets someone replaced ([release.md](release.md) has the plan
for that). The network is used only then, once per release, over HTTPS only (a redirect to
plain HTTP is refused) and at most 1 GiB per asset. A pinned release that can't be fetched or
checked (offline, no such release, neither list, no build for this host, another hash in either
list) is an error, and no other blaze builds in its place.
`BLAZE_RELEASES` names a mirror laid out as
`https://blaze-releases.ix.dev` (`<mirror>/v<version>/<asset>`,
an `https://` address; `http://` is refused), or a directory of `v<version>` directories of
the assets. `--manifest-path` picks the workspace a command is for, as it does for cargo.

A build of blaze's source tree (`blaze 0.0.0-dev`, built without `BLAZE_RELEASE`) runs itself
in blaze's own tree, whatever that pins: that is how blaze's tree, which pins a release for
everyone else, builds itself. blaze's tree is any workspace whose root holds
`crates/blaze-engine/Cargo.toml` naming the package `blaze-engine`, wherever it is (a worktree,
a copy in a VM, the source of a Nix build, a fork). In any other workspace that pins a release
it hands off to that release as a release does, with one line on stderr naming its own
version, the pin and the manifest, and saying `--ignore-pin` runs it instead.
`--ignore-pin`, as the first word after `blaze` (`blaze --ignore-pin build`), runs the blaze it
is given to in place of whatever the workspace pins, a release or a build of the tree: how a
build of blaze is tried on a pinned project. Anywhere else it is the command's own word
(`blaze run --bin tool x --ignore-pin` passes it to `tool`). It applies to that one command's
workspace: blaze sets `BLAZE_IGNORE_PIN_FOR` to its root manifest for the processes under it, so
a blaze a test or build script runs there (`$CARGO`) runs itself too, and one run in another
workspace (a test's temporary one) follows that workspace's pin. Likewise `BLAZE_PINNED_BY`,
which the blaze that hands off sets for the release it runs, holds only in the workspace it
names.

`blaze --version` answers in any workspace and says which blaze runs and why: as the workspace
pins it, as a build of blaze's tree in that tree, as `--ignore-pin` says, or as nothing pins
one; where the pinned one runs in its place it first names it, and where the pinned one can't
run (it can't be fetched or its pin can't be read) it prints this blaze's version and why.

The pin may name a channel instead of a version ([release.md](release.md)):
`version = "stable"` runs the newest release that passed the heavy tests, and
`version = "canary"` the newest release. blaze reads which release the channel names from its
pointer (`<releases>/channels/<channel>`, one small download, through `BLAZE_RELEASES` too),
keeps it in `~/.blaze/channels/<channel>` and reads it again after an hour; that release is
then run as a pinned version is. When the pointer can't be read again once the hour has passed
(offline), it is an error naming the release it named when last read, which does not run in
its place; `blaze version-resolve <channel>` reads it again. A pin to a
version stays on that release, whose bytes never change: promotion from canary to stable flips
the same release.

`blaze.lock` records the pinned release and its nightly ([Lock files](#lock-files)); a pin
moved to another release is a lock change, refused under CI or `lock = "required"` until
`blaze lock`, run by the newly pinned release, records it.

## How far a constant's evaluation may go

Every evaluation of a constant, static, array length or other compile-time code has a budget
of steps (basic blocks run, as rustc counts them) and of memory (its stack and heap together),
1 000 000 000 steps and 1 GiB unless the workspace's root `Cargo.toml` says otherwise:

    [workspace.metadata.blaze]
    const-eval = { steps = 4_000_000_000, memory = 4_294_967_296 }

An evaluation that reaches either stops the build with what it ran out of, where it was and
the setting to raise, as in ``const eval of `TABLE` exceeded 1000000000 steps at
src/lib.rs:12:9 (raise `const-eval.steps` under `[workspace.metadata.blaze]`)``. rustc's own
limit is its `long_running_const_eval` lint, an error at 2 000 000 steps unless the lint is
allowed (then it only warns, and runs on); the default here is 500 times that, so the budget
ends a loop that never would rather than refusing code rustc takes.

## Environment variables

The client sends its whole environment with each request, and the daemon reads these from
it, for that request alone:

| Variable | Meaning | Default |
|---|---|---|
| `CI`, `TF_BUILD` | `CI` not empty, `false` or `0`, or `TF_BUILD` set: a build may not change `Cargo.lock` or `blaze.lock` (`--no-locked` lets it; [Lock files](#lock-files)) | unset: a build writes them and says so |
| `BLAZE_LOCK` | `write`: builds may change `Cargo.lock` and `blaze.lock` whatever `CI` or `lock = "required"` says, unless `--locked` or `--frozen` is typed (for harnesses of throwaway workspaces; [Lock files](#lock-files)); any other value is an error | unset |
| `BLAZE_SANDBOX` | `off` (or `0`) runs build scripts and proc macros, and tests under `--reached`, outside the sandbox ([above](#build-scripts-and-proc-macros-run-in-a-sandbox)) | sandboxed |
| `BLAZE_ELIDE` | `verify` also compiles and runs a build script blaze elided as inert, and checks it said what elision did; any other value is an error | unset: an inert script is not run |
| `BLAZE_PROC_MACRO_CACHE` | `check` expands again each expansion the store kept and compares; any other value is an error | unset: a kept expansion is used |
| `BLAZE_SCRIPT_CACHE` | `check` runs again each build script run the store kept and compares, failing the build where they differ; a result another checkout verified that differs is recorded as depending on where its roots are, so no checkout is given it again; any other value is an error | unset: a kept run is used |

These are read by the process itself, the client or the daemon (from the environment of
whichever command started it; to change one there, run `blaze stop` first):

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_HOME` | the daemon's socket and state directory, and blaze's toolchains and releases ([Files](#files), [Install](#install)) | see there |
| `BLAZE_RUST_DIST` | where the pinned nightly's components come from: a URL, or a directory of them ([Install](#install)) | rust-lang's dist server, at the pinned date |
| `BLAZE_RELEASES` | a mirror of blaze's releases ([below](#pinning-a-release-of-blaze)) | `https://blaze-releases.ix.dev` |
| `BLAZE_API_TOKEN` | the bearer token `blaze serve --http` requires ([Files](#files)) | none: `--http` refuses to start |
| `BLAZE_CORES` | how many cores blaze's work runs on and at what priority: unset, the machine's less 2 at background priority; `all`, every core at normal priority; `n`, exactly `n` at background priority. A command that sets a value giving another budget or priority than the running daemon's is refused ([Cores and priority](#cores-and-priority)) | unset |
| `BLAZE_EMIT` | LLVM instruction selection: `optimized` or `fast`. It is in no unit's key, so code the store kept under one value is used under the other | `optimized`; `fast` at level 0 |

`BLAZE_BACKEND`, `BLAZE_OPT` and `BLAZE_INLINE` are not settings in v2. They change the code
made without being part of a unit's key, so set in the daemon's environment they fail every
unit, naming the variable (`crates/hir-ty-codegen/v2/backend.rs`, `CHANGES_CODE`); the profile
picks the backend ([below](#backends)). v1's splice setting, its build-script runner's,
`BLAZE_TESTS`, `BLAZE_TESTS_FROM`, `BLAZE_TEST_ENV`, `BLAZE_IDLE_SECS`, `BLAZE_MEMORY_MAX`,
`BLAZE_MEMORY_BUDGET`, `BLAZE_SPARE` and `BLAZE_LRU` are read by nothing in v2.

## Backends

A build compiles with the backend its profile's `opt-level` asks for, as cargo resolves the
profile: the `[profile]` tables of the root manifest, the configuration's and `--config`'s over
them, `CARGO_PROFILE_<NAME>_OPT_LEVEL`, and `inherits`. A custom profile that inherits
`release` is built as `release` is, unless it sets its own `opt-level`.

| `opt-level` | Backend | Profiles that have it by default |
|---|---|---|
| `0` | copy-and-patch stencils | `dev`, `test` |
| `1` | copy-and-patch stencils: rustc's `-O1` has no LLVM pipeline blaze runs to match it | |
| `2` | LLVM at `-O2` | |
| `3` | LLVM at `-O3` | `release`, `bench` |
| `s`, `z` | LLVM at `-O2`: blaze's LLVM has no pipeline for size yet, so the code is not smaller than at `-O2` | |

The level is chosen per function's package, as cargo's is: `[profile.<name>.package.<spec>]`
(`"*"` for every package that is not a workspace member) and `build-override` for what build
scripts and proc macros run are read over the profile (`crates/blaze-codegen/src/plan.rs`).
Native code is never Cranelift's: stencils at 0 and 1 (a tail call, `become`, included), and a
target with no stencil library stops at those levels with an error naming it. Stencil code
built to keep counts nothing; only a tiered run's counts its calls. A proc macro's wasm module
is the one place Cranelift's code generator runs. `codegen-backend = "cranelift"` at 0 or 1
asks for the fast unoptimized code stencils are, and is taken; at 2 and up it is an error.
`codegen-backend = "stencil"` compiles every function of the build as stencils whatever its
level, the standard library's and what build scripts and proc macros run included. It is the
seed's setting (docs/future/bootstrap.md, "std and assembly in the seed"), works with or without
LLVM, and is taken for a whole build from the profile, never per package. Otherwise the
standard library's own functions are compiled as rustc's shipped std is, at `-O3`, and a blaze
built without LLVM refuses a profile that asks for LLVM, saying so and naming
`codegen-backend = "stencil"`, rather than building it with stencils.

Each function's code is kept by the backend and level that compiled it, which are part of its
spec's hash (`crates/blaze-codegen/src/settings.rs`): stencil code is never given to a build
with LLVM, nor `-O2`'s to `-O3`'s.

A build with LLVM takes longer than one with stencils. A first build takes the most longer,
because LLVM compiles every function then
([#807](https://github.com/indexable-inc/blaze/issues/807)).

## Known problems

- **The build's verdict is blaze's front end's, and it is not yet all of rustc's.** An error
  that blaze's front end or borrow checker finds fails the build, written as rustc writes it,
  with cargo's status; no rustc runs to say it again. What they do not find is built, and
  the borrow checker passes part of rustc's tests ([borrowck.md](borrowck.md)). Code rustc
  builds that blaze takes for an error fails the build too; that is a blaze bug to report.
- What blaze can't build stops the build with an error naming it: a staticlib or dylib, an
  artifact dependency, a function blaze can't compile ([What a build does](#what-a-build-does)).
- Which platforms v2 builds and links for, and how far each has been run, is
  [platforms.md](platforms.md).

## Plain `cargo`, switched on globally

Gone: `blaze enable` and `blaze disable`, and the wrapper they put behind plain cargo, are v1's
and v2 has neither. A plain `cargo build` is cargo's and rustc's alone, and `blaze` is the only
way in. v2 does not take a rustc wrapper either: a `RUSTC_WORKSPACE_WRAPPER` or
`build.rustc-workspace-wrapper` that is set (one v1's `enable` left in `$CARGO_HOME/config.toml`,
say) fails the build, naming it (`crates/blaze-units/src/load/plan.rs`); remove it by hand.

## macOS arm64

v2 builds `aarch64-apple-darwin` as it does Linux, linking with blaze-link; what is known of
it is in [platforms.md](platforms.md). The rest of this section is a record of v1 (as of
2026-10-27), which linked rustc's objects and spliced edits into them; neither the harness it
names nor the splice is in v2.

What was run under v1, on an 8-CPU Apple M4 virtual machine with macOS 26.5 and Rust 1.99.0:
`harness/blaze_cli.py --small`, under Cranelift and under LLVM (`BLAZE_OPT=2`,
`BLAZE_EMIT=fast`). Under each it passed: ripgrep, fd and hyperfine gave the same output,
exit status and test results as cargo alone on every step of both modes (134 comparisons),
and the 101 checks of the flags, the daemon and plain cargo held. Those include the
`enable`/`disable` round trip of `config.toml`, byte for byte, over five configurations, and
the second user at the socket, for which the harness uses `nobody` on macOS.

Seconds for the build after an edit, blaze / cargo alone, one run each (plain cargo's columns
are of `blaze enable`, gone since):

| Project | `blaze`, Cranelift | `blaze`, LLVM | plain cargo, Cranelift | plain cargo, LLVM |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 2.08 / 1.43 | 0.40 / 0.67 | 1.35 / 0.97 | 1.07 / 0.69 |
| fd 10.3.0 | 0.24 / 0.42 | 0.25 / 0.43 | 0.26 / 0.42 | 0.29 / 0.42 |
| hyperfine 1.19.0 | 0.34 / 0.35 | 0.21 / 0.33 | 0.24 / 0.34 | 0.23 / 0.34 |

The ripgrep numbers are not steady. Two earlier Cranelift runs of the same step on the same
machine gave 0.52 / 0.73 and 1.08 / 1.30 for `blaze`, and 1.86 / 0.78 and 1.30 / 1.30
for plain cargo; in every run the daemon did that build, and under plain cargo it was not
faster than cargo. Why ripgrep's edit takes one to two seconds in some runs was not looked
into. A build with no change took 0.03 to 0.06 s through `blaze` against 0.31 to
0.41 s, and 0.16 to 0.19 s through plain cargo.

Builds that went to cargo in those runs, each with its reason printed: the first build of
every project, the build of source that does not compile, ripgrep's tests after an edit
under `blaze` (a library the unit tests are linked with as rustc built it), and the
tests of ripgrep and hyperfine after an edit under plain cargo ("the build is not one the
daemon has loaded").

Not run on macOS: the projects of the harness other than those three, `blaze` with
debug info looked at in a debugger, and anything on x86-64.

## Every subcommand

Andrew, 2026-10-26: blaze never runs rustc or cargo for anything, and the commands that are no
builds "should go to us and not cargo". So every `blaze <command>` is blaze's own: no process of
cargo, rustc, rustdoc, clippy-driver or rustfmt is started for it, and blaze runs no cargo
plugin (`cargo-<name>` on the path).

v2's client has only the commands of [Commands](#commands) (`crates/blaze-cargo/src/cli.rs`);
any other fails with status 101 and does nothing:

    error: no such command: `tree`

    help: blaze's commands are build, check, run, test, bench, doc, clean, cache, fmt, serve, stop, status, toolchain, daemon, codegen-inputs, trace, tables, pack, explore, debug, explain, why, reach and slow

The table below is a record of what v1 had (as of 2026-10-30): the crates it names
(blaze-metadata, blaze-fmt, blaze-lints, ...) are still in the tree, and a command comes back
once the client asks the daemon for it. None of it runs in v2, except `fmt`, which is back as
[above](#commands).

| Command | blaze's | What it is, and what it waits for |
|---|---|---|
| `build` (`b`), `run` (`r`), `test` (`t`), `check` (`c`) | the build | [above](#commands) |
| `metadata` | yes | `crates/blaze-metadata`: the workspace read by blaze-manifest, every locked package fetched by blaze-registry, resolved by blaze-resolve; `--no-deps`, `--features`, `--all-features`, `--no-default-features`, one `--filter-platform` (its `cfg` as blaze answers `rustc --print cfg` itself), `--manifest-path`, `--offline`. Measured equal to the pinned nightly's cargo before the comparison harnesses were deleted (2026-10-30): ripgrep, fd, helix, nushell and biome printed the same JSON for each form, with `locate-project`, `verify-project`, `pkgid`, `read-manifest`, `fetch` and `clean` (130 of 130 cases over the five). A package's `hints` has cargo 1.101's `"min-opt-level"`, always `null`: blaze-manifest does not read a value of it yet. A registry package the lock file does not pin, the lock file missing or stale, is chosen from the registry's index as cargo chooses it (blaze-registry's `IndexRegistry`), and the lock file written as cargo writes it. Several `--filter-platform`s fail with the reason |
| `locate-project` | yes | `--workspace`, `--message-format plain\|json`, `--manifest-path` |
| `verify-project` | yes | `{"success":"true"}`, or `{"invalid":...}` and status 1 |
| `version`, `help` | yes | blaze's version and help |
| `fmt` | yes | `crates/blaze-fmt`: rustfmt and cargo-fmt at rust-lang/rust 12433c307 ported onto a syntax tree of blaze's own, filled by rust-analyzer's parser with rustc's spans, run in process. `blaze fmt [--check] [-p NAME]... [--all] [--manifest-path PATH] [-v\|-q] [-- RUSTFMT_ARGS]` formats the targets blaze-manifest reads; `blaze fmt a.rs b.rs` formats files, with rustfmt's options. Measured against stock rustfmt (the pinned nightly's) before the comparison harnesses were deleted (2026-10-30): ripgrep, nushell, fd, helix and biome with their own rustfmt.toml, 6068 of 6068 files byte-equal (100%), and `--check` exit codes and diffs equal; the same with `max_width = 60`, 6068 of 6068 (100%); rustfmt's `tests/source`, 773 of 793 (97.5%; 18 more use line ranges, which it does not run). At blaze-fmt f43e4a20af on dev-aws-14. A file is refused with the reason, and left as it is, when it has nightly syntax rust-analyzer does not parse (postfix `.yield`, `.use` and `.match`, `const` block items, `use` closures, `&pin`, unsafe binders, `unsafe mod`, `mut ref`, `default` on an associated `const fn`), a parenthesized trait object with `+` (`(Foo) + 'static`), a `macro` with a parameter list, an `impl` or `mut` restriction, or a typed `try` block; a non-empty `ignore` option is an error |
| `clippy` | yes | `crates/blaze-lints`: 24 of clippy's lints ported onto rust-analyzer's analysis, run by the daemon, written as `cargo clippy` writes them; [below](#lints) |
| `doc` (`d`) | yes | `crates/blaze-doc`: every page from rust-analyzer's HIR, no rustdoc; cargo's flags, `target/doc/<crate>/index.html`, search, source pages, themes. Measured against the pinned nightly's `cargo doc` before the comparison harnesses were deleted (2026-10-30), with their dependencies: fd 100.0% of rustdoc's pages, 99.9% of their members and impls, 99.4% of the resolved links; ripgrep 99.9%, 99.6%, 99.1%; nushell 99.5%, 99.2%, 98.4%; helix 98.9%, 99.0%, 98.7%; biome 99.0%, 99.2%, 98.1%. biome in 92.5 s against `cargo doc`'s 1028.0 s, nushell in 28.4 s against 497.1 s, its build scripts already run ([doc.md](doc.md)). The load still runs cargo for build scripts and proc macros, as every load does; doctests are `blaze test --doc`'s |
| `tree` | yes | `crates/blaze-tree`, cargo 1.101's `cargo tree` ported onto blaze-resolve: every flag (`-p`, `--workspace`, `--exclude`, `-e`, `-i`, `--prune`, `--depth`, `--prefix`, `--no-dedupe`, `-d`, `-f`, `--target`, `--charset`, `--color`, the feature flags). Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 26 forms over ripgrep, fd, helix, nushell and biome, 130 of 130 byte-equal. Package patterns with globs (`-p 'serde*'`) and `-i` with no value are refused |
| `pkgid` | yes | the package's full spec from `Cargo.lock`, with cargo's errors for none or several matches |
| `fetch` | yes | every package `Cargo.lock` names, downloaded by blaze-registry. `--target` fetches every platform's, and with no lock file it fails (choosing versions is #321) |
| `clean` | yes | the workspace's whole build directory under blaze's root ([Where a build writes](#where-a-build-writes)), or the one `--target-dir`, `CARGO_TARGET_DIR` or `build.target-dir` names, and `build.build-dir`'s; `--dry-run`, `--manifest-path` and cargo's `Removed` summary. `--list` prints every workspace's build directory with its size, `--all` removes them all. One profile's (`--release`, `--profile`), `--doc`, `-p`, `--workspace` and `--target` are refused (`crates/blaze-cargo/src/cli/clean.rs`) |
| `new`, `init` | yes | `crates/blaze-new`, cargo 1.101's `ops/cargo_new.rs`: the manifest, source file, ignore file and repository, the workspace's `members`, and cargo's checks of the name. The repository is made by `git init` (or `hg`, `pijul`, `fossil`, as cargo runs them). Cargo runs rustfmt on the file it writes; blaze does not, so a `rustfmt.toml` above the package that would change the template is not applied. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 50 of 50 cases, what each prints and every file it leaves |
| `remove` (`rm`) | yes | the dependencies out of the member's manifest, with `toml_edit` keeping its layout and comments, as cargo 1.99's `cargo_remove.rs` and `toml_mut` do: the features that named them, then the `[workspace.dependencies]`, `[profile.*.package]`, `[replace]` and `[patch]` entries nothing uses any more; then the lock file resolved again by blaze with every version it keeps locked as it was (blaze-metadata's `relock`), honouring `--locked`. A workspace with no lock file is refused: writing one chooses versions (#321). Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30), manifests and lock files byte for byte: 43 of 43 cases on ripgrep and a made-up workspace |
| `vendor` | yes | every registry package of the lock file unpacked from its `.crate` into the vendor directory as cargo 1.99's `ops/vendor.rs` does, with `.cargo-checksum.json`, `--versioned-dirs`, `--no-delete`, `-s`/`--sync` and `--respect-source-config`, and the `[source]` configuration printed; the workspace resolved again first, as cargo does (blaze-metadata's `vendored`). A git package is refused: cargo vendors it with its manifest as `cargo package` writes it, which blaze does not write yet. A workspace with no lock file is refused: writing one chooses versions (#321). Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30), every file by its sha256: 18 of 18 cases on fd, ripgrep and a made-up workspace |
| `add`, `update`, `generate-lockfile` | not yet | they choose versions, which blaze-resolve does not do yet ([#321](https://github.com/indexable-inc/blaze/issues/321)) |
| `bench`, `rustc`, `rustdoc`, `fix`, `install`, `package` | not yet | builds blaze does not do yet ([no-rustc.md](no-rustc.md)) |
| `search`, `owner`, `yank`, `login`, `logout` | yes | blaze-registry's `api.rs`, the `crates-io` crate 0.41's client of a registry's web API over blaze's own HTTP, and cargo 1.99's `ops/registry` around it: which registry (`--registry`, `--index`, `registry.default`, `source.*.replace-with`, the `Updating` of its `config.json`), the token (`--token`, `CARGO_REGISTRY_TOKEN`, `CARGO_REGISTRIES_<NAME>_TOKEN`, `credentials.toml` and the old `credentials`, read and written as cargo writes them, mode 0600), cargo's errors of the server (`errors`, a status not 200), and `search`'s table with `--limit` and its highlighted names. The credential providers of `registry.global-credential-providers` and a registry's `credential-provider` are asked in turn as cargo 1.101 asks them (`crates/blaze-registry/src/auth`): `cargo:token`, `cargo:token-from-stdout`, and credential processes spoken to in cargo's protocol, version 1, with `[credential-alias]`, each answer's caching and cargo's words for each failure. `cargo:wincred`, `cargo:macos-keychain` and `cargo:libsecret` on their own platform are refused with the provider named and what to set instead; on another platform they answer for no registry, as in cargo. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30) against a mock registry: 101 of 101 cases, what each prints, every request the server saw, the credentials files left and what each provider was asked |
| `publish`, `info` | not yet | `publish` packages the crate first, which is a build of `package`; `info` is not written yet |
| `uninstall` | yes | cargo 1.99's `cargo_uninstall.rs` and install tracker: removes the programs from the install root's `bin` (`--root`, `CARGO_INSTALL_ROOT`, `install.root`, cargo's home) and their entries in `.crates.toml` and `.crates2.json`, written byte for byte as cargo writes them; specs, `-p`, `--bin`, the package of the directory, cargo's `Summary` of several. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 18 of 18 cases, what each prints and the files it leaves |
| `read-manifest` | yes | the package's entry of `metadata --no-deps`, as cargo prints it |
| `report` | yes | `future-incompatibilities` (`future-incompat`): the report a build left in the build directory, with `--id`, `-p` and `--color`, as cargo 1.99's `future_incompat.rs` prints it. `timings`, `sessions` and `rebuilds` are unstable in cargo: refused as cargo refuses them without `-Z build-analysis`, and with it they fail saying blaze's builds do not write the logs they read. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 23 of 23 cases |
| `config` | yes | `config get`, by blaze-manifest's port of cargo 1.99's `cargo_config.rs`: `--format toml`, `json` and `json-value`, `--show-origin`, `--merged no`. Unstable in cargo, and refused as cargo refuses it unless `-Z unstable-options`, `unstable.unstable-options` or `CARGO_UNSTABLE_UNSTABLE_OPTIONS` allows it. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 41 of 41 cases |
| any other | no | `error: no such command`; blaze runs no cargo plugin |

### Lints

v2's `blaze check` runs every lint, rustc's, clippy's and blaze's (`blaze::`), at the levels
the manifests set: `crates/blaze-lints` runs over each crate of the workspace's packages beside
its check, with `--cfg clippy` and the `clippy.toml` clippy's search finds; a registry crate is
checked, not linted. `--fix` makes every machine-applicable suggestion of the check (with
`--lint NAME`, given any number of times, only those of the lints named), checks the result
again up to four times, and writes the files only when no check of it has an error the first
did not; `--fix` checks every target when none is named.

Once every unit ends, `blaze check` lints each workspace package's items over every
configuration it checked (the lib, the lib as a test, each bin, test, bench and example):
`blaze::dead` when none uses an item (in place of rustc's `dead_code`, which a linted crate's
own check leaves out), `blaze::test_only` when only the tests do (`--fix` adds `#[cfg(test)]`)
and `blaze::cfg_only` when only some features or platforms do (with the narrowest exact
`#[cfg]`). The group `blaze::reach` sets all three; they warn by default. None is given when a
unit failed. See [the plan](v2/plan/multiconfig-check.md).

`--baselines DIR` holds the run's `clippy::` and `blaze::` findings, counted by lint and crate
(the package of the unit that gave each; a finding a crate's lib and its lib test both give
counts once), to the baselines checked in under `DIR`: one file per lint,
`<tool>.<lint>.txt`, with one `crate<TAB>count` line per package that has findings, sorted, and
a `TARGET` file naming the target triple the counts are of: cfg'd code differs, so a run for
another target is refused. It needs `--workspace`, one `--target` and no target selection (each
package's default targets), since a crate or target the run did not check would read as at
zero.
The run fails when a crate's count is over its baseline, when a crate the baseline does not
have has findings, or when a lint has findings and no baseline; and it fails when a count is
under its baseline, saying to run the same command again with `--bless-baselines`, which lowers
the baselines to the run's (it never raises one or adds a crate) and deletes one that reaches
empty, whose lint then moves to `deny`. `--new-baseline LINT[,LINT...]` starts a lint's
baseline at the run's counts, refusing a lint with no findings or one that has a baseline. A
baseline holds a lint at `warn`: a `deny` lint's finding fails the check, baselined or not.

blaze's own baselines go in crates/blaze-lints/baselines, of `aarch64-unknown-linux-gnu`
(AGENTS.md rule fix-through-blaze-lints), and the merge gate's `lint-baselines` job holds every
change to them with the blaze the change builds, on Linux AArch64. Run the same command in an
arm64 Linux VM (`ix new` gives one by default) to check or bless them; another host's check
needs that target's C compiler and LLVM for the build scripts, so it is not the same run:

    blaze check --workspace --target aarch64-unknown-linux-gnu --baselines crates/blaze-lints/baselines

Tests are not counted yet: blaze's check of a test target fails for now ("can't find crate for
`test`"), and once it passes the scope grows to every target and the baselines are made again.
None is made yet: blaze cannot yet finish this check of its own workspace (see
[the lints plan](v2/plan/lanes/lints.md#baselines)), so `lint-baselines` fails until a run does
and the first baselines are made with `--new-baseline`.

A record of v1 (as of 2026-10-27). v1's `blaze clippy` linted with
blaze's own lints, `crates/blaze-lints`:
each a port of one of clippy's, with its file naming the clippy source it follows, keeping its
messages, spans, suggestions and conditions. The daemon runs them over the crates it loads as
`blaze check` would, and the client writes them as `cargo clippy` does: rustc's
rendering, the notes and the docs link, cargo's line ending each crate's warnings with its
`--fix` hint, `--message-format=json`, and status 0 for warnings, 101 for an error. Levels are
read as clippy reads them: `#[allow]`, `#[expect]`, `[lints.clippy]`, `clippy.toml`, `-- -W`,
`-A`, `-D`, `-F clippy::name` and `-D warnings`. A lint blaze has not ported is not run.

The lints, chosen as those stock clippy reported most on the bench apps (ripgrep, fd, helix,
nushell, biome) so that they cover 90% of what it reported there:
`bool_assert_comparison`, `byte_char_slices`, `clone_on_copy`, `collapsible_if`,
`crate_in_macro_def`, `empty_enums`, `get_first`, `io_other_error`, `map_clone`,
`map_unwrap_or`, `match_like_matches_macro`, `needless_bool`, `needless_borrow`,
`needless_borrows_for_generic_args`, `needless_return`, `new_without_default`,
`nonstandard_macro_braces`, `redundant_static_lifetimes`, `single_char_add_str`,
`unnecessary_map_or`, `upper_case_acronyms`, `useless_borrows_in_formatting`, `useless_format`,
`useless_vec`, and rustc's `unfulfilled_lint_expectations` for an `#[expect]` nothing met.

They were held to stock clippy by a harness, deleted on 2026-10-30 with the other comparisons
with rustc and cargo ([harness.md](harness.md)), that ran both with `--message-format=json` and
compared each diagnostic by lint, file, line and column, then its rendered text, and the exit
status, over the apps and a crate that exercised every lint. Run on dev-aws-14 on 2026-10-27, with `blaze clippy --workspace --all-targets` against the pinned nightly's
`cargo clippy --workspace --all-targets --message-format=json`; biome with its `*_analyze`
packages under `unsandboxed-build-scripts` (they write their lists of rules into their own
`src`, which blaze's sandbox refuses):

| App | Stock diagnostics | Of the lints ported | Matched | Missed | Extra | Rendered the same | Exit status, stock and blaze |
|---|---|---|---|---|---|---|---|
| ripgrep | 676 | 553 | 553 | 0 | 0 | 553 | 0 and 0 |
| fd | 14 | 14 | 14 | 0 | 0 | 14 | 0 and 0 |
| helix | 57 | 20 | 20 | 0 | 0 | 20 | 0 and 0 |
| biome | 971 | 969 | 0 | 969 | 0 | 0 | 0 and 101 |
| the lint fixture | 53 | 53 | 53 | 0 | 0 | 53 | 101 and 101 |

On ripgrep, fd and helix, 587 of stock's 747 diagnostics (78.6%) are written the same; the 160
missed are all of lints not ported, most of them `doc_lazy_continuation` (40),
`explicit_auto_deref` (11) and `derivable_impls` (11).

Over the five apps stock clippy wrote 1718 diagnostics (each counted once, by lint and place),
1556 of them (90.6%) of the lints ported. Its most frequent: `empty_enums` 540,
`nonstandard_macro_braces` 276, `bool_assert_comparison` 164,
`needless_borrows_for_generic_args` 145, `map_unwrap_or` 141, `needless_borrow` 43,
`redundant_static_lifetimes` 42, `useless_vec` 41, `doc_lazy_continuation` 40 (not ported: it
needs rustdoc's Markdown reading), `unnecessary_map_or` 30. The diagnostics are the same; what
cargo writes around them still differs in two ways: blaze writes no `Checking` line, and the
order the crates finish in, with the `(N duplicates)` count that follows from it, is blaze's.

What is not done:

- biome lints nothing: `blaze check` of it fails in `biome_analyze` and `biome_wasm`
  with errors stock rustc does not give ([#423](https://github.com/indexable-inc/blaze/issues/423):
  the trait of an impl a proc-macro derive writes is not in scope in its body), and the crates
  that depend on one that does not check are not linted, as cargo does not build them.
- nushell's stock run fails under `--workspace` (a compile error in `ethnum`), so it has no
  oracle.
- The deny-by-default (correctness) lints: none fired on the bench apps, and none is ported.
- `--fix` is refused: blaze does not apply its suggestions yet.
- Ported in part, each file saying what it leaves out: `needless_borrow` (a receiver, a callee
  or a field's base, and chains of `&` and `*`), `map_clone` (`Clone::clone` as a path),
  `needless_borrows_for_generic_args` (bounds through projections or other parameters).
