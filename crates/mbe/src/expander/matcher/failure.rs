//! What rustc says of a call no rule matches (`rustc_expand/src/mbe/diagnostics.rs`,
//! `failed_to_match_macro`): the rules are matched again with a tracker, rustc's
//! `CollectTrackerAndEmitter`, which keeps the failure of the rule that got furthest into the
//! input; then each is matched with a `,` added where rustc's `TokenStream::add_comma` adds one.
//! Like rustc's second match, this runs only when no rule matched, so a call that expands pays
//! nothing for it.

use std::borrow::Cow;

use base_db::Db;
use blaze_mem::frame::Frame;
use intern::Symbol;
use span::{Edition, Span, TextRange};
use tt::{
    DelimSpan, DelimiterKind,
    iter::{TtElement, TtIter},
};

use super::{Buffers, Loc, Match, Matcher, NONE, match_loop};
use crate::{ExpandError, ExpandErrorKind, MacroCallStyle, MatchFailure, Rule, parser::Separator};

/// A `Var` location's `$name:spec`: rustc's `MatcherLoc::MetaVarDecl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::expander) struct VarDecl {
    pub(in crate::expander) name: Symbol,
    pub(in crate::expander) spec: Option<Symbol>,
    pub(in crate::expander) span: Span,
}

/// A visible group of a pattern: its `Open` and `Close` locations and its delimiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::expander) struct Group {
    pub(in crate::expander) open: u32,
    pub(in crate::expander) close: u32,
    pub(in crate::expander) delimiter: tt::Delimiter,
}

/// A location of a rule, with whether its separator was matched: rustc's `MatcherLoc`.
#[derive(Clone, Copy, Debug)]
struct At {
    rule: u32,
    dot: u32,
    sep_matched: bool,
}

/// The furthest failure so far: rustc's `BestFailure`.
#[derive(Debug)]
struct Best {
    position: u32,
    token: Option<Box<str>>,
    span: Span,
    remaining: At,
}

/// rustc's `CollectTrackerAndEmitter`, over the rules of one call.
#[derive(Debug)]
pub(super) struct Tracker {
    rule: u32,
    /// The tokens the rule being matched has taken: rustc's `approx_token_stream_pos`, which
    /// counts each delimiter and each glued punctuation as one.
    position: u32,
    /// The closing delimiter of the input.
    end: Span,
    /// The closing delimiters of the groups of the input being matched, innermost last.
    closes: Vec<(DelimiterKind, Span)>,
    /// rustc's `remaining_matcher`: once set, the location of the last position each step goes
    /// through, unless that is the end of the rule. Kept from rule to rule, as rustc keeps it.
    remaining: Option<At>,
    best: Option<Best>,
}

impl Tracker {
    /// Before a rule is matched against an input delimited by `span`.
    pub(super) fn start(&mut self, span: DelimSpan) {
        self.position = 0;
        self.end = span.close;
        self.closes.clear();
    }

    /// rustc's `before_match_loc`: a position at `dot` is processed, the step's last if `last`.
    pub(super) fn before_loc(&mut self, matcher: &Matcher, dot: u32, sep_matched: bool, last: bool) {
        if self.remaining.is_none() || (last && matcher.locs[dot as usize] != Loc::Eof) {
            self.remaining = Some(At { rule: self.rule, dot, sep_matched });
        }
    }

    /// The input went on by `tokens` of rustc's.
    pub(super) fn bump(&mut self, tokens: u32) {
        self.position += tokens;
    }

    /// The input went on by one token tree: into a group, out of one at its end, or past a
    /// token.
    pub(super) fn step(&mut self, next: Option<&TtElement<'_>>) {
        self.position += 1;
        match next {
            Some(TtElement::Subtree(subtree, _)) => {
                self.closes.push((subtree.delimiter.kind, subtree.delimiter.close));
            }
            None => {
                self.closes.pop();
            }
            Some(TtElement::Leaf(_)) => {}
        }
    }

    /// rustc's `failure`: no position goes on at the token `src` is at, or (`None`) at the end
    /// of the input. The first failure at the furthest position is kept.
    pub(super) fn fail(&mut self, db: &Db, src: Option<&TtIter<'_>>) {
        if self.best.as_ref().is_some_and(|best| self.position <= best.position) {
            return;
        }
        let remaining = self.remaining.expect("every step goes through a location");
        let (token, span) = match src {
            None => (None, self.end),
            Some(src) => {
                let (token, span) = self.token_at(db, src);
                // alloc: an error's text, when no rule matches.
                (Some(Box::from(token)), span)
            }
        };
        self.best = Some(Best { position: self.position, token, span, remaining });
    }

