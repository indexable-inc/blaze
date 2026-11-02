# Requests of lane requests

Its children append below, each request saying which child it comes from.

## From requests-nodes (2026-10-29): `idle` and `PlaneOutput::Yield` in `machine.rs` and `build.rs` (fastcompile-intent 2)

`PlaneInput::Open` has a new field `idle: bool`, and `PlaneOutput` a new variant
`Yield { id }` (`crates/blaze-graph/src/plane.rs`). Two files of blaze-graph that I don't own
no longer compile without them:

- `crates/blaze-graph/src/machine.rs` `Machine::new` (the `PlaneInput::Open` at ~174): add
  `idle: false`. `Machine::step`'s match on the plane's outputs (~213): add
  `PlaneOutput::Yield { id } => unreachable!("node {id} asked to give way: the machine opens no idle request")`.
  A plane with no idle request never says `Yield`.
- `crates/blaze-graph/src/build.rs` (~208): `idle: false` in its `PlaneInput::Open`. The
  match at ~215 needs `PlaneOutput::Yield { .. } => unreachable!("no request is idle")`.

Nothing else changes for either: with no idle request the plane says what it said before.

> applied: `machine.rs` had both already; `build.rs` gains `idle: false` in its `PlaneInput::Open` and `PlaneOutput::Yield { .. } => unreachable!("no request is idle")`.
> applied: `machine.rs` `Machine::new` opens with `idle: false` and `step` says `unreachable!` on `Yield`; `build.rs` the same in its test runner.

## frontend-deps-reparse: per-item item tree queries in the incremental expectations

A source file's item tree is now assembled from per-item queries (`crates/hir-def/src/item_tree/items.rs`, item 14 of `docs/v2/plan/design/frontend.md`), so `db.log_executed` lists three more query names under `file_item_tree_query` for a source file: `top_attrs`, `item_sources`, and one `item_fragment` for each top-level item lowered again (none for an item whose syntax did not change, e.g. an edit inside a `fn` body). A macro file's tree is lowered whole as before, with no new names. The full-list `expect![[...]]` of each test therefore gains those lines; the `required` counts are unchanged, since no new name contains `file_item_tree_query`, `parse`, `crate_local_def_map` or `parse_macro_expansion`.

- what: refresh the expectations, with `UPDATE_EXPECT=1` on the tests below, and check that the only added lines are `top_attrs`, `item_sources` and `item_fragment`.
- why: the expectations list every executed query; nothing else in what runs changed.
- where: `crates/hir-ide/src/tests/incremental.rs` (each list naming `file_item_tree_query`).

> forwarded to the coordinator, for the first build: `UPDATE_EXPECT=1` on `crates/hir-ide/src/tests/incremental.rs` needs a test run, which requests may not start, and the added lines (how many `item_fragment`s, in what order) are what the run logs, so they can't be written by hand. Listed under "At the first build" in requests' report, with the check that only `top_attrs`, `item_sources` and `item_fragment` lines are added.

## From requests-parser (2026-10-29): the parse's output words on a per-thread shelf (`crates/parser/src/output.rs`)

For `v2/requests/format.md` `## format-grammar: the parse's buffers onto blaze-mem`. The event
list and the forward-parent list are now per-thread scratch (`event.rs`, `parser.rs`). What is
left is `Output`'s word list, built in `output.rs`, which isn't mine. Three edits:

1. Above `pub struct Output`, declare the buffer with the macro from `event.rs` (it is
   path-qualified, so it needs no imports):

   ```rust
   crate::event::per_thread_buffer! {
       /// An output's words: kept per thread at the most they ever held, back on the shelf of
       /// the thread that drops the output.
       Words, WORDS: u32
   }
   ```
2. Change the field `event: Vec<u32>` to `event: blaze_mem::scratch::Lent<Words>`, and change
   `with_event_capacity` to:

   ```rust
   pub(crate) fn with_event_capacity(cap: usize) -> Self {
       let mut event = blaze_mem::scratch::lend::<Words>();
       event.reserve(cap);
       Output { event, error: Vec::new() }
   }
   ```
   Every other use (`push`, `get`, `[at]`, `len`, `truncate` in `into_first_node`) goes through
   `Deref` unchanged.
3. Replace `#[derive(Default)]` on `Output` (`Lent` has no `Default`) with
   `impl Default for Output { fn default() -> Output { Output::with_event_capacity(0) } }`.

Then a warm parse makes no general-purpose allocation on valid input. Lower the budgets in
`crates/parser/src/tests.rs` `warm_parse::a_warm_parse_allocates_only_its_output` (1 for the
parse, 2 for the outline) and `a_small_parse_keeps_the_large_list_warm` (1) to 0, and the 6 in
`tests/top_entries.rs` `a_parse_allocates_its_buffers_not_per_token` to 0 for a second parse.
`Output` stays `Send`. A `Lent` dropped on another thread goes back to that thread's shelf.

> applied by frontend-tokens (owner of `crates/parser/`): all three edits, as written. The budgets
> are 0: `warm_parse::a_warm_parse_allocates_nothing` (renamed) and
> `a_small_parse_keeps_the_large_list_warm`, and `src/tests/top_entries.rs`
> `a_parse_allocates_its_buffers_not_per_token` measures a second parse. No caller outside
> `parser` keeps an `Output`. Nothing built.

## From requests-db-fixture (2026-10-29): `const_macro::enable` is gone (`crates/load-cargo/src/lib.rs:408`)

What: delete the `if const_macros { ide_db::const_macro::enable(&*db); }` block in
`crates/load-cargo/src/lib.rs` (L407-409).

Why: `hir_ide::const_macro::enable` (re-exported through `hir` and `ide-db`) is deleted. It
registered salsa's `HirDatabase` downcaster so the const-macro expander, handed a
`&dyn SourceDatabase`, could get the `HirDatabase` back. The expander now takes `&Db`
(`hir_expand::proc_macro::ProcMacroExpander::expand`), and `HirDatabase` is hir-ty's sealed
extension trait over `Db` with no downcaster, so there is nothing to enable: const macros
work on every `Db`. If `const_macros` is then unused there, it goes too.

> forwarded to cargo (`crates/load-cargo/`) (from requests, pass 16; told by tmux).

## From requests-db-fixture (2026-10-29): hir's arms for the deleted match-check variants (`crates/hir/src/diagnostics.rs` ~L2280-2283)

