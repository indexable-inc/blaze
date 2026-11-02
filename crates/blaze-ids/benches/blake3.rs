//! blake3-servil against official BLAKE3 on blaze's own shapes, both through `blaze_ids::b3`'s
//! backends: `ContentId::of`'s one-shot derive at small sizes, `blaze_ids::Hasher`'s short
//! parts, blaze-diagnostics' `std::hash` writes, `ContentId::of_many`'s batches, and whole
//! files at 4 KiB, 64 KiB, 1 MiB and 64 MiB, in one slice and in the 64 KiB reads
//! blaze-usercode makes.
//!
//! Each shape first checks that both give the same bytes, then times both in alternating
//! order, 21 samples of at least 20 ms each, and prints the medians. The last line is the
//! verdict: `adopt` when blake3-servil is nowhere more than 3% slower (the run-to-run noise of a
//! quiet ix VM), `revert` naming each shape it lost.

#[cfg(all(
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
))]
fn main() {
    run::main();
}

#[cfg(not(all(
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
)))]
fn main() {
    eprintln!("blake3-servil builds for x86-64 Linux and AArch64 Linux and macOS only; this target has official BLAKE3 alone");
    std::process::exit(1);
}

#[cfg(all(
    target_endian = "little",
    any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
    ),
))]
mod run {
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };

    use blaze_ids::{
        ContentId,
        b3::{official, servil},
        rng::SplitMix64,
    };

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
            let rate = |ns: f64| if bytes == 0 { String::new() } else { format!("{:.2} GB/s", bytes as f64 / ns) };
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

    pub(super) fn main() {
        println!("backend in use: {}", blaze_ids::b3::BACKEND);
        println!("{:<44} {:>15} {:>15}  {:<7} {:>12} {:>12}", "shape", "blake3-servil", "blake3", "ratio", "servil", "official");
        let mut table = Table { lost: Vec::new() };
        let mut rng = SplitMix64::new(0xb1a3e);
        let mut bytes = vec![0u8; 64 * MIB];
        rng.fill(&mut bytes);
        let key = official::context_key("blaze v2 file 1");

        for len in [0, 8, 32, 64, 65, 100, 256, 512, 1024, 4096] {
            let input = &bytes[..len];
            assert_eq!(servil::derive_key_with(&key, input), official::derive_key_with(&key, input));
            let times = pair(
                || {
                    black_box(servil::derive_key_with(&key, black_box(input)));
                },
                || {
                    black_box(official::derive_key_with(&key, black_box(input)));
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
        assert_eq!(parts!(servil), parts!(official));
        let times = pair(
            || {
                black_box(parts!(servil));
            },
            || {
                black_box(parts!(official));
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
        assert_eq!(writes!(servil), writes!(official));
        let times = pair(
            || {
                black_box(writes!(servil));
            },
            || {
                black_box(writes!(official));
            },
        );
        table.row("Hasher, 16 writes of 8 B (blaze-diagnostics)", 0, times);

        let batches: [(&str, Vec<usize>); 4] = [
            ("of_many, 4096 x 32 B", vec![32; 4096]),
            ("of_many, 4096 x 0..=256 B", (0..4096).map(|_| rng.range(0..257) as usize).collect()),
            ("of_many, 4096 x 0..=1024 B", (0..4096).map(|_| rng.range(0..1025) as usize).collect()),
            (
                "of_many, 1024 x 0..=8 KiB",
                (0..1024).map(|_| if rng.one_in(8) { rng.range(1025..8 * KIB as u64 + 1) } else { rng.range(0..1025) } as usize).collect(),
            ),
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
            let (mut ours, mut theirs) = (Vec::new(), Vec::new());
            servil::of_many_into("blaze v2 a 1", &inputs, &mut ours);
            official::of_many_into("blaze v2 a 1", &inputs, &mut theirs);
            assert_eq!(ours, theirs, "{shape}");
            assert_eq!(ours[0], ContentId::of("blaze v2 a 1", inputs[0]));
            let times = pair(
                || servil::of_many_into("blaze v2 a 1", black_box(&inputs), &mut ours),
                || official::of_many_into("blaze v2 a 1", black_box(&inputs), &mut theirs),
            );
            table.row(shape, total, times);
        }

        for len in [4 * KIB, 64 * KIB, MIB, 64 * MIB] {
            let input = &bytes[..len];
            assert_eq!(servil::hash(input), official::hash(input));
            let times = pair(
                || {
                    black_box(servil::hash(black_box(input)));
                },
                || {
                    black_box(official::hash(black_box(input)));
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
            assert_eq!(reads!(servil, input), reads!(official, input));
            let times = pair(
                || {
                    black_box(reads!(servil, input));
                },
                || {
                    black_box(reads!(official, input));
                },
            );
            table.row(&format!("Hasher, 64 KiB reads, {} KiB", len / KIB), len, times);
        }

        if table.lost.is_empty() {
            println!("verdict: adopt (identical output; blake3-servil within x{NOISE} everywhere)");
        } else {
            println!("verdict: revert (slower on: {})", table.lost.join(", "));
        }
    }
}
