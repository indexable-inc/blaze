//! The algebra under verification, as properties on drawn values (blaze_sim): the field's laws,
//! its encoding, inversion and square root; the constants held to their definitions; the
//! reduction mod L against a slow one; and the group's laws on drawn points.

use blaze_sim::{Config, Tape};

use crate::field::{D, Fe, SQRT_M1};
use crate::point::{BASEPOINT, Encoding, Point, double_scalar_mul_basepoint};
use crate::scalar::{self, L, MU};

fn cases(n: u32) -> Config {
    Config::cases(if cfg!(miri) { 2 } else { n })
}

fn small(n: u64) -> Fe {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    Fe::from_bytes(&bytes)
}

/// Any 255-bit value, canonical or not; a third of the time one near 0 or near p, where carries
/// and the final reduction are tested hardest.
fn draw_fe(tape: &Tape) -> Fe {
    let mut bytes = [0u8; 32];
    tape.fill(&mut bytes, "element");
    match tape.below(3, "near an edge") {
        0 => {}
        1 => bytes[4..].fill(0),
        _ => {
            bytes[4..31].fill(0xff);
            bytes[31] = 0x7f;
        }
    }
    Fe::from_bytes(&bytes)
}

fn draw_scalar(tape: &Tape) -> [u64; 4] {
    let mut wide = [0u8; 64];
    tape.fill(&mut wide, "scalar");
    scalar::reduce_wide(&wide)
}

/// [s]B + T for a drawn s and a drawn point of small order T: points of every order.
fn draw_point(tape: &Tape) -> Point {
    let base = double_scalar_mul_basepoint(&[0; 4], &Point::IDENTITY, &draw_scalar(tape));
    let torsion = Point::decode(&TORSION_1).expect("an order-8 point");
    let mut point = base;
    for _ in 0..tape.below(8, "torsion") {
        point = point.add(&torsion);
    }
    point
}

const TORSION_1: [u8; 32] = [
    0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67, 0x0f, 0x2a, 0x20, 0x53, 0xfa,
    0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac, 0x03, 0x7a,
];

#[test]
fn the_field_laws() {
    blaze_sim::check(cases(300), |tape| {
        let (a, b, c) = (draw_fe(tape), draw_fe(tape), draw_fe(tape));
        assert!(a.add(&b).equals(&b.add(&a)), "a + b = b + a");
        assert!(a.mul(&b).equals(&b.mul(&a)), "ab = ba");
        assert!(a.add(&b).add(&c).equals(&a.add(&b.add(&c))), "(a + b) + c = a + (b + c)");
        assert!(a.mul(&b).mul(&c).equals(&a.mul(&b.mul(&c))), "(ab)c = a(bc)");
        assert!(a.mul(&b.add(&c)).equals(&a.mul(&b).add(&a.mul(&c))), "a(b + c) = ab + ac");
        assert!(a.sub(&b).add(&b).equals(&a), "(a - b) + b = a");
        assert!(a.add(&a.neg()).is_zero(), "a + -a = 0");
        assert!(a.square().equals(&a.mul(&a)), "a^2 = aa");
        assert!(a.pow2k(3).equals(&a.square().square().square()));
        assert!(a.mul(&Fe::ONE).equals(&a) && a.mul(&Fe::ZERO).is_zero());
        if !a.is_zero() {
            assert!(a.mul(&a.invert()).equals(&Fe::ONE), "a a^-1 = 1");
        }
        assert!(Fe::ZERO.invert().is_zero());
        // Limbs at their bound: a chain of additions and subtractions stays exact.
        let mut sum = Fe::ZERO;
        for _ in 0..20 {
            sum = sum.add(&a).sub(&b).add(&b);
        }
        assert!(sum.equals(&a.mul(&small(20))));
    });
}

