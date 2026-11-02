use std::collections::BTreeMap;

use blaze_ids::rng::SplitMix64 as Rng;

use super::*;
use crate::table::{
    Set,
    tests::{bound, id},
};

#[test]
fn postings_hold_every_set_in_order_across_blocks() {
    let mut blocks = Vec::new();
    let mut chains = [EMPTY; 3];
    let mut want: [Vec<u32>; 3] = Default::default();
    let mut rng = Rng::new(1);
    for set in 0..200u32 {
        let at = rng.index(3);
        post(&mut blocks, &mut chains[at], set);
        want[at].push(set);
    }
    for (chain, want) in chains.iter().zip(&want) {
        let mut seen = Vec::new();
        walk(&blocks, *chain, |set| seen.push(set));
        assert_eq!(&seen, want);
    }
    let mut seen = Vec::new();
    walk(&blocks, EMPTY, |set| seen.push(set));
    assert!(seen.is_empty());
}

#[test]
fn places_find_what_a_map_finds() {
    let mut places = Places::default();
    let mut keys: Vec<u64> = Vec::new();
    let mut map = BTreeMap::new();
    let mut rng = Rng::new(2);
    for _ in 0..5_000 {
        // Few distinct hashes, so probes run long and collide.
        let key = rng.below(3_000).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let found = places.find(key, |id| keys[id as usize] == key);
        assert_eq!(found, map.get(&key).copied());
        if found.is_none() {
            let id = keys.len() as u32;
            keys.push(key);
            let held = &keys;
            places.insert(key, id, |it| held[it as usize]);
            map.insert(key, id);
        }
    }
    assert!(places.len * 2 <= places.places.len());
}

fn image_of(versions: &[u32], beside: u32) -> Image {
    let fns = versions.iter().enumerate().map(|(n, &v)| Version { id: id(n as u32), bound: bound(n as u32, v) }).collect();
    let slots = (0..versions.len() as u32).collect();
    Image::new(fns, slots, ContentId::of("blaze v2 testsel beside 1", &beside.to_le_bytes())).unwrap()
}

fn given() -> ContentId {
    ContentId::of("blaze v2 testsel given 1", b"selector")
}

/// A set's fit, from counts, is `Image::changed`'s answer over random sets and edits, reverts
/// and functions that go and come back.
#[test]
fn a_fit_is_what_the_binary_says_of_the_set() {
    const FNS: usize = 40;
    let mut rng = Rng::new(9);
    let mut versions = vec![0u32; FNS];
    let mut selector = Selector::new(given(), image_of(&versions, 0));
    let mut sets: Vec<(u32, Vec<u8>)> = Vec::new();
    for round in 0..300 {
        if rng.below(2) == 0 {
            let fns: Vec<Version> = (0..1 + rng.below(12))
                .map(|_| {
                    let n = rng.below(FNS as u64) as u32;
                    // Sometimes a version the binary never had, or not yet.
                    let v = versions[n as usize] + u32::from(rng.below(5) == 0);
                    Version { id: id(n), bound: bound(n, v) }
                })
                .collect();
            let set = Set::of(selector.image().unlogged(), fns);
            let bytes = set.encode();
            let key = Set::key_of(&bytes);
            if selector.known(&key).is_none() {
                let view = SetView::decode(&bytes).unwrap();
                let id = selector.register(key, view.unlogged(), view.versions());
                sets.push((id, bytes));
            }
        }
        let n = rng.index(FNS);
        match rng.below(4) {
            0 => versions[n] = versions[n].saturating_sub(1),
            _ => versions[n] += 1,
        }
        let mut image = image_of(&versions, u32::from(round % 50 == 49));
        if rng.below(10) == 0 {
            // A binary without function n.
            let fns = (0..FNS as u32).filter(|&it| it != n as u32).map(|it| Version { id: id(it), bound: bound(it, versions[it as usize]) });
            image = Image::new(fns.collect(), Vec::new(), ContentId::of("blaze v2 testsel beside 1", &0u32.to_le_bytes())).unwrap();
        }
        selector.advance(image);
        for (id, bytes) in &sets {
            let view = SetView::decode(bytes).unwrap();
            assert_eq!(selector.fits(*id), selector.image().changed(&view).is_none(), "round {round}, set {id}");
        }
    }
    assert!(sets.len() > 50);
}

/// The edit-proportional part allocates nothing: advancing to a binary, changed or not.
#[test]
fn advancing_allocates_nothing() {
    let mut versions = vec![0u32; 64];
    let mut selector = Selector::new(given(), image_of(&versions, 0));
    for s in 0..32u32 {
        let fns = (0..8).map(|k| Version { id: id((s + k) % 64), bound: bound((s + k) % 64, 0) }).collect();
        let set = Set::of(selector.image().unlogged(), fns);
        let bytes = set.encode();
        let view = SetView::decode(&bytes).unwrap();
        selector.register(Set::key_of(&bytes), view.unlogged(), view.versions());
    }
    let same = image_of(&versions, 0);
    blaze_alloc_count::assert_at_most(0, "advancing to an equal binary", || selector.advance(same));
    versions[3] = 1;
    versions[40] = 2;
    let edited = image_of(&versions, 0);
    blaze_alloc_count::assert_at_most(0, "advancing past an edit", || selector.advance(edited));
    assert!(!selector.fits(0) && selector.fits(4));
    versions[3] = 0;
    versions[40] = 0;
    let back = image_of(&versions, 0);
    blaze_alloc_count::assert_at_most(0, "advancing past a revert", || selector.advance(back));
    assert!((0..32).all(|set| selector.fits(set)));
}
