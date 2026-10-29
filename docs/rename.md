---
status: record
summary: The rename from ra-jit and cargo blaze to blaze (#270), and what tools/rename.py changes.
date: 2026-10-28
---
# The rename from ra-jit (#270)

blaze was called ra-jit until 2026-10-24, and `cargo blaze` was its command until 2026-10-28.
Nothing of either is read or run any more: `blaze` is the only command, and the names below
are the only ones. [tools/rename.py](../tools/rename.py) made the change, and makes it again
on any branch from before it.

| before | now |
|---|---|
| `cargo blaze <command>`, `cargo ra-jit <command>` | `blaze <command>`, with the same commands and flags |
| the programs `cargo-blaze`, `cargo-ra-jit` | gone: `blaze` |
| the program `ra-jit`, the engine `blaze` runs | `blaze-engine` |
| `ra-jit-rustc`, `ra-jit-rustc-first` | `blaze-rustc`; the second is gone |
| `RA_JIT_<NAME>` | `BLAZE_<NAME>` |
| the crate `crates/ra-jit`, published as `cargo-blaze` | `crates/blaze-engine`, published as `blaze-engine` |
| the crates `ra-jit-<name>` | `blaze-<name>` |
| the forks published as `ra-jit-<name>` | `blaze-<name>`; `macros`, whose name is taken, is `blaze-fork-macros` |
| `#[ra_jit::cache]` | `#[blaze::cache]` |
| `[package.metadata.ra-jit]` | `[package.metadata.blaze]` |
| the fork of rowan, `crates/rowan`, `use rowan::...` | `syntax-tree`, `crates/syntax-tree`, `use syntax_tree::...` |
| the forks `ra-ap-rustc_abi`, `ra-ap-rustc_type_ir`, `ra-ap-rustc_next_trait_solver` | `rustc_abi`, `rustc_type_ir`, `rustc_next_trait_solver` |
| `~/.cache/ra-jit`, `$XDG_RUNTIME_DIR/ra-jit`, `target/ra-jit` | the same with `blaze` |
| `$CARGO_HOME/ra-jit-enabled.json` | `$CARGO_HOME/blaze-enabled.json` |

## Why nothing old is honoured

An old name that still worked would be one more name to document, test and keep. So:

- A variable named `RA_JIT_<NAME>` stops blaze, with one line for each: `RA_JIT_X was renamed
  BLAZE_X`.
- blaze starts its state afresh under the new directories and doesn't move the old ones. The
  first command that finds one says, once, where it is, how big it is, and that it can be
  deleted. `target/ra-jit` goes with `cargo clean`.
- [install.sh](../install.sh), over a blaze from before, disables the old one if it was
  enabled, and removes the old programs (saying so in one line when it removes `cargo-blaze`).
- `blaze` takes away the links to `cargo-blaze` an earlier `enable` made for `cargo fmt` and
  `cargo clippy`, with the rest of what `enable` wrote
  ([cargo.md](cargo.md#plain-cargo-switched-on-globally)).

The remote cache's keys and the fn-cache's files changed with the names they hash, so
neither finds what was stored under the old ones.

## A branch from before the rename

    python3 tools/rename.py && git commit -qam 'rename.py'
    git rebase origin/main

The script rewrites every tracked text file and `git mv`s every path whose name says ra-jit.
Running it a second time changes nothing. `--check` lists what still says ra-jit or
`cargo blaze`, `--env` renames only the settings, and `--dry-run` changes nothing. CHANGELOG.md,
this file, the script and `renamed.rs` (what blaze says of the old names) are left as they are,
and so is a line with `rename: old name` in a comment.
