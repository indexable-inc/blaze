//! `StaticSearch` against `partition_point` on the sorted keys.

use std::{collections::BTreeSet, format, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape};

use super::{counted, ensure, no_leaks, planted, under_every_failure};
use crate::{
    StaticSearch,
    plant::Plant,
    search::{SearchError, SearchKey},
};

fn case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        // Up to three levels (16, 288, 4,912 keys fill one, two, three).
        let n = blaze_int::idx(blaze_int::Int::truncate_to::<u32>(tape.below(6000, "n")));
        let spread = 1 + tape.below(8, "spread");
        let mut keys = BTreeSet::new();
        for _ in 0..n {
            keys.insert(blaze_int::Int::truncate_to::<u32>(tape.below(u64::try_from(n).unwrap_or(1) * spread + 1, "key")));
        }
        if tape.one_in(4, "max") {
            keys.insert(<u32 as SearchKey>::MAX);
        }
        let keys: StdVec<u32> = keys.into_iter().collect();
        let set = StaticSearch::try_new_in(&keys, &alloc).map_err(|e| Broke::Harness(format!("{e}")))?;
        ensure(set.len() == keys.len(), || format!("len differs"))?;
        let mut probes = StdVec::new();
        for _ in 0..200 {
            probes.push(match tape.below(3, "probe") {
                0 if !keys.is_empty() => keys[tape.index(keys.len(), "at")],
                1 => u32::MAX - blaze_int::Int::truncate_to::<u32>(tape.below(2, "top")),
                _ => tape.next_u32("x") >> tape.below(32, "scale"),
            });
        }
        let (eights, _) = probes.as_chunks::<8>();
        for xs in eights {
            let want: [usize; 8] = xs.map(|x| keys.partition_point(|&k| k < x));
            let got = set.lower_bounds(xs);
            ensure(got == want, || format!("the lower bounds of {xs:?} are {got:?}, the sorted keys' {want:?} of {}", keys.len()))?;
        }
        for &x in &probes {
            let want = keys.partition_point(|&k| k < x);
            let got = set.lower_bound(x);
            ensure(got == want, || format!("the lower bound of {x} is {got}, the sorted keys' {want} of {}", keys.len()))?;
            let rank = keys.binary_search(&x).ok();
            ensure(set.contains(x) == rank.is_some(), || format!("contains {x} said wrong"))?;
            ensure(set.rank(x) == rank, || format!("rank of {x} said wrong"))?;
        }
    }
    no_leaks(&alloc)
}

/// The refinement: `StaticSearch` returns the exact index `partition_point` returns on every sorted key slice and probe.
#[test]
fn static_search_matches_partition_point() {
    blaze_sim::check(Config::cases(200), case);
}

#[test]
fn the_oracle_catches_a_descent_one_child_left() {
    blaze_sim::control(Config::cases(200), Plant::Clean, Plant::SearchChildLeft, "the lower bound", |tape, p| {
        planted(p, tape, case)
    });
}

#[test]
fn unsorted_keys_are_refused_by_place() {
    let set = StaticSearch::<u32, _>::try_new_in(&[1, 5, 5, 9], &std::alloc::System);
    assert!(matches!(set, Err(SearchError::Unsorted { at: 2 })));
    let set = StaticSearch::<u32, _>::try_new_in(&[3, 2], &std::alloc::System);
    assert!(matches!(set, Err(SearchError::Unsorted { at: 1 })));
}

#[test]
fn static_search_try_new_fails_cleanly_at_every_allocation() -> Result<(), Broke> {
    let keys: StdVec<u64> = (0..1000).map(|k| k * 3).collect();
    under_every_failure("StaticSearch", |alloc| {
        let set = StaticSearch::try_new_in(&keys, alloc).map_err(|e| match e {
            SearchError::Alloc(failure) => super::Why::Refused(failure),
            other => super::Why::Changed(format!("{other}")),
        })?;
        if set.lower_bound(4) != 2 {
            return Err(super::Why::Changed(format!("a wrong lower bound")));
        }
        Ok(())
    })?;
    Ok(())
}
