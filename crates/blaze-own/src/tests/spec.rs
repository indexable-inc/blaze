//! The fast paths of `spec` against the generic ones, on random input: each operation run once on
//! values the fast path takes (`u8`, `i8`, `bool`, `u32`, an iterator that is `TrustedLen`) and
//! once on the same values behind a wrapper that hides the marker ([`Plain`], [`Hidden`]), which
//! goes the generic way; the two must give equal results, and the allocator must see every block
//! freed. Then the panic-safety of the paths that write past a length (`insert_many`, the
//! `TrustedLen` extend), and the one allocation of `collect_in` from an exact iterator.

use super::*;
use crate::{CloneIn, CollectIn, Soa, error::AllocFailure};

/// A value as it is, but not `TrivialClone` (no `Copy`), so every path takes it the generic way.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Plain<T>(T);

impl<T: Clone, B: Allocator> CloneIn<B> for Plain<T> {
    type In = Plain<T>;

    fn try_clone_in(&self, _alloc: B) -> Result<Plain<T>, AllocFailure> {
        Ok(self.clone())
    }
}

/// An iterator as it is, but not `TrustedLen`, so `extend` and `collect_in` take it the generic
/// way.
struct Hidden<I>(I);

impl<I: Iterator> Iterator for Hidden<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

fn values(tape: &Tape, most: usize) -> StdVec<u32> {
    (0..tape.index(most, "len")).map(|_| tape.below(8, "value") as u32).collect()
}

fn plain(values: &[u32]) -> StdVec<Plain<u32>> {
    values.iter().copied().map(Plain).collect()
}

fn equal<T: PartialEq + core::fmt::Debug>(what: &str, fast: &[T], generic: &[T]) -> Result<(), Broke> {
    same(what, fast.iter(), generic.iter())
}

fn unwrap(values: &[Plain<u32>]) -> StdVec<u32> {
    values.iter().map(|p| p.0).collect()
}

/// A ring holding `values`, its head moved `turn` slots in so the values may wrap.
fn ring<'a, T: Clone>(values: &[T], turn: usize, filler: T, alloc: &'a Counting) -> VecDeque<T, &'a Counting> {
    let mut ring = VecDeque::with_capacity_in(values.len() + turn + 1, alloc);
    for _ in 0..turn {
        ring.push_back(filler.clone());
    }
    for value in values {
        ring.push_back(value.clone());
        if ring.len() > values.len() {
            let _filler = ring.pop_front();
        }
    }
    while ring.len() > values.len() {
        let _filler = ring.pop_front();
    }
    ring
}

fn specialized_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    specialized_case_in(tape, &alloc)?;
    alloc.check()
}

