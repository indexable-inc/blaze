//! A macro definition's rules as rustc's macro machinery parses them: each rule's matcher
//! (with `$x:frag` declarations, `$(..) sep op` repetitions) and transcriber (with `$x`
//! uses and `${..}` expressions), as `rustc_expand::mbe::TokenTree`s.
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs`
//! (`compile_declarative_macro`: how the body is split into rules, and where a malformed one
//! stops it), `compiler/rustc_expand/src/mbe/quoted.rs` (`parse`, `parse_tree`,
//! `parse_sep_and_kleene_op`), `compiler/rustc_expand/src/mbe/metavar_expr.rs`
//! (`MetaVarExpr::parse`) and `compiler/rustc_ast/src/token.rs`
//! (`NonterminalKind::from_symbol`) (MIT or Apache-2.0).

use span::Edition;
use syntax::TextRange;

use super::tokens::{Delim, Group, Ids, Kind, Lit, Punct, Token, Tokens, Tt};

/// A fragment specifier: rustc's `NonterminalKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Frag {
    Item,
    Block,
    Stmt,
    /// `pat_param`, or `pat` before 2021 (`inferred`).
    PatParam { inferred: bool },
    /// `pat` from 2021 on.
    PatWithOr,
    /// `expr` from 2024 on.
    Expr,
    /// `expr_2021`, or `expr` before 2024 (`inferred`).
    Expr2021 { inferred: bool },
    Ty,
    Ident,
    Lifetime,
    Literal,
    Meta,
    Path,
    Vis,
    Guard,
    Tt,
}

impl Frag {
    /// rustc's `NonterminalKind::from_symbol`: the edition is the specifier's span's.
    fn of(name: &str, edition: Edition) -> Option<Frag> {
        Some(match name {
            "item" => Frag::Item,
            "block" => Frag::Block,
            "stmt" => Frag::Stmt,
            "pat" if edition.at_least_2021() => Frag::PatWithOr,
            "pat" => Frag::PatParam { inferred: true },
            "pat_param" => Frag::PatParam { inferred: false },
            "expr" if edition.at_least_2024() => Frag::Expr,
            "expr" => Frag::Expr2021 { inferred: true },
            "expr_2021" => Frag::Expr2021 { inferred: false },
            "ty" => Frag::Ty,
            "ident" => Frag::Ident,
            "lifetime" => Frag::Lifetime,
            "literal" => Frag::Literal,
            "meta" => Frag::Meta,
            "path" => Frag::Path,
            "vis" => Frag::Vis,
            "guard" => Frag::Guard,
            "tt" => Frag::Tt,
            _ => return None,
        })
    }
}

/// A repetition's operator: rustc's `KleeneOp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

/// A repetition's operator and where it is: rustc's `KleeneToken`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Kleene {
    pub(super) op: Op,
    pub(super) range: TextRange,
}

/// rustc's `mbe::TokenTree`. Spans are rustc's: a `$x` from `$` to `x`, a `$x:frag` to the
/// specifier's end.
#[derive(Clone, Copy, Debug)]
pub(super) enum Mtt {
    Token(Token),
    /// `$x` in a transcriber, or in a matcher of a macro a transcriber defines.
    MetaVar { range: TextRange, name: Token },
    /// `$x:frag` in a matcher.
    MetaVarDecl { range: TextRange, name: Token, kind: Frag },
    /// `${..}`: its braces, and the variables it names (in [`Matchers::vars`]).
    MetaVarExpr { range: TextRange, vars: Ids },
    Delimited { delim: Delim, open: Token, close: Token, tts: Ids },
    Sequence { tts: Ids, separator: Option<Token>, kleene: Kleene },
}

/// Whether a token tree is parsed as a matcher or a transcriber: rustc's `RulePart`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Part {
    Pattern,
    Body,
}

/// A rule of a definition, as far as rustc parsed it before it stopped: its `attr(..)`
/// arguments, its matcher and its transcriber.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Rule {
    pub(super) args: Option<u32>,
    pub(super) lhs: Option<u32>,
    pub(super) rhs: Option<u32>,
}

/// The rules of a definition, their token trees in one arena.
pub(super) struct Matchers {
    pub(super) tts: Vec<Mtt>,
    /// The variables `${..}` expressions name.
    pub(super) vars: Vec<Token>,
    scratch: Vec<Mtt>,
    /// The edition of the definition (rustc's `item.span.edition()`), which a specifier not of
    /// a macro's making is read in.
    edition: Edition,
}

