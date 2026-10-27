# Installing blaze

```sh
curl -fsSL https://ix.dev/blaze | sh
```

Then, in a project of yours:

```sh
blaze run
```

Change a function and run `blaze run` again: the `Finished` line now says `[ra-jit]`
(blaze's engine), which means blaze built it instead of cargo. What to do next:
[usage.md](usage.md).

## What you need

- **macOS on Apple silicon, or Linux on x86-64 or ARM64.** On Windows x86-64 (MSVC) blaze
  installs and builds through its engine, but live edits of a running program are not there
  yet. Intel Macs are not supported.
- **On Linux, glibc 2.34 or newer**: RHEL 9, Debian 12, Ubuntu 22.04 and anything newer. Each
  is checked by installing there in a container with no Rust. Alpine and other musl systems are
  not supported; the installer says so.
- **A C compiler for linking**: `gcc` (or `cc`) on Linux, Apple's command line tools on macOS
  (`xcode-select --install`). The installer says so when it is missing.
- **In a container, user namespaces.** Build scripts and proc macros run in a sandbox made of
  Linux namespaces ([cargo.md](cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox)), and
  Docker's default seccomp profile refuses them: run the container with
  `--security-opt seccomp=unconfined` (`options: --security-opt seccomp=unconfined` for a
  GitHub Actions `container:` job), or set `BLAZE_SANDBOX=off` to run them outside a sandbox,
  as cargo does. The error says so when it can tell it runs in a container.
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
published with it, puts `blaze`, `cargo-blaze` and the programs they run in `~/.cargo/bin`, and
has blaze download its Rust. It asks nothing. On Linux the build has `blaze-proc-macro-srv` too,
blaze's own proc-macro server, which a build from crates.io (below) does not have. Run it again
to upgrade. To install a given release, end the line with `| sh -s -- --version 0.2.0`;
`--no-toolchain` leaves the download to the first build, and `--help` says what else it takes.
`cargo blaze <command>` is the same as `blaze <command>`. Until the first release under the name
blaze, it installs the newest nightly build.

## From crates.io

```sh
cargo +nightly-2026-10-25 install --locked cargo-blaze
```

This compiles blaze from source with the nightly it is pinned to, which takes a few minutes (blaze
is built and tested with that nightly only; `rustup toolchain install nightly-2026-10-25` gets
it), and puts six
programs in `~/.cargo/bin`: `blaze`, `cargo-blaze` (what `cargo blaze` runs), `ra-jit` (blaze's
engine), `ra-jit-rustc`, `ra-jit-rustc-first` and `cargo-ra-jit` (the old name). They find each other by being in the same
directory, so keep them together. If you installed `ra-jit` with cargo before, run
`cargo uninstall ra-jit` first: cargo does not replace programs another package installed.
`cargo binstall cargo-blaze` installs a release's build instead of compiling it.

## With Nix

```sh
nix profile install github:indexable-inc/blaze#blaze
```

Or try it without installing anything: in a project, `nix run github:indexable-inc/blaze#blaze -- build`
is `cargo blaze build`. In a flake, take the package from the input:

```nix
{
  inputs.blaze.url = "github:indexable-inc/blaze";
  # ... then use blaze.packages.${system}.blaze in a dev shell or a profile.
}
```

Nix compiles blaze from source; there is no binary cache yet.

## The old name

blaze was called ra-jit until 2026-10-24. `cargo ra-jit` keeps working as another name for
`cargo blaze`, and its settings work as `BLAZE_*` and as `RA_JIT_*`.

Building blaze itself from a checkout, and every way to build it the release way:
[cargo.md](cargo.md#install) and [contributing.md](contributing.md).
