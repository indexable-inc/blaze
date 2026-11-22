//! What each kernel of blaze-sort costs against the standard library's form, in nanoseconds a
//! key (a probe for the searches), the best of several rounds: the machine is shared, and one
//! round of identical code has been seen 16% from another.
//!
//! `cost` runs the table: `u32`, `u64`, `(u32, u32)`, `f64` and `u64` keys with 16 significant
//! bits, at 16, 256, 4,096, 100,000 and 10,000,000 keys. `cost threshold` runs the sweep the
//! sorts' thresholds were set from. `cost quick` stops at 100,000 keys.

// Implementing `RadixKey`, a `#[rustc_specialization_trait]`, asks for the feature.
#![feature(min_specialization)]

use std::cmp::Ordering;
use std::fmt::Debug;
use std::hint::black_box;
use std::time::Instant;

use blaze_rng::SplitMix64 as Rng;
use blaze_sort::RadixKey;

/// Rounds of each measurement; the least is kept.
const ROUNDS: usize = 5;

trait Bench: RadixKey + PartialEq + Debug {
    const NAME: &'static str;
    fn make(rng: &mut Rng) -> Self;
    /// The standard library's order: `Ord`, or `total_cmp` for floats.
    fn order(a: &Self, b: &Self) -> Ordering;
}

impl Bench for u32 {
    const NAME: &'static str = "u32";
    fn make(rng: &mut Rng) -> u32 {
        rng.next_u32()
    }
    fn order(a: &u32, b: &u32) -> Ordering {
        a.cmp(b)
    }
}

impl Bench for u64 {
    const NAME: &'static str = "u64";
    fn make(rng: &mut Rng) -> u64 {
        rng.next_u64()
    }
    fn order(a: &u64, b: &u64) -> Ordering {
        a.cmp(b)
    }
}

impl Bench for (u32, u32) {
    const NAME: &'static str = "(u32,u32)";
    fn make(rng: &mut Rng) -> (u32, u32) {
        (rng.next_u32(), rng.next_u32())
    }
    fn order(a: &(u32, u32), b: &(u32, u32)) -> Ordering {
        a.cmp(b)
    }
}

impl Bench for f64 {
    const NAME: &'static str = "f64";
    fn make(rng: &mut Rng) -> f64 {
        (rng.unit_f64() - 0.5) * 1e6
    }
    fn order(a: &f64, b: &f64) -> Ordering {
        a.total_cmp(b)
    }
}

/// A `u64` with 16 significant bits: ids, small counts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Short(u64);

// The field: pure, one-to-one, and the derived order is the field's.
blaze_sort::radix_key!(unsafe Short => u64, |short| short.0);

impl Bench for Short {
    const NAME: &'static str = "u64/16b";
    fn make(rng: &mut Rng) -> Short {
        Short(rng.next_u64() & 0xffff)
    }
    fn order(a: &Short, b: &Short) -> Ordering {
        a.cmp(b)
    }
}

/// The least of `ROUNDS` timings of `body`, in nanoseconds.
fn best(mut body: impl FnMut() -> u128) -> f64 {
    (0..ROUNDS).map(|_| body()).min().unwrap() as f64
}

/// Nanoseconds a key of sorting copies of `src` with `sort`, the copy's own time taken off.
fn sort_cost<T: Copy>(src: &[T], mut sort: impl FnMut(&mut [T])) -> f64 {
    let n = src.len();
    let reps = (4_000_000 / n).max(1);
    let mut work = src.to_vec();
    let copy = best(|| {
        let start = Instant::now();
        for _ in 0..reps {
            work.copy_from_slice(src);
            black_box(&mut work);
        }
        start.elapsed().as_nanos()
    });
    let total = best(|| {
        let start = Instant::now();
        for _ in 0..reps {
            work.copy_from_slice(src);
            sort(black_box(&mut work));
            black_box(&work);
        }
        start.elapsed().as_nanos()
    });
    ((total - copy) / (reps * n) as f64).max(0.0)
}

