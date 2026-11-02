use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// rustc's `edit_distance` and `find_best_match_for_name_impl` as `rustc_span` writes them:
/// the reference the shared implementation is checked against.
mod rustc {
    use std::{cmp, mem};

    pub(super) fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let (mut a, mut b) = (&a[..], &b[..]);
        if a.len() < b.len() {
            mem::swap(&mut a, &mut b);
        }
        let min_dist = a.len() - b.len();
        if min_dist > limit {
            return None;
        }
        while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_first().zip(a.split_first())
            && a_char == b_char
        {
            a = a_rest;
            b = b_rest;
        }
        while let Some(((b_char, b_rest), (a_char, a_rest))) = b.split_last().zip(a.split_last())
            && a_char == b_char
        {
            a = a_rest;
            b = b_rest;
        }
        if b.is_empty() {
            return Some(min_dist);
        }
        let mut prev_prev = vec![usize::MAX; b.len() + 1];
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut current = vec![0; b.len() + 1];
        for i in 1..=a.len() {
            current[0] = i;
            let a_idx = i - 1;
            for j in 1..=b.len() {
                let b_idx = j - 1;
                let substitution_cost = usize::from(a[a_idx] != b[b_idx]);
                current[j] = cmp::min(prev[j] + 1, cmp::min(current[j - 1] + 1, prev[j - 1] + substitution_cost));
                if i > 1 && j > 1 && a[a_idx] == b[b_idx - 1] && a[a_idx - 1] == b[b_idx] {
                    current[j] = cmp::min(current[j], prev_prev[j - 2] + 1);
                }
            }
            [prev_prev, prev, current] = [prev, current, prev_prev];
        }
        let distance = prev[b.len()];
        (distance <= limit).then_some(distance)
    }

    pub(super) fn edit_distance_with_substrings(a: &str, b: &str, limit: usize) -> Option<usize> {
        let n = a.chars().count();
        let m = b.chars().count();
        let big_len_diff = (n * 2) < m || (m * 2) < n;
        let len_diff = m.abs_diff(n);
        let distance = edit_distance(a, b, limit + len_diff)?;
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

    pub(super) fn find_best_match<'a>(
        use_substring_score: bool,
        candidates: &[&'a str],
        lookup: &str,
        dist: Option<usize>,
    ) -> Option<&'a str> {
        let lookup_uppercase = lookup.to_uppercase();
        if let Some(found) = candidates.iter().find(|it| it.to_uppercase() == lookup_uppercase) {
            return Some(*found);
        }
        let lookup_len = lookup.chars().count();
        let mut dist = dist.unwrap_or_else(|| cmp::max(lookup_len, 3) / 3);
        let mut best = None;
        let mut next_candidates = Vec::new();
        for candidate in candidates {
            let distance = match use_substring_score {
                true => edit_distance_with_substrings(lookup, candidate, dist),
                false => edit_distance(lookup, candidate, dist),
            };
            match distance {
                Some(0) => return Some(*candidate),
                Some(d) => {
                    if use_substring_score {
                        if d < dist {
                            dist = d;
                            next_candidates.clear();
                        }
                        next_candidates.push(*candidate);
                    } else {
                        dist = d - 1;
                    }
                    best = Some(*candidate);
                }
                None => {}
            }
        }
        if next_candidates.len() > 1 {
            best = find_best_match(false, &next_candidates, lookup, Some(lookup.len()));
        }
        if best.is_some() {
            return best;
        }
        let sorted = sort_by_words(lookup);
        candidates.iter().fold(None, |found, it| if sort_by_words(it) == sorted { Some(*it) } else { found })
    }

    fn sort_by_words(name: &str) -> Vec<&str> {
        let mut words: Vec<&str> = name.split('_').collect();
        words.sort_unstable();
        words
    }
}

fn best<'a>(candidates: &[&'a str], lookup: &str, dist: Option<usize>) -> Option<&'a str> {
    find_best_match_for_name(candidates, lookup, dist).copied()
}

fn best_with_substrings<'a>(candidates: &[&'a str], lookup: &str, dist: Option<usize>) -> Option<&'a str> {
    find_best_match_for_name_with_substrings(candidates, lookup, dist).copied()
}

// From `blaze-diagnostics/src/lints/edit_distance.rs`.
#[test]
fn distances_as_rustc_counts_them() {
    assert_eq!(edit_distance("kitten", "sitting", usize::MAX), Some(3));
    assert_eq!(edit_distance("ab", "ba", usize::MAX), Some(1));
    assert_eq!(edit_distance("abc", "xyz", 2), None);
    assert_eq!(edit_distance("", "abc", usize::MAX), Some(3));
}

// From `hir-ty-matchck/src/distance.rs`.
#[test]
fn distances_are_rustcs() {
    assert_eq!(edit_distance("FOO", "FOO", 1), Some(0));
    assert_eq!(edit_distance("FOO", "FO0", 1), Some(1));
    assert_eq!(edit_distance("ab", "ba", 1), Some(1));
    assert_eq!(edit_distance("kitten", "sitting", 3), Some(3));
    assert_eq!(edit_distance("kitten", "sitting", 2), None);
    assert_eq!(edit_distance("a", "abcde", 2), None);
}