fn specialized_case_in(tape: &Tape, alloc: &Counting) -> Result<(), Broke> {
    // The two sides take different impls, or the comparison says nothing.
    use crate::spec::CloneSlice;
    if !<u32 as CloneSlice>::trivial() || <Plain<u32> as CloneSlice>::trivial() {
        return Err(Broke::Harness(String::from("u32 is not on the block copy, or Plain is")));
    }
    let head = values(tape, 20);
    let rest = values(tape, 40);
    let (plain_head, plain_rest) = (plain(&head), plain(&rest));

    // `extend_from_slice`, `from_slice_in`.
    let mut fast = Vec::from_slice_in(&head, alloc);
    let mut generic = Vec::from_slice_in(&plain_head, alloc);
    equal("from_slice_in", &fast, &Vec::from_slice_in(&plain_head, alloc).iter().map(|p| p.0).collect::<StdVec<_>>())?;
    fast.extend_from_slice(&rest);
    generic.extend_from_slice(&plain_rest);
    equal("extend_from_slice", &fast, &unwrap(&generic))?;

    // `insert_many`, anywhere up to the end.
    let at = tape.index(fast.len() + 1, "at");
    fast.insert_slice_shift(at, &head);
    generic.insert_slice_shift(at, &plain_head);
    equal("insert_many", &fast, &unwrap(&generic))?;

    // `clone`, `clone_in`.
    equal("clone", &fast.clone(), &unwrap(&generic.clone()))?;
    equal("clone_in", &fast.clone_in(alloc), &unwrap(&generic.clone_in(alloc)))?;
    equal("[T]::clone_in", &fast.as_slice().clone_in(alloc), &unwrap(&generic.as_slice().clone_in(alloc)))?;

    // `extend` from another vector's `into_iter`, partly taken first so it starts inside.
    let taken = tape.index(rest.len() + 1, "taken");
    let mut source = Vec::from_slice_in(&rest, alloc).into_iter();
    let mut hidden = Hidden(Vec::from_slice_in(&rest, alloc).into_iter());
    for _ in 0..taken {
        let _taken = (source.next(), hidden.next());
    }
    let (mut by_block, mut by_value) = (fast.clone(), fast.clone());
    by_block.extend(source);
    by_value.extend(hidden);
    equal("extend(into_iter)", &by_block, &by_value)?;

    // `extend` from `&[T]`'s iterator.
    let (mut by_block, mut by_value) = (fast.clone(), fast.clone());
    by_block.extend(rest.iter());
    by_value.extend(Hidden(rest.iter()));
    equal("extend(&[T])", &by_block, &by_value)?;

    // `collect_in`.
    let n = tape.index(50, "n");
    let scale = tape.below(7, "scale") as u32;
    let trusted = (0..n as u32).map(|i| i * scale).collect_in(alloc);
    let counted = (0..n as u32).map(|i| i * scale).collect::<StdVec<_>>();
    let untrusted = Hidden((0..n as u32).map(|i| i * scale)).collect_in(alloc);
    equal("collect_in", &trusted, &untrusted)?;
    equal("collect_in, the model", &trusted, &counted)?;

    // `contains`, `position`, over bytes, signed bytes and bools.
    let bytes: StdVec<u8> = (0..tape.index(80, "bytes")).map(|_| tape.below(6, "byte") as u8).collect();
    let needle = tape.below(7, "needle") as u8;
    let (fast_bytes, plain_bytes) = (Vec::from_slice_in(&bytes, alloc), Vec::from_iter_in(bytes.iter().copied().map(Plain), alloc));
    let signed: StdVec<i8> = bytes.iter().map(|b| (*b as i8) - 3).collect();
    let fast_signed = Vec::from_slice_in(&signed, alloc);
    let plain_signed = Vec::from_iter_in(signed.iter().copied().map(Plain), alloc);
    let flags: StdVec<bool> = bytes.iter().map(|b| b % 2 == 0).collect();
    let fast_flags = Vec::from_slice_in(&flags, alloc);
    let plain_flags = Vec::from_iter_in(flags.iter().copied().map(Plain), alloc);
    let found = [
        (fast_bytes.scan_for(&needle), plain_bytes.scan_for(&Plain(needle))),
        (fast_signed.scan_for(&((needle as i8) - 3)), plain_signed.scan_for(&Plain((needle as i8) - 3))),
        (fast_flags.scan_for(&(needle % 2 == 0)), plain_flags.scan_for(&Plain(needle % 2 == 0))),
    ];
    for (k, (fast, generic)) in found.iter().enumerate() {
        if fast != generic {
            return Err(Broke::Oracle(format!("position over kind {k}: {fast:?}, generic {generic:?}")));
        }
    }
    if fast_bytes.scan_for(&needle).is_some() != bytes.contains(&needle) {
        return Err(Broke::Oracle(format!("contains({needle}) differs from the model")));
    }

    // `VecDeque` append, `clone`, `clone_in`, from rings that wrap.
    let (turn, other_turn) = (tape.index(30, "turn"), tape.index(30, "other turn"));
    let mut fast_ring = ring(&head, turn, 0, alloc);
    let mut fast_other = ring(&rest, other_turn, 0, alloc);
    let mut plain_ring = ring(&plain_head, turn, Plain(0), alloc);
    let mut plain_other = ring(&plain_rest, other_turn, Plain(0), alloc);
    fast_ring.append(&mut fast_other);
    plain_ring.append(&mut plain_other);
    let unring = |r: &VecDeque<Plain<u32>, &Counting>| r.iter().map(|p| p.0).collect::<StdVec<_>>();
    same("append", fast_ring.iter().copied(), unring(&plain_ring).into_iter())?;
    if !fast_other.is_empty() {
        return Err(Broke::Oracle(String::from("append left values in the other ring")));
    }
    same("ring clone", fast_ring.clone().iter().copied(), unring(&plain_ring.clone()).into_iter())?;
    same("ring clone_in", fast_ring.clone_in(alloc).iter().copied(), unring(&plain_ring.clone_in(alloc)).into_iter())?;

    // `Soa` clone, `clone_in`: one column `u8`, one `u32`; the plain one hides one column.
    let mut fast_rows: Soa<(u8, u32), &Counting> = Soa::new_in(alloc);
    let mut plain_rows: Soa<(Plain<u8>, u32), &Counting> = Soa::new_in(alloc);
    for (k, value) in rest.iter().enumerate() {
        fast_rows.push((k as u8, *value));
        plain_rows.push((Plain(k as u8), *value));
    }
    let fast_copy = fast_rows.clone();
    let plain_copy = plain_rows.clone_in(alloc);
    same("soa clone", fast_copy.iter().map(|(a, b)| (*a, *b)), plain_copy.iter().map(|(a, b)| (a.0, *b)))?;
    let fast_copy = fast_rows.clone_in(alloc);
    let plain_copy = plain_rows.clone();
    same("soa clone_in", fast_copy.iter().map(|(a, b)| (*a, *b)), plain_copy.iter().map(|(a, b)| (a.0, *b)))?;

    // `==` and `cmp` of vectors (core's slice compares underneath).
    let other = Vec::from_slice_in(&rest, alloc);
    let plain_other = Vec::from_slice_in(&plain_rest, alloc);
    if (fast == other, fast.cmp(&other)) != (generic == plain_other, generic.cmp(&plain_other)) {
        return Err(Broke::Oracle(String::from("eq or cmp differs from the generic one")));
    }
    Ok(())
}