/// A cursor on a run of token trees: rustc's `TokenStreamIter`.
struct Iter<'t> {
    tts: &'t [Tt],
    at: usize,
}

impl<'t> Iter<'t> {
    fn new(tts: &'t [Tt]) -> Iter<'t> {
        Iter { tts, at: 0 }
    }

    fn next(&mut self) -> Option<&'t Tt> {
        let it = self.tts.get(self.at)?;
        self.at += 1;
        Some(it)
    }

    fn peek(&self) -> Option<&'t Tt> {
        self.tts.get(self.at)
    }
}

impl Matchers {
    /// The rules of `tokens`, a `macro_rules!` body (rules separated by `;`) or a `macro`'s (by
    /// `,`), as `compile_declarative_macro` takes them: it stops at the first rule it cannot
    /// read on, and a rule it stopped in has what it read of it.
    pub(super) fn rules(tokens: &Tokens<'_>, macro_rules: bool, edition: Edition) -> (Matchers, Vec<Rule>) {
        // alloc: once per macro definition, only when a lint of it (allowed by default) may fire
        let mut m = Matchers { tts: Vec::new(), vars: Vec::new(), scratch: Vec::new(), edition };
        // alloc: once per macro definition, only when a lint of it (allowed by default) may fire
        let mut rules = Vec::new();
        let top = tokens.get(tokens.top);
        let separator = if macro_rules { Punct::Semi } else { Punct::Comma };
        let is = |tt: &Tt, punct| matches!(tt, Tt::Token(it) if it.is(punct));
        let mut i = 0;
        while i < top.len() {
            if tokens.is_word(&top[i], "unsafe") {
                i += 1;
                if i == top.len() {
                    break;
                }
            }
            let mut rule = Rule::default();
            if tokens.is_word(&top[i], "attr") {
                i += 1;
                if i == top.len() {
                    break;
                }
                rule.args = Some(m.one(tokens, &top[i], Part::Pattern));
                i += 1;
                if i == top.len() {
                    rules.push(rule);
                    break;
                }
            } else if tokens.is_word(&top[i], "derive") {
                i += 2;
                if i >= top.len() || is(&top[i], Punct::FatArrow) {
                    break;
                }
            }
            rule.lhs = Some(m.one(tokens, &top[i], Part::Pattern));
            i += 1;
            if i == top.len() || !is(&top[i], Punct::FatArrow) {
                rules.push(rule);
                break;
            }
            i += 1;
            if i == top.len() {
                rules.push(rule);
                break;
            }
            rule.rhs = Some(m.one(tokens, &top[i], Part::Body));
            i += 1;
            rules.push(rule);
            if i == top.len() || !is(&top[i], separator) {
                break;
            }
            i += 1;
        }
        (m, rules)
    }

    pub(super) fn at(&self, id: u32) -> &Mtt {
        &self.tts[id as usize]
    }

    /// rustc's `parse_one_tt`: `tt` parsed alone.
    fn one(&mut self, tokens: &Tokens<'_>, tt: &Tt, part: Part) -> u32 {
        let tree = self.tree(tokens, tt, &mut Iter::new(&[]), part);
        self.tts.push(tree);
        (self.tts.len() - 1) as u32
    }

