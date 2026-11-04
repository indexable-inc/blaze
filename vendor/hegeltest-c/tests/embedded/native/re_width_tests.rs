//! Direct-equivalent tests for `src/native/re/width.rs`. Expected widths
//! come from CPython's `re._parser.parse(p).getwidth()`, with CPython's
//! `MAXWIDTH` shown here as `None`.

use super::*;
use crate::native::re::parser::parse_pattern;
use alloc::format;
use alloc::string::String;

fn width(pattern: &str) -> (u64, Option<u64>) {
    get_width(&parse_pattern(pattern, 0).unwrap().pattern)
}

fn lookbehind_width(pattern: &str) -> (u64, Option<u64>) {
    let parsed = parse_pattern(pattern, 0).unwrap();
    let body = parsed
        .pattern
        .data
        .iter()
        .find_map(|op| match op {
            OpCode::Assert { direction: -1, p } => Some(p),
            _ => None,
        })
        .unwrap();
    get_width_in(body, &parsed.pattern)
}

#[test]
fn literals_and_classes_are_one_wide() {
    assert_eq!(width("abc"), (3, Some(3)));
    assert_eq!(width(r"[a-z]\d."), (3, Some(3)));
    assert_eq!(width("[^x]"), (1, Some(1)));
}

#[test]
fn anchors_and_lookarounds_are_zero_wide() {
    assert_eq!(width(r"^a$\b"), (1, Some(1)));
    assert_eq!(width("(?=abc)(?!x)(?<=y)(?<!zz)"), (0, Some(0)));
    assert_eq!(width("(?!)"), (0, Some(0)));
}

#[test]
fn branches_take_min_and_max() {
    assert_eq!(width("a|bc|def"), (1, Some(3)));
    assert_eq!(width("a|b*"), (0, None));
}

#[test]
fn repeats_scale_the_item() {
    assert_eq!(width("a{2,5}"), (2, Some(5)));
    assert_eq!(width("(ab){3}"), (6, Some(6)));
    assert_eq!(width("a+"), (1, None));
    assert_eq!(width("a*?"), (0, None));
    assert_eq!(width("a?+"), (0, Some(1)));
    assert_eq!(width("(?:)*"), (0, Some(0)));
}

#[test]
fn groups_and_atomic_groups_pass_through() {
    assert_eq!(width("(?:ab)(c)(?>de)"), (5, Some(5)));
    assert_eq!(width("(?i:ab)"), (2, Some(2)));
}

#[test]
fn backreferences_use_the_group_width() {
    assert_eq!(width(r"(ab)\1"), (4, Some(4)));
    assert_eq!(width(r"(a+)\1"), (2, None));
}

#[test]
fn shared_group_widths_do_not_expand_the_reference_tree() {
    let mut pattern = String::from("(a)");
    for group in 1..40 {
        pattern.push_str(&format!("(\\{group}\\{group})"));
    }
    pattern.push_str("(?<=\\40)");
    assert_eq!(lookbehind_width(&pattern), (1 << 39, Some(1 << 39)));
}

#[test]
fn conditional_groups() {
    assert_eq!(width("(a)?(?(1)bc|d)"), (1, Some(3)));
    assert_eq!(width("(a)?(?(1)bc)"), (0, Some(3)));
    assert_eq!(width("(a)?(?(1)b*|c)"), (0, None));
}

#[test]
fn a_nested_subpattern_takes_the_widths_of_the_groups_closed_before_it() {
    let parsed = parse_pattern(r"(ab)(?<=\1)c", 0).unwrap();
    let OpCode::Assert { p, .. } = &parsed.pattern.data[1] else {
        panic!("{:?}", parsed.pattern);
    };
    assert_eq!(get_width(p), (0, None));
    assert_eq!(
        get_width_within(p, &[None, Some((2, Some(2)))]),
        (2, Some(2))
    );
    assert_eq!(get_width_within(p, &[None, Some((1, None))]), (1, None));
}

#[test]
fn saturates_instead_of_overflowing() {
    assert_eq!(
        width("(?:(?:a{4294967294}){4294967294}){4294967294}"),
        (u64::MAX, Some(u64::MAX))
    );
}

#[test]
fn a_part_of_a_pattern_takes_the_widths_of_the_groups_anywhere_in_it() {
    assert_eq!(lookbehind_width(r"(a)(?<=\1)b"), (1, Some(1)));
    assert_eq!(lookbehind_width(r"(ab)(c)?(?<=\2)d"), (1, Some(1)));
    assert_eq!(
        lookbehind_width(r"(?:x|(a))(?>(b))(c)*(?(1)(d)|(e))(?=(f))(?!(g))(?<=\1\2\3\4\5\6\7)"),
        (7, Some(7))
    );
    assert_eq!(lookbehind_width(r"(?<=(a)b)c"), (2, Some(2)));
}

#[test]
fn a_reference_to_no_group_of_the_pattern_is_unbounded() {
    let reference = SubPattern {
        data: alloc::vec![OpCode::GroupRef(1)],
    };
    let empty = SubPattern { data: Vec::new() };
    assert_eq!(get_width_in(&reference, &empty), (0, None));
}
