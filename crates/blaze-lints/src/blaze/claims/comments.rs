//! `comment_claims` (advice): a body comment that states what the code can't do (`cannot
//! overflow because`, `unreachable`, `can't be None`) where nothing checks it. The comment is
//! a claim the next edit can make false without a word; the finding names the checked form
//! that says the same and fails when it stops being true. The comment is never rewritten: what
//! the checked form is depends on why the author believed it.
//!
//! A comment is left alone when the code it is on already checks it: its line or the next
//! line of code asserts, expects, says `unreachable!`, or uses a `strict_`, `checked_` or
//! `NonZero` form. A `SAFETY:` comment is `safety_without_invariant`'s.

use std::alloc::Global;

use syntax::{SyntaxKind, TextRange};

use super::super::{Lint, std_mistakes::report, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::CommentClaims;

/// The claims, each with the phrases (lowercase) that say it and the checked form.
const CLAIMS: &[(&[&str], &str, &str)] = &[
    (
        &["cannot overflow", "can't overflow", "can not overflow", "won't overflow", "will not overflow", "never overflows"],
        "that an operation does not overflow",
        "`strict_add` (and its kin) panics where it would, `checked_add` with an `else` says what then, and `#[blaze::nopanic]` on the function has the compiler prove no overflow check is reachable",
    ),
    (
        &["unreachable", "can't happen", "cannot happen", "never happens", "can never happen"],
        "that a path is not taken",
        "`blaze::unreachable!()` there, which blaze proves no path reaches (`unreachable` is deny), or a type that leaves the case out",
    ),
    (
        &["can't be none", "cannot be none", "is never none", "never none", "is always some"],
        "that an `Option` holds a value",
        "`let Some(x) = .. else { blaze::unreachable!() }`, which blaze proves, or a type that is never `None`",
    ),
    (
        &["can't fail", "cannot fail", "never fails", "can never fail"],
        "that a call does not fail",
        "`let Ok(x) = .. else { blaze::unreachable!() }`, which blaze proves, or a signature that can't fail (`T`, or `Result<T, Infallible>`)",
    ),
    (
        &["always in bounds", "never out of bounds", "can't be out of bounds", "cannot be out of bounds"],
        "that an index is in bounds",
        "an `assert!` of the length before the loop, which the bounds checks fold into, or `#[blaze::nopanic]` on the function",
    ),
    (
        &["can't be zero", "cannot be zero", "is never zero", "never zero", "can't be empty", "cannot be empty", "is never empty"],
        "that a value is not zero or empty",
        "`NonZero<T>` or a non-empty type, so the type says it and every maker of one checks it",
    ),
];

/// What, on the comment's line or the next line of code, already checks the claim.
const CHECKED: &[&str] =
    &["assert", "expect(", "unreachable!", "strict_", "checked_", "NonZero", "requires", "ensures"];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let root = cx.root();
    if test_code::is_test_code(cx, root) {
        return;
    }
    let text = cx.text();
    let mut found = Vec::new_in(Global);
    for token in root.descendants_with_tokens().filter_map(|it| it.into_token()) {
        if token.kind() != SyntaxKind::COMMENT {
            continue;
        }
        let said = token.text();
        if !said.starts_with("//") || said.starts_with("///") || said.starts_with("//!") {
            continue;
        }
        let lower = said.to_ascii_lowercase();
        if lower.contains("safety:") {
            continue;
        }
        let Some((phrases, what, form)) =
            CLAIMS.iter().find(|(phrases, ..)| phrases.iter().any(|it| lower.contains(it)))
        else {
            continue;
        };
        let range = token.text_range();
        if checked_nearby(text, range) {
            continue;
        }
        let phrase = phrases.iter().find(|it| lower.contains(*it)).copied().unwrap_or_default();
        found.push((range, phrase, *what, *form));
    }
    for (range, phrase, what, form) in found {
        let message = format!(
            "this comment claims {what} (\"{phrase}\") where nothing checks it: the next edit can make it false without a word"
        );
        let note = format!("the checked form: {form}");
        report(cx, LINT, root, range, message, &note, None);
    }
}

/// Whether the comment at `range` of `text` is on, or right above, code that checks it.
fn checked_nearby(text: &str, range: TextRange) -> bool {
    let start = usize::from(range.start());
    let end = usize::from(range.end());
    let line_start = text[..start].rfind('\n').map_or(0, |it| it + 1);
    let line_end = text[end..].find('\n').map_or(text.len(), |it| end + it);
    let on = &text[line_start..line_end];
    // The next line that is not a comment or blank.
    let next = text[line_end..]
        .lines()
        .map(str::trim)
        .find(|it| !it.is_empty() && !it.starts_with("//"))
        .unwrap_or("");
    CHECKED.iter().any(|it| on.contains(it) || next.contains(it))
}

#[cfg(test)]
mod tests {
    use syntax::{TextRange, TextSize};

    use super::checked_nearby;

    #[test]
    fn a_comment_on_checked_code_is_left_alone() {
        let text = "// can't be None: set above\nlet x = y.expect(\"set\");\n// can't be None\nlet z = w.unwrap();\n";
        let first = TextRange::at(TextSize::new(0), TextSize::of("// can't be None: set above"));
        assert!(checked_nearby(text, first));
        let at = text.find("// can't be None\n").unwrap();
        let second = TextRange::at(TextSize::of(&text[..at]), TextSize::of("// can't be None"));
        assert!(!checked_nearby(text, second));
    }
}
