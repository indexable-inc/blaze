# Handoff: the profile gate as a real CI check (worktree blaze-wt-pd-gate, branch pd-gate)

Nothing is committed. I wrote code only and ran no build, check, test, clippy, cargo, rustc, blaze, daemon or VM. Things I did run:
- the pinned DuckDB CLI, to write the fixtures and to check every SQL query of the gate against them by hand;
- `python3 tools/docs.py check` (clean);
- small Python sanity runs of the scripts' pure logic.

## Files changed

Rust:
- `crates/blaze-profile/src/gate.rs`: rewritten. It now has 11 checks; the verdict reports each one and passes only when all passed. It also adds `summary()` and the tests.
- `crates/blaze-profile/src/history.rs`:
  - `process_cpu_ns()` (getrusage; `None` on non-unix).
  - `mark_cpu` at the start and end of each build, written as `process cpu ns` counter rows on the next flush.
  - `MERGED_ROWS = "blaze.merged_rows"`: `concat` writes the inputs' footer row total into the merged file's KV metadata, and drops any copied from inputs.
  - `concat`, `Writer::properties` and `mod compact` are now `pub(crate)`.
  - New test `a_merge_says_the_rows_its_inputs_held_and_holds_them`.
- `crates/blaze-profile/src/flight.rs`: `Plot::ProcessCpu` ("process cpu ns"); `Plot::ALL` is now 22 long.
- `crates/blaze-profile/src/slow/load.rs`: `recorder_sql` and `recorder` are now `pub(crate)`, for reuse.
- `crates/blaze-cargo/src/cli/observe.rs`: new `blaze debug gate [--build N] [--history DIR] [--json] [--summary FILE]`.
  - It prints the verdict (text or JSON), then writes the summary.
  - It exits 101 unless every check passed.

SQL:
- `crates/blaze-profile/sql/views.sql`: new view `build_summary`, one row per (build, span name): commit, tree, blaze, started_unix_ns, build_dur_ns, outcome, runs, median_ns, total_ns, self_cpu_ns. This is the one definition of the per-build summary.
- `crates/blaze-profile/sql/examples.sql`:
  - `slower_between_commits` now reads `build_summary` (median of per-build medians) and adds `earlier_builds` and `later_builds`.
  - `uncovered_time` now says 1% (`gate::UNCOVERED_MAX_PCT`), not 5%.

Fixtures and scripts:
- `crates/blaze-profile/fixtures/gate/<check>/*.parquet`: 9 fixtures, 27 files, 108 KB in all.
- `harness/profile_fixtures.py`: writes those fixtures. Run: `uv run --frozen --no-dev python harness/profile_fixtures.py '{"duckdb": "...", "out": "crates/blaze-profile/fixtures/gate"}'`.
- `harness/profile_gate.py`: the CI driver.
- `harness/profile-gate-known.toml`: the known list.
- `harness/profile_trend.py`: the nightly's driver.

CI:
- `.github/workflows/blaze-platform.yaml`:
  - The build job writes a `profile-since` marker before the build.
  - After `blaze stop` it copies the history files newer than the marker and uploads them as artifact `profile-<platform>` (kept 1 day).
  - New job `profile-gate`.
- `.github/workflows/profile-nightly.yaml`: new; runs nightly and by hand, over linux-x64, linux-arm64 and macos-arm64.
- `.github/scripts/gate_changes.py`: CODE gains `harness/profile_gate.py`, `harness/profile-gate-known.toml`, `flake.nix` and `flake.lock`.
- `pyproject.toml`: the three new scripts are in the ruff and pyright includes.

Docs:
- `docs/profiling.md`: new section "The gate: what a profile must hold" with a table of checks and bounds, and how CI runs, lists and trends it. The counters list now has `process cpu ns`. The blind-spot line no longer says "No OS CPU is recorded".
- `docs/harness.md`: the CI table has `profile-gate` and `profile-nightly.yaml`. The gate_changes paragraph and the description of what the merge gate is are updated.

## Design

