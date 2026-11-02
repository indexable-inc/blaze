//! Match-width bounds of a parsed pattern: a port of CPython's
//! `SubPattern.getwidth`, which Lark uses to order the alternatives of a
//! terminal (longest possible match first) and which the grammar generator
//! uses for the smallest string a terminal can produce.

use super::parser::{OpCode, SubPattern};
use alloc::vec::Vec;

/// The widths of the capture groups seen so far, indexed by group number,
/// for backreferences, and the pattern a group not seen is found in when
/// a part of one is being measured (see [`get_width_in`]).
struct GroupWidths<'a> {
    widths: Vec<Option<(u64, Option<u64>)>>,
    pattern: Option<&'a SubPattern>,
}

impl GroupWidths<'_> {
    fn record(&mut self, group: u32, width: (u64, Option<u64>)) {
        let index = group as usize;
        if self.widths.len() <= index {
            self.widths.resize(index + 1, None);
        }
        self.widths[index] = Some(width);
    }

    fn get(&mut self, group: u32) -> (u64, Option<u64>) {
        if let Some(width) = self.widths.get(group as usize).copied().flatten() {
            return width;
        }
        match self.pattern.and_then(|pattern| find_group(pattern, group)) {
            Some(p) => {
                let width = width_of(p, self);
                self.record(group, width);
                width
            }
            None => (0, None),
        }
    }
}

/// The subpattern of capture group `g`, searched for through the whole
/// of `p`.
fn find_group(p: &SubPattern, g: u32) -> Option<&SubPattern> {
    for op in &p.data {
        let found = match op {
            OpCode::Subpattern {
                group: Some(id), p, ..
            } if *id == g => Some(p),
            OpCode::Subpattern { p, .. } | OpCode::AtomicGroup(p) => find_group(p, g),
            OpCode::Branch(alternatives) => alternatives.iter().find_map(|a| find_group(a, g)),
            OpCode::MaxRepeat { item, .. }
            | OpCode::MinRepeat { item, .. }
            | OpCode::PossessiveRepeat { item, .. } => find_group(item, g),
            OpCode::GroupRefExists { yes, no, .. } => {
                find_group(yes, g).or_else(|| no.as_ref().and_then(|no| find_group(no, g)))
            }
            OpCode::Assert { p, .. } | OpCode::AssertNot { p, .. } => find_group(p, g),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn add_max(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(a?.saturating_add(b?))
}

fn width_of(p: &SubPattern, groups: &mut GroupWidths<'_>) -> (u64, Option<u64>) {
    let mut lo: u64 = 0;
    let mut hi: Option<u64> = Some(0);
    for op in &p.data {
        let (l, h) = match op {
            OpCode::Literal(_) | OpCode::NotLiteral(_) | OpCode::Any | OpCode::In(_) => {
                (1, Some(1))
            }
            OpCode::At(_) | OpCode::Assert { .. } | OpCode::AssertNot { .. } | OpCode::Failure => {
                (0, Some(0))
            }
            OpCode::Branch(items) => {
                let mut l = u64::MAX;
                let mut h = Some(0);
                for item in items {
                    let (il, ih) = width_of(item, groups);
                    l = l.min(il);
                    h = match (h, ih) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        _ => None,
                    };
                }
                (l, h)
            }
            OpCode::AtomicGroup(inner) => width_of(inner, groups),
            OpCode::Subpattern { group, p, .. } => {
                let w = width_of(p, groups);
                if let Some(g) = group {
                    groups.record(*g, w);
                }
                w
            }
            OpCode::GroupRef(g) => groups.get(*g),
            OpCode::GroupRefExists { yes, no, .. } => {
                let (yl, yh) = width_of(yes, groups);
                match no {
                    Some(no) => {
                        let (nl, nh) = width_of(no, groups);
                        (
                            yl.min(nl),
                            match (yh, nh) {
                                (Some(a), Some(b)) => Some(a.max(b)),
                                _ => None,
                            },
                        )
                    }
                    None => (0, yh),
                }
            }
            OpCode::MaxRepeat { min, max, item }
            | OpCode::MinRepeat { min, max, item }
            | OpCode::PossessiveRepeat { min, max, item } => {
                let (il, ih) = width_of(item, groups);
                let l = il.saturating_mul(*min as u64);
                let h = if *max == super::constants::MAXREPEAT {
                    match ih {
                        Some(0) => Some(0),
                        _ => None,
                    }
                } else {
                    ih.map(|ih| ih.saturating_mul(*max as u64))
                };
                (l, h)
            }
        };
        lo = lo.saturating_add(l);
        hi = add_max(hi, h);
    }
    (lo, hi)
}

/// The shortest and longest strings `p` can match, in codepoints; `None`
/// for an unbounded maximum.
pub fn get_width(p: &SubPattern) -> (u64, Option<u64>) {
    get_width_within(p, &[])
}

/// [`get_width`] for a subpattern nested in a pattern whose capture
/// groups closed before it have the widths `groups`, by group number:
/// what the backreferences in `p` to those groups are as wide as.
pub fn get_width_within(
    p: &SubPattern,
    groups: &[Option<(u64, Option<u64>)>],
) -> (u64, Option<u64>) {
    let mut groups = GroupWidths {
        widths: groups.to_vec(),
        pattern: None,
    };
    width_of(p, &mut groups)
}

/// [`get_width`] for the subpattern `p` of `pattern`, with a
/// backreference in `p` as wide as its group wherever in `pattern` the
/// group is: the width of a lookbehind's body, which the parser requires
/// to be one number, where the matcher starts it.
pub fn get_width_in(p: &SubPattern, pattern: &SubPattern) -> (u64, Option<u64>) {
    let mut groups = GroupWidths {
        widths: Vec::new(),
        pattern: Some(pattern),
    };
    width_of(p, &mut groups)
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/re_width_tests.rs"]
mod tests;
