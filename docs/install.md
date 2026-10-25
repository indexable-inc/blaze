# Installing blaze

<!-- Once https://ix.dev/blaze/install.sh serves install.sh, it is the line to show here. -->
```sh
curl -fsSL https://raw.githubusercontent.com/indexable-inc/blaze/main/install.sh | sh
```

Then, in a project of yours:

```sh
cargo blaze run
```

Change a function and run `cargo blaze run` again: the `Finished` line now says `[ra-jit]`
(blaze's engine), which means blaze built it instead of cargo. What to do next:
[usage.md](usage.md).

## What you need

- **macOS on Apple silicon, or Linux on x86-64 or ARM64.** On Windows blaze installs and runs,
  but hands every build to cargo. Intel Macs are not supported.
- **Rust 1.99.0 with its source code** (the `rust-src` component), which the installer adds
  through rustup. blaze works with this one version only: it links its code with code rustc
  1.99.0 compiled, so the two have to agree on how types are laid out and functions are called.
  `cargo blaze` builds your project with 1.99.0 whatever its `rust-toolchain.toml` says, so a
  project on an older stable builds as it is; code that needs a nightly's features fails with
  rustc's error and a note saying so.
- **Memory to spare.** blaze's background process holds a few GB for a small project and tens of
  GB for the largest ([numbers](benchmarks.md#how-much-memory-and-disk-it-takes)).

## The installer

The installer downloads the build for your machine from the
[releases](https://github.com/indexable-inc/blaze/releases), checks it against the checksums
published with it, and puts `cargo-blaze` and the programs it runs in `~/.cargo/bin`. It asks
nothing. On Linux the build has `blaze-proc-macro-srv` too, blaze's own proc-macro server,
which a build from crates.io (below) does not have. Run it again to upgrade. To install a given
release, end the line with
`| sh -s -- --version 0.2.0`; `--help` says what else it takes. Until the first release under the
name blaze, it installs the newest nightly build.

## From crates.io

```sh
rustup toolchain install 1.99.0 --component rust-src
cargo +1.99.0 install --locked cargo-blaze
```

This compiles blaze from source, which takes a few minutes, and puts five programs in
`~/.cargo/bin`: `cargo-blaze` (what `cargo blaze` runs), `ra-jit` (blaze's engine), `ra-jit-rustc`,
`ra-jit-rustc-first` and `cargo-ra-jit` (the old name). They find each other by being in the same
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

Nix compiles blaze from source; there is no binary cache yet. You still need Rust 1.99.0 with
`rust-src` for your project, as above.

## The old name

blaze was called ra-jit until 2026-10-24. `cargo ra-jit` keeps working as another name for
`cargo blaze`, and its settings work as `BLAZE_*` and as `RA_JIT_*`.

Building blaze itself from a checkout, and every way to build it the release way:
[cargo.md](cargo.md#install) and [contributing.md](contributing.md).
