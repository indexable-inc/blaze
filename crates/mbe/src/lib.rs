//! `mbe` (short for Macro By Example) crate contains code for handling
//! `macro_rules` macros. It uses `TokenTree` (from `tt` package) as the
//! interface, although it contains some code to bridge syntax trees and
//! `TokenTree`s as well!
//!
//! The tests for this functionality live in another crate:
//! `hir_def::macro_expansion_tests::mbe`.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]

extern crate rustc_lexer;

mod expander;
mod macro_call_style;
mod parser;

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use base_db::Db;
use blaze_mem::frame::Frame;
use smallvec::SmallVec;
use span::{Edition, Span, SyntaxContext};
use tt::DelimSpan;
use tt::iter::TtIter;

use std::alloc::Allocator;
use std::fmt;
use std::sync::Arc;

pub use crate::macro_call_style::{MacroCallStyle, MacroCallStyles};
use crate::parser::{MetaTemplate, MetaVarKind, Op};

pub use tt::{Delimiter, DelimiterKind, Punct};

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    UnexpectedToken(Box<str>),
    Expected(Box<str>),
    InvalidRepeat,
    RepetitionEmptyTokenTree,
}

impl ParseError {
    fn expected(e: &str) -> ParseError {
        ParseError::Expected(e.into())
    }

    fn unexpected(e: &str) -> ParseError {
        ParseError::UnexpectedToken(e.into())
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedToken(it) => f.write_str(it),
            ParseError::Expected(it) => f.write_str(it),
            ParseError::InvalidRepeat => f.write_str("invalid repeat"),
            ParseError::RepetitionEmptyTokenTree => f.write_str("empty token tree in repetition"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct ExpandError {
    pub inner: Arc<(Span, ExpandErrorKind)>,
}
#[derive(Debug, PartialEq, Eq, Clone, Hash)] // clone: hir-expand's error kind holds it; one copy per error, on the error path
pub enum ExpandErrorKind {
    BindingError(Box<Box<str>>),
    UnresolvedBinding(Box<Box<str>>),
    LeftoverTokens,
    LimitExceeded,
    NoMatchingRule,
    UnexpectedToken,
    /// No rule matches the call: rustc's account of the rule that got furthest.
    MatchFailure(Box<MatchFailure>),
}

/// What rustc's `failed_to_match_macro` says of a call no rule matches
/// (`rustc_expand/src/mbe/diagnostics.rs`): the `BestFailure` its `CollectTrackerAndEmitter`
/// keeps, the rule that got furthest into the input, and its missing-comma check. The error's
/// span is the token the rule stopped at, or the closing delimiter of the input at its end
/// (where rustc's span is the end of the last token, or the call when there is none).
#[derive(Debug, PartialEq, Eq, Clone, Hash)] // clone: inside `ExpandErrorKind`, on the error path
pub struct MatchFailure {
    /// The token the rule stopped at, as rustc's `token_descr` gives it (`` `x` ``, "keyword
    /// `fn`"); `None` at the end of the input.
    pub token: Option<Box<str>>,
    /// What the rule was trying to match there, as rustc's `MatcherLoc` displays it: "`,`",
    /// "meta-variable `$a:ident`", "end of macro".
    pub remaining: Box<str>,
    /// The span of that part of the rule, when it has one (rustc's `MatcherLoc::span`): the
    /// span of rustc's "while trying to match" note, a bare note without it.
    pub remaining_span: Option<Span>,
    /// Where a `,` makes some rule match (rustc's `TokenStream::add_comma`, the help "missing
    /// comma here"): empty, at the end of the token it goes after.
    pub missing_comma: Option<Span>,
}

impl MatchFailure {
    /// rustc's label at the error's span.
    pub fn label(&self) -> &'static str {
        match self.token {
            Some(_) => "no rules expected this token in macro call",
            None => "missing tokens in macro arguments",
        }
    }
}

impl ExpandError {
    fn new(span: Span, kind: ExpandErrorKind) -> ExpandError {
        // alloc: an error.
        ExpandError { inner: Arc::new((span, kind)) }
    }
    fn binding_error(span: Span, e: impl Into<Box<str>>) -> ExpandError {
        // alloc: an error.
        ExpandError { inner: Arc::new((span, ExpandErrorKind::BindingError(Box::new(e.into())))) }
    }
}
impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.1.fmt(f)
    }
}

