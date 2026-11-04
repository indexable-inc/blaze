//! Direct-equivalent tests for `src/native/re/matcher.rs`.
//!
//! Every expected end position and fullmatch verdict below was produced by
//! running CPython 3.14's `re.compile(pattern).match(text, pos)` /
//! `.fullmatch(text)` on the same inputs and transcribing the results, so
//! the tests check agreement with CPython's engine rather than with the
//! matcher's own reading of the semantics.

use super::*;
use crate::native::re::constants::{SRE_FLAG_ASCII, SRE_FLAG_UNICODE, SRE_FLAG_VERBOSE};
use crate::native::re::parser::parse_pattern;
use crate::native::re::vm::Program;
use alloc::string::String;
use alloc::vec;

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn match_at(matcher: &Matcher, text: &[char], pos: usize) -> Result<Option<usize>, MatchError> {
    matcher.match_at_touch(text, pos).map(|(end, _)| end)
}

fn end(pattern: &str, text: &str, pos: usize) -> Option<usize> {
    let parsed = parse_pattern(pattern, 0).unwrap();
    match_at(&Matcher::new(&parsed), &chars(text), pos).unwrap()
}

fn full(pattern: &str, text: &str) -> bool {
    let parsed = parse_pattern(pattern, 0).unwrap();
    Matcher::new(&parsed).fullmatch(&chars(text)).unwrap()
}

/// Checks `match_at` and `fullmatch` against one CPython observation,
/// and the simulation in [`vm`](crate::native::re::vm) too when it can
/// express the pattern.
fn check(pattern: &str, text: &str, pos: usize, expected_end: Option<usize>, expected_full: bool) {
    assert_eq!(
        end(pattern, text, pos),
        expected_end,
        "match end of {pattern:?} on {text:?} at {pos}"
    );
    if pos == 0 {
        assert_eq!(
            full(pattern, text),
            expected_full,
            "fullmatch of {pattern:?} on {text:?}"
        );
    }
    let parsed = parse_pattern(pattern, 0).unwrap();
    if let Ok(program) = Program::compile(&parsed) {
        let text = chars(text);
        assert_eq!(
            program.scan(&text, pos, None).0,
            expected_end,
            "simulated match end of {pattern:?} on {text:?} at {pos}"
        );
        if pos == 0 {
            assert_eq!(
                program.scan(&text, 0, Some(text.len())).0.is_some(),
                expected_full,
                "simulated fullmatch of {pattern:?} on {text:?}"
            );
        }
    }
}

#[test]
fn alternation_is_leftmost_first_not_longest() {
    check("a|ab", "ab", 0, Some(1), true);
    check("(a|ab)c", "abc", 0, Some(3), true);
    check("(?>a|ab)c", "abc", 0, None, false);
    check("(a|ab)(c|bcd)(d*)", "abcd", 0, Some(4), true);
    check("(?i)(?:a|ab)(?:c|bcd)(?:d*)", "abcd", 0, Some(4), true);
    check("(a)|(b)", "b", 0, Some(1), true);
    check("a(?!)|b", "b", 0, Some(1), true);
}

#[test]
fn greedy_and_lazy_single_character_repeats() {
    check("a*b", "aaab", 0, Some(4), true);
    check("a*", "aaa", 0, Some(3), true);
    check("a*?", "aaa", 0, Some(0), true);
    check("a*?b", "aaab", 0, Some(4), true);
    check("a{2,3}", "aaaa", 0, Some(3), false);
    check("a{2,3}?", "aaaa", 0, Some(2), false);
    check("a{0,2}", "aaa", 0, Some(2), false);
    check("a{3}", "aa", 0, None, false);
    check("a??b", "ab", 0, Some(2), true);
    check("a??", "a", 0, Some(0), true);
    check("x*?y", "xxy", 0, Some(3), true);
    check(".*?b", "aab", 0, Some(3), true);
    check("[a-c]+?c", "abc", 0, Some(3), true);
    check("[a-c]+", "abcd", 0, Some(3), false);
    check("[a-c]{2,}?", "abcd", 0, Some(2), false);
    check("[a-c]{2}", "ab", 0, Some(2), true);
    check("[a-c]{3}", "ab", 0, None, false);
    check("(?i)[a-c]*", "ABCd", 0, Some(3), false);
}

#[test]
fn greedy_and_lazy_compound_repeats() {
    check("(ab)*", "ababa", 0, Some(4), false);
    check("(?:ab)*?a", "aba", 0, Some(1), true);
    check("(?:ab){2}", "ababab", 0, Some(4), false);
    check("(?:ab){2}", "ab", 0, None, false);
    check("(?:ab){2,}?", "ababab", 0, Some(4), true);
    check("(?:ab){1,2}?c", "ababc", 0, Some(5), true);
    check("(?:ab){1}?ab", "abab", 0, Some(4), true);
    check("(?:ab)??ab", "ab", 0, Some(2), true);
    check("(?i)(?:ab)*", "ABab", 0, Some(4), true);
    check("(a|b)*c", "ababc", 0, Some(5), true);
    check("(?i:a)*b", "aAb", 0, Some(3), true);
    check("a{2,}?b", "ab", 0, None, false);
}

