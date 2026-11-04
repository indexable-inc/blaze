//! Embedded tests for the regex generator in `src/native/draws/regex.rs`:
//! the strings it draws for a pattern (and that every one matches the
//! pattern, as the engine's matcher and the pattern's constraint judge
//! it), which patterns it has the matcher check its draws against, the
//! character sets it builds for classes under each flag, and how it
//! records a drawn string as one choice under the pattern's constraint.
//! The generator has no matcher of its own: the matcher's own tests are
//! in `re_matcher_tests.rs`.

use super::*;
use crate::native::bignum::BigInt;
use crate::native::core::{ChoiceValue, Constraint, GenerationParameters, NativeTestCase};
use crate::native::draws::source::{RngDraws, SimplestDraws};
use crate::native::draws::{StringSpec, TextAlphabet};
use crate::native::re::constants::{ChCode, SRE_FLAG_ASCII, SRE_FLAG_IGNORECASE};
use crate::native::re::matcher::set_matches;
use crate::native::re::parser::{OpCode, SetItem, SubPattern};
use crate::native::rng::EngineRng;
use crate::unicodedata::UnicodeVersion;
use alloc::string::ToString;
use alloc::vec;

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn codepoints(s: &str) -> Vec<u32> {
    s.chars().map(u32::from).collect()
}

#[test]
fn oversize_regex_draws_stop_and_restore_the_output_buffer() {
    for pattern in [
        "a{100000000}",
        "[ab]{100000000}",
        "[^ab]{100000000}",
        "abcdefghij",
        "(abc)\\1",
    ] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        let mut rng = EngineRng::seeded(0);
        let mut draws = RngDraws::new(&mut rng, Default::default());
        let mut out = chars("prefix");
        assert!(matches!(
            generate_regex_into(&mut draws, &re, true, &mut out, 4),
            Err(EngineError::InvalidTestCase)
        ));
        assert_eq!(out, chars("prefix"));
        assert_eq!(draws.status(), Some(Status::Invalid));
    }
}

#[test]
fn padding_counts_toward_the_regex_size_limit() {
    for pattern in ["", "a", "a$"] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        let mut rejected = 0;
        for seed in 0..20 {
            let mut rng = EngineRng::seeded(seed);
            let mut draws = RngDraws::new(&mut rng, Default::default());
            let mut out = Vec::new();
            match generate_regex_into(&mut draws, &re, false, &mut out, 1) {
                Ok(()) => assert!(out.len() <= 1),
                Err(error) => {
                    assert!(matches!(error, EngineError::InvalidTestCase));
                    assert!(out.is_empty());
                    rejected += 1;
                }
            }
        }
        assert!(rejected > 0);
    }
}

#[test]
fn nullable_regex_repeats_have_a_work_limit() {
    let re = CompiledRegex::compile("(?:){65536}", None, UnicodeVersion::LATEST).unwrap();
    let mut state = ignorecase_state(&re.caches);
    state.remaining = 16;
    let mut draws = SimplestDraws::default();
    assert!(matches!(
        generate_subpattern(
            &mut draws,
            &re.parsed.pattern,
            &mut state,
            &None,
            &mut Vec::new()
        ),
        Err(EngineError::InvalidTestCase)
    ));
    assert_eq!(draws.status(), Some(Status::Invalid));
}

#[test]
fn the_simplest_regex_string_must_fit_the_choice() {
    let re = CompiledRegex::compile("a{10}", None, UnicodeVersion::LATEST).unwrap();
    let mut kind = re.kind(true).unwrap();
    kind.max_size = 4;
    let mut draws = SimplestDraws::default();
    assert!(matches!(
        ensure_simplest(&mut draws, &re, true, &kind),
        Err(EngineError::InvalidTestCase)
    ));
    assert!(kind.constraint.as_ref().unwrap().simplest().is_none());
}

#[test]
fn a_character_set_remembers_its_least_character() {
    let intervals = IntervalSet::unicode_scalars().unwrap();
    let chars = Chars::from(vec!['b', 'a', '1']);
    assert!(chars.least.get().is_none());
    assert_eq!(chars.least(&intervals).unwrap(), '1');
    assert_eq!(chars.least.get(), Some(&'1'));
    assert_eq!(chars.least(&intervals).unwrap(), '1');
    assert_eq!(&*chars, &['b', 'a', '1']);
}

fn lit(cp: char) -> OpCode {
    OpCode::Literal(cp as u32)
}

fn sub(ops: Vec<OpCode>) -> SubPattern {
    SubPattern { data: ops }
}

fn set_len(set: &InSet) -> u64 {
    match set {
        InSet::Listed(chars) => chars.len() as u64,
        InSet::Complement(complement) => complement.len,
    }
}

fn listed(set: &InSet) -> Vec<char> {
    match set {
        InSet::Listed(chars) => chars.to_vec(),
        InSet::Complement(complement) => (0..complement.len)
            .map(|idx| complement.get(idx).unwrap())
            .collect(),
    }
}

#[test]
fn build_in_set_ascii_flag_keeps_nonascii_positive_literal() {
    let items = vec![SetItem::Literal('a' as u32), SetItem::Literal(0xFF)];
    let out = build_in_set(&items, SRE_FLAG_ASCII, &None).unwrap();
    assert_eq!(out, InSet::Listed(Arc::new(vec!['a', '\u{FF}'].into())));
}

#[test]
fn build_in_set_ascii_ignorecase_folds_only_ascii_letters() {
    let items = vec![SetItem::Literal('à' as u32), SetItem::Literal('a' as u32)];
    let out = build_in_set(&items, SRE_FLAG_ASCII | SRE_FLAG_IGNORECASE, &None).unwrap();
    assert_eq!(listed(&out), vec!['à', 'a', 'A']);
}

#[test]
fn build_in_set_ignorecase_includes_every_case_equal_char() {
    let items = vec![SetItem::Literal('k' as u32)];
    let out = build_in_set(&items, SRE_FLAG_IGNORECASE, &None).unwrap();
    assert_eq!(listed(&out), vec!['k', 'K', '\u{212A}']);
}

#[test]
fn build_in_set_ignorecase_range_includes_case_equivalents_of_its_members() {
    let items = vec![SetItem::Range('a' as u32, 'z' as u32)];
    let out = listed(&build_in_set(&items, SRE_FLAG_IGNORECASE, &None).unwrap());
    for extra in ['K', '\u{212A}', 'ſ', '\u{131}', '\u{130}'] {
        assert!(out.contains(&extra), "{extra:?} missing from {out:?}");
    }
    assert_eq!(out.len(), 26 * 2 + 4);
}

#[test]
fn build_in_set_ascii_categories_are_ascii_only() {
    let items = vec![SetItem::Category(ChCode::Word)];
    let alphabet =
        IntervalSet::new(vec![('0' as u32, 'z' as u32), ('é' as u32, 'é' as u32)]).unwrap();
    let out = listed(&build_in_set(&items, SRE_FLAG_ASCII, &Some(alphabet.clone())).unwrap());
    assert!(
        out.iter().all(|c| c.is_ascii_alphanumeric() || *c == '_'),
        "{out:?}"
    );
    let out = listed(&build_in_set(&items, 0, &Some(alphabet)).unwrap());
    assert!(out.contains(&'é'));
}

#[test]
fn build_in_set_negated_ignorecase_excludes_case_equal_chars() {
    let items = vec![SetItem::Negate, SetItem::Literal('k' as u32)];
    let alphabet = IntervalSet::new(vec![
        ('K' as u32, 'K' as u32),
        ('k' as u32, 'k' as u32),
        ('x' as u32, 'x' as u32),
        (0x212A, 0x212A),
    ])
    .unwrap();
    let out = build_in_set(&items, SRE_FLAG_IGNORECASE, &Some(alphabet)).unwrap();
    assert_eq!(listed(&out), vec!['x']);
}

#[test]
fn build_in_set_positive_enumeration_agrees_with_the_matcher() {
    let cases: Vec<(Vec<SetItem>, u32)> = vec![
        (vec![SetItem::Literal('k' as u32)], SRE_FLAG_IGNORECASE),
        (vec![SetItem::Literal('ß' as u32)], SRE_FLAG_IGNORECASE),
        (
            vec![SetItem::Literal('\u{130}' as u32)],
            SRE_FLAG_IGNORECASE,
        ),
        (
            vec![SetItem::Range('a' as u32, 'z' as u32)],
            SRE_FLAG_IGNORECASE,
        ),
        (vec![SetItem::Range(0x3b1, 0x3c9)], SRE_FLAG_IGNORECASE),
        (vec![SetItem::Range(0x3b1, 0x3c9)], 0),
        (
            vec![
                SetItem::Range('À' as u32, 'Ö' as u32),
                SetItem::Literal('a' as u32),
            ],
            SRE_FLAG_IGNORECASE | SRE_FLAG_ASCII,
        ),
        (
            vec![
                SetItem::Category(ChCode::Digit),
                SetItem::Literal('x' as u32),
            ],
            SRE_FLAG_IGNORECASE | SRE_FLAG_ASCII,
        ),
    ];
    for (items, flags) in cases {
        let enumerated: HashSet<char> = listed(&build_in_set(&items, flags, &None).unwrap())
            .into_iter()
            .filter(|c| (*c as u32) <= 0xFFFF)
            .collect();
        let matched: HashSet<char> = gather_chars(&None, |c| set_matches(&items, c, flags))
            .into_iter()
            .collect();
        assert_eq!(enumerated, matched, "{items:?} with flags {flags}");
    }
}

