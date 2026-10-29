---
status: current
summary: Every blaze command and setting: cargo's commands and flags, what a build does, sandboxing, manifests.
---
# Using blaze

`blaze <command>` takes cargo's commands and flags (`blaze build`, `blaze test`, `blaze run`),
described here. The other way, switching blaze on globally so that plain `cargo` goes through
it, has its own section at the end.

blaze had another name and another command before 2026-10-28; [rename.md](rename.md) maps them.
Every setting below is `BLAZE_<NAME>`, and blaze stops when one is set under its name from
before, saying what it is called now.

Everything in this file was checked against the code in `crates/blaze-cargo/src/cargo/` and run on
Linux x86-64. On macOS arm64 `harness/blaze_cli.py --small` was run, and what is known of
it is under [macOS arm64](#macos-arm64). The design notes it was written from are in
[reports/CARGO.md](reports/CARGO.md).

## Install

`blaze` runs the `blaze-engine` executable in the same directory, blaze's engine, so the two
have to be installed side by side, with `blaze-rustc`. The [installer](../install.sh) and
`cargo install` both do that:

    curl -fsSL https://ix.dev/blaze | sh
    cargo +nightly-2026-10-27 install --locked blaze-engine

From the release after 0.1.0, `--profile dist` builds the profile a release is built with, less
the PGO a release adds ([reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)); 0.1.0 has no such
profile and fails with it. From a checkout, the release's own recipe builds all of it, with the profile
[tools/pgo/profiles.json](../tools/pgo/profiles.json) pins:

    python3 .github/scripts/build_release.py --target $(rustc +"$(cat harness/toolchain)" -vV | sed -n 's/^host: //p') --out shipped
    cp shipped/* ~/.cargo/bin/

That build has the LLVM backend (`BLAZE_BACKEND=llvm`) and the LLVM tiers, as the
installer's binaries do: the script fetches the LLVM 23 blaze pins and links it in statically
([which LLVM](contributing.md#llvm-for---features-llvm)). `cargo install` from crates.io has
the Cranelift backend only; `--features llvm` there needs `LLVM_SYS_231_PREFIX` set to that
same LLVM. `cargo build -p blaze-engine --release` is the quicker build the harnesses and CI use while
working on blaze; it is not what ships.

`blaze` builds with the one toolchain blaze compiles, the dated nightly in
`harness/toolchain` (`nightly-2026-10-27`): the language and standard library are the ones this
blaze ships. It ignores `rust-toolchain`/`rust-toolchain.toml`, `rustup override`,
`+toolchain`, `RUSTUP_TOOLCHAIN`, rustup's default and `RUSTC`, and never runs or asks a rustc
to learn anything ([toolchain.md](toolchain.md)): it runs cargo and the daemon under its own
release by naming it in `CARGO`, `RUSTC`, `RUSTDOC` and `RUSTUP_TOOLCHAIN`. The nightly builds
what older releases built, so a project on a stable release or an older nightly builds as it
is, `#![feature(...)]` included. A project that needs another Rust pins another blaze. Plain
`cargo` still builds with the project's toolchain, in the same target directory: cargo keys
what it builds by the rustc, so neither rebuilds the other's.

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
`crates/blaze-cargo/src/cargo/toolchain/pins.txt`), and a file with other bytes is refused. Concurrent
builds download it once; a download that stopped goes on from where it stopped the next time.
Building with blaze's copy, blaze names its cargo, rustc and rustdoc in `CARGO`, `RUSTC` and
`RUSTDOC` and puts its `bin` first on `PATH`, for the commands it runs and not for your shell.
`BLAZE_RUST_DIST` names a mirror of `https://static.rust-lang.org/dist` (or a directory with
the files), for a machine that cannot reach it. `blaze toolchain` (or `blaze toolchain`)
says which Rust blaze builds with, downloading its copy first when there is none.

## Commands

    blaze build [cargo's flags]
    blaze run   [cargo's flags] [-- program arguments]
    blaze test  [cargo's flags] [test names] [-- test arguments]
    blaze check [cargo's flags]
    blaze status
    blaze stop
    blaze toolchain
    blaze pin

`blaze <command>` is the same as `blaze <command>`, for a machine where blaze brought
its own Rust and there is no `cargo` on the path.

`blaze check` checks the targets `cargo check` with the same flags does: a plain one
the libraries and binaries of the packages selected, and `--lib`, `--bins`, `--tests`,
`--examples`, `--benches`, `--all-targets` and `--bin`, `--test`, `--example` and `--bench`
with a name those they pick, as cargo's unit graph of the check has them. A target picked as
a test (a library's unit tests under `--tests`) is checked with `cfg(test)` and the
dev-dependencies; a member the selected packages only depend on is checked as its library.

`build`, `run` and `test` take the flags cargo takes and leave the same files in the same
places under `target/`. `check` is what the daemon says of the source as it builds the
programs of the build with the same flags, when it has that build loaded: the errors a build
would fail with, or `Finished`, and no rustc. Any other check is the daemon's too: it loads
the workspace as a build of those flags would, builds nothing, and says what it finds in every
file of the workspace's crates. The build scripts and proc macros are the ones cargo built
before; a workspace that never had them built has rustc build them first. A check takes
`--message-format` (each of cargo's values), `-Z`, `--exclude` and several `--target`s, a
check for each; in JSON its errors are rustc's `compiler-message`s, with their spans,
`rendered` and the failure notes, then `build-finished`, which is what rust-analyzer's
check-on-save and other editors read, so they show the daemon's verdict. What only rustc writes
is not there: the labels and the help and notes of an error, its other spans, and
`code.explanation`; the message is rust-analyzer's, worded otherwise than rustc's. E0061's
span is where the missing argument goes, not the function called. `harness/check_json.py`
compares it with `cargo check` on the same errors. `status` prints what the daemon holds, and for every workspace how many builds
the daemon served and how many went to cargo, with the reasons. `stop` ends the daemon. Every
other subcommand of cargo's is blaze's own, or fails saying blaze has none yet: none is passed
to cargo, and no cargo plugin is run ([Every subcommand](#every-subcommand)).

While a build or check waits for the daemon, the command shows what the worker is at: the
worker sends events (`crates/blaze-protocol/src/events.rs`: each step as it starts and ends, and
`Progress` counts within it), and `crates/blaze-ui` draws them (`Resolving names 2.40s (3/120
crates)`). On a terminal that is a live view, cleared when the answer comes; off a terminal, one
line for each step the command saw; with `-q`, nothing. `harness/experience.py` runs each cold build on a terminal and fails one that shows
nothing for more than 1.5 s.

## What a build does

blaze never runs rustc, and a build has one path ([no-rustc.md](no-rustc.md)). `build` and
`run` build each program the build asks for whole: every crate it depends on is compiled by
blaze over the shipped standard library and linked by blaze, and the executable is put where
cargo puts it (`target/debug/<name>`, `target/release/<name>`, `examples/` for an example).
No `cargo build` runs. A function the program can reach that blaze cannot compile fails the
build, which says which and why; nothing is handed to cargo or rustc:

       Compiling kapp v0.3.1 (/src/k/app)
        Finished `dev` profile [blaze] target(s) in 18.40s

       Compiling kuses v0.1.0 (/src/k/uses)
    error: `kuses` can reach functions blaze could not compile, so it is not built:
      `khalf::halved`: unsupported: f16 and f128

What blaze does not build yet fails the same way, saying what: the targets `--bins`,
`--examples`, `--tests`, `--benches` and `--all-targets` pick, several `--target`s,
`--target-dir`, `--message-format`, `-Z`, and `--build-plan` and `--unit-graph`, and a
library whose crate types include `staticlib` or `dylib` when the build would write it (a
plain `build`, or `--lib`): blaze does not link one yet, and says which
([#262](https://github.com/indexable-inc/blaze/issues/262)). A library's `cdylib` (a native
Node addon, a Python extension, a plugin) is a program of the build: a plain `build` or
`--lib` links it as a shared library at cargo's path (`target/debug/lib<name>.so`, `.dylib`
on macOS), exporting the functions and statics its crate
names with `#[no_mangle]` or `#[export_name]` and nothing else, with the package's
`cargo:rustc-link-arg` and `cargo:rustc-cdylib-link-arg`. Each build after an edit compiles
what changed on what the worker loaded and links the library again; it is not spliced, so an
exported function an edit adds is exported. A host that loaded the library (node, python)
loads it again to see the edit. A build of other libraries alone (`--lib`, or a workspace with
no program) is the daemon's check of them: blaze writes no `rlib` for cargo, and says what it
finds.

The build is the daemon's: its worker loads the workspace, builds and links the programs, and
answers once they are linked. The next build asks the same worker, which compiles the
functions that changed on what it loaded and splices them into the executable or links again;
a changed manifest or lock file has a new worker load the workspace. Build scripts and proc
macros are still built and run as the daemon's load does (`target/rust-analyzer`).

`test` builds whole what cargo would test: each member's library, binaries and integration
tests with `test = true`, the examples built beside them to see that they compile, and the
binaries an integration test runs (`CARGO_BIN_EXE_<name>`), or what `--lib`, `--bin`,
`--example` and one `--test` name. One worker loads the workspace once, as `cargo test`
builds it, and builds them all: each library and binary twice, without `cfg(test)` and as its
unit tests with it, the integration tests with `cfg(test)`, and the examples without. Only an
integration test or a benchmark is told `CARGO_BIN_EXE_<name>` of its package's binaries and
`CARGO_TARGET_TMPDIR`, in its crate's own environment, as cargo tells them; a test's executable is
`target/debug/deps/<name>-<kind>`. A unit test with `harness = false`, several `--test`s and
the other target flags fail the build and say so. The doc tests come after, each example
built by blaze with no cargo, rustc or rustdoc (below).

`run` then runs the program and `test` runs the test executables the way cargo does: the
same working directory, the variables cargo sets, cargo's `Running` lines and the same exit
status, outside any sandbox, as cargo runs it, unless the workspace opts into sandboxed tests.
A hermetic test, one whose code reaches nothing outside the program and nothing blaze can't
see into, is skipped while it has passed with the code it reaches as it is now; every other
test runs every time ([below](#tests-are-hermetic-or-say-why-not)). `BLAZE_TESTS=all` runs
every test.

### Sandboxed tests (an experiment, opt-in)

Tests run in a sandbox only where the workspace opts in, in its root manifest:

```toml
[workspace.metadata.blaze]
experimental = ["sandboxed-tests"]
```

or where a package does, in its own manifest, with `tests = true` under
`[package.metadata.blaze.sandbox]`. `experimental` lists, by name, the experiments a workspace
opts into: `sandboxed-tests`, and `const-macros` (proc macros written as functions of a file of
the package, run by blaze's const evaluator: [future/const-macros.md](future/const-macros.md)).
Any other name, or a value that is not a list of names, is an error that names the manifest and
the names known. A test that is not
sandboxed is not kept as passed under `BLAZE_TESTS=affected`
([below](#running-only-the-tests-that-have-not-passed)). Build scripts run in the sandbox
whatever the workspace says ([below](#build-scripts-and-proc-macros-run-in-a-sandbox)), and
proc macros run as WebAssembly.

In a workspace that opts in, every test executable `blaze test` runs, under `all` and
`affected` alike, runs in a sandbox of its own, with libtest's threads inside it as cargo would
run them. In it the machine's files are read-only; the workspace, the target directory and
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

The sandbox records what each test reached, by name: the harness tells it, before and after
each test, which test the thread is running.

Tests are hermetic: "tests should be just like builds" (Andrew, 2026-10-27), their end a
function of their code and what they read, so that a pass can be kept and stand for a run
([below](#running-only-the-tests-that-have-not-passed)). So what would make a test's end
depend on something else is denied, and a test the sandbox denied something fails, whatever
it ended with, with what was denied and the way out:

    error: tests::writes_home fails: the sandbox denied it writing `/home/me/.cache/x` (the machine is read-only in a sandbox); tests are hermetic, so if it needs that, take it out of the sandbox, where it runs every time and is never cached: `unsandboxed-tests = ["tests::writes_home"]` under `[package.metadata.blaze.sandbox]` in /home/me/tsel/Cargo.toml (`tests = false` for every test of the package), or `BLAZE_SANDBOX=off` for every test of the command

| a test | in the sandbox |
|---|---|
| writes the workspace or the target directory | allowed, kept to its run (an overlay) |
| writes `/tmp`, `/var/tmp`, `/dev/shm` | allowed, empty at its start |
| writes anywhere else | denied (`EROFS`), and the test fails |
| reads a file, lists a directory, looks a path up | allowed, and recorded: part of its key |
| connects to, binds or sends to loopback | allowed: a network namespace of its own |
| connects to any other address, or a Unix socket of the machine's | denied (`ENETUNREACH`, `EACCES`), and the test fails; so no name is resolved: a DNS server outside is denied, and one on the machine's loopback is not in the sandbox's |
| starts a program | allowed on Linux: what the program reads, its executable and libraries too, is recorded with the test's reads, so it is a recorded input; on macOS a program macOS protects (`/bin/sh`) is started without the interposer, what it reads is not known, and the test is never kept |
| reads the clock, draws randomness, starts threads | allowed: a test whose end they decide passes and fails under one key, and is marked flaky and never kept |

What a thread no test marked or a process a test started was denied fails every test of the
run, since which test it was for is not known.

A package of a workspace that opts in takes its tests out of the sandbox in its manifest:

```toml
[package.metadata.blaze.sandbox]
tests = false                                 # every test of the package runs outside
unsandboxed-tests = ["tests::writes_home"]    # or these, by libtest's names
```

Any other key there is an error. The tests named run in a second run of the executable,
outside, after the sandboxed one (libtest's `--exact` and `--skip` split them); `-v` says
which and why. `BLAZE_SANDBOX=off` runs every test of the command outside, as cargo does
(`BLAZE_SANDBOX`, when set, wins over it, for tests, build scripts and proc macros alike). A
test outside the sandbox is not hermetic: it runs every time and its pass is never kept, and
the summary of the command names it.

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

What it costs, measured with `harness/sandboxbench.py` (one sandbox per executable against a
process per test, which blaze used before and dropped):

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
the 600-test library ten to thirty times as much, so there is none. On
macOS a stat under the interposer goes from 0.8 to 1.1 µs, and to 3 to 5 µs on a workspace
whose overlay has been written.

What is left: doctest examples run outside the sandbox; Windows has no sandbox
yet (its CI runs with `BLAZE_SANDBOX=off`). On Linux `openat2` always counts as a write; the
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

Every build script blaze's cargo runs, the workspace's and its dependencies', whoever compiled
it (blaze or rustc), and the daemon's own reruns of one, runs in the sandbox tests run in
([#147](https://github.com/indexable-inc/blaze/issues/147)): `cargo/script_sandbox.rs` is the
one place a script is run. A script is given, of what cargo gave it, what cargo sets for
scripts (`CARGO_*`, `DEP_*`, `OUT_DIR`, `TARGET`, `PROFILE`, ...), the C compiler's and
pkg-config's variables (`CC`, `CFLAGS_<target>`, `PKG_CONFIG_PATH`, `NIX_CFLAGS_COMPILE`,
`CPATH`, `SOURCE_DATE_EPOCH`, ...), `PATH`, `HOME`, `LANG` and the rest tests are given, and
what the root manifest's `build-script-env` names, and what it says it reads
(`cargo:rerun-if-env-changed=OPENSSL_DIR`) as cargo has it; what else the shell had is not
there. A run that said it reads a variable it was not given saw it unset, so the script runs
again given it, whether the first run succeeded or failed (`env::var("SDK_DIR").expect(..)`,
or a download the sandbox refused), and the store keeps that run under the variable's value.
The variables are the command's, not those of whoever started the daemon: `SDK_DIR=/opt/sdk
blaze build` gives the script `SDK_DIR` with a daemon started without it, and a command
without it, to a daemon started with it, gives it none. A command with another value of a
variable a script read, or of one `build-script-env` names, has the worker run the scripts
again and load the workspace with what they print
([#720](https://github.com/indexable-inc/blaze/issues/720)). The root manifest is the one of
the workspace built, also for a path dependency that is a workspace of its own. The machine is
read-only; it writes
its `OUT_DIR` for real, which is the one directory the Cargo book lets a script write ("Scripts
should not modify any files outside of that directory"); the temporary directories start empty;
there is no network but the sandbox's own loopback. Its package's directory, the workspace and
the target directory are seen as they are, even below `/tmp` or `/dev/shm`. On macOS the
per-user cache directory (`getconf DARWIN_USER_CACHE_DIR`, `/var/folders/../C`) is writable
too: clang's module cache, `xcrun`'s and the Metal compiler's live there, are found by
`confstr` and not by a variable a private one could be named in, and check what they cache.
Cargo's own rules for running a script again are unchanged.

A script that fails having been refused a write or a connection says so under its output:

    note: the build script of `sys-crate` failed in the sandbox, which refused it writing `/home/me/ws/sys-crate/src/bindings.rs` (the machine is read-only in a sandbox); if it needs that, run it outside: `unsandboxed-build-scripts = ["sys-crate"]` under `[workspace.metadata.blaze.sandbox]` in /home/me/ws/Cargo.toml (or `build-script = false` under `[package.metadata.blaze.sandbox]` in its own manifest, for a package of the workspace), or `BLAZE_SANDBOX=off` for every build script and test of the command

A script that downloads a prebuilt library (`v8`, `ort-sys`, `webrtc-sys`) is given it
instead, pinned as Nix pins a fixed-output derivation. `blaze lock --fetches` (`--target
<triple>` for each target, the host's by default) works out each download of the workspace's
resolve, keeps its bytes in blaze's store by their sha256 and writes them into
`Cargo.fetches.lock` beside `Cargo.lock`, by the package's name, version and `Cargo.lock`
checksum (commit it). A build writes the store's copy below the target directory and sets the
crate's own variable for a local copy to it; a missing or stale entry fails the build, naming
the command, and nothing is downloaded. The crates blaze knows and their variables are
`OVERRIDES` in `crates/blaze-cargo/src/cargo/fetches.rs`; a script refused the network that it does
not know is told which variable an entry there would set.

The ways out, in the workspace's root manifest, which is the only one a dependency from a
registry can be named in:

```toml
[workspace.metadata.blaze.sandbox]
unsandboxed-build-scripts = ["hegeltest", "flecs_ecs_sys"]   # by package name
unsandboxed-proc-macros = ["sqlx-macros"]                    # by package name
build-script-env = ["OPENSSL_DIR", "LIBGIT2_NO_VENDOR"]       # given to every build script
build-script-link-dirs = ["/opt/vendor/lib"]                 # rustc-link-search may name
```

What a sandboxed script prints is checked before cargo reads it (`cargo/directives.rs`), since
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
manifest. `BLAZE_SANDBOX=off` runs every build script, proc macro and test outside, as cargo
does. Where no sandbox can be made, the script fails with why and the same way out, as a test
run does, and a proc-macro server does not start and the load says why: nothing ever runs
outside instead, nor goes to cargo or rustc. Windows has no sandbox yet, so there a build with
build scripts or proc macros needs `BLAZE_SANDBOX=off`.

Where a user may not make a user namespace, a build script's sandbox is confined with Landlock
instead (Linux 5.13 on). Ubuntu 23.10 on keeps users from them through AppArmor
(`kernel.apparmor_restrict_unprivileged_userns=1`: `/proc/self/setgroups: Permission
denied`), and Docker's default seccomp profile refuses `unshare(CLONE_NEWUSER)` to a process
without `CAP_SYS_ADMIN` ([#420](https://github.com/indexable-inc/blaze/issues/420)). What a
build script is promised holds the same: it writes only its `OUT_DIR` (and the per-user cache
on macOS), reaches no network, sees exactly the environment cargo gave it, and what it read is
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

With `BLAZE_SCRIPT_VIEW=on` (Linux; off until CI pushes the closure to the shared remote) a
build script runs in such a view, its closure (`cargo/hermetic`): its package read-only with
the target directory and `.git` below it hidden, its `OUT_DIR` written, the `OUT_DIR`s its
`DEP_*` variables name, the toolchain and blaze's own rustc, and the pinned nixpkgs closure
at `/nix/store`: clang, lld, binutils, glibc, the Linux headers, pkg-config, bash (also
`/bin/sh`), coreutils, make, sed, awk, grep, diff and find, with a crate's libraries added.
blaze fetches and verifies the closure itself (`blaze-engine closure fetch`, no `nix` binary) and
lays it out from the store or the remote (`blaze-engine closure materialize`). A crate's libraries
are, first, the workspace's:

```toml
[workspace.metadata.blaze.system-deps]
openssl-sys = ["openssl.out", "openssl.dev"]   # nixpkgs attributes of harness/closure_pins.py
```

else the shipped table's (`cargo/hermetic/libs.toml`, by version), else none. Its environment
is cargo's documented variables for scripts (`CARGO`, `CARGO_*` but `CARGO_HOME`, `DEP_*`,
`OUT_DIR`, `TARGET`, `HOST`, `PROFILE`, `OPT_LEVEL`, `DEBUG`, `NUM_JOBS`, `RUSTC`, `RUSTDOC`,
the jobserver's), what it declares with `rerun-if-env-changed`, `build-script-env`, what
`blaze lock --fetches` hands it, and the closure's: `PATH` of its `bin`s, `CC`, `CXX`, `AR`,
`PKG_CONFIG(_PATH)`, `NIX_CFLAGS_COMPILE` and `NIX_LDFLAGS` of its libraries (with
`-ffile-prefix-map` of the package to `/package` and the target directory to `/target`),
`HOME=/homeless-shelter`, `TMPDIR=/tmp`. Never the runner's: no other variable, blaze's own
neither. A run is kept under one key of what it saw: what the script was built from, its
arguments and environment with the checkout's and target directory's paths taken out, its
package's source (`.cargo-checksum.json`, a git checkout's commit, a path package's tree),
each closure path's NarHash and its dependencies' `OUT_DIR` trees; nothing of what it read,
as it can read nothing else. One whose files name where it ran is kept for that place alone.
A failed run says what it was given and what it lacked: a library (the `system-deps` line to
add), a write outside `OUT_DIR` (the way out), a variable. Over the bench corpus 425 of 434
runs succeed in their closure, 24 of them with libraries of the table, and every one is a
store hit on the same machine and from another's store pulled from the remote, at the same
checkout path (`tools/bench/results/2026-10-27-bscache/closure.md`). A script cargo's rustc compiled
is keyed by its bytes, which name `CARGO_HOME`, so another checkout path misses it yet.
`BLAZE_KEY_TRACE=<dir>` writes each key's fields to `<dir>/<key>.txt`, to see what two
machines' keys differ in.

Compiling a build script, running it and building a proc macro are each an entry of the
store, so they travel with `blaze-engine cache push` and `pull` and through the gateway like any
compiled function ([remote-cache.md](remote-cache.md)). Each is kept under two keys
(`cargo/keyed.rs`): the first of what it is declared to be made of, and a second of what it
was seen reading and what each of those has, so `rerun-if-*` is a hint and the record is what
counts. A first key holds up to 16 sets of reads.

- A build blaze makes of a script or a proc macro (`own_script.rs`) is first keyed by the
  rust-project description of it and its dependencies (sources' crates, cfgs, features,
  edition, target) and the toolchain (its channel manifest and the names in its `lib`); then
  by every file the build read (modules, `include!`, `include_str!`), each variable `env!`
  read, its dependencies' proc-macro libraries and the files their dep-info names. Beside the
  executable, `<exe>.made-of` says which build it is.
- A run (`script.rs`) is first keyed by that build (or the executable's bytes, for one rustc
  linked), the toolchain of its `RUSTC`, its arguments and what it was given, with the
  checkout's and the target directory's paths taken out, so another checkout or machine finds
  it; then by every file the sandbox saw it read and every file of its `rerun-if-changed`,
  but those of the toolchain, blaze and its own build. A directory outside its package
  counts as being one, not by its names (git and the loader walk through those). What is
  kept is what it printed and what it left in `OUT_DIR`.

A run whose record is not whole (macOS records nothing of a program it protects, such as
`/bin/sh` or `/usr/bin/cc`) is not kept, and runs whenever cargo would run it. A run outside
the sandbox (`unsandboxed-build-scripts`, `BLAZE_SANDBOX=off`) is never kept, so it is never
pushed either. The daemon's check of a script (`blaze-reads` beside its `OUT_DIR`) needs no
run when what it read is as it was.

`BLAZE_BSCACHE=check` builds and runs everything the store has anyway and compares: a run
that printed or wrote something else, or a build that read files the store's build of it did
not, fails the build. Two builds of one script by blaze are not the same bytes (the order of
functions in the executable is not), so a build is compared by what it read, not by bytes.
With `BLAZE_STATS` each hit says so (`blaze: a run the store kept`, `blaze: a build the
store kept: ...`), where cargo shows what a script printed.

Every proc-macro server a load starts, the toolchain's and blaze's own, runs in the sandbox
too, writing nothing: no `OUT_DIR`, only the empty temporary directories. It is given what a
test is given and blaze's and rust-analyzer's own variables, not the shell's; a crate's
variables come with each expansion. A server is started
once, when a load first needs it, and serves every expansion after, so the sandbox costs its
start (about 8 ms on Linux, 25 ms on macOS) and nothing per expansion. The packages of
`unsandboxed-proc-macros` are expanded by servers of their own, started outside. The hook is
`proc_macro_api::wrap` (the command a server is started with) and
`load_cargo::unsandboxed_macros_with` (which libraries get the outside servers).

What it costs, measured on 2026-10-26 (cold builds with an empty store and target
directory, `BLAZE_SANDBOX=off` against the default; a server's start is its start and exit with
stdin closed, the median of 20):

| | Linux x86-64 (dev host, 16 cores) | macOS (M5 Max, loaded) |
|---|---|---|
| a proc-macro server's start, outside | 6.7 ms | 30 ms |
| the same, in the sandbox | 14.9 ms | 54 ms |
| a 4-package workspace with `ring`, `cc` and `serde`, cold, outside | 27.3 s | 15.4 s |
| the same, in the sandbox | 23.4 s | 15.7 s |
| the game's `space-server` (72 build scripts, 6 of them C or toolchain ones), cold, outside | | 441 s |
| the same, in the sandbox | | 415 s |

A build script's sandbox costs one launch (8 ms on Linux, 38 ms on macOS, as for a test
executable), which is lost in a cold build's noise.

What is left: the server's `--version` question runs outside (it loads no macro); a server
sees the directories of the libraries of the loads before it started, so a workspace below
`/tmp` loaded later by the same daemon is not seen by it on Linux; doctests and the
`blaze-build-script` crate's own runner (`harness/scripts.py` only) are not sandboxed.

### Build scripts under WASI (an experiment, off by default)

A build script can instead be compiled for `wasm32-wasip1` and run under Wasmtime. Nothing
of the host is reachable but what is handed to it, and the module is not built for the host
it runs on. It is off unless asked for, and asking needs a blaze built with the feature `wasi`
(`cargo build --release -p blaze-engine --features wasi`), which adds Wasmtime to it:

- `BLAZE_BUILD_SCRIPTS=wasi` runs every build script so, or
- in the workspace's root manifest, `wasi-build-scripts = ["pkg", ...]` (`["*"]` for all)
  under `[workspace.metadata.blaze.sandbox]` runs those packages' scripts so.

When a load's wrapper (`blaze-rustc`) is asked for a build script it also builds it for `wasm32-wasip1`, from the
same description, beside where rustc links it (`build_script_build.wasm`). The standard
library of that build is the toolchain's with one file of blaze's in place of
`std/src/sys/process/unsupported.rs` (`crates/blaze-wasi/std/process.rs`, `blaze
--run-tool`), so `std::process::Command` works. A command runs on the host when its output
is first needed: `wait`, `output`, a read of its stdout. blaze refuses that std when the
toolchain's file is not the one it was written against. Without `--run-tool` a wasip1 build
keeps rustc's behaviour: `Command` is unsupported.

When cargo runs the script, the module runs in its place:

| | under WASI |
|---|---|
| files | the package's directory, read-only, at its path and at `.`; `OUT_DIR`, writable, at its path; nothing else. `std::env::current_dir()` is `/` (wasi-libc's), not the package's directory, though relative paths are found in it |
| environment | the variables cargo documents for build scripts (`CARGO_*` of the package, features and cfgs, `OUT_DIR`, `TARGET`, `HOST`, `DEP_*`, `RUSTC`, ...) and no other |
| commands | the toolchain's `rustc` (the `RUSTC` it is given, which is blaze's: [above](#build-scripts-and-proc-macros-run-in-a-sandbox)), C and C++ compilers (`cc`, `c++`, `gcc`, `g++`, `clang`, `clang++`, `cpp`, with a target's prefix or a version), their archivers (`ar`, `ranlib`, `llvm-ar`, `llvm-ranlib`) and `pkg-config`: each runs in the sandbox the script would have run in, with the environment the script gave it over the sandbox's for C toolchains (`PATH`, `CC`, Nix's wrappers). Anything else is refused, naming the program: the script sees `PermissionDenied` |
| record | every command, with the program that ran, its arguments, how it ended or why it was refused, in `blaze-wasi-tools.jsonl` beside `OUT_DIR` |

What it printed is checked as a native run's is. A run is kept in the store under the Nix
model's key: the module's bytes, the package's files, the environment and arguments with
the checkout's and target directory's places unplaced, the toolchain, and each command it
may run as `PATH` finds it. There are no read sets. A module blaze can't build, or a run
that fails, is an error: the native script does not run instead.

`BLAZE_BUILD_SCRIPTS=wasi-compare` is for measuring it. The module runs first and what it
printed and wrote is set aside in `blaze-wasi/` beside `OUT_DIR`, then the native script
runs for cargo. [The report](reports/BSWASI.md) compared the two over the bench corpus: which
scripts run unchanged and why the others don't.

### Proc macros run as WebAssembly

By default every proc macro of a load is built by blaze, from its source, as a WebAssembly
module (`wasm32-wasip1`, a WASI reactor), and blaze expands with it in its own process under
Wasmtime and Cranelift (`crates/blaze-macro-wasm`): no rustc-built code runs, and no
proc-macro server is started for it. In blaze's own target directory the module is built
from cargo's unit graph, with the build scripts (`cargo/compile_time.rs`). Elsewhere it is
built from the load's own description of the macro's crate and what it depends on
(`load_cargo::described`), whoever compiled the macro's library. Either build is kept in the
store like a build script's. The macro's `proc_macro` crate is a shim
(`crates/blaze-macro-wasm/guest/proc_macro.rs`) whose handles go to the host by imports, so
`syn`, `quote` and `proc-macro2` build unchanged. A module is linked with the wasi-libc of
the toolchain's `wasm32-wasip1` target. When the toolchain does not have that target, blaze
runs `rustup target add wasm32-wasip1` for it, once.

A module sees only what cargo documents for a proc macro:

- Its package's directory, read-only, at `/blaze/package`: `CARGO_MANIFEST_DIR` names it, and
  `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), ..))` or a `std::fs` read below it
  works. Nothing else of the machine is there. A write fails with `EROFS`, a path out of the
  package with `ENOTCAPABLE`.
- The variables cargo sets for a crate (`CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`,
  `CARGO_PKG_*`, `CARGO_CRATE_NAME`, `CARGO_PRIMARY_PACKAGE`, `CARGO_BIN_NAME`, `OUT_DIR`), never
  the environment of the shell or the daemon.
- A clock that reads 0 and random bytes that are the same on every run.

Each calling crate gets its own module instance for a database revision. Its macros share
guest statics, stored tokens, source locations and open package files. Every new database
revision starts a fresh session, including a client request without source changes. Before cached bodies are checked, its declarations expand
again in collection order, including unchanged registration attributes. A trap fails that
session until the next revision.

Expansion outputs are not cached: finding an earlier output would skip the macro's state
changes. The compiled module's machine code remains cached for the host that compiled it.
Frozen declaration, impl and language-item artifacts are disabled for crates whose
declarations call these macros: those artifacts do not contain the macro's state. Depending
on a macro crate alone does not disable frozen artifacts or force declaration expansion.
Actual callers repeat declaration expansion every revision; compiled function caching remains enabled.
The list of a module's macros is kept in a `.macros` file beside it; a load compiles nothing
until a macro first expands.

A proc macro that can't be built or loaded as a module fails the load, naming it:

    error: the proc macro `my_derive` cannot run as WebAssembly: ...
      name its package in `native-proc-macros` under [workspace.metadata.blaze] in /home/me/ws/Cargo.toml to expand it natively, or set BLAZE_PROC_MACRO_WASM=0 for every proc macro

It is never expanded natively instead. The ways out, in the workspace's root manifest:

```toml
[workspace.metadata.blaze]
native-proc-macros = ["sqlx-macros"]   # by package name
```

A package listed there, and every proc macro under `BLAZE_PROC_MACRO_WASM=0`, is expanded
as before: rustc's library, by the toolchain's proc-macro server in the sandbox
([above](#build-scripts-and-proc-macros-run-in-a-sandbox)). A value that is not a list of
names is an error.

### Running only the tests that have not passed

`blaze test` runs, of each test executable the daemon built, the tests that have not passed
with what they are a function of as it is now (`BLAZE_TESTS=affected`, the default;
`BLAZE_TESTS=all` runs every test, as cargo does). A pass is kept in blaze's store, as a
compiled function is, so it travels with the store through a remote (`blaze-engine cache push|pull`,
`BLAZE_REMOTE`): a pass on main's CI stands on every branch whose test and inputs are the same.
Beside each test executable, `tests-passed` has the keys of its hermetic tests' passes this
machine found or kept, and `tests-programs` the digest of each program an integration test is
handed (`CARGO_BIN_EXE_<name>`) by its size, time and inode: a command whose tests all passed
before neither opens the store nor reads a program again (`BLAZE_TESTS=verify` asks the store).

A hermetic test's pass is kept by its code alone, sandboxed or not
([below](#tests-are-hermetic-or-say-why-not)). A test that reaches the world outside the
program (files, the environment, other processes, the network) is kept only in a workspace
that opts into sandboxed tests ([above](#sandboxed-tests-an-experiment-opt-in)), where the
sandbox records what it read and that is part of its key; elsewhere it runs every time, as does
a test that reaches what nothing records (foreign code, assembly, a static other tests change,
code blaze can't compile).

    Running unittests src/lib.rs (target/debug/deps/tsel-1f2e): 2 of 5 tests
    Cached tests/it.rs (target/debug/deps/it-9a8b): 2 tests, passed before as they are now
    Cached 5 tests from commit 2bfa62d8ce (run 18834512), 2 from an earlier run on this machine; 1 test run every time (`blaze test --why` says why); `BLAZE_TESTS=all` runs every test

At each build the daemon hashes every test's code with the code of every function it calls,
directly or through others, in any crate of the workspace or its dependencies, and its
attributes (`#[ignore]`, `#[should_panic]`). The standard library is named by the toolchain.
A function of a crate outside the workspace, instantiated with types of such crates, is hashed
by its stable name, which stands for its code and all it calls as it does for the compiled
functions the store keeps; what the hash needs of what is below it (whether that reaches code
blaze can't compile, or the world outside) is kept in the store under the name, so a daemon
that starts with the store compiles none of the dependencies to hash the tests.
A test's pass is kept under a key of that hash and of what else it is a function of:

- the blaze that ran it (every key of the store has it);
- the content of each program cargo hands an integration test as `CARGO_BIN_EXE_<name>`;
- the environment the sandbox gives it (below);
- what the command builds it with: features, `--profile`, `--target`, `--config` and `-Z`
  flags, and the machine's architecture and system;
- the harness's flags that change how it runs (`--nocapture`, `--test-threads`; not `-q`,
  `--color` or `--show-output`, which change only what is shown);
- for a test that reaches what its code does not decide (a file, the environment, another
  process, the network), what it read: every path the sandbox saw it open, look up, list or
  run, and what is there now (a file's bytes, whether it can be run, a link's text, a
  directory's names, or that nothing is). Of a path it only looked at (`stat`, `access`,
  `chdir`, `readlink`) the key has what that could tell: what is there, whether it can be run,
  a link's text and a file's size. A test run looks at `target/debug`, whose names change
  as the daemon writes there after a build; keyed by them, a pass missed once after each fresh
  target. What a stat says of times (`mtime`) is not in the key: a test
  that decides by a file's time is as one that decides by the clock.

That last part is found in two steps, as a build cache finds what a step read: the store keeps,
under the rest of the key, the paths a run that passed read (up to four sets, for a test that
reads other paths as what it reads differs), and the pass under those paths with what was at
each. A test is skipped when one of the sets, with what is at its paths now, has a pass. Only a
pass is kept; a test that failed runs until it passes, and once it does, a return to code that
passed before is skipped again, as a build is. `-v` says why each test that reaches outside its
code runs:

    Sandboxed tests::reads_a_file: it read `/home/me/tsel/data.txt`, which changed

A test that is not hermetic and that no sandbox makes so runs every time and is never kept: one
that reaches what nothing records, a benchmark, one that reaches the world outside and runs
outside a sandbox (its package takes it out, its workspace has not opted in, or
`BLAZE_SANDBOX=off`), and one whose reads are not all known (a program macOS starts without the
interposer). The summary says how many there are. `blaze test -v` says, under each test
executable, why each test it runs every time does (`Always`), by the fewest calls from the
test to it, and `blaze test --why` says it of every test without running any.

Of a test executable that fails, the tests libtest's summary lists as failed, and those the
sandbox denied something, are not kept; the others that ran are. One that ends with no summary
(a crash, or a test that exits the process), or whose summary counts other tests than it was
asked to run, has nothing kept. Its output is passed on through a pipe as it comes, colored as
libtest would color it on the terminal.

The environment of a sandboxed test under `affected` is cut down to the variables cargo sets for
a test, `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LANGUAGE`, `LC_*`, `TZ`, `CARGO`,
`CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `LD_LIBRARY_PATH`, `NIX_LD`,
`NIX_LD_LIBRARY_PATH`, `RUST_BACKTRACE`, `RUST_LIB_BACKTRACE`, `RUST_MIN_STACK`, and those
`BLAZE_TEST_ENV` names, by commas (under `all` a test gets the whole environment). Paths are
kept as they are, so a pass stands where the checkout, the target directory and the home are
at the same paths: every run of a CI job is, and two checkouts of one machine are not.

What is not in a key, and so is caught only as a flake: the time, randomness (a `HashMap`'s
seed too) and threads; the kernel's views (`/proc`, `/sys`, which say what the machine and the
process are, as the number of cores does); a file's times and owner. A test that passed and
failed under one key is flaky: the store marks it, it runs every time after, and the command
warns with where it passed. `BLAZE_TESTS=verify` picks the tests as `affected` does, runs them
all, and says whether each kept as passed passed again:

    Verified the passes kept: 245 of 245 tests passed again

One that failed is flaky or its key misses something it is a function of, which is a bug in
blaze; it is named, marked flaky, and the command fails. It is for the release train, which
does not run it yet.

On a CI runner's shape (a fresh target directory and daemon, with the store main's run filled),
every test that passed on main is skipped. After a one-function edit, only the tests that reach
the function run: just ran 344 of its 515 passing tests and took 171 from main, nushell 42 of 67,
cargo none of 8, and `verify` found every pass kept to pass again on seven projects. On these
projects it is not yet faster than running every test, whose tests are short: the walk for the
tests' hashes on a fresh daemon takes 0.94 s for axum and 0.45 s for just, down from 6.2 s and
7.6 s before dependencies were hashed by name, and the run takes 13.5 s against 11.9 s for every
test (axum) and 32.3 s against 28.8 s (just;
[the numbers](../tools/bench/results/2026-10-27-testcache/README.md)).

`-v`, `-q` and `--color` only change what cargo prints, and are the same build to the daemon.

A test executable cargo built, a command that names tests or passes the harness other flags
than `-q`, `--nocapture`, `--show-output`, `--test-threads` and `--color`, and one that runs
the ignored tests run as asked: every test, and nothing is kept. `all` runs every test, as
cargo does; the daemon then hashes nothing. With no store (`BLAZE_CACHE=off`)
`affected` keeps nothing and runs every test. `BLAZE_TESTS_FROM` names where the passes a
command keeps came from, for its summary; on GitHub Actions it is the commit and run
(`GITHUB_SHA`, `GITHUB_RUN_ID`). What is left is
[#227](https://github.com/indexable-inc/blaze/issues/227).

### Tests are hermetic, or say why not

"Instead of sandboxing tests, do the opposite: look at MIR, detect whether a test is hermetic,
don't rerun it; keep a list of non-hermetic tests and why, so people (or an LLM) can work out
how to speed up the repo" (Andrew, 2026-10-28). So by default, with no sandbox and nothing to
opt into, blaze reads each test from the code it compiled: the walk above, from the test
through every function it can call, finds what the test reaches that its code does not decide,
by kind. A test that reaches none of the kinds that block is hermetic, and its pass is kept by
its hash. Nothing is guessed pure: what blaze can't see into blocks.

| kind | what it is | blocks |
|---|---|---|
| `fs` | a file call (`open`, `stat`, `readdir`, `mkdir`, `getcwd`, ... and their Windows names) | yes |
| `env` | `getenv`, `environ`, `setenv` and the like | yes |
| `net` | `socket`, `connect`, `bind`, `accept`, `getaddrinfo` | yes |
| `process` | starting a process (`posix_spawn`, `fork`, `execve`, `CreateProcessW`), or ending this one (`exit`, which ends every other test of the run with it) | yes |
| `ffi` | a foreign function or static declared outside the toolchain that is not known pure (the allocator, `mem*`, libm, where `errno` is: `__errno_location`, `__error`) nor one of the above: its code is not blaze's to see | yes |
| `asm` | assembly in a function outside the toolchain | yes |
| `indirect` | a function pointer made of data (a `transmute` to a `fn` type outside the toolchain): what it calls is no function the walk found | yes |
| `uncompiled` | a function blaze can't compile, whose code tells nothing | yes |
| `global-state` | a static outside the toolchain that can change: `static mut`, an atomic, a `Mutex`, a `Cell` (a `LazyLock`, `OnceLock` or once_cell's or lazy_static's cell only when what it holds is one of those) | yes |
| `clock` | `clock_gettime`, `mach_absolute_time`, `QueryPerformanceCounter` (`Instant`, `SystemTime`) | no |
| `random` | `getrandom` and the like, and the seed std's `HashMap` makes | no |
| `thread` | starting a thread (`pthread_create`, `CreateThread`) | no |

What was decided, and why:

- The toolchain (std, core, alloc and the crates they are built of) is named by its commit.
  Its foreign calls count only where the table names them; the rest (`write` to a pipe,
  `futex`, `mmap`) give the same for the same input as far as a test can tell. std's
  `RUST_MIN_STACK` read when a thread starts is left out: it sizes the stack, which a test sees
  only by running out of it. So is what a panic's message and a backtrace read: what a failure
  prints is not how it ends.
- The clock, randomness and threads don't block. A test whose pass turns on them is flaky,
  run again or not; kept by its code, it is caught as one ([above](#running-only-the-tests-that-have-not-passed):
  passed and failed under one key, `BLAZE_TESTS=verify`). They are told, as
  `nondeterministic`, so a slow test that sleeps or races is found.
- `HashMap`'s random seed is not made fixed under `blaze test`. A fixed seed is not invisible
  to a correct program: one that prints a map, or hashes in the order a map gives, would print
  the same order every run under `blaze test` and another under `cargo test`, and std has no
  way to set the seed from outside but to replace `RandomState`'s code. So the seed is its own
  reason, `random`, which does not block: a test whose pass depends on a map's order is flaky.
- A thread-local is no shared state: libtest runs each test on a thread of its own.
- A static set once by code the test reaches (`LazyLock<Regex>`) is set to what that code
  gives; its code is in the hash. One that holds a lock, a cell or an atomic, or any other
  static that can change, can be changed by another test of the run, and a test whose pass
  depends on which ran before it is not kept by its code: `global-state`.
- A call through a `dyn` or a function pointer is bounded: the code reached makes every vtable
  and takes the address of every function it can call through, and those are walked. What is
  not bounded is a pointer made of data (`indirect`) or handed over by foreign code (`ffi`).
- A function chosen once by the CPU and kept in a static (memchr's runtime dispatch: `static
  FN: AtomicPtr<()>`, set to the fastest of its versions on the first call, then loaded and
  `transmute`d back to a `fn` on every call) is neither `global-state` nor `indirect`. What the
  static holds is one of the functions the code stores in it, and the test's hash has each of
  them; which one the CPU picks counts as std's own choice by the CPU does
  (`is_x86_feature_detected!`): not at all. blaze takes a static to be one only when it is
  sure no other value can get in: an `AtomicPtr` (not `static mut`) declared in a block of a
  function body, its first value a function's address, and every body that can name it (that
  function, its closures and the items in its blocks) only loads it or stores a function's
  address in it, never one handed in as an argument or returned by a call. Otherwise it is
  `global-state`, and the `transmute` of what is loaded from it `indirect`. On ripgrep this
  took the tests kept from 191 of 1144 to 363.
- What blaze made of a test is kept beside its program (`tests-reach`), with the build of blaze
  that made it (`tests-reach-by`). Another build of blaze tells tests by its own rules, so it
  walks every test program again the first time it builds it.
- A test the sandbox records (a workspace that opts into sandboxed tests) that blocks only by
  `fs`, `env`, `net` or `process` is kept with what it read, as before. One that blocks by any
  other kind runs every time, sandboxed or not.

Every `blaze test` writes what it found of each test it was asked about to
`target/blaze/tests.ndjson`: a line a test, written as each test executable finishes, and the
whole file written again at the end sorted by package, target and name, so two runs diff line
by line. Each line is one JSON object of the same schema, version `v` 1:

```json
{"v":1,"package":"tsel","target":"lib","name":"tests::reads_a_file","hermetic":false,"cached":"never","result":"passed","flaky":false,
 "reasons":[{"kind":"fs","what":"open64","why":"it reaches `open64`, which reads or writes files",
   "chain":[{"fn":"tsel::tests::reads_a_file","file":"src/lib.rs","line":128,"col":17},{"fn":"std::fs::read_to_string::<…>","file":null,"line":null,"col":null},{"fn":"open64","file":null,"line":null,"col":null}]}],
 "nondeterministic":[]}
```

(one line in the file; wrapped here). The fields:

| field | value |
|---|---|
| `v` | the schema's version, 1 |
| `package`, `target`, `name` | the package, `lib` or `bin:<name>`, `test:<name>`, `bench:<name>`, `example:<name>`, and libtest's name of the test |
| `hermetic` | whether no reason blocks: its pass is kept by its code |
| `cached` | `hit` (passed as it is now, not run), `miss` (it runs, and a pass is kept), `never` (it runs every time) |
| `result` | `passed`, `failed`, `cached`, `unknown` (its executable ended without saying) or `not-run` (`--why`) |
| `flaky` | it passed and failed under one key |
| `reasons` | what blocks, the first of each kind found, by the fewest calls |
| `nondeterministic` | `clock`, `random` and `thread`, found the same way |
| `reasons[].kind`, `what`, `why` | the kind (or `benchmark`), what by name (a symbol, a static, a function), and a sentence |
| `reasons[].chain` | the calls from the test to it: each `fn` without its type arguments, and the `file`, `line` and `col` where it calls the next (a file of the workspace relative to it, `null` where it is not told) |

`target/blaze/tests.summary.json`, written beside it at the end, has the counts (`tests`,
`hermetic`, `not_hermetic`, `cached` and `results` by value, `by_kind`: how many tests each kind
blocks) and two rankings, the most first: `top_reasons`, each kind and `what` with how many
tests it blocks and the first of them; and `top_helpers`, the last function of the workspace on
the way to a reason, with how many tests reach a reason through it and how many reach every
reason that blocks them through it (`only_through_it`): those are hermetic once it no longer
reaches any, which is where to start to make a repository's tests faster.

`blaze test --why` builds as `blaze test` does, runs no test, writes the same files (`result`
`not-run`) and prints a table: how many tests are hermetic, the top reasons and helpers, and
each test that is not with its reasons. `blaze test --why --json` prints the lines of
`tests.ndjson` to stdout instead.

While cargo builds, a spare worker already loads the workspace, resolves it and compiles the
functions of each crate cargo is done with on a quarter of the cores, and reads the symbols of
each library as cargo writes it. When cargo is done it reads the symbols of the executable
rustc linked, and the daemon is ready: the first edits are patches spliced into that
executable. The daemon builds its own base in the background once nothing has waited for a
second, and gives it up when that would take more than a quarter of the machine's memory
([architecture.md](architecture.md#the-first-edits-patch-what-rustc-linked)). On Linux
x86-64 (16 vCPUs, 8 cores), from an empty store, in interleaved runs:

| Project | `cargo build` | `blaze build` | Ready | First edit |
|---|---|---|---|---|
| just | 7.6 to 7.7 s | 8.0 to 8.3 s | 8.4 to 8.7 s | 0.5 s, a patch |
| ripgrep | 5.0 to 5.4 s | 5.4 to 5.7 s | 5.6 to 5.9 s | 1.4 to 1.5 s, a patch |
| nushell | 34.7 to 37.0 s | 37.4 to 38.9 s | 39.0 to 40.5 s | 1.7 to 2.4 s, a patch |

The command takes longer than cargo's because of the spare worker beside it: what it reads
and resolves of the standard library (some 10 core-seconds on every project) and of the
workspace takes cores from cargo, and on this machine every second core is a hyperthread of
another, so a core cargo leaves idle is still not free. With `BLAZE_SPARE=0` the command is
within a few percent of cargo's, and the daemon is ready later, as it loads the workspace only
then: 11.8 s for just, 8.2 s for ripgrep and 46.7 s for nushell. ripgrep's first edit, in
`main`, moves the lines of a generic function below it, which rustc copied into its callers:
the patch replaces those copies too. On macOS that patch's frames (244 KB) are more than the
unwind table of rustc's link (218 KB) they would take the place of, so the splice moves both
tables into the room the link leaves after its load commands, and the edit is a patch there too.

A spare worker is started for a build: one as the daemon starts, for the build that started
it, and one each time a build of a workspace the daemon has no worker for begins. Once a
workspace is loaded, none waits for the next: a spare with the library loaded holds 1.3 GB.
What that costs is the start of a second project whose program cargo has already built, as
the spare loads the library then rather than before. `BLAZE_SPARE=ready` keeps one loaded at
all times, as the daemon did until October 2026. On Linux x86-64 (16 cores), a second project
under a running daemon, medians of three interleaved rounds (`harness/spare.py`):

| Project | Daemon, idle | Cold start | Warm start | Daemon after the warm start |
|---|---|---|---|---|
| ripgrep | 1,275 MB (2,635 with `ready`) | 4.86 s (4.74 s) | 2.62 s (1.29 s) | 3,652 MB (4,918 MB) |
| fd | 1,267 MB (2,638) | 24.17 s (24.21 s) | 3.48 s (2.19 s) | 3,354 MB (4,543 MB) |
| nushell | 1,282 MB (2,638) | 30.70 s (30.48 s) | 9.51 s (8.62 s) | 7,219 MB (8,699 MB) |

The cold start is `blaze build` from an empty target directory until the daemon is
ready; the warm start is the same once cargo has built it, the daemon stopped and started
again. With `BLAZE_SPARE=0` the daemon holds the same as without the variable, a cold start
waits for the workspace to load after cargo (7.14 s on ripgrep, 38.43 s on nushell), and a
warm one is quicker (2.14 s, 6.83 s).

## When cargo builds instead

blaze hands a build to cargo whenever it cannot be sure its own result would be what cargo's
is. It prints one line with the reason, `blaze: bug: <reason>; building with cargo, which
blaze is never to do (<step>, and #84)`, and counts it in `blaze status`. Each is a bug:
blaze is never to build with cargo
([#84](https://github.com/indexable-inc/blaze/issues/84)); the line names the step of
[no-rustc.md](no-rustc.md) that removes it, M ([#93](https://github.com/indexable-inc/blaze/issues/93))
for an edit's build and K ([#91](https://github.com/indexable-inc/blaze/issues/91)) for a first
build; a reason of the test harness names H ([#88](https://github.com/indexable-inc/blaze/issues/88)), and a
function blaze does not compile whose rustc copy is from before the edit names E
([#85](https://github.com/indexable-inc/blaze/issues/85)). The build then behaves as cargo's because it is cargo's.

A whole invocation goes to cargo when any of these hold (`Invocation::only_cargo` in
`cargo/cli.rs`); for `check` only `--build-plan` and `--unit-graph` do:

| Condition | Reason printed |
|---|---|
| `--build-plan` or `--unit-graph` | `--build-plan and --unit-graph are cargo's to answer` |
| `--message-format` | `--message-format` is cargo's to answer |
| a `-Z` flag | unstable cargo flags |
| `--exclude` | `--exclude` is not loaded by the daemon yet |
| more than one `--target` | several targets in one build |
| `--target` or `CARGO_BUILD_TARGET` naming a target that is neither ELF nor Mach-O, or one blaze does not know | programs for the triple are not linked through `blaze` yet |

A single build goes to cargo when:

- a manifest, `Cargo.lock`, a cargo configuration file, or something a
  build script reads has changed since cargo last built (a source a build script makes newer
  before cargo runs rustc on it, as biome's do, has not). Cargo builds, and the daemon loads
  the workspace again afterwards. On a large workspace that reload is long: 36 to 51 s on
  nushell;
- rust-analyzer finds an error in the source. The daemon refuses it, cargo runs, and you get
  rustc's diagnostics and cargo's exit status. This costs 0.1 to 0.5 s over cargo alone.
  Errors rust-analyzer does not find are not caught here; see "Known problems";
- the daemon refused the build or failed.

Doc tests are blaze's, with no cargo, rustc or rustdoc (`doctest.rs`). An example blaze
cannot build fails, saying why; nothing is left to rustdoc. Of the test harness's flags, `-q`,
`--format pretty`, `--format terse`, `--color never`, `--exact`, `--skip`, `--test-threads`
and test filters are handled, and the results are shown as libtest shows them; any other flag
fails the library's doctests with the flag named. `blaze test` writes down the run of rustdoc
cargo would make for each library (`cargo/whole.rs`), then one run of blaze loads the
workspace, with blaze's own build scripts and proc macros, and tests the libraries' examples
in cargo's order, each in the directory and with the variables cargo gives its rustdoc, and
says `Doc-tests` and what failed as cargo does. The daemon does not run doctests yet.

An example marked `compile_fail` passes when blaze's front end finds an error in it that
rustc gives too: one of the errors `blaze check` is as sure of as rustc
(`SURE` in `lib.rs`), the borrow checker's, or a lint the example denies, and, as rustdoc
checks, every error code it is marked with (`compile_fail,E0499`). blaze's checker is weaker
than rustc's here: it reports errors it is not sure rustc gives (a trait bound not met,
`E0277`, is not in `SURE`), misses others, and the borrow checker skips bodies MIR lowering does
not support. Where it can't tell, the
example fails with the reason and is counted in the `doctests:` line as one blaze can't tell
fails as marked; it never passes on a guess.

The fallback is per invocation or per program, not per crate. If one program of an
invocation cannot be the daemon's, cargo builds the invocation.

These flags are handled on the daemon's path: `--target` naming the host, a single `-p`,
`--features`, `--bin`, `--example`, `--test`, `--lib`, `--manifest-path`, `--workspace`,
`--no-run`, `--no-fail-fast`, `-q`, test filters and the arguments after `--`.

The daemon's state is kept per command line. `blaze build`, `blaze build -q`
and `blaze build --features x` are three different builds to blaze, told apart by a
hash of the subcommand (`run` counts as `build`), the build flags, the working directory
and the environment variables that change what cargo builds. The first use of each is a
cargo build, usually a quick one because cargo's own output is shared between them.

## The daemon

There is one daemon per user. It is started by the first `blaze` that needs it, in a
session of its own, and it outlives the command that started it. It exits after 30 minutes
without a request; `BLAZE_IDLE_SECS` sets another time and is read when the daemon starts
(0 means never).

The daemon holds no compiler state itself. It owns worker processes, each holding one
rust-analyzer database for one repository, toolchain, target, feature set and `cfg(test)`
setting. Git worktrees of one repository share a worker, so what they have in common is
analysed once. Different repositories get different workers.

A new worktree still starts with a cargo build, because it has a target directory of its
own. What it saves is the daemon's side: the worktree joins the worker that is already
loaded instead of loading the workspace again. With a ripgrep checkout loaded, the first
`blaze build` in a new worktree of it was cargo's and took 16 s, the worktree showed
as ready under the same worker, and the first edit there was built by the daemon in 0.4 s. A `blaze test` uses
one worker: its unit tests and its integration tests are crates of one load.

`blaze status` shows the daemon, its workers and their log files:

    daemon: pid 10969, version 0.0.0-3da7208-18df73efa25a74db, exits after 1800s idle, workers hold at most 32768 MB
      worker: pid 13169, /dev/shm/ripgrep/, holds 1312 MB, log /home/user/.cache/blaze/workers/f30b80a4e02dffde/log
        /dev/shm/ripgrep: ready
    /dev/shm/ripgrep (dev, 1f59ed7b049b7fa0): 3 builds by the daemon, 0 by cargo

A worker for a large workspace holds a lot of memory: 1.3 GB for ripgrep and 9.4 GB for
dioxus-cli at the end of the agent-style edit runs in [results.md](results.md). The workers
together hold at most half of the machine's memory, or what `BLAZE_MEMORY_MAX` says. On
Linux the machine's memory is the least of what it has and what the control group the daemon
was started in allows (a container's, a CI job's, a systemd unit's). Once a
second, over that, the daemon stops the worker asked longest ago, and the next build of its
workspace is cargo's while a worker loads it again. A worker that is answering or loading is
never stopped, nor is the one asked last, nor any while a program takes live edits.

A client and a daemon built from different sources do not talk to each other. A daemon that
gets a build request from another version stops its workers and exits, and the client
starts its own.

### Files

The daemon's directory is `$BLAZE_HOME` if set (which moves blaze's toolchains there too,
[Install](#install)), else `$XDG_RUNTIME_DIR/blaze`, else `~/.cache/blaze`. It holds
`daemon.sock`, `daemon.sock.lock`, `daemon.log`, `workers/<id>/` with each worker's socket and log, and `states/`. The directory must belong
to you and be closed to group and others (mode 0700); otherwise the client and the daemon
refuse to use it. The socket is 0600, and a connection from another user is refused before
its request is read.

In a project, blaze keeps its records next to cargo's output, so `cargo clean` removes
them too:

    target/<profile>/.blaze-units/     what a load's wrapper recorded of each crate
    target/blaze/<key>/                state of one command line, and the daemon's objects

A clean while the daemon runs (`blaze clean`, `cargo clean` or `rm -rf target`) tells
it nothing, and it needs no telling: the worker keeps what it loaded, and before each build
looks at what it wrote under the target directory. The standard library's unit it linked
against, gone or another file than it wrote, is written again, from the store when it has it,
and a program whose object is gone is built in full again, by the same worker.

On Linux the object with all of a program the daemon compiled, `programs/<unit>/blaze.o`, is
kept compressed as `blaze.o.zst` between the links that read it, a sixth of its size, and
inflated again for the next: the executable has the debug information, so nothing else reads
it. With `split-debuginfo` set, or on macOS, where a debugger reads the object, it stays as it
is.

The store is on by default: `$XDG_CACHE_HOME/blaze/store`, else `~/.cache/blaze/store`,
limited to a tenth of its disk (at most 32 GiB). `BLAZE_CACHE=<dir>` moves it,
`BLAZE_CACHE=off` disables it, `BLAZE_CACHE_MAX` sets the limit.

## Pinning a release of blaze

A project pins blaze, not Rust: each release of blaze builds with the Rust it ships, and
`rust-toolchain` files are not read ([toolchain.md](toolchain.md)). `blaze pin` writes the
version of the blaze it is into the workspace's root `Cargo.toml`:

    [workspace.metadata.blaze]
    version = "0.20261027.0"

(the rest of the manifest is kept as it was; in a package with no `[workspace]` that adds
one, which makes it the root of its own workspace, as it is anyway when no other is above
it). From then on the `blaze` (or `blaze`) anyone runs in that workspace runs that
release, as Bazelisk runs the Bazel a `.bazelversion` names: when it is another release
itself, it runs `~/.blaze/versions/<version>/blaze` (`$BLAZE_HOME/versions` with `BLAZE_HOME`)
with the same arguments, and when that is not there it first downloads
`blaze-v<version>-<host>.tar.gz` from the release on GitHub, checks its sha256 against the
release's `SHA256SUMS` and unpacks it there. The network is used only then, once per
release. A pinned release that can't be fetched (offline, no such release, no build for
this host, another sha256) is an error, and no other blaze builds in its place.
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

These are read by `blaze` and passed to the worker it starts. They are part of which
build a command line is, so changing one starts from a cargo build again.

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_BACKEND` | `cranelift` or `llvm`, whatever the profile asks for. `llvm` needs a blaze built with `--features llvm`, as the released ones are | the profile's ([below](#backends)) |
| `BLAZE_OPT` | LLVM optimisation level, 0 to 3, whatever the profile's `opt-level`. Functions are still compiled one at a time | the profile's; 2 with `BLAZE_BACKEND=llvm` set |
| `BLAZE_EMIT` | LLVM instruction selection: `optimized` or `fast` | `optimized`; `fast` at level 0 |
| `BLAZE_INLINE` | With LLVM at level 2 or 3, a program built whole (`runbench.py`'s builds) has the functions it calls in any crate built into it or shown to LLVM, as a release build does; the standard library's only when they are generic or `#[inline]`, as rustc does across crates. `crate` keeps that to the function's own crate, `0` builds nothing in, `all` does it in other builds with LLVM at level 2 or 3 too (never the daemon's, whose store keeps each function by its own code alone) | across crates in a whole program, the function's own crate otherwise |
| `BLAZE_SPLICE` | `0` links in full after every edit instead of splicing | splice |
| `BLAZE_PROC_MACRO_WASM` | `0` expands proc macros natively, with rustc's libraries, rather than as WebAssembly ([above](#proc-macros-run-as-webassembly)) | WebAssembly |

`BLAZE_BUILD_SCRIPTS` (`wasi`, `wasi-compare`, `native`) is read by a load's wrapper and by each
run of a build script ([above](#build-scripts-under-wasi-an-experiment-off-by-default)).

`BLAZE_TESTS` is read by each `blaze test` alone and is not part of which build it
is: `affected` runs the tests that have not passed as they are now, by the passes the store
keeps ([above](#running-only-the-tests-that-have-not-passed)), `verify` runs those too and
checks that they pass again, `all` every test. The default is `affected`. `BLAZE_TESTS_FROM` names
where the passes a command keeps came from, for the summary of a command that finds them.
`BLAZE_SANDBOX=off` runs every test outside the sandbox
([above](#sandboxed-tests-an-experiment-opt-in)), and `BLAZE_TEST_ENV` names more variables a test
is given under `affected`, by commas; both are read the same way.

These are read by the daemon when it starts, from the environment of whichever command
started it. To change one, run `blaze stop` first.

| Variable | Meaning | Default |
|---|---|---|
| `BLAZE_HOME` | the daemon's directory (also read by every client) | see above |
| `BLAZE_IDLE_SECS` | seconds without a request before the daemon exits; 0 is never | 1800 |
| `BLAZE_MEMORY_MAX` | the most the workers hold together, a size such as `6G`, or `off` | half the machine's memory (on Linux, or of what its control group allows) |
| `BLAZE_SPARE` | `ready` keeps a spare worker with the library loaded at all times, for the next project; `0` starts none, and a workspace loads once cargo has built it | a spare for each build of a new workspace, as it begins |
| `BLAZE_OWN_BASE_MEMORY` | the most a worker may hold while it builds its own base in the background, a size such as `40G`, past which it gives that base up and edits patch rustc's executable; `off` is no bound | a quarter of the machine's memory (on Linux, or of what its control group allows) |
| `BLAZE_LRU` | how many values of the queries a build in full holds the most of a worker keeps once the build is over, `mir=N,runtime=N,compiled=N,inlinable=N` (lowered MIR, runtime and monomorphized MIR, each instance's code, bodies to inline); 0 keeps every value. One let go is computed again when an edit asks for it ([#261](https://github.com/indexable-inc/blaze/issues/261)) | `mir=2048,runtime=2048,compiled=16384,inlinable=8192` |
| `BLAZE_CACHE` | directory of the disk store, or `off` for none | `blaze/store` in the user's cache directory ([cache.md](cache.md)) |
| `BLAZE_REMOTE` and the other store variables | see [cache.md](cache.md) | not set |

The disk store is on by default and shared by every daemon and checkout of the user. The
daemon reads `BLAZE_CACHE` once, when it starts: a daemon started by a shell without the
variable keeps the default store whatever later shells have set, so run `blaze stop`
before changing it.

## Backends

A build compiles with the backend its profile's `opt-level` asks for, as cargo resolves the
profile: the `[profile]` tables of the root manifest, the configuration's and `--config`'s over
them, `CARGO_PROFILE_<NAME>_OPT_LEVEL`, and `inherits`. A custom profile that inherits
`release` is built as `release` is, unless it sets its own `opt-level`.

| `opt-level` | Backend | Profiles that have it by default |
|---|---|---|
| `0` | Cranelift | `dev`, `test` |
| `1` | Cranelift: rustc's `-O1` has no LLVM pipeline blaze runs to match it | |
| `2` | LLVM at `-O2` (`BLAZE_OPT=2`) | |
| `3` | LLVM at `-O3` (`BLAZE_OPT=3`) | `release`, `bench` |
| `s`, `z` | LLVM at `-O2`: blaze's LLVM has no pipeline for size yet, so the code is not smaller than at `-O2` | |

`BLAZE_BACKEND` set is the backend whatever the profile (`BLAZE_BACKEND=cranelift blaze build
--release` builds a release with Cranelift), and `BLAZE_OPT` set is LLVM's level. The level
belongs to the build as a whole, the root profile's: a `[profile.release.package.<name>]`
`opt-level` changes nothing. A blaze built without `--features llvm` refuses a profile that
asks for LLVM, saying so, rather than building it with Cranelift. The released binaries have
the feature on every target.

The backend and its level are part of a worker's environment, so a dev and a release build of
one project are kept by two workers, and neither evicts the other's state. The store keys each
function's code by the backend and level that compiled it as well: code Cranelift compiled is
never given to a build with LLVM, nor `-O2`'s to `-O3`'s.

A build with LLVM takes longer than one with Cranelift. A first build takes the most longer,
because LLVM compiles every function then
([#807](https://github.com/indexable-inc/blaze/issues/807)).

## Known problems

- **The daemon's verdict is the build's, and it is not yet all of rustc's.** An error that
  rust-analyzer or blaze's borrow checker finds in the edited files fails the build, written
  as rustc writes it, with cargo's status; no rustc runs to say it again
  ([#84](https://github.com/indexable-inc/blaze/issues/84)). What they do not find is
  built: rust-analyzer reports few of rustc's errors for certain, and the borrow checker
  passes part of rustc's tests ([borrowck.md](borrowck.md)). Code rustc builds that blaze
  takes for an error fails the build too; that is a blaze bug to report. A lint the source
  denies fails it as well, for the lints rust-analyzer finds only where rustc does
  (`non_snake_case`, `non_camel_case_types`, `non_upper_case_globals`, `unused_must_use`),
  unless `--cap-lints` or `--force-warn` is among rustc's flags. `blaze check` says
  what such a build would, when the daemon has it loaded, and what it finds in the whole
  workspace otherwise.
- Linux x86-64 and macOS arm64 have been run; on macOS only ripgrep, fd and hyperfine and
  the generated workspace of the harness. On Windows x86-64 (MSVC) builds go through the
  daemon and link with `rust-lld`; that was run by hand on a toy crate and `demo/life`, and
  this harness has not run there ([platforms.md](platforms.md#windows-x86-64-msvc)). The
  socket's permission and peer checks do nothing on Windows.
- On Linux `blaze` was measured with the Cranelift backend only; LLVM has been run
  through the harness gate, not through `harness/blaze_cli.py`. On macOS the harness was
  run under both.
- In ten runs of just's test suite through `harness/blaze_cli.py` there were two
  differences from cargo that are not explained. One was a test failing in the
  daemon-built executable and passing under cargo. The other was between two executables
  cargo built, which points at the tests being sensitive to load, but that was not shown.
- A function rustc emitted no symbol for is assumed to be dead code. One bug came from that
  assumption being wrong (test functions, fixed); no other is known, and it is not proved
  that there is none.
- The check that only your user can reach the daemon is tested for the same user only.
- `blaze` is not in the CI workflow. `harness/blaze_cli.py` needs the network and
  about 20 minutes.

## Plain `cargo`, switched on globally

Gone, with no-rustc.md's step M ([#93](https://github.com/indexable-inc/blaze/issues/93)):
`blaze enable` and `blaze disable`, and the wrapper they put behind plain cargo, no longer
exist. A plain `cargo build` is cargo's and rustc's alone, and `blaze` is the only way in.

On a machine where `enable` was run, the next `blaze` puts back what it wrote in cargo's home:
`build.rustc-workspace-wrapper` comes out of `$CARGO_HOME/config.toml` (the file is as it was
before `enable`, byte for byte, when nobody edited it since; otherwise only that key goes, and
a workspace wrapper `enable` chained comes back), the `cargo-fmt` and `cargo-clippy` its links
replaced come back from `$CARGO_HOME/blaze-kept`, and `blaze-enabled.json` is removed. It says
so in one line, and a record it cannot read, or a file edited so it no longer parses, is an
error naming the file.

## macOS arm64

`blaze` serves dev and test builds for
`aarch64-apple-darwin`. x86-64 macOS is not a target blaze knows, so builds for it are
cargo's.

Two things differ from Linux. Apple's linker has no way to let the first of two
definitions win, so the daemon links rustc's line with copies of the workspace's objects in
which every definition is weak, and its own object in front; this is the link the daemon
already did on macOS, and the line also gets `-Wl,-headerpad` (1 MiB to start with) so that
the executable can be spliced later. A program whose unwind table does not fit is linked
once more with the room it needs, which the links after keep (see
[platforms.md](platforms.md)).

What was run, on an 8-CPU Apple M4 virtual machine with macOS 26.5 and Rust 1.99.0:
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
builds "should go to us and not cargo". So every `blaze <command>` (and `blaze <command>`)
is blaze's own: no process of cargo, rustc, rustdoc, clippy-driver or rustfmt is started for it.
A command blaze has not written yet fails, says so and does nothing, with status 101:

    error: blaze has no `cargo publish` yet (it packages the crate first, a build blaze does not do yet); it hands nothing to cargo, so nothing was done
    note: what blaze has of each cargo command: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md#every-subcommand

A command cargo does not have is refused the same way (`error: no such command`): cargo would
run a plugin, `cargo-<name>`, from the path, and blaze runs none. `blaze --list` prints
the table below; flags typed before the command (`blaze -v fmt`) are read after it, as
cargo reads them. The table is `SUBCOMMANDS` in `crates/blaze-cargo/src/cargo/own/mod.rs`.

| Command | blaze's | What it is, and what it waits for |
|---|---|---|
| `build` (`b`), `run` (`r`), `test` (`t`), `check` (`c`) | the build | [above](#commands) |
| `metadata` | yes | `crates/blaze-metadata`: the workspace read by blaze-manifest, every locked package fetched by blaze-registry, resolved by blaze-resolve; `--no-deps`, `--features`, `--all-features`, `--no-default-features`, one `--filter-platform` (its `cfg` as blaze answers `rustc --print cfg` itself), `--manifest-path`, `--offline`. Held equal to cargo's by `harness/own_metadata.py`; against the pinned nightly's cargo, ripgrep, fd, helix, nushell and biome print the same JSON for each form (`harness/own_commands.py`, with `locate-project`, `verify-project`, `pkgid`, `read-manifest`, `fetch` and `clean`: 130 of 130 cases over the five). A package's `hints` has cargo 1.101's `"min-opt-level"`, always `null`: blaze-manifest does not read a value of it yet. A registry package the lock file does not pin, the lock file missing or stale, is chosen from the registry's index as cargo chooses it (blaze-registry's `IndexRegistry`), and the lock file written as cargo writes it. Several `--filter-platform`s fail with the reason |
| `locate-project` | yes | `--workspace`, `--message-format plain\|json`, `--manifest-path` |
| `verify-project` | yes | `{"success":"true"}`, or `{"invalid":...}` and status 1 |
| `version`, `help` | yes | blaze's version and help |
| `fmt` | yes | `crates/blaze-fmt`: rustfmt and cargo-fmt at rust-lang/rust 12433c307 ported onto a syntax tree of blaze's own, filled by rust-analyzer's parser with rustc's spans, run in process. `blaze fmt [--check] [-p NAME]... [--all] [--manifest-path PATH] [-v\|-q] [-- RUSTFMT_ARGS]` formats the targets blaze-manifest reads; `blaze fmt a.rs b.rs` formats files, with rustfmt's options. Held equal to stock rustfmt (the pinned nightly's) by `python3 harness/fmtdiff.py`: ripgrep, nushell, fd, helix and biome with their own rustfmt.toml, 6068 of 6068 files byte-equal (100%), and `--check` exit codes and diffs equal; the same with `max_width = 60`, 6068 of 6068 (100%); rustfmt's `tests/source`, 773 of 793 (97.5%; 18 more use line ranges, which it does not run). At blaze-fmt f43e4a20af on dev-aws-14. A file is refused with the reason, and left as it is, when it has nightly syntax rust-analyzer does not parse (postfix `.yield`, `.use` and `.match`, `const` block items, `use` closures, `&pin`, unsafe binders, `unsafe mod`, `mut ref`, `default` on an associated `const fn`), a parenthesized trait object with `+` (`(Foo) + 'static`), a `macro` with a parameter list, an `impl` or `mut` restriction, or a typed `try` block; a non-empty `ignore` option is an error |
| `clippy` | yes | `crates/blaze-lints`: 24 of clippy's lints ported onto rust-analyzer's analysis, run by the daemon, written as `cargo clippy` writes them; [below](#clippy) |
| `doc` (`d`) | yes | `crates/blaze-doc`: every page from rust-analyzer's HIR, no rustdoc; cargo's flags, `target/doc/<crate>/index.html`, search, source pages, themes. Against the pinned nightly's `cargo doc` (`harness/docdiff.py`), with their dependencies: fd 100.0% of rustdoc's pages, 99.9% of their members and impls, 99.4% of the resolved links; ripgrep 99.9%, 99.6%, 99.1%; nushell 99.5%, 99.2%, 98.4%; helix 98.9%, 99.0%, 98.7%; biome 99.0%, 99.2%, 98.1%. biome in 92.5 s against `cargo doc`'s 1028.0 s, nushell in 28.4 s against 497.1 s, its build scripts already run ([doc.md](doc.md)). The load still runs cargo for build scripts and proc macros, as every load does; doctests are `blaze test --doc`'s |
| `tree` | yes | `crates/blaze-tree`, cargo 1.101's `cargo tree` ported onto blaze-resolve: every flag (`-p`, `--workspace`, `--exclude`, `-e`, `-i`, `--prune`, `--depth`, `--prefix`, `--no-dedupe`, `-d`, `-f`, `--target`, `--charset`, `--color`, the feature flags). Held equal to cargo's by `harness/own_tree.py`: 26 forms over ripgrep, fd, helix, nushell and biome, 130 of 130 byte-equal. Package patterns with globs (`-p 'serde*'`) and `-i` with no value are refused |
| `pkgid` | yes | the package's full spec from `Cargo.lock`, with cargo's errors for none or several matches |
| `fetch` | yes | every package `Cargo.lock` names, downloaded by blaze-registry. `--target` fetches every platform's, and with no lock file it fails (choosing versions is #321) |
| `clean` | yes | the target and build directories, one profile's (`--release`, `--profile`), or `--doc`, with `--target-dir`, `--dry-run` and cargo's `Removed` summary. `-p` and `--workspace` (one package's files) are refused |
| `new`, `init` | yes | `crates/blaze-new`, cargo 1.101's `ops/cargo_new.rs`: the manifest, source file, ignore file and repository, the workspace's `members`, and cargo's checks of the name. The repository is made by `git init` (or `hg`, `pijul`, `fossil`, as cargo runs them). Cargo runs rustfmt on the file it writes; blaze does not, so a `rustfmt.toml` above the package that would change the template is not applied. Held equal to cargo's by `harness/own_new.py`: 50 of 50 cases, what each prints and every file it leaves |
| `remove` (`rm`) | yes | the dependencies out of the member's manifest, with `toml_edit` keeping its layout and comments, as cargo 1.99's `cargo_remove.rs` and `toml_mut` do: the features that named them, then the `[workspace.dependencies]`, `[profile.*.package]`, `[replace]` and `[patch]` entries nothing uses any more; then the lock file resolved again by blaze with every version it keeps locked as it was (blaze-metadata's `relock`), honouring `--locked`. A workspace with no lock file is refused: writing one chooses versions (#321). Held equal to cargo's by `harness/own_remove.py`, manifests and lock files byte for byte: 43 of 43 cases on ripgrep and a made-up workspace |
| `vendor` | yes | every registry package of the lock file unpacked from its `.crate` into the vendor directory as cargo 1.99's `ops/vendor.rs` does, with `.cargo-checksum.json`, `--versioned-dirs`, `--no-delete`, `-s`/`--sync` and `--respect-source-config`, and the `[source]` configuration printed; the workspace resolved again first, as cargo does (blaze-metadata's `vendored`). A git package is refused: cargo vendors it with its manifest as `cargo package` writes it, which blaze does not write yet. A workspace with no lock file is refused: writing one chooses versions (#321). Held equal to cargo's by `harness/own_vendor.py`, every file by its sha256: 18 of 18 cases on fd, ripgrep and a made-up workspace |
| `add`, `update`, `generate-lockfile` | not yet | they choose versions, which blaze-resolve does not do yet ([#321](https://github.com/indexable-inc/blaze/issues/321)) |
| `bench`, `rustc`, `rustdoc`, `fix`, `install`, `package` | not yet | builds blaze does not do yet ([no-rustc.md](no-rustc.md)) |
| `search`, `owner`, `yank`, `login`, `logout` | yes | blaze-registry's `api.rs`, the `crates-io` crate 0.41's client of a registry's web API over blaze's own HTTP, and cargo 1.99's `ops/registry` around it: which registry (`--registry`, `--index`, `registry.default`, `source.*.replace-with`, the `Updating` of its `config.json`), the token (`--token`, `CARGO_REGISTRY_TOKEN`, `CARGO_REGISTRIES_<NAME>_TOKEN`, `credentials.toml` and the old `credentials`, read and written as cargo writes them, mode 0600), cargo's errors of the server (`errors`, a status not 200), and `search`'s table with `--limit` and its highlighted names. The credential providers of `registry.global-credential-providers` and a registry's `credential-provider` are asked in turn as cargo 1.101 asks them (`crates/blaze-registry/src/auth`): `cargo:token`, `cargo:token-from-stdout`, and credential processes spoken to in cargo's protocol, version 1, with `[credential-alias]`, each answer's caching and cargo's words for each failure. `cargo:wincred`, `cargo:macos-keychain` and `cargo:libsecret` on their own platform are refused with the provider named and what to set instead; on another platform they answer for no registry, as in cargo. Held equal to cargo's by `harness/own_registry.py` against a mock registry: 101 of 101 cases, what each prints, every request the server saw, the credentials files left and what each provider was asked |
| `publish`, `info` | not yet | `publish` packages the crate first, which is a build of `package`; `info` is not written yet |
| `uninstall` | yes | cargo 1.99's `cargo_uninstall.rs` and install tracker: removes the programs from the install root's `bin` (`--root`, `CARGO_INSTALL_ROOT`, `install.root`, cargo's home) and their entries in `.crates.toml` and `.crates2.json`, written byte for byte as cargo writes them; specs, `-p`, `--bin`, the package of the directory, cargo's `Summary` of several. Held equal to cargo's by `harness/own_uninstall.py`: 18 of 18 cases, what each prints and the files it leaves |
| `read-manifest` | yes | the package's entry of `metadata --no-deps`, as cargo prints it |
| `report` | yes | `future-incompatibilities` (`future-incompat`): the report a build left in the build directory, with `--id`, `-p` and `--color`, as cargo 1.99's `future_incompat.rs` prints it. `timings`, `sessions` and `rebuilds` are unstable in cargo: refused as cargo refuses them without `-Z build-analysis`, and with it they fail saying blaze's builds do not write the logs they read. Held equal to cargo's by `harness/own_report.py`: 23 of 23 cases |
| `config` | yes | `config get`, by blaze-manifest's port of cargo 1.99's `cargo_config.rs`: `--format toml`, `json` and `json-value`, `--show-origin`, `--merged no`. Unstable in cargo, and refused as cargo refuses it unless `-Z unstable-options`, `unstable.unstable-options` or `CARGO_UNSTABLE_UNSTABLE_OPTIONS` allows it. Held equal to cargo's by `harness/own_config.py`, run as `blaze config`: 41 of 41 cases |
| any other | no | `error: no such command`; blaze runs no cargo plugin |

### `clippy`

`blaze clippy` (and `blaze clippy`) lints with blaze's own lints, `crates/blaze-lints`:
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

`harness/clippydiff.py` holds them to stock clippy: it runs both with `--message-format=json`
and compares each diagnostic by lint, file, line and column, then its rendered text, and the
exit status. `harness/clippy-fixture` is a crate that exercises every lint. Run on dev-aws-14 on
2026-10-27, with `blaze clippy --workspace --all-targets` against the pinned nightly's
`cargo clippy --workspace --all-targets --message-format=json`; biome with its `*_analyze`
packages under `unsandboxed-build-scripts` (they write their lists of rules into their own
`src`, which blaze's sandbox refuses):

| App | Stock diagnostics | Of the lints ported | Matched | Missed | Extra | Rendered the same | Exit status, stock and blaze |
|---|---|---|---|---|---|---|---|
| ripgrep | 676 | 553 | 553 | 0 | 0 | 553 | 0 and 0 |
| fd | 14 | 14 | 14 | 0 | 0 | 14 | 0 and 0 |
| helix | 57 | 20 | 20 | 0 | 0 | 20 | 0 and 0 |
| biome | 971 | 969 | 0 | 969 | 0 | 0 | 0 and 101 |
| `harness/clippy-fixture` | 53 | 53 | 53 | 0 | 0 | 53 | 101 and 101 |

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
