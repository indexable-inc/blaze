# Requests of frontend-tokens-block

## frontend-tokens-block: a `Symbol` from an ID this process gave, without the table lookup

For: `crates/intern/src/symbol.rs` (lane format's), through `frontend-tokens`.

Why: `tt::TopSubtree` becomes one `Box<[u32]>` block (mem's answer in `v2/requests/mem.md`), so
its symbols column holds `Symbol::id()`s. Every ident and literal a walk reads
(`for_each_token`, `leaf_at`, so `iter_flat_tokens`, `Cursor`, `TtIter`) turns an ID back into a
`Symbol`. `Symbol::from_id` checks the ID against the table (`table().get(id)`: a `OnceLock`
load, the base and fresh checks), which is several loads a token where the boxed column was one.
The IDs in a tree are all ones `Symbol::id` gave in this process (the builder writes them from
`Symbol`s, `read_wire` from `Symbol::intern`), so the check is never needed there. A wrong ID is
still memory safe: `as_str` goes through `Table::bytes`, which panics on an ID that names
nothing, as for a symbol whose value the image's GC freed.

Edit, in `impl Symbol` after `from_id` (`crates/intern/src/symbol.rs:179-183`), add:

```rust
    /// The symbol of an ID [`Symbol::id`] gave in this process, with no lookup: for columns of
    /// IDs that were symbols when written (`tt`'s token trees). An ID that names no symbol
    /// panics when its text is read, as a symbol whose value the image's GC freed does.
    #[inline]
    pub fn from_held_id(id: u32) -> Symbol {
        Symbol { id: NonZeroU32::new(id).expect("a symbol's ID is never 0") }
    }
```

`tt/src/storage.rs` calls it in one place (the free `fn symbol`), written against exactly
this signature: tt does not compile until it lands.

> applied: `crates/intern/src/symbol.rs` `Symbol::from_held_id` as written (format and requests-ids DONE), and `round_trips` checks it gives the interned symbol and its text (requests, pass 18).