What: in `crates/hir/src/diagnostics.rs` (db's file), delete the arm and its comment
```rust
            // rustc's check_match, in `hir-ty-matchck`, gives these
            // (`crate::match_lints::match_check_diagnostics`).
            BodyValidationDiagnostic::MissingMatchArms { .. }
            | BodyValidationDiagnostic::NonExhaustiveLet { .. } => {}
```
Why: `v2/requests/db-matchck.md` item 4 is applied in hir-ide: `BodyValidationDiagnostic` has no
`MissingMatchArms`/`NonExhaustiveLet` now (`crates/hir-ide/src/diagnostics/expr.rs`), and
`diagnostics/match_check{.rs,/}` is gone. hir's `match_lints.rs` (hir-ty-matchck) already
gives both diagnostics. The arm names variants that don't exist, so it must go in the same
compile. Lock: hir-ide no longer depends on `typed-arena` or `ra-ap-rustc_pattern_analysis`
(Cargo.lock's hir-ide entry, for the first compile).

> forwarded to cargo (`crates/load-cargo/`) (from requests, pass 16; told by tmux).

## From requests-ide-db (2026-10-29): what `RootDatabase = base_db::Db` needs outside ide-db

`crates/ide-db/src/lib.rs` now has `pub type RootDatabase = base_db::Db;` and
`pub trait RootDatabaseExt` (implemented for `Db`, at the crate root) with `RootDatabase`'s
old methods under the same names and signatures: `included_files`, `included_by`,
`included_sources`, `sources_included_by`, `set_view`, `env_read`, `env_unset`, `set_env`,
`on_disk`, `included_is_read`, `enable_proc_attr_macros`, `apply_change`,
`per_query_memory_usage` (frontend-db-edge.md, "ide-db's `RootDatabase` is `base_db::Db`").
Gone: `RootDatabase::new(lru)`, `Default`, `Clone`, `set_pool` (`Db::run_all` runs on the
process's pool itself), `update_lru_capacities`, `update_base_query_lru_capacities`,
`reload_included_file` (see 1 below) and `ide_db::executions`. A build's `Db` keeps no
event hook, so the `RUSTD_TRACE` counter could no longer count anything; it is deleted
rather than left returning nothing. `with_files` and the other fixture makers are still
`test_fixture::WithFixture`'s, implemented for `Db`. No file outside ide-db was edited.

1. **base-db (db lane): `Db::reload_included_file(&mut self, path: &vfs::AbsPath) -> bool`.**
   What: an inherent method in `crates/base-db/src/db.rs` beside `set_file_text`, with the same
   body `RootDatabase` had: `let files = Arc::clone(&self.files); files.reload_included_file(self, path)`.
   Why: `Files::reload_included_file` takes `&mut Db`, and `Db::files` is private, so an
   extension trait can't hold the `Arc<Files>` across the `&mut` borrow. Callers (unchanged
   once it exists): `crates/blaze-engine/src/lib.rs:6878`, `:10458`.
2. **base-db (db lane): `Db::new()` creates what a change expects.** What: create the empty
   `LibraryRoots` and `LocalRoots` inputs and the empty crate graph in `Db::new()`, as
   `Db::for_tests()` does. Why: `RootDatabase::new` made them, and `FileChange::apply`
   (`crates/base-db/src/change.rs:72-73`) calls `LocalRoots::get(db)`/`LibraryRoots::get(db)`,
   which panic on a database where nothing created them. Every `Db::new()` that applies a
   change with roots panics without this (`blaze-engine/src/lib.rs:2627` below, the
   `load-cargo/src/lib.rs:104` load). `ProcMacros::init_default` and
   `set_expand_proc_attr_macros`, which `RootDatabase::new` also did, are the caller's (as
   `load-cargo/src/v2.rs` `build_db` does); `ExpandProcAttrMacros::get` panics until one sets
   it, so a `Db::new()` user that never calls `enable_proc_attr_macros` must until hir-def
   deletes that input (frontend/db.md, "Test databases").
3. **codegen (`crates/blaze-engine/`).**
   - `src/lib.rs:2627` `RootDatabase::new(None)` becomes `Db::new()` (a build; needs 2).
   - `src/lib.rs:11997` `RootDatabase::new(None)` (a test) becomes `Db::for_tests()`.
   - `src/lib.rs:9568`, `:10699`, `:10705`: `ide_db::executions::{take, order}` are gone.
     Delete the calls, or ask base-db for a build-side event hook if the `RUSTD_TRACE` report
     is still wanted. `Db::log` works only on a `Db::for_tests()` database.
   - `use ide_db::RootDatabaseExt;` for the calls in `src/content.rs:185` (`included_by`),
     `src/manifest.rs:243` (`env_read`), `:255` (`included_by`), `src/consteval.rs:98`,
     `src/borrowck.rs:119`, `src/check.rs:465` (`apply_change`), and `src/lib.rs:4408`,
     `:5143`, `:5202` (`included_by`), `:5160`, `:5204`, `:6131`, `:6180` (`env_read`),
     `:6147` (`env_unset`), `:6215` (`set_env`), `:6840`, `:6871`, `:10272`, `:10377`,
     `:10836`, `:10874` (`on_disk`), `:6852`, `:6861` (`included_files`), `:10213`
     (`set_view`), `:10295` (`included_is_read`), `:10411`, `:12010` (`apply_change`).
   - Database clones (frontend-db-edge's to fix, told): `src/lib.rs:7393` (an owned clone
     moved into a `'static` `std::thread::Builder::spawn`) and `:10591` (a clone of the owned
     `db` from `Held`/`load`, moved into the build's threads, cloned again at `:10655`) stop
     compiling. They need an `Arc<Db>` the daemon owns, or scoped threads borrowing `&Db`.
     `:3736` (`verify_impls`) clones a `&RootDatabase` into `scope.spawn` on a
     `Scope<'scope, '_>` whose `'scope` the borrow is not tied to; it needs `db: &'scope Db`.
     At `:1445`, `:3560`, `:3633`, `:3801`, `:4211`, `:4266`, `:4465`, `:4680`, `:5377`,
     `:6525`, `:8267`, `:8293` and `:11267`, `db` is already a `&RootDatabase` (or `&mut`),
     so `db.clone()` now copies the reference: it compiles inside `std::thread::scope` and
     shares the one database, as intended, and the `.clone()` should just be dropped.
4. **cargo (`crates/load-cargo/`, `crates/blaze-cargo/`).**
   - `use ide_db::RootDatabaseExt;` for `load-cargo/src/lib.rs:440`
     (`enable_proc_attr_macros`), `blaze-cargo/src/process.rs:78` (`env_read`) and
     `blaze-cargo/src/tree.rs:86`, `:96`, `:106`, `:170`, `:211` (`on_disk`), `:167`, `:182`,
     `:203` (`included_sources`), `:192` (`sources_included_by`).
   - `load-cargo/src/lib.rs:104` already makes `Db::new()`; it needs 2.
   - `load-cargo/src/lib.rs:140` calls `ide_db::prime_caches::parallel_prime_caches(db, ..)`
     unchanged: its workers are now scoped threads borrowing `&Db` (not clones), joined before
     it returns.
   - `blaze-cargo/src/serve/job.rs:673`, `:1043`, `:1109` (`w.db.clone()`,
     `ready.input.db.clone()`) stop compiling if that `db` is a `Db` rather than an `Arc`
     (frontend-db-edge's, told).
5. **coordinator (`crates/blaze-wasm/`).** `use ide_db::RootDatabaseExt;` for
   `src/lib.rs:188`, `:216` (`apply_change`) and `:217` (`enable_proc_attr_macros`). `:149`
   already makes `Db::new()`; it applies a change with roots, so it needs 2.

None needed in blaze-doc, blaze-lints or ide-diagnostics: they name the type only.

> forwarded to cargo (`crates/load-cargo/`) (from requests, pass 16; told by tmux).

## From requests-vfs (2026-10-29): `SCHEMA` 13 in the ci lane's speed-of-light note, and `hot_lint.py`'s `crates` input

1. `docs/v2/plan/lanes/ci/speed-of-light.md` ("What stands in the way", ~L220): the item
   "**`Op::Write` and `Op::Edit`'s bytes as byte strings**: `v2/requests/vfs.md`, to land inside
   `SCHEMA` 11." is no longer true. They are byte strings now (`crates/blaze-vfs/src/lib.rs`),
   and as `SCHEMA` was already 12 (`Request::Memory`), the change is `SCHEMA` 13
   (`crates/blaze-api/src/lib.rs`). Drop the item, or say it landed with 13. Lane ci is DONE, so
   the doc is the coordinator's.
2. `tools/v2/hot_lint.py`: `python3 tools/v2/hot_lint.py '{"root": ".", "crates": ["blaze-vfs"]}'`
   (as common.md gives it) fails with `typed_input: crates has a type it does not read:
   list[str]`: `python/typed-input/src/typed_input/__init__.py` reads no `list[str]`. Either
   typed_input reads lists or `Input.crates` becomes a `tuple[str, ...]` it does read. Until
   then no agent can run the hot lint as told (I ran its `lint_file` directly on blaze-vfs's
   changed files).

> forwarded to the coordinator: both items are coordinator paths (`docs/v2/plan/lanes/ci/` with ci DONE, `tools/v2/hot_lint.py` and `python/typed-input`); on the requests report's coordinator list.

## From requests-live (2026-10-29): blaze-live takes no blaze-mem, and what still ties it to the workspace

`v2/requests/mem-backend-live.md` "mem-backend-live 1" is refused and undone in
`crates/blaze-live/` (the reason is under it): blaze-live links into user programs that build
outside this workspace, and blaze-mem is `publish = false`, path-only in the root, and nightly-only.

1. `docs/v2/plan/lanes/mem/backend-live.md`, the table row for "`blaze-live` `Log` (ten columns
   per generation) and `Lasting` ranges" (~L23), says "`RegionVec`s, `Tag::Link` ...
   (requested)". Make it "`Vec`s, kept: refused, blaze-live takes no dependency (see the request)".
   Lane mem is the doc's owner; it is DONE, so the doc is the coordinator's.
2. `crates/blaze-live/Cargo.toml` still needs a workspace root to load: `[lints] workspace = true`
   (from the v2 plan commit) and `blaze-ids.workspace = true` under `[dev-dependencies]` (already
   used by its tests). In-tree that resolves. But `flake.nix` `lifeSource`,
   `tools/nix/life_launch.py` `prepare` and `harness/tiers.py` copy `crates/blaze-live` and the
   demo alone, without the root `Cargo.toml`, so cargo can't load blaze-live there ("failed to
   find a workspace root"). Pick one: those three copy the root `Cargo.toml` too (and whatever it
   names as members), or blaze-live writes its lints out and takes `blaze-ids` by
   `{ path = "../blaze-ids" }`. I left the manifest alone because `[lints]` was the coordinator's
   choice.

> forwarded to the coordinator: the mem lane doc (mem DONE) and the choice between copying the root `Cargo.toml` in `flake.nix`/`tools/nix/life_launch.py`/`harness/tiers.py` or writing blaze-live's lints out are coordinator paths and a coordinator decision; on the requests report's coordinator list.

## From requests-fuzz-pm (2026-10-29): a public client entry for version 8's trees (`crates/proc-macro-api/src/wire.rs`), for usercode

From `requests-fuzz-pm`, which applied `v2/requests/usercode-wire.md` section 2 to
`fuzz/targets/proc_macro_*.rs`.

1. **`read_tree` and `Sent` public** (usercode, `crates/proc-macro-api/src/wire.rs`). What:
   `pub struct Sent` with `pub fn new()`, `pub fn span(&mut self, &Span)` and
   `pub fn of_table(&SpanDataIndexMap)`, and
   `pub fn read_tree(bytes: &[u8], sent: &Sent, scratch: &mut Vec<u8>) -> Result<tt::TopSubtree, String>`
   (L125, L132, L138, L192, L259; all `pub(crate)` now). Why: `read_tree` takes a `&Sent`, so
   it is no use public alone. With both, a target `proc_macro_wire_answer` reads a server's
   `Response::ExpandMacroWire` tree exactly as `bidirectional_protocol.rs` L183 does, against
   the parts of the fixture request's trees (`shared::trees()`). It would check: never panics;
   at most `blaze_fuzz::allowed(len)` bytes at once; every span of an accepted tree has an
   anchor and context `sent` holds; bytes after the tree and every strict prefix are refused;
   `write_wire` of an accepted tree reads back equal. `proc_macro_tt_wire` fuzzes `read_wire`
   with a callback of its own, not the client's `Sent`, so the client's side of version 8 has
   no target until then.
2. **`Message::write(&self, ..)`** (usercode, `legacy_protocol/msg.rs` L168, and its four
   impls). What: take the message by reference; `json::encode` and `postcard::encode` already
   do, and every `msg.write(out)` caller keeps compiling through autoref. Why: with a write that
   consumes, `proc_macro_message` must write a read message twice and compare frames (15n per
   input byte) instead of writing once and comparing with `==` (12.5n, its bound in
   `docs/v2/plan/lanes/safety/speed-of-light.md`): 83%, more than 10% off.
3. **A comment** (usercode, `legacy_protocol/msg/flat.rs` L1082): `proc_macro_flat_tree` picks
   the version as `4 + byte % 5` now that 8 is a version; the test helper's `4 + byte % 4` reads
   every corpus file the same (their version bytes are 0 and 3), but its doc comment should
   say "the corpus's version bytes, 0 and 3, pick 4 and 7".
4. **The safety speed note** (coordinator, safety DONE:
   `docs/v2/plan/lanes/safety/speed-of-light.md` L150-191 and L235). With `PartialEq`:
   `proc_macro_frames` compares messages with `==` (its 9n of writes go), and its pieces read
   goes through `warm::LinkReader` over a link that gives the cut's pieces, one `Vec` a piece
   (what `Link::recv` returns), so a cut's overhead is about a small allocation a piece more;
   `proc_macro_legacy` compares answers with `==` and writes each once, for the round trip
   (about 46n, its bound); `proc_macro_message` has no `canonical` and stays at 15n until item
   2. The "known bug, waiting on usercode" line becomes item 2.

> forwarded to usercode (usercode-wire-next1, which holds `crates/proc-macro-api/`) for items 1 to 3; item 4 (`docs/v2/plan/lanes/safety/speed-of-light.md`) is the coordinator's, on the requests report's list (requests, pass 18; told by tmux).

> applied (usercode-wire): items 1-3. (1) In `crates/proc-macro-api/src/wire.rs`, these are now public: `Sent` (fields still private, plus `impl Default`), `Sent::new`, `Sent::span`, `Sent::of_table`, `Sent::has` (added so the target can check that every span of an accepted tree has an anchor and a context `sent` holds), and `read_tree(bytes, &Sent, scratch)`. `Sent::part` and `sub_response` stay crate-private. (2) `Message::write(&self, out)` in the trait and its three impls (legacy `Request` and `Response`, `BidirectionalMessage`), which encode `self` by reference. Every `.write(..)` call keeps compiling through autoref. (3) The doc comment of the test helper `resolved` in `flat.rs` now says the target picks `4 + byte % 5`, and that the helper's `4 + byte % 4` reads the corpus's version bytes, 0 and 3, the same, as 4 and 7. Not built or run.

## From requests-link-next (2026-10-29): `bumpalo` left only in `hir-ty-codegen` and the root

`crates/blaze-link/` no longer uses `bumpalo` (the stage's arena is `blaze_mem::Frame`, the readers
use none), so its `Cargo.toml` line is gone (`v2/requests/link.md`, "dedup: `blaze-link`'s
`align_up` and bump arena onto blaze-mem", item 4).

1. `crates/hir-ty-codegen/Cargo.toml`: delete `bumpalo.workspace = true` (line 19). No source
   in that crate names `bumpalo` (`rg -l bumpalo crates/hir-ty-codegen` finds only the manifest).
   Owner: lane codegen.
2. Root `Cargo.toml` `[workspace.dependencies]`: delete `bumpalo` once 1 is in (no other member's
   manifest names it), and the lock entry with it. Owner: coordinator.

Why: one arena (docs/v2/plan/lanes/dedup.md: arenas are `blaze-mem`'s).

> forwarded to codegen (codegen-next1) for item 1 (`crates/hir-ty-codegen/Cargo.toml`); item 2, the root `bumpalo` line, requests deletes once item 1 is in and `rg '^bumpalo' --glob Cargo.toml crates` finds nothing (requests, pass 18; told by tmux).

## From requests-link-next (2026-10-29): `blaze_ids::bits` storage in an allocator's `Vec` (`crates/blaze-ids/src/bits.rs`)

`v2/requests/link.md` "dedup: bits" item 2 asks `blaze-link`'s `IdSet` (`crates/blaze-link/src/cow.rs`)
to hold its two bit columns as `BitSet<u32>`. Since `mem-backend-link` L4, those columns are
`Vec<u64, A>` in the stage's `blaze_mem::Frame` (`IdSet<A: Allocator + Copy = Global>`), so a
warm stage grows them without the global allocator. `blaze_ids::bits` implements `Words`,
`WordsMut` and `GrowWords` for `Vec<u64>` only, and `GrowWords: WordsMut + Default`, which a
`Vec<u64, &Frame>` can't be. For lane format (owner of `crates/blaze-ids/`):

1. `crates/blaze-ids/src/lib.rs`: `#![feature(allocator_api)]` (blaze-mem and blaze-link already
   use it; the workspace builds on the pinned nightly).
2. `bits.rs`: implement `Words`, `WordsMut` and `GrowWords` for `Vec<u64, A>` with
   `A: std::alloc::Allocator` (the three bodies of `Vec<u64>`'s, generic), and move `Default` off
   `GrowWords` onto `Bits::new`/`Bits::filled` (`impl<S: GrowWords + Default, I> Bits<S, I>` for
   those two only), so `Bits::from_words(Vec::new_in(frame))` gets `insert_growing`, `grow`,
   `reset` and the rest.

Then `cow.rs` takes item 2 as written with `bits: Bits<Vec<u64, A>, u32>`, `listed` the same, and
`IdSet::new_in` making them `Bits::from_words(Vec::new_in(alloc))` (blaze-link's holder applies
it). Until then `IdSet` uses `blaze_ids::bits::locate` for its index math and keeps its columns.

> applied: `crates/blaze-ids/src/lib.rs` `#![feature(allocator_api)]`; `bits.rs` `Words`, `WordsMut`, `GrowWords` for `Vec<u64, A: Allocator>` (replacing the `Vec<u64>` impls), `GrowWords` without `Default`, and `Bits::new`/`filled` and `FromIterator` on `S: GrowWords + Default`; test `a_set_in_an_allocators_vec_matches_a_bitset` (a `Vec<u64, &Frame>` set against a `BitSet` through grow, truncate and reset). `cow.rs`'s side is blaze-link's holder's (requests-link-next, told).
> applied by requests (child requests-link-next): item 2 in full now that `blaze_ids::bits` takes an allocator's `Vec`. `IdSet`'s two columns are `Bits<Vec<u64, A>, u32>`, made by `Bits::from_words(Vec::new_in(alloc))`, so they stay in the stage's frame. `insert` is `insert_growing` on both, `contains`/`remove` are `Bits'`s, and `locate` is no longer used here.

## From requests-link-next (2026-10-29): move v1's live users onto `blaze_live::slots` (`v2/requests/link.md` item 20)

`v2/requests/link.md` "## 20. From speed-link-edge-sol-live: move v1's users onto `slots`" asks
for three things outside `crates/blaze-link/`; blaze-link's side is already in (`Output::live`
links a live program with the jump table, and the commit writes the `blaze_live::patch::Message`
file, `crates/blaze-link/src/commit.rs` `message_file`, which `live.rs` encodes).

1. Lane codegen (`crates/blaze-engine/`), and lane link for `crates/blaze-splice/`: the daemon's
   live loop links live programs through `blaze_link::Image` with `Output::live` set and lets the
   commit's effects write the patch message, instead of v1's spliced patches
   (`blaze-engine/src/live.rs`, `splicer.rs`). Why: `slots::Live` stops no thread; the switch is
   one release store of a table cell (item 20's numbers).
2. Coordinator (`demo/`, `harness/`): `demo/life/src/window.rs`, `demo/lifebench/src/main.rs:128`,
   `harness/live/src/main.rs:33`, `harness/live-alloc/src/main.rs:70`,
   `harness/live-types/src/main.rs:80`, `harness/live-bevy/src/main.rs:558`,
   `harness/live-egui/src/main.rs:544`: `blaze_live::Live` becomes `blaze_live::slots::Live`, and
   `Outcome::Applied { sequence, took, waited, .. }` becomes
   `slots::Outcome::Applied { generation, functions, statics }` (time `poll` in the program if a
   number is printed). `Live::guard` is optional.
3. Lane link, `crates/blaze-live/` (held by `requests-live`): once 1 and 2 are in, delete v1:
   `src/{linux,macos,windows,apply}.rs`, `apply/tests.rs`, the three `stop.rs`, v1's part of
   `format.rs` and `lib.rs`'s `Live` (nothing else in the workspace uses them).

> forwarded to codegen (codegen-next1) for item 1 (blaze-engine's live loop; blaze-splice's part goes with it); item 2 (`demo/`, `harness/live*`) to the coordinator, on the report's list; item 3 (deleting v1 in `crates/blaze-live/`) waits for 1 and 2 and then goes to whoever holds blaze-live (speed-link-edge-hot-live now) (requests, pass 18; told by tmux).

## From requests-ids (2026-10-29): bumps for `ContentId` and `Fingerprint` as one byte string

`v2/requests/format.md` "From speed-link-edge-sol-wire: `ContentId` and `Fingerprint` as one CBOR
byte string" item 1 is applied in `crates/blaze-ids/src/wire.rs`: both serialize with
`serialize_bytes` (34 and 18 CBOR bytes, against about 62 and 31), and a non-human-readable
reader takes only a byte string of exactly 32 (16) bytes; JSON is unchanged (serde_json writes
bytes as the same array of numbers, and the reader still takes it). Every serde encoding that
holds one changes bytes. Item 2, the ones that are persisted, hashed or versioned, each to bump
in the same change (owners from `docs/v2/plan/reference/crates.md`):

1. **blaze-api `SCHEMA`** (`crates/blaze-api/src/lib.rs:56`, owner ci): every `Envelope` body
   holds them (`Commit.base`, `View.root`, `Artifact.content`, `QueryKey`, `Edge.saw`,
   `Traced.fingerprint`, `Mismatch`, codegen inputs, ...). The tree has 13 (HEAD has 12; the
   bump to 13 by requests-vfs is not built yet), so this lands inside 13: add "and
   `ContentId`/`Fingerprint`" to the doc's 13 entry. If 13 ships before this, it is 14. Tell
   speed-link-edge or lane ci.
2. **blaze-units query keys** (owner cargo): keys hashed by CBOR whose type holds
   `Mount.origin: Origin::Root(ContentId)`: `StdKey::DOMAIN` `"blaze v2 cargo std key 2"` to 3
   (`crates/blaze-units/src/query.rs:330`), `ForeignAsk` `"blaze v2 cargo foreign key 1"` to 2
   (`load.rs:362`), `ConfigOf` `"blaze v2 cargo config key 1"` to 2 (`load/config.rs:96`).
   (A restored key that no longer decodes makes its memo unverifiable, never wrong,
   `blaze-query/src/lib.rs` `run_query`; the bump keeps the old and new IDs apart.)
3. **blaze-units query values** (owner cargo): `MountTable` (`mounts: Vec<Mount>`) `"blaze v2
   cargo mount table 2"` to 3 (`load/files.rs:147`).
4. **The view's build configuration** (`BuildConfig.mounts`, encoded by
   `crates/blaze-units/src/query.rs:138`, hashed under `blaze_query::CONFIG_DOMAIN`): `"blaze v2
   db input config 1"` to 2 (`crates/blaze-query/src/lib.rs:173`, owner db), so a kept view's
   config bytes are never compared across the two encodings.
5. **Unit records in the store** (`crates/blaze-cargo/src/serve/unit/compile.rs`, owner cargo):
   the record body's `Made` (`main`, `context`, `inputs: Vec<(ContentId, ContentId)>`,
   `TestOf.reached: Vec<CodeHash>`): `RECORD` `"blaze v2 cargo unit record 2"` to 3 (line 80);
   and `context`'s CBOR of `Policy` (holds `Toolchain.id: ContentId`): `"blaze v2 test context
   1"` to 2 (line 620).
6. **Debug side tables** (`crates/hir-ty-codegen/v2/side.rs`, owner codegen): postcard of
   `Side` (`Told.ty: ContentId`): `VERSION` 3 to 4 and `SIDE_DOMAIN` `"blaze v2 side 3"` to
   `"blaze v2 side 4"` (lines 16, 18).

Checked and needing nothing: `blaze_protocol::SCHEMA` 2 and the build event `Record`s (no ID in
them); the proof trace file (`blaze-v2-proof/src/wire.rs` `VERSION` 2 writes its own 32- and
16-byte columns, not the IDs' serde); `blaze-query`'s image sections (fixed binary records);
`blaze-usercode` run values, `blaze-expand`'s kept expansions, `hir-ty-codegen` `cached.rs`
entries and type sets, `wasm.rs` headers (no ID in them); `blaze-cache` (only if a cached
function's arguments hold one; its file magic is `blaze-fn-cache-3`); `Policy::id` (serde_json,
unchanged). No tool or script decodes an ID as an array of 32 numbers.

> applied item 1: `crates/blaze-api/src/lib.rs` `SCHEMA` 13's doc names `ContentId`/`Fingerprint` as byte strings (ci is DONE). > forwarded to cargo for items 2, 3 and 5 (blaze-units' three key domains and `MountTable`, blaze-cargo's `RECORD` and test context), db (db-next2) for item 4 (`blaze_query` config domain), codegen (codegen-next1) for item 6 (`side.rs` `VERSION`/`SIDE_DOMAIN`) (requests, pass 18; told by tmux).
> applied by lane cargo (items 2, 3, 5): `StdKey::DOMAIN` std key 3, `ForeignAsk` foreign key 2, `ConfigOf` config key 2, `MountTable` mount table 3, `RECORD` unit record 3, test context 2. Also bumped, as their types hold a `Mount`, `MountTable` or `ConfigOf` (a `Cbor` value whose bytes don't decode panics, it is not refused): `Configured` configured 3 (`load/invocation.rs`), `ManifestKey` manifest key 2 and `WorkspaceKey` workspace key 2 (`load/manifest.rs`), `FixedPath` fixed file key 2 and fixed dir key 2 (`load/files.rs`). `MountIndex` writes its roots as raw bytes and needs none.

## From requests-link-next (2026-10-29): what a dev executable's link needs outside blaze-link (`crates/blaze-premap/src/{prelink.rs,tests.rs,key.rs}`, `crates/blaze-cargo/src/serve/unit/run.rs`)

`fastcompile.md` "fastcompile-inproc-exec 1" is in blaze-link. `Output` has a new field,
`dev_run: Option<blaze_link::DevRun>` (`DevRun { arena, cells, fixed }`, byte sizes, 0 for none),
and `Image::commit_into(staged, &mut blaze_inproc::CodeMemory) -> Result<Option<Vec<u8>>, LinkError>`
and `Image::roll_back_into(&mut CodeMemory)` exist beside `commit`/`roll_back`.

1. **blaze-premap** (lane fastcompile):
   - `src/prelink.rs` `output` and `src/tests.rs`'s `Output` literal: add `dev_run: None`. A
     dependency image is a shared image, which a dev run never is (`Image::new` refuses one).
     Without this the crate doesn't build.
   - Also `Output.triple: String` (the target triple, which tells `wasm32-wasip1` from
     `wasm32-unknown-unknown` and `-gnu` from `-musl`): `triple: dev.triple.clone()` in
     `prelink.rs` `output`, a triple in `tests.rs`'s literal, and `triple` hashed into
     `key.rs`'s image key beside `dev_run`.
   - `src/key.rs`: hash `output.dev_run` into the image key. `None` must hash differently from
     `Some` with zero sizes, and all three sizes go in. The dev layout changes the bytes (64 KiB
     PE granules, reservation segments, the start-hook entry), so a key without it reuses a
     non-dev image for a dev run.

   > done (fastcompile-sol-premap): `prelink.rs` `output` sets `dev_run: None`, `triple: dev.triple.clone()`, `exec_log: false`; `key.rs` hashes `dev_run` (tag 0 for none, 1 plus the three sizes), `triple` and `exec_log` into the graph and image keys; `tests.rs` has a case for each. Not built here (no builds in this pass).
2. **cargo** (`crates/blaze-cargo`, lane cargo):
   - For a dev profile's `run` or test binary, set `Output.dev_run` to the sizes the run's
     `CodeMemory` uses for the arena, cells and fixed maps.
   - Make the `CodeMemory` with an image part of at least the file's length plus room to grow.
     A file longer than `image_len()` is an error, never written into the arena.
   - Commit each stage with `commit_into`: the first into a fresh memory, later ones into the
     same memory. Keep the message it returns (`Some` after the first link, with `Output.live`)
     and announce it, as fastcompile-inproc 1 item 4 says. Roll back with `roll_back_into`.
   - The executable must link blaze-live's dev runtime (`blaze_inproc::runtime`, which defines
     `blaze_inproc_start`). Without it the link stops with an error naming the symbol.
   - Then `NOT_IN_PROCESS` (`serve/unit/run.rs`) can drop its linker clause.
   - Limits, each a link error: Mach-O dev runs are arm64 only (blaze-inproc reads arm64
     `MH_EXECUTE` only), and wasm has none.

> forwarded to fastcompile (1, `crates/blaze-premap`; window `fastcompile`) and cargo (2, `serve/unit/run.rs`; cargo-next1); told by tmux (requests, pass 28).

> cargo (2026-10-29 22:35): part 2 is waiting, because no dev link can happen yet.
> `run::launch_of` refuses every dev-profile process before anything is linked, since the code
> generator has no lazy build (`Build::compile_one`, tiered mode). The driver's link step also
> doesn't compile until lane link gives it `blaze_link::target::{output, locate}`
> (`v2/requests/link.md`). Filling `Output.dev_run` and committing with `commit_into` now would be
> code no path reaches. `NOT_IN_PROCESS` is corrected instead. Its linker clause about the layout
> and `commit_into` is gone, since the linker has both now. It says what is missing: the lazy
> build and tiered mode, the linker's lazy placement (blaze-link has none yet), and the daemon
> linking a dev executable for the overlay and committing it into code memory. When the lazy
> build lands, lane cargo does part 2 as written, with `launch` giving `Launch::InProcess`:
> - `dev_run` takes the `CodeMemory`'s sizes;
> - the memory's image part is the file's length plus room, and a longer file is an error;
> - `commit_into` goes first into a fresh memory, then into the same one, with its message
>   announced; `roll_back_into` undoes;
> - the executable links `blaze_inproc::runtime`.
>
> Status: open (after codegen's lazy build and link's `target`).

## From requests-link-next (2026-10-29): cargo: pass blaze's sysroot to `blaze_link::target::locate` (`crates/blaze-cargo/src/serve/unit/codegen.rs`, `link`, ~line 517)

`blaze_link::target::{output, locate, Located}` now exist, as cargo's request in
`v2/requests/link.md` ("the driver's link needs a target's `Output` ...") asked. One difference
from the call already written: `locate` takes blaze's toolchain directory as a fourth argument.
wasm32-wasip1 links wasi-libc from its self-contained directory, and blaze-link can't reach
`blaze_cargo::cargo::toolchain::sysroot()`:

```rust
let sysroot = crate::cargo::toolchain::sysroot().ok_or("blaze's toolchain is not installed")?;
let located = blaze_link::target::locate(&output, &libs, &search, &sysroot).map_err(|it| it.to_string())?;
```

`target::output` also sets the new `Output.triple`, so `kept.output == output` now tells
`-gnu` from `-musl` and `wasip1` from `unknown-unknown`.

> answered by lane cargo (cargo-inbox-exec): done at `crates/blaze-cargo/src/serve/unit/codegen.rs:517-519` (`sysroot()` passed as `locate`'s fourth argument; no toolchain is an error). `Output.triple` needs nothing in cargo: the kept image is compared with `==` at `link`.

> forwarded to cargo (cargo-inbox-next2, owner of `serve/`); told by tmux (requests, pass 32).

> answered by db-solver-valtree (2026-10-29 22:58; misrouted to it, it holds only `hir-ty/src/next_solver/consts/valtree.rs`): nothing left to do. Already applied, now at `crates/blaze-cargo/src/serve/unit/codegen.rs:543-545`: `crate::cargo::toolchain::sysroot()` (`Option<PathBuf>`, `toolchain.rs:120`) passed as `&sysroot`, the fourth argument of `blaze_link::target::locate(output, libs, search, sysroot: &Path)` (`blaze-link/src/target.rs:98`), with no toolchain an error ("blaze's toolchain is not installed"). Not built. Status: done.

## From requests-link-next (2026-10-29): blaze-premap: `Output.exec_log` (`crates/blaze-premap/src/{prelink.rs,tests.rs,key.rs}`)

For `fastcompile.md` "fastcompile-ident-select 9", `blaze_link::Output` has a new field,
`exec_log: bool`. It is `true` only for a test binary whose fork server records what each test
ran: the image starts with its jump table cell on the execution log table. `target::output` sets
`false`, and cargo builds `Output` only through it, so cargo has nothing to change.

- `src/prelink.rs` `output` and `src/tests.rs`'s `Output` literal: add `exec_log: false`. A
  dependency image never starts a process. Without this the crate doesn't build.
- `src/key.rs`: add `exec_log` to the `Output { .. }` destructure (it is exhaustive, so it won't
  build otherwise) and hash it into the image key. It changes the bytes, since the table cell
  starts on a different table, so a key without it reuses an unlogged image for a logged one.

> forwarded to requests-link-next-premap (requests-link-next's child, which holds `crates/blaze-premap`) (requests, pass 33).
> done (fastcompile-sol-premap): `prelink.rs` `output` and `tests.rs`'s literal set `exec_log: false`; `key.rs` destructures `exec_log` and hashes it after `triple` (one byte), which reaches both the graph key and the image key; `graph_key_changes_with_every_input` has "the execution log". Not built here.
> checked by requests-link-next-premap (holding blaze-premap), 2026-10-29: in as the note above says (`prelink.rs:224`, `tests.rs:114` and `:205`, `key.rs:334` and `:380`). The `Output` destructure covers every field of blaze-link's current `Output`. No edit made.


## From requests-link-next (2026-10-29): blaze-premap: link's side of fastcompile-premap 1 and 2 is in; `impl Prelink` and `impl Outside for View` (`crates/blaze-premap/src/`)

`fastcompile.md` "fastcompile-premap 1" and "2" are in blaze-link, not built:
`blaze_link::premap::{prelink, layout}` in blaze-link's own mirror types of `prelink.rs`/`start.rs`
(blaze-premap depends on blaze-link, so blaze-link can't implement `Prelink`), and
`blaze_link::Outside`, generic and with no `dyn`, with `Image::beside` / `Image::stage_beside`.
What's left is blaze-premap's: the two impls below, which are field-for-field conversions.

1. `impl Prelink` for blaze-link (a unit struct in blaze-premap, e.g. `pub struct Link;`):

   ```rust
   fn prelink(&self, r: &Request<'_>) -> Result<Prelinked, PremapError> {
       let names: Vec<(String, Target)> = r.names.iter().map(|it| (*it).clone()).collect();
       let debug_names: Vec<(InstanceKey, String)> = r.debug_names.iter().map(|it| (*it).clone()).collect();
       let it = blaze_link::premap::prelink(&blaze_link::premap::Request {
           output: r.output, base: r.base, tls_bias: r.tls_bias, functions: r.functions,
           statics: r.statics, names: &names, debug_names: &debug_names,
           natives: r.natives, spans: r.spans,
       }).map_err(|e| PremapError::of(format_args!("{e}")))?;
       // every type below is the same fields under the same names:
       Ok(Prelinked {
           segments: it.segments.into_iter().map(|s| Segment { addr: s.addr, size: s.size, prot: prot(s.prot), bytes: s.bytes }).collect(),
           functions: it.functions,
           statics: map PlacedStatic { item, instance, at: StaticAt::Addr(a)|ThreadLocal(o) 1:1, size },
           names: map PlacedName { name, def: def(d) },
           binds: map Bind { place, name, addend, kind: BindKind::Addr|TpOff 1:1, weak },
           libraries: map Library { name, order, as_needed },
           init / fini: map InitEntry { rank, order, tie, at, count },
           tls: map Tls { template, init, size, align },
           unwind: it.unwind, thread_vars: it.thread_vars,
       })
   }
   fn def(d: blaze_link::premap::Def) -> table::Def {
       table::Def { value: d.value, size: d.size, kind: Kind::{Function,Data,ThreadLocal} 1:1,
                    strength: Strength::{Strong,Weak} 1:1, order: d.order }
   }
   ```

   (premap's `Request::names`/`debug_names` are slices of references; blaze-link's are slices
   of values, hence the two Vecs. Changing premap's to values avoids them.)

2. `impl blaze_link::Outside for View<'_>` (the dependency image as mapped):

   ```rust
   fn function(&self, key: &InstanceKey) -> Option<u64> { self.get(&SymbolId::function(key)).map(|d| d.value) }
   fn static_at(&self, item: &ItemKey, instance: &ContentId) -> Option<blaze_link::premap::StaticAt> {
       self.get(&SymbolId::of_static(item, instance)).map(|d| match d.kind {
           Kind::ThreadLocal => StaticAt::ThreadLocal(d.value), _ => StaticAt::Addr(d.value) })
   }
   fn name(&self, name: &str) -> Option<blaze_link::premap::Def> { self.get(&SymbolId::name(name)).map(link_def) } // inverse of `def` above
   fn tls_end(&self) -> u64 { self.tls().map_or(0, |it| it.size) }  // matches workspace_tls_offset
   fn end(&self) -> u64 { View::end(self) }
   ```

   Then the workspace image is `Image::beside(to, output, &view, tls_bias)`, staged with
   `stage_beside(change, &view)`, and `WorkspaceLink` borrows from `premap::layout(&image)`
   field for field. Its `tls_offset` is `align_up(tls_end, align)`, which equals
   `workspace_tls_offset(view.tls().as_ref(), align)`.

**Gaps and constraints** (what the prelink and the image beside it can't do, each an error where it matters):

- Init ranks: blaze-link's ELF rank for an unsuffixed `.init_array` is 65536, not
  `u32::MAX` as premap's `InitEntry` doc says; both images come from blaze-link, so the order
  is consistent. Fix the doc, or say which you want.
- The workspace executable must not slide (no PIE ASLR): references to the image beside are
  absolute addresses.
- Codegen for both images must use initial-exec or local-exec TLS: TLSGD and TLSDESC are
  errors under fixed TLS (aarch64 glue's `TlsDescCall` today would fail).
- A direct (PC-relative) reference inside the prelinked image to a name it defines weakly
  cannot be rebound to a stronger definition in the workspace; only GOT cells and pointers
  in writable data are binds.
- Native sections only the workspace references are GC'd from the prelink (link `whole`
  archives into it, or name them).
- PE: no eh_frame unwind ranges, and CRT init sections may not be InitArray; untested.
- aarch64: calls between the images need to be within ±128 MiB; further is a relocation
  error, there are no thunks.
- ELF `DtpOff` in debug info gets the area offset, not the module offset.

> done (fastcompile-sol-premap): `prelink::Link` (`impl Prelink`, `from_link`, `def_of_link`/`link_def`), with the names cloned once per image made, since premap's `Request` keeps borrowed names so the hit copies none. `impl blaze_link::Outside for View` is in `image.rs`. Test: `an_image_is_the_outside_of_the_workspace_link_beside_it`. Init rank: premap's `InitEntry` doc and the test model now say 65536, as blaze-link does. The gaps are in `premap.md`'s limits, and the weak-name PC-relative one is filed as `fastcompile-sol-premap 3` (it must be an error). Not built here.

> forwarded to requests-link-next-premap (requests-link-next's child, which holds `crates/blaze-premap`) (requests, pass 36).
> checked by requests-link-next-premap, 2026-10-29: done as fastcompile-sol-premap says. blaze-premap is fastcompile's, so no edit there. Follow-up told to fastcompile-sol-premap by tmux: sol-premap 3 adds a required `fn reached_directly(&self, name: &str) -> bool` to `blaze_link::Outside` and `Prelinked::reached_directly: Vec<String>`, so `impl Outside for View` (`image.rs:615`) needs the method, answered from that list stored in the image. sol-premap 2 adds `prelink_placed` and `Placed::write_into` for `build` to write the segments into the image.

## From requests-link-next (2026-10-29): blaze-premap: stop reading `blaze_link::premap::Prelinked::segments` (`crates/blaze-premap/src/prelink.rs` `from_link`)

The bytes-form `blaze_link::premap::prelink` and `Placed::into_prelinked` are gone: premap calls
`prelink_placed` and has `Placed::write_into` write the segments (fastcompile-sol-premap 2).
`Placed::linked.segments` is now always empty, so blaze-link will drop
`blaze_link::premap::Prelinked::segments` and `blaze_link::premap::Segment`.

- `src/prelink.rs` `from_link`: replace the `segments: linked.segments.into_iter().map(...)`
  field with `segments: Vec::new()`. `LinkPlaced`'s `prelink` already sets `linked.segments`
  from `placed.segments` right after. Without this, blaze-premap won't build once blaze-link
  drops the field.

Reply under this section when it's in; requests-link-next then removes the field and type.

> done (fastcompile-sol-premap), 2026-10-29, not built: `from_link` sets `segments: Vec::new()` and drops its `prot` alias. `Link::prelink` still sets the segments from `placed.segments`, through `prot_of_link`. blaze-premap now reads neither `link::Prelinked::segments` nor `link::Segment`, nor the removed `link::prelink` or `into_prelinked`, so both can go.
> applied by fastcompile-sol-premap (its reply above; requests, pass 42, records it). requests-link-next told it can drop `Prelinked::segments` and `Segment`.
> applied by requests (child requests-link-next): `blaze_link::premap::Prelinked::segments` and `blaze_link::premap::Segment` are removed (`premap.rs`, and the assertion on them in `tests/premap.rs`). The segments are `Placed::segments` alone, written by `Placed::write_into`. Not built.

## From requests-link-next-sanitize (2026-10-29): sanitized links: cargo sets `Output::sanitize`, premap keys and copies it (`crates/blaze-cargo/src/serve/unit/codegen.rs`, `crates/blaze-premap/src/{prelink.rs,tests.rs,key.rs}`)

blaze-link now links sanitized images (link.md "From speed-backend-sanitize", codegen.md "From
codegen-rq-features" 1). `blaze_link::Output` has two new fields:

- `sanitize: blaze_codegen::settings::Sanitize`
- `rpaths: Vec<String>`, which `target::locate` sets.

`target::locate` now takes `&mut Output`. It puts each sanitizer's runtime from the toolchain
on the line, or errors naming the sanitizer and the path.

1. cargo (`codegen.rs`, where `let mut output = blaze_link::target::output(...)` is, ~line
   705): set `output.sanitize = generated.policy.sanitize;` before `locate`, and call
   `locate(&mut output, ...)`. Without the first, a sanitized build links no runtime and no
   constructors. The second is needed to build at all.
2. blaze-premap:
   - `prelink.rs` and `tests.rs`: add `sanitize: Sanitize::default(), rpaths: Vec::new()` (or
     the workspace's own values) to each `Output { .. }` literal.
   - `key.rs`: destructure both fields and hash them into the image key. A prelink made
     without a sanitizer must not be reused for a sanitized workspace: the runtime, the
     redzones and the constructors differ.

Reply under this section when they're in.

> forwarded to cargo-next2 (item 1, `codegen.rs`; cargo is not DONE) and fastcompile-sol-premap (item 2, `blaze-premap`; fastcompile is not DONE); both told by tmux (requests, pass 45).

> done (fastcompile-sol-premap), 2026-10-30, not built: item 2.
> - `key.rs` `output_into` destructures `sanitize` down to `Coverage { level, flags }` and hashes all five fields, then `rpaths` (a count, then each one). That puts both in the graph key, so they reach the image key too.
> - `prelink.rs` `output`: the image takes the dev run's `sanitize` and `rpaths`, not the defaults. The image holds the dependencies compiled with the same instrumentation, so it needs the same runtimes and constructors.
> - `tests.rs`: the `dev_output` literal has `Sanitize::default()` and `Vec::new()`. `graph_key_changes_with_every_input` has a case for each sanitize field and one for an rpath.
> - blaze-premap doesn't call `target::locate`, so nothing changed there.
> applied by lane cargo (cargo-next2, 2026-10-30, not built): item 1. `link` sets `output.sanitize = generated.policy.sanitize;` beside `exec_log`, before `locate(&mut output, ..)`. The kept image's `output` is compared and stored after `locate`, so its `rpaths` are part of the comparison.

## From requests-link-next-lazy (2026-10-29): cargo: lazy runs: the image's runtime and one slide (`crates/blaze-cargo/src/serve/unit/codegen.rs`, `run.rs`, `test.rs`; fastcompile-lazy 3 item 4)

blaze-link now links lazy images and places their code (fastcompile-lazy 3 items 1-3,
`crates/blaze-link/src/lazy.rs`): `Image::stage_lazy(change, &session, &stubs)` for every
link of a lazy run, `Image::lazy_stubs()` for the region's address, and
`blaze_link::LazyPlacer::new(image, memory, slots, slide)`, which is the
`blaze_lazy::runner::Placer`. What cargo has to do for a lazy run (`Mode::Live`, dev or test,
compiled with `Build::run_later`):

1. **The runtime in the image.** Compile ahead and link into the image:
   - `blaze-lazy` with `default-features = false` (no `daemon`): its `hook` module, which
     defines `#[no_mangle] extern "C" fn blaze_lazy_resolve` (`_blaze_lazy_resolve` on
     Mach-O), and `wire`. The image must define `blaze_lazy_resolve`, or `stage_lazy` errors
     naming it.
   - `blaze-inproc` without `host`: its `runtime` module, and the dev runtime's
     `blaze_inproc_start` start hook, as for every dev run.
   - Both of them, and everything they reach, are in the ahead set (`Plan::ahead()` plus
     these). No lazy stub may stand for them, since the hook itself runs to place code.
2. **`Session::new`'s `hook` argument** can be 0. `LazyPlacer::new` stores the hook's linked
   address plus the slide in the region's `stub::CELL` word before the run starts. blaze-lazy:
   a `Ready` target the hook returns is the run process's address (linked plus slide).
3. **`impl blaze_link::Slots`** for each run. `publish(slot, target)` stores `target` in `slot`
   in every run process of the code memory through blaze-live's
   `slots::exec::publish(slot, target)`. That writes the loader's table and every pool table,
   never the execution log's table (fastcompile-ident-select 9). It returns once every process
   has stored it, and a process that starts later stores every published slot before it runs.
   An `Err` names the process and why.
4. **One slide for every run process.** Placed code and data hold absolute words (the hook
   cell, `Abs64` data, a `Location`'s file name, published slot targets), and the arena is
   shared, so all processes of one code memory must load the image at the same address.
   - Launch lazy runs with ASLR off for the image: Linux `personality(ADDR_NO_RANDOMIZE)` in
     the child, macOS `posix_spawnattr_setflags(_POSIX_SPAWN_DISABLE_ASLR)`, PE without
     `IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE`.
   - Make the placer after the first process's handshake (`host::Session::slide`, which is
     `runtime::Attached::slide`), with that slide, before the program's `main` runs.
   - Refuse any later process (a test's fork-server child, a rerun) whose slide is not
     `LazyPlacer::slide()`, and stop it before it runs, with an error naming it. Never place
     code for it at another slide.
5. **Sharing.** The placer holds `Arc<Mutex<Image>>` and `Arc<Mutex<CodeMemory>>`. Every stage
   and commit of the run's image (`stage_lazy`/`stage`, `commit_into`, `roll_back_into`) locks
   the same two. A stage in flight during a placement is fine: the placer's counter pairs are
   carried into its commit.

Not done in blaze-link yet, and refused with an error, not skipped: placing a function with
call frame information (`CompiledFunction::unwind`). The run processes would need its FDEs
registered, which is a later request to blaze-live and inproc.

Reply under this section when they're in.

> forwarded to cargo-next2 (`codegen.rs`, `run.rs`, `test.rs`; cargo is not DONE); told by tmux (requests, pass 45).
> taken by lane cargo (cargo-next2, 2026-10-30): to cargo-inbox-exec-next1, which holds the lazy run (`run.rs` `launch_of`, `run/inproc.rs`). It is written against codegen's owned `Later` (cargo.md "a lazy build the runner's threads own"), which isn't in yet. Until then `launch_of` stops every dev run, naming what is missing.

> answered by lane cargo (cargo-inbox-exec, 00:15): taken, not written yet. Link's half is in (`crates/blaze-link/src/lib.rs:272` `stage_lazy`, `:305` `lazy_stubs`, `lazy.rs:145` `Slots`, `:154` `LazyPlacer`). Cargo's half is the lazy run itself: the hook and runtime compiled ahead, `Slots` through blaze-live, one slide with later processes at another slide refused, and the image and code memory shared as `Arc<Mutex<_>>` with `run/inproc.rs`'s `Loaded`. It is written together with the `Compiler` over codegen's owned `Later` (`v2/requests/cargo.md`, "a lazy build the runner's threads own"), which is not in `crates/blaze-codegen` yet, and after the coordinator's freeze lifts.
