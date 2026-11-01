//! What may follow each fragment of a matcher: rustc's FIRST sets and its walk of a matcher
//! with the set of what may come last, which gives `rust_2021_incompatible_or_patterns` (a
//! `pat` read as `pat_param` that `|` may follow) and decides whether a follow-set error stops
//! the walk where rustc stops it.
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs` (`check_lhs_nt_follows`,
//! `FirstSets`, `TokenSet`, `check_matcher_core`, `token_can_be_followed_by_any`,
//! `is_in_follow`) and `compiler/rustc_ast/src/token.rs` (`can_begin_type`) (MIT or
//! Apache-2.0).

use blaze_hash::HashMap;
use smallvec::SmallVec;
use syntax::TextRange;

use super::{
    quoted::{Frag, Matchers, Mtt, Op},
    tokens::{Delim, Ids, Kind, Punct, Token, Tokens},
};

/// A `pat` of an edition before 2021 that `|` may follow: rustc's `OrPatternsBackCompat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OrPattern {
    /// The `$x:pat`.
    pub(super) span: TextRange,
    pub(super) name: Token,
}

/// rustc's `TtHandle`: a token tree of the matcher, or a token rustc makes up (a delimiter, a
/// separator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handle {
    Tt(u32),
    Token(Token),
}

/// rustc's `TokenSet`: what may come first (or last), and whether nothing may.
#[derive(Clone, Debug)]
struct TokenSet {
    tokens: SmallVec<[Handle; 4]>,
    maybe_empty: bool,
}

impl TokenSet {
    fn empty() -> TokenSet {
        TokenSet { tokens: SmallVec::new(), maybe_empty: true }
    }

    fn singleton(tt: Handle) -> TokenSet {
        let mut tokens = SmallVec::new();
        tokens.push(tt);
        TokenSet { tokens, maybe_empty: false }
    }

    fn replace_with(&mut self, tt: Handle) {
        self.tokens.clear();
        self.tokens.push(tt);
        self.maybe_empty = false;
    }

    fn replace_with_irrelevant(&mut self) {
        self.tokens.clear();
        self.maybe_empty = false;
    }

    fn add_one(&mut self, tt: Handle) {
        self.add_one_maybe(tt);
        self.maybe_empty = false;
    }

    fn add_one_maybe(&mut self, tt: Handle) {
        if !self.tokens.contains(&tt) {
            self.tokens.push(tt);
        }
    }

    fn add_all(&mut self, other: &TokenSet) {
        for &tt in &other.tokens {
            self.add_one_maybe(tt);
        }
        if !other.maybe_empty {
            self.maybe_empty = false;
        }
    }
}

/// A follow-set error rustc gives: `$x:frag` is (or may be) followed by what is not allowed
/// after it. Not given here; what stops the walk is.
struct Errored;

/// The walk of one matcher.
struct Follow<'m> {
    m: &'m Matchers,
    tokens: &'m Tokens<'m>,
    /// The FIRST set of each repetition's contents, by the repetition.
    first: HashMap<u32, TokenSet>,
    out: &'m mut Vec<OrPattern>,
}

/// rustc's `check_lhs_nt_follows`: what the matcher `lhs` (a rule's, or its `attr(..)`
/// arguments) gives. One that is not in delimiters is an error rustc gives elsewhere.
pub(super) fn check_lhs(m: &Matchers, tokens: &Tokens<'_>, lhs: u32, out: &mut Vec<OrPattern>) {
    let Mtt::Delimited { tts, .. } = *m.at(lhs) else { return };
    let mut follow = Follow { m, tokens, first: HashMap::default(), out };
    follow.build(tts);
    let _ = follow.core(tts, &TokenSet::empty());
}