#[test]
fn build_in_set_alphabet_drops_disallowed_positive_literal() {
    let items = vec![SetItem::Literal('a' as u32), SetItem::Literal('b' as u32)];
    let alphabet = IntervalSet::new(vec![('a' as u32, 'a' as u32)]).unwrap();
    let out = build_in_set(&items, 0, &Some(alphabet)).unwrap();
    assert_eq!(out, InSet::Listed(Arc::new(vec!['a'].into())));
}

#[test]
fn build_in_set_negated_ascii_flag_keeps_nonascii() {
    let items = vec![SetItem::Negate, SetItem::Literal('a' as u32)];
    let alphabet = IntervalSet::new(vec![(b' ' as u32, 0x100)]).unwrap();
    let out = listed(&build_in_set(&items, SRE_FLAG_ASCII, &Some(alphabet)).unwrap());
    assert!(out.iter().all(|c| *c != 'a'));
    assert!(out.contains(&'\u{100}'));
}

#[test]
fn build_in_set_negated_default_set_is_the_complement_of_its_literals() {
    let items = vec![
        SetItem::Negate,
        SetItem::Literal('k' as u32),
        SetItem::Range('0' as u32, '2' as u32),
        SetItem::Literal(0x1_F600),
    ];
    let out = build_in_set(&items, 0, &None).unwrap();
    let InSet::Complement(complement) = &out else {
        panic!("{out:?} is not a complement");
    };
    assert_eq!(
        complement.excluded,
        vec![
            ('0' as u32, '2' as u32),
            ('k' as u32, 'k' as u32),
            (0xD800, 0xDFFF),
        ]
    );
    assert_eq!(set_len(&out), DEFAULT_ANY_LEN - 4);
    assert_eq!(listed(&out), reference_in_set(&items, 0, &None));
    let ignorecase = build_in_set(&items, SRE_FLAG_IGNORECASE, &None).unwrap();
    assert_eq!(set_len(&ignorecase), DEFAULT_ANY_LEN - 6);
    assert_eq!(
        listed(&ignorecase),
        reference_in_set(&items, SRE_FLAG_IGNORECASE, &None)
    );
    assert!(!listed(&ignorecase).contains(&'K'));
    assert!(!listed(&ignorecase).contains(&'\u{212A}'));
    assert!(complement.get(u64::from(u32::MAX) + 1).is_err());
}

#[test]
fn generate_op_empty_in_set_marks_invalid() {
    let mut ntc = NativeTestCase::for_choices(&[ChoiceValue::Integer(BigInt::from(0))], None, None);
    let caches = Caches::default();
    let mut state = ignorecase_state(&caches);
    let alphabet = Some(IntervalSet::new(vec![('x' as u32, 'x' as u32)]).unwrap());
    let items = vec![SetItem::Literal('a' as u32), SetItem::Literal('b' as u32)];
    let mut out = Vec::new();
    let result = generate_op(
        &mut ntc,
        &OpCode::In(items),
        &mut state,
        &alphabet,
        &mut out,
    );
    assert!(result.is_err());
    assert_eq!(ntc.status(), Some(Status::Invalid));
}

#[test]
fn generate_op_literal_with_no_candidate_in_alphabet_marks_invalid() {
    let mut ntc = NativeTestCase::for_choices(&[ChoiceValue::Integer(BigInt::from(0))], None, None);
    let caches = Caches::default();
    let mut state = ignorecase_state(&caches);
    let alphabet = Some(IntervalSet::new(vec![('b' as u32, 'b' as u32)]).unwrap());
    let mut out = Vec::new();
    let result = generate_op(&mut ntc, &lit('a'), &mut state, &alphabet, &mut out);
    assert!(result.is_err());
    assert_eq!(ntc.status(), Some(Status::Invalid));
    let mut ntc = NativeTestCase::for_choices(&[], None, None);
    let mut state = ignorecase_state(&caches);
    state.flags = 0;
    let result = generate_op(&mut ntc, &lit('a'), &mut state, &alphabet, &mut out);
    assert!(result.is_err());
    assert_eq!(ntc.status(), Some(Status::Invalid));
}

#[test]
fn generate_op_ignorecase_literal_takes_the_case_the_alphabet_allows() {
    let mut ntc = NativeTestCase::for_choices(&[], None, None);
    let caches = Caches::default();
    let mut state = ignorecase_state(&caches);
    let alphabet = Some(IntervalSet::new(vec![('A' as u32, 'A' as u32)]).unwrap());
    let mut out = Vec::new();
    generate_op(&mut ntc, &lit('a'), &mut state, &alphabet, &mut out).unwrap();
    assert_eq!(out, chars("A"));
}

#[test]
fn generate_op_negated_class_covering_the_bmp_marks_invalid() {
    let mut ntc = NativeTestCase::for_choices(&[ChoiceValue::Integer(BigInt::from(0))], None, None);
    let caches = Caches::default();
    let mut state = ignorecase_state(&caches);
    let items = vec![SetItem::Negate, SetItem::Range(0, 0xFFFF)];
    let mut out = Vec::new();
    let result = generate_op(&mut ntc, &OpCode::In(items), &mut state, &None, &mut out);
    assert!(result.is_err());
    assert_eq!(ntc.status(), Some(Status::Invalid));
}

#[test]
fn compile_rejects_unparseable_patterns() {
    let err = CompiledRegex::compile("(unclosed", None, UnicodeVersion::LATEST).unwrap_err();
    assert!(matches!(err, EngineError::InvalidArgument(_)));
    assert!(err.to_string().contains("invalid regex pattern"));
    assert!(CompiledRegex::compile("a+b?", None, UnicodeVersion::LATEST).is_ok());
}

fn ignorecase_state(caches: &Caches) -> GenState<'_> {
    static SCALARS: Lazy<IntervalSet> = Lazy::new(|| IntervalSet::unicode_scalars().unwrap());
    GenState {
        groups: HashMap::default(),
        max_size: REGEX_MAX_SIZE,
        remaining: GENERATION_BUDGET,
        capture: false,
        flags: SRE_FLAG_IGNORECASE,
        fullmatch: false,
        base: 0,
        intervals: &SCALARS,
        unproducible: false,
        caches,
    }
}

#[test]
fn typical_width_predicts_the_mean_length_of_generated_strings() {
    use crate::native::rng::EngineRng;
    let draws = 2000;
    for pattern in [
        "[0-9]+",
        "a?b*?c",
        "(ab|c){2,5}",
        "x(y)\\1",
        "(?>ab)c",
        "(a)?(?(1)b|cc)",
        "^k{3}$",
        "(?!z)[^a].",
        "a*+b",
    ] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        let predicted = re.typical_width();
        let mut total = 0;
        for seed in 0..draws {
            let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
            total += generate_regex(&mut ntc, &re, true).unwrap().chars().count();
        }
        let mean = total as f64 / draws as f64;
        assert!(
            (mean - predicted).abs() <= 0.15 * predicted + 0.1,
            "{pattern}: predicted {predicted}, drew {mean} on average"
        );
    }
}

#[test]
fn typical_count_is_the_minimum_plus_a_cut_off_geometric_tail() {
    assert_eq!(typical_count(3, Some(3)), 3.0);
    assert!((typical_count(0, Some(1)) - 0.6).abs() < 1e-9);
    assert!((typical_count(1, None) - 6.0).abs() < 1e-9);
    assert!(typical_count(0, Some(2)) < typical_count(0, Some(1000)));
}

#[test]
fn generate_op_ignorecase_eszett_emits_only_the_chars_python_folds_it_to() {
    use crate::native::rng::EngineRng;
    let caches = Caches::default();
    let mut seen = HashSet::default();
    for seed in 0..50 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let mut state = ignorecase_state(&caches);
        let mut out = Vec::new();
        generate_op(&mut ntc, &lit('ß'), &mut state, &None, &mut out).unwrap();
        assert!(
            out == chars("ß") || out == chars("\u{1E9E}"),
            "seed {seed} emitted a non-matching case variant {out:?}"
        );
        seen.insert(out);
    }
    assert_eq!(seen.len(), 2, "saw {seen:?}");
}

#[test]
fn generate_op_ignorecase_plain_letter_emits_both_cases() {
    use crate::native::rng::EngineRng;
    let mut seen = HashSet::default();
    let caches = Caches::default();
    for seed in 0..50 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let mut state = ignorecase_state(&caches);
        let mut out = Vec::new();
        generate_op(&mut ntc, &lit('a'), &mut state, &None, &mut out).unwrap();
        seen.insert(out.into_iter().collect::<String>());
    }
    assert!(seen.contains("a") && seen.contains("A"), "saw {seen:?}");
}

#[test]
fn generate_op_ignorecase_not_literal_excludes_every_case_equal_char() {
    use crate::native::rng::EngineRng;
    let alphabet = Some(
        IntervalSet::new(vec![
            ('I' as u32, 'I' as u32),
            ('i' as u32, 'i' as u32),
            ('x' as u32, 'x' as u32),
            (0x130, 0x130),
            (0x131, 0x131),
            (0x307, 0x307),
        ])
        .unwrap(),
    );
    let caches = Caches::default();
    let mut seen = HashSet::default();
    for seed in 0..100 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let mut state = ignorecase_state(&caches);
        let mut out = Vec::new();
        generate_op(
            &mut ntc,
            &OpCode::NotLiteral('İ' as u32),
            &mut state,
            &alphabet,
            &mut out,
        )
        .unwrap();
        assert!(
            out == chars("x") || out == chars("\u{307}"),
            "seed {seed} emitted a case-equal char {out:?}"
        );
        seen.insert(out);
    }
    assert_eq!(seen.len(), 2, "saw {seen:?}");
}

#[test]
fn generate_regex_handles_huge_character_class_ranges() {
    use crate::native::rng::EngineRng;
    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(0)).unwrap();
    let re = CompiledRegex::compile("[\\x20-\\U0010FFFF]", None, UnicodeVersion::LATEST).unwrap();
    let s = generate_regex(&mut ntc, &re, false).unwrap();
    assert!(!s.is_empty());
}

