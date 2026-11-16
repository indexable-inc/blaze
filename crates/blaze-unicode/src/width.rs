//! Display width in terminal columns, by UAX #11 and the emoji sequences of UTS #51, outside an
//! East Asian context (ambiguous characters are narrow).
//!
//! A character's width: none for a control (U+0000..U+001F, U+007F..U+009F); 0 for a
//! default-ignorable character, a grapheme extender, a Hangul medial vowel or final consonant
//! and the zero-width prepended marks; 2 for East Asian Wide and Fullwidth; 3 for U+17D8; else 1.
//!
//! A string's width is the sum of its characters', where a control counts 1, except for these
//! sequences: `"\r\n"` is 1; an emoji presentation sequence (a base and U+FE0F), an emoji
//! modifier sequence, a well-formed emoji ZWJ sequence (a keycap in it included) and a
//! pair of regional indicators are 2; a text presentation sequence (an `Emoji_Presentation`
//! base outside U+1F200..U+1F2FF and U+FE0E) is 1. These are unicode-width 0.2's sequence rules
//! but its script ligatures (Arabic lam-alef, Hebrew alef-lamed, Buginese, Khmer coeng, Lisu,
//! Old Turkic, Tifinagh, Kirat Rai) and its U+FE00..U+FE02 quotation rule, which this does not
//! read: those strings measure as the sum of their characters.

use crate::{in_set, tables, value_in};

/// The width class of `c` at or past U+00A0 in the table: 0, 1, 2, 3, or [`EMOJI`].
fn class(c: char) -> u8 {
    value_in(tables::WIDTH, c, 1)
}

/// The class of a character 2 wide with `Emoji_Presentation`, which can go on a ZWJ sequence.
const EMOJI: u8 = 4;

/// The display width of `c`, or none for a control character.
pub fn char_width(c: char) -> Option<usize> {
    match c {
        '\u{20}'..='\u{7e}' => Some(1),
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => None,
        _ => Some(match class(c) {
            EMOJI => 2,
            it => usize::from(it),
        }),
    }
}

/// What the characters after the one being read make of it, read right to left.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Nothing that joins with what comes before.
    Free,
    /// U+000A: a U+000D before it adds nothing.
    LineFeed,
    /// An emoji: a ZWJ before it continues the sequence.
    Emoji,
    /// An emoji modifier (U+1F3FB..U+1F3FF): a modifier base before it adds nothing.
    Modifier,
    /// One regional indicator: one before it makes a flag.
    Regional,
    /// A run of regional indicators.
    Regionals,
    /// U+FE0E: a text presentation base before it is 1 wide.
    TextStyle,
    /// U+FE0F: an emoji presentation base before it is 2 wide.
    EmojiStyle,
    /// ZWJ and an emoji: an emoji before it adds nothing.
    Zwj,
    /// U+FE0F, ZWJ and an emoji.
    EmojiStyleZwj,
    /// U+20E3 (keycap), ZWJ and an emoji.
    KeycapZwj,
    /// U+FE0F, U+20E3, ZWJ and an emoji.
    EmojiStyleKeycapZwj,
}

const ZWJ: char = '\u{200d}';

fn is_regional(c: char) -> bool {
    matches!(c, '\u{1f1e6}'..='\u{1f1ff}')
}

/// The width `c` adds before `next`, and what it makes for the character before it.
fn step(c: char, mut next: Next) -> (usize, Next) {
    if matches!(next, Next::EmojiStyle | Next::EmojiStyleZwj | Next::EmojiStyleKeycapZwj) {
        if in_set(tables::EMOJI_STYLE_BASE, c) {
            return (if next == Next::EmojiStyle { 2 } else { 0 }, Next::Emoji);
        }
        next = Next::Free;
    }
    if c <= '\u{a0}' {
        return match c {
            '\n' => (1, Next::LineFeed),
            '\r' if next == Next::LineFeed => (0, Next::Free),
            _ => (1, Next::Free),
        };
    }
    if next != Next::Free {
        match c {
            '\u{fe0f}' => {
                let styled = match next {
                    Next::Zwj => Next::EmojiStyleZwj,
                    Next::KeycapZwj => Next::EmojiStyleKeycapZwj,
                    _ => Next::EmojiStyle,
                };
                return (0, styled);
            }
            '\u{fe0e}' => return (0, Next::TextStyle),
            _ => {}
        }
        if next == Next::TextStyle {
            if in_set(tables::TEXT_STYLE_BASE, c) {
                return (1, Next::Free);
            }
            next = Next::Free;
        }
        match (next, c) {
            (Next::Modifier, _) if in_set(tables::EMOJI_MODIFIER_BASE, c) => return (0, Next::Emoji),
            (Next::Regional | Next::Regionals, _) if is_regional(c) => return (1, Next::Regionals),
            (Next::Emoji | Next::Regionals | Next::Modifier, ZWJ) => return (0, Next::Zwj),
            (Next::Zwj, '\u{20e3}') => return (0, Next::KeycapZwj),
            (Next::Zwj, '\u{1f3fb}'..='\u{1f3ff}') => return (0, Next::Modifier),
            (Next::Zwj, _) if class(c) == EMOJI => return (0, Next::Emoji),
            _ => {}
        }
    }
    let made = match c {
        '\u{fe0e}' => Next::TextStyle,
        '\u{fe0f}' => Next::EmojiStyle,
        '\u{1f3fb}'..='\u{1f3ff}' => Next::Modifier,
        _ if is_regional(c) => Next::Regional,
        _ => match class(c) {
            EMOJI => Next::Emoji,
            _ => Next::Free,
        },
    };
    let width = match class(c) {
        EMOJI => 2,
        it => usize::from(it),
    };
    (width, made)
}

/// The display width of `s` in columns.
pub fn str_width(s: &str) -> usize {
    if s.is_ascii() {
        // Every ASCII character, a control too, is 1, but `"\r\n"`.
        return s.len() - s.as_bytes().windows(2).filter(|it| **it == *b"\r\n").count();
    }
    // No step takes width away, so the sum only grows.
    let (sum, _) = s.chars().rfold((0, Next::Free), |(sum, next), c| {
        let (add, made) = step(c, next);
        (sum + add, made)
    });
    sum
}
