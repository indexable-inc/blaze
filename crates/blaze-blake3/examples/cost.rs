//! What a digest costs on this machine: ns a call and MB/s at 16 B to 16 MiB for this crate's
//! BLAKE3, a scalar reference written here (for comparison only), the official `blake3` crate (a
//! dev-dependency, for comparison only, never in the normal graph), and SHA-256 and SHA-512 by
//! the CPU's own instructions (a probe that changes nothing in the tree); then many short inputs
//! a call against one at a time. Run it with
//! `bash /Volumes/Projects/blaze-int-1112-notes/examplerun.sh blaze-blake3 cost`.
//!
//! Each number is the best of seven timed runs of about 20 ms each, hot in cache: the cost of the
//! compute, not of memory.

use std::{hint::black_box, time::Instant};

const SIZES: &[usize] = &[16, 64, 256, 1024, 2048, 3072, 4096, 8192, 65536, 16 << 20];

/// Seconds a call of `f`, best of seven runs of about 20 ms.
fn seconds_a_call(mut f: impl FnMut()) -> f64 {
    let mut calls = 1u64;
    loop {
        let start = Instant::now();
        for _ in 0..calls {
            f();
        }
        if start.elapsed().as_secs_f64() > 0.005 {
            break;
        }
        calls *= 2;
    }
    calls *= 4;
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        for _ in 0..calls {
            f();
        }
        best = best.min(start.elapsed().as_secs_f64() / calls as f64);
    }
    best
}

fn input(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|at| (at.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 24) as u8).collect()
}

fn cell(len: usize, seconds: f64) -> String {
    format!("{:>9.1} ns {:>7.0} MB/s", seconds * 1e9, len as f64 / seconds / 1e6)
}

fn main() {
    let hw = sha::Hardware::detect();
    sha::check(hw);
    reference::check();

    println!("# one input a call (ns a call, MB/s)");
    println!(
        "{:>9} | {:>24} | {:>24} | {:>24} | {:>24} | {:>24}",
        "bytes", "blaze_blake3::hash", "scalar reference", "official blake3 crate", "SHA-256 (ARMv8 SHA2)", "SHA-512 (ARMv8.2 SHA512)"
    );
    for &len in SIZES {
        let data = input(len, 7);
        let ours = seconds_a_call(|| {
            black_box(blaze_blake3::hash(black_box(&data)));
        });
        let reference = seconds_a_call(|| {
            black_box(reference::hash(black_box(&data)));
        });
        let official = seconds_a_call(|| {
            black_box(blake3::hash(black_box(&data)));
        });
        let sha256 = hw.sha256.map(|token| {
            cell(len, seconds_a_call(|| {
                black_box(sha::sha256(token, black_box(&data)));
            }))
        });
        let sha512 = hw.sha512.map(|token| {
            cell(len, seconds_a_call(|| {
                black_box(sha::sha512(token, black_box(&data)));
            }))
        });
        println!(
            "{len:>9} | {} | {} | {} | {:>24} | {:>24}",
            cell(len, ours),
            cell(len, reference),
            cell(len, official),
            sha256.as_deref().unwrap_or("not on this CPU"),
            sha512.as_deref().unwrap_or("not on this CPU"),
        );
    }

    println!();
    println!("# many inputs (a tape of short inputs; M inputs a second, MB/s)");
    println!(
        "{:>14} | {:>22} | {:>22} | {:>22} | {:>22}",
        "input bytes", "one at a time", "hash_many", "SHA-256 one at a time", "SHA-256 four at a time"
    );
    for (name, lens) in tapes() {
        let total: usize = lens.iter().sum();
        let datas: Vec<Vec<u8>> =
            lens.iter().enumerate().map(|(at, &len)| input(len, at as u32)).collect();
        let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
        let mut out = vec![[0u8; 32]; inputs.len()];
        let rate = |seconds: f64| {
            format!(
                "{:>7.2} M/s {:>7.0} MB/s",
                inputs.len() as f64 / seconds / 1e6,
                total as f64 / seconds / 1e6
            )
        };
        let single = seconds_a_call(|| {
            for (input, slot) in inputs.iter().zip(out.iter_mut()) {
                *slot = blaze_blake3::hash(black_box(input));
            }
            black_box(&out);
        });
        let many = seconds_a_call(|| {
            blaze_blake3::hash_many(black_box(&inputs), &mut out);
            black_box(&out);
        });
        let one_at_a_time: Vec<[u8; 32]> = inputs.iter().map(|input| blaze_blake3::hash(input)).collect();
        blaze_blake3::hash_many(&inputs, &mut out);
        assert!(out == one_at_a_time, "hash_many differs from hash one at a time on {name}");
        let sha256 = hw.sha256.map(|token| {
            rate(seconds_a_call(|| {
                for input in &inputs {
                    black_box(sha::sha256(token, black_box(input)));
                }
            }))
        });
        let equal = lens.iter().all(|&len| len == lens[0]);
        let sha256_x4 = hw.sha256.filter(|_| equal).map(|token| {
            rate(seconds_a_call(|| {
                for four in inputs.chunks_exact(4) {
                    black_box(sha::sha256_x4(token, [four[0], four[1], four[2], four[3]]));
                }
            }))
        });
        println!(
            "{name:>14} | {} | {} | {:>22} | {:>22}",
            rate(single),
            rate(many),
            sha256.as_deref().unwrap_or("not on this CPU"),
            sha256_x4.as_deref().unwrap_or("unequal lengths"),
        );
    }

    engines::tables();
}

