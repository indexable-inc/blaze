//! Processes out `#[cfg]` and `#[cfg_attr]` attributes from the input for the derive macro
use std::{cell::OnceCell, ops::ControlFlow};

use ::tt::TextRange;
use base_db::{Db, Crate};
use blaze_mem::Frame;
use cfg::CfgExpr;
use parser::T;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, WalkEvent,
    view::{self, AstNode as _, HasAttrs as _},
};
use syntax_bridge::{DocCommentDesugarMode, Inserted};

use crate::{
    attrs::{AttrId, expand_cfg_attr_view, is_item_tree_filtered_attr, with_one_segment_view},
    fixup::{self, SyntaxFixupUndoInfo},
    span_map::{InOrder, SpanMap},
    tt::{self, DelimSpan, Span},
};

struct ItemIsCfgedOut;

/// The callback's answers that insert no tokens, keeping or skipping the token.
const KEEP: (bool, Inserted) = (true, Inserted::new_const());
const SKIP: (bool, Inserted) = (false, Inserted::new_const());

/// An attribute `cfg_attr` leaves: where its text is.
#[derive(Debug)]
struct ExpandedAttrToProcess {
    range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextExpandedAttrState {
    NotStarted,
    InTheMiddle,
}

#[derive(Debug)]
struct AstAttrToProcess {
    range: TextRange,
    expanded_attrs: SmallVec<[ExpandedAttrToProcess; 1]>,
    expanded_attrs_idx: usize,
    next_expanded_attr: NextExpandedAttrState,
    pound_span: Span,
    brackets_span: DelimSpan,
    /// If `Some`, this is an inner attribute.
    excl_span: Option<Span>,
}

/// The callback that strips `cfg`s and censored attributes from an attribute or derive macro's
/// input: [`macro_input_callback`], over views.
macro_rules! macro_input_callback_impl {
    (
        $name:ident, $Preorder:ty, $Element:ty, $Token:ty, $ast:ident,
        $expand_cfg_attr:path, $parse_cfg:path, $with_one_segment:path
    ) => {
        #[allow(clippy::clone_on_copy, reason = "the body was written for handles too; a view is `Copy`")]
        fn $name(
            frame: &Frame,
            db: &Db,
            is_derive: bool,
            censor_item_tree_attr_ids: &[AttrId],
            krate: Crate,
            default_span: Span,
            span_map: SpanMap<'_>,
        ) -> impl FnMut(&mut $Preorder, &WalkEvent<$Element>) -> (bool, Inserted) {
            let cfg_options = OnceCell::new();
            let cfg_options = move || *cfg_options.get_or_init(|| krate.cfg_options(db));

            let mut should_strip_attr = {
                let mut item_tree_attr_id = 0;
                let mut censor_item_tree_attr_ids_index = 0;
                move || {
                    let mut result = false;
                    if let Some(&next_censor_attr_id) =
                        censor_item_tree_attr_ids.get(censor_item_tree_attr_ids_index)
                        && next_censor_attr_id.item_tree_index() == item_tree_attr_id
                    {
                        censor_item_tree_attr_ids_index += 1;
                        result = true;
                    }
                    item_tree_attr_id += 1;
                    result
                }
            };

            let mut attrs: Vec<AstAttrToProcess, &Frame> = Vec::new_in(frame);
            let mut attrs_idx = 0;
            let mut has_inner_attrs_owner = false;
            let mut in_attr = false;
            let mut done_with_attrs = false;
            let mut did_top_attrs = false;
            move |preorder, event| {
                match event {
                    WalkEvent::Enter(NodeOrToken::Node(node)) => {
                        if done_with_attrs {
                            return KEEP;
                        }

                        if view::Attr::can_cast(node.kind()) {
                            in_attr = true;
                            let node_range = node.text_range();
                            while attrs
                                .get(attrs_idx)
                                .is_some_and(|it: &AstAttrToProcess| it.range != node_range)
                            {
                                attrs_idx += 1;
                            }
                        } else if !in_attr && let Some(has_attrs) = $ast::AnyHasAttrs::cast(node.clone()) { // clone: a view's copy
                            // Attributes of the form `key = value` have `ast::Expr` in them, which returns `Some` for
                            // `AnyHasAttrs::cast()`, so we also need to check `in_attr`.

                            if has_inner_attrs_owner {
                                has_inner_attrs_owner = false;
                                return KEEP;
                            }

                            if did_top_attrs && !is_derive {
                                // Derives need all attributes handled, but attribute macros need only the top attributes handled.
                                done_with_attrs = true;
                                return KEEP;
                            }
                            did_top_attrs = true;

                            if let Some(inner_attrs_node) = has_attrs.inner_attributes_node()
                                && inner_attrs_node != *node
                            {
                                has_inner_attrs_owner = true;
                            }

                            let node_attrs = $ast::attrs_including_inner(&has_attrs);

                            attrs.clear();
                            // clone: an iterator of node handles
                            node_attrs.clone().for_each(|attr| {
                                let span_for = |token: Option<$Token>| {
                                    token
                                        .map(|token| span_map.span_for_range(token.text_range()))
                                        .unwrap_or(default_span)
                                };
                                attrs.push(AstAttrToProcess {
                                    range: attr.syntax().text_range(),
                                    pound_span: span_for(attr.pound_token()),
                                    brackets_span: DelimSpan {
                                        open: span_for(attr.l_brack_token()),
                                        close: span_for(attr.r_brack_token()),
                                    },
                                    excl_span: attr
                                        .excl_token()
                                        .map(|token| span_map.span_for_range(token.text_range())),
                                    expanded_attrs: SmallVec::new(),
                                    expanded_attrs_idx: 0,
                                    next_expanded_attr: NextExpandedAttrState::NotStarted,
                                });
                            });

                            attrs_idx = 0;
                            let strip_current_item =
                                $expand_cfg_attr(node_attrs, &cfg_options, |attr, top_attr| {
                                    // Find the attr.
                                    while attrs[attrs_idx].range != top_attr.syntax().text_range() {
                                        attrs_idx += 1;
                                    }

                                    let mut strip_current_attr = false;
                                    match &attr {
                                        $ast::Meta::CfgMeta(attr) => {
                                            if let Some(cfg_predicate) = attr.cfg_predicate() {
                                                let cfg_expr = $parse_cfg(cfg_predicate);
                                                if cfg_options().check(&cfg_expr) == Some(false) {
                                                    return ControlFlow::Break(ItemIsCfgedOut);
                                                }
                                                strip_current_attr = true;
                                            }
                                        }
                                        _ => {
                                            if attr.path().and_then(|path| {
                                                $with_one_segment(&path, is_item_tree_filtered_attr)
                                            }) != Some(true)
                                            {
                                                strip_current_attr = should_strip_attr();
                                            }
                                        }
                                    }

                                    if !strip_current_attr {
                                        attrs[attrs_idx]
                                            .expanded_attrs
                                            .push(ExpandedAttrToProcess { range: attr.syntax().text_range() });
                                    }

                                    ControlFlow::Continue(())
                                });
                            attrs_idx = 0;

                            if strip_current_item.is_some() {
                                preorder.skip_subtree();
                                attrs.clear();

                                'eat_comma: {
                                    // If there is a comma after this node, eat it too.
                                    let mut events_until_comma = 0;
                                    // clone: a cursor, for a cfg'd-out item only
                                    for event in preorder.clone() {
                                        match event {
                                            WalkEvent::Enter(NodeOrToken::Node(_))
                                            | WalkEvent::Leave(_) => {}
                                            WalkEvent::Enter(NodeOrToken::Token(token)) => {
                                                let kind = token.kind();
                                                if kind == T![,] {
                                                    break;
                                                } else if !kind.is_trivia() {
                                                    break 'eat_comma;
                                                }
                                            }
                                        }
                                        events_until_comma += 1;
                                    }
                                    preorder.nth(events_until_comma);
                                }

                                return SKIP;
                            }
                        }
                    }
                    WalkEvent::Leave(NodeOrToken::Node(node)) => {
                        if view::Attr::can_cast(node.kind()) {
                            in_attr = false;
                            attrs_idx += 1;
                        }
                    }
                    WalkEvent::Enter(NodeOrToken::Token(token)) => {
                        if !in_attr {
                            return KEEP;
                        }

                        let Some(ast_attr) = attrs.get_mut(attrs_idx) else {
                            return KEEP;
                        };
                        let token_range = token.text_range();
                        let Some(expanded_attr) = ast_attr.expanded_attrs.get(ast_attr.expanded_attrs_idx)
                        else {
                            // No expanded attributes in this `ast::Attr`, or we finished them all already, either way
                            // the remaining tokens should be discarded.
                            return SKIP;
                        };
                        match ast_attr.next_expanded_attr {
                            NextExpandedAttrState::NotStarted => {
                                if token_range.start() >= expanded_attr.range.start() {
                                    // We started the next attribute.
                                    let mut insert_tokens = Inserted::new();
                                    insert_tokens.push(tt::Leaf::Punct(tt::Punct {
                                        char: '#',
                                        spacing: tt::Spacing::Alone,
                                        span: ast_attr.pound_span,
                                    }));
                                    if let Some(span) = ast_attr.excl_span {
                                        insert_tokens.push(tt::Leaf::Punct(tt::Punct {
                                            char: '!',
                                            spacing: tt::Spacing::Alone,
                                            span,
                                        }));
                                    }
                                    insert_tokens.push(tt::Leaf::Punct(tt::Punct {
                                        char: '[',
                                        spacing: tt::Spacing::Alone,
                                        span: ast_attr.brackets_span.open,
                                    }));

                                    ast_attr.next_expanded_attr = NextExpandedAttrState::InTheMiddle;

                                    return (true, insert_tokens);
                                } else {
                                    // Before any attribute or between the attributes.
                                    return SKIP;
                                }
                            }
                            NextExpandedAttrState::InTheMiddle => {
                                if token_range.start() >= expanded_attr.range.end() {
                                    // Finished the current attribute.
                                    let mut insert_tokens = Inserted::new();
                                    insert_tokens.push(tt::Leaf::Punct(tt::Punct {
                                        char: ']',
                                        spacing: tt::Spacing::Alone,
                                        span: ast_attr.brackets_span.close,
                                    }));

                                    ast_attr.next_expanded_attr = NextExpandedAttrState::NotStarted;
                                    ast_attr.expanded_attrs_idx += 1;

                                    // It's safe to ignore the current token because between attributes
                                    // there is always at least one token we skip - either the closing bracket
                                    // in `#[]` or the comma in case of multiple attrs in `cfg_attr` expansion.
                                    return (false, insert_tokens);
                                } else {
                                    // Still in the middle.
                                    return KEEP;
                                }
                            }
                        }
                    }
                    WalkEvent::Leave(NodeOrToken::Token(_)) => {}
                }
                KEEP
            }
        }
    };
}