#[test]
fn repeats_of_unit_sequence_alternations() {
    check("(?:a|b)*c", "ababc", 0, Some(5), true);
    check("(?:a|[0-9]|.)+?b", "a1xb", 0, Some(4), true);
    check("(?:(?i:a)|b){2,3}", "AbA", 0, Some(3), true);
    check("(?:a|(?:b|c))*", "abcx", 0, Some(3), false);
    check("(?:a|ab)*c", "abc", 0, Some(3), true);
    check("(?:ab|a)*?c", "abac", 0, Some(4), true);
    check("(?:a|)*?b", "aab", 0, Some(3), true);
    check("(?:ab|cd){2,3}?e", "abcdabe", 0, Some(7), true);
    check("(?:ab|a){3}", "aab", 0, None, false);
    check("(?:ab|a){1,2}b", "aab", 0, Some(3), true);
    check("(?i)(?:AB|a)*", "abAB", 0, Some(4), true);
    check("(?:a|ab)*?c", "abc", 0, Some(3), true);
    check("(?:ab|a)*c", "aabc", 0, Some(4), true);
    check("(?:ab|a){2}?", "aab", 0, Some(3), true);
    check("(?:a|)*b", "b", 0, Some(1), true);
    check("(?:a|){2}b", "b", 0, Some(1), true);
    check("(?:a|){2}?b", "b", 0, Some(1), true);
    check("(?:xy|x)*?yz", "xyz", 0, Some(3), true);
    check("(?:ab|a)*?", "aab", 0, Some(0), true);
    check("(?:ab|a)+?b", "abb", 0, Some(3), true);
    check("(?:ab|a){1,1}?c", "abc", 0, Some(3), true);
    check("(?:ab){0}c", "c", 0, Some(1), true);
    check("(?:ab|a){0,1}ab", "ab", 0, Some(2), true);
    check("(?:[ab]c|a)+cb", "acb", 0, Some(3), true);
    check("(?:[ab]c|x)+ac", "acac", 0, Some(4), true);
    check("(?:[ab]c|a)+?cb", "acb", 0, Some(3), true);
    check("(?:[ab]c|a)+?cb", "acacb", 0, Some(5), true);
    check("(?:[ab]c|x)+?d", "acb", 0, None, false);
}

#[test]
fn empty_match_repeats_terminate() {
    check("(a*)*", "aaa", 0, Some(3), true);
    check("(a*)*b", "aaab", 0, Some(4), true);
    check("(a*)+", "", 0, Some(0), true);
    check("(a*)*", "b", 0, Some(0), false);
    check("(?:a|)*b", "aab", 0, Some(3), true);
    check("(?:)*", "x", 0, Some(0), false);
    check("(?:a*?)*", "aa", 0, Some(0), true);
    check("(a*?)*?b", "ab", 0, Some(2), true);
    check("(?:a*)*+b", "aab", 0, Some(3), true);
}

#[test]
fn possessive_and_atomic_never_give_back() {
    check("a*+a", "aaa", 0, None, false);
    check("a*a", "aaa", 0, Some(3), true);
    check("a*+", "aaa", 0, Some(3), true);
    check("(?:ab)*+ab", "abab", 0, None, false);
    check("(?>a*)a", "aaa", 0, None, false);
    check("(?:ab){2,}+", "ab", 0, None, false);
    check("(?:ab){1,2}+ab", "ababab", 0, Some(6), true);
    check("(?>(a))\\1", "aa", 0, Some(2), true);
    check("(?>ab)c", "abd", 0, None, false);
    check("(?>x)", "y", 0, None, false);
}

#[test]
fn backreferences() {
    check("(a+)\\1", "aaaa", 0, Some(4), true);
    check("(a+)\\1", "aaa", 0, Some(2), false);
    check("(a)|b\\1", "b", 0, None, false);
    check("(?P<x>a)(?P=x)", "aa", 0, Some(2), true);
    check("(?i)(a)\\1", "aA", 0, Some(2), true);
    check("(?i)(a)\\1", "ab", 0, None, false);
    check("(?ai)(a)\\1", "aA", 0, Some(2), true);
    check("(?i)(k)\\1", "k\u{212a}", 0, Some(2), true);
    check("(a)(b)?\\2", "a", 0, None, false);
    check("(?:(a)|b)*\\1", "aba", 0, Some(3), true);
    check("(?:(a)|b)*\\1", "ba", 0, None, false);
    check("(ab)\\1", "aba", 0, None, false);
}

#[test]
fn conditional_groups() {
    check("(a)?(?(1)b|c)", "ab", 0, Some(2), true);
    check("(a)?(?(1)b|c)", "c", 0, Some(1), true);
    check("(a)?(?(1)b|c)", "ac", 0, None, false);
    check("(a)?(?(1)b)", "ab", 0, Some(2), true);
    check("(a)?(?(1)b)", "x", 0, Some(0), false);
    check("(a)(b)?(?(2)x|y)", "ay", 0, Some(2), true);
}

