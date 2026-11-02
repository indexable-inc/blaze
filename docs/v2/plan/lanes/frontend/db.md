---
status: plan
summary: blaze v2's front end has one concrete database, base_db::Db, passed as &Db to every query (rustc's TyCtxt design on salsa), so a query call is static and a memo hit is a chain of loads with no call through a vtable or function pointer (frontend.md item 15).
date: 2026-10-29
---
# blaze v2: one concrete database, `&Db` (frontend-db)

Design notes of lane `frontend-db` for item 15 of [the front end's design](../../design/frontend.md):
every query went through `&dyn HirDatabase` or `&dyn SourceDatabase` (2,745 uses in the
front-end crates, the IDE crates aside), so a call per query went through a vtable and a memo hit
could not inline.

## What a memo hit costs today

A hit of `db.infer(def)` with `db: &dyn HirDatabase` makes these calls through pointers before it
reads the memo:

1. the trait method itself (`HirDatabase::infer`, a vtable slot);
2. `as_dyn_database()` inside `salsa::attach` (a vtable slot);
3. `zalsas()` in the tracked function's body (a vtable slot);
4. `task::current_depth()`, once in `attach` and once in `TaskLocals::current` under
   `zalsa_local()`: a `OnceLock<fn() -> usize>` hook (`salsa::set_task_depth`) that calls
   `blaze_graph::pump::depth`.

Each is a call the optimizer can't see through, so none of the hit inlines into the caller: the
caller spills its registers around four to five indirect calls to read what is, at the end, a
chain of loads.

## The speed of light of a hit

What a hit has to do is fixed by salsa's memo layout (lane db's columns): read the thread's
attached database and task (2 thread-local loads), the ingredient's cached index (1 load and a
compare with the storage's nonce), the memo's page and slot (3 to 4 dependent loads), compare its
`verified_at` with the current revision (2 loads), and record the read on the task's query stack
(2 to 3 stores). That is about 15 loads with a dependent chain of 5 to 6, all L1 hits for a hot
query: 25 to 30 cycles, 6 to 8 ns at 4 GHz. The target is that a hit compiles to exactly that,
inlined into its caller, with no call of any kind on the path (the miss paths stay out of line,
as salsa has them: `fetch_unclaimed`, `fetch_cold`). The four indirect calls above are what is
between today and that bound; with them gone, nothing else is on the path. The check for a
stop (`stop_readers`, below) adds one load from the `Runtime` the revision check already reads
and a branch that is never taken: about 1 cycle, inside the 10%.

The state of the art is rustc's query system: `tcx.type_of(def)` is a static call of
`query_get_at`, whose hit is a `VecCache` probe (2 to 3 dependent loads) and a dependency-graph
read through the implicit context (a thread-local load and a push). salsa's hit adds the
`verified_at` check, which rustc doesn't need because its revisions are sessions. We match
rustc's shape exactly: a concrete context, static query calls, providers only on the miss.

## The options weighed

The query traits were declared bottom-up (base-db, hir-expand, hir-def, hir-ty) and the struct that
implemented them lived above them all (`load-cargo/src/v2/db.rs`), so no lower crate could name the
concrete type.

- **Generic `db: &impl HirDatabase` everywhere.** Static calls once monomorphized over the one
  type, and the machine code is the same size (one instance of each function). But every generic
  function is code generated in the crate that names the type, so the front end's whole
  codegen moves into one leaf crate: about 320,000 lines (hir-def 71k, hir-ty 77k,
  hir-ty-codegen 78k, hir-ty-mir 29k, hir 20k, hir-ty-borrowck 17k, hir-expand 13k, the rest
  smaller). That code is then generated serially in one crate instead of in ten in parallel, and
  again on every change to that crate. It also infects every type that holds a database:
  `DbInterner<'db>` becomes `DbInterner<'db, D>`, and with it rustc_type_ir's `Interner` impl and
  every solver type, inference context and MIR builder. It costs compile time and adds a type
  parameter everywhere, and runs no faster than a concrete type.
- **rustc's `TyCtxt<'tcx>`**: one concrete `Copy` context over arenas, queries as methods, and
  providers registered by function-pointer tables that each crate fills in. This is the
  reference design for a compiler. On salsa, `&'db Db` already *is* that context: one pointer,
  `Copy`, with salsa's memos as the arenas and `'db` in the role of `'tcx`. A newtype around it
  would add nothing.
