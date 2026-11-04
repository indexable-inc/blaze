//! Tests for `src/native/re/vm.rs`. The CPython-verified cases in
//! `re_matcher_tests.rs` run against the simulation too; these cover
//! what the simulation declines and what only it decides.

use super::*;
use crate::native::re::matcher::Matcher;
use crate::native::re::parser::parse_pattern;

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn program(pattern: &str) -> Result<Program, Unsupported> {
    Program::compile(&parse_pattern(pattern, 0).unwrap())
}

fn run(p: &Program, text: &[char], start: usize, exact_end: Option<usize>) -> Option<usize> {
    p.scan(text, start, exact_end).0
}

#[test]
fn declines_constructs_whose_matching_depends_on_captures_or_commitment() {
    for pattern in ["(a)\\1", "(a)?(?(1)b|c)", "(?>ab|a)c", "a*+b", "(?:a|b)++"] {
        assert_eq!(program(pattern).err(), Some(Unsupported), "{pattern}");
    }
}

/// Expected ends and fullmatch verdicts from CPython 3.14's
/// `re.compile(pattern).match(text).end()` / `.fullmatch(text)`.
#[test]
fn nullable_unbounded_repeats_stay_with_the_backtracking_matcher() {
    for (pattern, text, end, full) in [
        ("(?:|a)*", "a", Some(0), true),
        ("(?:a|)*", "a", Some(1), true),
        ("(a?)*", "aa", Some(2), true),
        ("(a|)+b", "aab", Some(3), true),
        ("(?:a*)*?", "aa", Some(0), true),
        ("(\\b)*", "a", Some(0), false),
        ("(?:(?=a))+", "a", Some(0), false),
        ("(?:|a)*b", "ab", Some(2), true),
        ("(?:a|b?)*", "aab", Some(3), true),
        ("(?:|a)*?b", "aab", Some(3), true),
        ("(?:a|)*?", "aa", Some(0), true),
        ("(?:|a){2,}", "aa", Some(0), true),
        ("(?:a|)+?a", "aa", Some(2), true),
        ("(?:a?b?)*c", "abac", Some(4), true),
        ("(?:a?b?)*", "abba", Some(4), true),
        ("(?:(?:a|)*b)*", "abb", Some(3), true),
        ("(?:a|)*?$", "aa", Some(2), true),
        ("(?:x*)*y", "xxy", Some(3), true),
        ("(?:x*?)*y", "xxy", Some(3), true),
        ("(?:x*?)*", "xx", Some(0), true),
        ("(?:|x)*?x", "xx", Some(1), true),
        ("(?:(?:|a)*|b)*c", "aabc", Some(4), true),
        ("(?:(?=a)|.)*", "baba", Some(1), true),
        ("(?:(?=a)|.)+", "bba", Some(2), true),
        ("(?:(?!b)|.)*", "bba", Some(2), true),
        ("(?:(?:(?!b)|[ab])\\B|a)*", "aaa", Some(1), true),
        ("(?:(?:\\B)*?|(?:(?<!b)|[ab]))(?<!b)", "aa", Some(0), false),
    ] {
        let parsed = parse_pattern(pattern, 0).unwrap();
        assert_eq!(Program::compile(&parsed).err(), Some(Unsupported));
        let text = chars(text);
        let p = Program::compile_recognizer(&parsed).unwrap();
        assert_eq!(
            run(&p, &text, 0, None).is_some(),
            end.is_some(),
            "{pattern}"
        );
        assert_eq!(
            run(&p, &text, 0, Some(text.len())).is_some(),
            full,
            "{pattern}"
        );
        let matcher = Matcher::new(&parsed);
        assert_eq!(
            matcher.match_at_touch(&text, 0).unwrap().0,
            end,
            "{pattern:?}"
        );
        assert_eq!(matcher.fullmatch(&text).unwrap(), full, "{pattern:?}");
    }
    let long = chars(&"a".repeat(5000));
    let p = Program::compile_recognizer(&parse_pattern("(?:(a)|b?)*", 0).unwrap()).unwrap();
    assert_eq!(run(&p, &long, 0, None), Some(5000));
    assert_eq!(run(&p, &long, 0, Some(5000)), Some(5000));
}

#[test]
fn declines_programs_that_unroll_too_far() {
    assert_eq!(program("(?:ab){3000}").err(), Some(Unsupported));
    assert_eq!(program("(?:ab){0,3000}").err(), Some(Unsupported));
    assert_eq!(program("a{100}(?=b{4000})").err(), Some(Unsupported));
    assert!(program("(?:ab){1000}").is_ok());
    assert_eq!(program("(?:){100000000}").err(), Some(Unsupported));
    assert_eq!(program("(){100000000}").err(), Some(Unsupported));
}

