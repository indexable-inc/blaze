use std::{
    string::{String, ToString},
    vec::Vec,
};

use super::*;

const SETS: [(&str, &[(u32, u32)]); 8] = [
    ("XID_START", tables::XID_START),
    ("XID_CONTINUE", tables::XID_CONTINUE),
    ("EMOJI", tables::EMOJI),
    ("OTHER_PUNCTUATION", tables::OTHER_PUNCTUATION),
    ("DECIMAL_NUMBER", tables::DECIMAL_NUMBER),
    ("EMOJI_STYLE_BASE", tables::EMOJI_STYLE_BASE),
    ("TEXT_STYLE_BASE", tables::TEXT_STYLE_BASE),
    ("EMOJI_MODIFIER_BASE", tables::EMOJI_MODIFIER_BASE),
];

const VALUED: [(&str, &[(u32, u32, u8)], u8); 3] = [
    ("WIDTH", tables::WIDTH, 1),
    ("GRAPHEME_BREAK", tables::GRAPHEME_BREAK, 0),
    ("COMBINING_CLASS", tables::COMBINING_CLASS, 0),
];

fn ch(cp: u32) -> Option<char> {
    char::from_u32(cp)
}

#[test]
fn every_table_is_sorted_disjoint_and_holds_no_surrogate() {
    for (name, set) in SETS {
        assert!(!set.is_empty(), "{name} is empty");
        for (at, &(first, last)) in set.iter().enumerate() {
            assert!(first <= last && last <= 0x10ffff, "{name}[{at}] is no range");
            assert!(ch(first).is_some() && ch(last).is_some(), "{name}[{at}] ends on a surrogate");
            if at > 0 {
                assert!(set[at - 1].1 < first, "{name}[{at}] is not past the one before");
            }
        }
    }
    for (name, table, default) in VALUED {
        for (at, &(first, last, value)) in table.iter().enumerate() {
            assert!(first <= last && last <= 0x10ffff && value != default, "{name}[{at}]");
            if at > 0 {
                let (_, before, said) = table[at - 1];
                assert!(before < first, "{name}[{at}] is not past the one before");
                // Equal neighbours are one range.
                assert!(!(before + 1 == first && said == value), "{name}[{at}] could join the one before");
            }
        }
    }
    for (at, &(key, start, len)) in tables::NFD.iter().enumerate() {
        assert!(at == 0 || tables::NFD[at - 1].0 < key, "NFD[{at}] is not sorted");
        assert!(len > 0 && usize::from(start) + usize::from(len) <= tables::NFD_CHARS.len(), "NFD[{at}]");
    }
}

/// Every range's ends are in its set and the code points just outside are not.
#[test]
fn range_ends_are_the_boundaries() {
    let reads: [(&str, fn(char) -> bool, &[(u32, u32)]); 5] = [
        ("XID_Start", is_xid_start, tables::XID_START),
        ("XID_Continue", is_xid_continue, tables::XID_CONTINUE),
        ("Emoji", is_emoji, tables::EMOJI),
        ("Po", is_other_punctuation, tables::OTHER_PUNCTUATION),
        ("Nd", is_decimal_number, tables::DECIMAL_NUMBER),
    ];
    for (name, read, set) in reads {
        for &(first, last) in set {
            for inside in [first, last] {
                assert!(read(ch(inside).expect("checked above")), "{name} U+{inside:04X}");
            }
            for outside in [first.checked_sub(1), last.checked_add(1)].into_iter().flatten() {
                if let Some(c) = ch(outside) {
                    assert!(!read(c), "{name} U+{outside:04X} is past a range's end");
                }
            }
        }
    }
}

#[test]
fn ascii_fast_paths_agree_with_the_tables() {
    for c in (0u8..=0x7f).map(char::from) {
        assert_eq!(is_xid_start(c), in_set(tables::XID_START, c), "{c:?}");
        assert_eq!(is_xid_continue(c), in_set(tables::XID_CONTINUE, c), "{c:?}");
        assert_eq!(is_decimal_number(c), in_set(tables::DECIMAL_NUMBER, c), "{c:?}");
        assert_eq!(combining_class(c), value_in(tables::COMBINING_CLASS, c, 0), "{c:?}");
        let mut one = [0; 4];
        assert_eq!(str_width(c.encode_utf8(&mut one)), 1, "{c:?} in a string");
    }
}

