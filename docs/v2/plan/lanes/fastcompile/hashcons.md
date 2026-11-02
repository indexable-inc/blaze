---
status: plan
summary: blaze v2 lane fastcompile, idea 5 (hash-consing of query values): crates/blaze-hashcons, its sharded append-only interner with lock-free reads, fingerprints computed once at intern, persistence by content, the hooks in blaze-query and salsa, and the state of the art it matches or beats.
date: 2026-10-29
---
# fastcompile: hash-consing of query values (`crates/blaze-hashcons`)

Idea 5 of [compile-time.md](../../design/compile-time.md): equal values are stored once, so
comparing two is comparing IDs, and a value's fingerprint is computed once, when it is
interned. Early cutoff no longer hashes whole values.

## What it is

- **`Consed`**: a type that can be hash-consed. It names its hash domain
  (`blaze_ids::Domain`, `"blaze v2 <what> <n>"`) and feeds its content to a
  `blaze_ids::Hasher`: its own bytes, and each interned part by its content ID
  (`Interned::hash_into`), never by its index. So a composite's content is a Merkle hash of its
  parts: interning one costs its direct parts, not the whole tree under it.
- **`HashCons<T>`**: the interner. `intern(value)` computes the value's `ContentId` once
  (`content_of`) and returns an `Interned<'_, T>`; `get`, `handle` and `lookup` go from an
  `Idx<T>` or a `ContentId` to the value. It also implements `blaze_ids::Interner<T>`, checking
  the content ID it is handed against the value's.
- **`Interned<'a, T>`**: one pointer to the node (content ID, index, value). `==` is pointer
  equality; `Hash` writes the 128-bit fingerprint; no `Ord`; `Deref` to `T`. `idx()` gives the
  dense `blaze_ids::Idx<T>` for columns of this run.
- **Persistence by content** (`persist.rs`): `Interned<'static, T>` is `Persist` as `T`'s own
  bytes and schema. `Load` makes a value again from its bytes, interning its parts again;
  `HashCons::load` / `load_verified` intern one value, `write_pack` / `load_pack` /
  `load_pack_verified` a whole interner.

No second fingerprint type and no second ID type: the content ID, its first 128 bits as
`blaze_ids::Fingerprint` (the same as `ContentId::fingerprint` everywhere), and `blaze_ids::Idx`.

## The table

- **Shards**: `blaze_ids::shard_count()` of them (a power of two, at least 4 per core), picked
  by fingerprint bits 0..32, each padded to 128 bytes.
- **Per-shard arena**: append-only chunks, chunk `k` holding `64 << k` slots, so a node never
  moves and a handle lives as long as the interner. A slot is a `OnceLock<Node<T>>` set once
  under the shard's lock.
- **Per-shard index**: an open-addressed table of `AtomicU64`s, each the fingerprint's bits
  32..64 as a tag and the arena place plus one. A probe starts at bits 64..128 and loads a node
  only when the tag matches. A table at half load is copied into one twice its size under the
  lock and published with one release store of its level; tables it outgrew stay until the
  interner is dropped (at most as much again), so readers never wait for a resize.
- **IDs**: `Idx` raw = arena place shifted past the shard bits, ored with the shard. An index
  to `&T` is index arithmetic and two acquire loads: no lock, no shared write.
- **Reads** (`get`, `handle`, `lookup`, and an `intern` of a value already there): acquire
  loads only. **Inserts** of a new value lock its one shard to look again, append and publish
  the entry with a release store, and run no code of `T` under the lock (the comparison and
  drop of a value found on the second look happen after it is let go of).
- **Collisions**: every fingerprint match is checked on intern. Equal fingerprints with
  different content IDs (a 128-bit collision) or equal content IDs with unequal values (a
  `hash_content` that misses something `Eq` compares) stop the process naming the type, since
  either would alias two values in every memo that compares fingerprints. The odds of a real
  collision among n values are about n²/2¹²⁹ for the fingerprint: for 2³² values in one
  interner, 2⁻⁶⁵. The check still runs, as it costs one 32-byte compare and one `Eq` per hit,
  and the `Eq` of a value of interned parts is a few pointer compares.