macro_input_callback_impl!(
    macro_input_callback,
    view::PreorderWithTokens<'_>,
    view::Element<'_>,
    view::Token<'_>,
    view,
    expand_cfg_attr_view,
    CfgExpr::parse_from_view,
    with_one_segment_view
);

/// An attribute or derive macro's input, read from a view of the item: `cfg`s and the attributes
/// `censor_item_tree_attr_ids` names stripped, and syntax errors fixed up when `fix_errors`.
pub(crate) fn attr_macro_input_to_token_tree(
    db: &Db,
    node: view::Node<'_>,
    span_map: SpanMap<'_>,
    span: Span,
    is_derive: bool,
    censor_item_tree_attr_ids: &[AttrId],
    krate: Crate,
    fix_errors: bool,
) -> (tt::TopSubtree, SyntaxFixupUndoInfo) {
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let fixups = if fix_errors {
            fixup::fixup_syntax(frame, span_map, node, span, DocCommentDesugarMode::ProcMacro)
        } else {
            fixup::SyntaxFixups::new_in(frame)
        };
        let callback = macro_input_callback(
            frame,
            db,
            is_derive,
            censor_item_tree_attr_ids,
            krate,
            span,
            span_map,
        );
        let tt = syntax_bridge::call_input_modified(
            node,
            InOrder::new(span_map),
            fixups.append,
            fixups.remove,
            span,
            DocCommentDesugarMode::ProcMacro,
            callback,
        );
        (tt, fixups.undo_info)
    })
}
