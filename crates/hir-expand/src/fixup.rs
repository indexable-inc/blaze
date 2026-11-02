//! To make attribute macros work reliably when typing, we need to take care to
//! fix up syntax errors in the code we're passing to them.

use blaze_hash::FixedState;
use blaze_mem::Frame;
use intern::sym;
use smallvec::smallvec;
use span::{
    ErasedFileAstId, FIXUP_ERASED_FILE_AST_ID_MARKER, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor,
    SyntaxContext,
};
use stdx::never;
use syntax::{
    SyntaxKind, TextRange, TextSize, match_ast,
    view::{self, AstNode as _, HasLoopBody as _, NodeId},
};
use syntax_bridge::{Appended, DocCommentDesugarMode, Removed};
use thin_vec::ThinVec;
use tt::{Spacing, TransformTtAction, transform_tt};

use crate::{
    span_map::SpanMap,
    tt::{self, Ident, Leaf, Punct, TopSubtree},
};

/// The result of calculating fixes for a syntax node -- a bunch of changes
/// (appending to and replacing nodes), the information that is needed to
/// reverse those changes afterwards, and a token map.
///
/// `K` names an element: a view's [`NodeId`].
pub(crate) struct SyntaxFixups<'f, K = NodeId> {
    pub(crate) append: Appended<K, &'f Frame>,
    pub(crate) remove: Removed<K, &'f Frame>,
    pub(crate) undo_info: SyntaxFixupUndoInfo,
}

impl<'f, K> SyntaxFixups<'f, K> {
    /// No fixups, their maps in `frame`.
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        SyntaxFixups {
            append: Appended::with_hasher_in(FixedState, frame),
            remove: Removed::with_hasher_in(FixedState, frame),
            undo_info: SyntaxFixupUndoInfo::NONE,
        }
    }
}

/// This is the information needed to reverse the fixups.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyntaxFixupUndoInfo {
    original: Option<ThinVec<TopSubtree>>,
}

impl SyntaxFixupUndoInfo {
    pub(crate) const NONE: Self = SyntaxFixupUndoInfo { original: None };
}

// We mark spans with `FIXUP_DUMMY_AST_ID` to indicate that they are fake.
const FIXUP_DUMMY_AST_ID: ErasedFileAstId = FIXUP_ERASED_FILE_AST_ID_MARKER;
const FIXUP_DUMMY_RANGE: TextRange = TextRange::empty(TextSize::new(0));
// If the fake span has this range end, that means that the range start is an index into the
// `original` list in `SyntaxFixupUndoInfo`.
const FIXUP_DUMMY_RANGE_END: TextSize = TextSize::new(!0);

