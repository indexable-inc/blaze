//! Applying fixes: a pure function from an item's text and the edits chosen to the new text.
//! Edits that overlap are refused, never merged or dropped: two fixes that touch one byte are a
//! choice for a person.

use blaze_own::{AString, AVec, Alloc};

use crate::{Findings, Span};

/// Why a set of edits could not be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixError {
    /// Two edits touch the same bytes, or insert at the same point.
    Overlap { first: Span, second: Span },
    /// An edit's range ends past the text or starts after it ends.
    OutOfRange { range: Span },
    /// An edit's range splits a character at byte `at`.
    NotCharBoundary { at: u32 },
}

/// `text` with every edit of `edits` (an item-relative range and its replacement) made at once.
/// The order of `edits` does not matter; overlapping edits are refused.
pub fn apply<'a>(text: &str, edits: &[(Span, &str)], alloc: Alloc<'a>) -> Result<AString<'a>, FixError> {
    let mut order: AVec<'a, usize> = AVec::with_capacity_in(edits.len(), alloc);
    for (at, (range, _)) in edits.iter().enumerate() {
        if range.start > range.end || range.end as usize > text.len() {
            return Err(FixError::OutOfRange { range: *range });
        }
        for end in [range.start, range.end] {
            if !text.is_char_boundary(end as usize) {
                return Err(FixError::NotCharBoundary { at: end });
            }
        }
        order.push(at);
    }
    order.sort_unstable_by_key(|at| (edits[*at].0.start, edits[*at].0.end));
    for pair in order.windows(2) {
        let (first, second) = (edits[pair[0]].0, edits[pair[1]].0);
        // Touching ends are apart, unless both are one point (two insertions there).
        let apart = first.end < second.start || (first.end == second.start && !(first.is_empty() && second.is_empty()));
        if !apart {
            return Err(FixError::Overlap { first, second });
        }
    }
    let grows: usize = edits.iter().map(|(_, new)| new.len()).sum();
    let mut out = AString::with_capacity_in(text.len() + grows, alloc);
    let mut at = 0usize;
    for index in order.iter() {
        let (range, new) = edits[*index];
        out.push_str(&text[at..range.start as usize]);
        out.push_str(new);
        at = range.end as usize;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// `text` with the first fix of every finding of `findings` made: the result `blaze check --fix`
/// writes when every finding's first fix is chosen.
pub fn apply_all<'a>(text: &str, findings: &Findings<'_>, alloc: Alloc<'a>) -> Result<AString<'a>, FixError> {
    let mut edits: AVec<'a, (Span, &str)> = AVec::new_in(alloc);
    for finding in findings.list.iter() {
        if let Some(fix) = findings.fixes_of(finding).first() {
            for edit in findings.edits_of(fix) {
                edits.push((edit.range, findings.text_of(edit)));
            }
        }
    }
    apply(text, &edits, alloc)
}