    /// rustc's `token_descr` of the token `src` is at, and its span.
    fn token_at(&self, db: &Db, src: &TtIter<'_>) -> (Cow<'static, str>, Span) {
        match src.peek() {
            Some(TtElement::Leaf(tt::Leaf::Punct(_))) => {
                // clone: a cursor, one view.
                let mut fork = src.clone();
                let puncts = fork.expect_glued_punct().expect("the token is punctuation");
                if let [quote] = puncts.as_slice()
                    && quote.char == '\''
                    && let Some(TtElement::Leaf(tt::Leaf::Ident(name))) = fork.peek()
                {
                    // alloc: an error's text, when no rule matches.
                    let text = format!("`'{}`", name.sym);
                    return (Cow::Owned(text), quote.span.cover(name.span));
                }
                let span = puncts[0].span.cover(puncts[puncts.len() - 1].span);
                (Cow::Owned(puncts_descr(&puncts)), span)
            }
            Some(TtElement::Leaf(tt::Leaf::Ident(ident))) => {
                (Cow::Owned(ident_descr(&ident, ident.span.ctx.edition(db))), ident.span)
            }
            // alloc: an error's text, when no rule matches.
            Some(TtElement::Leaf(tt::Leaf::Literal(lit))) => (Cow::Owned(format!("`{lit}`")), lit.span),
            Some(TtElement::Subtree(subtree, _)) => {
                (Cow::Borrowed(delimiter_descr(subtree.delimiter.kind, false)), subtree.delimiter.open)
            }
            None => {
                let &(kind, span) = self.closes.last().expect("the input ends in a group");
                (Cow::Borrowed(delimiter_descr(kind, true)), span)
            }
        }
    }
}

/// The tokens of rustc's in `view`: a group's delimiters are two, and punctuation joined to the
/// token after it is one with it (rustc's glued punctuation and lifetimes).
pub(super) fn tokens(view: tt::TokenTreesView<'_>) -> u32 {
    let mut count = 0;
    let mut joined = false;
    for tt in view.iter_flat_tokens() {
        match tt {
            tt::TokenTree::Subtree(_) => {
                count += 2;
                joined = false;
            }
            tt::TokenTree::Leaf(leaf) => {
                if !joined {
                    count += 1;
                }
                joined = matches!(leaf, tt::Leaf::Punct(it) if it.spacing == tt::Spacing::Joint);
            }
        }
    }
    count
}

/// rustc's `failed_to_match_macro`, for a call of `rules` in `call_style` with `input`, which
/// no rule matches: the error at the token where the rule that got furthest stopped. `None`
/// when no rule failed as rustc's fail, which is where rustc gives its ambiguity error. The
/// matches' buffers are in `frame`, the call's.
pub(crate) fn match_failure(
    db: &Db,
    frame: &Frame,
    rules: &[Rule],
    input: &tt::TopSubtree,
    call_style: MacroCallStyle,
) -> Option<ExpandError> {
    let mut buffers = Buffers::new_in(frame);
    let mut res = Match::new_in(frame);
    let mut track = Tracker {
        rule: 0,
        position: 0,
        end: input.top_subtree().delimiter.close,
        // alloc: none until the input has a group.
        closes: Vec::new(),
        remaining: None,
        best: None,
    };
    for (idx, rule) in rules.iter().enumerate() {
        if rule.style != call_style {
            continue;
        }
        track.rule = idx as u32;
        match_loop(db, &rule.matcher, input, &mut buffers, &mut res, Some(&mut track));
    }
    let best = track.best?;
    let (remaining, remaining_span) =
        describe(db, &rules[best.remaining.rule as usize].matcher, best.remaining);
    let missing_comma = match call_style {
        MacroCallStyle::FnLike => missing_comma(db, frame, rules, input),
        MacroCallStyle::Attr | MacroCallStyle::Derive => None,
    };
    let failure = MatchFailure {
        token: best.token,
        // alloc: an error's text, when no rule matches.
        remaining: Box::from(remaining),
        remaining_span,
        missing_comma,
    };
    // alloc: the error, when no rule matches.
    Some(ExpandError::new(best.span, ExpandErrorKind::MatchFailure(Box::new(failure))))
}

