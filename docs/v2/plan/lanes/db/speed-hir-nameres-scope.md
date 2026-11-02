---
status: plan
summary: speed-hir-nameres-scope's layout of a module's scope (`ItemScope` as packed columns under one name index), its frozen bytes, and the visibility walks, against rustc's resolutions table, rust-analyzer's `ItemScope` and SwissTable; bytes per entry, hashes per lookup and allocations per decoded scope, before and after.
date: 2026-10-29
---
# speed-hir-nameres-scope: a module's scope as columns

Files: `crates/hir-def/src/item_scope.rs`, `per_ns.rs`, `visibility.rs`,
`nameres/frozen.rs`. Nothing was built or run when this was written (the brief forbids it), so
every size below is worked out from the types, and the size asserts in the code are what checks
them. The first build of the tree is the first measurement.

## What a scope was

rust-analyzer's `ItemScope`, as this tree had it: three `FxIndexMap<Name, Item<..>>` (types,
values, macros), an `FxHashSet<Name>` of unresolved names, `FxHashMap`s for the use-import maps,
`legacy_macros`, attribute and macro calls and derives, and `ThinVec`s for the small lists.

- An `IndexMap` entry is a bucket (`hash: u64`, key, value) plus a slot in a SwissTable of
  `usize` indices (8 bytes and a control byte, at 7/8 load). With `Name` 8 bytes and a
  `TypesItem` 44: 60 bytes of bucket and about 10 of table, about 73 bytes a name a namespace
  once growth slack (amortised half again on the bucket vector) is spread out.
- `get(name)` hashed the name three times, once per map, and probed three tables.
- `entries()` (every name, sorted by text) collected and sorted the keys of all three maps and
  the set into a fresh `Vec` on every call.
- Equality was `IndexMap`'s, which ignores order. Salsa keeps the old value when a tracked
  field compares equal, so a scope whose insertion order changed kept its stale order.

## What it is now

Each namespace is a `Column`: a `ThinVec<Entry>` in insertion order, one `Entry` per name,
40 bytes (`const` asserted):

