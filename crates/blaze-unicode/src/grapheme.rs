//! Legacy grapheme clusters (UAX #29): the rules GB3 to GB13 but GB9a, GB9b and GB9c, which
//! only extended clusters have. A spacing mark or a prepended character is its own cluster
//! here, as in `unicode_segmentation::UnicodeSegmentation::graphemes(s, false)`, which the
//! pinned nightly's rustfmt breaks strings by.

use crate::{tables, value_in};

/// `Grapheme_Cluster_Break`, as the table numbers it; `Extended_Pictographic` is one more value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Break {
    Any,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    Regional,
    L,
    V,
    T,
    Lv,
    Lvt,
    Pictographic,
}

fn break_of(c: char) -> Break {
    if let '\u{ac00}'..='\u{d7a3}' = c {
        // A Hangul syllable is LV when it has no final consonant (UAX #29, Hangul_Syllable_Type).
        return if (u32::from(c) - 0xac00) % 28 == 0 { Break::Lv } else { Break::Lvt };
    }
    match value_in(tables::GRAPHEME_BREAK, c, 0) {
        1 => Break::Cr,
        2 => Break::Lf,
        3 => Break::Control,
        4 => Break::Extend,
        5 => Break::Zwj,
        6 => Break::Regional,
        7 => Break::L,
        8 => Break::V,
        9 => Break::T,
        10 => Break::Lv,
        11 => Break::Lvt,
        12 => Break::Pictographic,
        _ => Break::Any,
    }
}

/// The legacy grapheme clusters of `s`, in order; they concatenate to `s`.
pub fn graphemes(s: &str) -> Graphemes<'_> {
    Graphemes { rest: s }
}

/// The iterator [`graphemes`] returns.
pub struct Graphemes<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Graphemes<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let mut chars = self.rest.char_indices();
        let (_, first) = chars.next()?;
        let mut before = break_of(first);
        // Regional indicators in a row ending at `before` (GB12, GB13).
        let mut regionals = usize::from(before == Break::Regional);
        // GB11: `before` ends an `Extended_Pictographic Extend*` (Some(false)) or that and a ZWJ
        // (Some(true)).
        let mut pictographic = (before == Break::Pictographic).then_some(false);
        let mut end = self.rest.len();
        for (at, c) in chars {
            let after = break_of(c);
            let joined = match (before, after) {
                (Break::Cr, Break::Lf) => true,
                (Break::Control | Break::Cr | Break::Lf, _) | (_, Break::Control | Break::Cr | Break::Lf) => false,
                // GB6, GB7, GB8 (Hangul), GB9.
                (Break::L, Break::L | Break::V | Break::Lv | Break::Lvt)
                | (Break::Lv | Break::V, Break::V | Break::T)
                | (Break::Lvt | Break::T, Break::T)
                | (_, Break::Extend | Break::Zwj) => true,
                (Break::Zwj, Break::Pictographic) => pictographic == Some(true),
                (Break::Regional, Break::Regional) => regionals % 2 == 1,
                _ => false,
            };
            if !joined {
                end = at;
                break;
            }
            regionals = if after == Break::Regional { regionals + 1 } else { 0 };
            pictographic = match (after, pictographic) {
                (Break::Pictographic, _) => Some(false),
                (Break::Extend, Some(false)) => Some(false),
                (Break::Zwj, Some(false)) => Some(true),
                _ => None,
            };
            before = after;
        }
        let (cluster, rest) = self.rest.split_at(end);
        self.rest = rest;
        Some(cluster)
    }
}
