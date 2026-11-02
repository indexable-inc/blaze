//! The run over a stack of candidates with indexed branches against the run as it was before
//! (copied below: candidates removed from the front, branches found by a scan, the `Failure`
//! branch and the `Int` branches scanned by `sort`), on generated arms: integers, ranges that
//! overlap them and each other, `bool`s, constants compared with `==`, slice lengths with what
//! is inside them, or-patterns nested in all of these, and guards. Both must reach the same
//! arms and say the same about falling through.

use blaze_mem::{Frame, Tag, frame};

use super::*;

// The run as it was, over lists that lose their first candidate.

struct OldTree<'t, 'f> {
    frame: &'t Frame,
    thens: Vec<Vec<Pair<'t>, &'t Frame>, &'t Frame>,
    reached: Bits<'f>,
    escaped: bool,
}

impl<'t> OldTree<'t, '_> {
    fn run(&mut self, mut start: bool, mut candidates: Vec<Candidate<'t>, &'t Frame>) -> bool {
        loop {
            let Some(first) = candidates.first() else { return start };
            match first.pairs.first() {
                None => start = self.matched(start, candidates.remove(0)),
                Some(_) if candidates.iter().any(starts_with_or) => {
                    (start, candidates) = self.expand(start, candidates)
                }
                Some(_) => (start, candidates) = self.test(start, candidates),
            }
        }
    }

    fn matched(&mut self, start: bool, candidate: Candidate<'t>) -> bool {
        let Candidate { arm, guarded, then, .. } = candidate;
        let Some(then) = then else {
            if start {
                self.reached.insert(arm);
            }
            return start && guarded;
        };
        let pairs = self.thens[then].to_vec_in(self.frame);
        let mut rest = Vec::with_capacity_in(1, self.frame);
        rest.push(Candidate { arm, pairs, guarded, then: None });
        let fell = self.run(start, rest);
        if guarded {
            return fell;
        }
        self.escaped |= fell;
        false
    }

    fn expand(
        &mut self,
        start: bool,
        mut candidates: Vec<Candidate<'t>, &'t Frame>,
    ) -> (bool, Vec<Candidate<'t>, &'t Frame>) {
        let last = candidates.iter().position(|it| starts_with_or(it) && it.pairs.len() > 1);
        let remaining = candidates.split_off(last.map_or(candidates.len(), |it| it + 1));
        let mut expanded = Vec::with_capacity_in(candidates.len(), self.frame);
        for mut candidate in candidates {
            if !starts_with_or(&candidate) {
                expanded.push(candidate);
                continue;
            }
            let Ask::Or(alternatives) = candidate.pairs.remove(0).ask else { continue };
            let mut left = Vec::new_in(self.frame);
            left.append(&mut candidate.pairs);
            if let Some(after) = candidate.then {
                left.extend_from_slice(&self.thens[after]);
            }
            let then = (!left.is_empty()).then(|| {
                self.thens.push(left);
                self.thens.len() - 1
            });
            expanded.extend(alternatives.into_iter().map(|pairs| Candidate {
                arm: candidate.arm,
                pairs,
                guarded: candidate.guarded,
                then,
            }));
        }
        let outer = std::mem::take(&mut self.escaped);
        let fell = self.run(start, expanded);
        let escaped = std::mem::replace(&mut self.escaped, outer);
        (fell || escaped, remaining)
    }

    fn test(
        &mut self,
        start: bool,
        mut candidates: Vec<Candidate<'t>, &'t Frame>,
    ) -> (bool, Vec<Candidate<'t>, &'t Frame>) {
        let Some(branch) = own_branch(&candidates[0].pairs[0].ask) else {
            return (start, candidates);
        };
        let mut first = candidates.remove(0);
        let Pair { place, ask, inside } = first.pairs.remove(0);
        first.pairs.extend(inside);
        radix::sort_by_key(&mut first.pairs, or_last);
        let mut sorted = Vec::with_capacity_in(1, self.frame);
        sorted.push(first);
        let mut branches = Vec::new_in(self.frame);
        branches.push((branch, sorted));
        let mut remaining = Vec::new_in(self.frame);
        let mut candidates = candidates.into_iter();
        for mut candidate in candidates.by_ref() {
            let Some(branch) = old_sort(&place, &ask, &mut candidate, &branches) else {
                remaining.push(candidate);
                break;
            };
            match branches.iter_mut().find(|(it, _)| *it == branch) {
                Some((_, sorted)) => sorted.push(candidate),
                None => {
                    let mut sorted = Vec::with_capacity_in(1, self.frame);
                    sorted.push(candidate);
                    branches.push((branch, sorted));
                }
            }
        }
        remaining.extend(candidates);
        let has = |branch: Branch| branches.iter().any(|(it, _)| *it == branch);
        let mut fell = start
            && match ask {
                Ask::Bool(_) => !(has(Branch::Success) && has(Branch::Failure)),
                _ => !has(Branch::Failure),
            };
        for (_, sorted) in branches {
            fell |= self.run(start, sorted);
        }
        (fell, remaining)
    }
}

