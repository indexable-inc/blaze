---
status: plan
summary: Where names and positions reach codegen's keys and output (queue 00957, 01008, 01003, 01004, 00043, 01028), what holds today, and what each owner changes so that a rename or a move reruns only the presentation layer and the binary stays byte-identical to a fresh build.
date: 2026-10-31
---
# Names and positions in codegen (codegen-names)

Design: [item-hash.md](../../design/item-hash.md) sections 3.3, 3.7, 12 and 13.

## Every codegen key that holds a name or a position today

| key | made in | holds | stale after a rename or move? |
|---|---|---|---|
| a body's code (`Hashes::code`) | `hir_ty_share::code::with_spans`, through `v2/hashes.rs` | the items lane's code hash, **the names of its bindings** (`NAMES_DOMAIN`, until queue 01386 drops it), and each span by (table, index) | no: a local's rename is a new key, so it reruns MIR codegen (the cost 00957 removes); positions are by index, never by value |
| a function's side table (`CompiledFunction::side`) | `v2/side.rs` `encode` | each named local by its binding's index in its store, its type, slot and scope: no name since 01351 (version 4) | no |
| an instance's arguments (`Described::args`) | `v2/frontend.rs` `args_of`, `shared_args` | each type by `hir_ty_share::identity`, which holds each item's `ItemKey` (crate, parent, name) | no: a renamed or moved type is another instance |
| a generic's share key | `hir_ty_share::walk` | `type_name`/`type_id` arguments by full identity (`Class::Identity`), transitively through callees | no |
| a const's code, a static's data | `blaze_items` form `const_value`, `v2/statics.rs` `value_id` | the evaluated value | no (00043) |
| a span table's anchor | `v2/hashes.rs` `anchors` | the body's store key: no position | no: the table's bytes are made at each link from the current database (`v2/linked.rs`) |
| a declaration's anchor | `v2/hashes.rs` `declaration_table` | the function's item key | no |
| an instance's identity (`Described::identity`) | `v2/frontend.rs` `identity` | item key, closure key | no |
| test selection's `Bound` (`ExecTable::fns`) | `blaze-codegen/src/exec.rs` `placed` | the build's bound, the closed value of each static the code names, each span table its positions, data, lines and statics name, by content | no, since 01028: before, positions were one byte and a static by its item only |

## The execution table's speed (01028)

Per relink of a test binary: one BLAKE3 hash per span table (the tables are a few hundred
bytes, so about 0.3 µs each), one walk of the statics (Tarjan, linear in their references),
then per function one probe per anchor into an open-addressed table keyed by the anchor's
first 8 bytes (uniform), a binary search per static it names and one hash of about 100 bytes:
about 5 + 11 ns per (function, anchor), split over every core with rayon as `versions` is.
For 100 000 functions naming 3 anchors each on 16 cores that is under 0.5 ms; the walk over
`Built::functions` reads only their relocation targets and lines.

Names and positions that reach the output and are not cached by any key, so are always the
current database's: an instance's debug name (`Described::name`, made at each describe and
given to the link), its symbol (`symbol_of`), each `Target::Position` and line row (filled at
link from the span tables `link_inputs` encodes), and file names (`ItemHashes::file_name`).

So no key misses a name or position today; what 00957 asks is that the binding names leave
the code key. Since 01351 no template holds one, and queue 01386 takes them out of the key.

## The split (00957, 01008, 01004)

- **Binding names as data, as positions already are.** A template's side record
  (`v2/side.rs` `Variable::binding`, done in 01351) holds the local's binding by its index in
  its owner's store, never its name, and the debug record the link (or the daemon, 01004 and
  queue 01354) writes reads the name from the current database by (owner, binding), as
  `v2/linked.rs` reads positions. Which locals are told (`v2/backend.rs` `named` leaves out
  those lowering names `<..>`) follows from the code, as no written name starts with `<`.
  Then `hir_ty_share::code::with_spans` drops `NAMES_DOMAIN` (queue 01386), and a local's
  rename reruns no MIR codegen. A coroutine's identity holds its code, so it then keeps a
  local's rename too: no cached output spells a coroutine's locals.
- **A debug record is keyed by (code hash, presentation of every item it spells).** The body's
  own `blaze_item_hash::presentation` and the presentation of each item it names (callees,
  argument types, impl self types: 01008). Built after the edit, never waited on (01004).
- **Symbols**: v2 links internal functions by key, so only exported symbols (`#[no_mangle]`,
  `#[export_name]`, std's linked names) and debug names are spelled; both are made fresh per
  build.

## Locations as data (01003)

v2 holds no location value in code: `Location`s, `#[track_caller]` callers' locations and
`file!()` are `Target::Position` relocations into records the link fills from the span
tables. Since 01352 `line!()` is one too where the code runs: MIR lowering
(`hir-ty-mir` `lower.rs` `lower_line`) reads it as `Location::caller().line` at the literal,
in a function that is neither `#[track_caller]` (where `Location::caller` is the caller's) nor
`const`, outside `const { .. }` blocks and borrows rustc may promote, which are values. The
`Location` a call there gets is where the literal is written, as rustc's `line!()` is (both are
the expansion's cause), so the program prints what a fresh build prints. Elsewhere the line
stays a literal in the MIR and so in the code hash, which is correct and only rebuilds more.

`column!()` stays a literal: a `Location`'s column is rustc's display width, `column!()`'s
counts chars, so the record cannot give it; and a move above a body changes no column.