// From `blaze-diagnostics/src/lints/edit_distance.rs`.
#[test]
fn the_name_rustc_suggests() {
    let names = ["unused_variables", "unused_imports", "dead_code"];
    assert_eq!(best(&names, "unused_variable", None), Some("unused_variables"));
    assert_eq!(best(&names, "DEAD_CODE", None), Some("dead_code"));
    assert_eq!(best(&names, "code_dead", None), Some("dead_code"));
    assert_eq!(best(&names, "nothing_like_it", None), None);
    let labels = ["'outer"];
    assert_eq!(best(&labels, "'outr", None), Some("'outer"));
}

// From `hir-ty-matchck/src/distance.rs`, over names given by a function.
#[test]
fn best_match_prefers_case_then_distance_then_words() {
    struct Named(&'static str);
    let candidates = [Named("MAGIC_TEST"), Named("FOO_BAR")];
    let pick = |s: &str| find_best_match_for_name_by(&candidates, |it| it.0, s, None).map(|it| it.0);
    assert_eq!(pick("magic_test"), Some("MAGIC_TEST"));
    assert_eq!(pick("MAGIC_TES"), Some("MAGIC_TEST"));
    assert_eq!(pick("BAR_FOO"), Some("FOO_BAR"));
    assert_eq!(pick("ZZZ"), None);
}

// From `blaze-diagnostics/src/lints/edit_distance.rs`.
#[test]
fn substrings_score_near() {
    assert_eq!(edit_distance_with_substrings("cap", "capture", usize::MAX), Some(4));
    let names = ["force_capture", "capture"];
    assert_eq!(best_with_substrings(&names, "forced_capture", None), Some("force_capture"));
}

#[test]
fn owned_candidates() {
    let names = vec!["alpha".to_owned(), "beta".to_owned()];
    assert_eq!(find_best_match_for_name(&names, "alpah", None).map(String::as_str), Some("alpha"));
    let lookups = ["xx::betta", "betta"];
    assert_eq!(find_best_match_for_names(&names, &lookups, None).map(String::as_str), Some("beta"));
}

// From cargo's `test_edit_distance` (`blaze-resolve/src/edit_distance.rs`).
#[test]
fn cargo_distances() {
    let a = "\nMäry häd ä little lämb\n\nLittle lämb\n";
    let b = "\nMary häd ä little lämb\n\nLittle lämb\n";
    let c = "Mary häd ä little lämb\n\nLittle lämb\n";
    assert_eq!(cargo::edit_distance(a, b, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance(b, a, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance(a, c, usize::MAX), Some(2));
    assert_eq!(cargo::edit_distance(c, a, usize::MAX), Some(2));
    assert_eq!(cargo::edit_distance(b, c, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance(c, b, usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance("ab", "ba", usize::MAX), Some(1));
    assert_eq!(cargo::edit_distance("CHECK", "check", 0), Some(0));
    assert_eq!(cargo::edit_distance("abcdefgh", "x", 3), None);
}

// From `blaze-resolve/src/edit_distance.rs`.
#[test]
fn cargo_suggestion() {
    let names = ["default", "std", "serde"];
    assert_eq!(
        cargo::closest_msg("serd", names.iter(), |n| n, "feature"),
        "\n\nhelp: a feature with a similar name exists: `serde`"
    );
    assert_eq!(cargo::closest_msg("tokio-full", names.iter(), |n| n, "feature"), "");
}

/// Names from a small alphabet (so that near-misses, transpositions and `_`-separated word
/// reorderings are common), with upper case and a non-ASCII letter to reach both paths.
fn name() -> gs::TextGenerator {
    gs::text().alphabet("abAB_éΣ").max_size(9)
}

#[hegel::test]
fn distance_is_rustcs(tc: TestCase) {
    let a = tc.draw(name());
    let b = tc.draw(name());
    let limit = tc.draw(hegel::one_of!(gs::integers::<usize>().max_value(5), gs::just(usize::MAX)));
    assert_eq!(edit_distance(&a, &b, limit), rustc::edit_distance(&a, &b, limit));
}

#[hegel::test]
fn substring_score_is_rustcs(tc: TestCase) {
    let a = tc.draw(name());
    let b = tc.draw(name());
    let limit = tc.draw(gs::integers::<usize>().max_value(5));
    assert_eq!(edit_distance_with_substrings(&a, &b, limit), rustc::edit_distance_with_substrings(&a, &b, limit));
}

#[hegel::test]
fn best_match_is_rustcs(tc: TestCase) {
    let candidates = tc.draw(gs::vecs(name()).max_size(7));
    let lookup = tc.draw(name());
    let dist = tc.draw(gs::optional(gs::integers::<usize>().max_value(3)));
    let use_substring_score = tc.draw(gs::booleans());
    let candidates: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let ours = match use_substring_score {
        true => best_with_substrings(&candidates, &lookup, dist),
        false => best(&candidates, &lookup, dist),
    };
    assert_eq!(ours, rustc::find_best_match(use_substring_score, &candidates, &lookup, dist));
}

#[hegel::test]
fn cargo_distance_is_rustcs_lowercased(tc: TestCase) {
    let a = tc.draw(name());
    let b = tc.draw(name());
    let limit = tc.draw(gs::integers::<usize>().max_value(5));
    assert_eq!(
        cargo::edit_distance(&a, &b, limit),
        rustc::edit_distance(&a.to_lowercase(), &b.to_lowercase(), limit)
    );
}
