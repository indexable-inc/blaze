//! `wildcard_arm`: a catch-all in a `match` (`_`, or a binding with no subpattern, `other =>`)
//! at a place whose type is an enum of the workspace, not `#[non_exhaustive]`. A variant added
//! to the enum is then taken by the catch-all with no one deciding it there; with the variants
//! written out, each `match` is a compile error until its author does. A catch-all nested in a
//! pattern counts the same (`Some(_)` over an `Option<Kind>`), and so does the one other arm of
//! a two-arm match (clippy's `match_wildcard_for_single_variants`).
//!
//! Which variants a catch-all stands for is match checking's to say:
//! `hir_ty_matchck::catch_alls` analyzes the arms above it and takes the variants its witnesses
//! leave at its place. A catch-all that stands for none (the arms above list every variant) is
//! left to `unreachable_patterns`. Another crate's enum is not checked (it may change under a
//! version bump, and the workspace doesn't decide its variants), nor a `match` a macro wrote, a
//! desugared one (`for`, `?`), or a catch-all a macro wrote into a pattern.
//!
//! The fix writes the variants in place of a `_`, joined by `|`, each `(..)` or `{ .. }` as
//! its fields need, with the path the match's other arms name the enum's variants by, or the
//! enum's path from the body's module where none does. It is machine-applicable for a `_` with
//! no guard on its arm: the variants listed are every value that reaches it, so the match stays
//! exhaustive and each value takes the arm it took before. A binding, or a guarded arm, gets
//! help text with the variants instead.

use hir_def::{
    AdtId, AttrDefId, DefWithBodyId, EnumId, EnumVariantId, HasModule as _, Lookup as _, VariantId,
    attrs::AttrFlags,
    hir::{Expr, ExprId, Pat as HirPat, PatId},
    item_tree::FieldsShape,
};
use syntax::{NodeOrToken, SyntaxKind, TextRange};

use super::Lint;
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::WildcardArm;

/// How many of the variants the message names.
const NAMED: usize = 8;

