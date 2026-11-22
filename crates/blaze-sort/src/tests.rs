//! Every kernel against the standard library's general form, on the shapes a sort meets
//! (random, sorted, reversed, nearly sorted, few distinct, all equal, low bits only, the edges
//! of each type: signed extremes, both zeros, NaNs, infinities), at every length from 0 to 65
//! and at lengths past each threshold; sort by key checked for stability against std's
//! stable sort; each vector routine against its scalar twin at every length and alignment;
//! and a planted bug for each oracle (`blaze_sim::control`).

use core::cmp::Ordering;
use core::fmt::Debug;
use core::num::NonZero;
use std::{
    format,
    panic::{AssertUnwindSafe, catch_unwind},
    string::{String, ToString},
    vec,
    vec::Vec,
};

use blaze_rng::SplitMix64 as Rng;
use blaze_sim::{Broke, Config, Tape};

use crate::plant::{self, Plant};
use crate::{RadixKey, compare, search};

/// Cases a property runs, fewer under Miri.
const CASES: u32 = if cfg!(miri) { 6 } else { 300 };
/// The longest input drawn: past every threshold, shorter under Miri.
const LONGEST: usize = if cfg!(miri) { 300 } else { 3000 };

fn ensure(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    if holds { Ok(()) } else { Err(Broke::Oracle(why())) }
}

/// A key type under test: how to make one from raw draws, its order by the standard
/// library's form, and when two are the same value.
trait Case: RadixKey + Debug {
    const NAME: &'static str;
    fn make(raw: u64, extra: u64) -> Self;
    /// The standard library's order: `Ord`, or `total_cmp` for floats.
    fn order(a: &Self, b: &Self) -> Ordering;
    /// The same value, bit for bit.
    fn same(a: &Self, b: &Self) -> bool {
        Self::order(a, b).is_eq()
    }
}

macro_rules! int_case {
    ($($t:ty),*) => {$(
        impl Case for $t {
            const NAME: &'static str = stringify!($t);
            fn make(raw: u64, extra: u64) -> $t {
                ((u128::from(extra) << 64 | u128::from(raw)) as u128) as $t
            }
            fn order(a: &$t, b: &$t) -> Ordering {
                a.cmp(b)
            }
        }
    )*};
}

int_case!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

impl Case for bool {
    const NAME: &'static str = "bool";
    fn make(raw: u64, _: u64) -> bool {
        raw & 1 == 1
    }
    fn order(a: &bool, b: &bool) -> Ordering {
        a.cmp(b)
    }
}

impl Case for char {
    const NAME: &'static str = "char";
    fn make(raw: u64, _: u64) -> char {
        let at = (raw % 0x11_0000) as u32;
        char::from_u32(at).unwrap_or(char::from_u32(at % 0xD800).unwrap())
    }
    fn order(a: &char, b: &char) -> Ordering {
        a.cmp(b)
    }
}

impl Case for NonZero<u32> {
    const NAME: &'static str = "NonZero<u32>";
    fn make(raw: u64, _: u64) -> NonZero<u32> {
        NonZero::new((raw as u32).max(1)).unwrap()
    }
    fn order(a: &NonZero<u32>, b: &NonZero<u32>) -> Ordering {
        a.cmp(b)
    }
}

impl Case for NonZero<i16> {
    const NAME: &'static str = "NonZero<i16>";
    fn make(raw: u64, _: u64) -> NonZero<i16> {
        NonZero::new(raw as i16).unwrap_or(NonZero::<i16>::MIN)
    }
    fn order(a: &NonZero<i16>, b: &NonZero<i16>) -> Ordering {
        a.cmp(b)
    }
}

const F64_SPECIAL: [f64; 10] =
    [0.0, -0.0, f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.0, -1.0, f64::MIN_POSITIVE, -5e-324];

impl Case for f64 {
    const NAME: &'static str = "f64";
    fn make(raw: u64, _: u64) -> f64 {
        if raw % 4 == 0 { F64_SPECIAL[(raw / 4 % 10) as usize] } else { f64::from_bits(raw) }
    }
    fn order(a: &f64, b: &f64) -> Ordering {
        a.total_cmp(b)
    }
}