#[test]
fn generate_regex_word_boundaries_hold_in_the_final_string() {
    use crate::native::rng::EngineRng;
    let re = CompiledRegex::compile(r"\bfoo\b", None, UnicodeVersion::LATEST).unwrap();
    let mut produced = 0;
    for seed in 0..300 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let Ok(s) = generate_regex(&mut ntc, &re, false) else {
            continue;
        };
        produced += 1;
        assert!(
            pattern_matches(re.parsed(), false, &chars(&s)),
            "seed {seed}: {s:?} contains no \\bfoo\\b match"
        );
    }
    assert!(produced > 0, "every draw was rejected");
}

#[test]
fn generate_regex_end_anchor_inside_branch_holds_in_the_final_string() {
    use crate::native::rng::EngineRng;
    let re = CompiledRegex::compile("foo$|bar", None, UnicodeVersion::LATEST).unwrap();
    let mut produced = 0;
    for seed in 0..300 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let Ok(s) = generate_regex(&mut ntc, &re, false) else {
            continue;
        };
        produced += 1;
        assert!(
            s.contains("bar") || s.ends_with("foo") || s.ends_with("foo\n"),
            "seed {seed}: {s:?} matches neither branch"
        );
    }
    assert!(produced > 0, "every draw was rejected");
}

#[test]
fn generate_regex_fullmatch_lookahead_does_not_emit_the_assertion_body() {
    use crate::native::rng::EngineRng;
    let re = CompiledRegex::compile("(?=a)ab", None, UnicodeVersion::LATEST).unwrap();
    let mut produced = 0;
    for seed in 0..100 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let Ok(s) = generate_regex(&mut ntc, &re, true) else {
            continue;
        };
        produced += 1;
        assert_eq!(s, "ab", "seed {seed}: {s:?} is not a fullmatch of (?=a)ab");
    }
    assert!(produced > 0, "every draw was rejected");
}

#[test]
fn generate_regex_unsatisfiable_possessive_pattern_never_yields_a_wrong_string() {
    use crate::native::rng::EngineRng;
    let re = CompiledRegex::compile("a*+a", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..100 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        for fullmatch in [false, true] {
            let mut ntc2 = NativeTestCase::new_random(EngineRng::seeded(seed + 1000)).unwrap();
            let ntc_ref = if fullmatch { &mut ntc2 } else { &mut ntc };
            if let Ok(s) = generate_regex(ntc_ref, &re, fullmatch) {
                panic!(
                    "seed {seed} fullmatch={fullmatch}: a*+a produced {s:?}, but no string matches it"
                );
            }
        }
    }
}

#[test]
fn generate_regex_ignorecase_negated_class_excludes_every_case_equal_char() {
    use crate::native::rng::EngineRng;
    let alphabet = Some(
        IntervalSet::new(vec![
            ('I' as u32, 'I' as u32),
            ('a' as u32, 'a' as u32),
            ('i' as u32, 'i' as u32),
            ('x' as u32, 'x' as u32),
            (0x130, 0x131),
            (0x307, 0x307),
        ])
        .unwrap(),
    );
    let re = CompiledRegex::compile("(?i)[^\u{130}a]", alphabet, UnicodeVersion::LATEST).unwrap();
    let mut seen = HashSet::default();
    for seed in 0..100 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let s = generate_regex(&mut ntc, &re, true).unwrap();
        assert!(
            s == "x" || s == "\u{307}",
            "seed {seed}: {s:?} is case-equal to an excluded char"
        );
        seen.insert(s);
    }
    assert_eq!(seen.len(), 2, "saw {seen:?}");
}

#[test]
fn generate_regex_fullmatch_lookbehind_is_validated_against_the_final_string() {
    use crate::native::rng::EngineRng;
    let re = CompiledRegex::compile("(?<=a)b", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert!(
            generate_regex(&mut ntc, &re, true).is_err(),
            "a fixed-width lookbehind can never hold at the start of a fullmatch"
        );
    }

    let re = CompiledRegex::compile("(?<=^)b", None, UnicodeVersion::LATEST).unwrap();
    let mut produced = 0;
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        if let Ok(s) = generate_regex(&mut ntc, &re, true) {
            produced += 1;
            assert_eq!(s, "b");
        }
    }
    assert!(
        produced > 0,
        "a zero-width lookbehind body that holds must pass"
    );
}

#[test]
fn generate_regex_retries_a_failed_negative_lookbehind_within_the_test_case() {
    use crate::native::rng::EngineRng;
    let re = CompiledRegex::compile(r"[ab]*(?<!a)c", None, UnicodeVersion::LATEST).unwrap();
    let mut produced = 0;
    for seed in 0..200 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        if let Ok(s) = generate_regex(&mut ntc, &re, true) {
            produced += 1;
            assert!(!s.ends_with("ac"), "{s:?} violates the lookbehind");
        }
    }
    assert!(
        produced > 190,
        "a candidate ending in `a` must be redrawn, not fail the test case: {produced}/200"
    );

    let re = CompiledRegex::compile(r"a(?<!a)c", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..10 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert!(
            generate_regex(&mut ntc, &re, true).is_err(),
            "a negative lookbehind that always matches can never hold"
        );
        assert_eq!(ntc.status(), Some(Status::Invalid));
    }
}

#[test]
fn generate_regex_end_anchor_with_no_newline_in_alphabet_does_not_pad() {
    use crate::native::rng::EngineRng;
    let alphabet = Some(IntervalSet::new(vec![('a' as u32, 'b' as u32)]).unwrap());
    let re = CompiledRegex::compile("a$", alphabet, UnicodeVersion::LATEST).unwrap();
    let mut produced = 0;
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let Ok(s) = generate_regex(&mut ntc, &re, false) else {
            continue;
        };
        produced += 1;
        assert!(s.ends_with('a'), "no newline available to pad with: {s:?}");
    }
    assert!(produced > 0);
}

#[test]
fn generate_regex_multiline_caret_in_the_middle_generates_nothing() {
    use crate::native::rng::EngineRng;
    let re = CompiledRegex::compile("(?m)a^b", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert!(
            generate_regex(&mut ntc, &re, false).is_err(),
            "a mid-pattern ^ preceded by a literal can never match"
        );
    }
}

#[test]
fn codepoint_to_char_reports_surrogates_as_internal_errors() {
    assert_eq!(codepoint_to_char('a' as u32).unwrap(), 'a');
    assert!(codepoint_to_char(0xD800).is_err());
}

fn reference_in_category(c: char, cat: ChCode, ascii: bool) -> bool {
    let group = |g: &str| UnicodeVersion::LATEST.is_in_group(c as u32, g).unwrap();
    let (digit, space, word) = if ascii {
        (
            c.is_ascii_digit(),
            matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'),
            c == '_' || c.is_ascii_alphanumeric(),
        )
    } else {
        (
            group("Nd"),
            matches!(
                c,
                ' ' | '\t'
                    | '\n'
                    | '\r'
                    | '\x0b'
                    | '\x0c'
                    | '\x1c'
                    | '\x1d'
                    | '\x1e'
                    | '\x1f'
                    | '\u{85}'
            ) || group("Z"),
            c == '_' || group("L") || group("N"),
        )
    };
    match cat {
        ChCode::Digit => digit,
        ChCode::NotDigit => !digit,
        ChCode::Space => space,
        ChCode::NotSpace => !space,
        ChCode::Word => word,
        ChCode::NotWord => !word,
    }
}

fn reference_in_set(items: &[SetItem], flags: u32, alphabet: &Option<IntervalSet>) -> Vec<char> {
    let negate = matches!(items.first(), Some(SetItem::Negate));
    let ascii = flags & SRE_FLAG_ASCII != 0;
    let mut positive = Vec::new();
    let mut categories = Vec::new();
    for item in items {
        match item {
            SetItem::Negate => {}
            SetItem::Literal(cp) => {
                casefold::push_equivalents(&mut positive, char::from_u32(*cp).unwrap(), flags);
            }
            SetItem::Range(lo, hi) => {
                for c in (*lo..=*hi).filter_map(char::from_u32) {
                    casefold::push_equivalents(&mut positive, c, flags);
                }
            }
            SetItem::Category(cat) => categories.push(*cat),
        }
    }
    let in_classes = |c: char| {
        categories
            .iter()
            .any(|cat| reference_in_category(c, *cat, ascii))
    };
    if !negate {
        let mut out = Vec::new();
        let mut seen: HashSet<char> = HashSet::default();
        for c in positive {
            if alphabet_allows(alphabet, c) && seen.insert(c) {
                out.push(c);
            }
        }
        for c in gather_chars(alphabet, in_classes) {
            if seen.insert(c) {
                out.push(c);
            }
        }
        out
    } else {
        let positive_set: HashSet<char> = positive.into_iter().collect();
        gather_chars(alphabet, |c| !positive_set.contains(&c) && !in_classes(c))
    }
}

#[test]
fn category_in_agrees_with_in_category_from_the_category_alone_codepoint_up() {
    use ChCode::*;
    let cats = [Digit, NotDigit, Space, NotSpace, Word, NotWord];
    for ascii in [false, true] {
        let flags = if ascii { SRE_FLAG_ASCII } else { 0 };
        for c in (CATEGORY_ALONE_FROM..=0x10FFFF).filter_map(char::from_u32) {
            let category = UnicodeVersion::LATEST.general_category_char(c);
            for cat in cats {
                assert_eq!(
                    category_in(category, cat, ascii),
                    in_category(c, cat, flags),
                    "{c:?} {cat:?} ascii {ascii}"
                );
            }
        }
    }
    let differs = |c: char, cat: ChCode, ascii: bool| {
        let category = UnicodeVersion::LATEST.general_category_char(c);
        let flags = if ascii { SRE_FLAG_ASCII } else { 0 };
        category_in(category, cat, ascii) != in_category(c, cat, flags)
    };
    assert!(differs('\t', Space, false));
    assert!(differs('_', Word, false));
    assert!(differs('_', Word, true));
    assert!(differs('7', Digit, true));
}