    /// rustc's `quoted::parse`: the children of a group appended to the arena after those of
    /// the groups in them.
    fn parse(&mut self, tokens: &Tokens<'_>, input: Ids, part: Part) -> Ids {
        let mark = self.scratch.len();
        let mut iter = Iter::new(tokens.get(input));
        while let Some(tt) = iter.next() {
            let tree = self.tree(tokens, tt, &mut iter, part);
            let Mtt::MetaVar { range: start, name } = tree else {
                self.scratch.push(tree);
                continue;
            };
            if part == Part::Body {
                self.scratch.push(tree);
                continue;
            }
            // A matcher's `$x` without a specifier is an error, read as `$x:tt`.
            let decl = |range, kind| Mtt::MetaVarDecl { range, name, kind };
            let Some(Tt::Token(colon)) = iter.peek().filter(|it| matches!(it, Tt::Token(t) if t.is(Punct::Colon)))
            else {
                self.scratch.push(decl(start, Frag::Tt));
                continue;
            };
            iter.next();
            let Some(&Tt::Token(fragment)) = iter.next() else {
                self.scratch.push(decl(TextRange::new(start.start(), colon.range.end()), Frag::Tt));
                continue;
            };
            if !matches!(fragment.kind, Kind::Ident { .. }) {
                self.scratch.push(decl(fragment.range, Frag::Tt));
                continue;
            }
            let range = TextRange::new(start.start(), fragment.range.end());
            let edition = if fragment.root { self.edition } else { fragment.edition };
            let kind = Frag::of(tokens.name(fragment.range), edition).unwrap_or(Frag::Tt);
            self.scratch.push(decl(range, kind));
        }
        let start = self.tts.len() as u32;
        self.tts.extend(self.scratch.drain(mark..));
        Ids { start, end: self.tts.len() as u32 }
    }

    /// rustc's `parse_tree`: `tt`, with what follows it in `iter` when it starts a `$..`.
    fn tree(&mut self, tokens: &Tokens<'_>, tt: &Tt, iter: &mut Iter<'_>, part: Part) -> Mtt {
        let dollar = match *tt {
            Tt::Token(it) if it.is(Punct::Dollar) => it,
            Tt::Token(it) => return Mtt::Token(it),
            Tt::Delimited(Group { delim, open, close, tts }) => {
                return Mtt::Delimited { delim, open, close, tts: self.parse(tokens, tts, part) };
            }
        };
        match iter.next() {
            Some(&Tt::Delimited(group)) => {
                if part == Part::Body && group.delim == Delim::Brace {
                    return match self.metavar_expr(tokens, group.tts) {
                        Some(vars) => Mtt::MetaVarExpr { range: group.range(), vars },
                        None => Mtt::Token(dollar),
                    };
                }
                let tts = self.parse(tokens, group.tts, part);
                let (separator, kleene) = sep_and_kleene_op(iter, group.range());
                Mtt::Sequence { tts, separator, kleene }
            }
            Some(&Tt::Token(name)) if matches!(name.kind, Kind::Ident { .. }) => {
                let range = TextRange::new(dollar.range.start(), name.range.end());
                if name.kind == (Kind::Ident { raw: false }) && tokens.text(name.range) == "crate" {
                    return Mtt::Token(Token { range, ..name });
                }
                Mtt::MetaVar { range, name }
            }
            Some(&Tt::Token(second)) if second.is(Punct::Dollar) => Mtt::Token(second),
            // `$` and what is not a name is an error, read as a variable with no name.
            Some(&Tt::Token(other)) => {
                Mtt::MetaVar { range: other.range, name: Token { range: TextRange::empty(other.range.start()), ..other } }
            }
            None => Mtt::Token(dollar),
        }
    }

    /// rustc's `MetaVarExpr::parse` of what is in `${..}`: the variables it names, or `None`
    /// where it is an error (and rustc reads the `$` alone, without the braces).
    fn metavar_expr(&mut self, tokens: &Tokens<'_>, input: Ids) -> Option<Ids> {
        let mark = self.vars.len();
        let parsed = self.metavar_expr_inner(tokens, input);
        if parsed.is_none() {
            self.vars.truncate(mark);
        }
        parsed.map(|()| Ids { start: mark as u32, end: self.vars.len() as u32 })
    }

    fn metavar_expr_inner(&mut self, tokens: &Tokens<'_>, input: Ids) -> Option<()> {
        let mut iter = Iter::new(tokens.get(input));
        let ident = plain_ident(iter.next())?;
        let Some(Tt::Delimited(Group { delim: Delim::Paren, tts: args, .. })) = iter.next() else { return None };
        if iter.peek().is_some() {
            return None;
        }
        let mut args = Iter::new(tokens.get(*args));
        match tokens.name(ident.range) {
            "concat" => {
                let mut elements = 0;
                loop {
                    let is_var = eat(&mut args, Punct::Dollar);
                    let Some(Tt::Token(token)) = args.next() else { return None };
                    if is_var {
                        self.vars.push(plain_ident(Some(&Tt::Token(*token)))?);
                    } else if token.kind != Kind::Literal(Lit::Str) {
                        plain_ident(Some(&Tt::Token(*token)))?;
                    }
                    elements += 1;
                    if args.peek().is_none() {
                        break;
                    }
                    if !eat(&mut args, Punct::Comma) {
                        return None;
                    }
                }
                if elements < 2 {
                    return None;
                }
            }
            "count" => {
                if !eat(&mut args, Punct::Dollar) {
                    return None;
                }
                self.vars.push(plain_ident(args.next())?);
                if eat(&mut args, Punct::Comma) {
                    args.peek()?;
                    depth(&mut args)?;
                }
            }
            "ignore" => {
                if !eat(&mut args, Punct::Dollar) {
                    return None;
                }
                self.vars.push(plain_ident(args.next())?);
            }
            "index" | "len" => depth(&mut args)?,
            _ => return None,
        }
        match args.peek() {
            Some(_) => None,
            None => Some(()),
        }
    }
}

