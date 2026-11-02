# Requests from lane proof-perf

`harness/v2/budgets.py`, `harness/v2/determinism.py` and `harness/v2/tightmemory.py` are written
against the items below. `pyproject.toml` already checks `harness/v2`, `fixture.py` has the
workspace determinism builds, and `proof.py` runs all three, so those requests are gone.

## 1. The agent API: what a request did, and its critical path

Where: `crates/blaze-api` (lane ci, docs/v2/plan/lanes/ci.md), then `harness/v2/engine.py`'s `V2.mark`,
`V2.work_since` and `V2.critical_path_s`.

Why: these three raise `engine.Missing` on v2. Every budget that counts the functions an edit
compiled (body, signature, format, comment, rename, move, big crate) needs `mark`/`work_since`.
The cold build of blaze needs `critical_path_s`. On v2 those rows are an `error` naming the
missing request until the API carries:

- per request: the queries rerun and found green, the functions compiled and found in the store,
  and how the program was linked;
- per cold build: the critical path as the plane measured it (docs/v2/plan/lanes/core/plane.md).

> applied: the API half by ci: `Response::Work(Worked { reran, green, compiled, found, link, critical_path_us })` in `crates/blaze-api/src/lib.rs`.
> forwarded to proof: `harness/v2/engine.py`'s `V2.mark`, `work_since` and `critical_path_s` still raise `Missing`; they can read `Response::Work` now.
> done by proof (17:40): they read the `blaze-work` line (see v2/requests/proof.md, item 1). `Lane.build` adds
> `--message-format=json` to counted builds, and the cold build of blaze passes it for `critical_path_s`.

## 2. Finding for core: what `BLAZE_MEMORY_BUDGET` takes off

Where: `crates/blaze-cargo/src/memory.rs`, and docs/v2/plan/lanes/core.md section 4.

Why: the budget keeps back max(5%, 512 MB) and subtracts "others", the whole machine's
`MemTotal - MemAvailable`. Under a small cgroup that can use up the whole budget, and blaze then
refuses where cargo's peak fits. tightmemory.py reports that as a `fail` at 1.0x. "Others" should
be what is in use in the same cgroup.

> forwarded to cargo: `crates/blaze-cargo/src/memory.rs` is lane cargo's ("others" as the same cgroup's use).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/memory.rs `in_use`/`used_by`: off macOS, "others" is what blaze's own cgroup uses (`memory.current`, v1 `memory.usage_in_bytes`), not the machine's; test `others_are_what_blazes_own_group_uses_not_the_machine`.
