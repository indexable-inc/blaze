---
status: plan
summary: blaze v2: the targets, measured on one warm VM.
date: 2026-10-29
---
# Targets

Measured on one warm VM (Graviton 5 or EPYC Turin, all cores), and on an M-series Mac for
macOS. "Edit to result" is the time from a save to a running program or a test result.

| | today (main) | v2 target |
|---|---|---|
| body edit, edit to result | ~0.29 s (nushell) | 10-30 ms |
| signature edit | ~0.31 s, 6,269 roots asked | proportional to the items whose facts changed |
| formatting or comment edit | recompiles the file's functions' places | compiles nothing |
| rename or move of an item | recompiles dependents | compiles nothing |
| no-op build | 11 ms (ripgrep) | under 5 ms |
| daemon start, warm | 35 s (nushell); up to 238 s in CI | under 1 s (mmap the image) |
| blaze's own CI, small PR | 30 min+ | under 2 min, the changed code and the tests it reaches |
| cold build of blaze | ~12 min of mostly serial codegen | bounded by the critical path, all cores busy |
| memory | runners OOM; cold ripgrep 2.0 GB live vs cargo's 0.56 GB (#806); nushell 12.5 GB (#734) | never OOM: admission against a budget; peak at most cargo's |
| body edit of a big top crate | ruff 75 s, ty 147 s; cargo 7 s; Astral's fork 0.3 s (#1450) | under 50 ms |
| dev-build code speed | ripgrep's dev build runs 5.8x slower than cargo's debug build (#1404) | at least as fast as cargo's debug build |
