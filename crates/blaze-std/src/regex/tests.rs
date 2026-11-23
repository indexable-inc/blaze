use core::alloc::Allocator;

use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{Match, Regex, RegexError, RegexErrorKind, RegexLimits, RegexPlant};

fn assert_is_core_error<E: core::error::Error>(_: &E) {}

fn repeat_char<A: Allocator>(ch: char, count: usize, alloc: A) -> OwnString<A> {
    let mut s = OwnString::with_capacity_in(count * ch.len_utf8(), alloc);
    for _ in 0..count {
        s.push(ch);
    }
    s
}

#[test]
fn literals_dot_and_utf8() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::SHIPPED;
    let steps = lim.most_steps;

    let re = Regex::compile("café .!", lim, a).expect("compile");
    assert!(re.is_match("hello café ☕! bye", steps, a).expect("match"));
    let m = re.find("hello café ☕! bye", steps, a).expect("find").expect("some");
    assert_eq!(m.as_str("hello café ☕! bye"), "café ☕!");

    // `.` does not match `\n` by default, but does under `(?s)`.
    let dot = Regex::compile("a.b", lim, a).expect("compile");
    assert!(!dot.is_match("a\nb", steps, a).expect("match"));
    assert!(dot.is_match("aXb", steps, a).expect("match"));

    let dot_all = Regex::compile("(?s)a.b", lim, a).expect("compile");
    assert!(dot_all.is_match("a\nb", steps, a).expect("match"));

    let scoped = Regex::compile("(?s:a.b)c.d", lim, a).expect("compile");
    assert!(scoped.is_match("a\nbcXd", steps, a).expect("match"));
    assert!(!scoped.is_match("a\nbc\nd", steps, a).expect("match"));
}

#[test]
fn escapes_and_character_classes() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::USUAL;
    let steps = lim.most_steps;

    let re = Regex::compile(r"\d+\s+\w+\t\n\r\\\.[\x41-\x43]", lim, a).expect("compile");
    assert!(re.is_match("42   foo_9\t\n\r\\.B", steps, a).expect("match"));
    assert!(!re.is_match("42   foo_9\t\n\r\\.D", steps, a).expect("match"));

    let neg = Regex::compile(r"[^\d]+", lim, a).expect("compile");
    let m = neg.find("123abc456", steps, a).expect("find").expect("some");
    assert_eq!(m.as_str("123abc456"), "abc");

    // Shorthands `\D`, `\W`, `\S` both outside and inside `[...]`.
    let non_ws = Regex::compile(r"\S+", lim, a).expect("compile");
    let all = non_ws.find_all("  one\t\ntwo  three ", steps, a).expect("find_all");
    let mut words = OwnVec::new_in(a);
    for m in all.as_slice() {
        words.push(m.as_str("  one\t\ntwo  three "));
    }
    assert_eq!(words.as_slice(), &["one", "two", "three"]);

    let class_with_shorthand = Regex::compile(r"^[\d_]+$", lim, a).expect("compile");
    assert!(class_with_shorthand.is_match("12_34", steps, a).expect("match"));
    assert!(!class_with_shorthand.is_match("12a34", steps, a).expect("match"));

    // Leading `]` and edge `-` inside classes.
    let edge = Regex::compile(r"^[]a-]+$", lim, a).expect("compile");
    assert!(edge.is_match("]a-]", steps, a).expect("match"));
    assert!(!edge.is_match("b", steps, a).expect("match"));

    let lead_dash = Regex::compile(r"^[-z]+$", lim, a).expect("compile");
    assert!(lead_dash.is_match("-z-", steps, a).expect("match"));

    // Negated class under `(?i)` must exclude both cases.
    let neg_ci = Regex::compile(r"(?i)^[^a]+$", lim, a).expect("compile");
    assert!(!neg_ci.is_match("bAb", steps, a).expect("match"));
    assert!(!neg_ci.is_match("bab", steps, a).expect("match"));
    assert!(neg_ci.is_match("bBb", steps, a).expect("match"));
}

