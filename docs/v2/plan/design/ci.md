---
status: plan
summary: blaze's own CI in seconds: a pull request is a view on a warm daemon that already holds main's state, on the Mac runner itself and on one long-lived Linux machine, with no VM per job; a fresh ix snapshot per job only for untrusted code. The per-step budget to a check in under 20 s, what blaze must do for it, the trust boundary, and what is built.
date: 2026-11-10
---
# CI in seconds

Objective T7 is blaze's CI on a small pull request in under 2 min; the aim here is under 20 s
in most cases. Today the gate (`blaze.yaml`) takes 5 to 49 min: each job is a fresh machine
(an ix runner forked from its label's last green snapshot, disk only, processes scrubbed), so
each starts a daemon from disk and loads the workspace again before it checks anything.

The plan's section 9 already says what it should be: CI is the same daemon. This is how.

## The warm path

A machine that stays, with a daemon that stays. **No VM is made and no snapshot is restored
for a job.**

- **macOS:** the daemon on the Mac runner itself.
- **Linux:** the same on one long-lived ix machine (not an ix runner fork).

The daemon holds main's state: the store, the image, the kept front end, the tests' kept
passes, the test fork servers. A job is:

| step | what | budget | today, and what closes it |
|---|---|---|---|
| pick up | the runner takes the job | 2 s | GitHub's own; gone with the webhook (below) |
| fetch | `git fetch` of the commit, checkout in the machine's clone | 1 s | built (`warm_ci.py`) |
| view | the watcher reports the files that differ; a new root | 0.1 s | exists; one clone, so one job at a time |
| check | `blaze check --workspace` of what the change reaches | 5 s | the client's own overhead (part A of lane fast-noop-1110), then T1/T2: work in proportion to the change |
| reached tests | `blaze test --reached`: tests whose executed functions and reads moved | 8 s | exists; holds for a small change once the check does |
| report | the job's status, the steps' times | 0.5 s | built |
| **total** | | **under 17 s** | |

Main's state is refreshed by the same job on each push to main: the daemon then holds main,
and the next pull request differs from it by its own files. A pull request checked after
another's differs from that one instead; the store is content-addressed, so going back to a
tree it has seen recomputes nothing.

What must be true of blaze:

- **The client costs milliseconds** (T5): no wait on the history thread, no second round trip.
- **The daemon stays up.** A restart (an update of blaze, a reboot) is the one cold job; T6
  (a restart under 1 s, [restore](restore.md): the front end restored, not lowered again)
  makes that one cheap too.
- **Work in proportion to the change** (T1 to T4): a check of the whole workspace after a
  small edit reruns that edit's queries.
- **A selection that fails is a failure.** `blaze test --reached` reports every test, run or
  kept; a run with no result is refused by the job, never read as a pass.

## Trust

The warm daemon is shared, so only trusted code runs on it: **branches of the repository
itself**. What blaze's sandbox covers, and what it does not:

- **Covered:** build scripts and proc macros, always (AGENTS.md `sandbox-everything`,
  `proc-macros-on-wasm`): a build script writes only its `OUT_DIR` and has no network; a proc
  macro is wasm in Wasmtime. Tests the daemon runs under `--reached` run in the sandbox too.
- **Not covered:** a test that runs outside the sandbox (docs/cargo.md, "Which tests run":
  it runs every time and is named), `blaze run`, and anything a test does that the sandbox
  allows. A test is arbitrary code as the runner's user, beside a daemon and a store every
  later job reads. Nor does the sandbox bound what a job costs the shared machine.

So **a pull request from a fork never runs on the warm daemon.** It runs on a fresh ix
snapshot per job (`ix new <snapshot-id>` of main's last green state, removed after), with no
secret on the machine: what `blaze.yaml`'s ix runners already are. Its check alone (build
scripts and proc macros, sandboxed) could move to the warm daemon later; its tests stay on
the snapshot. The warm machine holds no secret either: the job's token is read-only and is
kept from blaze's environment.

## What is built

- `harness/warm_ci.py` (one typed JSON input): fetch, check, reached tests, each timed and
  bounded, a result file and a summary. It refuses a missing clone unless told to make it, a
  step past its bound, a failed test, and a run with fewer results than `min_tests`.
- `.github/workflows/warm.yaml`: runs it on the machines the repository variable
  `BLAZE_WARM_RUNNERS` names, for pushes to main and the repository's own pull requests. Not
  part of `ci`, and nothing requires it, until its numbers are in and its failures are held
  to `harness/known-failures.toml`.

Not verified: neither has run (no warm runner is registered; this lane could run neither
Actions nor ix).

## Next

1. **Register the two machines** and read the steps' times from the job summaries.
2. **A view per pull request, no clone.** `crates/blaze-ci` (4,400 lines, no dependant,
   deployed nowhere) is this design's service: a signed webhook, the commit's tree applied to
   the daemon's VFS as ops on the base's root (a view, held open), one request per check,
   check runs posted. It removes the pick-up step and the one-job-at-a-time clone. Before it
   is deployed: it runs fork pull requests on the shared daemon (`service.rs` ignores
   `from_fork`), which the trust rule above forbids; route them to the snapshot path. Its
   self-snapshots and standby (`snapshot.rs`, `standby.rs`) are for the daemon's own recovery,
   not for jobs.
3. **The gate's build and tests leave `ci`** once the warm job has stood beside them green;
   the build from nothing stays in the train.
