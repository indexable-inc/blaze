//! `keyword_idents_2018` and `keyword_idents_2024` (the group `keyword_idents`): an identifier
//! that a later edition makes a keyword. rustc's pre-expansion pass gives them, on each file as
//! parsed, before `cfg` removes anything and before any macro is expanded: every identifier
//! its AST visitor comes to (`check_ident`), and every token of a macro call's arguments and of
//! a macro definition's body (`check_mac`, `check_mac_def`), but none of an attribute's
//! delimited arguments, which the visitor does not walk.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`KeywordIdents`: `check_tokens`,
//! `check_ident_token`), `compiler/rustc_lint/src/lints.rs` (`BuiltinKeywordIdents`),
//! `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedPreExpansionLintPass`, the
//! `keyword_idents` group), `compiler/rustc_ast/src/visit.rs` (what the visitor walks of an
//! attribute) and `compiler/rustc_interface/src/passes.rs` (`pre_expansion_lint`, of the crate
//! root and of each module file `rustc_expand/src/expand.rs` loads) (MIT or Apache-2.0).

use hir::InFile;
use span::Edition;
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange, TextSize, WalkEvent,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::{
        registry::LintId,
        syntax::{Emitted, Pass, Tree, source_fact},
    },
    lower::Phase,
    model::{Applicability, Diag},
};

const KEYWORD_IDENTS_2018: LintId = LintId::named("keyword_idents_2018");
const KEYWORD_IDENTS_2024: LintId = LintId::named("keyword_idents_2024");

/// The lints of `tree`, a file of the crate, into `out`. Both are allowed by default: a file
/// whose edition leaves no lint that may fire is not walked.
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, tree: &Tree<'db>, out: &mut Vec<Emitted>) {
    let edition = tree.file.edition(cx.db);
    let may_fire = match edition {
        _ if edition.at_least_2024() => false,
        Edition::Edition2015 => cx.lint_may_fire(KEYWORD_IDENTS_2018) || cx.lint_may_fire(KEYWORD_IDENTS_2024),
        _ => cx.lint_may_fire(KEYWORD_IDENTS_2024),
    };
    if !may_fire {
        return;
    }
    let mut keywords = Keywords { cx, tree, edition, out };
    keywords.walk(tree.root);
}

struct Keywords<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    tree: &'c Tree<'db>,
    edition: Edition,
    out: &'c mut Vec<Emitted>,
}

/// Whether an identifier is a token of a macro's (`UnderMacro`): `dyn` is linted only outside.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnderMacro {
    No,
    Yes,
}

