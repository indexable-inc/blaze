//! `default_refuses`: a trait method whose default body answers nothing (`Err(..)`, `None`,
//! an empty value made by a `new()` or `default()` with no arguments, `Default::default()`,
//! `""`, `[]`, or `Ok` of one of these), ahead of nothing but `let _ = ..;` lines that discard
//! its parameters. Every implementor that does not say otherwise inherits the refusal without
//! a word, and the compiler cannot tell an implementor that forgot from one that meant it.
//!
//! The incident (2026-11-14): `serve::work::Writer::still`, `env_named`, `read_outside` and
//! `read_seen` had defaults that answered `Err("its front end does not say ..")`. The real
//! front end said; the simulation's fake inherited the refusal, so no simulated build left a
//! request certificate, and nobody noticed for two days: every test passed over a path that
//! never ran.
//!
//! The fix: make the method required (the fix this lint offers replaces the default body with
//! `;`), so each implementor writes what it answers. The one accepted exception is a doc line
//! starting `The default:` on the method, saying why every implementor, present and future,
//! may inherit the answer (`/// The default: a driver with no waker has its ends read each
//! turn`). A body that does any work, an empty body `{}` or a constant like `0` or `true` is
//! not a finding: a hook that does nothing by default is a design, a refusal is not. Test code
//! and a body a macro wrote are left alone. Read from the body's HIR, so `None` and `Err` are
//! core's `Option` and `Result` variants by resolution, not by name.

use hir_def::{
    VariantId,
    hir::{Array, Expr, ExprId, Literal, Pat, Statement},
};
use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasGenericParams as _, HasName as _};

use super::{
    Lint,
    std_mistakes::{Fix, report},
    test_code,
};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::DefaultRefuses;

/// The start of the one doc line that says why every implementor may inherit a default.
pub(super) const EXCEPTION: &str = "The default:";

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(f) = ast::Fn::cast(cx.root()) else { return };
    let Some(block) = f.body() else { return };
    if cx.expansion().is_some() || !cx.is_written(f.syntax()) {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    let hir::DefWithBody::Function(func) = analysis.def() else { return };
    let db = cx.db();
    let Some(assoc) = hir::AsAssocItem::as_assoc_item(func, db) else { return };
    let hir::AssocItemContainer::Trait(trait_) = assoc.container(db) else { return };
    if says_why(f) || test_code::is_test_code(cx, f.syntax()) {
        return;
    }
    let Some(shape) = refusal_body(cx, analysis) else { return };
    let Some(name) = f.name() else { return };
    let (Some(at), Some(block_at)) = (cx.range(name.syntax()), cx.range(block.syntax())) else { return };
    // The fix replaces from where the signature ends (its `where` clause, return type or
    // parameters, the last written) to the body's end, the space between included, so the
    // result reads `-> T;`.
    let signature_end = match (f.where_clause(), f.ret_type(), f.param_list()) {
        (Some(it), _, _) => cx.range(it.syntax()),
        (None, Some(it), _) => cx.range(it.syntax()),
        (None, None, Some(it)) => cx.range(it.syntax()),
        (None, None, None) => None,
    };
    let Some(signature_end) = signature_end.map(|it| it.end()) else { return };
    let body = syntax::TextRange::new(signature_end, block_at.end());
    let message = format!(
        "the default body of `{}::{}` is `{shape}`, an answer of nothing that every implementor \
         inherits without saying so",
        trait_.name(db).as_str(),
        name.text()
    );
    let note = "an implementor that forgot this method and one that meant to refuse look the same: \
                `Writer::still`'s default `Err(..)` let the simulation's fake refuse every request \
                certificate for two days with every test passing (2026-11-14)";
    let mut parts = Vec::new_in(std::alloc::Global);
    parts.push((body, ";".to_owned()));
    let fix = Fix {
        message: "make the method required, so each implementor says what it answers",
        parts,
        // Each implementor that inherited the default must now write it.
        applicability: Applicability::MaybeIncorrect,
    };
    report(cx, LINT, f.syntax(), at, message, note, Some(fix));
}

/// Whether `f`'s doc has a line starting [`EXCEPTION`].
fn says_why(f: ast::Fn<'_>) -> bool {
    f.doc_comments().any(|doc| {
        let text = doc.syntax().text();
        let line = text.trim_start_matches("///").trim_start_matches("//!").trim_start();
        line.starts_with(EXCEPTION)
    })
}

/// The body of `analysis` as the refusal it is, written as the message says it, or `None`
/// when it does anything else. Statements `let _ = <expr>;` ahead of the tail discard
/// parameters and count as nothing.
fn refusal_body(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>) -> Option<&'static str> {
    let body = analysis.body();
    let root = body.root_expr();
    match body.expr(root) {
        Expr::Block { statements, tail: Some(tail), .. } => {
            let discards = statements.iter().all(|it| {
                matches!(it, Statement::Let { pat, else_branch: None, .. } if matches!(body.pat(pat), Pat::Wild))
            });
            if !discards {
                return None;
            }
            refusal(cx, analysis, tail, false)
        }
        Expr::Block { .. } => None,
        _ => refusal(cx, analysis, root, false),
    }
}

