---
status: plan
summary: speed pass over blaze-items' keys, hashes and facts (speed-types): item keys in rustc's DefPathHash shape so a key resolves with one lookup, batched preimages in one buffer, names hashed in place, against rustc's DefPathHash, Unison's component hashing and BLAKE3 batching.
date: 2026-10-29
---
# speed-types: item keys, hashes and facts

Files: `crates/blaze-items/src/{key,hash,facts}.rs` and `tests/hashes.rs`. `form.rs` is
[speed-types-form.md](speed-types-form.md); hir-ty's probe, impl tables and layout are
[speed-types-probe.md](speed-types-probe.md). Nothing here was run: costs are counted from the
code per call, not measured.

## Keys (`key.rs`)

**Benchmark.** rustc's `DefPathHash` is `(StableCrateId, local hash)`: the first 8 bytes route a
hash to its crate, so going from a hash back to a `DefId` is one lookup in that crate's table.
BLAKE3's `hash_many` hashes many inputs in one call.

**Resolve.**

- Before: `resolve` probed `crate_items(krate).index` for every crate, then
  `crate_inner_items(krate)` for every crate. A key of a closure in the last crate cost
  2 × crates hash probes, and a key that names nothing cost all of them. Facts resolve every
  subject (`facts::value`), and the scope index resolves every scope.
- After: an `ItemKey`'s bytes 0..8 are its crate key's first 8, and the top bit of byte 8 says it
  is inside a body (an item of a block, a block's module, a closure, an opaque type, a store or
  an anonymous constant). `resolve` does a binary search in `crates_by_prefix` (one tracked,
  sorted `Vec<(u64, Crate)>`), then one probe of `crate_items` or `crate_inner_items` by the
  bit. A miss costs the same as a hit.
- A child key inherits its parent's prefix and bit (`under`). The crate root takes its crate's
  prefix (`shaped`). The other 247 bits are the BLAKE3 hash of the same preimage as before.
- `KEY_DOMAIN` went to `"blaze v2 item key 2"`: every key's bytes changed, though no preimage did.
- `crates_by_prefix` replaces `crate_keys_distinct`. It stops on two crates with one key, as
  before, and also on two crates whose keys share 8 bytes, naming both.

**Index hashing.** `KeyHasher` folds the last 8 bytes of a 32-byte key in one step. Before, it
folded all four words; the first word is now the same for every key of a crate, and the last 8
bytes are as random as BLAKE3 makes them.

**Preimages.**

- Before: one `Vec<u8>` per key and one `String` per name (`as_str().to_owned()`). The batch
  then went into an `FxHashMap<ItemId, ItemKey>` per module, impl, trait and enum.
- After: one buffer per batch (`Batch`: bytes plus `u32` ends), with names written from the
  interned symbol in place while the signature's `Arc` is held. The batch is a sorted
  `Vec<(ItemId, ItemKey)>` (40 bytes an item, no hash table) found by binary search. Each key is
  bit for bit the key of its preimage hashed alone (`a_batch_of_keys_is_each_key_alone`).
- A macro's name is still read from its syntax (`to_node`), as hir's `Macro::name` does: hir-def
  keeps a macro's name nowhere else. Left for hir-def to keep a name in `MacroRulesLoc`,
  `Macro2Loc` and `ProcMacroLoc`.

**`crate_key`.** The cfg atoms are now written into one buffer and sorted as byte ranges. Before,
it built one `Vec<u8>` per atom. The bytes are the same.

**Paths.**

- `path_part(db, id, write)` gives exactly what `h.part(path(db, id).as_bytes())` gives: the
  length, then each segment's bytes in place. It makes two walks and builds no `String`.
- `path` itself is the same walk into a `String`, for messages and `type_name`.
- `facts::name_of` uses `path_part`. `a_path_hashed_in_parts_is_its_string_hashed` checks every
  kind of item.

## Hashes (`hash.rs`)

**Benchmark.** Unison hashes a cycle of definitions as one unit. rustc fingerprints query results
so equal results stop recomputation.

- Before (as the lead left it): flat ref lists, a fast path for a one-member component with no
  self-ref, and a Tarjan whose frames share one edge list. One `FxHashMap` of positions was still
  built per component hash, and almost every component has one member. Every layout hash also
  ran `format!("{:?}", target.arch)`.
- After: a one-member component finds a member by `to == root`, with no map. `arch_name` is an
  exhaustive match returning the `Debug` names, so the bytes are the same.

## Facts (`facts.rs`)

- `name_of` hashes the path in place (above).
- A subject resolves with one lookup (above).
- Vtables are read with `VTableMap::tys` (items-facts did this, on speed-types' request).

## Tests

All in `tests/hashes.rs`:

- `a_key_says_its_crate_and_whether_it_is_in_a_body`: the prefix and the bit, for top-level
  items, a block's item and a closure, across two crates.
- `an_item_of_each_crate_resolves_in_its_own`: three crates with one `S` each. A key with the bit
  flipped or the prefix changed resolves to nothing.
- `a_path_hashed_in_parts_is_its_string_hashed`.
- `every_key_resolves_back` and `a_batch_of_keys_is_each_key_alone` are kept, and still hold.

## Left

- `ContentId::of_many` is not in `blaze-ids` yet (requested in `v2/requests/format.md`).
  blaze-items does not build until it is.
- `of_many` should take a per-domain context key, `blake3::hazmat` style, so each input saves one
  compression.
- A SIMD `hash_many` would need equal-length whole blocks; keys' preimages vary in length.
- A macro's name without its syntax (hir-def).
