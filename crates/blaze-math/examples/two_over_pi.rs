//! A dev-only generator: prints `TWO_OVER_PI` (in `src/fns.rs`), `floor(2/pi * 2^1344)` in 21
//! words of 64 bits, most significant first, three to a line as the source holds them. DERIVED,
//! NOT RECALLED: pi comes from Machin's formula `pi = 16 atan(1/5) - 4 atan(1/239)`, each
//! arctangent's series summed in integers at 1536 fraction bits with every term floored (so pi is
//! a few thousand units of 2^-1536 low, which moves 2/pi by under 2^-1520: more than 170 bits
//! under the table's last), then `2^2881 / (pi 2^1536)` by bit-by-bit long division. Fixed-size arrays
//! throughout. The test `two_over_pi_is_machins` (in `src/precise.rs`) recomputes every word the
//! same way and compares. Run with `cargo run --release -p blaze-math --example two_over_pi`.

/// Words of a number, least significant first: 1664 bits, room for pi 2^1536 times 16.
const WL: usize = 26;
type Wide = [u64; WL];
/// Fraction bits of the series.
const N: u32 = 1536;

/// `a += b`; asserts no carry out.
fn add(a: &mut Wide, b: &Wide) {
    let mut carry = 0u64;
    for (w, &bi) in a.iter_mut().zip(b.iter()) {
        let (s1, c1) = w.overflowing_add(bi);
        let (s2, c2) = s1.overflowing_add(carry);
        *w = s2;
        carry = u64::from(c1) + u64::from(c2);
    }
    assert_eq!(carry, 0, "a sum overflows {WL} words");
}

/// `a -= b` for `a >= b`.
fn sub(a: &mut Wide, b: &Wide) {
    let mut borrow = 0u64;
    for (w, &bi) in a.iter_mut().zip(b.iter()) {
        let (d1, o1) = w.overflowing_sub(bi);
        let (d2, o2) = d1.overflowing_sub(borrow);
        *w = d2;
        borrow = u64::from(o1) + u64::from(o2);
    }
    assert_eq!(borrow, 0, "a difference went below zero");
}

/// `a >= b`.
fn at_least(a: &Wide, b: &Wide) -> bool {
    for (x, y) in a.iter().rev().zip(b.iter().rev()) {
        if x != y {
            return x > y;
        }
    }
    true
}

/// `a = 2 a + bit`; asserts nothing is shifted out.
fn shl1(a: &mut Wide, bit: u64) {
    let mut c = bit;
    for w in a.iter_mut() {
        let out = *w >> 63;
        *w = (*w << 1) | c;
        c = out;
    }
    assert_eq!(c, 0, "a doubling overflows {WL} words");
}

/// `a = floor(a / d)`.
fn div_small(a: &mut Wide, d: u64) {
    let d = u128::from(d);
    let mut r = 0u128;
    for w in a.iter_mut().rev() {
        let cur = (r << 64) | u128::from(*w);
        *w = (cur / d) as u64;
        r = cur % d;
    }
}

/// `a *= k`; asserts no carry out.
fn mul_small(a: &mut Wide, k: u64) {
    let mut carry = 0u128;
    for w in a.iter_mut() {
        let t = u128::from(*w) * u128::from(k) + carry;
        *w = t as u64;
        carry = t >> 64;
    }
    assert_eq!(carry, 0, "a product overflows {WL} words");
}

fn is_zero(a: &Wide) -> bool {
    a.iter().all(|&w| w == 0)
}

/// `atan(1/k) 2^N`: `sum (-1)^i / ((2i+1) k^(2i+1))`, each power and term floored.
fn atan_inv(k: u64) -> Wide {
    let mut sum = [0u64; WL];
    let mut power = [0u64; WL];
    power[(N / 64) as usize] = 1u64 << (N % 64);
    div_small(&mut power, k);
    let mut i = 0u64;
    while !is_zero(&power) {
        let mut term = power;
        div_small(&mut term, 2 * i + 1);
        if i % 2 == 0 {
            add(&mut sum, &term);
        } else {
            sub(&mut sum, &term);
        }
        div_small(&mut power, k * k);
        i += 1;
    }
    sum
}

fn main() {
    // pi 2^N = 16 atan(1/5) 2^N - 4 atan(1/239) 2^N.
    let mut pi = atan_inv(5);
    mul_small(&mut pi, 16);
    let mut tail = atan_inv(239);
    mul_small(&mut tail, 4);
    sub(&mut pi, &tail);

    // floor(2^(1345 + N) / (pi 2^N)) = floor(2/pi 2^1344), one numerator bit at a time.
    let top = 1345 + N;
    let mut rem = [0u64; WL];
    let mut quot = [0u64; WL];
    for i in (0..=top).rev() {
        shl1(&mut rem, u64::from(i == top));
        if at_least(&rem, &pi) {
            sub(&mut rem, &pi);
            let i = i as usize;
            assert!(i < 1344, "2/pi 2^1344 is below 2^1344, so no quotient bit at 2^{i}");
            quot[i / 64] |= 1u64 << (i % 64);
        }
    }

    // Most significant word first, three to a line, `0x` and four-digit groups as in `fns.rs`.
    println!("pub(crate) const TWO_OVER_PI: [u64; 21] = [");
    for line in 0..7 {
        let words: Vec<String> = (0..3)
            .map(|j| {
                let w = quot[20 - (3 * line + j)];
                format!("0x{:04x}_{:04x}_{:04x}_{:04x},", w >> 48, (w >> 32) & 0xffff, (w >> 16) & 0xffff, w & 0xffff)
            })
            .collect();
        println!("    {}", words.join(" "));
    }
    println!("];");
}
