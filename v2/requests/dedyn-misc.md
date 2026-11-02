# Requests from dedyn-misc

## dedyn: tools/v2/hot_lint.py: don't match inside string literals (2026-10-29)

What: in `lint_file` (`tools/v2/hot_lint.py`), blank out the contents of string literals
(`"..."` with `\"` escapes, and `r"..."` / `r#"..."#`) on each line before the `CHECKS` regexes
run, keeping the line's length so columns stay right.

Where: `tools/v2/hot_lint.py`, `lint_file`, before `for name, pattern, why in CHECKS`.

Why: `crates/blaze-lints/src/registry/clippy.rs:456` is flagged for `dyn`, but the match is
clippy's description of `coerce_container_to_any` ("coercing to `&dyn Any` ..."), text about
the Rust being linted, in a file `harness/clippy_registry.py` writes ("do not edit by hand"),
so it can't carry a `// dyn:` line. The same blind spot flags `format!`/`to_owned` named in
strings elsewhere. With strings blanked, the dyn count for every dedyn-misc crate is 0.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (requests, pass 25).
