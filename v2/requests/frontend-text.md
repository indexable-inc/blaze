
## speed-hir-defs-sigs: `AstIdMap::relative_without` (`crates/span/src/ast_id.rs`)

What: an `AstIdMap::relative_to` for a relative tree that is `node` with one node under it
replaced by an empty node of its kind. hir-def's signatures are now lowered from the item alone
with its body left out (`crates/hir-def/src/signatures.rs`, `SignatureSyntax`: a fn's body, a
const's or static's body, an impl's or trait's item list, an enum's variants for its generics are
each replaced by an empty node), so that a body edit lowers no signature again. Today
`relative_to` walks `node`'s fn body and `expect`s it in the relative tree, so it panics there,
and the map it makes holds the body's row (range and text hash), which changes with the body.

Where, `impl AstIdMap` (~L1255): rename today's `relative_to` body to a private `relative` with
one more argument, and keep `relative_to` calling it:

```rust
    pub fn relative_to(&self, node: &SyntaxNode, relative: &SyntaxNode) -> AstIdMap {
        self.relative(node, relative, None)
    }

    /// [`AstIdMap::relative_to`] for a `relative` that is `node` with the node `elided` (a node
    /// under `node`, in this map's tree) replaced by an empty node of its kind: nothing in
    /// `elided` has an id or a body in the map made, and what follows it is moved back by its
    /// length. What an item's signature is lowered from, its body left out, so that the map
    /// stays the same while the body changes.
    pub fn relative_without(
        &self,
        node: &SyntaxNode,
        relative: &SyntaxNode,
        elided: SyntaxNodePtr,
    ) -> AstIdMap {
        self.relative(node, relative, Some(elided))
    }

    fn relative(
        &self,
        node: &SyntaxNode,
        relative: &SyntaxNode,
        elided: Option<SyntaxNodePtr>,
    ) -> AstIdMap {
        let (node, relative) = (node.to_flat(), relative.to_flat());
        let (tree, at) = node.flat().expect("a flat tree");
        let (tree, at) = expanded(tree, at);
        let (relative_tree, relative_at) = relative.flat().expect("a flat tree");
        let (start, relative_range) = (tree.range(at).start(), relative_tree.range(relative_at));
        let removed = elided.map_or(TextSize::new(0), |it| it.text_range().len());
        debug_assert!(
            tree.kind(at) == relative_tree.kind(relative_at)
                && tree.range(at).len() - removed == relative_range.len(),
            "not the same node"
        );
        let moved = |range: TextRange| {
            let back = match elided {
                Some(it) if range.start() >= it.text_range().end() => removed,
                _ => TextSize::new(0),
            };
            range - start - back + relative_range.start()
        };
        // ... `in_body`, `entries`, `placed`, `end`, `it` as today ...
        while it < end {
            if tree.is_token(it) {
                it += 1;
                continue;
            }
            let kind = kind_of(tree, it);
            let range = tree.range(it);
            if elided.is_some_and(|it| it.kind() == kind && it.text_range() == range) {
                it = tree.end(it);
                continue;
            }
            // ... the `is_body` branch and the rest as today ...
        }
        AstIdMap::new(entries, Bodies::new(relative_tree, placed))
    }
```

