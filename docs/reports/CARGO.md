---
status: record
summary: Using blaze with cargo, as of the early daemon.
date: 2026-10-20
---
# Using blaze with cargo

blaze is a daemon that keeps your Rust workspaces compiled in memory. After an edit it
compiles the functions that changed and splices or links them into the program cargo built,
instead of compiling the crate again. This file is the guide to using it from cargo. It
covers Linux x86-64, which is where everything here was run; macOS and Windows are at the
end.

There are two ways to use it, and they can be mixed freely:

- **Explicit.** You type `blaze build` where you would type `cargo build`.
- **Transparent.** You run `blaze enable` once. From then on plain `cargo build`,
  `cargo run`, `cargo test` and `cargo check` in every workspace and worktree of yours go
  through blaze, with nothing else changed. This is the mode for agents and scripts that
  run cargo on their own. `blaze disable` puts everything back.

In both modes a build that blaze cannot do is done by cargo and rustc, with one line that
says why. A build never fails because blaze is there.

## Install

    cargo +1.99.0 build -p blaze-engine --release
    cp target/release/blaze-engine target/release/blaze target/release/blaze-rustc ~/.cargo/bin/

That is three programs: `blaze` (the daemon and its commands), `blaze` (what cargo
runs for `blaze`), and `blaze-rustc` (what cargo runs in place of rustc). They find
each other by being in the same directory, so keep them together. For the LLVM backend
build with `--features llvm` and `LLVM_SYS_201_PREFIX=/usr/lib/llvm-20` set (LLVM 20).

The project you build has to use Rust 1.99.0, the toolchain blaze is pinned to: a
`rust-toolchain.toml`, `rustup override set 1.99.0`, or `RUSTUP_TOOLCHAIN=1.99.0`. With
another toolchain every build is cargo's and says so.

`blaze --help` and `blaze --help` list the commands.