#[test]
fn lookahead() {
    check("a(?=b)", "ab", 0, Some(1), false);
    check("a(?=b)", "ac", 0, None, false);
    check("a(?!b)", "ac", 0, Some(1), false);
    check("a(?!b)", "ab", 0, None, false);
    check("(?=(a+))a", "aaa", 0, Some(1), false);
    check("(?=(a))\\1", "a", 0, Some(1), true);
    check("(?!(a)b)(?(1)x|a)", "ac", 0, Some(1), false);
    check("(?!)", "a", 0, None, false);
    check("(?=a)b", "a", 0, None, false);
}

#[test]
fn lookbehind() {
    check("(?<=a)b", "ab", 1, Some(2), true);
    check("(?<=a)b", "ab", 0, None, false);
    check("(?<=a)b", "b", 0, None, false);
    check("(?<!a)b", "cb", 1, Some(2), true);
    check("(?<!a)b", "ab", 1, None, false);
    check("(?<=(a))b", "ab", 1, Some(2), true);
    check("(?<=ab)c", "abc", 2, Some(3), true);
    check("(?<!ab)c", "xbc", 2, Some(3), true);
    check("(?<!ab)c", "abc", 2, None, false);
    check("(?<=ab|cd)e", "cde", 2, Some(3), true);
    check("(?<=(?:ab){2})c", "ababc", 4, Some(5), true);
    check("(?<=(?>ab))c", "abc", 2, Some(3), true);
    check("(?<=a\\b)c", "a c", 2, None, false);
    check("(?<=^ab)c", "abc", 2, Some(3), true);
    check("(?<=[^a])b", "cb", 1, Some(2), true);
    check("(?<=(?!x)a)b", "ab", 1, Some(2), true);
    check("(a)(?<=\\1)b", "ab", 0, Some(2), true);
    check("(a)(?<=(?(1)a|b))b", "ab", 0, Some(2), true);
    check("(ab)(c)?(?<=\\2)d", "abd", 0, None, false);
    check("(?:(a))+?(?<=\\1)b", "ab", 0, Some(2), true);
    check("(?:(ab))+?(?<=\\1)c", "ababc", 0, Some(5), true);
    check("(?<=(?:)*a)b", "ab", 1, Some(2), true);
    check("(?<=ab)c", "c", 0, None, false);
}

#[test]
fn word_boundaries() {
    check("\\bfoo\\b", "foo", 0, Some(3), true);
    check("foo\\b", "foobar", 0, None, false);
    check("\\Bo", "fo", 1, Some(2), true);
    check("\\Ba", "a", 0, None, false);
    check("(?a)\\b\u{e9}", "\u{e9}", 0, None, false);
    check("\\b\u{e9}", "\u{e9}", 0, Some(1), true);
    check("a\\b", "a", 0, Some(1), true);
    check("\\b", "", 0, None, false);
    check("\\B", "", 0, Some(0), true);
    check("\\B", " ", 0, Some(0), false);
    check("(?a)\\B", "\u{e9}", 0, Some(0), false);
}

#[test]
fn anchors_with_and_without_multiline() {
    check("(?m)^b", "a\nb", 2, Some(3), true);
    check("^b", "a\nb", 2, None, false);
    check("(?m)a$", "a\nb", 0, Some(1), false);
    check("a$", "a\nb", 0, None, false);
    check("a$", "a\n", 0, Some(1), false);
    check("a\\Z", "a\n", 0, None, false);
    check("\\Aa", "aa", 1, None, false);
    check("(?m)a$", "a", 0, Some(1), true);
    check("\\Z", "", 0, Some(0), true);
    check("(?m)^", "", 0, Some(0), true);
    check("$", "", 0, Some(0), true);
    check("^a", "ba", 1, None, false);
    check("(?m)^a", "ba", 1, None, false);
    check("(?m)$", "a\nb", 1, Some(1), false);
}

#[test]
fn ignorecase_literals() {
    check("(?i)abc", "ABC", 0, Some(3), true);
    check("(?i)k", "\u{212a}", 0, Some(1), true);
    check("(?i)s", "\u{17f}", 0, Some(1), true);
    check("(?i)\u{df}", "\u{1e9e}", 0, Some(1), true);
    check("(?i)1", "1", 0, Some(1), true);
    check("(?i)\u{130}", "i", 0, Some(1), true);
    check("(?i)i", "\u{130}", 0, Some(1), true);
    check("(?i)\u{131}", "i", 0, Some(1), true);
    check("(?i)\u{1c6}", "\u{1c5}", 0, Some(1), true);
    check("(?ai)k", "K", 0, Some(1), true);
    check("(?ai)k", "\u{212a}", 0, None, false);
    check("(?ai)\u{e9}", "\u{c9}", 0, None, false);
    check("(?i)(?-i:a)", "A", 0, None, false);
    check("(?i)(?-i:a)a", "aA", 0, Some(2), true);
    check("(?i)(?a:k)", "K", 0, Some(1), true);
    check("(?i)a", "b", 0, None, false);
}

