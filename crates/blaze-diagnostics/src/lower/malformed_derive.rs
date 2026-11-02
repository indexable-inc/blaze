//! A `#[derive]` that is not a list: "malformed `derive` attribute input" with the form it
//! must have, or "wrong meta list delimiters" for one in `[]` or `{}`; and the errors of the
//! elements of a list, which hir passes over: E0777 for a literal, and a path with arguments
//! or a value.
//!
//! Follows rustc's `compiler/rustc_builtin_macros/src/derive.rs` (`Expander::expand`,
//! `report_unexpected_meta_item_lit`, `report_path_args`), `diagnostics.rs` (`BadDeriveLit`,
//! `DerivePathArgsList`, `DerivePathArgsValue`) and `compiler/rustc_attr_parsing/src/
//! validate_attr.rs` (`emit_malformed_attribute`, `check_meta_bad_delim`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{
    NodeOrToken, SyntaxKind, T, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MalformedDerive) -> Vec<Found> {
    let file = d.range.file_id;
    let root = cx.root(file);
    let Some(attr) = root.covering_element(d.range.value).ancestors().find_map(ast::Attr::cast) else {
        // alloc: the error's parts, for an error hir found
        return vec![malformed(cx.span(d.range), "")];
    };
    let inner = if attr.excl_token().is_some() { "!" } else { "" };
    let meta = attr
        .skip_cfg_attrs()
        .find(|it| it.simple_name() == Some("derive"))
        .map(|it| match it {
            ast::Meta::UnsafeMeta(unsafe_meta) => unsafe_meta.meta().unwrap_or(ast::Meta::UnsafeMeta(unsafe_meta)),
            it => it,
        });
    // alloc: the error's parts, for an error hir found
    let Some(meta) = meta else { return vec![malformed(cx.span(d.range), inner)] };
    // An attribute of a `cfg_attr` is spanned by its own meta item once rustc expands it.
    let span = match meta.syntax().parent().is_some_and(|it| ast::Attr::can_cast(it.kind())) {
        true => cx.span(InFile::new(file, attr.syntax().text_range())),
        false => cx.span(InFile::new(file, meta.syntax().text_range())),
    };
    if let ast::Meta::TokenTreeMeta(list) = meta
        && let Some(tt) = list.token_tree()
        && tt.l_paren_token().is_none()
        && let (Some(open), Some(close)) = (tt.left_delimiter_token(), tt.right_delimiter_token())
    {
        let mut diag = Diag::error(
            None,
            "wrong meta list delimiters",
            cx.span(InFile::new(file, tt.syntax().text_range())),
        );
        diag.multipart_suggestion(
            "the delimiters should be `(` and `)`",
            // alloc: the error's parts, for an error hir found
            vec![
                // alloc: the error's text, for an error hir found
                (cx.span(InFile::new(file, open.text_range())), "(".to_owned()),
                // alloc: the error's text, for an error hir found
                (cx.span(InFile::new(file, close.text_range())), ")".to_owned()),
            ],
            Applicability::MachineApplicable,
        );
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Expand)];
    }
    // alloc: the error's parts, for an error hir found
    vec![malformed(span, inner)]
}

/// rustc's `emit_malformed_attribute` of `derive`, whose one form is a list.
fn malformed(span: Span, inner: &str) -> Found {
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(None, "malformed `derive` attribute input", span.clone());
    diag.suggestions(
        span,
        "must be of the form",
        // alloc: the error's text, for an error hir found
        [format!("#{inner}[derive(Trait1, Trait2, ...)]")],
        Applicability::HasPlaceholders,
        SuggestionStyle::ShowAlways,
    );
    Found::new(diag, Phase::Expand)
}

