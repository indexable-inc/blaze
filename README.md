<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/ix-mark-on-dark.svg">
    <img alt="ix" src="docs/brand/ix-mark-on-light.svg" height="40" align="absmiddle">
  </picture>
  ra-jit
</h1>

ra-jit makes rebuilding a Rust project after an edit take milliseconds. It runs beside cargo,
keeps your project compiled in memory, and after an edit compiles only the functions you changed
and writes them into the program cargo already built. A program that opts in can take the new
code while it keeps running.

On real projects such as ripgrep, nushell, helix and bun, an edit builds <!-- benchheadline:start -->10× faster on macOS arm64, 24× faster on Linux aarch64 and 12× faster on Linux x86-64<!-- benchheadline:end --> than with an incremental `cargo build` (the average over the projects; [How fast](#how-fast) has every number).

To try it on a project of yours, on macOS on Apple silicon or on Linux:

```sh
rustup toolchain install 1.99.0 --component rust-src
cargo +1.99.0 install --locked ra-jit
cd my-project
rustup override set 1.99.0
cargo ra-jit run
```

Wait a few seconds, change a function, and run `cargo ra-jit run` again: the `Finished` line now
says `[ra-jit]`, which means ra-jit built it instead of cargo.

## Install

What you need:

- **Rust 1.99.0 with its source code** (the `rust-src` component). ra-jit works with this one
  version only: it links its code with code rustc 1.99.0 compiled, so the two have to agree on how
  types are laid out and functions are called. Your project has to build with 1.99.0 too; on any
  other version every build is plain cargo's.
- **macOS on Apple silicon, or Linux on x86-64 or ARM64.** On Windows ra-jit installs and runs,
  but hands every build to cargo. Intel Macs are not supported.
- **Memory to spare.** ra-jit's background process holds a few GB for a small project and tens of
  GB for the largest ([numbers](#how-fast)).

### From crates.io

```sh
rustup toolchain install 1.99.0 --component rust-src
cargo +1.99.0 install --locked ra-jit
```

This compiles ra-jit from source, which takes a few minutes, and puts four programs in
`~/.cargo/bin`: `cargo-ra-jit` (what `cargo ra-jit` runs), `ra-jit`, `ra-jit-rustc` and
`ra-jit-rustc-first`. They find each other by being in the same directory, so keep them together.

### With Nix

```sh
nix profile install github:indexable-inc/ra-jit#ra-jit
```

Or try it without installing anything: in a project, `nix run github:indexable-inc/ra-jit#ra-jit -- build`
is `cargo ra-jit build`. In a flake, take the package from the input:

```nix
{
  inputs.ra-jit.url = "github:indexable-inc/ra-jit";
  # ... then use ra-jit.packages.${system}.ra-jit in a dev shell or a profile.
}
```

Nix compiles ra-jit from source; there is no binary cache yet. You still need Rust 1.99.0 with
`rust-src` for your project, as above.

### Prebuilt binaries

Not yet: no release with binaries has been published. The first one will be on the
[releases page](https://github.com/indexable-inc/ra-jit/releases), and `cargo binstall ra-jit`
will install it.

## Use it on your project

```sh
cd my-project
rustup override set 1.99.0    # or a rust-toolchain.toml with channel = "1.99.0"
cargo ra-jit build
```

The first build is cargo's, with cargo's usual output. When it is done, ra-jit starts a background
process, the daemon, which loads your project: a few seconds for a small one, up to about a minute
for a very large one. Until then builds are still cargo's, and each one says so:

```
ra-jit: the daemon is still loading the workspace (951ms so far); building with cargo
```

`cargo ra-jit status` says `ready` once it is done. From then on, an edit builds like this:

```
$ cargo ra-jit build
    Finished `dev` profile [ra-jit] target(s) in 0.02s
```

`[ra-jit]` means ra-jit built it. The program is where cargo would have put it, under `target/`.

- `cargo ra-jit build`, `cargo ra-jit run` and `cargo ra-jit test` take cargo's flags and do what
  cargo's commands do: `run` passes what follows `--` to your program, and `test` runs and reports
  the tests as `cargo test` does.
- `cargo ra-jit check` reports the errors a build would fail with.
- `cargo ra-jit status` shows what the daemon has loaded, how many builds it did and how many it
  left to cargo, and why.
- `cargo ra-jit stop` stops the daemon. It also stops by itself after 30 minutes without a build.

Any other subcommand goes to cargo unchanged. When ra-jit can't be sure its result would be the
same as cargo's, it lets cargo do that build and prints one line saying why
(`ra-jit: <reason>; building with cargo`). That happens after a change to `Cargo.toml`,
`Cargo.lock` or what a build script reads, when a test is added or removed, and for a few flags
such as `--message-format` and `-Z`. The full list is in [docs/cargo.md](docs/cargo.md).

### Rebuild on every save

ra-jit has no watch mode of its own; any file watcher can run it. With
[watchexec](https://github.com/watchexec/watchexec):

```sh
watchexec -e rs -- cargo ra-jit build
```

### Make plain `cargo build` use ra-jit

For editors, scripts and agents that run `cargo build` themselves:

```sh
ra-jit enable
```

From then on plain `cargo build`, `run`, `test` and `check` go through ra-jit, in every project of
your user. It adds one setting to `~/.cargo/config.toml`, and `ra-jit disable` puts the file back
as it was. Edits are slower this way than with `cargo ra-jit`, and on some projects slower than plain
cargo ([why](docs/cargo.md#plain-cargo-switched-on-globally)).

### Change a running program

A program that links the `ra-jit-live` crate takes each edit while it runs, and keeps its state:
useful for games, GUIs and anything slow to get back to where you were.

```toml
[dependencies]
ra-jit-live = "0.1"
```

```rust
let mut live = ra_jit_live::Live::start().ok();
loop {
    if let Some(outcome) = live.as_mut().and_then(|it| it.poll()) {
        eprintln!("ra-jit: {outcome}");
    }
    // ...one frame of your program
}
```

Start it with `cargo ra-jit run`. In another terminal, build after each edit with
`cargo ra-jit build` (or the `watchexec` line above). The next `poll` swaps in the functions that
changed, usually in under a millisecond, and prints
`ra-jit: edit ... applied: 1 functions in 745.4µs`. An edit that changes a type the program is
holding, such as a struct's fields, can't be applied to data that already exists, so `poll` asks
for a restart instead.

Live edits work on macOS on Apple silicon. Linux x86-64 support is on `main` and comes with the
next release. More in [docs/live.md](docs/live.md).

## What works and what doesn't yet

- **Platforms.** macOS on Apple silicon, Linux x86-64 and Linux ARM64 are measured below. On
  Windows every build is cargo's. Intel Macs are not supported.
- **One Rust version.** 1.99.0 only (see [Install](#install)).
- **Not every build is ra-jit's.** The changes listed above go to cargo, and so does code ra-jit
  can't compile yet. `cargo ra-jit status` counts both; in the tables below, `(c)` marks an edit
  that went to cargo.
- **Errors.** An edit with a type error fails to build, as it would under cargo. The borrow checker
  is new: 0.1.0 has none, so an edit with a borrow error builds and runs; the one on `main` finds
  about two thirds of rustc's borrow errors. Keep `cargo check` or your CI as the final word.
- **Memory and disk.** The daemon holds several times the memory cargo uses while building, and a
  project's build keeps more on disk than cargo's does ([numbers](#how-fast)).
- **Correctness.** Every change to ra-jit is checked by comparing what it builds with what rustc
  builds, on rustc's own tests and on real projects, but it is young: if a program behaves
  differently under ra-jit, that's a bug, and an
  [issue](https://github.com/indexable-inc/ra-jit/issues) is welcome.

What is supported in detail: [docs/support.md](docs/support.md). Everything about the cargo
commands: [docs/cargo.md](docs/cargo.md).

## How it works

ra-jit keeps your project parsed and type-checked in memory with
[rust-analyzer](https://github.com/rust-lang/rust-analyzer), the engine behind most Rust editor
plugins, and generates machine code itself with [Cranelift](https://cranelift.dev). After an edit
it compiles only the functions that changed, and writes them into the existing program instead of
linking it again from scratch, which is where most of an incremental build's time goes. The first
build is cargo's, and ra-jit's code is linked with what it produced. The whole design:
[docs/architecture.md](docs/architecture.md).

## How fast

How long an edit takes to build, how much memory and disk ra-jit uses, and how fast the programs
it builds run, against cargo. The tables are written by the benchmark harness
([docs/benchmarks.md](docs/benchmarks.md)) and kept current by CI.

<!-- bench:start -->
<picture><source media="(prefers-color-scheme: dark)" srcset="docs/benchmarks/charts/edits-dark.svg"><img alt="How much faster an edit builds under ra-jit than under cargo, by platform: All projects (geometric mean): macOS arm64 10× faster, Linux aarch64 24× faster, Linux x86-64 12× faster; ripgrep 15.1.0: macOS arm64 8.1× faster, Linux aarch64 23× faster, Linux x86-64 11× faster; fd 10.3.0: macOS arm64 6.0× faster, Linux aarch64 22× faster, Linux x86-64 9.4× faster; bat 0.26.0: Linux aarch64 33× faster, Linux x86-64 14× faster; hyperfine 1.19.0: Linux aarch64 14× faster, Linux x86-64 8.2× faster; just 1.43.0: Linux aarch64 22× faster, Linux x86-64 13× faster; axum 0.8.6: Linux aarch64 16× faster, Linux x86-64 8.4× faster; nushell 0.108.0: macOS arm64 10× faster, Linux aarch64 34× faster, Linux x86-64 15× faster; helix 25.07.1: Linux aarch64 48× faster, Linux x86-64 27× faster; bevy 0.20.0: Linux aarch64 26× faster, Linux x86-64 3.4× faster; bun 1.4.3: macOS arm64 21× faster, Linux aarch64 22× faster, Linux x86-64 22× faster" src="docs/benchmarks/charts/edits-light.svg" width="760"></picture>

Edit, then build, on real projects at pinned commits: incremental `cargo build` → `cargo ra-jit build`, both warm, same linker, Cranelift, the median of each run's edits. Each platform is a machine of its own: compare numbers within a platform, not across.

An edit to one function, cargo → ra-jit:

| Project | macOS arm64 | Linux aarch64 | Linux x86-64 |
|---|---:|---:|---:|
| ripgrep 15.1.0 | 993 ms → **126 ms** (7.9x) | 1.18 s → **62 ms** (19x) | 497 ms → **56 ms** (8.9x) |
| fd 10.3.0 | 630 ms → **109 ms** (5.8x) | 1.27 s → **58 ms** (22x) | 490 ms → **52 ms** (9.4x) |
| bat 0.26.0 | - | 1.62 s → **56 ms** (29x) | 490 ms → **49 ms** (10x) |
| hyperfine 1.19.0 | - | 753 ms → **38 ms** (20x) | 370 ms → **34 ms** (11x) |
| just 1.43.0 | - | 1.51 s → **43 ms** (35x) | 697 ms → **40 ms** (17x) |
| axum 0.8.6 | - | 803 ms → **51 ms** (16x) | 418 ms → **50 ms** (8.4x) |
| nushell 0.108.0 | 1.81 s → **454 ms** (4.0x) | 7.20 s → **323 ms** (22x) | 1.23 s → **262 ms** (4.7x) |
| helix 25.07.1 | - | 6.84 s → **82 ms** (83x) | 2.12 s → **79 ms** (27x) |
| bevy 0.20.0 | - | 22.81 s → **884 ms** (26x) | 3.09 s → **914 ms** (3.4x) |
| bun 1.4.3 | 7.55 s → **362 ms** (21x) | 10.63 s → **486 ms** (22x) | 9.11 s → **451 ms** (20x) |

- **macOS arm64**: MacBook Pro (Mac17,6), macOS 27.0.1, with other work running (Apple M5 Max, 18 cores, 128 GiB), ra-jit [`84760c3f51`](https://github.com/indexable-inc/ra-jit/commit/84760c3f517a76f6e8a3aa41ccd81fc3f26b87c7) on 2026-10-21, median of 3 edits, load average 48 at the start, by hand.
- **Linux aarch64**: AWS c8g.16xlarge, pinned to 16 cores with taskset (Neoverse-V2, 16 of its 64 cores, 124 GiB), ra-jit [`c93e530e47`](https://github.com/indexable-inc/ra-jit/commit/c93e530e474c4de2d67a7a9ec49fd5581894efb8) on 2026-10-23, median of 5 edits, by hand.
- **Linux x86-64**: AWS c8a.metal-24xl (dev-aws-3), pinned to 16 cores with taskset (AMD EPYC 9R45, 16 of its 96 cores, 188 GiB), ra-jit [`42cb85270f`](https://github.com/indexable-inc/ra-jit/commit/42cb85270f3822819deed0d8788b6f1180b4137a) on 2026-10-24, median of 5 edits, by hand.
- **bun** is measured on its own, by the `bench-bun` workflow: macOS arm64 on one machine (Apple M5 Max, 18 cores, 128 GiB), ra-jit [`27077156cc`](https://github.com/indexable-inc/ra-jit/commit/27077156ccbd476079d261e529fc25b3225f0ace) on 2026-10-23, median of 3 edits, by hand; Linux aarch64 on AWS c8g.24xlarge (dev-aws-eph-rajit-arm64bun), pinned to 16 cores with taskset (Neoverse-V2, 16 of its 96 cores, 185 GiB), ra-jit [`ac1ee2dd68`](https://github.com/indexable-inc/ra-jit/commit/ac1ee2dd682b7d321ba75a67924aaf95609572f0) on 2026-10-23, median of 3 edits, by hand; Linux x86-64 on AWS c8a.24xlarge (dev-aws-eph-rajit-x86edits), pinned to 16 cores with taskset (AMD EPYC 9R45, 16 of its 96 cores, 185 GiB), ra-jit [`961bb3105a`](https://github.com/indexable-inc/ra-jit/commit/961bb3105a8e90e0ac33488c803f5513f075bb87) on 2026-10-23, median of 3 edits, load average 12 at the start, by hand. It is the Rust port of [bun](https://github.com/oven-sh/bun), 1.25M lines of Rust in 103 crates on a nightly beside a JavaScript engine's C and C++, built with frame pointers as bun's own build is (`-C force-frame-pointers=yes`; [what it took](docs/research/bun.md)). Its test suites are not measured yet.

<details><summary>Every kind of edit, by platform</summary>

**macOS arm64**

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 993 ms → **126 ms** (7.9x) | 1.31 s → **159 ms** (8.2x) | 828 ms → **175 ms** (4.7x) | 67 ms → **54 ms** (1.2x) |
| fd 10.3.0 | 630 ms → **109 ms** (5.8x) | 734 ms → **102 ms** (7.2x) | 681 ms → **113 ms** (6.0x) | 144 ms → **49 ms** (2.9x) |
| nushell 0.108.0 | 1.81 s → **454 ms** (4.0x) | 14.39 s → **850 ms** (17x) | 10.28 s → **716 ms** (14x) | 407 ms → **45 ms** (9.0x) |
| bun 1.4.3 | 7.55 s → **362 ms** (21x) | 16.01 s → **457 ms** (35x) | 7.26 s → **823 ms** (8.8x) | 199 ms → **122 ms** (1.6x) |

**Linux aarch64**

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 1.18 s → **62 ms** (19x) | 1.54 s → **57 ms** (27x) | 1.34 s → **101 ms** (13x) | 25 ms → **18 ms** (1.4x) |
| fd 10.3.0 | 1.27 s → **58 ms** (22x) | 1.24 s → **35 ms** (35x) | 1.29 s → **64 ms** (20x) | 34 ms → **17 ms** (2.0x) |
| bat 0.26.0 | 1.62 s → **56 ms** (29x) | 1.92 s → **55 ms** (35x) | 1.87 s → **57 ms** (33x) | 65 ms → **17 ms** (3.8x) |
| hyperfine 1.19.0 | 753 ms → **38 ms** (20x) | 797 ms → **59 ms** (14x) | 806 ms → **93 ms** (8.7x) | 44 ms → **17 ms** (2.6x) |
| just 1.43.0 | 1.51 s → **43 ms** (35x) | 1.53 s → **71 ms** (22x) | 1.50 s → **84 ms** (18x) | 43 ms → **17 ms** (2.5x) |
| axum 0.8.6 | 803 ms → **51 ms** (16x) | 1.37 s → **43 ms** (32x) |  | 73 ms → **18 ms** (4.1x) |
| nushell 0.108.0 | 7.20 s → **323 ms** (22x) | 11.36 s → **295 ms** (39x) | 11.32 s → **371 ms** (31x) | 192 ms → **21 ms** (9.1x) |
| helix 25.07.1 | 6.84 s → **82 ms** (83x) | 8.20 s → **95 ms** (86x) | 7.59 s → **267 ms** (28x) | 92 ms → **18 ms** (5.1x) |
| bevy 0.20.0 | 22.81 s → **884 ms** (26x) | 32.13 s → **352 ms** (91x) |  | 217 ms → **24 ms** (9.0x) |
| bun 1.4.3 | 10.63 s → **486 ms** (22x) | 19.64 s → **477 ms** (41x) | 10.67 s → **629 ms** (17x) | 90 ms → **21 ms** (4.3x) |

**Linux x86-64**

| Project | Edit a function | Edit a widely used function | Change a signature | Nothing changed |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 497 ms → **56 ms** (8.9x) | 758 ms → **50 ms** (15x) | 611 ms → **88 ms** (6.9x) | 22 ms → **16 ms** (1.4x) |
| fd 10.3.0 | 490 ms → **52 ms** (9.4x) | 465 ms → **35 ms** (13x) | 505 ms → **56 ms** (9.0x) | 31 ms → **15 ms** (2.1x) |
| bat 0.26.0 | 490 ms → **49 ms** (10x) | 723 ms → **48 ms** (15x) | 689 ms → **50 ms** (14x) | 59 ms → **15 ms** (3.9x) |
| hyperfine 1.19.0 | 370 ms → **34 ms** (11x) | 404 ms → **49 ms** (8.2x) | 418 ms → **75 ms** (5.6x) | 41 ms → **15 ms** (2.7x) |
| just 1.43.0 | 697 ms → **40 ms** (17x) | 712 ms → **62 ms** (11x) | 706 ms → **74 ms** (9.5x) | 40 ms → **15 ms** (2.7x) |
| axum 0.8.6 | 418 ms → **50 ms** (8.4x) | 864 ms → **44 ms** (20x) |  | 72 ms → **16 ms** (4.5x) |
| nushell 0.108.0 | 1.23 s → **262 ms** (4.7x) | 4.74 s → **271 ms** (17x) | 4.77 s → **350 ms** (14x) | 171 ms → **18 ms** (9.5x) |
| helix 25.07.1 | 2.12 s → **79 ms** (27x) | 3.25 s → **116 ms** (28x) | 2.75 s → **275 ms** (10.0x) | 81 ms → **16 ms** (5.1x) |
| bevy 0.20.0 | 3.09 s → **914 ms** (3.4x) | 11.53 s → **726 ms** (16x) |  | 178 ms → **19 ms** (9.4x) |
| bun 1.4.3 | 9.11 s → **451 ms** (20x) | 16.51 s → **430 ms** (38x) | 9.22 s → **556 ms** (17x) | 72 ms → **18 ms** (4.0x) |

</details>

Measured by hand with `harness/bench.py`, or by the `bench-readme` workflow (a project measured on its own, by its own workflow) on each platform's benchmark machine once it is registered. The ra-jit measured is `cargo build --release`; the binaries a release ships are the `dist` profile with PGO, which start and edit faster ([how](https://github.com/indexable-inc/ra-jit/blob/main/docs/reports/RELEASE-BUILD.md)). Cold builds, the spread of every number, the LLVM tier and how fast the built programs run: [docs/benchmarks.md](docs/benchmarks.md). Raw JSON: [macOS arm64](docs/benchmarks/macos-arm64.json), [Linux aarch64](docs/benchmarks/linux-aarch64.json), [Linux x86-64](docs/benchmarks/linux-x86-64.json); bun: [macOS arm64](docs/benchmarks/bun-macos-arm64.json), [Linux aarch64](docs/benchmarks/bun-linux-aarch64.json), [Linux x86-64](docs/benchmarks/bun-linux-x86-64.json).
<!-- bench:end -->

<!-- benchmem:start -->
<picture><source media="(prefers-color-scheme: dark)" srcset="docs/benchmarks/charts/memory-dark.svg"><img alt="Peak memory while an edit to one function builds, ra-jit against cargo, by platform: All projects (geometric mean): macOS arm64 12× more, Linux aarch64 3.9× more, Linux x86-64 3.5× more; ripgrep 15.1.0: macOS arm64 12× more · 2.9 GB vs cargo's 235 MB, Linux aarch64 3.6× more · 1.7 GB vs cargo's 464 MB, Linux x86-64 3.4× more · 1.8 GB vs cargo's 529 MB; fd 10.3.0: macOS arm64 12× more · 2.6 GB vs cargo's 212 MB, Linux aarch64 2.9× more · 1.5 GB vs cargo's 520 MB, Linux x86-64 2.6× more · 1.6 GB vs cargo's 606 MB; bat 0.26.0: Linux aarch64 2.9× more · 2.0 GB vs cargo's 672 MB, Linux x86-64 2.6× more · 2.0 GB vs cargo's 751 MB; hyperfine 1.19.0: Linux aarch64 4.9× more · 1.8 GB vs cargo's 361 MB, Linux x86-64 4.0× more · 1.7 GB vs cargo's 425 MB; just 1.43.0: Linux aarch64 4.7× more · 2.1 GB vs cargo's 434 MB, Linux x86-64 3.6× more · 1.9 GB vs cargo's 538 MB; axum 0.8.6: Linux aarch64 4.8× more · 1.9 GB vs cargo's 399 MB, Linux x86-64 4.0× more · 1.9 GB vs cargo's 466 MB; nushell 0.108.0: macOS arm64 12× more · 12.5 GB vs cargo's 1.1 GB, Linux aarch64 3.2× more · 7.6 GB vs cargo's 2.4 GB, Linux x86-64 2.7× more · 7.2 GB vs cargo's 2.7 GB; helix 25.07.1: Linux aarch64 3.7× more · 6.6 GB vs cargo's 1.8 GB, Linux x86-64 2.9× more · 6.0 GB vs cargo's 2.1 GB; bevy 0.20.0: Linux aarch64 2.6× more · 17.8 GB vs cargo's 6.7 GB, Linux x86-64 3.3× more · 28.2 GB vs cargo's 8.5 GB; bun 1.4.3: macOS arm64 12× more · 26.1 GB vs cargo's 2.1 GB, Linux aarch64 7.9× more · 27.7 GB vs cargo's 3.5 GB, Linux x86-64 7.4× more · 24.5 GB vs cargo's 3.3 GB" src="docs/benchmarks/charts/memory-light.svg" width="760"></picture>

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/benchmarks/charts/disk-dark.svg"><img alt="What a project's build keeps on disk, ra-jit against cargo, by platform: All projects (geometric mean): macOS arm64 1.5× more, Linux aarch64 1.2× more, Linux x86-64 1.8× more; ripgrep 15.1.0: macOS arm64 1.4× more · 598 MB vs cargo's 416 MB, Linux aarch64 1.3× more · 518 MB vs cargo's 404 MB, Linux x86-64 2.0× more · 815 MB vs cargo's 407 MB; fd 10.3.0: macOS arm64 1.3× more · 460 MB vs cargo's 355 MB, Linux aarch64 1.2× more · 801 MB vs cargo's 678 MB, Linux x86-64 1.8× more · 1.0 GB vs cargo's 571 MB; bat 0.26.0: Linux aarch64 1.2× more · 1.2 GB vs cargo's 980 MB, Linux x86-64 2.0× more · 2.0 GB vs cargo's 973 MB; hyperfine 1.19.0: Linux aarch64 1.3× more · 432 MB vs cargo's 340 MB, Linux x86-64 3.2× more · 1.1 GB vs cargo's 342 MB; just 1.43.0: Linux aarch64 1.2× more · 784 MB vs cargo's 664 MB, Linux x86-64 2.2× more · 1.5 GB vs cargo's 667 MB; axum 0.8.6: Linux aarch64 1.1× more · 454 MB vs cargo's 405 MB, Linux x86-64 2.0× more · 825 MB vs cargo's 404 MB; nushell 0.108.0: macOS arm64 1.3× more · 6.9 GB vs cargo's 5.3 GB, Linux aarch64 1.0× · 5.6 GB vs cargo's 5.3 GB, Linux x86-64 1.4× more · 7.6 GB vs cargo's 5.3 GB; helix 25.07.1: Linux aarch64 1.3× more · 4.6 GB vs cargo's 3.7 GB, Linux x86-64 1.6× more · 6.0 GB vs cargo's 3.7 GB; bevy 0.20.0: Linux aarch64 1.1× less · 16.0 GB vs cargo's 17.5 GB, Linux x86-64 1.1× more · 19.2 GB vs cargo's 17.5 GB; bun 1.4.3: macOS arm64 1.9× more · 11.3 GB vs cargo's 6.0 GB, Linux aarch64 1.2× more · 6.7 GB vs cargo's 5.6 GB, Linux x86-64 1.2× more · 6.7 GB vs cargo's 5.7 GB" src="docs/benchmarks/charts/disk-light.svg" width="760"></picture>

Memory and disk, Cranelift, from the same runs as the build table. Peak: the most every process doing the work held at once, cargo → ra-jit. For ra-jit that is the daemon, its workers and what they run, and `cargo ra-jit build` with any cargo it ran; for plain cargo, cargo itself, rustc, build scripts and the linker. How it is measured: [docs/benchmarks.md](docs/benchmarks.md#memory-and-disk).

Peak while an edit to one function builds, cargo → ra-jit:

| Project | macOS arm64 | Linux aarch64 | Linux x86-64 |
|---|---:|---:|---:|
| ripgrep 15.1.0 | 235 MB → **2.9 GB** | 464 MB → **1.7 GB** | 529 MB → **1.8 GB** |
| fd 10.3.0 | 212 MB → **2.6 GB** | 520 MB → **1.5 GB** | 606 MB → **1.6 GB** |
| bat 0.26.0 | - | 672 MB → **2.0 GB** | 751 MB → **2.0 GB** |
| hyperfine 1.19.0 | - | 361 MB → **1.8 GB** | 425 MB → **1.7 GB** |
| just 1.43.0 | - | 434 MB → **2.1 GB** | 538 MB → **1.9 GB** |
| axum 0.8.6 | - | 399 MB → **1.9 GB** | 466 MB → **1.9 GB** |
| nushell 0.108.0 | 1.1 GB → **12.5 GB** | 2.4 GB → **7.6 GB** | 2.7 GB → **7.2 GB** |
| helix 25.07.1 | - | 1.8 GB → **6.6 GB** | 2.1 GB → **6.0 GB** |
| bevy 0.20.0 | - | 6.7 GB → **17.8 GB** | 8.5 GB → **28.2 GB** |
| bun 1.4.3 | 2.1 GB → **26.1 GB** | 3.5 GB → **27.7 GB** | 3.3 GB → **24.5 GB** |

<details><summary>The daemon between edits, cold builds and disk, by platform</summary>

**macOS arm64**

| Project | Edit a function, peak | Daemon between edits | Cold until ready, peak | On disk |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 235 MB → **2.9 GB** | 2.2 GB ready, 2.8 GB after 39 builds | 541 MB → **2.4 GB** | 416 MB → 598 MB |
| fd 10.3.0 | 212 MB → **2.6 GB** | 1.8 GB ready, 2.6 GB after 33 builds | 897 MB → **1.9 GB** | 355 MB → 460 MB |
| nushell 0.108.0 | 1.1 GB → **12.5 GB** | 11.5 GB ready, 12.5 GB after 39 builds | 2.4 GB → **16.1 GB** | 5.3 GB → 6.9 GB |
| bun 1.4.3 | 2.1 GB → **26.1 GB** | 11.8 GB ready, 27.8 GB after 33 builds | 3.1 GB → **13.7 GB** | 6.0 GB → 11.3 GB |

**Linux aarch64**

| Project | Edit a function, peak | Daemon between edits | Cold until ready, peak | On disk |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 464 MB → **1.7 GB** | 1.5 GB ready, 1.6 GB after 65 builds | 1.9 GB → **2.5 GB** | 404 MB → 518 MB |
| fd 10.3.0 | 520 MB → **1.5 GB** | 1.7 GB ready, 1.5 GB after 55 builds | 2.2 GB → **3.3 GB** | 678 MB → 801 MB |
| bat 0.26.0 | 672 MB → **2.0 GB** | 1.9 GB ready, 1.9 GB after 55 builds | 3.4 GB → **5.8 GB** | 980 MB → 1.2 GB |
| hyperfine 1.19.0 | 361 MB → **1.8 GB** | 1.9 GB ready, 1.7 GB after 55 builds | 2.2 GB → **2.5 GB** | 340 MB → 432 MB |
| just 1.43.0 | 434 MB → **2.1 GB** | 1.8 GB ready, 2.0 GB after 55 builds | 3.0 GB → **3.6 GB** | 664 MB → 784 MB |
| axum 0.8.6 | 399 MB → **1.9 GB** | 2.0 GB ready, 1.9 GB after 35 builds | 2.1 GB → **3.1 GB** | 405 MB → 454 MB |
| nushell 0.108.0 | 2.4 GB → **7.6 GB** | 5.2 GB ready, 7.5 GB after 65 builds | 4.5 GB → **7.5 GB** | 5.3 GB → 5.6 GB |
| helix 25.07.1 | 1.8 GB → **6.6 GB** | 3.7 GB ready, 6.6 GB after 55 builds | 3.5 GB → **5.8 GB** | 3.7 GB → 4.6 GB |
| bevy 0.20.0 | 6.7 GB → **17.8 GB** | 8.1 GB ready, 17.7 GB after 35 builds | 6.8 GB → **9.3 GB** | 17.5 GB → 16.0 GB |
| bun 1.4.3 | 3.5 GB → **27.7 GB** | 12.3 GB ready, 27.7 GB after 33 builds | 4.5 GB → **15.8 GB** | 5.6 GB → 6.7 GB |

**Linux x86-64**

| Project | Edit a function, peak | Daemon between edits | Cold until ready, peak | On disk |
|---|---:|---:|---:|---:|
| ripgrep 15.1.0 | 529 MB → **1.8 GB** | 1.6 GB ready, 1.7 GB after 65 builds | 2.4 GB → **4.6 GB** | 407 MB → 815 MB |
| fd 10.3.0 | 606 MB → **1.6 GB** | 1.6 GB ready, 1.5 GB after 55 builds | 2.7 GB → **5.2 GB** | 571 MB → 1.0 GB |
| bat 0.26.0 | 751 MB → **2.0 GB** | 1.9 GB ready, 2.0 GB after 55 builds | 3.8 GB → **7.9 GB** | 973 MB → 2.0 GB |
| hyperfine 1.19.0 | 425 MB → **1.7 GB** | 1.9 GB ready, 1.6 GB after 55 builds | 2.6 GB → **9.8 GB** | 342 MB → 1.1 GB |
| just 1.43.0 | 538 MB → **1.9 GB** | 1.8 GB ready, 1.9 GB after 55 builds | 3.4 GB → **8.7 GB** | 667 MB → 1.5 GB |
| axum 0.8.6 | 466 MB → **1.9 GB** | 2.0 GB ready, 1.9 GB after 35 builds | 2.5 GB → **5.6 GB** | 404 MB → 825 MB |
| nushell 0.108.0 | 2.7 GB → **7.2 GB** | 5.3 GB ready, 7.2 GB after 65 builds | 4.7 GB → **8.8 GB** | 5.3 GB → 7.6 GB |
| helix 25.07.1 | 2.1 GB → **6.0 GB** | 3.9 GB ready, 5.9 GB after 55 builds | 3.9 GB → **8.7 GB** | 3.7 GB → 6.0 GB |
| bevy 0.20.0 | 8.5 GB → **28.2 GB** | 8.4 GB ready, 15.2 GB after 35 builds | 8.5 GB → **9.6 GB** | 17.5 GB → 19.2 GB |
| bun 1.4.3 | 3.3 GB → **24.5 GB** | 10.5 GB ready, 24.5 GB after 33 builds | 4.5 GB → **14.1 GB** | 5.7 GB → 6.7 GB |

On disk: cargo's target directory → ra-jit's target directory, store and home.

</details>
<!-- benchmem:end -->

<!-- runbench:start -->
<picture><source media="(prefers-color-scheme: dark)" srcset="docs/benchmarks/charts/runtime-dark.svg"><img alt="How fast each benchmark runs built by ra-jit, against cargo build --release, by platform: All benchmarks (geometric mean): macOS arm64 1.18× slower, Linux aarch64 1.14× slower, Linux x86-64 1.10× slower; sieve: macOS arm64 1.12× slower, Linux aarch64 1.07× slower, Linux x86-64 1.00×; collatz: macOS arm64 1.01× slower, Linux aarch64 1.00×, Linux x86-64 1.00×; fnv: macOS arm64 1.00×, Linux aarch64 1.01× slower, Linux x86-64 1.00×; qsort: macOS arm64 1.05× faster, Linux aarch64 1.08× slower, Linux x86-64 1.01× slower; matmul: macOS arm64 1.21× slower, Linux aarch64 1.04× slower, Linux x86-64 1.07× slower; nbody: macOS arm64 1.31× slower, Linux aarch64 1.04× slower, Linux x86-64 1.02× faster; mandelbrot: macOS arm64 1.01× slower, Linux aarch64 1.00×, Linux x86-64 1.03× faster; words: macOS arm64 1.37× slower, Linux aarch64 1.44× slower, Linux x86-64 1.13× slower; numeric: macOS arm64 1.31× slower, Linux aarch64 1.22× slower, Linux x86-64 1.02× faster; jsonrt: macOS arm64 ≈1.38× slower, Linux aarch64 1.42× slower, Linux x86-64 1.25× slower; ripgrep: macOS arm64 ≈1.50× slower, Linux aarch64 1.30× slower, Linux x86-64 1.21× slower; fd: macOS arm64 ≈1.12× slower, Linux aarch64 1.23× slower, Linux x86-64 1.27× slower; tokei: macOS arm64 1.29× slower, Linux aarch64 1.21× slower, Linux x86-64 1.32× slower; eza: macOS arm64 ≈1.06× slower, Linux aarch64 1.05× slower, Linux x86-64 1.08× slower; bat: macOS arm64 ≈1.16× slower, Linux aarch64 1.13× slower, Linux x86-64 1.17× slower; bun: macOS arm64 1.25× slower, Linux aarch64 1.17× slower, Linux x86-64 1.22× slower" src="docs/benchmarks/charts/runtime-light.svg" width="760"></picture>

How fast the programs run. Each program is built by `cargo build --release` and by ra-jit with LLVM at `-O3`, every crate of it compiled by ra-jit, with the release profile; each benchmark is then run several times, the builds taking turns, median. A run is published only when every program did what the release build's did. Each platform is a machine of its own, and each number is ra-jit's time ÷ release's on it: 1x is as fast.

| Benchmark | macOS arm64 | Linux aarch64 | Linux x86-64 |
|---|---:|---:|---:|
| sieve | 1.12x | 1.07x | 1.00x |
| collatz | 1.01x | 1.00x | 1.00x |
| fnv | 1.00x | 1.01x | 1.00x |
| qsort | 0.95x | 1.08x | 1.01x |
| matmul | 1.21x | 1.04x | 1.07x |
| nbody | 1.31x | 1.04x | 0.98x |
| mandelbrot | 1.01x | 1.00x | 0.97x |
| words | 1.37x | 1.44x | 1.13x |
| numeric | 1.31x | 1.22x | 0.98x |
| jsonrt | ≈1.38x | 1.42x | 1.25x |
| ripgrep | ≈1.50x | 1.30x | 1.21x |
| fd | ≈1.12x | 1.23x | 1.27x |
| tokei | 1.29x | 1.21x | 1.32x |
| eza | ≈1.06x | 1.05x | 1.08x |
| bat | ≈1.16x | 1.13x | 1.17x |
| bun | 1.25x | 1.17x | 1.22x |
| **All benchmarks (geometric mean)** | **1.18x** | **1.14x** | **1.10x** |

≈: that benchmark's runs varied by more than 1.5× from fastest to slowest on that machine; read it as rough.

- **macOS arm64**: GitHub-hosted macos-15 runner (a shared VM) (Apple M1 (Virtual), 3 cores, 7 GiB), ra-jit [`9c31e64d83`](https://github.com/indexable-inc/ra-jit/commit/9c31e64d83cd4cc2d0bbbd2e56b7e48d8a9326c6) on 2026-10-23, each benchmark run 5 times, load average 12 at the start, [by CI](https://github.com/indexable-inc/ra-jit/actions/runs/39876666206).
- **Linux aarch64**: AWS c8g.8xlarge (Graviton4), pinned to 16 cores with taskset (Neoverse-V2, 16 of its 32 cores, 62 GiB), ra-jit [`e8d8a63da4`](https://github.com/indexable-inc/ra-jit/commit/e8d8a63da4) on 2026-10-23, each benchmark run 10 times, load average 11 at the start, by hand.
- **Linux x86-64**: AWS c8a.8xlarge, pinned to 16 cores with taskset (AMD EPYC 9R45, 16 of its 32 cores, 62 GiB), ra-jit [`e8d8a63da4`](https://github.com/indexable-inc/ra-jit/commit/e8d8a63da4) on 2026-10-23, each benchmark run 10 times, load average 13 at the start, by hand.
- **bun** is measured on its own, by the `bench-bun` workflow: macOS arm64 on one machine (Apple M5 Max, 18 cores, 128 GiB), ra-jit [`948d54e375`](https://github.com/indexable-inc/ra-jit/commit/948d54e375ef4f4685a932539d3a4118992a31c1) on 2026-10-23, each benchmark run 5 times, load average 29 at the start, by hand; Linux aarch64 on one machine (Neoverse-V2, 16 of its 96 cores, 185 GiB), ra-jit [`68c3558698`](https://github.com/indexable-inc/ra-jit/commit/68c3558698d1a050d728c592737766712fefae86) on 2026-10-23, each benchmark run 5 times, load average 12 at the start, by hand; Linux x86-64 on one machine (AMD EPYC 9R45, 16 of its 96 cores, 185 GiB), ra-jit [`68c3558698`](https://github.com/indexable-inc/ra-jit/commit/68c3558698d1a050d728c592737766712fefae86) on 2026-10-23, each benchmark run 5 times, load average 12 at the start, by hand.

<details><summary>Times, memory and builds, by platform</summary>

**macOS arm64**

| Benchmark | `cargo build --release` | ra-jit, LLVM | ra-jit ÷ release | release with dev checks ÷ release | Peak memory, release → ra-jit |
|---|---:|---:|---:|---:|---:|
| sieve | 45 ms | **50 ms** | 1.12x | 1.01x | 17 MB → 17 MB |
| collatz | 150 ms | **151 ms** | 1.01x | 1.49x | 1 MB → 2 MB |
| fnv | 98 ms | **98 ms** | 1.00x | 0.96x | 2 MB → 2 MB |
| qsort | 263 ms | **250 ms** | 0.95x | 0.91x | 13 MB → 13 MB |
| matmul | 32 ms | **39 ms** | 1.21x | 0.99x | 4 MB → 4 MB |
| nbody | 30 ms | **39 ms** | 1.31x | 1.01x | 1 MB → 2 MB |
| mandelbrot | 328 ms | **333 ms** | 1.01x | 1.00x | 1 MB → 2 MB |
| words | 71 ms | **98 ms** | 1.37x | 1.22x | 2 MB → 2 MB |
| numeric | 95 ms | **124 ms** | 1.31x | 1.17x | 14 MB → 14 MB |
| jsonrt | 1.23 s | **1.69 s** | ≈1.38x | 1.14x | 597 MB → 590 MB |
| ripgrep | 134 ms | **201 ms** | ≈1.50x | 1.10x | 5 MB → 7 MB |
| fd | 63 ms | **71 ms** | ≈1.12x | 0.98x | 7 MB → 8 MB |
| tokei | 643 ms | **828 ms** | 1.29x | 1.09x | 26 MB → 27 MB |
| eza | 417 ms | **440 ms** | ≈1.06x | 0.92x | 19 MB → 20 MB |
| bat | 156 ms | **181 ms** | ≈1.16x | 0.89x | 12 MB → 15 MB |
| bun | 332 ms | **415 ms** | 1.25x | - | 43 MB → 46 MB |
| **All benchmarks (geometric mean)** | | | **1.18x** | 1.05x | |

What each program takes, release → ra-jit: its build from cold (for ra-jit, cargo running the build scripts and proc macros, ra-jit's compile of every crate and the link), its size, and its build's memory (the most the build's processes held at once, and the largest one process: rustc or ra-jit, or the linker).

| Program | Build | Size | Build, peak | Build, largest process |
|---|---:|---:|---:|---:|
| kernels | 600 ms → 6.80 s | 0.48 MB → 1.64 MB | 146 MB → 1.1 GB | 146 MB → 1.1 GB |
| numeric | 3.90 s → 37.80 s | 0.75 MB → 2.23 MB | 288 MB → 1.3 GB | 288 MB → 1.3 GB |
| jsonrt | 11.60 s → 68.60 s | 0.97 MB → 2.63 MB | 570 MB → 1.8 GB | 428 MB → 1.8 GB |
| ripgrep | 33.40 s → 227.10 s | 6.66 MB → 10.92 MB | 553 MB → 2.8 GB | 403 MB → 2.8 GB |
| fd | 49.70 s → 220.30 s | 3.0 MB → 8.77 MB | 757 MB → 2.8 GB | 757 MB → 2.8 GB |
| tokei | 53.30 s → 258.70 s | 4.22 MB → 9.63 MB | 547 MB → 3.2 GB | 429 MB → 3.2 GB |
| eza | 97.50 s → 213.50 s | 2.0 MB → 6.66 MB | 545 MB → 2.9 GB | 545 MB → 2.8 GB |
| bat | 129.70 s → 679.00 s | 6.65 MB → 16.8 MB | 1.2 GB → 4.6 GB | 1.2 GB → 4.6 GB |
| bun | 177.40 s → 983.10 s | 103.09 MB → 129.61 MB | 7.1 GB → 27.3 GB | 4.7 GB → 27.3 GB |

**Linux aarch64**

| Benchmark | `cargo build --release` | ra-jit, LLVM | ra-jit ÷ release | release with dev checks ÷ release | Peak memory, release → ra-jit |
|---|---:|---:|---:|---:|---:|
| sieve | 50 ms | **54 ms** | 1.07x | 1.01x | 23 MB → 23 MB |
| collatz | 105 ms | **105 ms** | 1.00x | 1.91x | 23 MB → 23 MB |
| fnv | 71 ms | **72 ms** | 1.01x | 1.00x | 23 MB → 23 MB |
| qsort | 198 ms | **215 ms** | 1.08x | 1.06x | 23 MB → 23 MB |
| matmul | 28 ms | **29 ms** | 1.04x | 1.00x | 23 MB → 23 MB |
| nbody | 36 ms | **37 ms** | 1.04x | 1.00x | 23 MB → 23 MB |
| mandelbrot | 270 ms | **269 ms** | 1.00x | 1.00x | 23 MB → 23 MB |
| words | 61 ms | **87 ms** | 1.44x | 1.03x | 23 MB → 23 MB |
| numeric | 23 ms | **28 ms** | 1.22x | 1.10x | 41 MB → 41 MB |
| jsonrt | 865 ms | **1.23 s** | 1.42x | 1.05x | 546 MB → 546 MB |
| ripgrep | 58 ms | **75 ms** | 1.30x | 1.03x | 26 MB → 26 MB |
| fd | 18 ms | **23 ms** | 1.23x | 1.01x | 26 MB → 26 MB |
| tokei | 90 ms | **108 ms** | 1.21x | 1.00x | 50 MB → 52 MB |
| eza | 231 ms | **243 ms** | 1.05x | 0.99x | 46 MB → 45 MB |
| bat | 88 ms | **99 ms** | 1.13x | 1.04x | 30 MB → 30 MB |
| bun | 535 ms | **624 ms** | 1.17x | 1.10x | 54 MB → 58 MB |
| **All benchmarks (geometric mean)** | | | **1.14x** | 1.07x | |

What each program takes, release → ra-jit: its build from cold (for ra-jit, cargo running the build scripts and proc macros, ra-jit's compile of every crate and the link), its size, and its build's memory (the most the build's processes held at once, and the largest one process: rustc or ra-jit, or the linker).

| Program | Build | Size | Build, peak | Build, largest process |
|---|---:|---:|---:|---:|
| kernels | 500 ms → 4.50 s | 0.55 MB → 0.55 MB | 187 MB → 1.5 GB | 169 MB → 1.4 GB |
| numeric | 3.60 s → 13.10 s | 0.88 MB → 0.72 MB | 600 MB → 1.8 GB | 311 MB → 1.8 GB |
| jsonrt | 6.70 s → 21.10 s | 1.09 MB → 1.07 MB | 2.0 GB → 2.8 GB | 441 MB → 2.7 GB |
| ripgrep | 9.70 s → 57.30 s | 32.23 MB → 16.06 MB | 2.4 GB → 3.6 GB | 716 MB → 3.6 GB |
| fd | 49.10 s → 77.60 s | 3.74 MB → 4.34 MB | 2.3 GB → 3.8 GB | 790 MB → 3.8 GB |
| tokei | 14.50 s → 63.30 s | 5.43 MB → 5.55 MB | 3.4 GB → 4.5 GB | 539 MB → 4.5 GB |
| eza | 37.90 s → 45.20 s | 2.38 MB → 3.17 MB | 3.4 GB → 4.0 GB | 573 MB → 3.9 GB |
| bat | 53.70 s → 127.90 s | 6.99 MB → 8.51 MB | 3.7 GB → 6.2 GB | 1.3 GB → 6.2 GB |
| bun | 167.40 s → 160.20 s | 318.66 MB → 301.54 MB | 6.5 GB → 16.0 GB | 3.7 GB → 16.0 GB |

**Linux x86-64**

| Benchmark | `cargo build --release` | ra-jit, LLVM | ra-jit ÷ release | release with dev checks ÷ release | Peak memory, release → ra-jit |
|---|---:|---:|---:|---:|---:|
| sieve | 26 ms | **26 ms** | 1.00x | 1.00x | 22 MB → 22 MB |
| collatz | 94 ms | **94 ms** | 1.00x | 1.80x | 22 MB → 22 MB |
| fnv | 59 ms | **59 ms** | 1.00x | 1.00x | 22 MB → 22 MB |
| qsort | 203 ms | **206 ms** | 1.01x | 1.04x | 22 MB → 22 MB |
| matmul | 22 ms | **24 ms** | 1.07x | 1.01x | 22 MB → 22 MB |
| nbody | 20 ms | **19 ms** | 0.98x | 1.00x | 22 MB → 22 MB |
| mandelbrot | 180 ms | **175 ms** | 0.97x | 1.02x | 22 MB → 22 MB |
| words | 40 ms | **45 ms** | 1.13x | 1.14x | 22 MB → 22 MB |
| numeric | 21 ms | **21 ms** | 0.98x | 1.03x | 22 MB → 22 MB |
| jsonrt | 884 ms | **1.10 s** | 1.25x | 1.06x | 542 MB → 542 MB |
| ripgrep | 31 ms | **37 ms** | 1.21x | 1.06x | 23 MB → 23 MB |
| fd | 12 ms | **14 ms** | 1.27x | 1.02x | 23 MB → 23 MB |
| tokei | 63 ms | **83 ms** | 1.32x | 1.02x | 36 MB → 36 MB |
| eza | 123 ms | **134 ms** | 1.08x | 1.05x | 30 MB → 30 MB |
| bat | 54 ms | **63 ms** | 1.17x | 1.04x | 30 MB → 30 MB |
| bun | 389 ms | **476 ms** | 1.22x | 1.10x | 51 MB → 62 MB |
| **All benchmarks (geometric mean)** | | | **1.10x** | 1.07x | |

What each program takes, release → ra-jit: its build from cold (for ra-jit, cargo running the build scripts and proc macros, ra-jit's compile of every crate and the link), its size, and its build's memory (the most the build's processes held at once, and the largest one process: rustc or ra-jit, or the linker).

| Program | Build | Size | Build, peak | Build, largest process |
|---|---:|---:|---:|---:|
| kernels | 300 ms → 3.00 s | 0.5 MB → 0.52 MB | 256 MB → 1.5 GB | 195 MB → 1.4 GB |
| numeric | 2.20 s → 9.10 s | 0.81 MB → 0.8 MB | 707 MB → 1.9 GB | 333 MB → 1.8 GB |
| jsonrt | 4.20 s → 14.70 s | 1.07 MB → 1.22 MB | 2.4 GB → 2.9 GB | 460 MB → 2.8 GB |
| ripgrep | 6.00 s → 44.20 s | 31.4 MB → 17.26 MB | 2.7 GB → 3.6 GB | 619 MB → 3.5 GB |
| fd | 34.60 s → 59.30 s | 4.16 MB → 4.85 MB | 2.7 GB → 3.9 GB | 497 MB → 3.8 GB |
| tokei | 9.30 s → 47.10 s | 5.51 MB → 6.49 MB | 3.8 GB → 4.7 GB | 516 MB → 4.6 GB |
| eza | 25.40 s → 32.70 s | 2.68 MB → 3.4 MB | 3.8 GB → 3.9 GB | 509 MB → 3.8 GB |
| bat | 38.70 s → 98.20 s | 7.56 MB → 9.34 MB | 4.1 GB → 6.2 GB | 730 MB → 6.1 GB |
| bun | 135.60 s → 129.20 s | 314.01 MB → 297.55 MB | 6.2 GB → 15.9 GB | 3.7 GB → 15.8 GB |

Release with dev checks: `cargo build --release` with `-C overflow-checks=on -C debug-assertions=on`.

</details>

Measured by hand with `harness/bench.py --runtime` (harness/runbench.py), or by the `bench-readme` workflow (a project measured on its own, by its own workflow) on each platform's benchmark machine once it is registered. Raw JSON: [macOS arm64](docs/benchmarks/runtime-macos-arm64.json), [Linux aarch64](docs/benchmarks/runtime-linux-aarch64.json), [Linux x86-64](docs/benchmarks/runtime-linux-x86-64.json); bun: [macOS arm64](docs/benchmarks/runtime-bun-macos-arm64.json), [Linux aarch64](docs/benchmarks/runtime-bun-linux-aarch64.json), [Linux x86-64](docs/benchmarks/runtime-bun-linux-x86-64.json).
<!-- runbench:end -->

## Demo: a Game of Life you can edit

```sh
nix run github:indexable-inc/ra-jit
```

This opens a Game of Life window. On macOS on Apple silicon you can change its rules, colours and
speed while it runs; elsewhere the window runs without live edits. It needs Nix, and builds ra-jit
from source the first time.

![Editing the running Game of Life: rule, colours and speed change live](docs/demo.avif)

## Updates

<p align="center">
  <a href="https://ra-jit.ix.dev/updates?src=readme">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/brand/signup-on-dark.svg">
      <img alt="Tell us you use ra-jit" src="docs/brand/signup-on-light.svg" height="36">
    </picture>
  </a>
</p>

<p align="center">For release news: Watch → Custom → Releases on GitHub.</p>

## Contributing

How to build ra-jit from a checkout, run its tests and send a change:
[docs/contributing.md](docs/contributing.md). To report a security problem:
[SECURITY.md](SECURITY.md).

## Credits

ra-jit stands on the shoulders of giants. Thank you to the people who built:

- [rust-analyzer](https://github.com/rust-lang/rust-analyzer), whose analysis and MIR ra-jit compiles from
- [Cranelift](https://cranelift.dev) (Bytecode Alliance), the fast code generator
- [LLVM](https://llvm.org) and [inkwell](https://github.com/TheDan64/inkwell), for the optimised tiers
- [salsa](https://github.com/salsa-rs/salsa), the incremental engine underneath
- [object](https://github.com/gimli-rs/object), for reading and writing executables
- [Rust](https://github.com/rust-lang/rust) and its compiler crates
- [rustd](https://github.com/DioxusLabs/rustd) (DioxusLabs), whose code ra-jit includes

None of these projects endorses ra-jit.

Made at [ix](https://ix.dev) for den's native app. MIT or Apache-2.0.