impl Case for f32 {
    const NAME: &'static str = "f32";
    fn make(raw: u64, _: u64) -> f32 {
        if raw % 4 == 0 { F64_SPECIAL[(raw / 4 % 10) as usize] as f32 } else { f32::from_bits(raw as u32) }
    }
    fn order(a: &f32, b: &f32) -> Ordering {
        a.total_cmp(b)
    }
}

impl Case for (u32, u32) {
    const NAME: &'static str = "(u32, u32)";
    fn make(raw: u64, _: u64) -> (u32, u32) {
        ((raw >> 32) as u32, raw as u32)
    }
    fn order(a: &(u32, u32), b: &(u32, u32)) -> Ordering {
        a.cmp(b)
    }
}

impl Case for (i8, u64) {
    const NAME: &'static str = "(i8, u64)";
    fn make(raw: u64, extra: u64) -> (i8, u64) {
        (extra as i8, raw)
    }
    fn order(a: &(i8, u64), b: &(i8, u64)) -> Ordering {
        a.cmp(b)
    }
}

impl Case for (f64, u8) {
    const NAME: &'static str = "(f64, u8)";
    fn make(raw: u64, extra: u64) -> (f64, u8) {
        (f64::make(raw, 0), extra as u8)
    }
    fn order(a: &(f64, u8), b: &(f64, u8)) -> Ordering {
        a.0.total_cmp(&b.0).then(a.1.cmp(&b.1))
    }
}

impl Case for [u16; 3] {
    const NAME: &'static str = "[u16; 3]";
    fn make(raw: u64, _: u64) -> [u16; 3] {
        [(raw >> 32) as u16, (raw >> 16) as u16, raw as u16]
    }
    fn order(a: &[u16; 3], b: &[u16; 3]) -> Ordering {
        a.cmp(b)
    }
}

impl Case for (u128, i64) {
    const NAME: &'static str = "(u128, i64)";
    fn make(raw: u64, extra: u64) -> (u128, i64) {
        (u128::from(extra >> 60) << 100 | u128::from(raw >> 40), raw.cast_signed())
    }
    fn order(a: &(u128, i64), b: &(u128, i64)) -> Ordering {
        a.cmp(b)
    }
}

impl Case for [u64; 3] {
    const NAME: &'static str = "[u64; 3]";
    fn make(raw: u64, extra: u64) -> [u64; 3] {
        [extra % 3, raw >> 48, raw]
    }
    fn order(a: &[u64; 3], b: &[u64; 3]) -> Ordering {
        a.cmp(b)
    }
}

/// Raw values that land on each type's edges once truncated: zero, one, all ones, the sign
/// bits and the largest positives of 8, 16, 32 and 64 bits.
const EDGES: [u64; 13] = [
    0,
    1,
    u64::MAX,
    1 << 63,
    (1 << 63) - 1,
    1 << 31,
    (1 << 31) - 1,
    0xffff_ffff,
    1 << 15,
    0x7fff,
    0x80,
    0x7f,
    0xff,
];

/// The shapes of input.
const SHAPES: u64 = 9;

/// An input of `n` keys of shape `shape`.
fn input<T: Case>(rng: &mut Rng, n: usize, shape: u64) -> Vec<T> {
    let few: Vec<(u64, u64)> = (0..4).map(|_| (rng.next_u64(), rng.next_u64())).collect();
    let mut v: Vec<T> = (0..n)
        .map(|_| {
            let (raw, extra) = match shape {
                1 => few[rng.index(few.len())],
                2 => few[0],
                6 => (rng.next_u64() & 0xffff, 0),
                7 => (EDGES[rng.index(EDGES.len())], EDGES[rng.index(EDGES.len())]),
                8 => (rng.below(1 << 20), 0),
                _ => (rng.next_u64(), rng.next_u64()),
            };
            T::make(raw, extra)
        })
        .collect();
    match shape {
        3 => v.sort_by(T::order),
        4 => v.sort_by(|a, b| T::order(b, a)),
        5 => {
            v.sort_by(T::order);
            for _ in 0..(n / 40 + 1) {
                if n > 1 {
                    let (a, b) = (rng.index(n), rng.index(n));
                    v.swap(a, b);
                }
            }
        }
        _ => {}
    }
    v
}

