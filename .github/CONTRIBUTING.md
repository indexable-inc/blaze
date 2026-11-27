# Contributing

The long version, with the rules for code and how to change the vendored rust-analyzer, is
[docs/contributing.md](../docs/contributing.md). The short version:

## Build

```
git clone https://github.com/indexable-inc/blaze
cd blaze
sh site/install.sh --version "$(cat .github/blaze-version)"   # stage 0: the pinned release of blaze, with its own Rust
blaze build --release -p blaze-cli                            # blaze builds blaze; it fetches the pinned LLVM itself
```

blaze is never built with cargo or rustc (AGENTS.md, rule blaze-builds-blaze).

## Test

```
blaze test -p blaze-engine -p parser -p syntax
```

Formatting is not yours to run: `fmt.yaml` formats main after the merge with the pinned release's
`blaze fmt --all`; the CLI built from this tree has no `fmt` command yet.

`python3 harness/unittests.py` is what CI runs: every test of every crate, held to `harness/known-failures.toml`, the one list of tests that may fail.

The real gate is real crates' own test suites passing under blaze, and tests that state their
expected result: see [docs/harness.md](../docs/harness.md). Run what fits your change and say in the pull request
what you ran and on which platform.

## Changes

- Add a reduced case that fails before your change and passes after.
- Credit anyone whose idea, diagnosis or patch the change carries with a `Co-authored-by:`
  trailer: [docs/contributing.md](../docs/contributing.md#credit).
- Contributions are accepted under MIT OR Apache-2.0, the project's licence.

Security problems go through [SECURITY.md](SECURITY.md), not public issues.