impl fmt::Display for ExpandErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpandErrorKind::NoMatchingRule => f.write_str("no rule matches input tokens"),
            ExpandErrorKind::UnexpectedToken => f.write_str("unexpected token in input"),
            ExpandErrorKind::BindingError(e) => f.write_str(e),
            ExpandErrorKind::UnresolvedBinding(binding) => {
                f.write_str("could not find binding ")?;
                f.write_str(binding)
            }
            ExpandErrorKind::LimitExceeded => f.write_str("Expand exceed limit"),
            ExpandErrorKind::LeftoverTokens => f.write_str("leftover tokens"),
            ExpandErrorKind::MatchFailure(failure) => match &failure.token {
                Some(token) => write!(f, "no rules expected {token}"),
                None => f.write_str("unexpected end of macro invocation"),
            },
        }
    }
}

// FIXME: Showing these errors could be nicer.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum CountError {
    OutOfBounds,
    Misplaced,
}

impl fmt::Display for CountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CountError::OutOfBounds => f.write_str("${count} out of bounds"),
            CountError::Misplaced => f.write_str("${count} misplaced"),
        }
    }
}

/// Index of the matched macro arm on successful expansion.
pub type MatchedArmIndex = Option<u32>;

/// This struct contains AST for a single `macro_rules` definition. What might
/// be very confusing is that AST has almost exactly the same shape as
/// `tt::TokenTree`, but there's a crucial difference: in macro rules, `$ident`
/// and `$()*` have special meaning (see `Var` and `Repeat` data structures)
#[derive(Debug, PartialEq, Eq)]
pub struct DeclarativeMacro {
    rules: Box<[Rule]>,
    err: Option<Box<ParseError>>,
}

#[derive(Debug, PartialEq, Eq)]
struct Rule {
    /// Is this a normal fn-like rule, an `attr()` rule, or a `derive()` rule?
    style: MacroCallStyle,
    lhs: MetaTemplate,
    rhs: MetaTemplate,
    /// `lhs` flattened for the matcher, once per definition.
    matcher: expander::Matcher,
}

impl DeclarativeMacro {
    pub fn from_err(err: ParseError) -> DeclarativeMacro {
        // alloc: an error.
        DeclarativeMacro { rules: Box::default(), err: Some(Box::new(err)) }
    }

