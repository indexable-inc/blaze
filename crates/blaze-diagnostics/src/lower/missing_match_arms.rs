//! E0004: a `match` that does not cover every value of its scrutinee.
//!
//! Follows rustc's `compiler/rustc_mir_build/src/thir/pattern/check_match.rs`
//! (`report_non_exhaustive_match`, `joined_uncovered_patterns`, `report_adt_defined_here`,
//! `is_const_pat_that_looks_like_binding`) and `compiler/rustc_mir_build/src/errors.rs`
//! (`NonExhaustivePatternsTypeNotEmpty`, `AdtDefinedHere`, `NonExhaustiveMatchAllArmsGuarded`)
//! (MIT or Apache-2.0). hir gives what rustc's analysis found: the witnesses printed as
//! `print_witness_pat` prints them, the variants `maybe_point_at_variant` names and the types
//! `collect_special_tys` collects.

use blaze_items::{Fact, ItemId};
use hir::{Adt, InFile, ModuleDef, SpecialTy, Type};
use hir_def::attrs::AttrFlags;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range, item_range_view as item_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::MissingMatchArms<'db>) -> Vec<Found> {
    let scrut = cx.node_view(d.scrutinee_expr);
    let sp = cx.span_of_ptr(d.scrutinee_expr);
    let ty = &d.scrutinee_ty;
    let match_expr = scrut.value.syntax().parent().and_then(ast::MatchExpr::cast);
    let arms: Vec<ast::MatchArm<'db>> =
        // alloc: once per error, the arms the suggestion is placed after.
        match_expr.and_then(|it| it.match_arm_list()).map_or_else(Vec::new, |it| it.arms().collect());
    let braces = match_expr.and_then(|it| braces_span(cx, &sp, scrut.with_value(it.syntax().text_range())));
    let text = cx.file_of(&sp).map(|it| cx.file_text(it));
    let mut diag = match d.no_arms && !is_enum_with_variants(cx, ty) {
        true => type_not_empty(cx, d, &sp, braces, text),
        false => not_covered(cx, d, &sp, Arms { file: scrut.file_id, arms: &arms, braces, text }),
    };
    adt_fact(cx, &mut diag, ty);
    code_fact(cx, &mut diag, d.scrutinee_expr.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Mir)]
}

fn is_enum_with_variants(cx: &Cx<'_, '_>, ty: &Type<'_>) -> bool {
    matches!(ty.as_adt(), Some(Adt::Enum(it)) if !it.variants(cx.db).is_empty())
}

/// rustc's `NonExhaustivePatternsTypeNotEmpty`: an empty match on a type that is not an enum
/// with variants.
fn type_not_empty<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::MissingMatchArms<'db>,
    sp: &Span,
    braces: Option<Span>,
    text: Option<&str>,
) -> Diag {
    let ty = &d.scrutinee_ty;
    let printed = cx.ty(ty);
    let mut diag =
        // alloc: clone: the error's parts, for an error hir found
        Diag::error(Some("E0004"), format!("non-exhaustive patterns: type `{printed}` is non-empty"), sp.clone());
    let peeled = ty.strip_references();
    if let Some(adt) = peeled.as_adt()
        && let Some(at) = adt_span(cx, adt, true)
    {
        // clone: the error's parts, for an error hir found
        let mut span = MultiSpan::of(at.clone());
        // alloc: empty, allocates nothing
        span.labels.push((at, String::new()));
        // alloc: the error's text, for an error hir found
        diag.span_note(span, format!("`{}` defined here", cx.ty(&peeled)));
    }
    match foreign_non_exhaustive(cx, ty) {
        // alloc: the error's text, for an error hir found
        true => diag.note(format!("the matched value is of type `{printed}`, which is marked as non-exhaustive")),
        // alloc: the error's text, for an error hir found
        false => diag.note(format!("the matched value is of type `{printed}`")),
    };
    if d.ref_to_uninhabited {
        diag.note("references are always considered inhabited");
    }
    match braces {
        Some(braces) => {
            let body = match text.map(|it| indentation_before(it, sp.lo)) {
                // alloc: the error's text, for an error hir found
                Some(indent) => format!(" {{\n{indent}    _ => todo!(),\n{indent}}}"),
                // alloc: the error's text, for an error hir found
                None => " { _ => todo!(), }".to_owned(),
            };
            diag.suggestion_verbose(
                braces,
                "ensure that all possible cases are being handled by adding a match arm with a wildcard pattern as shown",
                body,
                Applicability::HasPlaceholders,
            );
        }
        None => {
            diag.help("ensure that all possible cases are being handled by adding a match arm with a wildcard pattern");
        }
    }
    diag
}

