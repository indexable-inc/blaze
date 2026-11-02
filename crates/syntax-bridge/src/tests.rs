use std::{
    alloc::Global,
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_hash::HashMap;
use blaze_ids::rng::SplitMix64;
use expect_test::expect;
use smallvec::smallvec;
use span::{Edition, Span};
use syntax::{NodeOrToken, ast, view};
use test_utils::extract_annotations;
use tt::{Leaf, Punct, Spacing, TopSubtreeBuilder, TtElement, buffer::Cursor};

use crate::{
    Appended, DocCommentDesugarMode, Inserted, Removed, call_input,
    dummy_test_span_utils::{DUMMY, DummyTestSpanMap},
    parse_to_token_tree_static_span,
    reference::{self, syntax_node_to_token_tree, syntax_node_to_token_tree_modified},
    to_matcher_input, to_parser_input, token_tree_to_syntax_node,
};

fn check_punct_spacing(fixture: &str) {
    let parse = ast::SourceFile::parse(fixture, span::Edition::CURRENT);
    assert!(parse.errors().is_empty(), "the fixture parses");
    let subtree = call_input(parse.root(), DummyTestSpanMap, DUMMY, DocCommentDesugarMode::Mbe);
    let mut annotations: HashMap<_, _> = extract_annotations(fixture)
        .into_iter()
        .map(|(range, annotation)| {
            let spacing = match annotation.as_str() {
                "Alone" => Spacing::Alone,
                "Joint" => Spacing::Joint,
                a => panic!("unknown annotation: {a}"),
            };
            (range, spacing)
        })
        .collect();

    let mut cursor = Cursor::new(subtree.as_token_trees());
    while !cursor.eof() {
        while let Some(token_tree) = cursor.token_tree() {
            if let tt::TokenTree::Leaf(Leaf::Punct(Punct {
                spacing, span: Span { range, .. }, ..
            })) = token_tree
                && let Some(expected) = annotations.remove(&range)
            {
                assert_eq!(expected, spacing);
            }
            cursor.bump();
        }
        cursor.bump_or_end();
    }

    assert!(annotations.is_empty(), "unchecked annotations: {annotations:?}");
}

#[test]
fn punct_spacing() {
    check_punct_spacing(
        r#"
fn main() {
    0+0;
   //^ Alone
    0+(0);
   //^ Alone
    0<=0;
   //^ Joint
   // ^ Alone
    0<=(0);
   // ^ Alone
    a=0;
   //^ Alone
    a=(0);
   //^ Alone
    a+=0;
   //^ Joint
   // ^ Alone
    a+=(0);
   // ^ Alone
    a&&b;
   //^ Joint
   // ^ Alone
    a&&(b);
   // ^ Alone
    foo::bar;
   //  ^ Joint
   //   ^ Alone
    use foo::{bar,baz,};
   //       ^ Alone
   //            ^ Alone
   //                ^ Alone
    struct Struct<'a> {};
   //            ^ Joint
   //             ^ Joint
    Struct::<0>;
   //       ^ Alone
    Struct::<{0}>;
   //       ^ Alone
    ;;
  //^ Joint
  // ^ Alone
}
        "#,
    );
}

#[test]
fn scientific_notation_field_access_recovers() {
    let tt = parse_to_token_tree_static_span(
        span::Edition::CURRENT,
        DUMMY,
        r#"
fn main() {
    s.00E+10;
}
"#,
    )
    .unwrap();

    let (parse, _) =
        token_tree_to_syntax_node(tt.view(), parser::TopEntryPoint::SourceFile, &mut |_| {
            span::Edition::CURRENT
        });
    let parse = parse.cast::<ast::SourceFile>().unwrap();

    expect![[r#"
        SOURCE_FILE@0..19
          FN@0..19
            FN_KW@0..2 "fn"
            NAME@2..6
              IDENT@2..6 "main"
            PARAM_LIST@6..8
              L_PAREN@6..7 "("
              R_PAREN@7..8 ")"
            BLOCK_EXPR@8..19
              STMT_LIST@8..19
                L_CURLY@8..9 "{"
                EXPR_STMT@9..18
                  FIELD_EXPR@9..17
                    FIELD_EXPR@9..17
                      PATH_EXPR@9..10
                        PATH@9..10
                          PATH_SEGMENT@9..10
                            NAME_REF@9..10
                              IDENT@9..10 "s"
                      DOT@10..11 "."
                      ERROR@11..17
                        FLOAT_NUMBER@11..17 "00E+10"
                  SEMICOLON@17..18 ";"
                R_CURLY@18..19 "}"
        error 11..11: illegal float literal
    "#]]
    .assert_eq(&parse.debug_dump());
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|it| it.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// The parser's test data without errors and the `syntax` crate's own sources.
fn corpus() -> Vec<(PathBuf, Arc<str>)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut paths = Vec::new();
    for dir in ["parser/test_data/parser/ok", "parser/test_data/parser/inline/ok", "syntax/src"] {
        rust_files(&crates.join(dir), &mut paths);
    }
    assert!(paths.len() > 300, "the corpus is {} files", paths.len());
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (path, Arc::from(text))
        })
        .collect()
}

fn token_tree(node: view::Node<'_>) -> tt::TopSubtree {
    call_input(node, DummyTestSpanMap, DUMMY, DocCommentDesugarMode::Mbe)
}

/// The sink's text buffer is the thread's string scratch: after a conversion it is back on the
/// shelf, as long as the longest token it wrote, for the next conversion to take instead of
/// allocating its own.
#[test]
fn a_sink_gives_its_text_buffer_back() {
    let text = "fn f() { let a_long_identifier = 1; }";
    let parse = ast::SourceFile::parse(text, Edition::CURRENT);
    let tree = token_tree(parse.root());
    let mut edition = |_: span::SyntaxContext| Edition::CURRENT;
    drop(token_tree_to_syntax_node(tree.view(), parser::TopEntryPoint::SourceFile, &mut edition));
    let buf = blaze_mem::scratch::lend_string();
    assert!(buf.capacity() >= "a_long_identifier".len(), "the sink's buffer is on the shelf");
}

/// A parse's map is the worker's map copied out: its columns at their exact lengths (glued
/// punctuation makes fewer pieces than tokens, which `with_capacity(tokens)` overshot), the
/// same map a cold parse gives, and the worker's map back on the thread for the next parse.
#[test]
fn a_parse_gives_its_map_at_its_length() {
    let text = "fn f() { a::b..=c; let d = (1, 2); }".repeat(50);
    let parse = ast::SourceFile::parse(&text, Edition::CURRENT);
    let tree = token_tree(parse.root());
    let mut edition = |_: span::SyntaxContext| Edition::CURRENT;
    let (_, cold) =
        token_tree_to_syntax_node(tree.view(), parser::TopEntryPoint::SourceFile, &mut edition);
    let (_, warm) =
        token_tree_to_syntax_node(tree.view(), parser::TopEntryPoint::SourceFile, &mut edition);
    assert!(warm == cold);
    assert_eq!(warm.capacity(), warm.iter().count(), "the map's columns are exact");
    let kept = crate::SINK_MAP.take().expect("the worker's map is back on the thread");
    assert!(kept.capacity() >= warm.capacity());
}

#[test]
fn an_outline_has_the_token_trees_of_a_full_parse() {
    for (path, text) in corpus() {
        let path = path.display();
        let full_parse = ast::SourceFile::parse_shared(text.clone(), Edition::CURRENT);
        let outline_parse = ast::SourceFile::parse_outline(text, Edition::CURRENT);
        let (full, outline) = (full_parse.root(), outline_parse.root());
        let mut in_full = HashMap::default();
        for node in full.descendants() {
            in_full.entry((node.kind(), node.text_range())).or_insert(node);
        }
        let in_full = |node: view::Node<'_>| {
            *in_full
                .get(&(node.kind(), node.text_range()))
                .unwrap_or_else(|| panic!("{path}: {node:?} is not in the full parse"))
        };

        let tree = outline.file();
        let lazies: Vec<u32> = tree.lazies().collect();
        for &at in &lazies {
            let body = view::Node::at(outline.shared_file(), view::NodeId { part: view::NodeId::FILE, at })
                .into_node()
                .expect("a lazy element is a node");
            assert!(body.is_lazy(), "{path}: {body:?} was parsed before it was asked about");
            assert_eq!(token_tree(body), token_tree(in_full(body)), "{path}: {body:?}");
            assert!(tree.is_parsed(at), "{path}: {body:?}");
            let owner = body.parent().expect("a body has an owner");
            assert_eq!(token_tree(owner), token_tree(in_full(owner)), "{path}: {owner:?}");
        }

        let expected = token_tree(full);
        assert_eq!(token_tree(outline), expected, "{path}");
    }
}

