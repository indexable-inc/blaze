//! A template written into Rust code so the code's tree reads it as the template's tree.
//!
//! Text splicing alone changes meaning and still parses: `$a * $b` with `$a` bound to `x + y`
//! writes `x + y * b`, and `$a + $b` written over `f(a, b)` in `f(a, b).len()` writes
//! `a + b.len()`. So the template is parsed with each metavariable as a hole, in the form of
//! the node it edits, and a bound expression is parenthesized where precedence
//! (`syntax::view::prec`) says its hole would read it otherwise; then the whole replacement
//! is, where the edited expression's parent would.
//!
//! Covered: holes in expression position, in a template that replaces an expression or
//! replaces or sits beside a statement or an item in a body. A metavariable spliced as a type,
//! a pattern, a path segment or any other non-expression is written as its text: no
//! precedence rule there is applied.

use astlog_engine::{Binding, Facts as _, Segment, Template, TextKind, Value};
use blaze_intent::Files;
use syntax::{
    Edition, SourceFile, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::sema::{Node, Sema};

/// A hole's name: an identifier no template writes (one that does is refused).
const HOLE: &str = "__astlog_hole_";

/// Whether templates are parenthesized by precedence. `Off` writes the plain splice, the
/// control that shows precedence is what keeps a rewrite's meaning.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Parens {
    ByPrecedence,
    Off,
}

/// The form a template is parsed as, from the node it edits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
    /// It replaces an expression: it must be one.
    Expr,
    /// It replaces or sits beside a statement or an item in a body: statements.
    Stmts,
}

impl Form {
    fn of(kind: TextKind, edited: ast::Node<'_>) -> Option<Form> {
        let stmt = ast::Stmt::cast(edited).is_some()
            && edited.parent().is_some_and(|it| it.kind() == SyntaxKind::STMT_LIST);
        match kind {
            TextKind::Replace if ast::Expr::cast(edited).is_some() => Some(Form::Expr),
            TextKind::Replace | TextKind::InsertBefore | TextKind::InsertAfter if stmt => Some(Form::Stmts),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Form::Expr => "an expression",
            Form::Stmts => "statements",
        }
    }

    fn wrap(self) -> (&'static str, &'static str) {
        match self {
            Form::Expr => ("const __ASTLOG: () = ", ";\n"),
            Form::Stmts => ("fn __astlog() { ", "\n}\n"),
        }
    }
}

/// `template` rendered for a `kind` edit of `edited`: each bound expression parenthesized
/// where its hole needs it.
///
/// # Errors
///
/// A template that is not the form of what it edits once its metavariables are holes (the
/// message, for the caller to place), or one that writes a hole's name itself.
pub(crate) fn render<'a, F: Files>(
    facts: &Sema<'a, F>,
    template: &Template,
    binding: &Binding<Node>,
    kind: TextKind,
    edited: ast::Node<'a>,
    parens: Parens,
) -> Result<String, String> {
    let plain = || -> Result<String, String> {
        astlog_engine::render(facts, template, binding).map_err(|error| error.to_string())
    };
    let Some(form) = Form::of(kind, edited) else { return plain() };
    if parens == Parens::Off {
        return plain();
    }
    let mut holed = String::new();
    let mut holes: Vec<&str> = Vec::new(); // alloc: one per metavariable use
    for segment in &template.segments {
        match segment {
            Segment::Lit(lit) if lit.contains(HOLE) => {
                return Err(format!("the template writes `{HOLE}`, the name of a metavariable's hole"));
            }
            Segment::Lit(lit) => holed.push_str(lit),
            Segment::Var(var) => {
                holed.push_str(&format!("{HOLE}{}", holes.len()));
                holes.push(var);
            }
        }
    }
    if let Some(message) = crate::pattern::too_many_tokens(&holed) {
        return Err(format!("the template {message}"));
    }
    let (before, after) = form.wrap();
    let source = format!("{before}{holed}{after}");
    let parse = SourceFile::parse(&source, Edition::CURRENT);
    if let Some(error) = parse.errors_with_bodies().first() {
        return Err(format!("the template is not {} with its metavariables as holes: {error}", form.name()));
    }
    let file = parse.root();
    let body = TextRange::at(TextSize::of(before), TextSize::of(holed.as_str()));
    if form == Form::Expr
        && !file.descendants().any(|it| it.text_range() == body && ast::Expr::cast(it).is_some())
    {
        return Err(format!("the template is not {}", form.name()));
    }
    let mut needs = vec![false; holes.len()];
    for hole in file.descendants().filter(|it| it.kind() == SyntaxKind::PATH_EXPR) {
        // A path that only starts with a hole's name (`__astlog_hole_0::x`) is no hole.
        let Some(Ok(at)) = hole.text().strip_prefix(HOLE).map(str::parse::<usize>) else { continue };
        let (Some(parent), Some(var)) = (hole.parent(), holes.get(at)) else { continue };
        // The hole is the template's whole body: the replacement's own place decides.
        if hole.text_range() == body {
            continue;
        }
        if let Some(Value::Node(value)) = binding.get(*var)
            && let Some(bound) = facts.view_of(*value).and_then(ast::Expr::cast)
        {
            needs[at] = bound.needs_parens_in_place_of(parent, hole);
        }
    }
    let mut out = String::new();
    let mut at = 0;
    for segment in &template.segments {
        match segment {
            Segment::Lit(lit) => out.push_str(lit),
            Segment::Var(var) => {
                let Some(value) = binding.get(var) else {
                    return Err(format!("`${var}` is bound by nothing in the rewrite's body"));
                };
                let text = facts.value_text(value);
                if needs[at] {
                    out.push('(');
                    out.push_str(&text);
                    out.push(')');
                } else {
                    out.push_str(&text);
                }
                at += 1;
            }
        }
    }
    Ok(out)
}

/// `text` as it replaces the expression `edited`: parenthesized when `edited`'s parent would
/// read it otherwise.
///
/// # Errors
///
/// A replacement of an expression that is no expression.
pub(crate) fn in_place(kind: TextKind, edited: ast::Node<'_>, text: String, parens: Parens) -> Result<String, String> {
    if parens == Parens::Off || Form::of(kind, edited) != Some(Form::Expr) {
        return Ok(text);
    }
    let Some(parent) = edited.parent() else { return Ok(text) };
    let (before, after) = Form::Expr.wrap();
    let source = format!("{before}{text}{after}");
    let parse = SourceFile::parse(&source, Edition::CURRENT);
    let body = TextRange::at(TextSize::of(before), TextSize::of(text.as_str()));
    let file = parse.root();
    let Some(replacement) =
        file.descendants().find(|it| it.text_range() == body).and_then(ast::Expr::cast)
    else {
        return Err(format!("`{text}` replaces an expression and is none"));
    };
    if replacement.needs_parens_in_place_of(parent, edited) {
        return Ok(format!("({text})"));
    }
    Ok(text)
}
