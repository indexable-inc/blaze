//! A `macro_rules!` rustc refuses to define: the checks of rustc's `compile_declarative_macro`
//! (`rustc_expand/src/mbe/macro_rules.rs`), the matcher parsing of `rustc_expand/src/mbe/quoted.rs`
//! and the binder check of `rustc_expand/src/mbe/macro_check.rs` (MIT or Apache-2.0).
//!
//! rustc builds an `mbe::TokenTree` (a `Vec` per delimited group), then walks it again for each
//! check and keeps binders in a hash map. Here one pass reads the token trees in place in the
//! syntax tree and does every check as it goes: a definition rustc accepts costs one walk of its
//! tokens with no allocation and no hashing (binders sit in an inline `SmallVec`, compared by
//! text, and a rule rarely has more than eight).
//!
//! The checks follow-set ambiguity (`check_matcher`), meta-variable expressions (`${...}`), the
//! `macro_metavar_expr`, `macro_attr` and `macro_derive` feature gates and the
//! `meta_variable_misuse` lint are not ported yet; a definition mbe refuses for one of those is
//! still an error naming what mbe said.

use hir::InFile;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, ChildrenWithTokens, Element, Token},
};

use super::macro_error::token_descr;
use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// rustc's `VALID_FRAGMENT_NAMES_MSG`.
const VALID_FRAGMENT_NAMES: &str = "valid fragment specifiers are `ident`, `block`, `stmt`, `expr`, `pat`, `ty`, `lifetime`, `literal`, `path`, `meta`, `tt`, `item` and `vis`, along with `expr_2021` and `pat_param` for edition compatibility";

/// A definition mbe refuses: nothing when [`check`] has rustc's error of it (`checks::macros`
/// gives it, with those of the definitions mbe accepts), else an error naming what mbe said.
pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MacroDefError) -> Vec<Found> {
    let node = cx.node_view(d.node);
    if !check(cx, node).is_empty() {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let range = d.name.unwrap_or_else(|| item_range(node.value.syntax()));
    let span = cx.span(InFile::new(d.node.file_id, range));
    // alloc: the error's text, for an error hir found
    let message = format!("blaze can't define this macro: {}", d.message);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(Diag::error(None, message, span), Phase::Expand)]
}

/// rustc's errors on the definition of `mac`, in the order rustc gives them. Empty, and nothing
/// allocated, when rustc defines it.
pub(crate) fn check<'db>(cx: &Cx<'_, 'db>, mac: InFile<ast::Macro<'db>>) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let ast::Macro::MacroRules(rules) = mac.value else { return Vec::new() };
    // alloc: empty, allocates nothing
    let Some(body) = rules.token_tree() else { return Vec::new() };
    // alloc: empty, allocates only when the definition is in error
    let mut checker = Checker { cx, file: mac.file_id, errors: Vec::new(), binders: SmallVec::new() };
    checker.definition(body, item_range(rules.syntax()));
    if checker.errors.is_empty() {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: the errors found, on the error path
    checker.errors.into_iter().map(|it| Found::new(it, Phase::Expand)).collect()
}

/// A token tree of rustc's `tokenstream`: a token, or a delimited group.
#[derive(Clone, Copy)]
enum Tree<'db> {
    Token(Token<'db>),
    Group(ast::TokenTree<'db>),
}

impl<'db> Tree<'db> {
    fn range(&self) -> TextRange {
        match self {
            Tree::Token(it) => it.text_range(),
            Tree::Group(it) => it.syntax().text_range(),
        }
    }

    fn token(&self) -> Option<Token<'db>> {
        match *self {
            Tree::Token(it) => Some(it),
            Tree::Group(_) => None,
        }
    }

    fn kind(&self) -> Option<SyntaxKind> {
        self.token().map(Token::kind)
    }

    /// A contextual keyword (`attr`, `derive`), which the lexer gives as an identifier.
    fn is_ident(&self, text: &str) -> bool {
        self.token().is_some_and(|it| it.kind() == SyntaxKind::IDENT && it.text() == text)
    }
}

