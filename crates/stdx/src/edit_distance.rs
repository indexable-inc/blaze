//! rustc's edit distance and its choice of a similar name to suggest
//! (`compiler/rustc_span/src/edit_distance.rs`, MIT or Apache-2.0), and cargo's case-blind
//! variant ([`cargo`]). Diagnostics must suggest what rustc and cargo suggest, so every rule,
//! default and tie-break here is theirs.

use std::cmp;

/// rustc's restricted Damerau-Levenshtein distance (optimal string alignment) between `a` and
/// `b`, counted in code points, or `None` if it is over `limit`.
pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    if a.is_ascii() && b.is_ascii() {
        return distance(a.as_bytes(), b.as_bytes(), limit);
    }
    // alloc: code points of a non-ASCII name, on a suggestion's (error) path.
    let a: Vec<char> = a.chars().collect();
    // alloc: as above.
    let b: Vec<char> = b.chars().collect();
    distance(&a, &b, limit)
}

/// The distance over code units: bytes when both sides are ASCII (where bytes are code points),
/// else `char`s.
fn distance<T: PartialEq>(a: &[T], b: &[T], limit: usize) -> Option<usize> {
    let (mut a, mut b) = if a.len() < b.len() { (b, a) } else { (a, b) };
    let min_dist = a.len() - b.len();
    if min_dist > limit {
        return None;
    }
    while let (Some((a_first, a_rest)), Some((b_first, b_rest))) = (a.split_first(), b.split_first())
        && a_first == b_first
    {
        a = a_rest;
        b = b_rest;
    }
    while let (Some((a_last, a_rest)), Some((b_last, b_rest))) = (a.split_last(), b.split_last())
        && a_last == b_last
    {
        a = a_rest;
        b = b_rest;
    }
    if b.is_empty() {
        return Some(min_dist);
    }
    let width = b.len() + 1;
    // alloc: the table's three rows, one buffer, on a suggestion's (error) path.
    let mut rows = vec![usize::MAX; 3 * width];
    let (mut prev_prev, mut prev, mut current) = (0, width, 2 * width);
    for (j, cell) in rows[prev..prev + width].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        rows[current] = i;
        for j in 1..=b.len() {
            let substitution_cost = usize::from(a[i - 1] != b[j - 1]);
            let mut cell =
                cmp::min(rows[prev + j] + 1, cmp::min(rows[current + j - 1] + 1, rows[prev + j - 1] + substitution_cost));
            // `prev_prev` holds a real row whenever `i > 1`: row 0 after the first rotation.
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cell = cmp::min(cell, rows[prev_prev + j - 2] + 1);
            }
            rows[current + j] = cell;
        }
        (prev_prev, prev, current) = (prev, current, prev_prev);
    }
    let distance = rows[prev + b.len()];
    (distance <= limit).then_some(distance)
}

/// rustc's `edit_distance_with_substrings`: [`edit_distance`] scoring a substring as near, so
/// that `foo` is closer to `foobar` than to `bar`. `limit + len_diff` saturates where rustc's
/// would overflow (only for a `limit` near `usize::MAX`, which rustc never passes).
pub fn edit_distance_with_substrings(a: &str, b: &str, limit: usize) -> Option<usize> {
    let n = a.chars().count();
    let m = b.chars().count();
    let big_len_diff = (n * 2) < m || (m * 2) < n;
    let len_diff = m.abs_diff(n);
    let distance = edit_distance(a, b, limit.saturating_add(len_diff))?;
    let score = distance - len_diff;
    let score = if score == 0 && len_diff > 0 && !big_len_diff {
        1
    } else if !big_len_diff {
        score + len_diff.div_ceil(2)
    } else {
        score + len_diff
    };
    (score <= limit).then_some(score)
}

/// rustc's `find_best_match_for_name`: the candidate equal to `lookup` but for case, else the
/// nearest within `dist` (a third of `lookup`'s length, at least one, by default; the first of
/// equally near ones), else one with the same `_`-separated words in another order (the last
/// such).
pub fn find_best_match_for_name<'c, S: AsRef<str>>(candidates: &'c [S], lookup: &str, dist: Option<usize>) -> Option<&'c S> {
    find_best_match_for_name_by(candidates, as_str::<S>, lookup, dist)
}

/// [`find_best_match_for_name`] over candidates whose name `name` gives.
pub fn find_best_match_for_name_by<'c, T>(
    candidates: &'c [T],
    name: impl Fn(&T) -> &str,
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'c T> {
    // alloc: the candidates as references, on a suggestion's (error) path.
    let candidates: Vec<&T> = candidates.iter().collect();
    find_best_match(false, &candidates, &name, lookup, dist)
}

/// rustc's `find_best_match_for_name_with_substrings`: [`find_best_match_for_name`] scoring
/// by [`edit_distance_with_substrings`], the nearest ones then compared by [`edit_distance`].
pub fn find_best_match_for_name_with_substrings<'c, S: AsRef<str>>(
    candidates: &'c [S],
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'c S> {
    // alloc: as in `find_best_match_for_name_by`.
    let candidates: Vec<&S> = candidates.iter().collect();
    find_best_match(true, &candidates, &as_str::<S>, lookup, dist)
}

