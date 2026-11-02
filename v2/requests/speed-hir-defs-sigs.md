# Requests from speed-hir-defs-sigs

## `tools/v2/hot_lint.py`: `crates` as a tuple, so the script runs

What: `tools/v2/hot_lint.py`, `class Input` (~L37):

```python
# before
    crates: list[str] = field(default_factory=lambda: list(HOT))
# after
    crates: tuple[str, ...] = HOT
```

and drop `field` from `from dataclasses import dataclass, field` if nothing else uses it.

Why: every run stops before linting, with or without `"crates"`:
`TypeError: typed_input: crates has a type it does not read: list[str]`.
`typed_input._check` reads `tuple[T, ...]` and not `list[T]`, and a frozen dataclass wants an
immutable default anyway. `HOT` is already a tuple, and the loop only iterates over `cfg.crates`.
Status: open

> forwarded to the coordinator (`tools/`), listed in requests' report.
