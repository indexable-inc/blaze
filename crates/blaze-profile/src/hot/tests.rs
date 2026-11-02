use blaze_hash::HashMap;
use hegel::{TestCase, generators as gs};

use super::*;

/// A stream of `(key, ticks)`: few keys, so they collide in the sketch and repeat, and ticks up
/// to a day at 3 GHz.
fn stream(tc: &TestCase) -> Vec<(u64, u64)> {
    let keys = gs::integers::<u64>().min_value(1).max_value(200);
    let ticks = gs::integers::<u64>().max_value(1 << 48);
    tc.draw(gs::vecs(gs::tuples!(keys, ticks)).max_size(400))
}

fn exact(stream: &[(u64, u64)]) -> HashMap<u64, u64> {
    let mut exact = HashMap::default();
    for &(key, ticks) in stream {
        *exact.entry(key).or_default() += ticks;
    }
    exact
}

#[hegel::test(test_cases = 200)]
fn count_min_never_undercounts(tc: TestCase) {
    let stream = stream(&tc);
    let mut sketch = CountMin::new();
    for &(key, ticks) in &stream {
        let after = sketch.add(key, ticks);
        assert_eq!(after, sketch.estimate(key), "add says the estimate it leaves");
    }
    let exact = exact(&stream);
    for (key, ticks) in &exact {
        assert!(sketch.estimate(*key) >= *ticks, "key {key}: estimate {} under {ticks}", sketch.estimate(*key));
    }
    assert_eq!(sketch.total(), stream.iter().map(|it| it.1).sum::<u64>());
}

#[hegel::test(test_cases = 200)]
fn count_min_merged_is_the_sketch_of_the_union(tc: TestCase) {
    let stream = stream(&tc);
    let cut = tc.draw(gs::integers::<usize>().max_value(stream.len()));
    let (mut whole, mut left, mut right) = (CountMin::new(), CountMin::new(), CountMin::new());
    for (at, &(key, ticks)) in stream.iter().enumerate() {
        whole.add(key, ticks);
        if at < cut { left.add(key, ticks) } else { right.add(key, ticks) };
    }
    let mut merged = CountMin::new();
    merged.merge(&left);
    merged.merge(&right);
    assert_eq!(merged.cells(), whole.cells());
    merged.since(&left);
    assert_eq!(merged.cells(), right.cells(), "the sum less an earlier copy is the stream since");
}

#[hegel::test(test_cases = 200)]
fn a_key_over_the_floor_is_listed_and_its_exact_ticks_are_never_over(tc: TestCase) {
    let stream = stream(&tc);
    let (mut sketch, mut top) = (CountMin::new(), TopKeys::new());
    for &(key, ticks) in &stream {
        let estimate = sketch.add(key, ticks);
        top.offer(key, estimate, ticks);
    }
    let exact = exact(&stream);
    let listed: HashMap<u64, (u64, u64)> = top.entries().map(|(key, estimate, ticks)| (key, (estimate, ticks))).collect();
    for (key, ticks) in &exact {
        if *ticks > top.floor() {
            assert!(listed.contains_key(key), "key {key} has {ticks} ticks, over the floor {}, and is not listed", top.floor());
        }
    }
    for (key, (estimate, counted)) in &listed {
        let ticks = exact[key];
        assert!(*counted <= ticks, "key {key}: listed {counted}, over its {ticks}");
        assert!(*estimate <= sketch.estimate(*key), "key {key}: its entry's estimate is the sketch's at an offer");
    }
    assert!(listed.len() <= TOP);
}