#[test]
fn build_in_set_category_classes_match_a_lookup_per_character() {
    use ChCode::*;
    let alphabets = [
        None,
        Some(IntervalSet::new(vec![(0x20, 0x7E), (0xA0, 0x3FF), (0x1_F600, 0x1_F64F)]).unwrap()),
    ];
    let item_sets: Vec<Vec<SetItem>> = [Digit, NotDigit, Space, NotSpace, Word, NotWord]
        .iter()
        .flat_map(|cat| {
            [
                vec![SetItem::Category(*cat)],
                vec![SetItem::Negate, SetItem::Category(*cat)],
            ]
        })
        .chain([
            vec![SetItem::Negate, SetItem::Literal('a' as u32)],
            vec![
                SetItem::Negate,
                SetItem::Range('a' as u32, 'z' as u32),
                SetItem::Literal(0x1_F600),
            ],
            vec![
                SetItem::Literal('0' as u32),
                SetItem::Range('x' as u32, 'z' as u32),
                SetItem::Category(Digit),
            ],
            vec![
                SetItem::Negate,
                SetItem::Literal('a' as u32),
                SetItem::Category(Space),
                SetItem::Category(Digit),
            ],
            vec![
                SetItem::Literal('a' as u32),
                SetItem::Category(Word),
                SetItem::Category(Space),
            ],
            vec![SetItem::Literal('α' as u32), SetItem::Category(Word)],
            vec![
                SetItem::Negate,
                SetItem::Literal('α' as u32),
                SetItem::Category(Space),
            ],
            vec![
                SetItem::Negate,
                SetItem::Literal('α' as u32),
                SetItem::Category(Word),
            ],
        ])
        .collect();
    for alphabet in &alphabets {
        for items in &item_sets {
            for flags in [
                0,
                SRE_FLAG_IGNORECASE,
                SRE_FLAG_ASCII,
                SRE_FLAG_IGNORECASE | SRE_FLAG_ASCII,
            ] {
                assert_eq!(
                    listed(&build_in_set(items, flags, alphabet).unwrap()),
                    reference_in_set(items, flags, alphabet),
                    "{items:?} with flags {flags}"
                );
            }
        }
    }
    let digits_and_zero = [SetItem::Literal('0' as u32), SetItem::Category(Digit)];
    let out = listed(&build_in_set(&digits_and_zero, 0, &None).unwrap());
    assert_eq!(out[0], '0');
    assert_eq!(out.iter().filter(|c| **c == '0').count(), 1);
    assert!(out.contains(&'٣'));
}

#[test]
fn default_any_char_indexes_the_default_any_scan() {
    for allow_newline in [false, true] {
        let scan = gather_chars(&None, |c| allow_newline || c != '\n');
        let n = DEFAULT_ANY_LEN - u64::from(!allow_newline);
        assert_eq!(scan.len() as u64, n);
        for (idx, &c) in scan.iter().enumerate() {
            assert_eq!(default_any_char(idx as u64, allow_newline).unwrap(), c);
        }
    }
    assert!(default_any_char(u64::from(u32::MAX) + 1, true).is_err());
}

#[test]
fn a_repeated_class_draws_what_repeating_it_in_a_group_would() {
    use crate::native::rng::EngineRng;
    fn repeated(re: &CompiledRegex) -> &SubPattern {
        match &re.parsed.pattern.data[..] {
            [OpCode::MaxRepeat { item, .. }]
            | [OpCode::MinRepeat { item, .. }]
            | [OpCode::PossessiveRepeat { item, .. }] => item,
            other => panic!("not a single repeat: {other:?}"),
        }
    }
    let listed = |c: char| c == 'a' || c == 'b';
    let complemented = |c: char| c != 'a' && c != 'b';
    let mut empty = 0;
    let mut filled = 0;
    for (direct, grouped, allowed) in [
        ("[ab]*", "([ab])*", &listed as &dyn Fn(char) -> bool),
        ("[ab]+?", "([ab])+?", &listed),
        ("[ab]{3}", "([ab]){3}", &listed),
        ("[ab]*+", "([ab])*+", &listed),
        ("[^ab]*", "([^ab])*", &complemented),
        ("[^ab]{2,4}", "([^ab]){2,4}", &complemented),
    ] {
        let direct = CompiledRegex::compile(direct, None, UnicodeVersion::LATEST).unwrap();
        let grouped = CompiledRegex::compile(grouped, None, UnicodeVersion::LATEST).unwrap();
        assert!(matches!(repeated(&direct).data[..], [OpCode::In(_)]));
        assert!(matches!(
            repeated(&grouped).data[..],
            [OpCode::Subpattern { group: Some(_), .. }]
        ));
        for seed in 0..200 {
            let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
            let one = generate_regex(&mut ntc, &direct, true).unwrap();
            let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
            let other = generate_regex(&mut ntc, &grouped, true).unwrap();
            assert_eq!(one, other, "seed {seed}");
            assert!(one.chars().all(allowed), "{one:?}");
            if one.is_empty() {
                empty += 1;
            } else {
                filled += 1;
            }
        }
    }
    assert!(empty > 0 && filled > 0, "{empty} empty, {filled} filled");
}

#[test]
fn a_repeated_empty_class_marks_invalid() {
    let alphabet = Some(IntervalSet::new(vec![('x' as u32, 'x' as u32)]).unwrap());
    for (items, alphabet, complemented) in [
        (vec![SetItem::Literal('a' as u32)], &alphabet, false),
        (
            vec![SetItem::Negate, SetItem::Range(0, 0xFFFF)],
            &None,
            true,
        ),
    ] {
        let mut ntc =
            NativeTestCase::for_choices(&[ChoiceValue::Integer(BigInt::from(0))], None, None);
        let caches = Caches::default();
        let mut state = ignorecase_state(&caches);
        let op = OpCode::MaxRepeat {
            min: 1,
            max: 1,
            item: sub(vec![OpCode::In(items)]),
        };
        let mut out = Vec::new();
        let result = generate_op(&mut ntc, &op, &mut state, alphabet, &mut out);
        assert!(result.is_err());
        assert_eq!(ntc.status(), Some(Status::Invalid));
        assert!(out.is_empty());
        let cache = caches.in_cache.lock();
        let set = cache.values().next().unwrap();
        assert_eq!(
            matches!(set, InSet::Complement(complement) if complement.len == 0),
            complemented
        );
        assert_eq!(
            matches!(set, InSet::Listed(chars) if chars.is_empty()),
            !complemented
        );
    }
}

#[test]
fn a_captured_group_is_kept_only_when_the_pattern_reads_it_back() {
    use crate::native::rng::EngineRng;
    let caches = Caches::default();
    let op = OpCode::Subpattern {
        group: Some(1),
        add_flags: 0,
        del_flags: 0,
        p: sub(vec![lit('a')]),
    };
    for capture in [false, true] {
        let mut ntc = NativeTestCase::for_choices(&[], None, None);
        let mut state = ignorecase_state(&caches);
        state.capture = capture;
        state.flags = 0;
        let mut out = Vec::new();
        generate_op(&mut ntc, &op, &mut state, &None, &mut out).unwrap();
        assert_eq!(out, chars("a"));
        assert_eq!(state.groups.get(&1), capture.then(|| chars("a")).as_ref());
    }
    let reading = CompiledRegex::compile("(a|b)\\1", None, UnicodeVersion::LATEST).unwrap();
    let plain = CompiledRegex::compile("(a|b)(?!c)", None, UnicodeVersion::LATEST).unwrap();
    assert!(reading.parsed.group_refs);
    assert!(!plain.parsed.group_refs);
    for seed in 0..50 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let s = generate_regex(&mut ntc, &reading, true).unwrap();
        assert!(s == "aa" || s == "bb", "{s:?}");
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let s = generate_regex(&mut ntc, &plain, true).unwrap();
        assert!(s == "a" || s == "b", "{s:?}");
    }
}

#[test]
fn drawing_from_an_rng_gives_the_strings_a_fresh_test_case_draws() {
    let patterns = [
        "[a-z]+",
        "(foo|bar)\\d{2,5}",
        "(?i)hello world",
        "a.b[^x]",
        "\\w+@\\w+\\.com",
        "^(ab|cd)*$",
        "x(?=y)y",
        "(?P<q>['\"])[^'\"]*(?P=q)",
    ];
    for pattern in patterns {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        for fullmatch in [true, false] {
            for seed in 0..10 {
                let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
                let mut rng = EngineRng::seeded(seed);
                let params = GenerationParameters::draw(&mut rng).unwrap();
                let mut draws = RngDraws::new(&mut rng, params.integer);
                let from_test_case = generate_regex(&mut ntc, &re, fullmatch);
                let from_rng = generate_regex(&mut draws, &re, fullmatch);
                assert_eq!(
                    from_test_case.as_ref().ok(),
                    from_rng.as_ref().ok(),
                    "{pattern:?} fullmatch={fullmatch} seed={seed}"
                );
                assert_eq!(from_test_case.is_err(), from_rng.is_err());
                assert_eq!(ntc.status(), draws.status());
            }
        }
    }
}

