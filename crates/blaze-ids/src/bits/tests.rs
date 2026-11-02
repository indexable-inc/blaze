use std::{collections::BTreeSet, sync::Arc, thread};

use hegel::TestCase;
use hegel::generators as gs;
use smallvec::SmallVec;
use thin_vec::ThinVec;

use crate::rng::SplitMix64;

use super::{
    AtomicBitSlice, AtomicBits, BitArray, BitMatrix, BitSet, BitSlice, Bits, Ordering, Ranked, rank_in, ranks_of,
};

/// The simplest copy `blaze_ids::bits` replaced (blaze-diagnostics' `lints/bodies/bits.rs`),
/// kept as the reference the differential tests hold the shared set to.
mod reference {
    #[derive(Default)]
    pub(super) struct Bits(Vec<u64>);

    impl Bits {
        pub(super) fn new(len: usize) -> Bits {
            Bits(vec![0; len.div_ceil(64)])
        }

        pub(super) fn insert(&mut self, at: usize) -> bool {
            let (word, bit) = (at / 64, 1u64 << (at % 64));
            let added = (self.0[word] & bit) == 0;
            self.0[word] |= bit;
            added
        }

        pub(super) fn remove(&mut self, at: usize) {
            self.0[at / 64] &= !(1u64 << (at % 64));
        }

        pub(super) fn contains(&self, at: usize) -> bool {
            (self.0[at / 64] & (1u64 << (at % 64))) != 0
        }
    }
}

#[test]
fn filled_holds_exactly_the_bits_below_its_size() {
    for size in [0, 1, 63, 64, 65, 127, 128, 200] {
        let set = BitSet::<usize>::filled(size);
        assert_eq!(set.iter().collect::<Vec<_>>(), (0..size).collect::<Vec<_>>(), "{size}");
        assert_eq!(set.words().len(), size.div_ceil(64));
        assert_eq!(set.count(), size);
        let inline = BitArray::<4>::all_below(size);
        assert_eq!(inline.iter().collect::<Vec<_>>(), (0..size).collect::<Vec<_>>(), "{size}");
    }
}

/// A set in a frame's `Vec` grows, resets and truncates like a [`BitSet`] given the same
/// members.
#[test]
fn a_set_in_an_allocators_vec_matches_a_bitset() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Intern).expect("reserve");
    let mut rng = SplitMix64::new(0x5851_f42d_4c95_7f2d);
    let mut held: Bits<Vec<u64, &blaze_mem::Frame>, u32> = Bits::from_words(Vec::new_in(&frame));
    let mut plain = BitSet::<u32>::default();
    for round in 0..64 {
        for _ in 0..200 {
            let at = rng.index(5000) as u32;
            assert_eq!(held.insert_growing(at), plain.insert_growing(at));
        }
        assert_eq!(held.words(), plain.words());
        match round % 3 {
            0 => {
                let len = rng.index(5000);
                held.truncate(len);
                plain.truncate(len);
            }
            1 => {
                held.reset(64 * (round + 1));
                plain.reset(64 * (round + 1));
            }
            _ => {}
        }
        assert_eq!(held.words(), plain.words());
        assert_eq!(held.iter().collect::<Vec<_>>(), plain.iter().collect::<Vec<_>>());
    }
}

#[test]
fn ranges_match_a_bit_at_a_time() {
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..2000 {
        let size = 1 + rng.index(300);
        let mut fast = BitSet::<usize>::new(size);
        let mut slow = vec![false; size];
        for _ in 0..8 {
            let start = rng.index(size + 1);
            let end = start + rng.index(size + 1 - start);
            let on = rng.coin();
            if on {
                fast.insert_range(start..end);
            } else {
                fast.remove_range(start..end);
            }
            slow[start..end].fill(on);
            let start = rng.index(size + 1);
            let end = start + rng.index(size + 1 - start);
            let want = (start..end).find(|&it| slow[it]);
            assert_eq!(fast.first_in(start..end), want);
            assert_eq!(fast.last_in(start..end), (start..end).rev().find(|&it| slow[it]));
        }
        let absent = (0..fast.capacity()).find(|&it| it >= size || !slow[it]);
        assert_eq!(fast.first_absent(), absent);
        let want: Vec<usize> = (0..size).filter(|&it| slow[it]).collect();
        assert_eq!(fast.iter().collect::<Vec<_>>(), want);
        assert_eq!(fast.iter().rev().collect::<Vec<_>>(), want.iter().rev().copied().collect::<Vec<_>>());
    }
}