/// The strategies at the crate's one seam ([`blaze_blake3::Engine`]), measured one against
/// another and against the official crate, so the default per CPU is a measured choice:
///
/// - by degree: MB/s of one input a call at 1, 2, 4, 8 and 16 lanes (the scalar lone-block
///   form), and at the default degree with the row lone-block form, beside the official crate
///   and the default engine (`hash`);
/// - one block: ns of a chain of dependent compressions, scalar against rows (the latency a
///   lone block pays), and of the lanes kernel a block a lane at each width;
/// - streaming: MB/s of a hasher fed small pieces, ours against the official crate's;
/// - many: M inputs a second of `many_checked_on` at each degree.
mod engines {
    use std::hint::black_box;

    use blaze_blake3::{Degree, Engine, EngineOpts, Hasher, Isa, Mode, Pair, Single};

    use super::{input, seconds_a_call, tapes};

    fn isas() -> Vec<Isa> {
        let mut isas = vec![Isa::BASELINE];
        let detected = Isa::detected();
        if detected != Isa::BASELINE {
            isas.push(detected);
        }
        #[cfg(target_arch = "x86_64")]
        if detected.name() == "avx512f" {
            // SAFETY: `Isa::detected` gave AVX-512F, which it gives only when it saw AVX2 too.
            isas.push(unsafe { Isa::avx2_unchecked() });
        }
        isas
    }

    fn mbs(len: usize, seconds: f64) -> String {
        format!("{:>8.0}", len as f64 / seconds / 1e6)
    }