#[test]
fn an_rng_source_records_the_invalid_conclusion_of_a_failing_pattern() {
    let re = CompiledRegex::compile("(?!)", None, UnicodeVersion::LATEST).unwrap();
    let mut rng = EngineRng::seeded(0);
    let mut draws = RngDraws::new(&mut rng, GenerationParameters::default().integer);
    assert!(matches!(
        generate_regex(&mut draws, &re, true).unwrap_err(),
        EngineError::InvalidTestCase
    ));
    assert_eq!(draws.status(), Some(Status::Invalid));
}

/// The simplest source takes the fewest repetitions, the least
/// character of a class or case-folded literal (`A` before `a`) and the
/// alternative whose string is least: `bar` before `foo`, both three
/// characters, as `b` is before `f` in the alphabet's shrink order.
#[test]
fn the_simplest_source_draws_the_simplest_match() {
    for (pattern, fullmatch, expected) in [
        ("[a-z]{2,5}x?", true, "aa"),
        ("(foo|bar)+", true, "bar"),
        ("abc", false, "abc"),
        ("(?i)abc", true, "ABC"),
        ("[1-9][0-9]*", true, "1"),
    ] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        let mut draws = SimplestDraws::default();
        assert_eq!(
            generate_regex(&mut draws, &re, fullmatch).unwrap(),
            expected,
            "{pattern:?}"
        );
        assert_eq!(draws.status(), None);
    }
}

fn simplest(pattern: &str, alphabet: Option<IntervalSet>) -> String {
    let re = CompiledRegex::compile(pattern, alphabet, UnicodeVersion::LATEST).unwrap();
    let mut draws = SimplestDraws::default();
    let simplest = generate_regex(&mut draws, &re, true).unwrap();
    assert_eq!(draws.status(), None, "{pattern:?}");
    simplest
}

/// The simplest string of an alternation is the least of its
/// alternatives' simplest strings in the shrink order of the string
/// choice: the shortest, then by each character's place in the
/// alphabet's shrink order (`A` before `a`, `0` before `1`), the first
/// alternative no longer preferred; a nested alternation is least the
/// same way from either end.
#[test]
fn the_simplest_string_of_an_alternation_is_the_least_of_its_alternatives() {
    for (pattern, expected) in [
        ("(?:bbbb|a)", "a"),
        ("(?:b|a)", "a"),
        ("(?:a|b)", "a"),
        ("(?:ab|c|d)", "c"),
        ("(?:ab|d|c)", "c"),
        ("(?:bb|a|cc)", "a"),
        ("(?:ba|ab)", "ab"),
        ("(?:a|A)", "A"),
        ("(?:1|0)", "0"),
        ("(?:(?:(?:a|bb)|cc)|dd)", "a"),
        ("(?:dd|(?:cc|(?:bb|a)))", "a"),
        ("(?:bbbb|a){2}", "aa"),
        ("x(?:bbbb|a)?", "x"),
    ] {
        assert_eq!(simplest(pattern, None), expected, "{pattern:?}");
    }
}

/// Among alternatives whose simplest strings are equal, the earliest is
/// taken: the group it captures is the one a conditional later sees.
/// The groups an alternative not taken captured are forgotten with it.
#[test]
fn equal_alternatives_tie_to_the_earliest_and_only_its_groups_are_kept() {
    assert_eq!(simplest("(?:(a)|a)(?(1)y|x)", None), "ay");
    assert_eq!(simplest("(?:a|(a))(?(1)x|y)", None), "ay");
    assert_eq!(simplest("(?:(bb)|a)(?(1)x|y)", None), "ay");
}

/// The matcher's check at the end sees the string with the alternative
/// taken: an assertion in the one taken must hold, and one in an
/// alternative not taken (which would fail, at a position the string
/// does not reach) does not matter.
#[test]
fn assertions_are_checked_on_the_alternative_taken() {
    assert_eq!(simplest(r"(?:a\Z|bb)", None), "a");
    assert_eq!(simplest(r"(?:bbbb\B|a)", None), "a");
}

/// Each alternative is drawn once per alternation it is in, so
/// alternations nested many levels deep cost the walk their size, not
/// its square or worse.
#[test]
fn the_least_of_nested_alternations_is_found_in_one_pass_over_each() {
    let mut pattern = "a|bb".to_string();
    for _ in 0..45 {
        pattern = format!("(?:{pattern})|cc");
    }
    assert_eq!(simplest(&pattern, None), "a");
    let mut pattern = "a|bb".to_string();
    for _ in 0..45 {
        pattern = format!("cc|(?:{pattern})");
    }
    assert_eq!(simplest(&pattern, None), "a");
}

/// An alternative the alphabet cannot supply is not a candidate, so the
/// least of the rest is taken; an alternation with none has no string.
#[test]
fn the_least_alternative_is_chosen_among_those_the_alphabet_can_supply() {
    let only_b = || Some(IntervalSet::new(vec![(u32::from('b'), u32::from('b'))]).unwrap());
    assert_eq!(simplest("(?:a|bb)", only_b()), "bb");
    let re = CompiledRegex::compile("(?:a|aa)", only_b(), UnicodeVersion::LATEST).unwrap();
    let mut draws = SimplestDraws::default();
    assert!(matches!(
        generate_regex(&mut draws, &re, true),
        Err(EngineError::InvalidTestCase)
    ));
    assert_eq!(draws.status(), Some(Status::Invalid));
}

/// An alternative that cannot be drawn where it stands fails the walk,
/// as it would fail any draw, whichever alternative is least; the
/// simplest string then comes from the seeded draws.
#[test]
fn an_alternative_that_cannot_be_drawn_fails_the_simplest_walk() {
    let re = CompiledRegex::compile(r"b(?:\A|a)", None, UnicodeVersion::LATEST).unwrap();
    let mut draws = SimplestDraws::default();
    assert!(matches!(
        generate_regex(&mut draws, &re, true),
        Err(EngineError::InvalidTestCase)
    ));
    assert_eq!(draws.status(), Some(Status::Invalid));
    let spec = spec(r"b(?:\A|a)", true);
    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(1)).unwrap();
    crate::native::draws::generate_string(&mut ntc, &spec).unwrap();
    assert_eq!(
        regex_kind(&spec).constraint.as_ref().unwrap().simplest(),
        Some(&[u32::from('b'), u32::from('a')][..])
    );
}

/// When the string built from the least alternative fails the pattern's
/// assertions, the simplest walk draws nothing, and the simplest string
/// is the first valid string of the seeded draws: the only other
/// alternative's, here.
#[test]
fn the_simplest_string_is_valid_when_the_least_alternative_fails_the_pattern() {
    for pattern in ["(?!a)(?:a|bb)", r"(?:a\B|bb)", r"(?:-|bb)\b"] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        let mut draws = SimplestDraws::default();
        assert!(
            matches!(
                generate_regex(&mut draws, &re, true),
                Err(EngineError::InvalidTestCase)
            ),
            "{pattern:?}"
        );
        let spec = spec(pattern, true);
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(1)).unwrap();
        crate::native::draws::generate_string(&mut ntc, &spec).unwrap();
        assert_eq!(
            regex_kind(&spec).constraint.as_ref().unwrap().simplest(),
            Some(&[u32::from('b'), u32::from('b')][..]),
            "{pattern:?}"
        );
    }
}

/// The random source still draws an alternation by index, so the
/// strings a seed draws are what they were.
#[test]
fn the_random_source_draws_alternatives_by_index() {
    let re = CompiledRegex::compile("(?:bbbb|a)", None, UnicodeVersion::LATEST).unwrap();
    let mut seen = HashSet::default();
    for seed in 0..20 {
        let mut rng = EngineRng::seeded(seed);
        let mut draws = RngDraws::new(&mut rng, Default::default());
        assert!(!draws.simplest());
        seen.insert(generate_regex(&mut draws, &re, true).unwrap());
    }
    assert!(seen.contains("bbbb") && seen.contains("a"), "saw {seen:?}");
}

#[test]
fn classes_and_case_folding_follow_the_unicode_version() {
    let v15_1 = UnicodeVersion::parse("15.1.0").unwrap();
    let v16 = UnicodeVersion::parse("16.0.0").unwrap();
    let rams_horn = Some(IntervalSet::new(vec![(0xA7CB, 0xA7CB)]).unwrap());
    assert!(
        CompiledRegex::compile(r"\w", rams_horn.clone(), v16)
            .unwrap()
            .producible()
    );
    assert!(
        !CompiledRegex::compile(r"\w", rams_horn.clone(), v15_1)
            .unwrap()
            .producible()
    );
    assert!(
        CompiledRegex::compile(r"\W", rams_horn, v15_1)
            .unwrap()
            .producible()
    );
    let drawn = |version: UnicodeVersion| {
        let re = CompiledRegex::compile(r"(?i)\uA7CE", None, version).unwrap();
        let mut seen = HashSet::default();
        for seed in 0..50 {
            let mut rng = EngineRng::seeded(seed);
            let mut draws = RngDraws::new(&mut rng, Default::default());
            seen.insert(generate_regex(&mut draws, &re, true).unwrap());
        }
        seen
    };
    assert_eq!(
        drawn(UnicodeVersion::LATEST),
        HashSet::from_iter(["\u{A7CE}".to_string(), "\u{A7CF}".to_string()])
    );
    assert_eq!(drawn(v16), HashSet::from_iter(["\u{A7CE}".to_string()]));
}

fn spec(pattern: &str, fullmatch: bool) -> StringSpec {
    StringSpec::regex(pattern, fullmatch, None, UnicodeVersion::LATEST).unwrap()
}

fn regex_kind(spec: &StringSpec) -> &Arc<StringChoice> {
    match spec {
        StringSpec::Regex { kind, .. } => kind,
        _ => unreachable!(),
    }
}

