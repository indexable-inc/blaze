//! A macro call that failed to expand, as rustc says it: `compile_error!` (rustc's
//! `rustc_builtin_macros/src/compile_error.rs`), a `macro_rules!` that no rule matches
//! (`rustc_expand/src/mbe/diagnostics.rs`'s `failed_to_match_macro`), a proc macro's panic
//! (`rustc_expand/src/proc_macro.rs`), the recursion limit (`rustc_expand/src/expand.rs`'s
//! `error_recursion_limit_reached`) and `#[derive(Default)]` on an enum with no `#[default]`
//! (`rustc_builtin_macros/src/deriving/default.rs`) (MIT or Apache-2.0). A builtin macro's or
//! derive's own error (`builtin-macro-error`, `builtin-derive-error`) is rustc's already, with
//! its code, spans, notes and helps, and is given as it is.
//!
//! hir gives the error as rendered text and the range of the call or of the token it failed at;
//! the kind is told from that text. rustc's "while trying to match" note and its missing-comma
//! help need the matcher's own account of the failure, which hir does not keep.

use hir::{BuiltinChild, InFile};
use hir_expand::RenderedExpandError;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, Node, Token},
};

use super::unresolved_macro_call::{Kind, head_span};
use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// What mbe's matcher says when no rule matches (`mbe::ExpandErrorKind`'s `Display`), apart
/// from its binding errors, which all start with `expected `.
const MATCH_FAILURES: &[&str] = &["no rule matches input tokens", "unexpected token in input", "leftover tokens"];

/// rustc's glued punctuation, longest first, as `Token::glue` builds it.
const GLUED: &[&str] = &[
    "<<=", ">>=", "...", "..=", "::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=",
    "*=", "/=", "%=", "^=", "&=", "|=", "<<", ">>", "..", "<-",
];

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MacroError) -> Vec<Found> {
    let file = d.range.file_id;
    let root = cx.root(file);
    let call = Call::at(cx, root, d.range.value);
    let at = |range| cx.span(InFile::new(file, range));
    let diag = if d.kind == "E0665" {
        no_default_variant(cx, file, root, d.range.value)
    } else if matches!(d.kind, RenderedExpandError::BUILTIN | RenderedExpandError::BUILTIN_DERIVE) {
        builtin(cx, d)
    } else if d.message == "macro definition has parse errors" {
        // alloc: empty, allocates nothing
        return Vec::new();
    } else if let Some(message) = d.message.strip_prefix("proc-macro panicked: ") {
        let what = match call.as_ref().map(|it| it.kind) {
            Some(Kind::Derive) => "proc-macro derive panicked",
            Some(Kind::Attr) => "custom attribute panicked",
            Some(Kind::Bang) | None => "proc macro panicked",
        };
        let range = call.as_ref().map_or(d.range.value, |it| it.range);
        let mut diag = Diag::error(None, what, at(range));
        // alloc: the error's text, for an error hir found
        diag.help(format!("message: {message}"));
        diag
    } else if d.message == "overflow expanding the original macro" {
        recursion_limit(cx, file, call.as_ref(), d.range.value)
    } else if let Some(call) = call.as_ref().filter(|it| it.is_builtin(cx, "compile_error")) {
        compile_error(cx, file, call, &d.message)
    } else if let Some(call) = call.as_ref().filter(|it| it.is_macro_rules(cx) && is_match_failure(&d.message)) {
        no_rules(cx, file, call, d.range.value)
    } else {
        let message = match &call {
            // alloc: the error's text, for an error hir found
            Some(call) => format!("blaze can't expand {} `{}`: {}", call.kind.descr(), call.name, d.message),
            // alloc: the error's text, for an error hir found
            None => format!("blaze can't expand this macro: {}", d.message),
        };
        Diag::error(None, message, at(call.as_ref().map_or(d.range.value, |it| it.range)))
    };
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Expand)]
}

/// An error a builtin macro or derive gives in `rustc_builtin_macros`' own words: the message as
/// it is, its code, its other primary spans and its notes and helps in rustc's order.
fn builtin(cx: &Cx<'_, '_>, d: &hir::MacroError) -> Diag {
    // alloc: the error's code, for an error hir found
    let code = d.code.map(|it| format!("E{it:04}"));
    // clone: the error's text, for an error hir found
    let mut diag = Diag::error(code.as_deref(), d.message.clone(), cx.span(d.range));
    for &at in &d.also_at {
        diag.primary(cx.span(at));
    }
    for (child, text) in &d.children {
        match child {
            BuiltinChild::Note => diag.note(&**text),
            BuiltinChild::Help => diag.help(&**text),
        };
    }
    diag
}

/// The call a macro error is about.
struct Call<'db> {
    kind: Kind,
    /// rustc's call-site span: the path to the closing delimiter of a function-like call, the
    /// attribute of an attribute macro, the path of a derive.
    range: TextRange,
    name: &'db str,
    macro_: Option<hir::Macro>,
    args: Option<ast::TokenTree<'db>>,
}

