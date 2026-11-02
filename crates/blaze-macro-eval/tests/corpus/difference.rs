//! The first difference of one proc-macro output from another, token by token, for a test that
//! holds an expansion found again to the one first made.

use span::Span;
use tt::{Leaf, TokenTree, TopSubtree};

/// The first difference of `first` from `second`, or `None` when they are the same: every
/// token tree, each delimiter, character, spacing, text, literal kind and suffix, and of every
/// span its range, anchor and syntax context.
pub(crate) fn difference(first: &TopSubtree, second: &TopSubtree) -> Option<String> {
    let mut firsts = first.as_token_trees().iter_flat_tokens();
    let mut seconds = second.as_token_trees().iter_flat_tokens();
    let mut at = 0usize;
    loop {
        match (firsts.next(), seconds.next()) {
            (None, None) => return None,
            (Some(ours), Some(theirs)) => {
                if let Some(what) = tree_difference(&ours, &theirs) {
                    // alloc: the message of a difference, made only when there is one.
                    return Some(format!("token tree {at}, {}: {what}", shown(&theirs)));
                }
            }
            (None, Some(theirs)) => {
                // alloc: the message of a difference, made only when there is one.
                return Some(format!(
                    "the first output ends after {at} token trees, where the second goes on with {}",
                    shown(&theirs),
                ));
            }
            (Some(ours), None) => {
                // alloc: the message of a difference, made only when there is one.
                return Some(format!(
                    "the first output goes on after the second's {at} token trees with {}",
                    shown(&ours),
                ));
            }
        }
        at += 1;
    }
}

fn tree_difference(ours: &TokenTree, theirs: &TokenTree) -> Option<String> {
    match (ours, theirs) {
        (TokenTree::Subtree(ours), TokenTree::Subtree(theirs)) => {
            let (a, b) = (ours.delimiter, theirs.delimiter);
            if a.kind != b.kind {
                // alloc: the message of a difference, made only when there is one.
                return Some(format!(
                    "its delimiter is {:?} first and {:?} second",
                    a.kind, b.kind
                ));
            }
            span_difference("its opening delimiter's", a.open, b.open)
                .or_else(|| span_difference("its closing delimiter's", a.close, b.close))
                .or_else(|| {
                    (ours.len != theirs.len).then(|| {
                        // alloc: the message of a difference, made only when there is one.
                        format!(
                            "it holds {} token trees first and {} second",
                            ours.len, theirs.len
                        )
                    })
                })
        }
        (TokenTree::Leaf(ours), TokenTree::Leaf(theirs)) => leaf_difference(ours, theirs),
        // alloc: the message of a difference, made only when there is one.
        _ => Some(format!(
            "it is {} first and {} second",
            tree_kind(ours),
            tree_kind(theirs)
        )),
    }
}

fn differs(what: &str, a: impl std::fmt::Debug, b: impl std::fmt::Debug) -> Option<String> {
    // alloc: the message of a difference, made only when there is one.
    Some(format!("its {what} is {a:?} first and {b:?} second"))
}

fn leaf_difference(ours: &Leaf, theirs: &Leaf) -> Option<String> {
    match (ours, theirs) {
        (Leaf::Ident(a), Leaf::Ident(b)) => {
            if a.sym != b.sym {
                return differs("text", &a.sym.as_str(), &b.sym.as_str());
            }
            if a.is_raw != b.is_raw {
                return differs("rawness", &a.is_raw, &b.is_raw);
            }
            span_difference("its", a.span, b.span)
        }
        (Leaf::Punct(a), Leaf::Punct(b)) => {
            if a.char != b.char {
                return differs("character", &a.char, &b.char);
            }
            if a.spacing != b.spacing {
                return differs("spacing", &a.spacing, &b.spacing);
            }
            span_difference("its", a.span, b.span)
        }
        (Leaf::Literal(a), Leaf::Literal(b)) => {
            if a.kind != b.kind {
                return differs("literal kind", &a.kind, &b.kind);
            }
            if a.text() != b.text() {
                return differs("text", &a.text(), &b.text());
            }
            if a.suffix() != b.suffix() {
                return differs("suffix", &a.suffix(), &b.suffix());
            }
            span_difference("its", a.span, b.span)
        }
        // alloc: the message of a difference, made only when there is one.
        _ => Some(format!(
            "it is {} first and {} second",
            leaf_kind(ours),
            leaf_kind(theirs)
        )),
    }
}

fn span_difference(whose: &str, ours: Span, theirs: Span) -> Option<String> {
    if ours.range != theirs.range {
        // alloc: the message of a difference, made only when there is one.
        return Some(format!(
            "{whose} range is {:?} first and {:?} second",
            ours.range, theirs.range
        ));
    }
    if ours.anchor != theirs.anchor {
        // alloc: the message of a difference, made only when there is one.
        return Some(format!(
            "{whose} anchor is {:?} first and {:?} second",
            ours.anchor, theirs.anchor
        ));
    }
    if ours.ctx != theirs.ctx {
        // alloc: the message of a difference, made only when there is one.
        return Some(format!(
            "{whose} syntax context is {:?} first and {:?} second",
            ours.ctx, theirs.ctx
        ));
    }
    None
}

fn tree_kind(tree: &TokenTree) -> &'static str {
    match tree {
        TokenTree::Subtree(_) => "a group",
        TokenTree::Leaf(leaf) => leaf_kind(leaf),
    }
}

fn leaf_kind(leaf: &Leaf) -> &'static str {
    match leaf {
        Leaf::Ident(_) => "an identifier",
        Leaf::Punct(_) => "a punctuation mark",
        Leaf::Literal(_) => "a literal",
    }
}

/// A token tree as a reader finds it in an error.
fn shown(tree: &TokenTree) -> String {
    match tree {
        TokenTree::Subtree(subtree) => match subtree.delimiter.kind {
            // alloc: the message of a difference, made only when there is one.
            tt::DelimiterKind::Parenthesis => "the group `(`".to_owned(),
            // alloc: the message of a difference, made only when there is one.
            tt::DelimiterKind::Brace => "the group `{`".to_owned(),
            // alloc: the message of a difference, made only when there is one.
            tt::DelimiterKind::Bracket => "the group `[`".to_owned(),
            // alloc: the message of a difference, made only when there is one.
            tt::DelimiterKind::Invisible => "a group without delimiters".to_owned(),
        },
        // alloc: the message of a difference, made only when there is one.
        TokenTree::Leaf(Leaf::Ident(it)) => format!("`{}{}`", it.is_raw.as_str(), it.sym.as_str()),
        // alloc: the message of a difference, made only when there is one.
        TokenTree::Leaf(Leaf::Punct(it)) => format!("`{}`", it.char),
        // alloc: the message of a difference, made only when there is one.
        TokenTree::Leaf(Leaf::Literal(it)) => format!("the literal `{}{}`", it.text(), it.suffix()),
    }
}