/// rustc's `parse_ident`: a name not written `r#`.
fn plain_ident(tt: Option<&Tt>) -> Option<Token> {
    match tt? {
        Tt::Token(it) if it.kind == (Kind::Ident { raw: false }) => Some(*it),
        _ => None,
    }
}

/// rustc's `try_eat_comma` and `try_eat_dollar`.
fn eat(iter: &mut Iter<'_>, punct: Punct) -> bool {
    match iter.peek() {
        Some(Tt::Token(it)) if it.is(punct) => {
            iter.next();
            true
        }
        _ => false,
    }
}

/// rustc's `parse_depth`: none, or an unsuffixed integer.
fn depth(iter: &mut Iter<'_>) -> Option<()> {
    match iter.next() {
        None => Some(()),
        Some(Tt::Token(it)) if it.kind == Kind::Literal(Lit::Int) => Some(()),
        Some(_) => None,
    }
}

/// rustc's `parse_sep_and_kleene_op`: a repetition's separator and operator, after `$(..)` at
/// `span`. Where there is none, an error, read as `*` at the place it is missing.
fn sep_and_kleene_op(iter: &mut Iter<'_>, span: TextRange) -> (Option<Token>, Kleene) {
    let missing = match kleene_op(iter, span) {
        Ok(Ok(kleene)) => return (None, kleene),
        Ok(Err(separator)) => match kleene_op(iter, separator.range) {
            // `?` takes no separator: an error.
            Ok(Ok(Kleene { op: Op::ZeroOrOne, range })) => {
                return (None, Kleene { op: Op::ZeroOrMore, range });
            }
            Ok(Ok(kleene)) => return (Some(separator), kleene),
            Ok(Err(Token { range, .. })) | Err(range) => range,
        },
        Err(range) => range,
    };
    (None, Kleene { op: Op::ZeroOrMore, range: missing })
}

/// rustc's `parse_kleene_op`: the next token tree, as an operator, a token that is not one, or
/// where it is when it is not a token.
fn kleene_op(iter: &mut Iter<'_>, span: TextRange) -> Result<Result<Kleene, Token>, TextRange> {
    match iter.next() {
        Some(&Tt::Token(token)) => {
            let op = match token.kind {
                Kind::Punct(Punct::Star) => Op::ZeroOrMore,
                Kind::Punct(Punct::Plus) => Op::OneOrMore,
                Kind::Punct(Punct::Question) => Op::ZeroOrOne,
                _ => return Ok(Err(token)),
            };
            Ok(Ok(Kleene { op, range: token.range }))
        }
        Some(tree) => Err(tree.range()),
        None => Err(span),
    }
}

#[cfg(test)]
mod tests {
    use syntax::{
        Parse, SourceFile,
        view::{self as ast, AstNode as _},
    };

    use super::*;

    fn rules(file: &Parse<SourceFile>, edition: Edition) -> (Tokens<'_>, Matchers, Vec<Rule>) {
        let def = file.root().descendants().find_map(ast::Macro::cast).unwrap();
        let tokens = Tokens::of(def, &|_| (edition, true)).unwrap();
        let (m, rules) = Matchers::rules(&tokens, matches!(def, ast::Macro::MacroRules(_)), edition);
        (tokens, m, rules)
    }

    fn get(m: &Matchers, ids: Ids) -> &[Mtt] {
        &m.tts[ids.start as usize..ids.end as usize]
    }

