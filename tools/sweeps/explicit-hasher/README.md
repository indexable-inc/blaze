# explicit-hasher: the D125/D126 sweep

ast-grep rules that move every hash table to the form where its hasher is written in its type
and as a value, and it is made by its one constructor, `new`, from an options struct with every
field written:

```rust
let map: Map<K, V, FixedState, &Frame> =
    Map::new(MapOpts { hasher: FixedState, alloc: frame, capacity: None });
```

The lint rule `ambient-hasher` (blaze-lint, off by default; name it in the workspace's
`lints-add`) finds what is left after the sweep.

Run each file with `ast-grep scan --rule <file> --update-all <paths>`, then build. The order
does not matter between the files, except that `imports.yml` should run last (it adds the names
the other rewrites use).

| File | Rewrites |
|---|---|
| `map-new-in.yml` | `HashMap::new_in(a)` to `Map::new(MapOpts { hasher: FixedState, alloc: a, capacity: None })` |
| `map-with-capacity-in.yml` | `HashMap::with_capacity_in(n, a)` to `... capacity: Some(n) })` |
| `map-from-iter-in.yml` | `HashMap::from_iter_in(i, a)` to a block: `new` with the iterator's lower size hint, then `extend` |
| `set-new-in.yml`, `set-with-capacity-in.yml`, `set-from-iter-in.yml` | the same for `HashSet` to `Set::new(SetOpts { .. })` |
| `order-map-new-in.yml`, `order-map-with-capacity-in.yml`, `order-map-from-iter-in.yml` | `IndexMap` to `OrderMap::new(OrderMapOpts { .. })` |
| `order-set-new-in.yml`, `order-set-with-capacity-in.yml`, `order-set-from-iter-in.yml` | `IndexSet` to `OrderSet::new(OrderSetOpts { .. })` |
| `by-hash-new.yml` | `ByHash` and `ByHashSet`'s `new_in` and `with_capacity_in`, with `hasher: BuildKeyHasher::default()` (four rules in one file) |
| `with-hasher-in.yml` | `with_hasher_in(h, a)` and `with_capacity_and_hasher_in(n, h, a)` on every single-owner table and alias, to `new(Opts { hasher: h, .. })`; not inside a `const` or `static` |
| `types.yml` | `HashMap<K, V, A>` to `Map<K, V, FixedState, A>`; `HashSet`, `IndexMap`, `IndexSet`, `ByHash`, `ByHashSet`, `KeyedMap`, `KeyedSet` likewise |
| `imports.yml` | `use p::HashMap;` to `use p::{FixedState, Map, MapOpts};`, and the same for each alias |

The `from_iter_in` rewrite reserves `size_hint().0` as `from_iter_in` did, so a table's capacity,
and with it its iteration order, stays what it was. `ByHash::from_iter_in` has no rule (no call
of it was in the tree when the rules were written); the lint finds one.

## What the rules cannot do

Each class below is left for a hand edit or the lint. Each has a pattern that finds it (`rg` from
the workspace root). In the raw Markdown each `\|` in a pattern is the table's escape: type `|`.
When the rules were written the tree held 736 calls of the six aliases' `new_in`,
`with_capacity_in` and `from_iter_in` under `crates/` (by `rg -c`), the macro and qualified ones
among them.

| Class | Why the rules miss it | Find it |
|---|---|---|
| Use lists: `use blaze_hash::{HashMap, HashSet};`, `use bstd::map::{HashMap, Entry};` | a list is one node; a rule cannot drop one name and add three | `rg -t rust 'use [^;]*\{[^}]*\b(HashMap\|HashSet\|IndexMap\|IndexSet\|ByHash\|ByHashSet\|KeyedMap\|KeyedSet)\b'` |
| A name imported twice after `imports.yml` (the file already imported `FixedState` or `Map`) | the rule sees one `use` at a time | the build's "defined multiple times" (E0252) |
| Qualified paths: `blaze_hash::HashMap::new_in(a)`, `crate::HashMap<K, V, A>` | the patterns name the bare alias; a path prefix is another node | `rg -t rust '(blaze_hash\|crate\|super)::(HashMap\|HashSet\|IndexMap\|IndexSet\|ByHash\|ByHashSet\|KeyedMap\|KeyedSet)\b'` |
| bstd paths: `bstd::map::HashMap::new_in(a)`, `bstd::set::HashSet<T, A>` | as above; bstd re-exports `Map`, `MapOpts`, `OrderMapOpts` in `map` and `Set`, `SetOpts`, `OrderSetOpts` in `set`, so the hand edit keeps the bstd path | `rg -t rust '\bbstd::(map\|set)::(HashMap\|HashSet\|IndexMap\|IndexSet\|ByHash\|ByHashSet\|KeyedMap\|KeyedSet)\b'` |
| Macro bodies: a call inside `macro_rules!` or a macro's arguments (`vec_in![..]`, `assert_eq!(..)`) | ast-grep reads a macro's tokens as a token tree, not as expressions | `rg -t rust '(HashMap\|HashSet\|IndexMap\|IndexSet\|ByHash\|ByHashSet)::(new_in\|with_capacity_in\|from_iter_in)' -g '*.rs'` after the sweep: every hit left is in a macro or a qualified path |
| A turbofish on the alias: `HashMap::<K, V, _>::new_in(a)` | the pattern's path is the bare alias | `rg -t rust '(HashMap\|HashSet\|IndexMap\|IndexSet\|ByHash\|ByHashSet)::<'` |
| An alias with its arguments elided by inference: `let m: HashMap<_, _, _>` | matched; the rewrite gives `Map<_, _, FixedState, _>`, which is right | none needed |
| `with_hasher_in` inside a `const` or `static` | `new` is not `const`; these stay, as `Map::with_hasher_in` and `Set::with_hasher_in` are kept `const` | `rg -t rust '(const\|static) [^=]*=.*with_hasher_in'` |
| The shared tables: `SharedMap::new_in(name, a)`, `SharedSet::new_in`, `SharedByHash::new_in`, `with_hasher_in(name, h, a)` | not in this sweep: blaze-core's aliases instantiate the lock, and their rewrite to `SharedMap::new(SharedMapOpts { name, hasher, alloc, capacity: None })` waits for blaze-core's aliases to name the hasher | `rg -t rust '\bShared(Map\|Set\|ByHash)::(new_in\|with_hasher_in)\('` |
| A table whose hasher is a type parameter: `Map<K, V, S, A>::new_in(a)` with `S: Default` | no alias names the hasher; the caller's `S::default()` is the value to write | `rg -t rust '\bMap::(new_in\|with_capacity_in\|from_iter_in)\('` and the same for `Set`, `OrderMap`, `OrderSet` |
| std's and hashbrown's tables and hashers (`std::collections::HashMap`, `RandomState`, `DefaultHasher`) | another type, not a rename; each use is a decision of which hasher | the lint's `std-table` findings, or `rg -t rust '(collections\|hashbrown)::Hash(Map\|Set)\|RandomState\|DefaultHasher'` |
| Doc comments and strings that show the old forms | text, not code | `rg -t rust '//.*(HashMap\|HashSet\|IndexMap\|IndexSet)::(new_in\|with_capacity_in\|from_iter_in)'` |