    pub(super) fn tables() {
        let sizes = [64, 1024, 2048, 3072, 4096, 8192, 65536, 16 << 20];
        for isa in isas() {
            let default = isa.default_degree();
            println!();
            println!(
                "# by degree on {} (MB/s, one input a call; default x{} scalar; rows = the row lone-block form at the default degree; pair = the default degree with batches two wide interleaved in general registers)",
                isa.name(),
                default.lanes()
            );
            println!(
                "{:>9} | {:>8} | {:>8} | {:>8} | {:>8} | {:>8} | {:>8} | {:>8} | {:>8} | {:>8} | {:>8}",
                "bytes", "official", "hash", "x1", "x2", "x4", "x8", "x16", "rows", "pair", "best/off"
            );
            for len in sizes {
                let data = input(len, 11);
                let official = seconds_a_call(|| {
                    black_box(blake3::hash(black_box(&data)));
                });
                let ours = seconds_a_call(|| {
                    black_box(blaze_blake3::hash(black_box(&data)));
                });
                let by_degree: Vec<f64> = Degree::ALL
                    .iter()
                    .map(|&degree| {
                        let engine = Engine::new(EngineOpts { single: Single::Scalar, degree, pair: Pair::Lanes, isa });
                        seconds_a_call(|| {
                            black_box(blaze_blake3::digest_on(engine, Mode::Hash, black_box(&data)));
                        })
                    })
                    .collect();
                let rows_engine =
                    Engine::new(EngineOpts { single: Single::Rows, degree: default, pair: Pair::Lanes, isa });
                let rows = seconds_a_call(|| {
                    black_box(blaze_blake3::digest_on(rows_engine, Mode::Hash, black_box(&data)));
                });
                let pair_engine =
                    Engine::new(EngineOpts { single: Single::Scalar, degree: default, pair: Pair::Interleaved, isa });
                let pair = seconds_a_call(|| {
                    black_box(blaze_blake3::digest_on(pair_engine, Mode::Hash, black_box(&data)));
                });
                let best = by_degree.iter().copied().chain([rows, pair, ours]).fold(f64::MAX, f64::min);
                println!(
                    "{len:>9} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:>8.2}",
                    mbs(len, official),
                    mbs(len, ours),
                    mbs(len, by_degree[0]),
                    mbs(len, by_degree[1]),
                    mbs(len, by_degree[2]),
                    mbs(len, by_degree[3]),
                    mbs(len, by_degree[4]),
                    mbs(len, rows),
                    mbs(len, pair),
                    official / best,
                );
            }

            println!();
            println!("# one block on {} (ns a compression, a chain of dependent ones)", isa.name());
            let scalar = Engine::new(EngineOpts { single: Single::Scalar, degree: Degree::One, pair: Pair::Lanes, isa });
            let rows = Engine::new(EngineOpts { single: Single::Rows, degree: Degree::One, pair: Pair::Lanes, isa });
            let block: [u32; 16] = core::array::from_fn(|at| (at as u32).wrapping_mul(0x9E37_79B9));
            for (label, engine) in [("scalar", scalar), ("rows", rows)] {
                let mut cv = blaze_blake3::IV;
                let chain = 64;
                let seconds = seconds_a_call(|| {
                    for _ in 0..chain {
                        let out = blaze_blake3::compress_by(engine, &cv, black_box(&block), 0, 64, 0);
                        cv = core::array::from_fn(|word| out[word]);
                    }
                    black_box(&cv);
                });
                println!("{label:>9} | {:>8.1} ns", seconds * 1e9 / f64::from(chain));
            }

            println!();
            println!("# streaming on {} (MB/s, 64 KiB fed in pieces of each size)", isa.name());
            println!("{:>9} | {:>8} | {:>8} | {:>8}", "piece", "official", "ours", "ours/off");
            let data = input(64 * 1024, 5);
            for piece in [1, 8, 13, 32, 64, 100, 1024, 4096] {
                let official = seconds_a_call(|| {
                    let mut hasher = blake3::Hasher::new();
                    for part in data.chunks(piece) {
                        hasher.update(black_box(part));
                    }
                    black_box(hasher.finalize());
                });
                let engine = Engine::new(EngineOpts { single: Single::Scalar, degree: default, pair: Pair::Lanes, isa });
                let ours = seconds_a_call(|| {
                    let mut hasher = Hasher::on(engine, Mode::Hash);
                    for part in data.chunks(piece) {
                        hasher.update(black_box(part));
                    }
                    black_box(hasher.finalize());
                });
                println!(
                    "{piece:>9} | {} | {} | {:>8.2}",
                    mbs(data.len(), official),
                    mbs(data.len(), ours),
                    official / ours
                );
            }

            println!();
            println!("# many at a time on {} (M inputs a second, by degree)", isa.name());
            println!(
                "{:>14} | {:>8} | {:>8} | {:>8} | {:>8} | {:>8}",
                "input bytes", "x1", "x2", "x4", "x8", "x16"
            );
            for (name, lens) in tapes() {
                let datas: Vec<Vec<u8>> =
                    lens.iter().enumerate().map(|(at, &len)| input(len, at as u32)).collect();
                let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
                let mut out = vec![[0u8; 32]; inputs.len()];
                let rates: Vec<String> = Degree::ALL
                    .iter()
                    .map(|&degree| {
                        let engine = Engine::new(EngineOpts { single: Single::Scalar, degree, pair: Pair::Lanes, isa });
                        let seconds = seconds_a_call(|| {
                            blaze_blake3::many_checked_on(engine, Mode::Hash, black_box(&inputs), &mut out)
                                .expect("a slot for each input");
                            black_box(&out);
                        });
                        format!("{:>8.2}", inputs.len() as f64 / seconds / 1e6)
                    })
                    .collect();
                println!("{name:>14} | {}", rates.join(" | "));
            }
        }
        lanes_kernel();
    }

