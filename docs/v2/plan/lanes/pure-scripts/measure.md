---
status: plan
summary: How to run harness/v2/pure_scripts.py, the measurement and differential of pure build scripts (design.md section 8), what it checks and how to read its report.
date: 2026-10-30
---
# Measuring pure build scripts

`harness/v2/pure_scripts.py` measures [design.md](design.md)'s pure path on real build scripts:
which ones blaze proves pure, whether each verdict is what the script's source says it should
be, how much cold-build time the pure path saves, and whether every kept value equals a fresh
sandboxed run (`BLAZE_PURE=verify`, the differential).

## Running it

Run it in the lane's ix VM (`ix new`, `ix shell`; never on the Mac or AWS), from the worktree,
with a blaze built by blaze. The input is one JSON value (inline, a file's path, or `-`), refused
when it has a field it does not know:

```sh
uv run --frozen python harness/v2/pure_scripts.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/pure.json"}'
uv run --frozen python harness/v2/pure_scripts.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/pure.json", "sets": ["crates"], "crates": ["serde_json", "typenum"]}'
uv run --frozen python harness/v2/pure_scripts.py selftest
```

| field | meaning | default |
|---|---|---|
| `engine` | the blaze measured (`harness/v2/engine.py`'s `EngineSpec`) | required |
| `out` | where the JSON report goes (`harness/v2/report.py`'s shape) | required |
| `work` | where sources, trees, homes and stores go | a new `/tmp/bz-pure-scripts-*` |
| `sets` | `crates` (the corpus), `apps` (the harness apps), or both | both |
| `crates` | corpus crates to build, by name | all 100 |
| `apps` | harness apps to build | `reachable.py`'s: ripgrep and every local app |
| `profile` | `dev` or `release` | `dev` |
| `keep` | keep each project's work directory | `false` |

With a v1 engine it reports one `unmeasured` outcome: v1 has no pure scripts. `selftest` checks the
data files, the input, the parsing of the build's lines, the expectations, the verdicts, the
environments of the passes and the table, with no blaze, cargo or network.

Cost: three cold builds per project. The whole corpus and the apps are about 110 projects, mostly
small single crates; budget a few hours on a large VM. Pick crates with `crates` while iterating.

## What it builds

**The corpus** (`harness/v2/pure_scripts/corpus.json`): 100 of the most downloaded crates.io
packages with a `build.rs`, each at a pinned version, each built as its own workspace from its
`.crate` (checked against the sparse index as `harness/v2/corpus.py` does) with default
features. Each entry says what its script is expected to be: `pure`, or why it is not:

| expectation | the script... |
|---|---|
| `pure` | reads only cargo's variables and its package's files, writes `OUT_DIR`, prints |
| `cc` | builds C with `cc` or `cmake`: spawns the C compiler, never pure |
| `rustc` | reads `RUSTC` and runs it (`rustversion`, `autocfg`, `version_check`, probe compiles) |
| `pkg-config` | probes a system library |
| `env` | reads a variable cargo does not give a build script (passed through, `DEP_*`) |
| `listing` | lists a directory |
| `process` | runs some other program (`git`, ...) |
| `thread`, `random`, `other` | spawns a thread, reaches the OS's randomness, anything else |

Every entry was read in its `build.rs` at its version in `~/.cargo/registry/src` (`seen:
registry`); `seen: memory` marks one whose expectation is from memory.

**The apps** (`harness/apps/apps.json`), set up as `harness/v2/reachable.py` sets them up, and
every build script in their graphs. `harness/v2/pure_scripts/apps.json` lists the expectations of
those scripts that the corpus does not pin at the same version.

A script found in neither file is expected by name at another version (the first entry listed),
else from its resolved build dependencies (`cargo metadata` of the lock file the builds read):
`cc` or `cmake` makes it `cc`, `pkg-config` makes it `pkg-config`, `autocfg`, `version_check` or
`rustc_version` makes it `rustc`. Else nothing is expected of it and only the verify build judges
it.

## The three passes

Each project is built cold three times at the same path into one store, each build with an empty
home, an empty target directory and a daemon of its own, timed from nothing to the end of
`blaze build --message-format=json`. blaze has no store holding only the pure keys, so the passes
share one store and change every compile key between them instead:

1. **sandboxed**: the store empty. Every script compiles and runs in the sandbox, its trace is
   checked against its proof, and each pure script's result is kept under its pure key as well.
2. **pure**: `RUSTFLAGS='--cfg blaze_pure_measure_2'`. Every compile key changes and no pure key
   does (the MIR identity is the call graph's, the key's variables are the proof's), so every unit
   compiles cold but the pure scripts, which are neither built nor run.
3. **verify**: `RUSTFLAGS='--cfg blaze_pure_measure_3'` and `BLAZE_PURE=verify`. Every unit
   compiles cold again and the pure scripts are built and run as well; the build fails where a
   run's value differs from the kept one. This is the differential and the control time.

**Time saved** is pass 3's time less pass 2's: the same cold build with and without the pure
scripts built and run. Pass 3 also compares the kept values, which is small beside a script's
compile and run, so the saving is slightly overstated by that. Pass 1 is reported too, as the
first build's cost.

## What it reads from the build

Each build script run of a build prints, under `--message-format=json`, cargo's own
`build-script-executed` line (byte for byte cargo's) and right after it a `blaze-script` line:

```json
{"reason": "blaze-script", "package_id": "...", "out_dir": "...", "purity": "unproved|impure|pure", "said": "...", "identity": "<hex>", "hit": true}
```

`said` is the first impurity as `hir_ty_pure::Impure::said` gives it (impure only), `identity` the
proof's identity (pure only), `hit` whether the value was found under the pure key. The line comes
from `blaze_api::BuildScriptRan`'s `purity`, filled from `script::pure::reported`
(`crates/blaze-cargo/src/serve/unit/script/pure.rs`). A `build-script-executed` line without its
`blaze-script` line, or a line that is not whole (impure with no `said`, pure with no identity, a
hit that is not pure), stops the project with an error naming it.

## What it checks

One outcome per script run of the sandboxed build, named `<project>: <package>@<version>`:

- **pass** when its purity is what is expected of it, or nothing is expected;
- **fail** when it is proved pure and has `cc` or `cmake` as a build dependency, whatever any file
  says: the proof is unsound;
- **fail** when it is proved pure and expected not to be (`rustc`, `env`, ...): unsound;
- **fail** when it is expected pure and is not proved so: the proof is incomplete, or the
  expectation is wrong (read `said` and the script to tell which);
- **error** when it is `unproved`: no proof was made (the sandbox was off, or the front end has no
  crate for the script).

Per project:

- `build`: the sandboxed build built;
- `hits`: in the pure build every pure script of the sandboxed build hit its pure key with the
  same identity, and every other script was judged the same (the same first impurity). A pure
  script that reads `CARGO_ENCODED_RUSTFLAGS` or whose MIR uses the fresh cfg misses here; that
  is reported as a failure naming the script, and its project's saving is understated by it;
- `time`: the three cold times and the saving;
- `verify`: the verify build built (every pure value equals its fresh run) and judged every script
  as the sandboxed one did.

## Reading the report

The run ends with a table, one line per project and the totals (the numbers here only show the
shape):

```
project                           scripts  pure  hits    held  sandboxed s control s   pure s  saved s verified
serde_json@1.0.154                      1     1     1     1/1          9.8       9.6      8.1      1.5      yes
...
total                                 312    88    88 309/312       2410.3    2391.0   2210.4    180.6  110/110
```

`held` is how many scripts' verdicts passed, `control s` pass 3, `pure s` pass 2. The JSON report
(`out`) has every outcome, and in `extra`: `scripts` (every script run: project, package, version,
`out_dir`, purity, `said`, identity, expectation and where it came from, build dependencies,
verdict), `projects` (each project's counts, times, whether it verified, and its lock file's
digest), and `saved_s`, the total saved.

Look first at any `fail` on a script proved pure: that is a soundness bug in `hir-ty-pure` or a
model, and a `verify` failure is its proof. Then the `fail`s on scripts expected pure: each `said`
names the effect and the calls reaching it, the list of what to model or prove next. `saved_s`
against the total control time is what the pure path is worth on a cold build.