fn old_sort<'t>(
    place: &[Step],
    ask: &Ask<'t>,
    candidate: &mut Candidate<'t>,
    branches: &[(Branch, Vec<Candidate<'t>, &'t Frame>)],
) -> Option<Branch> {
    let at = candidate.pairs.iter().position(|it| it.place == place)?;
    let (branch, answered) = match (ask, &candidate.pairs[at].ask) {
        (Ask::Variant(_), &Ask::Variant(variant)) => (Branch::Variant(variant), true),
        (Ask::Int(_), &Ask::Int(value)) => {
            let covers = |it: &Pair| {
                it.place == place
                    && matches!(it.ask, Ask::Range(lo, hi) if lo <= value && value <= hi)
            };
            let failed = branches.iter().filter(|(it, _)| *it == Branch::Failure);
            if failed.flat_map(|(_, sorted)| sorted).any(|it| it.pairs.iter().any(covers)) {
                return None;
            }
            (Branch::Int(value), true)
        }
        (Ask::Int(_), &Ask::Range(lo, hi)) => {
            let named = |it: &(Branch, Vec<Candidate<'t>, &'t Frame>)| {
                matches!(it.0, Branch::Int(value) if lo <= value && value <= hi)
            };
            if branches.iter().any(named) {
                return None;
            }
            (Branch::Failure, false)
        }
        (Ask::Bool(_), &Ask::Bool(value)) => {
            (if value { Branch::Success } else { Branch::Failure }, true)
        }
        (Ask::Range(lo, hi), &Ask::Range(other_lo, other_hi)) => {
            if (lo, hi) == (&other_lo, &other_hi) {
                (Branch::Success, true)
            } else if other_hi < *lo || *hi < other_lo {
                (Branch::Failure, false)
            } else {
                return None;
            }
        }
        (Ask::Range(lo, hi), &Ask::Int(value)) if value < *lo || *hi < value => {
            (Branch::Failure, false)
        }
        (Ask::Equal(..), other @ Ask::Equal(..)) if ask == other => (Branch::Success, true),
        (&Ask::Len { len: tested, or_more: false }, &Ask::Len { len, or_more }) => {
            match (tested.cmp(&len), or_more) {
                (std::cmp::Ordering::Equal, false) => (Branch::Success, true),
                (std::cmp::Ordering::Less, _) | (std::cmp::Ordering::Greater, false) => {
                    (Branch::Failure, false)
                }
                _ => return None,
            }
        }
        (&Ask::Len { len: tested, or_more: true }, &Ask::Len { len, or_more }) => {
            match (tested.cmp(&len), or_more) {
                (std::cmp::Ordering::Equal, true) => (Branch::Success, true),
                (std::cmp::Ordering::Greater, true) => return None,
                _ => (Branch::Failure, false),
            }
        }
        _ => return None,
    };
    if answered {
        let pair = candidate.pairs.remove(at);
        candidate.pairs.extend(pair.inside);
        radix::sort_by_key(&mut candidate.pairs, or_last);
    }
    Some(branch)
}