There is one threshold per check, each a constant in gate.rs:
- `UNCOVERED_MAX_PCT` 1.0 (slow.rs already uses it);
- `ROWS_LOST_MAX` 0;
- `SAMPLES_IN_SPANS_MIN_PCT` 99.0;
- `CPU_DIFF_MAX_PCT` 5.0;
- `CLOCK_SLACK_NS` 10 µs.

The checks:
- `workers_covered`: the `uncovered_per_worker` view.
- `blocks_in_waits`: the `raw_blocks` view. It cannot run when there are no off-CPU samples.
- `waits_say_what`.
- `rows_lost`: the `recorder_sql` deltas of `profile spans lost` and `profile rows skipped`.
- `samples_in_spans`: pool workers' samples only.
  - "Tied" means `span_id` is set and the span row, if present, was open at the sample time (within the slack).
  - A sample whose span has no row (folded or lost) counts as tied and is reported in the detail.
- `children_in_parents`: joined on (process, parent).
  - Children whose parent has no row are listed as not checked.
- `self_cpu_in_duration`: covers spans and folded sums.
- `cpu_matches_process`: max minus min of the build's `process cpu ns` marks, against the sum of spans' and folded `self_cpu_ns`.
- `slow_deterministic`: `slow::load::run` twice, serialized exactly as `blaze slow --json` prints it. A mismatch reports the first differing byte with context.
- `merged_rows_match`: every merged file of the build has a footer `num_rows` equal to its `blaze.merged_rows`.
- `merges_exact`:
  - It concatenates the build's files of each table again into a scratch directory; a table with a single file is concatenated with itself.
  - It checks the footer rows, then compares DuckDB `count(*)` and `sum(hash(row))` of the output against the inputs as multisets.

I split the brief's "merged file holds exactly its inputs' rows" into two checks. Seqs are global, so a merged file's inputs cannot be rebuilt after they are deleted. The new metadata key covers files already merged, and the fresh re-merge covers the merge code itself.

How the gate runs:
- The SQL checks run in one DuckDB (`run_all`). If that fails, each query is rerun alone so the error belongs to its own check.
- A failed query or missing rows makes a check `unchecked` with the reason, never `passed`.
- Only "no ended build" is a hard error.

The CI deadlock problem: the CI history is written by the release blaze, not by this tree. A plain blocking gate would hold every fix behind a release. So `profile_gate.py` holds the verdict to `profile-gate-known.toml`, in the pattern of known-failures.toml:
- Each entry is check, outcome, optional platforms, owner and cause.
- A check that fails or cannot run and is not listed with that outcome fails the job.
- So does a listed check that passes (the list only shrinks).
- So does a fixture whose check did not fail, and a build row with no tree or no blaze.

Pre-listed:
- `cpu_matches_process` unchecked, because the release writes no `process cpu ns`;
- `merged_rows_match` unchecked, because the release writes no key;
- `blocks_in_waits` unchecked on linux-x64 and linux-arm64, because Linux takes no off-CPU samples.

The `profile-gate` job:
- It needs the build job, runs on `inputs.os`, and is skipped on Windows and for focused runs.
- It gets the pinned DuckDB with nix-installer and `nix build .#duckdb`, and uses the blaze from the build job's `blaze.tar`.
- It has no secret and no environment, with `permissions: contents: read`, so fork PRs are safe.
- It is a job of the reusable platform workflow, so its failure fails blaze.yaml's `ci`.
- It uploads `summary.parquet`, `identity.json` and `verdict.json` as `profile-summary-<platform>-<runnerclass>-<commit12>-<tree12>-<blaze12>`. Every non-alphanumeric character in a field becomes `_`, so `-` only separates fields. They are kept 90 days.
- The runner class is `ix-<arch>-<cpus>` or `inputs.os`.

The nightly (`profile_trend.py`):
- It lists artifacts with `gh api --paginate`, keeping main's.
- It takes the runner class of the newest one, then the newest summary for each of the last N commits (default 10).
- It downloads them in parallel and builds a `build_summary` view, relabelling the newest as `later` and the rest as `earlier`.
- It runs `slower_between_commits`, read out of examples.sql.
- It fails naming each span whose median is at least 1.25 times and at least 50 ms slower.
- With fewer than 2 commits it fails, saying nothing was compared.