#[test]
fn a_lazy_body_in_a_node_is_parsed_for_its_token_tree() {
    let text: Arc<str> = Arc::from("struct S;\nfn f() { let x = 1 + 2; g(x) }\n");
    let outline = ast::SourceFile::parse_outline(text.clone(), Edition::CURRENT);
    let tree = outline.root().file();
    let lazies: Vec<u32> = tree.lazies().collect();
    assert_eq!(lazies.len(), 1);
    assert!(!tree.is_parsed(lazies[0]));
    let full = ast::SourceFile::parse_shared(text, Edition::CURRENT);
    assert_eq!(token_tree(outline.root()), token_tree(full.root()));
    assert!(tree.is_parsed(lazies[0]));
}

#[test]
fn a_flat_tree_converts_as_the_cursor_does() {
    for (path, text) in corpus() {
        let path = path.display();
        let full = ast::SourceFile::parse_shared(text, Edition::CURRENT).syntax_node();
        assert!(full.flat().is_some(), "{path}: a full parse is a flat tree");
        for mode in [DocCommentDesugarMode::Mbe, DocCommentDesugarMode::ProcMacro] {
            let nodes = std::iter::once(full.clone())
                .chain(full.descendants().filter(|it| it.kind() == syntax::SyntaxKind::FN));
            for node in nodes {
                let flat = syntax_node_to_token_tree(&node, DummyTestSpanMap, DUMMY, mode);
                let cursor = syntax_node_to_token_tree_modified(
                    &node,
                    DummyTestSpanMap,
                    Appended::<_, Global>::default(),
                    Removed::<_, Global>::default(),
                    DUMMY,
                    mode,
                    |_, _| (true, Inserted::new()),
                );
                assert_eq!(flat, cursor, "{path}: {node:?}");
                assert_eq!(flat.to_string(), cursor.to_string(), "{path}: {node:?}");
            }
        }
    }
}