The crate has no unsafe code of its own (`#![deny(unsafe_code)]`; `record!` writes the pack's
`unsafe impl Record`, allowed on that one module). A slot without `OnceLock` (a
`MaybeUninit` published by the shard's length) would save 8 bytes and one load per node; it is
not taken without a measurement that says it pays ([unsafe.md](../../design/unsafe.md)).

## Determinism: where the types enforce it

IDs depend on the order threads interned in; content IDs and fingerprints don't. The rule of
[crates.md](../../reference/crates.md) (a dense index never goes into a hash, a key or an
output) holds by types:

- `blaze_ids::Hasher` takes bytes and content IDs only, so a content hash can't include an
  `Idx` but by spelling out `Idx::raw`; `Consed::hash_content` feeds parts through
  `Interned::hash_into`, which feeds the content ID.
- `Idx` is neither `Serialize` nor a `blaze_format::Record`, so it can't be written into CBOR or
  a format buffer. `Interned` persists as its value's bytes.
- `Interned`'s `Hash` is its fingerprint, the same in every run, so a salsa value fingerprint or
  a map's hash of it is deterministic. It has no `Ord`; ordering goes through
  `Interned::content`, and `HashCons::sorted` and `write_pack` are in content ID order.
- What remains by convention: `Idx`'s own `Hash` and `Ord` (blaze-ids', for maps and columns of
  one run). Iterating a map keyed by `Idx` into an output would leak the order; nothing in this
  crate does.

The determinism test interns the same values from 8 threads in 8 shuffled orders and from one
thread in reverse, and checks equal content IDs, fingerprints and pack bytes.

## The hooks

- **blaze-query** (request `fastcompile-ident-hashcons 1`, `crates/blaze-query/src/lib.rs`,
  `Query::fingerprint` and `run_query`): early cutoff there already compares fingerprints
  (`Database::verify`); the whole-value cost is the default `fingerprint`, which serializes the
  value a second time and BLAKE3s it. Handed the bytes already written, the default writes
  once (cheaper than today for every query), and a query with `type Value =
  Interned<'static, T>` returns `value.fingerprint()`, computed once at intern.
- **salsa** (request `fastcompile-ident-hashcons 2`): no change. `backdate_if_appropriate`
  (`vendor/salsa/src/function/backdate.rs`) calls the generated `values_equal`
  (`old_value == new_value`), which for an `Interned` output is one pointer compare, and on an
  evicted value `Fingerprint::of_value`, which for one hashes 16 bytes. A tracked function opts
  in by its output type; the rest are unchanged.