#[test]
fn a_regex_string_is_one_choice_under_the_pattern_as_its_constraint() {
    let spec = spec("[a-z]+@[a-z]+", true);
    let kind = regex_kind(&spec);
    assert_eq!(kind.max_size, REGEX_MAX_SIZE);
    assert_eq!(kind.intervals.len(), 0x110000 - 0x800);
    assert!(matches!(kind.constraint, Some(Constraint::Regex(_))));
    assert_eq!(kind.constraint.as_ref().unwrap().simplest(), None);

    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(1)).unwrap();
    let drawn = crate::native::draws::generate_string(&mut ntc, &spec).unwrap();
    assert_eq!(ntc.nodes.len(), 1);
    let (recorded, value) = ntc.nodes[0].data.as_string().unwrap();
    assert_eq!(recorded, &**kind);
    assert_eq!(
        value,
        chars(&drawn)
            .iter()
            .map(|&c| u32::from(c))
            .collect::<Vec<u32>>()
    );
    assert!(kind.validate(value));
    assert_eq!(
        kind.constraint.as_ref().unwrap().simplest(),
        Some(&[u32::from('a'), u32::from('@'), u32::from('a')][..])
    );
}

#[test]
fn a_regex_string_drawn_over_an_alphabet_is_recorded_over_it() {
    let letters = StringSpec::text(
        &TextAlphabet {
            categories: Some(vec![]),
            include_characters: Some("abc".to_string()),
            ..TextAlphabet::default()
        },
        0,
        1,
    )
    .unwrap();
    let spec = StringSpec::regex("[a-z]+", true, Some(&letters), UnicodeVersion::LATEST).unwrap();
    let kind = regex_kind(&spec);
    assert_eq!(kind.intervals.len(), 3);
    assert!(kind.validate(&[u32::from('b')]));
    assert!(
        !kind.validate(&[u32::from('z')]),
        "in the pattern, outside the alphabet"
    );
}

#[test]
fn a_replayed_string_the_pattern_rejects_is_punned_to_the_simplest() {
    let spec = spec("[a-z]+@[a-z]+", true);
    let stored = vec![ChoiceValue::String(
        chars("!!!").iter().map(|&c| u32::from(c)).collect(),
    )];
    let mut ntc = NativeTestCase::for_choices(&stored, None, None);
    assert_eq!(
        crate::native::draws::generate_string(&mut ntc, &spec).unwrap(),
        "a@a"
    );
    let kept = vec![ChoiceValue::String(
        chars("zz@q").iter().map(|&c| u32::from(c)).collect(),
    )];
    let mut ntc = NativeTestCase::for_choices(&kept, None, None);
    assert_eq!(
        crate::native::draws::generate_string(&mut ntc, &spec).unwrap(),
        "zz@q"
    );
}

#[test]
fn the_simplest_string_falls_back_on_seeded_draws_when_the_simplest_walk_fails_its_check() {
    let spec = spec("(?!a)[ab]", true);
    let kind = regex_kind(&spec);
    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(1)).unwrap();
    crate::native::draws::generate_string(&mut ntc, &spec).unwrap();
    assert_eq!(
        kind.constraint.as_ref().unwrap().simplest(),
        Some(&[u32::from('b')][..])
    );
}

#[test]
fn a_pattern_with_no_string_concludes_the_test_case_invalid_at_the_first_draw() {
    let spec = spec("(?>a+)a", true);
    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(1)).unwrap();
    let result = crate::native::draws::generate_string(&mut ntc, &spec);
    assert!(
        matches!(result, Err(EngineError::InvalidTestCase)),
        "{result:?}"
    );
    assert_eq!(ntc.status(), Some(Status::Invalid));
}

#[test]
fn a_draw_over_the_size_cap_concludes_the_test_case_invalid() {
    let spec = spec("aaa", true);
    let (re, kind) = match &spec {
        StringSpec::Regex { compiled, kind, .. } => (compiled, kind),
        _ => unreachable!(),
    };
    let mut rng = EngineRng::seeded(0);
    let mut draws = RngDraws::new(&mut rng, Default::default());
    assert!(matches!(
        draw_regex(&mut draws, re, true, 2),
        Err(EngineError::InvalidTestCase)
    ));
    assert_eq!(draws.status(), Some(Status::Invalid));
    let mut draws = RngDraws::new(&mut rng, Default::default());
    assert_eq!(
        draw_regex(&mut draws, re, true, kind.max_size).unwrap(),
        vec![u32::from('a'); 3]
    );
}

#[test]
fn ensuring_the_simplest_string_of_a_kind_under_no_regex_is_an_internal_error() {
    let spec = spec("a", true);
    let plain = StringChoice {
        intervals: Arc::new(IntervalSet::unicode_scalars().unwrap()),
        min_size: 0,
        max_size: 4,
        constraint: None,
    };
    let (re, _) = match &spec {
        StringSpec::Regex { compiled, kind, .. } => (compiled, kind),
        _ => unreachable!(),
    };
    assert!(matches!(
        ensure_simplest(&mut SimplestDraws::default(), re, true, &plain),
        Err(EngineError::Internal(_))
    ));
}

/// A pattern of literal characters alone is emitted as the text it
/// matches without a walk, so drawing it takes nothing from the source;
/// any case folding, other construct or character outside the alphabet
/// leaves the pattern to the walk.
#[test]
fn a_literal_pattern_is_emitted_without_a_walk() {
    let compiled = |pattern: &str, alphabet: Option<IntervalSet>| {
        CompiledRegex::compile(pattern, alphabet, UnicodeVersion::LATEST).unwrap()
    };
    assert_eq!(
        compiled("abc", None).literal.as_deref(),
        Some(&chars("abc")[..])
    );
    assert_eq!(compiled("", None).literal.as_deref(), Some(&[][..]));
    assert!(compiled("(?i)abc", None).literal.is_none());
    assert!(compiled("a.c", None).literal.is_none());
    let letters = IntervalSet::new(vec![(u32::from('a'), u32::from('b'))]).unwrap();
    assert!(compiled("abc", Some(letters)).literal.is_none());

    let re = compiled("abc", None);
    let mut rng = EngineRng::seeded(3);
    let mut draws = RngDraws::new(&mut rng, Default::default());
    assert_eq!(generate_regex(&mut draws, &re, true).unwrap(), "abc");
    assert_eq!(
        rng.next_byte(),
        EngineRng::seeded(3).next_byte(),
        "the draw consumed randomness"
    );
}

#[test]
fn every_string_drawn_is_one_the_pattern_matches() {
    for pattern in [
        r"\bfoo\b",
        r"a\Bb",
        r"(a)\1",
        r"(a)?(?(1)b|cc)",
        r"(?=a)a",
        r"(?!z)[^a].",
        r"(?<=a)b",
        r"(?>ab)c",
        r"a*+b",
        r"(?i)[^k]",
        r"(?i)[^ks]",
        r"(?i:[^k])",
        r"(?:[^k]|\b)+",
        r"(?:\ba)*",
        r"(?:\ba)*?",
        r"(?a)\w+",
        r"[a-z]+",
        r"^a.$",
        r"(?a)[Ï-İ]",
        r"(?a)[^\x00-\xff]",
        r"(?a)\D\S\W",
        r"(?ai)À",
        r"(?ai)a",
        r"(?ai)[^k]",
        r"(?i)k",
        r"(?i)ß",
        r"(?i)İ1",
        r"(?i)[^İ]",
        r"(?i)[ks]x",
        r"(?i)[a-z]+",
        r"(?i)[^a-z]",
        r"(?i)[^\W]",
        r"(?i)(?-i:k)(?a:k)",
        "a\0b",
    ] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        for fullmatch in [true, false] {
            for seed in 0..100 {
                let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
                let Ok(s) = generate_regex(&mut ntc, &re, fullmatch) else {
                    continue;
                };
                assert!(
                    pattern_matches(re.parsed(), fullmatch, &chars(&s)),
                    "{pattern:?} drew {s:?} (fullmatch {fullmatch})"
                );
            }
        }
    }
}

#[test]
fn a_negated_class_under_ignorecase_never_draws_a_character_folding_into_it() {
    for (pattern, lookalike) in [("(?i)[^k]", '\u{212a}'), ("(?i)[^sx]", '\u{17f}')] {
        let alphabet = IntervalSet::new(vec![
            (u32::from('a'), u32::from('a')),
            (u32::from(lookalike), u32::from(lookalike)),
        ])
        .unwrap();
        let re = CompiledRegex::compile(pattern, Some(alphabet), UnicodeVersion::LATEST).unwrap();
        for seed in 0..40 {
            let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
            assert_eq!(
                generate_regex(&mut ntc, &re, true).unwrap(),
                "a",
                "{pattern:?}"
            );
        }
    }
}

#[test]
fn a_pattern_no_string_matches_rejects_the_test_case_instead_of_drawing_one() {
    for (pattern, fullmatch) in [(r"(?=a)b", false), (r"(?a)\bé", true)] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        for seed in 0..10 {
            let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
            assert!(
                matches!(
                    generate_regex(&mut ntc, &re, fullmatch),
                    Err(EngineError::InvalidTestCase)
                ),
                "{pattern:?}"
            );
        }
    }
}

#[test]
fn only_patterns_the_generator_approximates_are_verified_by_the_matcher() {
    for (pattern, verified) in [
        (r"[a-z]+", false),
        (r"^a.$", false),
        (r"[^k]", false),
        (r"(?i)[^k]", false),
        (r"(?i)[^ks]", false),
        (r"(?i:[^k])", false),
        (r"(?i)k[^k].", false),
        (r"a|\b", true),
        (r"(?:\ba)*", true),
        (r"(?:\ba)*?", true),
        (r"(a)\1", true),
        (r"(a)?(?(1)b|c)", true),
        (r"(?=a)a", true),
        (r"(?!a)b", true),
        (r"(?>a)", true),
        (r"a*+", true),
        (r"(?!)", false),
        (r"a$", false),
        (r"a\Z", false),
        (r"(?:a$)", false),
        (r"(a\Z)", false),
        (r"\Aa\Z", false),
        (r"a$b?", true),
        (r"(?:a$)b", true),
        (r"foo$|bar", true),
        (r"(a$)*", true),
        (r"(?:a\Z)+?", true),
        (r"$a", true),
        (r"(?:$)", false),
        (r"(?m)^a$", false),
    ] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        assert_eq!(re.verified, verified, "{pattern:?}");
    }
}

