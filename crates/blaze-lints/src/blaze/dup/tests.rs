//! The duplicate lints: the pure parts (shapes, the paste rules, Jaccard, the diff, the type
//! fingerprint, the generalisation, the merge of sibling crates) over parsed text, a property
//! of shapes over generated names and literals, and each lint over a crate: where it fires,
//! where it does not, the fixes made and linted again clean.

use blaze_sim::{Config, Tape};
use span::Edition;
use syntax::{SourceFile, SyntaxKind, TextRange, view as ast};

use super::{
    index, near, paste, shape,
    unify::{self, HoleClass},
};
use crate::tests::{CLIPPY_OFF, STD, lint, offered_in};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, fn, iterator, iterators, slice, index, range, deref, eq, ord, add, mul, from, builtin_impls, str\n";
const MAIN: &str = "//- /src/lib.rs crate:main deps:std cfg:test\n";

fn header() -> String {
    format!("{MINICORE}{}{MAIN}", STD.trim_start_matches('\n'))
}

/// What `blaze::<lint_name>` alone says of `main`.
fn found(lint_name: &str, main: &str) -> String {
    lint(&format!("{}{}", header(), main.trim_start_matches('\n')), &flags(lint_name))
}

fn flags(lint_name: &str) -> String {
    format!("{CLIPPY_OFF} --warn=blaze::{lint_name}")
}

