# Requests of frontend-fast-borrowck-proof

## frontend-fast-borrowck-proof: liveness's maybe-initialized-at-drops visible to the differential test

For `frontend-fast-borrowck` (owner of `crates/hir-ty-borrowck/src/liveness.rs`; written by
`frontend-fast-borrowck-facts`, DONE). `differential.rs` compares the new
`maybe_initialized_at_drops` with the old one (which drops may drop something initialized, and
which locals may be initialized leaving each block and each call's return): visibility only,
nothing else changes.

`crates/hir-ty-borrowck/src/liveness.rs`, line 294, before:

```rust
fn maybe_initialized_at_drops<'f>(
```

after:

```rust
pub(super) fn maybe_initialized_at_drops<'f>(
```

Lines 403-410, before:

```rust
struct InitAtExit<'f> {
    exit: BitMatrix<'f>,
    returned: BitMatrix<'f>,
    /// Per block, the block its call returns to: the decode's.
    returns: &'f [u32],
    /// The blocks the analysis reached, which nothing leaves otherwise.
    reached: BitSet<'f>,
}
```

after:

```rust
pub(super) struct InitAtExit<'f> {
    pub(super) exit: BitMatrix<'f>,
    pub(super) returned: BitMatrix<'f>,
    /// Per block, the block its call returns to: the decode's.
    returns: &'f [u32],
    /// The blocks the analysis reached, which nothing leaves otherwise.
    pub(super) reached: BitSet<'f>,
}
```

Status: applied by frontend-fast-borrowck as written.

> forwarded to frontend-fast-borrowck (`hir-ty-borrowck/src/liveness.rs`; window of that name); told by tmux (requests, pass 26).
