//! A constant used as a pattern that `const_to_pat` refuses: a union, a pointer, a value of a
//! type that can't be a pattern, a non-structural ADT (one that doesn't derive `PartialEq`), a
//! type without `PartialEq`, or NaN. Each part of the value refused is its own error.
//!
//! Follows rustc's `compiler/rustc_mir_build/src/thir/pattern/const_to_pat.rs` (`to_pat`,
//! `valtree_to_pat`, the `TypeNotStructural` suggestions) and
//! `compiler/rustc_mir_build/src/diagnostics.rs` (`UnionPattern`, `PointerPattern`,
//! `InvalidPattern`, `TypeNotStructural`, `SuggestEq`, `TypeNotPartialEq`, `NaNPattern`) (MIT
//! or Apache-2.0). Which constants are refused, and why, is match checking's.

use hir::{AsAssocItem as _, AssocItemContainer, ConstPatternError, InFile, LangItem, ModuleDef, PathResolution, Type};
use hir_def::attrs::AttrFlags;
use syntax::{
    AstPtr, SyntaxKind, ast as tag,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::{code_fact, def_span},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

const STRUCTURAL_PARTIAL_EQ: &str =
    "see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details";

const MUST_BE_DERIVED: &str = "the `PartialEq` trait must be derived, manual `impl`s are not sufficient; see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details";

const POINTER: &str = "function pointers and raw pointers not derived from integers in patterns behave unpredictably and should not be relied upon";

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::ConstPatternRefused<'db>) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let mut diag = match &d.error {
        ConstPatternError::Union => refused(span, "cannot use unions in constant patterns", "can't use a `union` here"),
        ConstPatternError::Pointer => {
            let mut diag = refused(span, POINTER, "can't be used in patterns");
            diag.note("see https://github.com/rust-lang/rust/issues/70861 for details");
            diag
        }
        ConstPatternError::Invalid(ty) => {
            let prefix = crate::print::prefix_string(cx, ty);
            let message = format!("{prefix} `{}` cannot be used in patterns", cx.ty(ty)); // alloc: the error's message, the edge
            refused(span, message, format!("{prefix} can't be used in patterns")) // alloc: its label
        }
        ConstPatternError::NotStructural { ty, inapplicable_structural } => {
            not_structural(cx, d.pat, span, ty, *inapplicable_structural)
        }
        ConstPatternError::NotPartialEq { ty, local_manual, local_without, manual, without } => {
            let message = format!("constant of non-structural type `{}` in a pattern", cx.ty(ty)); // alloc: the error's message, the edge
            let mut diag = refused(span, message, "constant of non-structural type");
            diag.note(STRUCTURAL_PARTIAL_EQ);
            not_partial_eq(cx, &mut diag, local_manual, local_without, manual, without);
            diag
        }
        ConstPatternError::NaN => {
            let mut diag = refused(span, "cannot use NaN in patterns", "evaluates to `NaN`, which is not allowed in patterns");
            diag.note("NaNs compare inequal to everything, even themselves, so this pattern would never match");
            diag.help("try using the `is_nan` method instead");
            diag
        }
    };
    defined_here(cx, &mut diag, d.pat);
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Mir)] // alloc: the error found, the edge
}