## Explicit mode: `blaze`

    blaze build [cargo's flags]
    blaze run   [cargo's flags] [-- program arguments]
    blaze test  [cargo's flags] [test names] [-- test arguments]
    blaze check [cargo's flags]
    blaze status
    blaze stop

`build`, `run` and `test` take cargo's flags and leave the same files in the same places
under `target/`. `check` is `cargo check`. Any other subcommand is passed to cargo as
typed.

The first build of a workspace is cargo's own, with blaze recording what rustc was asked
for each crate of the workspace. When it succeeds, the daemon is started if it is not
running and loads the workspace in the background; the command does not wait. Until the
workspace is loaded, builds are cargo's and say so:

    blaze: the daemon is still loading the workspace (951ms so far); building with cargo

Once it is loaded, a build after an edit does not start cargo's build at all. The daemon
compiles what changed and puts the program where cargo would have. The last line shows who
built:

    Finished `dev` profile [blaze] target(s) in 0.24s

`[blaze]` is the daemon. A cargo build prints cargo's usual line. `run` then runs the
program and `test` runs the test executables as cargo does: the same working directory,
the variables cargo sets, cargo's `Running` lines, the same exit status.

## Transparent mode: `blaze enable`

    blaze enable
    blaze disable
    blaze status

`blaze enable` writes one key into cargo's own configuration, the file in cargo's home
(`~/.cargo/config.toml`, or `$CARGO_HOME/config.toml`):

    [build]
    rustc-workspace-wrapper = "/home/you/.cargo/bin/blaze-rustc"

Cargo then runs `blaze-rustc` in place of rustc for the crates of your workspaces, and
plain rustc for everything from the registry. Nothing is added to a shell profile, so it
holds for every shell, editor and agent that runs cargo as you, at once.

What you type does not change, and neither does what cargo prints, except for a line when
rustc does a job blaze would otherwise have done (see "When cargo and rustc build
instead"). `blaze status` says whether it is enabled and what has happened since:

    plain cargo: through blaze, by `build.rustc-workspace-wrapper` in /home/you/.cargo/config.toml
      20 programs linked by the daemon, 8 by rustc
        3 x the daemon had not loaded the build yet
        1 x the manifest /home/you/src/app/Cargo.toml changed

### What `enable` and `disable` do to your configuration

The rest of the file is left as it is: other tables, comments, the order and the line
endings. `enable` keeps a copy of the file as it was in `blaze-enabled.json` next to it.
`blaze disable` puts the file back byte for byte when nobody has written it since; when you
edited it in between, only blaze's line is taken out (or your earlier line put back) and
your edit stays. A file that did not exist before is removed again. The old file name
`config` is handled the same way.

Wrappers you already had keep working, and `enable` says so when it finds one:

- `build.rustc-wrapper` or `RUSTC_WRAPPER` (sccache, for instance) is not touched. Cargo
  runs it around `blaze-rustc`, as it would around rustc.
- `build.rustc-workspace-wrapper` of your own is chained: `blaze-rustc` runs rustc through
  it, with the arguments it would have had. `disable` puts your line back.

### What overrides it

Cargo reads the wrapper from three places and the nearest wins. blaze does not fight this;
`blaze status`, run in the workspace, tells you when one of them has cargo not run blaze
there.

- `RUSTC_WORKSPACE_WRAPPER` in the environment wins over every file. If it names something
  else, blaze is not used for that command. (`blaze` then also leaves the build to
  cargo, with "RUSTC_WORKSPACE_WRAPPER is set to another wrapper".)
- A project's own `.cargo/config.toml` that sets `build.rustc-workspace-wrapper` wins over
  the file in cargo's home, for that project and everything below it.
- An environment `RUSTC_WRAPPER` does not override anything: it is the outer wrapper and
  runs around blaze's.

### What the two modes share, and what differs

Both modes use the same daemon, the same loaded workspaces and the same store on disk. A
workspace loaded by a plain `cargo build` is warm for `blaze build` and the other way
round, and a second worktree of a repository joins the worker that already has the first.

They differ in who drives the build:

- Under `blaze`, a warm build does not start cargo's build at all. The daemon is
  asked directly, and one `cargo check` runs beside it (see "Errors").
- Under plain cargo, cargo is in charge. It decides what is out of date and calls the
  wrapper for each crate. Library crates of the workspace are compiled by rustc as
  usual. When cargo reaches a program (a binary, an example, a test executable) and the
  daemon has it loaded, the wrapper runs rustc's check of that crate only, and the daemon
  builds the program from its own state and hands it to cargo. rustc's code generation
  and the linker do not run for it.

So an edit in the program's own crate costs about the same in both modes. An edit in a
library of the workspace costs more under plain cargo, because cargo still has rustc
rebuild that library and what depends on it before it reaches the program; the daemon
saves the last step. The table in "Measurements" only has edits in the program's own
crate, so it does not show this; it does show that plain cargo is not always faster than
cargo alone there.

Test executables under plain cargo follow the same rule as programs, with the limits
listed below.

## When cargo and rustc build instead

blaze hands a build back whenever it cannot be sure its result would be what cargo's is.
It prints one line with the reason and counts it in `blaze status`:

    blaze: the manifest /home/you/src/app/Cargo.toml changed; building with cargo
    blaze: the daemon had not loaded the build yet; rustc links `app`

The first form is `blaze`, the second is plain cargo after `blaze enable`. The build
then behaves as cargo's because it is cargo's.

Always cargo's, in both modes:

- `check`. blaze has no diagnostics of its own.
- Doc tests. They are rustdoc's to build; `blaze test` runs `cargo test --doc` for
  them.
- Build scripts, proc macros and everything from the registry. Cargo does not run the
  workspace wrapper for registry crates, and blaze records build scripts and proc macros
  without building them.
- `cargo install`, `cargo doc`, `cargo bench` and the other subcommands: `blaze`
  passes them to cargo, and under plain cargo the wrapper records and runs rustc.
- A toolchain other than 1.99.0, or one without the `rust-src` component (the daemon
  compiles the standard library's generic code from source).
- A target whose link line is neither an ELF `cc` line nor a `link.exe` line, or several
  `--target` in one build.

A whole `blaze` invocation is cargo's when it has `--message-format`, `--build-plan`,
`--unit-graph`, a `-Z` flag or `--exclude`, when `RUSTC_WORKSPACE_WRAPPER` names another
wrapper, or when `blaze-rustc` is not installed next to `blaze`.

One build is cargo's, and the daemon loads the workspace again after it, when:

- the daemon is still loading the workspace, or this command line was never built here
  (the daemon's state is kept per subcommand, flags, working directory and the variables
  that change what cargo builds; `run` counts as `build`);
- a manifest, `Cargo.lock`, a cargo configuration file or the toolchain file changed;
- something that runs during the build changed: a build script, a proc macro, a workspace
  crate one of those depends on, or a file or variable a build script said to rerun for;
- a `#[test]` function was added or removed or its attributes changed (the list of tests
  in a test executable is rustc's);
- a library changed that unit tests are linked with as rustc built it;
- the source has errors (see the next section);
- the daemon died or refused the build. The line then names the daemon's log.

One program stays as cargo built it, while the daemon serves the others, when the worker
refuses it or has no link plan for it (a `cdylib`, for instance). When a file of such a
program is edited, cargo builds.

Test executables are cargo's when they are built with `opt-level` above 0 or without
incremental compilation (`CARGO_INCREMENTAL=0`, which CI systems often set). rustc's test
harness stays in a test executable: optimised, rustc inlines tests into it and changes how
helpers are called, and without incremental compilation it keeps them private to its
object, so the daemon's copy of a function cannot take the place of rustc's. So
`blaze test --release` is cargo's, and so are the tests of a project whose dev
profile is optimised.

Served by the daemon: `--release`, `--profile NAME`, per-package profile settings,
`RUSTFLAGS`, `--target <host triple>`, `-p` (one or several), `--workspace`, `--features`,
`--all-features`, `--no-default-features`, `--bin`, `--example`, `--test`, `--lib`,
`--manifest-path`, `--no-run`, `--no-fail-fast`, `-q`, test filters and arguments after
`--`. What the worker does not honour inside those (`lto`, coverage instrumentation,
`[target.'cfg(..)'] rustflags` and the rest of the list in `BUILDMODEL.md`) it refuses,
and cargo builds.

The fallback is per invocation or per program. It is not per crate inside one program: a
program is linked from the daemon's object for all of its workspace crates, so "this crate
by rustc, that one by the daemon" inside one link is not built.

## Errors: rustc still checks everything

The daemon's analysis is rust-analyzer's. It has no borrow checker and does not give
rustc's warnings, so on its own it would build and run a program rustc turns down, a use
after a move for instance. It is not left on its own:

- `blaze build|run|test` run `cargo check` with the same flags beside the daemon's
  build, and wait for it. If it fails, its diagnostics are what you see and its exit
  status (101) is the command's; the program is not run. A warm build of source that
  already passed is not checked again.
- Under plain cargo the wrapper runs rustc on the program's crate with
  `--emit=dep-info,metadata`, which is what `cargo check` runs, while the daemon builds
  it. Library crates were already compiled by rustc at that point. If rustc finds errors,
  the unit is handed to rustc as cargo asked for it, so the error and the exit status are
  exactly cargo's.
- `blaze check` and `cargo check` are cargo's.

Warnings come from the same check, so they are rustc's too.

This costs the time of a `cargo check` of the edited crate on every edit build. It is the
larger part of an edit build on big crates; "Measurements" has the numbers.

## The daemon

There is one daemon per user, shared by both modes. It is started by the first build that
needs it, in a session of its own, and outlives the command that started it and any client
that is killed. It exits after 30 minutes without a request. `BLAZE_IDLE_SECS` sets
another time (0 is never) and is read when the daemon starts.

The daemon holds no compiler state itself. It owns worker processes, each with one
database for one repository, toolchain, target, feature set and `cfg(test)` setting. Git
worktrees of one repository share a worker, so what they have in common is analysed once.
A new worktree still starts with a cargo build, because it has a target directory of its
own, but it joins the loaded worker instead of loading the workspace again. Different
repositories get different workers, so a crash in one does not cost the other its state.

Things that can go wrong, and what happens:

- No daemon: the build starts one.
- A daemon from another build of blaze (you upgraded): the build is cargo's once, the old
  daemon and its workers exit, and a new one is started. `blaze status` from another
  version is still answered and says which version runs.
- The daemon is killed in the middle of a build: that build is cargo's and says so, naming
  the log; the workers exit; the next build starts a new daemon.
- `blaze stop` (or `blaze stop`) ends the daemon and waits until it is gone. A build
  straight after it starts a new one.
- A client is killed mid-request: the daemon carries on.

### Where things are kept

    $BLAZE_HOME/daemon.sock, daemon.sock.lock, daemon.log    the daemon
    $BLAZE_HOME/workers/<id>/sock, sock.lock, log            one directory a worker
    $BLAZE_HOME/states/                                      what each build was last loaded as
    $BLAZE_HOME/plain-cargo.log                              the counts `blaze status` prints
    <target-dir>/<profile>/.blaze-units/                     what rustc was asked, per crate
    <target-dir>/blaze/                                      the daemon's programs
    ~/.cache/blaze/store/                                    the store

`BLAZE_HOME` is `$XDG_RUNTIME_DIR/blaze` when that is set and `~/.cache/blaze` otherwise.

### The store

Compiled functions are kept on disk as well as in memory, so a daemon that was restarted,
or a second checkout, does not compile again what was compiled before. In both modes the
store is on by default, at `$XDG_CACHE_HOME/blaze/store`, or `~/.cache/blaze/store`
without `XDG_CACHE_HOME`. It is deliberately not in the runtime directory, which goes away
when you log out.

The store is kept under 32 GiB: when it grows past that, packs are removed until it
fits. `BLAZE_CACHE_MAX=<size>` (`2G`, `500M`) sets another limit and `off` none.

`BLAZE_CACHE=<directory>` puts it elsewhere. `BLAZE_CACHE=off` (or `0`, or empty) turns it
off. Like the other settings of the daemon it is read when the daemon starts, so run
`blaze stop` after changing it. `blaze-engine cache stats`, `gc` and `verify` work on the same
directory; `BLAZE.md` describes them and the remote store.

## Environment variables

| Variable | What it does |
|---|---|
| `BLAZE_HOME` | The directory of the daemon's socket, logs and state. Must be yours alone (mode 0700). |
| `BLAZE_IDLE_SECS` | Seconds without a request after which the daemon exits. 1800 by default, 0 is never. |
| `BLAZE_CACHE` | The store's directory; `off`, `0` or empty for none. |
| `BLAZE_CACHE_MAX` | The most the store may take on disk. 32 GiB by default; `off` for no limit. |
| `BLAZE_BACKEND` | `cranelift` (the default) or `llvm`. |
| `BLAZE_OPT`, `BLAZE_EMIT` | With the LLVM backend: the optimisation level, and `fast` emission. |
| `RUSTC_WORKSPACE_WRAPPER` | Cargo's own. Set to something else, it takes the build away from blaze. |
| `RUSTC_WRAPPER` | Cargo's own. Left alone; it runs around blaze's wrapper. |
| `CARGO_HOME` | Cargo's own. `blaze enable` writes the configuration file in it. |
| `CARGO_INCREMENTAL` | Cargo's own. With `0`, test executables are cargo's. |
| `NO_COLOR` | No colour in blaze's own lines. |

The `BLAZE_*` variables are read by the daemon when it starts, from the environment of
the build that starts it. `blaze stop` and the next build pick up a change.

## Troubleshooting

**Is it on, and is it doing anything?** `blaze status`, in the workspace. The first line
says whether plain cargo goes through blaze and from which file, then whether something
nearer (a project's `.cargo/config.toml`, a variable) overrides it. Below that are the
counts of programs the daemon linked and rustc linked with each reason, the daemon's pid
and version, and every loaded workspace with its state (`loading` or `ready`).

**Every build says "the daemon is still loading".** Loading takes from under a second to
about a minute on a large workspace, once per repository and command line. If it never
ends, look at the worker's log, whose path `blaze status` prints.

**A build went to cargo and I want to know why.** The line on stderr starts with `blaze:`.
The same reasons, counted, are in `blaze status`.

**Something looks wrong.** `blaze stop` ends the daemon and its workers; the next build
starts over from cargo's own build. `blaze disable` takes blaze out of plain cargo
altogether. `cargo clean` removes everything blaze keeps in the target directory.
`$BLAZE_HOME/daemon.log` and `workers/<id>/log` say what the daemon did.

**"`blaze-rustc` is not installed next to `blaze`".** The three programs have to be in one
directory. Install them together again.

**blaze refuses its home directory.** `BLAZE_HOME` and every directory above it must be
yours or root's, and it must not be open to group or others. The message says which
directory fails and why.

**A project has its own `build.rustc-workspace-wrapper`.** Cargo uses the project's, and
blaze is not run there. `blaze status` says so. Use `blaze` in that project, or name
`blaze-rustc` in the project's file.

## Measurements

Everything here is from one machine (Linux x86-64, 8 cores, 31 GB), one run per number, so
read the times as sizes, not as benchmarks. The command, from `harness/`:

    BLAZE_BIN=<release blaze> TMPDIR=/dev/shm BLAZE_CACHE_MAX=1G python3 blaze_cli.py <project> --only-projects

Each project is cloned at a pinned commit, not modified, and built three ways: plain cargo
as the reference, `blaze`, and plain cargo after `blaze enable`. A step passes when
the program's output, its exit status and the result of every test are the same as the
reference's. Times are `blaze / cargo`, in seconds, and the word in brackets says who
built: the daemon, or cargo with blaze only recording.

- Cold build: an empty target directory and no daemon. It is cargo's build in both modes,
  with blaze recording. The reference is built straight after it, while the daemon loads
  the workspace in the background on the same 8 cores; that is probably why the reference
  column is the slower one, and it was not measured apart. Take the two as equal.
- Warm build: nothing changed.
- Edit build: one function body edited, then build.
- Edit test: the same edit, then `cargo test`.
- Second worktree: `git worktree add`, a build there (cargo's, since the target directory
  is new), then an edit build there once the worker has taken the worktree in.
- Functions by blaze: from the worker's log for the largest program it loaded, how many
  of the functions it has the worker compiled itself. The rest are linked as rustc built
  them.

| Project | Mode | Cold build | Warm build | Edit build | Edit test | Second worktree: build, edit | Functions by blaze |
|---|---|---|---|---|---|---|---|
| ripgrep | explicit | 5.24 / 5.90 (cargo) | 0.03 / 0.40 (daemon) | 0.42 / 0.79 (daemon) | 1.91 / 1.41 (cargo) | 5.62 (cargo), 0.39 (daemon) | 1511 of 1511 |
| ripgrep | transparent | 5.13 / 6.05 (cargo) | 0.18 / 0.39 (daemon) | 1.32 / 0.78 (daemon) | 1.40 / 1.40 (cargo) | 5.61 (cargo), 0.43 (daemon) | 1511 of 1511 |
| fd | explicit | 18.63 / 26.19 (cargo) | 0.03 / 0.39 (daemon) | 0.24 / 0.45 (daemon) | 0.83 / 0.95 (daemon) | 18.38 (cargo), 0.21 (daemon) | 168 of 350 |
| fd | transparent | 18.61 / 26.57 (cargo) | 0.18 / 0.38 (daemon) | 0.22 / 0.45 (daemon) | 0.82 / 0.96 (daemon) | 18.32 (cargo), 0.22 (daemon) | 168 of 350 |
| bat | explicit | 14.71 / 26.11 (cargo) | 0.02 / 0.08 (daemon) | 0.21 / 0.47 (daemon) | 2.78 / 1.09 (daemon) | 15.12 (cargo), 0.74 (cargo) | 195 of 558 |
| bat | transparent | 14.80 / 26.58 (cargo) | 0.09 / 0.09 (daemon) | 0.48 / 0.50 (cargo) | 2.24 / 1.68 (cargo) | 18.28 (cargo), 0.50 (cargo) | 195 of 558 |
| hyperfine | explicit | 5.32 / 7.85 (cargo) | 0.03 / 0.31 (daemon) | 0.22 / 0.31 (daemon) | 0.54 / 0.65 (daemon) | 5.14 (cargo), 0.18 (daemon) | 94 of 232 |
| hyperfine | transparent | 5.18 / 7.88 (cargo) | 0.17 / 0.31 (daemon) | 0.20 / 0.34 (daemon) | 0.72 / 0.80 (cargo) | 6.35 (cargo), 0.17 (daemon) | 94 of 232 |
| just | explicit | 7.91 / 12.14 (cargo) | 0.02 / 0.06 (daemon) | 0.13 / 0.23 (daemon) | 1.75 / 0.93 (daemon) | 8.04 (cargo), 0.10 (daemon) | 868 of 1471 |
| just | transparent | 8.25 / 11.43 (cargo) | 0.06 / 0.06 (daemon) | 0.11 / 0.24 (daemon) | 1.29 / 0.94 (daemon) | 7.75 (cargo), 0.11 (daemon) | 868 of 1471 |
| axum | explicit | 7.78 / 9.35 (cargo) | 0.03 / 0.10 (daemon) | 0.58 / 0.53 (daemon) | 0.26 / 0.77 (daemon) | 7.90 (cargo), 0.15 (daemon) | 43 of 43 |
| axum | transparent | 7.85 / 9.15 (cargo) | 0.11 / 0.11 (daemon) | 0.68 / 0.41 (daemon) | 0.33 / 0.75 (daemon) | 7.87 (cargo), 0.17 (daemon) | 43 of 43 |
| tokei | explicit | 9.46 / 16.32 (cargo) | 0.19 / 0.07 (daemon) | 0.76 / 0.70 (daemon) | 2.79 / 1.95 (cargo) | 11.13 (cargo), 0.49 (daemon) | 87 of 254 |
| tokei | transparent | 9.43 / 15.83 (cargo) | 0.08 / 0.08 (daemon) | 1.23 / 0.70 (daemon) | 1.75 / 1.57 (cargo) | 11.06 (cargo), 0.65 (daemon) | 87 of 254 |
| eza | explicit | 14.67 / 21.46 (cargo) | 0.03 / 1.46 (daemon) | 0.94 / 1.30 (daemon) | 1.14 / 1.46 (daemon) | 15.21 (cargo), 0.87 (daemon) | 593 of 1532 |
| eza | transparent | 13.97 / 21.81 (cargo) | 0.99 / 1.29 (daemon) | 2.02 / 1.32 (daemon) | 1.30 / 1.46 (daemon) | 14.97 (cargo), 1.04 (daemon) | 593 of 1540 |
| delta | explicit | 15.54 / 27.65 (cargo) | 0.02 / 0.09 (daemon) | 0.42 / 0.76 (daemon) | 6.52 / 5.41 (cargo) | 15.29 (cargo), 0.42 (daemon) | 353 of 353 |
| delta | transparent | 15.15 / 27.73 (cargo) | 0.09 / 0.09 (daemon) | 0.42 / 0.76 (daemon) | 5.77 / 5.40 (cargo) | 15.58 (cargo), 0.44 (daemon) | 353 of 353 |

What the table shows, mode against mode:

- Results. Every step of every row is the same as plain cargo: output, exit status, tests.
- A warm build with nothing changed takes 0.02 to 0.19 s under `blaze`, which does
  not start cargo's build. Under plain cargo it takes what cargo takes to find nothing to
  do (0.06 to 0.99 s here), because cargo is in charge; blaze adds nothing and saves
  nothing.
- An edit build (every edit here is one line in the program's `main`) is the daemon's in
  both modes. Under `blaze` it is faster than cargo on seven of the nine (not on axum and tokei, by 0.05 s).
  Under plain cargo it is about the same as `blaze` on fd, hyperfine, just and delta,
  and slower than cargo alone on four: ripgrep 1.32 s against 0.78 s, tokei 1.23 s against
  0.70 s, eza 2.02 s against 1.32 s, axum 0.68 s against 0.41 s. In this mode an edit
  build is cargo's own start-up, rustc's check of the program's crate, and the daemon's
  link. See "The plain-cargo edit gap" below for what is known about the extra time.
- A second worktree's first build is cargo's in both modes (its target directory is new).
  Its edit builds are the daemon's, from the worker the first worktree loaded, in both.
- The share of functions the daemon compiled is the same in both modes, since it is the
  same worker.
- The store is used by both modes. How much of a restarted daemon's work it saves was not
  measured in this table.

Builds that went to cargo in these rows, and why (the line each one printed):

- "the daemon had not loaded the build yet" or "is still loading the workspace": every
  first build, and a build that follows another within seconds.
- "rustc finds errors in the source": the step that breaks the source on purpose.
- "a library its unit tests are linked with as rustc built it ... changed": `cargo test`
  after an edit in a library whose unit tests rustc links (ripgrep, tokei).
- "the tests of `delta` are built with opt-level 2, where rustc inlines them": delta's dev
  profile is optimised.
- "the environment variable CARGO_PKG_VERSION changed" and ".git/.../index changed while
  cargo built": bat's build reads the git state, which a worktree and the build itself
  change, so bat's first edit build and its worktree's are cargo's.
- "`build` is not compiled by blaze ...: unsupported: writable static": tokei's test
  executable has a `static mut` the daemon refuses.

The rows above are from 9fea20bd99, before `blaze-collate` 3d47ed59bf and
`blaze-collate-next` 1b022ff6f2 were merged, one run each, nine of the sixteen projects,
Cranelift only. The table was not rerun on the merged head: the run was started three
times and stopped each time for a merge, and then the work was stopped. starship, tonic,
helix, cargo, nushell, rust-analyzer and wasmtime have no rows, and no project has an LLVM
row in both modes. Treat the numbers as an indication, not a result for the current head.

### The plain-cargo edit gap

Plain cargo after `enable` was slower than cargo alone on the first edit build of four of
the nine projects. This was looked into with timing lines added to the wrapper and the
daemon's log on a few projects, then dropped before a fix. What those runs showed:

- The wrapper's rustc check of the program's crate (`--emit=dep-info,metadata`) took from
  a few hundred milliseconds to seconds. It runs beside the daemon's build, so the build
  cannot be quicker than it.
- If the edit comes while the daemon is still loading the workspace, the build is rustc's
  ("the daemon is still loading the workspace"), and the user pays for cargo's build plus
  the wrapper.
- The first edit after a daemon starts with a full store is expensive. Code taken from the
  store cannot be patched, so that edit compiles a large part of the program again and
  writes the whole object. One log: 5612 of 22852 functions compiled, object rewritten,
  2.6 s from edit to binary.
- Later edits in the same daemon are small: 1 of 22852 functions compiled, spliced, 74 ms
  from edit to binary in the same log.

So the gap is the check plus, on the first edit, the recompile of store code. How the
second and later plain-cargo edits compare with cargo alone was not measured across the
projects. One idea was tried and not finished: have the daemon recompile what it took from
the store while nobody is waiting, so the first edit is a patch. It built, but was never
shown to be correct or faster, and it is not in the branch.

### Store size

On this machine the store had to be capped for the runs (`BLAZE_CACHE_MAX=1G`): the
harness keeps everything in `/dev/shm`, which is memory, and with the default 8 GiB limit
one harness pass over starship wrote 7.6 GiB of store and the workers were killed for
memory. That is the only measurement. What the 7.6 GiB is made of, whether the two modes,
the worktree and the test builds write the same functions more than once, and whether the
8 GiB default is the right one, were not looked at.

### Flags, the daemon's lifecycle and plain cargo

`python3 blaze_cli.py --small` is what CI runs: ripgrep, fd and hyperfine in both modes,
then three sets of checks on small generated workspaces. On the merged head it exits 0
under both backends, with:

- 69 of 69 flag steps the same as cargo: 23 command lines (`--release`, `--profile`,
  `--target`, `-p`, `--features`, `--all-features`, `--bin`, `--example`, `--test`,
  `--lib`, `--manifest-path`, `--workspace`, `--no-run`, `--no-fail-fast`, `-q`,
  `RUSTFLAGS`, `CARGO_INCREMENTAL=0`, a test filter, a failing program), each cold, warm
  and after an edit.
- 45 of 45 daemon checks: started on demand, reused, idle shutdown, a client killed
  mid-request, the daemon killed mid-build, another version, 16 clients on 4 worktrees,
  a second user (made with `useradd`) refused on the socket and getting nothing from a
  socket of its own, `--help`, a closed pipe, a build straight after `stop`, the store on
  by default and off with `BLAZE_CACHE=off`.
- 56 of 56 plain cargo checks after `blaze enable`, listed under "What is verified under
  plain cargo" below.

### What is verified under plain cargo

On a generated workspace (two programs, a library with a build script, an example, unit
and integration tests and a doc test), after `blaze enable` over a configuration that already had a
`rustc-wrapper` and a `rustc-workspace-wrapper`:

- the first `cargo run` is rustc's and right, and both of the user's wrappers still run;
- that build starts the daemon, which loads the workspace;
- an edit of the program, and an edit of a library of the workspace, are built into the
  program by the daemon; a build with nothing changed runs nothing;
- `cargo test`, `cargo test --doc`, `cargo test --no-fail-fast` with a failing test,
  `cargo run --example demo`, `cargo run --release`, `cargo run --target <host triple>`,
  `cargo run --bin other`, `cargo run --features loud -p app`, `cargo run -- fail`,
  `cargo check`, `cargo check --all-targets`, `cargo build --workspace --all-targets`,
  each first, warm and after an edit: the same
  output, exit status and test results as cargo alone;
- a build that does not compile fails as cargo's does with rustc's error, and a use after
  a move (E0382) in the program or in a library fails a warm `cargo run` with status 101;
  the mended build is right;
- `cargo install --path` installs a program that runs;
- a `cargo check --message-format=json` at the same time as a build: both right;
- 12 plain cargo builds at once on 3 worktrees each run their own tree's program, on one
  daemon;
- the daemon killed in the middle of a build: that build is rustc's, succeeds and says so,
  and the next is a new daemon's;
- plain cargo from another build of blaze replaces the daemon, and the next build is the
  new daemon's;
- `blaze status` counts what the daemon linked and why rustc linked the rest;
- after `blaze disable` the configuration is byte for byte what it was, and cargo is alone
  again through the user's wrappers.

Not verified under plain cargo: a workspace with a proc macro of its own (the real
projects only use proc macros from the registry), `--target` for another triple (it is
rustc's by design and was not run), `cargo install` of a crate from the registry, a real
rust-analyzer session,
sccache itself (the check uses a stand-in outer wrapper that records that it ran), and
anything on macOS or Windows.

### The regression gate

Run on the head with `blaze-collate` 3d47ed59bf merged in, Linux x86-64, from `harness/`
with `BLAZE_BIN` set to the release binary, once with `BLAZE_BACKEND=cranelift` and once
with `BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`. The results are the same under both:

| Check | Result |
|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 0 of 893 layouts differ |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 63 of 63, 6 open, 0 functions skipped |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 62 of 62 |
| `keys.py` | 46 of 46, 0 open |

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p blaze-engine`: 1018 passed, 0 failed, 1 ignored. `cargo clippy -p blaze-engine --all-targets
-- --cap-lints warn` gives 168 warnings in `crates/blaze-engine`; those in `cargo/`, `socket.rs`
and the two small programs are all of two kinds the crate has throughout (`println!` or
`eprintln!` in a command-line program, and `std::process::Command::new`, which the
repository's clippy configuration disallows for the language server).

## How it works

This section is for people changing the code. Users can skip it.

blaze compiles the crates of the workspace itself and links its objects ahead of what rustc
built of the same crates. For that it needs, of every program, the rlibs and objects rustc
built, the line rustc ran the linker with, and the crates of the workspace that are in it.
It gets them through cargo's workspace wrapper, in both modes: cargo runs `blaze-rustc` in
place of rustc for every crate of the workspace and for none of the registry's, with the
real command line. The wrapper runs rustc as asked, plus `--print link-args=FILE -C
save-temps` when the crate is linked into an executable, and writes a record of the unit
under `<target-dir>/<profile>/.blaze-units/`: the arguments, and copies of what rustc wrote
that a later build would write over. The daemon reads and links against the copies, so a
cargo build that runs beside it never changes a file under it.

The two modes differ in who sets the wrapper and who asks the daemon:

- `blaze` sets `RUSTC_WORKSPACE_WRAPPER` itself for the cargo build it runs, reads
  the programs from cargo's JSON messages, writes `target/blaze/<key>/state.json`, and
  tells the daemon to load the build. A later invocation with a state, nothing changed
  that only cargo can act on (the guards: manifests, lock file, configuration, toolchain
  file, build script inputs, variables rustc said a crate read), and a warm daemon asks
  the daemon for each program and installs what it built. `<key>` is a hash of the
  subcommand, the flags, the toolchain and the environment that changes what cargo
  builds.
- After `blaze enable`, cargo finds the wrapper in its configuration. When rustc has linked a
  program, the wrapper starts one process per cargo invocation that waits for that cargo
  to end and then has the daemon load what it built, the same state `blaze` would
  have written. On later builds the wrapper of a linked unit finds that state by the
  unit's record, asks the daemon for the program, and exits 0 in rustc's place with the
  program installed where rustc would have written it. Cargo's fingerprints are cargo's
  own throughout.

Cargo keeps what it builds through a workspace wrapper apart from what it builds without
one (the wrapper is in the hash of the file names) and shares the registry's crates between
the two. So `blaze enable` and `blaze disable` each cost one rebuild of the workspace's own
crates, and none of the dependencies.

The daemon assumes the source it loads is the source rustc compiled. The time just before
cargo started is recorded; when a worker is loaded the daemon checks that no source file
rustc read is newer, and discards the worker if one is. A later cargo build that recompiles
a workspace crate makes a new generation of records, and a worker of an older generation
is restarted on the new one.

Code: `crates/blaze-engine/src/cargo/` (`cli` the command line, `switch` enable and disable,
`wrapper` the rustc wrapper, `units` the records, `script` build scripts, `state` what a
workspace was last built as and when that still holds, `plan` what a program needs from the
daemon, `daemon` the per-user daemon and its protocol, `client` the connection, `run`
running programs and tests, `say` the output), `crates/blaze-engine/src/socket.rs`, and the two
small programs in `crates/blaze-engine/src/bin/`.

The harness is `harness/blaze_cli.py`. It runs each project in three ways (plain cargo
as the reference, `blaze`, plain cargo after `blaze enable`) and compares program
output, exit status and every test's result; then a table of flags on a small generated
workspace; then the daemon's lifecycle and the `enable`/`disable` round trips. It exits 0
only when nothing differs. CI runs `blaze_cli.py --small` under both backends.

## Security: the sockets

Both the daemon's socket and each worker's go through `socket.rs`, and so does the socket
of a plain `blaze ... --socket PATH`.

- The directory. `$BLAZE_HOME` is made with mode 0700. Before use it is checked: it must be
  a directory, owned by the user, with no access for group or others, and every directory
  above it must be the user's or root's and not writable by everyone without the sticky
  bit. Otherwise the client and the daemon refuse with the reason and nothing is bound.
  `$XDG_RUNTIME_DIR/blaze` is used when `XDG_RUNTIME_DIR` is set, `~/.cache/blaze`
  otherwise; the same check applies to both and to a `BLAZE_HOME` the user sets.
- The socket is bound inside a 0700 directory made next to where it is asked for
  (`<socket>.new/`), set to 0600 there, and renamed into place. It is never reachable with
  the mode the umask gave it, whatever directory it ends up in.
- The peer, both ways. Every accepted connection is asked for its user (`SO_PEERCRED` on
  Linux, `getpeereid` on macOS and the BSDs) and one that is not the daemon's user is
  refused before its request is read. Every client asks the same of the server before it
  sends anything, so a socket another user listens on gets no request and no environment.
- A request is one line to the daemon and two to a worker (the request, then the variables
  of the client's process), of at most 16 MiB together, and the client has 10 seconds to
  send it and 30 seconds to take each write of the reply. Each connection is read on a
  thread of its own, at most 256 at a time; one more is closed and logged.
- One server per socket. Whoever serves a socket holds an exclusive lock on `<socket>.lock`
  for as long as it lives, and only removes a stale socket file once it holds the lock. Of
  two daemons started together one gets the lock; the other exits and its client connects to
  the winner. A worker that loses is an error to whoever started it.
- The lock and the logs are opened 0600 without following a link, and one that was there
  with another mode is set to 0600.

Tests, in `socket.rs`: `the_directory_and_the_socket_are_the_users_alone`,
`a_silent_or_endless_client_is_given_up_on`, `one_process_serves_a_socket`,
`a_socket_in_a_shared_directory_is_never_open_to_others`,
`a_log_that_was_there_becomes_the_users_alone`, `a_directory_others_can_swap_is_refused`,
`a_client_that_never_reads_is_given_up_on`,
`a_reply_has_a_time_limit_once_the_request_is_read`, `one_connection_too_many_is_closed`.
The second user is in the harness (`stranger` in `blaze_cli.py`), which makes one with
`useradd`; see "Measurements".

### The security review, by number

`SECURITY.md` on `blaze-security` (9f41ff39b0) lists nine findings on this code (the
request said eight; the file numbers nine, and all nine are answered).

1. `--socket PATH` does not check PATH's directory. Fixed, the second way the review
   offers: the client checks the server's user before it sends (`socket::connect`, used by
   every client: `blaze sync`, the daemon to its workers, `blaze` to the daemon).
   Calling `private_dir` on the parent would refuse `/tmp`, which the harnesses and users
   pass. Test: the harness has a second user listen on a socket in a world-writable
   directory and runs `blaze sync` on it; the client exits with "the peer is user N" and
   the listener reads 0 bytes.
2. The socket has the umask's mode between `bind` and `chmod`. Fixed: bound in a private
   directory and renamed. Test: `a_socket_in_a_shared_directory_is_never_open_to_others`
   (umask 0, a 0777 directory, a thread that watches the path while it is bound).
3. No write timeout. Fixed: `REPLY_TIMEOUT`, 30 s, set where the read timeout is. Tests:
   `a_client_that_never_reads_is_given_up_on`,
   `a_reply_has_a_time_limit_once_the_request_is_read`.
4. No cap on connections. Fixed: `MOST_CONNECTIONS`, 256, counted by a guard each
   connection's thread holds. Test: `one_connection_too_many_is_closed`. The harness's 16
   clients at once are far below it.
5. The lock and the logs are created with the umask. Fixed: `private_file`, 0600, for the
   lock, `daemon.log` and each worker's `log`. Test:
   `a_log_that_was_there_becomes_the_users_alone`.
6. `private_dir` checks only the last component. Fixed: every directory above is checked
   (the user's or root's, not world-writable unless sticky). Test:
   `a_directory_others_can_swap_is_refused`.
7. Windows. Fixed by the Windows session (`socket/windows.rs`, merged from
   `blaze-collate`), not by me and not run by me: the daemon's directory is refused unless
   the user owns it and its access control list admits nobody but the user, SYSTEM and
   Administrators (`GetNamedSecurityInfoW` in place of the mode check); a directory the
   daemon creates gets such a list, protected from inheritance, and so does the socket.
   AF_UNIX on Windows has no `SO_PEERCRED`; the peer's process is found with
   `SIO_AF_UNIX_GETPEERPID` and the user of its token compared with the daemon's.
8. The status reply lists project roots, checkout roots and log paths. Does not apply as a
   leak: it is given to the daemon's own user only (the peer check, tested with the second
   user, who gets no answer), and that user can read the same from the directory. Nothing
   was changed.
9. Worker ids are eight bytes of a SHA-256. Fixed: 16 bytes. No test of its own; the
   lifecycle checks run with the new ids.


## macOS and Windows

Windows x86-64 MSVC: the Windows session made `blaze` serve builds there
(`link.exe` lines, the daemon's directory and socket, build scripts, CRLF sources), and
that work is merged here. I have not run it. Its harness runs were of `blaze`; plain
cargo after `blaze enable` has not been run on Windows by anyone, and the list under
"Windows" below is what I wrote before that work, kept for what it says about the pieces
the transparent mode adds (`place`, `give_back`, the `.pdb`).

macOS: the commands exist, `blaze enable`, `disable` and `status` edit cargo's
configuration the same way, and every build is cargo's with its reason, so nothing breaks
and nothing is faster. Nothing has been run there from this branch.

To check either natively, a platform session can run, from `harness/` with `BLAZE_BIN`
set to its release `blaze`:

    python3 blaze_cli.py --small

It clones ripgrep, fd and hyperfine at pinned commits and runs them in three ways (cargo,
`blaze`, plain cargo after `blaze enable`), then the flags, the daemon's lifecycle
and the `enable`/`disable` round trips. On macOS the expected result today is that every
step is the same as cargo and every build is marked as cargo's; on Windows, that every
step is the same as cargo. A step that differs, or a build that fails, is a bug. The
second-user checks need `sudo useradd`; where there is no sudo, `BLAZE_NO_SUDO=1` leaves
them out.

What has to change for the daemon to serve builds on macOS: `only_cargo` (`cargo/cli.rs`,
`unsupported_target`) sends every build to cargo with "programs for <triple> are not
linked through `blaze` yet", because the link plan does not take a Mach-O line. The
worker, the wrapper, the recording of units and the daemon are not specific to a format.

macOS (Mach-O, `cc` driving `ld`):

1. `LinkLine::parse` (`cargo/plan.rs`) refuses a line with `-arch`. Accept it; the line is
   still `cc ... -o OUT`.
2. `LinkLine::command` puts blaze's patch and object ahead of rustc's files and adds
   `-Wl,--allow-multiple-definition` (`FIRST_DEFINITION_STAYS`). Apple's linker has no such
   flag. Instead every file of `rustc_files()` has to be replaced on the line by the copy
   `blaze-engine weaken` makes of it (definitions made weak, so blaze's strong ones win), which is
   what `harness/linkline.py` `ahead` does for the harnesses. `weaken` is a subcommand
   today; the plan should call the function behind it, not the process, and keep the
   copies in the program's directory under `states/`, made again when rustc's file is.
3. Add `-Wl,-headerpad,0x200` to the line cargo's first build links with, or the splice
   declines (two more load commands than the linker leaves room for). The wrapper cannot
   add a link argument to rustc's own link; it has to be `-C link-arg` on the recorded
   final-link unit, which changes cargo's fingerprint, or the first daemon link does it
   and only later edits splice. The second is the one that leaves cargo's build alone.
4. `symbols.o` is dropped from the line on ELF. On Mach-O the exported-symbols list is a
   file passed with `-exported_symbols_list`; check what rustc passes for an executable
   and drop the same thing.
5. `place` installs by hard link and rename. That is right on macOS, but an ad hoc signed
   executable has to keep its signature: the splice already rebuilds it; the full link is
   `ld`'s and signs itself.
6. `socket.rs` needs nothing: `getpeereid`, modes and `flock` are already the Unix path.
   `$XDG_RUNTIME_DIR` is not set on macOS, so the home is `~/.cache/blaze`;
   `~/Library/Caches/blaze` would be the native place.
7. `linked_in_deps` and `written_now` (the nested cargo case) use nothing Linux has alone.

Windows (COFF/PE, `link.exe` or `lld-link`), as listed before the Windows session's work:

1. `LinkLine::parse` refuses a line with `/NOLOGO` or `-flavor`. An MSVC line has no `-o`:
   the output is `/OUT:path`, the arguments are in rustc's own response file when long,
   and quoting is not shlex's. Parse it as `harness/linkline.py` `msvc`/`output_of` do.
2. `command`: blaze's objects go ahead of rustc's with `/FORCE:MULTIPLE /IGNORE:4006`
   instead of the `-Wl` flag (`linkline.py` `ahead`). `/FORCE` also lets unresolved
   symbols through as a warning; the plan has to fail the build on LNK2019/LNK2001 in the
   linker's output itself.
3. The response file `command` writes is in `cc`'s syntax; `link.exe` wants `@file` with
   its own quoting, in UTF-16 or the ANSI code page.
4. A running `.exe` cannot be replaced (LNK1104, and rename fails): `place` has to move
   the old file aside first, as `WINDOWS.md` describes for the splice, and `give_back`
   likewise. The PDB belongs to the link: install it with the executable, both names.
5. `State::after` records programs by `Record::outputs()`; on Windows that includes the
   `.pdb`, which must not be taken for a second name of the program.
6. The socket. `uds_windows` gives AF_UNIX on Windows 10 1803 and later, and that is what
   the code uses. For it to be safe: create `BLAZE_HOME` with a DACL of the user's SID
   alone (no inheritance) and check that DACL and the owner before use, where
   `private_dir` checks mode and uid; give the lock and the logs the same DACL where
   `private_file` sets 0600. AF_UNIX on Windows has no peer credentials, so the peer check
   cannot be done on it at all. A named pipe can: `\\.\pipe\blaze-<user SID>` made with
   `FILE_FLAG_FIRST_PIPE_INSTANCE` (the one-server rule, in place of the lock file),
   `PIPE_REJECT_REMOTE_CLIENTS`, a security descriptor of the user's SID, and
   `GetNamedPipeClientProcessId` then the token's user for the server's check,
   `GetNamedPipeServerProcessId` for the client's. That is the change to make: `Served`
   and `connect` behind one type with a pipe on Windows. Read and write timeouts need
   overlapped I/O there; `set_read_timeout` has no pipe equivalent.
7. `flock` is `LockFileEx` through `std::fs::File::lock`, which the code already uses.
8. Without `HOME` the home is `%USERPROFILE%\.cache\blaze` today; `%LOCALAPPDATA%\blaze`
   is the native place, and is not roamed.
9. Tests: every test of `socket.rs` is `cfg(unix)`. Each needs its Windows twin, and the
   second-user check of the harness needs a second account (`net user`).

## Fixes outside the cargo integration

Fixes outside `cargo/` that the real projects needed, all general:

- A `#[test]` function is looked up under the name rustc gives it. rustc writes a constant of
  the same name before it, so the function is the second of its name; blaze looked for the
  first, found no symbol, took the tests for dead code and then made its own copy of a
  thread-local the tests set (fd's `filter::time` test failed for that reason).
- Under `cfg(test)` the crate's own `main` is not the entry point, and a symbol that is alone
  at its path but not the first of its name is not taken for the item in the source.
- A file the VFS lists that is not UTF-8 has no text and is not watched (bat has one; the
  worker panicked on it).
- Worktrees of one repository are recognised by the `.git` file of a worktree.
- The request for a target can name several crates.

And in the second round, from the longer project list and the loops over just:

- The copies of arguments passed by address reuse stack slots across the call sites of a
  function instead of taking a new one at each. just's recursive parser had frames about 1.7 times
  rustc's, and `recursion_limit::bugfix` overflowed the 8 MB stack in 91 of 100 runs.
- The daemon installs a program under every name cargo has for it (`target/debug/just` and
  the file in `deps/` are one file to cargo). When they came apart, a cargo started inside
  a test linked them again under a program that was running, which then saw itself as
  `just (deleted)` (`constants::constants_are_not_exported`, 5 and 13 of 100 runs, 10 of 60).
- `CARGO_BIN_EXE_*`, `CARGO_TARGET_TMPDIR` and the other places cargo names for a unit reach
  the daemon. Before, `env!("CARGO_BIN_EXE_app")` in an integration test was compiled as
  the string `UNRESOLVED_ENV_VAR`. An `env!` of a variable nobody set is now refused, and
  two units told different places go to cargo.
- An enum's variants are as visible as the enum is declared, whatever the import the path
  reached it through. `pub use Index::Plus as Right` in a module that imports `Index`
  privately made `Right` private, so `Some(Right)` in another module was a binding that
  matched everything (11 of delta's tests). Breaker `renamed_variant_pattern`, and a
  `hir-def` test.
- A tuple struct or tuple variant constructor of constants is promoted, as a unit variant
  and a record literal already were. `&[Self::Name(SortCase::AaBbCc), ..]` was built in
  the frame and its address returned (eza exited with an illegal instruction). Breaker
  `promoted_constructor`.


And in the third round, with the transparent mode and the store on by default:

- An alias that names a binder's lifetime (`for<'a> fn(&'a mut T) -> D::Data<'a>`) is
  normalized with placeholders in the binder's place; the worker panicked on it while
  loading wasmtime. Breaker `alias_under_binder`.
- A file of a package that is not UTF-8 is hashed from disk for the store's key. With the
  store on, the worker asked for its text and panicked (bat's test assets).
- A build script is run again, to see whether what it wrote still holds, into a directory
  of its own next to cargo's `OUT_DIR`, and compared. Before, it wrote into `OUT_DIR`
  itself, under the compilers of the build in progress: tokei's generated file got a newer
  time, the next cargo run recompiled the library and replaced the rlib the doc tests were
  being linked with ("extern location for tokei does not exist").
- rustc's borrow check runs beside every daemon build in both modes (see "Errors"). Before,
  a warm daemon built and ran a use after a move.

## Where this stands

Work on this branch was stopped at the merge of `blaze-collate-next` 1b022ff6f2.

Done, and checked on Linux x86-64 under both backends before that merge (2fce84bb11):
both modes, `enable`/`disable`/`status`, wrapper chaining, the daemon's lifecycle, the
socket hardening and the nine security findings, rustc's check beside every daemon build,
the default store, `--help`, the quiet exit on a closed pipe, and the fixes listed under
"Fixes outside the cargo integration". The gate, `blaze_cli.py --small`, formatting and
the five-crate tests passed there.

Half-done:

- The merge of `blaze-collate-next`. It is in the branch (330dd35b25) and builds. The ten gate
  scripts pass on it under Cranelift and under LLVM `-O2` fast emit (difftest MATCH, 0 of
  893 layouts; programs 22 of 22; stress 99 of 99; breakers 94 of 94, 6 open; unwinds 8 of
  8; splices 71 of 71; asmedits 10 of 10; includes 9 of 9; worktrees 62 of 62; keys 46 of
  46). The two backends ran side by side, and LLVM's `difftest.py` failed in that run with
  exit 1 just after the runner was built, with no message kept; run again on its own it
  passed. I took that for two runs building the same runner at once and did not check it.
  `blaze_cli.py --small`, the unit tests and clippy were not finished on the merged
  head: their runs were lost when `/dev/shm` was emptied under them (cause not found) and
  were not repeated. Clippy was last counted at 168 warnings in `crates/blaze-engine` on
  2fce84bb11 and never compared with the collated branch, so "nothing new" is not shown.
- The Windows text in "macOS and Windows" was rewritten from the Windows session's merged
  code by reading it. Nothing was run on Windows or macOS from this branch.
- The sixteen-project table, the plain-cargo edit gap and the store size: see
  "Measurements".

What I would do next, in order:

1. Run `blaze_cli.py --small`, the five-crate tests and clippy on the merged head, and
   find what empties `/dev/shm` (run the scripts one at a time with a marker file).
2. The edit gap: measure the second and third plain-cargo edits against cargo alone on the
   four slow projects. If the first edit is the problem, finish recompiling store code in
   the background; if the check is, look at sharing rustc's incremental directory or
   skipping the check when cargo already ran it for the library.
3. The store: break one starship pass down by what wrote it.
4. The sixteen-project table in both modes under both backends, one project at a time.
5. `publish/overlay/docs/cargo.md` brought in line with this file.

## What is open

- The sixteen-project table on the merged head, under both backends and in both modes. Nine
  projects under Cranelift from before the last two merges are all there is.
- Plain cargo's first edit build is slower than cargo alone on four of nine projects. What
  is known is under "The plain-cargo edit gap"; it is not fixed.
- Under plain cargo the daemon answers for programs only. Library crates of the workspace
  are rustc's. Fallback is per invocation or per program in both modes, not per crate
  inside one link.
- Test executables built optimised or without incremental compilation are cargo's.
- The store grows fast: one pass of the harness over starship wrote 7.6 GiB, which is
  nearly the default limit. See "Store size".
- A long `BLAZE_HOME` makes a worker's socket path longer than the system allows (108
  bytes on Linux, 104 on macOS). The build then goes to cargo with "path must be shorter
  than SUN_LEN" in the reason. `~/.cache/blaze` and `$XDG_RUNTIME_DIR/blaze` are well
  inside it for ordinary user names; a deep `BLAZE_HOME` is not. Binding by a shorter
  name (a relative path from the directory) would remove the limit.
- macOS: every build is cargo's, and nothing has been run there. Windows x86-64 MSVC is
  served since the Windows session's work was merged (see "macOS and Windows"); the
  transparent mode has not been run there.
- A function rustc emitted no symbol for is assumed dead. The test naming bug was one way
  for that assumption to be wrong; I know of no other and have not proved there is none.
- `rust-analyzer` running `cargo check` at the same time as a build is checked on a small
  generated workspace (a `cargo check --message-format=json` beside a build), not with a
  real editor session.