#[hegel::test(test_cases = 300)]
fn a_ddsketch_quantile_is_within_alpha_of_the_exact(tc: TestCase) {
    let values: Vec<u64> = tc.draw(gs::vecs(gs::integers::<u64>().max_value(1 << 40)).min_size(1).max_size(500));
    let per_mille = tc.draw(gs::integers::<u16>().max_value(1000));
    let q = Quantile::per_mille(per_mille).unwrap();
    let mut sketch = DdSketch::new();
    for &it in &values {
        sketch.add(it);
    }
    let mut sorted = values.clone();
    sorted.sort_unstable();
    let exact = sorted[q.rank(sorted.len() as u64) as usize] as f64;
    let estimate = sketch.quantile(q).unwrap();
    // The float slack is the logarithm's rounding at a bucket's edge.
    assert!(
        (estimate - exact).abs() <= ALPHA * exact * (1.0 + 1e-9),
        "q {per_mille}/1000 of {} values: estimate {estimate}, exact {exact}",
        values.len()
    );
    assert_eq!(sketch.count(), values.len() as u64);
}

#[hegel::test(test_cases = 200)]
fn a_ddsketch_merged_is_the_sketch_of_the_union(tc: TestCase) {
    let values: Vec<u64> = tc.draw(gs::vecs(gs::integers::<u64>()).max_size(300));
    let cut = tc.draw(gs::integers::<usize>().max_value(values.len()));
    let (mut whole, mut left, mut right) = (DdSketch::new(), DdSketch::new(), DdSketch::new());
    for (at, &it) in values.iter().enumerate() {
        whole.add(it);
        if at < cut { left.add(it) } else { right.add(it) };
    }
    let mut merged = DdSketch::new();
    merged.merge(&left);
    merged.merge(&right);
    assert_eq!(merged.cells(), whole.cells());
    merged.since(&left);
    assert_eq!(merged.cells(), right.cells());
}

#[test]
fn an_empty_ddsketch_has_no_quantile_and_zero_is_its_own_bucket() {
    let mut sketch = DdSketch::new();
    assert_eq!(sketch.quantile(Quantile::P50), None);
    sketch.add(0);
    sketch.add(0);
    sketch.add(1_000_000);
    assert_eq!(sketch.quantile(Quantile::P50), Some(0.0));
    let max = sketch.quantile(Quantile::MAX).unwrap();
    assert!((max - 1e6).abs() <= ALPHA * 1e6, "{max}");
    assert_eq!(Quantile::per_mille(1001), None);
}

/// A value past the last bucket is counted there: the one place the accuracy does not hold,
/// said in [`BUCKETS`]'s doc.
#[test]
fn a_value_past_the_buckets_is_counted_in_the_last() {
    assert_eq!(DdSketch::cell_of(u64::MAX), BUCKETS);
    assert_eq!(DdSketch::cell_of(1), 1);
    assert!(DdSketch::value_of(BUCKETS) > 2e13, "the last bucket is past two hours at 3 GHz");
}

/// What the record path adds to each span's close: [`record::closed`] over 4,096 keys, a wait
/// every eighth. Compare with `what_a_span_and_a_plot_cost` in lib.rs.
#[test]
#[ignore = "a measurement, not a check: run it in release with --ignored --nocapture"]
fn what_recording_a_close_costs() {
    const N: u64 = 4_000_000;
    let cells = record::cells();
    let started = std::time::Instant::now();
    for it in 0..N {
        let key = record::key(1 + (it % 7) as u16, (it.wrapping_mul(0x9e37_79b9) % 4096) as u32);
        record::closed(&cells, key, it % 100_000, u64::from(it % 8 == 0));
    }
    let ns = started.elapsed().as_nanos() / u128::from(N);
    assert_eq!(Shared(&cells).get(record::SPANS), N);
    println!("a span's close into the sketches: {ns} ns");
}

/// The atomics a ring records into give the same sketch as the plain cells.
#[test]
fn shared_cells_record_as_plain_ones() {
    let atomics: Vec<AtomicU64> = (0..CountMin::CELLS).map(|_| AtomicU64::new(0)).collect();
    let mut plain = CountMin::new();
    for (key, ticks) in [(7, 10), (9, 3), (7, 5)] {
        CountMin::add_to(&mut Shared(&atomics), key, ticks);
        plain.add(key, ticks);
    }
    let mut read = CountMin::new();
    read.merge_cells(&Shared(&atomics));
    assert_eq!(read.cells(), plain.cells());
    assert_eq!(read.estimate(7), 15);
}