/// [`crate::call_input`] of a view is the token tree the cursor's walk of the same node makes:
/// for the file, each `fn` (an attribute macro's input) and each token tree (a call's input),
/// from a full parse and from an outline, whose `fn` bodies are lazy parts it enters.
/// The nodes of the tree under `root` by kind and range, the first in preorder of each: what
/// a walk finding the first node of a kind at a range finds, made once for every view.
fn nodes_by_kind_and_range(
    root: &syntax::SyntaxNode,
) -> HashMap<(syntax::SyntaxKind, syntax::TextRange), syntax::SyntaxNode> {
    let mut nodes = HashMap::default();
    for node in root.descendants() {
        nodes.entry((node.kind(), node.text_range())).or_insert(node);
    }
    nodes
}

#[test]
fn a_views_call_input_is_the_cursors_token_tree() {
    use syntax::SyntaxKind::{FN, TOKEN_TREE};
    for (path, text) in corpus() {
        let path = path.display();
        let full = ast::SourceFile::parse_shared(text.clone(), Edition::CURRENT);
        let full_nodes = nodes_by_kind_and_range(&full.syntax_node());
        let outline = ast::SourceFile::parse_outline(text, Edition::CURRENT);
        for parse in [&full, &outline] {
            let root = parse.root();
            let views = std::iter::once(root)
                .chain(root.descendants().filter(|it| matches!(it.kind(), FN | TOKEN_TREE)));
            for view in views {
                let range = view.text_range();
                if range.is_empty() {
                    continue;
                }
                let node = full_nodes
                    .get(&(view.kind(), range))
                    .unwrap_or_else(|| panic!("{path}: no {:?} of {range:?} in the full parse", view.kind()));
                for mode in [DocCommentDesugarMode::Mbe, DocCommentDesugarMode::ProcMacro] {
                    let new = crate::call_input(view, DummyTestSpanMap, DUMMY, mode);
                    let cursor = syntax_node_to_token_tree_modified(
                        &node,
                        DummyTestSpanMap,
                        Appended::<_, Global>::default(),
                        Removed::<_, Global>::default(),
                        DUMMY,
                        mode,
                        |_, _| (true, Inserted::new()),
                    );
                    assert_eq!(new, cursor, "{path}: {view:?}, outline {}", parse.is_outline());
                    assert_eq!(new.to_string(), cursor.to_string(), "{path}: {view:?}");
                }
            }
        }
    }
}