/// The arms of a match, where its braces are and the text of its file.
struct Arms<'s> {
    file: hir::HirFileId,
    arms: &'s [ast::MatchArm<'s>],
    braces: Option<Span>,
    text: Option<&'s str>,
}

/// rustc's `report_non_exhaustive_match` past its empty case.
fn not_covered<'db>(cx: &Cx<'_, 'db>, d: &hir::MissingMatchArms<'db>, sp: &Span, arms: Arms<'_>) -> Diag {
    let witnesses = &d.witnesses;
    let joined = joined(witnesses);
    // alloc: clone: the error's parts, for an error hir found
    let mut diag = Diag::error(Some("E0004"), format!("non-exhaustive patterns: {joined} not covered"), sp.clone());
    // alloc: clone: the error's parts, for an error hir found
    diag.span_label(sp.clone(), format!("pattern{} {joined} not covered", plural(witnesses.len())));
    if let Some(here) = DefinedHere::of(cx, &d.scrutinee_ty, &d.variants, true) {
        diag.span_note(here.span(true), here.message());
    }
    // alloc: the error's text, for an error hir found
    diag.note(format!("the matched value is of type `{}`", cx.ty(&d.scrutinee_ty)));
    special_ty_notes(cx, &mut diag, &d.special_tys);
    if d.ref_to_uninhabited {
        diag.note("references are always considered inhabited");
    }
    for (pat, konst) in &d.consts_like_bindings {
        let Some((name, def)) = const_like_binding(cx, cx.node_view(*pat).value, *konst) else { continue };
        let at = cx.span_of_ptr(*pat);
        diag.span_label(
            // clone: the error's parts, for an error hir found
            at.clone(),
            // alloc: the error's text, for an error hir found
            format!(
                "this pattern doesn't introduce a new catch-all binding, but rather pattern matches against the value of constant `{name}`"
            ),
        );
        // alloc: the error's text, for an error hir found
        diag.span_note(def, format!("constant `{name}` defined here"));
        diag.suggestion_verbose(
            at.shrink_to_hi(),
            "if you meant to introduce a binding, use a different name",
            "_var",
            Applicability::MaybeIncorrect,
        );
    }

    let suggest_the_witnesses = witnesses.len() < 4;
    let suggested_arm = match suggest_the_witnesses {
        // Only a never pattern prints as `!`, and its arm takes no body.
        true if witnesses.iter().all(|it| it == "!") => witnesses.join(" | "),
        // alloc: the error's text, for an error hir found
        true => format!("{} => todo!()", witnesses.join(" | ")),
        // alloc: the error's text, for an error hir found
        false => "_ => todo!()".to_owned(),
    };
    let suggestion = arm_suggestion(cx, sp, &arms, &suggested_arm);
    // alloc: the error's text, for an error hir found
    let message = format!(
        "ensure that all possible cases are being handled by adding a match arm with a wildcard pattern{}{}",
        match witnesses.len() > 1 && suggest_the_witnesses && suggestion.is_some() {
            true => ", a match arm with multiple or-patterns",
            false => "",
        },
        match (witnesses.len(), suggestion.is_some()) {
            (0, true) => " as shown",
            (0, false) => "",
            (1, true) => " or an explicit pattern as shown",
            (1, false) => " or an explicit pattern",
            (_, true) => " as shown, or multiple match arms",
            (_, false) => " or multiple match arms",
        },
    );
    if d.would_be_exhaustive_without_guards {
        diag.note("match arms with guards don't count towards exhaustivity");
    }
    match suggestion {
        Some((span, code)) => {
            diag.suggestion_verbose(span, message, code, Applicability::HasPlaceholders);
        }
        None => {
            diag.help(message);
        }
    }
    diag
}