impl Follow<'_> {
    fn token(&self, tt: Handle) -> Option<Token> {
        match tt {
            Handle::Token(it) => Some(it),
            Handle::Tt(id) => match *self.m.at(id) {
                Mtt::Token(it) => Some(it),
                _ => None,
            },
        }
    }

    fn decl(&self, tt: Handle) -> Option<(TextRange, Token, Frag)> {
        match tt {
            Handle::Tt(id) => match *self.m.at(id) {
                Mtt::MetaVarDecl { range, name, kind } => Some((range, name, kind)),
                _ => None,
            },
            Handle::Token(_) => None,
        }
    }

    /// `FirstSets::new`'s `build_recur`: the FIRST set of `tts`, walking it backwards, and
    /// that of every repetition in it.
    fn build(&mut self, tts: Ids) -> TokenSet {
        let m = self.m;
        let mut first = TokenSet::empty();
        for id in tts.iter().rev() {
            match *m.at(id) {
                Mtt::Token(_) | Mtt::MetaVar { .. } | Mtt::MetaVarDecl { .. } | Mtt::MetaVarExpr { .. } => {
                    first.replace_with(Handle::Tt(id));
                }
                Mtt::Delimited { open, tts, .. } => {
                    self.build(tts);
                    first.replace_with(Handle::Token(open));
                }
                Mtt::Sequence { tts, separator, kleene, .. } => {
                    let sub = self.build(tts);
                    // clone: once per repetition of a matcher, only when `rust_2021_incompatible_or_patterns` may fire
                    self.first.insert(id, sub.clone());
                    if let Some(separator) = separator
                        && sub.maybe_empty
                    {
                        first.add_one_maybe(Handle::Token(separator));
                    }
                    if sub.maybe_empty || matches!(kleene.op, Op::ZeroOrMore | Op::ZeroOrOne) {
                        first.add_all(&TokenSet { maybe_empty: true, ..sub });
                    } else {
                        first = sub;
                    }
                }
            }
        }
        first
    }

    /// `FirstSets::first`: the FIRST set of `tts`, walking it forwards.
    fn first(&self, tts: Ids) -> TokenSet {
        let mut first = TokenSet::empty();
        for id in tts.iter() {
            match *self.m.at(id) {
                Mtt::Token(_) | Mtt::MetaVar { .. } | Mtt::MetaVarDecl { .. } | Mtt::MetaVarExpr { .. } => {
                    first.add_one(Handle::Tt(id));
                    return first;
                }
                Mtt::Delimited { open, .. } => {
                    first.add_one(Handle::Token(open));
                    return first;
                }
                Mtt::Sequence { separator, kleene, .. } => {
                    let sub = &self.first[&id];
                    if let Some(separator) = separator
                        && sub.maybe_empty
                    {
                        first.add_one_maybe(Handle::Token(separator));
                    }
                    first.add_all(sub);
                    if sub.maybe_empty || matches!(kleene.op, Op::ZeroOrMore | Op::ZeroOrOne) {
                        first.maybe_empty = true;
                        continue;
                    }
                    return first;
                }
            }
        }
        first
    }

    /// `check_matcher_core`: that each fragment of `matcher` may be followed by what may
    /// come after it (`follow` after the matcher); the set of what may come last in it.
    fn core(&mut self, matcher: Ids, follow: &TokenSet) -> Result<TokenSet, Errored> {
        let m = self.m;
        let mut last = TokenSet::empty();
        let mut errored = false;
        for (i, id) in matcher.iter().enumerate() {
            let suffix = matcher.skip(i + 1);
            let suffix_first = || {
                let mut first = self.first(suffix);
                if first.maybe_empty {
                    first.add_all(follow);
                }
                first
            };
            let suffix_first = match *m.at(id) {
                Mtt::Token(_) | Mtt::MetaVar { .. } | Mtt::MetaVarDecl { .. } | Mtt::MetaVarExpr { .. } => {
                    if followed_by_any(m.at(id)) {
                        last.replace_with_irrelevant();
                        continue;
                    }
                    last.replace_with(Handle::Tt(id));
                    suffix_first()
                }
                Mtt::Delimited { close, tts, .. } => {
                    self.core(tts, &TokenSet::singleton(Handle::Token(close)))?;
                    last.replace_with_irrelevant();
                    continue;
                }
                Mtt::Sequence { tts, separator, .. } => {
                    let mut suffix = suffix_first();
                    if let Some(separator) = separator {
                        suffix.add_one_maybe(Handle::Token(separator));
                    }
                    let next = self.core(tts, &suffix)?;
                    if next.maybe_empty {
                        last.add_all(&next);
                    } else {
                        last = next;
                    }
                    continue;
                }
            };
            for &tt in &last.tokens {
                let Some((span, name, kind)) = self.decl(tt) else { continue };
                for &next in &suffix_first.tokens {
                    if kind == (Frag::PatParam { inferred: true })
                        && self.token(next).is_some_and(|it| it.is(Punct::Or))
                    {
                        self.out.push(OrPattern { span, name });
                    }
                    if !self.is_in_follow(next, kind) {
                        errored = true;
                    }
                }
            }
        }
        if errored {
            return Err(Errored);
        }
        Ok(last)
    }

    /// rustc's `is_in_follow`: whether `next` may follow a fragment of `kind`.
    fn is_in_follow(&self, next: Handle, kind: Frag) -> bool {
        let token = self.token(next);
        if token.is_some_and(|it| matches!(it.kind, Kind::Close(_))) {
            return true;
        }
        let punct = |puncts: &[Punct]| token.is_some_and(|it| matches!(it.kind, Kind::Punct(p) if puncts.contains(&p)));
        let word = |words: &[&str]| {
            token.is_some_and(|it| it.kind == (Kind::Ident { raw: false }) && words.contains(&self.tokens.text(it.range)))
        };
        let decl = |kinds: &[Frag]| self.decl(next).is_some_and(|(_, _, it)| kinds.contains(&it));
        match kind {
            Frag::Item | Frag::Block => true,
            Frag::Stmt | Frag::Expr | Frag::Expr2021 { .. } => punct(&[Punct::FatArrow, Punct::Comma, Punct::Semi]),
            Frag::PatParam { .. } => {
                punct(&[Punct::FatArrow, Punct::Comma, Punct::Eq, Punct::Or]) || word(&["if", "in"]) || decl(&[Frag::Guard])
            }
            Frag::PatWithOr => punct(&[Punct::FatArrow, Punct::Comma, Punct::Eq]) || word(&["if", "in"]) || decl(&[Frag::Guard]),
            Frag::Guard => {
                punct(&[Punct::FatArrow, Punct::Comma]) || token.is_some_and(|it| it.kind == Kind::Open(Delim::Brace))
            }
            Frag::Path | Frag::Ty => {
                token.is_some_and(|it| matches!(it.kind, Kind::Open(Delim::Brace | Delim::Bracket)))
                    || punct(&[
                        Punct::Comma,
                        Punct::FatArrow,
                        Punct::Colon,
                        Punct::Eq,
                        Punct::Gt,
                        Punct::Shr,
                        Punct::Semi,
                        Punct::Or,
                    ])
                    || word(&["as", "where"])
                    || decl(&[Frag::Block])
            }
            Frag::Ident | Frag::Lifetime | Frag::Literal | Frag::Meta | Frag::Tt => true,
            Frag::Vis => match token {
                Some(token) => match token.kind {
                    Kind::Punct(Punct::Comma) => true,
                    Kind::Ident { raw: true } => true,
                    Kind::Ident { raw: false } => self.tokens.text(token.range) != "priv",
                    _ => can_begin_type(token.kind),
                },
                None => decl(&[Frag::Ident, Frag::Ty, Frag::Path]),
            },
        }
    }
}