#[test]
fn anchors_multiline_and_word_boundaries() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::USUAL;
    let steps = lim.most_steps;

    let single = Regex::compile(r"^bar$", lim, a).expect("compile");
    assert!(!single.is_match("foo\nbar\nbaz", steps, a).expect("match"));
    assert!(single.is_match("bar", steps, a).expect("match"));

    let multi = Regex::compile(r"(?m)^bar$", lim, a).expect("compile");
    let m = multi.find("foo\nbar\nbaz", steps, a).expect("find").expect("some");
    assert_eq!(m, Match { start: 4, end: 7 });

    // `\A` and `\z` remain text-anchored even under `(?m)`.
    let text_anchored = Regex::compile(r"(?m)\Abar\z", lim, a).expect("compile");
    assert!(!text_anchored.is_match("foo\nbar\nbaz", steps, a).expect("match"));
    assert!(text_anchored.is_match("bar", steps, a).expect("match"));

    // Word boundaries `\b` and non-word boundaries `\B`.
    let word = Regex::compile(r"\bcat\b", lim, a).expect("compile");
    let text = "scatter cat category cat!";
    let matches = word.find_all(text, steps, a).expect("find_all");
    assert_eq!(matches.as_slice(), &[Match { start: 8, end: 11 }, Match { start: 21, end: 24 }]);

    let inner = Regex::compile(r"\Bcat\B", lim, a).expect("compile");
    let m = inner.find(text, steps, a).expect("find").expect("some");
    assert_eq!(m, Match { start: 1, end: 4 });
}

#[test]
fn captures_alternation_and_non_capturing_groups() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::USUAL;
    let steps = lim.most_steps;

    let re = Regex::compile(r"(?:https?://)?([a-z0-9.-]+)(?::(\d+))?(/.*)?", lim, a)
        .expect("compile");
    assert_eq!(re.capture_count(), 4);

    let text = "https://example.com:8080/api/v1";
    let caps = re.captures(text, steps, a).expect("captures").expect("some");
    assert_eq!(caps.len(), 4);
    assert_eq!(caps.group_str(text, 0), Some(text));
    assert_eq!(caps.group_str(text, 1), Some("example.com"));
    assert_eq!(caps.group_str(text, 2), Some("8080"));
    assert_eq!(caps.group_str(text, 3), Some("/api/v1"));

    let text2 = "example.org/home";
    let caps2 = re.captures(text2, steps, a).expect("captures").expect("some");
    assert_eq!(caps2.group_str(text2, 1), Some("example.org"));
    assert_eq!(caps2.get(2), None);
    assert_eq!(caps2.group_str(text2, 3), Some("/home"));

    // Leftmost-first alternation (`a|ab` matches `"a"` on `"ab"`).
    let alt = Regex::compile("a|ab", lim, a).expect("compile");
    let m = alt.find("ab", steps, a).expect("find").expect("some");
    assert_eq!(m.as_str("ab"), "a");

    // Failed branch must not leak capture slots into winning branch.
    let branch = Regex::compile("(a)|(b)", lim, a).expect("compile");
    let caps_b = branch.captures("b", steps, a).expect("captures").expect("some");
    assert_eq!(caps_b.get(1), None);
    assert_eq!(caps_b.group_str("b", 2), Some("b"));
}

#[test]
fn greedy_vs_lazy_and_counted_repetitions() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::USUAL;
    let steps = lim.most_steps;

    let html = "<b>one</b> and <b>two</b>";
    let greedy = Regex::compile("<b>.*</b>", lim, a).expect("compile");
    assert_eq!(greedy.find(html, steps, a).expect("find").expect("some").as_str(html), html);

    let lazy = Regex::compile("<b>.*?</b>", lim, a).expect("compile");
    let found = lazy.find_all(html, steps, a).expect("find_all");
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].as_str(html), "<b>one</b>");
    assert_eq!(found[1].as_str(html), "<b>two</b>");

    // `+?`, `??`, `{m}`, `{m,}`, `{m,n}`, `{m,n}?`, `{0}`.
    let plus_lazy = Regex::compile("a+?", lim, a).expect("compile");
    assert_eq!(plus_lazy.find("aaaa", steps, a).expect("find").expect("some").as_str("aaaa"), "a");

    let q_lazy = Regex::compile("a??b", lim, a).expect("compile");
    assert_eq!(q_lazy.find("ab", steps, a).expect("find").expect("some").as_str("ab"), "ab");

    let exact = Regex::compile(r"^\d{3}-\d{2,4}$", lim, a).expect("compile");
    assert!(exact.is_match("123-45", steps, a).expect("match"));
    assert!(exact.is_match("123-4567", steps, a).expect("match"));
    assert!(!exact.is_match("123-4", steps, a).expect("match"));
    assert!(!exact.is_match("123-45678", steps, a).expect("match"));

    let at_least = Regex::compile(r"^x{2,}$", lim, a).expect("compile");
    assert!(!at_least.is_match("x", steps, a).expect("match"));
    assert!(at_least.is_match("xx", steps, a).expect("match"));
    assert!(at_least.is_match("xxxxx", steps, a).expect("match"));

    let range_lazy = Regex::compile(r"a{1,4}?", lim, a).expect("compile");
    assert_eq!(range_lazy.find("aaaa", steps, a).expect("find").expect("some").as_str("aaaa"), "a");

    let range_greedy = Regex::compile(r"a{1,4}", lim, a).expect("compile");
    assert_eq!(
        range_greedy.find("aaaa", steps, a).expect("find").expect("some").as_str("aaaa"),
        "aaaa"
    );

    let zero = Regex::compile(r"^a{0}b$", lim, a).expect("compile");
    assert!(zero.is_match("b", steps, a).expect("match"));
    assert!(!zero.is_match("ab", steps, a).expect("match"));

    // Nullable nested loops terminate cleanly.
    let nullable = Regex::compile(r"(a*)*b", lim, a).expect("compile");
    assert_eq!(nullable.find("aaab", steps, a).expect("find").expect("some").as_str("aaab"), "aaab");
    let empty_loop = Regex::compile(r"(?:)*", lim, a).expect("compile");
    assert_eq!(empty_loop.find("abc", steps, a).expect("find"), Some(Match { start: 0, end: 0 }));
}

