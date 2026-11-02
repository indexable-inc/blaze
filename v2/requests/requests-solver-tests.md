# requests-solver-tests: requests outside `crates/hir-ide/src/tests/`

## 1. `tools/v2/hot_lint.py`: `crates` can't be given

- Where: `tools/v2/hot_lint.py:39`, `class Input`.
- Old: `crates: list[str] = field(default_factory=lambda: list(HOT))`.
- New: `crates: tuple[str, ...] = HOT`, with `from dataclasses import dataclass` (`field` is no
  longer used). `main` only iterates `cfg.crates`, so a tuple works there.
- Why: `typed_input._check` reads `tuple[X, ...]` but not `list[X]`. So
  `{"root": ".", "crates": ["hir-ide"]}` fails with
  `TypeError: typed_input: crates has a type it does not read: list[str]`, and the narrowed run
  the coordinator asks every lane to do cannot run at all. Only the default run, with no
  `crates`, works.

> forwarded to the coordinator (`tools/`; the same as coordinator.md's hot_lint item), listed in requests' report.
