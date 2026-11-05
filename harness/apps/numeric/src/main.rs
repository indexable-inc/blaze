//! Numeric work spread over rayon's pool: every result is the same whatever the order the
//! threads ran in, so the output is compared byte for byte.

use rayon::prelude::*;

/// Iterations until the point leaves the set, up to `limit`.
fn escape(cx: f64, cy: f64, limit: u32) -> u32 {
    let (mut x, mut y) = (0.0f64, 0.0f64);
    for i in 0..limit {
        let (xx, yy) = (x * x, y * y);
        if xx + yy > 4.0 {
            return i;
        }
        y = 2.0 * x * y + cy;
        x = xx - yy + cx;
    }
    limit
}

fn mandelbrot(size: usize, limit: u32) -> u64 {
    (0..size * size)
        .into_par_iter()
        .map(|at| {
            let (px, py) = (at % size, at / size);
            let cx = px as f64 / size as f64 * 3.5 - 2.5;
            let cy = py as f64 / size as f64 * 2.0 - 1.0;
            u64::from(escape(cx, cy, limit))
        })
        .sum()
}

/// A stream of numbers that is the same on every machine.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }

    fn unit(&mut self) -> f64 {
        self.next() as f64 / (1u64 << 53) as f64
    }
}

fn matrix(n: usize, seed: u64) -> Vec<f64> {
    let mut lcg = Lcg(seed);
    (0..n * n).map(|_| lcg.unit() - 0.5).collect()
}

fn multiply(a: &[f64], b: &[f64], n: usize) -> Vec<f64> {
    let mut out = vec![0.0; n * n];
    out.par_chunks_mut(n).enumerate().for_each(|(i, row)| {
        for k in 0..n {
            let aik = a[i * n + k];
            for (j, cell) in row.iter_mut().enumerate() {
                *cell += aik * b[k * n + j];
            }
        }
    });
    out
}

fn sieve(limit: usize) -> usize {
    let root = (limit as f64).sqrt() as usize + 1;
    let small: Vec<usize> = (2..=root).filter(|n| (2..*n).take_while(|d| d * d <= *n).all(|d| n % d != 0)).collect();
    let block = 1 << 16;
    (0..limit.div_ceil(block))
        .into_par_iter()
        .map(|at| {
            let (low, high) = (at * block, ((at + 1) * block).min(limit));
            let mut composite = vec![false; high - low];
            for &p in &small {
                let first = (low.div_ceil(p) * p).max(p * p);
                for multiple in (first..high).step_by(p) {
                    composite[multiple - low] = true;
                }
            }
            composite.iter().enumerate().filter(|(i, c)| !**c && low + i >= 2).count()
        })
        .sum()
}

fn fnv(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf29ce484222325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3))
}

fn sorted_hash(count: usize) -> u64 {
    let mut lcg = Lcg(7);
    let mut values: Vec<u64> = (0..count).map(|_| lcg.next()).collect();
    values.par_sort_unstable();
    fnv(values.iter().step_by(97).flat_map(|it| it.to_le_bytes()))
}

fn nbody(steps: usize) -> f64 {
    let mut lcg = Lcg(11);
    let mut bodies: Vec<([f64; 3], [f64; 3])> =
        (0..400).map(|_| ([lcg.unit(), lcg.unit(), lcg.unit()], [0.0; 3])).collect();
    for _ in 0..steps {
        let places: Vec<[f64; 3]> = bodies.iter().map(|it| it.0).collect();
        bodies.par_iter_mut().for_each(|(place, speed)| {
            let mut pull = [0.0; 3];
            for other in &places {
                let d = [other[0] - place[0], other[1] - place[1], other[2] - place[2]];
                let r2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2] + 1e-3;
                let scale = 1.0 / (r2 * r2.sqrt());
                for axis in 0..3 {
                    pull[axis] += d[axis] * scale;
                }
            }
            for axis in 0..3 {
                speed[axis] += pull[axis] * 1e-5;
                place[axis] += speed[axis] * 1e-2;
            }
        });
    }
    bodies.iter().map(|(place, _)| place[0] + place[1] + place[2]).sum()
}

fn main() {
    let scale: usize = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(1);
    if let Some(threads) = std::env::args().nth(2).and_then(|it| it.parse().ok()) {
        rayon::ThreadPoolBuilder::new().num_threads(threads).build_global().expect("first pool");
    }
    println!("mandelbrot {}", mandelbrot(300 * scale, 200));
    let n = 120 * scale;
    let product = multiply(&matrix(n, 1), &matrix(n, 2), n);
    println!("matrix {:x}", fnv(product.iter().flat_map(|it| it.to_bits().to_le_bytes())));
    println!("trace {:.9}", (0..n).map(|i| product[i * n + i]).sum::<f64>());
    println!("primes {}", sieve(2_000_000 * scale));
    println!("sorted {:x}", sorted_hash(300_000 * scale));
    println!("nbody {:.12}", nbody(10 * scale));
    let (evens, odds): (Vec<u64>, Vec<u64>) = (0..100_000u64).into_par_iter().partition(|n| n % 2 == 0);
    println!("partition {} {}", evens.len(), odds.iter().sum::<u64>());
    let words = ["alpha", "beta", "gamma", "delta"];
    let joined: String = (0..1000).into_par_iter().map(|i| words[i % 4]).collect::<Vec<_>>().join("");
    println!("joined {:x}", fnv(joined.bytes()));
    let caught = std::panic::catch_unwind(|| (0..100).into_par_iter().for_each(|i| assert!(i != 57, "fifty-seven")));
    println!("panic crossed the pool: {}", caught.is_err());
}