#[test]
fn inline_flags_replace_all_and_split() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::USUAL;
    let steps = lim.most_steps;

    let ci = Regex::compile(r"(?i)hello (?-i)World", lim, a).expect("compile");
    assert!(ci.is_match("HeLLo World", steps, a).expect("match"));
    assert!(!ci.is_match("HeLLo world", steps, a).expect("match"));

    // `replace_all` with literal and capture substitutions.
    let kv = Regex::compile(r"(\w+)=(\d+)", lim, a).expect("compile");
    let swapped = kv.replace_all("a=1, b=22", "$2:$1", steps, a).expect("replace_all");
    assert_eq!(swapped.as_str(), "1:a, 22:b");

    let braced = kv.replace_all("k=9", "${1}_val=$$${2}", steps, a).expect("replace_all");
    assert_eq!(braced.as_str(), "k_val=$9");

    let ws = Regex::compile(r"\s+", lim, a).expect("compile");
    let collapsed = ws.replace_all("a   b\t\nc", " ", steps, a).expect("replace_all");
    assert_eq!(collapsed.as_str(), "a b c");

    // `split`
    let sep = Regex::compile(r"\s*,\s*", lim, a).expect("compile");
    let parts = sep.split("alpha, beta ,gamma,", steps, a).expect("split");
    assert_eq!(parts.as_slice(), &["alpha", "beta", "gamma", ""]);

    let empty_re = Regex::compile("", lim, a).expect("compile");
    let chars_split = empty_re.split("ab", steps, a).expect("split");
    assert_eq!(chars_split.as_slice(), &["", "a", "b", ""]);
}

#[test]
fn catastrophic_backtracking_proof_linear_time() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::SHIPPED;

    // Classic pathological pattern: 25 `a?` followed by 25 `a`s (would take ~2^25 steps on a
    // backtracking engine). Thompson NFA / PikeVM solves it in strictly linear O(n * m) steps.
    let mut pat = OwnString::new_in(a);
    for _ in 0..25 {
        pat.push_str("a?");
    }
    for _ in 0..25 {
        pat.push('a');
    }

    let re = Regex::compile(pat.as_str(), lim, a).expect("compile pathological pattern");
    let input_25 = repeat_char('a', 25, a);
    let input_24 = repeat_char('a', 24, a);

    // Even a tight budget (50,000 steps << 2^25 = 33,554,432) easily suffices!
    let m = re.find(input_25.as_str(), 50_000, a).expect("linear find").expect("must match");
    assert_eq!(m, Match { start: 0, end: 25 });

    let none = re.find(input_24.as_str(), 50_000, a).expect("linear non-match");
    assert_eq!(none, None);
}