#[test]
fn identifiers_are_rusts() {
    for c in ['a', 'Z', 'ª', 'µ', 'é', 'ǅ', 'ℵ', '℘', '℮', '中', 'ᐁ', '𝔸'] {
        assert!(is_xid_start(c) && is_xid_continue(c), "{c:?} starts an identifier");
    }
    // `_` is no XID_Start (the lexer adds it); digits and these marks only continue one.
    for c in ['_', '0', '·', '\u{300}', '\u{203f}', '\u{0660}', '\u{200c}', '\u{200d}'] {
        assert!(!is_xid_start(c) && is_xid_continue(c), "{c:?} only continues one");
    }
    // ID_Start but not XID_Start (not closed under NFKC), Pattern_Syntax, punctuation, emoji.
    for c in ['\u{309b}', '\u{2e2f}', '-', ' ', '$', '😀', '\u{2028}', '\u{feff}'] {
        assert!(!is_xid_start(c) && !is_xid_continue(c), "{c:?} is in no identifier");
    }
}

#[test]
fn emoji_and_categories() {
    for c in ['#', '*', '0', '9', '©', '®', '‼', '😀', '🦀', '\u{1f3fb}'] {
        assert!(is_emoji(c), "{c:?} is Emoji");
    }
    for c in ['a', '!', '\u{200d}', '\u{fe0f}', '中'] {
        assert!(!is_emoji(c), "{c:?} is not Emoji");
    }
    for c in ['!', '"', '#', '%', '&', '\'', '*', ',', '.', '/', ':', ';', '?', '@', '\\', '¡', '§', '¿', '‼', '。'] {
        assert!(is_other_punctuation(c), "{c:?} is Po");
    }
    for c in ['(', ')', '-', '_', '$', '+', 'a', '“'] {
        assert!(!is_other_punctuation(c), "{c:?} is not Po");
    }
    for c in ['0', '9', '\u{0660}', '\u{0966}', '\u{ff10}', '\u{1d7ce}'] {
        assert!(is_decimal_number(c), "{c:?} is Nd");
    }
    for c in ['a', '²', '½', 'Ⅳ', '\u{2460}'] {
        assert!(!is_decimal_number(c), "{c:?} is not Nd");
    }
}

#[test]
fn char_widths() {
    let said: [(char, Option<usize>); 20] = [
        ('\0', None),
        ('\u{1b}', None),
        ('\u{7f}', None),
        ('\u{9f}', None),
        ('a', Some(1)),
        ('\u{a0}', Some(1)),
        ('\u{ad}', Some(0)),
        ('\u{300}', Some(0)),
        ('\u{200b}', Some(0)),
        ('\u{200d}', Some(0)),
        ('\u{fe0f}', Some(0)),
        ('\u{1160}', Some(0)),
        ('\u{1100}', Some(2)),
        ('中', Some(2)),
        ('Ａ', Some(2)),
        ('가', Some(2)),
        ('😀', Some(2)),
        ('\u{17d8}', Some(3)),
        ('\u{1f1e6}', Some(1)),
        ('é', Some(1)),
    ];
    for (c, width) in said {
        assert_eq!(char_width(c), width, "{c:?}");
    }
}