/// Nanoseconds a call of `body`, which is called `calls` times a round.
fn call_cost(calls: usize, mut body: impl FnMut(usize)) -> f64 {
    best(|| {
        let start = Instant::now();
        for at in 0..calls {
            body(at);
        }
        start.elapsed().as_nanos()
    }) / calls as f64
}

fn row(kernel: &str, ty: &str, n: usize, unit: &str, core: f64, ours: f64) {
    println!("{kernel:<16} {ty:<10} {n:>10} {core:>10.2} {ours:>10.2} {:>7.2}x  ns/{unit}", core / ours);
}

fn table<T: Bench>(sizes: &[usize], heap: &'static blaze_own::Heap) {
    let alloc = heap.alloc();
    for &n in sizes {
        let mut rng = Rng::new(n as u64 ^ 0x5eed);
        let src: Vec<T> = (0..n).map(|_| T::make(&mut rng)).collect();
        let name = T::NAME;

        let copy = sort_cost(&src, |_| {});
        let _ = copy;
        let core_stable = sort_cost(&src, |v| v.sort_by(T::order));
        let core_unstable = sort_cost(&src, |v| v.sort_unstable_by(T::order));
        let ours = sort_cost(&src, |v| blaze_sort::sort(v));
        let ours_in = sort_cost(&src, |v| blaze_sort::sort_in(alloc, v).unwrap());
        row("sort", name, n, "key", core_unstable, ours);
        row("sort_in", name, n, "key", core_unstable, ours_in);
        row("sort_in/stable", name, n, "key", core_stable, ours_in);

        let mut sorted = src.clone();
        sorted.sort_by(T::order);
        let already = sort_cost(&sorted, |v| blaze_sort::sort(v));
        let already_core = sort_cost(&sorted, |v| v.sort_unstable_by(T::order));
        row("sort/sorted", name, n, "key", already_core, already);

        let pairs: Vec<(T, u32)> = src.iter().enumerate().map(|(at, &k)| (k, at as u32)).collect();
        let core_by_key = sort_cost(&pairs, |v| v.sort_by(|a, b| T::order(&a.0, &b.0)));
        let ours_by_key = sort_cost(&pairs, |v| blaze_sort::sort_by_key_in(alloc, v, |p| p.0).unwrap());
        row("sort_by_key", name, n, "key", core_by_key, ours_by_key);

        let probes: Vec<T> = (0..1024).map(|_| T::make(&mut rng)).collect();
        let core_search = call_cost(1 << 16, |at| {
            black_box(sorted.binary_search_by(|x| T::order(x, &probes[at & 1023])).is_ok());
        });
        let ours_search = call_cost(1 << 16, |at| {
            black_box(blaze_sort::binary_search(&sorted, &probes[at & 1023]).is_ok());
        });
        row("binary_search", name, n, "probe", core_search, ours_search);

        // A needle that is not there: the whole slice is read.
        let absent = probes.iter().copied().find(|p| !src.contains(p)).unwrap_or(probes[0]);
        let reps = (4_000_000 / n).max(1);
        let core_contains = call_cost(reps, |_| {
            black_box(black_box(&src).contains(black_box(&absent)));
        }) / n as f64;
        let ours_contains = call_cost(reps, |_| {
            black_box(blaze_sort::contains(black_box(&src), black_box(&absent)));
        }) / n as f64;
        row("contains", name, n, "key", core_contains, ours_contains);

        let twin = src.clone();
        let core_eq = call_cost(reps, |_| {
            black_box(black_box(&src[..]) == black_box(&twin[..]));
        }) / n as f64;
        let ours_eq = call_cost(reps, |_| {
            black_box(blaze_sort::eq(black_box(&src), black_box(&twin)));
        }) / n as f64;
        row("==", name, n, "key", core_eq, ours_eq);

        let core_sorted = call_cost(reps, |_| {
            black_box(black_box(&sorted).is_sorted_by(|a, b| T::order(a, b).is_le()));
        }) / n as f64;
        let ours_sorted = call_cost(reps, |_| {
            black_box(blaze_sort::is_sorted(black_box(&sorted)));
        }) / n as f64;
        row("is_sorted", name, n, "key", core_sorted, ours_sorted);

        let core_min = call_cost(reps, |_| {
            black_box(black_box(&src).iter().copied().min_by(T::order));
        }) / n as f64;
        let ours_min = call_cost(reps, |_| {
            black_box(blaze_sort::min(black_box(&src)));
        }) / n as f64;
        row("min", name, n, "key", core_min, ours_min);

        let core_dedup = sort_cost(&sorted, |v| {
            let mut kept = 1;
            for at in 1..v.len() {
                if !T::order(&v[at], &v[kept - 1]).is_eq() {
                    v[kept] = v[at];
                    kept += 1;
                }
            }
            black_box(kept);
        });
        let ours_dedup = sort_cost(&sorted, |v| {
            black_box(blaze_sort::dedup(v));
        });
        row("dedup", name, n, "key", core_dedup, ours_dedup);
    }
}

