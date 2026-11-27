---
status: current
summary: Getting started: using blaze on a project, in short.
---
# Using blaze

Not installed yet: [install.md](install.md).

## Use it on your project

```sh
cd my-project
blaze check
```

There is no daemon: each command is a process that answers and exits, from records kept under
blaze's home (docs/v2/plan/design/bottom.md, "The commands"). No `cargo build` and no rustc run.

- `blaze run` and `blaze test` take `-p` and `--bin` as cargo does: `run` passes what follows
  `--` to your program, and `test` runs and reports the tests. Neither writes an executable.
- `blaze check` reports the errors and makes no code.
- `blaze build` writes a JIT-on-call executable; `--release` a standalone PGO-optimized one.

The others: `ci` (the checks with structured reporting), `inspect` (a file, lines or an item
with inline types and docs), `why` (why a file or item was rechecked), `explain` (a diagnostic,
lint or exit code), `clean`, `gc`, `peer`, `toolchain`, `warm-toolchain`, `store publish` and
`version`. Their source is `crates/blaze-cli-spec/src/blaze_app.rs`.

`blaze help <command>` lists the flags each command takes. A build blaze can't do fails with
the reason and is not handed to cargo. Each is a bug
([#84](https://github.com/indexable-inc/blaze/issues/84), [the plan](no-rustc.md)).

## Plain `cargo build`

Plain cargo does not build through blaze: `blaze` is the only way in.

## What works and what doesn't yet

- **Platforms.** macOS on Apple silicon and Linux ARM64 first; Linux x86-64 has a release build, and CI runs no test on it.
  Intel Macs and Windows are not supported.
- **One Rust version, the latest nightly.** blaze builds with the dated nightly it pins
  (`pins/toolchain`, bumped by hand) whatever toolchain the project pins. Stable code and
  `#![feature(...)]` code both build; code that needs what that nightly lacks, such as a feature
  it removed, fails with blaze's error (see [install.md](install.md#what-you-need) and
  [support.md](support.md#toolchain)).
- **Correctness.** If a program behaves differently under blaze, that's a bug, and an
  [issue](https://github.com/indexable-inc/blaze/issues) is welcome.

What blaze v1 supported in detail: [support.md](support.md).