/// A finding, before it is reported.
struct Found {
    range: TextRange,
    message: String,
    note: String,
    fix: Option<String>,
    help: Option<String>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    let root = cx.root();
    for it in found {
        let at = match root.text_range().contains_range(it.range) {
            true => match root.covering_element(it.range) {
                NodeOrToken::Node(node) => node,
                NodeOrToken::Token(token) => token.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(it.range);
        let fix_span = cx.span(it.range);
        let diagnostic = cx.blaze(LINT, at, span, it.message);
        diagnostic.note(it.note);
        if let Some(fix) = it.fix {
            diagnostic.multipart_suggestion(
                "write out the variants it stands for",
                // alloc: the fix's one part
                vec![(fix_span, fix)],
                Applicability::MachineApplicable,
            );
        }
        if let Some(help) = it.help {
            diagnostic.help(help);
        }
        diagnostic
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let edition = cx.edition();
    let body = analysis.body();
    let file = analysis.tree().1;
    let source_map = analysis.source_map();
    // Where `pat` is written in the walked tree; `None` for what a macro wrote.
    let pat_range = |pat: PatId| {
        let source = source_map.pat_syntax(pat).ok()?;
        (source.file_id == file).then(|| source.value.syntax_node_ptr().text_range())
    };
    for catch_all in hir_ty_matchck::catch_alls(db, def).arms.iter() {
        let enum_ = catch_all.enum_;
        if catch_all.variants.is_empty() || !of_workspace(cx, enum_) {
            continue;
        }
        // A `match` written in the file: not a macro's, not a `for` or `?` lowered to one.
        let written = source_map
            .expr_syntax(catch_all.match_expr)
            .is_ok_and(|it| it.file_id == file && it.value.kind() == SyntaxKind::MATCH_EXPR);
        if !written {
            continue;
        }
        let Some(range) = pat_range(catch_all.pat) else { continue };
        let enum_name = hir::Enum::from(enum_).name(db).display(db, edition).to_string();
        let variants = &enum_.enum_variants(db).variants;
        let prefix = written_prefix(cx, analysis, catch_all.match_expr, enum_, &pat_range)
            .or_else(|| module_prefix(cx, def, enum_));
        let pats: Vec<String> = catch_all // alloc: the variants of one finding, reported at the edge
            .variants
            .iter()
            .filter_map(|&variant| {
                let (name, (_, shape)) = variants.get_index(variant.index(db))?;
                let fields = match shape {
                    FieldsShape::Unit => "",
                    FieldsShape::Tuple => "(..)",
                    FieldsShape::Record => " { .. }",
                };
                Some(format!(
                    "{}{}{fields}",
                    prefix.as_deref().unwrap_or_default(),
                    name.display(db, edition)
                ))
            })
            .collect();
        let named = |it: &EnumVariantId| {
            variants
                .get_index(it.index(db))
                .map(|(name, _)| format!("`{enum_name}::{}`", name.display(db, edition)))
        };
        let mut shown =
            catch_all.variants.iter().take(NAMED).filter_map(named).collect::<Vec<_>>().join(", "); // alloc: a message's part
        if catch_all.variants.len() > NAMED {
            shown.push_str(&format!(" and {} more", catch_all.variants.len() - NAMED));
        }
        let binding = match body.pat(catch_all.pat) {
            HirPat::Bind { id, .. } => Some(body[id].name.display(db, edition).to_string()),
            _ => None,
        };
        let what = match &binding {
            Some(name) => format!("the catch-all binding `{name}`"),
            None => "this `_`".to_owned(), // alloc: a message's part
        };
        let message = format!("{what} stands for {shown} of `{enum_name}`");
        let note = format!(
            "a variant added to `{enum_name}` should be a compile error at every `match` on it, so that each decides \
             what the variant does; a catch-all takes it here with no decision"
        );
        let joined = || {
            let joined = pats.join(" | ");
            match pats.len() > 1 && needs_parens(body, catch_all.arm_pat, catch_all.pat) {
                true => format!("({joined})"),
                false => joined,
            }
        };
        let complete = pats.len() == catch_all.variants.len();
        let (fix, help) = match (&binding, catch_all.guarded, &prefix) {
            (None, false, Some(_)) if complete && cx.in_file() => (Some(joined()), None),
            (Some(name), _, Some(_)) if complete => (
                None,
                Some(format!("bind the variants instead: `{name} @ ({})`", pats.join(" | "))),
            ),
            (None, true, Some(_)) if complete => (
                None,
                Some(format!(
                    "write the variants out, `{}`, and keep the guard: the arm decides by the guard only what it matches",
                    joined()
                )),
            ),
            _ => (None, Some(format!("write out {shown} in place of the catch-all"))),
        };
        // alloc: reported at the edge
        found.push(Found { range, message, note, fix, help });
    }
    found
}

/// Whether `enum_` is of a workspace member and not `#[non_exhaustive]`.
fn of_workspace(cx: &Cx<'_, '_>, enum_: EnumId) -> bool {
    let db = cx.db();
    enum_.krate(db).data(db).origin.is_local()
        && !AttrFlags::query(db, AttrDefId::AdtId(AdtId::EnumId(enum_)))
            .contains(AttrFlags::NON_EXHAUSTIVE)
}

/// The path, up to and with its last `::`, another arm of the match names a variant of `enum_`
/// by (`Kind::`, `self::Kind::`, `Self::`); `None` when no arm does, or one names it bare (a
/// `use Kind::*` may not bring in every variant).
fn written_prefix(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    match_expr: ExprId,
    enum_: EnumId,
    pat_range: &impl Fn(PatId) -> Option<TextRange>,
) -> Option<String> {
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let Expr::Match { arms, .. } = body.expr(match_expr) else { return None };
    let mut prefix = None;
    for arm in arms {
        body.walk_pats(arm.pat, &mut |pat| {
            if prefix.is_some()
                || !matches!(
                    body.pat(pat),
                    HirPat::Path(_) | HirPat::TupleStruct { .. } | HirPat::Record { .. }
                )
            {
                return;
            }
            let Some(VariantId::EnumVariantId(variant)) = infer.variant_resolution_for_pat(pat)
            else {
                return;
            };
            if variant.lookup(db).parent != enum_ {
                return;
            }
            let Some(range) = pat_range(pat) else { return };
            let text = cx.snippet(range);
            let path = text.split(['(', '{']).next().unwrap_or(text).trim_end();
            prefix = Some(path.rsplit_once("::").map(|(head, _)| format!("{head}::")));
        });
        if prefix.is_some() {
            break;
        }
    }
    prefix.flatten()
}

/// The enum's path from the module of the body, with `::` after it.
fn module_prefix(cx: &Cx<'_, '_>, def: DefWithBodyId, enum_: EnumId) -> Option<String> {
    let db = cx.db();
    let module = hir::Module::from(def.module(db));
    let config = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: true,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let path = module.find_path(
        db,
        hir::ModuleDef::Adt(hir::Adt::Enum(hir::Enum::from(enum_))),
        config,
    )?;
    Some(format!("{}::", path.display(db, cx.edition())))
}

/// Whether `|` in place of `pat` would bind looser than the pattern around it: under `&`,
/// `box` or `name @`.
fn needs_parens(body: &hir_def::expr_store::Body, arm_pat: PatId, pat: PatId) -> bool {
    let mut parens = false;
    body.walk_pats(arm_pat, &mut |it| {
        parens |= match body.pat(it) {
            HirPat::Ref { pat: inner, .. } | HirPat::Box { inner } | HirPat::Deref { inner } => {
                inner == pat
            }
            HirPat::Bind { subpat: Some(inner), .. } => inner == pat,
            _ => false,
        };
    });
    parens
}
