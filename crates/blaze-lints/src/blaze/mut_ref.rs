//! `mut_ref` (in `blaze::edition`, allowed by default): in a crate that says `#![blaze::uniq]`
//! (docs/v2/plan/design/edition.md, change 10), a unique reference spelled with `mut`: `&mut T`
//! and `&'a mut T` in a type, `&mut expr` in an expression, `&mut self` and `&'a mut self` as a
//! receiver, and `ref mut x` in a pattern. Each is the same reference as its `uniq` spelling
//! from the syntax tree on (the parser reads `uniq` there as the `mut` token), so the fix,
//! machine-applicable, only respells it: `mut` becomes `uniq`.
//!
//! An operand whose first token would keep `uniq` a name of its own (`parser`'s
//! `grammar/uniq.rs`: `<`, `::` and `!` after it in a type, those and `.`, `{` and `#` in an
//! expression) is put in parentheses by the fix, as that rule says to write it:
//! `&mut <T>::new()` becomes `&uniq (<T>::new())`.
//!
//! Not found: `let mut x` and any `mut` binding without `ref`, `mut self`, `*mut T`,
//! `&raw mut place`, a `&mut pat` reference pattern (the gate has no `uniq` spelling of it), a
//! `mut` already spelled `uniq`, and everything a macro wrote or is handed (a macro's
//! definition is token trees, and an expansion is not the file's text to respell). In a crate
//! without the gate the lint says nothing: `uniq` is no keyword there.

use syntax::{
    SyntaxKind::{self, *},
    T, TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::Lint;
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::MutRef;

/// The first tokens of a type after which `uniq` stays a name: a path's `<` or `::`, a type
/// macro's `!`.
const TYPE_GOES_ON: &[SyntaxKind] = &[T![<], T![:], T![!]];

/// The first tokens of an expression after which `uniq` stays an expression of its own: a
/// path's `<` or `::`, a macro call's `!`, a field's, method's or range's `.`, a struct
/// literal's `{` and an attribute's `#`.
const EXPR_GOES_ON: &[SyntaxKind] = &[T![<], T![:], T![!], T![.], T!['{'], T![#]];

/// A unique reference spelled with `mut`.
struct Found<'t> {
    /// The `&` or `ref` the spelling starts at.
    first: ast::Token<'t>,
    /// Its `mut`.
    mut_: ast::Token<'t>,
    /// The operand after `mut` when its first token would keep `uniq` a name: the fix wraps it.
    wrap: Option<ast::Node<'t>>,
    /// What the finding calls it: "the reference type", "the borrow", ...
    what: &'static str,
    /// The spelling as written and as the gate spells it.
    written: &'static str,
    fixed: &'static str,
}

/// `node`, when it is a reference type, a borrow, a receiver or a `ref` binding spelled with
/// `mut`, in a crate that says `#![blaze::uniq]`.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) || !cx.in_file() || !cx.krate().syntax_gates(cx.db()).uniq() {
        return;
    }
    let Some(found) = found(node) else { return };
    let start = found.first.text_range().start();
    let span = cx.span(TextRange::new(start, found.mut_.text_range().end()));
    let mut parts = vec![(cx.span(found.mut_.text_range()), "uniq".to_owned())]; // alloc: a fix's parts
    if let Some(operand) = found.wrap {
        let range = operand.text_range();
        parts.push((cx.span(TextRange::empty(range.start())), "(".to_owned())); // alloc: as above
        parts.push((cx.span(TextRange::empty(range.end())), ")".to_owned())); // alloc: as above
    }
    let (what, written, fixed) = (found.what, found.written, found.fixed);
    let message = format!(
        "{what} in {} is spelled `{written}`, which `#![blaze::uniq]` spells `{fixed}`",
        item_of(node),
    );
    let diagnostic = cx.blaze(LINT, node, span, message);
    diagnostic.note(
        "`&uniq` and `ref uniq` are the same reference as `&mut` and `ref mut`, spelled for what \
         it guarantees: no other reference to the place is live while it is",
    );
    diagnostic.multipart_suggestion(
        format!("spell it `{fixed}`"),
        parts,
        Applicability::MachineApplicable,
    );
    diagnostic
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

fn found(node: ast::Node<'_>) -> Option<Found<'_>> {
    let spelled_mut = |token: ast::Token<'_>| token.text() == "mut";
    match node.kind() {
        REF_TYPE => {
            let it = ast::RefType::cast(node)?;
            let mut_ = it.mut_token().filter(|&it| spelled_mut(it))?;
            let ty = it.ty()?.syntax();
            let wrap = goes_on(mut_, TYPE_GOES_ON).then_some(ty);
            let first = it.amp_token()?;
            let (written, fixed) = ("&mut", "&uniq");
            Some(Found { first, mut_, wrap, what: "the reference type", written, fixed })
        }
        REF_EXPR => {
            let it = ast::RefExpr::cast(node)?;
            if it.raw_token().is_some() {
                return None;
            }
            let mut_ = it.mut_token().filter(|&it| spelled_mut(it))?;
            let expr = it.expr()?.syntax();
            let wrap = goes_on(mut_, EXPR_GOES_ON).then_some(expr);
            let first = it.amp_token()?;
            let (written, fixed) = ("&mut", "&uniq");
            Some(Found { first, mut_, wrap, what: "the borrow", written, fixed })
        }
        SELF_PARAM => {
            let it = ast::SelfParam::cast(node)?;
            let first = it.amp_token()?;
            let mut_ = it.mut_token().filter(|&it| spelled_mut(it))?;
            let (written, fixed) = ("&mut self", "&uniq self");
            Some(Found { first, mut_, wrap: None, what: "the receiver", written, fixed })
        }
        IDENT_PAT => {
            let it = ast::IdentPat::cast(node)?;
            let first = it.ref_token()?;
            let mut_ = it.mut_token().filter(|&it| spelled_mut(it))?;
            let (written, fixed) = ("ref mut", "ref uniq");
            Some(Found { first, mut_, wrap: None, what: "the binding", written, fixed })
        }
        _ => None,
    }
}

/// Whether the first token after `mut_` past trivia is one of `goes_on`, which would keep a
/// `uniq` there a name.
fn goes_on(mut_: ast::Token<'_>, goes_on: &[SyntaxKind]) -> bool {
    let mut next = mut_.next_token();
    while let Some(token) = next.filter(|it| it.kind().is_trivia()) {
        next = token.next_token();
    }
    next.is_some_and(|it| goes_on.contains(&it.kind()))
}

/// How a message names the item `node` is written in: `` `fn run` ``, `` `struct S` ``,
/// `` `impl` ``, or "this item".
fn item_of(node: ast::Node<'_>) -> String {
    let named = node.ancestors().find_map(ast::Item::cast).and_then(|item| match item {
        ast::Item::Fn(it) => it.name().map(|name| ("fn", name)),
        ast::Item::Struct(it) => it.name().map(|name| ("struct", name)),
        ast::Item::Enum(it) => it.name().map(|name| ("enum", name)),
        ast::Item::Union(it) => it.name().map(|name| ("union", name)),
        ast::Item::TypeAlias(it) => it.name().map(|name| ("type", name)),
        ast::Item::Const(it) => it.name().map(|name| ("const", name)),
        ast::Item::Static(it) => it.name().map(|name| ("static", name)),
        ast::Item::Trait(it) => it.name().map(|name| ("trait", name)),
        _ => None,
    });
    match named {
        // alloc: a finding's message part
        Some((kind, name)) => format!("`{kind} {}`", name.text()),
        None => "this item".to_owned(), // alloc: a finding's message part
    }
}