/// Encoding: a 255-bit value reads back to itself exactly when it is below p, and to itself
/// minus p otherwise; every boundary from p - 20 to 2^255 - 1.
#[test]
fn the_canonical_encoding() {
    let p_minus_20 = {
        let mut bytes = [0xff; 32];
        bytes[0] = 0xed - 20;
        bytes[31] = 0x7f;
        bytes
    };
    for add in 0u8..=37 {
        let mut bytes = p_minus_20;
        bytes[0] += add;
        let value = Fe::from_bytes(&bytes);
        if add < 20 {
            assert_eq!(value.to_bytes(), bytes, "p - {} is canonical", 20 - add);
        } else {
            let mut reduced = [0u8; 32];
            reduced[0] = add - 20;
            assert_eq!(value.to_bytes(), reduced, "p + {} reads as {}", add - 20, add - 20);
        }
    }
    blaze_sim::check(cases(500), |tape| {
        let mut bytes = [0u8; 32];
        tape.fill(&mut bytes, "bytes");
        bytes[31] &= 0x7f;
        let value = Fe::from_bytes(&bytes);
        assert_eq!(Fe::from_bytes(&value.to_bytes()).to_bytes(), value.to_bytes(), "encoding is a fixed point");
        // Bit 255 is ignored.
        let mut signed = bytes;
        signed[31] |= 0x80;
        assert!(Fe::from_bytes(&signed).equals(&value));
    });
}

#[test]
fn the_constants_are_their_definitions() {
    // d = -121665 / 121666
    assert!(D.mul(&small(121_666)).equals(&small(121_665).neg()), "d 121666 = -121665");
    // sqrt(-1)^2 = -1
    assert!(SQRT_M1.square().equals(&Fe::ONE.neg()), "sqrt(-1)^2 = -1");
    // MU = floor(2^512 / L): MU L <= 2^512 < (MU + 1) L, in 9 limbs.
    let product = mul_wide(&MU, &L);
    assert!(product[8] == 0 || (product[8] == 1 && product[..8].iter().all(|&x| x == 0)), "MU L <= 2^512");
    let next = add_wide(&product, &L);
    assert!(next[8] >= 1 && !(next[8] == 1 && next[..8].iter().all(|&x| x == 0)), "(MU + 1) L > 2^512");
    // L is 2^252 + 27742317777372353535851937790883648493.
    let c: u128 = 27_742_317_777_372_353_535_851_937_790_883_648_493;
    assert_eq!(L, [c as u64, (c >> 64) as u64, 0, 1 << 60]);
}

fn mul_wide(a: &[u64; 5], b: &[u64; 4]) -> [u64; 9] {
    let mut out = [0u128; 10];
    for i in 0..5 {
        for j in 0..4 {
            let product = a[i] as u128 * b[j] as u128;
            out[i + j] += product & u64::MAX as u128;
            out[i + j + 1] += product >> 64;
        }
    }
    for i in 0..9 {
        out[i + 1] += out[i] >> 64;
        out[i] &= u64::MAX as u128;
    }
    std::array::from_fn(|i| out[i] as u64)
}

fn add_wide(a: &[u64; 9], b: &[u64; 4]) -> [u64; 9] {
    let mut out = *a;
    let mut carry = 0u128;
    for i in 0..9 {
        let sum = out[i] as u128 + if i < 4 { b[i] as u128 } else { 0 } + carry;
        out[i] = sum as u64;
        carry = sum >> 64;
    }
    out
}

/// A 512-bit value mod L, the slow way: one bit at a time from the top, doubling and
/// subtracting L whenever the remainder reaches it.
fn slow_reduce(bytes: &[u8; 64]) -> [u64; 4] {
    let mut rem = [0u64; 5];
    for bit in (0..512).rev() {
        let incoming = (bytes[bit / 8] >> (bit % 8)) & 1;
        for i in (1..5).rev() {
            rem[i] = rem[i] << 1 | rem[i - 1] >> 63;
        }
        rem[0] = rem[0] << 1 | incoming as u64;
        let l = [L[0], L[1], L[2], L[3], 0];
        let at_least = (0..5).rev().find(|&i| rem[i] != l[i]).is_none_or(|i| rem[i] > l[i]);
        if at_least {
            let mut borrow = 0u128;
            for i in 0..5 {
                let d = rem[i] as u128 + (1u128 << 64) - l[i] as u128 - borrow;
                rem[i] = d as u64;
                borrow = u128::from(d >> 64 == 0);
            }
        }
    }
    [rem[0], rem[1], rem[2], rem[3]]
}