/// The fixups of the syntax errors under `node`: [`fixup_syntax`], over a view of the
/// database's tree.
macro_rules! fixup_syntax_impl {
    ($name:ident, $Node:ty, $Owned:ty, $Key:ty, $ast:ident, $key:ident, $to_tt:path) => {
        #[allow(clippy::clone_on_copy, reason = "the body was written for handles too; a view is `Copy`")]
        pub(crate) fn $name<'f>(
            frame: &'f Frame,
            span_map: SpanMap<'_>,
            node: $Node,
            call_site: Span,
            mode: DocCommentDesugarMode,
        ) -> SyntaxFixups<'f, $Key> {
            fn has_error(node: &$Owned) -> bool {
                node.children().any(|c| c.kind() == SyntaxKind::ERROR)
            }

            fn can_handle_error(node: &$Owned) -> bool {
                view::Expr::can_cast(node.kind())
            }

            fn has_error_to_handle(node: &$Owned) -> bool {
                has_error(node) || node.children().any(|c| !can_handle_error(&c) && has_error_to_handle(&c))
            }

            let SyntaxFixups { mut append, mut remove, .. } = SyntaxFixups::<$Key>::new_in(frame);
            let mut preorder = node.preorder();
            let mut original: Vec<TopSubtree, &Frame> = Vec::new_in(frame);
            let dummy_range = FIXUP_DUMMY_RANGE;
            let fake_span = |range| {
                let span = span_map.span_for_range(range);
                Span {
                    range: dummy_range,
                    anchor: SpanAnchor { ast_id: FIXUP_DUMMY_AST_ID, ..span.anchor },
                    ctx: span.ctx,
                }
            };
            while let Some(event) = preorder.next() {
                let syntax::WalkEvent::Enter(node) = event else { continue };

                let node_range = node.text_range();
                if can_handle_error(&node) && has_error_to_handle(&node) {
                    // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                    remove.insert($key!(node.clone()));
                    // the node contains an error node, we have to completely replace it by something valid
                    let original_tree =
                        $to_tt(&node, span_map, call_site, mode);
                    let idx = original.len() as u32;
                    original.push(original_tree);
                    let span = span_map.span_for_range(node_range);
                    let replacement = Leaf::Ident(Ident {
                        sym: sym::__ra_fixup,
                        span: Span {
                            range: TextRange::new(TextSize::new(idx), FIXUP_DUMMY_RANGE_END),
                            anchor: SpanAnchor { ast_id: FIXUP_DUMMY_AST_ID, ..span.anchor },
                            ctx: span.ctx,
                        },
                        is_raw: tt::IdentIsRaw::No,
                    });
                    // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                    append.insert($key!(node.clone()), smallvec![replacement]);
                    preorder.skip_subtree();
                    continue;
                }
                // In some other situations, we can fix things by just appending some tokens.
                match_ast! {
                    match node {
                        $ast::FieldExpr(it) => {
                            if it.name_ref().is_none() {
                                // incomplete field access: some_expr.|
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Ident(Ident {
                                        sym: sym::__ra_fixup,
                                        span: fake_span(node_range),
                                        is_raw: tt::IdentIsRaw::No
                                    }),
                                ]);
                            }
                        },
                        $ast::ExprStmt(it) => {
                            let needs_semi = it.semicolon_token().is_none() && it.expr().is_some_and(|e| e.syntax().kind() != SyntaxKind::BLOCK_EXPR);
                            if needs_semi {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Punct(Punct {
                                        char: ';',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range),
                                    }),
                                ]);
                            }
                        },
                        $ast::LetStmt(it) => {
                            if it.semicolon_token().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Punct(Punct {
                                        char: ';',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                ]);
                            }
                        },
                        $ast::IfExpr(it) => {
                            if it.condition().is_none() {
                                // insert placeholder token after the if token
                                let if_token = match it.if_token() {
                                    Some(t) => t,
                                    None => continue,
                                };
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(if_token), smallvec![
                                    Leaf::Ident(Ident {
                                        sym: sym::__ra_fixup,
                                        span: fake_span(node_range),
                                        is_raw: tt::IdentIsRaw::No
                                    }),
                                ]);
                            }
                            if it.then_branch().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Punct(Punct {
                                        char: '{',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                    Leaf::Punct(Punct {
                                        char: '}',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                ]);
                            }
                        },
                        $ast::WhileExpr(it) => {
                            if it.condition().is_none() {
                                // insert placeholder token after the while token
                                let while_token = match it.while_token() {
                                    Some(t) => t,
                                    None => continue,
                                };
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(while_token), smallvec![
                                    Leaf::Ident(Ident {
                                        sym: sym::__ra_fixup,
                                        span: fake_span(node_range),
                                        is_raw: tt::IdentIsRaw::No
                                    }),
                                ]);
                            }
                            if it.loop_body().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Punct(Punct {
                                        char: '{',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                    Leaf::Punct(Punct {
                                        char: '}',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                ]);
                            }
                        },
                        $ast::LoopExpr(it) => {
                            if it.loop_body().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Punct(Punct {
                                        char: '{',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                    Leaf::Punct(Punct {
                                        char: '}',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                ]);
                            }
                        },
                        $ast::MatchExpr(it) => {
                            if it.expr().is_none() {
                                let match_token = match it.match_token() {
                                    Some(t) => t,
                                    None => continue
                                };
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(match_token), smallvec![
                                    Leaf::Ident(Ident {
                                        sym: sym::__ra_fixup,
                                        span: fake_span(node_range),
                                        is_raw: tt::IdentIsRaw::No
                                    }),
                                ]);
                            }
                            if it.match_arm_list().is_none() {
                                // No match arms
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Punct(Punct {
                                        char: '{',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                    Leaf::Punct(Punct {
                                        char: '}',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                ]);
                            }
                        },
                        $ast::ForExpr(it) => {
                            let for_token = match it.for_token() {
                                Some(token) => token,
                                None => continue
                            };

                            let [pat, in_token, iter] = [
                                 sym::underscore,
                                 sym::in_,
                                 sym::__ra_fixup,
                            ].map(|sym|
                                Leaf::Ident(Ident {
                                    sym,
                                    span: fake_span(node_range),
                                    is_raw: tt::IdentIsRaw::No
                                }),
                            );

                            if it.pat().is_none() && it.in_token().is_none() && it.iterable().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(for_token), smallvec![pat, in_token, iter]);
                            // does something funky -- see test case for_no_pat
                            } else if it.pat().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(for_token), smallvec![pat]);
                            }

                            if it.loop_body().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node.clone()), smallvec![
                                    Leaf::Punct(Punct {
                                        char: '{',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                    Leaf::Punct(Punct {
                                        char: '}',
                                        spacing: Spacing::Alone,
                                        span: fake_span(node_range)
                                    }),
                                ]);
                            }
                        },
                        $ast::RecordExprField(it) => {
                            if let Some(colon) = it.colon_token()
                                && it.name_ref().is_some() && it.expr().is_none() {
                                    // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                    append.insert($key!(colon), smallvec![
                                        Leaf::Ident(Ident {
                                            sym: sym::__ra_fixup,
                                            span: fake_span(node_range),
                                            is_raw: tt::IdentIsRaw::No
                                        })
                                    ]);
                                }
                        },
                        $ast::Path(it) => {
                            if let Some(colon) = it.coloncolon_token()
                                && it.segment().is_none() {
                                    // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                    append.insert($key!(colon), smallvec![
                                        Leaf::Ident(Ident {
                                            sym: sym::__ra_fixup,
                                            span: fake_span(node_range),
                                            is_raw: tt::IdentIsRaw::No
                                        })
                                    ]);
                                }
                        },
                        $ast::ClosureExpr(it) => {
                            if it.body().is_none() {
                                // alloc: clone: error path: fixups run only where a syntax error touches the input (`has_syntax_error_in`)
                                append.insert($key!(node), smallvec![
                                    Leaf::Ident(Ident {
                                        sym: sym::__ra_fixup,
                                        span: fake_span(node_range),
                                        is_raw: tt::IdentIsRaw::No
                                    })
                                ]);
                            }
                        },
                        _ => (),
                    }
                }
            }
            let needs_fixups = !append.is_empty() || !original.is_empty();
            let original = needs_fixups.then(|| {
                let mut owned = ThinVec::with_capacity(original.len());
                owned.extend(original.drain(..));
                owned
            });
            SyntaxFixups { append, remove, undo_info: SyntaxFixupUndoInfo { original } }
        }
    };
}

