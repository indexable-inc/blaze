# rustc at the pinned nightly: how `-Zpolonius` is defined and defaulted

All excerpts are from rust-lang/rust at commit `a30aa9064df6a504b6a323c6884a2f4cfbfff666` (nightly-2026-10-09), fetched 2026-10-20 from
`https://raw.githubusercontent.com/rust-lang/rust/a30aa9064df6a504b6a323c6884a2f4cfbfff666/<path>`. Line numbers are of that commit.

## Polonius enum and its default

`compiler/rustc_session/src/config.rs` lines 3656-3690

```rust
/// `-Zpolonius` values, enabling the borrow checker polonius analysis, and which version: legacy,
/// or future prototype.
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum Polonius {
    /// Polonius is disabled, only use NLL.
    Off,

    /// Legacy version, using datalog and the `polonius-engine` crate. Historical value for `-Zpolonius`.
    Legacy,

    /// In-tree prototype, extending the NLL infrastructure.
    Next,
}

impl Default for Polonius {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Polonius {
    pub(crate) const DEFAULT: Self =
        if option_env!("CFG_DEFAULT_POLONIUS_NEXT").is_some() { Self::Next } else { Self::Off };

    /// Returns whether the legacy version of polonius is enabled
    pub fn is_legacy_enabled(&self) -> bool {
        matches!(self, Polonius::Legacy)
    }

    /// Returns whether the "next" version of polonius is enabled
    pub fn is_next_enabled(&self) -> bool {
        matches!(self, Polonius::Next)
    }
}

```

## NextSolverConfig default

`compiler/rustc_session/src/config.rs` lines 1049-1060

```rust
// FIXME(#160895): Using -Znext-solver as default on nightly
// See https://github.com/rust-lang/compiler-team/issues/1014
impl Default for NextSolverConfig {
    fn default() -> Self {
        if option_env!("CFG_DEFAULT_NEXT_SOLVER_GLOBALLY").is_some() {
            Self::Globally
        } else {
            Self::Coherence
        }
    }
}

```

## -Zpolonius value description

`compiler/rustc_session/src/options.rs` lines 916-917

```rust
    pub(crate) const parse_polonius: &str =
        "either no value or one of `legacy` (the default), `off`, or `next`";
```

## -Zpolonius parser

`compiler/rustc_session/src/options.rs` lines 1000-1017

```rust
    /// Parses whether polonius is enabled, and if so, which version.
    pub(crate) fn parse_polonius(slot: &mut Polonius, v: Option<&str>) -> bool {
        match v {
            Some("legacy") | None => {
                *slot = Polonius::Legacy;
                true
            }
            Some("next") => {
                *slot = Polonius::Next;
                true
            }
            Some("off") | Some("no") => {
                *slot = Polonius::Off;
                true
            }
            _ => false,
        }
    }
```

## -Z help text

`compiler/rustc_session/src/options.rs` lines 2371-2375

```rust
const POLONIUS_HELP: &str = match Polonius::DEFAULT {
    Polonius::Off => "enable polonius-based borrow-checker (default: no)",
    Polonius::Next => "enable polonius-based borrow-checker (default: next)",
    Polonius::Legacy => panic!("Polonius::Legacy is not a valid default value"),
};
```

## option declaration

`compiler/rustc_session/src/options.rs` lines 2782-2783

```rust
    polonius: Polonius = (Polonius::default(), parse_polonius, [TRACKED],
        POLONIUS_HELP),
```

## bootstrap sets the default on nightly/dev channels

`src/bootstrap/src/core/build_steps/compile.rs` lines 1372-1377

```rust
    let nightly = builder.config.channel == "nightly" || builder.config.channel == "dev";
    if nightly {
        // We want to enable Polonius Alpha and Next Trait Solver by default on nighty
        cargo.env("CFG_DEFAULT_POLONIUS_NEXT", "1");
        cargo.env("CFG_DEFAULT_NEXT_SOLVER_GLOBALLY", "1");
    }
```

## compiletest compare modes

`src/tools/compiletest/src/common.rs` lines 172-178

```rust
    #[derive(Clone, Debug, PartialEq)]
    pub(crate) enum CompareMode {
        Polonius => "polonius",
        NextSolver => "next-solver",
        SplitDwarf => "split-dwarf",
        SplitDwarfSingle => "split-dwarf-single",
    }
```

## compiletest `--compare-mode polonius` passes `-Zpolonius=next`

`src/tools/compiletest/src/runtest.rs` lines 1939-1945

```rust
        match self.config.compare_mode {
            Some(CompareMode::Polonius) => {
                compiler.args(&["-Zpolonius=next"]);
            }
            Some(CompareMode::NextSolver) => {
                compiler.args(&["-Znext-solver"]);
            }
```

## compiletest falls back to the non-polonius .stderr under the compare mode

`src/tools/compiletest/src/runtest.rs` lines 2757-2762

```rust

        if !path.exists() {
            if let Some(CompareMode::Polonius) = self.config.compare_mode {
                path = expected_output_path(&self.testpaths, self.variant.revision(), &None, kind);
            }
        }
```

## CI job testing the alpha on every PR

`src/ci/docker/scripts/x86_64-gnu-next-trait-solver-polonius.sh` (same commit):

```bash
#!/bin/bash

set -ex

# This script tests features intended to be stabilized in 2026. We want to
# ensure they don't regress until then.

# 1. For the next trait solver, we want to:
# - ensure it can build the standard library
# - ensure it actually bootstraps
#
# We test both by building the _stage 2_ library with the next solver enabled
# at stage 1 via rustflags.

RUSTFLAGS_NOT_BOOTSTRAP="-Znext-solver=globally" ../x build library --stage 2

# 2. For the polonius alpha, we run the UI tests under the polonius
# compare-mode.
#
# Note that we keep the same rustflags to avoid needing to rebuild any stage 1
# artifacts from the previous command. It also tests both features at the same
# time.

RUSTFLAGS_NOT_BOOTSTRAP="-Znext-solver=globally" ../x test tests/ui \
    --compare-mode polonius --stage 1
```

## The installed pinned toolchain agrees

`rustc +nightly-2026-10-09 -vV` reports `commit-hash: a30aa9064df6a504b6a323c6884a2f4cfbfff666`, and
`rustc +nightly-2026-10-09 -Z help` prints (run 2026-10-20 on the author's machine):

```text
    -Z                                      polonius=val -- enable polonius-based borrow-checker (default: next)
```
