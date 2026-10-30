# Const-eval speed (issue #867)

How fast blaze's MIR evaluator runs real crates inside a `const`. Three smoke cases each compute something heavy in a constant. Each has a twin that computes the same thing at run time (`--no-default-features`):

- `ce_sha256`: sha256 of 1 MB (the `sha2` crate).
- `ce_toml`: parses a TOML document (the `toml` crate).
- `ce_inflate`: inflates a deflate stream (`miniz_oxide`).

`const_heap` builds a long heap-allocated list in a constant, which tests the evaluator's heap and drop glue.

Times are seconds for one whole `blaze run --release`, build plus run. Each run gets a fresh `BLAZE_HOME` and a fresh copy of the case, so nothing is cached. Arms alternate (A B, then B A) on one ix VM; see `host.json`. Every run's stdout matched the case's `expected.txt` unless a row says otherwise.

## Finding: a frame cloned its whole MIR body (fixed in #1544)

`perf record --call-graph dwarf` of `ce_sha256`'s const build put about 85% of the evaluator's time in `interpret_mir`'s `let body = locals.body.clone();`. That line ran on every frame entry and every resume after a call, and deep-cloned the `&'db MirBody` behind it each time. The time went to `Vec<BasicBlock>::clone`, to dropping the copy, and to the atomic refcounts inside. #1544 copies the reference instead. `raw-bodyclone.txt` (script `ac.sh`): A is main 4519ddc7c8 plus #1532, C is A plus #1544.

| case | A | C | run-time twin |
|---|---|---|---|
| ce_sha256 | 360, 363 | 71, 71 | about 27 |
| ce_toml | 133, 134 | 60, 61 | about 37 |
| ce_inflate | 611 | 119 | about 23 |

Take away the roughly 20 s that a build costs without the constant. Const evaluation itself drops from about 335 s to 45 s for sha256, from 590 s to 95 s for inflate, and from 95 s to 23 s for toml. It is still a few times slower than compiling the same code and running it.

## Earlier: main against #1473 (inline const cached per evaluation)

`raw-main-vs-1473.jsonl` (script `bench.py`): A is main before #1473, B is A plus #1473.

| case | A const | B const | A rt | B rt |
|---|---|---|---|---|
| ce_sha256 | 353.8, 355.0 | 362.5, 359.4 | 26.9, 26.9 | 29.1, 27.5 |
| ce_inflate | 652.4, 619.9 | 617.5, 610.0 | 22.0, 23.0 | 24.4, 23.4 |
| const_heap | 27.4, 27.0 | 28.2, 25.7 | | |
| ce_toml | 62.2, 62.4 (exit 101) | 60.4, 63.5 (exit 101) | 37.4, 38.4 | 39.6, 35.9 |

#1473 is within noise for sha256 and slightly faster for inflate. In both arms, `ce_toml`'s constant failed on aarch64 with "Unknown syscall id 278": the evaluator knew `getrandom` only by its x86-64 number (318). #1532 picks the number by `target_arch`, and in `raw-bodyclone.txt` ce_toml's constant evaluates correctly.