    /// The old, `macro_rules! m {}` flavor.
    pub fn parse_macro_rules<A: Allocator>(
        tt: &tt::TopSubtree<A>,
        ctx_edition: impl Copy + Fn(SyntaxContext) -> Edition,
    ) -> DeclarativeMacro {
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            // Note: this parsing can be implemented using mbe machinery itself, by
            // matching against `$($lhs:tt => $rhs:tt);*` pattern, but implementing
            // manually seems easier.
            let mut src = tt.iter();
            let mut rules = Vec::new_in(frame);
            let mut err = None;

            while !src.is_empty() {
                let rule = match Rule::parse(ctx_edition, &mut src) {
                    Ok(it) => it,
                    Err(e) => {
                        // alloc: an error.
                        err = Some(Box::new(e));
                        break;
                    }
                };
                rules.push(rule);
                if let Err(()) = src.expect_char(';') {
                    if !src.is_empty() {
                        // alloc: an error.
                        err = Some(Box::new(ParseError::expected("expected `;`")));
                    }
                    break;
                }
            }

            for Rule { lhs, .. } in &rules {
                if let Err(e) = validate(lhs) {
                    // alloc: an error.
                    err = Some(Box::new(e));
                    break;
                }
            }

            // alloc: the definition's rules, its memo, once at their exact length.
            let rules: Box<[Rule]> = rules.drain(..).collect();
            DeclarativeMacro { rules, err }
        })
    }

    /// The new, unstable `macro m {}` flavor.
    pub fn parse_macro2<A: Allocator, B: Allocator>(
        args: Option<&tt::TopSubtree<A>>,
        body: &tt::TopSubtree<B>,
        ctx_edition: impl Copy + Fn(SyntaxContext) -> Edition,
    ) -> DeclarativeMacro {
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            let mut rules = Vec::new_in(frame);
            let mut err = None;

            if let Some(args) = args {
                // The presence of an argument list means that this macro uses the
                // "simple" syntax, where the body is the RHS of a single rule.
                cov_mark::hit!(parse_macro_def_simple);

                let rule = (|| {
                    let lhs = MetaTemplate::parse_pattern(ctx_edition, args.iter())?;
                    let rhs = MetaTemplate::parse_template(ctx_edition, body.iter())?;

                    // In the "simple" syntax, there is apparently no way to specify
                    // that the single rule is an attribute or derive rule, so it
                    // must be a function-like rule.
                    Ok(crate::Rule::new(MacroCallStyle::FnLike, lhs, rhs))
                })();

                match rule {
                    Ok(rule) => rules.push(rule),
                    // alloc: an error.
                    Err(e) => err = Some(Box::new(e)),
                }
            } else {
                // There was no top-level argument list, so this macro uses the
                // list-of-rules syntax, similar to `macro_rules!`.
                cov_mark::hit!(parse_macro_def_rules);
                let mut src = body.iter();
                while !src.is_empty() {
                    let rule = match Rule::parse(ctx_edition, &mut src) {
                        Ok(it) => it,
                        Err(e) => {
                            // alloc: an error.
                            err = Some(Box::new(e));
                            break;
                        }
                    };
                    rules.push(rule);
                    if let Err(()) = src.expect_any_char(&[';', ',']) {
                        if !src.is_empty() {
                            // alloc: an error.
                            err = Some(Box::new(ParseError::expected(
                                "expected `;` or `,` to delimit rules",
                            )));
                        }
                        break;
                    }
                }
            }

            for Rule { lhs, .. } in &rules {
                if let Err(e) = validate(lhs) {
                    // alloc: an error.
                    err = Some(Box::new(e));
                    break;
                }
            }

            // alloc: the definition's rules, its memo, once at their exact length.
            let rules: Box<[Rule]> = rules.drain(..).collect();
            DeclarativeMacro { rules, err }
        })
    }

    pub fn err(&self) -> Option<&ParseError> {
        self.err.as_deref()
    }

    pub fn num_rules(&self) -> usize {
        self.rules.len()
    }

    pub fn rule_styles(&self) -> MacroCallStyles {
        if self.rules.is_empty() {
            // No rules could be parsed, so fall back to assuming that this
            // is intended to be a function-like macro.
            MacroCallStyles::FN_LIKE
        } else {
            self.rules
                .iter()
                .map(|rule| MacroCallStyles::from(rule.style))
                .fold(MacroCallStyles::empty(), |a, b| a | b)
        }
    }

    pub fn expand(
        &self,
        db: &Db,
        tt: &tt::TopSubtree,
        marker: impl Fn(&mut Span) + Copy,
        call_style: MacroCallStyle,
        call_site: Span,
    ) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
        self.expand_making(db, tt, marker, call_style, call_site).map(|(tree, arm, _)| (tree, arm))
    }

    /// [`Self::expand`], with the spans of the identifiers the expansion's `${concat(..)}`s made.
    pub fn expand_making(
        &self,
        db: &Db,
        tt: &tt::TopSubtree,
        marker: impl Fn(&mut Span) + Copy,
        call_style: MacroCallStyle,
        call_site: Span,
    ) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex, MadeIdents)> {
        expander::expand_rules(db, &self.rules, tt, marker, call_style, call_site)
    }

    /// [`Self::expand`] with the output in `frame`, a temporary of the caller's task; the
    /// match's own temporaries are in a task of their own, so they are gone when this returns.
    pub fn expand_in<'f>(
        &self,
        frame: &'f Frame,
        db: &Db,
        tt: &tt::TopSubtree,
        marker: impl Fn(&mut Span) + Copy,
        call_style: MacroCallStyle,
        call_site: Span,
    ) -> ExpandResult<(tt::TopSubtree<&'f Frame>, MatchedArmIndex)> {
        self.expand_making_in(frame, db, tt, marker, call_style, call_site)
            .map(|(tree, arm, _)| (tree, arm))
    }

    /// [`Self::expand_making`] with the output in `frame`, as [`Self::expand_in`].
    pub fn expand_making_in<'f>(
        &self,
        frame: &'f Frame,
        db: &Db,
        tt: &tt::TopSubtree,
        marker: impl Fn(&mut Span) + Copy,
        call_style: MacroCallStyle,
        call_site: Span,
    ) -> ExpandResult<(tt::TopSubtree<&'f Frame>, MatchedArmIndex, MadeIdents)> {
        let build = move |builder: tt::TopSubtreeBuilder| builder.build_in(frame);
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |scratch| {
            let rules = &self.rules;
            expander::expand_rules_in(scratch, build, db, rules, tt, marker, call_style, call_site)
        })
    }
}