#[test]
fn union_says_whether_it_added_a_bit() {
    let mut a = BitSet::<usize>::new(130);
    let mut b = BitSet::<usize>::new(130);
    b.insert(3);
    b.insert(129);
    assert!(a.union_with(&b));
    assert!(!a.union_with(&b));
    assert_eq!(a.iter().collect::<Vec<_>>(), vec![3, 129]);
    assert!(a.remove(3));
    assert!(!a.remove(3));
    let mut c = a.clone();
    c.assign_words(b.words());
    assert_eq!(c, b);
    assert!(c.subtract(&a) && !c.subtract(&a));
    assert_eq!(c.iter().collect::<Vec<_>>(), vec![3]);
    assert!(b.intersect_with(&c) && !b.intersect_with(&c));
    assert_eq!(b, c);
}

#[test]
#[should_panic(expected = "a union with a member past 64 bits")]
fn a_union_never_drops_a_member() {
    let mut small = BitSet::<usize>::new(64);
    let mut large = BitSet::<usize>::new(65);
    large.insert(64);
    small.union_with(&large);
}

#[test]
fn a_growing_union_takes_every_member() {
    let mut small = BitSet::<u32>::new(10);
    small.insert(1);
    let large: BitSet<u32> = [1, 64, 300].into_iter().collect();
    assert!(small.union_growing(&large));
    assert_eq!(small, large);
    let mut longer = BitSet::<u32>::new(600);
    longer.insert(5);
    assert!(longer.intersect_with(&large) && longer.is_empty(), "past `large`'s words is cleared");
}

#[test]
fn matrix_rows_are_independent() {
    let mut m = BitMatrix::<usize>::new(3, 70);
    m.insert(1, 69);
    m.insert(2, 0);
    assert_eq!(m.row_words(), 2);
    assert!(m.contains(1, 69) && !m.contains(0, 69));
    assert_eq!(m.iter_row(0).collect::<Vec<_>>(), Vec::<usize>::new());
    assert_eq!(m.iter_row(1).collect::<Vec<_>>(), vec![69]);
    assert!(m.union_rows(0, 2) && !m.union_rows(0, 2) && m.union_rows(2, 1));
    assert!(!m.union_rows(1, 1));
    assert_eq!(m.iter_row(0).collect::<Vec<_>>(), vec![0]);
    assert_eq!(m.iter_row(2).collect::<Vec<_>>(), vec![0, 69]);
    let mut other = BitSet::<usize>::new(70);
    other.insert(5);
    assert!(m.union_row_with(0, &other));
    assert_eq!(m.row(0).iter().collect::<Vec<_>>(), vec![0, 5]);
    m.reset(2, 10);
    assert_eq!((m.rows(), m.row_words(), m.words().len()), (2, 1, 2));
    assert!(m.row(1).is_empty());
}

/// A matrix over words held elsewhere (borrowck's frame slices) works as an owned one, in the
/// same words.
#[test]
fn borrowed_matrix_is_the_owned_one() {
    let mut owned = BitMatrix::<u32>::new(3, 70);
    let mut words = [0u64; 6];
    let mut borrowed = BitMatrix::<u32, &mut [u64]>::from_words(3, 70, &mut words[..]);
    for (row, at) in [(1, 69), (2, 0), (0, 64)] {
        assert_eq!(owned.insert(row, at), borrowed.insert(row, at));
    }
    assert_eq!(owned.union_rows(0, 2), borrowed.union_rows(0, 2));
    assert_eq!(owned.union_rows(2, 1), borrowed.union_rows(2, 1));
    assert!(borrowed.remove(0, 64) && owned.remove(0, 64));
    assert_eq!(owned.words(), borrowed.words());
    assert_eq!(borrowed.iter_row(2).collect::<Vec<_>>(), vec![0, 69]);
    assert_eq!(format!("{owned:?}"), format!("{borrowed:?}"));
    borrowed.clear();
    assert!(borrowed.row(2).is_empty());
    assert_eq!(BitMatrix::<u32, &[u64]>::from_words(3, 70, &words[..]).words(), &[0; 6]);
    assert_eq!(BitMatrix::<u32, &[u64]>::default().rows(), 0);
}