/// rustc's `find_best_match_for_names`: the best match of any of `lookups`, the one nearest its
/// lookup (the first of equally near ones).
pub fn find_best_match_for_names<'c, S: AsRef<str>>(
    candidates: &'c [S],
    lookups: &[&str],
    dist: Option<usize>,
) -> Option<&'c S> {
    // alloc: as in `find_best_match_for_name_by`.
    let candidates: Vec<&S> = candidates.iter().collect();
    lookups
        .iter()
        .filter_map(|&lookup| Some((lookup, find_best_match(false, &candidates, &as_str::<S>, lookup, dist)?)))
        .min_by_key(|&(lookup, found)| edit_distance(lookup, as_str(found), usize::MAX))
        .map(|(_, found)| found)
}

fn find_best_match<'c, T: ?Sized, F: Fn(&T) -> &str>(
    use_substring_score: bool,
    candidates: &[&'c T],
    name: &F,
    lookup: &str,
    dist: Option<usize>,
) -> Option<&'c T> {
    if let Some(&found) = candidates.iter().find(|&&it| eq_uppercase(name(it), lookup)) {
        return Some(found);
    }
    let lookup_len = lookup.chars().count();
    let mut dist = dist.unwrap_or_else(|| cmp::max(lookup_len, 3) / 3);
    let mut best = None;
    // alloc: rustc's tie set for the substring score, on a suggestion's (error) path.
    let mut next_candidates: Vec<&'c T> = Vec::new();
    for &candidate in candidates {
        let found = match use_substring_score {
            true => edit_distance_with_substrings(lookup, name(candidate), dist),
            false => edit_distance(lookup, name(candidate), dist),
        };
        match found {
            Some(0) => return Some(candidate),
            Some(d) => {
                if use_substring_score {
                    if d < dist {
                        dist = d;
                        next_candidates.clear();
                    }
                    next_candidates.push(candidate);
                } else {
                    dist = d - 1;
                }
                best = Some(candidate);
            }
            None => {}
        }
    }
    if next_candidates.len() > 1 {
        best = find_best_match(false, &next_candidates, name, lookup, Some(lookup.len()));
    }
    if best.is_some() {
        return best;
    }
    let lookup_words = sort_by_words(lookup);
    candidates
        .iter()
        .fold(None, |found, &candidate| if sort_by_words(name(candidate)) == lookup_words { Some(candidate) } else { found })
}

/// `a.to_uppercase() == b.to_uppercase()` without the two strings: `str::to_uppercase` maps each
/// code point on its own, so comparing the mapped code points is the same test.
fn eq_uppercase(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    a.chars().flat_map(char::to_uppercase).eq(b.chars().flat_map(char::to_uppercase))
}

fn as_str<S: AsRef<str>>(it: &S) -> &str {
    it.as_ref()
}

fn sort_by_words(name: &str) -> Vec<&str> {
    // alloc: rustc's word sort, on a suggestion's (error) path.
    let mut words: Vec<&str> = name.split('_').collect();
    words.sort_unstable();
    words
}

/// cargo's "a ... with a similar name exists" suggestions (`src/util/edit_distance.rs` of cargo
/// 1.99.0, 5f94df4789): rustc's distance on lowercased strings, the nearest within 3.
pub mod cargo {
    /// cargo's `edit_distance`: [`super::edit_distance`] of `a` and `b` lowercased.
    pub fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
        if a.is_ascii() && b.is_ascii() && !a.bytes().chain(b.bytes()).any(|it| it.is_ascii_uppercase()) {
            return super::edit_distance(a, b, limit);
        }
        // alloc: cargo lowercases both sides, on an error message's path.
        super::edit_distance(&a.to_lowercase(), &b.to_lowercase(), limit)
    }

    /// cargo's `closest`: the element of `iter` nearest to `choice` within a distance of 3, the
    /// first of the nearest.
    pub fn closest<'a, T>(choice: &str, iter: impl Iterator<Item = T>, key: impl Fn(&T) -> &'a str) -> Option<T> {
        iter.filter_map(|e| Some((edit_distance(choice, key(&e), 3)?, e))).min_by_key(|t| t.0).map(|t| t.1)
    }

    /// cargo's `closest_msg`: `\n\nhelp: a {kind} with a similar name exists: `x`` or nothing.
    pub fn closest_msg<'a, T>(choice: &str, iter: impl Iterator<Item = T>, key: impl Fn(&T) -> &'a str, kind: &str) -> String {
        match closest(choice, iter, &key) {
            // alloc: cargo's error message, at the edge.
            Some(e) => format!("\n\nhelp: a {kind} with a similar name exists: `{}`", key(&e)),
            // alloc: empty, allocates nothing.
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests;