#[test]
fn ignorecase_sets() {
    check("(?i)[a-z]+", "AbC", 0, Some(3), true);
    check("(?i)[^k]", "\u{212a}", 0, None, false);
    check("(?i)[s]", "\u{17f}", 0, Some(1), true);
    check("(?i)[a-z]", "K", 0, Some(1), true);
    check("(?i)[K]", "k", 0, Some(1), true);
    check("(?i)[^a-z]", "K", 0, None, false);
    check("(?i)[\u{3a9}]", "\u{3c9}", 0, Some(1), true);
    check("(?i)[^_]", "_", 0, None, false);
    check("(?ai)[a-z]", "\u{212a}", 0, None, false);
    check("(?ai)[A-Z]", "k", 0, Some(1), true);
    check("(?i)[a-z]", "\u{17f}", 0, Some(1), true);
    check("(?i)[\u{17f}]", "s", 0, Some(1), true);
    check("(?i)[\u{1c6}]", "\u{1c5}", 0, Some(1), true);
    check("(?i)[\u{1c4}-\u{1c4}]", "\u{1c5}", 0, Some(1), true);
    check("(?i)[\u{e0}-\u{ff}]", "\u{c0}", 0, Some(1), true);
    check("(?i)[A-Z]", "k", 0, Some(1), true);
    check("(?i)[A-Z]", "\u{212a}", 0, Some(1), true);
    check("(?i)[^A-Z]", "\u{212a}", 0, None, false);
    check("(?i)[^a]", "A", 0, None, false);
    check("(?i)[^A]", "a", 0, None, false);
    check("(?i)[i]", "\u{130}", 0, Some(1), true);
    check("(?i)[\u{131}]", "I", 0, Some(1), true);
    check("(?i)[\\u212a]", "k", 0, Some(1), true);
    check("(?i)[^\\d]", "5", 0, None, false);
    check("(?i)[\\d]", "5", 0, Some(1), true);
    check("(?i)[^a-z\\d]", "5", 0, None, false);
    check("(?i)[^\\W]", "\u{c9}", 0, Some(1), true);
    check("(?i)[b-d]", "a", 0, None, false);
    check("(?i)[ab]", "B", 0, Some(1), true);
    check("(?i)[\u{17f}x]", "S", 0, Some(1), true);
    check("(?i)[^ab]", "A", 0, None, false);
}

#[test]
fn plain_sets_and_negated_literals() {
    check("[^a]", "b", 0, Some(1), true);
    check("[^a]", "\n", 0, Some(1), true);
    check("[^a]", "a", 0, None, false);
    check("[\\u00e9]", "\u{e9}", 0, Some(1), true);
    check("[a-c]", "b", 0, Some(1), true);
    check("[a-c]", "d", 0, None, false);
    check("\\101", "A", 0, Some(1), true);
    check("(?x) a b # c", "ab", 0, Some(2), true);
}

#[test]
fn dot_and_dotall() {
    check(".", "\n", 0, None, false);
    check("(?s).", "\n", 0, Some(1), true);
    check("(?s:.)", "\n", 0, Some(1), true);
    check(".", "a", 0, Some(1), true);
}

#[test]
fn categories_unicode_and_ascii() {
    check("\\w", "\u{e9}", 0, Some(1), true);
    check("(?a)\\w", "\u{e9}", 0, None, false);
    check("\\d", "\u{663}", 0, Some(1), true);
    check("(?a)\\d", "\u{663}", 0, None, false);
    check("\\s", "\u{a0}", 0, Some(1), true);
    check("(?a)\\s", "\u{a0}", 0, None, false);
    check("\\W", "\u{e9}", 0, None, false);
    check("\\D", "5", 0, None, false);
    check("\\S", " ", 0, None, false);
    check("[^\\W]", "\u{e9}", 0, Some(1), true);
    check("[\\d]", "\u{663}", 0, Some(1), true);
    check("(?a)[\\w]", "\u{e9}", 0, None, false);
    check("(?a)[\\s]", " ", 0, Some(1), true);
    check("(?a)[\\d]", "5", 0, Some(1), true);
    check("(?a)(?u:\\w)", "\u{e9}", 0, Some(1), true);
    check("\\s", "\u{1c}", 0, Some(1), true);
    check("(?a)\\S", "\u{a0}", 0, Some(1), true);
    check("(?a)\\D", "\u{663}", 0, Some(1), true);
    check("(?a)\\W", "\u{e9}", 0, Some(1), true);
    check("\\w", "_", 0, Some(1), true);
    check("(?a)\\w", "_", 0, Some(1), true);
}

#[test]
fn start_position_and_empty_inputs() {
    check("b", "ab", 1, Some(2), true);
    check("b", "ab", 5, None, false);
    check("", "", 0, Some(0), true);
    check("a", "", 0, None, false);
    assert!(!full("a*", "aab"));
    assert!(full("a|ab", "ab"));
}

#[test]
fn step_limit_fires_on_catastrophic_backtracking() {
    let parsed = parse_pattern("(a*)*b", 0).unwrap();
    let text = chars(&"a".repeat(30));
    let matcher = Matcher::new(&parsed).with_limits(10_000, DEFAULT_DEPTH_LIMIT);
    assert_eq!(match_at(&matcher, &text, 0), Err(MatchError::StepLimit));
    assert_eq!(matcher.fullmatch(&text), Err(MatchError::StepLimit));
}

