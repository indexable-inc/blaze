---
status: plan
summary: speed-unsafe-front's pass over the front end's crates against unsafe.md: root gates, hir-def's lifetime-erasing ids made safe, the pick cache's lock-free table model-checked, salsa's gates requested, and what each kept unsafe use buys.
date: 2026-10-30
---
# speed-unsafe-front: the front end under unsafe.md

The rules are [unsafe.md](../../design/unsafe.md). The crates are split among this agent and
three children, each with its own notes:

| crates | agent | notes |
|---|---|---|
| `hir-def`, `hir-expand`, `base-db` | speed-unsafe-front-def | [speed-unsafe-front-def.md](speed-unsafe-front-def.md) |
| `intern`, `span`, `tt`, `mbe`, `syntax-bridge`, `const-ast` | speed-unsafe-front-intern | [../format/speed-unsafe-front-intern.md](../format/speed-unsafe-front-intern.md) |
| `syntax-tree`, `parser`, `syntax` | speed-unsafe-front-syntax | [../format/speed-unsafe-front-syntax.md](../format/speed-unsafe-front-syntax.md) |
| `hir-ty`'s `method_resolution*`, `traits.rs`, `specialization.rs`, `opaques.rs`, `layout*`, `drop.rs`, `mir.rs`, `mir/`; `blaze-items`; `vendor/salsa*` (by request) | speed-unsafe-front | this file |

The benchmark is the one unsafe.md sets: the Rust standard library's and Rust for Linux's
practice (unsafe only behind a safe API, one operation per block with a `// SAFETY:` comment,
`unsafe_op_in_unsafe_fn`), checked by clippy's `undocumented_unsafe_blocks` and
`multiple_unsafe_ops_per_block`, with loom-style model checking of every lock-free structure.

## How `unsafe_code` treats macros

A root gate's reach decides where allows go. blaze's `unsafe_code` lint is rustc's: declared
without `report_in_external_macro` (`crates/blaze-lint-defs/src/lints.rs`), and
`crates/blaze-diagnostics/src/lints/levels.rs` drops a lint whose span is in another crate's
macro unless the lint sets that flag. So the `unsafe impl`s that salsa's macros,
`blaze_format::record!` and `intern::impl_internable!` expand into a crate never trip that crate's
`deny` or `forbid`. A macro defined in the same crate does, and needs its allow at the use site
or inside the expansion.

## Where the front end stands

| crate | root | unsafe modules |
|---|---|---|
| `syntax-tree` | `deny` | `cursor`, `flat::block` |
| `parser` | `deny` | `lexed_str::scan` (SIMD lexer), `syntax_kind::raw` |
| `syntax` | `deny` | `raw`; `examples/trees.rs` `deny` with `mod counting` |
| `span` | `deny` | `lines::simd` (`hygiene` is safe now: `SyntaxContext::from_u32` is a safe fn) |
| `intern` | `deny` (new) | `space`, `typed` (new); `arena`, `intern`, `intern_slice`, `table` safe outside tests |
| `tt` | `deny` | `storage::owned`, `storage::columns` |
| `hir-def` | `deny` (new) | `item_tree::block`, `hir::col` (and `columns!`' expansion) |
| `mbe`, `syntax-bridge`, `const-ast`, `hir-expand`, `base-db`, `blaze-items` | `forbid` | none |

Every block does one operation and has `// SAFETY:` directly above it, and every `unsafe fn`
has a `# Safety` section. The comment scan finds nothing in these crates outside Rust in test
strings and `quote!` bodies.

## hir-def's ids: unsafe that bought nothing

Most of hir-def's scattered unsafe (about 28 blocks in 11 files) was one of four calls:
`BlockIdLt::to_static`, `BlockId::to_db`, `ModuleIdLt::to_static`, `ModuleId::to_db`
(`crates/hir-def/src/lib.rs`). They were `unsafe fn`s that transmuted a salsa tracked struct's
`'db` away. The lifetime is a `PhantomData` over a `salsa::Id`. Salsa's macros generate a safe
`FromId`/`AsId` for every lifetime (`vendor/salsa-macro-rules/src/setup_tracked_struct.rs`
L253-262), and hir-def already round-tripped tracked structs through it (`nameres.rs`). A stale
tracked-struct id is, in salsa's own words, "unspecified results (but not UB)"
(`tracked_struct.rs` `delete_entity`): a deleted slot panics in `acquire_read_lock`, and a reused
one is a value of the same type. So the `unsafe` marked a logic contract, not memory safety, and
unsafe.md's first rule removes it. The four become safe `fn`s over `FromId`/`AsId` at the same
cost (a `u32` copy), and the call sites lose their blocks. That is what the safety lane's
"From speed-hir-nameres" section asked for, without making the ids interned.

## The pick cache

`crates/hir-ty/src/method_resolution/pick_cache/table.rs` is a fixed-size lock-free map: slots of
a 16-byte hint and entry pointer, entries published by CAS and retired through `blaze_reclaim`.
Before, it had seven blocks, each one op with a SAFETY comment, and a threaded test of racing
inserts, but no model check.

Changes:
- Its atomics and `Mutex` come from `blaze_check::sync` under `cfg(blaze_check)`, as
  `blaze-reclaim`'s do (`std`'s otherwise, unchanged).
