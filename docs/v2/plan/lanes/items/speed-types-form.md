---
status: plan
summary: speed-types-form's notes on `crates/blaze-items/src/form.rs`, the form writer every item, closure and coroutine is spelled by. They cover the benchmarks it is held to (rustc's interned-type fingerprints, Unison, batched BLAKE3), what it cost before and after the change, and what is left.
date: 2026-10-29
---
# speed-types-form: the form writer

Notes of `speed-types-form` (a child of `speed-types`, lane [items](../items.md)) on
`crates/blaze-items/src/form.rs` and its tests, as of 2026-10-29. `items-form` wrote the file
and kept tuning it the same day (a `debug` with no `String`, a tracked `loaded_auto_traits` and
`item_visibility`, attribute queries asked only when their flag is set). These notes cover the
rest.

## The hot path

`form::Cx` spells the interface and the body of every item: types, clauses, MIR (the two-pass
`Canon` walk), allocations and macro tokens. `hash.rs` (`interface_form`, `body_form`) and
`facts.rs` read the result. A cold build spells every item of every crate once. A body edit
spells that body again, and with it every type it names.

## Benchmarks

- **rustc**: `WithCachedTypeInfo` keeps each interned type's stable hash beside it, and
  `HashStable` reads it, so a type is hashed once per session however many items name it.
  Query results are fingerprinted, and a result whose fingerprint is unchanged stops the
  red-green walk.
- **Unison**: a definition is hashed once by content and named by that hash everywhere after.
- **BLAKE3**: many inputs per call (`hash_many`), one derived key per domain, and big inputs in
  one `update` so the multi-chunk SIMD path runs.

## Before and after

| | before | after |
|---|---|---|
| type forms | spelled again by each `Cx` (one per item), memoized only for that `Cx` | one tracked `ty_form` per distinct type per revision, backdated across revisions, behind a 64-entry direct-mapped cache per `Cx` |
| buffers per sub-form | a new `Vec<u8>` (digest), `Vec<u8>` (spans), `Vec<ItemId>` and `FxHashMap` each | taken from a per-worker spare list and given back on drop; at most 64 kept, none over 1 MiB |
| refs of finished sub-forms | copied into one `Cx::refs` buffer that only grew | merged straight from the sub-form; no copy, no buffer |
| slot of a ref | one `FxHashMap` entry per ref | a scan of up to 8 refs, then the map |
| where clauses and `dyn` bounds | a `Vec` per list plus 4 more for batching, even for 0 or 1 clause | no `Vec` for 0 or 1 (most items); batched `of_many` for 2 or more |
| first Canon pass | wrote every byte, slot, `format!` and nested anon-const body into a throwaway form | a quiet form: writes, slots, spans, types, constants, allocations and variants are skipped, and only the used-locals bitset is filled |
| Canon columns | `FxHashMap` for block numbers, local numbers and unwinds, `FxHashSet` for uses, a `Vec` per block for successors | dense `u32` columns by arena index, a bitset for uses, a sorted unwind list read with a cursor, successors pushed straight onto the stack; all from the spare list |
| nested bodies | `Vec::contains` per closure (quadratic) | a seen-set |
| counted lists (generics, used locals, unreached blocks, macro tokens, switch targets) | a `Vec` collected only to know its length | the length written after, into a slot written before |
| impl items against trait items | `FxHashSet` of used impl items | a `u64` mask (a set only past 64 items) |
| names for `type_name` / `type_id` | a `String` per path (`key::path`) and a second `Vec` to dedup | the path's interned symbols written in place (`key::path_part`), deduped in place |
| vtables of an allocation | probed by id up from 0 | `VTableMap::tys()` |

## Decisions

- **Tracked `ty_form`, on the merits.** The per-`Cx` map spells `Vec<T>`, `&str` and every std
  type again for each item, and again in every revision. A tracked query spells each distinct
  type once per revision. Its result is compared on re-execution, so an unchanged type
  backdates, and the items that name it check one shared node instead of each holding its own
  copy of the leaf edges. That is rustc's per-interned-type fingerprint plus salsa's early
  cutoff. Each call costs salsa's interning of the `StoredTy` key (a shard `Mutex`) and a memo
  probe. So `Cx` keeps a 64-entry direct-mapped cache of `(Ty, &'db Code)`, 1 KiB with no
  allocation, and a hit makes no call. A `Cx` lives inside one query, so a hit skips only a
  read that query has already recorded.
- **`Cx::ty` returns `&'db Sub`**, borrowed from the memo, so no `Box` of refs is cloned per
  call. Its caller (`facts.rs`, `ty_hash`) passes `&sub` on to `hash::filled`, which takes
  `&Sub`, and compiles unchanged.
- **The cycle guard is salsa's.** `cycle_result` (FallbackImmediate in this salsa) gives every
  query in a cycle the fallback, a `"ty"` form with `Missed::ErrorType`, whichever type was
  asked first. Only programs rustc rejects have a type that contains itself (a coroutine
  holding itself across a yield). The old `spelling` stack depended on which type of the cycle
  was entered first, so the new result is more deterministic.
- **Digests stay bit-identical**, and the domain stays `"blaze v2 form 1"`. Every
  length-prefixed count is written as a placeholder and patched, so the bytes are the ones the
  `Vec`-collecting code wrote. The quiet first pass only finds the used locals, which no write
  it skips can change: types, constants, allocations, variants and generic arguments name no
  local.

## Tests (`src/tests/form.rs`)

- `shared_type_forms_are_the_forms_each_cx_spelled`: over a set of fixtures, every item's
  interface and body, and every closure's code and captures, compared as `(digest, refs,
  missed, spans)` between `Cx::new`, which uses the tracked `ty_form`, and `Cx::reference`,
  which keeps the old per-`Cx` map with its `spelling` stack (test-only).
- `the_dense_walk_is_the_hashed_walk`: for every MIR body of the fixtures, `Canon`'s order,
  block numbers and cleanup blocks against a model built the old way (hash maps and a
  `successors` list).
- `the_quiet_first_walk_finds_the_locals_a_written_one_does`: for every body, the used-locals
  set from the quiet pass against a pass that writes everything.
- `a_path_written_in_place_is_the_path_spelled`: for every item of the fixtures,
  `key::path_part`'s bytes against the length-prefixed `key::path` string `spell_names` wrote
  before.
- `debug_and_counted_lengths_write_the_old_bytes`, `slots_by_scan_are_slots_by_map` and
  `a_reused_buffer_starts_empty`: unit checks of the writer.

The fixtures (`FIXTURES`) write every kind of item, each kind of generic parameter, where
clauses and `dyn` bounds two or more at a time (the batched `of_many` path), matches, loops,
conditional drops, closures in closures, `async` bodies, `impl Trait` and both kinds of
declarative macro.

## What is left

- Each `ContentId::of` derives BLAKE3's key from the domain again. One derived key per domain
  is asked of format (`v2/requests/format.md`, speed-types' request).
- Salsa interns the `StoredTy` argument of `ty_form` under a shard lock. If `StoredTy` were a
  salsa key by its own index (it is a dense `NonZeroU32` already), a miss would take no lock.
  That is the db lane's change in salsa. The per-`Cx` cache keeps it to one call per distinct
  type per query.
- `debug` still formats (`Debug` of an ABI, a `BinOp`, a `MirPhase`). A `u8` tag per variant
  would be faster, but it changes the bytes and so needs the domain bumped. Left for a profile
  to justify.
