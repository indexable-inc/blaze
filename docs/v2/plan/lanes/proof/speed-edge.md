---
status: plan
summary: Index of speed-link-edge's work on the proof lane (blaze-v2-proof, harness/v2), with where the before/after is.
date: 2026-10-29
---
# speed-edge: the proof harness

Part of [proof](../proof.md). The details, with the before/after table, are in
[speed-link-edge-proof](speed-link-edge-proof.md).

- Finding the first difference between two outputs: AVX-512BW, AVX2, SSE2 or NEON, chosen at run
  time and tested against scalar at every offset, over mapped files.
- Traces: one content-hash key table, joined by merge.
- The Python harness:
  - digests are mapped, computed in parallel and cached by content;
  - cargo's and rustc's answers are kept by everything they read, so a rerun only runs what
    changed;
  - the perf budgets use hyperfine-style statistics (warmup, outliers, a distribution-free
    interval).
- Still short: the Python harness hashes with SHA-256 until the `blake3` package is added
  (`v2/requests/speed.md`, `speed-link-edge-proof`).
