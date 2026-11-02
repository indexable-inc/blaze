---
status: plan
summary: Lane dbsp's impl tables and trait candidate sets maintained by deltas, as built in hir-ty's method_resolution/delta, the rules that keep them equal to a fresh pull, how body-block impls are tracked, and the write delta handed to the next maintainer.
date: 2026-10-30
---
# dbsp: impls and trait candidates

Design: [../../design/dbsp.md](../../design/dbsp.md) §7, §8 and §12. Code:
`crates/hir-ty/src/method_resolution/delta.rs` and `delta/`. Bounds:
[speed-of-light.md](speed-of-light.md#impls).

## What is maintained

The `impls` maintainer (order 200) runs after name resolution's (order 100). It goes over the
written crates in dependency order and skips the crates the frozen image serves. It reads name
resolution's delta for the write (`LastWrite`): impls and derive impls added or removed with
their positions, traits declared or gone, bindings changed and defs re-read.

For each impl it keeps:

- a key (`ImplKey`): the trait plus the simplified self type `TraitImpls` indexes it under, or
  the self type for an inherent impl, or skip;
- the read set of that key: each `(module, name)` binding and each def the path lowering read.

The mentions indexes map each binding and def to the impls whose read set has it. A changed
binding or re-read def wakes exactly those impls. Each one is rekeyed, and only the rows whose
key changed move.

It publishes three slices per crate, each a salsa input carrying a `ContentId` (domain
`blaze v2 impl set 1`, or `blaze v2 candidate set 1` for candidates):

| slice | per | rows |
|---|---|---|
| `TraitImplsSlice` | `(crate, trait)` | impls and derive impls with their self type, in `Pos` order; the modules whose bodies hold impls of the trait |
| `InherentSlice` | `(crate, self type)` | inherent impls with module and `Pos`; the modules whose bodies hold inherent impls for it |
| `CandidateSlice` | `(crate, item name)` | the traits declaring an associated item of that name |

`SliceDirectory` lists the keys of a crate's slices. It gains a key once and never loses one, so
a reader of a key that has no slice depends on the directory and nothing else. Its
`body_modules` field lists the modules whose bodies hold any inherent impl, which the crate-wide
inherent table merges.

## Readers

- `TraitImpls::for_crate_and_trait`, `in_crate_for_trait` and the blanket lookups read the
  trait slice plus the body-block part.
- `InherentImpls::in_crate_for_self_ty` reads the inherent slice plus the body-block part.
- `TraitImpls::for_crate` and `InherentImpls::for_crate` merge over the directory, for the
  readers that want the whole table.
- `in_crate_trait_value_items` and blaze-items' `traits_with` (through `traits_named`) read the
  candidate slice, and `module_items_named` intersects with it.

`modules_by_trait`, `modules_by_self_ty` and their per-module readers are gone, and so are the
crate-wide body queries (`trait_body_modules`, `inherent_body_modules`,
`inherent_body_module_list`). No reader calls `crate_def_map(..).modules()`; the only def-map
read is `module_rank`, one probe. Frozen crates keep their image paths unchanged.

## The order rule

A fresh pull walks the `DefMap`'s modules in order and, within a module, its impls in item order,
then its derive impls, then its body blocks. Each reader sorts its rows by
`(module rank, part, position)`, where the part is impl, then derive, then bodies. That makes a
maintained table equal to the pulled one entry by entry, not just as a set.

The crate-wide inherent table sorts by `Pos` across slices, so its first-seen ties match the
pull's exactly. The crate-wide trait table is a merge per trait, which is exact because
`TraitImpls` keys by trait.

## The read-set rule

A key depends only on what its read set records. Path lowering of the impl header reads the
bindings it resolves through and the defs it lands on, including type aliases followed to their
targets. Each of those reads is noted when the key is built, and the reads are noted again on
every rekey.

An impl whose module is re-read comes back from name resolution as removed and added, and is
rekeyed from scratch. The random edit-script test (`delta/tests/random.rs`) checks this against
a from-scratch build after every step: renaming an import, retargeting a type alias, toggling a
derive, and adding or removing impls in a body block.

## Impls in body blocks

A module's body-block impls stay in per-module pulled tables, `trait_bodies` and
`inherent_bodies` (`collect_bodies`). Which modules hold them is the maintainer's.

The pure query `body_part(module)` gives three things: the traits and the heads its bodies'
impls have, and whether it has impl holders at all. A module has holders when a body's tokens
show an `impl`, a call of a macro that may write one, or an attribute that isn't inert. That is
the `may_hold` filter the pulled tables use.

The maintainer keeps the part of every module that has holders, and three arrangements:
trait to modules, head to modules, and the modules with any inherent body impl. After a write
it reads the part again for:

- the modules of the defs and impls name resolution touched, which covers every module of a
  written file;
- the modules of the changed bindings, which covers an item a macro wrote into a module whose
  file wasn't written;
- every module that has holders. Its impls' paths and macro calls can resolve differently after
  an edit anywhere, and a holder inside a macro expansion always counts.

The other modules can't change their part without one of these happening. With no holders,
`collect_bodies` lowers no body, so its result depends only on the module's own text and scope.

A crate from empty, a crate graph change, or a change of `macros_writing_impls` (which macro
names may write an impl, crate-wide) reads every module. A module gone from the `DefMap` gets
an empty part; it is checked by rank before any of its fields is read.

A changed part moves the module's rows in the arrangements, and the slices of the keys that
moved are set. A reader merges a slice's own rows with the body tables of the modules the slice
lists, by module rank. So an edit to a body that holds no impls reruns no reader, and an edit
that leaves a body's impls as they were backdates that module's `trait_bodies` with no reader
rerun.

What remains crate-wide on the write side is one validation of `macros_writing_impls` per write
that changed something, plus reading again the modules that have holders.

## The write delta

After each write the maintainer publishes `method_resolution::delta::LastWrite { crates:
Vec<ImplsDelta> }`, read with `db.with_state`, for the maintainers after it (order 300, lane
items' hash columns, design §8b). It is cleared at the start of every write and lists the crates
in dependency order. Each `ImplsDelta` has three fields:

- `krate`;
- `keys_changed`: the impls whose `ImplKey` changed, or that were added or removed, sorted. A
  crate maintained from empty lists the impls it held before as well;
- `traits_changed`: the traits whose associated item names changed, sorted.

A crate the write left alone has no entry. Body-block impls have no key and aren't listed;
their changes show in the slices' body modules. The random test checks every write's delta
against the keys and item names made from scratch before and after it.

## Hashes

- `impl_set_hash(db, krate, trait_)` is the trait slice's `ContentId` (own rows) combined with
  `body_impl_set_hash`: the body tables of the modules the slice lists, in module order.
- `candidate_set_hash(db, krate, name)` is the candidate slice's.
- Identities come from lane item-hash. hir-ty can't depend on blaze-items, so `blaze-item-hash`
  installs two functions once per database with `install_identities(db, impl_, def)`, into the
  singleton input `ImplIdentities`:
  - `impl_` names an impl by its stable identity: signature hash, crate, and ordinal among
    impls of equal signature;
  - `def` names the def of a head or of a trait.

  `impl_identity` and every hash go through them. A hash asked of a database with none installed
  stops with an error: a location is no identity, because a module rename would change it and a
  where-clause edit would not. hir-ty's tests install location identities (`publish::location`),
  which are equal for the same text in two databases.
- Slices hold no hash. `trait_slice_hash`, `inherent_slice_hash` and `candidate_slice_hash` are
  queries over the rows, so a write computes no hash and a database without item-hash never
  hashes.
- `impl_row_hash(db, krate, trait_, self type)` and `blanket_hash(db, krate, trait_)` hash
  the rows that method resolution on one self type can see.

The random test compares every hash with a fresh database built from the final fixture.

## Tests

All under `delta/tests*`:

- random edit scripts: tables, slices (with their body modules), the directory's body modules,
  candidates and hashes, all equal to a from-scratch build; every write delta checked. The
  edits include impls added to and removed from bodies in every module, and a `macro_rules!`
  that a body calls, switched between writing an impl and writing nothing;
- cost: adding one impl gives `Work { ops: 1, woken: 1, rebuilt: 1, keys: 1, slices: 1, .. }`
  at 10 and at 1000 modules (`woken` is the leaf's body part, read again);
- a body edit gives `Work { woken: 1, .. }` with nothing else, equal at 10 and at 1000 modules,
  and executes no `TraitImpls` or `InherentImpls` reader and no body table. A body edit that
  keeps its impls reruns no trait table;
- a body-block impl added gives `Work { woken: 1, keys: 1, slices: 1, .. }`, which is its own
  facts (one body part and `T`'s slice), equal at 10 and at 1000 modules;
- trace: an impl of `T` executes `T`'s table and no other trait's;
- allocation budget: warm reads of the slices allocate nothing;
- the write delta names the added impl, nothing for an item that is no impl, and the trait whose
  names changed.