/// `f` of the functions, structs, enums and constants at the top of `text`, in order.
fn with_items<R>(text: &str, f: impl FnOnce(&[ast::Node<'_>]) -> R) -> R {
    let parse = SourceFile::parse(text, Edition::CURRENT);
    let root = parse.root();
    let items: Vec<ast::Node<'_>> = root
        .children()
        .filter(|it| {
            matches!(it.kind(), SyntaxKind::FN | SyntaxKind::STRUCT | SyntaxKind::ENUM | SyntaxKind::CONST)
        })
        .collect();
    f(&items)
}

/// The shape hash of each item of `text`.
fn alphas(text: &str) -> Vec<u64> {
    with_items(text, |items| {
        items
            .iter()
            .map(|&it| shape::alpha(&shape::tokens(it), &mut shape::Renumber::default()))
            .collect()
    })
}

#[test]
fn renamed_locals_keep_a_shape_and_one_literal_changes_it() {
    let shapes = alphas(
        r#"
fn a(items: &[u32], limit: u32) -> u32 { let mut total = 0; for item in items { if *item < limit { total = total + *item; } } total }
fn b(values: &[u32], cap: u32) -> u32 { let mut sum = 0; for value in values { if *value < cap { sum = sum + *value; } } sum }
fn c(values: &[u32], cap: u32) -> u32 { let mut sum = 1; for value in values { if *value < cap { sum = sum + *value; } } sum }
fn d(values: &[u32], cap: u32) -> u32 { let mut sum = 0; for value in values { if *value < cap { sum = sum + value.len(); } } sum }
"#,
    );
    assert_eq!(shapes[0], shapes[1], "a rename of every local is one shape");
    assert_ne!(shapes[1], shapes[2], "one literal differing is another shape");
    assert_ne!(shapes[1], shapes[3], "a method in place of a deref is another shape");
}

/// The invariant: renaming the locals of a function never changes its shape, and changing
/// one literal always does.
#[test]
fn a_shape_is_the_code_up_to_its_locals_names() {
    blaze_sim::check(Config::cases(64), |tape| {
        let names = ["alpha", "beta", "gamma", "delta", "eps", "zeta", "eta", "theta"].map(String::from);
        let pick = |tape: &Tape| -> String { tape.one_of(&names, "name").clone() };
        let (x, y, z) = (pick(tape), pick(tape), pick(tape));
        if x == y || y == z || x == z {
            return;
        }
        let value: u64 = tape.int_in(0..=1 << 40, "value");
        let other: u64 = tape.int_in(0..=1 << 40, "other");
        let write = |x: &str, y: &str, z: &str, value: u64| {
            format!("fn f({x}: u64, {y}: u64) -> u64 {{ let {z} = {x} + {value}; if {z} > {y} {{ {z} * {y} }} else {{ {x} }} }}\n")
        };
        let shapes = alphas(&format!(
            "{}{}{}",
            write("a", "b", "c", value),
            write(&x, &y, &z, value),
            write(&x, &y, &z, other)
        ));
        assert_eq!(shapes[0], shapes[1], "renamed to {x}, {y}, {z}");
        assert_eq!(shapes[1] == shapes[2], value == other, "{value} and {other}");
    });
}

/// The tokens of each top block (`{ .. }` statement of the body) of the first function.
fn blocks_of(text: &str, f: impl FnOnce(&[shape::Tok], &[TextRange])) {
    with_items(text, |items| {
        let toks = shape::tokens(items[0]);
        let ranges: Vec<TextRange> = items[0]
            .descendants()
            .filter(|it| it.kind() == SyntaxKind::IF_EXPR)
            .map(|it| it.text_range())
            .collect();
        f(&toks, &ranges);
    });
}

#[test]
fn a_rename_left_unfinished_is_a_mismatch_one_way() {
    blocks_of(
        r#"
fn f(left: R, right: R, mut total: u32) -> u32 {
    if left.w > 0 { total = total + left.w * left.h + left.d; total = total - left.h; }
    if right.w > 0 { total = total + right.w * right.h + left.d; total = total - right.h; }
    total
}
"#,
        |toks, ifs| {
            let (a, b) = (shape::within(toks, ifs[0]), shape::within(toks, ifs[1]));
            let found = paste::mismatch(a, b).expect("`left.d` is the rename not finished");
            assert_eq!((found.renamed, found.kept), (4, 1));
            assert_eq!(paste::mismatch(b, a), None, "the original is no copy of the copy");
        },
    );
    blocks_of(
        r#"
fn f(left: R, right: R, mut total: u32) -> u32 {
    if left.w > 0 { total = total + left.w * left.h + left.d; total = total - left.h; }
    if right.w > 0 { total = total + right.w * left.h + left.d; total = total - left.h; }
    total
}
"#,
        |toks, ifs| {
            let (a, b) = (shape::within(toks, ifs[0]), shape::within(toks, ifs[1]));
            assert_eq!(paste::mismatch(a, b), None, "renamed once and kept three times is a choice");
        },
    );
}

#[test]
fn the_row_that_repeats_the_row_above_is_odd() {
    with_items("fn f(x: &mut P, y: &P) { x.a = y.a; x.b = y.b; x.c = y.b; x.d = y.d; }", |items| {
        let toks = shape::tokens(items[0]);
        let list = items[0].descendants().find(|it| it.kind() == SyntaxKind::STMT_LIST).expect("a body");
        let rows: Vec<&[shape::Tok]> =
            shape::statements(list).iter().map(|it| shape::within(&toks, it.text_range())).collect();
        let odd = paste::odd_rows(&rows);
        assert_eq!(odd.len(), 1, "{odd:?}");
        assert_eq!(odd[0].row, 2);
    });
    with_items("fn f(x: &mut P, y: &P) { x.a = y.b; x.b = y.c; x.c = y.d; }", |items| {
        let toks = shape::tokens(items[0]);
        let list = items[0].descendants().find(|it| it.kind() == SyntaxKind::STMT_LIST).expect("a body");
        let rows: Vec<&[shape::Tok]> =
            shape::statements(list).iter().map(|it| shape::within(&toks, it.text_range())).collect();
        assert!(paste::odd_rows(&rows).is_empty(), "a shift by one in every row is a pattern");
    });
}

#[test]
fn jaccard_and_the_diff_read_sets_and_lines() {
    assert_eq!(near::jaccard(&[1, 2, 3, 4], &[1, 2, 3, 4]), 100);
    assert_eq!(near::jaccard(&[1, 2, 3, 4], &[3, 4, 5, 6]), 33);
    assert_eq!(near::jaccard(&[], &[]), 100);
    let diff = near::diff("a\nb\nc\n", "a\nx\nc\n").expect("short bodies are diffed");
    assert_eq!(diff, "\n  + x\n  - b");
    let sketch = near::sketch(&[1, 2, 3]);
    assert_eq!(near::agreement(&sketch, &near::sketch(&[1, 2, 3])), near::HASHES);
}

#[test]
fn a_types_fingerprint_is_its_fields_as_a_set() {
    with_items(
        r#"
struct A { x: u32, y: u32, z: String }
struct B { z: String, x: u32, y: u32 }
struct C { x: u32, y: u32 }
enum D { One, Two, Three }
enum E { One, Two, Three, Four }
"#,
        |items| {
            let prints: Vec<_> = items.iter().map(|&it| index::fingerprint(it)).collect();
            let (a, b) = (prints[0].expect("three fields"), prints[1].expect("three fields"));
            assert_eq!(a.0, b.0, "one set of fields");
            assert_ne!(a.1, b.1, "in two orders");
            assert_eq!(prints[2], None, "two fields are a pair, not a type");
            assert_eq!(prints[3], None, "three unit variants are under the floor");
            assert!(prints[4].is_some_and(|it| it.2), "four are an enum's");
        },
    );
}

/// The generalisation of the first two functions of `text`.
fn generalised(text: &str) -> Option<unify::Generalised> {
    with_items(text, |items| {
        let (a, b) = (items[0], items[1]);
        unify::unify(a, b, &shape::tokens(a), &shape::tokens(b))
    })
}

#[test]
fn one_literal_used_three_times_is_one_hole() {
    let general = generalised(
        r#"
fn small(n: u32) -> u32 { let a = n + 64; let b = a * 64; if b > 64 { b } else { a } }
fn large(n: u32) -> u32 { let a = n + 256; let b = a * 256; if b > 256 { b } else { a } }
"#,
    )
    .expect("one parameter is under the bound");
    assert_eq!(general.holes.len(), 1, "{general:?}");
    assert_eq!((general.holes[0].class, general.holes[0].uses), (HoleClass::Literal, 3));
}

#[test]
fn a_renamed_local_is_no_hole_and_a_callee_is_a_path() {
    let general = generalised(
        r#"
fn one(n: u32) -> u32 { let a = n + 1; head(a) }
fn two(m: u32) -> u32 { let b = m + 1; tail(b) }
"#,
    )
    .expect("one parameter");
    assert_eq!(general.holes.len(), 1, "{general:?}");
    assert_eq!(general.holes[0].class, HoleClass::Path);
}

#[test]
fn types_alone_are_type_holes() {
    let general = generalised(
        r#"
fn sum32(xs: &[u32]) -> u32 { let mut t: u32 = 0; for x in xs { t = t + *x; } t }
fn sum64(xs: &[u64]) -> u64 { let mut t: u64 = 0; for x in xs { t = t + *x; } t }
"#,
    )
    .expect("one parameter, a type");
    assert_eq!(general.holes.len(), 1, "{general:?}");
    assert_eq!((general.holes[0].class, general.holes[0].uses), (HoleClass::Type, 3));
}

#[test]
fn swapped_statements_do_not_generalise() {
    let general = generalised(
        r#"
fn one(n: u32) -> u32 { let a = step(n, 1); log(a); a + n }
fn two(n: u32) -> u32 { log(n); let a = step(n, 1); a + n }
"#,
    );
    assert_eq!(general, None, "a different order of statements is no parameter");
}

#[test]
fn copies_in_two_crates_merge_and_one_crates_alone_do_not() {
    let a: &[(u64, u32)] = &[(1, 10), (5, 11), (9, 12)];
    let b: &[(u64, u32)] = &[(5, 20), (7, 21)];
    let c: &[(u64, u32)] = &[(5, 30), (9, 31), (9, 32)];
    let own: &[(u64, u32)] = &[(7, 40), (7, 41)];
    let merged = index::merge_siblings(&[("a", a), ("b", b), ("c", c)]);
    let shown: Vec<(u64, Vec<(&str, u32)>)> =
        merged.iter().map(|it| (it.hash, it.items.iter().copied().collect())).collect();
    assert_eq!(
        shown,
        vec![(5, vec![("a", 11), ("b", 20), ("c", 30)]), (9, vec![("a", 12), ("c", 31), ("c", 32)])],
        "`7` is in `b` alone"
    );
    assert!(index::merge_siblings(&[("own", own)]).is_empty(), "two copies in one crate are its own");
    assert!(merged.iter().all(|it| it.common_dep.is_none()), "the caller names the home");
}

#[test]
fn same_fn_names_both_copies_and_a_literal_apart_is_not_one() {
    let out = found(
        "same_fn",
        r#"
fn first(items: &[u32], limit: u32) -> u32 {
    let mut total = 0;
    for item in items { if *item < limit { total = total + *item; } }
    total
}
fn second(values: &[u32], cap: u32) -> u32 {
    let mut sum = 0;
    for value in values { if *value < cap { sum = sum + *value; } }
    sum
}
fn third(values: &[u32], cap: u32) -> u32 {
    let mut sum = 1;
    for value in values { if *value < cap { sum = sum + *value; } }
    sum
}
pub fn all(xs: &[u32]) -> u32 { first(xs, 1) + second(xs, 2) + third(xs, 3) }
"#,
    );
    assert!(out.contains("`second` is the same function as `first`"), "{out}");
    assert!(!out.contains("`third` is the same"), "one literal apart is two functions:\n{out}");
    assert_eq!(out.matches("is the same function").count(), 1, "{out}");
}

#[test]
fn an_exported_copy_is_forwarded_and_lints_clean() {
    let main = r#"
pub fn first(items: &[u32], limit: u32) -> u32 {
    let mut total = 0;
    for item in items { if *item < limit { total = total + *item; } }
    total
}
pub fn second(values: &[u32], cap: u32) -> u32 {
    let mut sum = 0;
    for value in values { if *value < cap { sum = sum + *value; } }
    sum
}
"#;
    let after = offered_in(&header(), main.trim_start_matches('\n'), &flags("same_fn"));
    assert!(after.contains("pub fn second(values: &[u32], cap: u32) -> u32 { first(values, cap) }"), "{after}");
}

#[test]
fn a_block_is_reported_once_and_not_under_its_floor_or_again_inside() {
    let out = found(
        "same_block",
        r#"
pub fn one(xs: &[u32], scale: u32) -> u32 {
    let mut total = 0;
    for x in xs {
        if *x > 10 { total = total + *x * scale; total = total - 1; } else { total = total + 2; }
        if total > 1000 { total = total / 2; total = total + scale; } else { total = total * 3; }
    }
    total + 1
}
pub fn two(ys: &[u32], factor: u32) -> u64 {
    let mut total = 0;
    for x in ys {
        if *x > 10 { total = total + *x * factor; total = total - 1; } else { total = total + 2; }
        if total > 1000 { total = total / 2; total = total + factor; } else { total = total * 3; }
    }
    u64::from(total)
}
pub fn small(a: u32) -> u32 { if a > 3 { let b = a + 1; b * 2 } else { a } }
pub fn smaller(c: u32) -> u64 { u64::from(if c > 3 { let d = c + 1; d * 2 } else { c }) }
"#,
    );
    assert_eq!(out.matches("is written 2 times").count(), 1, "the loop, not its `if`s too:\n{out}");
    assert!(out.contains("a function made of it takes"), "{out}");
}

const PASTED: &str = r#"
pub struct R { pub w: u32, pub h: u32, pub d: u32 }
pub fn area(left: &R, right: &R) -> u32 {
    let mut total = 0;
    if left.w > 0 {
        total = total + left.w * left.h + left.d;
        total = total - left.h;
    }
    if right.w > 0 {
        total = total + right.w * right.h + left.d;
        total = total - right.h;
    }
    total
}
"#;

#[test]
fn a_paste_left_unfinished_is_found_and_renamed() {
    let out = found("paste_mismatch", PASTED);
    assert!(out.contains("`left` is left as it was in a copy that renames `left` to `right`"), "{out}");
    let after = offered_in(&header(), PASTED.trim_start_matches('\n'), &flags("paste_mismatch"));
    assert!(after.contains("right.w * right.h + right.d"), "{after}");
}

#[test]
fn an_expected_paste_is_kept() {
    let main = PASTED.replace(
        "pub fn area",
        "#[expect(blaze::paste_mismatch, reason = \"the depth is the left one's for both\")]\npub fn area",
    );
    let out = found("paste_mismatch", &main);
    assert!(!out.contains("paste_mismatch"), "{out}");
}

#[test]
fn a_row_that_repeats_the_row_above_is_found_and_fixed() {
    let main = r#"
pub struct P { pub a: u32, pub b: u32, pub c: u32 }
pub fn copy(x: &mut P, y: &P) {
    x.a = y.a;
    x.b = y.b;
    x.c = y.b;
}
"#;
    let out = found("paste_mismatch", main);
    assert!(out.contains("this row repeats `b` of the row above"), "{out}");
    let after = offered_in(&header(), main.trim_start_matches('\n'), &flags("paste_mismatch"));
    assert!(after.contains("x.c = y.c;"), "{after}");
}

#[test]
fn a_type_written_twice_is_found_and_a_pair_is_not() {
    let out = found(
        "same_type",
        r#"
pub struct Span { pub start: u32, pub end: u32, pub file: u32 }
pub struct Range { pub file: u32, pub start: u32, pub end: u32 }
pub struct Pair { pub start: u32, pub end: u32 }
pub struct Other { pub start: u32, pub end: u32 }
"#,
    );
    assert!(out.contains("`Range` has the fields of `Span`"), "{out}");
    assert!(out.contains("in different orders"), "{out}");
    assert!(!out.contains("`Other`"), "two fields are under the floor:\n{out}");
}

#[test]
fn a_constant_written_twice_is_found() {
    let out = found(
        "same_const",
        r#"
pub const GREETING: &str = "hello, the whole world";
pub const WELCOME: &str = "hello, the whole world";
pub const ONE: u32 = 1;
pub const UNIT: u32 = 1;
"#,
    );
    assert!(out.contains("`WELCOME` has the value of `GREETING`"), "{out}");
    assert!(!out.contains("`UNIT`"), "a short value is `magic_number`'s question:\n{out}");
}

#[test]
fn two_functions_apart_in_a_literal_and_a_callee_are_one() {
    let out = found(
        "extractable_duplicate",
        r#"
pub fn first(xs: &[u32]) -> u32 { xs[0] }
pub fn last(xs: &[u32]) -> u32 { xs[1] }
pub fn small(xs: &[u32], n: u32) -> u32 {
    let mut total = n;
    if total > 64 { total = total - 64; }
    total = total + first(xs);
    total
}
pub fn large(xs: &[u32], n: u32) -> u32 {
    let mut total = n;
    if total > 256 { total = total - 256; }
    total = total + last(xs);
    total
}
"#,
    );
    assert!(out.contains("`small` and `large` are one function with 2 parameters"), "{out}");
    assert!(out.contains("the literal (`64` / `256`, u32, in 2 places)"), "{out}");
}

#[test]
fn a_hole_of_two_types_is_not_extractable() {
    let out = found(
        "extractable_duplicate",
        r#"
pub fn show<T>(t: T) -> u32 { 0 }
pub fn one() -> u32 { let a = show(7); let b = show(a); let c = show(b); a + b + c }
pub fn two() -> u32 { let a = show("seven"); let b = show(a); let c = show(b); a + b + c }
"#,
    );
    assert!(!out.contains("one function with"), "`7` and `\"seven\"` are two types:\n{out}");
}

#[test]
fn tests_apart_in_their_literals_are_one_table() {
    let out = found(
        "tests_differ_by_literal",
        r#"
pub fn double(n: u32) -> u32 { n * 2 }
#[cfg(test)]
mod tests {
    use super::double;
    #[test]
    fn doubles_two() { let got = double(2); let want = 4; assert_eq!(got, want); }
    #[test]
    fn doubles_three() { let got = double(3); let want = 6; assert_eq!(got, want); }
    #[test]
    fn doubles_ten() { let got = double(10); let want = 20; assert_eq!(got, want); }
    #[test]
    fn halves() { let got = double(10) / 2; let want = 10; assert_eq!(got, want); }
}
"#,
    );
    assert!(out.contains("these 3 tests are one test over a table of 3 rows of 2 values"), "{out}");
    assert_eq!(out.matches("these 3 tests").count(), 1, "one finding, at the first:\n{out}");
}

#[test]
fn functions_apart_in_types_alone_are_one_generic_function() {
    let out = found(
        "impls_differ_by_type",
        r#"
pub fn sum32(xs: &[u32]) -> u32 { let mut t: u32 = 0; for x in xs { t = t + *x; } t }
pub fn sum64(xs: &[u64]) -> u64 { let mut t: u64 = 0; for x in xs { t = t + *x; } t }
pub fn max32(xs: &[u32]) -> u32 { let mut t: u32 = 0; for x in xs { if *x > t { t = *x; } } t }
"#,
    );
    assert!(out.contains("`sum64` is `sum32` with other types: the type (`u32` / `u64`, in 3 places)"), "{out}");
    assert!(!out.contains("`max32`"), "{out}");
}