- **The concrete `Db` declared in the lowest crate.** This is rustc's design realised on salsa.
  base-db declares `Db` and its storage. salsa registers ingredients by type when the storage is
  made (`register_jar!`, collected across the whole binary), so storage for every higher crate's
  queries exists without base-db naming any of them. Each crate declares its queries as tracked
  functions over `&Db`.

**Chosen: the concrete `Db` in base-db, which is rustc's `TyCtxt` design on salsa.** salsa
doesn't make it worse than rustc's own: a query is a free function or a method of an ID type
(`krate.data(db)`, `module.def_map(db)`) rather than a method of the context, which is the same
static call. Its providers are needed only where a query's code is above the crate that has to
call it, which in this tree is the MIR queries.

## The design

### `base_db::Db` (`crates/base-db/src/db.rs`)

One `#[salsa::db]` struct: its storage, the `Files`, the `CratesMap`, the nonce, how
`run_all` runs (the process's pool, or here in a test `RunOrder`) and, in a test database
only, salsa's event log. What `SourceDatabase` had as trait methods are inherent methods:
`file_text`, `source_root`, `file_source_root`, the setters, `resolve_path`, `included_file`,
`include_source`, `env_is_read`, `env_is_unset`, `run_all` (generic over its closure, so the
task is a static call too), `nonce_and_revision` and `line_column` (on base-db's
`line_index`). `Db::new()` is a build's database and `Db::for_tests()` a test's.

`included_file`, `include_source` and the `env_is_*` notes are ported from ide-db's
`RootDatabase`. The v2 database in load-cargo had dropped them to the trait's defaults (do
nothing, `None`), so `include_bytes!` read nothing and `env!` reads went unrecorded. With one
type there is one implementation, which also means no test database can differ from a
build's.

`SourceDatabase` is deleted. `&dyn SourceDatabase` becomes `&Db` everywhere.

### `HirDatabase`: an extension trait with one impl

hir-ty's queries can't be inherent methods of a type base-db declares. `HirDatabase` stays as the
name callers import, but it is now a sealed extension trait implemented once, `impl HirDatabase for
base_db::Db`, in hir-ty. A call `db.ty(def)` on `db: &Db` resolves statically to that impl, and
every method is `#[inline]` over the tracked function. The trait gets an associated const
(`const NOT_DYN: ();`), which makes it not dyn-compatible: `dyn HirDatabase` no longer compiles,
so it cannot come back. The `hir_database_is_dyn_compatible` test is deleted, and no test
replaces it: the const enforces it, and hir-ty keeps `doctest = false`.

`as_dyn()` is deleted (35 uses become `db`).

### Providers, for the queries hir-ty reads but can't compute

The MIR queries (`mir_body` and its closure, coroutine, runtime, optimized and monomorphized
forms), `interpret_mir`, `opaque_of_impl` and `type_name` are read from hir-ty (const eval,
layout of coroutines, the solver), hir-ty-borrowck and blaze-items. Their code is in hir-ty-mir
and the display code above it. Today these are required trait methods that `impl_hir_database!`
wires up at each concrete database.