    /// The lanes kernel alone, a chain of dependent compressions at each width: ns a block a
    /// lane, the floor the wide tree is held to (rule 67).
    fn lanes_kernel() {
        println!();
        println!("# the lanes kernel at the baseline (ns a block a lane, a chain of dependent ones)");
        fn at<const L: usize>() -> f64 {
            let rows: [[u32; 16]; L] =
                core::array::from_fn(|lane| core::array::from_fn(|word| (lane * 16 + word) as u32));
            let message = blaze_blake3::lanes::columns(&rows);
            let mut cv: [[u32; L]; 8] = blaze_blake3::IV.map(|word| [word; L]);
            let chain = 64;
            let seconds = seconds_a_call(|| {
                for _ in 0..chain {
                    cv = blaze_blake3::lanes::compress(&cv, black_box(&message), [0; L], [0; L], [64; L], 0);
                }
                black_box(&cv);
            });
            seconds * 1e9 / f64::from(chain) / L as f64
        }
        println!(
            "{:>8} | {:>8} | {:>8} | {:>8}\n{:>8.1} | {:>8.1} | {:>8.1} | {:>8.1}",
            "x2",
            "x4",
            "x8",
            "x16",
            at::<2>(),
            at::<4>(),
            at::<8>(),
            at::<16>()
        );
    }
}

/// Tapes of 65,536 inputs: equal lengths, then lengths spread from 1 to 1,024 bytes, log-uniform
/// (most items of a source file are short, a few run to a chunk).
fn tapes() -> Vec<(String, Vec<usize>)> {
    let count = 1 << 16;
    let mut tapes: Vec<(String, Vec<usize>)> =
        [16, 64, 256, 1024].iter().map(|&len| (format!("{len}"), vec![len; count])).collect();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let spread = (0..count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // 2^(10 u), u in [0, 1): 1 to 1,024 bytes, log-uniform.
            let u = (state >> 11) as f64 / (1u64 << 53) as f64;
            (2f64.powf(10.0 * u)) as usize
        })
        .collect();
    tapes.push(("1..1024 log".to_owned(), spread));
    tapes
}

/// BLAKE3 as its paper writes it: one block at a time, the tree built by recursion. For
/// comparison only; the crate is held to the official vectors by its tests.
mod reference {
    const IV: [u32; 8] = [
        0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A, 0x510E_527F, 0x9B05_688C, 0x1F83_D9AB,
        0x5BE0_CD19,
    ];
    const SCHEDULE: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
    const CHUNK_START: u32 = 1;
    const CHUNK_END: u32 = 2;
    const PARENT: u32 = 4;
    const ROOT: u32 = 8;

    fn g(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
        s[a] = s[a].wrapping_add(s[b]).wrapping_add(x);
        s[d] = (s[d] ^ s[a]).rotate_right(16);
        s[c] = s[c].wrapping_add(s[d]);
        s[b] = (s[b] ^ s[c]).rotate_right(12);
        s[a] = s[a].wrapping_add(s[b]).wrapping_add(y);
        s[d] = (s[d] ^ s[a]).rotate_right(8);
        s[c] = s[c].wrapping_add(s[d]);
        s[b] = (s[b] ^ s[c]).rotate_right(7);
    }