#[test]
fn start_is_clamped_to_the_text() {
    let p = program("a*").unwrap();
    assert_eq!(run(&p, &chars("aa"), 5, None), Some(2));
    assert_eq!(run(&p, &chars("aa"), 5, Some(2)), Some(2));
    assert_eq!(run(&p, &chars(""), 0, None), Some(0));
    assert_eq!(run(&program("a").unwrap(), &chars(""), 0, None), None);
}

#[test]
fn exact_end_backtracks_past_earlier_matches() {
    let p = program("(?:a|ab)(?:c|bcd)").unwrap();
    assert_eq!(run(&p, &chars("abcd"), 0, None), Some(4));
    assert_eq!(run(&p, &chars("abcd"), 0, Some(3)), Some(3));
    assert_eq!(run(&p, &chars("abcd"), 0, Some(2)), None);
    let p = program("a|ab|abc").unwrap();
    assert_eq!(run(&p, &chars("abcd"), 0, Some(2)), Some(2));
    assert_eq!(run(&p, &chars("abcd"), 0, Some(3)), Some(3));
    assert_eq!(run(&p, &chars("abcd"), 0, Some(4)), None);
}

#[test]
fn lookarounds_nest_and_look_behind_a_fixed_width() {
    let p = program("(?<=a(?=b)b)c").unwrap();
    assert_eq!(run(&p, &chars("abc"), 2, None), Some(3));
    assert_eq!(run(&p, &chars("xbc"), 2, None), None);
    let p = program("(?<=ab|xb)c").unwrap();
    assert_eq!(run(&p, &chars("abc"), 2, None), Some(3));
    assert_eq!(run(&p, &chars("xbc"), 2, None), Some(3));
    assert_eq!(run(&p, &chars("bc"), 1, None), None);
    assert_eq!(run(&p, &chars("c"), 0, None), None);
    let p = program("(?<!ab)c").unwrap();
    assert_eq!(run(&p, &chars("abc"), 2, None), None);
    assert_eq!(run(&p, &chars("xbc"), 2, None), Some(3));
    assert_eq!(run(&p, &chars("c"), 0, None), Some(1));
}

#[test]
fn an_impossible_alternative_is_skipped() {
    let p = program("(?:a(?!)|ab)c").unwrap();
    assert_eq!(run(&p, &chars("abc"), 0, None), Some(3));
    assert_eq!(run(&program("(?!)").unwrap(), &chars("a"), 0, None), None);
}

/// Every pattern that can be expressed finds the same ends as the
/// backtracking matcher at every position, and the same fullmatch
/// verdicts, over texts the patterns match and texts they do not.
#[test]
fn agrees_with_the_backtracking_matcher() {
    let patterns = [
        "\\/(?!\\/)(\\\\\\/|\\\\\\\\|[^\\/\\n])*?\\/[imslux]*",
        "(\\r?\\n)+\\s*",
        "\\s*//[^\\n]*",
        "\"(?:[^\"\\\\]|\\\\.)*\"",
        "\".*?(?<!\\\\)(\\\\\\\\)*?\"",
        "/\\*(\\*(?!/)|[^*])*\\*/",
        "[+*]|[?](?![a-z])",
        "(?i)ab+c?|d{2,3}e*?f",
        "(?:ab|a)(?:bc|c)?d*(?=e|$)",
        "^[a-z_]\\w*\\b",
        "(?m)^#.*$",
        "x{2,4}?y|x+",
    ];
    let texts = [
        "/ab\\/c/i rest",
        "//x\n\n  \r\n\t|",
        "\"a\\\"b\\\\\"c\"",
        "/* a * b **/ x",
        "? ?a *",
        "ABbbC dde ddfe",
        "abcd abcde acd",
        "foo_bar1 baz",
        "#c\n#d\nx",
        "xxxxy xy",
        "",
    ];
    for pattern in patterns {
        let parsed = parse_pattern(pattern, 0).unwrap();
        let program = Program::compile(&parsed).unwrap();
        let matcher = Matcher::new(&parsed);
        for text in texts {
            let text = chars(text);
            for pos in 0..=text.len() {
                let expected = matcher.match_at_touch(&text, pos).unwrap().0;
                assert_eq!(
                    run(&program, &text, pos, None),
                    expected,
                    "{pattern:?} {text:?} {pos}"
                );
            }
            for end in 0..=text.len() {
                let prefix = &text[..end];
                assert_eq!(
                    run(&program, prefix, 0, Some(end)).is_some(),
                    matcher.fullmatch(prefix).unwrap(),
                    "{pattern:?} {prefix:?}"
                );
            }
        }
    }
}

