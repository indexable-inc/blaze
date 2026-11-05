---
status: current
summary: Getting started: using blaze on a project, in short.
---
# Using blaze

Not installed yet: [install.md](install.md). Every command and setting in detail:
[cargo.md](cargo.md).

## Use it on your project

```sh
cd my-project
blaze build
```

The first build starts a background process, the daemon (`blaze serve`), which loads your
project and compiles every crate of it from source. No `cargo build` and no rustc run, and what
it prints is what cargo prints:

```
   Compiling my-project v0.1.0 (/home/me/my-project)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 41.20s
```

From then on, an edit builds only what it changed, on what the daemon holds. The program is
where cargo would have put it, under `target/`.

- `blaze build`, `blaze run` and `blaze test` take cargo's flags and do what
  cargo's commands do: `run` passes what follows `--` to your program, and `test` runs and reports
  the tests as `cargo test` does.
- `blaze check` reports the errors a build would fail with.
- `blaze bench` builds and runs the benchmarks as `cargo bench` does.
- `blaze status` shows the daemon's process, how many requests it has served, the checkouts
  it watches and the memory it holds.
- `blaze stop` makes the daemon's state durable and stops it. It does not stop by itself;
  `blaze daemon install` has systemd or launchd start it on the first connection instead.
  A command that finds a daemon of another build of blaze says so in one line, stops it and
  starts its own; a socket-activated one of another build is refused, naming both.

`b`, `c`, `r` and `t` are short for `build`, `check`, `run` and `test`, and `blaze help
<command>` lists a command's flags. Those, `serve`, `stop`, `status`, `daemon`, blaze's looks at
the last build (`codegen-inputs`, `trace`, `tables`) and `pin` are all of blaze's commands:
any other (`fmt`, `clippy`, `doc`, `tree`, ...) fails with `no such command`, and none runs
cargo. A build blaze can't do fails with the reason and is not handed to cargo: a crate type
of `staticlib` or `dylib`, an artifact dependency (`-Z bindeps`), and code blaze can't
compile. Each is a bug ([#84](https://github.com/indexable-inc/blaze/issues/84),
[the plan](no-rustc.md)). rustc's `-Z` flags in `RUSTFLAGS` or `rustflags`, such as
`-Zthreads=8`, are read by one table (`crates/blaze-cargo/src/unstable.rs`): each is ignored
(it changes only how rustc works), modelled, or refused, naming the flag; one the table lacks is
refused ([support.md](support.md#-z-flags-for-rustc)).

## Rebuild on every save

blaze has no watch mode of its own; any file watcher can run it. With
[watchexec](https://github.com/watchexec/watchexec):

```sh
watchexec -e rs -- blaze build
```

## Plain `cargo build`

Plain cargo does not build through blaze: `blaze` is the only way in. blaze
runs no rustc wrapper, so a build where `build.rustc-workspace-wrapper` or
`RUSTC_WORKSPACE_WRAPPER` is set (as an earlier blaze's `blaze enable` left in
`~/.cargo/config.toml`) fails saying so; take the key out
([cargo.md](cargo.md#plain-cargo-switched-on-globally)).

## Change a running program

Not in v2 yet. The `blaze-live` crate below is the program's side, and v2's daemon does not
send it patches yet: it links each program with no room for live edits (`live: None` in
`crates/blaze-cargo/src/serve/unit/codegen.rs`), so `poll` never has an edit to apply, and an
edit takes a restart. What follows is how it worked before v2, and how it is meant to work
again ([live.md](live.md)).

A program that links the `blaze-live` crate takes each edit while it runs, and keeps its state:
useful for games, GUIs and anything slow to get back to where you were.

```toml
[dependencies]
blaze-live = "0.1"
```

```rust
let mut live = blaze_live::Live::start().ok();
loop {
    if let Some(outcome) = live.as_mut().and_then(|it| it.poll()) {
        eprintln!("blaze: {outcome}");
    }
    // ...one frame of your program
}
```

Start it with `blaze run`. In another terminal, build after each edit with
`blaze build` (or the `watchexec` line above). The next `poll` switches to the functions that
changed in one atomic store of the program's jump table, with no thread stopped, and prints
`blaze: generation ... applied: 1 functions`. An edit that changes a type the program is
holding, such as a struct's fields, can't be applied to data that already exists, so `poll` asks
for a restart instead.

Live edits work on macOS on Apple silicon. Linux x86-64 and Windows x86-64 support is on `main`
and comes with the next release. How it works, and how fast from a keystroke to the running program:
[live.md](live.md).

## What works and what doesn't yet

- **Platforms.** macOS on Apple silicon, Linux x86-64 and Linux ARM64 are measured in
  [benchmarks.md](benchmarks.md). Windows x86-64 (MSVC) builds and takes live edits through
  blaze too, run by hand and not in CI ([platforms.md](platforms.md#windows-x86-64-msvc)).
  Intel Macs are not supported.
- **One Rust version, the latest nightly.** blaze builds with the dated nightly it pins
  (`harness/toolchain`, `nightly-2026-10-27` today, bumped daily) whatever toolchain the
  project pins. Stable code and `#![feature(...)]`
  code both build; code that needs what that nightly lacks, such as a feature it removed, fails
  with rustc's error (see [install.md](install.md#what-you-need) and
  [support.md](support.md#toolchain)).
- **Not everything builds yet.** What is listed above and code blaze can't compile yet fail the
  build with the reason; nothing goes to cargo. In the benchmark tables, `(c)` marks an edit
  that went to cargo when it was measured, before v2.
- **Errors.** An edit with a type error fails to build, as it would under cargo. The borrow checker
  is new: 0.1.0 has none, so an edit with a borrow error builds and runs; the one on `main` finds
  about two thirds of rustc's borrow errors. Keep `cargo check` or your CI as the final word.
- **Memory and disk.** The daemon holds several times the memory cargo uses while building, and a
  project's build keeps more on disk than cargo's does
  ([numbers](benchmarks.md#how-much-memory-and-disk-it-takes)).
- **Correctness.** Every change to blaze is checked by comparing what it builds with what rustc
  builds, on rustc's own tests and on real projects, but it is young: if a program behaves
  differently under blaze, that's a bug, and an
  [issue](https://github.com/indexable-inc/blaze/issues) is welcome.

What is supported in detail: [support.md](support.md).
