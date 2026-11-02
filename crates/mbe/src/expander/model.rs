//! The expander as it was before matching ran over a flattened pattern with its bindings in one
//! arena: a hashed map of bindings per match, a linked builder of `Rc` nodes, a boxed parent
//! per repetition and the whole rest of a level converted for each fragment. Kept as the model
//! `crate::differential` compares the expander with.

mod matcher;
mod transcriber;

use base_db::Db;
use blaze_hash::HashMap;
use span::Span;
use syntax_bridge::to_matcher_input;
use tt::{DelimSpan, iter::TtIter};

use crate::{
    ExpandError, ExpandErrorKind, ExpandResult, MacroCallStyle, MatchedArmIndex,
    expander::Fragment,
    parser::{MetaVar, MetaVarKind},
};

pub(crate) fn expand_rules(
    db: &Db,
    rules: &[crate::Rule],
    input: &tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
    let mut match_: Option<(matcher::Match<'_>, &crate::Rule, usize)> = None;
    for (idx, rule) in rules.iter().enumerate() {
        if call_style != rule.style {
            continue;
        }

        let new_match = matcher::match_(db, &rule.lhs, input);

        if new_match.err.is_none() {
            let ExpandResult { value, err: transcribe_err } =
                transcriber::transcribe(&rule.rhs, &new_match.bindings, marker, call_site);
            if transcribe_err.is_none() {
                return ExpandResult::ok((value, Some(idx as u32)));
            }
        }
        if let Some((prev_match, _, _)) = &match_ {
            if (new_match.unmatched_tts, -(new_match.bound_count as i32))
                < (prev_match.unmatched_tts, -(prev_match.bound_count as i32))
            {
                match_ = Some((new_match, rule, idx));
            }
        } else {
            match_ = Some((new_match, rule, idx));
        }
    }
    if let Some((match_, rule, idx)) = match_ {
        let ExpandResult { value, err: transcribe_err } =
            transcriber::transcribe(&rule.rhs, &match_.bindings, marker, call_site);
        // The account of a failure is the expander's, made apart from the match it compares;
        // a fragment that failed inside is reported where its parse failed, as there.
        let in_fragment = match_.err_in_fragment;
        let match_err = match_.err.map(|err| match in_fragment {
            true => err,
            false => blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
                super::matcher::match_failure(db, frame, rules, input, call_style)
            })
            .unwrap_or(err),
        });
        ExpandResult { value: (value, idx.try_into().ok()), err: match_err.or(transcribe_err) }
    } else {
        ExpandResult::new(
            (tt::TopSubtree::empty(tt::DelimSpan::from_single(call_site)), None),
            ExpandError::new(call_site, ExpandErrorKind::NoMatchingRule),
        )
    }
}

/// A multi-mapping from metavariables to the fragments they matched, one level of `Nested` per
/// repetition the metavariable is in.
#[derive(Debug, Default, Clone)] // clone: the `#[cfg(test)]` model of the old matcher, which copies bindings per fork
struct Bindings<'a> {
    inner: HashMap<MetaVar, Binding<'a>>,
}

#[derive(Debug, Clone)] // clone: as `Bindings`, test-only
enum Binding<'a> {
    Fragment(Fragment<'a>),
    Nested(Vec<Binding<'a>>),
    Empty,
    Missing(MetaVarKind),
}

/// [`crate::expect_fragment`] as it was: the parser's input made from every token left in the
/// level, whatever the fragment's length.
pub(crate) fn expect_fragment<'t>(
    db: &Db,
    tt_iter: &mut TtIter<'t>,
    entry_point: ::parser::PrefixEntryPoint,
    delim_span: DelimSpan,
) -> ExpandResult<tt::TokenTreesView<'t>> {
    use ::parser;
    let buffer = tt_iter.remaining();
    let parser_input = to_matcher_input(buffer, &mut |ctx| ctx.edition(db));
    let tree_traversal = entry_point.parse(&parser_input);
    let mut cursor = buffer.cursor();
    let mut error = None;
    for step in tree_traversal.iter() {
        match step {
            parser::Step::Token { kind, mut n_input_tokens } => {
                if kind == ::parser::SyntaxKind::LIFETIME_IDENT {
                    n_input_tokens = 2;
                }
                for _ in 0..n_input_tokens {
                    cursor.skip_invisible();
                    cursor.bump_or_end();
                }
            }
            parser::Step::FloatSplit { .. } => {
                cursor.skip_invisible();
                cursor.bump_or_end();
            }
            parser::Step::Enter { .. } | parser::Step::Exit => (),
            parser::Step::Error { msg } => {
                error = error.or_else(|| Some(crate::fragment_error(&cursor, msg, delim_span)));
            }
        }
    }

    cursor.leave_invisible();
    let err = match error {
        Some(err) => Some(err),
        None if !cursor.is_root() => Some(ExpandError::binding_error(
            buffer.cursor().token_tree().map_or(delim_span.close, |tt| tt.first_span()),
            format!("expected {entry_point:?}"),
        )),
        None => None,
    };

    while !cursor.is_root() {
        cursor.bump_or_end();
    }

    let res = cursor.crossed();
    tt_iter.flat_advance_to(&cursor);

    ExpandResult { value: res, err }
}
