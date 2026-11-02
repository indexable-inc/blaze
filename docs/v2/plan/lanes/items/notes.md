---
status: plan
summary: Design notes of blaze v2's items lane: item keys, how the four hashes are computed lazily per item, what leaks into an interface, names and positions, facts and their negative indexes, and what other lanes must keep for the hashes to be sound.
date: 2026-10-29
---
# blaze v2 items: design notes

Notes of the `items` lane on [items.md](../items.md) (section 6), as of 2026-10-29. Code:
`crates/blaze-items/` (`key.rs`, `form.rs`, `hash.rs`, `facts.rs`, `index.rs`) and the narrowed
queries in `crates/hir-ty/src/` (`method_resolution.rs`, `traits.rs`, `specialization.rs`,
`opaques.rs`, `layout.rs`, `drop.rs`).

## Keys

An `ItemKey` is a hash of the key of what the item is in, its kind, its name and where it is
written: the raw AST id of its declaration (`ErasedFileAstId::into_raw`: kind, a hash of the
name, an index among equal ones in the file) and the key of the file, which for a macro
expansion is the call's file key, the call's AST id and which derive or attribute it is. AST ids
don't move when another item is added or edited, so keys survive edits; a rename or a move gives
a new key and the same hashes.

- A module: its crate's for a crate root, its block's for a block's root, else its parent's and
  its name. A block: its module's and the block's AST id.
- A crate: its name, version, origin, kind of target and all its cfg, features included (one
  package is built for host and target with different features and, natively, the same target
  cfg). Two crates of one build with one key stop `resolve`.
- A closure: its expression store's key, whether it is a closure, coroutine or coroutine
  closure, and its place among the store's closures (`ClosureKey::nth`, as hir-ty interns it).
- An opaque type: its function's or alias's key and its index. A variant: its enum's and its
  name. An anonymous constant: its store's and its root expression. A built-in derive: its
  type's, the trait and which derive.

`resolve` goes back from a key by indexes: one per crate of the items of its modules, impls and
traits, and one per store (a signature, a body, a variant's fields) and per item with opaque
types of what is inside, looked at only when the first misses. Closures in anonymous constants
have keys but no facts of their own: whatever uses them reads the constant.

## Hashes

`form` writes an item's interface form and body form (digest, refs in slot order, `missed`,
spans). `hash` fills the slots:

| hash | own content | a ref is filled with |
|---|---|---|
| interface | interface form | the ref's interface hash |
| layout (types) | interface form and the target's data layout | a type's layout hash, anything else's interface hash |
| code (bodies) | interface hash and body form | the ref's interface hash |
| deep | interface and body forms | the ref's deep hash |

Items that refer to each other are one unit (Unison): the strongly connected component of the
item in the graph of refs, found by Tarjan's algorithm from the item over refs in its own crate
(a cycle never crosses crates). Members are ordered by content, refined by their in-cycle refs
until classes stop splitting; ties left (members that are alike) by key, which only matters
where swapping them hashes alike. Each ref inside is the target's position, and each member's
hash is the unit's and its position. A ref outside the component is the tracked query of that
ref, which cannot reach back, so there is no salsa cycle. Hashing is lazy: a hash asks for the
forms its component reaches and no more.

The cost: each item's hash walks the refs (memoized lists) it reaches in its crate, so hashing
all of a crate is the sum of what each item reaches. Forms are computed once each.

A form that could not be written (an error type or constant, MIR that failed to lower, a macro
without rules, a crate without a target) is `Err(Unhashable { item, what })`, never a hash.
blaze-next salted those with the item's location, which two different edits could share.

## What a body leaks into an interface

