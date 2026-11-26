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
blaze check
```

There is no daemon: each command is a process that answers and exits, from records kept under
blaze's home (docs/v2/plan/design/bottom.md, "The commands"). No `cargo build` and no rustc run.

- `blaze run` and `blaze test` take cargo's flags and do what cargo's commands do: `run`
  passes what follows `--` to your program, and `test` runs and reports the tests as
  `cargo test` does. Neither writes an executable.
- `blaze check` reports the errors and makes no code.
- `blaze build` writes a standalone PGO-optimized native executable.

A build blaze can't do fails with the reason and is not handed to cargo. Each is a bug
([#84](https://github.com/indexable-inc/blaze/issues/84), [the plan](no-rustc.md)). rustc's `-Z`
flags in `RUSTFLAGS` or `rustflags` are read by `crates/blaze-unit-graph`:
each is ignored (it changes only how rustc works), modelled, or refused, naming the flag; one the
table lacks is refused ([support.md](support.md#-z-flags-for-rustc)).

## Plain `cargo build`

Plain cargo does not build through blaze: `blaze` is the only way in. blaze
runs no rustc wrapper, so a build where `build.rustc-workspace-wrapper` or
`RUSTC_WORKSPACE_WRAPPER` is set (as an earlier blaze's `blaze enable` left in
`~/.cargo/config.toml`) fails saying so; take the key out
([cargo.md](cargo.md#plain-cargo-switched-on-globally)).

## What works and what doesn't yet

- **Platforms.** macOS on Apple silicon and Linux ARM64 first; Linux x86-64 is covered by CI.
  Intel Macs and Windows are not supported.
- **One Rust version, the latest nightly.** blaze builds with the dated nightly it pins
  (`pins/toolchain`, bumped daily) whatever toolchain the project pins. Stable code and
  `#![feature(...)]` code both build; code that needs what that nightly lacks, such as a feature
  it removed, fails with rustc's error (see [install.md](install.md#what-you-need) and
  [support.md](support.md#toolchain)).
- **Correctness.** If a program behaves differently under blaze, that's a bug, and an
  [issue](https://github.com/indexable-inc/blaze/issues) is welcome.

What blaze v1 supported in detail: [support.md](support.md).