#[test]
fn scan_reports_the_threads_the_end_of_the_text_left_waiting() {
    let newlines = program("(\\r?\\n)+\\s*").unwrap();
    let (end, touch, resume) = newlines.scan(&chars("\n"), 0, None);
    assert_eq!((end, touch), (Some(1), Touch::End));
    let resume = resume.unwrap();
    let longer = chars("\n\r\n  x");
    assert_eq!(newlines.resume(&resume, &longer, 0, 1).0, Some(5));
    assert_eq!(newlines.resume(&resume, &chars("\nx"), 0, 1).0, Some(1));
    let (end, touch, resume) = newlines.scan(&chars("\n "), 0, None);
    assert_eq!((end, touch), (Some(2), Touch::End));
    assert_eq!(
        newlines.resume(&resume.unwrap(), &chars("\n \n"), 0, 2).0,
        Some(3)
    );
    let closed = program("\"[^\"]*\"").unwrap();
    assert_eq!(
        closed.scan(&chars("\"a\""), 0, None),
        (Some(3), Touch::Never, None)
    );
    assert_eq!(closed.scan(&chars("\"a"), 0, None).2.unwrap().found, None);
    let boundary = program("\\bx").unwrap();
    assert_eq!(
        boundary.scan(&chars("x"), 0, None),
        (Some(1), Touch::End, None)
    );
    let behind = program("(?<=a)b*").unwrap();
    assert_eq!(behind.scan(&chars("b"), 0, None), (None, Touch::End, None));
    assert!(behind.scan(&chars("ab"), 1, None).2.is_some());
    let ahead = program("x(?=a*)").unwrap();
    assert_eq!(
        ahead.scan(&chars("x"), 0, None),
        (Some(1), Touch::End, None)
    );
    assert_eq!(
        ahead.scan(&chars("xb"), 0, None),
        (Some(1), Touch::Never, None)
    );
    let star = program("a*").unwrap();
    assert_eq!(
        star.scan(&chars("a"), 0, Some(1)),
        (Some(1), Touch::End, None)
    );
    assert_eq!(star.scan(&chars("a"), 5, None).0, Some(1));
}

#[test]
fn resume_continues_a_scan_as_a_fresh_run_would() {
    let patterns = [
        "(\\r?\\n)+\\s*",
        "\\s*//[^\\n]*",
        "\"(?:[^\"\\\\]|\\\\.)*\"",
        "[+*]|[?](?![a-z])",
        "(?i)ab+c?|d{2,3}e*?f",
        "(?:ab|a)(?:bc|c)?d*(?=e|$)",
        "^[a-z_]\\w*\\b",
        "(?m)^#.*$",
        "x{2,4}?y|x+",
        "[0-9]+(?:\\.[0-9]+)?(?:[eE][+-]?[0-9]+)?",
    ];
    let texts = [
        "//x\n\n  \r\n\t|",
        "\"a\\\"b\\\\\"c\"",
        "? ?a *",
        "ABbbC dde ddfe",
        "abcd abcde acd",
        "foo_bar1 baz",
        "#c\n#d\nx",
        "xxxxy xy",
        "12.5e-3 7. .5e",
        "",
    ];
    let (mut fixed, mut resumed, mut lost) = (0, 0, 0);
    for pattern in patterns {
        let program = program(pattern).unwrap();
        for text in texts {
            let text = chars(text);
            for split in 0..=text.len() {
                for start in 0..=split {
                    let full = program.scan(&text, start, None).0;
                    let (end, touch, resume) = program.scan(&text[..split], start, None);
                    match (touch, resume) {
                        (Touch::Never, None) => {
                            fixed += 1;
                            assert_eq!(end, full, "{pattern:?} {text:?} {start}..{split}");
                        }
                        (Touch::End, Some(resume)) => {
                            resumed += 1;
                            let carried = program.resume(&resume, &text, start, split - start);
                            assert_eq!(carried.0, full, "{pattern:?} {text:?} {start}..{split}");
                        }
                        (Touch::End, None) => lost += 1,
                        other => panic!("{pattern:?} {text:?} {start}..{split}: {other:?}"),
                    }
                }
            }
        }
    }
    assert!(
        fixed > 100 && resumed > 100 && lost > 100,
        "{fixed} {resumed} {lost}"
    );
}

