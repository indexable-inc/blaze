//! The examples' timing: the best of five runs of a body, as nanoseconds an operation, printed
//! with two decimals by integer arithmetic.

use std::time::Instant;

/// Nanoseconds an operation of `body`, which does `ops` operations: the best of five runs.
pub fn ns_per_op(ops: u64, mut body: impl FnMut() -> u64) -> String {
    let mut best = u128::MAX;
    let mut sink = 0u64;
    for _ in 0..5 {
        let began = Instant::now();
        sink = sink.wrapping_add(std::hint::black_box(body()));
        best = best.min(began.elapsed().as_nanos());
    }
    std::hint::black_box(sink);
    let hundredths = best * 100 / u128::from(ops.max(1));
    format!("{:>7}.{:02}", hundredths / 100, hundredths % 100)
}

/// A row: the size, then each column's label and time.
pub fn row(size: usize, columns: &[(&str, String)]) {
    let cells: Vec<String> = columns.iter().map(|(label, ns)| format!("{label} {ns} ns")).collect();
    println!("{size:>10}  {}", cells.join("   "));
}

/// A deterministic stream of numbers (SplitMix64), so each run times the same work.
pub struct Mix(pub u64);

impl Mix {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Below `n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
