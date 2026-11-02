# Requests from usercode-hot-sandbox

## 1. `blaze-alloc-count` as a dev-dependency of `blaze-cargo`

- **What**: in `crates/blaze-cargo/Cargo.toml`, under `[dev-dependencies]`, add one line:

  ```toml
  blaze-alloc-count.workspace = true
  ```

  `blaze-alloc-count` is already in the root `[workspace.dependencies]`.
- **Where it is used**: the allocation-budget tests of the sandbox's hot paths
  (`crates/blaze-cargo/src/sandbox.rs` `mod tests`, `crates/blaze-cargo/src/sandbox/linux.rs`
  `mod tests`). `sandbox.rs`'s `mod tests` installs the counting allocator for the crate's test
  binary (`#[global_allocator] static ALLOC: blaze_alloc_count::Counting`). It forwards to
  `System`, which is the allocator that binary uses today, so no other test changes.
- **Why**: docs/v2/plan/goals/principles.md, "By test": each hot path has an allocation-budget
  test. Without the line the crate's tests don't build.
- **Note for whoever adds a second budget test elsewhere in `blaze-cargo`**: a test binary can
  have only one `#[global_allocator]`, and this one is in `sandbox.rs`. Don't add another: call
  `blaze_alloc_count::measure` from anywhere in the crate.

> forwarded to cargo (`crates/blaze-cargo/Cargo.toml`) (from requests, pass 16; told by tmux).
