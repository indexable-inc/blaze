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

blaze prints what cargo prints, and builds all of it itself: no cargo or rustc runs. Change a
function and run `blaze run` again: blaze's daemon, still running, builds only what the edit
changed. What to do next: [usage.md](usage.md).

## What you need

- **macOS on Apple silicon, or Linux on x86-64 or ARM64.** Windows has a release archive, which
  promotion unpacks and tries, but the installer (`install.sh`) refuses it, and v2 has not been
  run there (v1's port: [platforms.md](platforms.md#windows-x86-64-msvc)). Intel Macs are not
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
`blaze-2026-10-27` today, the nightly of that date (`rustc 1.101.0-nightly (16d1ffac1
2026-10-26)`), moved to a newer one on blaze's schedule. blaze compiles your code and the
standard library itself and behaves as that nightly does. Its linker still takes a few files
from the nightly the Rust project publishes (rustc, cargo and the standard library, 97 MB on
macOS, 118 to 133 MB on Linux): WASI's C runtime and the sanitizer runtimes. It finds them in
`~/.blaze` or in rustup's `nightly-2026-10-27` when rustup has it. The standard library's
source comes from blaze's own release. v2's blaze does not download that copy itself yet (the installer asks for it with
`blaze toolchain`, which v2 lacks: [toolchain.md](toolchain.md)). blaze builds your project with
that nightly whatever its `rust-toolchain.toml` says, so a project on a stable release or an
older nightly builds as it is, and so does code that uses nightly features
(`#![feature(...)]`). Code the pinned nightly cannot build, such as a feature it removed, fails
with blaze's error.

## The installer

The installer downloads the build for your machine from the
[releases](https://github.com/indexable-inc/blaze/releases), checks it against the checksums
published with it, puts `blaze` and the programs it runs in `~/.cargo/bin`, and
runs `blaze toolchain` to download its Rust, a command v2's blaze does not have, so on v2 this
step fails. It asks nothing. Run it again to upgrade. Releases are dated, `0.YYYYMMDD.N`, and come in two channels
([release.md](release.md)): **stable**, the default, is releases that passed the heavy tests
(real projects on macOS and Linux), and **canary** is the newest release, published as soon as
it is built and promoted to stable, unchanged, once it passes them. End the line with
`| sh -s -- --channel canary` (or set `BLAZE_CHANNEL=canary`) for canary, or with
`| sh -s -- --version 0.YYYYMMDD.N` for a given release;
`--no-toolchain` skips that download (v1 left it to the first build; v2's first build does not
download it), and `--help` says what else it takes.
Over a blaze from before 2026-10-28 it removes the programs with their old names and says so.
Until the first dated release, it
installs the `nightly` prerelease, a build of 2026-10-20 that nothing updates any more.

## From crates.io

```sh
cargo +nightly-2026-10-27 install --locked blaze-engine
```

This compiles blaze from source with the nightly it is pinned to, which takes a few minutes (blaze
is built and tested with that nightly only; `rustup toolchain install nightly-2026-10-27` gets
it), and puts two
programs in `~/.cargo/bin`: `blaze` and `blaze-engine`, the same program under the name blaze's
tooling knows it by.
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
