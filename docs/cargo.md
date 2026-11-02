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

`blaze` is one program, also installed as `blaze-engine`. The [installer](../install.sh) and
`cargo install` both install it:

    curl -fsSL https://ix.dev/blaze | sh
    cargo +nightly-2026-10-27 install --locked blaze-engine

From the release after 0.1.0, `--profile dist` builds the profile a release is built with, less
the PGO a release adds ([reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)); 0.1.0 has no such
profile and fails with it. From a checkout, the release's own recipe builds all of it, with the profile
[tools/pgo/profiles.json](../tools/pgo/profiles.json) pins:

    python3 .github/scripts/build_release.py --target $(rustc +"$(cat harness/toolchain)" -vV | sed -n 's/^host: //p') --out shipped
    cp shipped/* ~/.cargo/bin/

That build has the LLVM backend (the release profile's, [below](#backends)) and the LLVM tiers, as the
installer's binaries do: the script fetches the LLVM 23 blaze pins and links it in statically
([which LLVM](contributing.md#llvm-for---features-llvm)). `cargo install` from crates.io has
the Cranelift backend only; `--features llvm` there needs `LLVM_SYS_231_PREFIX` set to that
same LLVM. `cargo build -p blaze-engine --release` is the quicker build the harnesses and CI use while
working on blaze; it is not what ships.

`blaze` builds with the one toolchain blaze compiles, the dated nightly in
`harness/toolchain` (`nightly-2026-10-27`): the language and standard library are the ones this
blaze ships. It ignores `rust-toolchain`/`rust-toolchain.toml`, `rustup override`,
`+toolchain`, `RUSTUP_TOOLCHAIN`, rustup's default and `RUSTC`, and never runs or asks a rustc
to learn anything ([toolchain.md](toolchain.md)). The nightly builds
what older releases built, so a project on a stable release or an older nightly builds as it
is, `#![feature(...)]` included. A project that needs another Rust pins another blaze. Plain
`cargo` still builds with the project's toolchain, in the same target directory.

Where the nightly's files come from, found with nothing run: rustup's `nightly-2026-10-27`,
when rustup has it (only as a copy of the same files; what rustup would pick
is not asked); else blaze's own copy in
`~/.blaze/toolchains/nightly-2026-10-27-<host>` (`BLAZE_HOME` moves `~/.blaze`), laid out as rustup lays
out a toolchain. blaze downloads its copy the first time it needs it, once, with a line on
stderr while it does: rustc, cargo and the built standard library from the Rust project's
own servers (the standard library's source is blaze's own `library/`, a 6.0 MB archive from
blaze's release, [toolchain.md](toolchain.md#the-standard-library-library)), 97 MB on macOS, 134 MB on Linux x86-64 and 118 MB on Linux ARM64 (548 MB on macOS
and 710 MB on Linux x86-64 once unpacked). Each file's sha256 is built
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
    blaze check [cargo's flags] --baselines DIR [--bless-baselines] [--new-baseline LINT[,LINT...]]
    blaze run   [cargo's flags] [-- program arguments]                (r)
    blaze test  [cargo's flags] [TESTNAME] [-- test arguments]        (t)
    blaze bench [cargo's flags] [BENCHNAME] [-- bench arguments]
    blaze doc   [cargo's flags]                                       (d)
    blaze serve [--socket PATH] [--http ADDR] [--activated] [--ready-fd N]
    blaze stop
    blaze status
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
and `--baselines` are [below](#lints). There is no `clippy` and no `fix` command: `check` is both. Any other command (v1's `fmt`, `tree`,
`metadata`, `toolchain`, ...) fails with `error: no such command` and status 101; none is
passed to cargo, and no cargo plugin is run ([Every subcommand](#every-subcommand)).

`blaze check` checks the targets `cargo check` with the same flags does: a plain one
the libraries and binaries of the packages selected, and `--lib`, `--bins`, `--tests`,
`--examples`, `--benches`, `--all-targets` and `--bin`, `--test`, `--example` and `--bench`
with a name those they pick, as cargo's unit graph of the check has them. A target picked as
a test (a library's unit tests under `--tests`) is checked with `cfg(test)` and the
dev-dependencies; a member the selected packages only depend on is checked as its library.

`build`, `run`, `test` and `bench` take the flags cargo takes and leave the same files in the
same places under `target/`. The client reads no manifest: it finds the checkout, sends the
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
watches and the memory it holds of its budget. `stop` has the daemon write its state and end.

`doc` takes `cargo doc`'s flags (`--open`, `--no-deps`, `--document-private-items`, `--lib`,
`--bins`, `--bin`, `--examples`, `--example`, and the package, feature, profile and target
ones) and documents what `cargo doc` documents, with no rustdoc: the daemon checks the targets
`cargo doc` picks (a library, and each binary not named as it; `Intent::Doc` in
`crates/blaze-units`) and every dependency, as any check, then makes every page from that front
end with `cfg(doc)` set (`crates/blaze-doc`, `crates/blaze-cargo/src/serve/unit/doc.rs`, the
agent API's `Request::Doc`). The client writes the pages to `target/doc`, or
`target/<triple>/doc` with `--target`, keeps the crates earlier runs documented there in the
search's list, and says `Documenting`, `Finished` and `Generated` (or `Opening`, and opens the
page with `$BROWSER` or the system's opener, for `--open`) as cargo does. What `cargo doc`
reads and blaze can't honor yet stops it with an error naming it: rustdoc's flags
(`RUSTDOCFLAGS`, `build.rustdocflags`, `target.<triple>.rustdocflags`), `doc.browser` under
`--open`, a JSON `--message-format`, `--unit-graph` and `--timings`; doctests are refused as
`test --doc` is. What the pages hold is in [doc.md](doc.md), a record of v1's
`blaze doc`, which wrote them from the same crate.

While a build waits, the client prints each unit's lines as the daemon sends them; there is no
live progress view in v2.

## What a build does

blaze never runs rustc, and a build has one path ([no-rustc.md](no-rustc.md)). The daemon
reads the command line as cargo does, into cargo's unit graph (`crates/blaze-units`: the
packages, features, profiles, targets and build-script runs cargo would have), and builds
each unit itself: every crate is compiled by blaze over blaze's standard library, a
function at a time ([v2/plan/design/model.md](v2/plan/design/model.md)), and each program is
linked by blaze's linker (`crates/blaze-link`) where cargo puts it (`target/debug/<name>`,
`target/release/<name>`, `examples/` for an example). No `cargo build` runs. A function the
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

The build is the daemon's, and the daemon keeps what it built: its database and its store.
The next build compiles only the functions an edit changed, and links each program again
against an image it kept of the last link at that path, so the link writes only what changed
(`crates/blaze-cargo/src/serve/unit/codegen.rs`, `Linked`). Build scripts and proc macros
are compiled by blaze too, as units of the same graph ([below](#build-scripts-and-proc-macros-run-in-a-sandbox)).

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
the doc tests, stopping at the first that fails unless `--no-fail-fast`; `--no-run` lists them.
That runs every test, outside any sandbox, as cargo does. blaze's `--reached` runs only the
tests an edit can have changed, in the daemon, and `--ub` runs them in blaze's evaluator
([below](#which-tests-run)). v2's daemon makes no doctest yet (below).

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
proc macros run in blaze's own process, on its evaluator.

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
started without it. What the run read, its files and the variables (all of them: a process may
walk `environ`, which no call shows), the sandbox traces, and the run is kept in the store by
its code, what it was given and what it read (`blaze_usercode::run`), so a command whose
inputs to it are as they were finds it there and runs nothing. A script that provably prints
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
process, confined by blaze's evaluator, which is their sandbox and has no way out
([below](#proc-macros-run-on-blazes-evaluator)), and a script is given the command's whole
environment already.

What a sandboxed script prints is checked before it is used (`crates/blaze-cargo/src/directives.rs`), since
it goes into every rustc and linker run of its package and of what links it. A
`rustc-link-search` (or `rustc-flags`' `-L`) names a directory in its `OUT_DIR`, its package,
the target directory's build directory (its dependencies' `OUT_DIR`s), a system directory
(`/usr/lib`, `/lib64`, `/usr/local/lib`, the Nix store, Homebrew's, Apple's SDKs) or one of
`build-script-link-dirs`; a `rustc-link-arg` is one of a short list (`-pthread`, `-rdynamic`,
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

### Proc macros run on blaze's evaluator

Every proc macro is built by blaze from its source and run in the daemon's own process on
blaze's MIR evaluator (`crates/blaze-macro-eval`,
[v2/plan/design/compile-time.md](v2/plan/design/compile-time.md), change 20): the macro's real
code, with the real std and `library/proc_macro`, every access checked, and the bridge's one
call answered by blaze's own server. Nothing rustc compiles is loaded and no process is
started: the evaluator is the proc-macro sandbox. The macro reads only below the calling
crate's `CARGO_MANIFEST_DIR` and `OUT_DIR`, writes nothing, sets no variable and makes no
system call blaze has no shim for (`crates/blaze-macro-eval/src/os.rs`). Its compiled tier,
off by default (`Policy::DEFAULT`), keeps the same confinement: the macro's code reaches only
a 4 GiB region of its own and calls only checked table entries and the same shims
(`crates/blaze-macro-eval/src/tier.rs`, `tier/confine.rs`). A macro blaze-macro-eval cannot
run stops the build with an error naming the macro and its crate; nothing runs natively and
nothing goes to another host (there is none). Each expansion is kept in the store
(`crates/proc-macro-api/src/kept.rs`) by the macro's library, its name, input and attribute
tokens, the crate's variables, its directory and the toolchain, and found by what else it
read; `BLAZE_PROC_MACRO_CACHE=check` expands every kept call anyway and fails naming a macro
whose output differs.

### Build scripts under WASI, and proc macros as WebAssembly (v1, not in v2)

v1 could compile a build script for `wasm32-wasip1` and run it under Wasmtime (`wasi-compare`,
`wasi-build-scripts`), built every proc macro as a WebAssembly module (`native-proc-macros`),
and could expand with rust-analyzer's out-of-process proc-macro server. v2 does none of these,
and their crates are deleted: every build script runs natively in the sandbox, and every proc
macro on blaze's evaluator (above). Nothing in blaze reads v1's variables or manifest lists
for them, so setting them changes nothing. The comparison of the two ways of running scripts is
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
  and the client names those last, on stderr. The design is
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

Doc tests are not run: v1 ran them itself, with no cargo, rustc or rustdoc, and that code
was deleted on 2026-10-30. The daemon makes no doctest yet (the agent API has the artifact
kind, `ArtifactKind::Doctest`, and `blaze test` runs any it is given), so `blaze test` runs a
library's unit and integration tests and none of its examples.

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

the requests it is answering, how many views (a workspace at a checkout) it holds, and what it
holds of its budget.

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

In a project, the daemon writes the programs and test executables where cargo would, under
`target/<profile>/`.

The store is always on: one per daemon, in `$BLAZE_HOME/v2/store`, the user's alone (the
daemon refuses one another user owns or others may write to;
[v2/plan/lanes/store.md](v2/plan/lanes/store.md)).

## Pinning a release of blaze

A project pins blaze, not Rust: each release of blaze builds with the Rust it ships, and
`rust-toolchain` files are not read ([toolchain.md](toolchain.md)). `blaze pin` writes the
version of the blaze it is into the workspace's root `Cargo.toml`:

    [workspace.metadata.blaze]
    version = "0.20261027.0"

(the rest of the manifest is kept as it was; in a package with no `[workspace]` that adds
one, which makes it the root of its own workspace, as it is anyway when no other is above
it). From then on the `blaze` anyone runs in that workspace runs that
release, as Bazelisk runs the Bazel a `.bazelversion` names: when it is another release
itself, it runs `~/.blaze/versions/<version>/blaze` (`$BLAZE_HOME/versions` with `BLAZE_HOME`)
with the same arguments, and when that is not there it first downloads
`blaze-v<version>-<host>.tar.gz` from the release on GitHub, checks its BLAKE3 hash against
the release's `B3SUMS` and unpacks it there. The network is used only then, once per
release. A pinned release that can't be fetched (offline, no such release, no `B3SUMS`, no
build for this host, another hash) is an error, and no other blaze builds in its place.
`BLAZE_RELEASES` names a mirror laid out as
`https://github.com/indexable-inc/blaze/releases/download` (`<mirror>/v<version>/<asset>`),
or a directory of `v<version>` directories of the assets. `blaze --version` says which
blaze runs and why: as the workspace pins it, or as nothing pins one. `--manifest-path`
picks the workspace a command is for, as it does for cargo.

The pin may name a channel instead of a version ([release.md](release.md)):
`version = "stable"` runs the newest release that passed the heavy tests, and
`version = "canary"` the newest release. blaze reads which release the channel names from its
pointer (`<releases>/channels/<channel>`, one small download, through `BLAZE_RELEASES` too),
keeps it in `~/.blaze/channels/<channel>` and reads it again after an hour; that release is
then run as a pinned version is. When the pointer can't be read (offline), the release it named
when last read runs, with a line saying so; with none read before, it is an error. A pin to a
version stays on that release, whose bytes never change: promotion from canary to stable flips
the same release.

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
| `BLAZE_SANDBOX` | `off` (or `0`) runs build scripts and proc macros, and tests under `--reached`, outside the sandbox ([above](#build-scripts-and-proc-macros-run-in-a-sandbox)) | sandboxed |
| `BLAZE_ELIDE` | `verify` also compiles and runs a build script blaze elided as inert, and checks it said what elision did; any other value is an error | unset: an inert script is not run |
| `BLAZE_PROC_MACRO_CACHE` | `check` expands again each expansion the store kept and compares; any other value is an error | unset: a kept expansion is used |

These are read by the process itself, the client or the daemon (from the environment of
whichever command started it; to change one there, run `blaze stop` first):

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_HOME` | the daemon's socket and state directory, and blaze's toolchains and releases ([Files](#files), [Install](#install)) | see there |
| `BLAZE_RUST_DIST` | where the pinned nightly's components come from: a URL, or a directory of them ([Install](#install)) | rust-lang's dist server, at the pinned date |
| `BLAZE_RELEASES` | a mirror of blaze's releases ([below](#pinning-a-release-of-blaze)) | GitHub's |
| `BLAZE_API_TOKEN` | the bearer token `blaze serve --http` requires ([Files](#files)) | none: `--http` refuses to start |
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
| `0` | Cranelift | `dev`, `test` |
| `1` | Cranelift: rustc's `-O1` has no LLVM pipeline blaze runs to match it | |
| `2` | LLVM at `-O2` | |
| `3` | LLVM at `-O3` | `release`, `bench` |
| `s`, `z` | LLVM at `-O2`: blaze's LLVM has no pipeline for size yet, so the code is not smaller than at `-O2` | |

The level is chosen per function's package, as cargo's is: `[profile.<name>.package.<spec>]`
(`"*"` for every package that is not a workspace member) and `build-override` for what build
scripts and proc macros run are read over the profile (`crates/blaze-codegen/src/plan.rs`).
Cranelift optimizes all but a workspace member's code, which no edit reaches and so is compiled
once. The standard library's own functions are compiled as rustc's shipped std is, at `-O3`. A
blaze built without LLVM refuses a profile that asks for it, saying so, rather than building it
with Cranelift.

Each function's code is kept by the backend and level that compiled it, which are part of its
spec's hash (`crates/blaze-codegen/src/settings.rs`): code Cranelift compiled is never given to
a build with LLVM, nor `-O2`'s to `-O3`'s.

A build with LLVM takes longer than one with Cranelift. A first build takes the most longer,
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

    error: no such command: `fmt`

    help: blaze's commands are build, check, run, test, bench, doc, serve, stop, status, toolchain, daemon, codegen-inputs, trace, tables, pack, explore, debug and explain

The table below is a record of what v1 had (as of 2026-10-30): the crates it names
(blaze-metadata, blaze-fmt, blaze-lints, ...) are still in the tree, and a command comes back
once the client asks the daemon for it. None of it runs in v2.

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
| `clean` | yes | the target and build directories, one profile's (`--release`, `--profile`), or `--doc`, with `--target-dir`, `--dry-run` and cargo's `Removed` summary. `-p` and `--workspace` (one package's files) are refused |
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
did not; `--fix` checks every target when none is named. `--baselines DIR` holds the run's lint
counts to the baselines checked in under `DIR`, `--bless-baselines` lowers them to the run's,
and `--new-baseline LINT[,LINT...]` starts a lint's at the run's
([lints.md](v2/plan/lanes/lints.md#blaze-check-lints)). A baseline holds a lint at `warn`: a
`deny` lint's finding fails the check, baselined or not.

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