/// The spans of the identifiers an expansion made with `${concat(..)}`, in the order it made
/// them: tokens no source has, which only the transcriber can tell from the others.
pub type MadeIdents = SmallVec<[Span; 2]>;

impl Rule {
    fn parse(
        edition: impl Copy + Fn(SyntaxContext) -> Edition,
        src: &mut TtIter<'_>,
    ) -> Result<Self, ParseError> {
        // Parse an optional `attr()` or `derive()` prefix before the LHS pattern.
        let style = parser::parse_rule_style(src)?;

        let (_, lhs) =
            src.expect_subtree().map_err(|()| ParseError::expected("expected subtree"))?;
        src.expect_char('=').map_err(|()| ParseError::expected("expected `=`"))?;
        src.expect_char('>').map_err(|()| ParseError::expected("expected `>`"))?;
        let (_, rhs) =
            src.expect_subtree().map_err(|()| ParseError::expected("expected subtree"))?;

        let lhs = MetaTemplate::parse_pattern(edition, lhs)?;
        let rhs = MetaTemplate::parse_template(edition, rhs)?;

        Ok(crate::Rule::new(style, lhs, rhs))
    }

    fn new(style: MacroCallStyle, mut lhs: MetaTemplate, mut rhs: MetaTemplate) -> Rule {
        let binders = parser::resolve_binders(&mut lhs, &mut rhs);
        let matcher = expander::Matcher::new(&lhs, binders);
        Rule { style, lhs, rhs, matcher }
    }
}

fn validate(pattern: &MetaTemplate) -> Result<(), ParseError> {
    for op in pattern.iter() {
        match op {
            Op::Subtree { tokens, .. } => validate(tokens)?,
            Op::Repeat { tokens: subtree, separator, .. } => {
                // Checks that no repetition which could match an empty token
                // https://github.com/rust-lang/rust/blob/a58b1ed44f5e06976de2bdc4d7dc81c36a96934f/src/librustc_expand/mbe/macro_rules.rs#L558
                let lsh_is_empty_seq = separator.is_none() && subtree.iter().all(|child_op| {
                    match *child_op {
                        // vis is optional
                        Op::Var { kind: Some(kind), .. } => kind == MetaVarKind::Vis,
                        Op::Repeat {
                            kind: parser::RepeatKind::ZeroOrMore | parser::RepeatKind::ZeroOrOne,
                            ..
                        } => true,
                        _ => false,
                    }
                });
                if lsh_is_empty_seq {
                    return Err(ParseError::RepetitionEmptyTokenTree);
                }
                validate(subtree)?
            }
            _ => (),
        }
    }
    Ok(())
}

pub type ExpandResult<T> = ValueResult<T, ExpandError>;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ValueResult<T, E> {
    pub value: T,
    pub err: Option<E>,
}

impl<T: Default, E> Default for ValueResult<T, E> {
    fn default() -> Self {
        Self { value: Default::default(), err: Default::default() }
    }
}

impl<T, E> ValueResult<T, E> {
    pub fn new(value: T, err: E) -> Self {
        Self { value, err: Some(err) }
    }

    pub fn ok(value: T) -> Self {
        Self { value, err: None }
    }

    pub fn only_err(err: E) -> Self
    where
        T: Default,
    {
        Self { value: Default::default(), err: Some(err) }
    }