- `table/model.rs`: three model tests from two threads, over a two-slot table so that every key
  shares one window. The value holds a tracked cell its readers read and its drop writes, so a
  free not ordered after a read is a reported race. Racing inserts of one key and state give one
  value. A reader of an entry replaced by a newer state reads it whole. A reader of an entry
  evicted from a full window reads it whole, and exactly one older key is evicted.
- The module doc says what the safe version costs: an `Arc` per entry or a lock per slot, so two
  read-modify-writes on a shared line per hit, where this is two loads.
- `mod table` and `mod erase` carry `#[allow(unsafe_code)]`. hir-ty has no root gate yet (it isn't
  this pass's crate as a whole). These are its first two unsafe modules marked, so the gate,
  when it lands, needs nothing more in these files.

`pick_cache.rs`'s `erase` module (four lifetime transmutes of `Pick` and the autoderef steps) is
kept. Every `'db` in those types is a `PhantomData` beside a `NonZeroU32` row of a table that is
never freed (`Ty`, `GenericArgs`, `Canonical`) or a salsa id. The safe version is a lift: a fold
rebuilding the value on every insert and every hit, where this is a move or a borrow. The
differential reference is `tests::on_hit`, which checks every cache hit in tests against a fresh
probe.

The remaining unsafe in these paths is contract impls of unsafe traits defined elsewhere, each
with a SAFETY comment that names why the contract holds: `scope_sets.rs`'s `unsafe impl Erase`
(types with no lifetime) and `mir.rs`'s `unsafe impl GenericTypeVisitable for PlaceElem` (no
field holds a type). `traits.rs`, `specialization.rs`, `opaques.rs`, `layout*`, `drop.rs` and
`mir/` have none. The `#[salsa_value(unsafe(prove(..)))]` attributes in `method_resolution.rs` are
salsa's macro, linted as external.

## blaze-items

`#![forbid(unsafe_code)]` already, with no other target roots (`src/` only). Nothing to do.

## salsa

The comment scan finds no block without `// SAFETY:` in `vendor/salsa*`. Missing: root gates in
all three crates. They are requested in `v2/requests/speed-salsa.md` under "speed-unsafe-front":
- `forbid` for salsa-macros, whose unsafe is all inside `quote!`, and for salsa-macro-rules;
- `deny` for salsa, with an allow on each of its 27 modules holding unsafe;
- `forbid` on the test and bench roots, after making `benches/field_access.rs`'s three
  transmutes safe through `FromId`/`AsId`.

speed-salsa applied all four (2026-10-30 01:37), leaving the trybuild fixtures ungated so that
their expected stderr line numbers don't move.

Salsa's ~450 unsafe sites are upstream's design and sol-salsa is rewriting its hot paths now,
so moving them into fewer modules is not asked for in this pass.
