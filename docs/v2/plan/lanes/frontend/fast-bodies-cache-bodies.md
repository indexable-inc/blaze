---
status: plan
summary: frontend-fast-bodies-cache-bodies — per body of a dependency crate, its InferenceRecord with every id relocated through a table and its borrowck verdict, in one blaze_format part beside frontend-deps' image, read back in another process (crates/hir-ty/src/check_cache.rs).
date: 2026-10-29
---
# frontend-fast-bodies-cache-bodies: a dependency's bodies, checked once

The bodies half of [fast-bodies-cache.md](fast-bodies-cache.md): for each body of a
dependency crate, what codegen reads of its check (the inference result and whether its borrow
check passed), in stable form, in the store beside frontend-deps' image ([deps.md](deps.md)),
and read back by another process. MIR is not stored (agreed with lane codegen, see the parent).
Costs are counted from the code as of 2026-10-29, not measured: nothing builds yet.

Code: `crates/hir-ty/src/check_cache.rs` (the API), `check_cache/ids.rs` (relocation and the
portable form of each id), `check_cache/part.rs` (the part), `check_cache/tests.rs`, and
`crates/hir-ty-borrowck/src/served.rs` (MIR and the gate, which hir-ty cannot reach). Edits to
files of other lanes are requests: `v2/requests/db.md` and `v2/requests/frontend-deps.md`, each
under `## frontend-fast-bodies-cache-bodies: ...`.

## Relocation in hir-ty's codecs

Every id the inference record and its type tables write goes through one function,
`check_cache::ids::id_bits::<T: Relocate>`, and every id they read through `id_of::<T>`.
`Relocate` is a trait with `const KIND: IdKind`, implemented by the ten hir-def item ids,
`BuiltinDeriveImplId` and hir-ty's five interned ids (opaque types, closures, coroutines,
coroutine closures, anonymous constants). Inside `ids::relocating(&mut table, f)` (a thread
local, as `hir_def::hir::persist::relocating`; scopes do not nest) an id is written as its
index in the table plus one, so 0 is still "no id"; outside a scope the bytes are exactly
today's (`as_bits`). Reading inside a scope looks the index up and checks the kind.

- `infer/persist.rs`: the helpers `id_bits`, `variant_*`, `candidate_*`, the trait reference of
  the type encoder, `Sink::id`, `Sink::generic_def`, the private-item diagnostic, and their
  readers (`InferenceRecordRef::id`, `Source::id`, `Source::generic_def`) become typed;
  `salsa_id` and `Source::salsa` go.
- `next_solver/persist.rs`: `Sink` gains `id::<T>` and `Source` `id::<T>()`. The in-memory
  pair (`Words`, `Row`, the debug round trip of every interned value) writes `as_bits` always,
  so interning inside a scope is untouched; the persisted pair (`Children`, `Reading`) goes
  through `id_bits`/`id_of`. `SolverDefId`'s codec writes its tag and then a typed id: the same
  words in the same order.
- `lower/persist.rs`: the diagnostic words the inference record embeds (`put_generic_def`,
  `AmbiguousAssociatedItem`) go through `def_bits`; the cold batches' own ids keep `id_bits`.

