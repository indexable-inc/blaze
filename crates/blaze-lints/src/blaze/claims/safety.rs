//! `safety_without_invariant`: a `// SAFETY:` comment above an `unsafe` block or `unsafe impl`
//! that does not name, by a path that resolves, the invariant it relies on, or does not say
//! where that invariant is upheld. A `SAFETY:` comment is the one claim the compiler is told to
//! believe; one that says "the pointer is valid" leaves a reader to rebuild why.
//!
//! The form, both parts in the comment's run of lines:
//!
//! ```text
//! // SAFETY: `Ring::cap` is a power of two (the invariant of [`Ring`]); upheld by `Ring::new`.
//! // SAFETY: `slice::from_raw_parts`'s contract: `ptr` and `len` come from one live `Vec`; upheld above.
//! // SAFETY: the `# Safety` contract of this `unsafe fn`; upheld by the caller.
//! ```
//!
//! What it names: a backticked path (`` `Ring::cap` ``, `` [`Ring`] ``) that resolves from the
//! item the code is in, as `doc_names_nothing` resolves one. Where it is upheld: `upheld`
//! (`established`, `ensured`) followed by such a path, by `above` or `here` (the code just
//! before, which a reader sees), or, in an `unsafe fn`, by `the caller`. Without a `SAFETY:`
//! comment it says nothing: clippy's `undocumented_unsafe_blocks` asks for one.

use std::alloc::Global;

use syntax::{
    TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::{
    super::{Lint, std_mistakes::report, test_code},
    names::{self, Owner},
};
use crate::check::Cx;

const LINT: Lint = Lint::SafetyWithoutInvariant;

/// The words that say where an invariant is upheld.
const UPHELD: &[&str] = &["upheld", "established", "ensured"];

/// What, after one of [`UPHELD`], places it in the code a reader sees.
const HERE: &[&str] = &["above", "here", "below"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) || !cx.in_file() {
        return;
    }
    let (start, owner, in_unsafe_fn) = if let Some(block) = ast::BlockExpr::cast(node) {
        let Some(token) = block.unsafe_token() else { return };
        let Some(analysis) = cx.analysis() else { return };
        let hir::DefWithBody::Function(function) = analysis.def() else { return };
        let unsafe_fn = ast::Fn::cast(cx.root()).is_some_and(|f| f.unsafe_token().is_some());
        (token.text_range().start(), Owner::Def(hir::ModuleDef::Function(function)), unsafe_fn)
    } else if let Some(imp) = ast::Impl::cast(node) {
        let Some(token) = imp.unsafe_token() else { return };
        let Some(def) = cx.impl_def(imp) else { return };
        (token.text_range().start(), Owner::Impl(def), false)
    } else {
        return;
    };
    if test_code::is_test_code(cx, node) {
        return;
    }
    let text = cx.text();
    let Some((at, comment)) = comment_above(text, usize::from(start)) else { return };
    if !comment.to_ascii_uppercase().contains("SAFETY:") {
        return;
    }
    let inner = hir::IsInnerDoc::No;
    let mut named = false;
    let mut dead = None;
    // Where the first path that resolves after an `upheld` word is, or a word that places it.
    let lower = comment.to_ascii_lowercase();
    let upheld_at = UPHELD.iter().filter_map(|it| lower.find(it)).min();
    let mut placed = upheld_at.is_some_and(|from| {
        let rest = &lower[from..];
        HERE.iter().any(|it| rest.contains(it)) || (in_unsafe_fn && rest.contains("the caller"))
    });
    for (offset, span) in spans(comment) {
        let Some(path) = names::path_like(span) else { continue };
        if names::resolves(cx, owner, path, inner) {
            if upheld_at.is_some_and(|from| offset > from) {
                placed = true;
            } else {
                named = true;
            }
        } else if dead.is_none() {
            dead = Some((offset, path));
        }
    }
    let what = names::owner_name(cx, owner);
    let (range, message) = if !named
        && let Some((offset, path)) = dead
    {
        // A path that resolves to nothing names no invariant: the one it meant moved or went.
        let range = TextRange::at(at + TextSize::of(&comment[..offset]), TextSize::of(path));
        (range, format!("this SAFETY comment in {what} names `{path}`, which resolves to nothing from there, and no other invariant"))
    } else if !named {
        let range = TextRange::at(at, TextSize::of(comment.trim_end()));
        (range, format!("this SAFETY comment in {what} names no invariant by its path: a reader can't find what the `unsafe` relies on"))
    } else if !placed {
        let range = TextRange::at(at, TextSize::of(comment.trim_end()));
        (range, format!("this SAFETY comment in {what} names what it relies on but not where that is upheld"))
    } else {
        return;
    };
    let note = "write `// SAFETY: `Type::field` is <the invariant> (the `# Invariants` of [`Type`]); upheld by `Type::new``: the invariant by its path, then `upheld by` a path, `upheld above`, or in an `unsafe fn` `upheld by the caller`";
    report(cx, LINT, node, range, message, note, None);
}

/// The run of `//` comment lines directly above the line `start` is on (attributes on lines of
/// their own between are passed over), with where it starts in `text`.
fn comment_above(text: &str, start: usize) -> Option<(TextSize, &str)> {
    let line_start = text[..start].rfind('\n').map_or(0, |it| it + 1);
    let mut from = line_start;
    let mut first = None;
    while from > 0 {
        let end = from - 1;
        let begin = text[..end].rfind('\n').map_or(0, |it| it + 1);
        let line = text[begin..end].trim();
        if line.starts_with("//") && !line.starts_with("///") && !line.starts_with("//!") {
            first = Some(begin);
        } else if !(line.starts_with("#[") && line.ends_with(']')) || first.is_some() {
            break;
        }
        from = begin;
    }
    let begin = first?;
    let comment = &text[begin..line_start];
    let lead = comment.len() - comment.trim_start().len();
    Some((TextSize::of(&text[..begin + lead]), &comment[lead..]))
}

/// The single-backtick spans of `text`, over its lines, each with its byte offset in `text`.
fn spans(text: &str) -> Vec<(usize, &str), Global> {
    let mut found = Vec::new_in(Global);
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        for (offset, span) in names::spans(line) {
            found.push((at + offset, span));
        }
        at += line.len();
    }
    found
}

#[cfg(test)]
mod tests {
    use super::comment_above;

    #[test]
    fn the_run_of_comment_lines_right_above_is_the_comment() {
        let text = "fn f() {\n    // other\n\n    // SAFETY: one\n    // two\n    #[allow(x)]\n    unsafe {}\n}";
        let start = text.find("unsafe").unwrap();
        let (_, comment) = comment_above(text, start).unwrap();
        assert_eq!(comment, "// SAFETY: one\n    // two\n    #[allow(x)]\n");
        assert_eq!(comment_above("let a = 1;\nunsafe {}", 11), None);
    }
}
