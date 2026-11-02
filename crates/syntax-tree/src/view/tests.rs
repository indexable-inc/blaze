//! The views answer as the cursors do, element by element, on random trees with lazy nodes.

use std::{collections::HashMap, sync::Arc};

use blaze_ids::rng::SplitMix64;

use crate::{
    api, view::{Element, Node, NodeId}, Direction, FlatTree, FlatTreeBuilder, Language,
    NodeOrToken, SyntaxKind, Text, TextRange, TextSize, TokenAtOffset, WalkEvent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Raw {}

impl Language for Raw {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: SyntaxKind) -> SyntaxKind {
        raw
    }

    fn kind_to_raw(kind: SyntaxKind) -> SyntaxKind {
        kind
    }
}

type Cursor = api::SyntaxNode<Raw>;
type CursorElement = api::SyntaxElement<Raw>;
type View<'t> = Node<'t, Raw>;
type ViewElement<'t> = Element<'t, Raw>;

const ROOT: SyntaxKind = SyntaxKind(1);
const GROUP: SyntaxKind = SyntaxKind(2);
const LAZY: SyntaxKind = SyntaxKind(3);
const WORD: SyntaxKind = SyntaxKind(10);
const SPACE: SyntaxKind = SyntaxKind(11);
const OPEN: SyntaxKind = SyntaxKind(12);
const CLOSE: SyntaxKind = SyntaxKind(13);
/// A kind past the word's 10 bits, so its kind is in the side table.
const FAR: SyntaxKind = SyntaxKind(1500);

/// A group's text: `[`, words, spaces and groups, `]`.
fn group_text(rng: &mut SplitMix64, depth: u32, out: &mut String) {
    out.push('[');
    for _ in 0..rng.below(6) {
        match rng.below(4) {
            0 if depth < 3 => group_text(rng, depth + 1, out),
            1 => out.push(' '),
            _ => (0..1 + rng.below(4)).for_each(|it| out.push((b'a' + it as u8) as char)),
        }
    }
    out.push(']');
}

/// Builds the elements of `text` (a group's inside, or a file's) onto `builder`, from `at`;
/// a `[` opens a node of `group` (a lazy node of `LAZY` when `lazy` says so and the builder
/// may make one). Gives where it stopped: past the `]` closing the group it is in.
fn build(builder: &mut FlatTreeBuilder, source: &str, mut at: usize, lazy: &mut impl FnMut() -> bool) -> usize {
    let bytes = source.as_bytes();
    while at < bytes.len() {
        match bytes[at] {
            b'[' => {
                let end = close_of(source, at);
                if lazy() {
                    builder.lazy(LAZY, (end - at) as u32);
                    at = end;
                    continue;
                }
                builder.start_node(if at % 7 == 0 { FAR } else { GROUP });
                builder.token(OPEN, &source[at..at + 1]);
                at = build(builder, source, at + 1, lazy);
                builder.finish_node();
            }
            b']' => {
                builder.token(CLOSE, &source[at..at + 1]);
                return at + 1;
            }
            b' ' => {
                builder.token(SPACE, &source[at..at + 1]);
                at += 1;
            }
            _ => {
                let end = (at..bytes.len()).find(|&it| !bytes[it].is_ascii_lowercase()).unwrap_or(bytes.len());
                builder.token(WORD, &source[at..end]);
                at = end;
            }
        }
    }
    at
}

/// Where the group opening at `at` ends, past its `]`.
fn close_of(source: &str, at: usize) -> usize {
    let mut depth = 0;
    for (it, byte) in source.bytes().enumerate().skip(at) {
        match byte {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return it + 1;
                }
            }
            _ => {}
        }
    }
    unreachable!("a group's `[` at {at} is closed")
}

fn expand(
    kind: SyntaxKind,
    source: &Text,
    range: TextRange,
    _data: u32,
) -> (FlatTree, Vec<(TextRange, Box<str>)>) {
    let mut builder = FlatTreeBuilder::sourced_range(source.clone(), range);
    let (start, end) = (usize::from(range.start()), usize::from(range.end()));
    builder.start_node(kind);
    builder.token(OPEN, &source[start..start + 1]);
    let stop = build(&mut builder, &source[..end], start + 1, &mut || false);
    assert_eq!(stop, end);
    builder.finish_node();
    (builder.finish(), Vec::new())
}