impl<'db> Call<'db> {
    fn at(cx: &Cx<'_, 'db>, root: Node<'db>, range: TextRange) -> Option<Call<'db>> {
        let node = root.covering_element(range).ancestors().next()?;
        if let Some(call) = node.ancestors().find_map(ast::MacroCall::cast) {
            let path = call.path()?;
            let args = call.token_tree();
            let end = args.map_or(path.syntax().text_range().end(), |it| it.syntax().text_range().end());
            let name = path.segment()?.name_ref()?.text();
            return Some(Call {
                kind: Kind::Bang,
                range: TextRange::new(path.syntax().text_range().start(), end),
                name,
                macro_: cx.vsema.resolve_macro_call(call),
                args,
            });
        }
        let attr = node.ancestors().find_map(ast::Attr::cast)?;
        let path = attr.path()?;
        let path_text = path.syntax().text();
        if path_text == "derive" {
            let name = &root.text()[range];
            return Some(Call { kind: Kind::Derive, range, name, macro_: None, args: None });
        }
        Some(Call { kind: Kind::Attr, range: attr.syntax().text_range(), name: path_text, macro_: None, args: None })
    }

    fn is_builtin(&self, cx: &Cx<'_, '_>, name: &str) -> bool {
        self.macro_.is_some_and(|it| {
            it.kind(cx.db) == hir::MacroKind::DeclarativeBuiltIn && it.name(cx.db).as_str() == name
        })
    }

    fn is_macro_rules(&self, cx: &Cx<'_, '_>) -> bool {
        self.macro_.is_some_and(|it| it.kind(cx.db) == hir::MacroKind::Declarative)
    }

    /// rustc's `ExpnKind::descr` of the call.
    fn descr(&self) -> String {
        match self.kind {
            Kind::Bang => format!("{}!", self.name), // alloc: the error's text, for an error hir found
            Kind::Attr => format!("#[{}]", self.name), // alloc: the error's text, for an error hir found
            Kind::Derive => format!("#[derive({})]", self.name), // alloc: the error's text, for an error hir found
        }
    }
}

fn is_match_failure(message: &str) -> bool {
    MATCH_FAILURES.contains(&message) || message.starts_with("expected ")
}

/// The trees between a call's delimiters, without trivia.
fn arg_tokens(args: ast::TokenTree<'_>) -> Vec<Token<'_>> {
    let (open, close) = (args.left_delimiter_token(), args.right_delimiter_token());
    args.syntax()
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|it| !it.kind().is_trivia() && Some(*it) != open && Some(*it) != close)
        // alloc: the error's parts, for an error hir found
        .collect()
}

/// rustc's `failed_to_match_macro`, from the token mbe's matcher stopped at.
fn no_rules(cx: &Cx<'_, '_>, file: hir::HirFileId, call: &Call<'_>, at: TextRange) -> Diag {
    let span = |range| cx.span(InFile::new(file, range));
    let tokens = call.args.map(arg_tokens).unwrap_or_default();
    let close = call.args.and_then(|it| it.right_delimiter_token());
    let failed = tokens.iter().find(|it| it.text_range().contains_range(at) && !at.is_empty()).copied();
    let at_end = close.is_some_and(|it| it.text_range() == at || (at.is_empty() && at.start() == it.text_range().start()));
    let mut diag = if let Some(token) = failed {
        let (range, text) = glued(token);
        // alloc: the error's text, for an error hir found
        let message = format!("no rules expected {}", describe(token, &text, cx.edition));
        let mut diag = Diag::error(None, message, span(range));
        diag.span_label(span(range), "no rules expected this token in macro call");
        diag
    } else if at_end {
        let range = tokens.last().map_or(call.range, |it| TextRange::empty(it.text_range().end()));
        let mut diag = Diag::error(None, "unexpected end of macro invocation", span(range));
        diag.span_label(span(range), "missing tokens in macro arguments");
        diag
    } else {
        // alloc: the error's text, for an error hir found
        Diag::error(None, format!("no rules of `{}!` match this call", call.name), span(call.range))
    };
    if let Some(macro_) = call.macro_.filter(|it| it.module(cx.db).krate(cx.db) == cx.krate)
        && let Some(head) = head_span(cx, macro_)
    {
        diag.span_label(head, "when calling this macro");
    }
    diag
}

/// rustc's `expand_compile_error`.
fn compile_error(cx: &Cx<'_, '_>, file: hir::HirFileId, call: &Call<'_>, message: &str) -> Diag {
    let span = |range| cx.span(InFile::new(file, range));
    if message != "`compile_error!` argument must be a string" {
        return Diag::error(None, message, span(call.range));
    }
    let mut tokens = call.args.map(arg_tokens).unwrap_or_default();
    if tokens.last().is_some_and(|it| it.kind() == SyntaxKind::COMMA) {
        tokens.pop();
    }
    match (tokens.first(), tokens.last()) {
        (Some(first), Some(last)) => {
            let range = TextRange::new(first.text_range().start(), last.text_range().end());
            Diag::error(None, "argument must be a string literal", span(range))
        }
        _ => Diag::error(None, "compile_error! takes 1 argument", span(call.range)),
    }
}

/// rustc's `RecursionLimitReached`.
fn recursion_limit(cx: &Cx<'_, '_>, file: hir::HirFileId, call: Option<&Call<'_>>, range: TextRange) -> Diag {
    let limit = hir_def::nameres::crate_def_map(cx.db, cx.krate.into()).recursion_limit();
    let suggested = if limit == 0 { 2 } else { limit * 2 };
    let crate_name = match cx.krate.display_name(cx.db) {
        Some(name) => name.crate_name().to_string(), // alloc: the error's text, for an error hir found
        None => {
            let path = cx.file_name(cx.krate.root_file(cx.db));
            let stem = path.rsplit('/').next().unwrap_or(&path);
            stem.strip_suffix(".rs").unwrap_or(stem).replace('-', "_")
        }
    };
    let (descr, range) = match call {
        Some(call) => (call.descr(), call.range),
        None => (String::from("this macro"), range), // alloc: the error's text, for an error hir found
    };
    // alloc: the error's text, for an error hir found
    let mut diag = Diag::error(None, format!("recursion limit reached while expanding `{descr}`"), cx.span(InFile::new(file, range)));
    // alloc: the error's text, for an error hir found
    diag.help(format!(
        "consider increasing the recursion limit by adding a `#![recursion_limit = \"{suggested}\"]` attribute to your crate (`{crate_name}`)"
    ));
    diag
}

/// rustc's `NoDefaultVariant` (E0665), at the `Default` of the derive.
fn no_default_variant(cx: &Cx<'_, '_>, file: hir::HirFileId, root: Node<'_>, range: TextRange) -> Diag {
    let span = |range| cx.span(InFile::new(file, range));
    let mut diag = Diag::error(Some("E0665"), "`#[derive(Default)]` on enum with no `#[default]`", span(range));
    let enum_ = root.covering_element(range).ancestors().find_map(ast::Enum::cast);
    let Some(enum_) = enum_ else { return diag };
    diag.span_label(span(item_range(enum_.syntax())), "this enum needs a unit variant marked with `#[default]`");
    for variant in enum_.variant_list().into_iter().flat_map(|it| it.variants()) {
        let non_exhaustive = variant
            .attrs()
            .any(|it| it.path().is_some_and(|path| path.syntax().text() == "non_exhaustive"));
        if variant.field_list().is_some() || non_exhaustive {
            continue;
        }
        diag.suggestion(
            span(TextRange::empty(item_range(variant.syntax()).start())),
            "make this unit variant default by placing `#[default]` on it",
            "#[default] ",
            Applicability::MaybeIncorrect,
        );
    }
    diag
}

/// The range and text of the token starting at `token` as rustc's lexer glues punctuation.
fn glued(token: Token<'_>) -> (TextRange, String) {
    if !token.kind().is_punct() {
        // alloc: the error's text, for an error hir found
        return (token.text_range(), token.text().to_owned());
    }
    // alloc: the error's text, for an error hir found
    let mut text = token.text().to_owned();
    let mut next = token.next_token();
    while let Some(it) = next.filter(|it| it.kind().is_punct() && text.len() < 3) {
        text.push_str(it.text());
        next = it.next_token();
    }
    let glued = GLUED.iter().find(|op| text.starts_with(**op) && op.len() > token.text().len());
    match glued {
        // alloc: the error's text, for an error hir found
        Some(op) => (TextRange::at(token.text_range().start(), syntax::TextSize::of(*op)), (*op).to_owned()),
        // alloc: the error's text, for an error hir found
        None => (token.text_range(), token.text().to_owned()),
    }
}

const USED_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while",
];
const UNUSED_KEYWORDS: &[&str] = &[
    "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized",
    "virtual", "yield",
];

/// rustc's `token_descr` of a token whose text is `text`.
fn describe(token: Token<'_>, text: &str, edition: span::Edition) -> String {
    let is_doc = matches!(token.kind(), SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT);
    let used = USED_KEYWORDS.contains(&text)
        || (edition.at_least_2018() && matches!(text, "async" | "await" | "dyn"));
    let unused = UNUSED_KEYWORDS.contains(&text)
        || (edition.at_least_2018() && text == "try")
        || (edition.at_least_2024() && text == "gen");
    if is_doc {
        format!("doc comment `{text}`") // alloc: the error's text, on the error path
    } else if text == "_" {
        format!("reserved identifier `{text}`") // alloc: the error's text, on the error path
    } else if used {
        format!("keyword `{text}`") // alloc: the error's text, on the error path
    } else if unused {
        format!("reserved keyword `{text}`") // alloc: the error's text, on the error path
    } else {
        format!("`{text}`") // alloc: the error's text, on the error path
    }
}

/// rustc's `token_descr` of `token`.
pub(super) fn token_descr(token: Token<'_>, edition: span::Edition) -> String {
    describe(token, token.text(), edition)
}