The rustc way to do this, and the one chosen: each such query is declared, with its memo, in
hir-ty (the crate of its key and value types: `MirBody`, `MirLowerError` and `MirOptLevel` are
hir-ty's), as a tracked function over `&Db`. Its cycle result (`Err(MirLowerError::Loop)`) is
declared there too, and its body is one call through a table of `fn` pointers,
`hir_ty::providers::Mir`. hir-ty-mir registers its one table at link time
(`inventory::submit!`, the mechanism salsa's `register_jar!` already uses for every ingredient
in this workspace), and hir-ty reads it once, on the first miss, into a `OnceLock`; `type_name`'s
table, `hir_ty::providers::Display`, is registered the same way by the display code. So a hit
from any crate is static, and only a miss calls through the table, as only a miss calls
`execute` in rustc; and nothing has to be installed where a `Db` is made, so no maker (a build,
a test, a tool) can forget to. A binary that links no table, or two, panics on the first miss,
naming the query and the crate that registers it: there is no default body to fall back to. A
crate whose tests read MIR links one with `use hir as _;` (or `use hir_ty_mir as _;`). The
registration is a `#[used]` static, which rustc links from every rlib in the graph whether or
not anything names it (since 1.62, rust-lang/rust#95604: the `symbols.o` that references each
`#[used]` symbol), so linking the crate is enough; blaze's own linker keeps the same rule, as
salsa's ingredients already need. hir-ty's own unit tests link no table and miss no MIR query.
An `extern "Rust"` symbol per provider, defined in hir-ty-mir, was the other way to need no
install call: a direct call and a link error when it is missing, but every hir-ty test binary
would then fail to link.
`opaque_of_impl`, `interpret_mir` and `type_name` are not memoized, so each is one call through
the table, made from inside a miss of its caller's own query.

rustc's `Providers` is a struct filled in by each crate's `provide(&mut providers)`, called from
`rustc_interface`. Here no one place calls them all; registering at link time keeps the one
table without that place, and the check that there is exactly one turns a missing or doubled
crate into an error that names it.

### salsa: tracked functions over a concrete database

The salsa fork's tracked-function macro takes `db: &dyn Trait` only. It is extended to take
`db: &Db` for a concrete `Db` with `#[salsa::db]` on the struct: `DbView = Db`, and the generated
`fn`s take `&Db` and `&mut Db`. The struct's `#[salsa::db]` also generates the hidden
`zalsa_register_downcaster` for the identity view, so the cold path's view lookup is unchanged.

`attach` and `TaskLocals::current` read the task depth through `Database::task_depth(&self)`,
a provided method (0, one task per thread) that the concrete type overrides (base-db's calls
`blaze_graph::pump::depth`), instead of through the process-wide `set_task_depth` hook. The
salsa paths that have no database (claims, the runtime's blocking) take the depth from the
task's `ZalsaLocal`, which records its `TaskId` when it is made. A method on `&self` is
static where the database is concrete (`attach` is generic over it and reads the depth before
it makes the `&dyn Database` it stores; `zalsa_local` is the blanket impl over `HasStorage`,
monomorphized per database),
and a virtual call only where a caller already holds a `dyn Database`. So no indirect call is
left on the hit path.

### Test databases

The twelve test databases (hir-def, hir, hir-ide, hir-ty-mir, hir-ty-codegen, hir-ty-share,
hir-ty-ub, hir-ty-unsafeck, blaze-items, blaze-intent, blaze-diagnostics, base-db) each
implemented the traits again. Each becomes `pub(crate) type TestDB = base_db::Db;`, made by
`Db::for_tests()`. A wrapper struct with `Deref` to `Db` would not do: salsa's accessors
(`krate.data(db)`, `id.lookup(db)`) are generic over the database, so a `&TestDB` would not
coerce to `&Db` there and every such call in the tests would change. Each crate's helpers (a
`fetch_test_crate`) become a crate-private extension trait implemented for `Db`, so their
call sites stay, and `executed` becomes `Db::log_executed`. `Db` has no `Default`, so a `TestDB::default()` that would quietly make a
build's database fails to compile; the tool rewrites it to `TestDB::for_tests()`.
`test-fixture`'s `WithFixture` is implemented for `Db` alone and makes `Db::for_tests()`.

The per-crate setup the test databases did goes away rather than moving. The providers need
none (above). `set_expand_proc_attr_macros(true)`, which every v2 database sets, is
rust-analyzer's switch for not expanding attribute macros in an editor; rustc always expands
them, so hir-def's `ExpandProcAttrMacros` input and the check in `nameres/collector.rs` are
deleted, and attribute macros always expand.

### Readers that outlive a borrow, and the write that stops them

`Db` has no `Clone`: there is one database, never a fork. Scoped work borrows `&Db`, and the
borrow checker keeps every write (`&mut Db`) out until it ends. The daemon also reads from
threads that outlive any borrow: a detached build, a tiering thread. Under rust-analyzer's
salsa each took a clone, and a write to the owner's handle cancelled the clones and waited for
them to drop. Both halves are now explicit:

- Stopping: `salsa::Database::stop_readers(&self)` sets a flag in salsa's `Runtime` that every
  query call checks, the same call that checks a task's own `CancellationToken`. A set flag
  unwinds with `Cancelled::PendingWrite`, inside cycle heads too. A task's own token is
  disabled there, and inference and MIR building, which take the time, run there. The next
  `&mut` access (`prepare_write`) clears the flag, and its cancellation epoch tells apart the
  provisional memos the stopped readers left behind (salsa's `cancellation_in_fixpoint.rs`).
  Upstream salsa's pending-write flag was checked at the same point. The hit pays one
  `Relaxed` load from `Runtime`, which it reads anyway for the current revision, plus a branch
  that is never taken: about 0.3 ns.
- Waiting: `base_db::Shared` owns the `Db` in an `Arc` and hands out `Reader`s. `write()` stops
  the readers if any is held, waits on a condvar until the last `Reader` drops, and clears a
  stop made since the last write. `idle()` returns the database only when no reader holds it,
  and stops none: for a write that can wait, such as a memory report. There is no `DerefMut`,
  because an `&mut` taken only to read would stop every reader.
- In blaze-engine, `lower_until` stops its scoped workers with `stop_readers` once the build
  stops waiting, and clears the stop after the scope. `run_holding`'s database is a `Shared`:
  the detached builds and `compile_tiered` hold `Reader`s, `Builds::settle` stops them where it
  called `trigger_cancellation`, and every write says `db.write()`.

## The migration

The work is mechanical at 2,745 sites, and the rest is a short list per crate.
`tools/v2/concrete_db.py` rewrites a crate's sources:

- `&dyn HirDatabase`, `&dyn SourceDatabase` and `&dyn salsa::Database`, with or without a
  lifetime, become `&Db` (and `&mut` likewise);
- `db.as_dyn()` becomes `db`;
- closures `Fn(&dyn SourceDatabase, usize)` become `Fn(&Db, usize)`;
- `use` lines get `base_db::Db` and lose `SourceDatabase`;
- `TestDB::default()` becomes `TestDB::for_tests()`.

It then prints what it couldn't rewrite: `impl SourceDatabase for`, `impl_hir_database!`,
generic bounds `DB: HirDatabase`, casts `as &dyn`, and `dyn` database uses in macros. Each crate's owner runs it on their crate
once their lane is done (or I do, once the crate is free), fixes what it printed, and runs
`hot_lint`, which must show no `dyn` database.

The order, bottom up, each step complete before the next: salsa (the macro and
`task_depth`), then base-db (the `mod db` hook, `Files`' signatures, the trait's deletion), then
hir-expand, hir-def, hir-ty (with the providers), hir-ty-mir (registering its table),
hir-ty-borrowck, hir-ty-wfcheck, hir-ty-share, hir-ty-ub, hir-ty-unsafeck, hir-ty-codegen, hir,
blaze-items, blaze-diagnostics, blaze-intent, blaze-engine, blaze-cargo and load-cargo. Since
nothing is compiled until every lane is done, the order only matters for which crate's code is
written against which; all of it is written against the API above.

From the moment base-db's `Db` exists, new front-end code takes `&Db`.

## Proof

- `crates/base-db/src/db/tests.rs`:
  - `Db` is `Send + Sync` (a const assertion);
  - `run_all` runs every task once, in each `RunOrder`;
  - a tracked function over `&Db` executes once, then hits (salsa's event log), and a write
    reruns it;
  - a hit allocates nothing (`blaze-alloc-count`);
  - `line_column` gives rustc's line and char column;
  - a build's database keeps no log;
  - `a_memo_hit_makes_no_indirect_call` (`#[ignore]` until compiling is allowed): disassembles
    the test binary's own out-of-line hit and fails on any `call`/`jmp` through a pointer.
- `crates/base-db/src/db/shared/tests.rs`: a write stops a reader that is querying on another
  thread (it unwinds with `PendingWrite`) and waits for it to let go, then clears the stop. A
  write with no reader left neither waits nor stops. `idle()` returns nothing while a reader
  holds the database, and stops none. A stop holds until the next write, for readers made
  after it too. Lane db's `vendor/salsa/tests/parallel/stop_readers.rs` checks that the stop
  reaches a cycle head, where a token alone doesn't.
- hir-ty: `dyn HirDatabase` is rejected, enforced by `NOT_DYN`.
- hir: `the_mir_and_display_providers_are_linked`, in a test binary that links `hir` and names
  nothing of hir-ty-mir: `hir_ty::providers::mir()` and `display()` find exactly one table.
- The front-end test suites run unchanged on the test databases above, which now share the
  build's `Db`, so the tests and a build run the same database code.
- `hot_lint` over each finished crate finds no `dyn` database.