/// rustc's `token_can_be_followed_by_any`: what is not a fragment, and the fragments of at most
/// one token tree.
fn followed_by_any(tt: &Mtt) -> bool {
    match *tt {
        Mtt::MetaVarDecl { kind, .. } => matches!(
            kind,
            Frag::Item | Frag::Block | Frag::Ident | Frag::Literal | Frag::Meta | Frag::Lifetime | Frag::Tt
        ),
        _ => true,
    }
}

/// rustc's `Token::can_begin_type`, of what is not an identifier.
fn can_begin_type(kind: Kind) -> bool {
    match kind {
        Kind::Open(Delim::Paren | Delim::Bracket) | Kind::Lifetime => true,
        Kind::Punct(it) => matches!(
            it,
            Punct::Bang
                | Punct::Star
                | Punct::And
                | Punct::AndAnd
                | Punct::Question
                | Punct::Lt
                | Punct::Shl
                | Punct::PathSep
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use span::Edition;
    use syntax::{
        SourceFile,
        view::{self as ast, AstNode as _},
    };

    use super::*;

    /// The `pat`s `|` may follow in the definition `text`.
    fn or_patterns(text: &str, edition: Edition) -> Vec<String> {
        let file = SourceFile::parse(text, edition);
        let def = file.root().descendants().find_map(ast::Macro::cast).unwrap();
        let tokens = Tokens::of(def, &|_| (edition, true)).unwrap();
        let (m, rules) = Matchers::rules(&tokens, true, edition);
        let mut out = Vec::new();
        for rule in rules {
            for lhs in rule.args.into_iter().chain(rule.lhs) {
                check_lhs(&m, &tokens, lhs, &mut out);
            }
        }
        out.iter().map(|it| tokens.text(it.span).to_owned()).collect()
    }

    #[test]
    fn a_pat_before_2021_that_an_or_may_follow() {
        assert_eq!(or_patterns("macro_rules! foo { ($x:pat | $y:pat) => {} }", Edition::Edition2015), ["$x:pat"]);
        assert_eq!(or_patterns("macro_rules! bar { ($($x:pat)+ | $($y:pat)+) => {} }", Edition::Edition2015), [
            "$x:pat"
        ]);
        assert_eq!(
            or_patterns("macro_rules! m { ( $expr:expr , $( $( $pat:pat )|+ => $expr_arm:expr ),+ ) => {}; }", Edition::Edition2018),
            ["$pat:pat"],
        );
    }

    #[test]
    fn a_pat_is_linted_for_each_or_that_may_follow_it() {
        assert_eq!(or_patterns("macro_rules! m { ($($x:pat)|* |) => {} }", Edition::Edition2015), ["$x:pat", "$x:pat"]);
    }

    #[test]
    fn no_pat_param_nor_pat_of_2021_is_linted() {
        assert!(or_patterns("macro_rules! baz { ($x:pat_param | $y:pat_param) => {} }", Edition::Edition2015).is_empty());
        assert!(or_patterns("macro_rules! foo { ($x:pat | $y:pat) => {} }", Edition::Edition2021).is_empty());
        assert!(or_patterns("macro_rules! foo { ($x:pat || $y:pat) => {} }", Edition::Edition2015).is_empty());
    }

    #[test]
    fn a_follow_set_error_in_a_group_stops_the_walk() {
        // `$e:expr` followed by `)` is fine; by `+` it is an error, and rustc stops there.
        assert!(or_patterns("macro_rules! m { (($e:expr +) $x:pat | ) => {} }", Edition::Edition2015).is_empty());
        assert_eq!(or_patterns("macro_rules! m { (($e:expr) $x:pat | ) => {} }", Edition::Edition2015), ["$x:pat"]);
    }
}