Note on the summary lane: I found no lane in the other pd-* worktrees that adds a stored per-build summary. `build_summary` in views.sql is the single definition. If that lane makes it a stored table, it should keep the same name and columns, and replace the view.

## Risks

- Unknown outcomes on the first run. I could not run the gate on a real self-build. Any of these may fail or be unable to run on the release history:
  - `workers_covered`, `samples_in_spans`, `children_in_parents`, `self_cpu_in_duration`, `rows_lost`, `slow_deterministic`;
  - `blocks_in_waits` on macOS.

  Then the first CI run fails `profile-gate` on every platform. Read the job's step summary and either fix the cause or add owned entries to the known list. Watch `rows_lost`: the docs say self-builds skip rows.
- Where the history lives: the build job reads `${BLAZE_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/blaze}/v2/profile`, taken from `blaze_manifest::home::root`.
  - If the release writes elsewhere, or keeps no history, the artifact holds only `copied.txt`, and the gate errors with "no build that ended".
  - The `-newer` filter is there because ix runners keep earlier runs' history.
- Last build only: the gate checks the history's last ended build. If `unittests.py {"build": true}` runs several blaze builds, the earlier ones go unchecked.
- `merges_exact` copies the build's whole history once into the temp dir, which costs disk space.
- `cpu_matches_process` compares process CPU, which includes threads outside spans (drain, flush, watcher). 5% may be too tight once it can run. It is honest, but expect it to need an owned entry or a cause fix.
- `samples_in_spans` counts only the pool workers in `switches`, the same set as `workers_covered`.
- The nightly needs `actions: read` on GITHUB_TOKEN and paginates every repo artifact. It is bounded at 900 s.
- The `slower_between_commits` output gained two columns. Every existing argument and test usage is unchanged.

## Tests

- gate.rs, not ignored:
  - `a_verdict_says_each_check_and_lists_what_could_not_run`
  - `a_check_that_could_not_run_fails_the_verdict`
  - `check_names_are_their_json`
  - `the_first_difference_is_the_first_byte_that_differs`
  - `every_check_a_file_can_fail_has_its_fixture` (filesystem only)
- gate.rs, ignored without `BLAZE_DUCKDB`:
  - `each_fixture_fails_its_own_check`, which takes seconds;
  - `a_profiled_self_build_passes_the_gate`, the old self-build test, kept.
- history.rs: `a_merge_says_the_rows_its_inputs_held_and_holds_them`.
- In CI, `profile_gate.py` runs every fixture through the real `blaze debug gate` and requires each one to fail its check.
- Done by hand with the DuckDB CLI:
  - every gate query (or the view it reads) gave the expected failing numbers on its fixture;
  - `hash(x)` over the row struct and the `KV_METADATA` round trip work in DuckDB 1.5.5;
  - the rewritten `slower_between_commits` ran over two synthetic summaries.

## What to check

1. `blaze check` / `blaze test -p blaze-profile -p blaze-cargo`. Likely snags:
   - alloc-comment lints on `format!`s inside the `sql_checks` vec;
   - `as Judge` fn-pointer casts;
   - `PageIndexPolicy::Skip` (parquet 60.1);
   - the `views.rs` identifier test over the new `build_summary` and `slower_between_commits` (I believe it uses only listed functions, keywords and columns).
2. `BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb blaze test -p blaze-profile -- --ignored each_fixture_fails_its_own_check`.
3. Run `blaze debug gate --json` on a local self-build's history. Then put real outcomes into `harness/profile-gate-known.toml`, with owners, before the first PR run, so the merge gate isn't red.
4. `uv run --frozen ruff check` and `uv run --frozen pyright`. Lines exceed 110 characters, but ruff's default rules don't include E501.
5. CI: confirm the `profile-<platform>` artifact holds this run's files, the `profile-gate` job runs and passes against the known list, and the summary artifact appears. After a few main pushes, run `profile-nightly.yaml` by hand.