#[test]
fn sets_match_beyond_ascii() {
    assert_eq!(
        run(&program("[α-ω]+").unwrap(), &chars("αβγ!"), 0, None),
        Some(3)
    );
    assert_eq!(
        run(&program("\\w+").unwrap(), &chars("héllo wörld"), 0, None),
        Some(5)
    );
    assert_eq!(
        run(&program("[^a]").unwrap(), &chars("é"), 0, None),
        Some(1)
    );
    assert_eq!(
        run(&program("(?i)[a-c]+").unwrap(), &chars("aBcÀ"), 0, None),
        Some(3)
    );
}

#[test]
fn programs_that_look_around_them_keep_no_automaton() {
    for pattern in [
        "a*",
        "\\d+(?:\\.\\d+)?",
        "(?:ab|a)(?:bc|c)?",
        "(?i)[a-c]+x*?",
    ] {
        assert!(program(pattern).unwrap().dfa.is_some(), "{pattern}");
    }
    for pattern in ["^a*", "a*$", "\\ba", "x(?=a*)", "(?<=a)b", "(?!x)y"] {
        assert!(program(pattern).unwrap().dfa.is_none(), "{pattern}");
    }
}

#[test]
fn the_automaton_remembers_the_states_a_run_passes_through() {
    let p = program("(?:ab|a)(?:bc|c)?d*").unwrap();
    let text = chars("abcdd abcd acd ad é");
    let states = |p: &Program| p.dfa.as_ref().unwrap().lock().states.len();
    assert_eq!(states(&p), 0);
    let twin = p.clone();
    for pos in 0..=text.len() {
        assert_eq!(run(&p, &text, pos, None), run(&twin, &text, pos, None));
    }
    let remembered = states(&p);
    assert!((4..=16).contains(&remembered), "{remembered}");
    assert_eq!(states(&twin), remembered);
    for pos in 0..=text.len() {
        run(&p, &text, pos, None);
    }
    assert_eq!(states(&p), remembered);
    let dfa = p.dfa.as_ref().unwrap().lock();
    assert!(dfa.states.iter().any(|state| !state.other.is_empty()));
    assert!(
        dfa.states
            .iter()
            .all(|state| state.waiting <= state.pcs.len())
    );
}

#[test]
fn the_automaton_forgets_its_states_when_there_are_too_many() {
    let p = program("(?:ab){0,600}c?").unwrap();
    let fresh = p.clone();
    let text: Vec<char> = "ab".repeat(600).chars().chain(['c']).collect();
    let states = |p: &Program| p.dfa.as_ref().unwrap().lock().states.len();
    let (end, touch, resume) = p.scan(&text, 0, None);
    assert_eq!((end, touch, resume), (Some(1201), Touch::Never, None));
    assert!(states(&p) <= DFA_STATES, "{}", states(&p));
    for split in [1, 700, 1199, 1200] {
        let (end, touch, resume) = p.scan(&text[..split], 0, None);
        assert_eq!(touch, Touch::End, "{split}");
        assert_eq!(
            (end, &resume),
            (
                fresh.scan(&text[..split], 0, None).0,
                &fresh.scan(&text[..split], 0, None).2
            ),
            "{split}"
        );
        let carried = p.resume(&resume.unwrap(), &text, 0, split);
        assert_eq!(carried.0, Some(1201), "{split}");
        assert!(states(&p) <= DFA_STATES, "{}", states(&p));
    }
}