A scope is a thread local, so a query the database runs inside one would see it: a persisted
table it read (a dependency's signature, say) would be read as the scope's indexes. The places
a read asks the database are wrapped in `check_cache::outside`, which suspends the scope:
`AdtDef::new` in `TyTableReader::open` and `TraitRef::new_from_args` in the record's trait
references. Any other database call found inside a scope gets the same wrapper; a reader that
takes its table as an argument, not a thread local, would remove the hazard and is the better
end state once the codecs are one.

### What the second database must order again

A read in another database gives other salsa ids, and two things are ordered by them:

- `InferenceResult::type_of_opaque` is a `SortedMap` by `InternedOpaqueTyId`: read inside a
  scope its entries are sorted again before `from_sorted`.
- A `dyn` type's predicate list is ordered by lowering (`lower.rs`: principal, projections by
  their associated item's id, auto traits by trait id). Two lists with the same predicates in
  another order are two interned types, so a list read inside a scope is put back in that
  order: a stable sort that keeps the principal first, then projections by id, then auto traits
  by id. Outside a scope the list is read as written. Projections of one associated item keep
  the writer's relative order; lowering sorts them unstably, and the two orders agree only
  because the elaboration that lists them is not ordered by id.

## The portable table

A body's ids, in the order the record first names them, each as a kind byte and its location:

| kind | written as |
|---|---|
| a hir-def item (`Function` … `EnumVariant`) | its index in the crate's portable table: `hir_def::deps::PortableIds`, written once per part by frontend-deps' frozen encoder (items in blocks through their body owner) |
| `BuiltinDeriveImpl` | the ADT (an entry), the trait's index in `BuiltinDeriveImplTrait::ALL`, the derive attribute's item-tree index, the derive's index |
| `OpaqueTy` | which (return-position or type alias), the function or alias (an entry), the `impl Trait` index |
| `Closure`, `Coroutine`, `CoroutineClosure` | the owner (a body as an entry, or an anonymous constant as an entry), the expression's raw index, the `ClosureKind` |
| `AnonConst` | the owner (`ExpressionStoreOwnerId` as a tag and an entry), the expression, the type (a place in the body's own small type table, `locs`), `allow_using_generic_params` |

`ExprId`s are raw indexes: the body store is the same in every process that lowers the same
text. Locations name other ids, which are appended to the same table (a work list), so the table
is closed.

**Read order** (strata): hir-def items from the crate table and derive impls; then the `locs`
type table; then opaque types and anonymous constants; then closures (whose owner may be an
anonymous constant); then the record, under the full table. A closure's expression is checked to
be a closure of the right kind before `InternedClosureId::new`, whose debug assertion would
otherwise be the first to see a bad record.

### Split by shape

What has no portable form leaves **that body out of the part**; it does not fail the part. A
body left out is inferred by the consumer as today: that is the uncached path, not another
answer, so it changes only speed. A part is refused whole only when its crate table does not
encode or two bodies share a key. Left out:

- a body with no signature key (an item in a block: `crate_body_owners` lists none anyway);
- a body whose hir-def ids frozen's encoder refuses (checked per body, so one does not take the
  crate's other bodies with it);
- a body whose anonymous constant's type names a hir-ty interned id (a closure, an opaque type,
  another anonymous constant): the `locs` table is opened with hir-def ids alone, which keeps
  the strata acyclic. Lowering gives such a type to no anonymous constant in practice (their
  types are `usize` and const parameters' types).
- Constants of signatures (`infer_anon_const_body_query`) are not served: the consumer infers
  them; they are small and few.

Each left-out body is listed in the part by its key with why (`PartRoot::left`, `left_ends`,
`left_why`; `PartRef::left_out`) and logged (`tracing::debug!`), and the count of bodies served is
in the part (`PartRoot::keys`), so the parent's `-v` can show the share. A body in a block has
no key and no entry. A served crate's body whose key the part neither records nor lists is a
panic naming the crate and the body (`served_body`, through `corrupt`): the writing database
gave the body another key, and inferring it instead would hide that, so it is not the uncached
path. `open` refuses a key that is both recorded and left out, left-out keys out of order, and
reasons that do not end in order or are not UTF-8. Tests:
`a_body_neither_recorded_nor_left_out_is_an_error` (a body the writer did not have: both
`served_inference` and `served_passed` panic, the other bodies are still served) and
`a_left_out_body_is_listed_with_why_and_not_served`.

## The part

One `blaze_format` buffer per crate, schema `check_cache::part::SCHEMA` (which covers the
inference record's schema, the type table's, frozen's encoding and the key domain):

- `PartRoot { keys: Slice<ContentId>, places: Slice<Place>, defs: Slice<u8>, kinds: Slice<u8> }`:
  each body's owner by its signature key (`hir_def::deps::body_key`), sorted; where each body's
  buffer is, at an offset aligned to 16; the crate's portable hir-def table and the kind of
  each of its entries.
- Each body is a buffer of its own (`BODY_SCHEMA`):
  `BodyRoot { locs: u64, record: Slice<u8>, ids: Slice<u8>, passed: u32, reserved: u32 }`, with
  the inference record's buffer at an aligned offset inside it and the `locs` type table written
  into it.
- `open` checks the header, the schema, the four columns, that keys are strictly increasing and
  that each place is aligned and inside the part: after it, finding a body is a binary search
  and a slice, nothing allocated (`find`, tested with `blaze-alloc-count`).
- `key(image, mode)`: the image's key as filed and the borrow checker's mode, under
  `BODIES_DOMAIN`, with `PRODUCER` (every schema above).

## The hooks (requests)

- `infer_body_query` returns `check_cache::served_inference(db, def)` when there is one.
- `hir_ty_borrowck::gate::checked` returns `Checked` without running the borrow check when
  `check_cache::served_passed(db, def)`. A body served with a failed verdict is checked again, so
  its errors are rustc's.
- `set_served` takes the parts of the crates served from images, opened by `open` first; a part
  that does not open there is a panic naming the crate. A served record that does not read is a
  panic naming the crate and body (the store checked the bytes against their key, so an encoding
  wrote what it does not read): never a quiet re-inference.
- **Inference reads no lint level** (`rg` of `lint`/`allow`/`warn` over `crates/hir-ty/src/infer*`:
  only `#[allow]` attributes; `SOLVER_DEBUG` is a debug printer), and the verdict is whether the
  borrow check found an error, which lint levels do not change: the key needs no lint flags.

## Speed bound per served body

For a body of 5 KB of record with 100 distinct types and 30 distinct definitions:

- find: a binary search of the key column (11 compares for 2,000 bodies) and the body's root
  check: about 0.1 µs;
- the table: 30 entries, a hir-def entry an index into the crate table decoded once per crate
  (one probe each), a hir-ty interned id one interning: about 30 × 30 ns ≈ 1 µs;
- the record: `InferenceRecord::read` checks its columns in place (about 0.3 µs for 5 KB) and
  `to_result` interns each of the 100 types once, at 40 to 50 ns per interning (a hash and a probe
  of a sharded table), about 4.5 µs, plus copying the columns (5 KB) back into their vectors,
  about 0.3 µs.

About 6 µs, against about 30 µs to infer it. The floor is the interning: a result that holds
this database's interned types has to make each of them here, whatever the format.

## Prior art

rustc keeps a dependency's typeck results only for what it encodes in `.rmeta` (MIR of generic
and `#[inline]` items), with `DefId`s through the crate's `DefPathHash` table, the same split
this part makes between hir-def items (frontend-deps' portable table) and local ids (raw
indexes). rustc's incremental on-disk cache relocates `DefId`s the same way. The difference: a
blaze part is keyed by content and shared by every checkout.

## Proof (`check_cache/tests.rs`, `hir-ty-borrowck/src/served.rs`)

- `a_part_reads_back_in_another_database`: a part written in one database and read in a second
  one from the same fixture, warmed in another order so its salsa ids differ (asserted); every
  served body gives the inference result the second database infers itself.
- `outside_a_scope_a_record_is_todays_bytes`: a record written outside any scope is the bytes of
  the same record written by today's code path (`to_record` then `to_bytes`), and the ids in it
  are `as_bits`.
- `a_served_body_lowers_to_the_same_mir` (borrowck crate): the MIR of a served body equals the
  MIR lowered from the second database's own inference.
- `a_served_verdict_skips_the_borrow_check` (borrowck crate).
- `an_edit_to_the_dependency_moves_the_part_key`.
- `damaged_parts_are_refused_within_bounds`: every truncation, and a flip of each header byte and
  each byte of the columns, is refused by `open` or by a body's read; nothing panics.
- `finding_a_record_allocates_nothing`.