macro_rules! view_key {
    ($element:expr) => {
        $element.id()
    };
}

fn view_tree(
    node: &view::Node<'_>,
    map: SpanMap<'_>,
    call_site: Span,
    mode: DocCommentDesugarMode,
) -> TopSubtree {
    syntax_bridge::call_input(*node, map, call_site, mode)
}

fixup_syntax_impl!(fixup_syntax, view::Node<'_>, view::Node<'_>, NodeId, view, view_key, view_tree);

pub(crate) fn reverse_fixups<A: core::alloc::Allocator + Clone>(
    tt: &mut TopSubtree<A>,
    undo_info: &SyntaxFixupUndoInfo,
) {
    let Some(undo_info) = &undo_info.original else { return };
    let undo_info = &**undo_info;
    let top_subtree = tt.top_subtree();
    let open_span = top_subtree.delimiter.open;
    let close_span = top_subtree.delimiter.close;
    if never!(
        close_span.anchor.ast_id == FIXUP_DUMMY_AST_ID
            || open_span.anchor.ast_id == FIXUP_DUMMY_AST_ID
    ) {
        let span = |file_id| Span {
            range: TextRange::empty(TextSize::new(0)),
            anchor: SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
            ctx: SyntaxContext::root(span::Edition::Edition2015),
        };
        tt.set_top_subtree_delimiter_span(tt::DelimSpan {
            open: span(open_span.anchor.file_id),
            close: span(close_span.anchor.file_id),
        });
    }
    reverse_fixups_(tt, undo_info);
}

