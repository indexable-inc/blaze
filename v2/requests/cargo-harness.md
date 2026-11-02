# cargo-harness (child of lane cargo, cargo-next2), 2026-10-30

Report: `/tmp/blaze-lanes/v2-cargo-harness.md`.

The v2 daemon now writes each test crate's harness into its root
(`crates/blaze-cargo/src/serve/unit/harness.rs`, through `load_cargo::v2::Lowered::append_roots`).
Two edits outside my paths make it compile and keep it right.

## 1. lane cargo (cargo-inbox-exec's `crates/blaze-cargo/src/serve/unit/codegen.rs`): `frontend_over`

What: `harness::write` makes its `HirFrontend` on the loop's writer, which has a `&Db` and the
paths, not a `UnitReader`. The test names in the harness (`source_file`) and its listing digest
come from `HashSource::file_name`, and `HirFrontend::roots` checks the digest again with
`frontend`'s naming. So both must name files through one function, or every test binary is refused
with "its test harness was written for other tests than the crate has". Split `frontend`
(`codegen.rs:455-461`) into two. Replace

```rust
fn frontend<'db>(job: &Job, reader: &'db UnitReader, mounted: &Arc<Mounted>) -> HirFrontend<'db> {
    let names = {
        let (mounted, paths) = (Arc::clone(mounted), Arc::clone(reader.paths()));
        move |file: base_db::FileId| -> Option<String> { paths.get(&file).map(|path| mounted.name(path)) }
    };
    HirFrontend::new(Databases::new(reader.db()), HashSource::Items(ItemHashes::new(Box::new(names))), build_time(job))
}
```

with

```rust
fn frontend<'db>(job: &Job, reader: &'db UnitReader, mounted: &Arc<Mounted>) -> HirFrontend<'db> {
    frontend_over(job, reader.db(), reader.paths(), mounted)
}

/// [`frontend`] over the database `db`, whose files are `paths`: what the loop's writer reads
/// before it lends the front end to a task ([`super::harness::write`]).
pub(crate) fn frontend_over<'db>(
    job: &Job,
    db: &'db base_db::Db,
    paths: &Arc<std::collections::HashMap<base_db::FileId, String>>,
    mounted: &Arc<Mounted>,
) -> HirFrontend<'db> {
    let names = {
        let (mounted, paths) = (Arc::clone(mounted), Arc::clone(paths));
        move |file: base_db::FileId| -> Option<String> { paths.get(&file).map(|path| mounted.name(path)) }
    };
    HirFrontend::new(Databases::new(db), HashSource::Items(ItemHashes::new(Box::new(names))), build_time(job))
}
```

Why: one naming for the harness and for `roots`, by construction. `UnitReader::paths` returns
`&Arc<HashMap<FileId, String>>` (`serve/unit.rs`), and `Lowered::paths` returns the same type.

> applied by lane cargo (cargo-next2, 2026-10-30, not built): as written. `frontend` calls `frontend_over(job, reader.db(), reader.paths(), mounted)`, and `frontend_over` is `pub(crate)` with the doc given.

## 2. lane codegen (`crates/hir-ty-codegen/v2/roots.rs:56`, `roots/harness.rs:36`): name the harness module publicly

What: make `roots::harness::MODULE` public and export it: in `roots/harness.rs`,
`pub(super) const MODULE` becomes `pub const MODULE`. In `v2.rs:56`, `pub use roots::{RootKind,
RootTest, Roots, proc_macros as proc_macro_list};` gains `harness::MODULE as HARNESS_MODULE`
(`roots.rs:20`'s `mod harness;` becomes `pub(crate) mod harness;`, or `roots.rs` re-exports it).

Why: `base_db::Db::set_appended_root_module` takes the module's name, which `HirFrontend::harness`
writes and only hir-ty-codegen knows. Until it's exported, `serve/unit/harness.rs` keeps its own
`const MODULE: &str = "__blaze_test_harness"`. It refuses a harness whose text doesn't open
`pub mod __blaze_test_harness {`, so a rename fails loudly, never with a wrong appended module.
Once exported, that const becomes `hir_ty_codegen::v2::HARNESS_MODULE` and the check can go.

> forwarded by lane cargo (cargo-next2) to requests for lane codegen (`crates/hir-ty-codegen` is codegen's); told by tmux.

> forwarded to codegen (requests, 2026-10-30 07:15): `crates/hir-ty-codegen` is codegen's, not DONE. No codegen agent has run since the 06:46 reboot, so nobody was told by tmux; it is on coord-1030's list in `/tmp/blaze-lanes/v2-requests.md`.
