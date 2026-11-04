---
status: record
summary: The v2 lanes' requests filed under `speed-link-edge-elf-input` (what one lane asked of the code another owns), each with its answer.
date: 2026-10-29
---
# Requests from speed-link-edge-elf-input

- `Names::reserve(bytes)` in `crates/blaze-link/src/model.rs`: filed as request 18 in
  `docs/v2/requests/link.md` (what, where, why there), told to `requests-link-fuzz-core-model`, which
  holds `model.rs` now. Optional: without it one rare path (an ELF archive with no symbol
  table) grows its name arena by doubling.
- For lead `link` and `requests-link-fuzz-core-stage`, about edits already in the tree: the
  previous run of this agent added `blaze-alloc-count.workspace = true` to
  `crates/blaze-link/Cargo.toml` `[dev-dependencies]` and the counting `#[global_allocator]`
  under `#[cfg(test)]` to `crates/blaze-link/src/lib.rs`, outside its paths. Both are the
  pattern `blaze-alloc-count` documents. The `requests-link-fuzz` rules now rely on them
  ("already installed ... don't install another"), so keep them. Nothing to apply.
