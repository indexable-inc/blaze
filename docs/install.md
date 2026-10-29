---
status: current
summary: Installing blaze: the install script, the release channels and what gets installed.
---
# Installing blaze

```sh
curl -fsSL https://ix.dev/blaze | sh
```

Then, in a project of yours:

```sh
blaze run
```

Change a function and run `blaze run` again: the `Finished` line now says `[blaze]`
(blaze's engine), which means blaze built it instead of cargo. What to do next:
[usage.md](usage.md).

## What you need

- **macOS on Apple silicon, or Linux on x86-64 or ARM64.** On Windows x86-64 (MSVC) blaze
  builds through its engine and takes live edits of a running program, but has no release
  build and the installer refuses Windows: install it [from crates.io](#from-cratesio). There
  is no sandbox there yet, so a build with build scripts or proc macros needs
  `BLAZE_SANDBOX=off` ([platforms.md](platforms.md#windows-x86-64-msvc)). Intel Macs are not
  supported.
- **On Linux, glibc 2.34 or newer**: RHEL 9, Debian 12, Ubuntu 22.04 and anything newer. Each
  is checked by installing there in a container with no Rust. Alpine and other musl systems are
  not supported; the installer says so.
- **A C compiler for linking**: `gcc` (or `cc`) on Linux, Apple's command line tools on macOS
  (`xcode-select --install`). The installer says so when it is missing.
- **User namespaces or Landlock.** Build scripts and proc macros run in a sandbox made of
  Linux namespaces ([cargo.md](cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox)), or,
  where a user may not make them (Ubuntu 23.10 on), confined with Landlock (Linux 5.13 on).
  Where neither can be had, as in a container whose seccomp profile refuses both: run the
  container with `--security-opt seccomp=unconfined` (`options: --security-opt
  seccomp=unconfined` for a GitHub Actions `container:` job), or set `BLAZE_SANDBOX=off` to run
  them outside a sandbox, as cargo does. The error says so when it can tell it runs in a
  container.
- **Memory to spare.** blaze's background process holds a few GB for a small project and tens of
  GB for the largest ([numbers](benchmarks.md#how-much-memory-and-disk-it-takes)).

You do not need Rust or rustup. blaze is its own Rust toolchain ([toolchain.md](toolchain.md)):
`blaze-2026-10-25` today, the nightly of that date (`rustc 1.101.0-nightly (12433c307
2026-10-24)`), moved to a newer one on blaze's schedule. It links its code with code that rustc compiled, so the two have to
agree on how types are laid out and functions are called. It downloads its own copy once into
`~/.blaze` (rustc, cargo, the standard library and its source from the Rust project: 97 MB on
macOS, 118 to 133 MB on Linux), or uses rustup's `nightly-2026-10-25` when rustup has it. blaze
builds your project with that nightly whatever its `rust-toolchain.toml` says, so a project on a
stable release or an older nightly builds as it is, and so does code that uses nightly features
(`#![feature(...)]`). Code the pinned nightly cannot build, such as a feature it removed, fails
with rustc's error and a note saying which toolchain built it.

## The installer

The installer downloads the build for your machine from the
[releases](https://github.com/indexable-inc/blaze/releases), checks it against the checksums
published with it, puts `blaze` and the programs it runs in `~/.cargo/bin`, and
has blaze download its Rust. It asks nothing. On Linux the build has `blaze-proc-macro-srv` too,
blaze's own proc-macro server, which a build from crates.io (below) does not have. Run it again
to upgrade. Releases are dated, `0.YYYYMMDD.N`, and come in two channels
([release.md](release.md)): **stable**, the default, is releases that passed the heavy tests
(real projects on macOS and Linux), and **canary** is the newest release, published as soon as
it is built and promoted to stable, unchanged, once it passes them. End the line with
`| sh -s -- --channel canary` (or set `BLAZE_CHANNEL=canary`) for canary, or with
`| sh -s -- --version 0.YYYYMMDD.N` for a given release;
`--no-toolchain` leaves the download to the first build, and `--help` says what else it takes.
Over a blaze from before 2026-10-28 it removes the programs with their old names and says so.
Until the first dated release, it
installs the `nightly` prerelease, a build of 2026-10-20 that nothing updates any more.

## From crates.io

```sh
cargo +nightly-2026-10-25 install --locked blaze-engine
```

This compiles blaze from source with the nightly it is pinned to, which takes a few minutes (blaze
is built and tested with that nightly only; `rustup toolchain install nightly-2026-10-25` gets
it), and puts three
programs in `~/.cargo/bin`: `blaze`, `blaze-engine` (blaze's engine) and `blaze-rustc`. They find each other by being in the same directory, so
keep them together.
`cargo binstall blaze-engine` installs a release's build instead of compiling it.

## With Nix

```sh
nix profile install github:indexable-inc/blaze
```

Or try it without installing anything: in a project, `nix run github:indexable-inc/blaze -- build`
is `blaze build`. In a flake, take the package from the input:

```nix
{
  inputs.blaze.url = "github:indexable-inc/blaze";
  # ... then use blaze.packages.${system}.default in a dev shell or a profile.
}
```

Nix compiles blaze from source; there is no binary cache yet.

## The old names

blaze had another name and another command before 2026-10-28, and no longer answers to either:
[rename.md](rename.md) maps them to the new ones and says what the installer does over a blaze
from before.

Building blaze itself from a checkout, and every way to build it the release way:
[cargo.md](cargo.md#install) and [contributing.md](contributing.md).
