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

blaze builds all of it itself: no cargo or rustc runs. Change a function and run `blaze run`
again: it builds only what the edit changed, from the records the last run kept. What to do next: [usage.md](usage.md).

## What you need

- **macOS on Apple silicon, or Linux on x86-64 or ARM64.** Windows is out of scope for now,
  and the installer refuses it. Intel Macs are not supported.
- **On Linux, glibc 2.34 or newer**: RHEL 9, Debian 12, Ubuntu 22.04 and anything newer. Each
  is checked by installing there in a container with no Rust. Alpine and other musl systems are
  not supported; the installer says so.
- **A C compiler for linking**: `gcc` (or `cc`) on Linux, Apple's command line tools on macOS
  (`xcode-select --install`). The installer says so when it is missing.
- **User namespaces or Landlock.** Proc macros and pure-Rust build scripts are compiled by blaze
  to wasm64 modules and run in-process under Wasmtime, sandboxed; what runs natively is confined
  with Linux namespaces ([build-scripts.md](v2/plan/design/build-scripts.md)), or, where a
  user may not make them (Ubuntu 23.10 on), confined with Landlock (Linux 5.13 on).
  Where neither can be had, as in a container whose seccomp profile refuses both: run the
  container with `--security-opt seccomp=unconfined` (`options: --security-opt
  seccomp=unconfined` for a GitHub Actions `container:` job). The error says so when it can
  tell it runs in a container.

You do not need Rust or rustup. blaze is its own Rust toolchain ([toolchain.md](toolchain.md)):
`blaze-2026-10-27` today, the nightly of that date (`rustc 1.101.0-nightly (16d1ffac1
2026-10-26)`), moved to a newer one on blaze's schedule. blaze compiles your code and the
standard library itself and behaves as that nightly does. It reads that toolchain as records
under `~/.blaze/v2`, never from rustup's directory, and downloads none of it (`blaze
toolchain`, which the installer runs, only prints the pinned nightly). blaze builds your project with
that nightly whatever its `rust-toolchain.toml` says, so a project on a stable release or an
older nightly builds as it is, and so does code that uses nightly features
(`#![feature(...)]`). Code the pinned nightly cannot build, such as a feature it removed, fails
with blaze's error.

## The installer

The installer downloads the build for your machine from blaze's releases
(`https://blaze-releases.ix.dev`, an R2 bucket at Cloudflare's edge, [release.md](release.md#where-the-bytes-are);
the [GitHub releases](https://github.com/indexable-inc/blaze/releases) list the same ones with
their notes), checks it against the checksums
published with it, puts `blaze` in `~/.cargo/bin`, and runs `blaze toolchain`, which on v2
prints the pinned nightly and downloads nothing. It asks nothing. Run it again to upgrade. Releases are dated, `0.YYYYMMDD.N`, and come in two channels
([release.md](release.md)): **stable**, the default, and **canary**, the newest release; a
tagged release moves both. End the line with
`| sh -s -- --channel canary` (or set `BLAZE_CHANNEL=canary`) for canary, or with
`| sh -s -- --version 0.YYYYMMDD.N` for a given release;
`--help` says what else it takes.
Over a blaze from before 2026-10-28 it removes the programs with their old names and says so.

## With Nix

Not today: `flake.nix`'s package builds `dist`, which runs .github/scripts/build_release.py, a
script not in the tree. Use the installer.

## The old names

blaze had another name and another command before 2026-10-28, and no longer answers to either:
[rename.md](rename.md) maps them to the new ones and says what the installer does over a blaze
from before.

Building blaze itself from a checkout: [contributing.md](contributing.md).