/// rustc's `TokenStream::add_comma` and the check after it: after the last token tree of the
/// input's top level that is a group, or an identifier or literal not joined to the identifier
/// or literal after it, and is not followed by a `,`, a `,` makes some function-like rule
/// match. The span is empty, at the end of that token tree.
fn missing_comma(db: &Db, frame: &Frame, rules: &[Rule], input: &tt::TopSubtree) -> Option<Span> {
    let mut iter = input.iter();
    let start = iter.savepoint();
    let mut at = None;
    while let Some(left) = iter.next() {
        let Some(right) = iter.peek() else { break };
        let end = match (&left, &right) {
            (_, TtElement::Leaf(tt::Leaf::Punct(comma))) if comma.char == ',' => continue,
            (TtElement::Leaf(left), TtElement::Leaf(right))
                if is_word(db, left) && is_word(db, right) && !joined(*left.span(), *right.span()) =>
            {
                *left.span()
            }
            (TtElement::Subtree(subtree, _), _) => subtree.delimiter.close,
            _ => continue,
        };
        at = Some((iter.from_savepoint(start), iter.remaining(), end));
    }
    let (before, after, end) = at?;
    let comma = Span { range: TextRange::empty(end.range.end()), ..end };
    // alloc: the input with the comma, only when no rule matches.
    let mut builder = tt::TopSubtreeBuilder::new(input.top_subtree().delimiter);
    builder.extend_with_tt(before);
    builder.push(tt::Leaf::Punct(tt::Punct { char: ',', spacing: tt::Spacing::Alone, span: comma }));
    builder.extend_with_tt(after);
    let with_comma = builder.build();
    let mut buffers = Buffers::new_in(frame);
    let mut res = Match::new_in(frame);
    let matches = rules.iter().filter(|rule| rule.style == MacroCallStyle::FnLike).any(|rule| {
        match_loop(db, &rule.matcher, &with_comma, &mut buffers, &mut res, None);
        res.err.is_none()
    });
    matches.then_some(comma)
}

/// rustc's `is_non_reserved_ident() || is_lit()`.
fn is_word(db: &Db, leaf: &tt::Leaf) -> bool {
    match leaf {
        tt::Leaf::Literal(_) => true,
        tt::Leaf::Ident(ident) => {
            ident.is_raw == tt::IdentIsRaw::Yes
                || keyword(ident.sym.as_str(), ident.span.ctx.edition(db)).is_none()
        }
        tt::Leaf::Punct(_) => false,
    }
}

/// Whether `left` ends where `right` starts: rustc's `Spacing::JointHidden`, not `Alone`.
fn joined(left: Span, right: Span) -> bool {
    left.anchor == right.anchor && left.range.end() == right.range.start()
}

/// rustc's `MatcherLoc` `Display` of `at`, and its `MatcherLoc::span`.
fn describe(db: &Db, matcher: &Matcher, at: At) -> (Cow<'static, str>, Option<Span>) {
    let dot = at.dot;
    let group = |pick: fn(&Group) -> u32| {
        matcher.groups.iter().find(|it| pick(it) == dot).expect("a delimiter's location has its group")
    };
    match matcher.locs[dot as usize] {
        Loc::Literal(it) => {
            let lit = &matcher.literals[it as usize];
            // alloc: an error's text, when no rule matches.
            (Cow::Owned(format!("`{lit}`")), Some(lit.span))
        }
        Loc::Ident(it) => {
            let ident = &matcher.idents[it as usize];
            (Cow::Owned(ident_descr(ident, ident.span.ctx.edition(db))), Some(ident.span))
        }
        Loc::Punct(it) => {
            let puncts = &matcher.puncts[it as usize];
            let span = puncts[0].span.cover(puncts[puncts.len() - 1].span);
            (Cow::Owned(puncts_descr(puncts)), Some(span))
        }
        Loc::Var { .. } => {
            let (_, decl) = matcher
                .var_decls
                .iter()
                .find(|(loc, _)| *loc == dot)
                .expect("a metavariable's location has its declaration");
            let text = match decl.spec {
                // alloc: an error's text, when no rule matches.
                Some(spec) => format!("meta-variable `${}:{spec}`", decl.name),
                // alloc: an error's text, when no rule matches.
                None => format!("meta-variable `${}`", decl.name),
            };
            (Cow::Owned(text), Some(decl.span))
        }
        Loc::Subtree(_) => (Cow::Borrowed("delimiter"), None),
        Loc::Open => {
            let group = group(|it| it.open);
            (Cow::Borrowed(delimiter_descr(group.delimiter.kind, false)), Some(group.delimiter.open))
        }
        Loc::Close => {
            let group = group(|it| it.close);
            (Cow::Borrowed(delimiter_descr(group.delimiter.kind, true)), Some(group.delimiter.close))
        }
        Loc::RepStart(_) => (Cow::Borrowed("sequence start"), None),
        Loc::RepEnd(rep) => {
            let rep = matcher.reps[rep as usize];
            if rep.separator == NONE || at.sep_matched {
                return (Cow::Borrowed("sequence end"), None);
            }
            let (text, span) = separator_descr(db, &matcher.separators[rep.separator as usize]);
            (Cow::Owned(text), Some(span))
        }
        Loc::MetaExpr => unreachable!("a pattern has no metavariable expressions"),
        Loc::Eof => (Cow::Borrowed("end of macro"), None),
    }
}

