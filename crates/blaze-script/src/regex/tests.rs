use super::{ErrorKind, Regex};

/// The texts of the first match's groups, `None` for no match.
fn groups(pattern: &str, haystack: &str) -> Option<Vec<Option<String>>> {
    let regex = Regex::new(pattern).unwrap_or_else(|e| panic!("{e}"));
    let caps = regex.captures(haystack)?;
    Some(caps.iter().map(|it| it.map(|m| m.as_str().to_owned())).collect())
}

/// Each row: a pattern, a text, and the first match's groups (`None`: no match).
#[test]
fn the_table_of_patterns_matches_as_written() {
    #[rustfmt::skip]
    let table: &[(&str, &str, Option<&[Option<&str>]>)] = &[
        // Literals, alternation, repetition greedy and lazy.
        ("abc", "xabcx", Some(&[Some("abc")])),
        ("a|b", "cb", Some(&[Some("b")])),
        ("a*", "baa", Some(&[Some("")])),
        ("a+", "baa", Some(&[Some("aa")])),
        ("a+?", "baa", Some(&[Some("a")])),
        ("a??b", "ab", Some(&[Some("ab")])),
        ("a{2}", "aaa", Some(&[Some("aa")])),
        ("a{2,}", "aaaa", Some(&[Some("aaaa")])),
        ("a{1,2}", "aaa", Some(&[Some("aa")])),
        ("a{1,2}?", "aaa", Some(&[Some("a")])),
        ("a{0}", "a", Some(&[Some("")])),
        ("a{3}", "aa", None),
        ("a|", "b", Some(&[Some("")])),
        ("", "abc", Some(&[Some("")])),
        ("()", "", Some(&[Some(""), Some("")])),
        ("x*", "\u{e9}", Some(&[Some("")])),
        // Groups.
        ("(a)(b)?", "ac", Some(&[Some("a"), Some("a"), None])),
        ("(?:ab)+", "ababx", Some(&[Some("abab")])),
        ("(?P<x>\\d+)-(?<y>\\d+)", "a 12-34", Some(&[Some("12-34"), Some("12"), Some("34")])),
        ("(a|ab)(c|bcd)", "abcd", Some(&[Some("abcd"), Some("a"), Some("bcd")])),
        ("(a|b)*", "ab", Some(&[Some("ab"), Some("b")])),
        ("(a*)*", "b", Some(&[Some(""), Some("")])),
        ("(a*)+", "b", Some(&[Some(""), Some("")])),
        // Dot, anchors, flags.
        (".", "\n", None),
        ("(?s).", "\n", Some(&[Some("\n")])),
        ("^b", "ab", None),
        ("(?m)^b", "a\nb", Some(&[Some("b")])),
        ("a$", "a\n", None),
        ("(?m)a$", "a\nb", Some(&[Some("a")])),
        ("\\Aa", "ba", None),
        ("a\\z", "aa", Some(&[Some("a")])),
        ("\\bfoo\\b", "a foo.", Some(&[Some("foo")])),
        ("\\bfoo\\b", "afoo", None),
        ("\\Boo", "foo", Some(&[Some("oo")])),
        // Classes.
        ("[a-c]+", "xbcay", Some(&[Some("bca")])),
        ("[^a-c]+", "abxyc", Some(&[Some("xy")])),
        ("[]a]+", "]a]b", Some(&[Some("]a]")])),
        ("[a-]+", "-a-", Some(&[Some("-a-")])),
        ("[\\w.]+", "a.b c", Some(&[Some("a.b")])),
        ("[\\d]+", "x42", Some(&[Some("42")])),
        ("[^\\s]+", "  ab ", Some(&[Some("ab")])),
        ("[\\w\\-,]+", "a-b,c d", Some(&[Some("a-b,c")])),
        ("[^\\n]+", "ab\nc", Some(&[Some("ab")])),
        // Perl classes, Unicode as Python's `re` reads them.
        ("\\w+", "\u{e9}_1 ", Some(&[Some("\u{e9}_1")])),
        ("\\d+", "x\u{663}4", Some(&[Some("\u{663}4")])),
        ("\\s+", "a\u{a0}\tb", Some(&[Some("\u{a0}\t")])),
        ("\\D+", "12ab3", Some(&[Some("ab")])),
        ("\\W+", "ab, c", Some(&[Some(", ")])),
        ("\\S+", "  x ", Some(&[Some("x")])),
        // Case folding.
        ("(?i)stra\u{df}e", "STRA\u{df}E", Some(&[Some("STRA\u{df}E")])),
        ("(?i)s", "\u{17f}", Some(&[Some("\u{17f}")])),
        ("(?i)k", "\u{212a}", Some(&[Some("\u{212a}")])),
        ("(?i)[a-c]+", "ABC", Some(&[Some("ABC")])),
        ("a(?i)b|c", "C", Some(&[Some("C")])),
        ("(?i:a)a", "Aa", Some(&[Some("Aa")])),
        ("(?i:a)a", "AA", None),
        ("(?i)a(?-i)a", "AA", None),
        ("(?i)a(?-i)a", "Aa", Some(&[Some("Aa")])),
        // Escapes.
        ("\\x41\\x{1F600}", "A\u{1f600}", Some(&[Some("A\u{1f600}")])),
        ("\\.\\+\\*\\?\\(\\)\\[\\]\\{\\}\\|\\^\\$\\-\\#\\&\\~", ".+*?()[]{}|^$-#&~", Some(&[Some(".+*?()[]{}|^$-#&~")])),
        ("a}b]", "a}b]", Some(&[Some("a}b]")])),
        // The tree's own patterns on lines like theirs.
        (r"\A\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(\w+)\s*;", "  #[cfg(test)] pub(crate) mod r#tests;", Some(&[Some("  #[cfg(test)] pub(crate) mod r#tests;"), Some("tests")])),
        (r"\A\s*#\[cfg\((?:test|all\(test\b[^\]]*)\)\]", "#[cfg(all(test, unix))]", Some(&[Some("#[cfg(all(test, unix))]")])),
        (r"//(?:\[(?P<revs>[\w\-,]+)])?~(?P<adjust>\?|\||[v\^]*)", "x; //[a,b]~^^ ERROR", Some(&[Some("//[a,b]~^^"), Some("a,b"), Some("^^")])),
        (r"panicked at ([^:\s]+):\d+:\d+", "thread 'main' panicked at src/main.rs:3:5:", Some(&[Some("panicked at src/main.rs:3:5"), Some("src/main.rs")])),
        (r"^\s*/// The (invariant|refinement|reference model)(?: \(([a-z-]+)(?:: ([^)]+))?\))?: (.*)$", "    /// The invariant (dst: a b): it holds", Some(&[Some("    /// The invariant (dst: a b): it holds"), Some("invariant"), Some("dst"), Some("a b"), Some("it holds")])),
        (r"(?m)^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$", "## Title ##\nnext", Some(&[Some("## Title ##"), Some("Title")])),
        (r#"!?\[(?:[^\[\]]|\[[^\]]*\])*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#, r#"see [a [b]](x.md "t") end"#, Some(&[Some(r#"[a [b]](x.md "t")"#), Some("x.md")])),
        (r"(?m)^[ \t]*(?:pub(?:\([a-z: ]+\))?[ \t]+)?const[ \t]+([A-Z0-9_]*(?:SCHEMA|LAYOUT|FORMAT|MAGIC|VERSION)[A-Z0-9_]*)[ \t]*:", "x\npub(crate) const STORE_LAYOUT_V2: u32 = 1;", Some(&[Some("pub(crate) const STORE_LAYOUT_V2:"), Some("STORE_LAYOUT_V2")])),
        (r"(?s)\bstd::\{([^;]*)\}", "use std::{fs,\n io};", Some(&[Some("std::{fs,\n io}"), Some("fs,\n io")])),
        // asks blaze: regex test pattern literal matching the rustcsites scanner regex
        (r"(?i)Command::new\([^)]*(rustc|rustdoc|cargo|rustup)|MetadataCommand::new\(\)", "command::NEW(\"RUSTC\")", Some(&[Some("command::NEW(\"RUSTC"), Some("RUSTC")])),
        (r"(?m)^\s*(?:(?:private|protected|noncomputable|unsafe)\s+)*axiom\s", "theorem x\n  private axiom foo", Some(&[Some("  private axiom ")])),
        (r"(?m)\s*:=|\s+where\s*$", "theorem t : P := by", Some(&[Some(" :=")])),
        (r"\A[A-Za-z_][\w']*(?:\.[A-Za-z_][\w']*)*\n?\z", "Blaze.Mir.step'", Some(&[Some("Blaze.Mir.step'")])),
        (r#"\A"(blaze[ -][^"\\{}]*?[ -]\d+(?:\\0)?|blaze v2 [^"\\{}<]+)""#, r#""blaze v2 goal 3 solver""#, Some(&[Some(r#""blaze v2 goal 3 solver""#), Some("blaze v2 goal 3 solver")])),
        (r#"\A"(blaze[ -][^"\\{}]*?[ -]\d+(?:\\0)?|blaze v2 [^"\\{}<]+)""#, r#""blaze a 1\0", x"#, Some(&[Some(r#""blaze a 1\0""#), Some(r"blaze a 1\0")])),
        (r"\bE\d{4}\b", "error[E0308]: x", Some(&[Some("E0308")])),
    ];
    for (pattern, haystack, expected) in table {
        let expected = expected.map(|it| it.iter().map(|g| g.map(str::to_owned)).collect::<Vec<_>>());
        assert_eq!(groups(pattern, haystack), expected, "`{pattern}` on {haystack:?}");
        let regex = Regex::new(pattern).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(regex.is_match(haystack), expected.is_some(), "is_match of `{pattern}` on {haystack:?}");
    }
}

#[test]
fn iteration_does_not_overlap_and_skips_an_empty_match_where_the_last_ended() {
    let spans = |pattern: &str, haystack: &str| -> Vec<(usize, usize)> {
        let regex = Regex::new(pattern).unwrap_or_else(|e| panic!("{e}"));
        regex.find_iter(haystack).map(|m| (m.start(), m.end())).collect()
    };
    assert_eq!(spans("a*", "aab"), [(0, 2), (3, 3)]);
    assert_eq!(spans("", "ab"), [(0, 0), (1, 1), (2, 2)]);
    assert_eq!(spans("a|", "bab"), [(0, 0), (1, 2), (3, 3)]);
    assert_eq!(spans("", "\u{e9}"), [(0, 0), (2, 2)]);
    assert_eq!(spans("\\d+", "a1b22c333"), [(1, 2), (3, 5), (6, 9)]);
    // A look-behind sees the text before where a later search starts.
    assert_eq!(spans("\\bb", "ab b"), [(3, 4)]);
    let regex = Regex::new("(\\w)=(\\w)").unwrap_or_else(|e| panic!("{e}"));
    let pairs: Vec<String> = regex.captures_iter("a=1 b=2").map(|c| format!("{}{}", &c[1], &c[2])).collect();
    assert_eq!(pairs, ["a1", "b2"]);
}

#[test]
fn replacement_expands_groups_as_the_regex_crate_does() {
    let replaced = |pattern: &str, haystack: &str, with: &str| {
        Regex::new(pattern).unwrap_or_else(|e| panic!("{e}")).replace_all(haystack, with).into_owned()
    };
    assert_eq!(replaced("(\\d)(\\d)", "1234", "${2}${1}_$$"), "21_$43_$");
    assert_eq!(replaced("(?P<a>x)", "x", "[$a]"), "[x]");
    assert_eq!(replaced("(x)", "x", "$1_new"), "", "`$1_new` names the group `1_new`, which is none");
    assert_eq!(replaced("x", "axb", "$"), "a$b");
    assert_eq!(replaced("x", "x", "${1"), "${1");
    assert_eq!(replaced("(x)", "x", "$9"), "");
    let regex = Regex::new("a").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(regex.replace_all("bab", |c: &super::Captures<'_>| c[0].to_uppercase()), "bAb");
    assert!(matches!(regex.replace_all("bbb", "x"), std::borrow::Cow::Borrowed("bbb")));
    assert_eq!(regex.replace("aba", "x"), "xba", "`replace` replaces the first match only");
    let named = Regex::new("(a)(?P<n>b)(?:c)").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(named.captures_len(), 3);
    assert_eq!(named.capture_names().collect::<Vec<_>>(), [None, None, Some("n")]);
    let caps = named.captures("abc").unwrap_or_else(|| panic!("matches"));
    assert_eq!((&caps["n"], caps.name("n").map(|m| m.range())), ("b", Some(1..2)));
}

#[test]
fn escape_makes_a_pattern_that_matches_the_text() {
    let text = "a.b*c(d)[e]{f}|g^h$i\\j-k#l&m~n?o+p";
    let regex = Regex::new(&format!("\\A{}\\z", super::escape(text))).unwrap_or_else(|e| panic!("{e}"));
    assert!(regex.is_match(text));
    assert!(!regex.is_match("a.b*c(d)[e]{f}|g^h$i\\j-k#l&m~n?o+"));
}

#[test]
fn what_the_matcher_does_not_take_is_refused_by_name() {
    let deep_groups = format!("{}a{}", "(".repeat(300), ")".repeat(300));
    let deep_repeats = format!("a{}", "*".repeat(300));
    let cases: Vec<(&str, ErrorKind)> = vec![
        ("(a", ErrorKind::UnclosedGroup),
        ("a)", ErrorKind::UnopenedGroup),
        ("[a", ErrorKind::UnclosedClass),
        ("[[:alpha:]]", ErrorKind::NestedClass),
        ("[a&&b]", ErrorKind::ClassSetOperation),
        ("[z-a]", ErrorKind::BadRange { from: 'z', to: 'a' }),
        ("[a-\\d]", ErrorKind::PerlClassInRange),
        ("\\q", ErrorKind::BadEscape('q')),
        ("\\<", ErrorKind::BadEscape('<')),
        ("a\\", ErrorKind::TrailingBackslash),
        ("\\xZZ", ErrorKind::BadHex),
        ("(a)\\1", ErrorKind::Backreference),
        ("(?P<a>x)(?P=a)", ErrorKind::Backreference),
        ("(?=a)", ErrorKind::Lookaround),
        ("(?<!a)", ErrorKind::Lookaround),
        ("(?x)a", ErrorKind::UnknownFlag('x')),
        ("(?)", ErrorKind::EmptyFlags),
        ("*a", ErrorKind::RepetitionMissing),
        ("a{", ErrorKind::BadRepetition),
        ("a{2,1}", ErrorKind::RepetitionRange { min: 2, max: 1 }),
        ("(?P<1a>x)", ErrorKind::BadGroupName),
        ("(?P<a>x)(?P<a>y)", ErrorKind::DuplicateName("a".to_owned())),
        ("\\p{L}", ErrorKind::UnicodeClass),
        (deep_groups.as_str(), ErrorKind::TooDeep),
        (deep_repeats.as_str(), ErrorKind::TooDeep),
        ("a{1000}{1000}", ErrorKind::TooBig { limit: super::DEFAULT_LIMIT }),
    ];
    for (pattern, kind) in cases {
        match Regex::new(pattern) {
            Ok(_) => panic!("`{pattern}` is taken"),
            Err(e) => assert_eq!(e.kind(), &kind, "`{pattern}`: {e}"),
        }
    }
    assert!(Regex::with_limit("a", 4).is_ok(), "save, the character, save, match");
    assert!(matches!(Regex::with_limit("aa", 4).map_err(|e| e.kind().clone()), Err(ErrorKind::TooBig { limit: 4 })));
}

/// A small pattern, built to be printed as a pattern and matched by [`Backtrack`].
#[derive(Clone, Debug)]
enum Re {
    Lit(char),
    Any,
    Class(Vec<char>, bool),
    Start,
    End,
    Word,
    Group(usize, Box<Re>),
    NonCap(Box<Re>),
    Cat(Vec<Re>),
    Alt(Vec<Re>),
    Rep(Box<Re>, u32, Option<u32>, bool),
}

const ALPHABET: [char; 4] = ['a', 'b', ' ', '\n'];

fn letter(c: char, out: &mut String) {
    if c == '\n' { out.push_str("\\n") } else { out.push(c) }
}

impl Re {
    fn generate(tape: &blaze_sim::Tape, depth: u32, groups: &mut usize) -> Re {
        let kinds = if depth == 0 { 5 } else { 10 };
        match tape.below(kinds, "a node's kind") {
            0 | 1 => Re::Lit(tape.pick(&ALPHABET, "a letter").copied().unwrap_or('a')),
            2 => Re::Any,
            3 => {
                let mut set = tape.vec(1..=3, "a class's letters", |tape| tape.pick(&ALPHABET, "a letter").copied().unwrap_or('a'));
                set.sort_unstable();
                set.dedup();
                Re::Class(set, tape.coin("a class is negated"))
            }
            4 => match tape.below(3, "an assertion") {
                0 => Re::Start,
                1 => Re::End,
                _ => Re::Word,
            },
            5 => {
                *groups += 1;
                let index = *groups;
                Re::Group(index, Box::new(Re::generate(tape, depth - 1, groups)))
            }
            6 => Re::NonCap(Box::new(Re::generate(tape, depth - 1, groups))),
            7 => Re::Cat(tape.vec(0..=3, "a concatenation", |tape| Re::generate(tape, depth - 1, groups))),
            8 => Re::Alt(tape.vec(2..=3, "an alternation", |tape| Re::generate(tape, depth - 1, groups))),
            _ => {
                let inner = Re::generate(tape, depth - 1, groups);
                let min = tape.int_in(0..=2u32, "a repetition's least");
                let max = if tape.coin("a repetition is bounded") {
                    Some(min + tape.int_in(0..=2u32, "a repetition's more"))
                } else {
                    None
                };
                Re::Rep(Box::new(inner), min, max, !tape.coin("a repetition is lazy"))
            }
        }
    }

    fn render(&self, out: &mut String) {
        match self {
            Re::Lit(c) => letter(*c, out),
            Re::Any => out.push('.'),
            Re::Class(set, negated) => {
                out.push('[');
                if *negated {
                    out.push('^');
                }
                for c in set {
                    letter(*c, out);
                }
                out.push(']');
            }
            Re::Start => out.push('^'),
            Re::End => out.push('$'),
            Re::Word => out.push_str("\\b"),
            Re::Group(_, inner) => {
                out.push('(');
                inner.render(out);
                out.push(')');
            }
            Re::NonCap(inner) => {
                out.push_str("(?:");
                inner.render(out);
                out.push(')');
            }
            Re::Cat(items) => {
                for item in items {
                    if matches!(item, Re::Alt(_)) {
                        out.push_str("(?:");
                        item.render(out);
                        out.push(')');
                    } else {
                        item.render(out);
                    }
                }
            }
            Re::Alt(items) => {
                for (at, item) in items.iter().enumerate() {
                    if at > 0 {
                        out.push('|');
                    }
                    item.render(out);
                }
            }
            Re::Rep(inner, min, max, greedy) => {
                if matches!(**inner, Re::Lit(_) | Re::Any | Re::Class(..) | Re::Group(..) | Re::NonCap(_)) {
                    inner.render(out);
                } else {
                    out.push_str("(?:");
                    inner.render(out);
                    out.push(')');
                }
                match (min, max) {
                    (0, None) => out.push('*'),
                    (1, None) => out.push('+'),
                    (0, Some(1)) => out.push('?'),
                    (n, None) => out.push_str(&format!("{{{n},}}")),
                    (n, Some(m)) if n == m => out.push_str(&format!("{{{n}}}")),
                    (n, Some(m)) => out.push_str(&format!("{{{n},{m}}}")),
                }
                if !greedy {
                    out.push('?');
                }
            }
        }
    }
}

type Slots = Vec<Option<usize>>;

/// The reference: a backtracking matcher over [`Re`], trying alternatives in order, exponential
/// and plainly right on small inputs. An iteration of an unbounded repetition that matches
/// empty ends it, kept only when it is the first past the least count (`x*` is `(?:x+)?`).
struct Backtrack<'t> {
    text: &'t [char],
}

impl Backtrack<'_> {
    fn word(&self, at: Option<usize>) -> bool {
        at.and_then(|at| self.text.get(at)).is_some_and(|c| c.is_alphanumeric() || *c == '_')
    }

    fn run(&self, re: &Re, at: usize, caps: &mut Slots, k: &mut dyn FnMut(usize, &mut Slots) -> bool) -> bool {
        match re {
            Re::Lit(c) => self.text.get(at) == Some(c) && k(at + 1, caps),
            Re::Any => self.text.get(at).is_some_and(|c| *c != '\n') && k(at + 1, caps),
            Re::Class(set, negated) => self.text.get(at).is_some_and(|c| set.contains(c) != *negated) && k(at + 1, caps),
            Re::Start => at == 0 && k(at, caps),
            Re::End => at == self.text.len() && k(at, caps),
            Re::Word => (self.word(at.checked_sub(1)) != self.word(Some(at))) && k(at, caps),
            Re::Group(index, inner) => {
                let (open, close) = (2 * index, 2 * index + 1);
                let old = (caps[open], caps[close]);
                caps[open] = Some(at);
                let found = self.run(inner, at, caps, &mut |end, caps| {
                    let before = caps[close];
                    caps[close] = Some(end);
                    if k(end, caps) {
                        return true;
                    }
                    caps[close] = before;
                    false
                });
                if !found {
                    (caps[open], caps[close]) = old;
                }
                found
            }
            Re::NonCap(inner) => self.run(inner, at, caps, k),
            Re::Cat(items) => self.cat(items, at, caps, k),
            Re::Alt(items) => {
                for item in items {
                    if self.run(item, at, caps, k) {
                        return true;
                    }
                }
                false
            }
            Re::Rep(inner, min, max, greedy) => self.rep(inner, (*min, *max, *greedy), 0, at, caps, k),
        }
    }

    fn cat(&self, items: &[Re], at: usize, caps: &mut Slots, k: &mut dyn FnMut(usize, &mut Slots) -> bool) -> bool {
        match items.split_first() {
            None => k(at, caps),
            Some((first, rest)) => self.run(first, at, caps, &mut |next, caps| self.cat(rest, next, caps, k)),
        }
    }

    fn more(
        &self,
        inner: &Re,
        shape: (u32, Option<u32>, bool),
        count: u32,
        at: usize,
        caps: &mut Slots,
        k: &mut dyn FnMut(usize, &mut Slots) -> bool,
    ) -> bool {
        let (min, max, _) = shape;
        if max.is_some_and(|max| count >= max) {
            return false;
        }
        let tail = max.is_none() && count >= min;
        self.run(inner, at, caps, &mut |next, caps| {
            if tail && next == at {
                return count == min && k(next, caps);
            }
            self.rep(inner, shape, count + 1, next, caps, k)
        })
    }

    fn rep(
        &self,
        inner: &Re,
        shape: (u32, Option<u32>, bool),
        count: u32,
        at: usize,
        caps: &mut Slots,
        k: &mut dyn FnMut(usize, &mut Slots) -> bool,
    ) -> bool {
        let (min, _, greedy) = shape;
        if count < min {
            return self.more(inner, shape, count, at, caps, k);
        }
        if greedy {
            self.more(inner, shape, count, at, caps, k) || k(at, caps)
        } else {
            k(at, caps) || self.more(inner, shape, count, at, caps, k)
        }
    }

    /// The leftmost-first match at or after `start`: every group's slots.
    fn find(&self, re: &Re, groups: usize, start: usize) -> Option<Slots> {
        let whole = Re::Group(0, Box::new(re.clone()));
        for at in start..=self.text.len() {
            let mut caps = vec![None; 2 * (groups + 1)];
            if self.run(&whole, at, &mut caps, &mut |_, _| true) {
                return Some(caps);
            }
        }
        None
    }
}

/// Refinement: on small patterns and texts, the Pike VM's first match and its groups, and
/// its iteration, are the backtracking reference's.
#[test]
fn the_pike_vm_matches_what_backtracking_matches() {
    blaze_sim::check(blaze_sim::Config::cases(2000), |tape| {
        let mut groups = 0;
        let re = Re::generate(tape, 3, &mut groups);
        let text: Vec<char> = tape.vec(0..=6, "the text", |tape| tape.pick(&ALPHABET, "a letter").copied().unwrap_or('a'));
        let haystack: String = text.iter().collect();
        let mut pattern = String::new();
        re.render(&mut pattern);
        let regex = Regex::new(&pattern).unwrap_or_else(|e| panic!("`{pattern}` is refused: {e}"));
        assert_eq!(regex.captures_len(), groups + 1, "`{pattern}`");
        let reference = Backtrack { text: &text };
        let expected = reference.find(&re, groups, 0);
        let ours = regex.captures(&haystack).map(|caps| {
            caps.iter().flat_map(|it| match it {
                Some(m) => [Some(m.start()), Some(m.end())],
                None => [None, None],
            })
            .collect::<Slots>()
        });
        assert_eq!(ours, expected, "captures of `{pattern}` on {haystack:?}");
        assert_eq!(regex.is_match(&haystack), expected.is_some(), "is_match of `{pattern}` on {haystack:?}");
        // The iteration, by the same rule from the reference's own matches.
        let mut spans = Vec::new();
        let (mut at, mut last_end) = (0, None);
        while at <= text.len() {
            let Some(found) = reference.find(&re, groups, at) else { break };
            let (start, end) = (found[0].unwrap_or(0), found[1].unwrap_or(0));
            if start == end && last_end == Some(end) {
                at += 1;
                continue;
            }
            spans.push((start, end));
            at = end;
            last_end = Some(end);
        }
        let ours: Vec<(usize, usize)> = regex.find_iter(&haystack).map(|m| (m.start(), m.end())).collect();
        assert_eq!(ours, spans, "find_iter of `{pattern}` on {haystack:?}");
    });
}