fn same_all<T: Case>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| T::same(x, y))
}

/// Every kernel of a `RadixKey` against the standard library's form on one input.
fn kernels<T: Case>(v: &[T]) -> Result<(), Broke> {
    let name = T::NAME;
    let n = v.len();
    let mut expect = v.to_vec();
    expect.sort_by(T::order);

    let mut got = v.to_vec();
    crate::sort(&mut got);
    ensure(same_all(&got, &expect), || format!("sort differs for {n} {name}: {v:?} gave {got:?}"))?;

    let mut got = v.to_vec();
    crate::sort_in(blaze_own::testing::heap().alloc(), &mut got).map_err(|e| Broke::Harness(e.to_string()))?;
    ensure(same_all(&got, &expect), || format!("sort_in differs (sort differs) for {n} {name}: {v:?} gave {got:?}"))?;

    // Stability: each key paired with its index; std's stable sort is the reference.
    let mut tagged: Vec<(T, u32)> = v.iter().enumerate().map(|(at, &k)| (k, at as u32)).collect();
    let mut stable = tagged.clone();
    stable.sort_by(|a, b| T::order(&a.0, &b.0));
    crate::sort_by_key_in(blaze_own::testing::heap().alloc(), &mut tagged, |pair| pair.0).map_err(|e| Broke::Harness(e.to_string()))?;
    ensure(tagged.iter().zip(&stable).all(|(a, b)| T::same(&a.0, &b.0) && a.1 == b.1), || {
        format!("sort_by_key differs from the stable sort for {n} {name}: {v:?} gave {tagged:?}")
    })?;

    ensure(crate::is_sorted(&expect), || format!("is_sorted differs: says the sorted {name} {expect:?} is not"))?;
    let says = crate::is_sorted(v);
    let truth = v.is_sorted_by(|a, b| T::order(a, b).is_le());
    ensure(says == truth, || format!("is_sorted differs for {name} {v:?}: {says}"))?;

    let mut deduped = expect.clone();
    let kept = crate::dedup(&mut deduped);
    let mut reference = expect.clone();
    reference.dedup_by(|a, b| T::same(a, b));
    ensure(same_all(&deduped[..kept], &reference), || format!("dedup differs for {name} {expect:?}: {:?}", &deduped[..kept]))?;

    let least = v.iter().copied().min_by(T::order);
    let most = v.iter().copied().max_by(T::order);
    let (got_least, got_most) = (crate::min(v), crate::max(v));
    let agree = |a: Option<T>, b: Option<T>| match (a, b) {
        (Some(a), Some(b)) => T::same(&a, &b),
        (None, None) => true,
        _ => false,
    };
    ensure(agree(least, got_least) && agree(most, got_most), || {
        format!("min or max differs for {name} {v:?}: {got_least:?} {got_most:?}")
    })?;

    // Searches on the sorted keys: every key, and keys made around them.
    for probe in v.iter().chain(&expect[..0]) {
        let want = expect.partition_point(|x| T::order(x, probe).is_lt());
        let got = crate::lower_bound(&expect, probe);
        ensure(got == want, || format!("lower_bound differs for {probe:?} in {n} {name}: {got}, not {want}"))?;
        let found = crate::binary_search(&expect, probe);
        ensure(found == Ok(want), || format!("binary_search differs for {probe:?}: {found:?}"))?;
    }
    for raw in [0, 1, u64::MAX, 1 << 63, 12345] {
        let probe = T::make(raw, raw);
        let want = expect.partition_point(|x| T::order(x, &probe).is_lt());
        let got = crate::lower_bound(&expect, &probe);
        ensure(got == want, || format!("lower_bound differs for {probe:?} in {n} {name}: {got}, not {want}"))?;
        let found = crate::binary_search(&expect, &probe);
        let present = expect.get(want).is_some_and(|x| T::same(x, &probe));
        ensure(found == if present { Ok(want) } else { Err(want) }, || format!("binary_search differs for {probe:?}: {found:?}"))?;
    }
    Ok(())
}