/// A random file: groups and words at the top, some groups lazy.
fn tree(seed: u64) -> Arc<FlatTree> {
    let mut rng = SplitMix64::new(seed);
    let mut text = String::new();
    for _ in 0..1 + rng.below(8) {
        match rng.below(3) {
            0 => text.push(' '),
            _ => group_text(&mut rng, 0, &mut text),
        }
    }
    let source = Text::from(text);
    let mut builder = FlatTreeBuilder::sourced(source.clone());
    builder.expand_with(expand, 0);
    builder.start_node(ROOT);
    let mut lazy = || rng.below(3) == 0;
    build(&mut builder, &source, 0, &mut lazy);
    builder.finish_node();
    Arc::new(builder.finish())
}

fn key(element: &CursorElement) -> (SyntaxKind, TextRange) {
    (element.kind(), element.text_range())
}

fn view_key(element: ViewElement<'_>) -> (SyntaxKind, TextRange) {
    (element.kind(), element.text_range())
}

fn cursor_text(element: &CursorElement) -> String {
    match element {
        NodeOrToken::Node(it) => it.text().to_string(),
        NodeOrToken::Token(it) => it.text().to_owned(),
    }
}

fn view_text<'t>(element: ViewElement<'t>) -> &'t str {
    match element {
        NodeOrToken::Node(it) => it.text(),
        NodeOrToken::Token(it) => it.text(),
    }
}

fn cursor_events(root: &Cursor, skip: impl Fn(&CursorElement) -> bool) -> Vec<(bool, SyntaxKind, TextRange)> {
    let mut walk = root.preorder_with_tokens();
    let mut events = Vec::new();
    while let Some(event) = walk.next() {
        match event {
            WalkEvent::Enter(it) => {
                events.push((true, it.kind(), it.text_range()));
                if skip(&it) {
                    walk.skip_subtree();
                }
            }
            WalkEvent::Leave(it) => events.push((false, it.kind(), it.text_range())),
        }
    }
    events
}

fn view_events(root: View<'_>, skip: impl Fn(ViewElement<'_>) -> bool) -> Vec<(bool, SyntaxKind, TextRange)> {
    let mut walk = root.preorder_with_tokens();
    let mut events = Vec::new();
    while let Some(event) = walk.next() {
        match event {
            WalkEvent::Enter(it) => {
                events.push((true, it.kind(), it.text_range()));
                if skip(it) {
                    walk.skip_subtree();
                }
            }
            WalkEvent::Leave(it) => events.push((false, it.kind(), it.text_range())),
        }
    }
    events
}

fn token_at<T>(it: TokenAtOffset<T>, f: impl Fn(T) -> (SyntaxKind, TextRange)) -> Vec<(SyntaxKind, TextRange)> {
    match it {
        TokenAtOffset::None => vec![],
        TokenAtOffset::Single(it) => vec![f(it)],
        TokenAtOffset::Between(l, r) => vec![f(l), f(r)],
    }
}