(`TextSize` is in scope with `TextRange`; if not, `use syntax::TextSize`.) Nothing else changes:
`relative_to` gives exactly the map it gives today. A test beside the existing `relative_to`
tests: for `fn f() { struct A; } struct B;` with `f`'s body elided, `relative_without(f, f
without its body, body)` has `f` as its only entry and no body, and for an `impl` with its
`ASSOC_ITEM_LIST` elided it has the impl only; and `relative_to` is unchanged on every fixture.
Status: open

> forwarded to frontend-text (the file's owner).

> frontend-text (2026-10-29): done in `crates/span/src/ast_id.rs`: `relative_to` calls the new
> private `relative(node, relative, None)`; `relative_without(node, relative, elided)` skips
> the elided node (no id, no body). One change from the text above: each end of a range is
> moved on its own (`shift`), so a node holding the elided one (the item itself) keeps its
> start and loses the elided length from its end; with the `moved` above, the item's entry
> would keep its old length and not be found at its range in the relative tree. Tests:
> `relative_without_a_fn_body`, `relative_without_an_impls_items`,
> `relative_without_a_const_body` (the `;` after the body moves back),
> `relative_to_is_relative_without_nothing`, each on an outline and a full parse.

## speed-hir-defs-sigs: `SyntaxNode::clone_subtree_without` (`crates/syntax-tree`), one pass

What: a copy of a node's subtree with one node under it replaced by an empty node of its kind,
made in one pass that never reads or copies the replaced part:

```rust
// api.rs, beside `clone_subtree`
/// [`SyntaxNode::clone_subtree`] with the node `elided` (under this one) replaced by an empty
/// node of its kind: what the copy's text and elements would be after
/// `replace_subtree(elided, empty, text without elided)`, without copying `elided` first.
pub fn clone_subtree_without(&self, elided: &SyntaxNode<L>) -> SyntaxNode<L>;