    fn compress(cv: &[u32; 8], block: &[u32; 16], counter: u64, len: u32, flags: u32) -> [u32; 16] {
        let mut s = [
            cv[0], cv[1], cv[2], cv[3], cv[4], cv[5], cv[6], cv[7], IV[0], IV[1], IV[2], IV[3],
            counter as u32, (counter >> 32) as u32, len, flags,
        ];
        let mut m = *block;
        for _ in 0..7 {
            g(&mut s, 0, 4, 8, 12, m[0], m[1]);
            g(&mut s, 1, 5, 9, 13, m[2], m[3]);
            g(&mut s, 2, 6, 10, 14, m[4], m[5]);
            g(&mut s, 3, 7, 11, 15, m[6], m[7]);
            g(&mut s, 0, 5, 10, 15, m[8], m[9]);
            g(&mut s, 1, 6, 11, 12, m[10], m[11]);
            g(&mut s, 2, 7, 8, 13, m[12], m[13]);
            g(&mut s, 3, 4, 9, 14, m[14], m[15]);
            m = SCHEDULE.map(|w| m[w]);
        }
        for w in 0..8 {
            s[w] ^= s[w + 8];
            s[w + 8] ^= cv[w];
        }
        s
    }

    fn words(block: &[u8]) -> [u32; 16] {
        let mut padded = [0u8; 64];
        padded[..block.len()].copy_from_slice(block);
        std::array::from_fn(|w| u32::from_le_bytes(padded[4 * w..4 * w + 4].try_into().unwrap()))
    }

    /// A node not yet compressed: chaining value in, block, counter, length, flags.
    type Node = ([u32; 8], [u32; 16], u64, u32, u32);

    fn cv(node: &Node) -> [u32; 8] {
        let out = compress(&node.0, &node.1, node.2, node.3, node.4);
        std::array::from_fn(|w| out[w])
    }

    fn node(input: &[u8], counter: u64) -> Node {
        if input.len() <= 1024 {
            let mut cv = IV;
            let blocks = input.len().max(1).div_ceil(64);
            for b in 0..blocks - 1 {
                let flags = if b == 0 { CHUNK_START } else { 0 };
                let out = compress(&cv, &words(&input[64 * b..64 * b + 64]), counter, 64, flags);
                cv = std::array::from_fn(|w| out[w]);
            }
            let last = &input[64 * (blocks - 1)..];
            let start = if blocks == 1 { CHUNK_START } else { 0 };
            return (cv, words(last), counter, last.len() as u32, start | CHUNK_END);
        }
        let chunks = input.len().div_ceil(1024);
        let left_chunks = 1usize << (usize::BITS - 1 - (chunks - 1).leading_zeros());
        let left = cv(&node(&input[..left_chunks * 1024], counter));
        let right = cv(&node(&input[left_chunks * 1024..], counter + left_chunks as u64));
        let block = std::array::from_fn(|w| if w < 8 { left[w] } else { right[w - 8] });
        (IV, block, 0, 64, PARENT)
    }

    pub(crate) fn hash(input: &[u8]) -> [u8; 32] {
        let root = node(input, 0);
        let out = compress(&root.0, &root.1, 0, root.3, root.4 | ROOT);
        std::array::from_fn(|b| (out[b / 4] >> (8 * (b % 4))) as u8)
    }

    pub(crate) fn check() {
        for len in [0, 1, 63, 64, 65, 1023, 1024, 1025, 2048, 2049, 5000, 17 * 1024 + 3] {
            let data = super::input(len, 3);
            assert_eq!(hash(&data), blaze_blake3::hash(&data), "the reference at {len} B");
        }
    }
}

/// SHA-256 and SHA-512 through the CPU's own instructions (ARMv8's SHA2 and ARMv8.2's SHA512),
/// whole messages with their padding, checked against FIPS 180's examples at start. A probe for
/// the table: nothing in the tree calls it.
mod sha {
    /// A CPU has the instructions: made only by [`Hardware::detect`].
    #[derive(Clone, Copy)]
    pub(crate) struct Sha256Token(());
    #[derive(Clone, Copy)]
    pub(crate) struct Sha512Token(());

    #[derive(Clone, Copy)]
    pub(crate) struct Hardware {
        pub(crate) sha256: Option<Sha256Token>,
        pub(crate) sha512: Option<Sha512Token>,
    }

    impl Hardware {
        #[cfg(target_arch = "aarch64")]
        pub(crate) fn detect() -> Hardware {
            Hardware {
                sha256: std::arch::is_aarch64_feature_detected!("sha2").then_some(Sha256Token(())),
                sha512: std::arch::is_aarch64_feature_detected!("sha3").then_some(Sha512Token(())),
            }
        }