#[test]
fn str_widths() {
    let said: [(&str, usize); 22] = [
        ("", 0),
        ("hello", 5),
        ("\r\n", 1),
        ("a\r\nb", 3),
        ("\t", 1),
        ("Ｈｅｌｌｏ, ｗｏｒｌｄ!", 23),
        ("“𘀀”", 4),
        ("e\u{301}", 1),
        ("中文", 4),
        // An emoji presentation sequence, and one whose base has none.
        ("\u{2764}", 1),
        ("\u{2764}\u{fe0f}", 2),
        ("a\u{fe0f}", 1),
        // A text presentation sequence.
        ("\u{231a}", 2),
        ("\u{231a}\u{fe0e}", 1),
        // A keycap, a flag, three regional indicators, a modifier sequence.
        ("1\u{fe0f}\u{20e3}", 2),
        ("🇺🇸", 2),
        ("🇺🇸🇫", 3),
        ("👍\u{1f3fd}", 2),
        // ZWJ sequences: a scientist, a family, a rainbow flag.
        ("👩\u{200d}🔬", 2),
        ("👨\u{200d}👩\u{200d}👧\u{200d}👦", 2),
        ("🏳\u{fe0f}\u{200d}🌈", 2),
        // A subdivision flag: its tags are zero wide.
        ("🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}", 2),
    ];
    for (s, width) in said {
        assert_eq!(str_width(s), width, "{s:?}");
    }
}

fn clusters(s: &str) -> Vec<&str> {
    graphemes(s).collect()
}

#[test]
fn legacy_grapheme_clusters() {
    let said: [(&str, &[&str]); 12] = [
        ("", &[]),
        ("abc", &["a", "b", "c"]),
        ("a\r\nb", &["a", "\r\n", "b"]),
        ("\n\r", &["\n", "\r"]),
        ("e\u{301}\u{302}x", &["e\u{301}\u{302}", "x"]),
        ("\u{1100}\u{1161}\u{11a8}가", &["\u{1100}\u{1161}\u{11a8}", "가"]),
        ("각\u{11a8}", &["각\u{11a8}"]),
        ("🇺🇸🇫🇷🇩", &["🇺🇸", "🇫🇷", "🇩"]),
        ("👩\u{200d}🔬!", &["👩\u{200d}🔬", "!"]),
        ("a\u{200d}🔬", &["a\u{200d}", "🔬"]),
        // A spacing mark and a prepended character break in legacy clusters (GB9a, GB9b).
        ("\u{915}\u{93f}", &["\u{915}", "\u{93f}"]),
        ("\u{600}1", &["\u{600}", "1"]),
    ];
    for (s, want) in said {
        assert_eq!(clusters(s), want, "{s:?}");
    }
}

fn nfd_of(s: &str) -> String {
    nfd_chars(s).collect()
}

/// NFD as it was written before [`nfd_chars`]: each part pushed onto the text, a non-starter
/// inserted after the last of the run at the text's end whose class is not above its own (a
/// stable insertion sort). The reference the iterator is held to.
fn nfd_by_insertion(s: &str, out: &mut String) {
    fn put(out: &mut String, run: &mut usize, c: char) {
        let class = combining_class(c);
        if class == 0 {
            out.push(c);
            *run = out.len();
            return;
        }
        let tail = &out[*run..];
        let at = match tail.char_indices().rev().find(|&(_, it)| combining_class(it) <= class) {
            Some((at, it)) => *run + at + it.len_utf8(),
            None => *run,
        };
        out.insert(at, c);
    }
    const S_BASE: u32 = 0xac00;
    const S_COUNT: u32 = 19 * 21 * 28;
    let mut run = out.len();
    for c in s.chars() {
        let cp = u32::from(c);
        if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
            let index = cp - S_BASE;
            for part in [0x1100 + index / (21 * 28), 0x1161 + index % (21 * 28) / 28] {
                out.push(char::from_u32(part).expect("a Hangul jamo"));
            }
            if index % 28 != 0 {
                out.push(char::from_u32(0x11a7 + index % 28).expect("a Hangul jamo"));
            }
            run = out.len();
            continue;
        }
        let at = tables::NFD.partition_point(|&(key, _, _)| key < cp);
        match tables::NFD.get(at) {
            Some(&(key, start, len)) if key == cp => {
                let start = usize::from(start);
                for &part in &tables::NFD_CHARS[start..start + usize::from(len)] {
                    put(out, &mut run, char::from_u32(part).expect("a decomposition is characters"));
                }
            }
            _ => put(out, &mut run, c),
        }
    }
}

/// The iterator gives what the insertion sort wrote, and the deprecated `nfd` appends it.
#[test]
fn nfd_chars_is_the_insertion_sort() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let s = text(tape);
        let mut want = String::new();
        nfd_by_insertion(&s, &mut want);
        assert_eq!(nfd_of(&s), want, "{s:?}");
        let mut out = String::from("a\u{301}");
        nfd(&s, &mut out);
        assert_eq!(out, std::format!("a\u{301}{want}"), "{s:?}");
    });
}

