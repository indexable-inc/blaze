# Requests of `diagnostics-emit` (child of lane `diagnostics`)

## Root `Cargo.toml`: a workspace dependency `memchr = "2.8.1"`

- What: add `memchr = "2.8.1"` to `[workspace.dependencies]`. `Cargo.lock` already has 2.8.1
  through other crates, so nothing new is downloaded.
- Where: used by `crates/blaze-diagnostics` (`memchr.workspace = true` in its `Cargo.toml`,
  already written) in `src/emit.rs`.
- Why: placing a span needs the start of each line of its file and each `\r\n` taken out (rustc's
  `analyze_source_file`, which scans 16 bytes at a time with SSE2). `memchr_iter` and
  `memmem::find_iter` do the same scan, choosing AVX2, SSE2 or NEON at run time, instead of a
  loop over bytes.

> applied: `memchr = "2.8.1"` in the root `[workspace.dependencies]` (already in `Cargo.lock` at 2.8.1).