        #[cfg(not(target_arch = "aarch64"))]
        pub(crate) fn detect() -> Hardware {
            Hardware { sha256: None, sha512: None }
        }
    }

    const K256: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    const H256: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K512: [u64; 80] = [
        0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
        0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
        0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
        0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
        0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
        0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
        0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
        0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
        0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
        0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
        0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
        0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
        0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
        0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
        0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
        0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
        0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
        0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
        0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
        0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
    ];
    const H512: [u64; 8] = [
        0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
        0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
    ];

    /// The message's whole blocks, then the padded tail (one or two blocks) with the length.
    fn padded<const B: usize, const L: usize>(input: &[u8], mut blocks: impl FnMut(&[u8])) {
        let whole = input.len() / B * B;
        blocks(&input[..whole]);
        let rest = &input[whole..];
        let mut tail = [0u8; 256];
        tail[..rest.len()].copy_from_slice(rest);
        tail[rest.len()] = 0x80;
        let tail_len = if rest.len() + 1 + L <= B { B } else { 2 * B };
        let bits = (input.len() as u128) * 8;
        tail[tail_len - L..tail_len].copy_from_slice(&bits.to_be_bytes()[16 - L..]);
        blocks(&tail[..tail_len]);
    }

    pub(crate) fn sha256(token: Sha256Token, input: &[u8]) -> [u8; 32] {
        let mut state = H256;
        padded::<64, 8>(input, |blocks| imp::blocks256(token, &mut state, blocks));
        std::array::from_fn(|b| state[b / 4].to_be_bytes()[b % 4])
    }

    /// SHA-256 of four messages of one length, their blocks run together so the four streams'
    /// rounds overlap in the pipeline: the fair rival of `hash_many`, which runs lanes too.
    pub(crate) fn sha256_x4(token: Sha256Token, inputs: [&[u8]; 4]) -> [[u8; 32]; 4] {
        let len = inputs[0].len();
        assert!(inputs.iter().all(|input| input.len() == len), "four messages of one length");
        let whole = len / 64 * 64;
        let tail_len = if len - whole + 9 <= 64 { 64 } else { 128 };
        let mut tails = [[0u8; 128]; 4];
        for (tail, input) in tails.iter_mut().zip(inputs) {
            tail[..len - whole].copy_from_slice(&input[whole..]);
            tail[len - whole] = 0x80;
            tail[tail_len - 8..tail_len].copy_from_slice(&(len as u64 * 8).to_be_bytes());
        }
        let mut states = [H256; 4];
        imp::blocks256_x4(token, &mut states, inputs.map(|input| &input[..whole]));
        imp::blocks256_x4(token, &mut states, [0, 1, 2, 3].map(|k| &tails[k][..tail_len]));
        states.map(|state| std::array::from_fn(|b| state[b / 4].to_be_bytes()[b % 4]))
    }

    pub(crate) fn sha512(token: Sha512Token, input: &[u8]) -> [u8; 64] {
        let mut state = H512;
        padded::<128, 16>(input, |blocks| imp::blocks512(token, &mut state, blocks));
        std::array::from_fn(|b| state[b / 8].to_be_bytes()[b % 8])
    }

    #[cfg(target_arch = "aarch64")]
    mod imp {
        use std::arch::aarch64::*;

        use super::{K256, K512, Sha256Token, Sha512Token};

        pub(super) fn blocks256(_: Sha256Token, state: &mut [u32; 8], blocks: &[u8]) {
            // SAFETY: a `Sha256Token` is made only where the CPU has the SHA2 instructions.
            unsafe { blocks256_sha2(state, blocks) }
        }

        pub(super) fn blocks512(_: Sha512Token, state: &mut [u64; 8], blocks: &[u8]) {
            // SAFETY: a `Sha512Token` is made only where the CPU has the SHA512 instructions.
            unsafe { blocks512_sha3(state, blocks) }
        }