#[test]
fn characters_beyond_ascii_share_transitions_by_class() {
    let p = program("[α-ω]+(?:[^a-z]x)?").unwrap();
    let text = chars("αβγδ Ωx");
    assert_eq!(run(&p, &text, 0, None), Some(4));
    assert_eq!(run(&p, &text, 4, None), None);
    assert_eq!(run(&p, &chars("αÀx"), 0, None), Some(3));
    let dfa = p.dfa.as_ref().unwrap().lock();
    let other: usize = dfa.states.iter().map(|state| state.other.len()).sum();
    assert!((2..=4).contains(&other), "{other}");
    drop(dfa);
    assert!(matches!(p.classes, Classes::Ranges(_)));
    assert!(p.class('α') != p.class('À') && p.class('β') == p.class('γ'));
    assert_eq!(p.class('a'), u64::from('a'));
    let word = program("\\w+\\D\\s").unwrap();
    assert!(matches!(&word.classes, Classes::Ranges(bounds) if bounds.len() > 100));
    assert_eq!(run(&word, &chars("héllo٣ \u{2003}"), 0, None), Some(8));
    assert!(word.class('é') == word.class('ö') && word.class('é') != word.class('\u{2003}'));
    let ascii = program("(?a)\\w+").unwrap();
    assert!(matches!(&ascii.classes, Classes::Ranges(bounds) if bounds.is_empty()));
    assert_eq!(run(&ascii, &chars("héllo"), 0, None), Some(1));
    let folded = program("(?i)[α-ω]+x").unwrap();
    assert!(matches!(folded.classes, Classes::Tests(_)));
    assert_eq!(run(&folded, &chars("αΒΓX"), 0, None), Some(4));
    assert!(folded.class('Β') == folded.class('γ') && folded.class('Β') != folded.class('À'));
    let copies = program("(?i)[^a]{70}").unwrap();
    assert!(matches!(&copies.classes, Classes::Tests(units) if units.len() == 1));
    let distinct: alloc::string::String = (0..70)
        .map(|i| alloc::format!("[^{}]", char::from_u32(0x100 + i).unwrap()))
        .collect();
    let many = program(&["(?i)", &distinct].concat()).unwrap();
    assert!(matches!(many.classes, Classes::ByChar));
    assert_eq!(many.class('α'), u64::from('α'));
    let text: Vec<char> = "αβ".repeat(35).chars().collect();
    assert_eq!(run(&many, &text, 0, None), Some(70));
    assert_eq!(run(&many, &text[..69], 0, None), None);
}

#[test]
fn a_resume_names_its_state_and_checks_it_is_still_the_same() {
    let p = program("ab*c?").unwrap();
    let (end, _, resume) = p.scan(&chars("ab"), 0, None);
    let resume = resume.unwrap();
    assert_eq!(end, Some(2));
    assert_ne!(resume.state, UNKNOWN);
    assert_eq!(p.resume(&resume, &chars("abbc"), 0, 2).0, Some(4));
    p.dfa.as_ref().unwrap().lock().clear();
    assert_eq!(p.resume(&resume, &chars("abbc"), 0, 2).0, Some(4));
    let other = program("xy*").unwrap();
    for text in ["x", "xy"] {
        other.scan(&chars(text), 0, None);
    }
    let mut stale = resume.clone();
    stale.state = 0;
    assert_eq!(p.resume(&stale, &chars("abc"), 0, 2).0, Some(3));
    assert!(p.swallows(&stale, 'b') && !p.swallows(&stale, 'd'));
}

#[test]
fn nested_repeats_are_found_inside_groups() {
    let nested = |pattern: &str| has_nested_repeats(&parse_pattern(pattern, 0).unwrap().pattern);
    assert!(nested("(a+)+"));
    assert!(nested("x(?:(a+)+)"));
    assert!(nested("((a+)+)"));
    assert!(nested("(?>(a+)+)"));
    assert!(nested("b|((ab*)*c)"));
    assert!(!nested("(a+)b+"));
    assert!(!nested("((a+)b)"));
    assert!(!nested("(?>a+)"));
    assert!(!nested("(a|b)*"));
}

#[test]
fn a_full_automaton_is_forgotten_before_a_run_is_carried_on() {
    let text: Vec<char> = "ab".repeat(600).chars().chain(['c']).collect();
    let states = |p: &Program| p.dfa.as_ref().unwrap().lock().states.len();
    let split = DFA_STATES - 1;
    let filled = || {
        let p = program("(?:ab){0,600}c?").unwrap();
        let (end, _, resume) = p.scan(&text[..split], 0, None);
        assert_eq!(end, Some(split - 1));
        assert_eq!(states(&p), DFA_STATES);
        (p, resume.unwrap())
    };
    let (p, resume) = filled();
    assert!(p.swallows(&resume, 'b'));
    assert!(states(&p) < DFA_STATES);
    assert!(!p.swallows(&resume, 'a'));
    let (p, resume) = filled();
    assert_eq!(p.resume(&resume, &text, 0, split).0, Some(1201));
    assert!(states(&p) < DFA_STATES);
    assert_eq!(
        p.resume(&resume, &text[..split + 2], 0, split).0,
        Some(split + 1)
    );
}

#[test]
fn a_program_that_looks_around_it_never_knows_what_it_swallows() {
    let p = program("(?!x)ab*").unwrap();
    assert!(!p.remembers());
    let (end, touch, resume) = p.scan(&chars("a"), 0, None);
    assert_eq!((end, touch), (Some(1), Touch::End));
    let resume = resume.unwrap();
    assert_eq!(p.resume(&resume, &chars("ab"), 0, 1).0, Some(2));
    assert!(!p.swallows(&resume, 'b'));
    assert!(!p.swallows(&resume, 'x'));
}
