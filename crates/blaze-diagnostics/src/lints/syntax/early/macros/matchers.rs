//! What rustc lints on a macro's definition, each read in the edition of its tokens' spans:
//!
//! - `meta_variable_misuse`: a variable of a rule's transcriber that its matcher does not bind,
//!   or uses at another depth or with another repetition operator; buffered at the definition.
//! - `rust_2021_incompatible_or_patterns`: a `pat` before 2021 that `|` may follow, once for
//!   each `|`; buffered at the crate.
//! - `edition_2024_expr_fragment_specifier`: an `expr` before 2024, of the early lint pass's
//!   `check_mac_def`.
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs` (`compile_declarative_macro`),
//! `compiler/rustc_expand/src/mbe/macro_check.rs`, `compiler/rustc_expand/src/mbe/quoted.rs`,
//! `compiler/rustc_lint/src/macro_expr_fragment_specifier_2024_migration.rs`, with the text of
//! `compiler/rustc_expand/src/diagnostics.rs` and `compiler/rustc_lint/src/lints.rs`
//! (`MacroExprFragment2024`), and `compiler/rustc_span/src/symbol.rs` (`Ident`'s `Display`)
//! (MIT or Apache-2.0).

mod check;
mod expr_2024;
mod follow;
mod quoted;
mod tokens;

use hir::{HirFileId, InFile};
use span::{Edition, SyntaxContext};
use syntax::{
    SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use self::{
    check::{Checker, Misuse},
    quoted::Matchers,
    tokens::{Token, Tokens},
};
use crate::{
    cx::Cx,
    lints::{
        registry::{LintId, LintSet},
        syntax::{Emitted, Pass, Tree, Walk, source_fact},
    },
    lower::Phase,
    model::{Applicability, Diag},
};

/// Where the lints buffered at the crate are placed: its root module's tree.
pub(super) fn crate_at(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Option<InFile<SyntaxNodePtr>> {
    cx.krate
        .root_module(cx.db)
        .as_source_file_id(cx.db)
        .and_then(|it| walk.tree(HirFileId::from(it)))
        .map(|it| it.ptr(it.root))
}

const META_VARIABLE_MISUSE: LintId = LintId::named("meta_variable_misuse");
const INCOMPATIBLE_OR_PATTERNS: LintId = LintId::named("rust_2021_incompatible_or_patterns");
const EXPR_FRAGMENT_SPECIFIER: LintId = LintId::named("edition_2024_expr_fragment_specifier");

/// The lints of this module, each allowed by default.
const LINTS: LintSet = LintSet::of(&[META_VARIABLE_MISUSE, INCOMPATIBLE_OR_PATTERNS, EXPR_FRAGMENT_SPECIFIER]);

/// The lints of the macro definition `node` (a `MACRO_RULES` or `MACRO_DEF` of `tree`), as the
/// early pass's one walk (`super::super::lints`) comes to it; `krate` is [`crate_at`]. A
/// definition is not read unless one of its lints may fire, and each check runs only when
/// its own lint may.
pub(super) fn definition(
    cx: &Cx<'_, '_>,
    krate: Option<InFile<SyntaxNodePtr>>,
    tree: &Tree<'_>,
    node: ast::Node<'_>,
    out: &mut Vec<Emitted>,
) {
    if !cx.any_may_fire(&LINTS) {
        return;
    }
    let (misuse, or_patterns, exprs) = (
        cx.lint_may_fire(META_VARIABLE_MISUSE),
        cx.lint_may_fire(INCOMPATIBLE_OR_PATTERNS),
        cx.lint_may_fire(EXPR_FRAGMENT_SPECIFIER),
    );
    let Some(def) = ast::Macro::cast(node) else { return };
    let at = tree.ptr(node);
    let map = tree.file.macro_file().map(|call| call.expansion_span_map(cx.db));
    let root = SyntaxContext::root(tree.file.edition(cx.db));
    let span = |offset: TextSize| {
        let ctx = map.map_or(root, |it| it.span_at(offset).ctx);
        (ctx.edition(cx.db), ctx.is_root())
    };
    let Some(tokens) = Tokens::of(def, &span) else { return };
    let (macro_rules, keyword) = match def {
        ast::Macro::MacroRules(it) => (true, it.macro_rules_token()),
        ast::Macro::MacroDef(it) => (false, it.macro_token()),
    };
    let start = keyword.map_or(node.text_range().start(), |it| it.text_range().start());
    let (m, rules) = Matchers::rules(&tokens, macro_rules, span(start).0);

    let mut checker = Checker::new(&m, &tokens);
    // alloc: empty until a lint fires; holds only what fires
    let mut found = Vec::new();
    for rule in &rules {
        if or_patterns {
            for lhs in rule.args.into_iter().chain(rule.lhs) {
                follow::check_lhs(&m, &tokens, lhs, &mut found);
            }
        }
        if misuse && let (Some(lhs), Some(rhs)) = (rule.lhs, rule.rhs) {
            checker.rule(rule.args, lhs, rhs);
        }
    }
    for misuse in &checker.misuses {
        let mut diag = misuse_diag(cx, tree, &tokens, misuse);
        source_fact(cx, &mut diag, at);
        out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at));
    }
    if let Some(krate) = krate {
        for it in found {
            let span = cx.span(InFile::new(tree.file, it.span));
            let mut diag = Diag::lint(
                "rust_2021_incompatible_or_patterns",
                "the meaning of the `pat` fragment specifier is changing in Rust 2021, which may affect this macro",
                // clone: a lint's span, for a lint found
                span.clone(),
            );
            diag.suggestion(
                span,
                "use pat_param to preserve semantics",
                // alloc: the lint's text, for a lint found
                format!("${}:pat_param", printed(&tokens, it.name)),
                Applicability::MachineApplicable,
            );
            source_fact(cx, &mut diag, krate);
            out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, krate));
        }
    }
    for range in exprs.then(|| expr_2024::exprs(&tokens)).into_iter().flatten() {
        let span = cx.span(InFile::new(tree.file, range));
        let mut diag = Diag::lint(
            "edition_2024_expr_fragment_specifier",
            "the `expr` fragment specifier will accept more expressions in the 2024 edition",
            // clone: a lint's span, for a lint found
            span.clone(),
        );
        diag.suggestion(
            span,
            "to keep the existing behavior, use the `expr_2021` fragment specifier",
            "expr_2021",
            Applicability::MachineApplicable,
        );
        source_fact(cx, &mut diag, at);
        out.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
    }
}

fn misuse_diag(cx: &Cx<'_, '_>, tree: &Tree<'_>, tokens: &Tokens<'_>, misuse: &Misuse) -> Diag {
    let span = |range: TextRange| cx.span(InFile::new(tree.file, range));
    match *misuse {
        Misuse::Duplicate { span: at, prev } => {
            let mut diag = Diag::lint("meta_variable_misuse", "duplicate matcher binding", span(at));
            diag.span_label(span(at), "duplicate binding");
            diag.span_label(span(prev), "previous binding");
            diag
        }
        Misuse::Unknown { span: at, name } => Diag::lint(
            "meta_variable_misuse",
            // alloc: the lint's text, for a lint found
            format!("unknown macro variable `{}`", printed(tokens, name)),
            span(at),
        ),
        Misuse::StillRepeating { span: at, binder, name } => {
            let mut diag = Diag::lint(
                "meta_variable_misuse",
                // alloc: the lint's text, for a lint found
                format!("variable `{}` is still repeating at this depth", printed(tokens, name)),
                span(at),
            );
            diag.span_label(span(binder), "expected repetition");
            diag
        }
        Misuse::WrongOperator { span: at, binder, occurrence } => {
            let mut diag =
                Diag::lint("meta_variable_misuse", "meta-variable repeats with different Kleene operator", span(at));
            diag.span_label(span(binder), "expected repetition");
            diag.span_label(span(occurrence), "conflicting repetition");
            diag
        }
    }
}

/// An identifier as rustc's `Ident` displays it: with `r#` when its name is reserved in its
/// span's edition and can be raw (`is_raw_guess`).
fn printed(tokens: &Tokens<'_>, ident: Token) -> String {
    let name = tokens.name(ident.range);
    let reserved = match name {
        "as" | "break" | "const" | "continue" | "else" | "enum" | "extern" | "false" | "fn" | "for" | "if"
        | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move" | "mut" | "pub" | "ref" | "return"
        | "static" | "struct" | "trait" | "true" | "type" | "unsafe" | "use" | "where" | "while"
        | "abstract" | "become" | "box" | "do" | "final" | "macro" | "override" | "priv" | "typeof"
        | "unsized" | "virtual" | "yield" => true,
        "async" | "await" | "dyn" | "try" => ident.edition >= Edition::Edition2018,
        "gen" => ident.edition >= Edition::Edition2024,
        _ => false,
    };
    match reserved {
        // alloc: the lint's text, for a lint found
        true => format!("r#{name}"),
        // alloc: the lint's text, for a lint found
        false => name.to_owned(),
    }
}