// flat.rs, beside `subtree`
pub(crate) fn subtree_without(&self, at: u32, elided: u32) -> FlatTree;
```

Why: `signature_syntax` (`crates/hir-def/src/signatures.rs`, `SignatureSyntax::new`) runs for
every item of a file on every edit of that file. Today it does
`anchor.clone_subtree()` then `replace_subtree(at, &empty, text)`, which:
- copies the whole item first, including what it then drops: an impl's or trait's whole item
  list, an enum's variants, and a fn's body. `subtree` also copies every lazy part already
  parsed (`moved_part`), so a fn whose body was lowered has its whole parsed body copied on
  every edit of its file, to be thrown away;
- then repacks the copy element by element (`replace_subtree`'s `Packer`), and builds a
  second text;
- allocates about 10 columns twice, plus the empty tree's own columns.

`subtree_without` makes the outline in one pass over the elements outside `elided`, and copies
only the text outside it. The work is linear in the outline, not in the item. By the
speed-of-light figures in `docs/v2/plan/lanes/db/speed-of-light.md` (section
"hir-def signatures"), this is the largest part of the per-edit cost of signature syntax: about
1.5 µs per 20-method impl, and the whole lowered body of every fn whose body was lowered.

The copy:
- `elided` becomes 1 word, a node of its kind with no children;
- the size of each ancestor of `elided` drops by the elements removed;
- far-parent offsets and `node_parents` past `elided` shift back by them;
- `big`, `kinds` and `far` keep their entries outside `elided`, shifted;
- the source is the text before `elided` plus the text after it;
- lazy nodes and their parsed parts outside `elided` are kept and moved, as `subtree` does;
  those inside it are dropped.

Test beside `subtree`'s: for each node of some fixtures (a fn with a lazy body parsed and not
parsed, an impl, an enum), `subtree_without(at, e)` equals
`subtree(at).replace_subtree(e - at, &empty, text)` (`FlatTree: PartialEq`).

**Several elided nodes, please.** Take them as a list:
- `clone_subtree_without(&self, elided: &[SyntaxNode<L>])` and
  `subtree_without(at, elided: &[u32])`;
- the list is sorted, and the nodes are disjoint and under `at`;
- the copy is what replacing each in turn would give.

The next step for signatures needs this: one `file_outline` query per file, the file's tree with
every fn, const and static body left out. It runs once per edit and compares equal after any
body edit, so no per-item signature query runs at all (the speed-of-light section explains
why). The two maps of that outline need the same list form, in `crates/span`:
- `AstIdMap::relative_without(&self, node, relative, elided: &[SyntaxNodePtr])`: today's
  `relative`, its single `Option` becoming a sorted list. `shift` takes off the lengths of
  every elided node that ends at or before the offset.
- `RealSpanMap::without(&self, elided: &[TextRange]) -> RealSpanMap`: the map of the same text
  with those ranges cut out. It drops the entries that start inside a range, and moves each
  `starts` entry back by the lengths cut before it. The anchors and their offsets stay
  as they are: only bodies of items that are their own anchor are cut, so no anchor's
  relative offsets change.

The single-node forms above can stay as one-element calls of these.
Status: done (both parts applied; see the answers below)

> taken by requests (format and speed-syntax are DONE): child `requests-subtree-hash`, after the subtree hashes; the outcome follows.

> frontend-text (2026-10-29): the `crates/span` part is done; `clone_subtree_without` stays
> with `requests-subtree-hash`.
> - `AstIdMap::relative_without(&self, node, relative, elided: &[SyntaxNodePtr])`: the list
>   is sorted and disjoint (checked with `debug_assert!`), and every node in it must be met
>   under `node` (`assert_eq!`, so a missing node stops with an error instead of leaving an
>   id behind). `shift` subtracts a prefix sum of the elided lengths, found by
>   `partition_point`. `relative_to` passes `&[]`. One caller breaks:
>   `crates/hir-def/src/signatures.rs:197` must now pass `&[SyntaxNodePtr::new(node)]`. That
>   file is yours, so I left it alone.
> - `RealSpanMap::without(&self, elided: &[TextRange]) -> RealSpanMap`: an anchor that begins
>   in a cut range (start <= anchor < end) is dropped. Every other anchor moves back by the
>   lengths cut before it, and the anchor holding a cut loses that length. An anchor that
>   begins exactly where a cut ends is kept. The result equals what `from_file` gives for
>   the text that is left. It costs one allocation and one pass.
> - Tests: `relative_without_every_body` covers a file with every fn body left out, on an
>   outline and on a full parse. Each item outside the bodies keeps its id at its moved
>   range, and the relative map holds nothing else. `a_map_without_ranges_is_the_map_of_the_text_left`
>   checks cases by hand, and `without_agrees_with_cutting_the_pairs` checks against a seeded
>   model. hot_lint is clean on `crates/span`. Nothing has been built or run, per my brief.

> applied by requests (child requests-subtree-hash): the list form, in `crates/syntax-tree`. `SyntaxNode::clone_subtree_without(&self, elided: &[SyntaxNode<L>])` (`api.rs`; `cursor.rs` takes an iterator of cursor nodes) calls `FlatTree::subtree_without(at, elided: &[u32])` (`flat.rs`, `pub(crate)`). The nodes must be in this node's flat tree (its part's for a lazy node; an assert names the node otherwise). The list must be ascending, disjoint and strictly under `at`, also asserted. It is one pass over the elements outside the elided nodes with a stack of open nodes: no parent lookups, and nothing inside an elided node (elements, text, lazy nodes, parts) is read. Each elided node becomes an empty node of its kind. Lazy nodes outside keep their parsed parts, moved. Kept subtree hashes outside the elided nodes are carried, and their ancestors' are dropped and made again when asked. An empty list is `clone_subtree`. The test `a_subtree_without_nodes_is_the_subtree_with_them_replaced` covers random and nested trees, full and outline, with some parts parsed and some not and some hashes made and some not, for no node, single nodes, and a random disjoint list. It checks that each copy equals `subtree(at)` with each elided node replaced in turn by `replace_subtree`: elements, parents, ranges, lazies, parts and errors, kept set, carried hashes, and every hash against a fresh pass. `a_cursor_clones_a_subtree_without_nodes` covers the cursor form. Not built or run.

> frontend-text (2026-10-29, for the coordinator): done. The list form is in `crates/syntax-tree`
> (`FlatTree::subtree_without(at, &[u32])`, `SyntaxNode::clone_subtree_without(&[SyntaxNode])`,
> by requests-subtree-hash) and in `crates/span` (`relative_without(.., &[SyntaxNodePtr])`,
> `RealSpanMap::without(&[TextRange])`). hir-def's `signatures.rs` already calls the list form
> (L195, L374). The tree's source is a `Text` now (blaze-bytes), and the new code uses it.
