---
status: current
summary: Every blaze command and setting: cargo's commands and flags, what a build does, sandboxing, manifests.
---
# Using blaze

`blaze <command>` takes cargo's commands and flags (`blaze check`, `blaze test`, `blaze run`),
described here. Plain `cargo` never goes through blaze (the section at the end).

blaze had another name and another command before 2026-10-28; [rename.md](rename.md) maps them.

The daemon (`blaze serve`), its client and `blaze build` were deleted on 2026-11-15; the new
command line is in `crates/blaze-cli`. v2's
design is [v2/plan.md](v2/plan.md).

## Install

`blaze` is one program, also installed as `blaze-engine`. The [installer](../site/install.sh) and
`cargo install` both install it:

    curl -fsSL https://ix.dev/blaze | sh
    cargo +nightly-2026-10-27 install --locked blaze-engine

From the release after 0.1.0, `--profile dist` builds the profile a release is built with, less
the PGO a release adds (reports/RELEASE-BUILD.md); 0.1.0 has no such
profile and fails with it. From a checkout, the release's own recipe builds all of it with the
blaze on PATH (the stage 0 `.github/blaze-version` names), for this machine's target:

    uv run --frozen --no-dev python .github/scripts/build_release.py '{"target": "aarch64-apple-darwin", "out": "shipped"}'
    cp shipped/* ~/.cargo/bin/

That build has the LLVM backend (the release profile's, [below](#backends)) and the LLVM tiers, as the
installer's binaries do, linking the LLVM 23 blaze pins in statically ([which
LLVM](contributing.md#llvm)). blaze itself fetches that LLVM, by the sha256
`pins/llvm.json` pins, and hands it to llvm-sys's build script: the root
manifest declares it a [pinned native input](#pinned-native-inputs), so a plain
`blaze build -p blaze-engine --release` on a clean Mac or Linux machine needs no Nix and no
variable. A stage 0 from before that change does not read the table, and still needs
`LLVM_SYS_231_PREFIX` set to that LLVM, as `cargo install` from crates.io does. `blaze build -p
blaze-engine --release` is the quicker build the harnesses and CI use while working on blaze;
it is not what ships.

`blaze` builds with the one toolchain blaze compiles, the dated nightly in
`pins/toolchain` (`nightly-2026-10-27`): the language and standard library are the ones this
blaze ships. It ignores `rust-toolchain`/`rust-toolchain.toml`, `rustup override`,
`+toolchain`, `RUSTUP_TOOLCHAIN`, rustup's default and `RUSTC`, and never runs or asks a rustc
to learn anything ([toolchain.md](toolchain.md)). The nightly builds
what older releases built, so a project on a stable release or an older nightly builds as it
is, `#![feature(...)]` included. A project that needs another Rust pins another blaze. Plain
`cargo` still builds with the project's toolchain, in its own `target/` in the checkout:
blaze's build directory is elsewhere.

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
`pins/rust.txt`), and a file with other bytes is refused. Concurrent
builds download it once; a download that stopped goes on from where it stopped the next time.
A build script is given blaze itself as `CARGO`, and as `RUSTC` and `RUSTDOC` links to blaze
in the daemon's tools directory, which answer what a script asks of rustc with no rustc run
(`crates/blaze-build-script`).
v1's `blaze toolchain`, which said which Rust
blaze builds with and fetched its copy, is gone; v2 has no such command (`blaze --version`
names blaze's release and commit, and why it is the one that runs), and fetches the copy when
a build first needs it.

## Commands

    blaze check [cargo's flags]                                       (c)
    blaze run   [cargo's flags] [-- program arguments]                (r)
    blaze test  [cargo's flags] [TESTNAME] [-- test arguments]        (t)
    blaze bench [cargo's flags] [BENCHNAME] [-- bench arguments]
    blaze doc   [cargo's flags]                                       (d)
    blaze fmt   [--check] [-p NAME]... [--all] [--manifest-path PATH] [-v|-q] [-- RUSTFMT_ARGS]
    blaze fmt   [RUSTFMT_ARGS] FILE.rs...
    blaze new   [--bin|--lib] [--name NAME] [--edition YEAR|blaze-2026.11] [--rust] [--bstd PATH] [--vcs VCS] [--registry NAME] PATH
    blaze init  [--bin|--lib] [--name NAME] [--edition YEAR|blaze-2026.11] [--rust] [--bstd PATH] [--vcs VCS] [--registry NAME] [PATH]
    blaze tree  [cargo tree's flags]
    blaze metadata [--format-version 1] [--no-deps] [--filter-platform TRIPLE] [the feature flags] [--manifest-path PATH]
    blaze toolchain [--json]
    blaze pin
    blaze agents install|remove [claude|codex]... [--project|--user] [--strict] [--dry-run]
    blaze agents status [--json]
    blaze agents hook claude|codex vN [--strict]
    blaze doctor [--json]
    blaze prove [-p PKG]... [--workspace] [--emit] [--subset] [--json]
    blaze --version

`blaze help <command>` lists the flags a command takes: cargo's, read as cargo's clap reads them
(`crates/blaze-cli`), with clap's errors and exit code 1 for a command line
cargo would refuse. `-C DIRECTORY` (with `-Z unstable-options`) and the global flags before
the command work as in cargo. `test` alone takes four flags of blaze's own
([below](#which-tests-run)): `--run-all`, `--ub`, `--ub-seed N` and `--ub-schedules K`
(`--reached`, which once opted into selection, is refused naming the default and `--run-all`); `test`
and `bench` take `--no-tests fail|pass`
([what a run of no test is](#a-run-that-ran-no-test-is-refused)). `build`,
`check`, `run`, `test` and `bench` take `--refused N|all`: how many of the functions blaze could
not compile a build's error names, the first `N` (16 without it) or every one. `build`,
`run`, `test` and `check` take `--goal edit|max`: what the build waits for before it answers.
`edit` answers once every function has tier 0 code (copy-and-patch stencils); `max` once every
function has its optimized code (LLVM's), and fails, naming the function, where it cannot.
Without the flag the profile says: `dev` and `test` edit, `release` and `bench` (and any
profile at `opt-level` 2, 3, `s` or `z`) are `max`. A package cargo gives its own `opt-level`
(`[profile.*.package.*]`) keeps it under both. The build's last line says where the linked
code stands: `tier 0 · 38% optimized` ([v2/plan/design/tiers.md](v2/plan/design/tiers.md),
[terminal.md](terminal.md)).
`pin` is [below](#pinning-a-release-of-blaze). Any other command fails with `error: no such
command` and status 101; none is passed to cargo, and no cargo plugin is run
([Every subcommand](#every-subcommand)).

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

The commands are being rebuilt with no daemon (docs/v2/plan/design/bottom.md, "The commands"):
each is a process that answers and exits; `run` and `test` write no executable, `check` makes
no code, and there is no `blaze build`. The rest of this page describes the parked command line
(`blaze-cargo`, `docs/v2/plan/parked.md`) where it is not marked as gone.

## What a build does

blaze never runs rustc, and a build has one path ([no-rustc.md](no-rustc.md)). The daemon
reads the command line as cargo does, into cargo's unit graph (`crates/blaze-units`: the
packages, features, profiles, targets and build-script runs cargo would have), and builds
each unit itself: every crate is compiled by blaze over blaze's standard library, a
function at a time ([v2/plan/design/model.md](v2/plan.md)), and each program is
linked (`crates/blaze-native`) where cargo puts it in the build directory
(`debug/<name>`, `release/<name>`, `examples/` for an example). No `cargo build` runs. A function the
program can reach that blaze cannot compile fails its unit, naming the function and why;
nothing is handed to cargo or rustc.

What a unit makes (the deleted daemon's codegen.rs, `makes`): a binary, an
example, a test or a build script is an executable; a `cdylib` (a native Node addon, a Python
extension, a plugin) or a proc macro is a shared library, at cargo's path
(`target/debug/lib<name>.so`, `.dylib` on macOS), linked with the package's
`cargo:rustc-link-arg` and `cargo:rustc-cdylib-link-arg`; a library (`lib`, `rlib`) is written
with no object code, its functions kept in the store for the programs that link it. A crate
type of `staticlib` or `dylib` fails the build, naming the target: blaze's linker writes
neither yet ([#262](https://github.com/indexable-inc/blaze/issues/262)). So does what else
blaze's load does not build, saying what: an artifact dependency (`-Z bindeps`), a
`metabuild`, a target spec file, a target blaze has no data of, and a rustc `-Z` flag in
`RUSTFLAGS` that `crates/blaze-unit-graph` refuses.

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
result and its record (the deleted daemon's compile.rs, `open`); an item
declared inside a body's block counts as the body's. At the end of a build that failed, blaze
says on stderr how many units it left unchecked and behind which failure, and which units an
open failure let through; under `--message-format json` the same comes as one
`{"reason":"blaze-unchecked",...}` line on stdout (`Response::Unchecked` in `blaze-api`).

The build is the daemon's, and the daemon keeps what it built: its database and its store.
The next build compiles only the functions an edit changed, and links each program again
against an image it kept of the last link at that path, so the link writes only what changed
(the deleted daemon's codegen.rs, `Linked`). Build scripts and proc macros
are compiled by blaze too, as units of the same graph ([below](#build-scripts-and-proc-macros-run-in-a-sandbox)).

The standard library is built to unwind, whatever the profile's `panic`, as the toolchain's
sysroot is; only the proc macros' and a target's that only aborts are built to abort
(`crates/blaze-unit-graph`). So a `panic = "abort"` program links it
with `panic_abort`, and its tests, benches and build scripts, which unwind under any profile as
cargo builds them, link the same std. Under an explicit `-Zbuild-std` that builds `std`, with
`panic = "abort"`, cargo builds std to abort; blaze builds it to unwind there too, so the
build's tests link the same std. That costs the program std's landing pads and unwind tables
in size and changes nothing it does. A `-Zbuild-std` with neither `std` nor `panic_unwind` (`core`,
`core,alloc`) is built with the profile's `panic`, as cargo builds it: nothing in it defines
`rust_eh_personality`, which a `core` built to unwind would need.

Linking checks most of what rustc's `dependency_format` checks (`crates/blaze-code`): one panic runtime, built with the program's strategy, and no
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
(the deleted daemon's harness.rs). An integration test or a benchmark is told
`CARGO_BIN_EXE_<name>` of its package's binaries and `CARGO_TARGET_TMPDIR`, as cargo tells
them.

`run` then becomes the program and `test` runs the test executables the client was told of, as
cargo's `ops::run_tests` does (`crates/blaze-cli/src/test_run.rs`): in cargo's order, on the
terminal, with the variables and working directory cargo gives, cargo's `Running` lines, then
the doc tests (on a terminal that redraws, `test` runs them under the screen instead:
[terminal.md](terminal.md)); `--no-run` lists them. By default blaze builds every unit it can, runs every
test executable and doc test that built whichever failed, and ends listing the targets that
failed (with what reruns each), then each test executable or doc test that never built and the
failure that held it back: its own compile's, or the unit it was skipped behind
(`blaze_api::Unchecked::tests`). The exit status is then 101. `--fail-fast` stops at the first
test executable that fails, with its exit code, as cargo does without `--no-fail-fast`.
`blaze test`'s tests run in the daemon, which reruns only a test whose executed code or reads
changed and reports the rest cached; `--run-all` runs every one, and `--ub` runs them in
blaze's evaluator ([below](#which-tests-run)). Doc tests, `--no-run` and `blaze bench` run as
above. Doc tests are blaze's own build of each example (below).

### Lock files

A build resolves the workspace against `Cargo.lock` and may change two files at the root:
`Cargo.lock`, cargo's own (byte for byte what cargo writes, so cargo and its tools read it), and
`blaze.lock`, which pins what blaze builds with besides the crates (below). Which a build may do
(`crates/blaze-manifest`):

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
they start their children (`harness/smoke.py`, the repro
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
  (`pins/toolchain`), recorded when that release runs it. An unpinned workspace, or one pinned
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
- `[git."<url>"]`, `<commit> = "blake3:<hex>"`: the digest of each git source's tree at the
  commit `Cargo.lock` locks. A commit id is SHA-1 and pins nothing, so a git dependency with
  no row here is refused (rule `weak_source_pin`), and a fetched tree that is not its digest
  is refused before it is mounted. `blaze audit pin` records them on trust at first use; review
  the diff. `blaze lock` keeps them as they are.
- `[advisories] tree` (with `url` and `commit` when fetched): the digest of the RustSec
  database `blaze audit` reads, checked before it is parsed
  (docs/v2/plan/design/audit.md).

A pinned input that disagrees with `blaze.lock` (a pin file edited, another blaze release) is a
lock change like a moved package: refused where changes are refused, written and said elsewhere.
A `blaze.lock` that would pin nothing is not written. Unknown keys and other format versions are
errors naming the file. Why a second file and not `Cargo.lock`'s `[metadata]`: that table is a
flat map of strings cargo copies through `cargo update` untouched, so a pin there would go stale
without a word, and anything blaze adds to `Cargo.lock` stops it being what cargo writes. One
file holds every pin that is not a crate (docs/v2/plan/design/lock-policy.md).

### Link arguments

blaze links every program with its own linker, so a link argument reaches no `cc`, ld64, GNU
ld or lld. `-C link-arg=ARG` and `-C link-args=ARGS` in the rustflags (`RUSTFLAGS`,
`CARGO_ENCODED_RUSTFLAGS`, `build.rustflags` and `target.<triple>.rustflags` in
`.cargo/config.toml`, a profile's `rustflags`) and a build script's `cargo:rustc-link-arg*`
(and `rustc-cdylib-link-arg`) are read by `crates/blaze-native`: the C driver's layer
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
| an executable's dynamic symbols | | `--export-dynamic-symbol PATTERN`, `--export-dynamic-symbol=PATTERN`: each name the image defines that the pattern matches (`*`, `?`, `[a-z]`, as `fnmatch`) is in `.dynsym` and kept for it; nothing for a shared library, which exports what its crate exports |

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

A crate's bundled static libraries (`static` without `-bundle`, from its rustflags' `-l`s, its
build script's `cargo:rustc-link-lib`s or its `#[link]`s) are found first in the directories its
own compile searches, the `-L`s rustc is given for that crate: its rustflags', then the build
scripts' of it and of what it depends on, its own `OUT_DIR` first; then the linked program's
directories and the toolchain's are searched. So two crates whose build scripts each make a
static library of one name in their own `OUT_DIR` (two copies of one `-sys` crate) link two
libraries, each its own crate's, as rustc bundles each into the crate that names it. Every
other kind (a dylib, a framework, a `-bundle` static library) is found as the final link finds
it, in the program's directories (`crates/blaze-native`). The linked
crates' rustflags' `-L`s are in the program's key.

The rest of the rustflags that are the link's, rather than the code's, are read by
`crates/blaze-native`, and none is dropped:

| Flag | What blaze does |
|---|---|
| `-l [KIND[:MODIFIERS]=]NAME` | linked as a build script's `cargo:rustc-link-lib` is, in the place of the crate whose rustflags give it |
| `-L [KIND=]DIR` | searched as a build script's `cargo:rustc-link-search` is, before the build scripts' directories; `crate=` and `dependency=` find crates, which blaze finds by the unit graph |
| `-C rpath` | a macOS shared library is named `@rpath/FILE`, as rustc names it; rustc's other rpaths are to Rust dylibs, and blaze links every Rust crate into the image |
| `-C relro-level` | `full` is what blaze's linker always does on ELF; `partial`, `off` and `none` are refused there, and on macOS every value but `none` |
| `-C link-dead-code` | off is taken; on is refused: blaze's linker always drops unreferenced sections |
| `-C strip` | `none` is taken, `debuginfo` links the image without debug info, `symbols` is refused: blaze always writes the functions' local symbols |
| `split-debuginfo` (the profile's) | **differs from cargo**: unset, `packed` and `unpacked` all put the debug info of an image blaze links in one file beside it, on Linux too (cargo's default there is `off`): `NAME.debug`, named by the executable's `.gnu_debuglink` and carrying its build id, or on macOS `NAME.dSYM`, found by `LC_UUID`. The executable keeps its code, data, unwind tables, symbol table and build id, so a panic's backtrace and a debugger show what they showed. `off` puts the debug info inside the executable, as before. Windows and wasm images are as before. The side file is written beside the executable by the link that made it, and is not in the store ([debug-side-file.md](v2/plan/design/debug-side-file.md)) |
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

### Sandboxed tests

Whether a test the daemon runs (`blaze test`, [below](#which-tests-run)) is sandboxed by
default depends on the host, decided in one place (`crates/blaze-wasi`):

| host | a test is sandboxed |
|---|---|
| Linux | always, with nothing to opt into, unless its package takes it out ([below](#a-package-takes-its-tests-out)) or the command says `BLAZE_SANDBOX=off` |
| macOS | only where its package opts in (`tests = true` in its `[package.metadata.blaze.sandbox]`) or the workspace does (`experimental = ["sandboxed-tests"]`), and then the same way out applies |

The two differ because of a known hole on macOS:
a sandboxed test there can read another test binary's launch token. The difference goes when
the channel to the daemon is a descriptor the test inherits, with no token in any environment.

The sandbox is what lets blaze keep a pass and rerun a test only when something it executed or
read changed: it records what each test reads, and a test outside it can read anything, so
nothing can say when it must run again. A test that is not sandboxed is never kept as passed.
Build scripts run in the sandbox on every host
([below](#build-scripts-and-proc-macros-run-in-a-sandbox)), and proc macros run in blaze's own
process, as WebAssembly modules.

`experimental` in `[workspace.metadata.blaze]` lists, by name, the experiments a workspace
opts into: `const-macros` (proc macros written as functions of a file of the package, run by
blaze's const evaluator: [future/const-macros.md](future/const-macros.md)) and
`sandboxed-tests` (every member's tests sandboxed, on macOS). Any other name, or a value that is
not a list of names, is an error that names the manifest and the names known
(`crates/blaze-manifest`). On Linux, where tests are sandboxed anyway,
`sandboxed-tests` does nothing, and every build says so in a note; delete it from the list once
no macOS machine builds the workspace.

Each test binary the daemon runs starts once in a sandbox of its
own, tracing what it reads, as a fork server, and each test runs in a child forked from it
(`crates/blaze-test-reach`). In it the machine's files are read-only; the workspace, the target directory and
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

#### A package takes its tests out

A member of the workspace takes its tests out of the sandbox in its own manifest:

```toml
[package.metadata.blaze.sandbox]
tests = false                                 # every test of the package runs outside
unsandboxed-tests = ["tests::writes_home"]    # or these, by libtest's names
```

On Linux `tests = true` is the default said out loud; on macOS it is the opt-in that sandboxes
the package's tests.
Any other key there is an error, and a
dependency's own table is not read. The tests named run from a second server of the binary,
started outside, after every sandboxed test (the deleted daemon's test.rs).
`BLAZE_SANDBOX=off` in the command's environment runs every test and build script of the
command outside, as cargo does. A test outside the sandbox runs every time: what it read is not
known, so its pass is never kept. Its line says so (`(not kept: not sandboxed)`), and the run
says, in one note before its results, how many tests run outside and the setting that put them
there:

```text
note: 3 tests run outside the sandbox, so they run every time: only a sandboxed run, which records what it reads, is kept as passed: 3 of `sel`, whose `[package.metadata.blaze.sandbox]` sets `tests = false`
```

On macOS a package that does not opt in has every line `(not kept: not sandboxed)`, every run,
and the note says the one thing to do and what it carries:

```text
note: 3 tests run outside the sandbox, so they run every time: only a sandboxed run, which records what it reads, is kept as passed: 3 of `sel`, which does not opt in. On macOS tests run outside the sandbox unless their package opts in: `tests = true` under its `[package.metadata.blaze.sandbox]` runs them sandboxed and keeps their passes, though a sandboxed test on macOS can still read the arguments and environment of the user's other processes, another test binary's launch token among them (docs/cargo.md, "Sandboxed tests")
```

Where the sandbox can't be made (or fails) for a test binary, its tests fail and `blaze test`
stops there: no other test starts, it says once, first, how to run the tests anyway
(`BLAZE_SANDBOX=off`, at the cost that nothing is kept and every test runs every time) and then
why the sandbox failed, and it exits 101. It does not go on outside with a warning: a run that
did would break, unseen, the promise that a test can't change the machine or reach the
network, and a warning in a long log is not seen. `BLAZE_SANDBOX=off` is the explicit way to
run outside.

### Build scripts and proc macros run in a sandbox

Every build script, the workspace's and its dependencies', is compiled by blaze and runs in
the sandbox tests run in ([#147](https://github.com/indexable-inc/blaze/issues/147)), as a
spawn of the driver through blaze's sandbox (`crates/blaze-wasi`); the deleted daemon's script.rs is the
one place a run is made. A script is given the command's whole environment with what cargo
sets for scripts over it (`CARGO_*`, `DEP_*`, `OUT_DIR`, `TARGET`, `PROFILE`, ...), as cargo
gives it (`script.rs`, `inherited`). The variables are the command's, not those of whoever
started the daemon: `SDK_DIR=/opt/sdk blaze build` gives the script `SDK_DIR` with a daemon
started without it. What the run read, its files and its variables, the sandbox traces, and the run is kept in
the store by its code, what it was given and what it read (`blaze_usercode::run`), so a
command whose inputs to it are as they were finds it there and runs nothing. On Linux and
macOS the variables passed through from the command (all but what cargo sets) are lent to the
script rather than put in its environment (`crates/blaze-build-script`): it sees each when it reads it by name, and that read
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
only constants is neither compiled nor run (`crates/blaze-build-script`).
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
unsandboxed-build-scripts = ["libghostty-vt-sys", "flecs_ecs_sys"]   # by package name
build-script-link-dirs = ["/opt/vendor/lib", "${OPENSSL_DIR}/lib"]   # rustc-link-search may name
build-script-env = ["OPENSSL_DIR"]                                   # a ${NAME} above must be listed here
```

An entry that starts `${NAME}` is under the variable's value in the environment the script runs
in, for a library whose place differs by machine; `build-script-env` must list `NAME`, and a
script run where it is unset, or is no absolute path, fails naming it.

The table may also have `unsandboxed-proc-macros`, `build-script-env` and `wasi-build-scripts`,
each a list of names, which v1 read: v2 checks that each is a list of strings and does nothing
else with it beyond naming the variables `build-script-link-dirs` may use
(`crates/blaze-manifest`). Proc macros run in blaze's own
process as WebAssembly modules, confined by the host, which is their sandbox and has no way out
([below](#proc-macros-run-as-webassembly)), and a script is given the command's whole
environment already.

What a sandboxed script prints is checked before it is used (`crates/blaze-build-script`), since
it goes into every rustc and linker run of its package and of what links it. A
`rustc-link-search` (or `rustc-flags`' `-L`) names a directory in its `OUT_DIR`, its package,
the target directory's build directory (its dependencies' `OUT_DIR`s), a system directory
(`/usr/lib`, `/lib64`, `/usr/local/lib`, the Nix store, Homebrew's, Apple's SDKs), one of
`build-script-link-dirs`, or a layout of a [pinned native input](#pinned-native-inputs) in
blaze's own store (`<BLAZE_HOME>/native/sha256-<digest>`), whether the script was given it or
a variable of the shell points it there; a `rustc-link-arg` is one of a short list (`-pthread`,
`-rdynamic`, `-Wl,--as-needed`, `-Wl,-z,now`, `-lNAME`, ...), a prefix with one value that is
no further linker word (`-Wl,-soname,NAME`, `-Wl,--export-dynamic-symbol,PATTERN`,
`-Wl,--version-script=FILE`), or an rpath to `$ORIGIN` or a directory a search may name; a `rustc-env` sets no variable cargo or rustc gives the crate
itself (`CARGO_PKG_*`, `CARGO_MANIFEST_DIR`, `OUT_DIR`, `RUSTC*`, ...); what cargo gives only
build scripts (`CARGO_CFG_*`, `CARGO_FEATURE_*`, `CARGO_ENCODED_RUSTFLAGS`) it may forward, as
rav1e's does. Anything
else fails the build, naming the script and each line. Over the bench corpus's 587 runs the
rules refuse none (counted 2026-10-27).

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
refused. A sandboxed test's overlays need a mount namespace, so `blaze test` on Linux,
where tests are sandboxed by default, still needs user namespaces: where they can't be made, its tests fail saying
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
code. Its run is a run of user code (`crates/blaze-build-script`): found in the store
when one is there for its inputs as they are now, else spawned in a sandbox that traces, its
result kept under its code, what it was given and exactly what it read, so `rerun-if-*` is a
hint and the record is what counts. What is kept is its exit, what it printed and what it left
in `OUT_DIR`, which a reused run writes back as it was. A run that failed is not kept, and
runs again on the next build, as cargo reruns it. A run outside the sandbox
(`unsandboxed-build-scripts`, `BLAZE_SANDBOX=off`) is never kept. The design is
[v2/plan/lanes/usercode/scripts.md](v2/plan.md).

A build script's sandbox costs one launch (8 ms on Linux, 38 ms on macOS, measured on
2026-10-26), which is lost in a cold build's noise.

#### Pinned native inputs

A build script that needs a prebuilt native library (llvm-sys needs LLVM) is given it by the
workspace's root manifest, pinned by its digest, in place of a variable each machine sets:

```toml
[workspace.metadata.blaze.native.llvm]
pin = "pins/llvm.json"     # the pin file, relative to the root manifest
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
pin file and the package (`crates/blaze-manifest`). `env` may not be a variable
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
(the deleted daemon's native.rs): one layout task a digest, shared by every job
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

Every build runs its proc macros as `wasm32-wasip1` modules under Wasmtime, in the engine's own
process (`crates/blaze-proc-macro`;
[v2/plan/lanes/usercode/macros.md](v2/plan/design/proc-macros.md)). blaze builds each
proc-macro crate, and every crate it depends on, for `wasm32-wasip1` with its own front end and
code generator, against the real std and `library/proc_macro` (`crates/blaze-unit-graph`): nothing rustc compiles is loaded and no process is started.
The crate's root gets the module `__blaze_proc_macros` (its list of macros and the bridge's
glue, `crates/blaze-proc-macro`), and it is linked as a
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
of std units. A std's directory is named by its key without the crates asked of it
(`StdKey::placement`): a program's build and its tests' build, which asks for `test` too, name
every std unit they share by the same files, and the second compiles only what it adds. Its
own units abort too: a profile's `panic = "unwind"`, cargo's default, gets no `-C panic` from
cargo, so blaze builds each unit with its target's own strategy, as rustc does
(`crates/blaze-unit-graph`). The standard library the macros link is their own
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

Each expansion is kept in the store (`crates/blaze-proc-macro`) by the macro's
library, its name, input and attribute tokens, the crate's variables, its directory and the
toolchain, and found by what else it read.

blaze's constant evaluator (`crates/blaze-consteval`,
[v2/plan/design/compile-time.md](v2/plan/design/compile-time.md), change 20) runs compile-time
evaluation, while proc macros run as WebAssembly modules (`crates/blaze-proc-macro`). A
build never runs a macro on it: a proc macro that reaches a build built for the host is an
error naming it (the deleted daemon's compile.rs, `macros`).

v1 could compile a build script for `wasm32-wasip1` and run it under Wasmtime (`wasi-compare`,
`wasi-build-scripts`), and could expand with rust-analyzer's out-of-process proc-macro server.
v2 does neither, and their crates are deleted: every build script runs natively in the
sandbox. Nothing in blaze reads v1's variables or manifest lists for them, so setting them
changes nothing. The comparison of the two ways of running scripts is
reports/BSWASI.md.

### Which tests run

`blaze test` reruns a test only when something it executed or read changed. The daemon runs
the tests and sends each result, which the client prints in libtest's format. Each test runs
in a process of its own, forked from its binary's fork server, on one queue over every binary
and every core (`crates/blaze-cli/src/test_run.rs`). blaze-test-reach's machine
(`crates/blaze-test-reach`, [v2/plan/design/test-selection.md](v2/plan/design/test-selection.md))
decides which run (the deleted daemon's test.rs performs what it asks). A pass
is kept in the store under two things: the functions the test executed, each with its bound
code (a byte per function slot that the runtime writes on the slot's first call), and what it
read, in the sandbox. When none of those changed, the test is not run. Its kept pass stands,
and the client marks it cached with the time its kept run took:

```text
test parse::empty ... ok <0.012s> (cached)
test result: ok. 41 passed (38 cached); 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.21s
```

A pass is kept only when rerunning it now would give the same result, so a test that passed
and is not kept says why, and runs again next time:

- `(not kept: not sandboxed)`: it ran outside the sandbox, which only `BLAZE_SANDBOX=off`,
  its package's manifest ([above](#a-package-takes-its-tests-out)) or, on macOS, the lack of an
  opt-in ([above](#sandboxed-tests)) does. The run's one note
  counts these, with the setting that put them there; it does not list them. A test outside
  never stands on a pass kept of it inside: it runs every time, whichever setting put it there.
- `(not kept: the sandbox did not say what it read)`: the trace is missing.
- `(not kept: it reaches code the tracer cannot see (a process start, the network or assembly))`:
  codegen marks it as reaching such code.
- `(not kept: it read the wall clock)`, `(not kept: it read randomness the sandbox did not answer)`,
  `(not kept: it used the network)` and `(not kept: it read through a descriptor the sandbox did not see opened)`.
- `(not kept: what it read changed while it ran)`.
- `(not kept: /w/data.txt changed after the test began, so its hash need not be what the test
  read)`, in JSON `"kept": {"no": "moved"}`: what a test read is hashed once it ends, so a
  file, a directory above it or a link on the way to it whose `stat` says it changed since a
  moment before the run began (git's racy window, two seconds) leaves the hash unproven
  (`crates/blaze-test-reach`). The line names the first such
  read and, when the name whose stamp moved is another, that one and how it stands to the
  read: `/w, above /w/data.txt which it read, changed ...`, `/w/l, a link on the way to
  /w/l/f which it read, changed ...`, or `/x/f, reached through a link from /w/l/f which it
  read, changed ...`; a read whose names can't be resolved (a relative path, one with `..`)
  is said as such. This includes a file the test wrote itself and read back. A build script
  run kept under no key for the same reason says it in the same words. A test's own
  executable is never this: the build that placed it is the one writer of its path, and a
  dev test binary starts from that file as placed, with the key the job's record holds for
  it; blaze's code-memory host writes no file, and a launch whose path holds another key
  is refused, naming the path and both keys.

A failure is never kept: it runs every time. Doc tests are neither selected nor kept: blaze
does not trace a doc test's run, so they run every time, after the daemon's tests, and the
client says so in a note. A suite's index head is held in the daemon's
memory. After a restart each test is looked up by what it executed and read, which gives the
same answer and costs a lookup per test.

`--run-all` runs every test asked and still keeps what each run learns. `--reached`, the flag
that once opted into selection, is refused, naming the default and `--run-all`.

With a JSON `--message-format` each result is a `blaze-test-result` line on stdout instead of
libtest's text, as `crates/blaze-cli` writes it. Its fields
are `name`, `binary`, `executable`, `outcome`, `millis`, `outside` and `output`, then:

- `cached`: null for a test that ran. For a cached test it holds the kept run's `basis`
  (`executed` or `reached`), its `why`, its `millis` and its `outcome`, and the result itself
  has outcome `passed` and `millis` 0.
- `kept`: only for a pass that is not kept, as `{"no": "clock"}`. Its word is one of
  `unsandboxed`, `untraced`, `foreign`, `clock`, `random`, `network`, `descriptor`, `raced`,
  `moved` or `too-many-shapes`.
- `ran`: always present. It is null for a test that did not run (cached, ignored) or that
  selection did not judge. For a test that ran it is `{"why", "ask", "before", "now"}`, why no
  kept pass held:
  - `run-all`: `--run-all` was given.
  - `no-head`: no pass of the suite was kept yet.
  - `unknown-state`: the head was restored from the journal for another binary, and this daemon
    has no image of it.
  - `no-row`: no pass of this test was kept.
  - `function-changed`: `ask` is `fn ID` (or `the code no slot calls`), and `before` and `now`
    are its bound code, or `gone`.
  - `read-changed`: `ask` is `file PATH`, `dir PATH`, `env NAME` or `env *` (the whole
    environment), and `before` and `now` are its digests, or `absent`.

  With `-v`, the libtest line ends with the same reason as a sentence:
  `test tests::muls ... ok (ran: fn 3f2a91c04b1e changed: 81aa02c3d4e5 -> 0c9d17e2b3f4)`.

`--ub` runs the tests picked (every one) in blaze's evaluator under the checker of
undefined behavior (`crates/blaze-consteval`), compiling nothing. `--ub-seed N` and
`--ub-schedules K` set its seed and how many thread schedules it tries. `--ub` with `--no-run`
is refused.

### A run that ran no test is refused

A `test` or `bench` that built, in which nothing failed, and that ran no test exits 4
(`NO_TESTS`, `crates/blaze-core/src/exit.rs`; cargo-nextest's code for the same end), where
cargo and libtest print `test result: ok. 0 passed` and exit 0. The error names the selection
and, when a name filter left every test out, the nearest tests that exist:

```text
error: no test ran: `blaze test adds_wrng` matched none of the 20 tests in 4 test executables
note: the nearest that exist: tests::adds_wrong
note: a run of no test is refused; `--no-tests pass` allows one, as cargo does
blaze: tests failed selected=0 ran=0 passed=0 failed=0 ignored=0 measured=0 filtered_out=20 cached=0 binaries=4 unbuilt=0 elapsed_ms=412 why="no test ran"
```

The rule is one function over the run's counts (`crates/blaze-cli/src/test_run.rs`), the same on a terminal, off one and for the engine's
runs. Not refused: `--no-run` and libtest's `--list` and `--help`, which are meant to run
nothing; a run whose tests were all cached, whose kept passes stand; and `--no-tests pass`
(`fail` is the default), which is cargo's behaviour, for a
script that runs one filter over many packages, or `--doc` over packages without doc tests,
and expects 0 where nothing matched. A test executable without libtest's harness that prints
no `test result` line is one test, its exit status the verdict.

Every run that runs tests ends by saying its counts, so a pass says what it is a pass of: on a
terminal `8 of 9 ran: 1 failed · 7 passed · 1 ignored · 0 filtered` ([terminal.md](terminal.md));
off one a `blaze: tests ok|failed` line on stderr with `selected` (passed, failed, measured
and ignored), `ran` (failed, measured and the passed that were not cached), each count
(`cached`: the passed whose kept pass stood), `binaries`, `unbuilt`
and `elapsed_ms`; and with a JSON `--message-format` a `blaze-test-summary` line on stdout with
the same fields, `ok` and `why` (`crates/blaze-cli`). Off
a terminal each test executable's stdout comes through blaze, which writes it on unchanged and
reads libtest's result lines from it.

v1's test cache is gone with v1: `BLAZE_TESTS` (`affected`, `verify`, `all`),
its environment setting, `blaze test --why`, and the `tests-passed`,
`tests-programs` and `target/blaze/tests.ndjson` files it wrote. No v2 crate reads or writes
them.

## blaze never builds with cargo

v1 handed a build to cargo when it could not be sure of its own (`blaze: bug: <reason>;
building with cargo`). v2 has no such path: the client parses cargo's command line itself
(`crates/blaze-cli`, with clap's and cargo's own refusals and texts), and
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
`--fail-fast`, every other unit's tests still run, and the end names the doc tests it held back. The design and what is left are in [v2/plan/doctests.md](v2/plan/design/doctests.md).

## blaze prove

`blaze prove` checks the proofs of a crate's theorem functions with Lean
([v2/plan/design/proofs-for-sans-io.md](v2/plan/design/proofs-for-sans-io.md), section 5;
`crates/blaze-cli`). It takes
the package and target flags of `blaze check` and proves the library crates that check would
check, without `cfg(test)`. Nothing is compiled: the daemon builds only the proc macros and
build scripts' runs the front end applies, as `blaze reach` does.

For each function marked `#[blaze::theorem]` (a `fn` returning `bool`, in a crate with
`#![register_tool(blaze)]`), in the order of its path, blaze translates it to Lean
(`lean/`): a model of it and of what it calls, and the statement
`<name>.spec.statement`, that it returns `true` for every argument. Lean then checks the
crate's proof of it: `<name>.spec.proof` in the proof file for its module, under `lean/` beside
the crate's `Cargo.toml` (`blaze_graph::ledger::f` is proved in `lean/BlazeGraph/Ledger.lean`,
a crate's root module in `Lib`). One proof file may prove many theorems and import the
crate's other files under `lean/`, the models of its theorems and of what they call, blaze's
library (`Blaze.Auto` has the tactic to try first, `blaze_auto`) and Lean's own. The models,
a copy of the proof files and Lean's outputs are written under `lean/` in the workspace's
build directory, never in the checkout. Each theorem is one line:

- `proved F: THEOREM (axioms: ...)`: Lean's kernel accepted the proof, with no `sorry` and no
  axiom but Lean's three.
- `open F: PROOF_FILE`, then Lean's whole message indented: there is no proof file, or Lean
  refused it. The remaining goals are in the message; edit the proof file and run it again.
- `model broken F: ..`, then Lean's first error with its file and line: the Lean model blaze
  generated for the function does not build. That is a bug in blaze's translator, which no
  proof file can fix. A theorem's model is compiled whether or not it has a proof file, so
  `open` always means a model that builds and a proof still to write.
- `refused F: WHAT at `WHERE``: the translator does not take a construct in it.

The last line counts them: `N proved, N open, N model broken, N refused`. Lean is the executable
`BLAZE_LEAN_PROVER` names, an absolute path, of the release `lean/lean-toolchain` pins: unset,
or another version, the command fails, naming the variable and the version, and checks
nothing.

Lean runs in blaze's sandbox, as a build script does: a proof file's tactics can do IO, so it
reads its own release and the build directory's `lean/`, writes there alone, and has no
network (`BLAZE_SANDBOX=off` turns the sandbox off, as for build scripts). It is given a copy
of the proof files as the daemon read them, never the checkout. One run of Lean a module (the
library's, each model, each proof file, each theorem's check), those that import nothing of
each other at once; a run that has not ended in ten minutes is stopped and its theorems are
open, naming the module. A module Lean accepted is not run again while its text and what it
imports stay the same: a second `blaze prove` of an unchanged tree runs Lean not at all, and
after an edit it runs the modules the edit reaches.

What a theorem's function may be, with a worked one of each kind in blaze-core:

- A property of a pure function or of one step of a state machine, for every state:
  `crates/blaze-core/src/busy.rs`, `wait/retry.rs` (`a_retry_waits_what_is_left`).
- A crash: the function takes the crash's witness as an argument and returns `true` where the
  disk's world does not allow it (`world/disk.rs`, three theorems; with effects in flight,
  `world/asks.rs`).
- A whole loop against everything a kernel may answer: the loop takes its kernel as one value,
  and the theorem's function runs the real loop on a kernel that answers from a list
  (`wait/retry.rs`, `a_wait_ends_by_its_deadline`; `agents/howto/H-prove-a-loop-against-a-world.json`).
- Two threads that share atomics, on every interleaving: the function takes a schedule and
  runs the two with `blaze_core::world::threads::two` (`wait/parking.rs`, `no_unpark_is_lost`;
  `agents/howto/H-prove-two-threads.json`). A function that reaches an atomic operation is
  translated as a program over shared cells, and the statement is that the program's value is
  `true`. Orderings weaker than `SeqCst` are modelled on one cell; over more, the run belongs
  to the weak world (`lean/Blaze/ThreadsWeak.lean`), which blaze-core's `two` does not run yet.

Each has a test that runs the function on drawn arguments where it is stated, and a control: a
planted bug the function must be `false` of.

- `--emit` translates and writes the models and stops: one `emitted F: STATEMENT (MODEL)` line
  each, and no Lean is needed.
- `--subset` also counts every function in a `#![blaze::sans_io]` module of those crates, in
  one line, `subset: N functions in sans-IO modules, N translated, N generic (translated by
  instance), N refused`: a generic function is translated at each instance a theorem reaches
  and alone at none, so it is counted apart, not refused. Then each refusal, `    N refused
  for WHAT`, most first; a function is counted under the first refusal the translator meets.
- `--functions` (with `--subset` only; alone it is refused) adds one line a function of the
  subset, by path: `translated PATH`, `generic PATH` or `refused PATH: WHAT`.
- `--json` prints one object: `records`, each with `outcome` (`Proved`, `Open`, `ModelBroken`,
  `Emitted`, `Refused`) and the record's fields; `subset` (`functions`, `translated`, `generic`,
  `refused` as `{what, count}`, and `outcomes`, every function by path with its `outcome`,
  `Translated`, `Generic` or `Refused` with its `what`), with or without `--functions`; and
  `failed` (why no proof was checked, when none could be).

It exits 0 when every theorem is proved (with `--emit`, translated), or there is none (it says
so), and 1 otherwise. The same tree gives the same output, byte for byte.

## blaze doctor

`blaze doctor` checks the whole installation without changing anything and without the network
(`crates/blaze-cli`). Its sections, in order:

| section | checks |
|---|---|
| `version` | this blaze against the workspace's pin; against the newest release the last update check kept (`<blaze home>/update/state.json`) |
| `agents` | `blaze agents status` for each agent installed for or used here ([agents.md](agents.md)) |
| `daemon` | who holds the state (`serve.lock`, and whether that process still runs); whether the running daemon is another build of blaze |
| `state` | the daemon image and the profile history read; the state's size against the disk's free bytes; directories of layouts this blaze no longer reads (`v2.store-format-*`, `runner-*`), with their sizes |
| `toolchain` | the pinned Rust and its library sources; the machine's shared directory |
| `project` | directories of 1 GiB or more with no `CACHEDIR.TAG` (not `target/`, `.git/`); a file system whose timestamps have no sub-second part |
| `tools` | the sandbox build scripts and proc macros run in |

Each check is `ok`, `absent` or `unused` (a part a daemon starts empty), `warn` (blaze works,
worse or older than it could) or `fault` (it does not), says what it found, and names the command
that fixes it. The first line is the answer (`blaze is sound here`, `blaze works here, with N
warnings`, or `N faults, M warnings`); the exit code is nonzero only for a fault. `--json` prints
`{"version": 2, "state": DIR, "ok": BOOL, "checks": [{"section", "what", "file", "verdict",
"said", "fix"}], "quarantine": [COMMAND]}`; version 2 added `section`, `fix`, `warn` and every
section past `state`.

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
blaze-engine's Cargo.toml naming the package `blaze-engine`, wherever it is (a worktree,
a copy in a VM, the source of a Nix build, a fork). In any other workspace that pins a release
it hands off to that release as a release does, with one line on stderr naming its own
version, the pin and the manifest, and saying `--ignore-pin` runs it instead.
`--ignore-pin`, as the first word after `blaze` (`blaze --ignore-pin build`), runs the blaze it
is given to in place of whatever the workspace pins, a release or a build of the tree: how a
build of blaze is tried on a pinned project. Anywhere else it is the command's own word
(`blaze run --bin tool x --ignore-pin` passes it to `tool`). It applies to that one command's
workspace so a blaze a test or build script runs there (`$CARGO`) runs itself too, while one run in another
workspace (a test's temporary one) follows that workspace's pin.

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
| `BLAZE_SANDBOX` | `off` (or `0`) runs build scripts, proc macros and tests outside the sandbox ([above](#build-scripts-and-proc-macros-run-in-a-sandbox)) | sandboxed |

These are read by the process itself, the client or the daemon (from the environment of
whichever command started it; to change one there, run `blaze stop` first):

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_LEAN_PROVER` | the `lean` executable `blaze prove` checks proofs with, an absolute path, of the Lean `lean/lean-toolchain` pins; read by the client and sent with the request ([blaze prove](#blaze-prove)) | unset: `blaze prove` fails unless `--emit` |
| `BLAZE_HOME` | the daemon's socket and state directory, and blaze's releases and settings; not its toolchains ([Install](#install)) | see there |
| `BLAZE_TOOLCHAINS` | the directory blaze keeps the Rust it builds with in, shared by every `BLAZE_HOME` ([toolchain.md](toolchain.md)) | `.blaze/toolchains` in the user's home (`HOME`, else the system's record of the user) |
| `BLAZE_RELEASES` | a mirror of blaze's releases ([below](#pinning-a-release-of-blaze)) | `https://blaze-releases.ix.dev` |
| `BLAZE_CORES` | how many cores blaze's work runs on and at what priority: unset, the machine's less 2 at background priority; `all`, every core at normal priority; `n`, exactly `n` at background priority. A command that sets a value giving another budget or priority than the running daemon's is refused | unset |
| `BLAZE_EMIT` | LLVM instruction selection: `optimized` or `fast`. It is in no unit's key, so code the store kept under one value is used under the other | `optimized`; `fast` at level 0 |

`BLAZE_BACKEND` and `BLAZE_OPT` are not settings in v2: the profile picks the backend
([below](#backends)). v1's `BLAZE_TESTS`, `BLAZE_MEMORY_BUDGET`, `BLAZE_SPARE` and `BLAZE_LRU`
are read by nothing in v2.

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
scripts and proc macros run are read over the profile (`crates/blaze-code`).
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
spec's hash (`crates/blaze-tier`): stencil code is never given to a build
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
say) fails the build, naming it (`crates/blaze-unit-graph`); remove it by hand.

## macOS arm64

v2 builds `aarch64-apple-darwin` as it does Linux, linking with blaze-link; what is known of
it is in [platforms.md](platforms.md). The rest of this section is a record of v1 (as of
2026-10-27), which linked rustc's objects and spliced edits into them; neither the harness it
names nor the splice is in v2.

What was run under v1, on an 8-CPU Apple M4 virtual machine with macOS 26.5 and Rust 1.99.0:
harness/blaze_cli.py `--small` (deleted since), under Cranelift and under LLVM (`BLAZE_OPT=2`,
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

v2's client has only the commands of [Commands](#commands) (`crates/blaze-cli`);
any other fails with status 101 and does nothing:

    error: no such command: `vendor`

    help: blaze's commands are build, check, run, test, bench, doc, lock, clean, cache, gc, store, fmt, new, init, tree, metadata, serve, stop, status, toolchain, daemon, codegen-inputs, trace, tables, explore, debug, explain, why, reach, prove, slow, diagnostics, doctor, audit, agents, pprof and perf

The table below is a record of what v1 had (as of 2026-10-30): the crates it names
(blaze-metadata, blaze-fmt, blaze-lints, ...) are still in the tree, and a command comes back
once the client is given it. None of it runs in v2, except `fmt`, `new`, `init`, `tree` and
`metadata`, which are back as [above](#commands): each runs in the client's process
(`crates/blaze-cli`) and asks the daemon nothing. `tree` and `metadata`
decide the lock files as a build does ([Lock files](#lock-files)).

| Command | blaze's | What it is, and what it waits for |
|---|---|---|
| `build` (`b`), `run` (`r`), `test` (`t`), `check` (`c`) | the build | [above](#commands) |
| `metadata` | yes | `blaze-metadata` (now in `crates/blaze-manifest` / `crates/blaze-unit-graph`): the workspace read by blaze-manifest, every locked package fetched by blaze-fetch, resolved by blaze-resolve; `--no-deps`, `--features`, `--all-features`, `--no-default-features`, one `--filter-platform` (its `cfg` as blaze answers `rustc --print cfg` itself), `--manifest-path`, `--offline`. Measured equal to the pinned nightly's cargo before the comparison harnesses were deleted (2026-10-30): ripgrep, fd, helix, nushell and biome printed the same JSON for each form, with `locate-project`, `verify-project`, `pkgid`, `read-manifest`, `fetch` and `clean` (130 of 130 cases over the five). A package's `hints` has cargo 1.101's `"min-opt-level"`, always `null`: blaze-manifest does not read a value of it yet. A registry package the lock file does not pin, the lock file missing or stale, is chosen from the registry's index as cargo chooses it (blaze-registry's `IndexRegistry`), and the lock file written as cargo writes it. Several `--filter-platform`s fail with the reason |
| `locate-project` | yes | `--workspace`, `--message-format plain\|json`, `--manifest-path` |
| `verify-project` | yes | `{"success":"true"}`, or `{"invalid":...}` and status 1 |
| `version`, `help` | yes | blaze's version and help |
| `fmt` | yes | `crates/blaze-fmt`: rustfmt and cargo-fmt at rust-lang/rust 12433c307 ported onto a syntax tree of blaze's own, filled by rust-analyzer's parser with rustc's spans, run in process. `blaze fmt [--check] [-p NAME]... [--all] [--manifest-path PATH] [-v\|-q] [-- RUSTFMT_ARGS]` formats the targets blaze-manifest reads; `blaze fmt a.rs b.rs` formats files, with rustfmt's options. Measured against stock rustfmt (the pinned nightly's) before the comparison harnesses were deleted (2026-10-30): ripgrep, nushell, fd, helix and biome with their own rustfmt.toml, 6068 of 6068 files byte-equal (100%), and `--check` exit codes and diffs equal; the same with `max_width = 60`, 6068 of 6068 (100%); rustfmt's `tests/source`, 773 of 793 (97.5%; 18 more use line ranges, which it does not run). At blaze-fmt f43e4a20af on dev-aws-14. A file is refused with the reason, and left as it is, when it has nightly syntax rust-analyzer does not parse (postfix `.yield`, `.use` and `.match`, `const` block items, `use` closures, `&pin`, unsafe binders, `unsafe mod`, `mut ref`, `default` on an associated `const fn`), a parenthesized trait object with `+` (`(Foo) + 'static`), a `macro` with a parameter list, an `impl` or `mut` restriction, or a typed `try` block; a non-empty `ignore` option is an error |
| `clippy` | yes | blaze-lints (deleted on 2026-11-15): 24 of clippy's lints ported onto rust-analyzer's analysis, run by the daemon, written as `cargo clippy` writes them |
| `doc` (`d`) | yes | `blaze-doc` (parked): every page from rust-analyzer's HIR, no rustdoc; cargo's flags, `target/doc/<crate>/index.html`, search, source pages, themes. Measured against the pinned nightly's `cargo doc` before the comparison harnesses were deleted (2026-10-30), with their dependencies: fd 100.0% of rustdoc's pages, 99.9% of their members and impls, 99.4% of the resolved links; ripgrep 99.9%, 99.6%, 99.1%; nushell 99.5%, 99.2%, 98.4%; helix 98.9%, 99.0%, 98.7%; biome 99.0%, 99.2%, 98.1%. biome in 92.5 s against `cargo doc`'s 1028.0 s, nushell in 28.4 s against 497.1 s, its build scripts already run ([doc.md](doc.md)). The load still runs cargo for build scripts and proc macros, as every load does; doctests are `blaze test --doc`'s |
| `tree` | yes | `blaze-tree` (parked), cargo 1.101's `cargo tree` ported onto blaze-resolve: every flag (`-p`, `--workspace`, `--exclude`, `-e`, `-i`, `--prune`, `--depth`, `--prefix`, `--no-dedupe`, `-d`, `-f`, `--target`, `--charset`, `--color`, the feature flags). Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 26 forms over ripgrep, fd, helix, nushell and biome, 130 of 130 byte-equal. Package patterns with globs (`-p 'serde*'`) and `-i` with no value are refused |
| `pkgid` | yes | the package's full spec from `Cargo.lock`, with cargo's errors for none or several matches |
| `fetch` | yes | every package `Cargo.lock` names, downloaded by blaze-registry. `--target` fetches every platform's, and with no lock file it fails (choosing versions is #321) |
| `clean` | yes | the workspace's whole build directory under blaze's root, or the one `--target-dir`, `CARGO_TARGET_DIR` or `build.target-dir` names, and `build.build-dir`'s; `--dry-run`, `--manifest-path` and cargo's `Removed` summary. `--list` prints every workspace's build directory with its size, `--all` removes them all. One profile's (`--release`, `--profile`), `--doc`, `-p`, `--workspace` and `--target` are refused (`crates/blaze-cli/src/clean.rs`) |
| `new`, `init` | yes | `crates/blaze-new`, cargo 1.101's `ops/cargo_new.rs`: the manifest, source file, ignore file and repository, the workspace's `members`, and cargo's checks of the name. The repository is made by `git init` (or `hg`, `pijul`, `fossil`, as cargo runs them). Cargo runs rustfmt on the file it writes; blaze does not, so a `rustfmt.toml` above the package that would change the template is not applied. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 50 of 50 cases, what each prints and every file it leaves |
| `new --bstd PATH`, `--rust` | blaze's | A package of the blaze edition ([std](v2/plan/design/std.md#the-blaze-edition-opting-in)): `edition = "blaze-2026.11"`, bstd named at `PATH`, and a `main(cx: &Cx, io: Io)` that writes its line through `Out`. `--rust` is the package of the row above whatever the default; the edition becomes the default when blaze ships bstd (`blaze_new::BSTD_SHIPS`). A release build is told the shipped bstd's digest at compile time in `BLAZE_BSTD_DIGEST` (`blaze_new::BSTD_DIGEST`); a build of the tree has none |
| `remove` (`rm`) | yes | the dependencies out of the member's manifest, with `toml_edit` keeping its layout and comments, as cargo 1.99's `cargo_remove.rs` and `toml_mut` do: the features that named them, then the `[workspace.dependencies]`, `[profile.*.package]`, `[replace]` and `[patch]` entries nothing uses any more; then the lock file resolved again by blaze with every version it keeps locked as it was (blaze-metadata's `relock`), honouring `--locked`. A workspace with no lock file is refused: writing one chooses versions (#321). Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30), manifests and lock files byte for byte: 43 of 43 cases on ripgrep and a made-up workspace |
| `vendor` | yes | every registry package of the lock file unpacked from its `.crate` into the vendor directory as cargo 1.99's `ops/vendor.rs` does, with `.cargo-checksum.json`, `--versioned-dirs`, `--no-delete`, `-s`/`--sync` and `--respect-source-config`, and the `[source]` configuration printed; the workspace resolved again first, as cargo does (blaze-metadata's `vendored`). A git package is refused: cargo vendors it with its manifest as `cargo package` writes it, which blaze does not write yet. A workspace with no lock file is refused: writing one chooses versions (#321). Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30), every file by its sha256: 18 of 18 cases on fd, ripgrep and a made-up workspace |
| `add`, `update`, `generate-lockfile` | not yet | they choose versions, which blaze-resolve does not do yet ([#321](https://github.com/indexable-inc/blaze/issues/321)) |
| `bench`, `rustc`, `rustdoc`, `fix`, `install`, `package` | not yet | builds blaze does not do yet ([no-rustc.md](no-rustc.md)) |
| `search`, `owner`, `yank`, `login`, `logout` | yes | blaze-registry's `api.rs` (parked), the `crates-io` crate 0.41's client of a registry's web API over blaze's own HTTP, and cargo 1.99's `ops/registry` around it: which registry (`--registry`, `--index`, `registry.default`, `source.*.replace-with`, the `Updating` of its `config.json`), the token (`--token`, `CARGO_REGISTRY_TOKEN`, `CARGO_REGISTRIES_<NAME>_TOKEN`, `credentials.toml` and the old `credentials`, read and written as cargo writes them, mode 0600), cargo's errors of the server (`errors`, a status not 200), and `search`'s table with `--limit` and its highlighted names. The credential providers of `registry.global-credential-providers` and a registry's `credential-provider` are asked in turn as cargo 1.101 asks them (`blaze-registry`'s `auth`): `cargo:token`, `cargo:token-from-stdout`, and credential processes spoken to in cargo's protocol, version 1, with `[credential-alias]`, each answer's caching and cargo's words for each failure. `cargo:wincred`, `cargo:macos-keychain` and `cargo:libsecret` on their own platform are refused with the provider named and what to set instead; on another platform they answer for no registry, as in cargo. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30) against a mock registry: 101 of 101 cases, what each prints, every request the server saw, the credentials files left and what each provider was asked |
| `publish`, `info` | not yet | `publish` packages the crate first, which is a build of `package`; `info` is not written yet |
| `uninstall` | yes | cargo 1.99's `cargo_uninstall.rs` and install tracker: removes the programs from the install root's `bin` (`--root`, `CARGO_INSTALL_ROOT`, `install.root`, cargo's home) and their entries in `.crates.toml` and `.crates2.json`, written byte for byte as cargo writes them; specs, `-p`, `--bin`, the package of the directory, cargo's `Summary` of several. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 18 of 18 cases, what each prints and the files it leaves |
| `read-manifest` | yes | the package's entry of `metadata --no-deps`, as cargo prints it |
| `report` | yes | `future-incompatibilities` (`future-incompat`): the report a build left in the build directory, with `--id`, `-p` and `--color`, as cargo 1.99's `future_incompat.rs` prints it. `timings`, `sessions` and `rebuilds` are unstable in cargo: refused as cargo refuses them without `-Z build-analysis`, and with it they fail saying blaze's builds do not write the logs they read. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 23 of 23 cases |
| `config` | yes | `config get`, by blaze-manifest's port of cargo 1.99's `cargo_config.rs`: `--format toml`, `json` and `json-value`, `--show-origin`, `--merged no`. Unstable in cargo, and refused as cargo refuses it unless `-Z unstable-options`, `unstable.unstable-options` or `CARGO_UNSTABLE_UNSTABLE_OPTIONS` allows it. Measured equal to cargo's before the comparison harnesses were deleted (2026-10-30): 41 of 41 cases |
| any other | no | `error: no such command`; blaze runs no cargo plugin |
