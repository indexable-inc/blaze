---
status: plan
summary: blaze v2: cargo.
date: 2026-10-29
---
# blaze v2: cargo

See [plan/process/building.md](../process/building.md), section 18.

## Done when

- the workspace load and unit graph are queries; nothing reloads on start
- blaze build, run, test, check and cargo blaze match cargo's flags, output and exit codes
- std built through the v2 pipeline into the base layer
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