- **Lifetime**: an interner lives as long as the database whose values it holds and is leaked
  with it (as `crates/intern`'s are), so `Interned<'static, T>` is a `'static` salsa value. At
  the settle swap the image's values are made again by content (`load_pack`), so a long-lived
  daemon's interners don't grow without bound across generations.

## Which values to hash-cons

Interning a value pays where equal copies are common, where it is compared or hashed again
and again, or where its content ID is wanted anyway; it costs one BLAKE3 of the value and a
probe at every make. By that measure, as of 2026-10-29:

| Value | Where | Owner | Request |
| --- | --- | --- | --- |
| every blaze-query memo of a `SHARE` kind, as bytes | `crates/blaze-query` | db | 3 |
| `Masked` (a function's masked IR) | `crates/blaze-codegen/src/fold.rs` | codegen | 4 |
| codegen's `TEMPLATE`, `RELEASE`, `MASKED`, `FOLDED`, `STATIC` values | `crates/blaze-codegen/src/query.rs` | codegen | 4 (by 3) |
| `Layout` (`layout_of_ty_query`, `layout_of_adt_query`) | `crates/hir-ty/src/layout.rs` | items | 5 (applied; callers in `v2/requests/items.md`) |

- **Memo bytes** (request 3) are the general case: one copy of equal values whichever view or
  key made them, keyed by the schema and fingerprint each memo has anyway, so it needs no
  interner of this crate. It is where idea 2's shared generic code lands: two instances whose
  templates are equal keep one buffer.
- **`Masked`** (request 4): `fold::groups` BLAKE3s every candidate's masked IR on every build.
  Interned in the daemon's one `HashCons<Masked>`, with the same domain and feed as today so
  every content ID is unchanged, a shape is hashed once per distinct shape ever seen.
- **`Layout`** (request 5): rustc's own interned value. Each read clones an `Arc<Layout>`
  today, a shared counter write on the read path; interned, a read is a pointer copy and
  equality a pointer compare. `hash_std` feeds `rustc_abi`'s derived `Hash` with `usize` fixed
  at 64 bits, so a foreign type needs no hand-written `hash_content`.

Not hash-consed, and why:

- `ItemTree` (`file_item_tree_query`), `Body`: one per file or body, rarely equal to another,
  and salsa's cutoff already compares them only when they are made again. Interning would hash
  every one for no sharing.
- `InferenceResult<'db>`, `MirBody`: they borrow the database (`'db`) or hold interned
  `crates/intern` types whose `Hash` writes addresses, so a content ID would be neither
  `'static` nor deterministic. Their equal parts are already interned by `crates/intern`.
- Macro expansions (`parse_macro_expansion`): keyed by the call, and equal expansions of
  different calls carry different spans.
- Signatures (`::of -> Arc<Self>`): small, one per item; an `Arc` clone per read is the same
  cost as `Layout`'s but there are few reads per item. Worth measuring after request 5 lands.
- `blaze-units`' plans and manifests: one per workspace; request 3's `SHARE` gives what-if views
  one copy if cargo wants it.

## The one-copy hook in blaze-query (request 3)

A memo's value is `Arc<Value>` in a `ValueCell`; today `resolve_held` makes a new one for every
execution. With `Query::SHARE`, the database keeps `values: Map<Arc<Value>>` keyed by the
value's kind, schema and fingerprint: an execution whose bytes are already kept publishes the
kept `Arc` (after a byte compare: a mismatch is a non-deterministic `Persist` or a collision,
and stops naming the query), so every view and key holding that value holds one buffer.
`collect` drops an entry no cell holds any more. The cold packs and the settle image write
each distinct value once, for every kind, so memos with equal values map the same bytes after
a swap. Nothing about a value's fingerprint, cutoff or bytes changes, so the hook can't change
what any query answers.

## The state of the art, and how this compares

- **rustc's interners** (`CtxtInterners`, `InternedSet` over a `ShardedHashMap`, values in a
  `DroplessArena`, `Interned<'tcx, T>` compared and hashed by address): lookups lock their shard
  in the parallel front end, and the address hash is meaningful in one process only. Here a
  lookup takes no lock and writes nothing, and the hash is the content fingerprint, the same in
  every run.
- **rustc's cached stable hashes** (`WithCachedTypeInfo::stable_hash`, kept beside interned
  types under incremental compilation) and **its red-green graph** (`DepGraph`, `hash_result`
  over the whole result on each execution, `try_mark_green` comparing dependencies'
  fingerprints): rustc computes the fingerprint once per type but hashes every query result
  again. Here every hash-consed value, of any type, gets its fingerprint once at intern, a
  composite from its parts' content IDs, and blaze-query's cutoff reads it.
- **salsa's interned structs** (`#[salsa::interned]`: sharded tables behind locks, IDs valid
  within one database and revision-tracked): IDs to values are lock-free there too, but a
  lookup by value takes its shard's lock, and IDs can't be persisted. Here a lookup by content
  is lock-free, and persistence goes by content.
