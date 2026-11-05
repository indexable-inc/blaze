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

- **macOS on Apple silicon, or Linux on x86-64 or ARM64.** Windows has no release build and
  the installer refuses it; v2 has not been run there (v1's port:
  [platforms.md](platforms.md#windows-x86-64-msvc)). Intel Macs are not supported.
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
from the nightly the Rust project publishes (rustc, cargo and the standard library of the host and of wasm32-wasip1, 122 MB on
macOS, 143 to 159 MB on Linux): WASI's C runtime and the sanitizer runtimes. It finds them in
`~/.blaze` only, never in rustup's directory. The standard library's
source comes from blaze's own release. v2's blaze does not download that copy itself yet (the installer asks for it with
`blaze toolchain`, which v2 lacks: [toolchain.md](toolchain.md)). blaze builds your project with
that nightly whatever its `rust-toolchain.toml` says, so a project on a stable release or an
older nightly builds as it is, and so does code that uses nightly features
(`#![feature(...)]`). Code the pinned nightly cannot build, such as a feature it removed, fails
with blaze's error.

## The installer

The installer downloads the build for your machine from blaze's releases
(`https://blaze-releases.ix.dev`, an R2 bucket at Cloudflare's edge, [release.md](release.md#where-the-bytes-are);
the [GitHub releases](https://github.com/indexable-inc/blaze/releases) list the same ones with
their notes), checks it against the checksums
published with it, puts `blaze` and the programs it runs in `~/.cargo/bin`, and
runs `blaze toolchain` to download its Rust, a command v2's blaze does not have, so on v2 this
step fails. It asks nothing. Run it again to upgrade, or run `blaze update`
([Updates](#updates)). Releases are dated, `0.YYYYMMDD.N`, and come in two channels
([release.md](release.md)): **stable**, the default, is releases that passed the heavy tests
(real projects on macOS and Linux), and **canary** is the newest release, published as soon as
it is built and promoted to stable, unchanged, once it passes them. End the line with
`| sh -s -- --channel canary` (or set `BLAZE_CHANNEL=canary`) for canary, or with
`| sh -s -- --version 0.YYYYMMDD.N` for a given release;
`--no-toolchain` skips that download (v1 left it to the first build; v2's first build does not
download it), and `--help` says what else it takes.
Over a blaze from before 2026-10-28 it removes the programs with their old names and says so.

## Updates

blaze tells you when a newer release is out, on stderr, before a command's output:

    blaze 0.20261104.3 is out (this is 0.20261104.0): blaze update · notes: https://github.com/indexable-inc/blaze/releases/tag/v0.20261104.3

and, in a workspace that pins an older release (`[workspace.metadata.blaze] version`):

    this workspace pins blaze 0.20261027.2, 8 days behind 0.20261104.3: blaze pin 0.20261104.3

Each is said on every command while it holds, even one release behind. A release that is
**yanked** (known broken; [release.md](release.md#yanking-a-release) says how a release is
yanked) is said louder, on every command, under CI too, with the reason and what moves off it:

    warning: blaze 0.20261101.0 is YANKED: miscompiles drops (#1234)
    warning: `blaze update` installs blaze 0.20261104.3

Nothing here waits on the network. blaze's daemon reads `https://ix.dev/blaze/latest.json` every
five minutes in the background (with its ETag, so an unchanged file is a `304`) and keeps what it
found in `~/.blaze/update/state.json`; a command reads only that file. Before the daemon's first
check nothing is said, and a check that fails keeps what the last one found and invents nothing.
What stops the daemon from doing its part (a `config.toml` or `~/.blaze/default` it cannot
read, an `"auto"` install that failed) is kept there too and said as one `warning:` line.
Under CI (`CI` set) or when stderr is not a terminal, only a yanked release is said.

`blaze update` reads `latest.json` at once, downloads the newest release of your channel,
checks it as a pinned release is checked (every list of sums it publishes:
[cargo.md](cargo.md#pinning-a-release-of-blaze)), puts it in `~/.blaze/versions/<version>`,
makes it the default and prints the first lines of its notes. The default is what runs outside
pinned workspaces: the `blaze` you installed runs it in its place, as it runs a pinned release
(`~/.blaze/default` names it; running install.sh again removes it, so the blaze installed last
runs). A daemon of the release before is replaced at the next command, as when any other build
of blaze meets it. `blaze update --check` says what is out and installs nothing.

`blaze update` never installs a yanked release: when your channel's newest is yanked it refuses
and says so, and when moving off a yanked release goes to an older one it says that it is a
downgrade. A `~/.blaze/default` it cannot parse is replaced, and it says so. When the blaze that
runs is newer than the channel's newest (a canary under `"stable"`), it says that rather than
calling it the newest.

`blaze update` never changes a workspace's pin: there, it says the pin and that `blaze pin
<version>` moves it. `blaze pin 0.YYYYMMDD.N` pins that release (fetched and checked first);
`blaze pin` alone pins the blaze that runs it.

The settings are `[update]` in `~/.blaze/config.toml` (`$BLAZE_HOME/config.toml`), which
`blaze update --mode M` and `blaze update --channel C` write:

| `mode` | |
|---|---|
| `"warn"` (the default) | the lines above |
| `"auto"` | the lines above, and the daemon installs the newest release as the default by itself, saying `blaze updated to <version>; notes: <url>` at the next command. Never a pin. A release it failed to install is not tried again by itself; `blaze update` tries it |
| `"off"` | no line about newer releases or old pins. The daemon still reads `latest.json` every five minutes, so that a yanked release is still said |
| `"offline"` | no network at all: the daemon reads nothing and `blaze update` refuses. A release yanked after this is set is never said (what an earlier check kept still is) |

`channel` is `"stable"` (the default) or `"canary"`, the releases whose newest counts;
install.sh with `--channel canary` sets it. A setting that is not one of these is said in place
of the lines, which stay off but for a yanked release until it is fixed. `BLAZE_RELEASES`, which
moves the releases ([cargo.md](cargo.md#pinning-a-release-of-blaze)), moves `latest.json` with
them: `<mirror>/latest.json`. A build of blaze's tree (`0.0.0-dev`) checks nothing and runs
itself; `blaze update --check` works there.

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

Nix compiles blaze from source; there is no binary cache yet. Such a build, like one from
`cargo install --git` or `--path`, is a build of blaze's tree (`blaze 0.0.0-dev`), not a
release: in a project that pins a release of blaze (`[workspace.metadata.blaze] version`) it
runs that release, fetched once, saying so in one line on stderr, as a release does;
`blaze --ignore-pin <command>` runs the build itself there
([cargo.md](cargo.md#pinning-a-release-of-blaze)).

## With MacPorts

```sh
sudo port install blaze
```

This fetches the blaze source and compiles locally without having to configure anything else on Apple Silicon Macs.

## The old names

blaze had another name and another command before 2026-10-28, and no longer answers to either:
[rename.md](rename.md) maps them to the new ones and says what the installer does over a blaze
from before.

Building blaze itself from a checkout, and every way to build it the release way:
[cargo.md](cargo.md#install) and [contributing.md](contributing.md).