#[test]
fn the_reduction_mod_l_equals_the_slow_one() {
    for edge in [[0u8; 64], [0xff; 64]] {
        assert_eq!(scalar::reduce_wide(&edge), slow_reduce(&edge));
    }
    let mut l_bytes = [0u8; 64];
    l_bytes[..32].copy_from_slice(&scalar::to_bytes(&L));
    assert_eq!(scalar::reduce_wide(&l_bytes), [0; 4], "L mod L");
    blaze_sim::check(cases(500), |tape| {
        let mut bytes = [0u8; 64];
        tape.fill(&mut bytes, "wide");
        let reduced = scalar::reduce_wide(&bytes);
        assert_eq!(reduced, slow_reduce(&bytes));
        assert!(scalar::less(&reduced, &L));
        assert!(scalar::is_canonical(&scalar::to_bytes(&reduced)));
    });
}

/// Decoding: a drawn y either decodes to a point on the curve, -x^2 + y^2 = 1 + d x^2 y^2, with
/// the asked sign, or has no x; both happen.
#[test]
fn the_square_root_of_decoding() {
    let config = if cfg!(miri) { cases(2) } else { cases(300).must_reach(&["a point", "no point"]) };
    blaze_sim::check(config, |tape| {
        let mut bytes = [0u8; 32];
        tape.fill(&mut bytes, "encoding");
        bytes[31] &= 0x7f;
        let y = Fe::from_bytes(&bytes);
        bytes = y.to_bytes();
        if tape.coin("sign") {
            bytes[31] |= 0x80;
        }
        match Point::decode(&bytes) {
            Ok(point) => {
                let (x, y) = point.to_affine();
                let (xx, yy) = (x.square(), y.square());
                assert!(yy.sub(&xx).equals(&Fe::ONE.add(&D.mul(&xx).mul(&yy))), "on the curve");
                assert_eq!(x.is_negative(), bytes[31] >> 7 == 1, "the sign asked");
                assert_eq!(point.encode(), bytes, "encodes to itself");
                tape.reached("a point");
            }
            Err(Encoding::NotOnCurve) => tape.reached("no point"),
            Err(other) => panic!("a canonical y refused as {other:?}"),
        }
    });
}

/// The group's laws on drawn points of every order, and [n]P by two routes: Straus's signed
/// windows, and plain double-and-add from the top bit.
#[test]
fn the_group_laws() {
    blaze_sim::check(cases(60), |tape| {
        let (p, q, r) = (draw_point(tape), draw_point(tape), draw_point(tape));
        assert!(p.add(&q).add(&r).equals(&p.add(&q.add(&r))), "(P + Q) + R = P + (Q + R)");
        assert!(p.add(&q).equals(&q.add(&p)), "P + Q = Q + P");
        assert!(p.add(&p).equals(&p.double()), "P + P = 2P");
        assert!(p.add(&p.neg()).is_identity(), "P - P = O");
        assert!(p.sub(&q).add(&q).equals(&p), "(P - Q) + Q = P");
        assert!(p.add(&Point::IDENTITY).equals(&p));
        let decoded = Point::decode(&p.encode()).expect("an encoding decodes");
        assert!(decoded.equals(&p), "decode(encode(P)) = P");
        assert_eq!(decoded.encode(), p.encode());
        let n = draw_scalar(tape);
        let mut by_bits = Point::IDENTITY;
        for bit in (0..256).rev() {
            by_bits = by_bits.double();
            if n[bit / 64] >> (bit % 64) & 1 == 1 {
                by_bits = by_bits.add(&p);
            }
        }
        assert!(double_scalar_mul_basepoint(&n, &p, &[0; 4]).equals(&by_bits), "[n]P by Straus and by bits");
        let mut base_bits = Point::IDENTITY;
        for bit in (0..256).rev() {
            base_bits = base_bits.double();
            if n[bit / 64] >> (bit % 64) & 1 == 1 {
                base_bits = base_bits.add(&BASEPOINT);
            }
        }
        assert!(double_scalar_mul_basepoint(&[0; 4], &p, &n).equals(&base_bits), "[n]B by the table and by bits");
        assert!(double_scalar_mul_basepoint(&n, &p, &n).equals(&by_bits.add(&base_bits)), "[n]P + [n]B");
    });
}
