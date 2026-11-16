//! Canonical decomposition, Normalization Form D (UAX #15): each character replaced by its full
//! canonical decomposition (Hangul syllables by the algorithm of Unicode section 3.12), then each
//! run of non-starters put in canonical order, a stable sort by combining class.

use alloc::string::String;

use crate::{tables, value_in};

/// `Canonical_Combining_Class` of `c`; 0 is a starter.
pub fn combining_class(c: char) -> u8 {
    if u32::from(c) < 0x300 {
        return 0;
    }
    value_in(tables::COMBINING_CLASS, c, 0)
}

const S_BASE: u32 = 0xac00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11a7;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const S_COUNT: u32 = 19 * V_COUNT * T_COUNT;

/// Appends `s` in NFD to `out`. What `out` held before is left as it is, and a non-starter of `s`
/// is never moved in front of it.
pub fn nfd(s: &str, out: &mut String) {
    // Where the run of non-starters at the end of `out` begins.
    let mut run = out.len();
    for c in s.chars() {
        let cp = u32::from(c);
        if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
            // A Hangul syllable is L V or L V T, every one a starter.
            let index = cp - S_BASE;
            for part in [L_BASE + index / (V_COUNT * T_COUNT), V_BASE + index % (V_COUNT * T_COUNT) / T_COUNT] {
                out.push(char::from_u32(part).expect("a Hangul jamo"));
            }
            if index % T_COUNT != 0 {
                out.push(char::from_u32(T_BASE + index % T_COUNT).expect("a Hangul jamo"));
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

/// Appends `c` to `out` in canonical order: a non-starter goes after every character of the run
/// at the end of `out` whose class is not above its own.
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