    fn lhs_tts(m: &Matchers, rule: &Rule) -> Vec<Mtt> {
        let Mtt::Delimited { tts, .. } = *m.at(rule.lhs.unwrap()) else { panic!() };
        get(m, tts).to_vec()
    }

    #[test]
    fn a_specifier_is_read_in_its_edition() {
        let file = SourceFile::parse("macro_rules! m { ($p:pat $e:expr) => {}; }", Edition::Edition2018);
        let (tokens, m, read) = rules(&file, Edition::Edition2018);
        let tts = lhs_tts(&m, &read[0]);
        let kinds: Vec<_> = tts
            .iter()
            .map(|it| match it {
                Mtt::MetaVarDecl { name, kind, range } => (tokens.name(name.range).to_owned(), *kind, tokens.text(*range).to_owned()),
                _ => panic!("{it:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("p".to_owned(), Frag::PatParam { inferred: true }, "$p:pat".to_owned()),
                ("e".to_owned(), Frag::Expr2021 { inferred: true }, "$e:expr".to_owned()),
            ]
        );
        let file = SourceFile::parse("macro_rules! m { ($p:pat) => {} }", Edition::Edition2021);
        let (_, m, rules) = rules(&file, Edition::Edition2021);
        assert!(matches!(lhs_tts(&m, &rules[0])[0], Mtt::MetaVarDecl { kind: Frag::PatWithOr, .. }));
    }

    #[test]
    fn a_repetition_has_its_separator_and_operator() {
        let file = SourceFile::parse("macro_rules! m { ($($x:tt),+ $(;)?) => { $($x)* } }", Edition::Edition2021);
        let (tokens, m, rules) = rules(&file, Edition::Edition2021);
        let tts = lhs_tts(&m, &rules[0]);
        let Mtt::Sequence { separator: Some(sep), kleene, .. } = tts[0] else { panic!("{:?}", tts[0]) };
        assert_eq!(tokens.text(sep.range), ",");
        assert_eq!(kleene.op, Op::OneOrMore);
        let Mtt::Sequence { separator: None, kleene, .. } = tts[1] else { panic!() };
        assert_eq!(kleene.op, Op::ZeroOrOne);
        let Mtt::Delimited { tts, .. } = *m.at(rules[0].rhs.unwrap()) else { panic!() };
        let Mtt::Sequence { tts, kleene, .. } = get(&m, tts)[0] else { panic!() };
        assert_eq!(kleene.op, Op::ZeroOrMore);
        assert!(matches!(get(&m, tts)[0], Mtt::MetaVar { .. }));
    }

    #[test]
    fn rules_stop_where_rustc_stops() {
        let file = SourceFile::parse("macro_rules! m { () => {}; (a) => {} (b) => {} }", Edition::Edition2021);
        let (_, _, all) = rules(&file, Edition::Edition2021);
        assert_eq!(all.len(), 2);
        let file = SourceFile::parse("macro_rules! m { () => {}; (a) }", Edition::Edition2021);
        let (_, _, all) = rules(&file, Edition::Edition2021);
        assert_eq!(all.len(), 2);
        assert!(all[1].lhs.is_some() && all[1].rhs.is_none());
        let file = SourceFile::parse("macro m { () => {}, (a) => {} }", Edition::Edition2021);
        let (_, _, all) = rules(&file, Edition::Edition2021);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn a_metavar_expression_names_its_variables() {
        let file = SourceFile::parse("macro_rules! m { ($($x:tt)*) => { ${count($x)} ${ignore($y)} ${index()} ${bogus($z)} } }", Edition::Edition2021);
        let (tokens, m, rules) = rules(&file, Edition::Edition2021);
        let Mtt::Delimited { tts, .. } = *m.at(rules[0].rhs.unwrap()) else { panic!() };
        let rhs = get(&m, tts);
        let names: Vec<Vec<&str>> = rhs
            .iter()
            .filter_map(|it| match it {
                Mtt::MetaVarExpr { vars, .. } => {
                    Some(m.vars[vars.start as usize..vars.end as usize].iter().map(|v| tokens.name(v.range)).collect())
                }
                _ => None,
            })
            .collect();
        assert_eq!(names, [vec!["x"], vec!["y"], vec![]]);
        assert!(matches!(rhs.last(), Some(Mtt::Token(t)) if t.is(Punct::Dollar)));
    }
}