- **The `hashconsing` crate** (a weak table `HashMap<T, Weak<…>>` with a unique `u64` id per
  value, `Arc`-counted handles, freed when the last handle drops): each handle clone is a shared
  counter write, and ids hash differently per run. Here a handle is a plain pointer copy, with
  no count, and lives as long as the interner; memory is reclaimed by generation (the settle
  swap), not per value.
- **egg's e-graph hash-consing** (`memo: HashMap<L, Id>` over e-nodes whose children are
  union-find `Id`s, canonicalized when rebuilding): single-threaded, with ids that depend on
  insertion order. Here children are named by content ID (Merkle, as Unison names definitions),
  so a composite's identity is the same in every run and on disk; there is no congruence
  closure, as query values are never rewritten.
- **Concurrent hash tables**: folly's `ConcurrentHashMap` (segment locks for writers, hazard
  pointers for readers), `papaya` (lock-free with incremental resizing and epoch reclamation via
  `seize`), `scc` (`HashIndex`: lock-free reads, EBR). They pay for deletion: hazard-pointer
  stores or epoch pins on every read. An append-only hash-cons never deletes, so a read here is
  acquire loads and no store of any kind, and a resize copies under the shard lock while
  readers go on probing the old level. Sylvan's lock-free unique table for BDD nodes inserts by
  compare-and-swap; here a miss locks its one shard, which also serializes the equality check of
  a racing equal value (a CAS insert would need a node published before its check). That is the
  measurement to make next: under a many-thread cold intern, whether shard contention shows
  (shards are 4 per core).

## Tests (`crates/blaze-hashcons/src/tests.rs`)

- Unit: one handle and index per value, both ways between index, content and value, chunks
  cover every place, tables grow and keep every value, values never move.
- Property: 20 000 random leaves and pairs (a composite of interned leaves) from a small domain,
  checked against a sequential model (a `BTreeMap` from value to its first handle):
  `intern(a) == intern(b)` exactly when `a == b`.
- Stress: 16 threads interning one value 2 000 times each and 10 000 shared values in rotated
  orders: one index per value, every thread's indices naming its values.
- Collisions: a type whose `hash_content` ignores a field stops on its twin; the `Interner`
  impl refuses a content ID that isn't the value's.