/// The simplest character of a class, a case-folded literal or the
/// wildcard is the least of its characters in the alphabet's shrink
/// order (`0` first, then up through `Z` in codepoint order, then the
/// characters below `0` from `/` down and the rest from `[` up),
/// however the set is listed or stored, and a repeat of a class
/// repeats it.
#[test]
fn the_simplest_character_of_a_class_is_its_least() {
    for (pattern, expected) in [
        (r"\w", "0"),
        (r"\d", "0"),
        (r"[^\n]", "0"),
        (r"[^0]", "1"),
        (r"[^0-Z]", "/"),
        (r"[^0-9A-Z]", ":"),
        (r"[\x00\x01]", "\x01"),
        ("[ba]", "a"),
        ("[b-z]", "b"),
        ("[a-z0]", "0"),
        ("[a-zA-Z]", "A"),
        ("(?i)a", "A"),
        ("(?i)k", "K"),
        (".", "0"),
        ("(?s).", "0"),
        ("[ba]{3}", "aaa"),
        ("[ba]+x", "ax"),
    ] {
        assert_eq!(simplest(pattern, None), expected, "{pattern:?}");
    }
    let letters = Some(IntervalSet::new(vec![(u32::from('a'), u32::from('z'))]).unwrap());
    for (pattern, expected) in [("[^b]", "a"), (".", "a"), (r"\w", "a"), ("[cb]", "b")] {
        assert_eq!(simplest(pattern, letters.clone()), expected, "{pattern:?}");
    }
}

/// Every string drawn for a pattern with anchors or lookarounds, whose
/// holding is the matcher's check at the end of the draw, is accepted
/// by the constraint a replay checks it against.
#[test]
fn strings_drawn_for_anchors_and_lookarounds_are_accepted_by_the_constraint() {
    for pattern in [
        r"a$",
        r"a\Z",
        r"a$b?",
        r"foo$|bar",
        r"(?:a$)",
        r"(a$)*",
        r"(?m)a$\nb",
        r"(?m)^a$",
        r"\bfoo\b",
        r"a\Bb",
        r"(?i)\bk\b",
        r"(?=a)ab",
        r"(?!b)a",
        r"x(?=y)y",
        r"(?<=a)b",
        r"[ab]*(?<!a)c",
        r"(a)(?<=\1)b",
        r"(?=a$)a",
    ] {
        let re = CompiledRegex::compile(pattern, None, UnicodeVersion::LATEST).unwrap();
        let mut produced = 0;
        for fullmatch in [true, false] {
            let kind = re.kind(fullmatch).unwrap();
            let Some(Constraint::Regex(constraint)) = &kind.constraint else {
                unreachable!()
            };
            for seed in 0..100 {
                let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
                let Ok(s) = generate_regex(&mut ntc, &re, fullmatch) else {
                    continue;
                };
                produced += 1;
                assert!(
                    constraint.accepts(&codepoints(&s)),
                    "{pattern:?} drew {s:?} (fullmatch {fullmatch})"
                );
            }
        }
        assert!(produced > 0, "{pattern:?} drew nothing");
    }
}

#[test]
fn generate_regex_ascii_flag_generates_explicit_nonascii_chars() {
    let re = CompiledRegex::compile("(?a)[Ï-İ]", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let s = generate_regex(&mut ntc, &re, true).unwrap();
        let c = s.chars().next().unwrap();
        assert!(s.chars().count() == 1 && ('Ï'..='İ').contains(&c), "{s:?}");
    }
    let re = CompiledRegex::compile("(?a)[^\\x00-\\xff]", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let s = generate_regex(&mut ntc, &re, true).unwrap();
        assert!(s.chars().all(|c| c as u32 > 0xFF), "{s:?}");
    }
}

#[test]
fn generate_regex_ascii_ignorecase_folds_only_ascii_letters() {
    let re = CompiledRegex::compile("(?ai)À", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert_eq!(generate_regex(&mut ntc, &re, true).unwrap(), "À");
    }
    let re = CompiledRegex::compile("(?ai)a", None, UnicodeVersion::LATEST).unwrap();
    let mut seen = HashSet::default();
    for seed in 0..50 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        seen.insert(generate_regex(&mut ntc, &re, true).unwrap());
    }
    assert!(seen.contains("a") && seen.contains("A"), "saw {seen:?}");
}

#[test]
fn generate_regex_skips_a_repeat_whose_body_the_alphabet_cannot_supply() {
    let alphabet = Some(IntervalSet::new(vec![(0, 127)]).unwrap());
    let re = CompiledRegex::compile("(?-i:Ā)*k", alphabet, UnicodeVersion::LATEST).unwrap();
    assert!(re.unproducible);
    for seed in 0..50 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert_eq!(
            generate_regex(&mut ntc, &re, true).unwrap(),
            "k",
            "seed {seed}"
        );
    }
    let mut ntc = NativeTestCase::for_choices(&[], None, None);
    assert_eq!(generate_regex(&mut ntc, &re, true).unwrap(), "k");
}

#[test]
fn generate_regex_required_repeat_of_an_unproducible_body_is_rejected() {
    let alphabet = Some(IntervalSet::new(vec![(0, 127)]).unwrap());
    let re = CompiledRegex::compile("Ā+k", alphabet, UnicodeVersion::LATEST).unwrap();
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert!(generate_regex(&mut ntc, &re, true).is_err(), "seed {seed}");
    }
}

#[test]
fn generate_regex_branch_picks_only_producible_alternatives() {
    let alphabet = Some(IntervalSet::new(vec![(0, 127)]).unwrap());
    let re = CompiledRegex::compile("Ā|b|Ē", alphabet.clone(), UnicodeVersion::LATEST).unwrap();
    for seed in 0..50 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert_eq!(
            generate_regex(&mut ntc, &re, true).unwrap(),
            "b",
            "seed {seed}"
        );
    }
    let re = CompiledRegex::compile("Āx|b|Ēy", alphabet.clone(), UnicodeVersion::LATEST).unwrap();
    let mut ntc = NativeTestCase::for_choices(&[], None, None);
    assert_eq!(generate_regex(&mut ntc, &re, true).unwrap(), "b");
    let re = CompiledRegex::compile("Āx|Ēy", alphabet, UnicodeVersion::LATEST).unwrap();
    for seed in 0..20 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert!(generate_regex(&mut ntc, &re, true).is_err(), "seed {seed}");
    }
    let mut draws = SimplestDraws::default();
    assert!(generate_regex(&mut draws, &re, true).is_err());
    assert_eq!(draws.status(), Some(Status::Invalid));
}

#[test]
fn a_pattern_the_alphabet_supplies_in_full_skips_the_producibility_lookups() {
    for (pattern, alphabet) in [
        ("a|b|c", None),
        ("(?:ab)*c", None),
        ("a|b|c", Some(IntervalSet::new(vec![(0, 127)]).unwrap())),
    ] {
        let re = CompiledRegex::compile(pattern, alphabet, UnicodeVersion::LATEST).unwrap();
        assert!(!re.unproducible, "{pattern:?}");
        assert!(re.caches.producible.lock().values().all(|&p| p));
    }
}

#[test]
fn generate_regex_class_the_alphabet_cannot_supply_is_rejected() {
    let alphabet = Some(IntervalSet::new(vec![(0, 127)]).unwrap());
    let re = CompiledRegex::compile("[ĀĒ]", alphabet, UnicodeVersion::LATEST).unwrap();
    for seed in 0..5 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert!(generate_regex(&mut ntc, &re, true).is_err(), "seed {seed}");
        assert_eq!(ntc.status(), Some(Status::Invalid));
    }
}

#[test]
fn generate_regex_empty_negative_lookahead_is_rejected() {
    let re = CompiledRegex::compile("(?!)", None, UnicodeVersion::LATEST).unwrap();
    for seed in 0..5 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        assert!(generate_regex(&mut ntc, &re, true).is_err(), "seed {seed}");
        assert_eq!(ntc.status(), Some(Status::Invalid));
    }
}

#[test]
fn generate_regex_pattern_with_a_nul_character_matches_itself() {
    let re = CompiledRegex::compile("a\0b", None, UnicodeVersion::LATEST).unwrap();
    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(0)).unwrap();
    assert_eq!(generate_regex(&mut ntc, &re, true).unwrap(), "a\0b");
}