        pub(super) fn blocks256_x4(_: Sha256Token, states: &mut [[u32; 8]; 4], blocks: [&[u8]; 4]) {
            assert!(blocks.iter().all(|b| b.len() == blocks[0].len()), "four streams of one length");
            // SAFETY: a `Sha256Token` is made only where the CPU has the SHA2 instructions.
            unsafe { blocks256_x4_sha2(states, blocks) }
        }

        #[target_feature(enable = "sha2")]
        unsafe fn blocks256_x4_sha2(states: &mut [[u32; 8]; 4], blocks: [&[u8]; 4]) {
            // SAFETY: every load is of 16 bytes inside a state, `K256` or a 64-byte block of a
            // stream (block `at` of each, below the common length), and the stores are into the
            // states.
            unsafe {
                let mut abcd: [uint32x4_t; 4] = std::array::from_fn(|k| vld1q_u32(states[k].as_ptr()));
                let mut efgh: [uint32x4_t; 4] =
                    std::array::from_fn(|k| vld1q_u32(states[k].as_ptr().add(4)));
                for at in (0..blocks[0].len()).step_by(64) {
                    let (abcd0, efgh0) = (abcd, efgh);
                    let mut m: [[uint32x4_t; 4]; 4] = std::array::from_fn(|k| {
                        std::array::from_fn(|q| {
                            vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(blocks[k].as_ptr().add(at + 16 * q))))
                        })
                    });
                    for i in 0..16 {
                        let key = vld1q_u32(K256.as_ptr().add(4 * i));
                        for k in 0..4 {
                            let wk = vaddq_u32(m[k][i % 4], key);
                            let a = abcd[k];
                            abcd[k] = vsha256hq_u32(abcd[k], efgh[k], wk);
                            efgh[k] = vsha256h2q_u32(efgh[k], a, wk);
                            if i < 12 {
                                m[k][i % 4] = vsha256su1q_u32(
                                    vsha256su0q_u32(m[k][i % 4], m[k][(i + 1) % 4]),
                                    m[k][(i + 2) % 4],
                                    m[k][(i + 3) % 4],
                                );
                            }
                        }
                    }
                    for k in 0..4 {
                        abcd[k] = vaddq_u32(abcd[k], abcd0[k]);
                        efgh[k] = vaddq_u32(efgh[k], efgh0[k]);
                    }
                }
                for k in 0..4 {
                    vst1q_u32(states[k].as_mut_ptr(), abcd[k]);
                    vst1q_u32(states[k].as_mut_ptr().add(4), efgh[k]);
                }
            }
        }

        #[target_feature(enable = "sha2")]
        unsafe fn blocks256_sha2(state: &mut [u32; 8], blocks: &[u8]) {
            // SAFETY: every load is of 16 bytes inside `state`, `K256` or a 64-byte block of
            // `blocks` (`chunks_exact`), and `vst1q_u32` stores into `state`.
            unsafe {
                let mut abcd = vld1q_u32(state.as_ptr());
                let mut efgh = vld1q_u32(state.as_ptr().add(4));
                for block in blocks.chunks_exact(64) {
                    let (abcd0, efgh0) = (abcd, efgh);
                    let mut m: [uint32x4_t; 4] = std::array::from_fn(|q| {
                        vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(16 * q))))
                    });
                    for i in 0..16 {
                        let wk = vaddq_u32(m[i % 4], vld1q_u32(K256.as_ptr().add(4 * i)));
                        let a = abcd;
                        abcd = vsha256hq_u32(abcd, efgh, wk);
                        efgh = vsha256h2q_u32(efgh, a, wk);
                        if i < 12 {
                            m[i % 4] = vsha256su1q_u32(
                                vsha256su0q_u32(m[i % 4], m[(i + 1) % 4]),
                                m[(i + 2) % 4],
                                m[(i + 3) % 4],
                            );
                        }
                    }
                    abcd = vaddq_u32(abcd, abcd0);
                    efgh = vaddq_u32(efgh, efgh0);
                }
                vst1q_u32(state.as_mut_ptr(), abcd);
                vst1q_u32(state.as_mut_ptr().add(4), efgh);
            }
        }

        #[target_feature(enable = "sha3")]
        unsafe fn blocks512_sha3(state: &mut [u64; 8], blocks: &[u8]) {
            // SAFETY: every load is of 16 bytes inside `state`, `K512` or a 128-byte block of
            // `blocks` (`chunks_exact`), and `vst1q_u64` stores into `state`.
            unsafe {
                let mut r: [uint64x2_t; 4] =
                    std::array::from_fn(|q| vld1q_u64(state.as_ptr().add(2 * q)));
                for block in blocks.chunks_exact(128) {
                    let saved = r;
                    let mut m: [uint64x2_t; 8] = std::array::from_fn(|q| {
                        vreinterpretq_u64_u8(vrev64q_u8(vld1q_u8(block.as_ptr().add(16 * q))))
                    });
                    for j in 0..40 {
                        // The four registers' roles turn by one each pair of rounds.
                        let t = (3 + 4 * 10 - j) % 4;
                        let (t1, t2, t3) = ((t + 3) % 4, (t + 2) % 4, (t + 1) % 4);
                        let k = vaddq_u64(m[j % 8], vld1q_u64(K512.as_ptr().add(2 * j)));
                        let sum = vaddq_u64(vextq_u64::<1>(k, k), r[t]);
                        let mid = vsha512hq_u64(
                            sum,
                            vextq_u64::<1>(r[t1], r[t]),
                            vextq_u64::<1>(r[t2], r[t1]),
                        );
                        r[t] = vsha512h2q_u64(mid, r[t2], r[t3]);
                        r[t2] = vaddq_u64(r[t2], mid);
                        if j < 32 {
                            m[j % 8] = vsha512su1q_u64(
                                vsha512su0q_u64(m[j % 8], m[(j + 1) % 8]),
                                m[(j + 7) % 8],
                                vextq_u64::<1>(m[(j + 4) % 8], m[(j + 5) % 8]),
                            );
                        }
                    }
                    for q in 0..4 {
                        r[q] = vaddq_u64(r[q], saved[q]);
                    }
                }
                for q in 0..4 {
                    vst1q_u64(state.as_mut_ptr().add(2 * q), r[q]);
                }
            }
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    mod imp {
        use super::{Sha256Token, Sha512Token};

        pub(super) fn blocks256(_: Sha256Token, _: &mut [u32; 8], _: &[u8]) {
            unreachable!("no token is made off aarch64")
        }

        pub(super) fn blocks512(_: Sha512Token, _: &mut [u64; 8], _: &[u8]) {
            unreachable!("no token is made off aarch64")
        }

        pub(super) fn blocks256_x4(_: Sha256Token, _: &mut [[u32; 8]; 4], _: [&[u8]; 4]) {
            unreachable!("no token is made off aarch64")
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// FIPS 180-4's examples (and the NIST CAVP million-`a` message).
    pub(crate) fn check(hw: Hardware) {
        let million = vec![b'a'; 1_000_000];
        let cases256: [(&[u8], &str); 4] = [
            (b"", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            (b"abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (&million, "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"),
        ];
        if let Some(token) = hw.sha256 {
            for (input, want) in cases256 {
                assert_eq!(hex(&sha256(token, input)), want, "SHA-256 of {} B", input.len());
            }
            for len in [0, 3, 55, 56, 63, 64, 119, 120, 1000, 1024] {
                let datas: [Vec<u8>; 4] = std::array::from_fn(|k| super::input(len, k as u32));
                let four = sha256_x4(token, std::array::from_fn(|k| datas[k].as_slice()));
                for k in 0..4 {
                    assert_eq!(four[k], sha256(token, &datas[k]), "SHA-256 x4, stream {k}, {len} B");
                }
            }
            println!("SHA-256 by the CPU's instructions: FIPS 180 examples pass, x4 equals x1");
        }
        let cases512: [(&[u8], &str); 4] = [
            (b"", "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"),
            (b"abc", "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
            (
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909",
            ),
            (&million, "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"),
        ];
        if let Some(token) = hw.sha512 {
            for (input, want) in cases512 {
                assert_eq!(hex(&sha512(token, input)), want, "SHA-512 of {} B", input.len());
            }
            println!("SHA-512 by the CPU's instructions: FIPS 180 examples pass");
        }
    }
}