/// What a test's `on_event` does at an element, the same for the cursor's walk and the view's: a
/// function of the element's kind and range and of whether the walk enters or leaves it.
fn modified_action(
    seed: u64,
    kind: syntax::SyntaxKind,
    range: syntax::TextRange,
    enter: bool,
) -> u64 {
    let mixed = seed
        ^ (kind as u64) << 48
        ^ u64::from(u32::from(range.start())) << 24
        ^ u64::from(u32::from(range.end()))
        ^ u64::from(enter) << 63;
    SplitMix64::new(mixed).below(16)
}

/// [`crate::call_input_modified`] removes, appends after and follows `on_event` as the cursor's
/// [`syntax_node_to_token_tree_modified`] does: over the corpus's files and `fn`s from full and
/// outline parses, with random removal and append sets of nodes and tokens, and an `on_event`
/// that skips subtrees, drops events and inserts leaves.
#[test]
fn a_views_modified_input_is_the_cursors() {
    use syntax::{NodeOrToken::Node, SyntaxKind::FN, WalkEvent};
    let punct = |char| Leaf::Punct(Punct { char, spacing: Spacing::Alone, span: DUMMY });
    for (seed, (path, text)) in (0u64..).zip(corpus()) {
        let path = path.display();
        let full = ast::SourceFile::parse_shared(text.clone(), Edition::CURRENT);
        let full_nodes = nodes_by_kind_and_range(&full.syntax_node());
        let outline = ast::SourceFile::parse_outline(text, Edition::CURRENT);
        for parse in [&full, &outline] {
            let root = parse.root();
            let fns = root.descendants().filter(|it| matches!(it.kind(), FN));
            for view in std::iter::once(root).chain(fns) {
                let range = view.text_range();
                if range.is_empty() {
                    continue;
                }
                let node = full_nodes
                    .get(&(view.kind(), range))
                    .unwrap_or_else(|| panic!("{path}: no {:?} of {range:?} in the full parse", view.kind()));
                let mut rng = SplitMix64::new(seed);
                let (mut remove, mut view_remove) =
                    (Removed::<_, Global>::default(), Removed::<_, Global>::default());
                let (mut append, mut view_append) =
                    (Appended::<_, Global>::default(), Appended::<_, Global>::default());
                let mut views = view.preorder_with_tokens();
                for event in node.preorder_with_tokens() {
                    match (event, views.next()) {
                        (WalkEvent::Enter(element), Some(WalkEvent::Enter(seen))) => {
                            assert_eq!(
                                (element.kind(), element.text_range()),
                                (seen.kind(), seen.text_range()),
                                "{path}: the walks differ"
                            );
                            if rng.one_in(12) {
                                remove.insert(element.clone());
                                view_remove.insert(seen.id());
                            }
                            if rng.one_in(10) {
                                let leaves = smallvec![punct('$'), punct(',')];
                                view_append.insert(seen.id(), leaves.clone());
                                append.insert(element, leaves);
                            }
                        }
                        (WalkEvent::Leave(_), Some(WalkEvent::Leave(_))) => {}
                        (event, seen) => panic!("{path}: {event:?} walked against {seen:?}"),
                    }
                }
                assert!(views.next().is_none(), "{path}: the view's walk is longer");
                for mode in [DocCommentDesugarMode::Mbe, DocCommentDesugarMode::ProcMacro] {
                    let cursor = syntax_node_to_token_tree_modified(
                        &node,
                        DummyTestSpanMap,
                        append.clone(),
                        remove.clone(),
                        DUMMY,
                        mode,
                        |preorder, event| {
                            let (enter, element) = match event {
                                WalkEvent::Enter(it) => (true, it),
                                WalkEvent::Leave(it) => (false, it),
                            };
                            match modified_action(seed, element.kind(), element.text_range(), enter)
                            {
                                0 if enter && matches!(element, Node(_)) => {
                                    preorder.skip_subtree();
                                    (false, smallvec![punct('#')])
                                }
                                1 => (true, smallvec![punct('@')]),
                                2 => (false, Inserted::new()),
                                _ => (true, Inserted::new()),
                            }
                        },
                    );
                    let new = crate::call_input_modified(
                        view,
                        DummyTestSpanMap,
                        view_append.clone(),
                        view_remove.clone(),
                        DUMMY,
                        mode,
                        |preorder, event| {
                            let (enter, element) = match *event {
                                WalkEvent::Enter(it) => (true, it),
                                WalkEvent::Leave(it) => (false, it),
                            };
                            match modified_action(seed, element.kind(), element.text_range(), enter)
                            {
                                0 if enter && matches!(element, Node(_)) => {
                                    preorder.skip_subtree();
                                    (false, smallvec![punct('#')])
                                }
                                1 => (true, smallvec![punct('@')]),
                                2 => (false, Inserted::new()),
                                _ => (true, Inserted::new()),
                            }
                        },
                    );
                    assert_eq!(new, cursor, "{path}: {view:?}, outline {}", parse.is_outline());
                }
            }
        }
    }
}

