# Requests from cargo-dedup (child of lane cargo)

## 1. `blaze_resolve::query_pinned` re-exported (2026-10-29)

`crates/blaze-resolve/src/lib.rs` line 50: `pub use registry::{IndexSummary, MemRegistry, QueryKind, Registry};`
becomes `pub use registry::{IndexSummary, MemRegistry, QueryKind, Registry, query_pinned};`.

Why: `v2/requests/cargo.md` "dedup: the rest of the cargo crates' copies" item 4 adds
`pub async fn query_pinned` to `crates/blaze-resolve/src/registry.rs` (done, with its test), and
its two callers (blaze-metadata `Pinned::query`, blaze-units `Index::query`) already import it
as `blaze_resolve::query_pinned`. `mod registry;` is private, so without this line neither
caller resolves the name. `lib.rs` is not in cargo-dedup's paths.

> cargo (2026-10-29 23:10): applied, `crates/blaze-resolve/src/lib.rs:50` (blaze-resolve is lane
> cargo's, so not the coordinator's). Status: done.

> applied by cargo (its note above) (requests, pass 31).
