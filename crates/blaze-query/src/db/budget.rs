//! Values kept by their bytes. Each `lru` function weighs its values (`weigh = ..`: the heap
//! bytes dropping one frees), the storage sums them by memory tag (the tag the function runs
//! as, `mem_tag`), and at a view boundary where a tag's values weigh more than its budget, a
//! pass lets go of the least recently verified until they weigh at most [`low_water`].
//!
//! The budget is the tag's (`blaze_mem::tag::budget`: the daemon sets each tag's share of the
//! machine's memory, `blaze_graph::admission::set_budgets`), or what the storage was given for
//! it (`Storage::set_value_budget`); a tag with neither keeps every value.
//!
//! Recency is the row's own stamp (the view it was last verified in), so nothing is counted on
//! a hit or a verify and no view walks what it holds: a pass reads the values once, which a
//! pass's eighth of the budget pays for. Recompute cost does not weigh in: the self-build's
//! values all fit under the budget (docs/v2/plan.md, "LRU and eviction").
//!
//! Eviction only costs time: an evicted value runs again when asked for, its stamp and hash
//! kept, so no policy here can change an answer.

#![blaze::sans_io]

/// A pass lets the values go down to an eighth under the budget, so the next pass comes after
/// at least an eighth of the budget of new values: a pass reads every value held, and this is
/// what keeps that to a small part of what making the values cost.
pub(crate) const SPARE: u64 = 8;

/// What the values of a tag may weigh after a pass over `budget`.
pub(crate) const fn low_water(budget: u64) -> u64 {
    budget - budget / SPARE
}

/// One value a pass sees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Held {
    /// The view it was last verified in.
    pub(crate) view: u32,
    /// Its function's ingredient and its key's raw ID: unique together, so the order is total.
    pub(crate) ingredient: u32,
    pub(crate) key: u32,
    /// What it weighs.
    pub(crate) bytes: u64,
}

/// Orders `values` (weighing `weight` together) so the ones to let go come first, and returns
/// how many: none when `weight` is within `budget`, else the least recently verified (of one
/// view, by ingredient and key) until the rest weigh at most [`low_water`]. The result depends
/// only on the set of values, never on their order coming in.
pub(crate) fn plan(values: &mut [Held], weight: u64, budget: u64) -> usize {
    values.sort_unstable_by_key(|it| (it.view, it.ingredient, it.key));
    if weight <= budget {
        return 0;
    }
    let target = low_water(budget);
    let mut left = weight;
    let mut gone = 0;
    for value in &*values {
        if left <= target {
            break;
        }
        left = left.saturating_sub(value.bytes);
        gone += 1;
    }
    gone
}

#[cfg(test)]
mod tests {
    use hegel::{TestCase, generators as gs};

    use super::*;

    fn values(tc: &TestCase) -> Vec<Held> {
        let len = tc.draw(gs::integers::<u32>().max_value(200));
        (0..len)
            .map(|key| Held {
                view: tc.draw(gs::integers::<u32>().min_value(1).max_value(9)),
                ingredient: tc.draw(gs::integers::<u32>().max_value(2)),
                key,
                bytes: tc.draw(gs::integers::<u64>().max_value(5000)),
            })
            .collect() // alloc: a test's values
    }

    #[hegel::test]
    fn a_pass_brings_the_values_under_the_budget_and_lets_go_of_the_oldest(tc: TestCase) {
        let mut values = values(&tc);
        let weight: u64 = values.iter().map(|it| it.bytes).sum();
        let budget = tc.draw(gs::integers::<u64>().max_value(weight.max(1) * 2));
        let gone = plan(&mut values, weight, budget);
        let (evicted, kept) = values.split_at(gone);
        let left: u64 = kept.iter().map(|it| it.bytes).sum();
        if weight <= budget {
            assert_eq!(gone, 0, "within the budget nothing goes");
            return;
        }
        assert!(left <= low_water(budget), "{left} left over the low water of {budget}");
        if let Some(last) = evicted.last() {
            assert!(left + last.bytes > low_water(budget), "one value too many let go");
        }
        for gone in evicted {
            for stays in kept {
                assert!(gone.view <= stays.view, "{gone:?} let go before the older {stays:?}");
            }
        }
    }

    #[hegel::test]
    fn a_plan_depends_only_on_the_set_of_values(tc: TestCase) {
        let mut values = values(&tc);
        let weight: u64 = values.iter().map(|it| it.bytes).sum();
        let budget = tc.draw(gs::integers::<u64>().max_value(weight.max(1)));
        let mut reversed = values.clone();
        reversed.reverse();
        assert_eq!(plan(&mut values, weight, budget), plan(&mut reversed, weight, budget));
        assert_eq!(values, reversed, "the same values go, in the same order");
    }

    #[test]
    fn a_value_of_the_newest_view_outlasts_every_older_one() {
        let held = |view, key| Held { view, ingredient: 0, key, bytes: 10 };
        let mut values = vec![held(3, 0)]; // alloc: a test's values
        values.extend((1..10).map(|key| held(1 + key % 2, key)));
        let gone = plan(&mut values, 100, 40);
        assert_eq!(gone, 7, "100 bytes down to the low water of 35: seven of ten go");
        assert!(values[gone..].iter().any(|it| it.key == 0), "the hot value stays");
    }
}
