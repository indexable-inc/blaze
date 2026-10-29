# Rival score: our blaze, Astral's fork and cargo (2026-10-28)

Machine: GitHub ubuntu-24.04 hosted runner (x86_64, 4 vCPU, 16 GB, shared), 4 cores used. 5 timed builds per phase, each blaze arm on its own beside cargo, one daemon at a time (harness/rival.py). Commits: ours (blaze) `a16b45c047`, astral (astral) `70c9a8178b`, cargo (cargo) `nightly-20`.

Seconds, median. cold = first build + the wait until the daemon is ready. tests = the second `test` run. gap = ours / fastest other arm: above 1.00x someone beats us. A cell with `!` was built by cargo (the daemon declined); `x` failed.

| project | phase | ours | astral | cargo | gap |
|---|---|---:|---:|---:|---:|
| ripgrep | cold | 24.75 | 30.94 | 9.88 | 2.51x |
| ripgrep | no-op | 0.01 | 0.03 | 1.88 | 0.41x |
| ripgrep | body | 0.21 | 0.10 | 1.91 | 2.10x |
| ripgrep | signature | 0.16 | 0.19 | 2.34 | 0.83x |
| ripgrep | tests | 9.92 (1165 ok, 0 failed) | 8.08 (1165 ok, 0 failed) | 8.60 (1165 ok, 0 failed) | 1.23x |
| ruff | cold | 528.01 | 332.35 | 199.13 | 2.65x |
| ruff | no-op | 0.06 | 0.05 | 0.24 | 1.34x |
| ruff | body | 75.22 | 0.25 | 7.24 | 299.70x |
| ruff | signature | 27.38 | 122.14! | 66.32 | 0.41x |
| ruff | tests | 13.94 (864 ok, 0 failed) | 7.08 (864 ok, 0 failed) | 1.66 (864 ok, 0 failed) | 8.38x |
| ty | cold | 537.00 | 421.99 | 237.67 | 2.26x |
| ty | no-op | 0.07 | 0.05 | 0.22 | 1.35x |
| ty | body | 146.68 | 0.35 | 5.07 | 422.72x |
| ty | signature | 27.31 | x | x |  |
| ty | tests | 17.59 (183 ok, 0 failed) | x | x |  |
| uv | cold | x | 452.07 | 181.16 |  |
| uv | no-op | x | 0.04 | 0.38 |  |
| uv | body | x | 11.88 | 4.50 |  |
| uv | signature | x | 12.04 | 34.49 |  |
| uv | tests | x | 9.42 (233 ok, 0 failed) | 1.24 (233 ok, 0 failed) |  |

## Wrong output or tests (against cargo)

- ripgrep / ours: tests: tests differ: blaze only [(('crates/ignore/src/types.rs - types (line 16)', 'ran'), 1), (('crates/ignore/src/types.rs - types (line 51)', 'ran'), 1), (('crates/ignore/src/types.rs - types (line 34)', 'ran'), 1)], cargo only [(('crates/ignore/src/types.rs - types (line 17)', 'ran'), 1), (('crates/ignore/src/types.rs - types (line 52)', 'ran'), 1), (('crates/ignore/src/types.rs - types (line 
- ripgrep / astral: tests: tests differ: blaze only [(('crates/globset/src/lib.rs - (line 18)', 'ran'), 1), (('crates/globset/src/lib.rs - (line 34)', 'ran'), 1), (('crates/globset/src/lib.rs - (line 50)', 'ran'), 1)], cargo only [(('crates/globset/src/lib.rs - (line 19)', 'ran'), 1), (('crates/globset/src/lib.rs - (line 35)', 'ran'), 1), (('crates/globset/src/lib.rs - (line 51)', 'ran'), 1)]
- ruff / ours: tests: tests differ: blaze only [(('token::tokens::tests::tokens_after_offset_inside_token', 'ran'), 1), (('token::tokens::tests::tokens_before_offset_inside_token', 'ran'), 1), (('token::tokens::tests::tokens_in_range_end_offset_inside_token', 'ran'), 1)], cargo only [(('token::tokens::tests::tokens_after_offset_inside_token - should panic', 'ran'), 1), (('token::tokens::tests::tokens_before_offs
- uv / astral: tests: tests differ: blaze only [(('crates/uv-pep508/src/verbatim_url.rs - verbatim_url::VerbatimUrl (line 64) - compile fail', 'ran'), 1), (('crates/uv-pep508/src/lib.rs - Pep508Url (line 321) - compile fail', 'ran'), 1)], cargo only [(('crates/uv-pep508/src/lib.rs - Pep508Url (line 324) - compile fail', 'ran'), 1), (('crates/uv-pep508/src/verbatim_url.rs - verbatim_url::VerbatimUrl (line 67) - c

## Failed

- ty / cargo: build failed: 'a> fn(&'a path::SearchPath, ()) -> bool {path::SearchPath::is_standard_library}`
- ty / astral: build failed: 'a> fn(&'a path::SearchPath, ()) -> bool {path::SearchPath::is_standard_library}`
- uv / ours: cold build failed: s)

Raw results: raw/<project>.json. Every command: commands.txt. Machine: host.json.

## Notes (lane astral)

- Run: https://github.com/indexable-inc/blaze/actions/runs/40767917539 (the `rival` artifact, copied as is).
- ty signature: the edit gave `is_standard_library` a second parameter while it is also passed as a path
  (`.is_some_and(SearchPath::is_standard_library)`), so cargo and Astral rightly reject it (E0277). Ours built it:
  a wrong verdict, filed on #84. rival.py now edits `is_site_packages` and flags an arm that builds what cargo rejects.
- The body edit gaps (ruff 75 s, ty 147 s, against Astral's 0.25-0.35 s and cargo's 5-7 s) are the biggest rows and a
  P0 (slower than cargo); the next run keeps each edit build's `BLAZE_STATS` in raw/ to say where the time goes.
- "tests differ" rows are doctest names whose line is one off from rustdoc's (`types (line 16)` for `(line 17)`)
  and `#[should_panic]` tests named without rustdoc's ` - should panic`: the totals agree.
- uv / ours: const-random-macro's wasm module did not resolve (#1402), fixed by #1406.