#[test]
fn syntax_and_budget_errors() {
    let h = heap();
    let a = h.alloc();
    let lim = RegexLimits::USUAL;

    let err_unclosed = Regex::compile("(abc", lim, a).expect_err("unclosed group");
    assert_eq!(err_unclosed, RegexError { offset: 0, kind: RegexErrorKind::UnclosedGroup });
    assert_is_core_error(&err_unclosed);
    assert!(format_in!(a, "{err_unclosed}").as_str().contains("unclosed group"));

    let err_close = Regex::compile("ab)", lim, a).expect_err("unexpected close");
    assert_eq!(err_close, RegexError { offset: 2, kind: RegexErrorKind::UnexpectedCloseParen });

    let err_class = Regex::compile("a[bc", lim, a).expect_err("unclosed class");
    assert_eq!(err_class, RegexError { offset: 1, kind: RegexErrorKind::UnclosedClass });

    let err_range = Regex::compile("[z-a]", lim, a).expect_err("reversed range");
    assert_eq!(
        err_range,
        RegexError { offset: 1, kind: RegexErrorKind::InvalidClassRange { start: 'z', end: 'a' } }
    );

    let err_esc = Regex::compile(r"\q", lim, a).expect_err("invalid escape");
    assert_eq!(err_esc, RegexError { offset: 0, kind: RegexErrorKind::InvalidEscape });

    let err_hex = Regex::compile(r"\xZZ", lim, a).expect_err("invalid hex");
    assert_eq!(err_hex.kind, RegexErrorKind::InvalidEscape);

    let err_q1 = Regex::compile("*abc", lim, a).expect_err("leading quantifier");
    assert_eq!(err_q1, RegexError { offset: 0, kind: RegexErrorKind::InvalidQuantifier });

    let err_q2 = Regex::compile("a**", lim, a).expect_err("double quantifier");
    assert_eq!(err_q2, RegexError { offset: 2, kind: RegexErrorKind::InvalidQuantifier });

    let err_q3 = Regex::compile("a{5,2}", lim, a).expect_err("inverted repeat");
    assert_eq!(err_q3, RegexError { offset: 1, kind: RegexErrorKind::InvalidQuantifier });

    let err_flag = Regex::compile("(?q)a", lim, a).expect_err("unknown flag");
    assert_eq!(err_flag.kind, RegexErrorKind::InvalidFlag);

    let err_flag_q = Regex::compile("(?i)*", lim, a).expect_err("quantified flag");
    assert_eq!(err_flag_q.kind, RegexErrorKind::InvalidQuantifier);

    // Budget limits.
    let tiny_bytes = RegexLimits { most_pattern_bytes: 3, ..lim };
    assert!(matches!(
        Regex::compile("abcd", tiny_bytes, a),
        Err(RegexError { kind: RegexErrorKind::PatternTooLarge { bytes: 4, limit: 3 }, .. })
    ));

    let tiny_caps = RegexLimits { most_captures: 1, ..lim };
    assert!(matches!(
        Regex::compile("(a)(b)", tiny_caps, a),
        Err(RegexError { kind: RegexErrorKind::TooManyCaptures { limit: 1 }, .. })
    ));

    let tiny_rep = RegexLimits { most_repeat: 4, ..lim };
    assert!(matches!(
        Regex::compile("a{5}", tiny_rep, a),
        Err(RegexError { kind: RegexErrorKind::RepeatTooLarge { count: 5, limit: 4 }, .. })
    ));

    let tiny_states = RegexLimits { most_states: 8, ..lim };
    assert!(matches!(
        Regex::compile("abcdefghijkl", tiny_states, a),
        Err(RegexError { kind: RegexErrorKind::TooManyStates { limit: 8 }, .. })
    ));

    let re = Regex::compile("a+b", lim, a).expect("compile");
    assert!(matches!(
        re.find("aaaaaaaaaab", 5, a),
        Err(RegexError { kind: RegexErrorKind::StepLimitExceeded { limit: 5, .. }, .. })
    ));
}

fn draw_from_pool<A: Allocator>(
    tape: &Tape,
    pool: &[&str],
    max_items: u64,
    alloc: A,
) -> OwnString<A> {
    let count = usize::try_from(tape.int_in(0..=max_items, "count")).unwrap_or(0);
    let pool_len = u64::try_from(pool.len()).unwrap_or(1);
    let mut out = OwnString::new_in(alloc);
    for _ in 0..count {
        let idx = usize::try_from(tape.below(pool_len, "frag")).unwrap_or(0);
        if let Some(frag) = pool.get(idx) {
            out.push_str(frag);
        }
    }
    out
}

