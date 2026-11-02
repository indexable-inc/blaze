---
status: plan
summary: blaze v2: the agent API and the VM as CI.
date: 2026-10-29
---
# blaze v2: ci

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 10. CI on the VM

- The project's warm VM **is** the CI. A webhook for a push or PR opens a view on the running
  daemon and builds its diff: no runners, no cache transfer, no workspace load.
- Results go back to GitHub as check runs, so required checks and `land.py` keep working.
- **Release train**: one at a time on the VM; a newer tag replaces the waiting one (latest wins).
- macOS and Windows checks from their own warm machines, the same way.
- Fork PRs run on the shared machine, so every build script, proc macro and test runs in a
  per-process sandbox and no secret is readable inside one.
- **Only the daemon writes the store.** User code's outputs are hashed and checked by the
  daemon before they're stored, so a sandbox escape can't poison what other views read. Each
  tenant has its own store; only the read-only base layer of public dependencies is shared.
- **One shared memo table means one wrong result is wrong everywhere**, so the VM runs the
  invalidation oracle continuously in production: it rebuilds sampled views from scratch and
  compares them; a mismatch stops the affected results and names the fact that missed.
- **No single point of failure**: the VM snapshots itself, and a standby resumes from the
  newest snapshot.
- First step: one persistent ix VM where each Actions job only sends a request to the warm
  daemon (the ix-runners App can place jobs there). Then the VM posts check runs itself and
  Actions is dropped for these checks.

## Issues this lane takes in

- **#277 the agent API** is the only interface (section 1). (`v2-ci`, coordinator)

## Done when

- the agent API's every request answered, schema versioned, the CLI a client of it
- the webhook service, check runs, the latest-wins train, the production oracle, snapshots
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