/// The front doors' choice for `Ord` keys: the same as std's sorts.
fn front_doors<T: Case + Ord>(v: &[T]) -> Result<(), Broke> {
    let mut expect = v.to_vec();
    expect.sort();
    let mut got = v.to_vec();
    crate::sort_stable_in(blaze_own::testing::heap().alloc(), &mut got).map_err(|e| Broke::Harness(e.to_string()))?;
    ensure(got == expect, || format!("sort_stable_in differs for {}", T::NAME))?;
    let mut got = v.to_vec();
    crate::sort_unstable(&mut got);
    ensure(got == expect, || format!("sort_unstable differs for {}", T::NAME))
}

/// A length: small often, then past each threshold.
fn length(tape: &Tape) -> usize {
    match tape.below(4, "scale") {
        0 | 1 => tape.int_in(0..=65usize, "short"),
        2 => tape.int_in(66..=600usize.min(LONGEST), "medium"),
        _ => tape.int_in(600usize.min(LONGEST)..=LONGEST, "long"),
    }
}

fn case_of<T: Case>(tape: &Tape) -> Result<(), Broke> {
    let n = length(tape);
    let shape = tape.below(SHAPES, "shape");
    let mut rng = Rng::new(tape.next_u64("seed"));
    kernels::<T>(&input::<T>(&mut rng, n, shape))
}

fn ord_case_of<T: Case + Ord>(tape: &Tape) -> Result<(), Broke> {
    let n = length(tape);
    let shape = tape.below(SHAPES, "shape");
    let mut rng = Rng::new(tape.next_u64("seed"));
    let v = input::<T>(&mut rng, n, shape);
    kernels::<T>(&v)?;
    front_doors::<T>(&v)
}

/// One case over a type the tape picks.
fn any_key(tape: &Tape) -> Result<(), Broke> {
    match tape.below(19, "type") {
        0 => ord_case_of::<u8>(tape),
        1 => ord_case_of::<u16>(tape),
        2 => ord_case_of::<u32>(tape),
        3 => ord_case_of::<u64>(tape),
        4 => ord_case_of::<u128>(tape),
        5 => ord_case_of::<i8>(tape),
        6 => ord_case_of::<i32>(tape),
        7 => ord_case_of::<i64>(tape),
        8 => ord_case_of::<i128>(tape),
        9 => ord_case_of::<bool>(tape),
        10 => ord_case_of::<char>(tape),
        11 => case_of::<f32>(tape),
        12 => case_of::<f64>(tape),
        13 => ord_case_of::<(u32, u32)>(tape),
        14 => ord_case_of::<(i8, u64)>(tape),
        15 => case_of::<(f64, u8)>(tape),
        16 => ord_case_of::<[u16; 3]>(tape),
        17 => ord_case_of::<(u128, i64)>(tape),
        _ => ord_case_of::<[u64; 3]>(tape),
    }
}

/// The refinement: every sort kernel, stable sort-by-key, and SIMD find/mismatch routine equals the standard library's ordering and scalar twins across all input shapes, lengths, and alignments.
#[test]
fn every_kernel_equals_the_standard_form() {
    blaze_sim::check(Config::cases(CASES), any_key);
}

#[test]
fn every_short_length_and_shape() {
    fn all<T: Case>() {
        let mut rng = Rng::new(7);
        for n in 0..=65 {
            for shape in 0..SHAPES {
                let v = input::<T>(&mut rng, n, shape);
                if let Err(broke) = kernels::<T>(&v) {
                    panic!("{broke}");
                }
            }
        }
    }
    all::<u8>();
    all::<u32>();
    all::<i64>();
    all::<i128>();
    all::<f64>();
    all::<f32>();
    all::<char>();
    all::<bool>();
    all::<NonZero<u32>>();
    all::<NonZero<i16>>();
    all::<usize>();
    all::<isize>();
    all::<i16>();
    all::<(u32, u32)>();
    all::<(f64, u8)>();
    all::<[u64; 3]>();
}

