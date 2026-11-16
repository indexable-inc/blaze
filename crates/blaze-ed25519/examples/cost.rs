//! What a verification costs on this machine, set against its floor (rule 67): verifications a
//! second (whole, and with the key decoded once), the key's decoding alone, SHA-512 MB/s, and
//! the field multiplication and squaring each verification is made of. Run it with
//! `bash /Volumes/Projects/blaze-int-1112-notes/examplerun.sh blaze-ed25519 cost`.
//!
//! The floor: a verification is a count of field multiplications and squarings, counted here
//! from the code's formulas with the digit counts of the vectors' real scalars, times the cost
//! of one multiplication and one squaring measured on independent operands (their throughput:
//! the formulas give the core 3 to 4 independent products at a time). What verification spends
//! above that is everything else: SHA-512, the carry chains of additions, encodings, branches.
//!
//! Each number is the best of seven timed runs of about 20 ms each, hot in cache.

use std::hint::black_box;
use std::time::Instant;

use blaze_ed25519::cost::{Element as Fe, digits, order_digits};
use blaze_ed25519::{Encoding, Refused, VerifyingKey, sha512, verify};

#[path = "../tests/data/mod.rs"]
#[allow(dead_code)]
mod data;

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

fn nonzero(digits: &[i8; 256]) -> usize {
    digits.iter().filter(|&&d| d != 0).count()
}

fn top(a: &[i8; 256], b: &[i8; 256]) -> usize {
    (0..256).rev().find(|&i| a[i] != 0 || b[i] != 0).map_or(0, |i| i + 1)
}

/// (squarings, multiplications) of one double-scalar multiplication with these digits: the
/// key's table (one doubling, seven additions), then per position a doubling (4S) brought back
/// to projective (3M), and per digit a conversion to extended (4M) and an addition (4M); then
/// the result to extended (4M).
fn straus_ops(a: &[i8; 256], b: &[i8; 256]) -> (usize, usize) {
    // P cached (1M), 2P (4S + 4M) cached (1M), then seven of: add (4M), to extended (4M), cached (1M).
    let table = (4, 1 + 4 + 1 + 7 * 9);
    let positions = top(a, b);
    let digits = nonzero(a) + nonzero(b);
    (table.0 + 4 * positions, table.1 + 3 * positions + 8 * digits + 4)
}

/// A point's decoding: the square root's power (251S + 11M) and the rest of RFC 8032 5.1.3.
const DECODE: (usize, usize) = (255, 19);
/// Of a decoding, the square root's power: a chain of operations each waiting on the one before,
/// so its floor is their latency, not their throughput.
const DECODE_CHAIN: (usize, usize) = (251, 11);
/// [8]P = identity: three doublings.
const SMALL_ORDER: (usize, usize) = (12, 10);

fn main() {
    let vector = &data::RFC[1];
    let key = VerifyingKey::decode(&vector.public).expect("TEST 2's key");
    assert_eq!(verify(&vector.public, vector.message, &vector.signature), Ok::<(), Refused>(()));
    let _ = Encoding::NotOnCurve;

    let whole = seconds_a_call(|| {
        black_box(verify(black_box(&vector.public), black_box(vector.message), black_box(&vector.signature))).ok();
    });
    let decoded = seconds_a_call(|| {
        black_box(key.verify(black_box(vector.message), black_box(&vector.signature))).ok();
    });
    let key_only = seconds_a_call(|| {
        black_box(VerifyingKey::decode(black_box(&vector.public))).ok();
    });

    // One field multiplication and squaring, 8 independent chains (throughput) and 1 (latency).
    let seeds: [Fe; 8] = std::array::from_fn(|i| {
        let mut bytes = [0x5au8; 32];
        bytes[0] = i as u8;
        bytes[31] = 0x3f;
        Fe::from_bytes(&bytes)
    });
    let factor = Fe::from_bytes(&[0x77; 32]);
    const ROUNDS: usize = 64;
    let mul_throughput = seconds_a_call(|| {
        let mut xs = black_box(seeds);
        for _ in 0..ROUNDS {
            for x in &mut xs {
                *x = x.mul(&factor);
            }
        }
        black_box(xs);
    }) / (ROUNDS * 8) as f64;
    let square_throughput = seconds_a_call(|| {
        let mut xs = black_box(seeds);
        for _ in 0..ROUNDS {
            for x in &mut xs {
                *x = x.square();
            }
        }
        black_box(xs);
    }) / (ROUNDS * 8) as f64;
    let mul_latency = seconds_a_call(|| {
        let mut x = black_box(seeds[0]);
        for _ in 0..ROUNDS {
            x = x.mul(&factor);
        }
        black_box(x);
    }) / ROUNDS as f64;

    // The verification's own count, with TEST 2's real scalars: k = H(R || A || M) mod L, S.
    let (k_digits, s_digits) = digits(&vector.public, vector.message, &vector.signature);
    let (loop_s, loop_m) = straus_ops(&k_digits, &s_digits);
    let verify_s = DECODE.0 + SMALL_ORDER.0 + loop_s;
    let verify_m = DECODE.1 + SMALL_ORDER.1 + loop_m + 4;
    let (l_s, l_m) = straus_ops(&order_digits(), &[0; 256]);
    let key_s = DECODE.0 + SMALL_ORDER.0 + l_s;
    let key_m = DECODE.1 + SMALL_ORDER.1 + l_m;

    let square_latency = seconds_a_call(|| {
        let mut x = black_box(seeds[0]);
        for _ in 0..ROUNDS {
            x = x.square();
        }
        black_box(x);
    }) / ROUNDS as f64;
    // The floor: each decoding's square-root chain at latency, every other operation at
    // throughput.
    let floor = |squarings: usize, multiplications: usize, decodes: usize| {
        let (chain_s, chain_m) = (DECODE_CHAIN.0 * decodes, DECODE_CHAIN.1 * decodes);
        (squarings - chain_s) as f64 * square_throughput
            + (multiplications - chain_m) as f64 * mul_throughput
            + chain_s as f64 * square_latency
            + chain_m as f64 * mul_latency
    };
    let ns = |seconds: f64| seconds * 1e9;
    println!(
        "field: mul {:.2} ns throughput, {:.2} ns latency; square {:.2} ns throughput, {:.2} ns latency",
        ns(mul_throughput),
        ns(mul_latency),
        ns(square_throughput),
        ns(square_latency)
    );
    println!(
        "{:<26} {:>10} {:>12} {:>9} {:>11} {:>7}",
        "", "ns a call", "a second", "S + M", "floor (ns)", "/ floor"
    );
    for (name, seconds, squarings, multiplications, decodes) in [
        ("verify (key decoded each)", whole, verify_s + key_s, verify_m + key_m, 2),
        ("VerifyingKey::verify", decoded, verify_s, verify_m, 1),
        ("VerifyingKey::decode", key_only, key_s, key_m, 1),
    ] {
        let floor_seconds = floor(squarings, multiplications, decodes);
        println!(
            "{name:<26} {:>10.0} {:>12.0} {:>4}+{:<4} {:>11.0} {:>7.2}",
            ns(seconds),
            1.0 / seconds,
            squarings,
            multiplications,
            ns(floor_seconds),
            seconds / floor_seconds
        );
    }

    for len in [64usize, 1024, 1 << 20] {
        let input: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        let seconds = seconds_a_call(|| {
            black_box(sha512::hash(black_box(&input)));
        });
        println!("sha512 {len:>8} B: {:>8.0} ns, {:>7.0} MB/s", ns(seconds), len as f64 / seconds / 1e6);
    }
}