/// rustc's notes on the types `collect_special_tys` found in the first witness.
fn special_ty_notes<'db>(cx: &Cx<'_, 'db>, diag: &mut Diag, special_tys: &[(Type<'db>, SpecialTy)]) {
    for (ty, special) in special_tys {
        let ty = cx.ty(ty);
        match special {
            // alloc: the error's text, for an error hir found
            SpecialTy::Usize => diag.note(format!(
                "`{ty}::MAX` is not treated as exhaustive, so half-open ranges are necessary to match exhaustively"
            )),
            // alloc: the error's text, for an error hir found
            SpecialTy::Isize => diag.note(format!(
                "`{ty}::MIN` and `{ty}::MAX` are not treated as exhaustive, so half-open ranges are necessary to match exhaustively"
            )),
            SpecialTy::Str => diag.note("`&str` cannot be matched exhaustively, so a wildcard `_` is necessary"),
            // alloc: the error's text, for an error hir found
            SpecialTy::ForeignNonExhaustive => diag.note(format!(
                "`{ty}` is marked as non-exhaustive, so a wildcard `_` is necessary to match exhaustively"
            )),
            // alloc: the error's text, for an error hir found
            SpecialTy::Uninhabited => diag.note(format!(
                "`{ty}` is uninhabited but is not being matched by value, so a wildcard `_` is required"
            )),
        };
    }
}

/// Where rustc suggests the missing arm, and its code: in the braces of an empty match, after
/// its only arm, or after its last when it is laid out as the one before it.
fn arm_suggestion(cx: &Cx<'_, '_>, sp: &Span, arms: &Arms<'_>, suggested_arm: &str) -> Option<(Span, String)> {
    let arm_span = |arm: &ast::MatchArm<'_>| cx.span(InFile::new(arms.file, arm_range(*arm)));
    // rustc asks whether the body is a block to leave the comma out, which never holds: THIR
    // wraps every arm's body in a scope.
    match arms.arms {
        [] => {
            // clone: the error's parts, for an error hir found
            let braces = arms.braces.clone()?;
            let code = match arms.text.map(|it| indentation_before(it, sp.lo)) {
                // alloc: the error's text, for an error hir found
                Some(indent) => format!(" {{\n{indent}    {suggested_arm},\n{indent}}}"),
                // alloc: the error's text, for an error hir found
                None => format!(" {{ {suggested_arm}, }}"),
            };
            Some((braces, code))
        }
        [only] => {
            let only = arm_span(only);
            let pre = match arms.text {
                Some(text) if is_multiline(text, only.lo, extend_while(text, only.hi, |c| c.is_whitespace() || c == ',')) => {
                    // alloc: the error's text, for an error hir found
                    format!("\n{}", indentation_before(text, only.lo))
                }
                // alloc: the error's text, for an error hir found
                _ => " ".to_owned(),
            };
            // alloc: the error's text, for an error hir found
            Some((only.shrink_to_hi(), format!(",{pre}{suggested_arm}")))
        }
        [.., prev, last] => {
            let (prev, last) = (arm_span(prev), arm_span(last));
            if prev.file != last.file || prev.expansion != last.expansion {
                return None;
            }
            let text = arms.text?;
            let spacing = match is_multiline(text, prev.hi, last.lo) {
                // alloc: the error's text, for an error hir found
                true => format!("\n{}", indentation_before(text, last.lo)),
                // alloc: the error's text, for an error hir found
                false => " ".to_owned(),
            };
            // alloc: the error's text, for an error hir found
            Some((last.shrink_to_hi(), format!(",{spacing}{suggested_arm}")))
        }
    }
}