#[test]
fn depth_limit_fires_on_long_repeats_of_capturing_items() {
    let parsed = parse_pattern("(?:(a)b)*", 0).unwrap();
    let text = chars(&"ab".repeat(100));
    let matcher = Matcher::new(&parsed).with_limits(DEFAULT_STEP_LIMIT, 10);
    assert_eq!(match_at(&matcher, &text, 0), Err(MatchError::DepthLimit));
    let relaxed = Matcher::new(&parsed);
    assert_eq!(match_at(&relaxed, &text, 0), Ok(Some(200)));
    let long = chars(&"ab".repeat(2000));
    assert_eq!(match_at(&relaxed, &long, 0), Err(MatchError::DepthLimit));
    let lazy = parse_pattern("(?:(a)b)*?$", 0).unwrap();
    assert_eq!(
        match_at(&Matcher::new(&lazy), &long, 0),
        Err(MatchError::DepthLimit)
    );
}

/// The default depth limit keeps the deepest recursion the matcher
/// allows (a repeat of a capturing alternation, which recurses several
/// levels per iteration) within a 1 MiB stack, the smallest a thread
/// commonly gets, in an unoptimised build too.
#[test]
fn the_depth_limit_fires_before_a_small_stack_overflows() {
    let parsed = parse_pattern("(?:(a)|(b))*", 0).unwrap();
    let text = chars(&"ab".repeat(2000));
    let worker = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let matcher = Matcher::new(&parsed);
            (match_at(&matcher, &text, 0), matcher.fullmatch(&text))
        })
        .unwrap();
    assert_eq!(
        worker.join().unwrap(),
        (Err(MatchError::DepthLimit), Err(MatchError::DepthLimit))
    );
}

#[test]
fn default_limits_handle_long_unit_sequence_repeats() {
    let parsed = parse_pattern("\"(?:\\\\.|[^\"\\\\])*\"", 0).unwrap();
    let mut text = String::from("\"");
    text.push_str(&"x\\\"".repeat(2000));
    text.push('"');
    let text = chars(&text);
    assert_eq!(
        match_at(&Matcher::new(&parsed), &text, 0),
        Ok(Some(text.len()))
    );
    let lazy = parse_pattern("\"(?:\\\\.|[^\"])*?\"", 0).unwrap();
    assert_eq!(
        match_at(&Matcher::new(&lazy), &text, 0),
        Ok(Some(text.len()))
    );
    let units = parse_pattern("(?:ab)*", 0).unwrap();
    let long = chars(&"ab".repeat(5000));
    assert_eq!(match_at(&Matcher::new(&units), &long, 0), Ok(Some(10_000)));
    let single = parse_pattern("[ab]*?$", 0).unwrap();
    assert_eq!(match_at(&Matcher::new(&single), &long, 0), Ok(Some(10_000)));
}

#[test]
fn errors_display() {
    assert_eq!(
        alloc::format!("{}", MatchError::StepLimit),
        "regex match exceeded its step limit"
    );
    assert_eq!(
        alloc::format!("{}", MatchError::DepthLimit),
        "regex match exceeded its nesting depth limit"
    );
}

#[test]
fn match_end_prefers_first_successful_alternative_with_fullmatch_backtracking() {
    let parsed = parse_pattern("(?:a|ab)(?:c|bcd)", 0).unwrap();
    let matcher = Matcher::new(&parsed);
    assert_eq!(match_at(&matcher, &chars("abcd"), 0), Ok(Some(4)));
    assert!(matcher.fullmatch(&chars("abcd")).unwrap());
    assert!(matcher.fullmatch(&chars("abc")).unwrap());
    assert!(!matcher.fullmatch(&chars("abce")).unwrap());
    assert_eq!(match_at(&matcher, &chars("abce"), 0), Ok(Some(3)));
}

#[test]
fn units_in_sequences() {
    check("a.c", "abc", 0, Some(3), true);
    check("a.c", "ab", 0, None, false);
    check("[^x]y", "ay", 0, Some(2), true);
    check("a[^x]", "a", 0, None, false);
    let items = vec![SetItem::Literal(0x61)];
    assert!(unit_matches(UnitOp::In(&items), 'a', 0));
    assert!(!unit_matches(UnitOp::NotLiteral(0x61), 'a', 0));
}

#[test]
fn unit_alternatives_recognises_only_unit_sequences() {
    let shapes = |pattern: &str| {
        let parsed = parse_pattern(pattern, 0).unwrap();
        let mut out = Vec::new();
        let ok = unit_alternatives(&parsed.pattern, parsed.flags, &mut out);
        (ok, out.iter().map(Vec::len).collect::<Vec<_>>())
    };
    assert_eq!(shapes("ab"), (true, vec![2]));
    assert_eq!(shapes("(?:ab|c|)"), (true, vec![2, 1, 0]));
    assert_eq!(shapes("(?:(?:ab)|(?:cd|e))"), (true, vec![2, 2, 1]));
    assert!(!shapes("(a)b").0);
    assert!(!shapes("a(?:b|cd)").0);
    assert!(!shapes("(?:a|(b))").0);
    assert!(!shapes("a*").0);
    assert!(single_unit(&parse_pattern("(?:(?:a))", 0).unwrap().pattern, 0).is_some());
    assert!(single_unit(&parse_pattern("(a)", 0).unwrap().pattern, 0).is_none());
    assert!(single_unit(&parse_pattern("ab", 0).unwrap().pattern, 0).is_none());
}