/// `expr` as a refusal: `Err(..)`, `None`, an empty value, or (not `inside` an `Ok` already)
/// `Ok` of `None` or of an empty value. Each shape is a fixed text: the set is closed, so no
/// shape is built.
fn refusal(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, expr: ExprId, inside: bool) -> Option<&'static str> {
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    match body.expr(expr) {
        Expr::Literal(Literal::String(text)) if text.as_str().is_empty() => Some("\"\""),
        Expr::Array(Array::ElementList { elements }) if elements.is_empty() => Some("[]"),
        Expr::Ref { expr: inner, .. } => match refusal(cx, analysis, inner, inside)? {
            "[]" => Some("&[]"),
            "\"\"" => Some("&\"\""),
            _ => None,
        },
        Expr::Path(_) => (variant(cx, infer.variant_resolution_for_expr(expr)?)? == "None").then_some("None"),
        Expr::Call { callee, args, .. } => {
            if let Some(found) = infer.variant_resolution_for_expr(callee) {
                return match (variant(cx, found)?, args) {
                    ("Err", _) => (!inside).then_some("Err(..)"),
                    ("Ok", [arg]) if !inside => match refusal(cx, analysis, *arg, true)? {
                        "None" => Some("Ok(None)"),
                        "\"\"" => Some("Ok(\"\")"),
                        "[]" => Some("Ok([])"),
                        "&[]" => Some("Ok(&[])"),
                        "&\"\"" => Some("Ok(&\"\")"),
                        "new()" => Some("Ok(new())"),
                        "default()" => Some("Ok(default())"),
                        "new_in(..)" => Some("Ok(new_in(..))"),
                        _ => None,
                    },
                    _ => None,
                };
            }
            let (f, _) = analysis.callee(expr)?;
            let name = hir::Function::from(f).name(db);
            match (name.as_str(), args.len()) {
                ("new", 0) => Some("new()"),
                ("default", 0) => Some("default()"),
                // An empty collection in a named allocator: `Vec::new_in(Global)`.
                ("new_in", 1) => Some("new_in(..)"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The name of `found` when it is `Ok` or `Err` of core's `Result` or `None` of its `Option`.
fn variant(cx: &Cx<'_, '_>, found: VariantId) -> Option<&'static str> {
    let db = cx.db();
    let VariantId::EnumVariantId(id) = found else { return None };
    let found = hir::EnumVariant::from(id);
    let famous = cx.famous();
    let parent = Some(found.parent_enum(db));
    let name = found.name(db);
    if parent == famous.core_result_Result() {
        return match name.as_str() {
            "Ok" => Some("Ok"),
            "Err" => Some("Err"),
            _ => None,
        };
    }
    (parent == famous.core_option_Option() && name.as_str() == "None").then_some("None")
}

#[cfg(test)]
mod tests {
    use crate::tests::{lint, offered_in};

    const FOUND: &str = "an answer of nothing that every implementor inherits";

    const FLAGS: &str = "-A clippy::all --warn=blaze::default_refuses";

    const HEADER: &str = r#"
//- minicore: sized, copy, clone, option, result, default
//- /src/lib.rs crate:main
"#;

    fn found(main: &str) -> String {
        lint(&format!("{}{}", HEADER.trim_start_matches('\n'), main.trim_start_matches('\n')), FLAGS)
    }

    /// The incident in miniature: a refusal by `Err`, an answer of `None` behind a discarded
    /// parameter, an empty value made by `new()`, `Default::default()` and `Ok(None)`.
    #[test]
    fn a_default_that_answers_nothing_is_found() {
        let out = found(
            r#"
pub struct Names;
impl Names { pub fn new() -> Names { Names } }
pub trait Writer {
    fn still(&self) -> Result<u8, u8> { Err(0) }
    fn named(&self, unit: usize) -> Option<u8> {
        let _ = unit;
        None
    }
    fn names(&self) -> Names { Names::new() }
    fn count(&self) -> Option<u8> { Default::default() }
    fn launch(&self) -> Result<Option<u8>, u8> { Ok(None) }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 5, "{out}");
        assert!(out.contains("the default body of `Writer::still` is `Err(..)`"), "{out}");
        assert!(out.contains("the default body of `Writer::named` is `None`"), "{out}");
        assert!(out.contains("the default body of `Writer::launch` is `Ok(None)`"), "{out}");
    }

    /// Not a finding: a default with its `The default:` line, one that does work, a hook whose
    /// default does nothing, a constant that is an answer, an impl's method, a free function,
    /// and a trait in test code.
    #[test]
    fn a_said_default_real_work_a_hook_and_an_impl_are_left_alone() {
        let out = found(
            r#"
pub trait Driver {
    /// A driver's waker.
    ///
    /// The default: a driver with no waker has its ends read each turn, which is right for
    /// every driver that has none.
    fn waker(&self) -> Option<u8> { None }
    fn plan(&self, n: u8) -> Option<u8> { Some(n) }
    fn wake(&self) {}
    fn connected(&self) -> usize { 0 }
    fn required(&self) -> Option<u8>;
}
pub struct D;
impl Driver for D {
    fn required(&self) -> Option<u8> { None }
}
pub fn free() -> Option<u8> { None }
#[cfg(test)]
mod tests {
    pub trait Fake { fn said(&self) -> Option<u8> { None } }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// The fix makes the method required: its default body becomes `;`.
    #[test]
    fn the_fix_makes_the_method_required() {
        let after = offered_in(
            HEADER.trim_start_matches('\n'),
            "pub trait Front {\n    fn subject(&self, unit: usize) -> Option<u8> {\n        let _ = unit;\n        None\n    }\n}\n",
            FLAGS,
        );
        assert_eq!(after, "pub trait Front {\n    fn subject(&self, unit: usize) -> Option<u8>;\n}\n");
    }
}