#[test]
fn images_order_as_the_standard_form() {
    // Each type's image order is its order, checked on every pair of a set of edge values.
    fn pairs<T: Case>(values: &[T]) {
        for a in values {
            for b in values {
                assert_eq!(a.cmp_image(b), T::order(a, b), "{} {a:?} against {b:?}", T::NAME);
                assert_eq!(a.image().cmp(&b.image()), T::order(a, b), "{} image of {a:?} against {b:?}", T::NAME);
            }
        }
    }
    let raws: Vec<(u64, u64)> = EDGES.iter().flat_map(|&a| EDGES.iter().map(move |&b| (a, b))).collect();
    fn make_all<T: Case>(raws: &[(u64, u64)]) -> Vec<T> {
        raws.iter().map(|&(a, b)| T::make(a, b)).collect()
    }
    pairs::<i8>(&make_all(&raws));
    pairs::<i32>(&make_all(&raws));
    pairs::<i64>(&make_all(&raws));
    pairs::<i128>(&make_all(&raws));
    pairs::<isize>(&make_all(&raws));
    pairs::<(i8, u64)>(&make_all(&raws));
    pairs::<f64>(&F64_SPECIAL);
    pairs::<f32>(&F64_SPECIAL.map(|x| x as f32));
}

/// A payload that is not `Copy` and needs drop: sort by key moves it by cycles.
#[test]
fn sort_by_key_moves_owned_values() {
    blaze_sim::check(Config::cases(CASES), |tape| {
        let n = length(tape);
        let mut rng = Rng::new(tape.next_u64("seed"));
        let distinct = 1 + tape.below(300, "distinct");
        let mut v: Vec<(u16, String)> =
            (0..n).map(|at| (rng.below(distinct) as u16, format!("value {at}"))).collect();
        let mut expect = v.clone();
        expect.sort_by_key(|pair| pair.0);
        crate::sort_by_key_in(blaze_own::testing::heap().alloc(), &mut v, |pair| pair.0).map_err(|e| Broke::Harness(e.to_string()))?;
        ensure(v == expect, || format!("sort_by_key differs on owned values ({n})"))
    });
}

/// A plain struct ordered by one field alone: stability is observable on it, so it is not a
/// `RadixKey` and the stable sort must be the merge sort's, equal to std's.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Keyed {
    key: u8,
    payload: u32,
}