#[test]
fn canonical_decomposition() {
    let said: [(&str, &str); 10] = [
        ("plain", "plain"),
        ("é", "e\u{301}"),
        ("\u{1e69}", "s\u{323}\u{307}"),
        ("a\u{307}\u{323}", "a\u{323}\u{307}"),
        ("\u{212b}", "A\u{30a}"),
        ("\u{2126}", "\u{3a9}"),
        ("\u{ac00}", "\u{1100}\u{1161}"),
        ("\u{ac01}", "\u{1100}\u{1161}\u{11a8}"),
        ("\u{d7a3}", "\u{1112}\u{1175}\u{11c2}"),
        // A non-starter with a starter's decomposition after it: U+0344 is two marks of 230.
        ("a\u{344}\u{316}", "a\u{316}\u{308}\u{301}"),
    ];
    for (s, want) in said {
        assert_eq!(nfd_of(s), want, "{s:?}");
    }
    // A run that begins in what `out` held is not ordered into it.
    let mut out = String::from("a\u{301}");
    nfd("\u{316}", &mut out);
    assert_eq!(out, "a\u{301}\u{316}");
}

/// Characters every rule above reads, for the property tests.
const ALPHABET: &[char] = &[
    'a', 'e', 's', 'A', '1', '_', ' ', '\r', '\n', '\t', '\u{0}', 'é', '\u{1e69}', '\u{300}', '\u{301}',
    '\u{307}', '\u{316}', '\u{323}', '\u{344}', '\u{93f}', '\u{915}', '\u{600}', '\u{1100}', '\u{1161}',
    '\u{11a8}', '가', '각', '中', '\u{200d}', '\u{fe0e}', '\u{fe0f}', '\u{20e3}', '\u{2764}', '\u{231a}',
    '👍', '\u{1f3fd}', '👩', '🔬', '🇺', '🇸', '🏴', '\u{e0067}', '\u{e007f}', '\u{212b}',
];

fn text(tape: &blaze_sim::Tape) -> String {
    tape.vec(0..=12, "text", |tape| *tape.one_of(ALPHABET, "char")).into_iter().collect()
}

#[test]
fn clusters_cover_the_text_and_none_is_empty() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let s = text(tape);
        let all = clusters(&s);
        assert!(all.iter().all(|it| !it.is_empty()), "{s:?}: {all:?}");
        assert_eq!(all.concat(), s);
        // A cluster is a cluster of its own: breaking each again changes nothing.
        for one in &all {
            assert_eq!(clusters(one), [*one], "{s:?}");
        }
    });
}

#[test]
fn nfd_is_idempotent_and_ordered() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let s = text(tape);
        let once = nfd_of(&s);
        assert_eq!(nfd_of(&once), once, "{s:?}");
        let classes: Vec<u8> = once.chars().map(combining_class).collect();
        for pair in classes.windows(2) {
            assert!(pair[1] == 0 || pair[0] <= pair[1], "{s:?} -> {once:?} is out of canonical order");
        }
        // Nothing left in NFD decomposes further.
        assert!(once.chars().all(|c| {
            let mut one = [0; 4];
            nfd_of(c.encode_utf8(&mut one)) == c.to_string()
        }));
    });
}

#[test]
fn width_adds_up_where_nothing_joins() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let s = text(tape);
        let left = tape.index(s.chars().count() + 1, "split");
        let at = s.char_indices().nth(left).map_or(s.len(), |(at, _)| at);
        // Free of sequences that join, two halves are the whole.
        let joins = s.chars().any(|c| matches!(c, '\r' | '\u{200d}' | '\u{fe0e}' | '\u{fe0f}' | '\u{1f3fd}' | '🇺' | '🇸' | '\u{20e3}'));
        if !joins {
            assert_eq!(str_width(&s[..at]) + str_width(&s[at..]), str_width(&s), "{s:?}");
        }
    });
}
