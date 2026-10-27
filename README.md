<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/fire/header-dark.webp">
    <img alt="blaze" src="docs/brand/fire/header-light.webp" width="344" height="184">
  </picture>
</p>

<h3 align="center">Rebuild Rust in milliseconds.</h3>

<p align="center">The same commands as cargo, with <code>blaze</code> in front.<br>Blazingly fast Rust, literally.</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#try-it">Try it</a> ·
  <a href="#how-fast">How fast</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#learn-more">Docs</a>
</p>

## Install

<!-- Once https://ix.dev/blaze/install.sh serves install.sh, it is the line to show here. -->
```sh
curl -fsSL https://raw.githubusercontent.com/indexable-inc/blaze/main/install.sh | sh
```

For macOS on Apple silicon and Linux, with or without Rust installed: blaze downloads the Rust it
builds with once (about 100 to 135 MB), or uses rustup's. Rather use crates.io or Nix?
[Other ways to install](docs/install.md).

## Try it

In any Rust project:

```text
blaze run     # like cargo run
blaze test    # like cargo test
blaze build   # like cargo build
```

The first build runs cargo, then waits a few seconds for blaze to load your project. Then change
a function and build again: it's back in milliseconds, and the `Finished` line says `[ra-jit]`
(blaze's engine). Every other cargo command passes straight through to cargo. Where cargo is
installed, `cargo blaze run` is the same as `blaze run`.

## How fast

- Edits rebuild <!-- benchheadline:start -->**10× faster** on macOS arm64, **24× faster** on Linux aarch64 and **11× faster** on Linux x86-64<!-- benchheadline:end --> than with `cargo build`, on average over real projects.
- **About 20 ms** from saving a file to a running program using the new code, with
  [live edits](docs/usage.md#change-a-running-program).

<!-- benchpick:start -->
| Edit one function, then build (Linux x86-64) | `cargo build` | `cargo blaze build` |
|---|---:|---:|
| ripgrep | 508 ms | **56 ms** (9.1× faster) |
| nushell | 1.25 s | **236 ms** (5.3× faster) |
| helix | 2.15 s | **116 ms** (19× faster) |
| bun (1.25M lines of Rust) | 9.11 s | **451 ms** (20× faster) |
<!-- benchpick:end -->

Every project, platform, chart, and the memory and disk it takes: [benchmarks](docs/benchmarks.md).

## How it works

blaze runs beside cargo and keeps your project compiled in memory. After an edit it compiles only
the functions you changed and writes them into the program cargo already built, instead of linking
it again from scratch. A program that opts in can take the new code while it keeps running.
[More](docs/architecture.md).

## Good to know

- **macOS on Apple silicon and Linux.** On Windows every build is cargo's.
- **One Rust, the latest nightly.** blaze builds your project with the dated nightly it pins
  (`nightly-2026-10-25` today) whatever toolchain the project pins. A project on a stable
  release builds as it is, and `#![feature(...)]` code builds too.
- **It trades memory for speed:** a few GB for a small project.
- **It's young.** Keep `cargo check` or your CI as the final word, and
  [tell us](https://github.com/indexable-inc/blaze/issues) when something differs from cargo.

All the details: [what works and what doesn't yet](docs/usage.md#what-works-and-what-doesnt-yet).

## Learn more

- [Install](docs/install.md): requirements, crates.io, Nix
- [Using blaze](docs/usage.md): day to day, rebuild on save, making plain `cargo build` use blaze
- [Benchmarks](docs/benchmarks.md): every number, how it's measured, and on what machines
- [How it works](docs/architecture.md): a map of the code
- [Cargo compatibility](docs/cargo.md): every command, flag and setting
- [Hot reload](docs/live.md): changing a running program
- [CI runner](docs/ci-runner.md) and [roadmap](docs/roadmap.md): where blaze is going
- [Contributing](docs/contributing.md)

## Demo: a Game of Life you can edit

```sh
nix run github:indexable-inc/blaze
```

This opens a Game of Life window. On macOS on Apple silicon you can change its rules, colours and
speed while it runs; elsewhere the window runs without live edits. It needs Nix, and builds blaze
from source the first time.

![Editing the running Game of Life: rule, colours and speed change live](docs/demo.avif)

## Updates

<p align="center">
  <a href="https://ra-jit.ix.dev/updates?src=readme">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/brand/signup-on-dark.svg">
      <img alt="Tell us you use blaze" src="docs/brand/signup-on-light.svg" height="36">
    </picture>
  </a>
</p>

<p align="center">For release news: Watch → Custom → Releases on GitHub.</p>

## Contributing

How to build blaze from a checkout, run its tests and send a change:
[docs/contributing.md](docs/contributing.md). To report a security problem:
[SECURITY.md](SECURITY.md).

## Sponsors

Companies that save engineer time with blaze can sponsor it: [SPONSORS.md](SPONSORS.md) has the
tiers and terms. No sponsors yet, be the first.

## Credits

blaze stands on the shoulders of giants. Thank you to the people who built:

- [rust-analyzer](https://github.com/rust-lang/rust-analyzer), whose analysis and MIR blaze compiles from
- [Cranelift](https://cranelift.dev) (Bytecode Alliance), the fast code generator
- [LLVM](https://llvm.org) and [inkwell](https://github.com/TheDan64/inkwell), for the optimised tiers
- [salsa](https://github.com/salsa-rs/salsa), the incremental engine underneath
- [object](https://github.com/gimli-rs/object), for reading and writing executables
- [Rust](https://github.com/rust-lang/rust) and its compiler crates
- [rustd](https://github.com/DioxusLabs/rustd) (DioxusLabs), whose code blaze includes

None of these projects endorses blaze.

blaze was called ra-jit until 2026-10-24; `cargo ra-jit` still works. Made at [ix](https://ix.dev)
for den's native app. MIT or Apache-2.0.