impl Keywords<'_, '_, '_> {
    /// The identifiers of the AST `node`, as rustc's visitor comes to them.
    fn walk(&mut self, node: ast::Node<'_>) {
        let mut events = node.preorder_with_tokens();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(element) = event else { continue };
            match element {
                NodeOrToken::Node(node) => match node.kind() {
                    SyntaxKind::ATTR => {
                        if let Some(meta) = ast::Attr::cast(node).and_then(|it| it.meta()) {
                            self.meta(meta);
                        }
                        events.skip_subtree();
                    }
                    SyntaxKind::TOKEN_TREE => {
                        if let Some(owner) = node.parent()
                            && matches!(owner.kind(), SyntaxKind::MACRO_CALL | SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF)
                        {
                            self.tokens(node, owner);
                        }
                        events.skip_subtree();
                    }
                    _ => {}
                },
                NodeOrToken::Token(token) => self.token(token, UnderMacro::No, None),
            }
        }
    }

    /// What the visitor walks of an attribute: its path, and the expression of `path = expr`.
    fn meta(&mut self, meta: ast::Meta<'_>) {
        match meta {
            ast::Meta::PathMeta(it) => self.walk_opt(it.path().map(|it| it.syntax())),
            ast::Meta::TokenTreeMeta(it) => self.walk_opt(it.path().map(|it| it.syntax())),
            ast::Meta::KeyValueMeta(it) => {
                self.walk_opt(it.path().map(|it| it.syntax()));
                self.walk_opt(it.expr().map(|it| it.syntax()));
            }
            ast::Meta::UnsafeMeta(it) => {
                if let Some(inner) = it.meta() {
                    self.meta(inner);
                }
            }
            ast::Meta::CfgMeta(_) | ast::Meta::CfgAttrMeta(_) => {}
        }
    }

    fn walk_opt(&mut self, node: Option<ast::Node<'_>>) {
        if let Some(node) = node {
            self.walk(node);
        }
    }

    /// `check_tokens` of the token tree `tt` of the macro call or definition `owner`: each
    /// identifier and lifetime not after a `$`.
    fn tokens(&mut self, tt: ast::Node<'_>, owner: ast::Node<'_>) {
        let mut prev_dollar = false;
        for token in tt.descendants_with_tokens().filter_map(|it| it.into_token()) {
            if token.kind().is_trivia() {
                continue;
            }
            if token.kind() == SyntaxKind::DOLLAR {
                prev_dollar = true;
                continue;
            }
            if !prev_dollar {
                self.token(token, UnderMacro::Yes, Some(owner));
            }
            prev_dollar = false;
        }
    }

    /// `check_ident_token` of `token` if it is an identifier or a lifetime, the lint decided
    /// at `at`, or at the token's parent when `None`.
    fn token(&mut self, token: ast::Token<'_>, under_macro: UnderMacro, at: Option<ast::Node<'_>>) {
        let kind = token.kind();
        if kind != SyntaxKind::IDENT && kind != SyntaxKind::LIFETIME_IDENT {
            return;
        }
        let text = token.text();
        let range = token.text_range();
        let (name, range, prefix) = match kind {
            SyntaxKind::IDENT if !text.starts_with("r#") => (text, range, ""),
            SyntaxKind::IDENT => return,
            _ => {
                let Some(name) = text.strip_prefix('\'') else { return };
                match name.strip_prefix("r#") {
                    // Before 2021 rustc lexes `'r#name` as `'r`, `#` and the identifier `name`.
                    Some(rest) if !self.edition.at_least_2021() => {
                        (rest, TextRange::new(range.start() + TextSize::from(3), range.end()), "")
                    }
                    Some(_) => return,
                    None => (name, range, "'"),
                }
            }
        };
        let Some(keyword) = keyword(name, under_macro) else { return };
        if self.edition >= keyword.0 {
            return;
        }
        let Some(at) = at.or_else(|| token.parent()) else { return };
        self.check(name, range, prefix, keyword, at);
    }

    /// `check_ident_token` of the identifier `name` at `range` (a lifetime's when `prefix` is
    /// `'`, its span the lifetime's), which a later edition makes the keyword `keyword`.
    fn check(&mut self, name: &str, range: TextRange, prefix: &str, keyword: Keyword, at: ast::Node<'_>) {
        let (_, lint, year) = keyword;
        let cx = self.cx;
        let span = cx.span(InFile::new(self.tree.file, range));
        // alloc: the diagnostic's text and span, as is all of `check`
        let mut diag = Diag::lint(lint, format!("`{name}` is a keyword in the {year} edition"), span.clone());
        diag.suggestion(
            span,
            "you can use a raw identifier to stay compatible",
            // alloc: as above
            format!("{prefix}r#{name}"),
            Applicability::MachineApplicable,
        );
        let at: InFile<SyntaxNodePtr> = self.tree.ptr(at);
        source_fact(cx, &mut diag, at);
        self.out.push(Emitted::new(diag, Phase::Resolve, Pass::PreExpansion, at));
    }
}

/// The edition that makes an identifier a keyword, the lint of it, and that edition's year.
type Keyword = (Edition, &'static str, &'static str);

/// The keyword a later edition makes of `name`: a match on its bytes, which compares lengths
/// before bytes, so most identifiers cost one length compare.
fn keyword(name: &str, under_macro: UnderMacro) -> Option<Keyword> {
    const K2018: Keyword = (Edition::Edition2018, "keyword_idents_2018", "2018");
    const K2024: Keyword = (Edition::Edition2024, "keyword_idents_2024", "2024");
    match name.as_bytes() {
        b"async" | b"await" | b"try" => Some(K2018),
        b"dyn" if under_macro == UnderMacro::No => Some(K2018),
        b"gen" => Some(K2024),
        _ => None,
    }
}