/// rustc's span of a match arm: from its pattern, after its attributes, to the end of its body,
/// without the comma after it.
fn arm_range(arm: ast::MatchArm<'_>) -> TextRange {
    let range = item_range(arm.syntax());
    let end = arm
        .syntax()
        .children_with_tokens()
        .filter(|it| !it.kind().is_trivia() && it.kind() != SyntaxKind::COMMA)
        .last()
        .map_or(range.end(), |it| it.text_range().end());
    TextRange::new(range.start(), end.max(range.start()))
}

/// The span after the scrutinee to the end of the match, when both are in the same place (the
/// braces with what is in them and the space before them).
fn braces_span(cx: &Cx<'_, '_>, sp: &Span, match_range: InFile<TextRange>) -> Option<Span> {
    let whole = cx.span(match_range);
    (whole.file == sp.file && whole.expansion == sp.expansion && sp.hi <= whole.hi)
        // clone: the error's parts, for an error hir found
        .then(|| Span { lo: sp.hi, hi: whole.hi, ..sp.clone() })
}

/// rustc's `SourceMap::indentation_before`: the white space that starts the line `at` is on.
pub(super) fn indentation_before(text: &str, at: u32) -> &str {
    let before = &text[..at as usize];
    let line = before.rsplit_once('\n').map_or(before, |(_, line)| line);
    line.split_once(|c: char| !c.is_whitespace()).map_or(line, |(indent, _)| indent)
}

/// rustc's `span_extend_while`: where the text from `hi` stops being what `f` takes.
fn extend_while(text: &str, hi: u32, f: impl Fn(char) -> bool) -> u32 {
    let rest = &text[hi as usize..];
    let len = rest.char_indices().find(|(_, c)| !f(*c)).map_or(rest.len(), |(at, _)| at);
    hi + len as u32
}

fn is_multiline(text: &str, lo: u32, hi: u32) -> bool {
    text[lo as usize..hi as usize].contains('\n')
}

pub(super) fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// rustc's `joined_uncovered_patterns`, also the text of `rustc_pattern_analysis`'s
/// `Uncovered` label: the first three witnesses, and how many more there are.
pub(super) fn joined(witnesses: &[String]) -> String {
    match witnesses {
        // alloc: empty, allocates nothing
        [] => String::new(),
        // alloc: the error's text, for an error hir found
        [only] => format!("`{only}`"),
        // alloc: the error's text, for an error hir found
        [head @ .., last] if head.len() < 3 => format!("`{}` and `{last}`", head.join("`, `")),
        // alloc: the error's text, for an error hir found
        _ => format!("`{}` and {} more", witnesses[..3].join("`, `"), witnesses.len() - 3),
    }
}

