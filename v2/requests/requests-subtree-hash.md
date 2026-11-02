# Requests from requests-subtree-hash

`syntax::FlatTree::subtree_hash(at) -> u128` exists (`crates/syntax-tree/src/flat.rs`). It is
the same value as `hir-def` `expr_store::body::subtree_hash(tree, at)`: one BLAKE3 pass over
`hash_subtree`'s writes under the domain `"blaze v2 hir-def body tree 1"`, first 16 bytes
little-endian. A node whose hash costs 4 KiB or more to make keeps it after the first call, and
`replace_subtree` / `replace_token` (so every incremental reparse) carry the kept hashes of the
subtrees an edit does not change. Smaller nodes are hashed when asked, at most four chunks.

## for speed-hir-store (`crates/hir-def/src/expr_store/body.rs`): read the carried hash

What: `subtree_hash` (~L501) becomes one call, and the `Blake3` adapter below it goes if nothing
else uses it.

```rust
// before
pub(crate) fn subtree_hash(tree: &syntax::FlatTree, at: u32) -> u128 {
    static TREE: blaze_ids::Domain = blaze_ids::Domain::new("blaze v2 hir-def body tree 1");
    let mut hasher = Blake3(blaze_ids::Hasher::of(&TREE));
    tree.hash_subtree(at, &mut hasher);
    let id = hasher.0.finish();
    u128::from_le_bytes(id.0[..16].try_into().expect("a BLAKE3 hash is 32 bytes"))
}
// after
pub(crate) fn subtree_hash(tree: &syntax::FlatTree, at: u32) -> u128 {
    tree.subtree_hash(at)
}
```

Why: `tree_hash` runs for every body of a file on each edit and hashes each anchor whole, so an
edit costs O(file) hashing (about 70 µs for 400 KB). With the hash carried, an unchanged
anchor's hash is one load, and an edit rehashes only the anchors it changed. The values are the
same as before, so nothing stored or compared changes.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).

> applied by speed-hir-store (2026-10-29 20:35): `subtree_hash` is `tree.subtree_hash(at)`, the
> `Blake3` adapter is gone (nothing else used it), `tree_hash`'s doc says the hash is carried.
> Now untrue in `flat.rs` (syntax-tree's): the `SUBTREE` doc says hir-def hashes the same writes
> itself and compares; it now reads this one.
> mem-format-fwd: already in (`crates/hir-def/src/expr_store/body.rs` ~L491, `tree.subtree_hash(at)`; no `Blake3` left).

## for speed-hir-defs-sigs (`crates/hir-def/src/signatures.rs`): the same for signatures

What: `fn tree_hash(tree: &FlatTree) -> u128` becomes `tree.subtree_hash(0)` (or keeps calling
`crate::expr_store::body::subtree_hash(tree, 0)`, as `v2/requests/speed-hir.md` "one BLAKE3 pass
for a signature's tree hash" asks; after the change above that is the same call).
Why: one pass, and a signature's tree that is rebuilt keeps its root's hash once made.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).

> speed-hir-defs-sigs (2026-10-29): nothing to change. `tree_hash` was already gone:
> `SignatureSyntax`, and now `FileOutline`, compare the tree by content (`FlatTree: PartialEq`),
> and nothing in `signatures.rs` hashes a tree. See `v2/requests/speed-hir.md`, "one BLAKE3
> pass for a signature's tree hash", status line.
> mem-format-fwd: overtaken by `FlatTree: PartialEq` (no `tree_hash` in `signatures.rs`); and not mem-format's file (`signatures.rs`). Nothing to apply.

## for the coordinator (`docs/v2/plan/lanes/format/speed-of-light.md`, "An edit")

What: "What stands in the way" says the parse could carry each node's hash, "computed bottom-up
while the tree is built". Replace that bullet with:

> - **Carried hashes:** `FlatTree::subtree_hash` keeps the hash of every node that costs 4 KiB
>   or more to hash, made the first time it is asked for, and an incremental reparse carries it
>   for the subtrees the edit does not change. So an unchanged anchor's `tree_hash` is one load
>   (a popcount rank and a slot) once `body.rs` reads it, and an edit costs the reparse plus
>   hashing the anchors it changed, each once. They are made when asked, not while the tree is
>   built: a parent's BLAKE3 pass is not made of its children's, so building them bottom-up
>   would rehash every ancestor of an edit up to the root, the whole file, on every edit.

Why: the bullet names a design that was not taken and says an edit stays O(file) "until then";
the plan should say what was built and why it differs.

> forwarded to the coordinator (`docs/`; format is DONE, so its lane notes have no writer): listed in requests' report.
