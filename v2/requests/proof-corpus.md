# Requests from proof-corpus (to `proof` / the coordinator)

## 1. `engine.V1.not_blazes` must not call a failing `blaze check` or `blaze test` cargo's

- **Where:** `harness/v2/engine.py`, `V1.not_blazes`.
- **What:** today it returns `blaze_cli.how_built(stderr)`, which says "cargo" whenever stderr has
  no `[blaze]` tag. Decide from the explicit fallback lines instead (`blaze_cli.by_rustc`, or
  how_built's `; building with cargo` / `this build is cargo's` reasons), at least when the exit
  status is not 0.
- **Why:** v1's `blaze check` prints `[blaze]` only in its `Finished` line on success; the
  turned-down path in `check()` (`crates/blaze-cargo/src/cargo/mod.rs`) prints no tag. So every
  unit `harness/v2/rejections.py` checks (each is meant to be rejected) would fail as "not
  blaze's: cargo", and so would `harness/v2/corpus.py` test steps with a failing test. Both
  scripts call `not_blazes` on every run on purpose (no-fallback).

> applied: by proof (`V2.not_blazes` in `harness/v2/engine.py` takes a failing run with no `by_rustc` line as blaze's).

## 2. A way for `rejections.py` to be gated by its floors alone (if wanted)

- **Where:** `harness/v2/report.py`.
- **What:** `Report.finish` exits 1 on any fail outcome. If the coordinator wants rejections
  gated only by `min_same` and `max_accepted`, report.py needs a way to count per-unit findings
  without failing the run (a flag on `add`, or a verdict that is counted but doesn't fail).
- **Why:** against v1 many units will fail one by one. Without the change, the script behaves as
  the brief says: each unit is an outcome and gates the run.

> forwarded to coordinator: a choice of how rejections gate, asked of the coordinator ("if wanted"); `harness/v2/report.py` is unchanged.

## 3. Lint and type-check `harness/v2/`

- **Where:** `pyproject.toml`.
- **What:** add `harness/v2/*.py` to `[tool.ruff] include` and `[tool.pyright] include`, and an
  E402 per-file ignore for `harness/v2/*.py`.
- **Why:** the template imports after `sys.path[:0] = ...`, as `harness/fnincr.py` and
  `harness/keys.py` already do (and already have the ignore).

> applied: `pyproject.toml` includes `harness/v2` for ruff and pyright and ignores E402 in `harness/v2/*.py`.

## 4. Optional: `engine.Ran.kept`

- **Where:** `harness/v2/engine.py`.
- **What:** `Ran` could carry blaze's kept test passes, as `blaze_cli.Side.cargo` does.
- **Why:** today `corpus.py` reads `target/blaze/tests.ndjson` itself and wraps the result in its
  own `Tested`. That works as it is; the change would only move the reading into the engine.

> refused: optional and only moves working code; proof is DONE, and `harness/v2/` is not an interface `requests` applies.

## 5. One check request for a whole workspace that keeps going and reports per package

- **Where:** the v2 check request in the agent API (`crates/blaze-api`, docs/v2/plan/lanes/ci.md) and the
  `blaze check` client.
- **What:** `blaze check --workspace --keep-going --message-format=json` (or the request behind
  it) checks every member even after one fails, and says for each member whether it was
  accepted (a `compiler-artifact`) or which errors it had (`target.name` on each message).
- **Why:** `harness/v2/rejections.py` checks thousands of rustc compile-fail tests, about 100 to
  a workspace. Today the default asks once per member (`-p`), one client round trip and plan
  per test. With this request it can use `whole: true`, one request per workspace, and the daemon
  checks the members in parallel on all cores. Rust's semantics are unchanged: each member is
  checked exactly as on its own.

> applied: by ci (`Targets::cargo` documents `--keep-going`: every member checked past one that fails, one `Built` per checked unit) and proof (`rejections.py`'s `whole`).
