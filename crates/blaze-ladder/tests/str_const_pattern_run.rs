//! A `&str` CONSTANT AS A PATTERN, run whole (`run_with`, the new front end) to a status.
//!
//! The stop (t1800, tempo's tests): serde_json 1.0.150's `serialize_struct(self, name:
//! &'static str, ..)` (value/ser.rs:276) matches `match name { crate::number::TOKEN => ..,
//! _ => .. }` with `pub(crate) const TOKEN: &str = "$serde_json::private::Number"` (number.rs
//! :18), refused as "pattern 3: str is not built to LIR yet". Read in blaze-build walk/matches.rs
//! `test_pat_chosen`: every path pattern peeled the scrutinee's references, so the constant was
//! compared at `str` itself; and walk/matches/forms.rs `const_test` refused any constant of a
//! reference type.
//!
//! The rule. The Reference, "Binding modes": "Non-reference patterns include all patterns
//! except bindings, wildcard patterns (`_`), const patterns of reference types, and reference
//! patterns": a `&str` constant peels nothing (rustc_hir_typeck pat.rs `ResolvedPat::
//! adjust_mode`: a constant is `AdjustMode::Pass`, "These constants can be of a reference
//! type"); blaze-typeck rules/pat.rs `step_pat` types it so, and the builder now reads the
//! pattern's recorded type to decide. rustc_mir_build const_to_pat.rs makes a `&str` constant
//! `PatKind::Constant`, tested by `<str as PartialEq>::eq`: the same length, then the same bytes;
//! the builder compares it as a string literal pattern (forms.rs `bytes_test`).
//!
//! - [`a_str_constant_matches_its_own_text`]: the site, reduced: 3 when the constant matches.
//! - [`a_str_constant_does_not_match_other_text`]: the twin: a text of the same length that
//!   differs in its last byte, and a shorter one, both fall to `_`.
//! - [`a_literal_and_a_non_reference_constant_still_match`]: the control: the same text as a
//!   string literal pattern, and a `u8` constant under a tuple behind a reference (it peels as
//!   before).

const PRELUDE: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

const TOKEN: &str = \"$serde_json::private::Number\";

#[inline(never)]
fn pick(name: &'static str) -> i32 {
    match name {
        TOKEN => 3,
        _ => 4,
    }
}
";

/// The program `text` run whole: its status.
fn run_whole(text: &str) -> u8 {
    let files = [blaze_ladder::refusal::SourceFile { path: "src/main.rs", text }];
    let new = blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_ladder::run::OverflowChecks::Off };
    blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(new), blaze_own::testing::heap().shared())
        .unwrap_or_else(|e| panic!("the whole program's run: {e}"))
}

/// [`PRELUDE`], `items`, and a `main` returning `ret` (an `i32` expression).
fn program(items: &str, ret: &str) -> String {
    format!(
        "{PRELUDE}
{items}

#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {{
    {ret}
}}
"
    )
}

/// `pick(\"$serde_json::private::Number\")` takes the `TOKEN` arm: 3. Before the fix: "str
/// is not built to LIR yet" (the scrutinee peeled to `str`).
/// Prints "str const pattern site: run status 3".
#[test]
fn a_str_constant_matches_its_own_text() {
    let status = run_whole(&program("", "pick(\"$serde_json::private::Number\")"));
    assert_eq!(status, 3, "the `TOKEN` arm, taken for the constant's own text");
    println!("str const pattern site: run status {status}");
}

/// A text of the constant's length with another last byte (the word compare's tail), and a
/// shorter text (the length test), each fall to `_` (4): 3 when both did. A compare by the
/// pointer, by the length alone or skipping the tail would give 1, 2 or 0.
/// Prints "str const pattern twin: run status 3".
#[test]
fn a_str_constant_does_not_match_other_text() {
    let status = run_whole(&program(
        "",
        "let same_len = pick(\"$serde_json::private::Numbex\");
    let shorter = pick(\"$serde_json\");
    match (same_len, shorter) {
        (4, 4) => 3,
        (3, 4) => 1,
        (4, 3) => 2,
        _ => 0,
    }",
    ));
    assert_eq!(status, 3, "both other texts fall to `_`");
    println!("str const pattern twin: run status {status}");
}

/// The control: the same text as a string literal pattern matches (forms.rs `str_test`, now
/// through the shared `bytes_test`), and a `u8` constant under a tuple pattern behind a
/// reference peels the reference as before (a non-reference pattern): 3 when both did.
/// Prints "str const pattern control: run status 3".
#[test]
fn a_literal_and_a_non_reference_constant_still_match() {
    let status = run_whole(&program(
        "const FIVE: u8 = 5;

#[inline(never)]
fn literal(name: &'static str) -> i32 {
    match name {
        \"$serde_json::private::Number\" => 3,
        _ => 4,
    }
}

#[inline(never)]
fn first_is_five(pair: &(u8, u8)) -> i32 {
    match pair {
        (FIVE, _) => 3,
        _ => 4,
    }
}",
        "match (literal(\"$serde_json::private::Number\"), first_is_five(&(5, 9)), first_is_five(&(6, 5))) {
        (3, 3, 4) => 3,
        _ => 0,
    }",
    ));
    assert_eq!(status, 3, "the literal arm and the peeled `u8` constant");
    println!("str const pattern control: run status {status}");
}
