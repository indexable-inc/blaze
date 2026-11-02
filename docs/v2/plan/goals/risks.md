---
status: plan
summary: blaze v2: risks, and what is left out.
date: 2026-10-29
---
# Risks

- **One huge PR**: review and merge conflicts with main. Mitigation: main merged into
  `blaze-v2` daily by the coordinator; the PR's review is the test suite plus the per-lane design
  notes, not a line-by-line read.
- **Facts too fine**: tracking costs more than it saves. Mitigation: the oracle measures both
  under- and over-invalidation; any fact kind can be coarsened without changing correctness.
- **ix VMs that don't place or stay up**: the `sansio` lane couldn't measure on 16 cores. A lane
  without a VM says so in its HANDOFF and stops on measurement, never runs on the Mac or AWS.
- **Front-end churn**: `hir-*` changes (IDs, SoA, fingerprints) touch the most code. They keep
  rustc's semantics; the differential corpus is the check.
- **Testing all at once**: the first full run will fail in many places together, and failures
  that cross lanes take longest. Mitigation: the oracle and corpus exist before integration
  (step 2), so failures come with the fact or crate that broke, and the explainer says why.
- **A chain of lanes** (format, then db, then items, then codegen, then link): downstream lanes
  build against the interfaces and their own stubs, not against finished upstream work.
- **Toolchain bumps get cheaper, not costlier**: with item hashes, a new nightly's std
  invalidates only the std items that changed.

## Left out

**Left out of v2 for now**: #1448 effect-generic I/O, #1444 tests as WASI modules, #868
purity facts, #859-#866 the hermetic build graph for build scripts, #104 mutation testing, #226
the Bazel remote execution API.