#[test]
fn producible_op_covers_every_op_kind() {
    let alphabet = Some(IntervalSet::new(vec![('a' as u32, 'z' as u32)]).unwrap());
    let ok = |op: &OpCode| producible_op(op, 0, &alphabet, &Caches::default()).unwrap();
    let dead = sub(vec![lit('Ā')]);
    let live = sub(vec![lit('a')]);
    assert!(ok(&lit('a')) && !ok(&lit('Ā')));
    assert!(ok(&OpCode::NotLiteral('a' as u32)));
    assert!(ok(&OpCode::Any));
    assert!(producible_op(&OpCode::Any, 0, &None, &Caches::default()).unwrap());
    assert!(ok(&OpCode::In(vec![SetItem::Literal('a' as u32)])));
    assert!(!ok(&OpCode::In(vec![SetItem::Literal('Ā' as u32)])));
    assert!(
        !producible_op(
            &OpCode::In(vec![SetItem::Negate, SetItem::Range(0, 0xFFFF)]),
            0,
            &None,
            &Caches::default()
        )
        .unwrap()
    );
    assert!(ok(&OpCode::At(AtCode::Boundary)));
    assert!(ok(&OpCode::GroupRef(1)));
    assert!(ok(&OpCode::AssertNot {
        direction: 1,
        p: dead.clone()
    }));
    assert!(!ok(&OpCode::Failure));
    assert!(ok(&OpCode::Branch(vec![dead.clone(), live.clone()])));
    assert!(!ok(&OpCode::Branch(vec![dead.clone()])));
    assert!(!ok(&OpCode::Subpattern {
        group: None,
        add_flags: 0,
        del_flags: 0,
        p: dead.clone()
    }));
    assert!(ok(&OpCode::GroupRefExists {
        cond_group: 1,
        yes: dead.clone(),
        no: None
    }));
    assert!(ok(&OpCode::GroupRefExists {
        cond_group: 1,
        yes: dead.clone(),
        no: Some(live.clone())
    }));
    assert!(!ok(&OpCode::GroupRefExists {
        cond_group: 1,
        yes: dead.clone(),
        no: Some(dead.clone())
    }));
    assert!(!ok(&OpCode::Assert {
        direction: 1,
        p: dead.clone()
    }));
    assert!(ok(&OpCode::AtomicGroup(live.clone())));
    assert!(ok(&OpCode::MaxRepeat {
        min: 0,
        max: 3,
        item: dead.clone()
    }));
    assert!(!ok(&OpCode::MinRepeat {
        min: 1,
        max: 3,
        item: dead.clone()
    }));
    assert!(ok(&OpCode::PossessiveRepeat {
        min: 1,
        max: 3,
        item: live.clone()
    }));
    let caches = Caches::default();
    assert!(producible_sub(&SubPattern::new(), 0, &alphabet, &caches).unwrap());
    assert!(!producible_sub(&dead, 0, &alphabet, &caches).unwrap());
    assert!(!producible_sub(&dead, 0, &alphabet, &caches).unwrap());
}

#[test]
fn producible_sub_answers_for_every_subpattern_of_an_alternation() {
    let alphabet = Some(IntervalSet::new(vec![('a' as u32, 'z' as u32)]).unwrap());
    let caches = Caches::default();
    let branch = sub(vec![OpCode::Branch(vec![
        sub(vec![lit('a')]),
        sub(vec![lit('Ā')]),
        sub(vec![OpCode::MaxRepeat {
            min: 0,
            max: 1,
            item: sub(vec![lit('Ē')]),
        }]),
    ])]);
    assert!(producible_sub(&branch, 0, &alphabet, &caches).unwrap());
    let answers = caches.producible.lock();
    assert_eq!(answers.len(), 5);
    assert_eq!(answers.values().filter(|&&p| !p).count(), 2);
}

#[test]
fn a_wildcard_over_an_alphabet_draws_from_it_and_leaves_the_newline_to_dotall() {
    let alphabet =
        IntervalSet::new(vec![('\n' as u32, '\n' as u32), ('a' as u32, 'b' as u32)]).unwrap();
    let re = CompiledRegex::compile("(?s:.).", Some(alphabet), UnicodeVersion::LATEST).unwrap();
    let mut seen = HashSet::default();
    for seed in 0..100 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let s = generate_regex(&mut ntc, &re, true).unwrap();
        let cs = chars(&s);
        assert!(cs.iter().all(|c| matches!(c, '\n' | 'a' | 'b')), "{s:?}");
        assert_ne!(cs[1], '\n', "{s:?}");
        seen.insert(cs[0]);
    }
    assert!(seen.contains(&'\n'), "saw {seen:?}");
    assert_eq!(re.caches.char_cache.lock().len(), 2);
}

#[test]
fn an_alphabet_is_described_by_its_first_ranges() {
    let set = |ranges: &[(char, char)]| {
        IntervalSet::new(ranges.iter().map(|&(a, b)| (a as u32, b as u32)).collect()).unwrap()
    };
    assert_eq!(describe_alphabet(&set(&[])), "of no characters");
    assert_eq!(describe_alphabet(&set(&[('a', 'a')])), "'a'");
    assert_eq!(
        describe_alphabet(&set(&[('0', '9'), ('a', 'f')])),
        "'0'..='9', 'a'..='f'"
    );
    assert_eq!(
        describe_alphabet(&set(&[
            ('0', '9'),
            ('a', 'f'),
            ('g', 'g'),
            ('p', 'q'),
            ('x', 'z')
        ])),
        "'0'..='9', 'a'..='f', 'g', 'p'..='q', and 1 more ranges"
    );
    assert_eq!(
        describe_alphabet(&IntervalSet::new(vec![(0xD800, 0xD800)]).unwrap()),
        "U+D800"
    );
}

fn text_alphabet(chars: &str) -> StringSpec {
    StringSpec::text(
        &TextAlphabet {
            categories: Some(vec![]),
            include_characters: Some(chars.to_string()),
            ..TextAlphabet::default()
        },
        0,
        0,
    )
    .unwrap()
}

fn codepoint_alphabet(min: u32, max: u32) -> StringSpec {
    StringSpec::text(
        &TextAlphabet {
            min_codepoint: min,
            max_codepoint: max,
            ..TextAlphabet::default()
        },
        0,
        0,
    )
    .unwrap()
}

/// A regex generator over an alphabet that can supply no string of its
/// pattern fails to build with an invalid-argument error naming the
/// pattern and the alphabet, instead of rejecting every draw.
#[test]
fn a_regex_generator_over_an_alphabet_that_cannot_match_its_pattern_is_rejected() {
    for (pattern, alphabet, expected) in [
        (
            "abc",
            text_alphabet("def"),
            "regex pattern \"abc\" matches no string over the alphabet 'd'..='f'",
        ),
        (
            "aa|bb",
            text_alphabet("c"),
            "regex pattern \"aa|bb\" matches no string over the alphabet 'c'",
        ),
        (
            "[abc]",
            text_alphabet("def"),
            "regex pattern \"[abc]\" matches no string over the alphabet 'd'..='f'",
        ),
        (
            "[ab]x[de]",
            text_alphabet("abcdef"),
            "regex pattern \"[ab]x[de]\" matches no string over the alphabet 'a'..='f'",
        ),
        (
            "[a-z]",
            text_alphabet("02468"),
            "regex pattern \"[a-z]\" matches no string over the alphabet '0', '2', '4', '6', and 1 more ranges",
        ),
        (
            r"\d",
            text_alphabet("abc"),
            "regex pattern \"\\\\d\" matches no string over the alphabet 'a'..='c'",
        ),
        (
            "[^a]",
            text_alphabet("a"),
            "regex pattern \"[^a]\" matches no string over the alphabet 'a'",
        ),
        (
            "[ĀĒ]",
            codepoint_alphabet(0, 127),
            "regex pattern \"[ĀĒ]\" matches no string over the alphabet '\\0'..='\\u{7f}'",
        ),
        (
            "abc",
            codepoint_alphabet(128, 0x10FFFF),
            "regex pattern \"abc\" matches no string over the alphabet '\\u{80}'..='\\u{d7ff}', '\\u{e000}'..='\\u{10ffff}'",
        ),
        (
            "a",
            text_alphabet(""),
            "regex pattern \"a\" matches no string over the alphabet of no characters",
        ),
    ] {
        for fullmatch in [true, false] {
            let Err(err) =
                StringSpec::regex(pattern, fullmatch, Some(&alphabet), UnicodeVersion::LATEST)
            else {
                panic!("{pattern:?} was built");
            };
            assert!(
                matches!(err, EngineError::InvalidArgument(_)),
                "{pattern:?}"
            );
            assert_eq!(err.to_string(), expected, "{pattern:?}");
        }
    }
}

/// A pattern the alphabet can supply some string of builds, however
/// many of its subpatterns it cannot supply.
#[test]
fn a_regex_generator_over_an_alphabet_that_can_match_its_pattern_is_built() {
    for (pattern, alphabet) in [
        ("[^a]|a", text_alphabet("a")),
        ("[^a]*a", text_alphabet("a")),
        ("(?-i:Ā)*k", codepoint_alphabet(0, 127)),
        ("(a)?(?(1)(?!)|k)", codepoint_alphabet(0, 127)),
        ("a?", text_alphabet("bcd")),
        ("(?i)A", text_alphabet("a")),
        ("", text_alphabet("a")),
        ("", text_alphabet("")),
    ] {
        for fullmatch in [true, false] {
            assert!(
                StringSpec::regex(pattern, fullmatch, Some(&alphabet), UnicodeVersion::LATEST)
                    .is_ok(),
                "{pattern:?}"
            );
        }
    }
}

/// A pattern no string matches at all is rejected over the default
/// alphabet too, as matching nothing the generator can produce.
#[test]
fn a_regex_generator_whose_pattern_matches_nothing_is_rejected() {
    for pattern in ["(?!)", "a(?!)b", "(?:(?!)|(?!))x", "(?!)+"] {
        let Err(err) = StringSpec::regex(pattern, true, None, UnicodeVersion::LATEST) else {
            panic!("{pattern:?} was built");
        };
        assert!(
            matches!(err, EngineError::InvalidArgument(_)),
            "{pattern:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("regex pattern {pattern:?} matches no string the generator can produce")
        );
    }
    for pattern in ["(?!)?", "x(?!(?!))y", "(?!)|a", "(a)?(?(1)(?!)|k)", "(?!)*"] {
        assert!(
            StringSpec::regex(pattern, true, None, UnicodeVersion::LATEST).is_ok(),
            "{pattern:?}"
        );
    }
}