- Persistence: a handle's bytes are its value's; a value loads and loads verified (a wrong key is
  `FormatError::Hash`); a pack loads into fresh interners (pairs through their leaves' interner)
  and writes the same bytes again; a pack refuses another schema, a wrong key, and a value whose
  content isn't its entry's (before interning it).
- Determinism: 8 threads in shuffled orders against one thread in reverse give the same content
  IDs, fingerprints and pack bytes.
- `hash_std`: writes integers little-endian at fixed width (`usize` as 64 bits) and `str` as
  std does, checked byte for byte against a hand feed; a derived-`Hash` enum interns as its
  value.
- Allocation budget (`blaze-alloc-count`): an intern of a value already in, `get`, `handle`,
  `lookup` (found and not), and comparing and hashing handles allocate nothing.

## Speed of light

Nothing here was run: costs are counted from the code and the machine, as in
[share.md](share.md#speed-of-light) and
[speed-of-light-infer-relate.md](../db/speed-of-light-infer-relate.md), whose machine this
uses. That is an Apple M5 Max performance core at about 4.5 GHz with sustained IPC about 3.5,
L1 about 3 cycles, L2 about 16, DRAM about 100 ns.

The hot paths are four. An intern of a value already there is the common case: a query
re-executing to equal output. Comparing two handles is early cutoff. An index or content ID
to a value is a read. An intern of a new value is the rarest of the four.

**The work that can't be skipped.**

- **An intern** must compute the value's content ID. The ID is the value's name everywhere
  after: the cutoff fingerprint, the persisted key, and the parts of a composite. It is BLAKE3
  over the bytes `hash_content` feeds.
- **Finding the value** costs at least one cache line of an index. Confirming it costs the
  node's line, for its 32-byte content ID and one `Eq`: a hit must be checked, or a
  collision or a bad `hash_content` would alias two values silently.
- **A compare** is one word compare.
- **A read** is the arithmetic from index to place and the loads that reach the node.
- **A new value** also takes one write of the node and one publication of the entry. Under
  threads it takes some serialization of two equal values racing, so that exactly one wins.

Unit costs:

| step | cost |
|---|---|
| `Hasher::of(domain)`: the domain's context key, cached in the `Domain` | about 10 ns |
| one BLAKE3 compression (64 bytes, a single chunk is sequential) | about 20 ns |
| a line in L1 / L2 / DRAM | about 1 / 4 / 100 ns |
| compare 32 bytes; `Eq` of a value of interned parts (pointer compares) | about 1 ns each |
| an uncontended lock and unlock of the shard's mutex | about 10 ns |

**The bound.** For a value feeding \(b\) bytes (a composite of \(k\) parts feeds
\(32k\) plus its own), with the index and node lines in L2:

\[ T_{\text{hit}} \approx 10 + 20 \lceil b / 64 \rceil + 2 \cdot 4 + 1 + T_{\text{Eq}} \ \text{ns}, \]

about 40 ns for a leaf or a pair of parts. Cold, the two lines are about 200 ns. The other
paths:

- comparing two handles, \(T_{\text{cmp}} \approx 0.3\) ns, one cycle;
- \(T_{\text{get}} \approx 2 \cdot 4\) ns, two dependent loads;
- \(T_{\text{lookup}} \approx T_{\text{hit}} - T_{\text{hash}}\), about 10 ns;
- \(T_{\text{new}} \approx T_{\text{hit}} + 10 + 4 + T_{\text{move}}\) ns: the lock, a second
  look, the node write, and amortized growth below one more probe, since a table doubles at
  half load.

**What the code does.**

- **`intern`** is `content_of` (one `Hasher::of`, the value's feed, `finish`), then `Key::of`:
  shifts of the fingerprint for the shard, tag and home. Then `Shard::find` loads the shard's
  level, its table's line at home, and the node on a tag match. `matched` compares the content
  ID and runs `Eq`, and the value passed in is dropped.
- **`==`** is `ptr::eq`.
- **`get`** is `chunk_of` (a leading-zeros count) and two acquire loads.
- **`lookup`** is `intern` without the hash.
- **A miss** locks the shard, looks again, appends to the arena (`OnceLock::set`) and
  publishes the entry with one release store.

Against the bound:

- **The hit** is at it. The one load more is the shard's level, a read-mostly line that stays
  in L1. An entry whose 32-bit tag (fingerprint bits 32..64) differs never loads a node.
- **The compare, the read and the lookup** are at it.
- **The miss** is at it for one thread. A compare-and-swap insert would save the lock on an
  uncontended miss, about 10 ns. It would also need the node published before its equality
  check, so a racing twin could be seen half-made. The lock is kept, and the contention
  measurement is the one named above.

None of these paths allocates: the allocation budget test holds the hit, the reads and the
compares to zero. The pending move of the shard's index onto `blaze_mem::swiss::Index` keeps
one probe: a group of tags compared at once, then the node.

**State of the art.**

- **rustc's interners** hash a value with FxHash, about 1 to 5 ns for a small one, and lock
  the shard in the parallel front end. That gives an address, not a name. The name, the
  stable hash that incremental compilation needs, is a second hash with `StableHasher`
  (SipHasher128). So a rustc intern with its fingerprint costs a lock plus two hashes. Here
  it is one BLAKE3 hash and no lock on a hit.
- **salsa's interned lookups** by value take a shard lock and record a read.
- **papaya and scc** pin an epoch on every read, which is a store. The reads here store
  nothing.
- **egg's hashcons** is a single-threaded `HashMap` probe, the same one or two lines, with no
  persistent name.

So a hit costs about what rustc's intern plus its stable hash costs, with no lock, and
yields a name that is the same in every run and on disk.
