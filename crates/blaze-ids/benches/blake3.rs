//! blaze's BLAKE3 (`blaze-blake3`, through `blaze_ids::b3`) against official BLAKE3 (the
//! `blake3` crate) on blaze's own shapes: `ContentId::of`'s one-shot derive at small sizes,
//! `blaze_ids::Hasher`'s short parts, blaze-diagnostics' `std::hash` writes,
//! `ContentId::of_many`'s batches, and whole files at 4 KiB, 64 KiB, 1 MiB and 64 MiB, in one
//! slice and in the 64 KiB reads blaze-usercode makes.
//!
//! Each shape first checks that both give the same bytes, then times both in alternating
//! order, 21 samples of at least 20 ms each, and prints the medians. The last line names each
//! shape where blaze's is more than 3% slower (the run-to-run noise of a quiet ix VM).

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use blaze_ids::{ContentId, rng::SplitMix64};

/// blaze's, as `blaze_ids` calls it.
mod ours {
    pub use blaze_ids::b3::{Hasher, context_key, derive_key_with, hash};
    use blaze_ids::ContentId;

    pub fn of_many_into(domain: &'static str, inputs: &[&[u8]], out: &mut Vec<ContentId>) {
        ContentId::of_many_into(domain, inputs, out);
    }
}

/// Official BLAKE3, in the same calls.
mod theirs {
    use blake3::hazmat::HasherExt;
    use blaze_ids::ContentId;

    pub fn hash(bytes: &[u8]) -> [u8; 32] {
        *blake3::hash(bytes).as_bytes()
    }

    pub fn derive_key_with(key: &[u8; 32], material: &[u8]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_from_context_key(key);
        hasher.update(material);
        *hasher.finalize().as_bytes()
    }

    #[derive(Default)]
    pub struct Hasher(blake3::Hasher);

    impl Hasher {
        pub fn new() -> Hasher {
            Hasher(blake3::Hasher::new())
        }

        pub fn new_from_context_key(key: &[u8; 32]) -> Hasher {
            Hasher(blake3::Hasher::new_from_context_key(key))
        }

        pub fn update(&mut self, bytes: &[u8]) -> &mut Hasher {
            self.0.update(bytes);
            self
        }

        pub fn finalize(&self) -> [u8; 32] {
            *self.0.finalize().as_bytes()
        }
    }

    /// `ContentId::of` of each input, one at a time.
    pub fn of_many_into(domain: &'static str, inputs: &[&[u8]], out: &mut Vec<ContentId>) {
        let key = blake3::hazmat::hash_derive_key_context(domain);
        out.clear();
        out.extend(inputs.iter().map(|input| ContentId(derive_key_with(&key, input))));
    }
}

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;

/// More than 3% slower is a loss; within it is noise.
const NOISE: f64 = 1.03;

/// Nanoseconds per call of `iters` calls of `f`.
fn sample(f: &mut impl FnMut(), iters: u64) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

/// The median nanoseconds per call of each side, timed in alternating order.
fn pair(mut ours: impl FnMut(), mut theirs: impl FnMut()) -> (f64, f64) {
    const SAMPLES: usize = 21;
    let mut iters = 1u64;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            ours();
            theirs();
        }
        if start.elapsed() >= Duration::from_millis(40) {
            break;
        }
        iters *= 2;
    }
    let (mut a, mut b) = (Vec::with_capacity(SAMPLES), Vec::with_capacity(SAMPLES));
    for at in 0..SAMPLES {
        if at % 2 == 0 {
            a.push(sample(&mut ours, iters));
            b.push(sample(&mut theirs, iters));
        } else {
            b.push(sample(&mut theirs, iters));
            a.push(sample(&mut ours, iters));
        }
    }
    a.sort_by(f64::total_cmp);
    b.sort_by(f64::total_cmp);
    (a[SAMPLES / 2], b[SAMPLES / 2])
}

struct Table {
    lost: Vec<String>,
}

impl Table {
    fn row(&mut self, shape: &str, bytes: usize, (ours, theirs): (f64, f64)) {
        let ratio = ours / theirs;
        let rate = |ns: f64| {
            if bytes == 0 { String::new() } else { format!("{:.2} GB/s", bytes as f64 / ns) }
        };
        println!(
            "{shape:<44} {ours:>12.1} ns {theirs:>12.1} ns  x{ratio:<6.3} {:>12} {:>12}",
            rate(ours),
            rate(theirs)
        );
        if ratio > NOISE {
            self.lost.push(format!("{shape} (x{ratio:.3})"));
        }
    }
}