/// A modified conversion whose callback inserts a leaf at every event allocates the same as one
/// that inserts none: the inserted leaves are inline, and the maps are the caller's.
#[test]
fn inserted_leaves_allocate_nothing() {
    let text = "fn f() { let a = 1; let b = 2; }";
    let parse = ast::SourceFile::parse(text, Edition::CURRENT);
    let leaf = Leaf::Punct(Punct { char: '#', spacing: Spacing::Alone, span: DUMMY });
    let convert = |insert: bool| {
        let leaf = &leaf;
        crate::call_input_modified(
            parse.root(),
            DummyTestSpanMap,
            Appended::<_, Global>::default(),
            Removed::<_, Global>::default(),
            DUMMY,
            DocCommentDesugarMode::Mbe,
            move |_, _| {
                // clone: a test's leaf, inline in the `SmallVec`.
                (true, if insert { smallvec![leaf.clone()] } else { Inserted::new() })
            },
        )
    };
    drop((convert(false), convert(true)));
    let (_, none) = blaze_alloc_count::measure(|| convert(false));
    let (_, some) = blaze_alloc_count::measure(|| convert(true));
    assert_eq!(none.allocations, some.allocations);
}

/// What a closure gives, or that it panicked: the new and the old conversion must fail alike too.
fn caught<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// Asserts `tree` converts to the same parser and matcher input as the old conversion did, for
/// each view the callers take, and parses to the same tree, errors and span map as each kind of
/// expansion.
fn assert_same_as_reference(
    tree: &tt::TopSubtree,
    edition: &mut impl FnMut(span::SyntaxContext) -> Edition,
    what: &impl Fn() -> String,
) {
    for view in [tree.view().strip_invisible(), tree.token_trees(), tree.as_token_trees()] {
        let new = caught(|| to_parser_input(view, &mut *edition));
        let old = caught(|| reference::to_parser_input(view, &mut *edition));
        match (&new, &old) {
            (Some(new), Some(old)) => {
                assert!(reference::same_input(new, old), "{}: the parser's input", what())
            }
            (None, None) => continue,
            _ => panic!("{}: one conversion panicked, the new one: {}", what(), new.is_none()),
        }
        let new = to_matcher_input(view, &mut *edition);
        let old = reference::to_matcher_input(view, &mut *edition);
        assert!(reference::same_input(&new, &old), "{}: the matcher's input", what());
    }
    assert_words_same_as_reference(tree, &mut *edition, what);
    use parser::TopEntryPoint::*;
    for entry in [MacroItems, MacroStmts, Expr, Pattern, Type, MetaItem] {
        let new = caught(|| {
            let (parse, map) = token_tree_to_syntax_node(tree.view(), entry, &mut *edition);
            (parse.debug_dump(), map)
        });
        let old = caught(|| {
            let (parse, map) = reference::token_tree_to_syntax_node(tree, entry, &mut *edition);
            (parse.debug_dump(), map)
        });
        match (new, old) {
            (Some((new_tree, new_map)), Some((old_tree, old_map))) => {
                assert_eq!(new_tree, old_tree, "{}: parsed as {entry:?}", what());
                assert!(new_map == old_map, "{}: the span maps as {entry:?}", what());
            }
            (None, None) => (),
            (new, _) => {
                panic!("{}: one parse as {entry:?} panicked, the new: {}", what(), new.is_none())
            }
        }
    }
}