/// Every answer of the views of `tree` against the cursors'.
fn check(tree: &Arc<FlatTree>) {
    let cursor = Cursor::new_flat_root(Arc::clone(tree));
    let view = View::root(tree);

    let cursors: Vec<CursorElement> = cursor.descendants_with_tokens().collect();
    let views: Vec<ViewElement<'_>> = view.descendants_with_tokens().collect();
    assert_eq!(cursors.len(), views.len(), "as many elements");
    let index: HashMap<CursorElement, usize> =
        cursors.iter().cloned().enumerate().map(|(i, it)| (it, i)).collect();
    let find = |it: Option<CursorElement>| it.map(|it| index[&it]);
    let same = |it: Option<ViewElement<'_>>, of: Option<usize>| match (it, of) {
        (Some(it), Some(of)) => it == views[of],
        (None, None) => true,
        _ => false,
    };

    for (i, (c, v)) in cursors.iter().zip(&views).enumerate() {
        let v = *v;
        assert_eq!(key(c), view_key(v), "element {i}");
        assert_eq!(cursor_text(c), view_text(v), "element {i}'s text");
        assert_eq!(c.index(), v.index(), "element {i}'s index");
        assert!(same(v.parent().map(NodeOrToken::Node), find(c.parent().map(NodeOrToken::Node))));
        assert!(same(v.next_sibling_or_token(), find(c.next_sibling_or_token())), "{i} next");
        assert!(same(v.prev_sibling_or_token(), find(c.prev_sibling_or_token())), "{i} prev");
        assert_eq!(v.ancestors().count(), c.ancestors().count(), "{i}'s ancestors");
        assert_eq!(v.tree_top(), view);
        let id = v.id();
        assert!(View::at(tree, id) == v, "{i} found by its id");
        // Identity is the tree's, not the `Arc` handle's: another count of it views the same.
        let other = Arc::clone(tree);
        assert!(View::at(&other, id) == v, "{i} found through another count of its tree");
        assert!(id.part == NodeId::FILE || tree.is_lazy(id.part));
        for (j, w) in views.iter().enumerate() {
            assert_eq!(*w == v, i == j, "views {i} and {j} are equal as their elements");
        }
        match (c, v) {
            (NodeOrToken::Node(c), NodeOrToken::Node(v)) => {
                let node = |it: Option<api::SyntaxNode<Raw>>| find(it.map(NodeOrToken::Node));
                fn as_element(it: Option<View<'_>>) -> Option<ViewElement<'_>> {
                    it.map(NodeOrToken::Node)
                }
                let token = |it: Option<api::SyntaxToken<Raw>>| find(it.map(NodeOrToken::Token));
                assert!(same(v.first_token().map(NodeOrToken::Token), token(c.first_token())));
                assert!(same(v.last_token().map(NodeOrToken::Token), token(c.last_token())));
                assert!(same(as_element(v.first_child()), node(c.first_child())), "{i} first child");
                assert!(same(as_element(v.last_child()), node(c.last_child())), "{i} last child");
                assert!(same(as_element(v.next_sibling()), node(c.next_sibling())), "{i} next sibling");
                assert!(same(as_element(v.prev_sibling()), node(c.prev_sibling())), "{i} prev sibling");
                assert!(same(v.first_child_or_token(), find(c.first_child_or_token())));
                assert!(same(v.last_child_or_token(), find(c.last_child_or_token())));
                let children: Vec<_> = c.children().map(|it| index[&NodeOrToken::Node(it)]).collect();
                let of_view: Vec<_> = v.children().collect();
                assert_eq!(children.len(), of_view.len(), "{i}'s children");
                for (c, v) in children.iter().zip(&of_view) {
                    assert!(views[*c] == NodeOrToken::Node(*v));
                }
                let tokens: Vec<_> = c.children_with_tokens().map(|it| index[&it]).collect();
                let of_view: Vec<_> = v.children_with_tokens().collect();
                assert_eq!(tokens.len(), of_view.len());
                for (c, v) in tokens.iter().zip(&of_view) {
                    assert!(views[*c] == *v);
                }
                let below: Vec<_> = c.descendants().map(|it| index[&NodeOrToken::Node(it)]).collect();
                let of_view: Vec<_> = v.descendants().collect();
                assert_eq!(below.len(), of_view.len(), "{i}'s descendants");
                for (c, v) in below.iter().zip(&of_view) {
                    assert!(views[*c] == NodeOrToken::Node(*v));
                }
                for direction in [Direction::Next, Direction::Prev] {
                    assert_eq!(c.siblings(direction).count(), v.siblings(direction).count());
                    assert_eq!(
                        c.siblings_with_tokens(direction).count(),
                        v.siblings_with_tokens(direction).count()
                    );
                }
                assert_eq!(
                    cursor_events(&c, |_| false),
                    view_events(v, |_| false),
                    "{i}'s preorder"
                );
                let skip = |kind: SyntaxKind, range: TextRange| kind == GROUP && u32::from(range.start()) % 2 == 0;
                assert_eq!(
                    cursor_events(&c, |it| skip(it.kind(), it.text_range())),
                    view_events(v, |it| skip(it.kind(), it.text_range())),
                    "{i}'s preorder, skipping"
                );
                let nodes: Vec<_> = c.preorder().map(|it| match it {
                    WalkEvent::Enter(it) => (true, it.kind(), it.text_range()),
                    WalkEvent::Leave(it) => (false, it.kind(), it.text_range()),
                }).collect();
                let of_view: Vec<_> = v.preorder().map(|it| match it {
                    WalkEvent::Enter(it) => (true, it.kind(), it.text_range()),
                    WalkEvent::Leave(it) => (false, it.kind(), it.text_range()),
                }).collect();
                assert_eq!(nodes, of_view);
                for kind in [GROUP, LAZY, FAR, ROOT] {
                    assert!(same(
                        v.child_where(|it| it == kind).map(NodeOrToken::Node),
                        node(c.child_where(|it| it == kind))
                    ));
                    assert!(same(
                        v.ancestor_where(|it| it == kind).map(NodeOrToken::Node),
                        node(c.ancestor_where(|it| it == kind))
                    ));
                }
                for kind in [OPEN, CLOSE, WORD, SPACE] {
                    assert!(same(
                        v.child_token(kind).map(NodeOrToken::Token),
                        find(c.child_token(kind).map(NodeOrToken::Token))
                    ));
                }
                assert_eq!(v.first_child_token_text(), c.first_child_token_text());
                assert_eq!(v.to_string(), c.to_string());
                assert_eq!(format!("{v:#?}"), format!("{c:#?}"));
                let range = v.text_range();
                for offset in u32::from(range.start())..=u32::from(range.end()) {
                    let offset = TextSize::from(offset);
                    assert_eq!(
                        token_at(c.token_at_offset(offset), |it: api::SyntaxToken<Raw>| (it.kind(), it.text_range())),
                        token_at(v.token_at_offset(offset), |it: crate::view::Token<'_, Raw>| (it.kind(), it.text_range())),
                        "{i} token at {offset:?}"
                    );
                }
                for other in &cursors {
                    let range = other.text_range();
                    if !v.text_range().contains_range(range) {
                        continue;
                    }
                    assert!(same(Some(v.covering_element(range)), find(Some(c.covering_element(range)))));
                    assert!(same(v.child_or_token_at_range(range), find(c.child_or_token_at_range(range))));
                    for kind in [GROUP, LAZY, FAR, ROOT] {
                        assert!(same(
                            v.descendant_at_range(range, kind).map(NodeOrToken::Node),
                            node(c.descendant_at_range(range, kind))
                        ), "{i} descendant {kind:?} at {range:?}");
                    }
                }
                assert_eq!(&v.subtree(), &**c.clone_subtree().flat_tree().unwrap());
                let elided: Vec<View<'_>> = v.children().step_by(2).collect();
                let elided_cursors: Vec<Cursor> = c.children().step_by(2).collect();
                assert_eq!(
                    &v.subtree_without(&elided),
                    &**c.clone_subtree_without(&elided_cursors).flat_tree().unwrap(),
                    "{i} subtree without every other child"
                );
            }
            (NodeOrToken::Token(c), NodeOrToken::Token(v)) => {
                let token = |it: Option<api::SyntaxToken<Raw>>| find(it.map(NodeOrToken::Token));
                assert!(same(v.next_token().map(NodeOrToken::Token), token(c.next_token())), "{i} next token");
                assert!(same(v.prev_token().map(NodeOrToken::Token), token(c.prev_token())), "{i} prev token");
                assert_eq!(v.parent_ancestors().count(), c.parent_ancestors().count());
                assert_eq!(format!("{v:?}"), format!("{c:?}"));
            }
            _ => panic!("element {i} is a node in one walk and a token in the other"),
        }
    }
}

#[test]
fn views_answer_as_cursors() {
    for seed in 0..300 {
        check(&tree(seed));
    }
}

#[test]
fn a_walk_with_ranges_gives_each_elements_text_range() {
    for seed in 0..300 {
        let tree = tree(seed);
        let root = View::root(&tree);
        for node in std::iter::once(root).chain(root.descendants()) {
            let ranged: Vec<_> = node.descendants_with_ranges().collect();
            let asked: Vec<_> = node.descendants_with_tokens().map(|it| (it, it.text_range())).collect();
            assert_eq!(ranged, asked, "seed {seed}, from {node:?}");
        }
    }
}

#[test]
fn a_lazy_node_is_entered_only_when_walked_into() {
    let tree = (0u64..)
        .map(tree)
        .find(|it| View::root(it).children().any(|it| it.kind() == LAZY))
        .unwrap();
    let root = View::root(&tree);
    let lazies: Vec<_> = root.children().filter(|it| it.kind() == LAZY).collect();
    for lazy in &lazies {
        assert!(lazy.is_lazy());
        assert_eq!(lazy.text().as_bytes()[0], b'[');
    }
    // Reading a lazy node's own kind, range, text and siblings parses nothing.
    assert!(tree.lazies().all(|it| !tree.is_parsed(it)));
    let _ = root.descendants().count();
    assert!(tree.lazies().all(|it| tree.is_parsed(it)));
    for lazy in lazies {
        assert!(!lazy.is_lazy());
        let first = lazy.first_child_or_token().unwrap();
        assert_eq!(first.kind(), OPEN);
        assert_eq!(first.parent(), Some(lazy));
        assert_eq!(lazy.id().part, NodeId::FILE);
        assert_eq!(first.id().part, lazy.id().at);
    }
}

#[test]
fn views_are_small_copy_and_shared_across_threads() {
    fn shared<T: Copy + Send + Sync>() {}
    shared::<View<'_>>();
    shared::<crate::view::Token<'_, Raw>>();
    shared::<ViewElement<'_>>();
    assert_eq!(std::mem::size_of::<View<'_>>(), 24);
    assert_eq!(std::mem::size_of::<crate::view::Token<'_, Raw>>(), 24);
    let tree = tree(7);
    let root = View::root(&tree);
    let counts: Vec<usize> = std::thread::scope(|scope| {
        let walks: Vec<_> = (0..4).map(|_| scope.spawn(move || root.descendants_with_tokens().count())).collect();
        walks.into_iter().map(|it| it.join().unwrap()).collect()
    });
    assert!(counts.windows(2).all(|it| it[0] == it[1]));
}

#[test]
fn a_view_copies_into_a_builder() {
    for seed in 0..50 {
        let tree = tree(seed);
        let root = View::root(&tree);
        let mut builder = FlatTreeBuilder::new();
        builder.view(NodeOrToken::Node(root));
        let copy = Arc::new(builder.finish());
        let copy_root = View::root(&copy);
        let ours: Vec<_> = root.descendants_with_tokens().map(view_key).collect();
        let theirs: Vec<_> = copy_root.descendants_with_tokens().map(view_key).collect();
        // The copy has its lazy nodes' parts copied in as plain nodes: the same elements.
        assert_eq!(ours, theirs);
        assert_eq!(copy_root.text(), root.text());
    }
}

/// An item whose body, a node of `body` with `words` in it, is elided: an edit that turns
/// the body from one kind into another (a literal into a binary expression) leaves the copy
/// with one placeholder kind the same, and changes the copy that keeps each node's kind.
#[test]
fn an_elided_body_of_another_kind_leaves_the_copy_with_one_placeholder_the_same() {
    const LITERAL: SyntaxKind = SyntaxKind(20);
    const BINARY: SyntaxKind = SyntaxKind(21);
    const PLACEHOLDER: SyntaxKind = SyntaxKind(22);
    fn item(body: SyntaxKind, words: &[&str]) -> Arc<FlatTree> {
        let mut builder = FlatTreeBuilder::new();
        builder.start_node(ROOT);
        builder.start_node(GROUP);
        builder.token(WORD, "k");
        builder.token(SPACE, " ");
        builder.start_node(body);
        for (i, word) in words.iter().enumerate() {
            if i > 0 {
                builder.token(SPACE, " ");
            }
            builder.token(WORD, word);
        }
        builder.finish_node();
        builder.token(CLOSE, ";");
        builder.finish_node();
        builder.finish_node();
        Arc::new(builder.finish())
    }
    let copies = |tree: &Arc<FlatTree>| {
        let group = View::root(tree).first_child().expect("the item");
        let body = group.children().next().expect("the body");
        (Arc::new(group.subtree_without_as(&[body], PLACEHOLDER)), Arc::new(group.subtree_without(&[body])))
    };
    let (literal, binary) = (item(LITERAL, &["2"]), item(BINARY, &["2", "*", "2"]));
    let ((literal_as, literal_own), (binary_as, binary_own)) = (copies(&literal), copies(&binary));
    assert_eq!(literal_as, binary_as, "one placeholder kind: the copies are the same");
    assert_eq!(View::root(&literal_as).to_string(), "k ;");
    let placeholder = View::root(&literal_as).children().next().expect("the placeholder");
    assert_eq!((placeholder.kind(), placeholder.text_range().is_empty()), (PLACEHOLDER, true));
    assert_ne!(literal_own, binary_own, "each node's kind kept: the copies differ by it");
}