fn main() {
    println!("backend in use: {}", blaze_ids::b3::BACKEND);
    println!(
        "{:<44} {:>15} {:>15}  {:<7} {:>12} {:>12}",
        "shape", "blaze-blake3", "blake3", "ratio", "ours", "official"
    );
    let mut table = Table { lost: Vec::new() };
    let mut rng = SplitMix64::new(0xb1a3e);
    let mut bytes = vec![0u8; 64 * MIB];
    rng.fill(&mut bytes);
    let key = ours::context_key("blaze v2 file 1");

    for len in [0, 8, 32, 64, 65, 100, 256, 512, 1024, 4096] {
        let input = &bytes[..len];
        assert_eq!(ours::derive_key_with(&key, input), theirs::derive_key_with(&key, input));
        let times = pair(
            || {
                black_box(ours::derive_key_with(&key, black_box(input)));
            },
            || {
                black_box(theirs::derive_key_with(&key, black_box(input)));
            },
        );
        table.row(&format!("ContentId::of, {len} B"), len, times);
    }

    // blaze_ids::Hasher: a context key, three length-prefixed parts and two content IDs.
    macro_rules! parts {
        ($m:ident) => {{
            let mut hasher = $m::Hasher::new_from_context_key(&key);
            for part in [&bytes[..24], &bytes[24..37], &bytes[37..90]] {
                hasher.update(&(part.len() as u64).to_le_bytes());
                hasher.update(black_box(part));
            }
            hasher.update(black_box(&bytes[100..132]));
            hasher.update(black_box(&bytes[132..164]));
            hasher.finalize()
        }};
    }
    assert_eq!(parts!(ours), parts!(theirs));
    let times = pair(
        || {
            black_box(parts!(ours));
        },
        || {
            black_box(parts!(theirs));
        },
    );
    table.row("Hasher, 3 parts and 2 ids (blaze-ids)", 0, times);

    // blaze-diagnostics' key: a diagnostic's `Hash` form, sixteen 8-byte writes.
    macro_rules! writes {
        ($m:ident) => {{
            let mut hasher = $m::Hasher::new();
            for word in bytes[..128].chunks_exact(8) {
                hasher.update(black_box(word));
            }
            hasher.finalize()
        }};
    }
    assert_eq!(writes!(ours), writes!(theirs));
    let times = pair(
        || {
            black_box(writes!(ours));
        },
        || {
            black_box(writes!(theirs));
        },
    );
    table.row("Hasher, 16 writes of 8 B (blaze-diagnostics)", 0, times);

    let mixed = |rng: &mut SplitMix64| {
        if rng.one_in(8) { rng.range(1025..8 * KIB as u64 + 1) } else { rng.range(0..1025) }
    };
    let batches: [(&str, Vec<usize>); 4] = [
        ("of_many, 4096 x 32 B", vec![32; 4096]),
        ("of_many, 4096 x 0..=256 B", (0..4096).map(|_| rng.range(0..257) as usize).collect()),
        ("of_many, 4096 x 0..=1024 B", (0..4096).map(|_| rng.range(0..1025) as usize).collect()),
        ("of_many, 1024 x 0..=8 KiB", (0..1024).map(|_| mixed(&mut rng) as usize).collect()),
    ];
    for (shape, lens) in &batches {
        let mut at = 0;
        let inputs: Vec<&[u8]> = lens
            .iter()
            .map(|&len| {
                let input = &bytes[at..at + len];
                at += len + 7;
                input
            })
            .collect();
        let total: usize = lens.iter().sum();
        let (mut mine, mut official) = (Vec::new(), Vec::new());
        ours::of_many_into("blaze v2 a 1", &inputs, &mut mine);
        theirs::of_many_into("blaze v2 a 1", &inputs, &mut official);
        assert_eq!(mine, official, "{shape}");
        assert_eq!(mine[0], ContentId::of("blaze v2 a 1", inputs[0]));
        let times = pair(
            || ours::of_many_into("blaze v2 a 1", black_box(&inputs), &mut mine),
            || theirs::of_many_into("blaze v2 a 1", black_box(&inputs), &mut official),
        );
        table.row(shape, total, times);
    }

    for len in [4 * KIB, 64 * KIB, MIB, 64 * MIB] {
        let input = &bytes[..len];
        assert_eq!(ours::hash(input), theirs::hash(input));
        let times = pair(
            || {
                black_box(ours::hash(black_box(input)));
            },
            || {
                black_box(theirs::hash(black_box(input)));
            },
        );
        table.row(&format!("hash, {} KiB", len / KIB), len, times);
    }

    // blaze-usercode's `hash_reader`: a file in 64 KiB reads.
    macro_rules! reads {
        ($m:ident, $input:expr) => {{
            let mut hasher = $m::Hasher::new();
            for read in $input.chunks(64 * KIB) {
                hasher.update(black_box(read));
            }
            hasher.finalize()
        }};
    }
    for len in [64 * KIB, MIB, 64 * MIB] {
        let input = &bytes[..len];
        assert_eq!(reads!(ours, input), reads!(theirs, input));
        let times = pair(
            || {
                black_box(reads!(ours, input));
            },
            || {
                black_box(reads!(theirs, input));
            },
        );
        table.row(&format!("Hasher, 64 KiB reads, {} KiB", len / KIB), len, times);
    }

    if table.lost.is_empty() {
        println!("verdict: blaze-blake3 within x{NOISE} of official BLAKE3 everywhere");
    } else {
        println!("verdict: blaze-blake3 slower on: {}", table.lost.join(", "));
    }
}