// Generated arms.

/// xorshift64*: the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// What the value at a place is, which decides what a pattern there asks.
#[derive(Clone, Copy)]
enum Kind {
    Int,
    Bool,
    Equal,
    Slice,
}

struct Gen<'t> {
    frame: &'t Frame,
    literals: &'t [Literal],
    /// How wide the integers are: a narrow span makes values and ranges meet often.
    span: u64,
}

impl<'t> Gen<'t> {
    fn place(&self, base: &[Step], step: Step) -> Vec<Step, &'t Frame> {
        let mut place = Vec::with_capacity_in(base.len() + 1, self.frame);
        place.extend_from_slice(base);
        place.push(step);
        place
    }

    fn kind(field: u32) -> Kind {
        match field % 4 {
            0 => Kind::Int,
            1 => Kind::Bool,
            2 => Kind::Equal,
            _ => Kind::Slice,
        }
    }

    /// What a pattern asks of the value at `place`, of kind `kind`, into `pairs`; nothing for
    /// a wildcard.
    fn pattern(
        &self,
        rng: &mut Rng,
        place: Vec<Step, &'t Frame>,
        kind: Kind,
        depth: u32,
        pairs: &mut Vec<Pair<'t>, &'t Frame>,
    ) {
        let none = || Vec::new_in(self.frame);
        if rng.chance(20) {
            return;
        }
        if depth > 0 && rng.chance(15) {
            let count = 2 + rng.below(2) as usize;
            let mut each = Vec::with_capacity_in(count, self.frame);
            for _ in 0..count {
                let mut alternative = Vec::new_in(self.frame);
                self.pattern(rng, place.to_vec_in(self.frame), kind, depth - 1, &mut alternative);
                radix::sort_by_key(&mut alternative, or_last);
                each.push(alternative);
            }
            pairs.push(Pair { place, ask: Ask::Or(each), inside: none() });
            return;
        }
        let ask = match kind {
            Kind::Int => {
                let lo = rng.below(self.span) as u128;
                match rng.chance(35) {
                    true => Ask::Range(lo, lo + rng.below(self.span / 2 + 1) as u128 + 1),
                    false => Ask::Int(lo),
                }
            }
            Kind::Bool => Ask::Bool(rng.chance(50)),
            Kind::Equal => Ask::Equal(&self.literals[rng.below(self.literals.len() as u64) as usize], false),
            Kind::Slice => {
                let len = rng.below(3);
                let mut inside = Vec::new_in(self.frame);
                if depth > 0 {
                    for i in 0..len {
                        let element = self.place(&place, Step::Index(i));
                        self.pattern(rng, element, Kind::Int, depth - 1, &mut inside);
                    }
                }
                pairs.push(Pair { place, ask: Ask::Len { len, or_more: rng.chance(40) }, inside });
                return;
            }
        };
        pairs.push(Pair { place, ask, inside: none() });
    }

    /// An arm: a tuple of `fields` fields, each with a pattern or a wildcard.
    fn arm(&self, rng: &mut Rng, arm: usize, fields: u32) -> Candidate<'t> {
        let mut pairs = Vec::new_in(self.frame);
        for field in 0..fields {
            let place = self.place(&[], Step::Field(field));
            self.pattern(rng, place, Self::kind(field), 2, &mut pairs);
        }
        radix::sort_by_key(&mut pairs, or_last);
        Candidate { arm, pairs, guarded: rng.chance(15), then: None }
    }
}

fn reached<'f>(bits: &Bits<'f>, arms: usize) -> Vec<bool> {
    (0..arms).map(|it| bits.contains(it)).collect()
}

