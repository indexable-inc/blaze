//! Tests for `src/native/re/constraint.rs`: which strings a regex
//! constraint accepts and what its identity depends on.

use super::*;
use crate::native::re::parser::parse_pattern;
use alloc::format;
use alloc::vec;

fn constraint(pattern: &str, fullmatch: bool) -> RegexConstraint {
    RegexConstraint::new(pattern, parse_pattern(pattern, 0).unwrap(), fullmatch, None)
}

fn cps(s: &str) -> Vec<u32> {
    s.chars().map(u32::from).collect()
}

#[test]
fn a_whole_match_constraint_accepts_exactly_the_strings_the_pattern_matches_whole() {
    let c = constraint("[a-z]+@[a-z]+", true);
    assert!(c.accepts(&cps("ab@cd")));
    assert!(!c.accepts(&cps("ab@cd!")));
    assert!(!c.accepts(&cps("!ab@cd")));
    assert!(!c.accepts(&cps("")));
    assert!(!c.accepts(&[0xD800]), "a surrogate is in no pattern");
}

#[test]
fn a_search_constraint_accepts_a_string_the_pattern_matches_anywhere_in() {
    let c = constraint("a.c", false);
    assert!(c.accepts(&cps("abc")));
    assert!(c.accepts(&cps("xxaxcxx")));
    assert!(!c.accepts(&cps("ab")));
    assert!(!c.fullmatch());
}

#[test]
fn the_simplest_string_is_set_once() {
    let c = constraint("a+", true);
    assert_eq!(c.simplest(), None);
    assert_eq!(
        c.simplest_or_init(|| Ok::<_, ()>(cps("a"))).unwrap(),
        cps("a")
    );
    assert_eq!(
        c.simplest_or_init(|| Ok::<_, ()>(cps("aa"))).unwrap(),
        cps("a")
    );
    assert_eq!(c.simplest(), Some(cps("a").as_slice()));
}

#[test]
fn identity_follows_the_pattern_its_flags_the_match_mode_and_the_alphabet() {
    use core::hash::{BuildHasher, Hash};
    fn hash_of(value: &impl Hash) -> u64 {
        rustc_hash::FxBuildHasher.hash_one(value)
    }
    let whole = constraint("a+", true);
    assert_eq!(whole, constraint("a+", true));
    assert_eq!(whole.identity(), constraint("a+", true).identity());
    assert_eq!(hash_of(&whole), hash_of(&constraint("a+", true)));
    assert_ne!(whole, constraint("a+", false));
    assert_ne!(whole.identity(), constraint("a+", false).identity());
    assert_ne!(hash_of(&whole), hash_of(&constraint("a+", false)));
    assert_ne!(whole, constraint("b+", true));
    assert_ne!(
        whole,
        RegexConstraint::new("a+", parse_pattern("(?i)a+", 0).unwrap(), true, None)
    );
    let letters = IntervalSet::new(vec![(u32::from('a'), u32::from('z'))]).unwrap();
    assert_ne!(
        whole,
        RegexConstraint::new("a+", parse_pattern("a+", 0).unwrap(), true, Some(&letters))
    );
    assert!(alloc::format!("{whole:?}").contains("RegexConstraint"));
}

#[test]
fn a_long_repeat_of_a_group_is_accepted_whole_and_anywhere() {
    let many = "ab".repeat(2000);
    assert!(constraint("(ab)+", true).accepts(&cps(&many)));
    assert!(constraint("(a|bc)+", true).accepts(&cps(&"abc".repeat(1000))));
    assert!(constraint("([ab]){400}", true).accepts(&cps(&"a".repeat(400))));
    assert!(!constraint("([ab]){400}", true).accepts(&cps(&"a".repeat(399))));
    let search = constraint("x(ab)+y", false);
    assert!(search.accepts(&cps(&format!("..x{many}y.."))));
    assert!(!search.accepts(&cps(&format!("..x{many}.."))));
}

#[test]
fn a_long_repeat_of_an_item_that_can_match_empty_is_accepted_whole_and_anywhere() {
    let many = "a".repeat(5000);
    assert!(constraint("(?:a|b?)*", true).accepts(&cps(&many)));
    assert!(constraint("(?:(a)|b?)*", true).accepts(&cps(&many)));
    assert!(!constraint("(?:a|b?)*", true).accepts(&cps(&format!("{many}c"))));
    let search = constraint("x(?:a|b?)*y", false);
    assert!(search.accepts(&cps(&format!("..x{many}y.."))));
    assert!(!search.accepts(&cps(&format!("..x{many}.."))));
}

#[test]
fn a_search_keeps_its_anchors_at_the_edges_of_the_text() {
    let start = constraint("^a", false);
    assert!(start.accepts(&cps("ab")));
    assert!(!start.accepts(&cps("ba")));
    let end = constraint("a$", false);
    assert!(end.accepts(&cps("ba")));
    assert!(!end.accepts(&cps("ab")));
    assert!(constraint("\\bab", false).accepts(&cps("x ab")));
    assert!(!constraint("\\bab", false).accepts(&cps("xab")));
}

#[test]
fn a_pattern_the_program_cannot_express_is_checked_by_the_matcher() {
    let c = constraint("(a)\\1", true);
    assert!(c.accepts(&cps("aa")));
    assert!(!c.accepts(&cps("ab")));
    let c = constraint("(a)\\1", false);
    assert!(c.accepts(&cps("xaax")));
    assert!(!c.accepts(&cps("xabx")));
}

#[test]
fn a_string_the_generator_drew_is_accepted_without_a_check() {
    let c = constraint("(a)\\1", true);
    assert!(!c.accepts(&cps("ab")));
    c.note_drawn(&cps("ab"));
    assert!(c.accepts(&cps("ab")));
}

#[test]
fn the_program_and_the_matcher_agree_on_what_a_constraint_accepts() {
    let texts = [
        "", "a", "b", "ab", "ba", "aab", "bba", "abab", "baba", "xx", "aaa", "aa",
    ];
    for pattern in [
        "(?:|a)*",
        "(?:a|)*",
        "(?:a*)*",
        "(?:a*)+b",
        "(?:x*?)*",
        "(?:|x)*?x",
        "(?:(?:|a)*|b)*c",
        "(?:(?=a)|.)*",
        "(?:(?=a)|.)+",
        "(?:(?!b)|.)*",
        "(?:(?:(?!b)|[ab])\\B|a)*",
        "(?:(?:\\B)*?|(?:(?<!b)|[ab]))(?<!b)",
        "(?:a?)*b",
        "(?:(?:a|)(?:b|))*",
        "^(?:|a)*$",
        "(?:|a)*\\b",
    ] {
        let parsed = parse_pattern(pattern, 0).unwrap();
        for fullmatch in [true, false] {
            let c = constraint(pattern, fullmatch);
            assert!(c.program.is_some(), "{pattern}");
            for text in texts {
                let chars: Vec<char> = text.chars().collect();
                assert_eq!(
                    c.accepts(&cps(text)),
                    matches(&parsed, fullmatch, &chars),
                    "{pattern} on {text:?} whole={fullmatch}"
                );
            }
        }
    }
}
