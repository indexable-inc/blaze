# Using blaze with cargo

There are two ways to use blaze from cargo. `cargo blaze <command>` is an explicit
subcommand, described here. The second, switching blaze on globally so that plain `cargo`
goes through it, has its own section at the end.

blaze was called ra-jit until 2026-10-24. Below this section the guide still says
`cargo ra-jit`, which is the same command as `cargo blaze` and keeps working; the programs and
the daemon's messages keep the name ra-jit for now.

Everything in this file was checked against the code in `crates/ra-jit/src/cargo/` and run on
Linux x86-64. On macOS arm64 `harness/cargo_ra_jit.py --small` was run, and what is known of
it is under [macOS arm64](#macos-arm64). The design notes it was written from are in
[reports/CARGO.md](reports/CARGO.md).

## Install

`cargo blaze` is cargo's way of running a program called `cargo-blaze` on your `PATH`. That
program is built with blaze and runs the `ra-jit` executable in the same directory, blaze's
engine, so the two have to be installed side by side, with `ra-jit-rustc` and
`ra-jit-rustc-first` (and `cargo-ra-jit`, the old name of `cargo-blaze`). The
[installer](../install.sh) and `cargo install` both do that:

    curl -fsSL https://raw.githubusercontent.com/indexable-inc/blaze/main/install.sh | sh
    cargo +nightly-2026-10-25 install --locked cargo-blaze

From the release after 0.1.0, `--profile dist` builds the profile a release is built with, less
the PGO a release adds ([reports/RELEASE-BUILD.md](reports/RELEASE-BUILD.md)); 0.1.0 has no such
profile and fails with it. From a checkout, the release's own recipe builds all of it, with the profile
[pgo/profiles.json](../pgo/profiles.json) pins:

    python3 .github/scripts/build_release.py --target $(rustc +"$(cat harness/toolchain)" -vV | sed -n 's/^host: //p') --out shipped
    cp shipped/* ~/.cargo/bin/

That build has the LLVM backend (`BLAZE_BACKEND=llvm`) and the LLVM tiers, as the
installer's binaries do: the script fetches the LLVM 23 blaze pins and links it in statically
([which LLVM](contributing.md#llvm-for---features-llvm)). `cargo install` from crates.io has
the Cranelift backend only; `--features llvm` there needs `LLVM_SYS_231_PREFIX` set to that
same LLVM. `cargo build -p ra-jit --release` is the quicker build the harnesses and CI use while
working on blaze; it is not what ships.

`cargo blaze` builds with the one toolchain blaze compiles, the dated nightly in
`harness/toolchain` (`nightly-2026-10-25`), whatever toolchain the project names (a
`rust-toolchain.toml`, `rustup override`, `+toolchain`, `RUSTUP_TOOLCHAIN` or rustup's
default): it runs cargo, rustc and the daemon under that nightly by naming it in
`RUSTUP_TOOLCHAIN` and `CARGO`. The nightly builds what older releases built, so a project on a
stable release or an older nightly builds as it is, `#![feature(...)]` included. Code only the
project's own toolchain builds (a feature the pinned nightly removed, a newer nightly's
library) fails with rustc's error and a note naming both toolchains. Plain `cargo` still builds with the project's toolchain, in the same target
directory: cargo keys what it builds by the rustc, so neither rebuilds the other's.
`BLAZE_OTHER_TOOLCHAIN=1` builds with the project's toolchain instead
([support.md](support.md#toolchain)).

Where the nightly comes from, in this order: `RUSTC` naming a rustc whose `rustc -V` line is
the pinned one's (`rustc 1.101.0-nightly (12433c307 2026-10-24)`: the whole line, since every
nightly of a release says `1.101.0`) with its library's source; rustup's `nightly-2026-10-25`
with `rust-src`, when rustup has it; else blaze's own copy in
`~/.blaze/toolchains/nightly-2026-10-25-<host>` (`BLAZE_HOME` moves `~/.blaze`), laid out as rustup lays
out a toolchain. blaze downloads its copy the first time it needs it, once, with a line on
stderr while it does: rustc, cargo, the standard library and its source from the Rust project's
own servers, 97 MB on macOS, 134 MB on Linux x86-64 and 118 MB on Linux ARM64 (548 MB on macOS
and 710 MB on Linux x86-64 once unpacked). Each file's sha256 is built
into blaze, from the nightly's published manifest (`harness/toolchain_pins.py` writes them into
`crates/ra-jit/src/cargo/toolchain/pins.txt`), and a file with other bytes is refused. Concurrent
builds download it once; a download that stopped goes on from where it stopped the next time.
Building with blaze's copy, blaze names its cargo, rustc and rustdoc in `CARGO`, `RUSTC` and
`RUSTDOC` and puts its `bin` first on `PATH`, for the commands it runs and not for your shell.
`RA_JIT_RUST_DIST` names a mirror of `https://static.rust-lang.org/dist` (or a directory with
the files), for a machine that cannot reach it. `cargo blaze toolchain` (or `blaze toolchain`)
says which Rust blaze builds with, downloading its copy first when there is none.

Every setting below named `RA_JIT_<NAME>` can be given as `BLAZE_<NAME>` too; when both are
set, the `BLAZE_` one wins.

## Commands

    cargo blaze build [cargo's flags]
    cargo blaze run   [cargo's flags] [-- program arguments]
    cargo blaze test  [cargo's flags] [test names] [-- test arguments]
    cargo blaze check [cargo's flags]
    cargo blaze status
    cargo blaze stop
    cargo blaze apply [--build] [--memory] [--json] PATH[=FROM]...
    cargo blaze toolchain

`blaze <command>` is the same as `cargo blaze <command>`, for a machine where blaze brought
its own Rust and there is no `cargo` on the path.

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
the daemon served and how many went to cargo, with the reasons. `stop` ends the daemon.
`apply` hands files to the daemon as one change ([the agent API](#the-agent-api)). Every other
subcommand of cargo's is blaze's own, or fails saying blaze has none yet: none is passed to
cargo, and no cargo plugin is run ([Every subcommand](#every-subcommand)).

## What a build does

blaze never runs rustc, and a build has one path ([no-rustc.md](no-rustc.md)). `build` and
`run` build each program the build asks for whole: every crate it depends on is compiled by
blaze over the shipped standard library and linked by blaze, and the executable is put where
cargo puts it (`target/debug/<name>`, `target/release/<name>`, `examples/` for an example).
No `cargo build` runs. A function the program can reach that blaze cannot compile fails the
build, which says which and why; nothing is handed to cargo or rustc:

       Compiling kapp v0.3.1 (/src/k/app)
        Finished `dev` profile [ra-jit] target(s) in 18.40s

       Compiling kuses v0.1.0 (/src/k/uses)
    error: `kuses` can reach functions blaze could not compile, so it is not built:
      `khalf::halved`: unsupported: f16 and f128

What blaze does not build yet fails the same way, saying what: the targets `--bins`,
`--examples`, `--tests`, `--benches` and `--all-targets` pick, several `--target`s,
`--target-dir`, `--message-format`, `-Z`, and `--build-plan` and `--unit-graph`. A build of
libraries alone (`--lib`, or a workspace with no program) is the daemon's check of them:
blaze writes no library for cargo, and says what it finds.

The build is the daemon's: its worker loads the workspace, builds and links the programs, and
answers once they are linked. The next build asks the same worker, which compiles the
functions that changed on what it loaded and splices them into the executable or links again;
a changed manifest or lock file has a new worker load the workspace. Build scripts and proc
macros are still built and run as the daemon's load does (`target/rust-analyzer`).

`test` builds whole what cargo would test: each member's library, binaries and integration
tests with `test = true`, the examples built beside them to see that they compile, and the
binaries an integration test runs (`CARGO_BIN_EXE_<name>`), or what `--lib`, `--bin`,
`--example` and one `--test` name. A worker that loads the workspace with `cfg(test)` builds
the unit tests, one that loads it without builds the rest; a test's executable is
`target/debug/deps/<name>-<kind>`. A unit test with `harness = false`, several `--test`s and
the other target flags fail the build and say so. The doc tests are still taken out through
`cargo test --doc`, which runs no rustc.

`test` builds whole what cargo would test: each member's library, binaries and integration
tests with `test = true`, the examples built beside them to see that they compile, and the
binaries an integration test runs (`CARGO_BIN_EXE_<name>`), or what `--lib`, `--bin`,
`--example` and one `--test` name. A worker that loads the workspace with `cfg(test)` builds
the unit tests, one that loads it without builds the rest; a test's executable is
`target/debug/deps/<name>-<kind>`. A unit test with `harness = false`, several `--test`s and
the other target flags fail the build and say so. The doc tests are still taken out through
`cargo test --doc`, which runs no rustc.

`run` then runs the program and `test` runs the test executables the way cargo does: the
same working directory, the variables cargo sets, cargo's `Running` lines and the same exit
status. Every test runs, unless `RA_JIT_TESTS=affected` says otherwise, and every test runs
in a sandbox.

### Every test runs in a sandbox

"Everything should by default be in sandbox and yea it should have sandbox support macos"
(Andrew, 2026-10-26). Every test executable `cargo ra-jit test` runs, under `all` and
`affected` alike, runs in a sandbox of its own, with libtest's threads inside it as cargo would
run them. In it the machine's files are read-only; the workspace and the target directory are
as they are, but what a test writes there is kept to the run and gone when it ends; `/tmp`,
`/var/tmp` and `/dev/shm` (on macOS the temporary directory) start empty; there is no network
but the sandbox's own loopback, and no Unix socket but those the run makes itself; and the
environment is fixed. A write outside gets the error a read-only file system gives, and a
connection to anything but loopback is refused. The program is a session of its own, with no
controlling terminal, so it can't push input into the terminal it was started from
(`TIOCSTI`); Ctrl-C and `SIGTERM` reach it all the same, passed on by the sandbox's own
process.

The sandbox records what each test reached, by name: the harness tells it, before and after
each test, which test the thread is running. Of a test executable that fails, the note under
it names each failed test the sandbox refused something, and how to run it outside:

    note: tests::writes_home failed in the sandbox, which refused it writing `/home/me/.cache/x` (the machine is read-only in a sandbox); if it needs that, run it outside: `unsandboxed-tests = ["tests::writes_home"]` under `[package.metadata.blaze.sandbox]` in /home/me/tsel/Cargo.toml (`tests = false` for every test of the package), or `RA_JIT_SANDBOX=off` for every test of the command

A package takes its tests out of the sandbox in its manifest:

```toml
[package.metadata.blaze.sandbox]
tests = false                                 # every test of the package runs outside
unsandboxed-tests = ["tests::writes_home"]    # or these, by libtest's names
```

Any other key there is an error. The tests named run in a second run of the executable,
outside, after the sandboxed one (libtest's `--exact` and `--skip` split them); `-v` says
which and why. `RA_JIT_SANDBOX=off` runs every test of the command outside, as cargo does
(`BLAZE_SANDBOX`, when set, wins over it, for tests, build scripts and proc macros alike).

Where no sandbox can be made, `cargo ra-jit test` stops before any test runs, says why and how
to fix it, and exits 101. It does not go on outside with a warning: a run that did would break,
unseen, the promise that a test can't change the machine or reach the network, and a warning
in a long log is not seen. `RA_JIT_SANDBOX=off` is the explicit way to run outside.

On Linux the sandbox is user, mount, network, IPC and PID namespaces: the machine bound
read-only, overlays for the workspace and target directory, tmpfs for the empty directories, a
network namespace with only loopback up, and a seccomp filter whose supervisor reads the paths
asked for. `/run` starts empty too, but for the links at its top (NixOS's `PATH` goes through
`/run/current-system`), and so does the Nix daemon's socket directory: a network namespace
hides abstract sockets but not the machine's Unix sockets (the Docker, D-Bus, systemd, Nix
daemon and agents' sockets of `/run` and `/run/user/$UID`), which would let a test have a daemon
outside do what it may not. It needs namespaces a user can make; Ubuntu's AppArmor stops them
unless `kernel.apparmor_restrict_unprivileged_userns` is 0, which the error says. A filter is
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
FSEvents stream (`FSEvents`) and `getpwuid` (`libinfo`); a cold `cargo ra-jit build` of a
workspace with `ring`, `cc`, `serde` and a build script that runs `xcrun metal` builds under
the list, and a build script's `defaults write` and `launchctl submit` fail. An interposer library ra-jit builds and loads with
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
process per test, which ra-jit used before and dropped):

| | Linux x86-64 (dev host, 16 cores) | macOS (M5 Max, 8 at a time, loaded) |
|---|---|---|
| a trivial program, bare | 0.86 ms | 5.2 ms |
| in a sandbox | 8.0 ms | 29 ms |
| in a sandbox, recording reads | 9.3 ms | 25 ms |
| 600-test library, test phase, `RA_JIT_SANDBOX=off` | 18 ms | 20 ms |
| the same, sandboxed, `all` | 37 ms | 68 ms |
| the same, sandboxed, `affected` (every test forced to run) | 80 ms | 100 ms |
| the same, a sandbox per test (16 at a time on Linux, 8 on macOS) | 0.68 s | 2.26 s |

A sandbox per test executable costs one launch, about 8 ms on Linux, whatever its number of
tests, and about 25 ms on macOS (mostly the fork, Seatbelt and dyld); a sandbox per test cost
the 600-test library ten to thirty times as much, so there is none. On
macOS a stat under the interposer goes from 0.8 to 1.1 µs, and to 3 to 5 µs on a workspace
whose overlay has been written.

What is left: doctests run in rustdoc's process and are not sandboxed; Windows has no sandbox
yet (its CI runs with `RA_JIT_SANDBOX=off`). On Linux `openat2` always counts as a write; the
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
it (ra-jit or rustc), and the daemon's own reruns of one, runs in the sandbox tests run in
([#147](https://github.com/indexable-inc/blaze/issues/147)): `cargo/script_sandbox.rs` is the
one place a script is run. A script's environment is the whole of what cargo gave it (`PATH`,
`CC`, `TARGET`, the jobserver's descriptors and the rest); the machine is read-only; it writes
its `OUT_DIR` for real, which is the one directory the Cargo book lets a script write ("Scripts
should not modify any files outside of that directory"); the temporary directories start empty;
there is no network but the sandbox's own loopback. Its package's directory, the workspace and
the target directory are seen as they are, even below `/tmp` or `/dev/shm`. On macOS the
per-user cache directory (`getconf DARWIN_USER_CACHE_DIR`, `/var/folders/../C`) is writable
too: clang's module cache, `xcrun`'s and the Metal compiler's live there, are found by
`confstr` and not by a variable a private one could be named in, and check what they cache.
Cargo's own rules for running a script again are unchanged.

A script that fails having been refused a write or a connection says so under its output:

    note: the build script of `sys-crate` failed in the sandbox, which refused it writing `/home/me/ws/sys-crate/src/bindings.rs` (the machine is read-only in a sandbox); if it needs that, run it outside: `unsandboxed-build-scripts = ["sys-crate"]` under `[workspace.metadata.blaze.sandbox]` in /home/me/ws/Cargo.toml (or `build-script = false` under `[package.metadata.blaze.sandbox]` in its own manifest, for a package of the workspace), or `RA_JIT_SANDBOX=off` for every build script and test of the command

The ways out, in the workspace's root manifest, which is the only one a dependency from a
registry can be named in:

```toml
[workspace.metadata.blaze.sandbox]
unsandboxed-build-scripts = ["hegeltest", "flecs_ecs_sys"]   # by package name
unsandboxed-proc-macros = ["sqlx-macros"]                    # by package name
```

and, for a package of the workspace, in its own manifest:

```toml
[package.metadata.blaze.sandbox]
build-script = false
```

Any other key in either table is an error, and so is a value that is not a list of names (a
`bool` for `build-script`). A package from a registry or git can't take itself out with its own
manifest. `RA_JIT_SANDBOX=off` runs every build script, proc macro and test outside, as cargo
does. Where no sandbox can be made, the script fails with why and the same way out, as a test
run does, and a proc-macro server does not start and the load says why: nothing ever runs
outside instead, nor goes to cargo or rustc. Windows has no sandbox yet, so there a build with
build scripts or proc macros needs `RA_JIT_SANDBOX=off`.

What a script read is recorded, and decides one thing only. Cargo runs a script again when a
file of its `rerun-if-changed` changed, when a variable of `rerun-if-env-changed` did, and,
when it named neither, when any file of its package did. In that last case a run whose record
is whole (macOS records nothing of a program it protects, such as `/bin/sh` or `/usr/bin/cc`,
and the record then says it is not whole) and whose every read path is as it was stands for a
run now: the store keeps it with what it read, and cargo's rerun is answered from the store
without running it; the daemon's check of the script (`ra-jit-reads` beside its `OUT_DIR`)
needs no run either. A script that named what it reads runs again exactly when cargo says, and
one whose record is not whole whenever cargo would run it.

Every proc-macro server a load starts, the toolchain's and ra-jit's own, runs in the sandbox
too, writing nothing: no `OUT_DIR`, only the empty temporary directories. A server is started
once, when a load first needs it, and serves every expansion after, so the sandbox costs its
start (about 8 ms on Linux, 25 ms on macOS) and nothing per expansion. The packages of
`unsandboxed-proc-macros` are expanded by servers of their own, started outside. The hook is
`proc_macro_api::wrap` (the command a server is started with) and
`load_cargo::unsandboxed_macros_with` (which libraries get the outside servers).

What it costs, measured on 2026-10-26 (cold builds with an empty store and target
directory, `RA_JIT_SANDBOX=off` against the default; a server's start is its start and exit with
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

### Running only the tests an edit reached

With `RA_JIT_TESTS=affected`, `cargo ra-jit test` runs, of each test executable the daemon
built, the tests an edit could have changed the end of since they last passed:

    Running unittests src/lib.rs (target/debug/deps/tsel-1f2e): 2 of 5 tests
    Skipping tests/it.rs (target/debug/deps/it-9a8b): 2 tests, unchanged since the last pass
    Skipped 5 tests, unchanged since the last pass; 1 test run every time (`-v` says why); `RA_JIT_TESTS=all` runs every test

At each build the daemon hashes every test's code with the code of every function it calls,
directly or through others, in any crate of the workspace or its dependencies, and its
attributes (`#[ignore]`, `#[should_panic]`). The standard library is named by the toolchain.
A test runs again when that hash is not the one it last passed with, when it has not passed
since it was added or failed, and every time when it reaches what its code does not decide:
a file, the environment, another process, the network, or a function ra-jit can't compile.
Of a test executable that fails, the tests libtest's summary lists as failed run again, and
the others that ran are remembered as passed; one that ends with no summary (a crash, or a
test that exits the process) has every test it ran run again. Its output is passed on through
a pipe as it comes, colored as libtest would color it on the terminal.
An integration test also runs again when a program of its package that cargo hands it as
`CARGO_BIN_EXE_<name>` was built again. `cargo ra-jit test -v` says, under each test
executable, why each test it runs every time does, by the fewest calls from the test to it:

    Always tests::reads_a_file: it reaches `statx`, which reads or writes files, by tsel::tests::reads_a_file -> std::fs::read_to_string::<…> -> std::fs::read_to_string::inner -> <std::fs::File>::metadata -> … -> statx

Under `affected` a test that reaches outside its code is not run every time: the sandbox
records every path it opens, looks at, lists or runs, with the environment it was given (the
variables cargo sets for a test, `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LANGUAGE`,
`LC_*`, `TZ`, `CARGO`, `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `LD_LIBRARY_PATH`,
`NIX_LD`, `NIX_LD_LIBRARY_PATH`, `RUST_BACKTRACE`, `RUST_LIB_BACKTRACE`, `RUST_MIN_STACK`, and
those `RA_JIT_TEST_ENV` names, by commas; under `all` a test gets the whole environment). Once
it passes, what was at each path (a file's contents, a directory's names, nothing) is kept
beside its program, and the test runs again when its code, its environment or one of those
changed; otherwise it is skipped like any other. `-v` says why one runs:

    Sandboxed tests::reads_a_file: it read `/home/me/tsel/data.txt`, which changed

A test whose reads are not all known (a program macOS starts without the interposer), one its
package takes out of the sandbox, and every one under `RA_JIT_SANDBOX=off` runs every time.
The time a test reads, the randomness it draws and the threads it starts are its own still.

`-v`, `-q` and `--color` only change what cargo prints, and are the same build to the daemon.

A test executable cargo built, a command that names tests or passes the harness other flags
than `-q`, `--nocapture`, `--show-output`, `--test-threads` and `--color`, and one that runs
the ignored tests run as asked: every test. Time, randomness (a `HashMap`'s seed too) and
threads are not counted as outside the code, so a test that depends on them by chance is as
flaky as it was; nor is a backtrace, whose capture reads `RUST_BACKTRACE` and whose text the
working directory, for what a failure prints. Unset, or `all`, runs every test, as cargo does;
the daemon then hashes nothing. What is left (a hash that never compiles on a warm store, a
watch mode) is [#227](https://github.com/indexable-inc/ra-jit/issues/227).

While cargo builds, a spare worker already loads the workspace, resolves it and compiles the
functions of each crate cargo is done with on a quarter of the cores, and reads the symbols of
each library as cargo writes it. When cargo is done it reads the symbols of the executable
rustc linked, and the daemon is ready: the first edits are patches spliced into that
executable. The daemon builds its own base in the background once nothing has waited for a
second, and gives it up when that would take more than a quarter of the machine's memory
([architecture.md](architecture.md#the-first-edits-patch-what-rustc-linked)). On Linux
x86-64 (16 vCPUs, 8 cores), from an empty store, in interleaved runs:

| Project | `cargo build` | `cargo ra-jit build` | Ready | First edit |
|---|---|---|---|---|
| just | 7.6 to 7.7 s | 8.0 to 8.3 s | 8.4 to 8.7 s | 0.5 s, a patch |
| ripgrep | 5.0 to 5.4 s | 5.4 to 5.7 s | 5.6 to 5.9 s | 1.4 to 1.5 s, a patch |
| nushell | 34.7 to 37.0 s | 37.4 to 38.9 s | 39.0 to 40.5 s | 1.7 to 2.4 s, a patch |

The command takes longer than cargo's because of the spare worker beside it: what it reads
and resolves of the standard library (some 10 core-seconds on every project) and of the
workspace takes cores from cargo, and on this machine every second core is a hyperthread of
another, so a core cargo leaves idle is still not free. With `RA_JIT_SPARE=0` the command is
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
the spare loads the library then rather than before. `RA_JIT_SPARE=ready` keeps one loaded at
all times, as the daemon did until October 2026. On Linux x86-64 (16 cores), a second project
under a running daemon, medians of three interleaved rounds (`harness/spare.py`):

| Project | Daemon, idle | Cold start | Warm start | Daemon after the warm start |
|---|---|---|---|---|
| ripgrep | 1,275 MB (2,635 with `ready`) | 4.86 s (4.74 s) | 2.62 s (1.29 s) | 3,652 MB (4,918 MB) |
| fd | 1,267 MB (2,638) | 24.17 s (24.21 s) | 3.48 s (2.19 s) | 3,354 MB (4,543 MB) |
| nushell | 1,282 MB (2,638) | 30.70 s (30.48 s) | 9.51 s (8.62 s) | 7,219 MB (8,699 MB) |

The cold start is `cargo ra-jit build` from an empty target directory until the daemon is
ready; the warm start is the same once cargo has built it, the daemon stopped and started
again. With `RA_JIT_SPARE=0` the daemon holds the same as without the variable, a cold start
waits for the workspace to load after cargo (7.14 s on ripgrep, 38.43 s on nushell), and a
warm one is quicker (2.14 s, 6.83 s).

## When cargo builds instead

ra-jit hands a build to cargo whenever it cannot be sure its own result would be what cargo's
is. It prints one line with the reason, `ra-jit: bug: <reason>; building with cargo, which
ra-jit is never to do (<step>, and #84)`, and counts it in `cargo ra-jit status`. Each is a bug:
ra-jit is never to build with cargo
([#84](https://github.com/indexable-inc/ra-jit/issues/84)); the line names the step of
[no-rustc.md](no-rustc.md) that removes it, M ([#93](https://github.com/indexable-inc/ra-jit/issues/93))
for an edit's build and K ([#91](https://github.com/indexable-inc/ra-jit/issues/91)) for a first
build; a reason of the test harness names H ([#88](https://github.com/indexable-inc/ra-jit/issues/88)), and a
function ra-jit does not compile whose rustc copy is from before the edit names E
([#85](https://github.com/indexable-inc/ra-jit/issues/85)). The build then behaves as cargo's because it is cargo's.

A whole invocation goes to cargo when any of these hold (`Invocation::only_cargo` in
`cargo/cli.rs`); for `check` only `--build-plan` and `--unit-graph` do:

| Condition | Reason printed |
|---|---|
| `--build-plan` or `--unit-graph` | `--build-plan and --unit-graph are cargo's to answer` |
| `--message-format` | `--message-format` is cargo's to answer |
| a `-Z` flag | unstable cargo flags |
| `--exclude` | `--exclude` is not loaded by the daemon yet |
| more than one `--target` | several targets in one build |
| `RUSTC_WORKSPACE_WRAPPER` set to another wrapper | `RUSTC_WORKSPACE_WRAPPER` is set to another wrapper |
| `ra-jit-rustc` missing | `ra-jit-rustc` is not installed next to `ra-jit` |
| `--target` or `CARGO_BUILD_TARGET` naming a target that is neither ELF nor Mach-O, or one ra-jit does not know | programs for the triple are not linked through `cargo ra-jit` yet |

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

Doc tests go through ra-jit in rustdoc's place. An example ra-jit cannot build is left to
rustdoc, with a line saying why. Of the test harness's flags, `-q`, `--format pretty`,
`--format terse`, `--color never`, `--exact`, `--skip`, `--test-threads` and test filters
are handled, and the results are shown as libtest shows them; any other flag leaves the
library's doctests to rustdoc. Cargo's run of rustdoc for each library is only written
down; once cargo is done, one run of ra-jit loads the workspace and tests the libraries'
examples in the order cargo asked, each in the directory and with the variables cargo gave
its rustdoc, and says `Doc-tests` and what failed as cargo does. The daemon does not run
doctests yet.

The fallback is per invocation or per program, not per crate. If one program of an
invocation cannot be the daemon's, cargo builds the invocation.

These flags are handled on the daemon's path: `--target` naming the host, a single `-p`,
`--features`, `--bin`, `--example`, `--test`, `--lib`, `--manifest-path`, `--workspace`,
`--no-run`, `--no-fail-fast`, `-q`, test filters and the arguments after `--`.

The daemon's state is kept per command line. `cargo ra-jit build`, `cargo ra-jit build -q`
and `cargo ra-jit build --features x` are three different builds to ra-jit, told apart by a
hash of the subcommand (`run` counts as `build`), the build flags, the working directory
and the environment variables that change what cargo builds. The first use of each is a
cargo build, usually a quick one because cargo's own output is shared between them.

## The daemon

There is one daemon per user. It is started by the first `cargo ra-jit` that needs it, in a
session of its own, and it outlives the command that started it. It exits after 30 minutes
without a request; `RA_JIT_IDLE_SECS` sets another time and is read when the daemon starts
(0 means never).

The daemon holds no compiler state itself. It owns worker processes, each holding one
rust-analyzer database for one repository, toolchain, target, feature set and `cfg(test)`
setting. Git worktrees of one repository share a worker, so what they have in common is
analysed once. Different repositories get different workers.

A new worktree still starts with a cargo build, because it has a target directory of its
own. What it saves is the daemon's side: the worktree joins the worker that is already
loaded instead of loading the workspace again. With a ripgrep checkout loaded, the first
`cargo ra-jit build` in a new worktree of it was cargo's and took 16 s, the worktree showed
as ready under the same worker, and the first edit there was built by the daemon in 0.4 s. A `cargo ra-jit test` can use
two workers, because unit tests are built with `cfg(test)` and integration tests without.

`cargo ra-jit status` shows the daemon, its workers and their log files:

    daemon: pid 10969, version 0.0.0-3da7208-18df73efa25a74db, exits after 1800s idle, workers hold at most 32768 MB
      worker: pid 13169, /dev/shm/ripgrep/, holds 1312 MB, log /home/user/.cache/ra-jit/workers/f30b80a4e02dffde/log
        /dev/shm/ripgrep: ready
    /dev/shm/ripgrep (dev, 1f59ed7b049b7fa0): 3 builds by the daemon, 0 by cargo

A worker for a large workspace holds a lot of memory: 1.3 GB for ripgrep and 9.4 GB for
dioxus-cli at the end of the agent-style edit runs in [results.md](results.md). The workers
together hold at most half of the machine's memory, or what `RA_JIT_MEMORY_MAX` says. On
Linux the machine's memory is the least of what it has and what the control group the daemon
was started in allows (a container's, a CI job's, a systemd unit's). Once a
second, over that, the daemon stops the worker asked longest ago, and the next build of its
workspace is cargo's while a worker loads it again. A worker that is answering or loading is
never stopped, nor is the one asked last, nor any while a program takes live edits.

A client and a daemon built from different sources do not talk to each other. A daemon that
gets a build request from another version stops its workers and exits, and the client
starts its own.

### Files

The daemon's directory is `$RA_JIT_HOME` if set, else `$XDG_RUNTIME_DIR/ra-jit`, else
`~/.cache/ra-jit`. It holds `daemon.sock`, `daemon.sock.lock`, `daemon.log`, `daemon.api`
(the schema of the calls the daemon takes), `http-token` when it serves HTTP,
`workers/<id>/` with each worker's socket and log, and `states/`. The directory must belong
to you and be closed to group and others (mode 0700); otherwise the client and the daemon
refuse to use it. The socket is 0600, and a connection from another user is refused before
its request is read.

In a project, ra-jit keeps its records next to cargo's output, so `cargo clean` removes
them too:

    target/<profile>/.ra-jit-units/     what the wrapper recorded of each rustc run
    target/ra-jit/<key>/                state of one command line, and the daemon's objects

On Linux the object with all of a program the daemon compiled, `programs/<unit>/ra-jit.o`, is
kept compressed as `ra-jit.o.zst` between the links that read it, a sixth of its size, and
inflated again for the next: the executable has the debug information, so nothing else reads
it. With `split-debuginfo` set, or on macOS, where a debugger reads the object, it stays as it
is.

Cargo keeps what it builds through a workspace wrapper apart from what it builds without
one. `cargo build` and `cargo ra-jit build` in one checkout therefore each compile the
workspace's crates once, and share the dependencies.

### The agent API

The daemon's socket takes calls as well as `cargo ra-jit`'s lines: deterministic CBOR (RFC 8949
section 4.2: the shortest form of every number, definite lengths, map keys in bytewise order,
no floats or tags), each call framed by its length as four big-endian bytes, at most 16 MiB.
A frame begins with a 0 byte, which no line does, and that is how the daemon tells a
connection of calls. A call is a map `{id, schema, body}`, and its answer `{id, schema, body}`
with the same `id`. A connection takes any number of calls, each answered when it is done, so
a call that waits for a build holds up none sent after it. `schema` is 1; a call of another,
or bytes that are not deterministic CBOR, is answered `{"Refused": why}`.

| Body | Answer |
|---|---|
| `"Hello"` | `{"Hello": {version, schema}}` |
| `{"Apply": {files: [{path, bytes}], build, disk}}` | `{"Applied": {root, built}}` |
| `{"Cargo": request}` | `{"Cargo": reply}`: what `cargo ra-jit` asks for its builds |

`Apply` is a transaction of files of one loaded workspace, absolute paths: `bytes` is the
file's new text, or null to delete it. The next build reads all of them or none, with no look
at the disk, and `root` is the BLAKE3 root of the workspace's sources after it (32 bytes).
`disk` (default true) writes them too; a file that is no source yet, a new module, has to be
written. With `build` the programs that have the files are built and linked as
`cargo ra-jit build` builds them, `built` says how (`ok ...`, or why not), and the next
`cargo ra-jit build` places what was built. `cargo ra-jit apply PATH[=FROM]...` is a client of
it: `PATH` as it is on disk, as the file `FROM` is, or deleted with `PATH=`; `--json` prints
the answer as JSON, a rendering of the CBOR for people.

With `RA_JIT_HTTP` set to an address of the loopback (`127.0.0.1:0` picks a port, which the
daemon's log says), the daemon takes the same calls as `POST /v1` with the call as the body,
and answers in CBOR, or in JSON for `Accept: application/json`. A request has to send
`Authorization: Bearer` with the token in `http-token`: the loopback is open to every user of
the machine, and only you can read the file. `harness/api.py` tests both.

## Environment variables

These are read by `cargo ra-jit` and passed to the worker it starts. They are part of which
build a command line is, so changing one starts from a cargo build again.

| Variable | Meaning | Default |
|---|---|---|
| `RA_JIT_BACKEND` | `cranelift` or `llvm`. `llvm` needs a ra-jit built with `--features llvm` | `cranelift` |
| `RA_JIT_OPT` | LLVM optimisation level, 0 to 3. Functions are still compiled one at a time | 2 |
| `RA_JIT_EMIT` | LLVM instruction selection: `optimized` or `fast` | `optimized`; `fast` at level 0 |
| `RA_JIT_INLINE` | With LLVM at level 2 or 3, a program built whole (`runbench.py`'s builds) has the functions it calls in any crate built into it or shown to LLVM, as a release build does; the standard library's only when they are generic or `#[inline]`, as rustc does across crates. `crate` keeps that to the function's own crate, `0` builds nothing in, `all` does it in other builds with LLVM at level 2 or 3 too (never the daemon's, whose store keeps each function by its own code alone) | across crates in a whole program, the function's own crate otherwise |
| `RA_JIT_SPLICE` | `0` links in full after every edit instead of splicing | splice |
| `RA_JIT_OTHER_TOOLCHAIN` | `1` builds with the project's toolchain when it is not the pinned nightly, rather than with the pinned nightly, for a project pinned to a nightly near it; nothing checks that it agrees ([support.md](support.md#toolchain)) | not set |

`RA_JIT_TESTS` is read by each `cargo ra-jit test` alone and is not part of which build it
is: `affected` runs the tests an edit reached
([above](#running-only-the-tests-an-edit-reached)), `all` every test. The default is `all`.
`RA_JIT_SANDBOX=off` runs every test outside the sandbox
([above](#every-test-runs-in-a-sandbox)), and `RA_JIT_TEST_ENV` names more variables a test
is given under `affected`, by commas; both are read the same way.

These are read by the daemon when it starts, from the environment of whichever command
started it. To change one, run `cargo ra-jit stop` first.

| Variable | Meaning | Default |
|---|---|---|
| `RA_JIT_HOME` | the daemon's directory (also read by every client) | see above |
| `RA_JIT_IDLE_SECS` | seconds without a request before the daemon exits; 0 is never | 1800 |
| `RA_JIT_HTTP` | an address of the loopback, such as `127.0.0.1:7171`, where the daemon takes [calls](#the-agent-api) over HTTP | not set |
| `RA_JIT_MEMORY_MAX` | the most the workers hold together, a size such as `6G`, or `off` | half the machine's memory (on Linux, or of what its control group allows) |
| `RA_JIT_SPARE` | `ready` keeps a spare worker with the library loaded at all times, for the next project; `0` starts none, and a workspace loads once cargo has built it | a spare for each build of a new workspace, as it begins |
| `RA_JIT_OWN_BASE_MEMORY` | the most a worker may hold while it builds its own base in the background, a size such as `40G`, past which it gives that base up and edits patch rustc's executable; `off` is no bound | a quarter of the machine's memory (on Linux, or of what its control group allows) |
| `RA_JIT_LRU` | how many values of the queries a build in full holds the most of a worker keeps once the build is over, `mir=N,runtime=N,compiled=N,inlinable=N` (lowered MIR, runtime and monomorphized MIR, each instance's code, bodies to inline); 0 keeps every value. One let go is computed again when an edit asks for it ([#261](https://github.com/indexable-inc/blaze/issues/261)) | `mir=2048,runtime=2048,compiled=16384,inlinable=8192` |
| `RA_JIT_CACHE` | directory of the disk store, or `off` for none | `ra-jit/store` in the user's cache directory ([cache.md](cache.md)) |
| `RA_JIT_REMOTE` and the other store variables | see [cache.md](cache.md) | not set |

The disk store is on by default and shared by every daemon and checkout of the user. The
daemon reads `RA_JIT_CACHE` once, when it starts: a daemon started by a shell without the
variable keeps the default store whatever later shells have set, so run `cargo ra-jit stop`
before changing it.

## Known problems

- **The daemon's verdict is the build's, and it is not yet all of rustc's.** An error that
  rust-analyzer or ra-jit's borrow checker finds in the edited files fails the build, written
  as rustc writes it, with cargo's status; no rustc runs to say it again
  ([#84](https://github.com/indexable-inc/ra-jit/issues/84)). What they do not find is
  built: rust-analyzer reports few of rustc's errors for certain, and the borrow checker
  passes part of rustc's tests ([borrowck.md](borrowck.md)). Code rustc builds that ra-jit
  takes for an error fails the build too; that is a ra-jit bug to report. A lint the source
  denies fails it as well, for the lints rust-analyzer finds only where rustc does
  (`non_snake_case`, `non_camel_case_types`, `non_upper_case_globals`, `unused_must_use`),
  unless `--cap-lints` or `--force-warn` is among rustc's flags. `cargo ra-jit check` says
  what such a build would, when the daemon has it loaded, and what it finds in the whole
  workspace otherwise.
- Linux x86-64 and macOS arm64 have been run; on macOS only ripgrep, fd and hyperfine and
  the generated workspace of the harness. The code compiles for Windows, but every build
  there would go to cargo (the link step knows ELF and Mach-O), and the command has not
  been built or run there. The socket's permission and peer checks do nothing on Windows.
- On Linux `cargo ra-jit` was measured with the Cranelift backend only; LLVM has been run
  through the harness gate, not through `harness/cargo_ra_jit.py`. On macOS the harness was
  run under both.
- In ten runs of just's test suite through `harness/cargo_ra_jit.py` there were two
  differences from cargo that are not explained. One was a test failing in the
  daemon-built executable and passing under cargo. The other was between two executables
  cargo built, which points at the tests being sensitive to load, but that was not shown.
- A function rustc emitted no symbol for is assumed to be dead code. One bug came from that
  assumption being wrong (test functions, fixed); no other is known, and it is not proved
  that there is none.
- The check that only your user can reach the daemon is tested for the same user only.
- `cargo ra-jit` is not in the CI workflow. `harness/cargo_ra_jit.py` needs the network and
  about 20 minutes.

## Plain `cargo`, switched on globally

`cargo ra-jit` needs whoever runs the build to type something different. The global switch is
for when they won't: agents, scripts, editors that call `cargo build` themselves.

    ra-jit enable     # plain cargo now goes through ra-jit, for this user
    ra-jit status     # is it on, and what the daemon served or left to rustc
    ra-jit disable    # back to how it was

`ra-jit`, `cargo-ra-jit` and `ra-jit-rustc` have to be in the same directory.

### What `enable` changes

One key in `$CARGO_HOME/config.toml` (usually `~/.cargo/config.toml`):

    [build]
    rustc-workspace-wrapper = "<dir>/ra-jit-rustc"

And `$CARGO_HOME/bin/cargo-fmt` and `cargo-clippy`, for those of the two that are blaze's own:
links to `cargo-blaze`, with what was there kept in `$CARGO_HOME/blaze-kept`
([Plain `cargo <command>` after `enable`](#plain-cargo-command-after-enable)).

No shell profile or environment variable is touched. `enable` keeps a copy of the file as it
was in `ra-jit-enabled.json` next to it. `disable` restores that copy byte for byte, or, if
you edited the file in between, removes only its own line, and puts back the files the links
replaced.

### Wrappers you already have

- `build.rustc-wrapper` or `RUSTC_WRAPPER` (sccache and the like) is left alone and runs
  around `ra-jit-rustc`.
- An existing `rustc-workspace-wrapper` is chained: `ra-jit-rustc` runs rustc through it, and
  `disable` puts your line back.
- `enable` and `status` print both, so you can see what will run.

A `RUSTC_WORKSPACE_WRAPPER` in the environment, or a project `.cargo/config.toml` that sets
the key, takes precedence over the user config. ra-jit is not used in that workspace, and
`ra-jit status` run there says so.

### What you get, and what you don't

The daemon, its loaded workspaces, the sharing between worktrees and the store are the same
ones `cargo ra-jit` uses. The difference is where ra-jit sits. Cargo still plans the build and
calls the wrapper once per crate; the daemon answers for programs and test executables, and
workspace library crates are still compiled by rustc. A build with nothing to do costs what
cargo's own no-op costs. The daemon loads a build that rustc did once cargo exits: after
`cargo test` has run the tests, after `cargo run` has run the program.

When the daemon cannot serve a unit, rustc builds it and one line on stderr says why:

    ra-jit: the daemon had not loaded the build yet; rustc links `app`

`ra-jit status` counts these by reason. A build never fails because ra_jit is enabled: a
daemon that is missing, dies during the build, or is another version (it gets replaced) all
end in rustc doing the work. An error ra-jit cannot diagnose itself, such as a use after
move, fails with rustc's message and status 101, as under cargo.

The store is on by default in this mode and in `cargo ra-jit`: `$XDG_CACHE_HOME/ra-jit/store`,
else `~/.cache/ra-jit/store`, limited to a tenth of its disk (at most 32 GiB). `RA_JIT_CACHE=<dir>` moves it, `RA_JIT_CACHE=off`
disables it, `RA_JIT_CACHE_MAX` sets the limit. The socket and logs are in
`$XDG_RUNTIME_DIR/ra-jit`, else `~/.cache/ra-jit`.

### What has been checked

On Linux x86-64, under both backends, with a generated workspace, each of these gave the
same output, exit status and test results as cargo alone on the first build, a warm build
and a build after an edit: `cargo test`, `test --doc`, `test --no-fail-fast` with a failing
test, `run --example`, `run --release`, `run --target <host>`, `run --bin`,
`run --features -p`, `check`, `check --all-targets`, `build --workspace --all-targets`,
`cargo install --path`, and a `cargo check --message-format=json` running beside a build.
Also: 12 builds at once over 3 worktrees on one daemon, the daemon killed during a build,
both kinds of user wrapper, and the `enable`/`disable` round trip. ripgrep, fd and hyperfine
match cargo on every step. `harness/cargo_ra_jit.py` runs all of this.

Not checked: a proc macro defined in the workspace, `--target` for another triple,
`cargo install` from the registry, a real rust-analyzer session next to it, real sccache (a
stand-in wrapper was used). On Windows every build is cargo's in this mode and nothing was
run. macOS arm64 is below.

### Known problems

- Edits are slower than with `cargo ra-jit`, and on four of nine projects slower than cargo
  alone (one run each, Cranelift: ripgrep 1.32 s against 0.78 s, eza 2.02 against 1.32,
  tokei 1.23 against 0.70, axum 0.68 against 0.41). The cause has not been measured yet.
- The store grows fast: one harness pass over starship wrote 7.6 GiB.
- A socket path over 108 bytes cannot be bound; the build goes to cargo and says so. The
  default locations are well under that.


## macOS arm64

`cargo ra-jit` and plain cargo after `ra-jit enable` serve dev and test builds for
`aarch64-apple-darwin`. x86-64 macOS is not a target ra-jit knows, so builds for it are
cargo's.

Two things differ from Linux. Apple's linker has no way to let the first of two
definitions win, so the daemon links rustc's line with copies of the workspace's objects in
which every definition is weak, and its own object in front; this is the link the daemon
already did on macOS, and the line also gets `-Wl,-headerpad` (1 MiB to start with) so that
the executable can be spliced later. A program whose unwind table does not fit is linked
once more with the room it needs, which the links after keep (see
[platforms.md](platforms.md)). And there is no `/proc`: in the global mode, the cargo above the
wrapper is read through `libproc` and `sysctl`.

What was run, on an 8-CPU Apple M4 virtual machine with macOS 26.5 and Rust 1.99.0:
`harness/cargo_ra_jit.py --small`, under Cranelift and under LLVM (`RA_JIT_OPT=2`,
`RA_JIT_EMIT=fast`). Under each it passed: ripgrep, fd and hyperfine gave the same output,
exit status and test results as cargo alone on every step of both modes (134 comparisons),
and the 101 checks of the flags, the daemon and plain cargo held. Those include the
`enable`/`disable` round trip of `config.toml`, byte for byte, over five configurations, and
the second user at the socket, for which the harness uses `nobody` on macOS.

Seconds for the build after an edit, ra-jit / cargo alone, one run each:

| Project | `cargo ra-jit`, Cranelift | `cargo ra-jit`, LLVM | plain cargo, Cranelift | plain cargo, LLVM |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 2.08 / 1.43 | 0.40 / 0.67 | 1.35 / 0.97 | 1.07 / 0.69 |
| fd 10.3.0 | 0.24 / 0.42 | 0.25 / 0.43 | 0.26 / 0.42 | 0.29 / 0.42 |
| hyperfine 1.19.0 | 0.34 / 0.35 | 0.21 / 0.33 | 0.24 / 0.34 | 0.23 / 0.34 |

The ripgrep numbers are not steady. Two earlier Cranelift runs of the same step on the same
machine gave 0.52 / 0.73 and 1.08 / 1.30 for `cargo ra-jit`, and 1.86 / 0.78 and 1.30 / 1.30
for plain cargo; in every run the daemon did that build, and under plain cargo it was not
faster than cargo. Why ripgrep's edit takes one to two seconds in some runs was not looked
into. A build with no change took 0.03 to 0.06 s through `cargo ra-jit` against 0.31 to
0.41 s, and 0.16 to 0.19 s through plain cargo.

Builds that went to cargo in those runs, each with its reason printed: the first build of
every project, the build of source that does not compile, ripgrep's tests after an edit
under `cargo ra-jit` (a library the unit tests are linked with as rustc built it), and the
tests of ripgrep and hyperfine after an edit under plain cargo ("the build is not one the
daemon has loaded").

Not run on macOS: the projects of the harness other than those three, `cargo ra-jit` with
debug info looked at in a debugger, and anything on x86-64.

## Every subcommand

Andrew, 2026-10-26: blaze never runs rustc or cargo for anything, and the commands that are no
builds "should go to us and not cargo". So every `cargo blaze <command>` (and `blaze <command>`)
is blaze's own: no process of cargo, rustc, rustdoc, clippy-driver or rustfmt is started for it.
A command blaze has not written yet fails, says so and does nothing, with status 101:

    error: blaze has no `cargo publish` yet (it packages the crate first, a build blaze does not do yet); it hands nothing to cargo, so nothing was done
    note: what blaze has of each cargo command: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md#every-subcommand

A command cargo does not have is refused the same way (`error: no such command`): cargo would
run a plugin, `cargo-<name>`, from the path, and blaze runs none. `cargo blaze --list` prints
the table below; flags typed before the command (`cargo blaze -v fmt`) are read after it, as
cargo reads them. The table is `SUBCOMMANDS` in `crates/ra-jit/src/cargo/own/mod.rs`.

| Command | blaze's | What it is, and what it waits for |
|---|---|---|
| `build` (`b`), `run` (`r`), `test` (`t`), `check` (`c`) | the build | [above](#commands) |
| `metadata` | yes | `crates/blaze-metadata`: the workspace read by blaze-manifest, every locked package fetched by blaze-registry, resolved by blaze-resolve; `--no-deps`, `--features`, `--all-features`, `--no-default-features`, one `--filter-platform` (its `cfg` as blaze answers `rustc --print cfg` itself), `--manifest-path`, `--offline`. Held equal to cargo's by `harness/own_metadata.py`; against the pinned nightly's cargo, ripgrep, fd, helix, nushell and biome print the same JSON for each form (`harness/own_commands.py`, with `locate-project`, `verify-project`, `pkgid`, `read-manifest`, `fetch` and `clean`: 130 of 130 cases over the five). A package's `hints` has cargo 1.101's `"min-opt-level"`, always `null`: blaze-manifest does not read a value of it yet. Several `--filter-platform`s, and a workspace whose lock file is missing or stale, fail with the reason (choosing versions is #321) |
| `locate-project` | yes | `--workspace`, `--message-format plain\|json`, `--manifest-path` |
| `verify-project` | yes | `{"success":"true"}`, or `{"invalid":...}` and status 1 |
| `version`, `help` | yes | blaze's version and help |
| `fmt` | yes | `crates/blaze-fmt`: rustfmt and cargo-fmt at rust-lang/rust 12433c307 ported onto a syntax tree of blaze's own, filled by rust-analyzer's parser with rustc's spans, run in process. `cargo blaze fmt [--check] [-p NAME]... [--all] [--manifest-path PATH] [-v\|-q] [-- RUSTFMT_ARGS]` formats the targets blaze-manifest reads; `blaze fmt a.rs b.rs` formats files, with rustfmt's options. Held equal to stock rustfmt (the pinned nightly's) by `python3 harness/fmtdiff.py`: ripgrep, nushell, fd, helix and biome with their own rustfmt.toml, 6068 of 6068 files byte-equal (100%), and `--check` exit codes and diffs equal; the same with `max_width = 60`, 6068 of 6068 (100%); rustfmt's `tests/source`, 773 of 793 (97.5%; 18 more use line ranges, which it does not run). At blaze-fmt f43e4a20af on dev-aws-14. A file is refused with the reason, and left as it is, when it has nightly syntax rust-analyzer does not parse (postfix `.yield`, `.use` and `.match`, `const` block items, `use` closures, `&pin`, unsafe binders, `unsafe mod`, `mut ref`, `default` on an associated `const fn`), a parenthesized trait object with `+` (`(Foo) + 'static`), a `macro` with a parameter list, an `impl` or `mut` restriction, or a typed `try` block; a non-empty `ignore` option is an error |
| `clippy` | yes | `crates/blaze-lints`: 24 of clippy's lints ported onto rust-analyzer's analysis, run by the daemon, written as `cargo clippy` writes them; [below](#clippy) |
| `doc` (`d`) | yes | `crates/blaze-doc`: every page from rust-analyzer's HIR, no rustdoc; cargo's flags, `target/doc/<crate>/index.html`, search, source pages, themes. Against the pinned nightly's `cargo doc` (`harness/docdiff.py`), with their dependencies: fd 100.0% of rustdoc's pages, 99.9% of their members and impls, 99.4% of the resolved links; ripgrep 99.9%, 99.6%, 99.1%; nushell 99.5%, 99.2%, 98.4%; helix 98.9%, 99.0%, 98.7%; biome 99.0%, 99.2%, 98.1%. biome in 92.5 s against `cargo doc`'s 1028.0 s, nushell in 28.4 s against 497.1 s, its build scripts already run ([doc.md](doc.md)). The load still runs cargo for build scripts and proc macros, as every load does; doctests are `cargo blaze test --doc`'s |
| `tree` | yes | `crates/blaze-tree`, cargo 1.101's `cargo tree` ported onto blaze-resolve: every flag (`-p`, `--workspace`, `--exclude`, `-e`, `-i`, `--prune`, `--depth`, `--prefix`, `--no-dedupe`, `-d`, `-f`, `--target`, `--charset`, `--color`, the feature flags). Held equal to cargo's by `harness/own_tree.py`: 26 forms over ripgrep, fd, helix, nushell and biome, 130 of 130 byte-equal. Package patterns with globs (`-p 'serde*'`) and `-i` with no value are refused |
| `pkgid` | yes | the package's full spec from `Cargo.lock`, with cargo's errors for none or several matches |
| `fetch` | yes | every package `Cargo.lock` names, downloaded by blaze-registry. `--target` fetches every platform's, and with no lock file it fails (choosing versions is #321) |
| `clean` | yes | the target and build directories, one profile's (`--release`, `--profile`), or `--doc`, with `--target-dir`, `--dry-run` and cargo's `Removed` summary. `-p` and `--workspace` (one package's files) are refused |
| `new`, `init` | yes | `crates/blaze-new`, cargo 1.101's `ops/cargo_new.rs`: the manifest, source file, ignore file and repository, the workspace's `members`, and cargo's checks of the name. The repository is made by `git init` (or `hg`, `pijul`, `fossil`, as cargo runs them). Cargo runs rustfmt on the file it writes; blaze does not, so a `rustfmt.toml` above the package that would change the template is not applied. Held equal to cargo's by `harness/own_new.py`: 50 of 50 cases, what each prints and every file it leaves |
| `remove` (`rm`) | yes | the dependencies out of the member's manifest, with `toml_edit` keeping its layout and comments, as cargo 1.99's `cargo_remove.rs` and `toml_mut` do: the features that named them, then the `[workspace.dependencies]`, `[profile.*.package]`, `[replace]` and `[patch]` entries nothing uses any more; then the lock file resolved again by blaze with every version it keeps locked as it was (blaze-metadata's `relock`), honouring `--locked`. A workspace with no lock file is refused: writing one chooses versions (#321). Held equal to cargo's by `harness/own_remove.py`, manifests and lock files byte for byte: 43 of 43 cases on ripgrep and a made-up workspace |
| `vendor` | yes | every registry package of the lock file unpacked from its `.crate` into the vendor directory as cargo 1.99's `ops/vendor.rs` does, with `.cargo-checksum.json`, `--versioned-dirs`, `--no-delete`, `-s`/`--sync` and `--respect-source-config`, and the `[source]` configuration printed; the workspace resolved again first, as cargo does (blaze-metadata's `vendored`). A git package is refused: cargo vendors it with its manifest as `cargo package` writes it, which blaze does not write yet. A workspace with no lock file is refused: writing one chooses versions (#321). Held equal to cargo's by `harness/own_vendor.py`, every file by its sha256: 18 of 18 cases on fd, ripgrep and a made-up workspace |
| `add`, `update`, `generate-lockfile` | not yet | they choose versions, which blaze-resolve does not do yet ([#321](https://github.com/indexable-inc/blaze/issues/321)) |
| `bench`, `rustc`, `rustdoc`, `fix`, `install`, `package` | not yet | builds blaze does not do yet ([no-rustc.md](no-rustc.md)) |
| `search`, `owner`, `yank`, `login`, `logout` | yes | blaze-registry's `api.rs`, the `crates-io` crate 0.41's client of a registry's web API over blaze's own HTTP, and cargo 1.99's `ops/registry` around it: which registry (`--registry`, `--index`, `registry.default`, `source.*.replace-with`, the `Updating` of its `config.json`), the token (`--token`, `CARGO_REGISTRY_TOKEN`, `CARGO_REGISTRIES_<NAME>_TOKEN`, `credentials.toml` and the old `credentials`, read and written as cargo writes them, mode 0600), cargo's errors of the server (`errors`, a status not 200), and `search`'s table with `--limit` and its highlighted names. Of the credential providers only `cargo:token`, cargo's default, is blaze's: any other (`cargo:wincred`, `cargo:macos-keychain`, `cargo:libsecret`, a program) is refused, since cargo would start it. Held equal to cargo's by `harness/own_registry.py` against a mock registry: 73 of 73 cases, what each prints, every request the server saw and the credentials files left |
| `publish`, `info` | not yet | `publish` packages the crate first, which is a build of `package`; `info` is not written yet |
| `uninstall` | yes | cargo 1.99's `cargo_uninstall.rs` and install tracker: removes the programs from the install root's `bin` (`--root`, `CARGO_INSTALL_ROOT`, `install.root`, cargo's home) and their entries in `.crates.toml` and `.crates2.json`, written byte for byte as cargo writes them; specs, `-p`, `--bin`, the package of the directory, cargo's `Summary` of several. Held equal to cargo's by `harness/own_uninstall.py`: 18 of 18 cases, what each prints and the files it leaves |
| `read-manifest` | yes | the package's entry of `metadata --no-deps`, as cargo prints it |
| `report` | yes | `future-incompatibilities` (`future-incompat`): the report a build left in the build directory, with `--id`, `-p` and `--color`, as cargo 1.99's `future_incompat.rs` prints it. `timings`, `sessions` and `rebuilds` are unstable in cargo: refused as cargo refuses them without `-Z build-analysis`, and with it they fail saying blaze's builds do not write the logs they read. Held equal to cargo's by `harness/own_report.py`: 23 of 23 cases |
| `config` | yes | `config get`, by blaze-manifest's port of cargo 1.99's `cargo_config.rs`: `--format toml`, `json` and `json-value`, `--show-origin`, `--merged no`. Unstable in cargo, and refused as cargo refuses it unless `-Z unstable-options`, `unstable.unstable-options` or `CARGO_UNSTABLE_UNSTABLE_OPTIONS` allows it. Held equal to cargo's by `harness/own_config.py`, run as `cargo blaze config`: 41 of 41 cases |
| any other | no | `error: no such command`; blaze runs no cargo plugin |

### `clippy`

`cargo blaze clippy` (and `blaze clippy`) lints with blaze's own lints, `crates/blaze-lints`:
each a port of one of clippy's, with its file naming the clippy source it follows, keeping its
messages, spans, suggestions and conditions. The daemon runs them over the crates it loads as
`cargo blaze check` would, and the client writes them as `cargo clippy` does: rustc's
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
2026-10-27, with `cargo blaze clippy --workspace --all-targets` against the pinned nightly's
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

- biome lints nothing: `cargo blaze check` of it fails in `biome_analyze` and `biome_wasm`
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

### Plain `cargo <command>` after `enable`

`enable` changes one key, `build.rustc-workspace-wrapper`, so plain `cargo build` and `cargo
test` are still cargo's process: cargo plans the build and starts the wrapper for each workspace
crate, and the wrapper's first build is rustc's ([What a build does](#what-a-build-does)). The
other commands stay cargo's entirely. Here is what each way of sending them to blaze needs:

- **`fmt` and `clippy`** are not built into cargo: it runs `cargo-fmt` and `cargo-clippy`,
  looking in `~/.cargo/bin` before `PATH`. `enable` puts links named so there, to
  `cargo-blaze`, for each of the two that is blaze's own (`cargo blaze --list`), so plain
  `cargo fmt` and `cargo clippy` run blaze's, with nothing printed of it. The file a link
  replaces, rustup's proxy, is kept in `~/.cargo/blaze-kept` and `disable` puts it back;
  `status` says whose each command is, and notices when `rustup update` put its proxy back
  (`enable` links it again). macOS and Linux only. An `[alias]` (`fmt = "blaze fmt"`) would
  route them too, but cargo warns on every run that it shadows an external command and means
  to make that an error
  ([rust-lang/cargo#10049](https://github.com/rust-lang/cargo/issues/10049)).
- **Built-in commands** (`build`, `test`, `check`, `doc`, `tree`, `new`, ...) cannot be aliased:
  cargo ignores an alias that shadows a built-in. Two ways remain. `build.rustc` set to blaze
  keeps cargo's process but answers every rustc that cargo starts (`-vV`, `--print`, each
  compile), which is no-rustc.md's step M. Or `rustup toolchain link blaze <dir>`, with
  `<dir>/bin/cargo` being blaze and the project's toolchain named `blaze`, so that rustup's
  `cargo` proxy starts blaze and no cargo process at all. The second way changes the
  toolchain that editors and rust-analyzer see (their `cargo metadata` and `rustc --print
  sysroot` would be blaze's, which it answers). It is not written; it touches `switch.rs`,
  which is the build lane's.
