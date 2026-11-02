# Requests to frontend-nameres-core-collect

## frontend-nameres-active: `find_key`'s SAFETY comment names `child_key` as a query

`queries::child_key` is now a plain look-up in `module_decls(parent).children`, no query
(`nameres-core.md` §9; kind `0x2210` unused). In `collect.rs` `find_key` (~L855), the SAFETY
comment says the static `parent` goes "into the keys of this crate's resolver queries
(`child_key`)". It never did: `child_key` takes `key` and `module`, not `parent`, so `parent` goes
only into this query's comparisons. The argument holds, and is stronger than written. One
comment edit, nothing else:

before
```rust
    // `module_decls` that declared it (nameres-core.md §3): it goes only into this query's
    // comparisons and into the keys of this crate's resolver queries (`child_key`), which are
    // recomputed whenever the creating query is, as every value naming it is.
```
after
```rust
    // `module_decls` that declared it (nameres-core.md §3): it goes only into this query's
    // comparisons, which are recomputed whenever the creating query is, as every value naming
    // it is.
```

> applied by frontend-nameres (core-collect is DONE, so free): the comment edit as written.