fn reverse_fixups_<A: core::alloc::Allocator + Clone>(
    tt: &mut TopSubtree<A>,
    undo_info: &[TopSubtree],
) {
    transform_tt(tt, |tt| match tt {
        tt::TokenTree::Leaf(leaf) => {
            let span = leaf.span();
            let is_real_leaf = span.anchor.ast_id != FIXUP_DUMMY_AST_ID;
            let is_replaced_node = span.range.end() == FIXUP_DUMMY_RANGE_END;
            if !is_real_leaf && !is_replaced_node {
                return TransformTtAction::remove();
            }

            if !is_real_leaf {
                // we have a fake node here, we need to replace it again with the original
                let original = &undo_info[u32::from(leaf.span().range.start()) as usize];
                TransformTtAction::ReplaceWith(original.view().strip_invisible())
            } else {
                // just a normal leaf
                TransformTtAction::Keep
            }
        }
        tt::TokenTree::Subtree(tt) => {
            // fixup should only create matching delimiters, but proc macros
            // could just copy the span to one of the delimiters. We don't want
            // to leak the dummy ID, so we remove both.
            if tt.delimiter.close.anchor.ast_id == FIXUP_DUMMY_AST_ID
                || tt.delimiter.open.anchor.ast_id == FIXUP_DUMMY_AST_ID
            {
                return TransformTtAction::remove();
            }
            TransformTtAction::Keep
        }
    });
}

#[cfg(test)]
mod tests {
    use expect_test::{Expect, expect};
    use span::{Edition, EditionedFileId, FileId};
    use syntax::TextRange;
    use syntax_bridge::DocCommentDesugarMode;

    use crate::{
        fixup::reverse_fixups,
        span_map::{RealSpanMap, SpanMap},
        tt,
    };

    // The following three functions are only meant to check partial structural equivalence of
    // `TokenTree`s, see the last assertion in `check()`.
    fn check_leaf_eq(a: &tt::Leaf, b: &tt::Leaf) -> bool {
        match (a, b) {
            (tt::Leaf::Literal(a), tt::Leaf::Literal(b)) => a.text_and_suffix == b.text_and_suffix,
            (tt::Leaf::Punct(a), tt::Leaf::Punct(b)) => a.char == b.char,
            (tt::Leaf::Ident(a), tt::Leaf::Ident(b)) => a.sym == b.sym,
            _ => false,
        }
    }

    fn check_subtree_eq(a: &tt::TopSubtree, b: &tt::TopSubtree) -> bool {
        let a = a.view().as_token_trees();
        let b = b.view().as_token_trees();
        a.len() == b.len()
            && std::iter::zip(a.iter_flat_tokens(), b.iter_flat_tokens())
                .all(|(a, b)| check_tt_eq(&a, &b))
    }

    fn check_tt_eq(a: &tt::TokenTree, b: &tt::TokenTree) -> bool {
        match (a, b) {
            (tt::TokenTree::Leaf(a), tt::TokenTree::Leaf(b)) => check_leaf_eq(a, b),
            (tt::TokenTree::Subtree(a), tt::TokenTree::Subtree(b)) => {
                a.delimiter.kind == b.delimiter.kind
            }
            _ => false,
        }
    }

