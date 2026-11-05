//! The Unicode-mode character classes `\d`, `\s` and `\w` as tables over
//! the codespace, for the matcher's per-character tests and for the
//! lexer simulation's first-character sets. CPython defines the classes
//! through `unicodedata` (decimal digits; the `Z` categories plus a few
//! controls; letters, numbers and `_`), which here become one bitmap over
//! the Basic Multilingual Plane and sorted ranges above it, built once
//! per Unicode version from its General Category runs.

use super::constants::ChCode;
use crate::unicodedata::{Category, PerVersion, UnicodeVersion};
use alloc::vec::Vec;

const BMP_END: u32 = 0x10000;

/// One class: a bit per codepoint below [`BMP_END`], ranges above it, and
/// the ranges of the whole class.
struct ClassTable {
    bmp: Vec<u64>,
    high: Vec<(u32, u32)>,
    ranges: Vec<(u32, u32)>,
}

impl ClassTable {
    fn new(ranges: Vec<(u32, u32)>) -> ClassTable {
        let mut bmp = alloc::vec![0u64; (BMP_END / 64) as usize];
        let mut high = Vec::new();
        for &(start, end) in &ranges {
            for cp in start..=end.min(BMP_END - 1) {
                bmp[(cp / 64) as usize] |= 1 << (cp % 64);
            }
            if end >= BMP_END {
                high.push((start.max(BMP_END), end));
            }
        }
        ClassTable { bmp, high, ranges }
    }

    fn contains(&self, c: char) -> bool {
        let cp = c as u32;
        if cp < BMP_END {
            return self.bmp[(cp / 64) as usize] & (1 << (cp % 64)) != 0;
        }
        self.high
            .binary_search_by(|&(start, end)| {
                if end < cp {
                    core::cmp::Ordering::Less
                } else if start > cp {
                    core::cmp::Ordering::Greater
                } else {
                    core::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }
}

/// Whether CPython counts the character `cp` of General Category
/// `category` as a digit, a space and a word character, in Unicode mode.
fn classify(cp: u32, category: Category) -> [bool; 3] {
    let major = category.as_str().as_bytes()[0];
    let digit = category == Category::Nd;
    let space = char::from_u32(cp).is_some_and(is_space_by_code) || major == b'Z';
    let word = cp == u32::from(b'_') || major == b'L' || major == b'N';
    [digit, space, word]
}

/// Whether `c` is whitespace to CPython's Unicode-mode `\s` by its code
/// rather than its General Category, as CPython decides every ASCII
/// character: the ASCII whitespace (`\t` to `\r` and `\x1c` to the
/// space) and U+0085. Every other space character is in the `Z`
/// categories.
pub fn is_space_by_code(c: char) -> bool {
    matches!(c, '\t'..='\r' | '\x1c'..=' ' | '\u{85}')
}

fn build_tables(version: UnicodeVersion) -> [ClassTable; 3] {
    let mut ranges: [Vec<(u32, u32)>; 3] = Default::default();
    for (start, end, category) in version.category_runs() {
        let mut cp = start;
        while cp <= end {
            let classes = classify(cp, category);
            let run_end = if cp < 0x100 { cp } else { end };
            for (i, ranges) in ranges.iter_mut().enumerate() {
                if classes[i] {
                    match ranges.last_mut() {
                        Some(last) if last.1 + 1 == cp => last.1 = run_end,
                        _ => ranges.push((cp, run_end)),
                    }
                }
            }
            cp = run_end + 1;
        }
    }
    ranges.map(ClassTable::new)
}

fn table(category: ChCode, version: UnicodeVersion) -> Option<&'static ClassTable> {
    static TABLES: PerVersion<[ClassTable; 3]> = PerVersion::new();
    let tables = TABLES.get(version, || build_tables(version));
    match category {
        ChCode::Digit => Some(&tables[0]),
        ChCode::Space => Some(&tables[1]),
        ChCode::Word => Some(&tables[2]),
        ChCode::NotDigit | ChCode::NotSpace | ChCode::NotWord => None,
    }
}

/// Whether `c` is in the positive class `category` in Unicode mode under
/// `version`; `false` for the negated classes, which have no table.
pub fn in_class(c: char, category: ChCode, version: UnicodeVersion) -> bool {
    table(category, version).is_some_and(|table| table.contains(c))
}

/// The codepoint ranges of the positive class `category` in Unicode
/// mode under `version`, in increasing order with gaps between them, or
/// `None` for the negated classes.
pub fn class_ranges(category: ChCode, version: UnicodeVersion) -> Option<&'static [(u32, u32)]> {
    table(category, version).map(|table| table.ranges.as_slice())
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/re_categories_tests.rs"]
mod tests;