| field | bytes |
| --- | --- |
| `name: Name` (an interned pointer) | 8 |
| `def: Option<salsa::Id>` | 8 |
| `vis: Option<salsa::Id>` (the module of `pub(in ..)`/`pub(self)`, the crate of `pub(crate)`) | 8 |
| `import: Option<salsa::Id>` (the `use`, glob's `use` or `extern crate`) | 8 |
| `tree: u32` (the import's index in its use tree) | 4 |
| `def_tag`, `vis_tag`, `import_tag: u8` | 3 (+1 padding) |

A builtin type has no id; its tag indexes a 19-entry table. `PerNs`, `Item` and `Visibility`
are unchanged and are what every caller sees; an entry is packed on the way in and unpacked on
the way out, both a match on a tag.

One `Index` serves all three namespaces and the unresolved names: an open-addressed table of
`u32` slots, power-of-two size, linear probing, load at most 3/4. A slot is
`namespace (2 bits) | tag (6 bits of the hash) | position (24 bits)`, so a probe rejects
most other names without touching an entry. The hash is FxHash of the name's pointer, one
multiply. Up to 8 names in all there is no index and a lookup scans the entries (a cache line
or two); most modules are that small, and they pay nothing for the table.

The side maps are a `Keyed<K, V>`: a `ThinVec<(K, V)>` and the same `Index`, built past 8
keys. Their equality stays set equality, as the hash maps had. The namespaces compare in
insertion order: same names, same items, same order, which is what the frozen bytes and the
dump show. The index and the sorted order are derived and never compared.

`entries()` walks a sorted order (positions into the columns), built once per scope on first
use into a `OnceLock` (written once, never locked on read) and cleared by any insertion while
the scope is collected.

`name_of`/`names_of` (`find_path` asks one per module it reaches) scan the column in a scope of
at most 8 names. Above that they binary-search `by_def`, the namespace-and-def order of the
entries, 4 bytes each. It is built into a `OnceLock` on first use and cleared when a def is added
or replaced. Within one def the entries keep their column order, so the first name is the same
as before.

`BUILTIN_SCOPE` was an `FxIndexMap<Name, PerNs>` built on first use; it is a boxed slice of
names, one of `PerNs` and the same `Index`, so a lookup is one hash and one probe with no
`IndexMap` bucket in between. `.get` and `.iter` keep their signatures.

### Bytes per name, per namespace it has

| | bytes |
| --- | --- |
| before (`IndexMap` bucket + table, with slack) | ~73 |
| after, scope above 8 names (entry + 4/(3/4) of index, exact capacity) | ~45-47 |
| after, scope of 8 names or fewer | 40 |

A name in two namespaces (a unit struct: type and value) costs two entries and two index slots
before and after.

### Hashes per `get`

| | hashes | tables probed |
| --- | --- | --- |
| before | 3 | 3 |
| after, above 8 names | 1 | 1 probe run, all namespaces at once |
| after, 8 or fewer | 0 | none (a scan) |

### Allocations per decoded scope (a cache hit)

Before: every list went through `Dec::many` into a `Vec` and was then collected into its
`IndexMap` (entries and table: 2), `FxHashSet`/`FxHashMap` (1) or `ThinVec` (1), so a
non-empty map field cost 3 allocations, a non-empty list 2, and `legacy_macros` and the derive
lists a `SmallVec` each beyond that. A module with types, values, macros, two lists and
some imports: about 15-20.

After: each non-empty column, list or keyed map is reserved once to its written length and
filled in place: 1 allocation each, plus one for the name index if the scope has more than 8
names and one per keyed map of more than 8 keys. `SmallVec`s of `legacy_macros`, derive
invocations and derive calls fill without allocating up to their inline size. The sorted order
is not built on a cache hit. The same module: about 6-7.

A length is capped at the bytes left before anything is reserved (each entry is at least one
byte), so a corrupt length can't make a decode reserve gigabytes; `Dec::many` and the module
skeleton do the same now.

## Against the state of the art

- **rustc** keeps a module's names in
  `resolutions: RefCell<FxIndexMap<BindingKey, &RefCell<NameResolution>>>`, keyed by
  `(Ident, Namespace, disambiguator)`, with bindings arena-allocated and shared by reference.
  One table for all namespaces, like the shared index here, but each entry is a bucket of
  key (~24 bytes) and pointer plus a `NameResolution` in the arena (single imports set, binding,
  shadowed glob: ~40-60 bytes) and a `RefCell` borrow check per access. rustc can afford
  `RefCell` because resolution is single-threaded; this scope is read from many threads
  through salsa, so it has no interior mutability on the read path at all.
- **rust-analyzer**'s `ItemScope` is what this tree had (above): three maps, three hashes,
  `entries()` sorting on every call.
- **SwissTable** (hashbrown) probes 16 control bytes per group with SIMD and keeps 7 bits of
  hash per slot. The index here keeps 6 bits of hash next to the position in the same `u32`,
  so a probe reads one word per candidate and rejects 63 in 64 non-matching names; with
  at most 3/4 load and linear probing, the expected run is under 2.5 slots on a hit. Groups and
  SIMD buy little at the sizes a module has (mostly under 64 names), and the slot staying 4
  bytes (half of hashbrown's `usize` index in `IndexMap`) is what keeps the table small.

## Frozen bytes

Unchanged; the format version is not bumped. The encoder writes the namespaces in insertion
order, as before; unresolved names sorted by text and the keyed maps in fingerprint order, as
before. The decoder reads straight into the columns and keyed maps, then `seal()` builds every
index once at the exact size and refuses a scope with a name twice in one namespace or a key
twice in a map (frozen bytes are not trusted to be what the encoder wrote).

Locks in `frozen.rs`: `DIFFERENCES` is a named `Differences` type around a `Mutex`, written
only in `Mode::Check` and `Mode::Verify` (testing modes) and taken once per call, never across a
database call; `Mode::Serve` never touches it. The test `TURN` mutex is test-only. There is no
`thread_local!`.

## Visibility

`is_visible_from` no longer fetches the `DefMap` of the module asked from: both walks read the
ids' own fields. Going up from a block's module is `block.module(db)` (what the block's `DefMap`
calls its parent); going up inside a `DefMap` is the id's `containing_module_inside_def_map`,
which the collector and the frozen decoder set to the same module as `ModuleData::parent`.
`max` and `min` share one ancestor walk over the same field. Each step is a field read where
it was a `def_map` query lookup and a `Modules` lookup.

## Not done

- No measurement. The asserts check the sizes at the first build; the counts above are
  worked out from the code.
- Persisted ast ids in the frozen bytes (raised in `v2/requests/format.md`) need a version bump
  if bytes are ever read across blaze versions; untouched here.
