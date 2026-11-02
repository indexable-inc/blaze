//! The views (`syntax::view`) against the handles they replace (frontend.md item 16), on every
//! parser fixture and on real files, full parses and outlines: the same elements in the same
//! order with the same kind, range, text, index and neighbours, the same answers to the
//! position searches, and every typed accessor the same (`accessors.rs`, generated). Then the
//! budget: walking a whole parsed file through views allocates nothing.

#![forbid(unsafe_code)]

#[path = "view/accessors.rs"]
mod accessors;

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use syntax::{
    Edition, NodeOrToken, Parse, SourceFile, SyntaxElement, SyntaxNode, TextRange, TextSize,
    TokenAtOffset, WalkEvent,
    view::{self, AstNode as _, HasName as _},
};

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {dir:?}: {e}"));
    let mut entries: Vec<_> = entries.map(|it| it.expect("a directory entry").path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// Every parser fixture, and the sources of `syntax`, `parser` and `hir-def`.
fn corpus() -> Vec<(PathBuf, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut paths = Vec::new();
    for dir in ["parser/test_data", "syntax/src", "parser/src", "hir-def/src"] {
        rust_files(&crates.join(dir), &mut paths);
    }
    assert!(paths.len() > 500, "the corpus is {} files", paths.len());
    paths
        .into_iter()
        .filter_map(|path| Some((fs::read_to_string(&path).ok()?, path)))
        .map(|(text, path)| (path, text))
        .collect()
}

fn parses(text: &str) -> [Parse<SourceFile>; 2] {
    [SourceFile::parse(text, Edition::CURRENT), SourceFile::parse_outline(text, Edition::CURRENT)]
}

type Key = (syntax::SyntaxKind, TextRange);

fn key(it: &SyntaxElement) -> Key {
    (it.kind(), it.text_range())
}

fn view_key(it: view::Element<'_>) -> Key {
    (it.kind(), it.text_range())
}

fn handle_tokens(it: TokenAtOffset<syntax::SyntaxToken>) -> Vec<Key> {
    let key = |it: syntax::SyntaxToken| (it.kind(), it.text_range());
    match it {
        TokenAtOffset::None => vec![],
        TokenAtOffset::Single(it) => vec![key(it)],
        TokenAtOffset::Between(l, r) => vec![key(l), key(r)],
    }
}

fn view_tokens(it: TokenAtOffset<view::Token<'_>>) -> Vec<Key> {
    let key = |it: view::Token<'_>| (it.kind(), it.text_range());
    match it {
        TokenAtOffset::None => vec![],
        TokenAtOffset::Single(it) => vec![key(it)],
        TokenAtOffset::Between(l, r) => vec![key(l), key(r)],
    }
}

/// Every answer of the views of `parse` against the handles'.
fn check(path: &Path, parse: &Parse<SourceFile>) {
    let handle = parse.syntax_node();
    let root = parse.root();
    let lazies: Vec<bool> = handle.children().map(|it| it.is_lazy()).collect();
    assert_eq!(lazies, root.children().map(|it| it.is_lazy()).collect::<Vec<_>>(), "{path:?}");

    let handles: Vec<SyntaxElement> = handle.descendants_with_tokens().collect();
    let views: Vec<view::Element<'_>> = root.descendants_with_tokens().collect();
    assert_eq!(handles.len(), views.len(), "{path:?}: as many elements");
    let index: HashMap<SyntaxElement, usize> =
        handles.iter().cloned().enumerate().map(|(i, it)| (it, i)).collect();
    let same = |v: Option<view::Element<'_>>, h: Option<SyntaxElement>| match (v, h) {
        (Some(v), Some(h)) => v == views[index[&h]],
        (None, None) => true,
        _ => false,
    };
    for (i, (h, &v)) in handles.iter().zip(&views).enumerate() {
        let at = || format!("{path:?}, element {i} {:?}", key(h));
        assert_eq!(key(h), view_key(v), "{}", at());
        assert_eq!(h.index(), v.index(), "{}", at());
        assert!(same(v.parent().map(NodeOrToken::Node), h.parent().map(NodeOrToken::Node)), "{}", at());
        assert!(same(v.next_sibling_or_token(), h.next_sibling_or_token()), "{}", at());
        assert!(same(v.prev_sibling_or_token(), h.prev_sibling_or_token()), "{}", at());
        assert!(view::Node::at(root.shared_file(), v.id()) == v, "{}", at());
        match (h, v) {
            (NodeOrToken::Node(h), NodeOrToken::Node(v)) => {
                assert_eq!(h.text().to_string(), v.text(), "{}", at());
                assert!(same(v.first_child_or_token(), h.first_child_or_token()), "{}", at());
                assert!(same(v.last_child_or_token(), h.last_child_or_token()), "{}", at());
                let node = |it: Option<SyntaxNode>| it.map(NodeOrToken::Node);
                assert!(same(v.first_child().map(NodeOrToken::Node), node(h.first_child())), "{}", at());
                assert!(same(v.last_child().map(NodeOrToken::Node), node(h.last_child())), "{}", at());
                assert!(same(v.next_sibling().map(NodeOrToken::Node), node(h.next_sibling())), "{}", at());
                assert!(same(v.prev_sibling().map(NodeOrToken::Node), node(h.prev_sibling())), "{}", at());
                assert_eq!(h.children().count(), v.children().count(), "{}", at());
                assert_eq!(h.first_child_token_text(), v.first_child_token_text(), "{}", at());
                let range = h.text_range();
                let found = v.descendant_at_range(range, h.kind()).map(NodeOrToken::Node);
                assert!(same(found, node(h.descendant_at_range(range, h.kind()))), "{}", at());
                let found = root.descendant_at_range(range, h.kind()).map(NodeOrToken::Node);
                assert!(same(found, node(handle.descendant_at_range(range, h.kind()))), "{}", at());
                accessors::check(h, v);
            }
            (NodeOrToken::Token(h), NodeOrToken::Token(v)) => {
                assert_eq!(h.text(), v.text(), "{}", at());
                let token = |it: Option<syntax::SyntaxToken>| it.map(NodeOrToken::Token);
                assert!(same(v.next_token().map(NodeOrToken::Token), token(h.next_token())), "{}", at());
                assert!(same(v.prev_token().map(NodeOrToken::Token), token(h.prev_token())), "{}", at());
                let offset = h.text_range().start();
                assert_eq!(
                    handle_tokens(handle.token_at_offset(offset)),
                    view_tokens(root.token_at_offset(offset)),
                    "{}", at()
                );
                let range = h.text_range();
                assert!(same(Some(root.covering_element(range)), Some(handle.covering_element(range))), "{}", at());
            }
            _ => panic!("{}: a node in one walk and a token in the other", at()),
        }
    }
    assert_eq!(
        handle
            .preorder_with_tokens()
            .map(|it| match it {
                WalkEvent::Enter(it) => (true, key(&it)),
                WalkEvent::Leave(it) => (false, key(&it)),
            })
            .collect::<Vec<_>>(),
        root.preorder_with_tokens()
            .map(|it| match it {
                WalkEvent::Enter(it) => (true, view_key(it)),
                WalkEvent::Leave(it) => (false, view_key(it)),
            })
            .collect::<Vec<_>>(),
        "{path:?}: preorder"
    );
    let end = handle.text_range().end();
    assert_eq!(handle_tokens(handle.token_at_offset(end)), view_tokens(root.token_at_offset(end)));
    assert_eq!(format!("{handle:#?}"), format!("{root:#?}"), "{path:?}: the tree's dump");
}

