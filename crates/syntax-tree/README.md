# syntax-tree

blaze's lossless syntax trees: flat trees (`src/flat.rs`, docs/flat-syntax.md) and the cursors
over them (`SyntaxNode`, `SyntaxToken`). Every parse makes a flat tree, and nothing changes one: an
edit makes a new tree. The crate began as rowan 0.17.0; only its cursor API remains.

A flat tree is every node and token in preorder, packed to the budgets of
docs/v2/plan/design/data.md, a token 4 bytes and a node 8. Each element has one `u32` holding its kind,
whether it is a node, token or lazy node, and either a token's length and how far back its parent
is, or a node's subtree size. Values too large for the word are in small sorted side tables. A
node's parent is in a column of nodes only, found by its rank (a bit per element and a count per
64), and the start of every eighth element is kept. That is about 6.5 bytes an element for Rust.
Its text is not copied: the tree shares the file's source (`FlatTree::source`,
`FlatTreeBuilder::sourced`).

A flat tree may have lazy nodes (`FlatTreeBuilder::lazy`): a node whose text is not parsed, as
an outline parse leaves the body of every `fn`. The arrays show it without children, so walks of
the arrays never parse; the cursor is transparent over it, parsing its part once
(`FlatTree::part`, thread-safe) when it moves into it, so every reader through `SyntaxNode` sees
the full tree. A part keeps the errors parsing it found (`FlatTree::part_errors`), so a body is
parsed once. `FlatTree::replace_subtree` splices a reparsed item into a tree, and
`FlatTree::replace_token` gives a token new text in place; both keep lazy nodes lazy and the
parts already parsed.

Editing builds new trees. `SyntaxNode::splice_children` makes the tree with one node's children
replaced, in one pass copying the elements around them (the syntax editor's step);
`FlatTreeBuilder::node` and `element` copy a node or token from any tree into one being built (how
`make::quote!` interpolates); `SyntaxNode::clone_subtree` makes a node the root of a tree of its
own.

## Testing

`src/flat.rs`'s tests compare flat trees with an independent model built from the same shape
(random trees, and trees past what an element's word holds) through every move of the cursor,
outlines with the same trees in full, parsing a part once under threads (and its errors with
it), and `replace_subtree`, `replace_token` and `splice_children` with building the edited tree
directly. `crates/syntax`'s editor tests make random edits of real files.

## License

It is distributed under the terms of both the MIT
license and the Apache License (Version 2.0).

See LICENSE-APACHE and LICENSE-MIT for details.