#[test]
fn property_hostile_patterns_never_panic() {
    let h = heap();
    const FRAGS: &[&str] = &[
        "a", "b", "0", ".", "^", "$", "|", "*", "+", "?", "*?", "+?", "??", "(", ")", "(?:",
        "(?i)", "(?s)", "(?m)", "(?-i)", "(?i:", "[", "]", "[^", "a-z", "z-a", "-", "{0}", "{2}",
        "{1,3}", "{3,1}", "{1,}", "{", "}", "\\", "\\d", "\\D", "\\w", "\\W", "\\s", "\\S", "\\b",
        "\\B", "\\A", "\\z", "\\x41", "\\x", "\\q", "\\.", "é", "\n",
    ];
    const TEXT_FRAGS: &[&str] = &["a", "b", "0", " ", "\n", "_", "A", "é", "!", "ab"];

    check(Config::cases(256), |tape| -> Result<(), Broke> {
        let a = h.alloc();
        let pat = draw_from_pool(tape, FRAGS, 12, a);
        let text = draw_from_pool(tape, TEXT_FRAGS, 16, a);
        let limits = RegexLimits {
            most_pattern_bytes: 256,
            most_states: 256,
            most_captures: 8,
            most_repeat: 8,
            most_steps: 2_000,
        };
        if let Ok(re) = Regex::compile(pat.as_str(), limits, a) {
            let _ = re.is_match(text.as_str(), limits.most_steps, a);
            let _ = re.find(text.as_str(), limits.most_steps, a);
            drop(re.captures(text.as_str(), limits.most_steps, a));
            drop(re.replace_all(text.as_str(), "$0-X", limits.most_steps, a));
            drop(re.split(text.as_str(), limits.most_steps, a));
            if let Ok(all) = re.find_all(text.as_str(), limits.most_steps, a) {
                verify_find_all_invariants(text.as_str(), all.as_slice(), a)?;
            }
        }
        Ok(())
    });
}

fn verify_find_all_invariants<A: Allocator>(
    text: &str,
    matches: &[Match],
    alloc: A,
) -> Result<(), Broke> {
    let mut prev: Option<Match> = None;
    for &m in matches {
        if m.start > m.end || m.end > text.len() {
            return Err(Broke::Oracle(
                format_in!(alloc, "out-of-bounds match {m:?} for len {}", text.len())
                    .as_str()
                    .into(),
            ));
        }
        if !text.is_char_boundary(m.start) || !text.is_char_boundary(m.end) {
            return Err(Broke::Oracle(
                format_in!(alloc, "non-UTF8 boundary match {m:?}").as_str().into(),
            ));
        }
        if let Some(p) = prev {
            if p.end > m.start {
                return Err(Broke::Oracle(
                    format_in!(alloc, "overlapping matches {p:?} then {m:?}").as_str().into(),
                ));
            }
            if m.start == p.start && m.end == p.end {
                return Err(Broke::Oracle(
                    format_in!(alloc, "duplicate zero-progress match {m:?}").as_str().into(),
                ));
            }
        }
        prev = Some(m);
    }
    Ok(())
}

/// The invariant: compiled literal patterns match substring search and split reconstructs the exact input.
#[test]
fn property_literals_match_substring_search_and_split_reconstructs() {
    let h = heap();
    const ALPHA: &[&str] = &["a", "b", "c", "1", "é", " "];

    check(Config::cases(256), |tape| -> Result<(), Broke> {
        let a = h.alloc();
        let needle = draw_from_pool(tape, ALPHA, 5, a);
        let haystack = draw_from_pool(tape, ALPHA, 20, a);
        let lim = RegexLimits::USUAL;
        let re = Regex::compile(needle.as_str(), lim, a)
            .map_err(|e| Broke::Oracle(format_in!(a, "compile literal failed: {e}").as_str().into()))?;

        let expected = haystack.as_str().find(needle.as_str()).map(|s| Match {
            start: s,
            end: s + needle.len(),
        });
        let got = re
            .find(haystack.as_str(), lim.most_steps, a)
            .map_err(|e| Broke::Oracle(format_in!(a, "find failed: {e}").as_str().into()))?;
        if got != expected {
            return Err(Broke::Oracle(
                format_in!(
                    a,
                    "literal {:?} in {:?}: expected {expected:?}, got {got:?}",
                    needle.as_str(),
                    haystack.as_str()
                )
                .as_str()
                .into(),
            ));
        }
        let is_m = re
            .is_match(haystack.as_str(), lim.most_steps, a)
            .map_err(|e| Broke::Oracle(format_in!(a, "is_match failed: {e}").as_str().into()))?;
        if is_m != expected.is_some() {
            return Err(Broke::Oracle(
                format_in!(a, "is_match disagreed with find").as_str().into(),
            ));
        }

        let all = re
            .find_all(haystack.as_str(), lim.most_steps, a)
            .map_err(|e| Broke::Oracle(format_in!(a, "find_all failed: {e}").as_str().into()))?;
        verify_find_all_invariants(haystack.as_str(), all.as_slice(), a)?;

        let parts = re
            .split(haystack.as_str(), lim.most_steps, a)
            .map_err(|e| Broke::Oracle(format_in!(a, "split failed: {e}").as_str().into()))?;
        if parts.len() != all.len() + 1 {
            return Err(Broke::Oracle(
                format_in!(a, "split len {} != matches len + 1 ({})", parts.len(), all.len() + 1)
                    .as_str()
                    .into(),
            ));
        }
        let mut rebuilt = OwnString::new_in(a);
        for (i, part) in parts.as_slice().iter().enumerate() {
            rebuilt.push_str(part);
            if let Some(m) = all.get(i) {
                rebuilt.push_str(m.as_str(haystack.as_str()));
            }
        }
        if rebuilt.as_str() != haystack.as_str() {
            return Err(Broke::Oracle(
                format_in!(a, "split + matches did not reconstruct haystack").as_str().into(),
            ));
        }
        Ok(())
    });
}

