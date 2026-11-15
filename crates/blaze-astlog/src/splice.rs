//! A template written into Rust code so the code's tree reads it as the template's tree.
//!
//! Text splicing alone changes meaning and still parses: `$a * $b` with `$a` bound to `x + y`
//! writes `x + y * b`, and `$a + $b` written over `f(a, b)` in `f(a, b).len()` writes
//! `a + b.len()`. So the template is parsed with each metavariable as a hole, in the form of
//! the node it edits, and a bound expression is parenthesized where precedence
//! (`syntax::view::prec`) says its hole would read it otherwise; then the whole replacement
//! is, where the edited expression's parent would. A capture the edits inside it rewrote
//! (`rewrite::nest`) is judged as its rewritten text, reparsed, since that is what lands.
//!
//! Covered: holes in expression position, in a template that replaces an expression or
//! replaces or sits beside a statement or an item in a body. A metavariable spliced as a type,
//! a pattern, a path segment or any other non-expression is written as its text: no
//! precedence rule there is applied.

use std::alloc::Allocator;
use std::borrow::Cow;

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

/// The text a metavariable's capture reads as once the edits inside it are made, by
/// metavariable: what `render` splices in place of the capture's own text.
#[derive(Clone, Copy)]
pub(crate) struct Rewritten<'r>(pub &'r [(&'r str, String)]);

impl<'r> Rewritten<'r> {
    fn of(self, var: &str) -> Option<&'r str> {
        self.0.iter().find(|(it, _)| *it == var).map(|(_, text)| text.as_str())
    }
}

/// `template` rendered for a `kind` edit of `edited`: each capture as `rewritten` has it, or
/// else as written, and each bound expression parenthesized where its hole needs it, judged
/// on the text that lands there.
///
/// # Errors
///
/// A template that is not the form of what it edits once its metavariables are holes (the
/// message, for the caller to place), one that writes a hole's name itself, and a rewritten
/// expression capture that is no expression. Its scratch is in `alloc`.
#[expect(clippy::too_many_arguments, reason = "the template, its binding and its place, each its own input")]
pub(crate) fn render<'a, F: Files, A: Allocator>(
    facts: &Sema<'a, F>,
    template: &Template,
    binding: &Binding<Node>,
    rewritten: Rewritten<'_>,
    kind: TextKind,
    edited: ast::Node<'a>,
    parens: Parens,
    alloc: A,
) -> Result<String, String> {
    // Whether each metavariable use, in order, needs parentheses in its hole; none when the
    // template is written as plain text.
    let mut needs = Vec::new_in(alloc);
    let form = Form::of(kind, edited).filter(|_| parens == Parens::ByPrecedence);
    if let Some(form) = form {
        holes_needing_parens(facts, template, binding, rewritten, form, &mut needs)?;
    }
    let mut out = String::new();
    let mut at = 0;
    for segment in &template.segments {
        match segment {
            Segment::Lit(lit) => out.push_str(lit),
            Segment::Var(var) => {
                let text = match (rewritten.of(var), binding.get(var)) {
                    (Some(text), _) => Cow::Borrowed(text),
                    (None, Some(value)) => facts.value_text(value),
                    (None, None) => return Err(format!("`${var}` is bound by nothing in the rewrite's body")),
                };
                if needs.get(at) == Some(&true) {
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

/// Into `needs`, for each metavariable use of `template` in order, whether the expression it
/// splices needs parentheses in its hole.
fn holes_needing_parens<F: Files, A: Allocator>(
    facts: &Sema<'_, F>,
    template: &Template,
    binding: &Binding<Node>,
    rewritten: Rewritten<'_>,
    form: Form,
    needs: &mut Vec<bool, A>,
) -> Result<(), Cow<'static, str>> {
    let mut holed = String::new();
    let mut holes: Vec<&str> = Vec::new(); // alloc: one per metavariable use
    for segment in &template.segments {
        match segment {
            Segment::Lit(lit) if lit.contains(HOLE) => {
                return Err(format!("the template writes `{HOLE}`, the name of a metavariable's hole").into());
            }
            Segment::Lit(lit) => holed.push_str(lit),
            Segment::Var(var) => {
                holed.push_str(&format!("{HOLE}{}", holes.len()));
                holes.push(var);
            }
        }
    }
    if let Some(message) = crate::pattern::too_many_tokens(&holed) {
        return Err(format!("the template {message}").into());
    }
    let (before, after) = form.wrap();
    let source = format!("{before}{holed}{after}");
    let parse = SourceFile::parse(&source, Edition::CURRENT);
    if let Some(error) = parse.errors_with_bodies().first() {
        return Err(format!("the template is not {} with its metavariables as holes: {error}", form.name()).into());
    }
    let file = parse.root();
    let body = TextRange::at(TextSize::of(before), TextSize::of(holed.as_str()));
    if form == Form::Expr
        && !file.descendants().any(|it| it.text_range() == body && ast::Expr::cast(it).is_some())
    {
        return Err(format!("the template is not {}", form.name()).into());
    }
    needs.resize(holes.len(), false);
    for hole in file.descendants().filter(|it| it.kind() == SyntaxKind::PATH_EXPR) {
        // A path that only starts with a hole's name (`__astlog_hole_0::x`) is no hole.
        let Some(Ok(at)) = hole.text().strip_prefix(HOLE).map(str::parse::<usize>) else { continue };
        let (Some(parent), Some(var)) = (hole.parent(), holes.get(at)) else { continue };
        // The hole is the template's whole body: the replacement's own place decides.
        if hole.text_range() == body {
            continue;
        }
        let Some(Value::Node(value)) = binding.get(*var) else { continue };
        let Some(bound) = facts.view_of(*value).and_then(ast::Expr::cast) else { continue };
        // An expression the edits inside it rewrote is judged as the text that lands here.
        needs[at] = match rewritten.of(var) {
            None => bound.needs_parens_in_place_of(parent, hole),
            Some(text) => needs_parens(text, parent, hole)
                .ok_or_else(|| format!("`${var}`, rewritten as `{text}`, is no expression"))?,
        };
    }
    Ok(())
}

/// Whether the expression `text` needs parentheses in the place of `place_of`, a child of
/// `parent`; `None` when `text` is no expression.
fn needs_parens(text: &str, parent: ast::Node<'_>, place_of: ast::Node<'_>) -> Option<bool> {
    let (before, after) = Form::Expr.wrap();
    let source = format!("{before}{text}{after}");
    let parse = SourceFile::parse(&source, Edition::CURRENT);
    let body = TextRange::at(TextSize::of(before), TextSize::of(text));
    let file = parse.root();
    let expr = file.descendants().find(|it| it.text_range() == body).and_then(ast::Expr::cast)?;
    Some(expr.needs_parens_in_place_of(parent, place_of))
}

/// Whether `text`, replacing the expression `edited`, needs parentheses because `edited`'s
/// parent would read it otherwise.
///
/// # Errors
///
/// A replacement of an expression that is no expression.
pub(crate) fn in_place(kind: TextKind, edited: ast::Node<'_>, text: &str, parens: Parens) -> Result<bool, String> {
    if parens == Parens::Off || Form::of(kind, edited) != Some(Form::Expr) {
        return Ok(false);
    }
    let Some(parent) = edited.parent() else { return Ok(false) };
    needs_parens(text, parent, edited).ok_or_else(|| format!("`{text}` replaces an expression and is none"))
}