fn touch_at(pattern: &str, text: &str, pos: usize) -> (Option<usize>, Touch) {
    let parsed = parse_pattern(pattern, 0).unwrap();
    Matcher::new(&parsed)
        .match_at_touch(&chars(text), pos)
        .unwrap()
}

fn touch(pattern: &str, text: &str) -> (Option<usize>, Touch) {
    touch_at(pattern, text, 0)
}

#[test]
fn touch_reports_how_a_match_depended_on_the_edges_of_the_text() {
    let unicode = SRE_FLAG_UNICODE;
    assert_eq!(touch("abc", "abc"), (Some(3), Touch::Never));
    assert_eq!(
        touch("abc", "ab"),
        (
            None,
            Touch::Unless(vec![Probe::Literal('c' as u32, unicode)])
        )
    );
    assert_eq!(touch("\"[^\"]*\"", "\"ab\""), (Some(4), Touch::Never));
    assert!(matches!(touch("a*", "aa"), (Some(2), Touch::Tail(_))));
    assert!(matches!(touch("(?:a*)*", "aa"), (Some(2), Touch::Tail(_))));
    assert!(matches!(
        touch("a*b*", "aa"),
        (Some(2), Touch::Unless(probes)) if matches!(probes[..], [Probe::Run(_), Probe::Run(_)])
    ));
    assert_eq!(
        touch("a{1,3}", "aa"),
        (
            Some(2),
            Touch::Unless(vec![Probe::Literal('a' as u32, unicode)])
        )
    );
    assert_eq!(
        touch("a(?:.|b)?", "a"),
        (
            Some(1),
            Touch::Unless(vec![
                Probe::Any(unicode),
                Probe::Literal('b' as u32, unicode)
            ])
        )
    );
    assert_eq!(
        touch("a[^b]?[^b]?", "a"),
        (
            Some(1),
            Touch::Unless(vec![Probe::NotLiteral('b' as u32, unicode)])
        )
    );
    assert!(matches!(
        touch("[a-z]+(?<=a)", "abc"),
        (Some(1), Touch::Unless(probes)) if matches!(probes[..], [Probe::Run(_)])
    ));
    assert!(matches!(
        touch("a*(?!)", "aa"),
        (None, Touch::Unless(probes)) if matches!(probes[..], [Probe::Run(_)])
    ));
    assert!(matches!(
        touch("[ab]*a*", "a"),
        (Some(1), Touch::Unless(probes)) if matches!(probes[..], [Probe::Run(_), Probe::Run(_)])
    ));
    assert!(matches!(touch("x?a*", "a"), (Some(1), Touch::Tail(_))));
    assert_eq!(touch("a$", "a"), (Some(1), Touch::End));
    assert_eq!(touch("\\ba", "a"), (Some(1), Touch::End));
    assert_eq!(touch("x\\b", "x"), (Some(1), Touch::End));
    assert_eq!(touch("(?<=x)a", "xa"), (None, Touch::End));
    assert_eq!(touch("x(?=a*)", "x"), (Some(1), Touch::End));
    assert_eq!(touch("x(?=a)", "xa"), (Some(1), Touch::Never));
    assert_eq!(touch("(a)\\1", "a"), (None, Touch::End));
    assert_eq!(touch("a*?b", "aab"), (Some(3), Touch::Never));
    assert_eq!(touch("a*?", "aa"), (Some(0), Touch::Never));
    for (pattern, text, pos) in [
        ("b+B|", "b", 1),
        ("(?>b+)B|", "b", 1),
        ("(?>b*)(?!)|", "b", 1),
        ("(?:b*)++(?!)|", "b", 1),
        ("b*(?!)|", "b", 1),
        ("b*(?<=a)", "a", 1),
        ("(?m)b*^", "\n", 1),
        ("(b*)\\1", "", 0),
    ] {
        assert!(
            matches!(
                touch_at(pattern, text, pos),
                (Some(end), Touch::Unless(probes))
                    if end == text.len() && matches!(probes[..], [Probe::Run(_)])
            ),
            "{pattern:?} on {text:?} at {pos}: {:?}",
            touch_at(pattern, text, pos)
        );
    }
}

