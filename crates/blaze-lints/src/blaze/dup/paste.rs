//! Copy-paste inconsistencies, after CP-Miner (Li, Lu, Myagmar and Zhou, OSDI 2004): two pieces
//! of one structure are a copy and its original, and the names of one map onto the other's,
//! position by position. A name the copy renamed in most places and left as it was in a few is
//! the paste nobody finished: `a` became `b` at [`RENAMED_FLOOR`] or more places and stayed `a`
//! at no more than a third as many, `b` is nowhere in the original, and `a` is in the copy only
//! where the original has it. The second form is in one list of statements: rows of the same
//! structure (`x.a = y.a; x.b = y.b; x.c = y.b;`) in which two names agree on every row but
//! one, and that one repeats the row above. Pure over tokens: the index and the body's check
//! feed it the runs.

use std::alloc::Global;

use super::shape::{Class, Tok};

/// The fewest places a copy renamed a name for a place it did not to be a mistake: two can be
/// a deliberate half-change, three is a pattern the author meant to keep.
pub(crate) const RENAMED_FLOOR: usize = 3;
/// The fewest tokens of a piece compared by its names: below it a piece holds too few names
/// for a rename to count three times.
pub(crate) const PASTE_FLOOR: u32 = 30;
/// The most pieces of one structure compared pairwise: past it the structure is an idiom
/// (a `match` arm of one call each), and the pairs would cost its square.
pub(crate) const PASTE_BUCKET_CAP: usize = 16;
/// The fewest rows in a row of statements of one structure: with two, either could be the
/// odd one.
pub(crate) const ROWS_FLOOR: usize = 3;

/// A rename a copy did not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mismatch {
    /// The token of the copy left unrenamed, by its index in the copy.
    pub(crate) at: usize,
    /// A token of the copy the name was renamed to, by its index there.
    pub(crate) renamed_at: usize,
    /// How many places the copy renamed it at.
    pub(crate) renamed: usize,
    /// How many places it was left at.
    pub(crate) kept: usize,
}

/// Whether a token is a name a copy can rename.
fn is_name(tok: &Tok) -> bool {
    matches!(tok.class, Class::Local | Class::Name)
}

/// The unfinished rename of `original` in `copy`, when `copy` has one: of two runs of one
/// structure (equal lengths and classes), the name with the most renamed places, first by
/// where the original uses it.
pub(crate) fn mismatch(original: &[Tok], copy: &[Tok]) -> Option<Mismatch> {
    if original.len() != copy.len()
        || original.iter().zip(copy).any(|(a, b)| a.class != b.class || (a.class == Class::Fixed && a.key != b.key))
    {
        return None;
    }
    let names: Vec<usize, Global> = {
        let mut out = Vec::new_in(Global);
        out.extend((0..original.len()).filter(|&i| is_name(&original[i])));
        out
    };
    let mut best: Option<Mismatch> = None;
    let mut done: Vec<u64, Global> = Vec::new_in(Global);
    for &i in &names {
        let from = original[i].text;
        if done.contains(&from) {
            continue;
        }
        done.push(from);
        let places: Vec<usize, Global> = {
            let mut out = Vec::new_in(Global);
            out.extend(names.iter().copied().filter(|&j| original[j].text == from));
            out
        };
        let kept: Vec<usize, Global> = {
            let mut out = Vec::new_in(Global);
            out.extend(places.iter().copied().filter(|&j| copy[j].text == from));
            out
        };
        let Some(&first_kept) = kept.first() else { continue };
        // The name the copy put in its place most often, and the first place it did.
        let mut to: Option<(u64, usize, usize)> = None;
        for &j in &places {
            let text = copy[j].text;
            if text == from || to.is_some_and(|(it, _, _)| it == text) {
                continue;
            }
            let count = places.iter().filter(|&&k| copy[k].text == text).count();
            if to.is_none_or(|(_, _, most)| count > most) {
                to = Some((text, j, count));
            }
        }
        let Some((to, renamed_at, renamed)) = to else { continue };
        let fresh = names.iter().all(|&j| original[j].text != to);
        let only_there = names.iter().all(|&j| copy[j].text != from || original[j].text == from);
        if renamed < RENAMED_FLOOR || kept.len() * 3 > renamed || !fresh || !only_there {
            continue;
        }
        let found = Mismatch { at: first_kept, renamed_at, renamed, kept: kept.len() };
        if best.is_none_or(|it| found.renamed > it.renamed) {
            best = Some(found);
        }
    }
    best
}

/// A row of statements that breaks the pattern of the rows around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OddRow {
    /// The row, by its index among the rows.
    pub(crate) row: usize,
    /// The token of the row that repeats the row above, by its index in the row.
    pub(crate) at: usize,
    /// The token of the row it agrees with in every other row, by its index in the row.
    pub(crate) with: usize,
}

/// The odd rows of `rows`, runs of one structure in a row: at name positions `p` and `q`, `p`
/// differs in every row, `q` equals `p` in every row but one, and in that one `q` is what
/// it was in the row above. At most one per row, by its first `q`.
pub(crate) fn odd_rows(rows: &[&[Tok]]) -> Vec<OddRow, Global> {
    let mut out = Vec::new_in(Global);
    let Some(first) = rows.first() else { return out };
    if rows.len() < ROWS_FLOOR || rows.iter().any(|it| it.len() != first.len()) {
        return out;
    }
    let names: Vec<usize, Global> = {
        let mut out = Vec::new_in(Global);
        out.extend((0..first.len()).filter(|&i| is_name(&first[i])));
        out
    };
    let text = |row: usize, at: usize| rows[row][at].text;
    for &p in &names {
        let distinct = (0..rows.len())
            .all(|a| (a + 1..rows.len()).all(|b| text(a, p) != text(b, p)));
        if !distinct {
            continue;
        }
        for &q in &names {
            if q == p {
                continue;
            }
            let odd: Vec<usize, Global> = {
                let mut it = Vec::new_in(Global);
                it.extend((0..rows.len()).filter(|&row| text(row, q) != text(row, p)));
                it
            };
            let [row] = odd[..] else { continue };
            if row == 0 || text(row, q) != text(row - 1, q) {
                continue;
            }
            if out.iter().any(|it: &OddRow| it.row == row) {
                continue;
            }
            out.push(OddRow { row, at: q, with: p });
        }
    }
    out.sort_by_key(|it| (it.row, it.at));
    out
}