The interface form spells, beyond the signature: hidden types of `impl Trait` returns and
`async fn`s, with their auto-trait bits; a `const fn`'s body, a const's value and a non-`mut`
static's initializer (whose value users may read at compile time); closures as their
signature and captures, coroutines with their saved locals (#906). Including a static's
initializer is coarse: it is in the interface because const eval can read it.

A const without generics (its own or its container's) that evaluates is its value, the
allocation spelled as a body spells an evaluated constant, in its interface and as its code:
an edit of a `const fn` it calls that keeps the value changes neither, so it reaches no user
(E4 of [the firewall notes](../firewall/notes.md)). A generic const, and one whose evaluation
fails, is its body, from which each use evaluates it. A `const fn` keeps its body in its
interface for those.

## Names and positions

No item, field, variant or module name is in a hash, but where the program sees it: symbol
names (`#[no_mangle]`, extern blocks, `export_name`, `link_name`, `link_section`), intrinsics
that observe a type's identity or name (`type_name`, `type_id`: their nominal arguments spell
key and path), trait items as slots in declaration order, impl items as (trait slot, impl slot)
pairs. Visibility is in the interface as public, crate, or the module n levels up. Positions
(MIR spans) are in `Sub::spans`, never in a digest; literals of `line!()` stay in the code
until expansion makes them span references. Macro tokens keep text and jointness; a literal's
bytes are never dropped (#1326). An expansion that failed (#1325: an `include_str!` of a
deleted file) is an error, not a hash.

## Facts

Every `Fact` of section 6.2 has a value (`facts::value`, a content id; an item gone is
"absent") and its fingerprint. Negative facts are answered from indexes keyed by
`ImplIndexKey` (trait, simplified self type), the blanket impls of a trait, and
`ScopeIndexKey` (scope, name), each built per trait and per scope over all crates, so an added
impl or item changes only the entries with its key. The cross-item rules map onto facts: inference
fallback reads `Implements`; globs and shadowing read `Resolves`; hygiene resolves in the
def-site scope; coherence, specialization, drop check, vtables and derives have their own.
The hir-ty reads they stand for are narrowed to per-(impl or trait, name) and per-item queries,
so salsa's own dependencies are no wider than the facts.

## Hardware

docs/v2/plan/design/hardware.md's BLAKE3 row is items': many small inputs, one batched call.
Every key is a tracked query, so a parent's key and a crate's cfg are hashed once, not once per
descendant. The keys of a module's, an impl's, a trait's and an enum's members are built as
preimages and hashed together with `ContentId::of_many` (`key::hash_all`), which gives exactly
`ContentId::of` per input on every CPU path; `tests::hashes::a_batch_of_keys_is_each_key_alone`
holds it. Form's sub-forms and index's name hashes batch the same way. Maps keyed by a key are
`HashMap<_, _, KeyHashing>`, which folds the BLAKE3 bytes it already has instead of hashing them
again; hashbrown does the SIMD probing. CRC32C, huge pages, NUMA, io_uring and threads are other
lanes' rows.

## Data

docs/v2/plan/design/data.md's row for items: hashes as columns per kind, facts as sorted
triples, negative-fact indexes as sorted arrays. `table::HashColumns` (built per crate by
`crate_hashes`) holds a crate's item hashes sorted by key, one column per hash kind; a layout or
code hash only some items have sits densely in its own column, found through a presence bitset
(`table::Bits`) by rank, and the unhashable items are a side table. `changed_since` is one merge
of two builds' key columns. `table::FactSet` holds facts as a `u8` kind and two `u32`s over a
sorted dictionary of the ids they name (9 bytes a fact, `const`-asserted): one kind's facts
about one subject are one range. The hash of a component writes every member's refs into one
CSR list and hashes each refinement round in one batch. The indexes in `index.rs` keep the
searched column apart from the rest.

## What other lanes must keep

- Codegen and link name callees by identity (key and instance arguments, including the impls
  selected), never by something a callee's body edit changes.
- Code keyed by a code hash has no positions baked in; they come from the span table.
- The db records `Field` and `Resolves` facts where inference reads a field name or resolves a
  path.
- Facts are computed over all crates of the build (`all_crates`), which is coarse for a build
  of many workspaces but never misses an impl.

The requests are in `v2/requests/items.md`.