/// The errors rustc gives of the elements of the list `tt` of a `#[derive(...)]` that is in
/// effect: a literal (E0777), and a path with arguments or a value. hir drops such an element
/// and reports nothing, so the attribute checks call this for each derive of an item.
pub(crate) fn elements(cx: &Cx<'_, '_>, tt: InFile<ast::TokenTree<'_>>) -> Vec<Found> {
    let file = tt.file_id;
    let span = |range| cx.span(InFile::new(file, range));
    // alloc: empty, allocates only when an element is in error
    let mut found = Vec::new();
    // One pass over the children between the delimiters (the first and the last child), split at
    // the commas, trivia skipped.
    let mut children = tt.value.syntax().children_with_tokens().peekable();
    children.next();
    let mut element = Element::default();
    while let Some(child) = children.next() {
        let last = children.peek().is_none();
        match child {
            _ if last => {}
            NodeOrToken::Token(token) if token.kind() == T![,] => {}
            NodeOrToken::Token(token) if token.kind().is_trivia() => continue,
            it => {
                element.push(it);
                continue;
            }
        }
        if let Some(error) = std::mem::take(&mut element).error(&span) {
            found.push(error);
        }
    }
    found
}

/// One element of a derive list, as it is read: what its errors need of it.
#[derive(Default)]
struct Element<'t> {
    first: Option<ast::Element<'t>>,
    /// The end of the path the element starts with: its first part and the path parts after it.
    path_end: TextSize,
    /// The first part after that path.
    after: Option<ast::Element<'t>>,
    end: TextSize,
}

impl<'t> Element<'t> {
    fn push(&mut self, part: ast::Element<'t>) {
        let range = part.text_range();
        self.end = range.end();
        if self.first.is_none() {
            self.path_end = range.end();
            self.first = Some(part);
        } else if self.after.is_none() {
            if is_path_part(part) {
                self.path_end = range.end();
            } else {
                self.after = Some(part);
            }
        }
    }

    /// rustc's error of the element: a literal (E0777), or a path with arguments or a value.
    fn error(self, span: &impl Fn(TextRange) -> Span) -> Option<Found> {
        if let Some(NodeOrToken::Token(lit)) = self.first
            && is_literal(lit.kind())
        {
            let at = span(lit.text_range());
            // clone: on the error path
            let mut diag = Diag::error(Some("E0777"), "expected path to a trait, found literal", at.clone());
            diag.span_label(at, "not a trait");
            match cooked_ident(lit.kind(), lit.text()) {
                // alloc: the error's text, on the error path
                Some(name) => diag.help(format!("try using `#[derive({name})]`")),
                None => diag.help("for example, write `#[derive(Debug)]` for `Debug`"),
            };
            return Some(Found::new(diag, Phase::Expand));
        }
        let (message, help) = match self.after? {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::TOKEN_TREE => {
                ("traits in `#[derive(...)]` don't accept arguments", "remove the arguments")
            }
            NodeOrToken::Token(token) if token.kind() == T![=] => {
                ("traits in `#[derive(...)]` don't accept values", "remove the value")
            }
            _ => return None,
        };
        let at = span(TextRange::new(self.path_end, self.end));
        // clone: on the error path
        let mut diag = Diag::error(None, message, at.clone());
        diag.suggestion(at, help, "", Applicability::MachineApplicable);
        Some(Found::new(diag, Phase::Expand))
    }
}

fn is_literal(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, T![true] | T![false])
}

fn is_path_part(element: ast::Element<'_>) -> bool {
    match element {
        NodeOrToken::Token(token) => {
            matches!(token.kind(), SyntaxKind::IDENT | T![::] | T![self] | T![super] | T![crate] | T![Self])
        }
        NodeOrToken::Node(_) => false,
    }
}

/// The name in a cooked string literal when it is an identifier (`rustc_lexer::is_ident`), as
/// rustc's `BadDeriveLitHelp::StrLit` names it.
fn cooked_ident(kind: SyntaxKind, text: &str) -> Option<&str> {
    if kind != SyntaxKind::STRING {
        return None;
    }
    let name = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    let starts = first == '_' || unicode_ident::is_xid_start(first);
    (starts && chars.all(unicode_ident::is_xid_continue)).then_some(name)
}