fn tree(element: Element<'_>) -> Option<Tree<'_>> {
    match element {
        NodeOrToken::Node(node) => ast::TokenTree::cast(node).map(Tree::Group),
        NodeOrToken::Token(token) => match token.kind() {
            SyntaxKind::WHITESPACE | SyntaxKind::COMMENT => None,
            kind if kind.is_delimiter() => None,
            _ => Some(Tree::Token(token)),
        },
    }
}

fn is_doc(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT)
}

/// The trees between a group's delimiters, read in place: whitespace and plain comments skipped,
/// doc comments kept (they are tokens to rustc). A direct token child that is a delimiter is the
/// group's own; nested ones are inside child groups.
struct Trees<'db> {
    children: std::iter::Peekable<std::iter::FilterMap<ChildrenWithTokens<'db>, fn(Element<'db>) -> Option<Tree<'db>>>>,
    /// Where the last tree taken ends: rustc's `prev_token.span.hi()`.
    end: TextSize,
}

impl<'db> Trees<'db> {
    fn of(group: ast::TokenTree<'db>) -> Trees<'db> {
        let end = group.left_delimiter_token().map_or(group.syntax().text_range().start(), |it| it.text_range().end());
        let tree: fn(Element<'db>) -> Option<Tree<'db>> = tree;
        Trees { children: group.syntax().children_with_tokens().filter_map(tree).peekable(), end }
    }

    fn next(&mut self) -> Option<Tree<'db>> {
        let tree = self.children.next()?;
        self.end = tree.range().end();
        Some(tree)
    }

    fn peek(&mut self) -> Option<&Tree<'db>> {
        self.children.peek()
    }

    fn peek_kind(&mut self) -> Option<SyntaxKind> {
        self.peek().and_then(Tree::kind)
    }

    /// The range of a `=>` next. A token tree holds single-character punctuation, so this is
    /// rustc's glue of a `=` written right before a `>`.
    fn peek_fat_arrow(&mut self) -> Option<TextRange> {
        let eq = self.peek()?.token()?;
        if eq.kind() == SyntaxKind::FAT_ARROW {
            return Some(eq.text_range());
        }
        if eq.kind() != SyntaxKind::EQ {
            return None;
        }
        let NodeOrToken::Token(gt) = eq.next_sibling_or_token()? else { return None };
        (gt.kind() == SyntaxKind::R_ANGLE).then(|| TextRange::new(eq.text_range().start(), gt.text_range().end()))
    }

    /// Takes a `=>` ([`Self::peek_fat_arrow`]); `false`, taking nothing, when none is next.
    fn eat_fat_arrow(&mut self) -> bool {
        let Some(arrow) = self.peek_fat_arrow() else { return false };
        while self.children.next_if(|it| it.range().end() <= arrow.end()).is_some() {}
        self.end = arrow.end();
        true
    }
}

fn delimiter(group: ast::TokenTree<'_>) -> Option<SyntaxKind> {
    group.left_delimiter_token().map(|it| it.kind())
}

/// rustc's `token.is_ident()`: an identifier, a keyword or `_`.
fn is_ident(token: Token<'_>) -> bool {
    token.kind().is_any_identifier() || token.kind() == SyntaxKind::UNDERSCORE
}