/// rustc's `token_descr` of glued punctuation.
fn puncts_descr(puncts: &[tt::Punct]) -> String {
    // alloc: an error's text, when no rule matches.
    let mut text = String::from("`");
    text.extend(puncts.iter().map(|it| it.char));
    text.push('`');
    text
}

/// rustc's `token_descr` of a repetition's separator, and its span.
fn separator_descr(db: &Db, separator: &Separator) -> (String, Span) {
    match separator {
        // alloc: an error's text, when no rule matches.
        Separator::Literal(lit) => (format!("`{lit}`"), lit.span),
        Separator::Ident(ident) => (ident_descr(ident, ident.span.ctx.edition(db)), ident.span),
        Separator::Puncts(puncts) => {
            (puncts_descr(puncts), puncts[0].span.cover(puncts[puncts.len() - 1].span))
        }
        // alloc: an error's text, when no rule matches.
        Separator::Lifetime(quote, name) => (format!("`'{}`", name.sym), quote.span.cover(name.span)),
    }
}

/// rustc's `token_descr` of an opening (or `closing`) delimiter. rustc prints an invisible
/// group as the kind of the fragment it holds ("`expr` metavariable"), which the group of a
/// transcribed fragment here does not keep.
fn delimiter_descr(kind: DelimiterKind, closing: bool) -> &'static str {
    match (kind, closing) {
        (DelimiterKind::Parenthesis, false) => "`(`",
        (DelimiterKind::Parenthesis, true) => "`)`",
        (DelimiterKind::Brace, false) => "`{`",
        (DelimiterKind::Brace, true) => "`}`",
        (DelimiterKind::Bracket, false) => "`[`",
        (DelimiterKind::Bracket, true) => "`]`",
        (DelimiterKind::Invisible, _) => "metavariable",
    }
}

/// What kind of keyword rustc's `token_descr` calls an identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keyword {
    /// `_` and `$crate`: rustc's "reserved identifier".
    ReservedIdent,
    /// A keyword in use: rustc's "keyword".
    Used,
    /// A keyword reserved for later: rustc's "reserved keyword".
    Unused,
}

/// rustc's `Symbol::is_special`, `is_used_keyword` and `is_unused_keyword` in `edition`.
fn keyword(text: &str, edition: Edition) -> Option<Keyword> {
    const USED: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while",
    ];
    const UNUSED: &[&str] = &[
        "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof",
        "unsized", "virtual", "yield",
    ];
    if matches!(text, "_" | "$crate") {
        Some(Keyword::ReservedIdent)
    } else if USED.contains(&text)
        || (edition.at_least_2018() && matches!(text, "async" | "await" | "dyn"))
    {
        Some(Keyword::Used)
    } else if UNUSED.contains(&text)
        || (edition.at_least_2018() && text == "try")
        || (edition.at_least_2024() && text == "gen")
    {
        Some(Keyword::Unused)
    } else {
        None
    }
}

/// rustc's `token_descr` of an identifier in `edition`.
fn ident_descr(ident: &tt::Ident, edition: Edition) -> String {
    let kind = match ident.is_raw {
        tt::IdentIsRaw::Yes => None,
        tt::IdentIsRaw::No => keyword(ident.sym.as_str(), edition),
    };
    match kind {
        // alloc: an error's text, when no rule matches.
        Some(Keyword::ReservedIdent) => format!("reserved identifier `{ident}`"),
        // alloc: an error's text, when no rule matches.
        Some(Keyword::Used) => format!("keyword `{ident}`"),
        // alloc: an error's text, when no rule matches.
        Some(Keyword::Unused) => format!("reserved keyword `{ident}`"),
        // alloc: an error's text, when no rule matches.
        None => format!("`{ident}`"),
    }
}