    pub fn zip_val<U>(self, other: U) -> ValueResult<(T, U), E> {
        ValueResult { value: (self.value, other), err: self.err }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> ValueResult<U, E> {
        ValueResult { value: f(self.value), err: self.err }
    }

    pub fn as_ref(&self) -> ValueResult<&T, E>
    where
        E: Clone,
    {
        // clone: an error's `Arc`, only when there is one.
        ValueResult { value: &self.value, err: self.err.clone() }
    }

    pub fn map_err<E2>(self, f: impl FnOnce(E) -> E2) -> ValueResult<T, E2> {
        ValueResult { value: self.value, err: self.err.map(f) }
    }

    pub fn result(self) -> Result<T, E> {
        self.err.map_or(Ok(self.value), Err)
    }
}

impl<T: Default, E> From<Result<T, E>> for ValueResult<T, E> {
    fn from(result: Result<T, E>) -> Self {
        result.map_or_else(Self::only_err, Self::ok)
    }
}

pub fn expect_fragment<'t>(
    db: &Db,
    tt_iter: &mut TtIter<'t>,
    entry_point: ::parser::PrefixEntryPoint,
    delim_span: DelimSpan,
) -> ExpandResult<tt::TokenTreesView<'t>> {
    use ::parser;
    let buffer = tt_iter.remaining();
    let mut edition = |ctx: SyntaxContext| ctx.edition(db);
    // The parser reads only the first top-level trees of the level, doubled until it provably
    // read nothing past them: its position only moves forward, and it reads at most
    // `PARSER_LOOKAHEAD - 1` tokens past it (`Parser::nth` asserts `n <= 3`, and glued
    // punctuation looks two further). A fragment costs its own length, not the rest of the
    // level's, so `$($e:expr),*` is linear, not quadratic. Its tokens are the words the input
    // tree keeps (`syntax_bridge::view_tokens`, made once, at its first fragment), read in place
    // when the window ends where the tree does and otherwise copied, never converted.
    let mut window_trees = FIRST_WINDOW_TREES;
    let mut parser_input = parser::Input::with_capacity(4 * window_trees);
    let tree_traversal = loop {
        // clone: a cursor, one view.
        let mut fork = tt_iter.clone();
        let savepoint = fork.savepoint();
        let mut taken = 0;
        while taken < window_trees && fork.next().is_some() {
            taken += 1;
        }
        let whole = fork.is_empty();
        let window = fork.from_savepoint(savepoint);
        let tokens = syntax_bridge::view_tokens(window, true, &mut edition, &mut parser_input);
        let output = entry_point.parse(tokens);
        if whole || consumed_input_tokens(&output) + PARSER_LOOKAHEAD <= tokens.len() {
            break output;
        }
        window_trees *= 2;
    };
    // Over the whole level, so that `flat_advance_to` keeps its end.
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
                // FIXME: We need to split the tree properly here, but mutating the token trees
                // in the buffer is somewhat tricky to pull off.
                cursor.skip_invisible();
                cursor.bump_or_end();
            }
            parser::Step::Enter { .. } | parser::Step::Exit => (),
            parser::Step::Error { msg } => {
                error = error.or_else(|| Some(fragment_error(&cursor, msg, delim_span)));
            }
        }
    }

    cursor.leave_invisible();
    let err = match error {
        Some(err) => Some(err),
        None if !cursor.is_root() => Some(ExpandError::binding_error(
            buffer.cursor().token_tree().map_or(delim_span.close, |tt| tt.first_span()),
            // alloc: an error.
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

/// The first error a fragment's parse made: the parser's message, at the token it was at when
/// it made it (`cursor`), or at the group's close when it was at the end.
pub(crate) fn fragment_error(cursor: &tt::buffer::Cursor<'_>, msg: &str, delim_span: DelimSpan) -> ExpandError {
    let span = cursor.token_tree().map_or(delim_span.close, |tt| tt.first_span());
    ExpandError::binding_error(span, msg)
}

/// How many tokens past its position the `parser` crate's parser may read, plus one.
const PARSER_LOOKAHEAD: usize = 6;

/// The top-level token trees [`expect_fragment`]'s first window holds.
const FIRST_WINDOW_TREES: usize = 8;

/// The parser input tokens a parse consumed: the position it ended at.
fn consumed_input_tokens(output: &::parser::Output) -> usize {
    output
        .iter()
        .map(|step| match step {
            ::parser::Step::Token { n_input_tokens, .. } => n_input_tokens as usize,
            ::parser::Step::FloatSplit { .. } => 1,
            ::parser::Step::Enter { .. } | ::parser::Step::Exit | ::parser::Step::Error { .. } => 0,
        })
        .sum()
}
