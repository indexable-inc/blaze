//! `StaticMutRefs`: `static_mut_refs`, a shared or mutable reference made to a `static mut`
//! (or a field of one): a borrow `&X` / `&mut X.f`, a method whose receiver is a reference
//! called on one, or `let ref x = X`.
//!
//! Follows rustc's `compiler/rustc_lint/src/static_mut_refs.rs` (`check_expr`, `check_stmt`,
//! `path_is_static_mut`, `emit_static_mut_refs`, `interior_mutability_suggestion`,
//! `static_mutability_span`) and `compiler/rustc_lint/src/lints.rs` (`RefOfMutStatic`,
//! `MutRefSugg`, `StaticMutRefsInteriorMutabilitySugg`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::{
    HasModule as _, Lookup as _, ModuleDefId, StaticId,
    hir::{
        BindingAnnotation, Expr, ExprId, Pat, Statement,
        type_ref::{Mutability, Rawness},
    },
    resolver::ValueNs,
    signatures::{StaticFlags, StaticSignature},
    src::HasSource as _,
};
use hir_ty::{db::HirDatabase as _, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self, AstNode as _, HasName as _},
};

use super::{expr_span, from_expansion};
use crate::{
    lints::bodies::{
        BodyCx, ExprKinds, ExprKind, Hooks, LatePass, LintSet, lint_set,
        types::util::{Solver, method_fn, path_value},
    },
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

const SHARED_NOTE: &str = "shared references to mutable statics are dangerous; it's undefined behavior if the static is mutated or if a mutable reference is created for it while the shared reference lives";
const MUT_NOTE: &str = "mutable references to mutable statics are dangerous; it's undefined behavior if any other pointer to the static is used or if any other reference is created for the static while the mutable reference lives";
const INTERIOR_MUTABILITY_HELP: &str = "use a type that relies on \"interior mutability\" instead; to read more on this, visit <https://doc.rust-lang.org/reference/interior-mutability.html>";
const INTERIOR_MUTABILITY_SUGG: &str = "this type already provides \"interior mutability\", so its binding doesn't need to be declared as mutable when borrowed with a shared reference";

#[derive(Default)]
pub(crate) struct StaticMutRefs;

impl LatePass for StaticMutRefs {
    const LINTS: LintSet = lint_set(["static_mut_refs"]);
    const HOOKS: Hooks = Hooks::STMT;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Ref, ExprKind::MethodCall]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        match b.store().expr(expr) {
            Expr::Ref { expr: inner, rawness: Rawness::Ref, mutability } => {
                let mutable = mutability == Mutability::Mut;
                let Some(def) = static_mut(b, inner) else { return };
                // A borrow made by lowering (`format_args!`'s of its arguments) is in rustc a
                // borrow in the macro's expansion, linted where what it borrows is.
                let Some(at) = b.expr_node(expr).or_else(|| b.expr_node(inner)) else { return };
                let Some(inner_span) = expr_span(b, inner) else { return };
                let outer = expr_span(b, expr).filter(|it| !from_expansion(it));
                let (span, sugg) = match outer {
                    Some(outer) => {
                        let sugg = addr_of_span(b, &outer, &inner_span);
                        (outer, Some(sugg))
                    }
                    None => (inner_span, None),
                };
                emit(b, at, span, sugg, mutable, def, out);
            }
            Expr::MethodCall { receiver, .. } => {
                let Some(def) = static_mut(b, receiver) else { return };
                let Some(method) = method_fn(b, expr) else { return };
                let sig = b.db().callable_item_signature(method.into());
                let inputs = sig.skip_binder().inputs().skip_binder();
                let Some(self_ty) = inputs.first() else { return };
                let TyKind::Ref(_, _, mutability) = self_ty.kind() else { return };
                let mutable = mutability == rustc_type_ir::Mutability::Mut;
                let (Some(at), Some(outer)) = (b.expr_node(expr), expr_span(b, expr)) else { return };
                let span = match from_expansion(&outer) {
                    true => match expr_span(b, receiver) {
                        Some(it) => it,
                        None => return,
                    },
                    false => outer,
                };
                emit(b, at, span, None, mutable, def, out);
            }
            _ => {}
        }
    }

    fn check_stmt(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
        let Expr::Block { statements, .. } = b.store().expr(block) else { return };
        let Some(Statement::Let { pat, initializer: Some(init), .. }) = statements.get(index) else { return };
        let Pat::Bind { id, .. } = b.store().pat(pat) else { return };
        let mutable = match b.store()[id].mode {
            BindingAnnotation::Ref => false,
            BindingAnnotation::RefMut => true,
            BindingAnnotation::Unannotated | BindingAnnotation::Mutable => return,
        };
        let Some(def) = static_mut(b, init) else { return };
        let (Some(at), Some(span)) = (let_node(b, pat), expr_span(b, init)) else { return };
        emit(b, at, span, None, mutable, def, out);
    }
}