#[test]
fn views_answer_as_handles_on_every_fixture_and_file() {
    // Each file is parsed and checked on its own, so the corpus is shared out to every core:
    // asking every token's position of the root is quadratic in a file's top-level items, and
    // one core takes a quarter of an hour of a debug build.
    let corpus = corpus();
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some((path, text)) = corpus.get(next.fetch_add(1, Ordering::Relaxed)) {
                    for parse in parses(text) {
                        check(path, &parse);
                    }
                }
            });
        }
    });
}

#[test]
fn an_outline_walked_by_views_parses_what_handles_parse() {
    for (path, text) in corpus().into_iter().take(200) {
        let [full, _] = parses(&text);
        let outline = SourceFile::parse_outline(text.as_str(), Edition::CURRENT);
        // Walked by views only: every lazy body is parsed by the walk, to the full parse's tree.
        let walked: Vec<Key> = outline.root().descendants_with_tokens().map(view_key).collect();
        let whole: Vec<Key> = full.root().descendants_with_tokens().map(view_key).collect();
        assert_eq!(walked, whole, "{path:?}");
    }
}

/// Everything a lowering reads in a walk, through views.
fn walk(root: view::Node<'_>) -> (usize, usize) {
    let (mut elements, mut bytes) = (0, 0);
    for element in root.descendants_with_tokens() {
        elements += 1;
        bytes += usize::from(element.text_range().len());
        if let NodeOrToken::Node(node) = element {
            bytes += node.text().len();
            elements += node.children().count() + node.children_with_tokens().count();
            elements += node.ancestors().count();
            if let Some(parent) = node.parent() {
                elements += parent.index();
            }
            if let Some(it) = view::Fn::cast(node) {
                elements += usize::from(it.name().is_some()) + usize::from(it.body().is_some());
                elements += it.param_list().map_or(0, |it| it.params().count());
            }
        }
    }
    for event in root.preorder_with_tokens() {
        if let WalkEvent::Enter(NodeOrToken::Token(token)) = event {
            bytes += token.text().len();
        }
    }
    let offset = TextSize::from(u32::from(root.text_range().end()) / 2);
    if let TokenAtOffset::Single(token) = root.token_at_offset(offset) {
        elements += token.parent_ancestors().count();
    }
    (elements, bytes)
}

#[test]
fn walking_a_whole_file_allocates_nothing() {
    for (path, text) in corpus().into_iter().step_by(7) {
        for parse in parses(&text) {
            // An outline's bodies are parsed by the first walk; the budget is for reads.
            let first = walk(parse.root());
            let again = blaze_alloc_count::assert_at_most(0, &format!("walking {path:?}"), || walk(parse.root()));
            assert_eq!(first, again);
        }
    }
}

#[test]
fn validating_a_file_without_errors_allocates_nothing() {
    let mut checked = 0;
    for (path, text) in corpus().into_iter().step_by(7) {
        let parse = SourceFile::parse(&text, Edition::CURRENT);
        if !parse.errors().is_empty() {
            continue;
        }
        let errors = blaze_alloc_count::assert_at_most(0, &format!("validating {path:?}"), || {
            parse.errors()
        });
        assert!(errors.is_empty());
        checked += 1;
    }
    assert!(checked > 0, "the corpus has no file without errors");
}