/// The rest of rustc's `is_const_pat_that_looks_like_binding` for a pattern hir found is a
/// constant named by one identifier: its source text is a plain name. With the constant's name
/// and the span of its definition.
pub(super) fn const_like_binding<'t>(
    cx: &Cx<'_, '_>,
    pat: impl ast::AstNode<'t>,
    konst: hir::Const,
) -> Option<(String, Span)> {
    if !all_chars(pat.syntax(), |c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    // alloc: the error's text, for an error hir found
    let name = konst.name(cx.db)?.as_str().to_owned();
    let source = cx.vsema.source(konst)?;
    let start = item_range(source.value.syntax()).start();
    let end = source.value.name().map_or(start, |it| it.syntax().text_range().end());
    Some((name, cx.span(source.with_value(TextRange::new(start, end.max(start))))))
}

/// Whether every character of `node`'s text is one `f` takes, read from its tokens in place.
pub(super) fn all_chars(node: ast::Node<'_>, f: impl Fn(char) -> bool) -> bool {
    node.text().chars().all(f)
}

/// rustc's `variant_list_has_applicable_non_exhaustive`: an ADT of another crate marked
/// `#[non_exhaustive]`.
fn foreign_non_exhaustive(cx: &Cx<'_, '_>, ty: &Type<'_>) -> bool {
    ty.as_adt().is_some_and(|adt| {
        adt.module(cx.db).krate(cx.db) != cx.krate
            && AttrFlags::query(cx.db, hir_def::AdtId::from(adt).into()).contains(AttrFlags::NON_EXHAUSTIVE)
    })
}

/// The fact of what a match on `ty` read of its ADT: its variants or fields.
pub(super) fn adt_fact(cx: &Cx<'_, '_>, diag: &mut Diag, ty: &Type<'_>) {
    let Some(adt) = ty.strip_references().as_adt() else { return };
    let what = match adt {
        Adt::Enum(_) => "variants",
        Adt::Struct(_) | Adt::Union(_) => "fields",
    };
    let path = cx.def_path(ModuleDef::Adt(adt));
    // alloc: the error's text, for an error hir found
    diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the {what} of `{path}`"));
}

/// Where rustc points at an ADT in a note: its name when it is the crate's own, else when
/// `non_local` its definition (`def_span`).
fn adt_span(cx: &Cx<'_, '_>, adt: Adt, non_local: bool) -> Option<Span> {
    let source = cx.vsema.source(adt)?;
    if adt.module(cx.db).krate(cx.db) == cx.krate {
        let name = source.value.name()?;
        return Some(cx.span(source.with_value(name.syntax().text_range())));
    }
    non_local.then(|| cx.span(source.with_value(def_range(source.value.syntax()))))
}

/// rustc's `AdtDefinedHere`: the ADT a match is on, with the variants its witnesses name.
pub(super) struct DefinedHere {
    at: Span,
    ty: String,
    variants: Vec<Span>,
}

impl DefinedHere {
    /// rustc's `report_adt_defined_here`, of the variants hir found `maybe_point_at_variant`
    /// names.
    pub(super) fn of<'db>(
        cx: &Cx<'_, 'db>,
        ty: &Type<'db>,
        variants: &[hir::EnumVariant],
        non_local: bool,
    ) -> Option<DefinedHere> {
        let peeled = ty.strip_references();
        let at = adt_span(cx, peeled.as_adt()?, non_local)?;
        let variants = variants
            .iter()
            .filter_map(|variant| {
                let source = cx.vsema.source(*variant)?;
                let name = source.value.name()?;
                Some(cx.span(source.with_value(name.syntax().text_range())))
            })
            .collect(); // alloc: once per error.
        Some(DefinedHere { at, ty: cx.ty(&peeled), variants })
    }

    /// Its spans; rustc's E0004 labels the ADT's own with an empty label, E0005 does not.
    pub(super) fn span(&self, empty_label: bool) -> MultiSpan {
        // clone: the error's parts, for an error hir found
        let mut span = MultiSpan::of(self.at.clone());
        if empty_label {
            // alloc: clone: the error's parts, for an error hir found
            span.labels.push((self.at.clone(), String::new()));
        }
        // alloc: clone: the error's parts, for an error hir found
        span.labels.extend(self.variants.iter().map(|it| (it.clone(), "not covered".to_owned())));
        span
    }

    pub(super) fn message(&self) -> String {
        // alloc: the error's text, for an error hir found
        format!("`{}` defined here", self.ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_witnesses_are_joined_as_rustc_joins_them() {
        let witnesses = |names: &[&str]| names.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
        assert_eq!(joined(&witnesses(&["None"])), "`None`");
        assert_eq!(joined(&witnesses(&["A", "B(_)"])), "`A` and `B(_)`");
        assert_eq!(joined(&witnesses(&["A", "S { a: and, .. }", "C"])), "`A`, `S { a: and, .. }` and `C`");
        assert_eq!(joined(&witnesses(&["A", "B", "C", "D"])), "`A`, `B`, `C` and 1 more");
        assert_eq!(joined(&witnesses(&["A", "B", "C", "D", "E", "F"])), "`A`, `B`, `C` and 3 more");
    }

    #[test]
    fn the_indentation_is_that_of_the_line() {
        let text = "fn main() {\n    match x {\n        A => {}\n    }\n}\n";
        let at = text.find("A =>").unwrap() as u32;
        assert_eq!(indentation_before(text, at), "        ");
        assert_eq!(extend_while(text, at + 7, |c| c.is_whitespace() || c == ','), text.find("    }").unwrap() as u32 + 4);
    }
}