/// Every claim a touch makes about a longer text, checked against a
/// match of that longer text: a `Never` result stands with anything
/// before or after the text, a `Tail` match goes on through the run, and
/// an `Unless` result stands when the next character passes none of
/// the probes.
#[test]
fn touch_claims_hold_on_longer_texts() {
    let cases = [
        ("abc", "abc", 0),
        ("abc", "ab", 0),
        ("a*", "aa", 0),
        ("x?a*", "a", 0),
        ("(?:a*)*", "aa", 0),
        ("a*b*", "aa", 0),
        ("a{1,3}", "aa", 0),
        ("a(?:.|b)?", "a", 0),
        ("[a-z]+(?<=a)", "abc", 0),
        ("a*(?!)", "aa", 0),
        ("[ab]*a*", "a", 0),
        ("a$", "a", 0),
        ("\\ba", "a", 0),
        ("(?<=x)a", "xa", 1),
        ("x(?=a*)", "x", 0),
        ("x(?=a)", "xa", 0),
        ("a*?b", "aab", 0),
        ("a*?", "aa", 0),
        ("(?:a|b)*c", "ab", 0),
        ("\"[^\"]*\"", "\"ab\"", 0),
        ("(a)\\1", "a", 0),
        ("(b*)\\1", "", 0),
        ("b*(?<=b)", "b", 0),
        ("b*(?<=a)", "a", 1),
        ("(?m)b*^", "\n", 1),
        ("b+B|", "b", 1),
        ("(?>b+)B|", "b", 1),
        ("(?>b*)(?!)|", "b", 1),
        ("(?:b*)++(?!)|", "b", 1),
        ("b*(?!)|", "b", 1),
        ("(?:ab|a)*", "aba", 0),
        ("(?i)a*", "aA", 0),
        ("a*+", "a", 0),
        ("(?:a*)++b?", "a", 0),
        ("(a*)(?(1)b|c)", "a", 0),
        ("a*\\Z", "a", 0),
        ("[a-z]+\\b", "ab", 0),
        ("(?>a*)b", "a", 0),
        ("(?>a*)", "a", 0),
        ("(?:a*)++", "a", 0),
    ];
    let extensions = ["", "a", "b", "x", "\n", "B", "ab", "ba", "bb", "aa", "b\n"];
    let mut seen = [0usize; 4];
    for (pattern, text, pos) in cases {
        let parsed = parse_pattern(pattern, 0).unwrap();
        let matcher = Matcher::new(&parsed);
        let text = chars(text);
        let (end, touch) = matcher.match_at_touch(&text, pos).unwrap();
        seen[touch.stamp().0 as usize] += 1;
        for extension in extensions {
            let mut longer = text.clone();
            longer.extend(extension.chars());
            let actual = match_at(&matcher, &longer, pos).unwrap();
            let claimed = match &touch {
                Touch::Never => Some(end),
                Touch::Tail(run_id) => {
                    let more = run_len(&parsed, *run_id, &longer, text.len()).unwrap();
                    Some(Some(text.len() + more))
                }
                Touch::Unless(probes) => extension
                    .chars()
                    .next()
                    .is_none_or(|c| {
                        !probes
                            .iter()
                            .any(|probe| probe_matches(&parsed, probe, c).unwrap())
                    })
                    .then_some(end),
                Touch::End => None,
            };
            if let Some(claimed) = claimed {
                assert_eq!(
                    actual, claimed,
                    "{pattern:?} on {text:?} at {pos} then {extension:?}: {touch:?}"
                );
            }
        }
        if touch == Touch::Never {
            for prefix in ["a", "b", "x", "\n"] {
                let mut longer = chars(prefix);
                longer.extend(&text);
                assert_eq!(
                    match_at(&matcher, &longer, pos + 1).unwrap(),
                    end.map(|end| end + 1),
                    "{pattern:?} on {text:?} at {pos} after {prefix:?}"
                );
            }
        }
    }
    assert!(seen.iter().all(|&count| count > 0), "{seen:?}");
}

fn tail_len(pattern: &str, text: &str, longer: &str) -> Option<usize> {
    let parsed = parse_pattern(pattern, 0).unwrap();
    let text = chars(text);
    let (end, touch) = Matcher::new(&parsed).match_at_touch(&text, 0).unwrap();
    assert_eq!(end, Some(text.len()), "{pattern:?} on {text:?}");
    let Touch::Tail(run_id) = touch else {
        panic!("{pattern:?} on {text:?} gave {touch:?}");
    };
    let longer = chars(longer);
    let full = match_at(&Matcher::new(&parsed), &longer, 0).unwrap();
    let extended = run_len(&parsed, run_id, &longer, text.len()).map(|more| text.len() + more);
    assert_eq!(extended, full, "{pattern:?} on {longer:?}");
    extended
}

fn probes(pattern: &str, text: &str) -> (ParsedPattern, Vec<Probe>) {
    let parsed = parse_pattern(pattern, 0).unwrap();
    let (_, touch) = Matcher::new(&parsed)
        .match_at_touch(&chars(text), 0)
        .unwrap();
    let Touch::Unless(probes) = touch else {
        panic!("{pattern:?} on {text:?} gave {touch:?}");
    };
    (parsed, probes)
}