#[test]
#[should_panic(expected = "2 rows of 70 bits in 3 words")]
fn borrowed_matrix_needs_its_words_exactly() {
    let words = [0u64; 3];
    BitMatrix::<usize, &[u64]>::from_words(2, 70, &words[..]);
}

/// blaze-v2-proof's trace columns: bits on both sides of a word boundary.
#[test]
fn bits_cross_word_boundaries() {
    let mut bits = BitSet::<u32>::new(130);
    for i in [0, 63, 64, 129] {
        bits.insert(i);
    }
    assert_eq!(bits.words().len(), 3);
    assert!([0, 63, 64, 129].iter().all(|&i| bits.contains(i)));
    assert!(!bits.contains(1) && !bits.contains(65) && !bits.contains(128));
    assert_eq!(bits.count(), 4);
    assert!(!bits.contains(1 << 20), "past the capacity is not a member");
}

/// blaze-units' target features: 256 bits inline, set operations a word each.
#[test]
fn a_feature_set_is_its_bits() {
    let mut bits = BitArray::<4, u16>::EMPTY;
    for it in [0, 63, 64, 200, 255] {
        bits.insert(it);
    }
    assert_eq!(bits.iter().collect::<Vec<_>>(), [0, 63, 64, 200, 255]);
    assert_eq!(bits.count(), 5);
    bits.remove(64);
    assert!(!bits.contains(64) && bits.contains(63));
    let mut low = BitArray::<4, u16>::EMPTY;
    low.insert(0);
    assert!(low.is_subset(&bits) && !bits.is_subset(&low));
    bits.subtract(&low);
    assert_eq!(bits.and(&low), BitArray::<4, u16>::EMPTY);
    assert!(bits.is_disjoint(&low));
    const MADE: BitArray<2> = BitArray::<2>::EMPTY.with_bit(3).with_bit(100);
    assert!(MADE.has_bit(3) && MADE.has_bit(100) && !MADE.has_bit(4) && !MADE.has_bit(500));
    assert_eq!(MADE.or(&BitArray::EMPTY.with_bit(7)).iter().collect::<Vec<_>>(), [3, 7, 100]);
    assert_eq!(MADE.and_not(&BitArray::EMPTY.with_bit(3)).iter().collect::<Vec<_>>(), [100]);
}