/// Every view a caller can take of `tree`, at every depth: what is left from each token tree
/// on (all of them at a top level of at most 64 token trees, the views that end where the tree
/// does and are read in place; the last 8 of every other level, as of a file's long top level,
/// each such view converted whole by the reference), and each run of up to 4 token trees, a matcher's window, which
/// ends inside the tree (never inside a lifetime). None starts inside a lifetime either.
fn every_view(tree: &tt::TopSubtree) -> Vec<tt::TokenTreesView<'_>> {
    fn level<'a>(iter: tt::iter::TtIter<'a>, small: bool, out: &mut Vec<tt::TokenTreesView<'a>>) {
        let is_quote = |it: &Option<TtElement<'_>>| {
            matches!(it, Some(TtElement::Leaf(Leaf::Punct(Punct { char: '\'', .. }))))
        };
        let n = iter.clone().count();
        let mut at = iter;
        // A lifetime's `'` and its name are one token: no view starts or ends between them.
        let mut after_quote = false;
        for i in 0..=n {
            if !after_quote {
                if small || n - i <= 8 {
                    out.push(at.remaining());
                }
                let start = at.savepoint();
                let mut end = at.clone();
                for _ in 0..4.min(n - i) {
                    if !is_quote(&end.next()) {
                        out.push(end.from_savepoint(start));
                    }
                }
            }
            let next = at.next();
            after_quote = is_quote(&next);
            match next {
                Some(TtElement::Subtree(_, inner)) => level(inner, false, out),
                Some(TtElement::Leaf(_)) => (),
                None => break,
            }
        }
    }
    let small = tree.iter().count() <= 64;
    let mut out = vec![tree.view().strip_invisible(), tree.token_trees(), tree.as_token_trees()];
    level(tree.iter(), small, &mut out);
    out
}

/// Asserts the parser's tokens of every view of `tree` read from the words it keeps
/// ([`crate::view_tokens`]) are the reference's input of that view, token by token, as a matcher's
/// and as an expansion's. Making a tree's words converts all of it, so it panics on a token only
/// a matcher's input of the whole tree panics on: a lone `'` or a punct that is no token's.
fn assert_words_same_as_reference(
    tree: &tt::TopSubtree,
    edition: &mut impl FnMut(span::SyntaxContext) -> Edition,
    what: &impl Fn() -> String,
) {
    let whole_panics = caught(|| reference::to_matcher_input(tree.as_token_trees(), &mut *edition)).is_none();
    let mut copy = parser::Input::with_capacity(0);
    for view in every_view(tree) {
        for fragments in [true, false] {
            let new = caught(|| {
                let tokens = crate::view_tokens(view, fragments, &mut *edition, &mut copy);
                (0..tokens.len()).map(|i| tokens.token(i)).collect::<Vec<_>>()
            });
            let old = caught(|| match fragments {
                true => reference::to_matcher_input(view, &mut *edition),
                false => reference::to_parser_input(view, &mut *edition),
            });
            match (new, old) {
                (Some(new), Some(old)) => {
                    let old = (0..old.len()).map(|i| old.token(i)).collect::<Vec<_>>();
                    assert_eq!(new, old, "{}: the words of {view:?}, fragments {fragments}", what());
                }
                (None, _) => assert!(whole_panics, "{}: the words of {view:?} panicked", what()),
                (Some(_), None) => panic!("{}: only the reference panicked on {view:?}", what()),
            }
        }
    }
}

const EDITIONS: [Edition; 4] =
    [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021, Edition::Edition2024];