/// rustc's `mk_err`: the constant's `def_span`, after an unlabelled one at the impl or trait
/// of a local associated constant.
fn defined_here(cx: &Cx<'_, '_>, diag: &mut Diag, pat: InFile<AstPtr<tag::Pat>>) {
    let Some(konst) = konst(cx, pat) else { return };
    let db = cx.db;
    if konst.module(db).krate(db) == cx.krate
        && let Some(item) = konst.as_assoc_item(db)
    {
        let container = match item.container(db) {
            AssocItemContainer::Impl(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
            AssocItemContainer::Trait(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        };
        if let Some(node) = container {
            diag.span_label(cx.span(node.map(def_span)), "");
        }
    }
    if let Some(source) = cx.vsema.source(konst) {
        diag.span_label(cx.span(source.map(|it| def_span(it.syntax()))), "constant defined here");
    }
}

/// The constant the pattern names: a path, or a lone name that resolves to one.
fn konst(cx: &Cx<'_, '_>, pat: InFile<AstPtr<tag::Pat>>) -> Option<hir::Const> {
    let def = match cx.node_view(pat).value {
        ast::Pat::IdentPat(it) => cx.vsema.resolve_bind_pat_to_const(it)?,
        ast::Pat::PathPat(it) => match cx.vsema.resolve_path(it.path()?)? {
            PathResolution::Def(def) => def,
            _ => return None,
        },
        _ => return None,
    };
    match def {
        ModuleDef::Const(it) => Some(it),
        _ => None,
    }
}

/// The error at the constant, its primary span labelled.
fn refused(span: Span, message: impl Into<String>, label: impl Into<String>) -> Diag {
    let mut diag = Diag::error(None, message, span.clone()); // clone: the error's span, the edge
    diag.span_label(span, label);
    diag
}

/// rustc's `TypeNotStructural`. With a `StructuralPartialEq` impl whose bounds don't hold
/// (`inapplicable_structural`) there is no note on a manual `PartialEq` impl, and the
/// suggestion is given only for a local ADT; otherwise the note is the impl's span note when
/// the crate writes one, else the plain note.
fn not_structural<'db>(
    cx: &Cx<'_, 'db>,
    pat: InFile<AstPtr<tag::Pat>>,
    span: Span,
    ty: &Type<'db>,
    inapplicable_structural: bool,
) -> Diag {
    let printed = cx.ty(ty);
    let message = format!("constant of non-structural type `{printed}` in a pattern"); // alloc: the error's message, the edge
    let mut diag = refused(span.clone(), message, "constant of non-structural type"); // clone: the error's span, the edge
    let is_local = ty.as_adt().is_some_and(|adt| adt.module(cx.db).krate(cx.db) == cx.krate);
    if let Some(adt) = ty.as_adt()
        && let Some(source) = cx.vsema.source(adt)
    {
        let def = cx.span(source.map(|it| def_span(it.syntax())));
        let label = match is_local {
            // alloc: the error's label, the edge
            true => format!("`{printed}` must be annotated with `#[derive(PartialEq)]` to be usable in patterns"),
            false => format!("`{printed}` is not usable in patterns"), // alloc: as above
        };
        diag.span_label(def, label);
    }
    if !inapplicable_structural {
        match manual_partial_eq(cx, ty) {
            Some(at) => diag.span_note(at, MUST_BE_DERIVED),
            None => diag.note(STRUCTURAL_PARTIAL_EQ),
        };
    }
    if (is_local || !inapplicable_structural)
        && let Some(name) = cx.snippet(&span)
    {
        let manual = !inapplicable_structural;
        suggest_eq(cx, &mut diag, pat, &span, name, (!manual).then_some(printed.as_str()));
    }
    diag
}

/// rustc's `extend_type_not_partial_eq`: a span note at each local ADT with a manual
/// `PartialEq` impl (at the impl, else the ADT), a label at each local ADT with none, then the
/// foreign types with a manual impl and those with none, each sorted by their text.
fn not_partial_eq<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    local_manual: &[hir::ManualPartialEq],
    local_without: &[hir::Adt],
    manual: &[Type<'db>],
    without: &[Type<'db>],
) {
    for &it in local_manual {
        let source = match it {
            hir::ManualPartialEq::Impl(it) => cx.vsema.source(it).map(|it| it.map(|it| def_span(it.syntax()))),
            hir::ManualPartialEq::Adt(it) => cx.vsema.source(it).map(|it| it.map(|it| def_span(it.syntax()))),
        };
        if let Some(at) = source {
            diag.span_note(cx.span(at), MUST_BE_DERIVED);
        }
    }
    for &adt in local_without {
        if let Some(source) = cx.vsema.source(adt) {
            let at = cx.span(source.map(|it| def_span(it.syntax())));
            diag.span_label(at, "must be annotated with `#[derive(PartialEq)]` to be usable in patterns");
        }
    }
    let sorted = |tys: &[Type<'db>]| {
        let mut printed: Vec<String> = tys.iter().map(|it| cx.ty(it)).collect(); // alloc: the error's notes, the edge
        printed.sort_unstable();
        printed
    };
    for ty in sorted(manual) {
        // alloc: the error's note, the edge
        diag.note(format!(
            "`{ty}` must be annotated with `#[derive(PartialEq)]` to be usable in patterns, manual `impl`s are not sufficient; see https://doc.rust-lang.org/stable/std/marker/trait.StructuralPartialEq.html for details"
        ));
    }
    for ty in sorted(without) {
        diag.note(format!("`{ty}` must be annotated with `#[derive(PartialEq)]` to be usable in patterns")); // alloc: as above
    }
}

/// rustc's `non_blanket_impl` when it is local and not derived: a `PartialEq` impl for the
/// type the crate writes itself, at its `def_span`.
fn manual_partial_eq(cx: &Cx<'_, '_>, ty: &Type<'_>) -> Option<Span> {
    let db = cx.db;
    let partial_eq = hir::Trait::lang(db, cx.krate, LangItem::PartialEq)?;
    // clone: `hir::Type` is an owner ID and an interned `Ty`, a copy
    hir::Impl::all_for_type(db, ty.clone()).into_iter().find_map(|it| {
        if it.trait_(db) != Some(partial_eq) || it.module(db).krate(db) != cx.krate {
            return None;
        }
        // A builtin derive has no `ImplId`: it is not written.
        let id = hir_def::ImplId::try_from(it).ok()?;
        if AttrFlags::query(db, id.into()).contains(AttrFlags::AUTOMATICALLY_DERIVED) {
            return None;
        }
        let source = cx.vsema.source(it)?;
        Some(cx.span(source.map(|it| def_span(it.syntax()))))
    })
}

/// rustc's `SuggestEq`: by what the constant's outermost pattern is part of, bind it and
/// compare instead. rustc goes up through patterns only, so a constant under a struct
/// pattern's field (a `PatField`, not a pattern) gets none. `not_manual` is the type printed
/// when rustc's `manual_partialeq_impl` is false: the message is then conditional on it.
fn suggest_eq(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    pat: InFile<AstPtr<tag::Pat>>,
    span: &Span,
    name: &str,
    not_manual: Option<&str>,
) {
    let node = cx.node_view(pat).map(|it| it.syntax());
    let mut outer = node;
    let mut parent = None;
    for ancestor in cx.vsema.ancestors_with_macros_file(node).skip(1) {
        match ancestor.value.kind() {
            // A pattern macro's call: HIR has the expansion in its place.
            SyntaxKind::MACRO_CALL => {}
            kind if ast::Pat::can_cast(kind) => outer = ancestor,
            _ => {
                parent = Some(ancestor);
                break;
            }
        }
    }
    let Some(parent) = parent else { return };
    let outer = cx.span(outer.map(|it| it.text_range()));
    let range = |node: ast::Node<'_>| cx.span(parent.with_value(node.text_range()));
    let part = |at: Span, code: &str| (at, code.to_owned()); // alloc: a suggestion being built, the edge
    let binding = part(span.clone(), "binding"); // clone: the constant's span, the edge
    let compare = format!(" && binding == {name}"); // alloc: a suggestion being built, the edge
    let (message, parts) = match parent.value.kind() {
        SyntaxKind::MATCH_ARM => {
            let Some(arm) = ast::MatchArm::cast(parent.value) else { return };
            match arm.guard().and_then(|it| it.condition()) {
                None => {
                    let add = part(outer.shrink_to_hi(), &format!(" if binding == {name}")); // alloc: as above
                    ("add a condition to the match arm checking for equality", [binding, add].into())
                }
                Some(guard) => (
                    "add a check for equality to the condition of the match arm",
                    [binding, part(range(guard.syntax()).shrink_to_hi(), &compare)].into(),
                ),
            }
        }
        SyntaxKind::LET_EXPR => {
            let Some(let_expr) = ast::LetExpr::cast(parent.value) else { return };
            let Some(init) = let_expr.expr() else { return };
            let whole = range(let_expr.syntax());
            if outer.source_equal(span) {
                let init = range(init.syntax());
                // rustc's `between` an expansion's span gives no suggestion (`if let V = vec![]`).
                if init.expansion.is_some() {
                    return;
                }
                let parts = [part(whole.with_hi(span.lo), ""), part(span.shrink_to_hi().with_hi(init.lo), " == ")];
                ("check for equality instead of pattern matching", parts.into())
            } else if cx.edition.at_least_2024() {
                let parts = [binding, part(whole.shrink_to_hi(), &compare)];
                ("check for equality instead of pattern matching", parts.into())
            } else {
                return;
            }
        }
        SyntaxKind::LET_STMT => {
            let Some(stmt) = ast::LetStmt::cast(parent.value) else { return };
            let (Some(init), Some(els)) = (stmt.initializer(), stmt.let_else().and_then(|it| it.block_expr())) else {
                return;
            };
            let (init, els) = (range(init.syntax()), range(els.syntax()));
            if init.expansion.is_some() || els.expansion.is_some() {
                return;
            }
            let Some(let_token) = stmt.let_token() else { return };
            let start = cx.span(parent.with_value(let_token.text_range()));
            let parts = [
                part(start.with_hi(outer.lo), "if "),
                part(outer.shrink_to_hi().with_hi(init.lo), " == "),
                part(init.shrink_to_hi().with_hi(els.lo), " "),
            ];
            ("check for equality instead of pattern matching", parts.into())
        }
        _ => return,
    };
    let message = match not_manual {
        // alloc: the suggestion's message, the edge
        Some(ty) => format!("if `{ty}` manually implemented `PartialEq`, you could {message}"),
        None => message.to_owned(), // alloc: as above
    };
    diag.multipart_suggestion(message, parts, Applicability::MaybeIncorrect);
}
