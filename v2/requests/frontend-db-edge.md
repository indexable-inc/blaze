## frontend-db-edge: ide-db's `RootDatabase` is `base_db::Db` (to the coordinator, `crates/ide-db/`; 2026-10-29)

What: in `crates/ide-db/src/lib.rs`, replace `pub struct RootDatabase` (L82) and its impls
(`salsa::Database`, `Drop`, `Clone`, `Debug`, `SourceDatabase`, the `HirDatabase` wiring) with
`pub type RootDatabase = base_db::Db;`. Its inherent helpers (`included_files`, `included_by`,
`included_sources`, `sources_included_by`, `with_files` and the rest) move into a
`pub trait RootDatabaseExt` implemented for `Db`, so call sites stay, as the test databases'
helpers did (`docs/v2/plan/lanes/frontend/db.md`, "Test databases"). Its fields (`storage`,
`files`, `crates_map`, `nonce`, `pool`) are `Db`'s already.

Why: `HirDatabase` is sealed to its one impl for `Db`, so `RootDatabase` can no longer be one.
blaze-engine, blaze-cargo, load-cargo, blaze-wasm, blaze-doc and blaze-lints hold a
`RootDatabase` and now pass it where `&Db` is taken (the tool turned their
`&dyn HirDatabase`/`&dyn SourceDatabase` parameters into `&Db`). With the alias those calls
type-check unchanged until ide-db is removed at integration, and there is still one database
type.

Status: open

> frontend-db agrees: this is the bridge for ide-db's users outside the IDE crates (34 files in
> blaze-engine, blaze-doc, blaze-cargo, blaze-wasm, blaze-lints and load-cargo), and it adds no
> second database type. Two things to keep exact:
> - `Db` has no `Default` and no `Clone`, and `Db::new()` takes no arguments. A trait method
>   can't stand in for `RootDatabase::new(lru_cap)` or `RootDatabase::default()`, because
>   `Db`'s inherent `new` wins the path call. Those callers become `Db::new()` (a build) or
>   `Db::for_tests()` (a test). `with_files` keeps its name in `RootDatabaseExt`. The LRU
>   capacities `RootDatabase::new` set are an IDE knob with no place in the build's `Db`, so
>   they go with it.
> - A clone of the database stops compiling. In the IDE crates (`ide*`, `rust-analyzer`) that
>   is fine, because they are deleted at integration. On the build's path it isn't, and
>   frontend-db-edge fixes those (told): `blaze-engine/src/frozen.rs:317`, `outline.rs:389`,
>   `doctest.rs:1348`, `blaze-doc/src/collect.rs:36`, `places.rs:446` (`map_with(db.clone(), ..)`),
>   and `blaze-cargo/src/serve/job.rs:672, 1034, 1100` if `w.db` is a `Db` rather than an
>   `Arc`. `Db` is `Sync` and shared by every thread (db.md, "One database, shared by every
>   thread"), so each worker borrows `&Db` (`map_with(db.clone(), ..)` becomes `map(|x| ..)`
>   capturing `db: &Db`). Where a task must own its handle, use an `Arc<Db>`, never a fork.
> taken by requests (ide-db is requests'): child `requests-ide-db`; the outcome follows.
> applied by requests (child requests-ide-db): `pub type RootDatabase = base_db::Db;` plus `pub trait RootDatabaseExt` for `Db` (same names and signatures; no `new`, `Default`, `Clone`, `set_pool`, LRU knobs or `executions`), `apply_change.rs` and `ra_fixture.rs` as free functions, `symbol_index` batches on `Db::run_all`, prime caches on scoped threads borrowing `&Db`, `rayon` and `blaze-graph` out of the manifest; `with_files` stays `WithFixture`'s; what callers outside need (and two base-db asks: an inherent `Db::reload_included_file`, and `Db::new()` making the empty roots) is in `v2/requests/requests.md`, "From requests-ide-db". Nothing built.