/// Runs both on the same arms; gives what the new run reached.
fn both<'t>(frame: &'t Frame, arms: Vec<Candidate<'t>, &'t Frame>) -> (Vec<bool>, bool) {
    let count = arms.len();
    let mut old_arms = Vec::with_capacity_in(count, frame);
    old_arms.extend(arms.iter().cloned());
    let mut old =
        OldTree { frame, thens: Vec::new_in(frame), reached: framed::bits_in(count, frame), escaped: false };
    let old_fell = old.run(true, old_arms);

    let mut new_arms = arms;
    new_arms.reverse();
    let mut new =
        Tree { frame, thens: Vec::new_in(frame), reached: framed::bits_in(count, frame), escaped: false };
    let new_fell = new.run(true, new_arms);

    let (old_reached, new_reached) = (reached(&old.reached, count), reached(&new.reached, count));
    assert_eq!(old_reached, new_reached, "the arms reached");
    assert_eq!(old_fell, new_fell, "whether no arm matching can happen");
    (new_reached, new_fell)
}

fn literals() -> &'static [Literal] {
    // alloc: test setup
    Box::leak(Box::new([Literal::Int(1, None), Literal::Int(2, None), Literal::Uint(7, None)]))
}

#[test]
fn the_stacked_run_reaches_what_the_old_run_reached() {
    let literals = literals();
    for seed in 1..=3000u64 {
        frame::task(Tag::Mir, |frame| {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let span = [3, 8, 40][rng.below(3) as usize];
            let generator = Gen { frame, literals, span };
            let fields = 1 + rng.below(3) as u32;
            let count = 1 + rng.below(if seed % 10 == 0 { 120 } else { 24 }) as usize;
            let mut arms = Vec::with_capacity_in(count, frame);
            for arm in 0..count {
                arms.push(generator.arm(&mut rng, arm, fields));
            }
            both(frame, arms);
        });
    }
}

/// A `match` on a byte with an arm per value and ranges between them, past `LINEAR` branches:
/// the index and the sorted ranges answer what the scans did.
#[test]
fn many_values_and_ranges_go_through_the_index() {
    frame::task(Tag::Mir, |frame| {
        let mut arms = Vec::with_capacity_in(300, frame);
        for value in 0..256u128 {
            let mut pairs = Vec::with_capacity_in(1, frame);
            let mut place = Vec::with_capacity_in(1, frame);
            place.push(Step::Field(0));
            let ask = match value % 16 {
                5 => Ask::Range(value, value + 20),
                9 => Ask::Range(value.saturating_sub(30), value),
                _ => Ask::Int(value),
            };
            pairs.push(Pair { place, ask, inside: Vec::new_in(frame) });
            arms.push(Candidate { arm: arms.len(), pairs, guarded: value % 7 == 0, then: None });
        }
        arms.push(Candidate { arm: arms.len(), pairs: Vec::new_in(frame), guarded: false, then: None });
        let (reached, _) = both(frame, arms);
        assert!(reached[0]);
    });
}

/// Arms that each compare with a constant: every test takes one candidate and leaves the rest
/// where they are, which the old run copied each time.
#[test]
fn constants_one_test_each() {
    let literals = literals();
    frame::task(Tag::Mir, |frame| {
        let mut arms = Vec::with_capacity_in(2001, frame);
        for arm in 0..2000 {
            let mut pairs = Vec::with_capacity_in(1, frame);
            let mut place = Vec::with_capacity_in(1, frame);
            place.push(Step::Field(0));
            pairs.push(Pair { place, ask: Ask::Equal(&literals[arm % 3], false), inside: Vec::new_in(frame) });
            arms.push(Candidate { arm, pairs, guarded: false, then: None });
        }
        arms.push(Candidate { arm: 2000, pairs: Vec::new_in(frame), guarded: false, then: None });
        let (reached, _) = both(frame, arms);
        assert!(reached[0]);
    });
}

#[test]
fn failed_ranges_merge() {
    frame::task(Tag::Mir, |frame| {
        let mut branches = Branches::new_in(frame);
        for (lo, hi) in [(10, 20), (30, 40), (15, 32), (50, 50), (0, 1)] {
            branches.fail_range(lo, hi);
        }
        assert_eq!(branches.failed[..], [(0, 1), (10, 40), (50, 50)]);
        for (value, covered) in [(0, true), (2, false), (10, true), (40, true), (41, false), (50, true), (51, false)] {
            assert_eq!(branches.failed_covers(value), covered, "{value}");
        }
    });
}