/// rustc's `path_is_static_mut`: the `static mut` that `expr`, through its fields, names.
fn static_mut(b: &BodyCx<'_, '_, '_>, mut expr: ExprId) -> Option<StaticId> {
    while let Expr::Field { expr: base, .. } = b.store().expr(expr) {
        expr = base;
    }
    let Expr::Path(_) = b.store().expr(expr) else { return None };
    match path_value(b, expr)? {
        ValueNs::StaticId(it) if StaticSignature::of(b.db(), it).flags.contains(StaticFlags::MUTABLE) => Some(it),
        _ => None,
    }
}

/// Where `&` or `&mut ` is written before what the borrow `outer` borrows (`inner`), after the
/// parentheses that open `outer`: what `&raw const ` or `&raw mut ` replaces.
fn addr_of_span(b: &BodyCx<'_, '_, '_>, outer: &Span, inner: &Span) -> Span {
    let Some(snippet) = b.cx.snippet(outer) else { return outer.with_hi(inner.lo) };
    let exclude: usize =
        snippet.chars().take_while(|it| it.is_whitespace() || *it == '(').map(char::len_utf8).sum();
    outer.with_lo(outer.lo + exclude as u32).with_hi(inner.lo)
}

/// The `let` statement the pattern `pat` is of: where rustc's `check_stmt` lints.
fn let_node(b: &BodyCx<'_, '_, '_>, pat: hir_def::hir::PatId) -> Option<InFile<SyntaxNodePtr>> {
    let node = b.pat_node(pat)?;
    let syntax = b.cx.syntax_view(node);
    let stmt = syntax.value.ancestors().find(|it| it.kind() == SyntaxKind::LET_STMT)?;
    Some(InFile::new(syntax.file_id, view::ptr(stmt)))
}

/// rustc's `emit_static_mut_refs`: the lint at `span`, with the `&raw` suggestion at `sugg`
/// when there is one (`suggest_addr_of`).
fn emit(
    b: &BodyCx<'_, '_, '_>,
    at: InFile<SyntaxNodePtr>,
    span: Span,
    sugg: Option<Span>,
    mutable: bool,
    def: StaticId,
    out: &mut Vec<Found>,
) {
    let label = if mutable { "mutable " } else { "shared " };
    let mut diag =
        // alloc: the lint's text, for a lint found
        Diag::lint("static_mut_refs", format!("creating a {label}reference to mutable static"), span.clone());
    // alloc: the lint's text, for a lint found
    diag.span_label(span, format!("{label}reference to mutable static"));
    let suggest_addr_of = sugg.is_some();
    if let Some(sugg) = sugg {
        let (message, code) = match mutable {
            true => ("use `&raw mut` instead to create a raw pointer", "&raw mut "),
            false => ("use `&raw const` instead to create a raw pointer", "&raw const "),
        };
        diag.suggestion_with_style(
            // alloc: the lint's text, for a lint found
            vec![(sugg, code.to_owned())],
            message,
            Applicability::MaybeIncorrect,
            SuggestionStyle::ShowAlways,
        );
    }
    diag.note(if mutable { MUT_NOTE } else { SHARED_NOTE });
    let (help, interior_sugg) = interior_mutability(b, def, mutable, suggest_addr_of);
    if help {
        diag.help(INTERIOR_MUTABILITY_HELP);
    }
    if let Some(mut_span) = interior_sugg {
        diag.suggestion_with_style(
            // alloc: the lint's text, for a lint found
            vec![(mut_span, String::new())],
            INTERIOR_MUTABILITY_SUGG,
            Applicability::MaybeIncorrect,
            SuggestionStyle::ShowAlways,
        );
    }
    let name = b.cx.def_path(ModuleDefId::StaticId(def).into());
    // alloc: the lint's text, for a lint found
    diag.fact(Fact::Signature(b.cx.key(ItemId::Static(def))), format!("the signature of `{name}`"));
    out.push(b.lint(diag, Phase::Late, at));
}