#[test]
fn the_oracle_catches_inverted_greediness() {
    let h = heap();
    control(
        Config::cases(128),
        RegexPlant::None,
        RegexPlant::InvertGreediness,
        "",
        |tape, plant| {
            let a = h.alloc();
            let count = usize::try_from(tape.int_in(2..=12, "len")).unwrap_or(2);
            let text = repeat_char('a', count, a);
            let lim = RegexLimits::USUAL;

            let greedy = Regex::compile_planted("a+", lim, a, plant)
                .map_err(|e| Broke::Oracle(format_in!(a, "compile greedy: {e}").as_str().into()))?;
            let m_greedy = greedy
                .find(text.as_str(), lim.most_steps, a)
                .map_err(|e| Broke::Oracle(format_in!(a, "find greedy: {e}").as_str().into()))?;
            if m_greedy != Some(Match { start: 0, end: count }) {
                return Err(Broke::Oracle(
                    format_in!(a, "greedy a+ on len {count} returned {m_greedy:?}").as_str().into(),
                ));
            }

            let lazy = Regex::compile_planted("a+?", lim, a, plant)
                .map_err(|e| Broke::Oracle(format_in!(a, "compile lazy: {e}").as_str().into()))?;
            let m_lazy = lazy
                .find(text.as_str(), lim.most_steps, a)
                .map_err(|e| Broke::Oracle(format_in!(a, "find lazy: {e}").as_str().into()))?;
            if m_lazy != Some(Match { start: 0, end: 1 }) {
                return Err(Broke::Oracle(
                    format_in!(a, "lazy a+? on len {count} returned {m_lazy:?}").as_str().into(),
                ));
            }
            Ok(())
        },
    );
}

#[test]
fn the_oracle_catches_ignored_case_flag() {
    let h = heap();
    control(
        Config::cases(128),
        RegexPlant::None,
        RegexPlant::IgnoreCaseFlag,
        "",
        |tape, plant| {
            let a = h.alloc();
            const LETTERS: &[&str] = &["a", "b", "c", "x", "z"];
            let base = draw_from_pool(tape, LETTERS, 6, a);
            if base.is_empty() {
                return Ok(());
            }
            let mut upper = OwnString::new_in(a);
            for ch in base.as_str().chars() {
                upper.push(ch.to_ascii_uppercase());
            }
            let pat = format_in!(a, "(?i)^{}$", base.as_str());
            let lim = RegexLimits::USUAL;
            let re = Regex::compile_planted(pat.as_str(), lim, a, plant)
                .map_err(|e| Broke::Oracle(format_in!(a, "compile: {e}").as_str().into()))?;
            let matched = re
                .is_match(upper.as_str(), lim.most_steps, a)
                .map_err(|e| Broke::Oracle(format_in!(a, "is_match: {e}").as_str().into()))?;
            if !matched {
                return Err(Broke::Oracle(
                    format_in!(a, "(?i) pattern {:?} missed {:?}", pat.as_str(), upper.as_str())
                        .as_str()
                        .into(),
                ));
            }
            Ok(())
        },
    );
}
