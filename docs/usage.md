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

The first build starts a background process, the daemon, which loads your project and compiles
every crate of it from source. No `cargo build` and no rustc run:

```
    Finished `dev` profile [blaze] target(s) in 41.20s
```

From then on, an edit builds like this:

```
$ blaze build
    Finished `dev` profile [blaze] target(s) in 0.02s
```

`[blaze]` means blaze built it. The program is where cargo would have put it, under `target/`.

- `blaze build`, `blaze run` and `blaze test` take cargo's flags and do what
  cargo's commands do: `run` passes what follows `--` to your program, and `test` runs and reports
  the tests as `cargo test` does.
- `blaze check` reports the errors a build would fail with.
- `blaze status` shows what the daemon has loaded, how many builds it did and how many it
  left to cargo, and why.
- `blaze stop` stops the daemon. It also stops by itself after 30 minutes without a build.

Every other subcommand is blaze's own too (`fmt`, `clippy`, `doc`, `tree` and the rest); one blaze
has not written yet fails and says so. None of them runs cargo. A build blaze can't do yet fails
with the reason, `blaze does not build ... yet`, and is not handed to cargo: so far that is a few
flags such as `--message-format`, cargo's own `-Z` and several `--target`s at once, and code blaze
can't compile. Each is a bug ([#84](https://github.com/indexable-inc/blaze/issues/84),
[the plan](no-rustc.md)).
rustc's `-Z` flags in `RUSTFLAGS` or `rustflags`, such as `-Zthreads=8`, are another matter: rustc
still gets them, and what blaze does with each is in [support.md](support.md#-z-flags-for-rustc).

## Rebuild on every save

blaze has no watch mode of its own; any file watcher can run it. With
[watchexec](https://github.com/watchexec/watchexec):

```sh
watchexec -e rs -- blaze build
```

## Plain `cargo build`

Plain cargo does not build through blaze: `blaze` is the only way in. Where an earlier blaze's
`blaze enable` was run, the next `blaze` takes its setting out of `~/.cargo/config.toml` and
says so in one line ([cargo.md](cargo.md#plain-cargo-switched-on-globally)).

## Change a running program

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
`blaze build` (or the `watchexec` line above). The next `poll` swaps in the functions that
changed, usually in under a millisecond, and prints
`blaze: edit ... applied: 1 functions in 745.4µs`. An edit that changes a type the program is
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
  (`nightly-2026-10-25`) whatever toolchain the project pins. Stable code and `#![feature(...)]`
  code both build; code that needs what that nightly lacks, such as a feature it removed, fails
  with rustc's error (see [install.md](install.md#what-you-need) and
  [support.md](support.md#toolchain)).
- **Not everything builds yet.** Under `blaze`, the flags listed above and code blaze can't
  compile yet fail the build with the reason; nothing goes to cargo. In the benchmark tables, `(c)` marks an edit that went to cargo when it was
  measured.
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