/// rustc's `NonterminalKind::from_symbol`: the same names in every edition.
fn is_fragment(name: &str) -> bool {
    matches!(
        name,
        "item" | "block" | "stmt" | "pat" | "pat_param" | "expr" | "expr_2021" | "ty" | "ident" | "lifetime"
            | "literal" | "meta" | "path" | "vis" | "tt"
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kleene {
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

impl Kleene {
    fn of(kind: Option<SyntaxKind>) -> Option<Kleene> {
        match kind? {
            SyntaxKind::STAR => Some(Kleene::ZeroOrMore),
            SyntaxKind::PLUS => Some(Kleene::OneOrMore),
            SyntaxKind::QUESTION => Some(Kleene::ZeroOrOne),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Pattern,
    Body,
}

/// What rustc's `is_empty_token_tree` needs of one parsed tree of a repetition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Elem {
    Token,
    Doc,
    Group,
    Var,
    Decl { vis: bool },
    Sequence(Kleene),
}

impl Elem {
    fn matches_empty(self) -> bool {
        matches!(
            self,
            Elem::Doc | Elem::Decl { vis: true } | Elem::Sequence(Kleene::ZeroOrMore | Kleene::ZeroOrOne)
        )
    }
}

/// The first repetition of a matcher that can match nothing (rustc's `check_lhs_no_empty_seq`
/// walks in pre-order, which is source order), and what its `vis` suggestion needs.
#[derive(Clone, Copy)]
struct EmptySequence {
    range: TextRange,
    vis_op: Option<TextRange>,
}

struct Checker<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    file: hir::HirFileId,
    errors: Vec<Diag>,
    /// The metavariables bound by the rule being read: the name and the declaration's range.
    binders: SmallVec<[(Token<'db>, TextRange); 8]>,
}

impl<'db> Checker<'_, '_, 'db> {
    fn span(&self, range: TextRange) -> Span {
        self.cx.span(InFile::new(self.file, range))
    }

    fn error(&mut self, range: TextRange, message: impl Into<String>) -> &mut Diag {
        let span = self.span(range);
        self.errors.push(Diag::error(None, message, span));
        self.errors.last_mut().expect("just pushed")
    }

    /// rustc's `compile_declarative_macro` over a `macro_rules!` body.
    fn definition(&mut self, body: ast::TokenTree<'db>, item: TextRange) {
        let mut trees = Trees::of(body);
        let mut rules = 0u32;
        while trees.peek().is_some() {
            self.binders.clear();
            let unsafe_rule = trees.peek_kind() == Some(SyntaxKind::UNSAFE_KW);
            let mut unsafe_range = TextRange::default();
            if unsafe_rule {
                unsafe_range = trees.next().expect("peeked").range();
                if self.no_eof(&mut trees, "expected `attr`") {
                    return;
                }
            }
            if trees.peek().is_some_and(|it| it.is_ident("attr")) {
                trees.next();
                if self.no_eof(&mut trees, "expected macro attr args") {
                    return;
                }
                let args = trees.next().expect("checked");
                self.args_parens("attr", &args);
                self.lhs(&args);
                if self.no_eof(&mut trees, "expected macro attr body") {
                    return;
                }
            } else if trees.peek().is_some_and(|it| it.is_ident("derive")) {
                let derive_range = trees.next().expect("peeked").range();
                if unsafe_rule {
                    self.error(unsafe_range, "`unsafe` is only supported on `attr` rules");
                }
                if self.no_eof(&mut trees, "expected `()` after `derive`") {
                    return;
                }
                let args = trees.next().expect("checked");
                self.args_parens("derive", &args);
                let args_empty = matches!(args, Tree::Group(it) if Trees::of(it).peek().is_none());
                if !args_empty {
                    let msg = "`derive` rules do not accept arguments; `derive` must be followed by `()`";
                    self.error(args.range(), msg);
                }
                if self.no_eof(&mut trees, "expected macro derive body") {
                    return;
                }
                if let Some(arrow) = trees.peek_fat_arrow() {
                    let derive = self.span(derive_range);
                    let diag = self.error(arrow, "expected macro derive body, got `=>`");
                    if !args_empty {
                        diag.span_label(derive, "need `()` after this `derive`");
                    }
                    return;
                }
            } else if unsafe_rule {
                self.error(unsafe_range, "`unsafe` is only supported on `attr` rules");
            }
            let lhs = trees.next().expect("checked");
            self.lhs(&lhs);
            if !trees.eat_fat_arrow() {
                self.expected(&mut trees, "=>");
                return;
            }
            if self.no_eof(&mut trees, "expected right-hand side of macro rule") {
                return;
            }
            let rhs = trees.next().expect("checked");
            self.parse_one(&rhs, Part::Body);
            if let Tree::Token(token) = &rhs {
                self.error(token.text_range(), "macro rhs must be delimited");
            }
            self.duplicate_binders();
            rules += 1;
            if trees.peek().is_none() {
                break;
            }
            if trees.peek_kind() != Some(SyntaxKind::SEMICOLON) {
                self.expected(&mut trees, ";");
                return;
            }
            trees.next();
        }
        if rules == 0 {
            self.error(item, "macros must contain at least one rule");
        }
    }

    /// rustc's `check_no_eof`.
    fn no_eof(&mut self, trees: &mut Trees<'db>, label: &str) -> bool {
        if trees.peek().is_some() {
            return false;
        }
        let at = TextRange::empty(trees.end);
        let span = self.span(at);
        self.error(at, "macro definition ended unexpectedly").span_label(span, label);
        true
    }

    /// rustc's `Parser::expect` of `token` failing at the next tree.
    fn expected(&mut self, trees: &mut Trees<'db>, token: &str) {
        let prev_end = trees.end;
        // alloc: the error's text, on the error path
        let label = format!("expected `{token}`");
        let Some(found) = trees.peek() else {
            let at = TextRange::empty(prev_end);
            let span = self.span(at);
            // alloc: the error's text, on the error path
            self.error(at, format!("expected `{token}`, found end of macro arguments")).span_label(span, label);
            return;
        };
        let (at, found_descr) = match *found {
            Tree::Token(it) => (it.text_range(), token_descr(it, self.cx.edition)),
            Tree::Group(it) => match it.left_delimiter_token() {
                // alloc: the error's text, on the error path
                Some(open) => (open.text_range(), format!("`{}`", open.text())),
                // alloc: the error's text, on the error path
                None => (it.syntax().text_range(), format!("`{}`", it.syntax().text())),
            },
        };
        // alloc: the error's text, on the error path
        let message = format!("expected `{token}`, found {found_descr}");
        let span = self.span(at);
        let root = self.cx.root(self.file);
        let on_next_line = prev_end < at.start() && root.text()[TextRange::new(prev_end, at.start())].contains('\n');
        if token == ";" && on_next_line {
            let prev = self.span(TextRange::empty(prev_end));
            self.error(at, message).span_label(prev, label).span_label(span, "unexpected token");
        } else {
            self.error(at, message).span_label(span, label);
        }
    }

    /// rustc's `check_args_parens`.
    fn args_parens(&mut self, rule_kw: &str, args: &Tree<'db>) {
        let Tree::Group(group) = *args else { return };
        if delimiter(group) == Some(SyntaxKind::L_PAREN) {
            return;
        }
        let (Some(open), Some(close)) = (group.left_delimiter_token(), group.right_delimiter_token()) else { return };
        // alloc: the error's parts, on the error path
        let parts = vec![(self.span(open.text_range()), "(".to_owned()), (self.span(close.text_range()), ")".to_owned())];
        // alloc: the error's text, on the error path
        self.error(group.syntax().text_range(), format!("`{rule_kw}` rule argument matchers require parentheses"))
            .multipart_suggestion("the delimiters should be `(` and `)`", parts, Applicability::MachineApplicable);
    }

    /// rustc's `parse_one_tt` of a matcher, then `check_lhs`: delimited, and no repetition that
    /// matches nothing.
    fn lhs(&mut self, lhs: &Tree<'db>) {
        let mut empty = None;
        match *lhs {
            Tree::Group(group) => {
                self.parse(&mut Trees::of(group), Part::Pattern, &mut empty);
            }
            Tree::Token(token) => {
                self.error(token.text_range(), "invalid macro matcher; matchers must be contained in balanced delimiters");
            }
        }
        let Some(EmptySequence { range, vis_op }) = empty else { return };
        let diag = self.error(range, "repetition matches empty token tree");
        let Some(op) = vis_op else { return };
        diag.note("a `vis` fragment can already be empty");
        let open = TextRange::new(range.start() - TextSize::of('$'), range.start() + TextSize::of('('));
        let close = TextRange::new(range.end() - TextSize::of(')'), op.end());
        // alloc: the error's parts, on the error path
        let parts = vec![(self.span(open), String::new()), (self.span(close), String::new())];
        self.errors
            .last_mut()
            .expect("just pushed")
            .multipart_suggestion("remove the `$(` and `)?`", parts, Applicability::MaybeIncorrect);
    }

    /// `check_meta_variables`' `check_binders`: a name bound twice in the matchers of one rule.
    /// Quadratic in the binders of one rule, which are few.
    fn duplicate_binders(&mut self) {
        for i in 1..self.binders.len() {
            let name = unraw(self.binders[i].0.text());
            let Some(prev) = self.binders[..i].iter().find(|(it, _)| unraw(it.text()) == name).map(|it| it.1) else {
                continue;
            };
            let range = self.binders[i].1;
            let (span, prev) = (self.span(range), self.span(prev));
            self.error(range, "duplicate matcher binding")
                .span_label(span, "duplicate binding")
                .span_label(prev, "previous binding");
        }
    }

    /// rustc's `parse_one_tt` of a rule's body.
    fn parse_one(&mut self, tree: &Tree<'db>, part: Part) {
        if let Tree::Group(group) = *tree {
            self.parse(&mut Trees::of(group), part, &mut None);
        }
    }

    /// rustc's `quoted::parse`: whether every tree matches nothing, and whether the first is a
    /// `vis` declaration.
    fn parse(&mut self, trees: &mut Trees<'db>, part: Part, empty: &mut Option<EmptySequence>) -> (bool, bool) {
        let (mut all_empty, mut first_vis, mut first) = (true, false, true);
        while let Some(tree) = trees.next() {
            let elem = match self.parse_tree(tree, trees, part, empty) {
                (Elem::Var, Some((start, name))) if part == Part::Pattern => self.declaration(start, name, trees),
                (elem, _) => elem,
            };
            all_empty &= elem.matches_empty();
            if first {
                first_vis = elem == Elem::Decl { vis: true };
                first = false;
            }
        }
        (all_empty, first_vis)
    }

    /// The fragment specifier after a metavariable `$name` of a matcher, starting at `start`.
    fn declaration(&mut self, start: TextRange, name: Option<Token<'db>>, trees: &mut Trees<'db>) -> Elem {
        let mut name = name;
        let mut record = |this: &mut Self, range: TextRange| {
            if let Some(name) = name.take() {
                this.binders.push((name, range));
            }
        };
        if trees.peek_kind() != Some(SyntaxKind::COLON) {
            record(self, start);
            self.missing_fragment(start, TextRange::empty(start.end()));
            return Elem::Decl { vis: false };
        }
        let colon = trees.next().expect("peeked").range();
        let with_colon = TextRange::new(start.start(), colon.end());
        let Some(Tree::Token(token)) = trees.next() else {
            record(self, with_colon);
            self.missing_fragment(with_colon, TextRange::empty(colon.end()));
            return Elem::Decl { vis: false };
        };
        if !is_ident(token) {
            record(self, token.text_range());
            if token.kind() == SyntaxKind::DOLLAR && trees.peek().and_then(Tree::token).is_some_and(is_ident) {
                let span = self.span(token.text_range());
                self.error(token.text_range(), "missing fragment specifier")
                    .note("fragment specifiers must be provided")
                    .help(VALID_FRAGMENT_NAMES)
                    .suggestion_verbose(span, "fragment specifiers should not be prefixed with `$`", "", Applicability::MaybeIncorrect);
            } else {
                self.missing_fragment(token.text_range(), TextRange::empty(token.text_range().end()));
            }
            return Elem::Decl { vis: false };
        }
        let range = TextRange::new(start.start(), token.text_range().end());
        record(self, range);
        let fragment = unraw(token.text());
        if is_fragment(fragment) {
            return Elem::Decl { vis: fragment == "vis" };
        }
        // alloc: the error's text, on the error path
        self.error(range, format!("invalid fragment specifier `{}`", token.text())).help(VALID_FRAGMENT_NAMES);
        Elem::Decl { vis: false }
    }

    /// rustc's `MissingFragmentSpecifier`.
    fn missing_fragment(&mut self, range: TextRange, add: TextRange) {
        let add = self.span(add);
        self.error(range, "missing fragment specifier")
            .note("fragment specifiers must be provided")
            .help(VALID_FRAGMENT_NAMES)
            .suggestion_verbose(add, "try adding a specifier here", ":spec", Applicability::MaybeIncorrect);
    }

    /// rustc's `quoted::parse_tree`. A metavariable comes back with its range and name (none when
    /// rustc gives it a dummy name after an error).
    fn parse_tree(
        &mut self,
        tree: Tree<'db>,
        trees: &mut Trees<'db>,
        part: Part,
        empty: &mut Option<EmptySequence>,
    ) -> (Elem, Option<(TextRange, Option<Token<'db>>)>) {
        let dollar = match tree {
            Tree::Token(it) if it.kind() == SyntaxKind::DOLLAR => it.text_range(),
            Tree::Token(it) if is_doc(it.kind()) => return (Elem::Doc, None),
            Tree::Token(_) => return (Elem::Token, None),
            Tree::Group(group) => {
                self.parse(&mut Trees::of(group), part, empty);
                return (Elem::Group, None);
            }
        };
        let Some(next) = trees.next() else { return (Elem::Token, None) };
        match next {
            Tree::Group(group) => {
                let entire = group.syntax().text_range();
                match (part, delimiter(group)) {
                    (Part::Pattern, Some(SyntaxKind::L_CURLY)) => self.dollar_in_lhs(entire, "{"),
                    (Part::Pattern, Some(SyntaxKind::L_BRACK)) => self.dollar_in_lhs(entire, "["),
                    // A meta-variable expression, whose parse is not ported: mbe checks it.
                    (Part::Body, Some(SyntaxKind::L_CURLY)) => return (Elem::Token, None),
                    (Part::Body, Some(SyntaxKind::L_BRACK)) => {
                        self.error(entire, "expected `(` or `{`, found `[`");
                    }
                    _ => {}
                }
                let (contents_empty, first_vis) = self.parse(&mut Trees::of(group), part, empty);
                let (separator, op, op_range) = self.separator_and_kleene(trees, entire);
                if part == Part::Pattern
                    && !separator
                    && contents_empty
                    && empty.is_none_or(|it| entire.start() < it.range.start())
                {
                    let vis_op = (op == Kleene::ZeroOrOne && first_vis).then_some(op_range);
                    *empty = Some(EmptySequence { range: entire, vis_op });
                }
                (Elem::Sequence(op), None)
            }
            Tree::Token(token) if is_ident(token) => {
                let range = TextRange::new(dollar.start(), token.text_range().end());
                if token.kind() == SyntaxKind::CRATE_KW {
                    (Elem::Token, None)
                } else {
                    (Elem::Var, Some((range, Some(token))))
                }
            }
            Tree::Token(token) if token.kind() == SyntaxKind::DOLLAR => {
                if part == Part::Pattern {
                    self.dollar_in_lhs(token.text_range(), "$");
                }
                (Elem::Token, None)
            }
            Tree::Token(token) => {
                // alloc: the error's text, on the error path
                self.error(token.text_range(), format!("expected identifier, found `{}`", token.text()));
                (Elem::Var, Some((token.text_range(), None)))
            }
        }
    }

    /// rustc's `span_dollar_dollar_or_metavar_in_the_lhs_err`.
    fn dollar_in_lhs(&mut self, range: TextRange, token: &str) {
        let span = self.span(range);
        // alloc: the error's text, on the error path
        self.error(range, format!("unexpected token: {token}")).span_note(
            span,
            "`$$` and meta-variable expressions are not allowed inside macro parameter definitions",
        );
    }

    /// rustc's `parse_sep_and_kleene_op`: whether there is a separator, the operator and its range.
    fn separator_and_kleene(&mut self, trees: &mut Trees<'db>, span: TextRange) -> (bool, Kleene, TextRange) {
        let first = trees.next();
        let at = match first {
            Some(tree) => match (Kleene::of(tree.kind()), tree) {
                (Some(op), tree) => return (false, op, tree.range()),
                (None, Tree::Token(separator)) => match trees.next() {
                    Some(tree) => match Kleene::of(tree.kind()) {
                        Some(Kleene::ZeroOrOne) => {
                            let msg = "the `?` macro repetition operator does not take a separator";
                            self.error(separator.text_range(), msg);
                            return (false, Kleene::ZeroOrMore, tree.range());
                        }
                        Some(op) => return (true, op, tree.range()),
                        None => tree.range(),
                    },
                    None => separator.text_range(),
                },
                (None, group) => group.range(),
            },
            None => span,
        };
        self.error(at, "expected one of: `*`, `+`, or `?`");
        (false, Kleene::ZeroOrMore, at)
    }
}

fn unraw(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}