/// Copies `trees` into `out` with what expansions add: groups without delimiters around some
/// token trees (and some empty ones), as transcribed fragments are, and runs of tokens from the
/// root contexts of different editions, as tokens from different macros are. A lifetime's `'`
/// and its name stay in one group, as the lexer and every fragment keep them.
fn regroup(
    trees: tt::TokenTreesView<'_>,
    rng: &mut SplitMix64,
    ctx: &mut usize,
    out: &mut TopSubtreeBuilder,
) {
    fn with_ctx(span: Span, rng: &mut SplitMix64, ctx: &mut usize) -> Span {
        if rng.one_in(6) {
            *ctx = rng.index(EDITIONS.len());
        }
        Span { ctx: span::SyntaxContext::root(EDITIONS[*ctx]), ..span }
    }
    let mut iter = trees.iter();
    while let Some(tree) = iter.next() {
        if rng.one_in(16) {
            out.open(tt::DelimiterKind::Invisible, DUMMY);
            out.close(DUMMY);
        }
        let group = rng.one_in(4);
        if group {
            out.open(tt::DelimiterKind::Invisible, with_ctx(DUMMY, rng, ctx));
        }
        let mut tree = Some(tree);
        while let Some(it) = tree.take() {
            match it {
                TtElement::Leaf(mut leaf) => {
                    let quote = matches!(&leaf, Leaf::Punct(Punct { char: '\'', .. }));
                    let span = match &mut leaf {
                        Leaf::Punct(it) => &mut it.span,
                        Leaf::Ident(it) => &mut it.span,
                        Leaf::Literal(it) => &mut it.span,
                    };
                    *span = with_ctx(*span, rng, ctx);
                    out.push(leaf);
                    if quote {
                        tree = iter.next();
                    }
                }
                TtElement::Subtree(subtree, inner) => {
                    out.open(subtree.delimiter.kind, with_ctx(subtree.delimiter.open, rng, ctx));
                    regroup(inner.remaining(), rng, ctx, out);
                    out.close(with_ctx(subtree.delimiter.close, rng, ctx));
                }
            }
        }
        if group {
            out.close(with_ctx(DUMMY, rng, ctx));
        }
    }
}

#[test]
fn parser_input_is_the_old_conversions() {
    let mut current = |_: span::SyntaxContext| Edition::CURRENT;
    let mut by_root = |ctx: span::SyntaxContext| {
        *EDITIONS
            .iter()
            .find(|&&it| span::SyntaxContext::root(it) == ctx)
            .expect("the trees here hold root contexts only")
    };
    for (file, (path, text)) in corpus().into_iter().enumerate() {
        let path = path.display();
        let full = ast::SourceFile::parse_shared(text, Edition::CURRENT);
        let root = full.root();
        let nodes =
            std::iter::once(root).chain(root.descendants().filter(|it| it.kind() == syntax::SyntaxKind::FN));
        for (idx, node) in nodes.enumerate() {
            for mode in [DocCommentDesugarMode::Mbe, DocCommentDesugarMode::ProcMacro] {
                let tree = call_input(node, DummyTestSpanMap, DUMMY, mode);
                assert_same_as_reference(&tree, &mut current, &|| format!("{path}: {node:?}"));
            }
            let tree = token_tree(node);
            let seed = (file as u64) << 20 | idx as u64;
            let mut rng = SplitMix64::new(seed);
            let mut out = TopSubtreeBuilder::new(tree.top_subtree().delimiter);
            regroup(tree.token_trees(), &mut rng, &mut 0, &mut out);
            let regrouped = out.build();
            assert_same_as_reference(&regrouped, &mut by_root, &|| {
                format!("{path}: {node:?} regrouped\n{regrouped:?}")
            });
            // The words the tree keeps hold `by_root`'s editions; read with other answers, each
            // view's are made afresh, and the kept ones stay `by_root`'s.
            assert_words_same_as_reference(&regrouped, &mut current, &|| {
                format!("{path}: {node:?} regrouped, read in {:?}\n{regrouped:?}", Edition::CURRENT)
            });
            assert_words_same_as_reference(&regrouped, &mut by_root, &|| {
                format!("{path}: {node:?} regrouped, read again by root\n{regrouped:?}")
            });
        }
    }
}

/// A mapper giving every token one span, as [`parse_to_token_tree_static_span`] does.
struct StaticSpanMap;

impl crate::SpanMapper for StaticSpanMap {
    fn span_for(&self, _: syntax::TextRange) -> Span {
        DUMMY
    }
}

/// The tokens and nodes of `node` but its trivia, in preorder, as kind and text.
fn structure(node: view::Node<'_>) -> Vec<(syntax::SyntaxKind, String)> {
    node.descendants_with_tokens()
        .filter_map(|it| match it {
            NodeOrToken::Node(it) => Some((it.kind(), String::new())),
            NodeOrToken::Token(it) => {
                (!it.kind().is_trivia()).then(|| (it.kind(), it.text().to_owned()))
            }
        })
        .collect()
}