    #[track_caller]
    fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, mut expect: Expect) {
        let parsed = syntax::SourceFile::parse(ra_fixture, span::Edition::CURRENT);
        let span_map = SpanMap::RealSpanMap(&RealSpanMap::absolute(EditionedFileId::new(
            FileId::from_raw(0),
            Edition::CURRENT,
        )));
        let call_site = span_map.span_for_range(TextRange::empty(0.into()));
        let frame = &blaze_mem::Frame::new(blaze_mem::Tag::Expand).expect("a test's frame");
        let fixups = super::fixup_syntax(
            frame,
            span_map,
            parsed.root(),
            call_site,
            DocCommentDesugarMode::Mbe,
        );
        let undo_info = fixups.undo_info;
        let mut tt = syntax_bridge::call_input_modified(
            parsed.root(),
            span_map,
            fixups.append,
            fixups.remove,
            call_site,
            DocCommentDesugarMode::Mbe,
            |_, _| (true, syntax_bridge::Inserted::new()),
        );

        let actual = format!("{tt}\n");

        expect.indent(false);
        expect.assert_eq(&actual);

        // the fixed-up tree should be syntactically valid
        let (parse, _) = syntax_bridge::token_tree_to_syntax_node(
            tt.view(),
            syntax_bridge::TopEntryPoint::MacroItems,
            &mut |_| parser::Edition::CURRENT,
        );
        assert!(
            parse.errors().is_empty(),
            "parse has syntax errors. parse tree:\n{:#?}",
            parse.root()
        );

        // the fixed-up tree should not contain braces as punct
        // FIXME: should probably instead check that it's a valid punctuation character
        for x in tt.token_trees().iter_flat_tokens() {
            match x {
                ::tt::TokenTree::Leaf(::tt::Leaf::Punct(punct)) => {
                    assert!(!matches!(punct.char, '{' | '}' | '(' | ')' | '[' | ']'))
                }
                _ => (),
            }
        }

        reverse_fixups(&mut tt, &undo_info);

        // the fixed-up + reversed version should be equivalent to the original input
        // modulo token IDs and `Punct`s' spacing.
        let original_as_tt = syntax_bridge::call_input(
            parsed.root(),
            span_map,
            span_map.span_for_range(TextRange::empty(0.into())),
            DocCommentDesugarMode::Mbe,
        );
        assert!(
            check_subtree_eq(&tt, &original_as_tt),
            "different token tree:\n{tt:?}\n\n{original_as_tt:?}"
        );
    }

    /// Code without syntax errors is still changed by `fixup_syntax` (a statement `if` gets a
    /// `;`), so a macro's input skips it unless an error touches the item
    /// (`HirFileId::has_syntax_error_in`): the macro sees the tokens rustc gives it.
    #[test]
    fn valid_code_is_changed_by_fixups() {
        let parsed = syntax::SourceFile::parse("fn foo() { if a {} b(); }", Edition::CURRENT);
        assert!(parsed.errors().is_empty());
        let span_map = SpanMap::RealSpanMap(&RealSpanMap::absolute(EditionedFileId::new(
            FileId::from_raw(0),
            Edition::CURRENT,
        )));
        let call_site = span_map.span_for_range(TextRange::empty(0.into()));
        let frame = &blaze_mem::Frame::new(blaze_mem::Tag::Expand).expect("a test's frame");
        let fixups = super::fixup_syntax(
            frame,
            span_map,
            parsed.root(),
            call_site,
            DocCommentDesugarMode::ProcMacro,
        );
        assert!(!fixups.append.is_empty());
    }

    #[test]
    fn just_for_token() {
        check(
            r#"
fn foo() {
    for
}
"#,
            expect![[r#"
fn foo () {for _ in __ra_fixup {}}
"#]],
        )
    }

    #[test]
    fn for_no_iter_pattern() {
        check(
            r#"
fn foo() {
    for {}
}
"#,
            expect![[r#"
fn foo () {for _ in __ra_fixup {}}
"#]],
        )
    }

    #[test]
    fn for_no_body() {
        check(
            r#"
fn foo() {
    for bar in qux
}
"#,
            expect![[r#"
fn foo () {for bar in qux {}}
"#]],
        )
    }

    // FIXME: https://github.com/rust-lang/rust-analyzer/pull/12937#discussion_r937633695
    #[test]
    fn for_no_pat() {
        check(
            r#"
fn foo() {
    for in qux {

    }
}
"#,
            expect![[r#"
fn foo () {__ra_fixup}
"#]],
        )
    }

    #[test]
    fn match_no_expr_no_arms() {
        check(
            r#"
fn foo() {
    match
}
"#,
            expect![[r#"
fn foo () {match __ra_fixup {}}
"#]],
        )
    }

    #[test]
    fn match_expr_no_arms() {
        check(
            r#"
fn foo() {
    match it {

    }
}
"#,
            expect![[r#"
fn foo () {match it {}}
"#]],
        )
    }

    #[test]
    fn match_no_expr() {
        check(
            r#"
fn foo() {
    match {
        _ => {}
    }
}
"#,
            expect![[r#"
fn foo () {match __ra_fixup {}}
"#]],
        )
    }

    #[test]
    fn incomplete_field_expr_1() {
        check(
            r#"
fn foo() {
    a.
}
"#,
            expect![[r#"
fn foo () {a . __ra_fixup}
"#]],
        )
    }

    #[test]
    fn incomplete_field_expr_2() {
        check(
            r#"
fn foo() {
    a.;
}
"#,
            expect![[r#"
fn foo () {a .__ra_fixup ;}
"#]],
        )
    }

    #[test]
    fn incomplete_field_expr_3() {
        check(
            r#"
fn foo() {
    a.;
    bar();
}
"#,
            expect![[r#"
fn foo () {a .__ra_fixup ; bar () ;}
"#]],
        )
    }

    #[test]
    fn incomplete_let() {
        check(
            r#"
fn foo() {
    let it = a
}
"#,
            expect![[r#"
fn foo () {let it = a ;}
"#]],
        )
    }

    #[test]
    fn incomplete_field_expr_in_let() {
        check(
            r#"
fn foo() {
    let it = a.
}
"#,
            expect![[r#"
fn foo () {let it = a . __ra_fixup ;}
"#]],
        )
    }

    #[test]
    fn field_expr_before_call() {
        // another case that easily happens while typing
        check(
            r#"
fn foo() {
    a.b
    bar();
}
"#,
            expect![[r#"
fn foo () {a . b ; bar () ;}
"#]],
        )
    }

    #[test]
    fn extraneous_comma() {
        check(
            r#"
fn foo() {
    bar(,);
}
"#,
            expect![[r#"
fn foo () {__ra_fixup ;}
"#]],
        )
    }

    #[test]
    fn fixup_if_1() {
        check(
            r#"
fn foo() {
    if a
}
"#,
            expect![[r#"
fn foo () {if a {}}
"#]],
        )
    }

    #[test]
    fn fixup_if_2() {
        check(
            r#"
fn foo() {
    if
}
"#,
            expect![[r#"
fn foo () {if __ra_fixup {}}
"#]],
        )
    }

    #[test]
    fn fixup_if_3() {
        check(
            r#"
fn foo() {
    if {}
}
"#,
            expect![[r#"
fn foo () {if __ra_fixup {} {}}
"#]],
        )
    }

    #[test]
    fn fixup_while_1() {
        check(
            r#"
fn foo() {
    while
}
"#,
            expect![[r#"
fn foo () {while __ra_fixup {}}
"#]],
        )
    }

    #[test]
    fn fixup_while_2() {
        check(
            r#"
fn foo() {
    while foo
}
"#,
            expect![[r#"
fn foo () {while foo {}}
"#]],
        )
    }
    #[test]
    fn fixup_while_3() {
        check(
            r#"
fn foo() {
    while {}
}
"#,
            expect![[r#"
fn foo () {while __ra_fixup {}}
"#]],
        )
    }

    #[test]
    fn fixup_loop() {
        check(
            r#"
fn foo() {
    loop
}
"#,
            expect![[r#"
fn foo () {loop {}}
"#]],
        )
    }

    #[test]
    fn fixup_path() {
        check(
            r#"
fn foo() {
    path::
}
"#,
            expect![[r#"
fn foo () {path :: __ra_fixup}
"#]],
        )
    }

    #[test]
    fn fixup_record_ctor_field() {
        check(
            r#"
fn foo() {
    R { f: }
}
"#,
            expect![[r#"
fn foo () {R {f : __ra_fixup}}
"#]],
        )
    }

    #[test]
    fn no_fixup_record_ctor_field() {
        check(
            r#"
fn foo() {
    R { f: a }
}
"#,
            expect![[r#"
fn foo () {R {f : a}}
"#]],
        )
    }

    #[test]
    fn fixup_arg_list() {
        check(
            r#"
fn foo() {
    foo(a
}
"#,
            expect![[r#"
fn foo () {foo (a)}
"#]],
        );
        check(
            r#"
fn foo() {
    bar.foo(a
}
"#,
            expect![[r#"
fn foo () {bar . foo (a)}
"#]],
        );
    }

    #[test]
    fn fixup_closure() {
        check(
            r#"
fn foo() {
    ||
}
"#,
            expect![[r#"
fn foo () {|| __ra_fixup}
"#]],
        );
    }

    #[test]
    fn fixup_regression_() {
        check(
            r#"
fn foo() {
    {}
    {}
}
"#,
            expect![[r#"
fn foo () {{} {}}
"#]],
        );
    }
}
