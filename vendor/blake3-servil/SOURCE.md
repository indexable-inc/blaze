# blake3-servil

- Upstream: https://github.com/johnservil/BLAKE3, commit
  `c6bd679a0911cc233cb28297f2b25875381215a1` (package `blake3-servil`
  0.8.0+b3c5efc7c975d903762dea809066f7c0a0603861), the rev the root manifest pinned as a git
  dependency before this copy.
- Licence: CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception (LICENSE_*).
- Why vendored: `tools/nix/unit_graph.py` takes only registry and path sources, and
  `blaze_ids::b3` builds on this crate (docs/v2/plan/process/decisions.md).
- Kept: what the manifest's `include` ships (`src/`, `c/`, `build.rs`, README.md, CHANGELOG.md,
  QUALITY.md, the licences, `tools/require_nextest.rs`), and `tests/`, `clocks/` and
  `test_vectors/`, which the manifest's `[[test]]` targets and path dev-dependency name. The root
  manifest excludes this directory from the workspace, so its own tests are not built.
- Left out: `.git`, `.github`, `.config`, `b3sum/`, `capi/`, `c2sp/`, `supercop/`, `docs/`,
  `media/`, `host-lab-reports/`, the rest of `tools/` and the repository's notes.

No changes: every kept file is upstream's at that commit (`diff -r` against cargo's checkout).