/// `cmp` for the `Ord` keys: two equal slices, read to the end.
fn cmp_rows<T: Bench + Ord>(sizes: &[usize]) {
    for &n in sizes {
        let mut rng = Rng::new(n as u64);
        let src: Vec<T> = (0..n).map(|_| T::make(&mut rng)).collect();
        let twin = src.clone();
        let reps = (4_000_000 / n).max(1);
        let core = call_cost(reps, |_| {
            black_box(black_box(&src[..]).cmp(black_box(&twin[..])));
        }) / n as f64;
        let ours = call_cost(reps, |_| {
            black_box(blaze_sort::cmp(black_box(&src), black_box(&twin)));
        }) / n as f64;
        row("cmp", T::NAME, n, "key", core, ours);
    }
}

/// The sweep the thresholds were set from: each kernel at lengths around them.
fn threshold<T: Bench>(heap: &'static blaze_own::Heap) {
    let alloc = heap.alloc();
    println!("{:<10} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}", "type", "n", "core", "core-img", "insertion", "msd", "lsd");
    for n in [4, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 2048, 4096] {
        let mut rng = Rng::new(n as u64);
        let src: Vec<T> = (0..n).map(|_| T::make(&mut rng)).collect();
        let core = sort_cost(&src, |v| v.sort_unstable_by(T::order));
        let core_images = sort_cost(&src, |v| blaze_sort::kernels::core_on_images(v));
        let insertion = if n <= 128 { sort_cost(&src, |v| blaze_sort::kernels::insertion(v)) } else { f64::NAN };
        let msd = sort_cost(&src, |v| blaze_sort::kernels::msd(v));
        let lsd = sort_cost(&src, |v| blaze_sort::kernels::lsd(alloc, v).unwrap());
        println!("{:<10} {n:>6} {core:>10.2} {core_images:>10.2} {insertion:>10.2} {msd:>10.2} {lsd:>10.2}", T::NAME);
    }
}

fn main() {
    let heap = blaze_own::Heap::process();
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "threshold" {
        threshold::<u32>(heap);
        threshold::<u64>(heap);
        threshold::<(u32, u32)>(heap);
        return;
    }
    let sizes: &[usize] =
        if mode == "quick" { &[16, 256, 4096, 100_000] } else { &[16, 256, 4096, 100_000, 10_000_000] };
    println!("{:<16} {:<10} {:>10} {:>10} {:>10} {:>8}", "kernel", "type", "n", "core", "blaze", "core/blaze");
    table::<u32>(sizes, heap);
    table::<u64>(sizes, heap);
    table::<(u32, u32)>(sizes, heap);
    table::<f64>(sizes, heap);
    table::<Short>(sizes, heap);
    cmp_rows::<u32>(sizes);
    cmp_rows::<u64>(sizes);
}