#[test]
fn probes_run_their_tests_again_on_the_next_character() {
    for (pattern, text, passing, failing) in [
        ("x[ab]?", "x", "ab", "cx"),
        ("x[^b]?", "x", "acx", "b"),
        ("x(?:[ab])?", "x", "ab", "c"),
        ("x(?:[ab]|c)?", "x", "abc", "d"),
        ("x(?:[ab]y|cz)?", "x", "abc", "d"),
        ("x(?>[ab])?", "x", "ab", "c"),
        ("xa(?<=a)[bc]?", "xa", "bc", "d"),
        ("xa(?<!b)[bc]?", "xa", "bc", "d"),
        ("x[ab]*?y", "x", "aby", "c"),
        ("(a)?(?(1)[bc]|d)", "a", "bc", "ad"),
        ("(a)?(?(1)b|[cd])", "", "acd", "b"),
        ("x(?i:[ab])?", "x", "abAB", "c"),
        ("[a-z]+[0-9]?", "ab", "z5", "-"),
        ("a.?", "a", "x", "\n"),
        ("(?s)a.?", "a", "x\n", ""),
    ] {
        let (parsed, probes) = probes(pattern, text);
        let matched = |c: char| {
            probes
                .iter()
                .any(|probe| probe_matches(&parsed, probe, c).unwrap())
        };
        for c in passing.chars() {
            assert!(matched(c), "{pattern:?} on {text:?} then {c:?}");
        }
        for c in failing.chars() {
            assert!(!matched(c), "{pattern:?} on {text:?} then {c:?}");
        }
    }
    let parsed = parse_pattern("a", 0).unwrap();
    assert_eq!(probe_matches(&parsed, &Probe::In(0, 0), 'a'), None);
    assert_eq!(probe_matches(&parsed, &Probe::Run(0), 'a'), None);
}

#[test]
fn run_len_continues_a_tail_run_through_every_construct() {
    assert_eq!(tail_len("a*", "aa", "aaab"), Some(3));
    assert_eq!(tail_len("x|a*", "aa", "aa"), Some(2));
    assert_eq!(tail_len("(a*)", "a", "aaa"), Some(3));
    assert_eq!(tail_len("(x)?(?(1)a*|b*)", "xa", "xaa"), Some(3));
    assert_eq!(tail_len("(x)?(?(1)a*|b*)", "b", "bbc"), Some(2));
    assert_eq!(tail_len("(?>a*)", "a", "aa"), Some(2));
    assert_eq!(tail_len("(?:a*)++", "aa", "aaa"), Some(3));
    assert_eq!(tail_len("(?:a*)+?", "aa", "aaaa"), Some(4));
    assert_eq!(tail_len("(?i)a*", "aA", "aAa"), Some(3));
    let parsed = parse_pattern("a*", 0).unwrap();
    assert_eq!(run_len(&parsed, usize::MAX, &chars("aaa"), 0), None);
    assert_eq!(
        run_len(&parse_pattern("a", 0).unwrap(), 0, &chars("a"), 0),
        None
    );
}

#[test]
fn repeat_iterations_ending_where_an_earlier_one_failed_are_not_retried() {
    check("(?:ab|ab?)*c", "ababx", 0, None, false);
    check("(?:ab|ab?)*c", "ababc", 0, Some(5), true);
    check("(?:ab|ab?){2,}c", "ababx", 0, None, false);
    check("(?:ab|ab?)*?c", "ababx", 0, None, false);
    check("(?:ab|ab?)*?c", "ababc", 0, Some(5), true);
    check("(?:ab|ab?){2,}?c", "ababx", 0, None, false);
}

#[test]
fn literal_sequences_are_plain_literals_inside_flag_groups() {
    let unicode = SRE_FLAG_UNICODE;
    let sequence = |pattern: &str| literal_sequence(&parse_pattern(pattern, 0).unwrap());
    assert_eq!(sequence("abc"), Some((vec![97, 98, 99], unicode)));
    assert_eq!(
        sequence("(?i:abc)"),
        Some((vec![97, 98, 99], unicode | SRE_FLAG_IGNORECASE))
    );
    assert_eq!(
        sequence("(?s:(?i:ab))"),
        Some((
            vec![97, 98],
            unicode | SRE_FLAG_IGNORECASE | SRE_FLAG_DOTALL
        ))
    );
    assert_eq!(
        sequence("(?x:a\\ b)"),
        Some((vec![97, 32, 98], unicode | SRE_FLAG_VERBOSE))
    );
    assert_eq!(sequence("(?a:ab)"), Some((vec![97, 98], SRE_FLAG_ASCII)));
    assert_eq!(sequence("(?i)(?-i:ab)"), Some((vec![97, 98], unicode)));
    assert_eq!(
        sequence("(?i:)"),
        Some((vec![], unicode | SRE_FLAG_IGNORECASE))
    );
    assert_eq!(sequence("a.c"), None);
    assert_eq!(sequence("(ab)"), None);
    assert_eq!(sequence("(?i:a)b"), None);
    assert_eq!(sequence("(?i:ab)(?i:cd)"), None);
    assert_eq!(sequence("ab|cd"), None);
    assert_eq!(sequence("[ab]"), None);
    assert_eq!(sequence("a*"), None);
    assert_eq!(sequence("\\bab"), None);
}