/// blaze-usercode's queue: the first member found again and again as members come and go.
#[test]
fn bits_are_a_set() {
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    for len in [0usize, 1, 63, 64, 65, 200] {
        let mut bits = BitSet::<u32>::filled(len);
        let mut set: BTreeSet<u32> = (0..len as u32).collect();
        assert_eq!(bits.iter().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
        if len == 0 {
            assert_eq!(bits.first(), None);
            continue;
        }
        for _ in 0..2_000 {
            let at = rng.index(len) as u32;
            match rng.index(3) {
                0 => {
                    assert_eq!(bits.insert(at), set.insert(at));
                }
                1 => {
                    assert_eq!(bits.remove(at), set.remove(&at));
                }
                _ => {
                    let first = bits.first();
                    assert_eq!(first, set.first().copied());
                    assert_eq!(bits.last(), set.last().copied());
                    if let Some(first) = first {
                        bits.remove(first);
                        set.remove(&first);
                    }
                }
            }
            assert_eq!(bits.contains(at), set.contains(&at));
            assert_eq!(bits.count_below(at), set.range(..at).count());
        }
        assert_eq!(bits.iter().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
        bits.clear();
        assert_eq!((bits.first(), bits.iter().next()), (None, None));
    }
}

/// hir-def's lists of impls: a bit per position pushed beside the list, past its end clear.
#[test]
fn a_set_kept_beside_a_list_has_the_lists_words() {
    let mut beside = Bits::<ThinVec<u64>>::default();
    let mut other = Bits::<ThinVec<u64>>::default();
    for at in 0..65 {
        beside.set_growing(at, at % 2 == 0);
        other.set_growing(at, at % 2 == 0);
    }
    assert_eq!(beside.words().len(), 2);
    assert_eq!(beside, other);
    assert!(beside.contains(64) && !beside.contains(63) && !beside.contains(65));
}

/// hir-def's enum variants: inline up to 64, cut to the variants kept.
#[test]
fn a_small_set_is_inline_and_truncates() {
    let mut set = Bits::<SmallVec<[u64; 1]>>::new(130);
    set.insert(1);
    set.insert(70);
    set.insert(129);
    set.truncate(71);
    assert_eq!(set.words().len(), 2);
    assert_eq!(set.iter().collect::<Vec<_>>(), [1, 70]);
    set.truncate(64);
    set.shrink_to_fit();
    assert_eq!(set.iter().collect::<Vec<_>>(), [1]);
    assert!(!set.into_words().spilled());
}

/// A view of words held elsewhere reads as the set it is.
#[test]
fn a_slice_is_a_set() {
    let words = [0b1010u64, 0, 1];
    let view = BitSlice::<usize>::from_words(&words[..]);
    assert_eq!(view.iter().collect::<Vec<_>>(), [1, 3, 128]);
    let mut owned = BitSet::<usize>::new(192);
    owned.copy_from(&view);
    assert_eq!(owned.as_slice(), view);
    let mut words = [0u64; 2];
    let mut view = Bits::<&mut [u64]>::from_words(&mut words[..]);
    view.insert(65);
    assert_eq!(words, [0, 2]);
}

/// blaze-items' tables: a row's rank is how many rows before it are set.
#[test]
fn a_bitset_ranks_each_row() {
    let pattern: Vec<bool> = (0..200).map(|it| it % 3 == 0 || it == 130).collect();
    let bits = Ranked::from_bools(pattern.iter().copied());
    assert_eq!(bits.len(), 200);
    let mut set = 0;
    for (row, &bit) in pattern.iter().enumerate() {
        assert_eq!(bits.contains(row), bit, "row {row}");
        assert_eq!(bits.rank(row), set, "row {row}");
        assert_eq!(bits.place(row), bit.then_some(set), "row {row}");
        set += usize::from(bit);
    }
    assert_eq!(bits.count(), set);
    assert_eq!(bits.place(200), None);
    assert_eq!(Ranked::from_bools([]).count(), 0);
    let ranked: Vec<(usize, usize)> = bits.iter_ranked().collect();
    assert_eq!(ranked.len(), set);
    assert!(ranked.iter().enumerate().all(|(place, &(_, rank))| place == rank));
    assert_eq!(bits.iter_ranked().next_back(), ranked.last().copied());
    let ranks = ranks_of(bits.words()).unwrap();
    assert_eq!(&*ranks, bits.ranks());
    assert_eq!(rank_in(bits.words(), &ranks, 130), bits.rank(130));
}

#[test]
fn ranks_are_one_allocation() {
    let words = [u64::MAX, 0, 1 << 3, 5];
    let ranks = blaze_alloc_count::assert_at_most(1, "a ranked set's counts", || ranks_of(&words));
    assert_eq!(&*ranks.unwrap(), &[0, 64, 64, 65]);
}

#[test]
#[should_panic(expected = "with a bit past its end")]
fn a_ranked_set_has_no_bit_past_its_end() {
    let _ = Ranked::from_words(vec![1 << 10], 10);
}

/// blaze-cas' flags: set from many threads, every one lands.
#[test]
#[cfg(not(blaze_check))]
fn atomic_bits_set_from_many_threads_all_land() {
    let bits = Arc::new(AtomicBits::new(64 * 64));
    let threads: Vec<_> = (0..8usize)
        .map(|t| {
            let bits = Arc::clone(&bits);
            thread::spawn(move || {
                for at in (t..64 * 64).step_by(8) {
                    assert!(bits.insert_if_clear(at, Ordering::Relaxed));
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!((0..64 * 64).all(|at| bits.contains(at, Ordering::Relaxed)));
    assert!(!bits.contains(64 * 64, Ordering::Relaxed), "past the capacity is clear");
    assert!(!bits.insert(5, Ordering::Relaxed) && !bits.insert_if_clear(5, Ordering::Relaxed));
    assert!(bits.remove(5, Ordering::Relaxed) && !bits.contains(5, Ordering::Relaxed));
    let mut bits = Arc::into_inner(bits).unwrap();
    bits.clear();
    assert!(!bits.contains(0, Ordering::Relaxed));
}

/// blaze-cas' rotation: what was read becomes read before, over words held elsewhere.
#[test]
#[cfg(not(blaze_check))]
fn a_slice_moves_its_bits_into_another() {
    let (read, before) = (AtomicBits::new(200), AtomicBits::new(200));
    before.insert(7, Ordering::Relaxed);
    let view = read.as_slice();
    assert!(view.insert(3, Ordering::Relaxed) && view.insert_if_clear(150, Ordering::Relaxed));
    assert_eq!(view.capacity(), 256);
    view.move_into(before.as_slice(), Ordering::Relaxed);
    assert!(!read.contains(3, Ordering::Relaxed) && !read.contains(150, Ordering::Relaxed));
    assert!(before.contains(3, Ordering::Relaxed) && before.contains(150, Ordering::Relaxed));
    assert!(!before.contains(7, Ordering::Relaxed), "the period before is replaced, not added to");
    let held = AtomicBitSlice::from_words(read.words());
    assert!(held.insert(0, Ordering::Relaxed) && read.contains(0, Ordering::Relaxed));
}

/// Reads and in-place writes allocate nothing: a lookup, an insert into room it has, an
/// iteration, a union, a matrix row.
#[test]
#[cfg(not(blaze_check))]
fn reads_and_writes_allocate_nothing() {
    let mut a = BitSet::<u32>::new(1000);
    let mut b = BitSet::<u32>::new(1000);
    let mut m = BitMatrix::<u32>::new(4, 1000);
    let ranked = Ranked::from_bools((0..1000).map(|it| it % 7 == 0));
    let mut frame = [0u64; 32];
    blaze_alloc_count::assert_at_most(0, "bit set reads and writes", || {
        let mut held = BitMatrix::<u32, &mut [u64]>::from_words(2, 1000, &mut frame[..]);
        assert!(held.insert(1, 999) && held.union_rows(0, 1) && held.contains(0, 999));
        for at in (0..1000).step_by(3) {
            a.insert(at);
            b.insert_growing(at + 1);
            m.insert(at as usize % 4, at);
        }
        assert!(a.union_with(&b) && a.contains(1) && a.count() > 0);
        assert!(a.iter().sum::<u32>() > 0 && a.first_in(10..20).is_some());
        assert!(m.union_rows(0, 1) && m.iter_row(0).count() > 0);
        assert!(ranked.rank(999) > 0 && ranked.place(7) == Some(1));
        a.reset(500);
        a.clear();
    });
}

/// The shared set against the reference copy, op by op.
#[hegel::test]
fn the_set_is_the_reference(tc: TestCase) {
    let len: usize = tc.draw(gs::integers().min_value(1).max_value(399));
    let ops: Vec<(u8, usize)> = tc.draw(
        gs::vecs(gs::tuples!(gs::integers().max_value(3), gs::integers().max_value(len - 1))).max_size(199),
    );
    let mut set = BitSet::<usize>::new(len);
    let mut reference = reference::Bits::new(len);
    for (op, at) in ops {
        match op {
            0 | 1 => assert_eq!(set.insert(at), reference.insert(at)),
            2 => {
                let had = reference.contains(at);
                reference.remove(at);
                assert_eq!(set.remove(at), had);
            }
            _ => assert_eq!(set.contains(at), reference.contains(at)),
        }
    }
    let want: Vec<usize> = (0..len).filter(|&at| reference.contains(at)).collect();
    assert_eq!(set.iter().collect::<Vec<_>>(), want.clone());
    assert_eq!(set.iter().rev().collect::<Vec<_>>(), want.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(set.count(), want.len());
    assert_eq!(set.iter().count(), want.len());
    assert_eq!(set.is_empty(), want.is_empty());
}

/// Union, intersection and difference against the reference copy, a bit at a time, with
/// the changed flag a dataflow iterates on.
#[hegel::test]
fn set_operations_are_the_references(tc: TestCase) {
    let len: usize = tc.draw(gs::integers().min_value(1).max_value(299));
    let a: Vec<usize> = tc.draw(gs::vecs(gs::integers().max_value(len - 1)).max_size(99));
    let b: Vec<usize> = tc.draw(gs::vecs(gs::integers().max_value(len - 1)).max_size(99));
    let op: u8 = tc.draw(gs::integers().max_value(2));
    let (mut x, mut y) = (BitSet::<usize>::new(len), BitSet::<usize>::new(len));
    let (mut rx, mut ry) = (reference::Bits::new(len), reference::Bits::new(len));
    for &at in &a {
        x.insert(at);
        rx.insert(at);
    }
    for &at in &b {
        y.insert(at);
        ry.insert(at);
    }
    let mut changed = false;
    for at in 0..len {
        let (had, other) = (rx.contains(at), ry.contains(at));
        let now = match op {
            0 => had || other,
            1 => had && other,
            _ => had && !other,
        };
        changed |= now != had;
        if now { rx.insert(at); } else { rx.remove(at); }
    }
    let said = match op {
        0 => x.union_with(&y),
        1 => x.intersect_with(&y),
        _ => x.subtract(&y),
    };
    assert_eq!(said, changed);
    assert_eq!(x.iter().collect::<Vec<_>>(), (0..len).filter(|&at| rx.contains(at)).collect::<Vec<_>>());
    let subset = (0..len).all(|at| !rx.contains(at) || ry.contains(at));
    assert_eq!(x.is_subset(&y), subset);
}

/// Iteration from both ends at once meets in the middle with every member once.
#[hegel::test]
fn both_ends_meet(tc: TestCase) {
    let words: Vec<u64> = tc.draw(gs::vecs(gs::integers()).max_size(5));
    let backs: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).max_size(399));
    let set = BitSlice::<usize>::from_words(&words[..]);
    let mut it = set.iter();
    let (mut front, mut back) = (Vec::new(), Vec::new());
    for from_back in backs.iter().copied().chain(std::iter::repeat(false)) {
        let next = if from_back { it.next_back() } else { it.next() };
        match next {
            Some(at) if from_back => back.push(at),
            Some(at) => front.push(at),
            None => break,
        }
    }
    front.extend(back.into_iter().rev());
    let want: Vec<usize> = (0..words.len() * 64).filter(|&at| words[at / 64] >> (at % 64) & 1 == 1).collect();
    assert_eq!(front, want);
}

/// A splice of a range of one set into another at an offset, against a bit at a time.
#[hegel::test]
fn a_range_ors_in_at_an_offset(tc: TestCase) {
    let words: Vec<u64> = tc.draw(gs::vecs(gs::integers()).min_size(1).max_size(4));
    let into: Vec<u64> = tc.draw(gs::vecs(gs::integers()).min_size(1).max_size(4));
    let x: usize = tc.draw(gs::integers().max_value(words.len() * 64));
    let y: usize = tc.draw(gs::integers().max_value(words.len() * 64));
    let from = BitSlice::<usize>::from_words(&words[..]);
    let range = x.min(y)..x.max(y);
    let capacity = into.len() * 64;
    tc.assume(range.len() <= capacity);
    let at: usize = tc.draw(gs::integers().max_value(capacity - range.len()));
    let mut set = BitSet::<usize>::from_words(into.clone());
    set.or_range_at(at, &from, range.clone());
    let mut want = BitSet::<usize>::from_words(into);
    for (offset, bit) in range.enumerate() {
        if from.contains(bit) {
            want.insert(at + offset);
        }
    }
    assert_eq!(set, want);
}

/// A rank against a count of the reference's members below it.
#[hegel::test]
fn a_rank_counts_the_members_below(tc: TestCase) {
    let bools: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).min_size(1).max_size(499));
    let ranked = Ranked::from_bools(bools.iter().copied());
    let mut below = 0;
    for (at, &bit) in bools.iter().enumerate() {
        assert_eq!(ranked.rank(at), below);
        assert_eq!(ranked.contains(at), bit);
        below += usize::from(bit);
    }
    assert_eq!(ranked.count(), below);
}