#[test]
fn specialized_paths_match_the_generic_ones() {
    blaze_sim::check(cases(300), specialized_case);
}

/// A `TrustedLen` iterator of tokens that panics at its `k`-th value.
fn tokens_until(n: u64, k: u64) -> impl Iterator<Item = Token> + core::iter::TrustedLen {
    (0..n).map(move |v| {
        if v == k {
            panic!("a planned panic at value {v}");
        }
        Token::new(v)
    })
}

#[test]
fn trusted_extend_frees_everything_when_the_iterator_panics() -> Result<(), Broke> {
    let alloc = Counting::new();
    tokens::fresh();
    {
        let mut vec = ten(&alloc);
        panics(|| vec.extend(tokens_until(20, 6)))?;
        // The ten and the six written before the panic, each counted by the length.
        if vec.len() != 16 {
            return Err(Broke::Oracle(format!("{} values after the panic, not 16", vec.len())));
        }
        tokens::check(16)?;
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn insert_many_frees_everything_when_a_clone_panics() -> Result<(), Broke> {
    let alloc = Counting::new();
    tokens::fresh();
    {
        let mut vec = ten(&alloc);
        let values: StdVec<Token> = (100..106).map(Token::new).collect();
        tokens::panic_on_clone(103);
        panics(|| vec.insert_slice_shift(4, &values))?;
        // The three clones made stay, at 4..7, and the tail closes up behind them.
        let seen: StdVec<u64> = vec.iter().map(|t| t.value).collect();
        let model: StdVec<u64> = (0..4).chain(100..103).chain(4..10).collect();
        same("insert_many after a panic", seen.into_iter(), model.into_iter())?;
        tokens::check(13 + 6)?;
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn collect_in_from_an_exact_iterator_allocates_once() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let before = alloc.allocs();
        let squares = (0..1000u64).map(|i| i * i).collect_in(&alloc);
        let made = alloc.allocs() - before;
        if made != 1 || squares.capacity() != 1000 {
            return Err(Broke::Oracle(format!("{made} allocations, capacity {}", squares.capacity())));
        }
    }
    alloc.check()
}