/// Lexing a file into token trees gives what converting its syntax tree gives, and parsing those
/// tokens gives the file's syntax tree but its trivia: no step between the lexer's tokens, the
/// token trees and the parser changes a token.
#[test]
fn lexing_then_parsing_gives_the_files_tree() {
    let mut parsed = 0;
    for (path, text) in corpus() {
        let path = path.display();
        // A shebang and a frontmatter are only in the file.
        if text.starts_with("#!") || text.starts_with("---") {
            continue;
        }
        let full_parse = ast::SourceFile::parse_shared(text.clone(), Edition::CURRENT);
        let full = full_parse.root();
        let lexed = parse_to_token_tree_static_span(Edition::CURRENT, DUMMY, &text)
            .unwrap_or_else(|| panic!("{path}: the corpus lexes"));
        let converted = call_input(full, StaticSpanMap, DUMMY, DocCommentDesugarMode::ProcMacro);
        assert_eq!(lexed, converted, "{path}");
        // A doc comment is a `DOC_COMMENT` node in the file (or a comment where no item or
        // field follows it) and an attribute in its tokens.
        let has_docs = full.descendants_with_tokens().any(|it| match it {
            NodeOrToken::Node(it) => it.kind() == syntax::SyntaxKind::DOC_COMMENT,
            NodeOrToken::Token(it) => {
                it.kind() == syntax::SyntaxKind::COMMENT
                    && ["///", "//!", "/**", "/*!"].iter().any(|doc| it.text().starts_with(doc))
            }
        });
        if has_docs {
            continue;
        }
        let (parse, _) = token_tree_to_syntax_node(
            lexed.view(),
            parser::TopEntryPoint::SourceFile,
            &mut |_| Edition::CURRENT,
        );
        assert_eq!(structure(parse.root()), structure(full), "{path}");
        parsed += 1;
    }
    assert!(parsed > 200, "only {parsed} files parsed back");
}

/// An identifier interned by the hash the lexer took of it is the symbol interning its text
/// gives, in every edition: keywords, `_`, raw identifiers, compiled-in names and names of 8
/// bytes or fewer, longer and non-ASCII.
#[test]
fn identifiers_interned_by_the_lexers_hash_are_interned_text() {
    let text = "fn r#fn(self, _: Self) -> Option<Some> { let r#a_much_longer_name = std::iter::new; \
                crate::ünïcödé::exactly8 + async + gen + r#union + Ok + iter_mut + _x }";
    for edition in Edition::iter() {
        let tree = parse_to_token_tree_static_span(edition, DUMMY, text).expect("the text lexes");
        let mut names = 0;
        let mut cursor = Cursor::new(tree.as_token_trees());
        while !cursor.eof() {
            while let Some(token_tree) = cursor.token_tree() {
                if let tt::TokenTree::Leaf(Leaf::Ident(ident)) = token_tree {
                    assert_eq!(ident.sym, intern::Symbol::intern(ident.sym.as_str()), "{edition:?}");
                    names += 1;
                }
                cursor.bump();
            }
            cursor.bump_or_end();
        }
        assert_eq!(names, 21, "{edition:?}");
    }
}

/// Converting a tree into an input that was used before allocates nothing: the keywords' table
/// is built once, the editions of a tree with one context stay inline, and the input keeps its
/// buffers.
#[test]
fn a_reused_input_converts_without_allocating() {
    let text = r#"
        macro_rules! m { ($($t:tt)*) => { union U<'a> { r#try: &'a u8, _x: f32 } } }
        fn f<'a>(r#fn: &'a [u8; 2], s: &str) -> impl Fn() + 'a {
            let _ = (1.5, 2.0f64, 1., 'c', b"b", r"r", 0x1F_u32, s.len(), async { 1 }.await);
            move || if let [a, ..] = r#fn { yield_(*a >> 1) } else { loop {} }
        }
    "#;
    let parse = ast::SourceFile::parse(text, Edition::CURRENT);
    let tree = token_tree(parse.root());
    let mut edition = |_: span::SyntaxContext| Edition::CURRENT;
    let mut input = parser::Input::with_capacity(0);
    crate::to_parser_input_into(tree.token_trees(), &mut edition, &mut input);
    let tokens = input.len();
    blaze_alloc_count::assert_at_most(0, "a parser's input into a used one", || {
        crate::to_parser_input_into(tree.token_trees(), &mut edition, &mut input)
    });
    assert_eq!(input.len(), tokens);
    crate::to_matcher_input_into(tree.token_trees(), &mut edition, &mut input);
    blaze_alloc_count::assert_at_most(0, "a matcher's input into a used one", || {
        crate::to_matcher_input_into(tree.token_trees(), &mut edition, &mut input)
    });
    assert_eq!(input.len(), tokens);
}