/// rustc's `interior_mutability_suggestion`: whether to say to use a type with interior
/// mutability, and where the `mut` of the static is when its type already has it and the
/// reference is shared.
fn interior_mutability(b: &BodyCx<'_, '_, '_>, def: StaticId, mutable: bool, suggest_addr_of: bool) -> (bool, Option<Span>) {
    let ty = b.db().type_for_static(def).skip_binder();
    if Solver::of(b).is_freeze(ty) {
        return (!suggest_addr_of, None);
    }
    if mutable {
        return (false, None);
    }
    (false, mutability_span(b, def))
}

/// rustc's `static_mutability_span`: the `mut ` (and one space or tab after it) between the
/// visibility and the name of the crate's own `static mut`, when it is written in a file.
fn mutability_span(b: &BodyCx<'_, '_, '_>, def: StaticId) -> Option<Span> {
    let db = b.db();
    if hir::Crate::from(def.krate(db)) != b.cx.krate {
        return None;
    }
    let flags = StaticSignature::of(db, def).flags;
    if flags.contains(StaticFlags::EXTERN) || !flags.contains(StaticFlags::MUTABLE) {
        return None;
    }
    let source = def.lookup(db).source_view(db);
    let file = source.file_id.file_id()?.file_id(db);
    let node = source.value.syntax();
    let first = node.children_with_tokens().find(|it| !it.kind().is_trivia() && it.kind() != SyntaxKind::ATTR)?;
    let lo = match first.kind() {
        SyntaxKind::VISIBILITY => first.text_range().end(),
        _ => first.text_range().start(),
    };
    let hi = source.value.name()?.syntax().text_range().start();
    let snippet = b.cx.file_text(file).get(usize::from(lo)..usize::from(hi))?;
    let (_, static_end) = find_word(snippet, "static", 0)?;
    let (mut_start, mut_end) = find_word(snippet, "mut", static_end)?;
    let mut_end = extend_trailing_space(snippet, mut_end);
    let range = TextRange::new(lo + TextSize::from(mut_start as u32), lo + TextSize::from(mut_end as u32));
    Some(b.cx.span_in_file(file, range))
}

/// rustc's `find_word`: where `word` is first in `snippet` from `start` as a word of its own.
fn find_word(snippet: &str, word: &str, start: usize) -> Option<(usize, usize)> {
    let bytes = snippet.as_bytes();
    let mut search = start;
    while search <= snippet.len() {
        let at = search + snippet[search..].find(word)?;
        let end = at + word.len();
        let before = at == 0 || !is_ident_char(bytes[at - 1]);
        let after = end >= bytes.len() || !is_ident_char(bytes[end]);
        if before && after {
            return Some((at, end));
        }
        search = end;
    }
    None
}

fn is_ident_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// rustc's `extend_trailing_space`: past one space or tab after `end`.
fn extend_trailing_space(snippet: &str, end: usize) -> usize {
    match snippet[end..].chars().next() {
        Some(ch @ (' ' | '\t')) => end + ch.len_utf8(),
        _ => end,
    }
}

#[cfg(test)]
mod tests {
    use super::{extend_trailing_space, find_word};

    #[test]
    fn words_are_found_whole() {
        let snippet = "static /* mutable */ mut ";
        let (_, end) = find_word(snippet, "static", 0).unwrap();
        assert_eq!(find_word(snippet, "mut", end), Some((21, 24)));
        assert_eq!(extend_trailing_space(snippet, 24), 25);
        assert_eq!(find_word("static mutex", "mut", 6), None);
    }
}