impl PartialOrd for Keyed {
    fn partial_cmp(&self, other: &Keyed) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Keyed {
    fn cmp(&self, other: &Keyed) -> Ordering {
        self.key.cmp(&other.key)
    }
}

#[test]
fn stable_sort_of_other_types_equals_std() {
    blaze_sim::check(Config::cases(CASES), |tape| {
        let n = length(tape);
        let mut rng = Rng::new(tape.next_u64("seed"));
        let distinct = 1 + tape.below(40, "distinct");
        let mut v: Vec<Keyed> =
            (0..n).map(|at| Keyed { key: rng.below(distinct) as u8, payload: at as u32 }).collect();
        if tape.coin("presorted") {
            v.sort_by_key(|k| u32::MAX - k.payload);
        }
        let mut expect = v.clone();
        expect.sort();
        let mut got = v.clone();
        crate::sort_stable_in(blaze_own::testing::heap().alloc(), &mut got).map_err(|e| Broke::Harness(e.to_string()))?;
        ensure(got == expect, || format!("the stable sort of {n} keyed values differs from std's"))?;
        let mut unstable = v.clone();
        crate::sort_unstable(&mut unstable);
        ensure(unstable.iter().map(|k| k.key).eq(expect.iter().map(|k| k.key)), || format!("sort_unstable's keys differ"))
    });
}

#[test]
fn a_panic_in_the_merge_keeps_every_value() {
    let mut rng = Rng::new(3);
    for n in [21, 64, 100, 777] {
        for stop in [0, 1, 5, 40, 300, 2000] {
            let v: Vec<String> = (0..n).map(|_| format!("{}", rng.below(50))).collect();
            let mut sorted = v.clone();
            let mut calls = 0;
            let result = catch_unwind(AssertUnwindSafe(|| {
                crate::sort_stable_by_in(blaze_own::testing::heap().alloc(), &mut sorted, |a, b| {
                    calls += 1;
                    assert!(calls != stop, "planned panic");
                    a < b
                })
            }));
            let mut left = sorted.clone();
            left.sort();
            let mut was = v.clone();
            was.sort();
            assert_eq!(left, was, "values lost or doubled at n {n}, panic at call {stop}");
            if result.is_ok() {
                let mut expect = v.clone();
                expect.sort();
                assert_eq!(sorted, expect);
            }
        }
    }
}

#[test]
fn vector_find_equals_scalar_at_every_length_and_alignment() {
    fn each<T: Copy + PartialEq + Debug>(make: impl Fn(u64) -> T, find: fn(&[T], T) -> Option<usize>) {
        let longest = if cfg!(miri) { 40 } else { 150 };
        let buffer: Vec<T> = (0..longest + 16).map(|at| make(at as u64 % 7)).collect();
        let needle = make(100);
        for offset in 0..16 {
            for len in 0..=longest {
                let mut hay: Vec<T> = buffer[offset..offset + len].to_vec();
                assert_eq!(find(&hay, needle), search::scalar::find(&hay, needle));
                for at in 0..len {
                    let old = hay[at];
                    hay[at] = needle;
                    assert_eq!(find(&hay, needle), Some(at), "len {len} offset {offset} at {at}");
                    if at + 1 < len {
                        hay[len - 1] = needle;
                        assert_eq!(find(&hay, needle), search::scalar::find(&hay, needle));
                        hay[len - 1] = buffer[offset + len - 1];
                    }
                    hay[at] = old;
                }
                // Many hits: every value equal to one at a third of the places.
                let thirds: Vec<T> = hay.iter().enumerate().map(|(at, &x)| if at % 3 == 2 { needle } else { x }).collect();
                assert_eq!(find(&thirds, needle), search::scalar::find(&thirds, needle));
            }
        }
    }
    each(|x| x as u16, search::find_u16);
    each(|x| x as u32, search::find_u32);
    each(|x| x, search::find_u64);
}

#[test]
fn vector_mismatch_equals_scalar_at_every_length_and_alignment() {
    let longest = if cfg!(miri) { 70 } else { 200 };
    let a: Vec<u8> = (0..longest + 16).map(|at| (at * 7 % 251) as u8).collect();
    for offset in 0..16 {
        for len in 0..=longest {
            let x = &a[offset..offset + len];
            let mut y = x.to_vec();
            assert_eq!(compare::mismatch(x, &y), None);
            for at in 0..len {
                y[at] ^= 1;
                assert_eq!(compare::mismatch(x, &y), compare::scalar::mismatch(x, &y), "len {len} offset {offset} at {at}");
                y[at] ^= 1;
            }
        }
    }
}

/// `position`, `contains`, `eq` and `cmp` against core on one type.
fn compares<T: Ord + Copy + Debug>(tape: &Tape, make: impl Fn(u64) -> T) -> Result<(), Broke> {
    let n = length(tape);
    let mut rng = Rng::new(tape.next_u64("seed"));
    let distinct = 1 + tape.below(20, "distinct");
    let a: Vec<T> = (0..n).map(|_| make(rng.below(distinct))).collect();
    let mut b = a.clone();
    if n > 0 && tape.coin("change") {
        let at = rng.index(n);
        b[at] = make(rng.below(distinct + 1));
    }
    if tape.coin("cut") {
        b.truncate(rng.index(n + 1));
    }
    let needle = make(rng.below(distinct + 1));
    let want = a.iter().position(|x| *x == needle);
    ensure(crate::position(&a, &needle) == want, || format!("position differs: {want:?}"))?;
    ensure(crate::contains(&a, &needle) == a.contains(&needle), || format!("contains differs"))?;
    ensure(crate::eq(&a, &b) == (a == b), || format!("eq differs"))?;
    ensure(crate::cmp(&a, &b) == a.cmp(&b), || format!("cmp differs for {a:?} and {b:?}"))
}

fn any_compare(tape: &Tape) -> Result<(), Broke> {
    match tape.below(8, "type") {
        0 => compares(tape, |x| x as u8),
        1 => compares(tape, |x| x as i16 - 7),
        2 => compares(tape, |x| x as u32 * 0x0101_0101),
        3 => compares(tape, |x| x as i32 - 9),
        4 => compares(tape, |x| (x as i64 - 3) << 40),
        5 => compares(tape, |x| char::from_u32(x as u32 + 0x3b1).unwrap()),
        6 => compares(tape, |x| [x as u8, 3, (x * 5) as u8]),
        _ => compares(tape, |x| x as u128 * 0x1_0000_0001),
    }
}

#[test]
fn compares_equal_core() {
    blaze_sim::check(Config::cases(CASES), any_compare);
}

/// The control of one plant: the clean code passes `case`, the planted one is caught saying
/// `says`.
fn control(plant: Plant, says: &str, case: fn(&Tape) -> Result<(), Broke>) {
    blaze_sim::control(Config::cases(CASES), Plant::Clean, plant, says, |tape, planted| {
        plant::with(planted, || case(tape))
    });
}

#[test]
fn the_oracle_catches_a_skipped_digit() {
    control(Plant::SkipVaryingDigit, "sort differs", |tape| ord_case_of::<u64>(tape));
}

#[test]
fn the_oracle_catches_an_unflipped_sign() {
    control(Plant::SignNotFlipped, "sort differs", |tape| ord_case_of::<i32>(tape));
}

#[test]
fn the_oracle_catches_merged_zeros() {
    control(Plant::ZerosMerged, "sort differs", |tape| case_of::<f64>(tape));
}

#[test]
fn the_oracle_catches_an_unstable_sort_by_key() {
    control(Plant::ByKeyUnstable, "sort_by_key differs", |tape| case_of::<u16>(tape));
}

#[test]
fn the_oracle_catches_a_short_lower_bound() {
    control(Plant::LowerBoundShort, "lower_bound differs", |tape| case_of::<u32>(tape));
}

#[test]
fn the_oracle_catches_a_misordered_find() {
    control(Plant::FindSecondVectorFirst, "position differs", |tape| compares(tape, |x| x as u32));
}

#[test]
fn the_oracle_catches_a_skipped_mismatch_vector() {
    control(Plant::MismatchSkipsVector, "cmp differs", |tape| compares(tape, |x| x as u64));
}

#[test]
fn the_scratch_comes_from_the_named_allocator_and_only_when_needed() {
    use blaze_own::{Erased, testing::Counting};
    let counting = Erased::new(Counting::new(std::alloc::System));
    let n = if cfg!(miri) { 300 } else { 10_000 };
    let mut rng = Rng::new(9);
    let mut v: Vec<u32> = (0..n).map(|_| rng.next_u32()).collect();
    crate::sort_in(counting.handle(), &mut v).unwrap();
    assert!(v.is_sorted());
    assert_eq!(counting.inner().allocations(), 1, "one scratch buffer for a sort of random keys");
    // In order, reversed, or all one value: no scratch.
    crate::sort_in(counting.handle(), &mut v).unwrap();
    v.reverse();
    crate::sort_in(counting.handle(), &mut v).unwrap();
    let mut same = vec![5u64; n];
    crate::sort_in(counting.handle(), &mut same).unwrap();
    assert_eq!(counting.inner().allocations(), 1, "input in order, reversed or constant asked for scratch");
    assert_eq!(counting.inner().live(), 0, "the scratch was not freed");
}
